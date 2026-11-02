#!/usr/bin/env node

import { spawn } from "node:child_process";
import {
  copyFileSync,
  existsSync,
  mkdtempSync,
  mkdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import os from "node:os";
import path from "node:path";

const fakeSentinel = "ORCA_TUI_APPROVAL_RECOVERY_FAKE_OK";
const realSentinel = "ORCA_TUI_APPROVAL_RECOVERY_REAL_OK";

function parseArgs(argv) {
  const args = { bin: null, timeoutMs: 180000 };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--bin") args.bin = argv[++index];
    else if (arg === "--timeout-ms") args.timeoutMs = Number.parseInt(argv[++index], 10);
    else throw new Error(`Unknown argument: ${arg}`);
  }
  if (!args.bin || !path.isAbsolute(args.bin)) throw new Error("--bin must be an absolute path");
  if (!existsSync(args.bin) || !statSync(args.bin).isFile()) {
    throw new Error(`--bin is not an executable file: ${args.bin}`);
  }
  if (!Number.isInteger(args.timeoutMs) || args.timeoutMs <= 0) {
    throw new Error("--timeout-ms must be a positive integer");
  }
  return args;
}

function killGroup(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  try {
    process.kill(-child.pid, "SIGKILL");
  } catch {
    try {
      child.kill("SIGKILL");
    } catch {
      // The child may have exited between the status check and signal.
    }
  }
}

function terminalText(output) {
  return output
    .replace(/\x1b\][\s\S]*?(?:\x07|\x1b\\)/g, "")
    // TUI cell writes can place words in separate cursor-addressed spans.
    .replace(/\x1b\[[0-9;]*[HfG]/g, " ")
    .replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, "")
    .replace(/\x1b[@-_]/g, "");
}

function terminalOccurrences(output, expected) {
  const normalized = terminalText(output);
  return normalized.split(expected).length - 1;
}

function spawnCaptured(command, commandArgs, options, timeoutMs) {
  const child = spawn(command, commandArgs, {
    ...options,
    detached: true,
    stdio: ["pipe", "pipe", "pipe"],
  });
  let output = "";
  let stderr = "";
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stdout.on("data", (chunk) => {
    output += chunk;
  });
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
  });
  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    killGroup(child);
  }, timeoutMs);
  const closed = new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("close", (code, signal) => {
      clearTimeout(timer);
      resolve({ code, signal, timedOut, output, stderr });
    });
  });
  return { child, closed, output: () => output, stderr: () => stderr };
}

function validateFakeTrace(output) {
  const events = terminalText(output)
    .split(/\r?\n/)
    .filter((line) => line.startsWith("TUI_HARNESS "))
    .map((line) => line.slice("TUI_HARNESS ".length).trim().split(/\s+/));
  const expected = [
    ["approval_requested", "request-1"],
    ["approval_resolved", "request-1"],
    ["cancel_committed", "turn-1"],
    ["terminal_committed", "turn-1"],
    ["terminal_flushed", "turn-1"],
    ["restart_recovered", "turn-1"],
  ];
  if (events.length !== expected.length) {
    throw new Error(`TUI fake trace missing event: ${JSON.stringify(events)}`);
  }
  for (let index = 0; index < expected.length; index += 1) {
    if (events[index][0] !== expected[index][0] || events[index][1] !== expected[index][1]) {
      throw new Error(
        `TUI fake trace order or identity mismatch at ${index}: expected=${expected[index].join(" ")} actual=${events[index].join(" ")}`,
      );
    }
  }
}

async function runFake(args) {
  const processRun = spawnCaptured(args.bin, [], { env: process.env }, args.timeoutMs);
  const result = await processRun.closed;
  if (result.timedOut) throw new Error("TUI fake child timed out or remained unreaped");
  if (result.code !== 0) {
    throw new Error(`TUI fake child failed with code ${result.code}: ${result.stderr}`);
  }
  validateFakeTrace(result.output);
  console.log(fakeSentinel);
}

function isolatedHome() {
  const home = mkdtempSync(path.join(os.tmpdir(), "orca-real-tui-home-"));
  const sourceHome = process.env.ORCA_HOME ?? path.join(os.homedir(), ".orca");
  const sourceAuth = path.join(sourceHome, "auth.json");
  if (existsSync(sourceAuth)) copyFileSync(sourceAuth, path.join(home, "auth.json"));
  if (!process.env.ORCA_API_KEY && !existsSync(path.join(home, "auth.json"))) {
    rmSync(home, { recursive: true, force: true });
    throw new Error("DeepSeek credentials are required through ORCA_API_KEY or ORCA_HOME/auth.json");
  }
  return home;
}

async function waitFor(run, expected, timeoutMs, label) {
  return waitForCount(run, expected, 1, timeoutMs, label);
}

async function waitForCount(run, expected, count, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs;
  while (true) {
    if (terminalOccurrences(run.output(), expected) >= count) return;
    const remainingMs = deadline - Date.now();
    if (remainingMs <= 0) {
      // Give a just-delivered PTY chunk one event-loop turn before reporting a
      // timeout. The child and its output streams are independent callbacks.
      await new Promise((resolve) => setImmediate(resolve));
      if (terminalOccurrences(run.output(), expected) >= count) return;
      throw new Error(`${label} missing occurrence ${count} of ${JSON.stringify(expected)}\nstdout:\n${run.output()}\nstderr:\n${run.stderr()}`);
    }
    await new Promise((resolve) => setTimeout(resolve, Math.min(25, remainingMs)));
  }
}

async function waitForOptional(run, expected, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (terminalOccurrences(run.output(), expected) === 0 && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  return terminalOccurrences(run.output(), expected) > 0;
}

function spawnTui(bin, args, env, timeoutMs, cwd) {
  if (process.platform === "win32") throw new Error("TUI real API harness requires a Unix PTY");
  const bridge = String.raw`
import errno, fcntl, os, select, struct, subprocess, sys, termios

master, slave = os.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
child = subprocess.Popen(sys.argv[1:], stdin=slave, stdout=slave, stderr=slave, close_fds=True)
os.close(slave)
stdin_open = True
while True:
    watched = [master]
    if stdin_open:
        watched.append(0)
    readable, _, _ = select.select(watched, [], [], 0.1)
    if master in readable:
        try:
            data = os.read(master, 65536)
        except OSError as error:
            if error.errno == errno.EIO:
                data = b""
            else:
                raise
        if data:
            os.write(1, data)
        elif child.poll() is not None:
            break
    if stdin_open and 0 in readable:
        data = os.read(0, 65536)
        if data:
            os.write(master, data)
        else:
            stdin_open = False
    if child.poll() is not None:
        try:
            while True:
                readable, _, _ = select.select([master], [], [], 0)
                if master not in readable:
                    break
                data = os.read(master, 65536)
                if not data:
                    break
                os.write(1, data)
        except OSError as error:
            if error.errno != errno.EIO:
                raise
        break
os.close(master)
raise SystemExit(child.wait())
`;
  return spawnCaptured("python3", ["-u", "-c", bridge, bin, ...args], { env, cwd }, timeoutMs);
}

async function exitTui(run) {
  run.child.stdin.write(Buffer.from([0x03]));
  if (!(await waitForOptional(run, "Press Ctrl+C again to quit.", 2000))) {
    await new Promise((resolve) => setTimeout(resolve, 500));
    run.child.stdin.write(Buffer.from([0x03]));
    await waitFor(run, "Press Ctrl+C again to quit.", 5000, "TUI idle exit arm");
  }
  run.child.stdin.write(Buffer.from([0x03]));
  run.child.stdin.end();
  const result = await run.closed;
  if (result.timedOut) throw new Error("TUI did not exit after terminal flush");
  if (![0, 130].includes(result.code)) {
    throw new Error(`TUI exited with code ${result.code} signal ${result.signal ?? "none"}: ${result.stderr}`);
  }
}

async function runReal(args) {
  const home = isolatedHome();
  const cwd = path.join(home, "workspace");
  mkdirSync(cwd);
  const token = `ORCA_TUI_RECOVERY_${Date.now()}_${process.pid}`;
  const env = { ...process.env, ORCA_HOME: home, TERM: "xterm-256color" };
  const activeRuns = new Set();
  const startTui = (tuiArgs) => {
    const run = spawnTui(args.bin, tuiArgs, env, args.timeoutMs, cwd);
    activeRuns.add(run);
    void run.closed.then(
      () => activeRuns.delete(run),
      () => activeRuns.delete(run),
    );
    return run;
  };
  try {
    writeFileSync(path.join(cwd, ".orca-release-token"), token);
    const approvalPrompt = [
      "Use request_permissions to request write access to the current workspace,",
      "then use bash to run exactly: cat .orca-release-token.",
      "Do not read the file with another tool or repeat its contents before bash succeeds.",
    ].join(" ");
    const approval = startTui(["--cwd", cwd, "--mode", "suggest", approvalPrompt]);
    await waitFor(approval, "Continue untrusted", args.timeoutMs, "fresh workspace welcome");
    approval.child.stdin.write("u\r");
    await waitFor(approval, "Filesystem Permission Required", args.timeoutMs, "permission request");
    approval.child.stdin.write("1");
    await waitFor(approval, "Approval Required", args.timeoutMs, "tool approval");
    approval.child.stdin.write("1");
    if (await waitForOptional(approval, "Unsandboxed Shell Required", 10000)) {
      approval.child.stdin.write("1");
    }
    await waitFor(approval, token, args.timeoutMs, "approved turn terminal content");
    await exitTui(approval);

    const resumed = startTui([
      "--cwd",
      cwd,
      "--resume",
      "latest",
      "Reply with the exact ORCA_TUI_RECOVERY token from history.",
    ]);
    await waitFor(resumed, token, args.timeoutMs, "restart recovery");
    await exitTui(resumed);

    const cancellation = startTui([
      "--cwd",
      cwd,
      "Write 200 numbered lines slowly. Each line must contain ORCA_CANCEL_STREAM and no tools.",
    ]);
    await waitForCount(cancellation, "ORCA_CANCEL_STREAM", 2, args.timeoutMs, "cancellable stream");
    cancellation.child.stdin.write(Buffer.from([0x03]));
    await new Promise((resolve) => setTimeout(resolve, 300));
    await exitTui(cancellation);
    console.log(`${realSentinel} ${token}`);
  } finally {
    const remaining = [...activeRuns];
    for (const run of remaining) {
      killGroup(run.child);
      run.child.stdin.end();
    }
    await Promise.allSettled(remaining.map((run) => run.closed));
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
