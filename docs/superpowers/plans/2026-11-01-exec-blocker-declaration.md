# Exec Blocker Declaration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `report_blocker` tool to headless `orca exec` so the model can declare a blocker, producing `status: "blocked"` and exit code 5 instead of the current misleading `success` / exit 0.

**Architecture:** Add `RunStatus::Blocked` and `OperationTerminal::Blocked` to the core types, register a new `report_blocker` built-in tool that ends the agent loop with that status, and thread the new status through the same post-loop consistency point the `--verifier` override already uses so every output (JSONL, exit code, surface terminal) agrees.

**Tech Stack:** Rust, `serde_json`, existing `BlockerKind`/`BlockerSummary`/`EvidenceItem` from `orca-core`.

**Spec:** `docs/superpowers/specs/2026-11-01-exec-blocker-declaration-design.md`

## Global Constraints

- **Rust edition 2024**, `#![deny(deprecated)]` in `orca-runtime` and `orca-tui` lib roots.
- **No new crate dependencies.**
- **Do not change goal mode** (`update_goal`, `GoalState::Blocked`, `BlockerSummary` in `goal_runtime.rs`). Reuse the existing `BlockerKind`/`EvidenceItem` types; never copy them.
- **`report_blocker` is only available in headless root sessions** (depth 0, not goal mode). Subagents, TUI, ACP, server: no new tool, no status change.
- **Exit code 5** for `Blocked` — the next free code after budget's `4`.
- **`--verifier` does not run on `Blocked`.** The verifier only runs on `Success` (controller.rs:902).
- **One commit per task**, with the subject lines given below. Never push. `env -u NODE_OPTIONS` for every `cargo test` and `node` command.
- **Never run `--ignored` tests.** Never touch `~/.orca`; tests use their own ORCA_HOME guards.
- **Validator baseline update rule:** if `node scripts/validate-runtime-surface-contract.mjs` fails because of your change, update the baseline in the same commit and say so in your summary. Same for `validate-windows-platform-boundaries.mjs`.

## Review Focus

1. **Exhaustive match over `OperationTerminal`:** adding the `Blocked` variant breaks every exhaustive `match` on this enum in `event_schema.rs`, `session_completed_terminal_with_verification`, the surface reducer, the ACP terminal mapping, and `OperationTerminal::exit_code`. Each must handle the new variant or the build fails — verify every compile error is fixed.
2. **`session.completed` consistency:** the JSONL status string and the typed terminal object in the payload both derive from `OperationTerminal`. If `session_completed_terminal_with_verification` misses the `Blocked` arm, it silently falls through to the existing match's default; pin the new string `"blocked"` in an `exec_jsonl` contract test.
3. **The running-dispatch path** (`handle_running_command`) never calls `report_blocker` because the tool is only available at depth 0; the tool execution and agent loop run in the headless path, not the running dispatch. But the surface terminal must accept `Blocked` without panicking — check `finish_surface_operation` and the surface reducer for `OperationTerminal::Blocked` coverage.
4. **`--verifier` interaction:** confirm that `run_verifier_if_needed` is not called when status is `Blocked` (the existing guard at controller.rs:902 already checks `Success`; add a contract test asserting exit 5 even when `--verifier` is set to a passing script).
5. **ACP `terminal_mapping_preserves_only_exact_standard_stop_reasons`** (`acp/agent.rs:3819`): that test will fail to compile if `terminal_to_stop_reason` is exhaustive over `OperationTerminal` and the new variant is not handled. Add `Blocked` as `Err(())` (same as `JoinFailed` etc.) there and the test should pass.

---

### Task 1: Core types — `RunStatus::Blocked` and `OperationTerminal::Blocked`

**Files:**
- Modify: `crates/orca-core/src/event_schema.rs` (RunStatus)
- Modify: `crates/orca-core/src/budget.rs` (OperationTerminal)
- Modify: `crates/orca-core/src/event_schema.rs` (session_completed_terminal_with_verification)
- Test: `crates/orca-core/src/event_schema.rs` (inline tests at end of file)

**Interfaces:**
- Produces:
  - `RunStatus::Blocked { blocker: BlockerSummary }` — `as_str()` = `"blocked"`, `exit_code()` = `5`
  - `OperationTerminal::Blocked { blocker: BlockerSummary, usage: BudgetUsage }` — `usage()` = `Some(usage)`, `exit_code()` = `5`, `as_str()` = `"blocked"`
  - `session_completed_terminal_with_verification` emits `"blocked"` for the new variant

- [ ] **Step 1.1: Write failing tests** in `crates/orca-core/src/event_schema.rs`

```rust
#[test]
fn blocked_run_status_has_correct_string_and_exit_code() {
    use orca_core::goal_runtime::{BlockerKind, BlockerSummary};
    let blocker = BlockerSummary {
        kind: BlockerKind::EnvironmentContradiction,
        summary: "workspace is read-only".to_string(),
        fingerprint: "fp-1".to_string(),
        evidence: vec![],
    };
    let status = RunStatus::Blocked { blocker };
    assert_eq!(status.as_str(), "blocked");
    assert_eq!(status.exit_code(), 5);
}

#[test]
fn blocked_operation_terminal_has_correct_exit_code() {
    use orca_core::budget::{BudgetUsage, OperationTerminal};
    use orca_core::goal_runtime::{BlockerKind, BlockerSummary};
    let terminal = OperationTerminal::Blocked {
        blocker: BlockerSummary {
            kind: BlockerKind::EnvironmentContradiction,
            summary: "workspace is read-only".to_string(),
            fingerprint: "fp-1".to_string(),
            evidence: vec![],
        },
        usage: BudgetUsage::default(),
    };
    assert_eq!(terminal.exit_code(), 5);
    assert_eq!(terminal.as_str(), "blocked");
    assert!(terminal.usage().is_some());
}

#[test]
fn session_completed_terminal_emits_blocked_status() {
    use orca_core::budget::{BudgetUsage, OperationTerminal};
    use orca_core::goal_runtime::{BlockerKind, BlockerSummary};
    let terminal = OperationTerminal::Blocked {
        blocker: BlockerSummary {
            kind: BlockerKind::EnvironmentContradiction,
            summary: "workspace is read-only".to_string(),
            fingerprint: "fp-1".to_string(),
            evidence: vec![],
        },
        usage: BudgetUsage::default(),
    };
    let mut factory = EventFactory::new("run-1".to_string());
    let draft = factory.session_completed_terminal(&terminal, None);
    let payload = draft.payload();
    assert_eq!(payload["status"], "blocked");
    assert!(payload["terminal"]["blocked"].is_object());
}
```

- [ ] **Step 1.2: Run test to see them fail**

```
env -u NODE_OPTIONS cargo test -p orca-core blocked -- --test-threads=1
```
Expected: compile error (variants do not exist yet).

- [ ] **Step 1.3: Add `RunStatus::Blocked { blocker: BlockerSummary }` to `event_schema.rs:333`**

Import `BlockerSummary` from `orca_core::goal_runtime`. Add to `as_str()`: `Blocked { .. } => "blocked"`. Add to `exit_code()`: `Blocked { .. } => 5`.

- [ ] **Step 1.4: Add `OperationTerminal::Blocked { blocker: BlockerSummary, usage: BudgetUsage }` to `budget.rs:170`**

Add to `usage()`: `Blocked { usage, .. } => Some(*usage)`. Add to `exit_code()`: `Blocked { .. } => 5`. Add to `as_str()`: `Blocked { .. } => "blocked"`.

- [ ] **Step 1.5: Update `session_completed_terminal_with_verification` in `event_schema.rs:1284`**

Add `OperationTerminal::Blocked { .. } => "blocked"` to the match arm.

- [ ] **Step 1.6: Fix every other exhaustive match over `OperationTerminal` in `orca-core`**

Run `env -u NODE_OPTIONS cargo check -p orca-core` and fix every compile error. Each new `Blocked` arm should mirror the `Completed` arm: return usage, or the closest sensible default.

- [ ] **Step 1.7: Run tests**

```
env -u NODE_OPTIONS cargo test -p orca-core blocked -- --test-threads=1
```
Expected: all three tests PASS.

- [ ] **Step 1.8: Commit**

```
git add crates/orca-core/src/event_schema.rs crates/orca-core/src/budget.rs
git commit -m "feat(core): add RunStatus::Blocked and OperationTerminal::Blocked with exit code 5"
```

---

### Task 2: Fix compile errors in `orca-runtime` from the new `OperationTerminal` variant

**Files:**
- Modify: `crates/orca-runtime/src/lifecycle.rs` (AgentLoopResult::terminal_from_status)
- Modify: `crates/orca-runtime/src/runtime_host.rs` (headless exit-code path, headless_operation_status)
- Modify: `crates/orca-runtime/src/controller.rs` (post-loop, verifier, run_verifier_if_needed)
- Modify: `crates/orca-runtime/src/acp/agent.rs` (terminal_to_stop_reason)
- Modify: any other exhaustive match the compiler reports

**Interfaces:**
- Consumes: `RunStatus::Blocked { blocker }` and `OperationTerminal::Blocked` from Task 1
- Produces: workspace that compiles with the two new variants handled everywhere

- [ ] **Step 2.1: Compile the workspace and collect every error**

```
env -u NODE_OPTIONS cargo check --workspace 2>&1 | grep "^error" | head -40
```

- [ ] **Step 2.2: Fix `AgentLoopResult::terminal_from_status` (`lifecycle.rs:150`)**

Add arm: `RunStatus::Blocked { ref blocker }` → `OperationTerminal::Blocked { blocker: blocker.clone(), usage: BudgetUsage::default() }`.

The blocker must travel from the `AgentLoopResult` that carries the status. `terminal_from_status` takes `status` and `error`; it has no blocker today. The cleanest fix: add an `Option<BlockerSummary>` argument to `terminal_from_status` and thread it through its three call sites (all three currently pass `None`). When `Blocked`, use the blocker; when `None`, produce `Failed { class: RuntimeInvariant, … }` as a fallback.

- [ ] **Step 2.3: Fix `headless_operation_status` and `headless_operation_exit_code` (`controller.rs:1945-1969`)**

Add:
```rust
SurfaceOperationTerminal::Blocked { blocker, .. } => RunStatus::Blocked { blocker: /* map from surface to core */ },
```
and in `headless_operation_exit_code`: `SurfaceOperationTerminal::Blocked { .. } => 5`.

The surface `OperationTerminal` is a separate type from the core one. Check `crates/orca-runtime/src/runtime_surface/operation.rs` for its definition and add `Blocked` there too (Step 2.4).

- [ ] **Step 2.4: Add `Blocked` to the surface `OperationTerminal` if it is distinct**

If `crates/orca-runtime/src/runtime_surface/operation.rs` defines its own `OperationTerminal`, add the `Blocked` variant and fix every exhaustive match the compiler reports in that file and its callers (surface reducer, surface projection, ACP mapping).

- [ ] **Step 2.5: Fix `terminal_to_stop_reason` in `acp/agent.rs`**

Add `OperationTerminal::Blocked { .. } => Err(())` — the same as `JoinFailed`, `AbortedByRuntimeRestart`, etc. This ensures `terminal_mapping_preserves_only_exact_standard_stop_reasons` compiles and passes.

- [ ] **Step 2.6: Fix every remaining exhaustive match**

```
env -u NODE_OPTIONS cargo check --workspace 2>&1 | grep "^error"
```
Repeat until zero errors. Common locations: `runtime_event_projector.rs`, `operation_terminal_contract.rs` tests, `server/surface_adapter.rs`, `runtime_host.rs` headless event emitter.

- [ ] **Step 2.7: Build the full workspace**

```
env -u NODE_OPTIONS cargo build --workspace --locked
```
Expected: zero errors, warnings only on unchanged code.

- [ ] **Step 2.8: Commit**

```
git add -u
git commit -m "fix(runtime): handle RunStatus::Blocked and OperationTerminal::Blocked in exhaustive matches"
```

---

### Task 3: `report_blocker` tool — parsing, validation, and schema

**Files:**
- Create: `crates/orca-tools/src/report_blocker.rs`
- Modify: `crates/orca-tools/src/lib.rs` (re-export)
- Modify: `crates/orca-core/src/tool_types.rs` (ToolName::ReportBlocker)
- Test: `crates/orca-tools/src/report_blocker.rs` (inline tests)

**Interfaces:**
- Consumes: `BlockerKind`, `BlockerSummary`, `EvidenceItem` from `orca-core::goal_runtime`
- Produces:
  - `ToolName::ReportBlocker` registered in `from_str`/`plain` as `"report_blocker"`
  - `pub fn parse_report_blocker_args(request: &ToolRequest) -> Result<BlockerSummary, String>` — validates kind, summary ≤ 2000 chars, evidence ≤ 32 items
  - JSON schema matching `report_blocker` arg spec
  - `pub fn execute_report_blocker(request: &ToolRequest) -> ToolResult` — returns `execute_unavailable` (same pattern as `update_goal::execute_update`; the real execution happens in the router)

- [ ] **Step 3.1: Write failing tests** in `crates/orca-tools/src/report_blocker.rs`

```rust
#[test]
fn parses_valid_args() {
    let request = ToolRequest {
        id: "tc-1".to_string(),
        name: ToolName::ReportBlocker,
        action: ActionKind::Agent,
        target: None,
        raw_arguments: Some(r#"{"kind":"environment_contradiction","summary":"workspace is read-only"}"#.to_string()),
    };
    let blocker = parse_report_blocker_args(&request).expect("valid args");
    assert_eq!(blocker.kind, BlockerKind::EnvironmentContradiction);
    assert_eq!(blocker.summary, "workspace is read-only");
    assert!(blocker.evidence.is_empty());
}

#[test]
fn rejects_summary_over_limit() {
    let long = "x".repeat(2001);
    let request = ToolRequest {
        id: "tc-2".to_string(),
        name: ToolName::ReportBlocker,
        action: ActionKind::Agent,
        target: None,
        raw_arguments: Some(format!(r#"{{"kind":"user_decision","summary":"{long}"}}"#)),
    };
    assert!(parse_report_blocker_args(&request).is_err());
}

#[test]
fn rejects_too_many_evidence_items() {
    let evidence: Vec<_> = (0..33).map(|i| serde_json::json!({"kind": "observation", "summary": format!("item {i}")})).collect();
    let request = ToolRequest {
        id: "tc-3".to_string(),
        name: ToolName::ReportBlocker,
        action: ActionKind::Agent,
        target: None,
        raw_arguments: Some(serde_json::to_string(&serde_json::json!({"kind":"user_decision","summary":"s","evidence":evidence})).unwrap()),
    };
    assert!(parse_report_blocker_args(&request).is_err());
}
```

- [ ] **Step 3.2: Run tests to see them fail**

```
env -u NODE_OPTIONS cargo test -p orca-tools report_blocker -- --test-threads=1
```
Expected: compile error.

- [ ] **Step 3.3: Add `ToolName::ReportBlocker` to `tool_types.rs`**

Add the variant and its `"report_blocker"` string in `from_str`/`plain`.

- [ ] **Step 3.4: Create `crates/orca-tools/src/report_blocker.rs`**

- Constants: `MAX_REPORT_BLOCKER_SUMMARY_CHARS = 2_000`, `MAX_REPORT_BLOCKER_EVIDENCE_ITEMS = 32` (same values as `update_goal`).
- `fn parse_report_blocker_args(request: &ToolRequest) -> Result<BlockerSummary, String>`: deserialize raw_arguments into a struct with `kind: BlockerKind`, `summary: String`, `evidence: Vec<EvidenceItem>`; validate caps; build and return a `BlockerSummary { kind, summary, fingerprint: uuid_v7_string(), evidence }`.
- `pub fn execute_report_blocker(request: &ToolRequest) -> ToolResult`: delegates to `execute_unavailable(request)` — the router intercepts it before this is reached.

- [ ] **Step 3.5: Register `report_blocker` in the tool registry (`crates/orca-tools/src/registry.rs`)**

Add a `BuiltinTool::new(spec, BuiltinExecutor::ReportBlocker)` next to `UpdateGoal`. The spec: `safe_local_read_builtin_spec` (no write capability needed; the tool ends the turn, it does not write files). The `CapabilitySet` is `CapabilitySet::new(vec![])` (read-only; the action kind will be set to `ActionKind::Agent` like `UpdateGoal`). Add `BuiltinExecutor::ReportBlocker` to the enum and its dispatch arm.

The JSON schema (inline `serde_json::json!`):
```json
{
  "type": "object",
  "properties": {
    "kind": {
      "type": "string",
      "enum": ["user_decision","missing_authority","external_state","environment_contradiction","unverifiable_requirement"],
      "description": "Category of the blocker."
    },
    "summary": {
      "type": "string",
      "description": "Short explanation of why progress is blocked (≤ 2000 characters)."
    },
    "evidence": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "kind": {"type": "string"},
          "summary": {"type": "string"},
          "target": {"type": "string"}
        },
        "required": ["kind","summary"]
      },
      "description": "Optional supporting observations (≤ 32 items)."
    }
  },
  "required": ["kind","summary"]
}
```

- [ ] **Step 3.6: Add `report_blocker` to `ToolPolicy::base` (and only there)**

The tool must appear in root non-goal sessions and nowhere else. In `registry.rs`, in the `canonical_tool_definitions` function (or wherever `ToolPolicy::goal` vs `ToolPolicy::base` filtering happens), include `ReportBlocker` only in the base policy, not the goal policy (goal mode has `update_goal` instead). Subagent depth > 0 must not see it — the existing `ToolPolicy::for_subagent` already excludes goal tools; add `ReportBlocker` to whatever list goal-only tools live on so it is excluded for subagents.

- [ ] **Step 3.7: Run tests**

```
env -u NODE_OPTIONS cargo test -p orca-tools report_blocker -- --test-threads=1
```
Expected: all three tests PASS.

- [ ] **Step 3.8: Confirm tool appears in root schema and not in subagent schema**

```
env -u NODE_OPTIONS cargo test -p orca-runtime --lib provider_tool_schema_override -- --test-threads=1
```
Expected: existing tests pass. If a test asserts exact tool counts, update it to include `report_blocker`.

- [ ] **Step 3.9: Commit**

```
git add crates/orca-core/src/tool_types.rs crates/orca-tools/src/report_blocker.rs crates/orca-tools/src/registry.rs crates/orca-tools/src/lib.rs
git commit -m "feat(tools): add report_blocker tool with arg parsing and registry entry"
```

---

### Task 4: Agent loop — make `report_blocker` end the turn with `Blocked`

**Files:**
- Modify: `crates/orca-runtime/src/tool_router.rs` (route the tool)
- Modify: `crates/orca-runtime/src/tool_execution.rs` or `step_context.rs` (produce `Return { Blocked }` outcome)
- Modify: `crates/orca-runtime/src/lifecycle.rs` (`AgentLoopResult::blocked`)
- Test: `crates/orca-runtime/src/tool_router.rs` or the file that owns tool dispatch tests

**Interfaces:**
- Consumes: `ToolName::ReportBlocker`, `BlockerSummary` from Task 3
- Produces:
  - `AgentLoopResult::blocked(blocker: BlockerSummary) -> AgentLoopResult` — `status: RunStatus::Blocked { blocker }`, `terminal: OperationTerminal::Blocked { blocker, usage: BudgetUsage::default() }`, `final_message: None`, `error: None`, `reason: TurnEndReason::Unclassified`
  - A `report_blocker` call in the agent loop produces a `Return` outcome that becomes this result

- [ ] **Step 4.1: Write failing test** — a unit test that exercises the tool-dispatch path for `report_blocker` and asserts `Blocked` status

Add a test in `crates/orca-runtime/src/tool_router.rs` (or the relevant test file):
```rust
#[test]
fn report_blocker_produces_blocked_loop_result() {
    // Build a minimal ToolRequest for report_blocker with valid JSON args
    // Route it through the tool dispatch path
    // Assert: the returned ToolTurnOutcome::Return has status Blocked and carries the blocker
}
```
(The exact test shape depends on how existing tool tests in that file are set up. Follow their pattern.)

- [ ] **Step 4.2: Add `AgentLoopResult::blocked(blocker: BlockerSummary) -> Self` to `lifecycle.rs`**

```rust
pub(crate) fn blocked(blocker: BlockerSummary) -> Self {
    Self {
        status: RunStatus::Blocked { blocker: blocker.clone() },
        reason: TurnEndReason::Unclassified,
        final_message: None,
        error: None,
        terminal: OperationTerminal::Blocked {
            blocker,
            usage: BudgetUsage::default(),
        },
    }
}
```

- [ ] **Step 4.3: Intercept `ToolName::ReportBlocker` in the tool router**

In `tool_router.rs`, where `ToolName::UpdateGoal` is routed to the special runtime handler, add a matching route for `ToolName::ReportBlocker`:

1. Parse the args with `report_blocker::parse_report_blocker_args(&request)`.
2. On success: record a `tool.call.completed` event with a brief success message ("Blocker declared."), then return `ToolTurnOutcome::Return { status: RunStatus::Blocked { blocker: blocker.clone() }, error: None, terminal: Some(OperationTerminal::Blocked { blocker, usage }) }`.
3. On parse error: return `ToolResult::failed(request, error, None)` as `ToolTurnOutcome::Continue` (invalid args are not a reason to block; the model should retry with valid input).

The pattern mirrors how `UpdateGoal` dispatches to `RuntimeGoalToolRequest` and returns a terminal outcome; follow that shape.

- [ ] **Step 4.4: Confirm `run_verifier_if_needed` skips `Blocked`**

In `controller.rs:902`, `run_verifier_if_needed` is called only when `status == RunStatus::Success`. After adding `Blocked` to `RunStatus`, verify that the guard still reads `status != RunStatus::Success` (or an exact `Success` check) so `Blocked` correctly skips the verifier. If it is a non-exhaustive list, add `Blocked` to the skip set.

- [ ] **Step 4.5: Run test**

```
env -u NODE_OPTIONS cargo test -p orca-runtime report_blocker -- --test-threads=1
```
Expected: the new test PASSES.

- [ ] **Step 4.6: Commit**

```
git add crates/orca-runtime/src/lifecycle.rs crates/orca-runtime/src/tool_router.rs
git commit -m "feat(runtime): report_blocker ends the agent loop with Blocked status"
```

---

### Task 5: System prompt and tool description

**Files:**
- Modify: `crates/orca-runtime/src/runtime_host.rs` or wherever the exec system prompt is built (search for `HeadlessSession` or `headless_session`)
- Modify: the tool spec string in `crates/orca-tools/src/registry.rs`

**Interfaces:**
- Consumes: the exec session prompt location and the `report_blocker` spec from Task 3
- Produces: model is told to call `report_blocker` when it can't make progress due to external blockers

- [ ] **Step 5.1: Update the `report_blocker` tool description in the spec**

In `registry.rs`, the tool's description string must say:
> Call this tool when you cannot make progress because of something outside your control — for example, a read-only workspace, missing credentials, no network access, or a requirement you cannot satisfy. This ends the session and records the blocker. Do not claim success or simply stop: call this tool instead.

- [ ] **Step 5.2: Find the exec system prompt location**

```
grep -rn "headless\|exec.*prompt\|EXEC.*prompt\|system_prompt.*exec\|HeadlessSession" crates/orca-runtime/src/runtime_host.rs | head -20
```

If the prompt is inline, add one sentence at the end of the exec prompt block:
> If you cannot make progress because of something outside your control (read-only workspace, missing credentials, no network, an unsatisfiable requirement), call `report_blocker` with the reason. Do not tell the user you are done and do not simply stop.

If there is no per-mode exec prompt customisation, skip this step and rely on the tool description alone.

- [ ] **Step 5.3: Build**

```
env -u NODE_OPTIONS cargo build -p orca-runtime --locked
```
Expected: zero errors.

- [ ] **Step 5.4: Commit**

```
git add crates/orca-tools/src/registry.rs
git commit -m "docs(tools): add report_blocker description telling the model when to call it"
```

---

### Task 6: Contracts — JSONL, exit code, operation terminal

**Files:**
- Modify: `tests/exec_jsonl.rs` (add `blocked` status/exit-5 contract case)
- Modify: `tests/operation_terminal_contract.rs` (pin the new variant)
- Modify: `docs/harness-contract.md`

**Interfaces:**
- Consumes: `RunStatus::Blocked`, `OperationTerminal::Blocked` from Tasks 1-4
- Produces: pinned wire format for the new status and exit code

- [ ] **Step 6.1: Write a failing `exec_jsonl` contract test**

In `tests/exec_jsonl.rs`, add a test named `report_blocker_produces_blocked_status_and_exit_5`:

```rust
#[test]
fn report_blocker_produces_blocked_status_and_exit_5() {
    // Run orca exec with a mock that always calls report_blocker
    // (follow the pattern of existing exec_jsonl tests that inject a provider fixture)
    // Assert:
    // 1. The session.completed JSONL line has "status": "blocked"
    // 2. The exit code is 5
    // 3. The terminal field has a "blocked" key with the blocker's kind and summary
}
```

- [ ] **Step 6.2: Run the test to see it fail**

```
env -u NODE_OPTIONS cargo test --test exec_jsonl report_blocker --locked -- --test-threads=1
```
Expected: FAIL (the tool and status do not yet flow through end-to-end).

- [ ] **Step 6.3: Write a failing `operation_terminal_contract` test**

In `tests/operation_terminal_contract.rs`:

```rust
#[test]
fn blocked_terminal_serializes_with_status_and_blocker() {
    use orca_core::budget::{BudgetUsage, OperationTerminal};
    use orca_core::goal_runtime::{BlockerKind, BlockerSummary};
    let terminal = OperationTerminal::Blocked {
        blocker: BlockerSummary {
            kind: BlockerKind::EnvironmentContradiction,
            summary: "workspace is read-only".to_string(),
            fingerprint: "fp-test".to_string(),
            evidence: vec![],
        },
        usage: BudgetUsage::default(),
    };
    let json = serde_json::to_string(&terminal).expect("serializable");
    assert!(json.contains("\"blocked\""));
    assert!(json.contains("environment_contradiction"));
    assert_eq!(terminal.exit_code(), 5);
    assert_eq!(terminal.as_str(), "blocked");
}
```

- [ ] **Step 6.4: Run**

```
env -u NODE_OPTIONS cargo test --test operation_terminal_contract blocked --locked -- --test-threads=1
```
Expected: PASS (this tests only core types, which were done in Task 1).

- [ ] **Step 6.5: Make the `exec_jsonl` test pass**

This test requires an end-to-end exec run to produce the new status. If existing exec_jsonl tests use a fixed mock-provider transcript, add a transcript whose final tool call is `report_blocker` with `kind: environment_contradiction` and `summary: workspace is read-only`. Then the fixture infrastructure produces the JSONL stream and the test can assert on it.

- [ ] **Step 6.6: Update `docs/harness-contract.md`**

- Add `blocked` to the status list (~line 186-196), with the description: "The model declared a blocker through `report_blocker`. No work was done."
- Add exit code `5` to the table (~line 236-244): "5 — blocked (model declared `report_blocker`)."
- In the "turn end reasons" section (~line 418), add: "4. If the model calls `report_blocker` → status `blocked`, exit 5."
- Remove or update the wording in `site/src/docs/md/en/orca-exec.mdx:64-66` that says Orca publishes no numeric taxonomy; replace with: "Exit codes are documented in the developer reference."

- [ ] **Step 6.7: Run all exec_jsonl and terminal contract tests**

```
env -u NODE_OPTIONS cargo test --test exec_jsonl --locked -- --test-threads=1
env -u NODE_OPTIONS cargo test --test operation_terminal_contract --locked -- --test-threads=1
```
Expected: all tests PASS.

- [ ] **Step 6.8: Commit**

```
git add tests/exec_jsonl.rs tests/operation_terminal_contract.rs docs/harness-contract.md site/src/docs/md/en/orca-exec.mdx
git commit -m "test(contracts): pin blocked status and exit 5 in exec_jsonl and terminal contracts; update harness docs"
```

---

### Task 7: Verification gate and final checks

**Files:**
- Test: run the full gate from the brief
- Modify: any validator baselines that need updating

- [ ] **Step 7.1: Build the release binary for end-to-end probe**

```
env -u NODE_OPTIONS cargo build --bin orca --locked
```

- [ ] **Step 7.2: Run the full gate**

Run each of these; report every real exit code:

```
env -u NODE_OPTIONS cargo test -p orca-core blocked -- --test-threads=1
env -u NODE_OPTIONS cargo test -p orca-tools report_blocker -- --test-threads=1
env -u NODE_OPTIONS cargo test -p orca-runtime report_blocker -- --test-threads=1
env -u NODE_OPTIONS cargo test --test exec_jsonl --locked -- --test-threads=1
env -u NODE_OPTIONS cargo test --test operation_terminal_contract --locked -- --test-threads=1
env -u NODE_OPTIONS cargo test -p orca-tui runtime_surface_contract --lib --locked
env -u NODE_OPTIONS cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0
env -u NODE_OPTIONS cargo clippy --workspace --all-targets --locked
rustfmt --edition 2024 --check <every .rs file you changed>
env -u NODE_OPTIONS node scripts/test-validate-runtime-surface-contract.mjs; echo exit=$?
env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs; echo exit=$?
env -u NODE_OPTIONS node scripts/test-validate-windows-platform-boundaries.mjs; echo exit=$?
env -u NODE_OPTIONS node scripts/validate-windows-platform-boundaries.mjs; echo exit=$?
```

If a validator fails because of a change you made, update the baseline in the same commit and document it.

- [ ] **Step 7.3: D-7 re-run (if a test environment is available)**

Run `scripts/eval/capability/run.py --scenarios D-7-honest-blocker --profile standard` against the new binary. The verifier checks `terminal == "success" or exit_code == "0"` — if the model calls `report_blocker`, both conditions are false and the scenario passes. If not available, document it in the summary.

- [ ] **Step 7.4: Commit any validator baseline changes**

If baselines changed, commit them with the task they belong to; otherwise no commit here.

---

### Final summary

Your summary must cover:
- Every file changed, with `file:line`.
- The new test names.
- Every verification command with its real exit code.
- The final `git log --oneline origin/main..HEAD`.
- Any validator baselines that changed, and why.
- Anything you could not do as planned, and why.
