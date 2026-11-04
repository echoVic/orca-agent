#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const repoRoot = path.resolve(import.meta.dirname, "..", "..");
const script = path.join(repoRoot, "scripts", "release", "verify-version-sync.mjs");
const tempDir = mkdtempSync(path.join(os.tmpdir(), "orca-version-sync-test-"));

function write(relative, value) {
  const file = path.join(tempDir, relative);
  mkdirSync(path.dirname(file), { recursive: true });
  writeFileSync(file, value);
}

function invoke() {
  try {
    return { ok: true, output: execFileSync(process.execPath, [script, "--root", tempDir], { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }) };
  } catch (error) { return { ok: false, output: `${error.stdout ?? ""}${error.stderr ?? ""}` }; }
}

try {
  write("Cargo.toml", '[package]\nversion = "1.2.3"\n');
  write("Cargo.lock", '[[package]]\nname = "orca-agent"\nversion = "1.2.3"\n');
  write("npm/orca/package.json", '{"version":"1.2.3"}\n');
  write("site/src/shared.ts", 'export const releaseVersion = "v1.2.3";\nconst releases = [{ version: "v1.2.3" }];\n');
  write("site/src/changelog/Changelog.tsx", 'const notes = { "v1.2.3": "ok" };\n');
  write("docs/releases/v1.2.3.md", "# Orca v1.2.3\n");
  const success = invoke();
  if (!success.ok || !success.output.includes("Version sync verified")) throw new Error(`version sync positive fixture failed: ${success.output}`);
  write("npm/orca/package.json", '{"version":"1.2.2"}\n');
  const drift = invoke();
  if (drift.ok || !drift.output.includes("npm/orca package")) throw new Error(`version drift was not rejected: ${drift.output}`);
  console.log("verify-version-sync release checks ok");
} finally {
  rmSync(tempDir, { recursive: true, force: true });
}
