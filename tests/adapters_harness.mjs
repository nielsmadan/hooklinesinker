// Executes the bundled agent adapters (adapters/*.ts) against a stubbed host and a fake
// hooklinesinker binary, then prints what each adapter tried to ingest.
//
//   node --experimental-strip-types adapters_harness.mjs <repo-root> <workdir>
//
// Every scenario runs in this one process, sequentially. That is deliberate: the adapters kill
// a hook that outlives HOOK_TIMEOUT_MS, so a scenario racing a dozen sibling node boots would
// lose events to a timeout that has nothing to do with the behaviour under test.
//
// Prints one JSON object on stdout, keyed by scenario:
//   { "<scenario>": { invocations: [{ args, stdin }], registeredChannels,
//                     subscribedChannels, elapsedMs } }

import { mkdirSync, chmodSync, writeFileSync, readFileSync, existsSync } from "node:fs";
import { join, relative } from "node:path";
import { pathToFileURL } from "node:url";

const [repoRoot, workdir] = process.argv.slice(2);
if (!repoRoot || !workdir) {
  throw new Error("usage: adapters_harness.mjs <repo-root> <workdir>");
}

const PI_ADAPTER = join(repoRoot, "adapters/pi-hooklinesinker.ts");
const OPENCODE_ADAPTER = join(repoRoot, "adapters/opencode-hooklinesinker.ts");

// The shipped budget stays pinned by a Rust test; scenarios that are not timing the budget
// only need it to be far larger than any spawn.
const RELAXED_HOOK_TIMEOUT_MS = 60000;
const BIN_EXPRESSION = 'join(homedir(), "__HOOKLINESINKER_BIN__")';
const HOME_RELATIVE_SUFFIX = ":home-relative";

// Keep every pi scenario's session_start scan off the developer's real pi-subagents temp root.
const PI_EMPTY_ASYNC_ROOT = join(workdir, "pi-empty-async-root");
mkdirSync(PI_EMPTY_ASYNC_ROOT, { recursive: true });
process.env.PI_SUBAGENTS_TEMP_ROOT = PI_EMPTY_ASYNC_ROOT;

function timesOutOnPurpose(scenario) {
  return scenario.endsWith(":hang") || scenario.endsWith(":slow") || scenario.endsWith(":stubborn");
}

function recorderTail(scenario) {
  if (scenario.endsWith(":hang")) return "sleep 3600";
  if (scenario.endsWith(":slow")) return "sleep 0.05";
  if (scenario.endsWith(":stubborn")) return "trap '' TERM; sleep 3600";
  return "exit 0";
}

/// Stages a scenario: a fake binary that records its argv and stdin, and a copy of the adapter
/// pointed at it. A scenario suffixed with ":hang" gets a binary that records and then blocks,
/// so the adapter's own timeout and kill are what have to end the call; ":stubborn" also ignores
/// SIGTERM, so only the follow-up SIGKILL ends it and the hook outlives its deadline.
function stage(assetPath, scenario) {
  const dir = join(workdir, scenario.replaceAll(":", "-"));
  mkdirSync(dir, { recursive: true });
  const recordLog = join(dir, "invocations.jsonl");
  const pidLog = join(dir, "pids.txt");
  const binPath = join(dir, "fake-hooklinesinker");

  // Shell, not node: a second node runtime boot is slow enough to trip the adapter's own
  // HOOK_TIMEOUT_MS. `stdin` is already JSON, so it is embedded as a nested value.
  const recorder = `#!/bin/bash
printf '%s\\n' "$$" >> ${JSON.stringify(pidLog)}
IFS= read -r stdin || true
args=""
for arg in "$@"; do args="$args\${args:+,}\\"$arg\\""; done
printf '{"args":[%s],"stdin":%s}\\n' "$args" "\${stdin:-null}" >> ${JSON.stringify(recordLog)}
${recorderTail(scenario)}
`;
  writeFileSync(binPath, recorder);
  chmodSync(binPath, 0o755);

  // The adapters carry an expression the installer rewrites; do the same substitution here so
  // the shipped asset is exercised verbatim apart from the binary expression. A home-relative
  // scenario resolves the binary through `homedir()` with HOME pointed at the scenario dir.
  const source = readFileSync(assetPath, "utf8");
  if (!source.includes(BIN_EXPRESSION)) {
    throw new Error(`${assetPath} has no ${BIN_EXPRESSION} expression`);
  }
  const home = scenario.endsWith(HOME_RELATIVE_SUFFIX) ? dir : undefined;
  const binExpression =
    home === undefined
      ? JSON.stringify(binPath)
      : `join(homedir(), ${JSON.stringify(relative(home, binPath))})`;
  let staged = source.replaceAll(BIN_EXPRESSION, binExpression);

  // Only scenarios that time out on purpose are about the hook budget. Everywhere else a loaded
  // machine could let the real 2s deadline kill a spawn mid-handshake and silently drop the
  // invocation the assertion is waiting for, so give those runs room they will never use.
  if (!timesOutOnPurpose(scenario)) {
    const relaxed = staged.replace(
      /^const HOOK_TIMEOUT_MS = \d+;$/m,
      `const HOOK_TIMEOUT_MS = ${RELAXED_HOOK_TIMEOUT_MS};`,
    );
    if (relaxed === staged) {
      throw new Error(`${assetPath} has no HOOK_TIMEOUT_MS declaration to relax`);
    }
    staged = relaxed;
  }

  const adapterPath = join(dir, "adapter.ts");
  writeFileSync(adapterPath, staged);

  const invocations = () => {
    if (!existsSync(recordLog)) return [];
    return readFileSync(recordLog, "utf8")
      .split("\n")
      .filter((line) => line.length > 0)
      .map((line) => JSON.parse(line));
  };
  const pids = () => {
    if (!existsSync(pidLog)) return [];
    return readFileSync(pidLog, "utf8")
      .split("\n")
      .filter((line) => line.length > 0)
      .map((line) => Number.parseInt(line, 10));
  };
  // A distinct path per scenario means a distinct module instance, so no adapter state leaks
  // between scenarios.
  return { adapterPath, invocations, pids, home };
}

function runningPids(pids) {
  return pids.filter((pid) => {
    try {
      process.kill(pid, 0);
      return true;
    } catch {
      return false;
    }
  });
}

async function waitForPidsToExit(pids) {
  const deadline = Date.now() + 1000;
  let running = runningPids(pids);
  while (running.length > 0 && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 10));
    running = runningPids(pids);
  }
  return running;
}

// MARK: - Pi

function makePi() {
  const lifecycle = new Map();
  const events = new Map();
  const registeredChannels = [];
  const pi = {
    events: {
      on(channel, handler) {
        registeredChannels.push(channel);
        events.set(channel, handler);
        return () => events.delete(channel);
      },
    },
    on(event, handler) {
      lifecycle.set(event, handler);
    },
    sessionName: undefined,
    getSessionName() {
      return this.sessionName;
    },
  };
  return { pi, lifecycle, events, registeredChannels };
}

async function runPi(scenario, adapterPath, invocations) {
  const module = await import(pathToFileURL(adapterPath).href);
  const { pi, lifecycle, events, registeredChannels } = makePi();
  module.default(pi);

  const ctx = {
    hasUI: true,
    sessionManager: {
      getSessionId: () => "pi-parent",
      getSessionFile: () => "/sessions/pi-parent.jsonl",
    },
  };
  const start = (c = ctx, reason = "startup") => lifecycle.get("session_start")({ reason }, c);
  const shutdown = (reason, c = ctx) => lifecycle.get("session_shutdown")({ reason }, c);
  const prompt = (requestId) => events.get("permissions:ui_prompt")({ requestId });
  const decide = (requestId, resolution) =>
    events.get("permissions:decision")({ requestId, resolution });
  const settle = (c = ctx) => lifecycle.get("agent_settled")({}, c);
  const asyncStarted = (payload) => events.get("subagent:async-started")(payload);
  const asyncComplete = (payload) => events.get("subagent:async-complete")(payload);

  async function waitForInvocationCount(count, timeoutMs = RELAXED_HOOK_TIMEOUT_MS) {
    const deadline = Date.now() + timeoutMs;
    while (invocations().length < count) {
      if (Date.now() > deadline) {
        throw new Error(
          `timed out waiting for ${count} invocations (have ${invocations().length})`,
        );
      }
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
  }

  switch (scenario) {
    case "pi:session_switches":
      await start();
      for (const reason of ["new", "resume", "fork", "reload"]) {
        const next = { hasUI: true, sessionManager: { getSessionId: () => `pi-${reason}` } };
        await shutdown(reason);
        await start(next, reason);
        await lifecycle.get("agent_start")({}, next);
      }
      break;

    case "pi:start_without_reason":
    case "pi:start_without_reason:home-relative":
      await lifecycle.get("session_start")({}, ctx);
      break;

    case "pi:details_from_context": {
      pi.sessionName = "Fix login";
      const detailed = {
        ...ctx,
        model: { id: "claude-opus-4" },
        thinkingLevel: "high",
        getContextUsage: () => ({ tokens: 417, contextWindow: 1000, percent: 41.7 }),
      };
      await start(detailed);
      await lifecycle.get("agent_start")({}, detailed);
      await settle(detailed);
      break;
    }

    case "pi:details_with_throwing_getters": {
      pi.sessionName = "Fix login";
      const stale = { ...ctx };
      for (const key of ["model", "thinkingLevel"]) {
        Object.defineProperty(stale, key, {
          get() {
            throw new Error("stale");
          },
        });
      }
      stale.getContextUsage = () => {
        throw new Error("stale");
      };
      await start(stale);
      break;
    }

    case "pi:details_with_throwing_session_name": {
      pi.getSessionName = () => {
        throw new Error("stale");
      };
      await start({ ...ctx, model: { id: "claude-opus-4" }, thinkingLevel: "high" });
      break;
    }

    case "pi:held_settle_carries_details": {
      const detailed = {
        ...ctx,
        model: { id: "claude-opus-4" },
        thinkingLevel: "high",
        getContextUsage: () => ({ tokens: 417, contextWindow: 1000, percent: 41.7 }),
      };
      await start(detailed);
      await lifecycle.get("agent_start")({}, detailed);
      asyncStarted({ id: "run-1", sessionId: "/sessions/pi-parent.jsonl", pid: process.pid });
      await lifecycle.get("agent_settled")({ aborted: true }, detailed);
      break;
    }

    case "pi:null_context_percent": {
      const unknown = { ...ctx, getContextUsage: () => ({ tokens: null, contextWindow: 1000, percent: null }) };
      await start(unknown);
      break;
    }

    case "pi:session_info_changed":
      await start();
      await lifecycle.get("session_info_changed")({ name: "Renamed" }, ctx);
      await lifecycle.get("session_info_changed")({ name: undefined }, ctx);
      break;

    case "pi:model_and_effort_selected":
      await start();
      await lifecycle.get("model_select")({ model: { id: "gpt-5" } }, ctx);
      await lifecycle.get("thinking_level_select")({ level: "low" }, ctx);
      break;

    case "pi:aborted_settle":
      await start();
      await lifecycle.get("agent_settled")({ aborted: true }, ctx);
      await lifecycle.get("agent_settled")({ aborted: false }, ctx);
      break;

    case "pi:shutdown_sends_no_details": {
      const detailed = { ...ctx, model: { id: "m" }, thinkingLevel: "high" };
      await start(detailed);
      await shutdown("quit", detailed);
      break;
    }

    case "pi:permission_lifecycle":
      await start();
      prompt("prompt-1");
      decide("prompt-1", "user_denied");
      await shutdown("reload");
      break;

    case "pi:permission_after_switch": {
      await start();
      await shutdown("resume");
      const resumed = {
        hasUI: true,
        sessionManager: { getSessionId: () => "pi-resumed" },
      };
      await start(resumed, "resume");
      prompt("prompt-1");
      decide("prompt-1", "user_approved");
      await shutdown("reload", resumed);
      break;
    }

    case "pi:malformed_permission_payloads":
      await start();
      for (const event of [null, 1, "prompt", [], { requestId: 42 }]) {
        events.get("permissions:ui_prompt")(event);
      }
      prompt("prompt-1");
      for (const event of [null, 1, "decision", [], { resolution: true }]) {
        events.get("permissions:decision")(event);
      }
      decide("prompt-1", "user_denied");
      await shutdown("reload");
      break;

    case "pi:silent_and_orphan_decisions":
      await start();
      // A decision with no pending prompt, then a non-user resolution: neither resolves.
      decide("orphan", "user_approved");
      prompt("prompt-1");
      decide("prompt-1", "policy_allow");
      if (invocations().some((i) => i.args.includes("permission_resolved"))) {
        throw new Error("a silent or orphan decision resolved a pending prompt");
      }
      decide("prompt-1", "user_denied");
      await shutdown("reload");
      break;

    case "pi:settled_discards_prompts":
      await start();
      prompt("prompt-1");
      await lifecycle.get("agent_settled")({}, ctx);
      decide("prompt-1", "user_denied");
      await shutdown("reload");
      break;

    case "pi:child_session_silent": {
      const child = { hasUI: false, sessionManager: { getSessionId: () => "pi-child" } };
      await start(child);
      await lifecycle.get("agent_start")({}, child);
      await lifecycle.get("agent_settled")({}, child);
      await shutdown("quit", child);
      break;
    }

    case "pi:async_runs_hold_working_until_the_woken_parent_settles":
      await start();
      await lifecycle.get("agent_start")({}, ctx);
      asyncStarted({ id: "run-1", sessionId: "/sessions/pi-parent.jsonl", pid: process.pid });
      await settle();
      asyncComplete({ runId: "run-1", triggerTurn: true });
      await lifecycle.get("agent_start")({}, ctx);
      await settle();
      break;

    case "pi:async_run_completing_without_a_wake_settles":
      await start();
      await lifecycle.get("agent_start")({}, ctx);
      asyncStarted({ id: "run-1", sessionId: "/sessions/pi-parent.jsonl", pid: process.pid });
      await settle();
      asyncComplete({ runId: "run-1", triggerTurn: false });
      // The listener settles via a fire-and-forget hook, so wait for it to land.
      await waitForInvocationCount(4);
      break;

    case "pi:dead_async_runs_do_not_hold_working":
      await start();
      asyncStarted({ id: "run-1", sessionId: "/sessions/pi-parent.jsonl", pid: 99999999 });
      await settle();
      break;

    case "pi:async_runs_of_other_sessions_are_ignored":
      await start();
      asyncStarted({ id: "run-1", sessionId: "/sessions/someone-else.jsonl", pid: process.pid });
      await settle();
      break;

    case "pi:async_run_finishing_mid_turn_settles_normally":
      await start();
      await lifecycle.get("agent_start")({}, ctx);
      asyncStarted({ id: "run-1", sessionId: "/sessions/pi-parent.jsonl", pid: process.pid });
      asyncComplete({ runId: "run-1", triggerTurn: true });
      await settle();
      break;

    case "pi:async_runs_rebuilt_on_start": {
      const asyncRoot = join(workdir, scenario.replaceAll(":", "-"), "async-root");
      const runsDir = join(asyncRoot, "async-subagent-runs");
      const writeStatus = (runId, status) => {
        const dir = join(runsDir, runId);
        mkdirSync(dir, { recursive: true });
        writeFileSync(join(dir, "status.json"), JSON.stringify(status));
      };
      writeStatus("r1", {
        runId: "r1",
        state: "running",
        sessionId: "/sessions/pi-parent.jsonl",
        pid: process.pid,
      });
      writeStatus("r2", {
        runId: "r2",
        state: "complete",
        sessionId: "/sessions/pi-parent.jsonl",
        pid: process.pid,
      });
      writeStatus("r3", {
        runId: "r3",
        state: "running",
        sessionId: "/sessions/pi-parent.jsonl",
        pid: 99999999,
      });
      writeStatus("r4", {
        runId: "r4",
        state: "running",
        sessionId: "/sessions/someone-else.jsonl",
        pid: process.pid,
      });
      const previousTempRoot = process.env.PI_SUBAGENTS_TEMP_ROOT;
      process.env.PI_SUBAGENTS_TEMP_ROOT = asyncRoot;
      try {
        await start();
        // The scan isn't awaited by session_start, so wait for the held-working event it queues.
        await waitForInvocationCount(2);
        await settle();
        asyncComplete({ runId: "r1", triggerTurn: false });
        await waitForInvocationCount(4);
      } finally {
        if (previousTempRoot === undefined) {
          delete process.env.PI_SUBAGENTS_TEMP_ROOT;
        } else {
          process.env.PI_SUBAGENTS_TEMP_ROOT = previousTempRoot;
        }
      }
      break;
    }

    case "pi:scan_excludes_dead_pids_before_any_settle": {
      const asyncRoot = join(workdir, scenario.replaceAll(":", "-"), "async-root");
      const runsDir = join(asyncRoot, "async-subagent-runs", "r3");
      mkdirSync(runsDir, { recursive: true });
      writeFileSync(
        join(runsDir, "status.json"),
        JSON.stringify({
          runId: "r3",
          state: "running",
          sessionId: "/sessions/pi-parent.jsonl",
          pid: 99999999,
        }),
      );
      const previousTempRoot = process.env.PI_SUBAGENTS_TEMP_ROOT;
      process.env.PI_SUBAGENTS_TEMP_ROOT = asyncRoot;
      try {
        await start();
        // Let the scan finish first: only liveAsyncRun's pid check keeps a dead run from holding.
        await new Promise((resolve) => setTimeout(resolve, 500));
        await settle();
      } finally {
        if (previousTempRoot === undefined) {
          delete process.env.PI_SUBAGENTS_TEMP_ROOT;
        } else {
          process.env.PI_SUBAGENTS_TEMP_ROOT = previousTempRoot;
        }
      }
      break;
    }

    case "pi:completion_during_rebuild_is_not_resurrected": {
      const asyncRoot = join(workdir, scenario.replaceAll(":", "-"), "async-root");
      const runsDir = join(asyncRoot, "async-subagent-runs", "r1");
      mkdirSync(runsDir, { recursive: true });
      writeFileSync(
        join(runsDir, "status.json"),
        JSON.stringify({
          runId: "r1",
          state: "running",
          sessionId: "/sessions/pi-parent.jsonl",
          pid: process.pid,
        }),
      );
      const previousTempRoot = process.env.PI_SUBAGENTS_TEMP_ROOT;
      process.env.PI_SUBAGENTS_TEMP_ROOT = asyncRoot;
      try {
        await start();
        // Races the completion against the still in-flight scan on purpose.
        asyncComplete({ runId: "r1", triggerTurn: false });
        await settle();
        // Let the scan resolve; only a second settle shows whether it resurrected r1.
        await new Promise((resolve) => setTimeout(resolve, 500));
        await settle();
      } finally {
        if (previousTempRoot === undefined) {
          delete process.env.PI_SUBAGENTS_TEMP_ROOT;
        } else {
          process.env.PI_SUBAGENTS_TEMP_ROOT = previousTempRoot;
        }
      }
      break;
    }

    case "pi:shutdown_forgets_async_runs": {
      const asyncRoot = join(workdir, scenario.replaceAll(":", "-"), "async-root");
      const runsDir = join(asyncRoot, "async-subagent-runs", "run-1");
      mkdirSync(runsDir, { recursive: true });
      writeFileSync(
        join(runsDir, "status.json"),
        JSON.stringify({
          runId: "run-1",
          state: "running",
          sessionId: "/sessions/pi-parent.jsonl",
          pid: process.pid,
        }),
      );
      const previousTempRoot = process.env.PI_SUBAGENTS_TEMP_ROOT;
      process.env.PI_SUBAGENTS_TEMP_ROOT = asyncRoot;
      try {
        await start();
        // Shuts down while the on-disk scan from start() is still in flight, on purpose.
        await shutdown("quit");
        // Let the stale scan resolve; only its epoch guard stops a false "working" after shutdown.
        await new Promise((resolve) => setTimeout(resolve, 500));
      } finally {
        if (previousTempRoot === undefined) {
          delete process.env.PI_SUBAGENTS_TEMP_ROOT;
        } else {
          process.env.PI_SUBAGENTS_TEMP_ROOT = previousTempRoot;
        }
      }
      break;
    }

    case "pi:scan_skips_runs_owned_by_another_process": {
      const asyncRoot = join(workdir, scenario.replaceAll(":", "-"), "async-root");
      for (const [runId, completionOwnerId] of [["r1", "other-process"], ["r2", "this-process"]]) {
        const dir = join(asyncRoot, "async-subagent-runs", runId);
        mkdirSync(dir, { recursive: true });
        writeFileSync(
          join(dir, "status.json"),
          JSON.stringify({
            runId,
            state: "running",
            sessionId: "/sessions/pi-parent.jsonl",
            pid: process.pid,
            completionOwnerId,
          }),
        );
      }
      const ownerKey = Symbol.for("pi-subagents.completion-owner-id");
      const previousTempRoot = process.env.PI_SUBAGENTS_TEMP_ROOT;
      process.env.PI_SUBAGENTS_TEMP_ROOT = asyncRoot;
      globalThis[ownerKey] = "this-process";
      try {
        await start();
        await waitForInvocationCount(2);
        await settle();
        asyncComplete({ runId: "r2", triggerTurn: false });
        await lifecycle.get("session_before_compact")({}, ctx);
      } finally {
        delete globalThis[ownerKey];
        if (previousTempRoot === undefined) {
          delete process.env.PI_SUBAGENTS_TEMP_ROOT;
        } else {
          process.env.PI_SUBAGENTS_TEMP_ROOT = previousTempRoot;
        }
      }
      break;
    }

    case "pi:completion_without_a_wake_mid_turn_settles_once":
      await start();
      await lifecycle.get("agent_start")({}, ctx);
      asyncStarted({ id: "run-1", sessionId: "/sessions/pi-parent.jsonl", pid: process.pid });
      asyncComplete({ runId: "run-1", triggerTurn: false });
      await settle();
      break;

    case "pi:one_of_two_runs_completing_keeps_the_hold":
      await start();
      await lifecycle.get("agent_start")({}, ctx);
      asyncStarted({ id: "run-1", sessionId: "/sessions/pi-parent.jsonl", pid: process.pid });
      asyncStarted({ id: "run-2", sessionId: "/sessions/pi-parent.jsonl", pid: process.pid });
      await settle();
      asyncComplete({ runId: "run-1", triggerTurn: false });
      prompt("marker");
      asyncComplete({ runId: "run-2", triggerTurn: false });
      await lifecycle.get("session_before_compact")({}, ctx);
      break;

    case "pi:unknown_completion_keeps_the_hold":
      await start();
      await lifecycle.get("agent_start")({}, ctx);
      asyncStarted({ id: "run-1", sessionId: "/sessions/pi-parent.jsonl", pid: process.pid });
      await settle();
      asyncComplete({ runId: "run-1", triggerTurn: true });
      asyncComplete({ runId: "run-9", triggerTurn: false });
      prompt("marker");
      await lifecycle.get("agent_start")({}, ctx);
      await settle();
      break;

    case "pi:manual_compaction_with_live_runs_holds_working":
      await start();
      await lifecycle.get("agent_start")({}, ctx);
      asyncStarted({ id: "run-1", sessionId: "/sessions/pi-parent.jsonl", pid: process.pid });
      await lifecycle.get("session_compact")({ reason: "manual" }, ctx);
      break;

    case "pi:shutdown_quit":
    case "pi:shutdown_reload":
    case "pi:shutdown_new":
    case "pi:shutdown_resume":
    case "pi:shutdown_fork":
      await start();
      await shutdown(scenario.slice("pi:shutdown_".length));
      break;

    case "pi:hanging_binary:hang":
      await start();
      await shutdown("reload");
      break;

    case "pi:queue_bound:slow":
    case "pi:queue_bound":
      await start();
      for (let index = 0; index < 100; index += 1) {
        prompt(`prompt-${index}`);
      }
      await shutdown("reload");
      break;

    default:
      throw new Error(`unknown pi scenario ${scenario}`);
  }

  return { registeredChannels, subscribedChannels: [...events.keys()] };
}

// MARK: - OpenCode

const BACKGROUND_SUFFIX = ":background";
const EXPERIMENTAL_SUFFIX = ":experimental";

const holdTheRoot = [
  { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
  { type: "session.created", properties: { sessionID: "C", info: { id: "C", parentID: "R" } } },
  { type: "session.status", properties: { sessionID: "C", status: { type: "busy" } } },
  { type: "session.status", properties: { sessionID: "C", status: { type: "idle" } } },
];

async function runOpenCode(scenario, adapterPath) {
  const module = await import(pathToFileURL(adapterPath).href);
  const background = scenario.endsWith(BACKGROUND_SUFFIX);
  const experimental = scenario.endsWith(EXPERIMENTAL_SUFFIX);
  delete process.env.OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS;
  delete process.env.OPENCODE_EXPERIMENTAL;
  if (background) process.env.OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS = "1";
  if (experimental) process.env.OPENCODE_EXPERIMENTAL = "true";
  const plugin = await module.HooklinesinkerPlugin({
    project: {},
    client: {},
    $: {},
    directory: "/work/repo",
    worktree: "/work/repo",
  });
  delete process.env.OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS;
  delete process.env.OPENCODE_EXPERIMENTAL;

  const suffix = background ? BACKGROUND_SUFFIX : experimental ? EXPERIMENTAL_SUFFIX : "";
  switch (scenario.slice(0, scenario.length - suffix.length)) {
    case "opencode:selection_and_status_are_serialized":
      await Promise.all([
        plugin.event({ event: { type: "tui.session.select", properties: { sessionID: "a" } } }),
        plugin.event({ event: { type: "session.status", properties: { sessionID: "a", status: { type: "busy" } } } }),
        plugin.event({ event: { type: "tui.session.select", properties: { sessionID: "b" } } }),
        plugin.event({ event: { type: "session.deleted", properties: { info: { id: "a" } } } }),
      ]);
      break;

    case "opencode:instance_disposal_removes_known_sessions":
      await plugin.event({
        event: { type: "session.idle", properties: { sessionID: "a" } },
      });
      await plugin.event({
        event: { type: "session.idle", properties: { sessionID: "b" } },
      });
      await plugin.event({
        event: { type: "session.deleted", properties: { sessionID: "a" } },
      });
      await plugin.event({ event: { type: "server.instance.disposed" } });
      break;

    case "opencode:load_posts_created":
    case "opencode:load_posts_created:home-relative":
    case "opencode:hanging_binary:hang":
      break;

    case "opencode:queue_bound:slow":
    case "opencode:queue_bound":
      await Promise.all(
        Array.from({ length: 100 }, (_, index) =>
          plugin.event({
            event: {
              type: "session.idle",
              properties: { sessionID: `oc-${index}` },
            },
          }),
        ),
      );
      break;

    case "opencode:status_becomes_event_suffix":
      await plugin.event({
        event: {
          type: "session.status",
          properties: { sessionID: "oc-1", status: { type: "busy" } },
        },
      });
      break;

    case "opencode:status_without_type_is_dropped":
      await plugin.event({
        event: { type: "session.status", properties: { sessionID: "oc-1" } },
      });
      break;

    case "opencode:untracked_event_is_dropped":
      await plugin.event({
        event: { type: "message.updated", properties: { sessionID: "oc-1" } },
      });
      break;

    case "opencode:malformed_events_are_dropped":
      for (const event of [
        null,
        42,
        "session.status",
        [],
        { type: 42 },
        { type: "session.status", properties: null },
        { type: "session.status", properties: { status: { type: 42 } } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:session_id_fallbacks":
      for (const event of [
        { type: "session.created", properties: { sessionID: 42, info: { id: "from-info" } } },
        { type: "session.idle", properties: { info: [] }, session_id: "from-snake" },
        { type: "session.deleted", properties: null, sessionID: "from-camel" },
        { type: "session.idle", properties: { sessionID: 42 } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:child_sessions_report_through_the_root":
      for (const event of [
        { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        {
          type: "session.created",
          properties: { sessionID: "C", info: { id: "C", parentID: "R" } },
        },
        { type: "session.status", properties: { sessionID: "R", status: { type: "busy" } } },
        { type: "session.status", properties: { sessionID: "C", status: { type: "busy" } } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "idle" } } },
        { type: "session.status", properties: { sessionID: "C", status: { type: "idle" } } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "busy" } } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "idle" } } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:child_permission_surfaces_on_the_root":
      for (const event of [
        { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        {
          type: "session.created",
          properties: { sessionID: "C", info: { id: "C", parentID: "R" } },
        },
        { type: "session.status", properties: { sessionID: "C", status: { type: "busy" } } },
        { type: "permission.asked", properties: { id: "p1", sessionID: "C" } },
        { type: "permission.replied", properties: { sessionID: "C", requestID: "p1" } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:root_permission_reply_resumes_working":
      for (const event of [
        { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "busy" } } },
        { type: "permission.asked", properties: { id: "p1", sessionID: "R" } },
        { type: "permission.replied", properties: { sessionID: "R", requestID: "p1" } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:grandchildren_resolve_to_the_root":
      for (const event of [
        { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        {
          type: "session.created",
          properties: { sessionID: "C", info: { id: "C", parentID: "R" } },
        },
        {
          type: "session.created",
          properties: { sessionID: "G", info: { id: "G", parentID: "C" } },
        },
        { type: "session.status", properties: { sessionID: "G", status: { type: "busy" } } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:child_learned_late_loses_its_row":
      for (const event of [
        { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        { type: "session.status", properties: { sessionID: "X", status: { type: "busy" } } },
        {
          type: "session.created",
          properties: { sessionID: "X", info: { id: "X", parentID: "R" } },
        },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:deleting_a_busy_child_releases_its_root":
      for (const event of [
        { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        {
          type: "session.created",
          properties: { sessionID: "C", info: { id: "C", parentID: "R" } },
        },
        { type: "session.status", properties: { sessionID: "C", status: { type: "busy" } } },
        { type: "session.deleted", properties: { sessionID: "C", info: { id: "C" } } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:disposal_removes_only_roots":
      await plugin.event({
        event: { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
      });
      await plugin.event({
        event: {
          type: "session.created",
          properties: { sessionID: "C", info: { id: "C", parentID: "R" } },
        },
      });
      await plugin.event({ event: { type: "server.instance.disposed" } });
      break;

    case "opencode:child_compaction_keeps_the_root_phase":
      for (const event of [
        { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        {
          type: "session.created",
          properties: { sessionID: "C", info: { id: "C", parentID: "R" } },
        },
        { type: "session.status", properties: { sessionID: "C", status: { type: "busy" } } },
        { type: "session.compacted", properties: { sessionID: "C" } },
        { type: "session.status", properties: { sessionID: "C", status: { type: "idle" } } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "busy" } } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "idle" } } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:child_learned_late_through_an_update":
      for (const event of [
        { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        { type: "session.status", properties: { sessionID: "X", status: { type: "busy" } } },
        {
          type: "session.updated",
          properties: { sessionID: "X", info: { id: "X", parentID: "R" } },
        },
        { type: "session.updated", properties: { sessionID: "R", info: { id: "R" } } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:root_deletion_forgets_its_descendants":
      for (const event of [
        { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        {
          type: "session.created",
          properties: { sessionID: "C", info: { id: "C", parentID: "R" } },
        },
        { type: "session.status", properties: { sessionID: "C", status: { type: "busy" } } },
        { type: "session.deleted", properties: { sessionID: "R", info: { id: "R" } } },
        { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "idle" } } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:sessionless_tree_events_are_dropped":
      for (const event of [
        { type: "session.updated", properties: { info: {} } },
        { type: "permission.replied", properties: { requestID: "p1" } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:overlapping_child_events_settle_on_the_tree_phase": {
      const ask = (id) => ({
        event: { type: "permission.asked", properties: { id, sessionID: "C" } },
      });
      const reply = (id) => ({
        event: { type: "permission.replied", properties: { sessionID: "C", requestID: id } },
      });
      await plugin.event({
        event: { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
      });
      await plugin.event({
        event: {
          type: "session.created",
          properties: { sessionID: "C", info: { id: "C", parentID: "R" } },
        },
      });
      await plugin.event({
        event: { type: "session.status", properties: { sessionID: "C", status: { type: "busy" } } },
      });
      // Dispatched together like the host, so each event is handled before any earlier hook runs.
      await Promise.all([ask("p1"), reply("p1")].map((event) => plugin.event(event)));
      await Promise.all([ask("p2"), reply("p2"), ask("p3")].map((event) => plugin.event(event)));
      break;
    }

    case "opencode:background_child_completion_holds_the_root_until_its_wake":
      for (const event of [
        { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        {
          type: "session.created",
          properties: { sessionID: "C", info: { id: "C", parentID: "R" } },
        },
        { type: "session.status", properties: { sessionID: "R", status: { type: "busy" } } },
        { type: "session.status", properties: { sessionID: "C", status: { type: "busy" } } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "idle" } } },
        { type: "session.status", properties: { sessionID: "C", status: { type: "idle" } } },
        { type: "session.idle", properties: { sessionID: "C" } },
        { type: "session.updated", properties: { sessionID: "R", info: { id: "R" } } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "busy" } } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "idle" } } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:the_roots_own_event_releases_its_hold":
      for (const event of [
        { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        {
          type: "session.created",
          properties: { sessionID: "C", info: { id: "C", parentID: "R" } },
        },
        { type: "session.status", properties: { sessionID: "C", status: { type: "busy" } } },
        { type: "session.status", properties: { sessionID: "C", status: { type: "idle" } } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "busy" } } },
        {
          type: "session.created",
          properties: { sessionID: "D", info: { id: "D", parentID: "R" } },
        },
        { type: "session.status", properties: { sessionID: "D", status: { type: "busy" } } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "idle" } } },
        { type: "session.deleted", properties: { sessionID: "D", info: { id: "D" } } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:foreground_child_completion_under_a_busy_root":
      for (const event of [
        { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        {
          type: "session.created",
          properties: { sessionID: "C", info: { id: "C", parentID: "R" } },
        },
        { type: "session.status", properties: { sessionID: "R", status: { type: "busy" } } },
        { type: "session.status", properties: { sessionID: "C", status: { type: "busy" } } },
        { type: "session.status", properties: { sessionID: "C", status: { type: "idle" } } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "idle" } } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:deleting_a_child_keeps_a_held_root_held":
      for (const event of [
        ...holdTheRoot,
        { type: "session.deleted", properties: { sessionID: "C", info: { id: "C" } } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "busy" } } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "idle" } } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:child_permission_interrupts_a_hold":
      for (const event of [
        ...holdTheRoot,
        {
          type: "session.created",
          properties: { sessionID: "D", info: { id: "D", parentID: "R" } },
        },
        { type: "session.status", properties: { sessionID: "D", status: { type: "busy" } } },
        { type: "permission.asked", properties: { id: "p1", sessionID: "D" } },
        { type: "permission.replied", properties: { sessionID: "D", requestID: "p1" } },
        { type: "session.status", properties: { sessionID: "D", status: { type: "idle" } } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "busy" } } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "idle" } } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:the_roots_own_idle_and_error_release_its_hold":
      for (const event of [
        ...holdTheRoot,
        { type: "session.idle", properties: { sessionID: "R" } },
        {
          type: "session.created",
          properties: { sessionID: "D", info: { id: "D", parentID: "R" } },
        },
        { type: "session.status", properties: { sessionID: "D", status: { type: "busy" } } },
        { type: "session.status", properties: { sessionID: "D", status: { type: "idle" } } },
        { type: "session.error", properties: { sessionID: "R" } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:root_deletion_drops_its_hold":
      for (const event of [
        ...holdTheRoot,
        { type: "session.deleted", properties: { sessionID: "R", info: { id: "R" } } },
        { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        { type: "session.status", properties: { sessionID: "X", status: { type: "busy" } } },
        { type: "session.status", properties: { sessionID: "X", status: { type: "idle" } } },
        {
          type: "session.updated",
          properties: { sessionID: "X", info: { id: "X", parentID: "R" } },
        },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:late_link_under_a_held_root_keeps_it_working":
      for (const event of [
        ...holdTheRoot,
        { type: "session.status", properties: { sessionID: "X", status: { type: "busy" } } },
        { type: "session.status", properties: { sessionID: "X", status: { type: "idle" } } },
        {
          type: "session.updated",
          properties: { sessionID: "X", info: { id: "X", parentID: "R" } },
        },
        { type: "session.status", properties: { sessionID: "R", status: { type: "busy" } } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "idle" } } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:phase_dropped_by_the_queue_bound_is_resent":
      await plugin.event({
        event: { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
      });
      await Promise.all([
        ...Array.from({ length: 32 }, (_, index) =>
          plugin.event({
            event: { type: "session.idle", properties: { sessionID: `oc-${index}` } },
          }),
        ),
        plugin.event({
          event: { type: "session.status", properties: { sessionID: "R", status: { type: "busy" } } },
        }),
      ]);
      await plugin.event({
        event: {
          type: "session.created",
          properties: { sessionID: "C", info: { id: "C", parentID: "R" } },
        },
      });
      break;

    case "opencode:phase_dropped_by_the_deadline_is_resent:stubborn":
      await Promise.all([
        plugin.event({
          event: { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        }),
        plugin.event({
          event: { type: "session.status", properties: { sessionID: "R", status: { type: "busy" } } },
        }),
      ]);
      await plugin.event({
        event: {
          type: "session.created",
          properties: { sessionID: "C", info: { id: "C", parentID: "R" } },
        },
      });
      break;

    case "opencode:late_link_under_a_busy_root_reclaims_the_row_first":
      for (const event of [
        { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "busy" } } },
        { type: "session.status", properties: { sessionID: "X", status: { type: "busy" } } },
        {
          type: "session.updated",
          properties: { sessionID: "X", info: { id: "X", parentID: "R" } },
        },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:child_selection_selects_the_root":
      for (const event of [
        { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        {
          type: "session.created",
          properties: { sessionID: "C", info: { id: "C", parentID: "R" } },
        },
        { type: "tui.session.select", properties: { sessionID: "C" } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:root_busy_under_a_pending_child_ask_stays_permission":
      for (const event of [
        { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        {
          type: "session.created",
          properties: { sessionID: "C", info: { id: "C", parentID: "R" } },
        },
        { type: "permission.asked", properties: { id: "p1", sessionID: "C" } },
        { type: "session.status", properties: { sessionID: "R", status: { type: "busy" } } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:each_pending_ask_is_tracked":
      for (const event of [
        { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        {
          type: "session.created",
          properties: { sessionID: "C", info: { id: "C", parentID: "R" } },
        },
        { type: "session.status", properties: { sessionID: "C", status: { type: "busy" } } },
        { type: "permission.asked", properties: { id: "p1", sessionID: "C" } },
        { type: "permission.asked", properties: { id: "p2", sessionID: "C" } },
        { type: "permission.replied", properties: { sessionID: "C", requestID: "p2" } },
        { type: "tui.session.select", properties: { sessionID: "R" } },
        { type: "permission.replied", properties: { sessionID: "C", requestID: "p1" } },
      ]) {
        await plugin.event({ event });
      }
      break;

    case "opencode:child_idle_clears_its_asks":
      for (const event of [
        { type: "session.created", properties: { sessionID: "R", info: { id: "R" } } },
        {
          type: "session.created",
          properties: { sessionID: "C", info: { id: "C", parentID: "R" } },
        },
        { type: "session.status", properties: { sessionID: "C", status: { type: "busy" } } },
        { type: "permission.asked", properties: { id: "p1", sessionID: "C" } },
        { type: "session.idle", properties: { sessionID: "C" } },
      ]) {
        await plugin.event({ event });
      }
      break;

    default:
      throw new Error(`unknown opencode scenario ${scenario}`);
  }

  return { registeredChannels: [], subscribedChannels: [] };
}

const SCENARIOS = [
  [PI_ADAPTER, "pi:session_switches"],
  [PI_ADAPTER, "pi:start_without_reason"],
  [PI_ADAPTER, "pi:start_without_reason:home-relative"],
  [PI_ADAPTER, "pi:details_from_context"],
  [PI_ADAPTER, "pi:details_with_throwing_getters"],
  [PI_ADAPTER, "pi:details_with_throwing_session_name"],
  [PI_ADAPTER, "pi:held_settle_carries_details"],
  [PI_ADAPTER, "pi:null_context_percent"],
  [PI_ADAPTER, "pi:session_info_changed"],
  [PI_ADAPTER, "pi:model_and_effort_selected"],
  [PI_ADAPTER, "pi:aborted_settle"],
  [PI_ADAPTER, "pi:shutdown_sends_no_details"],
  [PI_ADAPTER, "pi:permission_lifecycle"],
  [PI_ADAPTER, "pi:permission_after_switch"],
  [PI_ADAPTER, "pi:malformed_permission_payloads"],
  [PI_ADAPTER, "pi:silent_and_orphan_decisions"],
  [PI_ADAPTER, "pi:settled_discards_prompts"],
  [PI_ADAPTER, "pi:child_session_silent"],
  [PI_ADAPTER, "pi:async_runs_hold_working_until_the_woken_parent_settles"],
  [PI_ADAPTER, "pi:async_run_completing_without_a_wake_settles"],
  [PI_ADAPTER, "pi:dead_async_runs_do_not_hold_working"],
  [PI_ADAPTER, "pi:async_runs_of_other_sessions_are_ignored"],
  [PI_ADAPTER, "pi:async_run_finishing_mid_turn_settles_normally"],
  [PI_ADAPTER, "pi:async_runs_rebuilt_on_start"],
  [PI_ADAPTER, "pi:scan_excludes_dead_pids_before_any_settle"],
  [PI_ADAPTER, "pi:completion_during_rebuild_is_not_resurrected"],
  [PI_ADAPTER, "pi:shutdown_forgets_async_runs"],
  [PI_ADAPTER, "pi:scan_skips_runs_owned_by_another_process"],
  [PI_ADAPTER, "pi:completion_without_a_wake_mid_turn_settles_once"],
  [PI_ADAPTER, "pi:one_of_two_runs_completing_keeps_the_hold"],
  [PI_ADAPTER, "pi:unknown_completion_keeps_the_hold"],
  [PI_ADAPTER, "pi:manual_compaction_with_live_runs_holds_working"],
  [PI_ADAPTER, "pi:shutdown_quit"],
  [PI_ADAPTER, "pi:shutdown_reload"],
  [PI_ADAPTER, "pi:shutdown_new"],
  [PI_ADAPTER, "pi:shutdown_resume"],
  [PI_ADAPTER, "pi:shutdown_fork"],
  [PI_ADAPTER, "pi:hanging_binary:hang"],
  [PI_ADAPTER, "pi:queue_bound:slow"],
  [PI_ADAPTER, "pi:queue_bound"],
  [OPENCODE_ADAPTER, "opencode:load_posts_created"],
  [OPENCODE_ADAPTER, "opencode:load_posts_created:home-relative"],
  [OPENCODE_ADAPTER, "opencode:selection_and_status_are_serialized"],
  [OPENCODE_ADAPTER, "opencode:instance_disposal_removes_known_sessions"],
  [OPENCODE_ADAPTER, "opencode:status_becomes_event_suffix"],
  [OPENCODE_ADAPTER, "opencode:status_without_type_is_dropped"],
  [OPENCODE_ADAPTER, "opencode:untracked_event_is_dropped"],
  [OPENCODE_ADAPTER, "opencode:malformed_events_are_dropped"],
  [OPENCODE_ADAPTER, "opencode:session_id_fallbacks"],
  [OPENCODE_ADAPTER, "opencode:child_sessions_report_through_the_root"],
  [OPENCODE_ADAPTER, "opencode:child_sessions_report_through_the_root:background"],
  [OPENCODE_ADAPTER, "opencode:child_permission_surfaces_on_the_root"],
  [OPENCODE_ADAPTER, "opencode:root_permission_reply_resumes_working"],
  [OPENCODE_ADAPTER, "opencode:grandchildren_resolve_to_the_root"],
  [OPENCODE_ADAPTER, "opencode:child_learned_late_loses_its_row"],
  [OPENCODE_ADAPTER, "opencode:deleting_a_busy_child_releases_its_root"],
  [OPENCODE_ADAPTER, "opencode:disposal_removes_only_roots"],
  [OPENCODE_ADAPTER, "opencode:child_compaction_keeps_the_root_phase"],
  [OPENCODE_ADAPTER, "opencode:child_compaction_keeps_the_root_phase:background"],
  [OPENCODE_ADAPTER, "opencode:child_learned_late_through_an_update"],
  [OPENCODE_ADAPTER, "opencode:root_deletion_forgets_its_descendants"],
  [OPENCODE_ADAPTER, "opencode:sessionless_tree_events_are_dropped"],
  [OPENCODE_ADAPTER, "opencode:overlapping_child_events_settle_on_the_tree_phase"],
  [OPENCODE_ADAPTER, "opencode:background_child_completion_holds_the_root_until_its_wake:background"],
  [OPENCODE_ADAPTER, "opencode:background_child_completion_holds_the_root_until_its_wake:experimental"],
  [OPENCODE_ADAPTER, "opencode:foreground_child_completion_under_a_busy_root"],
  [OPENCODE_ADAPTER, "opencode:the_roots_own_event_releases_its_hold:background"],
  [OPENCODE_ADAPTER, "opencode:deleting_a_child_keeps_a_held_root_held:background"],
  [OPENCODE_ADAPTER, "opencode:child_permission_interrupts_a_hold:background"],
  [OPENCODE_ADAPTER, "opencode:the_roots_own_idle_and_error_release_its_hold:background"],
  [OPENCODE_ADAPTER, "opencode:root_deletion_drops_its_hold:background"],
  [OPENCODE_ADAPTER, "opencode:late_link_under_a_held_root_keeps_it_working:background"],
  [OPENCODE_ADAPTER, "opencode:phase_dropped_by_the_queue_bound_is_resent"],
  [OPENCODE_ADAPTER, "opencode:phase_dropped_by_the_deadline_is_resent:stubborn"],
  [OPENCODE_ADAPTER, "opencode:late_link_under_a_busy_root_reclaims_the_row_first"],
  [OPENCODE_ADAPTER, "opencode:child_selection_selects_the_root"],
  [OPENCODE_ADAPTER, "opencode:root_busy_under_a_pending_child_ask_stays_permission"],
  [OPENCODE_ADAPTER, "opencode:each_pending_ask_is_tracked"],
  [OPENCODE_ADAPTER, "opencode:child_idle_clears_its_asks"],
  [OPENCODE_ADAPTER, "opencode:hanging_binary:hang"],
  [OPENCODE_ADAPTER, "opencode:queue_bound:slow"],
  [OPENCODE_ADAPTER, "opencode:queue_bound"],
];

const report = {};
for (const [assetPath, scenario] of SCENARIOS) {
  const watchdog = setTimeout(() => {
    process.stderr.write(`harness watchdog fired: ${scenario}\n`);
    process.exit(3);
  }, 60_000);
  const { adapterPath, invocations, pids, home } = stage(assetPath, scenario);
  const started = Date.now();
  const previousHome = process.env.HOME;
  if (home !== undefined) process.env.HOME = home;
  const result = scenario.startsWith("pi:")
    ? await runPi(scenario, adapterPath, invocations)
    : await runOpenCode(scenario, adapterPath);
  process.env.HOME = previousHome;
  const runningPids = await waitForPidsToExit(pids());
  report[scenario] = {
    invocations: invocations(),
    runningPids,
    elapsedMs: Date.now() - started,
    ...result,
  };
  clearTimeout(watchdog);
}

// A hung fake binary leaves its pipes open; the recorded results are already complete.
process.stdout.write(JSON.stringify(report), () => process.exit(0));
