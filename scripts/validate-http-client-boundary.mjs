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

// A `use` declaration at the start of a line. It names an item without
// calling it, so the call patterns look at the source without it.
const importDeclaration = /^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?use[ \t]+[^;]+;/gm;

function trackedRustSources() {
  return execFileSync("git", ["ls-files", "crates", "-z"], { cwd: repoRoot })
    .toString()
    .split("\0")
    .filter((file) => file.endsWith(".rs"));
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
function reqwestNames(source) {
  const names = {
    crate: new Set(["reqwest"]),
    blocking: new Set(),
    clientTypes: new Set(),
    getFunctions: new Set(),
  };
  for (const [, tree] of source.matchAll(/\buse\s+([^;]+);/g)) {
    for (const { path: leaf, alias } of useLeaves(tree)) {
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
  for (const [, name] of source.matchAll(
    /\btype\s+(\w+)\s*=\s*(?:::)?reqwest::(?:blocking::)?(?:Client|ClientBuilder)\s*;/g,
  )) {
    names.clientTypes.add(name);
  }
  for (const [, name] of source.matchAll(/\bextern\s+crate\s+reqwest\s+as\s+(\w+)\s*;/g)) {
    names.crate.add(name);
  }
  return names;
}

// Every way to build a client that skips the entry: `Client::new()`,
// `Client::builder()`, `Client::default()` and the same of `ClientBuilder`,
// and `get(url)`, which builds a client of its own; for reqwest and for
// reqwest::blocking, qualified or through the names the file gave them.
function directClientPatterns(source) {
  const { crate, blocking, clientTypes, getFunctions } = reqwestNames(source);
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
    const calls = source.replace(importDeclaration, "");
    if (directClientPatterns(source).some((pattern) => pattern.test(calls))) {
      fail(`direct reqwest client in ${relativePath}; build it with orca_mcp::http`);
    }
  }
  return true;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  validateHttpClientBoundary();
  console.log("http client boundary passed");
}
