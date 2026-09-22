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
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const [repoRoot, workdir] = process.argv.slice(2);
if (!repoRoot || !workdir) {
  throw new Error("usage: adapters_harness.mjs <repo-root> <workdir>");
}

// An adapter whose hook never resolves would otherwise hang the caller forever; fail loudly
// instead. The explicit process.exit(0) below wins in every passing run.
setTimeout(() => {
  process.stderr.write("harness watchdog fired\n");
  process.exit(3);
}, 60_000);

const PI_ADAPTER = join(repoRoot, "adapters/pi-hooklinesinker.ts");
const OPENCODE_ADAPTER = join(repoRoot, "adapters/opencode-hooklinesinker.ts");

// The shipped budget stays pinned by a Rust test; scenarios that are not timing the budget
// only need it to be far larger than any spawn.
const RELAXED_HOOK_TIMEOUT_MS = 60000;

function timesOutOnPurpose(scenario) {
  return scenario.endsWith(":hang") || scenario.endsWith(":slow");
}

/// Stages a scenario: a fake binary that records its argv and stdin, and a copy of the adapter
/// pointed at it. A scenario suffixed with ":hang" gets a binary that records and then blocks,
/// so the adapter's own timeout and kill are what have to end the call.
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
${scenario.endsWith(":hang") ? "sleep 3600" : scenario.endsWith(":slow") ? "sleep 0.05" : "exit 0"}
`;
  writeFileSync(binPath, recorder);
  chmodSync(binPath, 0o755);

  // The adapters carry a placeholder the installer rewrites; do the same substitution here so
  // the shipped asset is exercised verbatim apart from the binary path.
  const source = readFileSync(assetPath, "utf8");
  if (!source.includes("__HOOKLINESINKER_BIN__")) {
    throw new Error(`${assetPath} has no __HOOKLINESINKER_BIN__ placeholder`);
  }
  let staged = source.replaceAll("__HOOKLINESINKER_BIN__", binPath);

  // Only the :hang and :slow scenarios are about the hook budget. Everywhere else a loaded
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
  return { adapterPath, invocations, pids };
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
  };
  return { pi, lifecycle, events, registeredChannels };
}

async function runPi(scenario, adapterPath, invocations) {
  const module = await import(pathToFileURL(adapterPath).href);
  const { pi, lifecycle, events, registeredChannels } = makePi();
  module.default(pi);

  const ctx = { hasUI: true, sessionManager: { getSessionId: () => "pi-parent" } };
  const start = (c = ctx, reason = "startup") => lifecycle.get("session_start")({ reason }, c);
  const shutdown = (reason, c = ctx) => lifecycle.get("session_shutdown")({ reason }, c);
  const prompt = (requestId) => events.get("permissions:ui_prompt")({ requestId });
  const decide = (requestId, resolution) =>
    events.get("permissions:decision")({ requestId, resolution });

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
      await lifecycle.get("session_start")({}, ctx);
      break;

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

async function runOpenCode(scenario, adapterPath) {
  const module = await import(pathToFileURL(adapterPath).href);
  const plugin = await module.HooklinesinkerPlugin({
    project: {},
    client: {},
    $: {},
    directory: "/work/repo",
    worktree: "/work/repo",
  });

  switch (scenario) {
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

    default:
      throw new Error(`unknown opencode scenario ${scenario}`);
  }

  return { registeredChannels: [], subscribedChannels: [] };
}

const SCENARIOS = [
  [PI_ADAPTER, "pi:session_switches"],
  [PI_ADAPTER, "pi:start_without_reason"],
  [PI_ADAPTER, "pi:permission_lifecycle"],
  [PI_ADAPTER, "pi:permission_after_switch"],
  [PI_ADAPTER, "pi:malformed_permission_payloads"],
  [PI_ADAPTER, "pi:silent_and_orphan_decisions"],
  [PI_ADAPTER, "pi:settled_discards_prompts"],
  [PI_ADAPTER, "pi:child_session_silent"],
  [PI_ADAPTER, "pi:shutdown_quit"],
  [PI_ADAPTER, "pi:shutdown_reload"],
  [PI_ADAPTER, "pi:shutdown_new"],
  [PI_ADAPTER, "pi:shutdown_resume"],
  [PI_ADAPTER, "pi:shutdown_fork"],
  [PI_ADAPTER, "pi:hanging_binary:hang"],
  [PI_ADAPTER, "pi:queue_bound:slow"],
  [PI_ADAPTER, "pi:queue_bound"],
  [OPENCODE_ADAPTER, "opencode:load_posts_created"],
  [OPENCODE_ADAPTER, "opencode:selection_and_status_are_serialized"],
  [OPENCODE_ADAPTER, "opencode:instance_disposal_removes_known_sessions"],
  [OPENCODE_ADAPTER, "opencode:status_becomes_event_suffix"],
  [OPENCODE_ADAPTER, "opencode:status_without_type_is_dropped"],
  [OPENCODE_ADAPTER, "opencode:untracked_event_is_dropped"],
  [OPENCODE_ADAPTER, "opencode:malformed_events_are_dropped"],
  [OPENCODE_ADAPTER, "opencode:session_id_fallbacks"],
  [OPENCODE_ADAPTER, "opencode:hanging_binary:hang"],
  [OPENCODE_ADAPTER, "opencode:queue_bound:slow"],
  [OPENCODE_ADAPTER, "opencode:queue_bound"],
];

const report = {};
for (const [assetPath, scenario] of SCENARIOS) {
  const { adapterPath, invocations, pids } = stage(assetPath, scenario);
  const started = Date.now();
  const result = scenario.startsWith("pi:")
    ? await runPi(scenario, adapterPath, invocations)
    : await runOpenCode(scenario, adapterPath);
  const runningPids = await waitForPidsToExit(pids());
  report[scenario] = {
    invocations: invocations(),
    runningPids,
    elapsedMs: Date.now() - started,
    ...result,
  };
}

process.stdout.write(JSON.stringify(report));
// A hung fake binary leaves its pipes open; the recorded results are already complete.
process.exit(0);
