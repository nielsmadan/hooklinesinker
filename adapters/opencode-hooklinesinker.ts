import { spawn } from "node:child_process";

const HOOKLINESINKER_BIN = "__HOOKLINESINKER_BIN__";

const TRACKED_EVENTS = new Set([
  "session.created",
  "session.status",
  "session.deleted",
  "session.compacted",
  "session.error",
  "session.idle",
  "permission.asked",
  "server.instance.disposed",
  "tui.session.select",
]);

const HOOK_TIMEOUT_MS = 2000;
const MAX_PENDING_HOOKS = 32;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function runHook(
  event: string,
  sessionId?: string,
  cwd?: string,
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
        ["ingest", "--agent", "opencode", "--event", event],
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
      if (cwd) native.cwd = cwd;
      child.stdin.end(JSON.stringify(native));
    } catch {
      finish();
    }
  });
}

export const HooklinesinkerPlugin = async ({
  directory,
}: {
  directory: string;
}) => {
  const knownSessionIds = new Set<string>();
  let hookQueue: Promise<void> = Promise.resolve();
  let pendingHooks = 0;
  function queueHook(event: string, sessionId?: string): Promise<void> {
    if (pendingHooks >= MAX_PENDING_HOOKS) return Promise.resolve();
    const queuedAt = Date.now();
    pendingHooks += 1;
    const queued = hookQueue.then(() => {
      const remaining = HOOK_TIMEOUT_MS - (Date.now() - queuedAt);
      return remaining > 0
        ? runHook(event, sessionId, directory, remaining)
        : Promise.resolve();
    });
    hookQueue = queued.finally(() => {
      pendingHooks -= 1;
    });
    return queued;
  }

  // Resuming a session can skip the native session.created event.
  await queueHook("session.created");

  return {
    event: async ({
      event,
    }: {
      event: unknown;
    }) => {
      if (!isRecord(event) || typeof event.type !== "string") return;
      if (!TRACKED_EVENTS.has(event.type)) return;

      const properties = isRecord(event.properties) ? event.properties : undefined;
      const info = isRecord(properties?.info) ? properties.info : undefined;
      const sessionId = [properties?.sessionID, info?.id, event.session_id, event.sessionID]
        .find((id): id is string => typeof id === "string" && id.length > 0);

      if (event.type === "server.instance.disposed") {
        const removals = [...knownSessionIds].map((id) => queueHook("session.deleted", id));
        knownSessionIds.clear();
        await Promise.all(removals);
        return;
      }
      if (sessionId && event.type === "session.deleted") {
        knownSessionIds.delete(sessionId);
      } else if (sessionId) {
        knownSessionIds.add(sessionId);
      }

      let eventName = event.type;
      if (event.type === "session.status") {
        const status = isRecord(properties?.status) ? properties.status.type : undefined;
        if (typeof status !== "string" || !status) return;
        eventName = `session.status.${status}`;
      }

      await queueHook(eventName, sessionId);
    },
  };
};
