#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, copyFileSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";

const repoRoot = path.resolve(import.meta.dirname, "..", "..");
const script = path.join(repoRoot, "scripts", "release", "verify-published.mjs");
const tempDir = mkdtempSync(path.join(os.tmpdir(), "orca-verify-published-test-"));
const version = "9.8.7";
const targets = [
  ["darwin-arm64", "aarch64-apple-darwin", "orca", "tar.gz"],
  ["darwin-x64", "x86_64-apple-darwin", "orca", "tar.gz"],
  ["linux-arm64", "aarch64-unknown-linux-gnu", "orca", "tar.gz"],
  ["linux-x64", "x86_64-unknown-linux-gnu", "orca", "tar.gz"],
  ["win32-arm64", "aarch64-pc-windows-msvc", "orca.exe", "zip", ["orca-windows-runner.exe", "orca-windows-sandbox-setup.exe"]],
  ["win32-x64", "x86_64-pc-windows-msvc", "orca.exe", "zip", ["orca-windows-runner.exe", "orca-windows-sandbox-setup.exe"]],
];

function executable(filePath, contents) { writeFileSync(filePath, contents); chmodSync(filePath, 0o755); }
function sha(filePath, algorithm = "sha256", encoding = "hex") { return createHash(algorithm).update(readFileSync(filePath)).digest(encoding); }
function tar(source, destination, entries = ["."]) { execFileSync("tar", ["-C", source, "-czf", destination, ...entries]); }
function zip(source, destination, entry) { execFileSync("zip", ["-j", "-q", destination, path.join(source, entry)]); }

try {
  const fixture = path.join(tempDir, "fixture");
  const releaseDir = path.join(fixture, "release");
  const npmDir = path.join(fixture, "npm");
  const metadataDir = path.join(fixture, "metadata");
  const binDir = path.join(tempDir, "bin");
  for (const dir of [releaseDir, npmDir, metadataDir, binDir]) mkdirSync(dir, { recursive: true });
  const aliases = Object.fromEntries(targets.map(([suffix]) => [`@blade-ai/orca-${suffix}`, `npm:@blade-ai/orca@${version}-${suffix}`]));

  for (const [suffix, triple, binaryName, extension, helpers = []] of targets) {
    const binaryContent = `orca-binary-${triple}\n`;
    const releaseStage = path.join(tempDir, `release-${suffix}`);
    mkdirSync(releaseStage);
    executable(path.join(releaseStage, binaryName), binaryContent);
    for (const helper of helpers) executable(path.join(releaseStage, helper), `${helper}-${triple}\n`);
    if (helpers.length > 0) writeFileSync(path.join(releaseStage, "LICENSE"), "MIT\n");
    const archive = path.join(releaseDir, `orca-${triple}.${extension}`);
    if (extension === "zip") execFileSync("zip", ["-j", "-q", archive, ...[binaryName, ...helpers, ...(helpers.length > 0 ? ["LICENSE"] : [])].map((entry) => path.join(releaseStage, entry))]);
    else tar(releaseStage, archive, [binaryName]);
    writeFileSync(`${archive}.sha256`, `${sha(archive)}  ${path.basename(archive)}\n`);

    const packageVersion = `${version}-${suffix}`;
    const packageRoot = path.join(tempDir, `package-${suffix}`, "package");
    mkdirSync(path.join(packageRoot, "vendor", triple, "bin"), { recursive: true });
    writeFileSync(path.join(packageRoot, "package.json"), JSON.stringify({ name: "@blade-ai/orca", version: packageVersion }));
    executable(path.join(packageRoot, "vendor", triple, "bin", binaryName), binaryContent);
    for (const helper of helpers) executable(path.join(packageRoot, "vendor", triple, "bin", helper), `${helper}-${triple}\n`);
    const tarball = path.join(npmDir, `blade-ai-orca-${packageVersion}.tgz`);
    tar(path.dirname(packageRoot), tarball, ["package"]);
    copyFileSync(tarball, path.join(releaseDir, path.basename(tarball)));
    writeFileSync(path.join(metadataDir, `${packageVersion}.json`), JSON.stringify({ name: "@blade-ai/orca", version: packageVersion, dist: { integrity: `sha512-${sha(tarball, "sha512", "base64")}` } }));
  }

  const mainRoot = path.join(tempDir, "package-main", "package");
  mkdirSync(path.join(mainRoot, "bin"), { recursive: true });
  writeFileSync(path.join(mainRoot, "package.json"), JSON.stringify({ name: "@blade-ai/orca", version, optionalDependencies: aliases }));
  executable(path.join(mainRoot, "bin", "orca.js"), `#!/bin/sh\necho orca ${version}\n`);
  const mainTarball = path.join(npmDir, `blade-ai-orca-${version}.tgz`);
  tar(path.dirname(mainRoot), mainTarball, ["package"]);
  copyFileSync(mainTarball, path.join(releaseDir, path.basename(mainTarball)));
  writeFileSync(path.join(metadataDir, `${version}.json`), JSON.stringify({ name: "@blade-ai/orca", version, optionalDependencies: aliases, dist: { integrity: `sha512-${sha(mainTarball, "sha512", "base64")}` } }));

  executable(path.join(binDir, "gh"), `#!/usr/bin/env node
import { appendFileSync, copyFileSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";
const a = process.argv.slice(2), fixture = process.env.ORCA_VERIFY_FIXTURE;
if (a[0] === "release" && a[1] === "view") {
  let assets = readdirSync(path.join(fixture, "release")).map(name => ({ name }));
  if (process.env.ORCA_VERIFY_SCENARIO === "missing-asset") assets = assets.filter(a => !a.name.includes("linux-x64.tgz"));
  console.log(JSON.stringify({ tagName: "v${version}", url: "https://example.test", isDraft: false, assets }));
} else if (a[0] === "api") {
  console.log(process.env.ORCA_VERIFY_SCENARIO === "wrong-target" && a[1].endsWith("/v${version}") ? "tag-sha" : "main-sha");
} else if (a[0] === "release" && a[1] === "download") {
  const out = a[a.indexOf("--dir") + 1];
  for (const name of readdirSync(path.join(fixture, "release"))) copyFileSync(path.join(fixture, "release", name), path.join(out, name));
  if (process.env.ORCA_VERIFY_SCENARIO === "checksum-failure") writeFileSync(path.join(out, "orca-aarch64-apple-darwin.tar.gz.sha256"), "0".repeat(64) + "  bad\\n");
  if (process.env.ORCA_VERIFY_SCENARIO === "mismatched-npm-asset") appendFileSync(path.join(out, "blade-ai-orca-${version}.tgz"), "different");
} else process.exit(42);
`);
  executable(path.join(binDir, "npm"), `#!/usr/bin/env node
import { chmodSync, copyFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
const a = process.argv.slice(2), fixture = process.env.ORCA_VERIFY_FIXTURE;
if (a[0] === "view") {
  const version = a[1].slice(a[1].lastIndexOf("@") + 1);
  const metadata = JSON.parse(readFileSync(path.join(fixture, "metadata", version + ".json")));
  if (process.env.ORCA_VERIFY_SCENARIO === "bad-integrity") metadata.dist.integrity = "sha512-bad";
  // The registry returns object keys by length, then bytes, not in publish order.
  if (process.env.ORCA_VERIFY_SCENARIO === "registry-key-order" && metadata.optionalDependencies) {
    metadata.optionalDependencies = Object.fromEntries(Object.entries(metadata.optionalDependencies).sort(([a], [b]) => a.length - b.length || (a < b ? -1 : 1)));
  }
  if (process.env.ORCA_VERIFY_SCENARIO === "wrong-alias" && metadata.optionalDependencies) {
    metadata.optionalDependencies["@blade-ai/orca-linux-x64"] = "npm:@blade-ai/orca@0.0.0-linux-x64";
  }
  process.stdout.write(JSON.stringify(metadata));
} else if (a[0] === "pack") {
  const version = a[1].slice(a[1].lastIndexOf("@") + 1), out = a[a.indexOf("--pack-destination") + 1];
  const name = "blade-ai-orca-" + version + ".tgz";
  copyFileSync(path.join(fixture, "npm", name), path.join(out, name)); console.log(name);
} else if (a[0] === "install") {
  if (process.env.ORCA_VERIFY_SCENARIO === "install-failure") process.exit(44);
  const spec = a.at(-1), version = spec.slice(spec.lastIndexOf("@") + 1), cwd = process.cwd();
  const packageDir = path.join(cwd, "node_modules", "@blade-ai", "orca"), binDir = path.join(cwd, "node_modules", ".bin");
  mkdirSync(packageDir, { recursive: true }); mkdirSync(binDir, { recursive: true });
  writeFileSync(path.join(packageDir, "package.json"), JSON.stringify({ name: "@blade-ai/orca", version }));
  const bin = path.join(binDir, "orca"); writeFileSync(bin, "#!/bin/sh\\necho orca " + version + "\\n"); chmodSync(bin, 0o755);
} else process.exit(43);
`);

  function invoke(scenario = "ok") {
    try {
      const output = execFileSync(process.execPath, [script, "--version", version, "--repo", "echoVic/blade-deepseek", "--retries", "1", "--retry-delay-ms", "0"], {
        cwd: repoRoot,
        env: { ...process.env, PATH: `${binDir}${path.delimiter}${process.env.PATH}`, ORCA_VERIFY_FIXTURE: fixture, ORCA_VERIFY_SCENARIO: scenario },
        encoding: "utf8",
        stdio: ["ignore", "pipe", "pipe"],
      });
      return { ok: true, output };
    } catch (error) { return { ok: false, output: `${error.stdout ?? ""}${error.stderr ?? ""}` }; }
  }
  const success = invoke();
  if (!success.ok || !success.output.includes("Published release verified")) throw new Error(`positive verification failed: ${success.output}`);
  const reordered = invoke("registry-key-order");
  if (!reordered.ok || !reordered.output.includes("Published release verified")) throw new Error(`registry key order was rejected: ${reordered.output}`);
  for (const [scenario, expected] of [
    ["missing-asset", "missing asset"],
    ["wrong-target", "does not match main"],
    ["checksum-failure", "Checksum failure"],
    ["mismatched-npm-asset", "GitHub/npm tarball mismatch"],
    ["bad-integrity", "registry integrity mismatch"],
    ["wrong-alias", "wrong optional-dependency aliases"],
    ["install-failure", "Command failed"],
  ]) {
    const result = invoke(scenario);
    if (result.ok || !result.output.includes(expected)) throw new Error(`${scenario} was not rejected: ${result.output}`);
  }
  console.log("verify-published release checks ok");
} finally {
  rmSync(tempDir, { recursive: true, force: true });
}
