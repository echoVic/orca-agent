import { createHash } from "node:crypto";
import { existsSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import vm from "node:vm";

const scriptPath = process.argv[2];
const argsJson = process.argv[3] ?? "null";
const mailboxPath = process.argv[4] || null;
const taskListReplyPath = process.argv[5];
const maxFrameBytes = Number(process.argv[6]);
const scriptDigest = process.argv[7];
JSON.parse(argsJson);

// The source scan below turns the obvious ways out of a workflow script into a
// clear error. It is not what keeps a script in: any name can be spelled in a
// way a scan cannot see. What keeps a script in is that it runs in a realm of
// its own, where nothing it can hold belongs to this realm (see
// `installWorkflowApi`), in a process started with only the access it needs.
const FORBIDDEN_IDENTIFIERS = new Set([
  "process",
  "require",
  "constructor",
  "__proto__",
  "prototype",
  "eval",
  "Function",
  "globalThis",
]);
const FORBIDDEN_COMPUTED_PROPERTY_NAMES = new Set([
  "constructor",
  "__proto__",
  "prototype",
  "getBuiltinModule",
]);
const FORBIDDEN_MODULE_SPECIFIERS = new Set(["node:fs", "child_process"]);
const MODULE_SPECIFIER_CALLEES = new Set(["import", "require", "getBuiltinModule"]);

let callSeq = 0;
let stdinBuffer = "";
// Agent calls the script is still waiting on. The script's realm holds what
// resumes each one; `scriptRealm` is how this realm reaches it.
const pendingCalls = new Set();
let scriptRealm = null;
const mailboxState = loadMailboxState();
let messageSeq = mailboxState.nextSeq;
const messageChannels = new Map(Object.entries(mailboxState.channels));
let taskListCallSeq = 0;
const taskListWait = new Int32Array(new SharedArrayBuffer(4));
let stdinClosed = false;
let workflowTerminal = false;

process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  stdinBuffer += chunk;
  flushStdin();
});
process.stdin.on("end", () => {
  stdinClosed = true;
  if (workflowTerminal) {
    pendingCalls.clear();
    return;
  }
  flushStdin();
});

function emit(value) {
  writeFileSync(1, `${JSON.stringify(value)}\n`);
}

// Everything a workflow script can ask of this process, by name. A request and
// its answer cross between the realms as JSON text.
const HOST_OPERATIONS = {
  agent_call({ prompt, opts, phase }) {
    callSeq += 1;
    const callId = `agent-${callSeq}`;
    pendingCalls.add(callId);
    emit({
      type: "agent_call",
      call_id: callId,
      call_path: `${phase ?? "root"}:${callSeq}`,
      phase: phase ?? null,
      prompt,
      opts: opts ?? null,
    });
    if (stdinClosed) {
      // No answer can come. Say so once the script is waiting for it.
      queueMicrotask(flushStdin);
    }
    return callId;
  },

  phase_started({ name }) {
    emit({ type: "phase_started", name });
  },

  phase_completed({ name }) {
    emit({ type: "phase_completed", name });
  },

  phase_failed({ name, error, fallback }) {
    emit({ type: "phase_failed", name, error, fallback });
  },

  send_message({ channel, message, from, phase }) {
    const channelName = normalizeMessageChannel(channel);
    messageSeq += 1;
    const entry = {
      seq: messageSeq,
      channel: channelName,
      from: normalizeMessageSender(from, phase),
      phase: phase ?? null,
      message: message ?? null,
    };
    const messages = messageChannels.get(channelName) ?? [];
    messages.push(entry);
    messageChannels.set(channelName, messages);
    persistMailboxState();
    return entry;
  },

  read_messages({ channel }) {
    return messageChannels.get(normalizeMessageChannel(channel)) ?? [];
  },

  clear_messages({ channel }) {
    const channelName = normalizeMessageChannel(channel);
    const count = (messageChannels.get(channelName) ?? []).length;
    messageChannels.delete(channelName);
    persistMailboxState();
    return count;
  },

  create_task_list({ name, items }) {
    if (!Array.isArray(items)) {
      throw new Error("Workflow task list items must be an array");
    }
    return callTaskList("create", { name: String(name ?? ""), items });
  },

  claim_task({ name, by, phase }) {
    return callTaskList("claim", {
      name: String(name ?? ""),
      by: normalizeTaskWorker(by, phase),
    });
  },

  complete_task({ name, taskId, result, by, phase }) {
    return callTaskList("complete", {
      name: String(name ?? ""),
      task_id: String(taskId ?? ""),
      result: result ?? null,
      by: normalizeTaskWorker(by, phase),
    });
  },

  list_tasks({ name }) {
    return callTaskList("list", { name: String(name ?? "") });
  },

  completed({ result }) {
    workflowTerminal = true;
    emit({ type: "workflow_completed", result: result ?? null });
  },

  failed({ error }) {
    workflowTerminal = true;
    emit({ type: "workflow_failed", error });
    process.exitCode = 1;
  },
};

// The one function of this realm the script's realm is given. It takes text
// and returns text, and it never throws: an error raised here would be an
// object of this realm in the script's hands.
function hostCall(operation, requestJson) {
  try {
    if (typeof operation !== "string" || typeof requestJson !== "string") {
      throw new Error("Workflow host calls take text");
    }
    if (!Object.prototype.hasOwnProperty.call(HOST_OPERATIONS, operation)) {
      throw new Error(`Unknown workflow host operation: ${operation}`);
    }
    const value = HOST_OPERATIONS[operation](JSON.parse(requestJson));
    return JSON.stringify({ value: value ?? null });
  } catch (error) {
    try {
      return JSON.stringify({ error: error instanceof Error ? error.message : String(error) });
    } catch {
      return '{"error":"Workflow host call failed"}';
    }
  }
}

function normalizeMessageChannel(channel) {
  const channelName = String(channel ?? "").trim();
  if (channelName.length === 0) {
    throw new Error("Workflow message channel must be a non-empty string");
  }
  return channelName;
}

function normalizeMessageSender(from, phase) {
  const sender = String(from ?? phase ?? "workflow").trim();
  return sender.length === 0 ? "workflow" : sender;
}

function normalizeTaskWorker(worker, phase) {
  const workerName = String(worker ?? phase ?? "workflow").trim();
  return workerName.length === 0 ? "workflow" : workerName;
}

function loadMailboxState() {
  if (mailboxPath === null || !existsSync(mailboxPath)) {
    return { nextSeq: 0, channels: Object.create(null) };
  }
  const state = JSON.parse(readFileSync(mailboxPath, "utf8"));
  return {
    nextSeq: Number(state?.nextSeq ?? 0),
    channels: state?.channels && typeof state.channels === "object" ? state.channels : Object.create(null),
  };
}

function persistMailboxState() {
  if (mailboxPath === null) {
    return;
  }
  writeJsonAtomic(mailboxPath, {
    nextSeq: messageSeq,
    channels: Object.fromEntries(messageChannels),
  });
}

// Task list calls stay synchronous in scripts. Rust serves them on its frame
// loop and publishes the reply atomically while the agent-result stream waits.
function callTaskList(operation, args) {
  if (taskListReplyPath === undefined) {
    throw new Error("Workflow task lists require a runner");
  }
  const id = ++taskListCallSeq;
  const frame = `${JSON.stringify({ task_list_call: { id, request: { operation, ...args } } })}\n`;
  if (Buffer.byteLength(frame) > maxFrameBytes) {
    throw new Error(`Workflow task list request exceeds ${maxFrameBytes} bytes`);
  }
  writeFileSync(1, frame);
  const deadline = performance.now() + 30_000;
  while (performance.now() < deadline) {
    let reply;
    try {
      reply = JSON.parse(readFileSync(taskListReplyPath, "utf8"));
    } catch (error) {
      // Windows can briefly deny reads while the runner replaces the reply.
      if (!["ENOENT", "EBUSY", "EPERM", "EACCES"].includes(error.code)) {
        throw error;
      }
    }
    if (reply?.id === id) {
      if (typeof reply.error === "string") {
        throw new Error(reply.error);
      }
      return reply.value;
    }
    Atomics.wait(taskListWait, 0, 0, 5);
  }
  // The runner may still be writing the answer. End the workflow so a script
  // cannot unknowingly repeat a claim whose answer has been delayed.
  HOST_OPERATIONS.failed({ error: "Workflow task list request timed out" });
  process.exit(1);
}

// Replaces the mailbox in one step using the temporary name the launcher allows.
function writeJsonAtomic(path, value) {
  const tempPath = `${path}.host-tmp`;
  writeFileSync(tempPath, JSON.stringify(value, null, 2));
  try {
    renameSync(tempPath, path);
  } catch (error) {
    try {
      rmSync(tempPath, { force: true });
    } catch {
      // Ignore cleanup errors; the original rename error is more useful.
    }
    throw error;
  }
}

// Builds what a workflow script sees: `args` and the workflow functions. This
// function never runs in this realm. Its text is compiled in the script's
// realm, so every function, promise, error and value it makes belongs there,
// and a script that walks from one of them to its constructor finds its own
// realm's, which cannot compile code from text.
//
// It may use nothing from this file but its two arguments. `hostCall` is the
// only thing of the host's realm it holds. It stays a local here: it is never
// stored on, passed to or returned to anything the script can touch.
function installWorkflowApi(hostCall, argsJson) {
  "use strict";

  const { parse, stringify } = JSON;
  const RealmError = Error;
  const waiting = new Map();
  let currentPhase = null;
  let activeMarkerPhase = null;

  function ask(operation, request) {
    const requestJson = stringify(request);
    let answerJson;
    try {
      answerJson = hostCall(operation, requestJson);
    } catch {
      // Whatever was thrown belongs to the host's realm. Leave it there.
      throw new RealmError("Workflow host call failed");
    }
    const answer = parse(answerJson);
    if (typeof answer.error === "string") {
      throw new RealmError(answer.error);
    }
    return answer.value;
  }

  async function agent(prompt, opts = {}) {
    const callId = ask("agent_call", { prompt, opts, phase: currentPhase });
    const message = await new Promise((resolve, reject) => {
      waiting.set(callId, { resolve, reject });
    });
    if (message.type === "agent_result") {
      return message.result;
    }
    if (message.type === "agent_error") {
      throw new RealmError(message.error ?? `Agent ${callId} failed`);
    }

    throw new RealmError(`Unexpected workflow host protocol message: ${message.type}`);
  }

  async function parallel(items) {
    const results = await Promise.allSettled(items);
    const firstError = results.find((r) => r.status === "rejected");
    if (firstError) {
      throw firstError.reason;
    }
    return results.map((r) => r.value);
  }

  async function pipeline(items) {
    let previous;
    for (const item of items) {
      previous = typeof item === "function" ? await item(previous) : await item;
    }
    return previous;
  }

  function sendMessage(channel, message, opts = {}) {
    return ask("send_message", { channel, message, from: opts?.from, phase: currentPhase });
  }

  function readMessages(channel) {
    return ask("read_messages", { channel });
  }

  function clearMessages(channel) {
    return ask("clear_messages", { channel });
  }

  function createTaskList(name, items = []) {
    return ask("create_task_list", { name, items });
  }

  function claimTask(name, opts = {}) {
    return ask("claim_task", { name, by: opts?.by ?? opts?.from, phase: currentPhase });
  }

  function completeTask(name, taskId, result = null, opts = {}) {
    return ask("complete_task", {
      name,
      taskId,
      result,
      by: opts?.by ?? opts?.from,
      phase: currentPhase,
    });
  }

  function listTasks(name) {
    return ask("list_tasks", { name });
  }

  async function runWorkflowPhases(phaseDefinitions) {
    let previousPhaseResults = [];
    const allPhaseResults = [];

    for (const phaseDefinition of phaseDefinitions) {
      const phaseName = String(phaseDefinition?.name ?? phaseDefinition?.description ?? `phase-${allPhaseResults.length + 1}`);
      const tasks = Array.isArray(phaseDefinition?.tasks) ? phaseDefinition.tasks : [];
      const phaseResults = await phase(
        phaseName,
        async () => runWorkflowTasks(tasks, previousPhaseResults, phaseDefinition),
        phaseDefinition
      );
      previousPhaseResults = phaseResults;
      allPhaseResults.push({ name: phaseName, results: phaseResults });
    }

    return { phases: allPhaseResults };
  }

  async function runWorkflowTasks(tasks, previousPhaseResults, phaseDefinition) {
    if (tasks.length === 0) {
      return [];
    }

    if (phaseDefinition?.parallel === true) {
      return Promise.all(tasks.map((task) => runWorkflowTask(task, previousPhaseResults)));
    }

    const results = [];
    for (const task of tasks) {
      results.push(await runWorkflowTask(task, previousPhaseResults));
    }
    return results;
  }

  async function runWorkflowTask(task, previousPhaseResults) {
    if (task?.type && task.type !== "agent") {
      throw new RealmError(`Unsupported workflow task type: ${task.type}`);
    }
    const prompt = task?.prompt;
    if (typeof prompt !== "string" || prompt.trim().length === 0) {
      throw new RealmError("Workflow task missing `prompt`");
    }

    const opts = Object.assign(Object.create(null), task);
    delete opts.type;
    delete opts.prompt;

    return agent(enrichTaskPrompt(prompt, previousPhaseResults), opts);
  }

  function enrichTaskPrompt(prompt, previousPhaseResults) {
    if (!Array.isArray(previousPhaseResults) || previousPhaseResults.length === 0) {
      return prompt;
    }

    return `${prompt}\n\n[Previous phase outputs]\n${stringify(previousPhaseResults, null, 2)}`;
  }

  async function phase(name, body, opts = {}) {
    if (typeof body !== "function") {
      if (activeMarkerPhase === name) {
        currentPhase = name;
        return undefined;
      }
      completeActiveMarkerPhase();
      currentPhase = name;
      activeMarkerPhase = name;
      ask("phase_started", { name });
      return undefined;
    }

    const prior = currentPhase;
    currentPhase = name;
    ask("phase_started", { name });
    try {
      const result = await body();
      ask("phase_completed", { name });
      return result;
    } catch (error) {
      const message = error instanceof RealmError ? error.message : String(error);
      if (opts?.fallback === "continue") {
        ask("phase_failed", { name, error: message, fallback: "continue" });
        return { fallback: "continue", error: message };
      }
      if (opts?.fallback && Object.prototype.hasOwnProperty.call(opts.fallback, "value")) {
        ask("phase_failed", { name, error: message, fallback: "value" });
        return opts.fallback.value;
      }
      if (typeof opts?.fallback === "function") {
        ask("phase_failed", { name, error: message, fallback: "function" });
        return await opts.fallback({ name, error: message });
      }
      throw error;
    } finally {
      currentPhase = prior;
    }
  }

  function completeActiveMarkerPhase() {
    if (activeMarkerPhase === null) {
      return;
    }
    const name = activeMarkerPhase;
    activeMarkerPhase = null;
    ask("phase_completed", { name });
  }

  function describe(error) {
    try {
      return String(error?.stack ?? error);
    } catch {
      return "Workflow script failed";
    }
  }

  Object.assign(globalThis, {
    args: parse(argsJson),
    agent,
    parallel,
    pipeline,
    phase,
    sendMessage,
    readMessages,
    clearMessages,
    createTaskList,
    claimTask,
    completeTask,
    listTasks,
  });

  // What the host's realm may call. It passes text only, and it reads nothing
  // back: a value returned to it, or a promise it waited on, would run the
  // script's code with the host's objects in reach.
  return {
    // Runs the script, compiled in this realm as `load`, and tells the host
    // how it ended.
    async run(load) {
      try {
        const namespace = await load();
        const phaseDefinitions = Array.isArray(namespace.phases)
          ? namespace.phases
          : Array.isArray(namespace.meta?.phases)
            ? namespace.meta.phases
            : null;
        const result = namespace.defaultSet
          ? namespace.default
          : Array.isArray(phaseDefinitions) && phaseDefinitions.some((phaseDefinition) => typeof phaseDefinition === "object")
            ? await runWorkflowPhases(phaseDefinitions)
            : null;
        completeActiveMarkerPhase();
        ask("completed", { result: result ?? null });
      } catch (error) {
        ask("failed", { error: describe(error) });
      }
    },

    // Hands an agent call its answer, as the JSON text the host received.
    deliver(callId, messageJson) {
      const call = waiting.get(callId);
      if (call === undefined) {
        return;
      }
      waiting.delete(callId);
      call.resolve(parse(messageJson));
    },

    // Ends an agent call that will get no answer.
    fail(callId, reason) {
      const call = waiting.get(callId);
      if (call === undefined) {
        return;
      }
      waiting.delete(callId);
      call.reject(new RealmError(reason));
    },

    refuseImport() {
      throw new RealmError("Dynamic import is not available in workflow scripts");
    },
  };
}

// Calls into the script's realm. A script can break its own realm's built-ins
// and make such a call throw; what it throws is the script's, so it is not
// looked at here.
function tellScriptRealm(call) {
  try {
    call();
  } catch {
    workflowTerminal = true;
    pendingCalls.clear();
    emit({ type: "workflow_failed", error: "Workflow script broke the workflow functions it runs on" });
    process.exit(1);
  }
}

function failPendingCalls(reasonFor) {
  const callIds = [...pendingCalls];
  pendingCalls.clear();
  for (const callId of callIds) {
    const reason = reasonFor(callId);
    tellScriptRealm(() => scriptRealm.fail(callId, reason));
  }
}

function flushStdin() {
  while (true) {
    const newlineIndex = stdinBuffer.indexOf("\n");
    if (newlineIndex === -1) {
      if (!stdinClosed) {
        return;
      }

      const trailing = stdinBuffer.trim();
      stdinBuffer = "";
      failPendingCalls((callId) =>
        trailing.length > 0
          ? `Workflow host protocol ended with partial JSON: ${trailing}`
          : `Workflow host protocol closed before result for ${callId}`,
      );
      return;
    }

    const line = stdinBuffer.slice(0, newlineIndex).trim();
    stdinBuffer = stdinBuffer.slice(newlineIndex + 1);
    if (line.length === 0) {
      continue;
    }

    let callId;
    try {
      callId = JSON.parse(line).call_id;
      if (!pendingCalls.has(callId)) {
        throw new Error(`Workflow host protocol received result for unknown call ${callId}`);
      }
    } catch (error) {
      const reason = error instanceof Error ? error.message : String(error);
      failPendingCalls(() => reason);
      continue;
    }
    pendingCalls.delete(callId);
    tellScriptRealm(() => scriptRealm.deliver(callId, line));
  }
}

function startWorkflow(source) {
  guardWorkflowSource(source);
  const transformed = transformWorkflowSource(source);

  // The context starts empty: `installWorkflowApi` fills it from inside.
  const context = vm.createContext(Object.create(null), {
    codeGeneration: {
      strings: false,
      wasm: false,
    },
  });
  const install = vm.runInContext(`(${installWorkflowApi})`, context, {
    filename: "orca-workflow-api.js",
  });
  const realm = install(hostCall, argsJson);
  scriptRealm = { deliver: realm.deliver, fail: realm.fail };
  const refuseImport = realm.refuseImport;
  const load = vm.compileFunction(
    `
      "use strict";
      return (async () => {
        let __workflow_default__ = null;
        let __workflow_default_set__ = false;
        ${transformed}
        return {
          meta,
          phases: typeof phases === "undefined" ? null : phases,
          default: __workflow_default__,
          defaultSet: __workflow_default_set__,
        };
      })();
    `,
    [],
    {
      parsingContext: context,
      filename: scriptPath,
      importModuleDynamically() {
        // Thrown by the script's realm, so the script gets an error of its own.
        return refuseImport();
      },
    },
  );

  // Not awaited: the script's realm reports the end itself, through `hostCall`.
  realm.run(load);
}

function transformWorkflowSource(source) {
  const replacements = findWorkflowExportReplacements(source).sort((left, right) => right.start - left.start);
  let transformed = source;

  for (const replacement of replacements) {
    transformed =
      transformed.slice(0, replacement.start) +
      replacement.text +
      transformed.slice(replacement.end);
  }

  return transformed;
}

function findWorkflowExportReplacements(source) {
  const replacements = [];
  scanWorkflowExports(source, 0, null, replacements);
  return replacements;
}

function scanWorkflowExports(source, startIndex, terminator, replacements) {
  let index = startIndex;

  while (index < source.length) {
    const char = source[index];
    const next = source[index + 1];

    if (terminator && char === terminator) {
      return index + 1;
    }

    if (isWhitespace(char)) {
      index += 1;
      continue;
    }

    if (char === "/" && next === "/") {
      index = skipLineComment(source, index + 2);
      continue;
    }

    if (char === "/" && next === "*") {
      index = skipBlockComment(source, index + 2);
      continue;
    }

    if (char === "'" || char === "\"") {
      index = readQuotedString(source, index, char).nextIndex;
      continue;
    }

    if (char === "`") {
      index = scanTemplateLiteralForWorkflowExports(source, index + 1, replacements);
      continue;
    }

    if (isIdentifierStart(char)) {
      const exportMatch = matchWorkflowExport(source, index);
      if (exportMatch) {
        replacements.push(exportMatch);
        index = exportMatch.end;
        continue;
      }

      index = readIdentifierEnd(source, index + 1);
      continue;
    }

    index += 1;
  }

  if (terminator) {
    throw new Error(`Unterminated workflow syntax before ${terminator}`);
  }

  return index;
}

function scanTemplateLiteralForWorkflowExports(source, startIndex, replacements) {
  let index = startIndex;

  while (index < source.length) {
    const char = source[index];
    const next = source[index + 1];

    if (char === "\\") {
      index += 2;
      continue;
    }

    if (char === "`") {
      return index + 1;
    }

    if (char === "$" && next === "{") {
      index = scanWorkflowExports(source, index + 2, "}", replacements);
      continue;
    }

    index += 1;
  }

  throw new Error("Unterminated template literal in workflow script");
}

function matchWorkflowExport(source, startIndex) {
  const exportEnd = readIdentifierEnd(source, startIndex + 1);
  if (source.slice(startIndex, exportEnd) !== "export") {
    return null;
  }

  const firstTokenStart = skipIgnorable(source, exportEnd);
  if (firstTokenStart >= source.length || !isIdentifierStart(source[firstTokenStart])) {
    return null;
  }

  const firstTokenEnd = readIdentifierEnd(source, firstTokenStart + 1);
  const firstToken = source.slice(firstTokenStart, firstTokenEnd);

  if (firstToken === "const") {
    const secondTokenStart = skipIgnorable(source, firstTokenEnd);
    if (secondTokenStart >= source.length || !isIdentifierStart(source[secondTokenStart])) {
      return null;
    }

    const secondTokenEnd = readIdentifierEnd(source, secondTokenStart + 1);
    const secondToken = source.slice(secondTokenStart, secondTokenEnd);
    if (secondToken !== "meta" && secondToken !== "phases" && secondToken !== "args") {
      return null;
    }

    const equalsIndex = skipIgnorable(source, secondTokenEnd);
    if (source[equalsIndex] !== "=") {
      return null;
    }

    if (secondToken === "args") {
      return {
        start: startIndex,
        end: secondTokenEnd,
        text: "const __workflow_args_schema__",
      };
    }

    return {
      start: startIndex,
      end: firstTokenStart,
      text: source.slice(exportEnd, firstTokenStart),
    };
  }

  if (firstToken === "default") {
    return {
      start: startIndex,
      end: firstTokenEnd,
      text: `${source.slice(exportEnd, firstTokenStart)}__workflow_default_set__ = true; __workflow_default__ =`,
    };
  }

  return null;
}

function guardWorkflowSource(source) {
  scanExecutableTokens(source, 0, null, [], []);
}

function scanExecutableTokens(source, startIndex, terminator, callStack, bracketStack) {
  let index = startIndex;
  let pendingCallee = null;
  let lastTokenType = null;

  while (index < source.length) {
    const char = source[index];
    const next = source[index + 1];

    if (terminator && char === terminator) {
      return index + 1;
    }

    if (isWhitespace(char)) {
      index += 1;
      continue;
    }

    if (char === "/" && next === "/") {
      index = skipLineComment(source, index + 2);
      continue;
    }

    if (char === "/" && next === "*") {
      index = skipBlockComment(source, index + 2);
      continue;
    }

    if (char === "'") {
      const stringResult = readQuotedString(source, index, "'");
      checkComputedPropertyName(source, index, stringResult.value, stringResult.nextIndex, bracketStack);
      checkModuleSpecifier(callStack, stringResult.value);
      index = stringResult.nextIndex;
      pendingCallee = null;
      lastTokenType = "string";
      continue;
    }

    if (char === "\"") {
      const stringResult = readQuotedString(source, index, "\"");
      checkComputedPropertyName(source, index, stringResult.value, stringResult.nextIndex, bracketStack);
      checkModuleSpecifier(callStack, stringResult.value);
      index = stringResult.nextIndex;
      pendingCallee = null;
      lastTokenType = "string";
      continue;
    }

    if (char === "`") {
      index = scanTemplateLiteral(source, index + 1, callStack);
      pendingCallee = null;
      lastTokenType = "string";
      continue;
    }

    if (isIdentifierStart(char)) {
      const identEnd = readIdentifierEnd(source, index + 1);
      const ident = source.slice(index, identEnd);
      if (FORBIDDEN_IDENTIFIERS.has(ident)) {
        throw new Error(`Workflow script contains prohibited syntax: ${ident}`);
      }
      if (ident === "import" && nextNonWhitespaceChar(source, identEnd) === "(") {
        throw new Error("Workflow script contains prohibited syntax: import(");
      }
      pendingCallee = ident;
      lastTokenType = "identifier";
      index = identEnd;
      continue;
    }

    if (char === "(") {
      callStack.push({ callee: pendingCallee, argIndex: 0 });
      pendingCallee = null;
      lastTokenType = "open_paren";
      index += 1;
      continue;
    }

    if (char === ")") {
      callStack.pop();
      pendingCallee = null;
      lastTokenType = "close_paren";
      index += 1;
      continue;
    }

    if (char === "[") {
      bracketStack.push({ computedProperty: startsComputedProperty(lastTokenType) });
      pendingCallee = null;
      lastTokenType = "open_bracket";
      index += 1;
      continue;
    }

    if (char === "]") {
      bracketStack.pop();
      pendingCallee = null;
      lastTokenType = "close_bracket";
      index += 1;
      continue;
    }

    if (char === "{") {
      pendingCallee = null;
      lastTokenType = "open_brace";
      index += 1;
      continue;
    }

    if (char === "}") {
      pendingCallee = null;
      lastTokenType = "close_brace";
      index += 1;
      continue;
    }

    if (char === ",") {
      if (callStack.length > 0) {
        callStack[callStack.length - 1].argIndex += 1;
      }
      pendingCallee = null;
      lastTokenType = "comma";
      index += 1;
      continue;
    }

    if (char !== ".") {
      pendingCallee = null;
    }
    lastTokenType = "other";
    index += 1;
  }

  if (terminator) {
    throw new Error(`Unterminated workflow syntax before ${terminator}`);
  }
  return index;
}

function scanTemplateLiteral(source, startIndex, callStack) {
  let index = startIndex;

  while (index < source.length) {
    const char = source[index];
    const next = source[index + 1];

    if (char === "\\") {
      index += 2;
      continue;
    }

    if (char === "`") {
      return index + 1;
    }

    if (char === "$" && next === "{") {
      index = scanExecutableTokens(source, index + 2, "}", [], []);
      continue;
    }

    index += 1;
  }

  throw new Error("Unterminated template literal in workflow script");
}

function readQuotedString(source, startIndex, quote) {
  let index = startIndex + 1;
  let value = "";

  while (index < source.length) {
    const char = source[index];
    if (char === "\\") {
      value += source.slice(index, index + 2);
      index += 2;
      continue;
    }
    if (char === quote) {
      return { value, nextIndex: index + 1 };
    }
    value += char;
    index += 1;
  }

  throw new Error("Unterminated string literal in workflow script");
}

function checkModuleSpecifier(callStack, value) {
  const currentCall = callStack[callStack.length - 1];
  if (
    currentCall &&
    currentCall.argIndex === 0 &&
    MODULE_SPECIFIER_CALLEES.has(currentCall.callee) &&
    FORBIDDEN_MODULE_SPECIFIERS.has(value)
  ) {
    throw new Error(`Workflow script contains prohibited module specifier: ${value}`);
  }
}

function checkComputedPropertyName(source, startIndex, value, nextIndex, bracketStack) {
  const currentBracket = bracketStack[bracketStack.length - 1];
  if (
    currentBracket?.computedProperty &&
    previousNonWhitespaceChar(source, startIndex) === "[" &&
    nextNonWhitespaceChar(source, nextIndex) === "]" &&
    FORBIDDEN_COMPUTED_PROPERTY_NAMES.has(value)
  ) {
    throw new Error(`Workflow script contains prohibited computed property: ${value}`);
  }
}

function skipLineComment(source, startIndex) {
  let index = startIndex;
  while (index < source.length && source[index] !== "\n") {
    index += 1;
  }
  return index;
}

function skipBlockComment(source, startIndex) {
  let index = startIndex;
  while (index < source.length - 1) {
    if (source[index] === "*" && source[index + 1] === "/") {
      return index + 2;
    }
    index += 1;
  }
  throw new Error("Unterminated block comment in workflow script");
}

function readIdentifierEnd(source, startIndex) {
  let index = startIndex;
  while (index < source.length && isIdentifierPart(source[index])) {
    index += 1;
  }
  return index;
}

function previousNonWhitespaceChar(source, startIndex) {
  let index = startIndex - 1;
  while (index >= 0 && isWhitespace(source[index])) {
    index -= 1;
  }
  return source[index] ?? null;
}

function nextNonWhitespaceChar(source, startIndex) {
  let index = startIndex;
  while (index < source.length && isWhitespace(source[index])) {
    index += 1;
  }
  return source[index] ?? null;
}

function skipIgnorable(source, startIndex) {
  let index = startIndex;

  while (index < source.length) {
    const char = source[index];
    const next = source[index + 1];

    if (isWhitespace(char)) {
      index += 1;
      continue;
    }

    if (char === "/" && next === "/") {
      index = skipLineComment(source, index + 2);
      continue;
    }

    if (char === "/" && next === "*") {
      index = skipBlockComment(source, index + 2);
      continue;
    }

    break;
  }

  return index;
}

function startsComputedProperty(lastTokenType) {
  return (
    lastTokenType === "identifier" ||
    lastTokenType === "string" ||
    lastTokenType === "close_paren" ||
    lastTokenType === "close_bracket" ||
    lastTokenType === "close_brace"
  );
}

function isWhitespace(char) {
  return char === " " || char === "\n" || char === "\r" || char === "\t";
}

function isIdentifierStart(char) {
  return (
    (char >= "A" && char <= "Z") ||
    (char >= "a" && char <= "z") ||
    char === "_" ||
    char === "$"
  );
}

function isIdentifierPart(char) {
  return isIdentifierStart(char) || (char >= "0" && char <= "9");
}

try {
  const script = await readFile(scriptPath);
  if (scriptDigest && createHash("sha256").update(script).digest("hex") !== scriptDigest) {
    throw new Error("workflow script changed after preparation");
  }
  startWorkflow(script.toString("utf8"));
} catch (error) {
  workflowTerminal = true;
  emit({ type: "workflow_failed", error: error?.stack ?? String(error) });
  process.exitCode = 1;
}
