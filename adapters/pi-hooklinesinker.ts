import { spawn } from "node:child_process";

const HOOKLINESINKER_BIN: string = "__HOOKLINESINKER_BIN__";

const HOOK_TIMEOUT_MS = 2000;
const MAX_PENDING_HOOKS = 32;

interface NativeEvent {
  session_id?: string;
  reason?: string;
}

interface HookRequest {
  event: string;
  sessionId?: string;
  reason?: string;
}

interface HookOptions extends HookRequest {
  timeoutMs?: number;
}

interface PiContext {
  hasUI?: boolean;
  sessionManager?: { getSessionId?: () => string | undefined };
}

interface PiEvents {
  session_start: { reason?: "startup" | "reload" | "new" | "resume" | "fork" };
  agent_start: unknown;
  agent_settled: unknown;
  session_before_compact: unknown;
  session_compact: { reason?: "manual" | "threshold" | "overflow" };
  session_shutdown: { reason?: "quit" | "reload" | "new" | "resume" | "fork" };
}

interface PiHost {
  on<Event extends keyof PiEvents>(
    event: Event,
    handler: (event: PiEvents[Event], ctx: PiContext) => Promise<void>,
  ): void;
  events: {
    on(channel: string, handler: (event: unknown) => void): () => void;
  };
}

type Session = { ui: false } | { ui: true; id: string | undefined };

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

function runHook({
  event,
  sessionId,
  reason,
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
      if (reason) native.reason = reason;
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
    session = { ui: true, id };
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

  pi.on("session_start", async (event, ctx) => {
    pendingPermissionRequestIds.clear();
    if (ctx?.hasUI === false) {
      session = { ui: false };
      return;
    }
    const id = sessionIdFromContext(ctx);
    session = { ui: true, id };
    const reason = typeof event?.reason === "string" ? event.reason : undefined;
    await queueHook({ event: "session_start", sessionId: id, reason });
  });

  pi.on("agent_start", async (_event, ctx) => {
    if (!session.ui) return;
    await queueHook({ event: "agent_start", sessionId: rememberSessionId(ctx) });
  });
  // agent_settled fires after automatic retries and compaction complete.
  pi.on("agent_settled", async (_event, ctx) => {
    if (!session.ui) return;
    pendingPermissionRequestIds.clear();
    await queueHook({ event: "agent_settled", sessionId: rememberSessionId(ctx) });
  });

  // Automatic compaction resumes the current turn; manual compaction leaves it idle.
  pi.on("session_before_compact", async (_event, ctx) => {
    if (!session.ui) return;
    await queueHook({ event: "session_before_compact", sessionId: rememberSessionId(ctx) });
  });
  pi.on("session_compact", async (event, ctx) => {
    if (!session.ui) return;
    const done = event?.reason === "manual" ? "session_compact_idle" : "session_compact_working";
    await queueHook({ event: done, sessionId: rememberSessionId(ctx) });
  });

  // Session switches are reconciled by the next session_start.
  pi.on("session_shutdown", async (event, ctx) => {
    const wasUI = session.ui;
    const id = rememberSessionId(ctx);
    pendingPermissionRequestIds.clear();
    session = { ui: false };
    if (wasUI && event?.reason === "quit") {
      await queueHook({ event: "session_shutdown", sessionId: id });
    } else {
      await hookQueue;
    }
  });
}
