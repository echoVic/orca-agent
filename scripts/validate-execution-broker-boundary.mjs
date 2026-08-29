#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const allowedDirectProcessJob = new Set([
  "crates/orca-core/src/execution_broker.rs",
  "crates/orca-platform/src/process.rs",
  "crates/orca-tools/src/process.rs",
  "crates/orca-windows-runner/src/main.rs",
]);

function fail(message) {
  throw new Error(`execution broker boundary: ${message}`);
}

function trackedRustSources() {
  return execFileSync("git", ["ls-files", "crates", "-z"], { cwd: repoRoot })
    .toString()
    .split("\0")
    .filter((file) => file.endsWith(".rs"));
}

export function validateExecutionBrokerBoundary({ sourceOverrides = new Map() } = {}) {
  const sources = new Set([...trackedRustSources(), ...sourceOverrides.keys()]);
  for (const relativePath of [...sources].sort()) {
    if (relativePath.includes("/tests/") || relativePath.startsWith("crates/*/tests/")) {
      continue;
    }
    const source = sourceOverrides.has(relativePath)
      ? sourceOverrides.get(relativePath)
      : readFileSync(path.join(repoRoot, relativePath), "utf8");
    const directCalls = [...source.matchAll(/\bProcessJob::spawn(?:_named)?\s*\(/g)];
    if (directCalls.length > 0 && !allowedDirectProcessJob.has(relativePath)) {
      fail(`direct ProcessJob launch in ${relativePath}; use ExecutionBroker`);
    }
  }
  return true;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  validateExecutionBrokerBoundary();
  console.log("execution broker boundary passed");
}
