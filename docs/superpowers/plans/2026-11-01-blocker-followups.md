# Blocker Follow-ups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A blocked subagent reports `blocked` (not `failed`) to its parent, `report_blocker` can carry one question that `orca exec resume` answers, and the TUI/ACP present a Blocked terminal properly.

**Architecture:**
- **C.** Add a `Blocked` variant to the task and subagent status enums.
- **C.** Offer `report_blocker` to subagents whose root run is headless exec.
- **C.** Map a child's `RunStatus::Blocked` to the new variants instead of `Failed`.
- **D.** Add an optional `question` to `BlockerSummary`, `report_blocker`'s args, the exec text output and the docs.
- **E.** Change only how the TUI diagnostic and the ACP stop-reason mapping present an existing Blocked terminal.

**Tech Stack:** Rust (edition 2024), serde, the built-in mock provider (`--provider mock`) for end-to-end tests.

**Spec:** `docs/superpowers/specs/2026-11-01-blocker-followups-design.md` (read it first). It builds on `docs/superpowers/specs/2026-11-01-exec-blocker-declaration-design.md` (#156).

## Global Constraints

- **Repository:** worktree `/Users/qingyun/Documents/GitHub/.orca-fix/blocker-followups`, branch `feat/blocker-followups`, stacked on `fix/issue-156-exec-blocker-declaration` (head `e1c1722d` after the review fixes).
  - Work only there.
  - Never touch `/Users/qingyun/Documents/GitHub/blade-deepseek` or `~/.orca`.
- **Exec status and exit code:** exit code `5` and `status: "blocked"` stay as #156 defined them. No new run status and no new exit code.
- **Who gets `report_blocker`:**
  - It stays unavailable to interactive (TUI), ACP, server and goal-mode roots.
  - A subagent may get it only when its root run is headless exec (`AgentToolPolicyContext::is_headless()` at the root).
  - Goal mode never gets it, at any depth.
- **Effect of a child's `report_blocker`:**
  - It ends only that child, never the parent run.
  - It is a failed tool result for the parent (`ToolStatus::Failed`). Only the result text and the task status say `Blocked`.
- **Goal records:** `BlockerSummary` keeps serializing exactly as today when `question` is `None`.
- **No new dependencies.**
- **Off limits:** `crates/orca-tui/src/surface_actions.rs`, `hosted_workflow.rs`, `hosted_controller.rs`, `protocol.rs` and `slash_command_actions.rs` (validators pin them). Do not add lines above line 520 of `crates/orca-tui/src/surface_client.rs`.
- **Commands:**
  - Use `env -u NODE_OPTIONS` for every `cargo test` and `node` command.
  - Never run `--ignored` tests.
- **Commits:** one commit per task, with the subject given. Never push.
- **Validator baselines:** if `scripts/validate-runtime-surface-contract.mjs` or `validate-windows-platform-boundaries.mjs` fails because of your change, update its baseline in the same commit and say so.

## Review Focus

1. **Old persisted records:** a `TaskStatus`/`SurfaceTaskStatus` value written by an older binary (no `blocked`) must still deserialize. Pin it with a round-trip of every old variant name.
2. **A blocked child inside a still-running parent:** the parent's later success must not be overwritten by the child's `Blocked`, and `task_list` must show both correctly. Covered by the Task 3 end-to-end test, whose root ends successfully after the child blocks.
3. **Questions:** a question that is empty or whitespace only must be rejected, not stored as `""`. One over 1,000 chars must be rejected. Both must leave the turn continuing, the same as other invalid args. Pinned in Task 4.
4. **Text-mode output without saved history:** with no recorded history there is no session id, so the resume line must not print a bogus id. Pinned in Task 4's text-mode test with `--no-history`.
5. **ACP after the change:** an IDE must see why the turn ended. `terminal_to_stop_reason` must not lose the blocker summary. Pinned in Task 5.

---

### Task 1: Close #156's typed-surface contract gap

#156 added `OperationTerminal::Blocked` and `GenerationCompletionStatus::Blocked` to the typed surface. The surface contract spec and manifest still list only the old variants:

- `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.md:914` (`GenerationCompletionStatus`) and `:1212` (`OperationTerminal`);
- `closed_inventory.operation_terminal_variants` in the `.manifest.json` (~line 166).

No validator compares them to the code, so nothing failed.

**Files:**
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.md`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`

**Interfaces:**
- Produces: the contract lists `Blocked { blocker: BlockerSummary, usage: UsageTotals }` in `OperationTerminal`, and `Blocked { blocker: BlockerSummary }` in `GenerationCompletionStatus`. Check the field names against `crates/orca-runtime/src/runtime_surface/operation.rs:604` and `:869` and copy them exactly.

- [ ] **Step 1:** Add the two variants to the spec's closed enum definitions. Add a one-line note after `OperationTerminal`:
  > `Blocked` is produced only when the model ends a headless exec turn with `report_blocker`. Its legacy run status is `blocked`, exit code 5.
- [ ] **Step 2:** Add `"Blocked"` to `closed_inventory.operation_terminal_variants`. If `operation_terminal_mapping` lists the legacy dispositions, add a row `["Completed(Blocked)", "Blocked", "headless exec turn ended with report_blocker"]`, in the same shape as its neighbours.
- [ ] **Step 3:** Run:
  ```
  env -u NODE_OPTIONS node scripts/test-validate-runtime-surface-contract.mjs; echo exit=$?
  env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs; echo exit=$?
  ```
  Expected: both exit 0. If the manifest change makes a validator check fail, read the check and fix the manifest to satisfy it. Never weaken the validator.
- [ ] **Step 4:** Commit:
  `docs(surface): list the Blocked operation terminal in the typed-surface contract`

---

### Task 2: `Blocked` task and subagent statuses

**Files:**
- Modify: `crates/orca-core/src/task_types.rs` (`TaskStatus`, ~line 9)
- Modify: `crates/orca-runtime/src/runtime_lifecycle.rs` (`RuntimeTaskStatus`, ~line 31, and its `from_run_status`)
- Modify: `crates/orca-runtime/src/runtime_surface/projection.rs` (`SurfaceTaskStatus` ~1146, `SurfaceSubagentStatus` ~1414, `SurfaceSubagentTerminalStatus` ~1531)
- Modify: every match the compiler then reports, including:
  - `crates/orca-runtime/src/workflow/runner.rs:2692`;
  - `task_view.rs`, `runtime_special.rs` (`task_status_label`), `tasks.rs`, `terminal_service.rs` (`task_status_label`), `server.rs`, `runtime_host.rs`, `execution_scope.rs`;
  - `runtime_surface/{commit,reducer,host}.rs`;
  - the `runtime_actor/*` files;
  - in `crates/orca-tui/src/`: `ui.rs`, `surface_projection.rs`, `workflow_panel.rs`, `hosted_session_lifecycle.rs`.
- Modify: the typed-surface contract spec's `SurfaceTaskStatus` (~line 3333) and `SurfaceSubagentStatus` (~line 3673) definitions, with `"Blocked"` added wherever the manifest lists these enums' variants
- Test: inline tests in `task_types.rs` and `projection.rs`

**Interfaces:**
- Produces:
  - `TaskStatus::Blocked`, `RuntimeTaskStatus::Blocked`, `SurfaceTaskStatus::Blocked`, `SurfaceSubagentStatus::Blocked`, `SurfaceSubagentTerminalStatus::Blocked`.
  - Serde names: `"blocked"` where the enum uses `rename_all = "snake_case"`, otherwise whatever its existing variants use. Check each enum's attribute.
  - `TaskStatus::Blocked`: `is_active()` is false, `requires_attention()` is false, and it is terminal wherever the code lists terminal statuses.
  - Labels: `"blocked"` in every `task_status_label` and in `workflow/runner.rs`'s status string.
  - `RuntimeTaskStatus::from_run_status(RunStatus::Blocked { .. })` returns `Blocked`. #156 maps it to `Failed`; change it.
  - Do NOT yet change `runtime_subagent_call.rs:1493`/`:2278`, `subagent_async_worker.rs:654` or `child_agent_types.rs:702`. Task 3 owns them.

- [ ] **Step 1: Write failing tests**
  - `task_status_blocked_is_terminal_without_attention`:
    ```rust
    assert!(!TaskStatus::Blocked.is_active());
    assert!(!TaskStatus::Blocked.requires_attention());
    assert_eq!(serde_json::to_string(&TaskStatus::Blocked).unwrap(), "\"blocked\"");
    ```
  - `task_status_reads_every_pre_blocked_value`: deserialize each of the nine old wire names (`"queued"`, `"running"`, `"paused"`, `"stopping"`, `"stopped"`, `"completed"`, `"failed"`, `"approval_required"`, `"cancelled"`). `TaskStatus` has `#[serde(rename_all = "snake_case")]`. Assert each round-trips unchanged.
  - The same pair for `SurfaceTaskStatus` in `projection.rs`.
    - It has no `rename_all`, so its wire names are the variant names (`"Queued"` … `"Cancelled"`) and the new one is `"Blocked"`.
    - Assert `serde_json::to_string(&SurfaceTaskStatus::Blocked).unwrap() == "\"Blocked\""`.
    - Use the PascalCase names in the round-trip test.
- [ ] **Step 2:** Run `env -u NODE_OPTIONS cargo test -p orca-core task_status -- --test-threads=1`. Expected: compile error (no variant).
- [ ] **Step 3:** Add the variants. Run `env -u NODE_OPTIONS cargo check --workspace --all-targets --locked 2>&1 | grep -c '^error'`, then fix every reported match until the count is 0.
  - In the TUI, render `Blocked` with the same visual treatment as `Failed` and the label text `blocked`. Do not invent a new colour token.
- [ ] **Step 4:** Update the surface contract spec definitions and the manifest lists, as in Task 1. Run both surface validators; expected exit 0.
- [ ] **Step 5:** Run:
  ```
  env -u NODE_OPTIONS cargo test -p orca-core task_status -- --test-threads=1
  env -u NODE_OPTIONS cargo test -p orca-runtime --lib surface_task_status -- --test-threads=1
  env -u NODE_OPTIONS cargo test -p orca-tui runtime_surface_contract --lib --locked
  ```
  Expected: PASS.
- [ ] **Step 6:** Commit:
  `feat(tasks): add a Blocked task and subagent status`

---

### Task 3: A headless run's subagents can block, and the parent sees `Blocked`

**Files:**
- Modify: `crates/orca-runtime/src/tool_invocation.rs`
  - The schema filter added in `f00a3dcb` (`definition.name != "report_blocker" || (tool_policy.is_headless() && subagent_depth == 0 && !tool_policy.is_goal_mode())`): allow `subagent_depth > 0` when the policy is headless.
- Modify: the child policy construction
  - `crates/orca-runtime/src/child_agent_loop_setup.rs:153` builds `AgentToolPolicyContext::new(...)` without the headless flag.
  - Carry the root's headless flag into it through `ChildAgentRequest` (`child_agent_types.rs:210`): add `pub headless_root: bool`, set it where `ChildAgentRequest` is built (`runtime_subagent_call.rs:2062`, `subagent_async_worker.rs:489`, `child_agent_loop_setup.rs:310`), and call `.with_headless(...)`. `with_headless` exists in `tool_execution.rs`/`tool_invocation.rs` since `f00a3dcb`.
- Modify: role tool ceilings
  - `report_blocker` must survive a subagent role's `ToolSelection::Subagent(allowed_tools)` filter (`crates/orca-tools/src/schema.rs:141`).
  - Add it to the allowed list at filter time: when the policy is headless and the selection is `Subagent`, always keep `report_blocker`. Do not add it to every role's static `tools` list in `orca-core/src/subagent_types.rs`. A read-only explorer may also declare a blocker.
- Modify: the dispatch refusal in `crates/orca-runtime/src/tool_router.rs` (`if !headless || goal_mode || subagent_depth > 0`). A headless subagent call is now allowed.
- Modify: the parent-side mappings
  - `runtime_subagent_call.rs:1493`: `RunStatus::Blocked` gives `RuntimeTaskStatus::Blocked`.
  - `runtime_subagent_call.rs:2278` and `subagent_async_worker.rs:654`: `RunStatus::Blocked` gives `SurfaceSubagentTerminalStatus::Blocked`.
  - The sync result (`runtime_subagent_call.rs` ~3036, the `status =>` arm): for `RunStatus::Blocked { blocker }`, build the failed tool result text as follows. The `question:` line appears only when present, after Task 4 adds the field; until then, omit it.
    ```
    Subagent status: Blocked
    kind: <blocker.kind as snake_case>
    summary: <blocker.summary>
    ```
  - Leave `child_agent_types.rs:702` mapped to `Failed`. Its activity status describes a child tool call, not the child.
- Test: `crates/orca-runtime/src/tool_invocation.rs` (schema), `tool_router.rs` (dispatch), and an end-to-end test in `tests/exec_jsonl.rs`

**Interfaces:**
- Consumes: Task 2's statuses; #156's `AgentToolPolicyContext::{headless, is_headless, with_headless}` and `RuntimeSpecialToolDispatch::ReportBlocker`.
- Produces: `ChildAgentRequest.headless_root: bool`.

- [ ] **Step 1: Write failing tests**
  - Rewrite `provider_tool_schema_override_exposes_report_blocker_only_for_headless_root` as `provider_tool_schema_override_exposes_report_blocker_to_headless_trees`. Table of `(depth, policy, expected)`:

    | depth | policy | expected |
    |---|---|---|
    | 0 | headless | true |
    | 1 | headless | true |
    | 1 | headless with a read-only role's `ToolSelection::Subagent(["read_file","grep"])` | true |
    | 0 | unrestricted | false |
    | 1 | unrestricted | false |
    | 0 | goal_mode | false |

  - `report_blocker_dispatch_allows_headless_child_and_refuses_interactive_child`: depth 1 with headless gives `Some(RunStatus::Blocked{..})`; depth 1 non-headless gives continue-with-failed and the message `report_blocker is only available in headless orca exec runs`.
  - End-to-end `exec_subagent_report_blocker_reports_blocked_to_parent`:
    - Uses `--provider mock`.
    - `parse_mock_prompt` already has a `subagent <description>` form (`crates/orca-provider/src/lib.rs` ~1540). Read how the mock answers inside a child turn. It picks the child's reply from the child prompt; `subagent batch …` branches show the pattern.
    - Add the smallest mock form that makes the root call `subagent` with a prompt the mock answers with `report_blocker environment_contradiction child cannot write`, then has the root give a final message.
    - Assert all of:
      - the root exits `0` with `status: "success"`;
      - the root's `tool.call.completed` for the `subagent` call has status `failed` and output text containing `Subagent status: Blocked`, `kind: environment_contradiction` and `summary: child cannot write`;
      - no `session.completed` other than the root's is emitted.
- [ ] **Step 2:** Run the three tests; expected: FAIL.
- [ ] **Step 3:** Implement the changes listed under **Files**.
- [ ] **Step 4:** Run:
  ```
  env -u NODE_OPTIONS cargo test -p orca-runtime --lib report_blocker -- --test-threads=1
  env -u NODE_OPTIONS cargo test -p orca-runtime --lib provider_tool_schema_override -- --test-threads=1
  env -u NODE_OPTIONS cargo test --test exec_jsonl --locked -- --test-threads=1
  env -u NODE_OPTIONS cargo test -p orca-runtime --test subagent_delegation_contract --locked -- --test-threads=1
  env -u NODE_OPTIONS cargo test --test subagent_contract --locked -- --test-threads=1
  ```
  Expected: PASS.
- [ ] **Step 5:** Commit:
  `feat(subagents): a headless run's subagent can report a blocker to its parent`

---

### Task 4: An optional question on `report_blocker`

**Files:**
- Modify: `crates/orca-core/src/goal_runtime.rs:123`
  - `BlockerSummary` gains `#[serde(default, skip_serializing_if = "Option::is_none")] pub question: Option<String>`.
  - Fix every `BlockerSummary { .. }` literal the compiler reports. Goal code sets `question: None`.
- Modify: `crates/orca-tools/src/report_blocker.rs`
  - Args gain `#[serde(default)] question: Option<String>`.
  - `MAX_REPORT_BLOCKER_QUESTION_CHARS = 1_000`.
  - Reject a question that is empty after `trim()`, with `report_blocker question must not be empty`.
  - Reject one over the cap, with `report_blocker question must be at most 1000 characters`.
- Modify: `crates/orca-tools/src/registry.rs`, the `report_blocker` spec (~line 1465)
  - Schema property `question` (string). Its description, verbatim:
    > One concrete question for a person whose answer would let the work continue. Set it only when an answer would unblock the work; ask exactly one question.
- Modify: `crates/orca-provider/src/lib.rs`, the mock `report_blocker <kind> <summary...>` form (added in `d03766c5`)
  - Accept an optional ` ? <question...>` suffix: everything after the first ` ? ` is the question.
- Modify: `crates/orca-runtime/src/runtime_subagent_call.rs`: Task 3's result text gains `question: <question>` when present.
- Modify: the exec text-mode output (`crates/orca-runtime/src/controller.rs`, around the resume hint at `:1734` and its siblings at `:1787`/`:3016`)
  - For a blocked run, print these lines before the existing resume hint:
    ```
    Blocked: <summary>
    Question: <question>          (only when present)
    ```
  - The hint's form changes to `orca exec resume <session-id> "<your answer>"` when a question is present.
  - When no session id exists (history not recorded), print neither the resume line nor a placeholder. The hint at `controller.rs:1728` already checks `status != RunStatus::Success && let Some(session_id) = thread.session_id()`; keep that check.
  - The `Blocked:`/`Question:` lines print whether or not a session id exists.
  - Write them to the same `writer` the hint uses, which is stdout in text mode.
- Modify: `docs/harness-contract.md`: in the Run Status section, document `blocker.question` and the answer flow. Resuming a blocked JSONL run requires `--save-history`, because only then does `session.completed` carry a `session_id`.
- Modify: `site/src/docs/md/en/orca-exec.mdx` and `site/src/docs/md/zh/orca-exec.mdx`: one short paragraph each on answering a blocked run's question with `orca exec resume`.
- Test: `report_blocker.rs` unit tests, `tests/exec_jsonl.rs`, a goal-runtime serialization test

**Interfaces:**
- Consumes: Task 3's result text.
- Produces: `BlockerSummary.question: Option<String>`, and JSON `terminal.blocked.blocker.question` when set.

- [ ] **Step 1: Write failing tests**
  - In `report_blocker.rs`:
    - `parses_optional_question`: `{"kind":"user_decision","summary":"s","question":"Use port 8080?"}` gives `question == Some("Use port 8080?")`.
    - `rejects_empty_question`: `"question":"   "` gives `Err` containing `must not be empty`.
    - `rejects_long_question`: 1,001 chars gives `Err`.
  - In `goal_runtime.rs`: `blocker_summary_without_question_serializes_unchanged`. Serialize a `BlockerSummary` with `question: None` and assert the JSON object has exactly the keys `kind`, `summary`, `fingerprint` and `evidence`.
  - In `tests/exec_jsonl.rs`:
    - `exec_report_blocker_carries_question`: mock prompt `report_blocker user_decision need a port ? Use port 8080?`. Assert exit `5`, and `payload.terminal.blocked.blocker.question == "Use port 8080?"`.
    - `exec_blocked_text_mode_prints_question_without_resume_line_when_history_is_off`: `--output-format text --no-history` with the same prompt. Assert stdout contains `Blocked: need a port` and `Question: Use port 8080?`, and does not contain `orca exec resume`.
- [ ] **Step 2:** Run them; expected: FAIL.
- [ ] **Step 3:** Implement the changes listed under **Files**.
- [ ] **Step 4:** Run:
  ```
  env -u NODE_OPTIONS cargo test -p orca-tools report_blocker -- --test-threads=1
  env -u NODE_OPTIONS cargo test -p orca-core blocker_summary -- --test-threads=1
  env -u NODE_OPTIONS cargo test --test exec_jsonl --locked -- --test-threads=1
  env -u NODE_OPTIONS cargo test --test public_docs_contract --locked
  env -u NODE_OPTIONS cargo test -p orca-runtime --lib goal -- --test-threads=1
  ```
  Expected: PASS. The last command guards goal mode against the new field.
- [ ] **Step 5:** Commit:
  `feat(exec): let report_blocker carry one question for a person`

---

### Task 5: TUI and ACP present a Blocked terminal

**Files:**
- Modify: `crates/orca-tui/src/diagnostics.rs:323`
  - The `OperationTerminal::Blocked` arm keeps code `operation.blocked` and title `Task blocked`.
  - Detail: `blocker.summary`, followed by `\nQuestion: <question>` when present.
  - Action, verbatim:
    - with a question: `Answer the question, then continue.`
    - without: `Resolve what is missing, then continue.`
- Modify: `crates/orca-runtime/src/acp/agent.rs`
  - `terminal_to_stop_reason` (~line 2759): `OperationTerminal::Blocked { .. }` gives `Ok(StopReason::EndTurn)` instead of `Err("blocked")`.
  - Where the ACP turn completes and calls it (~lines 2505 and 2706), when the terminal is Blocked, send one `agent_message_chunk` notification before the stop. Its text is `Blocked: <summary>`, plus `\nQuestion: <question>` when present. Use the same notification path the ACP agent already uses for assistant text; read `emit_surface_event` / `emit_assistant_patch` to find it.
- Test: `crates/orca-tui/src/diagnostics.rs` tests; `acp/agent.rs` tests (next to `terminal_mapping_preserves_only_exact_standard_stop_reasons`)

**Interfaces:**
- Consumes: Task 4's `BlockerSummary.question`.

- [ ] **Step 1: Write failing tests**
  - `blocked_terminal_diagnostic_offers_answer_step_with_a_question`: the action equals `Answer the question, then continue.` and the detail contains `Question: Use port 8080?`.
  - `blocked_terminal_diagnostic_offers_resolve_step_without_a_question`: the action equals `Resolve what is missing, then continue.`
  - `blocked_terminal_ends_the_acp_turn_normally`: `terminal_to_stop_reason(&OperationTerminal::Blocked{..}) == Ok(StopReason::EndTurn)`.
  - If the ACP notification path has a test harness, add `blocked_terminal_sends_its_summary_before_end_turn`. If it has none, say so in the summary instead of building one.
- [ ] **Step 2:** Run them; expected: FAIL.
- [ ] **Step 3:** Implement the changes listed under **Files**.
- [ ] **Step 4:** Run:
  ```
  env -u NODE_OPTIONS cargo test -p orca-tui --lib diagnostics
  env -u NODE_OPTIONS cargo test -p orca-runtime --lib terminal_mapping -- --test-threads=1
  env -u NODE_OPTIONS cargo test -p orca-runtime --lib blocked_terminal -- --test-threads=1
  env -u NODE_OPTIONS cargo test -p orca-runtime --test acp_agent --locked -- --test-threads=1
  ```
  Expected: PASS.
- [ ] **Step 5:** Commit:
  `feat(tui,acp): present a blocked terminal with its question and next step`

---

### Task 6: Full verification

- [ ] **Step 1:** Run each command. Report every real exit code; never judge through `tail`/`head`.
  ```
  env -u NODE_OPTIONS cargo build --workspace --locked
  env -u NODE_OPTIONS cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0
  env -u NODE_OPTIONS cargo test -p orca-runtime --lib -- --test-threads=1
  env -u NODE_OPTIONS cargo test --test exec_jsonl --locked -- --test-threads=1
  env -u NODE_OPTIONS cargo test --test tool_contract --locked -- --test-threads=1
  env -u NODE_OPTIONS cargo test --test verification_contract --locked -- --test-threads=1
  env -u NODE_OPTIONS cargo test --test subagent_contract --locked -- --test-threads=1
  env -u NODE_OPTIONS cargo test --test public_docs_contract --locked
  env -u NODE_OPTIONS cargo test -p orca-runtime --test runtime_host --locked -- --test-threads=1
  env -u NODE_OPTIONS cargo test -p orca-runtime --test runtime_surface_interaction --locked -- --test-threads=1
  env -u NODE_OPTIONS cargo test -p orca-runtime --test operation_terminal_contract --locked -- --test-threads=1
  env -u NODE_OPTIONS cargo clippy --workspace --all-targets --locked
  rustfmt --edition 2024 --check <every .rs file changed on this branch since e1c1722d>
  env -u NODE_OPTIONS node scripts/test-validate-runtime-surface-contract.mjs; echo exit=$?
  env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs; echo exit=$?
  env -u NODE_OPTIONS node scripts/test-validate-windows-platform-boundaries.mjs; echo exit=$?
  env -u NODE_OPTIONS node scripts/validate-windows-platform-boundaries.mjs; echo exit=$?
  ```
- [ ] **Step 2:** A failure in `orca-runtime --lib` or `runtime_surface_interaction` may be a known flake when the tests run in parallel. These commands already run serially. If one still fails, rerun that one test alone. If it passes alone, report it as flaky with both results. If it fails alone, check it on `e1c1722d` (`git stash` or a scratch worktree) before calling it pre-existing.
- [ ] **Step 3:** No commit unless a validator baseline changed. A baseline change goes in the task commit it belongs to; amend only your own commits on this branch.

## Final summary

End with:
- every change, with `file:line`;
- the new test names;
- every verification command with its real exit code;
- the final `git log --oneline e1c1722d..HEAD`;
- anything you did differently from the plan, and why.
