import { spawn } from "node:child_process";

const HOOKLINESINKER_BIN = "__HOOKLINESINKER_BIN__";

const HOOK_TIMEOUT_MS = 2000;

interface PiContext {
  hasUI?: boolean;
  sessionManager?: { getSessionId?: () => string | undefined };
}

interface PiEvents {
  session_start: unknown;
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

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function runHook(event: string, sessionId?: string): Promise<void> {
  return new Promise((resolve) => {
    let settled = false;
    const finish = () => {
      if (settled) return;
      settled = true;
      resolve();
    };

    try {
      const child = spawn(
        HOOKLINESINKER_BIN,
        ["ingest", "--agent", "pi", "--event", event],
        { stdio: ["pipe", "ignore", "ignore"] }
      );

      const timer = setTimeout(() => {
        try {
          child.kill();
        } catch {
          // already exited
        }
        finish();
      }, HOOK_TIMEOUT_MS);
      timer.unref?.();

      child.on("error", finish);
      child.on("close", () => {
        clearTimeout(timer);
        finish();
      });
      child.stdin.on("error", () => {});

      const native: Record<string, string> = {};
      if (sessionId) native.session_id = sessionId;
      child.stdin.end(JSON.stringify(native));
    } catch {
      finish();
    }
  });
}

export default function (pi: PiHost) {
  let currentSessionId: string | undefined;
  let isUISession = false;
  const pendingPermissionRequestIds = new Set<string>();
  let hookQueue: Promise<void> = Promise.resolve();

  function queueHook(event: string, sessionId?: string): Promise<void> {
    hookQueue = hookQueue.then(() => runHook(event, sessionId));
    return hookQueue;
  }

  function sessionId(ctx: PiContext): string | undefined {
    try {
      return ctx?.sessionManager?.getSessionId?.() ?? undefined;
    } catch {
      return undefined;
    }
  }

  function rememberSessionId(ctx: PiContext): string | undefined {
    currentSessionId = sessionId(ctx) ?? currentSessionId;
    return currentSessionId;
  }

  function permissionRequestId(event: unknown): string | undefined {
    return isRecord(event) && typeof event.requestId === "string" ? event.requestId : undefined;
  }

  function queuePermissionResolvedIfNonePending() {
    if (pendingPermissionRequestIds.size === 0) {
      void queueHook("permission_resolved", currentSessionId);
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

  const unsubscribePermissionPrompt = pi.events.on("permissions:ui_prompt", (event) => {
    if (!isUISession) return;
    const requestId = permissionRequestId(event);
    if (!requestId) return;
    pendingPermissionRequestIds.add(requestId);
    void queueHook("permission_prompt", currentSessionId);
  });
  const unsubscribePermissionDecision = pi.events.on("permissions:decision", (event) => {
    if (!isUISession || !isPromptDecision(event)) return;
    const requestId = pendingPermissionRequestIds.values().next().value;
    if (typeof requestId !== "string") return;
    pendingPermissionRequestIds.delete(requestId);
    queuePermissionResolvedIfNonePending();
  });

  // Session appears (fires at launch with reason "startup", and again on
  // new/resume/reload/fork — re-queuing idle is correct: the session is idle).
  pi.on("session_start", async (_event, ctx) => {
    pendingPermissionRequestIds.clear();
    isUISession = ctx?.hasUI !== false;
    currentSessionId = isUISession ? sessionId(ctx) : undefined;
    if (isUISession) {
      await queueHook("session_start", currentSessionId);
    }
  });

  // Turn boundaries. agent_settled is Pi's recommended "done" signal — unlike
  // agent_end, Pi will not auto-retry/compact/continue after it.
  pi.on("agent_start", async (_event, ctx) => {
    if (!isUISession) return;
    await queueHook("agent_start", rememberSessionId(ctx));
  });
  pi.on("agent_settled", async (_event, ctx) => {
    if (!isUISession) return;
    pendingPermissionRequestIds.clear();
    await queueHook("agent_settled", rememberSessionId(ctx));
  });

  // Compaction. session_compact carries a reason: a manual /compact leaves the
  // session idle; a threshold/overflow compaction is mid-turn and resumes work.
  pi.on("session_before_compact", async (_event, ctx) => {
    if (!isUISession) return;
    await queueHook("session_before_compact", rememberSessionId(ctx));
  });
  pi.on("session_compact", async (event, ctx) => {
    if (!isUISession) return;
    const done = event?.reason === "manual" ? "session_compact_idle" : "session_compact_working";
    await queueHook(done, rememberSessionId(ctx));
  });

  // Session removal. Only a real quit removes the session — new/resume/reload/
  // fork keep the same terminal session and are followed by a session_start.
  pi.on("session_shutdown", async (event, ctx) => {
    const id = isUISession ? rememberSessionId(ctx) : undefined;
    unsubscribePermissionPrompt();
    unsubscribePermissionDecision();
    pendingPermissionRequestIds.clear();
    currentSessionId = undefined;
    if (isUISession && event?.reason === "quit") {
      await queueHook("session_shutdown", id);
    } else {
      await hookQueue;
    }
    isUISession = false;
  });
}
