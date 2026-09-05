#!/usr/bin/env node
// Push model: an Electron-main-process-shaped consumer. Start listening, register a
// sink, hydrate from `sessions --json`, then apply live POSTs through the same code
// path — keyed by bindingId so hydration and a racing live event never double up.
// See ../../README.md#integrating.

import { createServer } from "node:http";
import { execFile } from "node:child_process";

const BIN = process.env.HOOKLINESINKER_BIN || "hooklinesinker";
const CONSUMER = process.env.HOOKLINESINKER_CONSUMER || "sink-server-example";
const HOST = process.env.HOOKLINESINKER_HOST || "127.0.0.1";
const PORT = Number(process.env.PORT || 4870);

const sessions = new Map();

function log(...parts) {
  console.log("[sink]", ...parts);
}

function applyEvent(record, source) {
  if (!record || record.protocol !== 1) {
    log(source, "skip: unsupported protocol", record && record.protocol);
    return;
  }
  const id = record.bindingId;
  if (!record.running) {
    const existed = sessions.delete(id);
    log(source, "remove", id, "reason=running:false", existed ? "" : "(was not tracked)");
    return;
  }
  const known = sessions.has(id);
  sessions.set(id, record);
  log(
    source,
    known ? "update" : "add",
    id,
    `agent=${record.agent}`,
    `phase=${record.phase}`,
    `session=${record.session && record.session.id}`
  );
}

function runCli(args) {
  return new Promise((resolve, reject) => {
    execFile(BIN, args, (err, stdout, stderr) => {
      if (err) {
        reject(new Error(`${args.join(" ")}: ${stderr.trim() || err.message}`));
        return;
      }
      resolve(stdout);
    });
  });
}

async function register(sinkUrl) {
  await runCli(["install", "--consumer", CONSUMER, "--sink", sinkUrl]);
  log("registered consumer", CONSUMER, "sink=" + sinkUrl);
}

async function hydrate() {
  const stdout = await runCli(["sessions", "--json"]);
  const envelope = JSON.parse(stdout);
  if (envelope.protocol !== 1) {
    log("hydrate: refusing envelope with protocol", envelope.protocol);
    return;
  }
  for (const problem of envelope.problems || []) {
    log("hydrate: problem", problem.observedAt, problem.message);
  }
  for (const record of envelope.sessions || []) {
    applyEvent(record, "hydrate");
  }
  log("hydrate complete:", (envelope.sessions || []).length, "session(s)");
}

const server = createServer((req, res) => {
  if (req.method !== "POST" || req.url !== "/hook") {
    res.writeHead(404).end();
    return;
  }
  const chunks = [];
  req.on("data", (chunk) => chunks.push(chunk));
  req.on("end", () => {
    res.writeHead(200, { "content-type": "application/json" }).end('{"ok":true}');
    const body = Buffer.concat(chunks).toString("utf8");
    // Real work happens after the response is already on the wire: the sender's
    // budget is 200ms and it never retries a slow ack.
    setImmediate(() => {
      let record;
      try {
        record = JSON.parse(body);
      } catch (err) {
        log("post: invalid JSON body:", err.message);
        return;
      }
      log("post", record.bindingId, `event=${record.event}`, `running=${record.running}`);
      applyEvent(record, "post");
    });
  });
  req.on("error", () => {
    try {
      res.destroy();
    } catch {
      // already gone
    }
  });
});

server.listen(PORT, HOST, async () => {
  const { port } = server.address();
  const sinkUrl = `http://${HOST}:${port}/hook`;
  log("listening on", sinkUrl);
  try {
    await register(sinkUrl);
    await hydrate();
  } catch (err) {
    log("startup failed:", err.message);
  }
});
