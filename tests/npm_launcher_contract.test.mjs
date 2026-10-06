import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { chmodSync, cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
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

test("the published package carries every module the launcher imports", () => {
  const launcher = readFileSync(path.join(packageRoot, "bin", "orca.js"), "utf8");
  const files = JSON.parse(readFileSync(path.join(packageRoot, "package.json"), "utf8")).files;
  const imported = [...launcher.matchAll(/from "\.\/([^"]+)"/g)].map(([, file]) => `bin/${file}`);

  assert.ok(imported.length > 0, "the launcher imports no module of its own");
  for (const file of imported) {
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
