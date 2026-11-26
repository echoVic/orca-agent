(function (startCall, finish, __orcaLogLine, firstLineShift) {
  "use strict";

  // A function set here is handed the frames of the calls beneath the error,
  // and each frame's function: the prelude's own among them. Nothing else in
  // the engine hands out frames.
  Object.defineProperty(Error, "prepareStackTrace", {
    value: undefined,
    writable: false,
    enumerable: false,
    configurable: false,
  });

  // Taken before the script runs: a script that replaces these breaks only
  // what it does with them itself.
  const { stringify } = JSON;
  const RealmPromise = Promise;
  const RealmError = Error;
  const RealmTypeError = TypeError;
  const waiting = new Map();
  const logged = [];

  function callTool(tool, args) {
    return new RealmPromise((resolve, reject) => {
      const text = stringify(args ?? {});
      if (typeof text !== "string") {
        throw new RealmTypeError(`${tool} takes an object of arguments`);
      }
      waiting.set(startCall(tool, text), { resolve, reject });
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
    const line = values.map((value) => show(value)).join(" ");
    __orcaLogLine(line);
    logged.push(line);
  }

  // An error as the model should read it: what it is, and where in the script.
  // The script's first line follows the opening of the function it is the body
  // of, so a column on that line is counted from where the script starts.
  function describe(error) {
    try {
      const where = /script\.js:(\d+)(?::(\d+))?/.exec(String(error?.stack ?? ""));
      if (!where) {
        return String(error);
      }
      const line = Number(where[1]);
      if (where[2] === undefined) {
        return `${String(error)} (script.js:${line})`;
      }
      const column = Number(where[2]) - (line === 1 ? firstLineShift : 0);
      return `${String(error)} (script.js:${line}:${column})`;
    } catch {
      return "The script threw a value that cannot be shown";
    }
  }

  // The engine's build of the language adds this one name of its own.
  delete globalThis.queueMicrotask;
  globalThis.tools = tools;
  globalThis.console = { log, info: log, warn: log, error: log, debug: log };

  return {
    // Runs the script and says how it ended.
    async run(main) {
      let kind;
      let text;
      try {
        const value = await main();
        const parts = value === undefined ? logged : [...logged, show(value, 2)];
        kind = "completed";
        text = parts.join("\n");
      } catch (error) {
        const output = logged.length > 0 ? `\n\nLogged before the error:\n${logged.join("\n")}` : "";
        kind = "failed";
        text = `${describe(error)}${output}`;
      }
      finish(kind, text);
    },

    // Hands a tool call its answer.
    deliver(number, ok, text) {
      const call = waiting.get(number);
      if (call === undefined) {
        return;
      }
      waiting.delete(number);
      if (ok) {
        call.resolve(text);
      } else {
        call.reject(new RealmError(text));
      }
    },

    describe,
  };
})
