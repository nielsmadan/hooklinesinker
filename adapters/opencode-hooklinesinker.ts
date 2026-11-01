import { spawn } from "node:child_process";
import { homedir } from "node:os";
import { join } from "node:path";

const HOOKLINESINKER_BIN: string = join(homedir(), "__HOOKLINESINKER_BIN__");

const TRACKED_EVENTS = new Set([
  "session.created",
  "session.updated",
  "session.status",
  "session.deleted",
  "session.compacted",
  "session.error",
  "session.idle",
  "permission.asked",
  "permission.replied",
  "server.instance.disposed",
  "tui.session.select",
]);

interface HostConfig {
  agent: "opencode" | "kilo";
  statuses: Record<string, "idle" | "working">;
  backgroundEnv: string;
  backgroundFallbackEnv: string | null;
  backgroundDefault: boolean;
  compaction: "active" | "complete";
  disposal: "event" | "callback";
}

const HOST_CONFIG: HostConfig = JSON.parse("__HOOKLINESINKER_HOST_CONFIG__");

const HOOK_TIMEOUT_MS = 2000;
const MAX_PENDING_HOOKS = 32;
const MAX_TERMINAL_HOOKS = 8;

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

type TreePhase = "idle" | "working" | "permission";

interface SessionTree {
  parentOf: Map<string, string>;
  busy: Set<string>;
  pendingAsks: Map<string, Set<string>>;
}

interface PhaseRecord {
  root: string;
  phase: TreePhase;
}

const TRUE_VALUES = new Set(["true", "yes", "on", "1", "y"]);

const PHASE_EVENT: Record<TreePhase, string> = {
  idle: "session.status.idle",
  working: "session.status.busy",
  permission: "permission.asked",
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isHostStatus(value: unknown): value is string {
  return typeof value === "string" && Object.hasOwn(HOST_CONFIG.statuses, value);
}

function rootOf(tree: SessionTree, sessionId: string): string {
  const visited = new Set<string>();
  let current = sessionId;
  while (!visited.has(current)) {
    const parent = tree.parentOf.get(current);
    if (!parent) return current;
    visited.add(current);
    current = parent;
  }
  return current;
}

function treePhase(tree: SessionTree, rootId: string): TreePhase {
  const candidates = new Set<string>([rootId, ...tree.busy, ...tree.pendingAsks.keys()]);
  let working = false;
  for (const id of candidates) {
    if (rootOf(tree, id) !== rootId) continue;
    const asks = tree.pendingAsks.get(id);
    if (asks && asks.size > 0) return "permission";
    if (tree.busy.has(id)) working = true;
  }
  return working ? "working" : "idle";
}

function descendantsOf(tree: SessionTree, root: string): string[] {
  const ids = new Set<string>([...tree.parentOf.keys(), ...tree.busy, ...tree.pendingAsks.keys()]);
  return [...ids].filter((id) => id !== root && rootOf(tree, id) === root);
}

function forgetSession(tree: SessionTree, id: string): void {
  tree.parentOf.delete(id);
  tree.busy.delete(id);
  tree.pendingAsks.delete(id);
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
        ["ingest", "--agent", HOST_CONFIG.agent, "--event", event],
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
  const tree: SessionTree = { parentOf: new Map(), busy: new Set(), pendingAsks: new Map() };
  const lastSent = new Map<string, TreePhase>();
  const lastQueued = new Map<string, PhaseRecord>();
  const heldRoots = new Set<string>();
  const backgroundValue = process.env[HOST_CONFIG.backgroundEnv] ??
    (HOST_CONFIG.backgroundFallbackEnv ? process.env[HOST_CONFIG.backgroundFallbackEnv] : undefined);
  const backgroundSubagents = backgroundValue === undefined
    ? HOST_CONFIG.backgroundDefault
    : TRUE_VALUES.has(backgroundValue);
  let hookQueue: Promise<void> = Promise.resolve();
  let pendingHooks = 0;
  function queueHook(request: HookRequest, record?: PhaseRecord): Promise<void> {
    if (pendingHooks >= MAX_PENDING_HOOKS) return Promise.resolve();
    const queuedAt = Date.now();
    pendingHooks += 1;
    if (record) lastQueued.set(record.root, record);
    const queued = hookQueue.then(() => {
      const remaining = HOOK_TIMEOUT_MS - (Date.now() - queuedAt);
      if (remaining <= 0) {
        if (record && lastQueued.get(record.root) === record) {
          const sent = lastSent.get(record.root);
          if (sent) lastQueued.set(record.root, { root: record.root, phase: sent });
          else lastQueued.delete(record.root);
        }
        return Promise.resolve();
      }
      if (record) lastSent.set(record.root, record.phase);
      return runHook({ ...request, cwd: directory, timeoutMs: remaining });
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

  type Send = (request: HookRequest, record?: PhaseRecord) => void;

  // A finished background child wakes its root with an injected prompt; idle until then is false.
  function sendTreePhase(root: string, send: Send, fromChild: boolean): void {
    const phase = treePhase(tree, root);
    const queued = lastQueued.get(root)?.phase;
    if (
      phase === "idle" &&
      (heldRoots.has(root) || (backgroundSubagents && fromChild && queued === "working"))
    ) {
      heldRoots.add(root);
      return;
    }
    if (queued !== phase) {
      heldRoots.delete(root);
      send({ event: PHASE_EVENT[phase], sessionId: root }, { root, phase });
    }
  }

  function handleEvent(event: unknown, send: Send): void {
    if (!isRecord(event) || typeof event.type !== "string") return;
    if (!TRACKED_EVENTS.has(event.type)) return;

    const properties = isRecord(event.properties) ? event.properties : undefined;
    const info = isRecord(properties?.info) ? properties.info : undefined;
    const sessionId = [properties?.sessionID, info?.id, event.session_id, event.sessionID]
      .find((id): id is string => typeof id === "string" && id.length > 0);

    if (event.type === "server.instance.disposed") {
      for (const id of knownSessionIds) send({ event: "session.deleted", sessionId: id });
      knownSessionIds.clear();
      tree.parentOf.clear();
      tree.busy.clear();
      tree.pendingAsks.clear();
      lastQueued.clear();
      lastSent.clear();
      heldRoots.clear();
      return;
    }

    if (!sessionId) {
      if (event.type === "session.updated" || event.type === "permission.replied") return;
      let eventName = event.type;
      if (event.type === "session.status") {
        const status = isRecord(properties?.status) ? properties.status.type : undefined;
        if (!isHostStatus(status)) return;
        eventName = `session.status.${status}`;
      }
      send({ event: eventName });
      return;
    }

    if (event.type === "session.created" || event.type === "session.updated") {
      const parentId =
        typeof info?.parentID === "string" && info.parentID.length > 0
          ? info.parentID
          : undefined;
      if (parentId) {
        tree.parentOf.set(sessionId, parentId);
        if (knownSessionIds.has(sessionId)) {
          knownSessionIds.delete(sessionId);
          lastQueued.delete(sessionId);
          lastSent.delete(sessionId);
          // The root re-claims the stray's row first, so removing the stray keeps that row.
          const lateRoot = rootOf(tree, sessionId);
          const lateTreePhase = treePhase(tree, lateRoot);
          const phase =
            lateTreePhase === "idle" && heldRoots.has(lateRoot) ? "working" : lateTreePhase;
          send({ event: PHASE_EVENT[phase], sessionId: lateRoot }, { root: lateRoot, phase });
          send({ event: "session.deleted", sessionId });
        }
      }
    }

    const root = rootOf(tree, sessionId);

    if (event.type === "session.deleted") {
      knownSessionIds.delete(sessionId);
      if (sessionId !== root) {
        forgetSession(tree, sessionId);
        sendTreePhase(root, send, false);
        return;
      }
      for (const id of descendantsOf(tree, root)) forgetSession(tree, id);
      forgetSession(tree, root);
      lastQueued.delete(root);
      lastSent.delete(root);
      heldRoots.delete(root);
      send({ event: "session.deleted", sessionId: root });
      return;
    }

    knownSessionIds.add(root);

    if (
      sessionId !== root &&
      (event.type === "session.created" || event.type === "session.compacted")
    ) {
      sendTreePhase(root, send, true);
      return;
    }

    if (event.type === "session.created") {
      send({ event: "session.created", sessionId: root }, { root, phase: "idle" });
      return;
    }

    if (event.type === "session.compacted" && HOST_CONFIG.compaction === "complete") {
      send({ event: "session.compaction.completed", sessionId: root });
      return;
    }

    if (event.type === "tui.session.select" || event.type === "session.compacted") {
      send({ event: event.type, sessionId: root });
      return;
    }

    if (event.type === "session.updated" || event.type === "permission.replied") {
      if (event.type === "permission.replied") {
        const requestId =
          typeof properties?.requestID === "string" ? properties.requestID : undefined;
        if (requestId) {
          const asks = tree.pendingAsks.get(sessionId);
          asks?.delete(requestId);
          if (asks && asks.size === 0) tree.pendingAsks.delete(sessionId);
        }
      }
      sendTreePhase(root, send, sessionId !== root);
      return;
    }

    let nativeEventName: string;
    let incomingPhase: TreePhase;
    if (event.type === "session.status") {
      const status = isRecord(properties?.status) ? properties.status.type : undefined;
      if (!isHostStatus(status)) return;
      const statusPhase = HOST_CONFIG.statuses[status];
      if (statusPhase === undefined) return;
      incomingPhase = statusPhase;
      if (incomingPhase === "idle") tree.busy.delete(sessionId);
      else tree.busy.add(sessionId);
      nativeEventName = `session.status.${status}`;
    } else if (event.type === "session.idle" || event.type === "session.error") {
      incomingPhase = "idle";
      tree.busy.delete(sessionId);
      tree.pendingAsks.delete(sessionId);
      nativeEventName = event.type;
    } else if (event.type === "permission.asked") {
      incomingPhase = "permission";
      const askId = typeof properties?.id === "string" ? properties.id : undefined;
      if (askId) {
        const asks = tree.pendingAsks.get(sessionId) ?? new Set<string>();
        asks.add(askId);
        tree.pendingAsks.set(sessionId, asks);
      }
      nativeEventName = "permission.asked";
    } else {
      return;
    }

    if (sessionId !== root) {
      sendTreePhase(root, send, true);
      return;
    }

    heldRoots.delete(root);
    const phase = treePhase(tree, root);
    if (incomingPhase === "idle" && phase !== "idle") {
      send({ event: PHASE_EVENT[phase], sessionId: root }, { root, phase });
      return;
    }
    if (incomingPhase === "working" && phase === "permission") {
      send({ event: "permission.asked", sessionId: root }, { root, phase });
      return;
    }
    send({ event: nativeEventName, sessionId: root }, { root, phase });
  }

  // Resuming a session can skip the native session.created event.
  await queueHook({ event: "session.created" });

  let disposal: Promise<void> | undefined;
  const dispose = () => {
    if (disposal) return disposal;
    const removals: HookRequest[] = [];
    handleEvent({ type: "server.instance.disposed" }, (request) => removals.push(request));
    disposal = hookQueue.then(async () => {
      const deadline = Date.now() + HOOK_TIMEOUT_MS;
      let attempted = 0;
      const sendRemovals = async () => {
        while (attempted < removals.length) {
          const remaining = deadline - Date.now();
          if (remaining <= 0) return;
          const request = removals[attempted++];
          if (!request) return;
          // eslint-disable-next-line no-await-in-loop -- Each worker owns one subprocess at a time.
          await runHook({ ...request, cwd: directory, timeoutMs: remaining });
        }
      };
      await Promise.all(Array.from({ length: Math.min(MAX_TERMINAL_HOOKS, removals.length) }, sendRemovals));
      if (Date.now() >= deadline) {
        console.warn(`hooklinesinker: disposal budget expired; ${removals.length - attempted} of ${removals.length} root deletions unattempted. Cleanup is best effort; stale records remain until process-liveness cleanup.`);
      }
    });
    return disposal;
  };

  return {
    ...(HOST_CONFIG.disposal === "callback" ? { dispose } : {}),
    event: async ({
      event,
    }: {
      event: unknown;
    }) => {
      if (HOST_CONFIG.disposal === "callback") {
        if (disposal) return disposal;
        if (isRecord(event) && event.type === "server.instance.disposed") return dispose();
      }
      const queued: Promise<void>[] = [];
      handleEvent(event, (request, record) => {
        queued.push(queueHook(request, record));
      });
      await Promise.all(queued);
    },
  };
};
