#!/usr/bin/env node

import { existsSync, readFileSync } from "node:fs";
import path from "node:path";

let repoRoot = path.resolve(import.meta.dirname, "..", "..");
for (let index = 2; index < process.argv.length; index += 1) {
  if (process.argv[index] === "--root") repoRoot = path.resolve(process.argv[++index]);
  else throw new Error(`Unknown argument: ${process.argv[index]}`);
}

function read(relative) {
  return readFileSync(path.join(repoRoot, relative), "utf8");
}

function capture(relative, pattern, label) {
  const match = read(relative).match(pattern);
  if (!match) throw new Error(`Unable to read ${label} from ${relative}`);
  return match[1];
}

const cargoVersion = capture("Cargo.toml", /^version\s*=\s*"([^"]+)"/m, "Cargo version");
const checks = [
  ["Cargo.lock root package", capture("Cargo.lock", /name = "orca-agent"\nversion = "([^"]+)"/, "Cargo.lock root version")],
  ["npm/orca package", JSON.parse(read("npm/orca/package.json")).version],
  ["site releaseVersion", capture("site/src/shared.ts", /releaseVersion = "v([^"]+)"/, "site releaseVersion")],
  ["site latest release entry", capture("site/src/shared.ts", /version: "v([^"]+)"/, "site latest release")],
];

const failures = checks
  .filter(([, actual]) => actual !== cargoVersion)
  .map(([label, actual]) => `${label}: expected ${cargoVersion}, got ${actual}`);
for (const [relative, label] of [
  [`docs/releases/v${cargoVersion}.md`, "release note"],
]) {
  if (!existsSync(path.join(repoRoot, relative))) failures.push(`${label}: missing ${relative}`);
}
if (!read("site/src/changelog/Changelog.tsx").includes(`"v${cargoVersion}"`)) {
  failures.push(`site changelog: missing v${cargoVersion}`);
}

if (failures.length > 0) throw new Error(`Version sync failed:\n- ${failures.join("\n- ")}`);
console.log(`Version sync verified: ${cargoVersion}`);
