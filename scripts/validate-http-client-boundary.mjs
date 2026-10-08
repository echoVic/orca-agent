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

function trackedRustSources() {
  return execFileSync("git", ["ls-files", "crates", "-z"], { cwd: repoRoot })
    .toString()
    .split("\0")
    .filter((file) => file.endsWith(".rs"));
}

// The names a file gives reqwest's client types: `use reqwest::Client;`,
// `use reqwest::blocking::{Client as BlockingClient, …};`, …
function importedClientNames(source) {
  const names = [];
  for (const [, body] of source.matchAll(/\buse\s+reqwest::([^;]+);/g)) {
    for (const [, kind, alias] of body.matchAll(/\b(Client|ClientBuilder)\b(?:\s+as\s+(\w+))?/g)) {
      names.push(alias ?? kind);
    }
  }
  return names;
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
    const qualified = /\breqwest::(?:blocking::)?(?:Client|ClientBuilder)::(?:new|builder)\s*\(/.test(source);
    const imported = importedClientNames(source).some((name) =>
      new RegExp(`\\b${name}::(?:new|builder)\\s*\\(`).test(source),
    );
    if (qualified || imported) {
      fail(`direct reqwest client in ${relativePath}; build it with orca_mcp::http`);
    }
  }
  return true;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  validateHttpClientBoundary();
  console.log("http client boundary passed");
}
