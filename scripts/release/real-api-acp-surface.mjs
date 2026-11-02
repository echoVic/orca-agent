#!/usr/bin/env node

import { spawn } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, rmSync, statSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import readline from "node:readline";

const fakeSentinel = "ORCA_ACP_SURFACE_FAKE_OK";
const realSentinel = "ORCA_ACP_SURFACE_REAL_OK";

function parseArgs(argv) {
  const args = { bin: null, timeoutMs: 180000 };
  for (let index = 0; index < argv.length; index += 1) {
    if (argv[index] === "--bin") args.bin = argv[++index];
    else if (argv[index] === "--timeout-ms") args.timeoutMs = Number.parseInt(argv[++index], 10);
    else throw new Error(`Unknown argument: ${argv[index]}`);
  }
  if (!args.bin || !path.isAbsolute(args.bin)) throw new Error("--bin must be an absolute path");
  if (!existsSync(args.bin) || !statSync(args.bin).isFile()) throw new Error(`invalid --bin: ${args.bin}`);
  if (!Number.isInteger(args.timeoutMs) || args.timeoutMs <= 0) throw new Error("invalid --timeout-ms");
  return args;
}

function killGroup(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  try { process.kill(-child.pid, "SIGKILL"); } catch { try { child.kill("SIGKILL"); } catch {} }
}

async function raceWithTimeout(promise, timeoutMs, timeoutValue) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((resolve) => {
        timer = setTimeout(() => resolve(timeoutValue), timeoutMs);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function runFake(args) {
  const child = spawn(args.bin, [], { detached: true, stdio: ["ignore", "pipe", "pipe"], env: process.env });
  let output = "";
  let stderr = "";
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stdout.on("data", (chunk) => { output += chunk; });
  child.stderr.on("data", (chunk) => { stderr += chunk; });
  let timedOut = false;
  const timer = setTimeout(() => { timedOut = true; killGroup(child); }, args.timeoutMs);
  const result = await new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("close", (code) => resolve(code));
  });
  clearTimeout(timer);
  if (timedOut) throw new Error("ACP fake child timed out or remained unreaped");
  if (result !== 0) throw new Error(`ACP fake child failed: ${stderr}`);
  const actual = output.split(/\r?\n/).filter((line) => line.startsWith("ACP_HARNESS ")).map((line) => line.slice(12).trim());
  const expected = ["initialized connection-1", "new session-1", "prompt_update prompt-1", "prompt_response prompt-1", "load_update session-1", "load_response session-1", "cancel_sent prompt-2", "cancel_response prompt-2", "transport_closed connection-1"];
  if (actual.length !== expected.length || actual.some((line, index) => line !== expected[index])) {
    throw new Error(`ACP fake trace order or identity mismatch: ${JSON.stringify(actual)}`);
  }
  console.log(fakeSentinel);
}

function isolatedHome() {
  const home = mkdtempSync(path.join(os.tmpdir(), "orca-real-acp-home-"));
  const sourceHome = process.env.ORCA_HOME ?? path.join(os.homedir(), ".orca");
  const sourceAuth = path.join(sourceHome, "auth.json");
  if (existsSync(sourceAuth)) copyFileSync(sourceAuth, path.join(home, "auth.json"));
  if (!process.env.ORCA_API_KEY && !existsSync(path.join(home, "auth.json"))) {
    rmSync(home, { recursive: true, force: true });
    throw new Error("DeepSeek credentials are required through ORCA_API_KEY or ORCA_HOME/auth.json");
  }
  return home;
}

function startAcp(args, env) {
  const child = spawn(args.bin, ["--mode", "acp"], {
    detached: true,
    stdio: ["pipe", "pipe", "pipe"],
    env,
  });
  let stderr = "";
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk) => { stderr += chunk; });
  const lines = readline.createInterface({ input: child.stdout });
  const iterator = lines[Symbol.asyncIterator]();
  const frames = [];
  const exited = new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("close", (code, signal) => resolve({ code, signal }));
  });
  const send = (frame) => child.stdin.write(`${JSON.stringify(frame)}\n`);
  const readUntil = async (predicate, timeoutMs, label) => {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const remaining = deadline - Date.now();
      if (remaining <= 0) throw new Error(`${label} timed out; frames=${JSON.stringify(frames)} stderr=${stderr}`);
      const next = await raceWithTimeout(iterator.next(), remaining, { timeout: true });
      if (next.timeout) throw new Error(`${label} timed out; frames=${JSON.stringify(frames)} stderr=${stderr}`);
      if (next.done) throw new Error(`${label} transport closed; frames=${JSON.stringify(frames)} stderr=${stderr}`);
      const frame = JSON.parse(next.value);
      frames.push(frame);
      if (predicate(frame, frames)) return frame;
    }
  };
  const close = async () => {
    child.stdin.end();
    const result = await raceWithTimeout(
      exited,
      args.timeoutMs,
      { timeout: true },
    );
    lines.close();
    if (result.timeout) { killGroup(child); throw new Error("ACP child did not exit after stdin close"); }
    if (result.code !== 0) throw new Error(`ACP child exited ${result.code}/${result.signal ?? "none"}: ${stderr}`);
  };
  return { child, frames, send, readUntil, close, exited, lines };
}

function initialize(run) {
  run.send({ jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: 1, clientCapabilities: {}, clientInfo: { name: "orca-release-gate", version: "1" } } });
  return run.readUntil((frame) => frame.id === 1, 30000, "ACP initialize");
}

function assistantText(frames, sessionId) {
  return frames
    .filter((frame) => frame.method === "session/update"
      && frame.params?.sessionId === sessionId
      && frame.params?.update?.sessionUpdate === "agent_message_chunk"
      && frame.params?.update?.content?.type === "text")
    .map((frame) => frame.params.update.content.text)
    .join("");
}

async function runReal(args) {
  const home = isolatedHome();
  const cwd = path.join(home, "workspace");
  mkdirSync(cwd);
  const env = { ...process.env, ORCA_HOME: home };
  const token = `ORCA_ACP_RECOVERY_${Date.now()}_${process.pid}`;
  let sessionId;
  const activeRuns = new Set();
  const start = () => {
    const run = startAcp(args, env);
    activeRuns.add(run);
    return run;
  };
  try {
    const first = start();
    await initialize(first);
    first.send({ jsonrpc: "2.0", id: 2, method: "session/new", params: { cwd, mcpServers: [] } });
    const created = await first.readUntil((frame) => frame.id === 2, args.timeoutMs, "ACP session/new");
    sessionId = created.result?.sessionId;
    if (!sessionId) throw new Error(`ACP session/new missing sessionId: ${JSON.stringify(created)}`);
    first.send({ jsonrpc: "2.0", id: 3, method: "session/prompt", params: { sessionId, prompt: [{ type: "text", text: `Reply with exactly ${token}` }] } });
    const update = await first.readUntil((frame, frames) => frame.method === "session/update" && assistantText(frames, sessionId).includes(token), args.timeoutMs, "ACP prompt update");
    const response = await first.readUntil((frame) => frame.id === 3, args.timeoutMs, "ACP prompt response");
    if (response.result?.stopReason !== "end_turn") throw new Error(`ACP prompt terminal mismatch: ${JSON.stringify(response)}`);
    if (first.frames.indexOf(update) > first.frames.indexOf(response)) throw new Error("ACP prompt response preceded terminal update flush");
    await first.close();

    const resumed = start();
    await initialize(resumed);
    resumed.send({ jsonrpc: "2.0", id: 4, method: "session/load", params: { sessionId, cwd, mcpServers: [] } });
    const loadResponse = await resumed.readUntil((frame) => frame.id === 4, args.timeoutMs, "ACP session/load");
    const replayFrames = resumed.frames.slice(0, resumed.frames.indexOf(loadResponse));
    if (!assistantText(replayFrames, sessionId).includes(token)) {
      throw new Error(`ACP load replay was not flushed before response: ${JSON.stringify(resumed.frames)}`);
    }
    resumed.send({ jsonrpc: "2.0", id: 5, method: "session/prompt", params: { sessionId, prompt: [{ type: "text", text: "Write 200 numbered lines containing ACP_CANCEL_STREAM." }] } });
    await resumed.readUntil((frame) => frame.method === "session/update", args.timeoutMs, "ACP cancellable prompt update");
    resumed.send({ jsonrpc: "2.0", method: "session/cancel", params: { sessionId } });
    const cancelled = await resumed.readUntil((frame) => frame.id === 5, args.timeoutMs, "ACP cancelled prompt response");
    if (cancelled.result?.stopReason !== "cancelled") throw new Error(`ACP cancellation mismatch: ${JSON.stringify(cancelled)}`);
    await resumed.close();
    console.log(`${realSentinel} ${token}`);
  } finally {
    const remaining = [...activeRuns].filter((run) => run.child.exitCode === null && run.child.signalCode === null);
    for (const run of remaining) {
      killGroup(run.child);
      run.child.stdin.end();
      run.lines.close();
    }
    await Promise.allSettled(remaining.map((run) => run.exited));
    rmSync(home, { recursive: true, force: true });
  }
}

try {
  const args = parseArgs(process.argv.slice(2));
  if (process.env.ORCA_RELEASE_FAKE_SCENARIO) await runFake(args);
  else await runReal(args);
} catch (error) {
  console.error(error.message);
  process.exit(1);
}
