// Executes the bundled agent adapters (assets/*.ts) against a stubbed host and a fake
// hooklinesinker binary, then prints what each adapter tried to ingest.
//
//   node --experimental-strip-types assets_harness.mjs <repo-root> <workdir>
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
  throw new Error("usage: assets_harness.mjs <repo-root> <workdir>");
}

// An adapter whose hook never resolves would otherwise hang the caller forever; fail loudly
// instead. The explicit process.exit(0) below wins in every passing run.
setTimeout(() => {
  process.stderr.write("harness watchdog fired\n");
  process.exit(3);
}, 60_000);

const PI_ASSET = join(repoRoot, "assets/pi-hooklinesinker.ts");
const OPENCODE_ASSET = join(repoRoot, "assets/opencode-hooklinesinker.ts");

/// Stages a scenario: a fake binary that records its argv and stdin, and a copy of the adapter
/// pointed at it. A scenario suffixed with ":hang" gets a binary that records and then blocks,
/// so the adapter's own timeout and kill are what have to end the call.
function stage(assetPath, scenario) {
  const dir = join(workdir, scenario.replaceAll(":", "-"));
  mkdirSync(dir, { recursive: true });
  const recordLog = join(dir, "invocations.jsonl");
  const binPath = join(dir, "fake-hooklinesinker");

  // Shell, not node: a second node runtime boot is slow enough to trip the adapter's own
  // HOOK_TIMEOUT_MS. `stdin` is already JSON, so it is embedded as a nested value.
  const recorder = `#!/bin/bash
stdin=$(cat)
args=""
for arg in "$@"; do args="$args\${args:+,}\\"$arg\\""; done
printf '{"args":[%s],"stdin":%s}\\n' "$args" "\${stdin:-null}" >> ${JSON.stringify(recordLog)}
${scenario.endsWith(":hang") ? "sleep 3600" : "exit 0"}
`;
  writeFileSync(binPath, recorder);
  chmodSync(binPath, 0o755);

  // The adapters carry a placeholder the installer rewrites; do the same substitution here so
  // the shipped asset is exercised verbatim apart from the binary path.
  const source = readFileSync(assetPath, "utf8");
  if (!source.includes("__HOOKLINESINKER_BIN__")) {
    throw new Error(`${assetPath} has no __HOOKLINESINKER_BIN__ placeholder`);
  }
  const adapterPath = join(dir, "adapter.ts");
  writeFileSync(adapterPath, source.replaceAll("__HOOKLINESINKER_BIN__", binPath));

  const invocations = () => {
    if (!existsSync(recordLog)) return [];
    return readFileSync(recordLog, "utf8")
      .split("\n")
      .filter((line) => line.length > 0)
      .map((line) => JSON.parse(line));
  };
  // A distinct path per scenario means a distinct module instance, so no adapter state leaks
  // between scenarios.
  return { adapterPath, invocations };
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
  const start = (c = ctx) => lifecycle.get("session_start")({}, c);
  const shutdown = (reason, c = ctx) => lifecycle.get("session_shutdown")({ reason }, c);
  const prompt = (requestId) => events.get("permissions:ui_prompt")({ requestId });
  const decide = (resolution) => events.get("permissions:decision")({ resolution });

  switch (scenario) {
    case "pi:permission_lifecycle":
      await start();
      prompt("prompt-1");
      decide("user_denied");
      await shutdown("reload");
      break;

    case "pi:silent_and_orphan_decisions":
      await start();
      // A decision with no pending prompt, then a non-user resolution: neither resolves.
      decide("user_approved");
      prompt("prompt-1");
      decide("policy_allow");
      if (invocations().some((i) => i.args.includes("permission_resolved"))) {
        throw new Error("a silent or orphan decision resolved a pending prompt");
      }
      decide("user_denied");
      await shutdown("reload");
      break;

    case "pi:settled_discards_prompts":
      await start();
      prompt("prompt-1");
      await lifecycle.get("agent_settled")({}, ctx);
      decide("user_denied");
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
    case "opencode:load_posts_created":
    case "opencode:hanging_binary:hang":
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

    default:
      throw new Error(`unknown opencode scenario ${scenario}`);
  }

  return { registeredChannels: [], subscribedChannels: [] };
}

const SCENARIOS = [
  [PI_ASSET, "pi:permission_lifecycle"],
  [PI_ASSET, "pi:silent_and_orphan_decisions"],
  [PI_ASSET, "pi:settled_discards_prompts"],
  [PI_ASSET, "pi:child_session_silent"],
  [PI_ASSET, "pi:shutdown_quit"],
  [PI_ASSET, "pi:shutdown_reload"],
  [PI_ASSET, "pi:shutdown_new"],
  [PI_ASSET, "pi:shutdown_resume"],
  [PI_ASSET, "pi:shutdown_fork"],
  [PI_ASSET, "pi:hanging_binary:hang"],
  [OPENCODE_ASSET, "opencode:load_posts_created"],
  [OPENCODE_ASSET, "opencode:status_becomes_event_suffix"],
  [OPENCODE_ASSET, "opencode:status_without_type_is_dropped"],
  [OPENCODE_ASSET, "opencode:untracked_event_is_dropped"],
  [OPENCODE_ASSET, "opencode:hanging_binary:hang"],
];

const report = {};
for (const [assetPath, scenario] of SCENARIOS) {
  const { adapterPath, invocations } = stage(assetPath, scenario);
  const started = Date.now();
  const result = scenario.startsWith("pi:")
    ? await runPi(scenario, adapterPath, invocations)
    : await runOpenCode(scenario, adapterPath);
  report[scenario] = {
    invocations: invocations(),
    elapsedMs: Date.now() - started,
    ...result,
  };
}

process.stdout.write(JSON.stringify(report));
// A hung fake binary leaves its pipes open; the recorded results are already complete.
process.exit(0);
