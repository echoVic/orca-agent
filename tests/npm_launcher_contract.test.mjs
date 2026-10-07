import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { chmodSync, cpSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { signalDispositions } from "../npm/orca/bin/signals.js";

const root = path.resolve(import.meta.dirname, "..");
const packageRoot = path.join(root, "npm", "orca");

test("on macOS and Linux the launcher passes every stop signal on to the binary", () => {
  for (const platform of ["darwin", "linux"]) {
    assert.deepEqual(
      signalDispositions(platform),
      { SIGINT: "forward", SIGTERM: "forward", SIGHUP: "forward" },
      platform,
    );
  }
});

// On Windows child.kill() is TerminateProcess whatever the signal: passing
// Ctrl+C on would end orca.exe before it can stop its run and exit with 130.
// The console gives orca.exe the Ctrl+C itself; the launcher only stays up.
test("on Windows the launcher leaves Ctrl+C to the binary and still passes SIGTERM on", () => {
  assert.deepEqual(signalDispositions("win32"), { SIGINT: "ignore", SIGTERM: "forward" });
});

test("the published package carries every file of the launcher", () => {
  const files = JSON.parse(readFileSync(path.join(packageRoot, "package.json"), "utf8")).files;
  const launcherFiles = readdirSync(path.join(packageRoot, "bin")).map((file) => `bin/${file}`);

  assert.ok(launcherFiles.includes("bin/signals.js"), "the launcher's signal module is gone");
  for (const file of launcherFiles) {
    assert.ok(files.includes(file), `package.json "files" leaves out ${file}`);
  }
});

// The launcher as installed: its own files, and a stand-in for the binary
// where it looks when no platform package is installed. The stand-in says
// when it is ready, and which signal reached it, and exits with 130 on SIGINT;
// left alone, it gives up after about 20 seconds, so that it never outlives a
// failed test for long.
test(
  "on macOS and Linux SIGINT reaches the binary, and the launcher exits with its code",
  { skip: process.platform === "win32" },
  async () => {
    const installed = mkdtempSync(path.join(os.tmpdir(), "orca-launcher-test-"));
    try {
      cpSync(path.join(packageRoot, "bin"), path.join(installed, "bin"), { recursive: true });
      cpSync(path.join(packageRoot, "package.json"), path.join(installed, "package.json"));
      for (const triple of [
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "aarch64-unknown-linux-gnu",
        "x86_64-unknown-linux-gnu",
      ]) {
        const binary = path.join(installed, "vendor", triple, "bin", "orca");
        mkdirSync(path.dirname(binary), { recursive: true });
        writeFileSync(
          binary,
          [
            "#!/bin/sh",
            "trap 'echo got SIGINT; exit 130' INT",
            "echo ready",
            "i=0",
            'while [ "$i" -lt 400 ]; do sleep 0.05; i=$((i + 1)); done',
            "exit 99",
            "",
          ].join("\n"),
        );
        chmodSync(binary, 0o755);
      }

      const launcher = spawn(process.execPath, [path.join(installed, "bin", "orca.js")], {
        stdio: ["ignore", "pipe", "inherit"],
      });
      let output = "";
      // "close", not "exit": by then all of the launcher's output has been read.
      const exited = new Promise((resolve) => launcher.on("close", (code, signal) => resolve({ code, signal })));
      await new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error(`the binary never started: ${output}`)), 10_000);
        launcher.stdout.on("data", (chunk) => {
          output += chunk;
          if (output.includes("ready")) {
            clearTimeout(timer);
            resolve();
          }
        });
      }).catch((error) => {
        launcher.kill("SIGKILL");
        throw error;
      });

      launcher.kill("SIGINT");
      const { code, signal } = await exited;

      assert.match(output, /got SIGINT/);
      assert.deepEqual({ code, signal }, { code: 130, signal: null });
    } finally {
      rmSync(installed, { recursive: true, force: true });
    }
  },
);

// The release smoke runs the launcher the way npm installs it: through the
// `.bin` link, with `node --preserve-symlinks-main`, so the launcher's own URL
// is the link's and not its real file's. A module beside the launcher must
// still load, and the platform package must still be found.
test(
  "run through npm's .bin link with --preserve-symlinks-main, the launcher still starts the binary",
  { skip: process.platform === "win32" },
  async () => {
    const platformPackages = {
      "darwin:arm64": ["@blade-ai/orca-darwin-arm64", "aarch64-apple-darwin"],
      "darwin:x64": ["@blade-ai/orca-darwin-x64", "x86_64-apple-darwin"],
      "linux:arm64": ["@blade-ai/orca-linux-arm64", "aarch64-unknown-linux-gnu"],
      "linux:x64": ["@blade-ai/orca-linux-x64", "x86_64-unknown-linux-gnu"],
    };
    const [platformPackage, triple] = platformPackages[`${process.platform}:${process.arch}`];
    const project = mkdtempSync(path.join(os.tmpdir(), "orca-launcher-link-test-"));
    try {
      const modules = path.join(project, "node_modules");
      const main = path.join(modules, "@blade-ai", "orca");
      cpSync(path.join(packageRoot, "bin"), path.join(main, "bin"), { recursive: true });
      cpSync(path.join(packageRoot, "package.json"), path.join(main, "package.json"));
      const platform = path.join(modules, ...platformPackage.split("/"));
      mkdirSync(platform, { recursive: true });
      writeFileSync(
        path.join(platform, "package.json"),
        JSON.stringify({ name: platformPackage, version: "0.0.0" }),
      );
      const binary = path.join(platform, "vendor", triple, "bin", "orca");
      mkdirSync(path.dirname(binary), { recursive: true });
      writeFileSync(
        binary,
        [
          "#!/bin/sh",
          "trap 'echo got SIGINT; exit 130' INT",
          "echo ready",
          "i=0",
          'while [ "$i" -lt 400 ]; do sleep 0.05; i=$((i + 1)); done',
          "exit 99",
          "",
        ].join("\n"),
      );
      chmodSync(binary, 0o755);
      const link = path.join(modules, ".bin", "orca");
      mkdirSync(path.dirname(link), { recursive: true });
      symlinkSync(path.join("..", "@blade-ai", "orca", "bin", "orca.js"), link);

      const launcher = spawn(process.execPath, ["--preserve-symlinks-main", link], {
        cwd: project,
        stdio: ["ignore", "pipe", "pipe"],
      });
      let output = "";
      let errors = "";
      launcher.stderr.on("data", (chunk) => {
        errors += chunk;
      });
      const exited = new Promise((resolve) => launcher.on("close", (code, signal) => resolve({ code, signal })));
      await new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error(`the binary never started: ${output}${errors}`)), 10_000);
        launcher.on("exit", (code) => {
          clearTimeout(timer);
          reject(new Error(`the launcher exited with ${code} before the binary started: ${errors}`));
        });
        launcher.stdout.on("data", (chunk) => {
          output += chunk;
          if (output.includes("ready")) {
            clearTimeout(timer);
            resolve();
          }
        });
      }).catch((error) => {
        launcher.kill("SIGKILL");
        throw error;
      });

      launcher.kill("SIGINT");
      const { code, signal } = await exited;

      assert.match(output, /got SIGINT/);
      assert.deepEqual({ code, signal }, { code: 130, signal: null });
    } finally {
      rmSync(project, { recursive: true, force: true });
    }
  },
);
