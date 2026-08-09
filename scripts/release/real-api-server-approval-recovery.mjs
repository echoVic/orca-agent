#!/usr/bin/env node

import { spawn } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import readline from "node:readline";

const fakeSentinel = "ORCA_SERVER_APPROVAL_RECOVERY_FAKE_OK";
const realSentinel = "ORCA_SERVER_APPROVAL_RECOVERY_REAL_OK";

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
  child.stdout.setEncoding("utf8");
  child.stdout.on("data", (chunk) => { output += chunk; });
  let timedOut = false;
  const timer = setTimeout(() => { timedOut = true; killGroup(child); }, args.timeoutMs);
  const code = await new Promise((resolve, reject) => { child.once("error", reject); child.once("close", resolve); });
  clearTimeout(timer);
  if (timedOut) throw new Error("server fake child timed out or remained unreaped");
  if (code !== 0) throw new Error(`server fake child failed with ${code}`);
  const actual = output.split(/\r?\n/).filter((line) => line.startsWith("SERVER_HARNESS ")).map((line) => line.slice(15).trim());
  const expected = ["thread_started thread-1", "permission_requested request-1", "permission_resolved request-1", "output_flushed turn-1", "turn_terminal turn-1", "eof_settled request-2", "restart_resumed thread-1", "replay_visible turn-1", "eof_restart_recovered thread-2", "shutdown_complete connection-2"];
  if (actual.length !== expected.length || actual.some((line, index) => line !== expected[index])) {
    throw new Error(`server fake trace order or identity mismatch: ${JSON.stringify(actual)}`);
  }
  console.log(fakeSentinel);
}

function isolatedHome() {
  const home = mkdtempSync(path.join(os.tmpdir(), "orca-real-server-home-"));
  const sourceHome = process.env.ORCA_HOME ?? path.join(os.homedir(), ".orca");
  const sourceAuth = path.join(sourceHome, "auth.json");
  if (existsSync(sourceAuth)) copyFileSync(sourceAuth, path.join(home, "auth.json"));
  if (!process.env.ORCA_API_KEY && !existsSync(path.join(home, "auth.json"))) {
    rmSync(home, { recursive: true, force: true });
    throw new Error("DeepSeek credentials are required through ORCA_API_KEY or ORCA_HOME/auth.json");
  }
  writeFileSync(home + "/config.toml", "mode = \"suggest\"\n[[permissions.rules]]\ntool = \"write_file\"\npattern = \"**\"\ndecision = \"allow\"\n");
  return home;
}

function startServer(args, env, cwd) {
  const child = spawn(args.bin, ["--mode", "server", "--cwd", cwd], { detached: true, stdio: ["pipe", "pipe", "pipe"], env });
  let stderr = "";
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk) => { stderr += chunk; });
  const lines = readline.createInterface({ input: child.stdout });
  const iterator = lines[Symbol.asyncIterator]();
  const events = [];
  const exited = new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("close", (code, signal) => resolve({ code, signal }));
  });
  const send = (frame) => child.stdin.write(`${JSON.stringify(frame)}\n`);
  const readUntil = async (predicate, timeoutMs, label) => {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const remaining = deadline - Date.now();
      if (remaining <= 0) throw new Error(`${label} timed out: ${JSON.stringify(events)} ${stderr}`);
      const next = await raceWithTimeout(iterator.next(), remaining, { timeout: true });
      if (next.timeout) throw new Error(`${label} timed out: ${JSON.stringify(events)} ${stderr}`);
      if (next.done) throw new Error(`${label} transport closed: ${stderr}`);
      const event = JSON.parse(next.value);
      events.push(event);
      if (predicate(event)) return event;
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
    if (result.timeout) { killGroup(child); throw new Error("server child did not exit after EOF"); }
    if (result.code !== 0) throw new Error(`server child exited ${result.code}/${result.signal ?? "none"}: ${stderr}`);
  };
  return { child, events, send, readUntil, close, exited };
}

async function runReal(args) {
  const home = isolatedHome();
  const cwd = path.join(home, "workspace");
  mkdirSync(cwd);
  const env = { ...process.env, ORCA_HOME: home };
  const token = `ORCA_SERVER_RECOVERY_${Date.now()}_${process.pid}`;
  const outputFile = path.join(cwd, "approved.txt");
  let threadId;
  const activeRuns = new Set();
  const start = () => {
    const run = startServer(args, env, cwd);
    activeRuns.add(run);
    return run;
  };
  try {
    const first = start();
    first.send({ id: "thread", method: "thread/start", params: {} });
    const started = await first.readUntil((event) => event.id === "thread" && event.event === "thread_started", args.timeoutMs, "server thread/start");
    threadId = started.threadId;
    first.send({
      id: "turn",
      method: "turn/start",
      params: {
        threadId,
        input: [{
          type: "text",
          text: `Use request_permissions to request write access to the current workspace, then use write_file to write exactly ${token} to approved.txt. Use the relative path approved.txt only. Do not use bash or another tool.`,
        }],
      },
    });
    const permission = await first.readUntil(
      (event) => event.id === "turn" && event.event === "permission_request",
      args.timeoutMs,
      "server permission request",
    );
    const requestedTool = [...first.events]
      .reverse()
      .find((event) => event.id === "turn" && event.event === "tool_requested");
    if (requestedTool?.tool !== "request_permissions" || !permission.requestId) {
      throw new Error(`server permission request owner mismatch: tool=${JSON.stringify(requestedTool)} permission=${JSON.stringify(permission)}`);
    }
    first.send({
      id: "permission",
      method: "permission/respond",
      params: {
        requestId: permission.requestId,
        decision: "allow",
        scope: "turn",
        permissions: permission.permissions,
      },
    });
    const resolved = await first.readUntil(
      (event) => event.id === "permission" && event.event === "permission_resolved",
      args.timeoutMs,
      "server permission response",
    );
    if (resolved.requestId !== permission.requestId) {
      throw new Error(`server permission response identity mismatch: ${JSON.stringify(resolved)}`);
    }
    let terminal;
    for (;;) {
      const next = await first.readUntil(
        (event) =>
          (event.id === "turn" && event.event === "permission_request") ||
          (event.id === "turn" && event.event === "turn_completed"),
        args.timeoutMs,
        "server turn terminal",
      );
      if (next.event === "turn_completed") {
        terminal = next;
        break;
      }
      const toolApproval = [...first.events]
        .reverse()
        .find((event) => event.id === "turn" && event.event === "tool_requested");
      if (toolApproval?.tool !== "write_file" || !next.requestId) {
        throw new Error(`server tool approval owner mismatch: tool=${JSON.stringify(toolApproval)} permission=${JSON.stringify(next)}`);
      }
      first.send({
        id: "tool-approval",
        method: "permission/respond",
        params: {
          requestId: next.requestId,
          decision: "allow",
          scope: "turn",
          permissions: next.permissions,
        },
      });
      const toolResolved = await first.readUntil(
        (event) => event.id === "tool-approval" && event.event === "permission_resolved",
        args.timeoutMs,
        "server tool approval response",
      );
      if (toolResolved.requestId !== next.requestId) {
        throw new Error(`server tool approval response identity mismatch: ${JSON.stringify(toolResolved)}`);
      }
    }
    const durableOutput = existsSync(outputFile) ? readFileSync(outputFile, "utf8") : null;
    if (terminal.status !== "success" || durableOutput !== token) {
      throw new Error(
        `server terminal preceded durable tool output: terminal=${JSON.stringify(terminal)} output=${JSON.stringify(durableOutput)} expected=${JSON.stringify(token)} tail=${JSON.stringify(first.events.slice(-30))}`,
      );
    }

    first.send({ id: "eof-thread", method: "thread/start", params: {} });
    const eofThread = await first.readUntil((event) => event.id === "eof-thread" && event.event === "thread_started", args.timeoutMs, "server EOF thread");
    first.send({
      id: "eof-turn",
      method: "turn/start",
      params: {
        threadId: eofThread.threadId,
        input: [{
          type: "text",
          text: "Call request_permissions now to request write access to the current workspace. Do not call any other tool or answer before it resolves.",
        }],
      },
    });
    await first.readUntil((event) => event.id === "eof-turn" && event.event === "permission_request", args.timeoutMs, "server EOF pending permission");
    await first.close();

    const resumed = start();
    resumed.send({ id: "resume", method: "thread/resume", params: { threadId } });
    const resumedEvent = await resumed.readUntil(
      (event) => event.id === "resume" && event.event === "thread_started",
      args.timeoutMs,
      "server restart resume",
    );
    if (resumedEvent.threadId !== threadId) throw new Error(`server resumed wrong thread: ${JSON.stringify(resumedEvent)}`);
    resumed.send({ id: "read", method: "thread/read", params: { threadId, includeMessages: true, includeTurns: true } });
    const read = await resumed.readUntil((event) => event.id === "read" && event.event === "thread_read", args.timeoutMs, "server restart read");
    if (!JSON.stringify(read).includes(token)) throw new Error(`server restart replay missing ${token}: ${JSON.stringify(read)}`);

    resumed.send({ id: "eof-resume", method: "thread/resume", params: { threadId: eofThread.threadId } });
    const eofResumed = await resumed.readUntil(
      (event) => event.id === "eof-resume" && event.event === "thread_started",
      args.timeoutMs,
      "server EOF thread restart resume",
    );
    if (eofResumed.threadId !== eofThread.threadId) {
      throw new Error(`server resumed wrong EOF thread: ${JSON.stringify(eofResumed)}`);
    }
    resumed.send({
      id: "eof-read",
      method: "thread/read",
      params: { threadId: eofThread.threadId, includeMessages: true, includeTurns: true },
    });
    const eofRead = await resumed.readUntil(
      (event) => event.id === "eof-read" && event.event === "thread_read",
      args.timeoutMs,
      "server EOF thread restart read",
    );
    if (!JSON.stringify(eofRead).includes("request_permissions")) {
      throw new Error(`server EOF restart lost pending interaction history: ${JSON.stringify(eofRead)}`);
    }
    const eofRecoveryToken = `ORCA_SERVER_EOF_RECOVERED_${Date.now()}_${process.pid}`;
    resumed.send({
      id: "eof-recovery-turn",
      method: "turn/start",
      params: {
        threadId: eofThread.threadId,
        input: [{ type: "text", text: `Reply with exactly ${eofRecoveryToken}` }],
      },
    });
    let eofRecoveryText = "";
    const eofRecoveryTerminal = await resumed.readUntil((event) => {
      if (event.id !== "eof-recovery-turn") return false;
      if (event.event === "message_delta") eofRecoveryText += event.text ?? "";
      if (event.event === "error") throw new Error(`server EOF recovery turn failed: ${JSON.stringify(event)}`);
      return event.event === "turn_completed";
    }, args.timeoutMs, "server EOF recovery turn terminal");
    if (eofRecoveryTerminal.status !== "success" || !eofRecoveryText.includes(eofRecoveryToken)) {
      throw new Error(`server EOF recovery lease was not released: terminal=${JSON.stringify(eofRecoveryTerminal)} text=${JSON.stringify(eofRecoveryText)}`);
    }
    await resumed.close();
    console.log(`${realSentinel} ${token}`);
  } finally {
    const remaining = [...activeRuns].filter((run) => run.child.exitCode === null && run.child.signalCode === null);
    for (const run of remaining) {
      killGroup(run.child);
      run.child.stdin.end();
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
