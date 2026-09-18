# Terminal-Bench Integration

Run [Terminal-Bench 2.0](https://www.tbench.ai/) against Orca using [Harbor](https://www.harborframework.com/).

## Prerequisites

```bash
uv tool install harbor
```

Docker must be running (Harbor spins up containers per task).

## Quick Start (Recommended)

Build a static musl binary first, then mount it into containers:

```bash
# Build static Linux binary (one-time, ~7 min)
docker run --rm -v "$(pwd):/src" -w /src \
  messense/rust-musl-cross:x86_64-musl \
  cargo build --release --target x86_64-unknown-linux-musl --bin orca

# Run benchmark (2 trials per task)
harbor run \
  -d "terminal-bench/terminal-bench-2" \
  --agent "terminal_bench.orca_agent:OrcaInstalledAgent" \
  -k 2 \
  --mounts '[{"type":"bind","source":"'"$(pwd)"'/target/x86_64-unknown-linux-musl/release","target":"/mnt/orca-bin","read_only":true}]'
```

## Single Task Smoke Test

```bash
harbor run \
  -d "terminal-bench/terminal-bench-2" \
  --agent "terminal_bench.orca_agent:OrcaInstalledAgent" \
  -k 2 \
  --mounts '[{"type":"bind","source":"'"$(pwd)"'/target/x86_64-unknown-linux-musl/release","target":"/mnt/orca-bin","read_only":true}]' \
  --include-task-name "terminal-bench/openssl-selfsigned-cert"
```

## Configuration

The API key is read from `~/.orca/auth.json` (`DEEPSEEK_API_KEY` field). Falls back to `ORCA_API_KEY` env var.

| Variable | Default | Description |
|----------|---------|-------------|
| `ORCA_BASE_URL` | `https://api.deepseek.com` | API endpoint |
| `ORCA_MODEL` | `deepseek-flash` | Model to use |

## Why musl?

Terminal-Bench containers use various base images with different glibc versions.
The release binary (`x86_64-unknown-linux-gnu`) requires glibc 2.39+, which many
task containers lack. The static musl binary works universally.

## Filtering Tasks

```bash
# Limit the number of tasks
harbor run -d "terminal-bench/terminal-bench-2" \
  --agent "terminal_bench.orca_agent:OrcaInstalledAgent" \
  --n-tasks 10 \
  --mounts '[{"type":"bind","source":"'"$(pwd)"'/target/x86_64-unknown-linux-musl/release","target":"/mnt/orca-bin","read_only":true}]'

# Single task
harbor run -d "terminal-bench/terminal-bench-2" \
  --agent "terminal_bench.orca_agent:OrcaInstalledAgent" \
  --include-task-name "terminal-bench/build-pov-ray" \
  --mounts '[{"type":"bind","source":"'"$(pwd)"'/target/x86_64-unknown-linux-musl/release","target":"/mnt/orca-bin","read_only":true}]'
```

## Package Setup

The adapter is exposed to Harbor via a `pyproject.toml` at the repo root.
After modifying the adapter code, reinstall:

```bash
uv tool install --force harbor \
  --with-editable . \
  --index-url https://pypi.org/simple/
```

## Agents

| File | Class | Description |
|------|-------|-------------|
| `orca_agent.py` | `OrcaInstalledAgent` | Copies musl binary from mount into container. |
| `orca_external.py` | `OrcaExternalAgent` | Uses a pre-built Orca binary on the host. |

The installed adapter writes Orca's raw JSONL output to the trial's
`agent/trajectory.jsonl` artifact for `harbor analyze` and `harbor view`.
Command output is not assigned to `AgentContext`: Harbor 0.20.0 does not define
an `output` field, and adding one aborts the trial before verifier execution.

The trajectory is persisted on **every** exit path. Harbor discards the
`ExecResult` when the command exits non-zero and when the exec is killed at the
task timeout, so the adapter also tees the stream to `/tmp/orca-trajectory.jsonl`
inside the container and downloads it after the run. `agent/execution_metadata.json`
records the real `exit_code`, `trajectory_bytes`, and a `trajectory_persisted`
flag, so a 0-byte artifact is visible instead of being reported as success.

## Quarantined tasks

Some dataset tasks cannot be scored no matter what the agent does, so they are excluded
from reported means instead of silently entering the score as a 0. The list lives in
[`quarantine.json`](quarantine.json); each entry records the task, the reason, and the
evidence path.

| Task | Why |
|------|-----|
| `terminal-bench/qemu-startup` | The task image's pinned Debian `bullseye-security` packages (`libcurl4`, `libnghttp2-14`) have been superseded, so the verifier's own `apt-get install` 404s and `/tests/test.sh` aborts (`curl: command not found`, `uvx: command not found`) before it can score. See `jobs/regression-20260916/qemu-startup__fLTBLJS/verifier/test-stdout.txt`. |

Exclude quarantined tasks from a run with Harbor's own filter:

```bash
harbor run -d "terminal-bench/terminal-bench-2" \
  --agent "terminal_bench.orca_agent:OrcaInstalledAgent" \
  --exclude-task-name "terminal-bench/qemu-startup" \
  --mounts '[{"type":"bind","source":"'"$(pwd)"'/target/x86_64-unknown-linux-musl/release","target":"/mnt/orca-bin","read_only":true}]'
```

This is an upstream `terminal-bench-2` image defect, not an Orca one: the adapter's own
package step cannot repair a task's `test.sh`, and the image re-installs unconditionally.
Remove an entry once the dataset pins a working image.
