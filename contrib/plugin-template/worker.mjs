#!/usr/bin/env node
// Template worker for the Agent of Empires plugin system.
//
// Wire format: one JSON-RPC 2.0 object per line on stdin/stdout (see
// docs/development/writing-plugins.md). There is no init handshake: the
// host spawns this process lazily on the first call and shuts it down by
// closing stdin, so exit on EOF. stderr lines land in the host log.
//
// Messages are told apart by shape:
// - has "method" and "id": a request FROM the host (answer it),
// - has "method", no "id": a notification (e.g. events.event),
// - has "id", no "method": the host's response to one of OUR requests.

import { createInterface } from "node:readline";

function writeLine(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

// Worker -> host requests (the capability-gated host API). We use our own
// id space; responses are matched back through `pending`.
let nextId = 1;
const pending = new Map();

function callHost(method, params) {
  const id = nextId++;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    writeLine({ jsonrpc: "2.0", id, method, params });
  });
}

// Push the dashboard card declared as `template_card` in aoe-plugin.toml.
// ui.state.set needs no capability; the host checks the contribution id
// against the approved manifest and validates the payload for the slot
// (dashboard-card takes kind "blocks"; text fields cap at 200 chars).
let refreshes = 0;

async function pushCard() {
  await callHost("ui.state.set", {
    contribution_id: "template_card",
    // dashboard-card is a global slot: no session_id. Optional ttl_ms
    // would expire the state; we keep it until the next push.
    payload: {
      kind: "blocks",
      severity: "info",
      blocks: [
        { type: "metric", label: "Refreshes", value: String(refreshes) },
        { type: "text", text: "Press ctrl+t in the TUI to refresh." },
      ],
    },
  });
}

// Host -> worker request handlers, keyed by the rpc_method values this
// plugin's manifest declares.
async function handle(method, params) {
  if (method === "template.refresh") {
    refreshes += 1;
    await pushCard();
    // The TUI passes { session_id } for keybound actions; unused here.
    void params;
    return { ok: true, refreshes };
  }
  throw new Error(`template worker does not handle ${method}`);
}

const rl = createInterface({ input: process.stdin });

rl.on("line", (line) => {
  if (!line.trim()) return;
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return; // non-JSON lines are ignored, mirroring the host
  }
  if (msg.method !== undefined && msg.id !== undefined) {
    // Request from the host: always answer the id.
    handle(msg.method, msg.params)
      .then((result) => writeLine({ jsonrpc: "2.0", id: msg.id, result }))
      .catch((e) =>
        writeLine({
          jsonrpc: "2.0",
          id: msg.id,
          error: { code: -32601, message: String(e.message ?? e) },
        }),
      );
  } else if (msg.id !== undefined) {
    // Response to one of our host calls.
    const waiter = pending.get(msg.id);
    if (!waiter) return;
    pending.delete(msg.id);
    if (msg.error) waiter.reject(new Error(msg.error.message ?? "host error"));
    else waiter.resolve(msg.result);
  }
  // Notifications (events.event) would land here; this template
  // subscribes to nothing.
});

rl.on("close", () => process.exit(0));

// UI state is ephemeral: the host evicts it when the worker dies, so push
// the initial card on every startup.
pushCard().catch((e) => console.error(`initial push failed: ${e.message}`));
