#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
// reqwest 0.13 panics on a client built before a crypto provider is
// installed; the entry installs one.
const entry = "crates/orca-mcp/src/http.rs";

function fail(message) {
  throw new Error(`http client boundary: ${message}`);
}

// A `use` declaration, with the visibility it may start with. Only one that
// starts an item is a declaration: see `startsItem`.
const declaration = /(?:\bpub(?:\s*\([^)]*\))?\s+)?\buse\s+([^;]+);/g;

function trackedRustSources() {
  return execFileSync("git", ["ls-files", "crates", "-z"], { cwd: repoRoot })
    .toString()
    .split("\0")
    .filter((file) => file.endsWith(".rs"));
}

const identifierCharacter = /[\p{L}\p{N}_]/u;
// A raw string literal up to its opening quote, with its `#`s: `r#"`, `br"`.
const rawStringStart = /(?:br|cr|r)(#*)"/y;
// A character literal, as against the tick of a lifetime or a label.
const characterLiteral = /'(?:\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]+\}|[^\r\n])|[^\\'\r\n])'/uy;

// The opening of the raw string literal that starts at `at`, if one does:
// the match of `r#"`, `br"` and the like, and the `#`s in it.
function rawStringAt(source, at) {
  if (!"rbc".includes(source[at]) || identifierCharacter.test(source[at - 1] ?? "")) {
    return null;
  }
  rawStringStart.lastIndex = at;
  return rawStringStart.exec(source);
}

// `source` as the compiler reads code: comments (`//`, `///`, `//!` and
// `/* … */`, which nest) are blanked out, and so are the insides of string
// and character literals, with every line break kept. A word in a comment, or
// text in a string, is then never taken for a `use` or a call, and a `//` in
// a string never starts a comment. Throws when a block comment or a literal
// never ends: what follows would be skipped unseen, and Rust that compiles
// has none.
export function codeOnly(source) {
  const pieces = [];
  let copied = 0; // source[0, copied) is in `pieces`
  let at = 0;
  const blankOut = (end) => {
    pieces.push(source.slice(copied, at), source.slice(at, end).replace(/[^\r\n]/g, " "));
    copied = end;
    at = end;
  };
  while (at < source.length) {
    const character = source[at];
    if (character === "/" && source[at + 1] === "/") {
      const lineEnd = source.indexOf("\n", at);
      blankOut(lineEnd === -1 ? source.length : lineEnd);
    } else if (character === "/" && source[at + 1] === "*") {
      let depth = 1;
      let end = at + 2;
      while (depth > 0 && end < source.length) {
        if (source.startsWith("/*", end)) {
          depth += 1;
          end += 2;
        } else if (source.startsWith("*/", end)) {
          depth -= 1;
          end += 2;
        } else {
          end += 1;
        }
      }
      if (depth > 0) {
        throw new Error("a block comment never ends");
      }
      blankOut(end);
    } else if (character === '"') {
      let end = at + 1;
      while (end < source.length && source[end] !== '"') {
        end += source[end] === "\\" ? 2 : 1;
      }
      if (end >= source.length) {
        throw new Error("a string literal never ends");
      }
      blankOut(end + 1);
    } else if (rawStringAt(source, at)) {
      const [opening, hashes] = rawStringAt(source, at);
      const closing = `"${hashes}`;
      const close = source.indexOf(closing, at + opening.length);
      if (close === -1) {
        throw new Error("a raw string literal never ends");
      }
      blankOut(close + closing.length);
    } else if (character === "'") {
      characterLiteral.lastIndex = at;
      const literal = characterLiteral.exec(source);
      if (literal) {
        blankOut(at + literal[0].length);
      } else {
        at += 1; // the tick of a lifetime or a label
      }
    } else {
      at += 1;
    }
  }
  pieces.push(source.slice(copied));
  return pieces.join("");
}

// Whether an item starts at `index` of `code`: it is the first thing in the
// file, or follows `;`, `{`, `}` or the `]` that ends an attribute.
function startsItem(code, index) {
  let before = index - 1;
  while (before >= 0 && /\s/.test(code[before])) {
    before -= 1;
  }
  return before < 0 || ";{}]".includes(code[before]);
}

// `code` without its `use` declarations. A declaration names an item without
// calling it, so the call patterns look at the code without them.
function withoutImports(code) {
  return code.replace(declaration, (match, _tree, index) =>
    startsItem(code, index) ? match.replace(/[^\r\n]/g, " ") : match,
  );
}

// What a `use` brings in, one leaf per name:
// `reqwest::blocking::{Client as C, get}` gives
// { path: ["reqwest", "blocking", "Client"], alias: "C" } and
// { path: ["reqwest", "blocking", "get"], alias: "get" }. A glob has no alias.
function useLeaves(tree) {
  const tokens = tree.match(/::|\*|[{},]|[A-Za-z_]\w*/g) ?? [];
  let next = 0;
  function parse(prefix) {
    if (tokens[next] === "::") {
      next += 1;
    }
    const names = [...prefix];
    for (;;) {
      const token = tokens[next];
      if (token === "{") {
        next += 1;
        const leaves = [];
        while (next < tokens.length && tokens[next] !== "}") {
          const before = next;
          leaves.push(...parse(names));
          if (tokens[next] === ",") {
            next += 1;
          }
          if (next === before) {
            next += 1;
          }
        }
        next += 1;
        return leaves;
      }
      if (token === "*") {
        next += 1;
        return [{ path: names, alias: null }];
      }
      if (token === undefined || !/^[A-Za-z_]/.test(token)) {
        return [];
      }
      next += 1;
      names.push(token);
      if (tokens[next] === "::") {
        next += 1;
        continue;
      }
      let alias = token;
      if (tokens[next] === "as") {
        alias = tokens[next + 1];
        next += 2;
      }
      if (token === "self") {
        // `{self}` names the module the braces are in.
        names.pop();
        if (alias === "self") {
          alias = names[names.length - 1];
        }
      }
      return [{ path: names, alias }];
    }
  }
  return parse([]);
}

// The names a file gives the reqwest items that build a client without
// going through the entry, however it imports or aliases them:
// `use reqwest::Client;`, `use reqwest::blocking::{Client as BlockingClient, …};`,
// `use reqwest::blocking;`, `use reqwest as http;`, `use reqwest::blocking::*;`,
// `type Http = reqwest::Client;`, …
function reqwestNames(code) {
  const names = {
    crate: new Set(["reqwest"]),
    blocking: new Set(),
    clientTypes: new Set(),
    getFunctions: new Set(),
  };
  for (const match of code.matchAll(declaration)) {
    if (!startsItem(code, match.index)) {
      continue;
    }
    for (const { path: leaf, alias } of useLeaves(match[1])) {
      if (leaf[0] !== "reqwest") {
        continue;
      }
      const rest = leaf.slice(1);
      const inBlocking = rest[0] === "blocking";
      const item = inBlocking ? rest.slice(1) : rest;
      if (alias === null) {
        if (rest.length === 0) {
          names.blocking.add("blocking");
        }
        if (rest.length === 0 || (rest.length === 1 && inBlocking)) {
          names.clientTypes.add("Client").add("ClientBuilder");
          names.getFunctions.add("get");
        }
      } else if (rest.length === 0) {
        names.crate.add(alias);
      } else if (rest.length === 1 && inBlocking) {
        names.blocking.add(alias);
      } else if (item.length === 1 && (item[0] === "Client" || item[0] === "ClientBuilder")) {
        names.clientTypes.add(alias);
      } else if (item.length === 1 && item[0] === "get") {
        names.getFunctions.add(alias);
      }
    }
  }
  for (const [, name] of code.matchAll(
    /\btype\s+(\w+)\s*=\s*(?:::)?reqwest::(?:blocking::)?(?:Client|ClientBuilder)\s*;/g,
  )) {
    names.clientTypes.add(name);
  }
  for (const [, name] of code.matchAll(/\bextern\s+crate\s+reqwest\s+as\s+(\w+)\s*;/g)) {
    names.crate.add(name);
  }
  return names;
}

// Every way to build a client that skips the entry: `Client::new()`,
// `Client::builder()`, `Client::default()` and the same of `ClientBuilder`,
// and `get(url)`, which builds a client of its own; for reqwest and for
// reqwest::blocking, qualified or through the names the file gave them.
function directClientPatterns(code) {
  const { crate, blocking, clientTypes, getFunctions } = reqwestNames(code);
  const any = (set) => [...set].join("|");
  const build = "(?:new|builder|default)";
  const patterns = [
    `(?<!\\w)(?:${any(crate)})::(?:blocking::)?get\\b`,
    `(?<!\\w)(?:${any(crate)})::(?:blocking::)?(?:Client|ClientBuilder)::${build}\\b`,
  ];
  if (blocking.size > 0) {
    patterns.push(`(?<![\\w:])(?:${any(blocking)})::get\\b`);
    patterns.push(`(?<![\\w:])(?:${any(blocking)})::(?:Client|ClientBuilder)::${build}\\b`);
  }
  if (clientTypes.size > 0) {
    patterns.push(`(?<![\\w:])(?:${any(clientTypes)})::${build}\\b`);
  }
  if (getFunctions.size > 0) {
    // A call, not a method (`.get(`), a path (`::get(`) or a definition.
    patterns.push(`(?<![\\w:.])(?<!\\bfn\\s+)(?:${any(getFunctions)})\\s*\\(`);
  }
  return patterns.map((pattern) => new RegExp(pattern));
}

export function validateHttpClientBoundary({ sourceOverrides = new Map() } = {}) {
  const sources = new Set([...trackedRustSources(), ...sourceOverrides.keys()]);
  for (const relativePath of [...sources].sort()) {
    if (relativePath === entry) {
      continue;
    }
    const source = sourceOverrides.has(relativePath)
      ? sourceOverrides.get(relativePath)
      : readFileSync(path.join(repoRoot, relativePath), "utf8");
    if (!source.includes("reqwest")) {
      continue;
    }
    let code;
    try {
      code = codeOnly(source);
    } catch (error) {
      fail(`cannot read ${relativePath}: ${error.message}`);
    }
    const calls = withoutImports(code);
    if (directClientPatterns(code).some((pattern) => pattern.test(calls))) {
      fail(`direct reqwest client in ${relativePath}; build it with orca_mcp::http`);
    }
  }
  return true;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  validateHttpClientBoundary();
  console.log("http client boundary passed");
}
