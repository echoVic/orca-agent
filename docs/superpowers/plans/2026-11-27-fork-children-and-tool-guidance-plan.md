# Fork Children and Tool Guidance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement two items:
- multi-agent part 4: a `subagent` call with `context: "fork"` starts a child from the parent's completed history, with the parent's byte-identical system prompt, tool list and model, so the child's first request hits the cache;
- tool-quality part 3: the tool-description guidance sentences.

**Architecture:**
- **Fork children.** The `subagent` tool schema gains an optional `context` field, `"fresh"` by default. A fork child's first request is:
  - the parent's system prompt, tool list and completed-turn history, byte for byte, with balanced tool calls and the in-flight turn excluded;
  - then the task as a user message that carries the role and return guidance.

  The child uses the parent's model and reasoning effort. Role tool limits are enforced at execution time: a tool outside the role is refused with a clear message instead of being hidden. The child's result excludes the inherited range.
- **Tool guidance.** The guidance is a set of sentences added to the descriptions of `bash`, `edit` and `read_file`. The stray run of spaces in the bash description is removed.

**Tech Stack:** Rust: orca-runtime (`subagent*.rs`, `child_agent_*.rs`, `agent_common.rs`, `tool_invocation.rs`), orca-core (`subagent_types.rs`: `RoleContextPolicy::Fork`), orca-tools (`registry.rs` descriptions).

**Specs:**
- `docs/superpowers/specs/2026-11-27-multi-agent-design.md` (part 4)
- `docs/superpowers/specs/2026-11-27-tool-quality-design.md` (part 3)

## Global Constraints

- **Fork children.**
  - The default stays `"fresh"`, and fresh children behave exactly as today.
  - A fork child is refused cleanly, with a clear error, when the parent has no completed turn yet. It then starts as fresh.
  - A fork child is also refused cleanly when the parent's model cannot be used for the child.
- **Tool guidance sentences**, worded to fit each description:
  - bash: each call starts a fresh shell, so pass the directory argument instead of `cd`.
  - bash: a sandbox block is a policy decision. Do not retry it another way; report it or ask.
  - bash: guard variables in delete or overwrite paths with `${VAR:?}`.
  - edit: read the file first, unless you just created or edited it in this session.
  - read_file: use it instead of `cat`, `head`, `tail` or `sed` through bash.
- **Prompt and schema bytes.**
  - These change once for every session, and the release note must say so.
  - If the prompt-prefix ceiling is exceeded, first tighten the wording. If that is not enough, raise the ceiling and its Windows counterpart to the smallest round value that fits, and report the old and new values.
- **Compatibility.** Anything persisted for fork children must be readable by Orca 0.5.25.
- **Process.**
  - Commit after each task. Never push.
  - Do not override `TMPDIR` or `HOME`, and never touch `~/.orca`.
  - Use `env -u NODE_OPTIONS` for node, npm and workspace-wide cargo test runs.

## Review Focus

- **Prefix identity.** A fork child's first request equals the parent's last request up to the end of the inherited history, byte for byte, including `reasoning_content` and tool order. Test `a_fork_childs_first_request_shares_the_parents_prefix`.
- **The in-flight turn.** It is never inherited, so a parent with open tool calls cannot leak a half turn. Test `the_in_flight_turn_is_not_inherited`.
- **Role limits.** A fork child whose role forbids writes calls `write_file` and is refused at execution with a clear message, and nothing is written. Test `a_fork_childs_role_limits_are_enforced_at_execution`.
- **Model choice.** A fork with a different model requested is refused or ignored, and the cache-relevant settings stay the parent's. Test `a_fork_child_uses_the_parents_model`.
- **Bash description bytes.** The stray spaces are gone, and every guidance sentence is present. Test `the_bash_description_has_the_guidance_and_no_stray_spaces`.

---

### Task 1: Fork children

- [ ] **Step 1: Write the failing tests.** Write the five Review Focus tests, plus `fresh_children_are_unchanged` and `a_fork_before_any_completed_turn_starts_fresh_with_a_note`.
- [ ] **Step 2: Run them and confirm they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests.** Run `env -u NODE_OPTIONS cargo test -p orca-runtime --lib -- --test-threads=1` and the subagent contract tests, and confirm they pass.
- [ ] **Step 5: Commit** `feat(subagents): a fork child starts from the parent's history and reuses its cache`.

### Task 2: Tool guidance

- [ ] **Step 1: Write the failing tests.** Write `the_bash_description_has_the_guidance_and_no_stray_spaces`, `the_edit_description_says_to_read_first` and `the_read_file_description_prefers_it_over_shell`.
- [ ] **Step 2: Run them and confirm they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests.** Run the prompt and schema size tests and `cargo test -p orca-tools --lib -- --test-threads=1`, and confirm they pass.
- [ ] **Step 5: Commit** `feat(tools): tool descriptions say how to use the shell, edits and reads`.

### Task 3: Docs and checks

- [ ] **Docs.** Update the subagents docs and the tool docs in English and Chinese.
- [ ] **Workspace checks.** Run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -j 4`, the runtime-surface validator, the Windows platform-boundary validator and the site build.
- [ ] **Commit** `docs: fork children and tool guidance`.
