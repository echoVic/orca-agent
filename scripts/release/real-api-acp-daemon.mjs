#!/usr/bin/env node

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { EventEmitter, once } from "node:events";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";

const bin = path.resolve(process.argv[2] ?? "target/debug/orca");
const timeoutMs = 180_000;
const projectionKey = "orca.dev/projection";
const children = new Set();
const peers = new Set();
let root;

function start(args, env) {
  const child = spawn(bin, args, { env, detached: true, stdio: ["ignore", "pipe", "pipe"] });
  child.output = "";
  child.diagnostics = "";
  child.stdout.on("data", chunk => { child.output = (child.output + chunk).slice(-256 * 1024); });
  child.stderr.on("data", chunk => { child.diagnostics = (child.diagnostics + chunk).slice(-64 * 1024); });
  child.finished = new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("close", (code, signal) => resolve({ code, signal }));
  });
  children.add(child);
  return child;
}

async function stop(child) {
  if (child.exitCode === null && child.signalCode === null) {
    try { process.kill(-child.pid, "SIGTERM"); } catch {}
    const kill = setTimeout(() => {
      try { process.kill(-child.pid, "SIGKILL"); } catch {}
    }, 10_000);
    try { await child.finished; } finally { clearTimeout(kill); }
  } else {
    await child.finished;
  }
  children.delete(child);
}

class Peer {
  constructor(socket) {
    this.socket = socket;
    this.frames = [];
    this.events = new EventEmitter();
    this.nextId = 1;
    this.closed = false;
    let pending = "";
    socket.setEncoding("utf8");
    socket.on("data", chunk => {
      pending += chunk;
      if (Buffer.byteLength(pending) > 16 * 1024 * 1024) { socket.destroy(); return; }
      let index;
      while ((index = pending.indexOf("\n")) !== -1) {
        const line = pending.slice(0, index);
        pending = pending.slice(index + 1);
        if (!line.trim()) continue;
        let frame;
        try { frame = JSON.parse(line); } catch { socket.destroy(); return; }
        this.frames.push(frame);
        if (this.frames.length > 20_000) { socket.destroy(); return; }
        // This smoke needs no client-owned capabilities. Never auto-approve.
        if (frame.method && frame.id !== undefined) {
          const answer = frame.method === "session/request_permission"
            ? { result: { outcome: { outcome: "cancelled" } } }
            : { error: { code: -32601, message: "capability not advertised" } };
          socket.write(`${JSON.stringify({ jsonrpc: "2.0", id: frame.id, ...answer })}\n`);
        }
        this.events.emit("change");
      }
    });
    socket.on("error", () => {});
    socket.on("close", () => { this.closed = true; this.events.emit("change"); });
    peers.add(this);
  }

  async wait(predicate, from = 0) {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const found = this.frames.slice(from).find(predicate);
      if (found) return found;
      if (this.closed) throw new Error("ACP connection closed before expected response");
      const remaining = deadline - Date.now();
      if (remaining <= 0) throw new Error("ACP response deadline exceeded");
      await once(this.events, "change", { signal: AbortSignal.timeout(remaining) });
    }
  }

  send(method, params) {
    const id = this.nextId++;
    this.socket.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    return id;
  }

  async request(method, params) {
    const id = this.send(method, params);
    const frame = await this.wait(frame => frame.id === id);
    assert.equal(frame.error, undefined, `${method}: ${JSON.stringify(frame.error)}`);
    return frame.result;
  }

  async close() {
    if (!this.closed) {
      const closed = once(this.socket, "close");
      this.socket.destroy();
      await closed;
    }
    peers.delete(this);
  }
}

async function connect(socketPath, inspect = false) {
  const socket = net.createConnection(socketPath);
  await once(socket, "connect", { signal: AbortSignal.timeout(10_000) });
  const peer = new Peer(socket);
  await peer.request("initialize", {
    protocolVersion: 1,
    clientCapabilities: inspect ? { _meta: { [projectionKey]: { version: 1 } } } : {},
    clientInfo: { name: inspect ? "orca-acceptance-inspector" : "standard-acp-client", version: "1" },
  });
  return peer;
}

async function waitDaemon(child, socket) {
  const deadline = Date.now() + 15_000;
  while (!existsSync(socket)) {
    if (child.exitCode !== null || child.signalCode !== null || Date.now() > deadline) {
      throw new Error(`Daemon did not start: ${child.diagnostics}`);
    }
    await delay(25);
  }
}

function messages(frames, kind = "agent_message_chunk") {
  return frames.filter(frame => frame.method === "session/update" && frame.params.update.sessionUpdate === kind)
    .map(frame => frame.params.update.content?.text ?? "").join("");
}

async function main() {
  assert.notEqual(process.platform, "win32", "local ACP daemon is currently Unix-only");
  const sourceHome = process.env.ORCA_HOME ?? path.join(os.homedir(), ".orca");
  let key = process.env.ORCA_API_KEY ?? process.env.DEEPSEEK_API_KEY;
  if (!key && existsSync(path.join(sourceHome, "auth.json"))) {
    key = JSON.parse(readFileSync(path.join(sourceHome, "auth.json"), "utf8")).DEEPSEEK_API_KEY;
  }
  assert.ok(key, "DeepSeek credential is required; missing credentials are not a pass");
  root = realpathSync(mkdtempSync("/tmp/orca-acp-acceptance-"));
  const home = path.join(root, "home");
  const cwd = path.join(root, "workspace");
  mkdirSync(home, { mode: 0o700 });
  mkdirSync(cwd, { mode: 0o700 });
  writeFileSync(path.join(root, "ownership.json"), JSON.stringify({ purpose: "ACP acceptance", root, process: process.pid }), { mode: 0o600 });
  writeFileSync(path.join(home, "config.toml"), "update_check = false\nauto_memory = false\n", { mode: 0o600 });
  const env = { ...process.env, ORCA_HOME: home, ORCA_API_KEY: key };
  const socket = path.join(root, "daemon.sock");
  const daemonArgs = ["--model", "deepseek-flash", "daemon", "--cwd", cwd, "--socket", socket];
  let daemon = start(daemonArgs, env);
  await waitDaemon(daemon, socket);

  const owner = await connect(socket);
  const { sessionId } = await owner.request("session/new", { cwd, mcpServers: [] });
  assert.ok(sessionId);
  const follower = await connect(socket);
  await follower.request("session/load", { sessionId, cwd, mcpServers: [] });
  const inspector = await connect(socket, true);
  await inspector.request("session/load", { sessionId, cwd, mcpServers: [] });
  const startIndex = inspector.frames.length;
  const token = `ACP_DURABLE_${Date.now()}`;
  const promptId = owner.send("session/prompt", { sessionId, prompt: [{ type: "text", text: `Do not call tools. Write 40 short numbered lines about persistence, then end with exactly ${token}.` }] });
  await owner.wait(frame => frame.method === "session/update" && frame.params.update.sessionUpdate === "agent_message_chunk");
  const overlapId = follower.send("session/prompt", { sessionId, prompt: [{ type: "text", text: "THIS_COMPETING_PROMPT_MUST_NOT_BE_ADMITTED" }] });
  const overlap = await follower.wait(frame => frame.id === overlapId);
  assert.ok(overlap.error, "concurrent prompt must be rejected, not queued or admitted");
  assert.ok(!owner.frames.some(frame => frame.id === promptId), "fixture completed before disconnect; no disconnect evidence");
  await owner.close();
  await inspector.wait(frame => frame.params?._meta?.[projectionKey]?.phase === "terminal", startIndex);
  assert.ok(messages(follower.frames).includes(token), "standard follower must receive the disconnected owner's answer");
  assert.ok(!messages(follower.frames, "user_message_chunk").includes("THIS_COMPETING_PROMPT_MUST_NOT_BE_ADMITTED"));
  console.log("ACP daemon real API: shared observer, exclusive prompt, disconnected owner completion verified");

  const reconnect = await connect(socket);
  await reconnect.request("session/load", { sessionId, cwd, mcpServers: [] });
  const replay = messages(reconnect.frames);
  assert.equal(replay.split(token).length - 1, 1, "reconnect must replay answer once");
  await reconnect.close();
  await follower.close();
  await inspector.close();
  await stop(daemon);
  assert.equal(existsSync(socket), false, "shutdown must remove its owned socket");
  daemon = start(daemonArgs, env);
  await waitDaemon(daemon, socket);
  const recovered = await connect(socket);
  await recovered.request("session/load", { sessionId, cwd, mcpServers: [] });
  assert.equal(messages(recovered.frames).split(token).length - 1, 1, "daemon restart must recover the same answer once");
  await recovered.close();
  const headless = start(["attach", sessionId, "--cwd", cwd, "--socket", socket, "--exec", "Do not call tools. Reply with exactly ACP_HEADLESS_OK."], env);
  const result = await headless.finished;
  assert.equal(result.code, 0, headless.diagnostics);
  assert.ok(headless.output.includes("ACP_HEADLESS_OK"));
  console.log("ACP daemon real API: reconnect, daemon restart, and headless attach verified");
}

const watchdog = setTimeout(() => {
  for (const child of children) { try { process.kill(-child.pid, "SIGKILL"); } catch {} }
}, 8 * 60 * 1000);
try {
  await main();
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
} finally {
  clearTimeout(watchdog);
  for (const peer of peers) await peer.close();
  for (const child of children) await stop(child).catch(() => {});
  if (root) rmSync(root, { recursive: true, force: true });
}
