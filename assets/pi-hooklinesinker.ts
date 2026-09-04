import { spawn } from "node:child_process";

const HOOKLINESINKER_BIN = "__HOOKLINESINKER_BIN__";

const HOOK_TIMEOUT_MS = 2000;

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

export default function (pi: any) {
  let currentSessionId: string | undefined;
  let isUISession = false;
  const pendingPermissionRequestIds = new Set<string>();
  let hookQueue: Promise<void> = Promise.resolve();

  function queueHook(event: string, sessionId?: string): Promise<void> {
    hookQueue = hookQueue.then(() => runHook(event, sessionId));
    return hookQueue;
  }

  function sessionId(ctx: any): string | undefined {
    try {
      return ctx?.sessionManager?.getSessionId?.() ?? undefined;
    } catch {
      return undefined;
    }
  }

  function rememberSessionId(ctx: any): string | undefined {
    currentSessionId = sessionId(ctx) ?? currentSessionId;
    return currentSessionId;
  }

  function permissionRequestId(event: any): string | undefined {
    return typeof event?.requestId === "string" ? event.requestId : undefined;
  }

  function queuePermissionResolvedIfNonePending() {
    if (pendingPermissionRequestIds.size === 0) {
      void queueHook("permission_resolved", currentSessionId);
    }
  }

  function isPromptDecision(event: any): boolean {
    return [
      "user_approved",
      "user_approved_for_session",
      "user_denied",
      "confirmation_unavailable",
    ].includes(event?.resolution);
  }

  const unsubscribePermissionPrompt = pi.events.on("permissions:ui_prompt", (event: any) => {
    if (!isUISession) return;
    const requestId = permissionRequestId(event);
    if (!requestId) return;
    pendingPermissionRequestIds.add(requestId);
    void queueHook("permission_prompt", currentSessionId);
  });
  const unsubscribePermissionDecision = pi.events.on("permissions:decision", (event: any) => {
    if (!isUISession || !isPromptDecision(event)) return;
    const requestId = pendingPermissionRequestIds.values().next().value;
    if (typeof requestId !== "string") return;
    pendingPermissionRequestIds.delete(requestId);
    queuePermissionResolvedIfNonePending();
  });

  // Session appears (fires at launch with reason "startup", and again on
  // new/resume/reload/fork — re-queuing idle is correct: the session is idle).
  pi.on("session_start", async (_event: any, ctx: any) => {
    pendingPermissionRequestIds.clear();
    isUISession = ctx?.hasUI !== false;
    currentSessionId = isUISession ? sessionId(ctx) : undefined;
    if (isUISession) {
      await queueHook("session_start", currentSessionId);
    }
  });

  // Turn boundaries. agent_settled is Pi's recommended "done" signal — unlike
  // agent_end, Pi will not auto-retry/compact/continue after it.
  pi.on("agent_start", async (_event: any, ctx: any) => {
    if (!isUISession) return;
    await queueHook("agent_start", rememberSessionId(ctx));
  });
  pi.on("agent_settled", async (_event: any, ctx: any) => {
    if (!isUISession) return;
    pendingPermissionRequestIds.clear();
    await queueHook("agent_settled", rememberSessionId(ctx));
  });

  // Compaction. session_compact carries a reason: a manual /compact leaves the
  // session idle; a threshold/overflow compaction is mid-turn and resumes work.
  pi.on("session_before_compact", async (_event: any, ctx: any) => {
    if (!isUISession) return;
    await queueHook("session_before_compact", rememberSessionId(ctx));
  });
  pi.on("session_compact", async (event: any, ctx: any) => {
    if (!isUISession) return;
    const done = event?.reason === "manual" ? "session_compact_idle" : "session_compact_working";
    await queueHook(done, rememberSessionId(ctx));
  });

  // Session removal. Only a real quit removes the session — new/resume/reload/
  // fork keep the same terminal session and are followed by a session_start.
  pi.on("session_shutdown", async (event: any, ctx: any) => {
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
