# DeepSeek Agent Capabilities

## Scope

Implement cache-aware context management, file-defined agents, and ACP
leader/follower sessions. Keep Orca DeepSeek-native. Preserve existing user
commits `028746cb` and `33c165c5`; do not create worktrees.

Remote summaries already exist (`docs/reports/2026-08-10-compaction-remote-evaluation.md`).
Extend that production path; do not invent a DeepSeek compaction endpoint.
Prefix reuse is constrained by context pressure and semantic correctness. A
rewritten historical suffix cannot retain its old KV cache.

### Task 1: Baseline and Ownership

- [x] Read current repository and project constraints.
- [x] Verify existing remote summary evaluation and clean initial worktree.
- [x] Map implementation boundaries and existing tests.

### Task 2: Implement Independent Feature Slices

- [x] Context: stable immutable system prefix, cache-aware reduction of complete
  turns, bounded hierarchical DeepSeek summaries, image budgeting, deterministic
  prefix-reuse measurements, and safe fallback/cancellation.
- [x] Output: durable bounded offset reads, restart recovery, explicit cursor and
  truncation semantics, with authorization through the existing tool path.
- [x] Agents: project/user Markdown definitions, strict parsed frontmatter,
  discovery, model-visible catalog, permission narrowing, and immutable effective
  definitions across detached execution and resume.
- [x] ACP: one local daemon owns hosted sessions, standard ACP connections,
  exclusive mutation with multiple attached observers, client disconnection and
  reattachment without losing durable state, CLI/editor/headless/TUI entry paths.

### Task 3: Integration and Acceptance

- [x] Relevant unit, contract, and cross-process integration suites; targeted
  feature checks pass, and broad-suite failures are inventoried in the report.
- [x] Real DeepSeek comparisons with framework retries disabled where available;
  record credential/endpoint constraints and never count a skip as a pass.
- [x] Interactive TUI smoke and reconnection behavior.
- [x] Formatting, clippy, documentation and site checks; existing full-workspace
  formatting/lint debt remains separate from passing changed-file checks.
- [x] Record exact remaining limitations; do not mark the goal complete while
  required behavior remains unimplemented or unverified.

## Acceptance Matrix

| Area | Required cases |
| --- | --- |
| Context | soft/hard pressure, unchanged leading prefix, tool-call/result pairing, pending calls, huge single messages, summary failure/cancellation, repeated compaction, image retention/budgets, recovery |
| Output | initial page, next page, repeated cursor, UTF-8 boundary, empty/EOF, invalid cursor, truncation, restart, missing artifact, unauthorized/outside-root access |
| Agents | user/project precedence, invalid/duplicate definitions, reserved names, unknown tools/model, inherited permission intersection, sync/async execution, changed/deleted definition after launch, resume |
| ACP | new/load/list, two clients one session, updates to observers, single active prompt, permissions routed to owner, disconnect during prompt, reconnect/load, daemon restart, stale socket, endpoint permissions, graceful shutdown |
| Clients | editor stdio bridge, headless entry, TUI attach/input/output/resume, cancellation, no duplicate history after reattach |

## Resource Ownership

- Initial `.claude/ralph-loop.local.md`: absent. Temporary coordinator state owned
  by goal `6aa152d440f16a65e856582c`; removed after completion.
- `/tmp/orca-capabilities-tests.l7crcG`: newly allocated empty test preparation
  directory owned by this goal; removed after collecting verification results
  and verifying that all logged commands had settled.
- `/private/tmp/orca-agent-acceptance-XfHrjL`: isolated real-provider recovery
  repro, retained explicitly for ownership inspection; removed after validation.
- `/tmp/orca-nextest-current.GuDzai`: release-profile nextest logs owned by this
  continuation; removed after recording the full result inventory.
- Temporary TUI daemon PIDs 82990 and 46862: exited and reaped. The user's
  existing Orca process and sessions are untouched.
- Test directories must be RAII temporary directories or recorded here before
  use. Do not use or alter the user's actual session store or credentials.
- Do not publish releases or mutate remote repositories as part of local
  implementation without a release request.

## Results

See [validation evidence](../../reports/2026-09-09-deepseek-capabilities-validation.md).
Custom-child continuation ownership across recorded parent restarts is verified
for synchronous and detached sources and all four execution-mode transitions.
The actor records the association between the generation and registry root;
synthetic loop IDs are never treated as registry task identities. The real
provider matrix passed three consecutive runs after clarifying the fixture
instructions; the earlier model-output deviation remains in the report.
