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

function runHook(event: string, sessionId?: string, cwd?: string): Promise<void> {
  return new Promise((resolve) => {
    const native: Record<string, string> = {};
    if (sessionId) native.session_id = sessionId;
    if (cwd) native.cwd = cwd;

    const child = spawn(
      HOOKLINESINKER_BIN,
      ["ingest", "--agent", "opencode", "--event", event],
      { stdio: ["pipe", "ignore", "ignore"] }
    );
    child.on("error", () => resolve());
    child.on("close", () => resolve());
    child.stdin.end(JSON.stringify(native));
  });
}

export const HooklinesinkerPlugin = async ({
  directory,
}: {
  project: any;
  client: any;
  $: any;
  directory: string;
  worktree: string;
}) => {
  // Post session.created on plugin load so status is seen immediately, even
  // when OpenCode resumes a previous session (which skips session.created).
  await runHook("session.created", undefined, directory);

  return {
    event: async ({
      event,
    }: {
      event: { type: string; [key: string]: any };
    }) => {
      if (!TRACKED_EVENTS.has(event.type)) return;

      const sessionId =
        (event as any).properties?.sessionID ||
        (event as any).properties?.info?.id ||
        (event as any).session_id ||
        (event as any).sessionID;

      let eventName = event.type;
      if (event.type === "session.status") {
        const status = (event as any).properties?.status?.type;
        if (!status) return;
        eventName = `session.status.${status}`;
      }

      await runHook(eventName, sessionId, directory);
    },
  };
};
