import { spawn } from "node:child_process";

const HOOKLINESINKER_BIN: string = "__HOOKLINESINKER_BIN__";

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

const OPENCODE_STATUSES = ["idle", "busy", "retry"] as const;

const HOOK_TIMEOUT_MS = 2000;
const MAX_PENDING_HOOKS = 32;

interface NativeEvent {
  session_id?: string;
  cwd?: string;
}

interface HookRequest {
  event: string;
  sessionId?: string;
}

interface HookOptions extends HookRequest {
  cwd?: string;
  timeoutMs?: number;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isOpenCodeStatus(value: unknown): value is (typeof OPENCODE_STATUSES)[number] {
  return typeof value === "string" && OPENCODE_STATUSES.some((known) => known === value);
}

function runHook({
  event,
  sessionId,
  cwd,
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
  function queueHook(request: HookRequest): Promise<void> {
    if (pendingHooks >= MAX_PENDING_HOOKS) return Promise.resolve();
    const queuedAt = Date.now();
    pendingHooks += 1;
    const queued = hookQueue.then(() => {
      const remaining = HOOK_TIMEOUT_MS - (Date.now() - queuedAt);
      return remaining > 0
        ? runHook({ ...request, cwd: directory, timeoutMs: remaining })
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

  // Resuming a session can skip the native session.created event.
  await queueHook({ event: "session.created" });

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
        const removals = [...knownSessionIds].map((id) =>
          queueHook({ event: "session.deleted", sessionId: id }),
        );
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
        if (!isOpenCodeStatus(status)) return;
        eventName = `session.status.${status}`;
      }

      await queueHook({ event: eventName, sessionId });
    },
  };
};
