#!/usr/bin/env node
// Minimal Agent Control Centre adapter for Node 18+ (no dependencies).
// Run the HUD first, then:  node examples/node_agent.mjs
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { randomUUID } from "node:crypto";

const BASE = `http://${process.env.ACC_ADDR ?? "127.0.0.1:7777"}/api/v1`;
const dataDir =
  process.env.ACC_DATA_DIR ??
  join(process.platform === "win32" ? process.env.APPDATA : process.env.XDG_DATA_HOME ?? join(homedir(), ".local", "share"), "agent-control-centre");
const TOKEN = readFileSync(join(dataDir, "token"), "utf8").trim();

async function call(method, path, body) {
  const r = await fetch(BASE + path, {
    method,
    headers: { Authorization: `Bearer ${TOKEN}`, "Content-Type": "application/json" },
    body: body && JSON.stringify(body),
  });
  if (!r.ok) throw new Error(`${method} ${path}: ${r.status} ${await r.text()}`);
  return r.json();
}

// The HUD hands out the id; sequence keeps events in order and event_id makes retries harmless.
const { agent_id } = await call("POST", "/register", { name: "Node demo", capabilities: ["demo"], workspace: process.cwd() });
let sequence = 0;
const emit = (event, fields = {}) =>
  call("POST", "/event", { protocol_version: 1, event_id: randomUUID(), sequence: ++sequence, event, agent_id, ...fields });

/** Resolves true only for an explicit approval of exactly this action. */
async function ask(detail, expires_in = 120) {
  const approval_id = `${agent_id}-${randomUUID().slice(0, 8)}`;
  await emit("approval.required", { approval_id, detail, expires_in });
  for (const end = Date.now() + (expires_in + 2) * 1000; Date.now() < end; ) {
    const { status } = await call("GET", `/approvals/${approval_id}`);
    if (status !== "pending") return status === "approved"; // "denied" and "expired" are both a no
    await new Promise((r) => setTimeout(r, 500));
  }
  return false;
}

await emit("agent.started", { task: "Publish the package", ttl: 30 });
for (const progress of [25, 60, 90]) {
  await new Promise((r) => setTimeout(r, 700));
  await emit("agent.progress", { progress, message: `working… ${progress}%` });
}
await emit("resource.updated", { tokens_in: 1800, tokens_out: 420, cost_usd: 0.03 });
if (await ask("$ npm publish")) await emit("agent.completed", { message: "published" });
else await emit("agent.failed", { message: "not approved, nothing was published" });
