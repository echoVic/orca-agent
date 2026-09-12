# Release Process

## Checklist

Every release must complete these steps in order.

### 1. Integrate code changes

Develop and verify changes on a branch. Merge through a reviewed pull request;
do not version or tag from an unintegrated branch. Before tagging, local
`main`, `origin/main`, and the intended release commit must match.

### 2. Bump versions

Update the version in **all three places** — they must match:

- `Cargo.toml` — `version = "x.y.z"`
- `npm/orca/package.json` — `"version": "x.y.z"`
- `Cargo.lock` — updated automatically by `cargo build` or `cargo check`

Commit all three together:

```sh
cargo check  # updates Cargo.lock
git add Cargo.toml npm/orca/package.json Cargo.lock
git commit -m "release: prepare vX.Y.Z"
```

### 3. Write release notes

Create `docs/releases/vX.Y.Z.md` following the format of an existing release note.
Include: summary sentence, Changes, Compatibility, Verification commands, Upgrade commands.

```sh
git add docs/releases/vX.Y.Z.md
git commit -m "docs: add vX.Y.Z release notes"  # or include in step 2 commit
```

### 4. Run pre-release checks

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -j 1
node --test scripts/test-validate-runtime-surface-contract.mjs
node scripts/validate-runtime-surface-contract.mjs
node --test scripts/test-validate-windows-platform-boundaries.mjs
node scripts/validate-windows-platform-boundaries.mjs
cargo nextest run -p orca-tui --lib --locked --profile ci-serial
cargo nextest run --workspace --all-targets --locked --profile ci --no-fail-fast
node scripts/release/test-verify-version-sync.mjs
node scripts/release/test-stage-npm.mjs
node scripts/release/test-verify-published.mjs
npm --prefix site run build
npm --prefix site run check:seo
git diff --check
```

Run this matrix before and after the version commit. Focused latency,
durability, dependency, and Windows behavior gates remain required even when a
broad workspace run passes. Clippy warnings are tracked separately from release
failures; do not hide them with a global allow, and do not turn a patch release
into an unrelated public-API cleanup merely to satisfy a newer toolchain lint.

For a tag-triggered release, `release.yml` verifies that the tagged commit is
the current `origin/main` commit and that the Linux `linux-full` check plus the
Windows `native-x64` and `native-arm64` checks already succeeded for that exact
SHA. These PR/main checks retain the complete runtime, subprocess, SQLite,
filesystem, and platform coverage. The tag workflow reuses that evidence
instead of rerunning the same full suites. A manual `workflow_dispatch` remains
the full release dry-run path and executes every gate above.

### 5. Update the website

Edit `site/src/shared.ts`:
- Set `releaseVersion` to `"vX.Y.Z"`
- Add a new entry at the top of the `releases` array

Edit `site/src/changelog/Changelog.tsx`:
- Add a `"vX.Y.Z": "..."` entry at the top of **both** the English and Chinese `summaries` objects

Verify the site builds:

```sh
npm --prefix site run build
npm --prefix site run check:seo
```

Commit:

```sh
git add site/src/shared.ts site/src/changelog/Changelog.tsx
git commit -m "chore(site): add vX.Y.Z to changelog and release list"
```

### 6. Push, merge, and tag

```sh
git push -u origin <release-branch>
# open the pull request, wait for required Linux and Windows checks, then merge
git switch main
git pull --ff-only origin main
git tag -a vX.Y.Z -F docs/releases/vX.Y.Z.md
git push origin vX.Y.Z
```

**Never force-push `main`, delete a published tag, or recreate an existing
tag.** Resolve the tag and remote target before creating it. If the intended
version already exists or points elsewhere, stop and choose a new version.

### 7. CI publishes automatically

The `release.yml` workflow is the sole publisher. It triggers on the tag push
and:
1. Verifies the tagged SHA already passed the required `main` checks
2. Builds binaries for all six targets, including native Windows x64 and ARM64
3. Creates a GitHub Release with binary assets
4. Stages, smoke-tests, and publishes npm packages

Monitor progress:

```sh
gh run list --repo echoVic/orca-agent --limit 5
```

### 8. Post-publish verification

```sh
node scripts/release/verify-published.mjs \
  --version X.Y.Z \
  --repo echoVic/orca-agent \
  --package @blade-ai/orca \
  --bin orca
```

## Common mistakes

| Mistake | Fix |
|---|---|
| Forgot `Cargo.lock` | `cargo check && git add Cargo.lock && git commit --amend` |
| Forgot site update | Push a follow-up commit to `site/src/` — pages workflow re-deploys automatically |
| Local main differs from origin/main | Stop and reconcile with a non-destructive fast-forward or reviewed PR |
| Version tag already exists or points elsewhere | Do not move it; select a new patch version |
| `summaries` missing new version in Changelog.tsx | TypeScript build fails — add entry to both EN and ZH summaries objects |
