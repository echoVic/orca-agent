#!/usr/bin/env node

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const bin = path.resolve(process.argv[2] ?? "target/debug/orca");
let root;

async function run(cwd, env, prompt, session, mode) {
  const args = ["exec", "--cwd", cwd, "--mode", "full-auto", "--model", "deepseek-flash",
    "--output-format", "jsonl", "--save-history", "--max-turns", "8", "--max-tool-calls", "8",
    "--max-wall-time-secs", "120"];
  if (mode === "sync") args.push("--max-cost-usd", "0.20");
  if (session) args.push("--resume", session);
  args.push(prompt);
  const child = spawn(bin, args, { env, detached: true, stdio: ["ignore", "pipe", "pipe"] });
  let output = "";
  let stderr = "";
  child.stdout.on("data", chunk => { output += chunk; });
  child.stderr.on("data", chunk => { stderr = (stderr + chunk).slice(-8192); });
  const kill = () => { try { process.kill(-child.pid, "SIGKILL"); } catch {} };
  const timer = setTimeout(kill, 150_000);
  try {
    const code = await new Promise((resolve, reject) => {
      child.once("error", reject);
      child.once("close", resolve);
    });
    const events = output.split(/\r?\n/).filter(Boolean).map(line => JSON.parse(line));
    assert.equal(code, 0, `custom-agent CLI failed: ${stderr}\n${JSON.stringify(events.filter(event => event.type === "error" || event.type === "tool.call.completed" || event.type === "session.completed")).slice(-6000)}`);
    const errors = events.filter(event => event.type === "error");
    assert.equal(errors.length, 0, JSON.stringify(errors));
    return events;
  } finally {
    clearTimeout(timer);
    if (child.exitCode === null && child.signalCode === null) kill();
  }
}

function childResult(events, mode) {
  const calls = events.filter(event => event.type === "tool.call.completed"
    && event.payload.name === "subagent");
  assert.equal(calls.length, 1, "must invoke exactly one custom subagent");
  assert.equal(calls[0].payload.status, "completed");
  const output = calls[0].payload.output;
  let continuation;
  if (mode === "async") {
    const launch = JSON.parse(output);
    assert.equal(launch.status, "async_launched");
    continuation = launch.continuation_id;
  } else {
    continuation = output.split("\n")
      .find(line => line.startsWith("resume_from="))?.slice("resume_from=".length).trim();
  }
  assert.ok(continuation, `missing continuation in child result: ${output.slice(-1400)}`);
  const session = events.find(event => event.type === "session.completed")?.payload.session_id;
  assert.ok(session, "the parent must be recorded");
  assert.match(session, /^[0-9a-f-]{36}$/);
  assert.match(continuation, /^[0-9a-f-]{36}$/);
  return { session, continuation };
}

function verifyCheckpoint(home, identity, token, original, previous) {
  const record = JSON.parse(readFileSync(path.join(home, "task-sessions",
    identity.session, "continuations", `${identity.continuation}.json`), "utf8"));
  assert.equal(record.terminal?.status, "completed", "the child must finish before parent exit");
  assert.equal(record.terminal.result?.trim(), token, "the child must obey the frozen definition");
  assert.deepEqual(record.frozen_agent.definition.allowed_tools, []);
  assert.ok(record.checkpoint, "completion must include a safe checkpoint");
  if (previous) {
    assert.equal(record.source_task_id, original.source_task_id);
    assert.equal(record.parent_session_id, original.parent_session_id);
    assert.deepEqual(record.compatibility_hash, original.compatibility_hash);
    assert.deepEqual(record.frozen_agent, original.frozen_agent);
    assert.notEqual(record.parent_task_id, previous.parent_task_id);
    assert.notEqual(record.latest_task_id, previous.latest_task_id);
    assert.notEqual(record.current_attempt.attempt_id, previous.current_attempt.attempt_id);
    assert.equal(record.current_attempt.resumed_from_attempt_id, previous.current_attempt.attempt_id);
    assert.ok(record.revision > previous.revision);
    assert.ok(record.checkpoint.sequence > previous.checkpoint.sequence);
  }
  return record;
}

async function cleanupWorkers() {
  const sessions = path.join(root, "home", "task-sessions");
  if (!existsSync(sessions)) return;
  for (const entry of readdirSync(sessions, { withFileTypes: true })) {
    const tasks = path.join(sessions, entry.name, "tasks.json");
    if (!entry.isDirectory() || !existsSync(tasks)) continue;
    for (const [id, task] of Object.entries(JSON.parse(readFileSync(tasks, "utf8")))) {
      const pid = task.worker_pid;
      if (!Number.isSafeInteger(pid) || pid <= 1) continue;
      const owned = () => {
        const probe = spawnSync("ps", ["-p", String(pid), "-o", "command="], { encoding: "utf8" });
        assert.ifError(probe.error);
        const command = probe.stdout.trim();
        return command.startsWith(`${bin} subagent-worker `)
          && command.includes(`--cwd ${path.join(root, "workspace")} `)
          && command.includes(`--agent-id ${id} `);
      };
      if (!owned()) continue;
      const signal = value => {
        try { process.kill(pid, value); } catch (error) { if (error.code !== "ESRCH") throw error; }
      };
      signal("SIGTERM");
      for (let count = 0; count < 50 && owned(); count++) {
        await new Promise(resolve => setTimeout(resolve, 100));
      }
      if (owned()) {
        signal("SIGKILL");
        for (let count = 0; count < 50 && owned(); count++) {
          await new Promise(resolve => setTimeout(resolve, 100));
        }
      }
      assert.ok(!owned(), `owned worker ${pid} did not exit`);
    }
  }
}

try {
  const sourceHome = process.env.ORCA_HOME ?? path.join(os.homedir(), ".orca");
  let key = process.env.ORCA_API_KEY ?? process.env.DEEPSEEK_API_KEY;
  if (!key && existsSync(path.join(sourceHome, "auth.json"))) {
    key = JSON.parse(readFileSync(path.join(sourceHome, "auth.json"), "utf8")).DEEPSEEK_API_KEY;
  }
  assert.ok(key, "DeepSeek credentials required; absent credentials are not a pass");
  root = realpathSync(mkdtempSync("/tmp/orca-agent-acceptance-"));
  writeFileSync(path.join(root, "ownership.json"), JSON.stringify({ root, pid: process.pid, purpose: "custom agent acceptance" }), { mode: 0o600 });
  const home = path.join(root, "home");
  const cwd = path.join(root, "workspace");
  mkdirSync(path.join(home, "agents"), { recursive: true, mode: 0o700 });
  mkdirSync(cwd);
  writeFileSync(path.join(home, "config.toml"), "update_check = false\nauto_memory = false\n");
  const token = `FROZEN_AGENT_${Date.now()}`;
  const definition = path.join(home, "agents", "contract-proof.md");
  writeFileSync(definition, `---\nname: contract-proof\ndescription: Return the identifier stored in immutable agent instructions\ntools: []\nmodel: deepseek-flash\n---\nYour only task is to return this identifier: ${token}\nEvery answer must contain exactly that identifier, with no explanation, reasoning, Markdown, or tool calls. You have no tools and must not inspect tasks or files.\n`);
  const env = { ...process.env, ORCA_HOME: home, ORCA_API_KEY: key };
  let identity;
  let original;
  let previous;
  for (const mode of ["sync", "sync", "async", "async", "sync"]) {
    const selector = identity
      ? `resume_from ${identity.continuation}; do not specify subagent_type or model`
      : "subagent_type contract-proof";
    const completion = mode === "async"
      ? "After launch, poll subagent_status with the returned agent_id until completed, then relay the child reply. Do not end your turn before the child completes."
      : "After the subagent returns, relay its reply exactly.";
    const prompt = `Call the subagent tool exactly once with ${selector}, description Verify frozen instructions, prompt "Return the exact identifier defined in your system instructions. Do not inspect tasks or files and do not call tools.", mode ${mode}. Use no tools other than subagent and subagent_status. ${completion}`;
    const events = await run(cwd, env, prompt, identity?.session, mode);
    const next = childResult(events, mode);
    if (identity) assert.deepEqual(next, identity);
    identity = next;
    const record = verifyCheckpoint(home, identity, token, original, previous);
    if (!original) {
      original = record;
      rmSync(definition);
    }
    previous = record;
    console.log(`Custom agent real API: ${mode} completed, frozen checkpoint verified`);
  }
  console.log("Custom agent real API: discovery, deleted definition, sync/sync, sync/async, async/async, async/sync separate-process recovery verified");
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
} finally {
  if (root) {
    try {
      await cleanupWorkers();
      if (process.env.ORCA_ACCEPTANCE_KEEP === "1") {
        console.log(`Retained isolated acceptance artifacts: ${root}`);
      } else {
        rmSync(root, { recursive: true, force: true });
      }
    } catch (error) {
      console.error(`Cleanup failed; retained ${root}: ${error.message}`);
      process.exitCode = 1;
    }
  }
}
