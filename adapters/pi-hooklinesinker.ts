import { spawn } from "node:child_process";
import { readdir, readFile } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { join, resolve as resolvePath } from "node:path";

const HOOKLINESINKER_BIN: string = join(homedir(), "__HOOKLINESINKER_BIN__");

const HOOK_TIMEOUT_MS = 2000;
// Reports working without starting a turn, so a held session keeps its tool, problem and turn count.
const HELD_WORKING_EVENT = "session_compact_working";
const MAX_PENDING_HOOKS = 32;

interface PiHookFields {
  reason?: string;
  model?: string;
  effort?: string;
  context_percent?: number | null;
  session_title?: string | null;
  problem?: "interrupted";
}

interface NativeEvent extends PiHookFields {
  session_id?: string;
}

interface HookRequest {
  event: string;
  sessionId?: string;
  fields?: PiHookFields;
}

interface HookOptions extends HookRequest {
  timeoutMs?: number;
}

interface PiContext {
  hasUI?: boolean;
  model?: { id?: string };
  thinkingLevel?: string;
  getContextUsage?: () => { percent: number | null } | undefined;
  sessionManager?: {
    getSessionId?: () => string | undefined;
    getSessionFile?: () => string | undefined;
  };
}

interface PiEvents {
  session_start: { reason?: "startup" | "reload" | "new" | "resume" | "fork" };
  agent_start: unknown;
  agent_settled: { aborted?: boolean };
  session_before_compact: unknown;
  session_compact: { reason?: "manual" | "threshold" | "overflow" };
  session_info_changed: { name?: string };
  model_select: { model?: { id?: string } };
  thinking_level_select: { level?: string };
  session_shutdown: { reason?: "quit" | "reload" | "new" | "resume" | "fork" };
}

interface PiHost {
  on<Event extends keyof PiEvents>(
    event: Event,
    handler: (event: PiEvents[Event], ctx: PiContext) => Promise<void>,
  ): void;
  getSessionName?: () => string | undefined;
  events: {
    on(channel: string, handler: (event: unknown) => void): () => void;
  };
}

type Session =
  | { ui: false }
  | { ui: true; id: string | undefined; runOwner: string | undefined };

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function sessionIdFromContext(ctx: PiContext): string | undefined {
  try {
    return ctx?.sessionManager?.getSessionId?.() ?? undefined;
  } catch {
    return undefined;
  }
}

// Mirrors pi-subagents' resolveCurrentSessionId (src/shared/session-identity.js).
function runOwnerFromContext(ctx: PiContext): string | undefined {
  try {
    return ctx?.sessionManager?.getSessionFile?.() ?? ctx?.sessionManager?.getSessionId?.() ?? undefined;
  } catch {
    return undefined;
  }
}

function attempt<T>(read: () => T): T | undefined {
  try {
    return read();
  } catch {
    return undefined;
  }
}

function contextPercent(ctx: PiContext): number | null | undefined {
  const percent = attempt(() => ctx.getContextUsage?.())?.percent;
  if (percent === null) return null;
  return typeof percent === "number" && Number.isFinite(percent) ? Math.round(percent) : undefined;
}

function contextFields(ctx: PiContext, sessionName: string | undefined): PiHookFields {
  const model = attempt(() => ctx.model?.id);
  const effort = attempt(() => ctx.thinkingLevel);
  const percent = contextPercent(ctx);
  return {
    ...(typeof model === "string" ? { model } : {}),
    ...(typeof effort === "string" ? { effort } : {}),
    ...(percent !== undefined ? { context_percent: percent } : {}),
    ...(typeof sessionName === "string" ? { session_title: sessionName } : {}),
  };
}

function isErrnoException(error: unknown): error is NodeJS.ErrnoException {
  return error instanceof Error && "code" in error;
}

function pidAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return isErrnoException(error) && error.code === "EPERM";
  }
}

// Mirrors pi-subagents' temp-root layout (src/shared/types.js).
function asyncRunsDir(): string | undefined {
  const configured = process.env.PI_SUBAGENTS_TEMP_ROOT?.trim();
  if (configured) {
    return join(resolvePath(configured), "async-subagent-runs");
  }
  if (typeof process.getuid === "function") {
    return join(tmpdir(), `pi-subagents-uid-${process.getuid()}`, "async-subagent-runs");
  }
  return undefined;
}

// Mirrors pi-subagents' currentCompletionOwnerId (src/shared/completion-owner.js).
function completionOwnerId(): unknown {
  return Reflect.get(globalThis, Symbol.for("pi-subagents.completion-owner-id"));
}

async function liveAsyncRun(
  dir: string,
  entryName: string,
  runOwner: string,
): Promise<[string, number] | undefined> {
  try {
    const raw = await readFile(join(dir, entryName, "status.json"), "utf8");
    const status: unknown = JSON.parse(raw);
    if (!isRecord(status)) return undefined;
    if (status.state !== "queued" && status.state !== "running") return undefined;
    if (status.sessionId !== runOwner) return undefined;
    // Only the pi process that owns a run is told when it completes.
    if ("completionOwnerId" in status && status.completionOwnerId !== completionOwnerId()) {
      return undefined;
    }
    if (typeof status.pid !== "number" || !pidAlive(status.pid)) return undefined;
    const id = typeof status.runId === "string" ? status.runId : entryName;
    return [id, status.pid];
  } catch {
    return undefined;
  }
}

async function liveAsyncRuns(runOwner: string): Promise<Map<string, number>> {
  const dir = asyncRunsDir();
  if (!dir) return new Map();
  try {
    const entries = await readdir(dir, { withFileTypes: true });
    const found = await Promise.all(
      entries
        .filter((entry) => entry.isDirectory())
        .map((entry) => liveAsyncRun(dir, entry.name, runOwner)),
    );
    return new Map(found.filter((entry): entry is [string, number] => entry !== undefined));
  } catch {
    return new Map();
  }
}

function runHook({
  event,
  sessionId,
  fields,
  timeoutMs = HOOK_TIMEOUT_MS,
}: HookOptions): Promise<void> {
  return new Promise((resolve) => {
    let settled = false;
    let timer: NodeJS.Timeout | undefined;
    let forceTimer: NodeJS.Timeout | undefined;
    const finish = () => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      clearTimeout(forceTimer);
      resolve();
    };

    try {
      const child = spawn(
        HOOKLINESINKER_BIN,
        ["ingest", "--agent", "pi", "--event", event],
        { detached: process.platform !== "win32", stdio: ["pipe", "ignore", "ignore"] }
      );

      const kill = (signal: NodeJS.Signals) => {
        if (process.platform !== "win32" && child.pid) {
          process.kill(-child.pid, signal);
        } else {
          child.kill(signal);
        }
      };
      timer = setTimeout(() => {
        try {
          kill("SIGTERM");
        } catch {
          finish();
          return;
        }
        forceTimer = setTimeout(() => {
          try {
            kill("SIGKILL");
          } catch {}
          finish();
        }, 100);
        forceTimer.unref?.();
      }, timeoutMs);
      timer.unref?.();

      child.on("error", finish);
      child.on("close", finish);
      child.stdin.on("error", () => {});

      const native: NativeEvent = {};
      if (sessionId) native.session_id = sessionId;
      Object.assign(native, fields);
      child.stdin.end(JSON.stringify(native));
    } catch {
      finish();
    }
  });
}

export default function (pi: PiHost) {
  let session: Session = { ui: false };
  const pendingPermissionRequestIds = new Set<string>();
  let hookQueue: Promise<void> = Promise.resolve();
  let pendingHooks = 0;

  const liveRuns = new Map<string, number>();
  let heldWorking = false;
  let midTurn = false;
  let epoch = 0;
  let completedDuringScan: Set<string> | undefined;

  function pruneDeadRuns(): void {
    for (const [id, pid] of liveRuns) {
      if (!pidAlive(pid)) liveRuns.delete(id);
    }
  }

  async function scanAsyncRuns(
    runOwner: string,
    scanEpoch: number,
    completed: Set<string>,
  ): Promise<void> {
    const scanned = await liveAsyncRuns(runOwner);
    if (completedDuringScan === completed) completedDuringScan = undefined;
    if (epoch !== scanEpoch) return;
    for (const [id, pid] of scanned) {
      if (!completed.has(id)) liveRuns.set(id, pid);
    }
    if (liveRuns.size > 0 && !midTurn) {
      heldWorking = true;
      await queueHook({
        event: HELD_WORKING_EVENT,
        sessionId: session.ui ? session.id : undefined,
      });
    }
  }

  async function settleOrHold({ event, sessionId, fields }: HookRequest): Promise<void> {
    pruneDeadRuns();
    if (liveRuns.size > 0) {
      heldWorking = true;
      midTurn = true;
      await queueHook({ event: HELD_WORKING_EVENT, sessionId, fields });
      return;
    }
    heldWorking = false;
    midTurn = false;
    await queueHook({ event, sessionId, fields });
  }

  function detailsFrom(ctx: PiContext): PiHookFields {
    return contextFields(ctx, attempt(() => pi.getSessionName?.()));
  }

  function annotate(event: string, ctx: PiContext, fields: PiHookFields | undefined): Promise<void> {
    return queueHook({ event, sessionId: rememberSessionId(ctx), fields });
  }

  function queueHook(request: HookRequest): Promise<void> {
    if (pendingHooks >= MAX_PENDING_HOOKS) return Promise.resolve();
    const queuedAt = Date.now();
    pendingHooks += 1;
    const queued = hookQueue.then(() => {
      const remaining = HOOK_TIMEOUT_MS - (Date.now() - queuedAt);
      return remaining > 0
        ? runHook({ ...request, timeoutMs: remaining })
        : Promise.resolve();
    });
    const settledQueue = queued.then(
      () => {},
      () => {},
    ).finally(() => {
      pendingHooks -= 1;
    });
    hookQueue = settledQueue;
    return settledQueue;
  }

  function rememberSessionId(ctx: PiContext): string | undefined {
    if (!session.ui) return undefined;
    const id = sessionIdFromContext(ctx) ?? session.id;
    const runOwner = runOwnerFromContext(ctx) ?? session.runOwner;
    session = { ui: true, id, runOwner };
    return id;
  }

  function permissionRequestId(event: unknown): string | undefined {
    return isRecord(event) && typeof event.requestId === "string" ? event.requestId : undefined;
  }

  function queuePermissionResolvedIfNonePending(sessionId: string | undefined) {
    if (pendingPermissionRequestIds.size === 0) {
      void queueHook({ event: "permission_resolved", sessionId });
    }
  }

  function isPromptDecision(event: unknown): boolean {
    if (!isRecord(event) || typeof event.resolution !== "string") return false;
    return [
      "user_approved",
      "user_approved_for_session",
      "user_denied",
      "confirmation_unavailable",
    ].includes(event.resolution);
  }

  pi.events.on("permissions:ui_prompt", (event) => {
    if (!session.ui) return;
    const requestId = permissionRequestId(event);
    if (!requestId) return;
    pendingPermissionRequestIds.add(requestId);
    void queueHook({ event: "permission_prompt", sessionId: session.id });
  });
  pi.events.on("permissions:decision", (event) => {
    if (!session.ui || !isPromptDecision(event)) return;
    const requestId = permissionRequestId(event);
    if (!requestId || !pendingPermissionRequestIds.has(requestId)) return;
    pendingPermissionRequestIds.delete(requestId);
    queuePermissionResolvedIfNonePending(session.id);
  });
  pi.events.on("subagent:async-started", (payload) => {
    if (!session.ui) return;
    if (!isRecord(payload)) return;
    if (typeof payload.sessionId !== "string" || payload.sessionId !== session.runOwner) return;
    if (typeof payload.id !== "string" || typeof payload.pid !== "number") return;
    liveRuns.set(payload.id, payload.pid);
  });
  pi.events.on("subagent:async-complete", (payload) => {
    if (!session.ui) return;
    if (!isRecord(payload)) return;
    const runId =
      typeof payload.runId === "string"
        ? payload.runId
        : typeof payload.id === "string"
          ? payload.id
          : undefined;
    if (!runId) return;
    completedDuringScan?.add(runId);
    const wasLive = liveRuns.delete(runId);
    if (wasLive && liveRuns.size === 0 && heldWorking && payload.triggerTurn === false) {
      heldWorking = false;
      midTurn = false;
      void queueHook({ event: "agent_settled", sessionId: session.id });
    }
  });

  pi.on("session_start", async (event, ctx) => {
    pendingPermissionRequestIds.clear();
    if (ctx?.hasUI === false) {
      session = { ui: false };
      return;
    }
    const id = sessionIdFromContext(ctx);
    const runOwner = runOwnerFromContext(ctx);
    session = { ui: true, id, runOwner };
    liveRuns.clear();
    heldWorking = false;
    midTurn = false;
    epoch += 1;
    const scanEpoch = epoch;
    const completed = new Set<string>();
    if (runOwner !== undefined) completedDuringScan = completed;
    const reason = typeof event?.reason === "string" ? event.reason : undefined;
    await queueHook({
      event: "session_start",
      sessionId: id,
      fields: {
        ...(reason === "new" || reason === "fork" ? { session_title: null } : {}),
        ...detailsFrom(ctx),
        ...(reason ? { reason } : {}),
      },
    });
    if (runOwner !== undefined) {
      void scanAsyncRuns(runOwner, scanEpoch, completed);
    }
  });

  pi.on("agent_start", async (_event, ctx) => {
    if (!session.ui) return;
    heldWorking = false;
    midTurn = true;
    await queueHook({
      event: "agent_start",
      sessionId: rememberSessionId(ctx),
      fields: detailsFrom(ctx),
    });
  });
  // agent_settled fires after automatic retries and compaction complete.
  pi.on("agent_settled", async (event, ctx) => {
    if (!session.ui) return;
    pendingPermissionRequestIds.clear();
    await settleOrHold({
      event: "agent_settled",
      sessionId: rememberSessionId(ctx),
      fields: {
        ...detailsFrom(ctx),
        ...(event?.aborted === true ? { problem: "interrupted" } : {}),
      },
    });
  });

  // Automatic compaction resumes the current turn; manual compaction leaves it idle.
  pi.on("session_before_compact", async (_event, ctx) => {
    if (!session.ui) return;
    await queueHook({
      event: "session_before_compact",
      sessionId: rememberSessionId(ctx),
      fields: detailsFrom(ctx),
    });
  });
  pi.on("session_compact", async (event, ctx) => {
    if (!session.ui) return;
    const sessionId = rememberSessionId(ctx);
    const fields = detailsFrom(ctx);
    if (event?.reason === "manual") {
      await settleOrHold({ event: "session_compact_idle", sessionId, fields });
      return;
    }
    await queueHook({ event: "session_compact_working", sessionId, fields });
  });

  pi.on("session_info_changed", async (event, ctx) => {
    if (!session.ui) return;
    const name = event?.name;
    await annotate("session_info_changed", ctx, { session_title: typeof name === "string" ? name : null });
  });
  pi.on("model_select", async (event, ctx) => {
    if (!session.ui) return;
    const model = event?.model?.id;
    await annotate("model_select", ctx, typeof model === "string" ? { model } : undefined);
  });
  pi.on("thinking_level_select", async (event, ctx) => {
    if (!session.ui) return;
    const effort = event?.level;
    await annotate("thinking_level_select", ctx, typeof effort === "string" ? { effort } : undefined);
  });

  // Session switches are reconciled by the next session_start.
  pi.on("session_shutdown", async (event, ctx) => {
    const wasUI = session.ui;
    const id = rememberSessionId(ctx);
    pendingPermissionRequestIds.clear();
    liveRuns.clear();
    heldWorking = false;
    midTurn = false;
    completedDuringScan = undefined;
    epoch += 1;
    session = { ui: false };
    if (wasUI && event?.reason === "quit") {
      await queueHook({ event: "session_shutdown", sessionId: id });
    } else {
      await hookQueue;
    }
  });
}
