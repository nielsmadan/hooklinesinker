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
]);

const HOOK_TIMEOUT_MS = 2000;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function runHook(event: string, sessionId?: string, cwd?: string): Promise<void> {
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
  // Post session.created on plugin load so status is seen immediately, even
  // when OpenCode resumes a previous session (which skips session.created).
  await runHook("session.created", undefined, directory);

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

      let eventName = event.type;
      if (event.type === "session.status") {
        const status = isRecord(properties?.status) ? properties.status.type : undefined;
        if (typeof status !== "string" || !status) return;
        eventName = `session.status.${status}`;
      }

      await runHook(eventName, sessionId, directory);
    },
  };
};
