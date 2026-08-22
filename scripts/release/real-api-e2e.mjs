#!/usr/bin/env node

import { execFileSync, spawn } from "node:child_process";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import os from "node:os";
import readline from "node:readline";
import path from "node:path";
import { isDeepStrictEqual } from "node:util";

const repoRoot = path.resolve(import.meta.dirname, "..", "..");
const cliSentinel = "ORCA_REAL_E2E_OK";
const historyReplaySentinel = "ORCA_HISTORY_REPLAY_OK";
const historyReplayCallId = "legacy-missing-tool-call";
const stableThreadIdentitySentinel = `ORCA_STABLE_THREAD_IDENTITY_OK_${Date.now()}_${process.pid}`;
const stableThreadIdentityReadySentinel = "ORCA_STABLE_THREAD_IDENTITY_READY";
const serverSentinel = "ORCA_SERVER_REAL_OK";
const serverThreadSentinel = `ORCA_SERVER_THREAD_MEMORY_OK_${Date.now()}_${process.pid}`;
const serverThreadReadySentinel = "READY";
const serverThreadTitle = `ORCA server thread metadata e2e ${serverThreadSentinel}`;
const serverResumeSentinel = `ORCA_SERVER_RESUME_OK_${Date.now()}_${process.pid}`;

function parseArgs(argv) {
  const args = {
    orcaBin: path.join(repoRoot, "target", "debug", "orca"),
    maxBudget: "0.02",
    timeoutMs: 180000,
    skipBuild: false,
    skipProviderSummary: false,
    skipProviderVision: false,
    skipGoalMode: false,
    skipCli: false,
    skipServer: false,
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--orca-bin") {
      args.orcaBin = argv[++index];
    } else if (arg === "--max-budget") {
      args.maxBudget = argv[++index];
    } else if (arg === "--timeout-ms") {
      args.timeoutMs = Number.parseInt(argv[++index], 10);
    } else if (arg === "--skip-build") {
      args.skipBuild = true;
    } else if (arg === "--skip-provider-summary") {
      args.skipProviderSummary = true;
    } else if (arg === "--skip-provider-vision") {
      args.skipProviderVision = true;
    } else if (arg === "--skip-goal-mode") {
      args.skipGoalMode = true;
    } else if (arg === "--skip-cli") {
      args.skipCli = true;
    } else if (arg === "--skip-server") {
      args.skipServer = true;
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }

  if (!args.orcaBin) {
    throw new Error("--orca-bin must not be empty");
  }
  if (!args.maxBudget || Number.isNaN(Number(args.maxBudget)) || Number(args.maxBudget) <= 0) {
    throw new Error("--max-budget must be a positive number");
  }
  if (!Number.isInteger(args.timeoutMs) || args.timeoutMs <= 0) {
    throw new Error("--timeout-ms must be a positive integer");
  }

  return args;
}

function commandLabel(command, args) {
  return [command, ...args].join(" ");
}

function run(command, args, options = {}) {
  try {
    return execFileSync(command, args, {
      cwd: repoRoot,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      timeout: options.timeoutMs,
      ...options,
    });
  } catch (error) {
    const stdout = error.stdout ? `\nstdout:\n${error.stdout}` : "";
    const stderr = error.stderr ? `\nstderr:\n${error.stderr}` : "";
    throw new Error(`Command failed: ${commandLabel(command, args)}${stdout}${stderr}`);
  }
}

function killProcessGroup(child) {
  if (child.exitCode !== null || child.signalCode !== null) {
    return;
  }
  try {
    process.kill(-child.pid, "SIGKILL");
  } catch {
    try {
      child.kill("SIGKILL");
    } catch {
      // The child may exit between the status check and signal.
    }
  }
}

function runBuild(args) {
  if (args.skipBuild) {
    console.log("Build skipped");
    return;
  }

  run("cargo", ["build", "--bin", "orca"], { timeoutMs: args.timeoutMs });
  console.log("Build verified");
}

function runProviderSummary(args) {
  if (args.skipProviderSummary) {
    console.log("Provider summary real API e2e skipped");
    return;
  }

  const output = run("cargo", ["run", "-p", "orca-provider", "--example", "summary_render_realapi"], {
    timeoutMs: args.timeoutMs,
  });
  if (!output.includes("ALL TARGETS MET")) {
    throw new Error(`Provider summary real API e2e did not report ALL TARGETS MET:\n${output}`);
  }
  console.log("Provider summary real API e2e verified");
}

function runProviderVision(args) {
  if (args.skipProviderVision) {
    console.log("Provider vision real API e2e skipped");
    return;
  }

  const output = run("cargo", ["run", "-p", "orca-provider", "--example", "vision_realapi"], {
    timeoutMs: args.timeoutMs,
  });
  if (!output.includes("vision smoke ok:")) {
    throw new Error(`Provider vision real API e2e did not report success:\n${output}`);
  }
  console.log("Provider vision real API e2e verified");
}

function parseGoalMetrics(line, label) {
  const separator = line.indexOf(":");
  if (separator < 0) {
    throw new Error(`${label} is missing a metric separator: ${line}`);
  }
  const entries = line
    .slice(separator + 1)
    .trim()
    .split(/\s+/)
    .map((entry) => entry.split("="));
  if (entries.some(([key, value, extra]) => !key || value === undefined || extra !== undefined)) {
    throw new Error(`${label} contains a malformed metric: ${line}`);
  }
  return Object.fromEntries(entries);
}

function requireGoalInteger(metrics, key, { positive = false } = {}) {
  const value = Number(metrics[key]);
  if (!Number.isInteger(value) || (positive ? value <= 0 : value < 0)) {
    const qualifier = positive ? "positive " : "non-negative ";
    throw new Error(`Goal Mode real API missing ${qualifier}${key}: ${JSON.stringify(metrics)}`);
  }
  return value;
}

function runGoalMode(args) {
  if (args.skipGoalMode) {
    console.log("Goal Mode real API e2e skipped");
    return;
  }

  const home = mkdtempSync(path.join(os.tmpdir(), "orca-goal-mode-e2e-"));
  const sourceHome = process.env.ORCA_HOME ?? path.join(os.homedir(), ".orca");
  const sourceAuthPath = path.join(sourceHome, "auth.json");
  if (existsSync(sourceAuthPath)) {
    copyFileSync(sourceAuthPath, path.join(home, "auth.json"));
  }

  try {
    const output = run(
      "cargo",
      [
        "run",
        "-p",
        "orca-runtime",
        "--example",
        "goal_mode_realapi",
        "--",
        "--max-budget",
        args.maxBudget,
      ],
      { env: { ...process.env, ORCA_HOME: home }, timeoutMs: args.timeoutMs },
    );
    const lines = output.split(/\r?\n/);
    const scenarioLines = lines.filter((line) =>
      line.startsWith("Goal Mode real API scenario verified:"),
    );
    const scenarios = new Map();
    for (const line of scenarioLines) {
      const metrics = parseGoalMetrics(line, "Goal Mode real API scenario");
      if (!metrics.scenario || scenarios.has(metrics.scenario)) {
        throw new Error(`Goal Mode real API duplicate or unnamed scenario: ${line}`);
      }
      scenarios.set(metrics.scenario, metrics);
    }

    const expected = {
      completion: { state: "complete", reason: "verified_complete", verifier: true },
      rejected_completion: {
        state: "paused",
        reason: "paused",
        rejectionCode: "plan_mode",
        rejected: true,
      },
      blocked: { state: "blocked", reason: "verified_blocked", verifier: true },
      cancellation: {
        state: "paused",
        reason: "paused",
        pauseReason: "user",
        cancelled: true,
      },
      resume: {
        state: "complete",
        reason: "verified_complete",
        verifier: true,
        resumed: true,
      },
    };
    if (
      scenarios.size !== Object.keys(expected).length ||
      Object.keys(expected).some((scenario) => !scenarios.has(scenario))
    ) {
      throw new Error(
        `Goal Mode real API scenario matrix mismatch: expected=${Object.keys(expected).join(",")} actual=${[
          ...scenarios.keys(),
        ].join(",")}`,
      );
    }

    for (const [scenario, contract] of Object.entries(expected)) {
      const metrics = scenarios.get(scenario);
      for (const key of ["outer_turns", "usage_events", "charged_tokens", "journal_goal_events"]) {
        requireGoalInteger(metrics, key, { positive: true });
      }
      for (const key of [
        "update_goal_requests",
        "update_goal_acks",
        "accepted_acks",
        "rejected_acks",
        "persisted_intents",
        "verifier_outcomes",
        "verifier_tokens",
        "cost_micros",
        "continuations",
        "stale_continuations",
        "in_flight_runs",
      ]) {
        requireGoalInteger(metrics, key);
      }
      if (
        metrics.state !== contract.state ||
        metrics.reason !== contract.reason ||
        (contract.rejectionCode && metrics.rejection_code !== contract.rejectionCode) ||
        (contract.pauseReason && metrics.pause_reason !== contract.pauseReason) ||
        metrics.update_goal_requests !== metrics.update_goal_acks ||
        Number(metrics.accepted_acks) + Number(metrics.rejected_acks) !==
          Number(metrics.update_goal_acks) ||
        metrics.continuations !== "0" ||
        metrics.stale_continuations !== "0" ||
        metrics.in_flight_runs !== "0"
      ) {
        throw new Error(`Goal Mode real API ${scenario} audit mismatch: ${JSON.stringify(metrics)}`);
      }
      if (metrics.persisted_intents !== metrics.accepted_acks) {
        throw new Error(
          `Goal Mode real API ${scenario} persistence mismatch: ${JSON.stringify(metrics)}`,
        );
      }
      if (
        contract.verifier &&
        (requireGoalInteger(metrics, "verifier_outcomes", { positive: true }) < 1 ||
          requireGoalInteger(metrics, "verifier_tokens", { positive: true }) < 1)
      ) {
        throw new Error(`Goal Mode real API ${scenario} missed verifier audit`);
      }
      if (
        contract.rejected &&
        (requireGoalInteger(metrics, "rejected_acks", { positive: true }) < 1 ||
          metrics.verifier_outcomes !== "0" ||
          metrics.verifier_tokens !== "0")
      ) {
        throw new Error(`Goal Mode real API ${scenario} was not rejected before verification`);
      }
      if (
        contract.cancelled &&
        (metrics.update_goal_requests !== "0" ||
          metrics.update_goal_acks !== "0" ||
          metrics.verifier_outcomes !== "0")
      ) {
        throw new Error(`Goal Mode real API ${scenario} crossed a terminal-intent boundary`);
      }
      if (contract.resumed && requireGoalInteger(metrics, "resume_turns", { positive: true }) < 1) {
        throw new Error(`Goal Mode real API ${scenario} did not record a resume-origin turn`);
      }
    }

    const summary = lines.find((line) => line.startsWith("Goal Mode real API e2e verified:"));
    if (!summary) {
      throw new Error(`Goal Mode real API e2e did not report success:\n${output}`);
    }
    const aggregate = parseGoalMetrics(summary, "Goal Mode real API aggregate");
    if (
      aggregate.scenarios !== String(Object.keys(expected).length) ||
      aggregate.stale_continuations !== "0" ||
      aggregate.in_flight_runs !== "0"
    ) {
      throw new Error(`Goal Mode real API aggregate mismatch: ${summary}`);
    }
    console.log(output.trim());
  } finally {
    rmSync(home, { recursive: true, force: true });
  }
}

function parseJsonLines(output, label) {
  return output
    .split(/\r?\n/)
    .filter((line) => line.trim().length > 0)
    .map((line) => {
      try {
        return JSON.parse(line);
      } catch (error) {
        throw new Error(`Unable to parse ${label} JSONL line: ${error.message}\n${line}`);
      }
    });
}

function assertStatus(events, typeField, expectedType, statusPath, label) {
  const completed = events.find((event) => event[typeField] === expectedType);
  if (!completed) {
    throw new Error(`${label} did not emit ${expectedType}`);
  }

  let status = completed;
  for (const key of statusPath) {
    status = status?.[key];
  }
  if (status !== "success") {
    throw new Error(`${label} completed with unexpected status: ${JSON.stringify(completed)}`);
  }
}

function runCli(args) {
  if (args.skipCli) {
    console.log("CLI real API e2e skipped");
    return;
  }

  const output = run(
    args.orcaBin,
    [
      "exec",
      "--output-format",
      "jsonl",
      "--no-history",
      "--mode",
      "suggest",
      "--max-budget",
      args.maxBudget,
      `Reply with exactly: ${cliSentinel}`,
    ],
    { timeoutMs: args.timeoutMs },
  );
  const events = parseJsonLines(output, "CLI");
  const text = events
    .filter((event) => event.type === "assistant.message.delta")
    .map((event) => event.payload?.text ?? "")
    .join("");

  if (!text.includes(cliSentinel)) {
    throw new Error(`CLI real API e2e missing sentinel ${cliSentinel}:\n${output}`);
  }
  assertStatus(events, "type", "session.completed", ["payload", "status"], "CLI real API e2e");
  console.log(`CLI real API e2e verified: ${cliSentinel}`);
}

function runHistoryReplay(args) {
  if (args.skipCli) {
    console.log("History replay real API e2e skipped");
    return;
  }

  const home = mkdtempSync(path.join(os.tmpdir(), "orca-history-replay-e2e-"));
  const sourceHome = process.env.ORCA_HOME ?? path.join(os.homedir(), ".orca");
  const sourceAuthPath = path.join(sourceHome, "auth.json");
  if (existsSync(sourceAuthPath)) {
    copyFileSync(sourceAuthPath, path.join(home, "auth.json"));
  }
  const sessionDir = path.join(home, "sessions", "2026", "07", "11");
  const sessionPath = path.join(
    sessionDir,
    "session-2026-07-11T00-00-00-history-replay-e2e.jsonl",
  );
  const sideEffectPath = path.join(home, "legacy-missing-tool-call-reexecuted");
  mkdirSync(sessionDir, { recursive: true });
  const records = [
    {
      type: "session.meta",
      schema_version: 1,
      session_id: "history-replay-e2e",
      cwd: repoRoot,
      provider: "deepseek",
      model: "deepseek-v4-flash",
      title: "History replay validity e2e",
      created_at: "2026-07-11T00:00:00Z",
    },
    {
      type: "conversation.message",
      message: {
        role: "user",
        content: "Legacy valid user context.",
        pinned: false,
      },
    },
    {
      type: "conversation.message",
      message: {
        role: "assistant",
        content: null,
        reasoning_content: "synthetic incomplete tool invocation",
        tool_calls: [
          {
            id: historyReplayCallId,
            function_name: "bash",
            arguments: JSON.stringify({
              command: `touch ${JSON.stringify(sideEffectPath)}`,
            }),
          },
        ],
        pinned: false,
      },
    },
  ];
  writeFileSync(
    sessionPath,
    `${records.map((record) => JSON.stringify(record)).join("\n")}\n`,
  );

  try {
    const output = run(
      args.orcaBin,
      [
        "exec",
        "--output-format",
        "jsonl",
        "--mode",
        "full-auto",
        "--max-budget",
        args.maxBudget,
        "--resume",
        "latest",
        `Do not call tools or retry prior work. Reply with exactly: ${historyReplaySentinel}`,
      ],
      {
        env: { ...process.env, ORCA_HOME: home },
        timeoutMs: args.timeoutMs,
      },
    );
    const events = parseJsonLines(output, "history replay CLI");
    const text = events
      .filter((event) => event.type === "assistant.message.delta")
      .map((event) => event.payload?.text ?? "")
      .join("");
    if (!text.includes(historyReplaySentinel)) {
      throw new Error(
        `History replay real API e2e missing sentinel ${historyReplaySentinel}:\n${output}`,
      );
    }
    assertStatus(
      events,
      "type",
      "session.completed",
      ["payload", "status"],
      "History replay real API e2e",
    );
    if (existsSync(sideEffectPath)) {
      throw new Error(
        `History replay re-executed missing invocation ${historyReplayCallId}: ${sideEffectPath}`,
      );
    }

    const inspectRequest = JSON.stringify({
      id: "history-replay-items",
      method: "thread/items/list",
      params: {
        threadId: "history-replay-e2e",
        limit: 20,
      },
    });
    const inspectOutput = run(args.orcaBin, ["--mode", "server"], {
      env: { ...process.env, ORCA_HOME: home },
      input: `${inspectRequest}\n`,
      stdio: ["pipe", "pipe", "pipe"],
      timeoutMs: args.timeoutMs,
    });
    const inspectEvents = parseJsonLines(inspectOutput, "history replay thread items");
    const itemsEvent = inspectEvents.find(
      (event) => event.id === "history-replay-items" && event.event === "thread_items_list",
    );
    const repairedItem = itemsEvent?.data?.find?.(
      (entry) => entry.item?.id === historyReplayCallId,
    )?.item;
    if (
      repairedItem?.status !== "indeterminate" ||
      repairedItem?.kind !== "indeterminate" ||
      repairedItem?.terminalSource !== "compatibility_repair"
    ) {
      throw new Error(
        `History replay did not expose repaired terminal ${historyReplayCallId}: ${inspectOutput}`,
      );
    }

    console.log(`History replay real API e2e verified: ${historyReplaySentinel}`);
    console.log(
      `History replay repair verified: ${historyReplayCallId} status=indeterminate terminalSource=compatibility_repair`,
    );
    console.log(`History replay invocation not re-executed: ${historyReplayCallId}`);
  } finally {
    rmSync(home, { recursive: true, force: true });
  }
}

function readStableThreadProjection(args, home, threadId, label) {
  const requests = [
    {
      id: "stable-identity-turns",
      method: "thread/turns/list",
      params: { threadId, limit: 100 },
    },
    {
      id: "stable-identity-items",
      method: "thread/items/list",
      params: { threadId, limit: 100 },
    },
  ];
  const output = run(args.orcaBin, ["--mode", "server"], {
    env: { ...process.env, ORCA_HOME: home },
    input: `${requests.map((request) => JSON.stringify(request)).join("\n")}\n`,
    stdio: ["pipe", "pipe", "pipe"],
    timeoutMs: args.timeoutMs,
  });
  const events = parseJsonLines(output, label);
  const turnsEvent = events.find(
    (event) => event.id === "stable-identity-turns" && event.event === "thread_turns_list",
  );
  const itemsEvent = events.find(
    (event) => event.id === "stable-identity-items" && event.event === "thread_items_list",
  );
  if (!turnsEvent || !itemsEvent) {
    throw new Error(`${label} did not return turn and item projections:\n${output}`);
  }

  const turns = Array.isArray(turnsEvent.data) ? turnsEvent.data : [];
  const itemEntries = Array.isArray(itemsEvent.data) ? itemsEvent.data : [];
  const turnIds = turns.map((turn) => turn.turnId);
  const itemIds = itemEntries.map((entry) => entry.itemId);
  const itemObjects = itemEntries.map((entry) => entry.item);
  const canonicalItems = itemEntries.filter((entry) =>
    ["agent_message", "reasoning", "plan"].includes(entry.item?.type),
  );
  const userItemTurnIds = itemEntries
    .filter((entry) => entry.item?.role === "user")
    .map((entry) => entry.turnId);
  if (
    turnIds.length === 0 ||
    itemIds.length === 0 ||
    turnIds.some((id) => typeof id !== "string" || !id.startsWith("turn_")) ||
    itemIds.some((id) => typeof id !== "string" || !id.startsWith("item_")) ||
    new Set(turnIds).size !== turnIds.length ||
    new Set(itemIds).size !== itemIds.length
  ) {
    throw new Error(
      `${label} did not expose unique typed turn/item ids: ${JSON.stringify({ turnIds, itemIds })}`,
    );
  }
  if (
    !canonicalItems.some((entry) => entry.item?.type === "agent_message") ||
    canonicalItems.some((entry) => entry.item?.id !== entry.itemId)
  ) {
    throw new Error(
      `${label} did not expose canonical completed model item objects: ${JSON.stringify(itemEntries)}`,
    );
  }
  if (
    userItemTurnIds.length !== turnIds.length ||
    turnIds.some(
      (turnId) => userItemTurnIds.filter((userTurnId) => userTurnId === turnId).length !== 1,
    )
  ) {
    throw new Error(
      `${label} did not expose exactly one user item per turn: ${JSON.stringify({ turnIds, userItemTurnIds, itemEntries })}`,
    );
  }
  return { turnIds, itemIds, itemObjects };
}

function assertStablePrefix(before, after, label) {
  if (
    after.length <= before.length ||
    before.some((value, index) => after[index] !== value)
  ) {
    throw new Error(
      `${label} did not preserve the prior identity prefix: ${JSON.stringify({ before, after })}`,
    );
  }
}

function assertStableObjectPrefix(before, after, label) {
  if (
    after.length <= before.length ||
    before.some((value, index) => !isDeepStrictEqual(after[index], value))
  ) {
    throw new Error(
      `${label} did not preserve the prior object prefix: ${JSON.stringify({ before, after })}`,
    );
  }
}

function canonicalAgentMessageIncludes(item, expectedText) {
  return (
    item?.type === "agent_message" &&
    typeof item.id === "string" &&
    item.id.startsWith("item_") &&
    String(item.text ?? "").includes(expectedText)
  );
}

function canonicalAgentMessageEntryIncludes(entry, expectedText) {
  return (
    typeof entry?.itemId === "string" &&
    entry.itemId === entry.item?.id &&
    canonicalAgentMessageIncludes(entry.item, expectedText)
  );
}

function runStableThreadIdentityResume(args) {
  if (args.skipCli || args.skipServer) {
    console.log("Stable thread identity resume real API e2e skipped");
    return;
  }

  const home = mkdtempSync(path.join(os.tmpdir(), "orca-stable-thread-identity-e2e-"));
  const sourceHome = process.env.ORCA_HOME ?? path.join(os.homedir(), ".orca");
  const sourceAuthPath = path.join(sourceHome, "auth.json");
  if (existsSync(sourceAuthPath)) {
    copyFileSync(sourceAuthPath, path.join(home, "auth.json"));
  }
  const env = { ...process.env, ORCA_HOME: home };

  try {
    const firstOutput = run(
      args.orcaBin,
      [
        "exec",
        "--output-format",
        "jsonl",
        "--save-history",
        "--mode",
        "suggest",
        "--max-budget",
        args.maxBudget,
        `Remember this exact token for the next process: ${stableThreadIdentitySentinel}. Reply with exactly: ${stableThreadIdentityReadySentinel}`,
      ],
      { env, timeoutMs: args.timeoutMs },
    );
    const firstEvents = parseJsonLines(firstOutput, "stable identity first process");
    const firstText = firstEvents
      .filter((event) => event.type === "assistant.message.delta")
      .map((event) => event.payload?.text ?? "")
      .join("");
    if (!firstText.includes(stableThreadIdentityReadySentinel)) {
      throw new Error(`Stable identity first process missing ready sentinel:\n${firstOutput}`);
    }
    assertStatus(
      firstEvents,
      "type",
      "session.completed",
      ["payload", "status"],
      "Stable identity first process",
    );

    const threadId = firstEvents.find((event) => event.type === "session.started")?.run_id;
    const firstTurnId = firstEvents.find((event) => event.type === "turn.started")?.payload
      ?.turn_id;
    if (
      typeof threadId !== "string" ||
      threadId.length === 0 ||
      typeof firstTurnId !== "string" ||
      !firstTurnId.startsWith("turn_")
    ) {
      throw new Error(`Stable identity first process did not expose typed identity:\n${firstOutput}`);
    }
    const firstProjection = readStableThreadProjection(
      args,
      home,
      threadId,
      "stable identity first cold projection",
    );
    if (!firstProjection.turnIds.includes(firstTurnId)) {
      throw new Error(
        `Stable identity first turn event was not persisted: ${JSON.stringify({ firstTurnId, firstProjection })}`,
      );
    }

    const resumedOutput = run(
      args.orcaBin,
      [
        "exec",
        "--output-format",
        "jsonl",
        "--save-history",
        "--mode",
        "suggest",
        "--max-budget",
        args.maxBudget,
        "--resume",
        threadId,
        "Reply with exactly the token I asked you to remember in the previous process.",
      ],
      { env, timeoutMs: args.timeoutMs },
    );
    const resumedEvents = parseJsonLines(resumedOutput, "stable identity resumed process");
    const resumedText = resumedEvents
      .filter((event) => event.type === "assistant.message.delta")
      .map((event) => event.payload?.text ?? "")
      .join("");
    if (!resumedText.includes(stableThreadIdentitySentinel)) {
      throw new Error(
        `Stable identity resumed process lost DeepSeek context ${stableThreadIdentitySentinel}:\n${resumedOutput}`,
      );
    }
    assertStatus(
      resumedEvents,
      "type",
      "session.completed",
      ["payload", "status"],
      "Stable identity resumed process",
    );
    if (
      resumedEvents.find((event) => event.type === "session.started")?.run_id !== threadId
    ) {
      throw new Error(`Stable identity resumed process changed thread id:\n${resumedOutput}`);
    }
    const resumedTurnId = resumedEvents.find((event) => event.type === "turn.started")
      ?.payload?.turn_id;
    if (
      typeof resumedTurnId !== "string" ||
      !resumedTurnId.startsWith("turn_") ||
      resumedTurnId === firstTurnId
    ) {
      throw new Error(`Stable identity resumed process did not mint a new typed turn id:\n${resumedOutput}`);
    }

    const resumedProjection = readStableThreadProjection(
      args,
      home,
      threadId,
      "stable identity resumed cold projection",
    );
    assertStablePrefix(firstProjection.turnIds, resumedProjection.turnIds, "Stable turn ids");
    assertStablePrefix(firstProjection.itemIds, resumedProjection.itemIds, "Stable item ids");
    assertStableObjectPrefix(
      firstProjection.itemObjects,
      resumedProjection.itemObjects,
      "Stable completed model items",
    );
    if (!resumedProjection.turnIds.includes(resumedTurnId)) {
      throw new Error(
        `Stable identity resumed turn event was not persisted: ${JSON.stringify({ resumedTurnId, resumedProjection })}`,
      );
    }

    console.log(
      `Stable thread identity resume real API e2e verified: ${stableThreadIdentitySentinel}`,
    );
    console.log(
      `Stable completed model item objects preserved: ${firstProjection.itemObjects.length}`,
    );
  } finally {
    rmSync(home, { recursive: true, force: true });
  }
}

function runServerSubmit(args) {
  if (args.skipServer) {
    console.log("Server real API e2e skipped");
    return;
  }

  const request = JSON.stringify({
    id: 101,
    op: "submit",
    prompt: `Reply with exactly: ${serverSentinel}`,
  });
  const output = run(args.orcaBin, ["--mode", "server"], {
    input: `${request}\n`,
    stdio: ["pipe", "pipe", "pipe"],
    timeoutMs: args.timeoutMs,
  });
  const events = parseJsonLines(output, "server");
  const text = events
    .filter((event) => event.event === "message_delta")
    .map((event) => event.text ?? "")
    .join("");

  if (!text.includes(serverSentinel)) {
    throw new Error(`Server real API e2e missing sentinel ${serverSentinel}:\n${output}`);
  }
  assertStatus(events, "event", "turn_completed", ["status"], "Server real API e2e");
  console.log(`Server real API e2e verified: ${serverSentinel}`);
}

async function runServerThread(args) {
  const child = spawn(args.orcaBin, ["--mode", "server"], {
    cwd: repoRoot,
    detached: true,
    stdio: ["pipe", "pipe", "pipe"],
  });

  let stderr = "";
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
  });

  let timedOut = false;
  const timeout = setTimeout(() => {
    timedOut = true;
    killProcessGroup(child);
  }, args.timeoutMs);

  const closed = new Promise((resolve, reject) => {
    child.on("error", reject);
    child.on("close", (code, signal) => resolve({ code, signal }));
  });

  const stdout = readline.createInterface({ input: child.stdout });
  const iterator = stdout[Symbol.asyncIterator]();
  const events = [];

  const send = (request) => {
    child.stdin.write(`${JSON.stringify(request)}\n`);
  };

  const readNext = async (label) => {
    const result = await iterator.next();
    if (result.done) {
      throw new Error(`${label} ended before expected server event${stderr ? `\nstderr:\n${stderr}` : ""}`);
    }
    try {
      const event = JSON.parse(result.value);
      events.push(event);
      return event;
    } catch (error) {
      throw new Error(`Unable to parse server thread JSONL line: ${error.message}\n${result.value}`);
    }
  };

  const readUntil = async (label, predicate) => {
    for (;;) {
      const event = await readNext(label);
      if (predicate(event)) {
        return event;
      }
    }
  };

  try {
    send({
      id: "server-thread",
      method: "thread/start",
      params: {},
    });
    const threadStarted = await readUntil(
      "server thread/start",
      (event) => event.id === "server-thread" && (event.event === "thread_started" || event.event === "error"),
    );
    if (threadStarted.event === "error") {
      throw new Error(`Server thread/start failed: ${JSON.stringify(threadStarted)}`);
    }
    const threadId = threadStarted.threadId;
    if (typeof threadId !== "string" || threadId.length === 0) {
      throw new Error(`Server thread/start did not return a threadId: ${JSON.stringify(threadStarted)}`);
    }

    send({
      id: "server-thread-turn-1",
      method: "turn/start",
      params: {
        threadId,
        input: [
          {
            type: "text",
            text: `Remember this exact token for the next turn: ${serverThreadSentinel}. Reply with exactly: ${serverThreadReadySentinel}.`,
          },
        ],
      },
    });
    let turnOneText = "";
    await readUntil("server thread turn 1", (event) => {
      if (event.id !== "server-thread-turn-1") {
        return false;
      }
      if (event.event === "message_delta") {
        turnOneText += event.text ?? "";
      }
      if (event.event === "error") {
        throw new Error(`Server thread turn 1 failed: ${JSON.stringify(event)}`);
      }
      return event.event === "turn_completed";
    });
    if (!turnOneText.includes(serverThreadReadySentinel)) {
      throw new Error(`Server thread turn 1 missing sentinel ${serverThreadReadySentinel}:\n${JSON.stringify(events)}`);
    }

    send({
      id: "server-thread-turn-2",
      method: "turn/start",
      params: {
        threadId,
        input: [
          {
            type: "text",
            text: "Reply with exactly the token I asked you to remember.",
          },
        ],
      },
    });
    let turnTwoText = "";
    const turnTwoCompleted = await readUntil("server thread turn 2", (event) => {
      if (event.id !== "server-thread-turn-2") {
        return false;
      }
      if (event.event === "message_delta") {
        turnTwoText += event.text ?? "";
      }
      if (event.event === "error") {
        throw new Error(`Server thread turn 2 failed: ${JSON.stringify(event)}`);
      }
      return event.event === "turn_completed";
    });
    if (turnTwoCompleted.status !== "success") {
      throw new Error(`Server thread turn 2 completed with unexpected status: ${JSON.stringify(turnTwoCompleted)}`);
    }
    if (!turnTwoText.includes(serverThreadSentinel)) {
      throw new Error(`Server thread e2e missing sentinel ${serverThreadSentinel}:\n${JSON.stringify(events)}`);
    }

    send({
      id: "server-resume-thread",
      method: "thread/start",
      params: {},
    });
    const resumeThreadStarted = await readUntil(
      "server active-resume thread/start",
      (event) => event.id === "server-resume-thread" && (event.event === "thread_started" || event.event === "error"),
    );
    if (resumeThreadStarted.event === "error") {
      throw new Error(`Server active-resume thread/start failed: ${JSON.stringify(resumeThreadStarted)}`);
    }
    const resumeThreadId = resumeThreadStarted.threadId;
    if (typeof resumeThreadId !== "string" || resumeThreadId.length === 0 || resumeThreadId === threadId) {
      throw new Error(`Server active-resume thread/start returned malformed threadId: ${JSON.stringify(resumeThreadStarted)}`);
    }

    send({
      id: "server-resume-turn",
      method: "turn/start",
      params: {
        threadId: resumeThreadId,
        approvalPolicy: "never",
        input: [
          {
            type: "text",
            text: `Do not call tools or inspect files. This is a text-only streaming test. Write 80 short numbered lines containing STREAM. The final line must be exactly: ${serverResumeSentinel}`,
          },
        ],
      },
    });
    const resumeTurnStarted = await readUntil("server active-resume turn/start", (event) => {
      if (event.id !== "server-resume-turn") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server active-resume turn/start failed: ${JSON.stringify(event)}`);
      }
      return event.event === "turn_started";
    });
    const resumeTurnId = resumeTurnStarted.turnId;
    if (typeof resumeTurnId !== "string" || resumeTurnId.length === 0) {
      throw new Error(`Server active-resume turn/start did not expose a turnId: ${JSON.stringify(resumeTurnStarted)}`);
    }
    const resumeTaskId = resumeTurnStarted.task?.task_id;
    if (typeof resumeTaskId !== "string" || resumeTaskId.length === 0 || resumeTaskId === resumeTurnId) {
      throw new Error(
        `Server active-resume turn/start did not separate logical turn and runtime task identity: ${JSON.stringify(resumeTurnStarted)}`,
      );
    }
    await readUntil("server active-resume first stream delta", (event) => {
      if (event.id !== "server-resume-turn") {
        return false;
      }
      if (event.event === "error" || event.event === "turn_completed") {
        throw new Error(`Server active-resume turn ended before interrupt: ${JSON.stringify(event)}`);
      }
      return event.event === "reasoning_delta" || event.event === "message_delta";
    });

    send({
      id: "server-resume-interrupt",
      method: "turn/interrupt",
      params: { threadId: resumeThreadId, turnId: resumeTurnId },
    });
    send({
      id: "server-resume-resume",
      method: "turn/resume",
      params: { threadId: resumeThreadId, turnId: resumeTurnId },
    });
    const activeInterrupt = await readUntil("server active turn/interrupt", (event) => {
      if (event.id !== "server-resume-interrupt") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server active turn/interrupt failed: ${JSON.stringify(event)}`);
      }
      return event.event === "turn_controlled";
    });
    if (activeInterrupt.status !== "interrupted" || activeInterrupt.turnId !== resumeTurnId) {
      throw new Error(`Server active turn/interrupt returned malformed control event: ${JSON.stringify(activeInterrupt)}`);
    }
    const activeResume = await readUntil("server active turn/resume", (event) => {
      if (event.id !== "server-resume-resume") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server active turn/resume failed: ${JSON.stringify(event)}`);
      }
      return event.event === "turn_controlled";
    });
    if (activeResume.status !== "resumed" || activeResume.turnId !== resumeTurnId) {
      throw new Error(`Server active turn/resume returned malformed control event: ${JSON.stringify(activeResume)}`);
    }
    const activeResumeCompleted = await readUntil("server active resumed turn completion", (event) => {
      if (event.id !== "server-resume-turn") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server active resumed turn failed: ${JSON.stringify(event)}`);
      }
      return event.event === "turn_completed";
    });
    const activeResumeText = events
      .filter((event) => event.id === "server-resume-turn" && event.event === "message_delta")
      .map((event) => event.text ?? "")
      .join("");
    const activeResumeTerminals = events.filter(
      (event) => event.id === "server-resume-turn" && event.event === "turn_completed",
    );
    if (
      activeResumeCompleted.status !== "success" ||
      !activeResumeText.includes(serverResumeSentinel) ||
      activeResumeTerminals.length !== 1
    ) {
      throw new Error(
        `Server active turn resume returned unexpected output: ${JSON.stringify({
          activeResumeCompleted,
          activeResumeText,
          activeResumeTerminals,
        })}`,
      );
    }

    send({
      id: "server-turn-interrupt",
      method: "turn/interrupt",
      params: {
        turnId: "turn-idle-real-api",
      },
    });
    const turnInterrupt = await readUntil("server turn/interrupt", (event) => {
      if (event.id !== "server-turn-interrupt") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server turn/interrupt failed: ${JSON.stringify(event)}`);
      }
      return event.event === "turn_controlled";
    });
    if (
      turnInterrupt.action !== "interrupt" ||
      turnInterrupt.turnId !== "turn-idle-real-api" ||
      turnInterrupt.status !== "idle"
    ) {
      throw new Error(`Server turn/interrupt returned malformed control event: ${JSON.stringify(turnInterrupt)}`);
    }

    send({
      id: "server-turn-resume",
      method: "turn/resume",
      params: {
        turnId: "turn-idle-real-api",
      },
    });
    const turnResume = await readUntil("server turn/resume", (event) => {
      if (event.id !== "server-turn-resume") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server turn/resume failed: ${JSON.stringify(event)}`);
      }
      return event.event === "turn_controlled";
    });
    if (
      turnResume.action !== "resume" ||
      turnResume.turnId !== "turn-idle-real-api" ||
      turnResume.status !== "idle"
    ) {
      throw new Error(`Server turn/resume returned malformed control event: ${JSON.stringify(turnResume)}`);
    }

    send({
      id: "server-turn-steer",
      method: "turn/steer",
      params: {
        turnId: "turn-idle-real-api",
        input: [
          {
            type: "text",
            text: "steer this idle turn",
          },
        ],
      },
    });
    const turnSteer = await readUntil("server turn/steer", (event) => {
      if (event.id !== "server-turn-steer") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server turn/steer failed: ${JSON.stringify(event)}`);
      }
      return event.event === "turn_controlled";
    });
    if (
      turnSteer.action !== "steer" ||
      turnSteer.turnId !== "turn-idle-real-api" ||
      turnSteer.status !== "idle" ||
      turnSteer.input !== "steer this idle turn"
    ) {
      throw new Error(`Server turn/steer returned malformed control event: ${JSON.stringify(turnSteer)}`);
    }

    send({
      id: "server-thread-metadata",
      method: "thread/metadata/update",
      params: {
        threadId,
        title: serverThreadTitle,
      },
    });
    const metadataUpdated = await readUntil("server thread/metadata/update", (event) => {
      if (event.id !== "server-thread-metadata") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server thread/metadata/update failed: ${JSON.stringify(event)}`);
      }
      return event.event === "thread_metadata_updated";
    });
    if (metadataUpdated.threadId !== threadId || metadataUpdated.title !== serverThreadTitle) {
      throw new Error(`Server thread/metadata/update returned malformed projection: ${JSON.stringify(metadataUpdated)}`);
    }

    send({
      id: "server-thread-extra",
      method: "thread/start",
      params: {},
    });
    const extraThreadStarted = await readUntil(
      "server extra thread/start",
      (event) => event.id === "server-thread-extra" && (event.event === "thread_started" || event.event === "error"),
    );
    if (extraThreadStarted.event === "error") {
      throw new Error(`Server extra thread/start failed: ${JSON.stringify(extraThreadStarted)}`);
    }
    const extraThreadId = extraThreadStarted.threadId;
    if (typeof extraThreadId !== "string" || extraThreadId.length === 0 || extraThreadId === threadId) {
      throw new Error(`Server extra thread/start returned malformed threadId: ${JSON.stringify(extraThreadStarted)}`);
    }

    send({
      id: "server-thread-extra-turn",
      method: "turn/start",
      params: {
        threadId: extraThreadId,
        input: [
          {
            type: "text",
            text: `Reply with exactly this token for list pagination coverage: ${serverThreadSentinel}.`,
          },
        ],
      },
    });
    let extraTurnText = "";
    const extraTurnCompleted = await readUntil("server extra thread turn", (event) => {
      if (event.id !== "server-thread-extra-turn") {
        return false;
      }
      if (event.event === "message_delta") {
        extraTurnText += event.text ?? "";
      }
      if (event.event === "error") {
        throw new Error(`Server extra thread turn failed: ${JSON.stringify(event)}`);
      }
      return event.event === "turn_completed";
    });
    if (extraTurnCompleted.status !== "success" || !extraTurnText.includes(serverThreadSentinel)) {
      throw new Error(`Server extra thread turn returned unexpected output: ${JSON.stringify({ extraTurnCompleted, extraTurnText })}`);
    }

    const extraThreadTitle = `ORCA server extra thread ${serverThreadSentinel}`;
    send({
      id: "server-thread-extra-metadata",
      method: "thread/metadata/update",
      params: {
        threadId: extraThreadId,
        title: extraThreadTitle,
      },
    });
    const extraMetadataUpdated = await readUntil("server extra thread/metadata/update", (event) => {
      if (event.id !== "server-thread-extra-metadata") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server extra thread/metadata/update failed: ${JSON.stringify(event)}`);
      }
      return event.event === "thread_metadata_updated";
    });
    if (extraMetadataUpdated.threadId !== extraThreadId || extraMetadataUpdated.title !== extraThreadTitle) {
      throw new Error(`Server extra thread/metadata/update returned malformed projection: ${JSON.stringify(extraMetadataUpdated)}`);
    }

    send({
      id: "server-thread-list",
      method: "thread/list",
      params: {
        searchTerm: serverThreadSentinel,
        sortKey: "updatedAt",
        limit: 1,
      },
    });
    const threadList = await readUntil("server thread/list", (event) => {
      if (event.id !== "server-thread-list") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server thread/list failed: ${JSON.stringify(event)}`);
      }
      return event.event === "thread_list";
    });
    const listedThreads = Array.isArray(threadList.data) ? threadList.data : [];
    if (listedThreads.length !== 1 || typeof threadList.nextCursor !== "string") {
      throw new Error(`Server thread/list first page did not paginate: ${JSON.stringify(threadList)}`);
    }
    send({
      id: "server-thread-list-page-2",
      method: "thread/list",
      params: {
        cursor: threadList.nextCursor,
        searchTerm: serverThreadSentinel,
        sortKey: "updatedAt",
        limit: 10,
      },
    });
    const threadListPage2 = await readUntil("server thread/list page 2", (event) => {
      if (event.id !== "server-thread-list-page-2") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server thread/list page 2 failed: ${JSON.stringify(event)}`);
      }
      return event.event === "thread_list";
    });
    const allListedThreads = listedThreads.concat(Array.isArray(threadListPage2.data) ? threadListPage2.data : []);
    if (
      !allListedThreads.some((thread) => thread.threadId === threadId && thread.title === serverThreadTitle) ||
      !allListedThreads.some((thread) => thread.threadId === extraThreadId)
    ) {
      throw new Error(`Server thread/list did not include both created threads: ${JSON.stringify({ threadList, threadListPage2 })}`);
    }
    const firstListedThread = allListedThreads.find((thread) => thread.threadId === threadId);
    if (!firstListedThread?.cwd || !firstListedThread?.provider || !firstListedThread?.model) {
      throw new Error(`Server thread/list missing metadata for filter coverage: ${JSON.stringify(allListedThreads)}`);
    }

    send({
      id: "server-thread-list-metadata-filter",
      method: "thread/list",
      params: {
        searchTerm: serverThreadSentinel,
        cwd: firstListedThread.cwd,
        modelProviders: [firstListedThread.provider],
        model: firstListedThread.model,
        sortKey: "updatedAt",
        limit: 10,
      },
    });
    const threadListMetadataFilter = await readUntil("server thread/list metadata filter", (event) => {
      if (event.id !== "server-thread-list-metadata-filter") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server thread/list metadata filter failed: ${JSON.stringify(event)}`);
      }
      return event.event === "thread_list";
    });
    const metadataFilteredThreads = Array.isArray(threadListMetadataFilter.data) ? threadListMetadataFilter.data : [];
    if (
      metadataFilteredThreads.length !== 2 ||
      metadataFilteredThreads.some(
        (thread) =>
          thread.cwd !== firstListedThread.cwd ||
          thread.provider !== firstListedThread.provider ||
          thread.model !== firstListedThread.model,
      )
    ) {
      throw new Error(`Server thread/list metadata filter returned unexpected threads: ${JSON.stringify(threadListMetadataFilter)}`);
    }

    send({
      id: "server-thread-list-metadata-filter-miss",
      method: "thread/list",
      params: {
        searchTerm: serverThreadSentinel,
        cwd: `${firstListedThread.cwd}/missing`,
        modelProviders: [firstListedThread.provider],
        model: firstListedThread.model,
        sortKey: "updatedAt",
        limit: 10,
      },
    });
    const threadListMetadataFilterMiss = await readUntil("server thread/list metadata filter miss", (event) => {
      if (event.id !== "server-thread-list-metadata-filter-miss") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server thread/list metadata filter miss failed: ${JSON.stringify(event)}`);
      }
      return event.event === "thread_list";
    });
    if ((Array.isArray(threadListMetadataFilterMiss.data) ? threadListMetadataFilterMiss.data : []).length !== 0) {
      throw new Error(
        `Server thread/list metadata filter miss should be empty: ${JSON.stringify(threadListMetadataFilterMiss)}`,
      );
    }

    send({
      id: "server-thread-search",
      method: "thread/search",
      params: {
        searchTerm: serverThreadSentinel,
        sortKey: "updatedAt",
        limit: 1,
      },
    });
    const threadSearch = await readUntil("server thread/search", (event) => {
      if (event.id !== "server-thread-search") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server thread/search failed: ${JSON.stringify(event)}`);
      }
      return event.event === "thread_search";
    });
    const searchHits = Array.isArray(threadSearch.data) ? threadSearch.data : [];
    if (searchHits.length !== 1 || typeof threadSearch.nextCursor !== "string") {
      throw new Error(`Server thread/search first page did not paginate: ${JSON.stringify(threadSearch)}`);
    }
    send({
      id: "server-thread-search-page-2",
      method: "thread/search",
      params: {
        searchTerm: serverThreadSentinel,
        cursor: threadSearch.nextCursor,
        sortKey: "updatedAt",
        limit: 10,
      },
    });
    const threadSearchPage2 = await readUntil("server thread/search page 2", (event) => {
      if (event.id !== "server-thread-search-page-2") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server thread/search page 2 failed: ${JSON.stringify(event)}`);
      }
      return event.event === "thread_search";
    });
    const allSearchHits = searchHits.concat(Array.isArray(threadSearchPage2.data) ? threadSearchPage2.data : []);
    if (
      !allSearchHits.some((hit) => hit.thread?.threadId === threadId && String(hit.snippet ?? "").includes(serverThreadSentinel)) ||
      !allSearchHits.some((hit) => hit.thread?.threadId === extraThreadId && String(hit.snippet ?? "").includes(serverThreadSentinel))
    ) {
      throw new Error(`Server thread/search did not include both created threads: ${JSON.stringify({ threadSearch, threadSearchPage2 })}`);
    }

    send({
      id: "server-thread-turns-list",
      method: "thread/turns/list",
      params: {
        threadId,
        limit: 1,
      },
    });
    const threadTurns = await readUntil("server thread/turns/list", (event) => {
      if (event.id !== "server-thread-turns-list") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server thread/turns/list failed: ${JSON.stringify(event)}`);
      }
      return event.event === "thread_turns_list";
    });
    const turns = Array.isArray(threadTurns.data) ? threadTurns.data : [];
    if (typeof threadTurns.nextCursor !== "string") {
      throw new Error(`Server thread/turns/list did not return a next cursor: ${JSON.stringify(threadTurns)}`);
    }

    send({
      id: "server-thread-turns-list-page-2",
      method: "thread/turns/list",
      params: {
        threadId,
        cursor: threadTurns.nextCursor,
        limit: 10,
      },
    });
    const threadTurnsPage2 = await readUntil("server thread/turns/list page 2", (event) => {
      if (event.id !== "server-thread-turns-list-page-2") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server thread/turns/list page 2 failed: ${JSON.stringify(event)}`);
      }
      return event.event === "thread_turns_list";
    });
    const turnsPage2 = Array.isArray(threadTurnsPage2.data) ? threadTurnsPage2.data : [];
    const allTurns = [...turns, ...turnsPage2];
    if (
      allTurns.some(
        (turn) =>
          !Array.isArray(turn.items) ||
          turn.items.filter((item) => item?.role === "user").length !== 1,
      )
    ) {
      throw new Error(
        `Server thread/turns/list did not expose exactly one user item per turn: ${JSON.stringify({ threadTurns, threadTurnsPage2 })}`,
      );
    }
    const turnText = turns
      .concat(turnsPage2)
      .flatMap((turn) => (Array.isArray(turn.items) ? turn.items : []))
      .map((item) => item.content ?? "")
      .join("\n");
    const turnWithRecallAndAssistant = allTurns.find((turn) => {
      const items = Array.isArray(turn.items) ? turn.items : [];
      return (
        turn.threadId === threadId &&
        items.some(
          (item) =>
            item.role === "user" &&
            String(item.content ?? "").includes("Reply with exactly the token I asked you to remember."),
        ) &&
        items.some((item) => canonicalAgentMessageIncludes(item, serverThreadSentinel))
      );
    });
    if (
      !allTurns.some((turn) => turn.threadId === threadId && typeof turn.turnId === "string") ||
      !turnText.includes(`Remember this exact token for the next turn: ${serverThreadSentinel}`) ||
      !turnWithRecallAndAssistant
    ) {
      throw new Error(
        `Server thread/turns/list missing expected projection: ${JSON.stringify({ threadTurns, threadTurnsPage2 })}`,
      );
    }

    send({
      id: "server-thread-turns-list-desc",
      method: "thread/turns/list",
      params: {
        threadId,
        limit: 1,
        sortDirection: "desc",
      },
    });
    const threadTurnsDesc = await readUntil("server thread/turns/list desc", (event) => {
      if (event.id !== "server-thread-turns-list-desc") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server thread/turns/list desc failed: ${JSON.stringify(event)}`);
      }
      return event.event === "thread_turns_list";
    });
    const descTurns = Array.isArray(threadTurnsDesc.data) ? threadTurnsDesc.data : [];
    const firstDescTurnItems = Array.isArray(descTurns[0]?.items) ? descTurns[0].items : [];
    if (
      descTurns.length !== 1 ||
      !firstDescTurnItems.some((item) => canonicalAgentMessageIncludes(item, serverThreadSentinel))
    ) {
      throw new Error(`Server thread/turns/list desc did not return latest turn first: ${JSON.stringify(threadTurnsDesc)}`);
    }

    send({
      id: "server-thread-turns-list-not-loaded",
      method: "thread/turns/list",
      params: {
        threadId,
        limit: 1,
        itemsView: "notLoaded",
      },
    });
    const threadTurnsNotLoaded = await readUntil("server thread/turns/list notLoaded", (event) => {
      if (event.id !== "server-thread-turns-list-not-loaded") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server thread/turns/list notLoaded failed: ${JSON.stringify(event)}`);
      }
      return event.event === "thread_turns_list";
    });
    const notLoadedTurns = Array.isArray(threadTurnsNotLoaded.data) ? threadTurnsNotLoaded.data : [];
    if (
      notLoadedTurns.length !== 1 ||
      notLoadedTurns[0]?.itemsView !== "notLoaded" ||
      !Array.isArray(notLoadedTurns[0]?.items) ||
      notLoadedTurns[0].items.length !== 0
    ) {
      throw new Error(`Server thread/turns/list notLoaded returned unexpected items: ${JSON.stringify(threadTurnsNotLoaded)}`);
    }

    send({
      id: "server-thread-items-list",
      method: "thread/items/list",
      params: {
        threadId,
        limit: 2,
      },
    });
    const threadItems = await readUntil("server thread/items/list", (event) => {
      if (event.id !== "server-thread-items-list") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server thread/items/list failed: ${JSON.stringify(event)}`);
      }
      return event.event === "thread_items_list";
    });
    const items = Array.isArray(threadItems.data) ? threadItems.data : [];
    if (typeof threadItems.nextCursor !== "string") {
      throw new Error(`Server thread/items/list did not return a next cursor: ${JSON.stringify(threadItems)}`);
    }
    send({
      id: "server-thread-items-list-page-2",
      method: "thread/items/list",
      params: {
        threadId,
        cursor: threadItems.nextCursor,
        limit: 10,
      },
    });
    const threadItemsPage2 = await readUntil("server thread/items/list page 2", (event) => {
      if (event.id !== "server-thread-items-list-page-2") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server thread/items/list page 2 failed: ${JSON.stringify(event)}`);
      }
      return event.event === "thread_items_list";
    });
    const itemsPage2 = Array.isArray(threadItemsPage2.data) ? threadItemsPage2.data : [];
    const allItems = [...items, ...itemsPage2];
    const userItemTurnIds = allItems
      .filter((entry) => entry.item?.role === "user")
      .map((entry) => entry.turnId);
    if (
      allTurns.some(
        (turn) => userItemTurnIds.filter((turnId) => turnId === turn.turnId).length !== 1,
      )
    ) {
      throw new Error(
        `Server thread/items/list did not expose exactly one user item per turn: ${JSON.stringify({ allTurns, allItems })}`,
      );
    }
    const itemText = allItems.map((entry) => entry.item?.content ?? entry.item?.text ?? "").join("\n");
    if (
      !allItems.some((entry) => entry.threadId === threadId && typeof entry.itemId === "string") ||
      !itemText.includes(`Remember this exact token for the next turn: ${serverThreadSentinel}`) ||
      !allItems.some((entry) => canonicalAgentMessageEntryIncludes(entry, serverThreadSentinel))
    ) {
      throw new Error(
        `Server thread/items/list missing expected projection: ${JSON.stringify({ threadItems, threadItemsPage2 })}`,
      );
    }

    send({
      id: "server-thread-items-list-desc",
      method: "thread/items/list",
      params: {
        threadId,
        limit: 1,
        sortDirection: "desc",
      },
    });
    const threadItemsDesc = await readUntil("server thread/items/list desc", (event) => {
      if (event.id !== "server-thread-items-list-desc") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server thread/items/list desc failed: ${JSON.stringify(event)}`);
      }
      return event.event === "thread_items_list";
    });
    const descItems = Array.isArray(threadItemsDesc.data) ? threadItemsDesc.data : [];
    if (
      descItems.length !== 1 ||
      allItems.length === 0 ||
      !isDeepStrictEqual(descItems[0], allItems.at(-1))
    ) {
      throw new Error(`Server thread/items/list desc did not return latest item first: ${JSON.stringify(threadItemsDesc)}`);
    }

    send({
      id: "server-thread-read",
      method: "thread/read",
      params: {
        threadId,
        includeMessages: true,
        includeTurns: true,
      },
    });
    const threadRead = await readUntil("server thread/read", (event) => {
      if (event.id !== "server-thread-read") {
        return false;
      }
      if (event.event === "error") {
        throw new Error(`Server thread/read failed: ${JSON.stringify(event)}`);
      }
      return event.event === "thread_read";
    });
    const messages = Array.isArray(threadRead.messages) ? threadRead.messages : [];
    const readTurns = Array.isArray(threadRead.turns) ? threadRead.turns : [];
    if (
      readTurns.some(
        (turn) =>
          !Array.isArray(turn.items) ||
          turn.items.filter((item) => item?.role === "user").length !== 1,
      )
    ) {
      throw new Error(
        `Server thread/read did not expose exactly one user item per turn: ${JSON.stringify(threadRead)}`,
      );
    }
    const userText = messages
      .filter((message) => message.role === "user")
      .map((message) => message.content ?? "")
      .join("\n");
    const assistantText = messages
      .filter((message) => message.role === "assistant")
      .map((message) => message.content ?? "")
      .join("\n");
    if (threadRead.threadId !== threadId || !Number.isInteger(threadRead.messageCount)) {
      throw new Error(`Server thread/read returned malformed projection: ${JSON.stringify(threadRead)}`);
    }
    if (threadRead.title !== serverThreadTitle) {
      throw new Error(`Server thread/read did not reflect metadata update: ${JSON.stringify(threadRead)}`);
    }
    if (
      !userText.includes(`Remember this exact token for the next turn: ${serverThreadSentinel}`) ||
      !userText.includes("Reply with exactly the token I asked you to remember.")
    ) {
      throw new Error(`Server thread/read missing expected user history: ${JSON.stringify(threadRead)}`);
    }
    if (!assistantText.includes(serverThreadSentinel)) {
      throw new Error(`Server thread/read missing expected assistant history: ${JSON.stringify(threadRead)}`);
    }
    const readTurnWithRecallAndAssistant = readTurns.find((turn) => {
      const items = Array.isArray(turn.items) ? turn.items : [];
      return (
        turn.threadId === threadId &&
        items.some(
          (item) =>
            item.role === "user" &&
            String(item.content ?? "").includes("Reply with exactly the token I asked you to remember."),
        ) &&
        items.some((item) => canonicalAgentMessageIncludes(item, serverThreadSentinel))
      );
    });
    if (!readTurnWithRecallAndAssistant) {
      throw new Error(`Server thread/read includeTurns missing expected projection: ${JSON.stringify(threadRead)}`);
    }
  } finally {
    child.stdin.end();
    stdout.close();
    clearTimeout(timeout);
  }

  const result = await closed;
  if (timedOut) {
    throw new Error(`Server thread real API e2e timed out after ${args.timeoutMs}ms${stderr ? `\nstderr:\n${stderr}` : ""}`);
  }
  if (result.code !== 0) {
    throw new Error(
      `Server thread real API e2e exited with code ${result.code}${result.signal ? ` signal ${result.signal}` : ""}${
        stderr ? `\nstderr:\n${stderr}` : ""
      }`,
    );
  }

  console.log(`Server thread real API e2e verified: ${serverThreadSentinel}`);
  console.log(`Server active turn resume e2e verified: ${serverResumeSentinel}`);
  console.log("Server thread/read e2e verified");
  console.log("Server thread/metadata/update e2e verified");
  console.log("Server turn controls e2e verified");
  console.log("Server thread/list e2e verified");
  console.log("Server thread/list metadata filters e2e verified");
  console.log("Server thread/search e2e verified");
  console.log("Server thread/turns/list e2e verified");
  console.log("Server thread/items/list e2e verified");
}

async function runServer(args) {
  if (args.skipServer) {
    console.log("Server real API e2e skipped");
    return;
  }

  runServerSubmit(args);
  await runServerThread(args);
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  runBuild(args);
  runProviderSummary(args);
  runProviderVision(args);
  runGoalMode(args);
  runCli(args);
  runHistoryReplay(args);
  runStableThreadIdentityResume(args);
  await runServer(args);
}

try {
  await main();
} catch (error) {
  console.error(error.message);
  process.exit(1);
}
