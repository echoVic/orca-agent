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
| `ORCA_REASONING_EFFORT` / `DEEPSEEK_REASONING_EFFORT` | Orca's own default (`max`) | Reasoning effort forwarded into the container: `low`, `high`, or `max` |
| `ORCA_PROVIDER` | configuration default | Provider override forwarded into the container |

## Model latency and the timeout class

TB2 task budgets are fixed (`[agent] timeout_sec` = 900–3600 s) and the median trial
spends ~60 % of its wall clock inside model generation, so token volume per turn — not
tool latency — is what decides the timeout class (issue #64). Two consequences for
benchmark runs:

- Use the reasoning-effort lever above to A/B latency under one budget. Measured on the
  same 16 tasks, `ORCA_REASONING_EFFORT=low` cut `video-processing` from 1295 s /
  135 k output tokens to 297 s / 32 k at the same reward, `mcmc-sampling-stan` from
  1773 s / 72 k to 683 s / 10 k, and turned `gcode-to-text` (900 s timeout) into a pass
  in 735 s — but it also lost `model-extraction-relu-logits` and `qemu-alpine-ssh`, so
  the lever has to be measured per task rather than assumed to be free.
- `agent/execution_metadata.json` records `duration_seconds` plus the largest `usage`
  counters (`turns`, `output_tokens`, `input_tokens`, `cache_tokens`, `cost_usd_micros`)
  seen in the stream, which exist even for a trial killed at the task timeout, because
  the task status events carry the running totals.


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
