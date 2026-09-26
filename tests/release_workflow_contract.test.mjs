import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import test from "node:test";

const root = path.resolve(import.meta.dirname, "..");
const workflow = readFileSync(path.join(root, ".github", "workflows", "release.yml"), "utf8");

test("release uses npm trusted publishing instead of a long-lived token", () => {
  assert.match(workflow, /Verify repository version sync\n\s+run: node scripts\/release\/verify-version-sync\.mjs/);
  assert.match(workflow, /npm:\n[\s\S]*permissions:\n\s+contents: read\n\s+id-token: write/);
  assert.match(workflow, /npm:\n[\s\S]*node-version: 22\.14\.0/);
  assert.doesNotMatch(workflow, /npm-auth:/);
  assert.doesNotMatch(workflow, /NPM_TOKEN/);
  assert.doesNotMatch(workflow, /npm whoami/);
  assert.match(workflow, /release:[\s\S]*needs: \[build, version, npm-trust\]/);
  assert.doesNotMatch(workflow, /NPM_TOKEN is not configured; npm publish skipped/);
});

test("npm trust is proven before any public asset exists", () => {
  const trust = workflow.slice(workflow.indexOf("  npm-trust:"), workflow.indexOf("  release:"));
  assert.match(trust, /permissions:\n\s+id-token: write/);
  assert.match(trust, /audience=npm:registry\.npmjs\.org/);
  assert.match(trust, /oidc\/token\/exchange\/package\/@blade-ai%2forca/);
});

test("npm publishes with an npm that performs the OIDC exchange", () => {
  const npmJob = workflow.slice(workflow.indexOf("\n  npm:\n"), workflow.indexOf("npm-release-assets:"));
  const pinned = npmJob.match(/npm install -g npm@(\d+)\.(\d+)\.(\d+)/);
  assert.ok(pinned, "the npm job must install a pinned npm");
  const [major, minor, patch] = pinned.slice(1).map(Number);
  assert.ok(major > 11 || (major === 11 && (minor > 5 || (minor === 5 && patch >= 1))), "npm >= 11.5.1");
});

test("every published package names the repository its provenance comes from", () => {
  const main = JSON.parse(readFileSync(path.join(root, "npm", "orca", "package.json"), "utf8"));
  const platform = JSON.parse(readFileSync(path.join(root, "npm", "platform-package.json"), "utf8"));
  assert.equal(main.repository.url, "git+https://github.com/echoVic/orca-agent.git");
  assert.equal(platform.repository.url, main.repository.url);
});

test("npm publishes the five immutable tarballs native-first and main-last", () => {
  const publish = workflow.slice(workflow.indexOf("- name: Publish npm packages"), workflow.indexOf("npm-release-assets:"));
  const names = ["darwin-arm64.tgz", "darwin-x64.tgz", "linux-arm64.tgz", "linux-x64.tgz", "${version}.tgz"];
  let cursor = -1;
  for (const name of names) {
    const next = publish.indexOf(name, cursor + 1);
    assert.ok(next > cursor, `${name} must appear in publication order`);
    cursor = next;
  }
  assert.match(publish, /npm publish "\$tarball"/);
  assert.match(publish, /registry_integrity/);
  assert.match(publish, /registry_output.*E404/s);
  assert.match(publish, /already published with identical integrity/);
});

test("final verification always runs and rejects a partial publication", () => {
  assert.match(workflow, /verify:\n\s+if: \$\{\{ always\(\) && github\.ref_type == 'tag' \}\}/);
  for (const variable of ["RELEASE_RESULT", "NPM_RESULT", "ASSETS_RESULT"]) {
    assert.match(workflow, new RegExp(`test "\\$${variable}" = success`));
  }
});
