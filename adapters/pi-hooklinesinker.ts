import { spawn } from "node:child_process";

const HOOKLINESINKER_BIN = "__HOOKLINESINKER_BIN__";

const HOOK_TIMEOUT_MS = 2000;
const MAX_PENDING_HOOKS = 32;

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

function runHook(
  event: string,
  sessionId?: string,
  reason?: string,
  timeoutMs = HOOK_TIMEOUT_MS,
): Promise<void> {
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
        { detached: process.platform !== "win32", stdio: ["pipe", "ignore", "ignore"] }
      );

      const kill = (signal: NodeJS.Signals) => {
        if (process.platform !== "win32" && child.pid) {
          process.kill(-child.pid, signal);
        } else {
          child.kill(signal);
        }
      };
      const timer = setTimeout(() => {
        try {
          kill("SIGTERM");
        } catch {
          finish();
        }
        const forceTimer = setTimeout(() => {
          try {
            kill("SIGKILL");
          } catch {}
          finish();
        }, 100);
        forceTimer.unref?.();
      }, timeoutMs);
      timer.unref?.();

      child.on("error", finish);
      child.on("close", () => {
        clearTimeout(timer);
        finish();
      });
      child.stdin.on("error", () => {});

      const native: Record<string, string> = {};
      if (sessionId) native.session_id = sessionId;
      if (reason) native.reason = reason;
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
  let pendingHooks = 0;

  function queueHook(event: string, sessionId?: string, reason?: string): Promise<void> {
    if (pendingHooks >= MAX_PENDING_HOOKS) return Promise.resolve();
    const queuedAt = Date.now();
    pendingHooks += 1;
    const queued = hookQueue.then(() => {
      const remaining = HOOK_TIMEOUT_MS - (Date.now() - queuedAt);
      return remaining > 0
        ? runHook(event, sessionId, reason, remaining)
        : Promise.resolve();
    });
    hookQueue = queued.finally(() => {
      pendingHooks -= 1;
    });
    return queued;
  }

  function rememberSessionId(ctx: PiContext): string | undefined {
    currentSessionId = sessionIdFromContext(ctx) ?? currentSessionId;
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

  pi.events.on("permissions:ui_prompt", (event) => {
    if (!isUISession) return;
    const requestId = permissionRequestId(event);
    if (!requestId) return;
    pendingPermissionRequestIds.add(requestId);
    void queueHook("permission_prompt", currentSessionId);
  });
  pi.events.on("permissions:decision", (event) => {
    if (!isUISession || !isPromptDecision(event)) return;
    const requestId = permissionRequestId(event);
    if (!requestId || !pendingPermissionRequestIds.has(requestId)) return;
    pendingPermissionRequestIds.delete(requestId);
    queuePermissionResolvedIfNonePending();
  });

  pi.on("session_start", async (event, ctx) => {
    pendingPermissionRequestIds.clear();
    isUISession = ctx?.hasUI !== false;
    currentSessionId = isUISession ? sessionIdFromContext(ctx) : undefined;
    if (isUISession) {
      const reason = typeof event?.reason === "string" ? event.reason : undefined;
      await queueHook("session_start", currentSessionId, reason);
    }
  });

  pi.on("agent_start", async (_event, ctx) => {
    if (!isUISession) return;
    await queueHook("agent_start", rememberSessionId(ctx));
  });
  // agent_settled fires after automatic retries and compaction complete.
  pi.on("agent_settled", async (_event, ctx) => {
    if (!isUISession) return;
    pendingPermissionRequestIds.clear();
    await queueHook("agent_settled", rememberSessionId(ctx));
  });

  // Automatic compaction resumes the current turn; manual compaction leaves it idle.
  pi.on("session_before_compact", async (_event, ctx) => {
    if (!isUISession) return;
    await queueHook("session_before_compact", rememberSessionId(ctx));
  });
  pi.on("session_compact", async (event, ctx) => {
    if (!isUISession) return;
    const done = event?.reason === "manual" ? "session_compact_idle" : "session_compact_working";
    await queueHook(done, rememberSessionId(ctx));
  });

  // Session switches are reconciled by the next session_start.
  pi.on("session_shutdown", async (event, ctx) => {
    const id = isUISession ? rememberSessionId(ctx) : undefined;
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
