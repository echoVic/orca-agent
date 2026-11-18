import { readFileSync, writeFileSync } from "node:fs";
import vm from "node:vm";

// Runs one `run_script` script. The script gets a realm of its own, holding
// `tools` and `console` and nothing of Node. Each tool call leaves this
// process as a line on stdout and its answer comes back as a line on stdin:
// this process reads no file but the script and does nothing on the script's
// behalf. What keeps a script in is the same as for a workflow script (see
// `workflow/host.mjs`): nothing it can hold belongs to this realm, this realm
// cannot compile text, and the process is started with only the access it
// needs.

const scriptPath = process.argv[2];

let callSeq = 0;
let stdinBuffer = "";
// Tool calls the script is waiting on. The script's realm holds what resumes
// each one; `scriptRealm` is how this realm reaches it.
const pendingCalls = new Set();
let scriptRealm = null;
let stdinClosed = false;

process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  stdinBuffer += chunk;
  flushStdin();
});
process.stdin.on("end", () => {
  stdinClosed = true;
  flushStdin();
});

function emit(value) {
  writeFileSync(1, `${JSON.stringify(value)}\n`);
}

// The script is over: say how, and stop.
function finish(value) {
  emit(value);
  process.exit(0);
}

// Everything a script can ask of this process, by name. A request and its
// answer cross between the realms as JSON text.
const HOST_OPERATIONS = {
  tool_call({ tool, arguments: args }) {
    callSeq += 1;
    const callId = `call-${callSeq}`;
    pendingCalls.add(callId);
    emit({ type: "tool_call", call_id: callId, tool, arguments: args ?? {} });
    if (stdinClosed) {
      // No answer can come. Say so once the script is waiting for it.
      queueMicrotask(flushStdin);
    }
    return callId;
  },

  completed({ output }) {
    finish({ type: "completed", output });
  },

  failed({ error }) {
    finish({ type: "failed", error });
  },
};

// The one function of this realm the script's realm is given. It takes text
// and returns text, and it never throws: an error raised here would be an
// object of this realm in the script's hands.
function hostCall(operation, requestJson) {
  try {
    if (typeof operation !== "string" || typeof requestJson !== "string") {
      throw new Error("Script host calls take text");
    }
    if (!Object.prototype.hasOwnProperty.call(HOST_OPERATIONS, operation)) {
      throw new Error(`Unknown script host operation: ${operation}`);
    }
    const value = HOST_OPERATIONS[operation](JSON.parse(requestJson));
    return JSON.stringify({ value: value ?? null });
  } catch (error) {
    try {
      return JSON.stringify({ error: error instanceof Error ? error.message : String(error) });
    } catch {
      return '{"error":"Script host call failed"}';
    }
  }
}

// Builds what a script sees. This function never runs in this realm: its text
// is compiled in the script's realm, so every function, promise, error and
// value it makes belongs there.
//
// It may use nothing from this file but its argument. `hostCall` is the only
// thing of the host's realm it holds, and it stays a local: it is never stored
// on, passed to or returned to anything the script can touch.
function installScriptApi(hostCall) {
  "use strict";

  const { parse, stringify } = JSON;
  const RealmError = Error;
  const RealmPromise = Promise;
  const waiting = new Map();
  const logged = [];

  function ask(operation, request) {
    const requestJson = stringify(request);
    let answerJson;
    try {
      answerJson = hostCall(operation, requestJson);
    } catch {
      // Whatever was thrown belongs to the host's realm. Leave it there.
      throw new RealmError("Script host call failed");
    }
    const answer = parse(answerJson);
    if (typeof answer.error === "string") {
      throw new RealmError(answer.error);
    }
    return answer.value;
  }

  function callTool(tool, args) {
    return new RealmPromise((resolve, reject) => {
      const callId = ask("tool_call", { tool, arguments: args });
      waiting.set(callId, { resolve, reject });
    });
  }

  const tools = {};
  for (const tool of ["read_file", "glob", "grep", "git_status"]) {
    tools[tool] = (args) => callTool(tool, args);
  }

  // One value as text: a string as it is, anything else as JSON.
  function show(value, indent) {
    if (typeof value === "string") {
      return value;
    }
    try {
      return stringify(value, null, indent) ?? String(value);
    } catch {
      return String(value);
    }
  }

  function log(...values) {
    logged.push(values.map((value) => show(value)).join(" "));
  }

  // An error as the model should read it: what it is, and where in the script.
  function describe(error) {
    try {
      const where = /script\.js:\d+(:\d+)?/.exec(String(error?.stack ?? ""));
      return where ? `${String(error)} (${where[0]})` : String(error);
    } catch {
      return "The script threw a value that cannot be shown";
    }
  }

  Object.assign(globalThis, {
    tools,
    console: { log, info: log, warn: log, error: log, debug: log },
  });

  // What the host's realm may call. It passes text only, and it reads nothing
  // back: a value returned to it, or a promise it waited on, would run the
  // script's code with the host's objects in reach.
  return {
    // Runs the script, compiled in this realm as `main`, and tells the host
    // how it ended.
    async run(main) {
      let report;
      try {
        const value = await main();
        const parts = value === undefined ? logged : [...logged, show(value, 2)];
        report = ["completed", { output: parts.join("\n") }];
      } catch (error) {
        const output = logged.length > 0 ? `\n\nLogged before the error:\n${logged.join("\n")}` : "";
        report = ["failed", { error: `${describe(error)}${output}` }];
      }
      ask(report[0], report[1]);
    },

    // Hands a tool call its answer, as the JSON text the host received.
    deliver(callId, messageJson) {
      const call = waiting.get(callId);
      if (call === undefined) {
        return;
      }
      waiting.delete(callId);
      const message = parse(messageJson);
      if (message.type === "tool_result") {
        call.resolve(String(message.output ?? ""));
      } else {
        call.reject(new RealmError(String(message.error ?? "The tool call failed")));
      }
    },

    // Ends a tool call that will get no answer.
    fail(callId, reason) {
      const call = waiting.get(callId);
      if (call === undefined) {
        return;
      }
      waiting.delete(callId);
      call.reject(new RealmError(reason));
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
    finish({ type: "failed", error: "The script broke the functions it runs on" });
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
      if (stdinClosed) {
        stdinBuffer = "";
        failPendingCalls(() => "The tool call was never answered");
      }
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
    } catch {
      callId = undefined;
    }
    if (!pendingCalls.has(callId)) {
      failPendingCalls(() => "The script host received an answer it could not match to a call");
      continue;
    }
    pendingCalls.delete(callId);
    tellScriptRealm(() => scriptRealm.deliver(callId, line));
  }
}

function startScript(source) {
  // The context starts empty: `installScriptApi` fills it from inside.
  const context = vm.createContext(Object.create(null), {
    codeGeneration: {
      strings: false,
      wasm: false,
    },
  });
  const install = vm.runInContext(`(${installScriptApi})`, context, {
    filename: "orca-script-api.js",
  });
  const realm = install(hostCall);
  scriptRealm = { deliver: realm.deliver, fail: realm.fail };
  // The script is the body of an async function. The offset makes its first
  // line line 1 in errors.
  const main = vm.runInContext(`(async () => {\n${source}\n})`, context, {
    filename: "script.js",
    lineOffset: -1,
  });

  // Not awaited: the script's realm reports the end itself, through `hostCall`.
  realm.run(main);
}

try {
  startScript(readFileSync(scriptPath, "utf8"));
} catch (error) {
  // A script that does not compile. Node's message starts with the line and a
  // marker under the place; the frames after it are this file's.
  finish({ type: "failed", error: String(error?.stack ?? error).split("\n    at ")[0] });
}
