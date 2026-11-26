# Small Reliability Batch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Four independent fixes:
- Orca's own tools are never cut by the 128-tool limit.
- A stopped script keeps what it logged.
- MCP servers' instructions reach the model.
- Edits work on CRLF files.

Also correct the goal-mode doc.

**Architecture:** Each task touches its own crate area and carries its own tests. No task depends on another task's code.

**Tech Stack:** Rust workspace (orca-provider, orca-runtime, orca-mcp, orca-core, orca-tools); the script engine is QuickJS through `rquickjs` with a JS prelude.

**Spec:** `docs/superpowers/specs/2026-11-27-small-reliability-batch-design.md`

## Global Constraints

- The repository is public. Describe each change by what it does for a user.
- No tool's model-facing description or schema changes.
- If a change moves a count pinned in `docs/superpowers/specs/2026-07-28-native-windows-platform-foundation.manifest.json`, or a runtime-surface contract pin, update it in the same commit and run its validator.
- Commit after each task; never push. No `TMPDIR`/`HOME` override; never touch `~/.orca`.
- Run node, npm and workspace-wide cargo test commands with `env -u NODE_OPTIONS`.

## Review Focus

- **Built-in tools alone over 128.** This cannot happen today, but the limit must still return at most 128 tools by truncating the built-in names in name order. Task 1 test `limit_truncates_builtins_only_when_they_alone_exceed_the_limit`.
- **A script that logs megabytes before the clock stops it.** The kept lines must be bounded like a completed result's output. Task 2 test `kept_log_lines_are_bounded`.
- **A blank or non-string `instructions`.** Whitespace only, a number or null is ignored, and that server contributes nothing. Task 3 test `initialize_ignores_blank_or_non_string_instructions`.
- **A CRLF file whose `old_text` appears twice once line endings are normalized.** This is "matched multiple locations", never a silent first match. Task 4 test `uniqueness_is_counted_on_normalized_text`.
- **A file with a lone `\r` (classic Mac line ending) or no line ending at all.** It is edited byte-exactly, as today. Task 4 test `a_file_without_crlf_majority_is_edited_byte_exactly`.

---

### Task 1: Orca's own tools are kept within the 128-tool limit

**Files:**
- Modify: `crates/orca-provider/src/tool_schema.rs`. Add the limit function and a public `DEEPSEEK_MAX_TOOLS: usize = 128`.
- Modify: `crates/orca-provider/src/deepseek_http.rs`. `cap_tools_for_deepseek` uses the same priority and `tracing::warn!`, and the private constant is removed.
- Modify: `crates/orca-runtime/src/tool_invocation.rs`. `provider_config_for_agent_loop` applies the limit after its filtering.
- Modify: `crates/orca-runtime/src/mcp_startup.rs`. Add the new warning.

**Interfaces:**
- Produces: `pub fn limit_tool_definitions(definitions: &mut Vec<ProviderToolDefinition>, max: usize) -> Vec<String>` in `orca-provider::tool_schema`.
  - It keeps every definition whose name does not start with `mcp__`.
  - It fills the remaining places with `mcp__` definitions in name order.
  - It returns the removed names in name order.
  - When the non-MCP definitions alone exceed `max`, it keeps the first `max` of them by name.
  - Order of the kept definitions is unchanged.

- [ ] **Step 1: Write the failing tests.**
  - In `tool_schema.rs`:
    - `limit_keeps_every_builtin_tool_and_drops_mcp_tools_past_the_limit`: 30 definitions named `b00`…`b29` plus 110 named `mcp__srv__t000`…`mcp__srv__t109`. After `limit_tool_definitions(&mut defs, 128)`:
      - all 30 `b*` remain;
      - exactly 98 `mcp__` remain, `t000`…`t097`;
      - the returned list is `mcp__srv__t098`…`mcp__srv__t109`.
    - `limit_leaves_a_list_under_the_limit_unchanged`: 5 definitions, `max` 128. The list is unchanged and nothing is returned.
    - `limit_truncates_builtins_only_when_they_alone_exceed_the_limit`: 130 `b*` names and 3 `mcp__` names, `max` 128. The 128 lowest `b*` names remain, and every `mcp__` name is returned.
  - In `deepseek_http.rs` tests:
    - `deepseek_tool_cap_keeps_orca_tools_before_mcp_tools`: 20 values named `write_file`, `read_file`, `task_wait`, … plus 120 `mcp__x__*` values. `cap_tools_for_deepseek` returns 128 values, every non-`mcp__` name among them, still sorted by name.
  - In `mcp_startup.rs` tests, using whatever fake-registry helper the module's existing tests use:
    - `startup_warns_when_mcp_tools_do_not_fit`: a registry whose servers offer more tools than `DEEPSEEK_MAX_TOOLS - <default registry tool count>`. `mcp_startup_warnings` contains one line that starts with `MCP servers offer ` and contains `left out of requests` and `disabled_tools`.
    - `startup_does_not_warn_when_mcp_tools_fit`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p orca-provider --lib tool_schema && cargo test -p orca-provider --lib deepseek_tool_cap && cargo test -p orca-runtime --lib mcp_startup`
Expected: FAIL. The functions are missing, and the cap drops built-in names.

- [ ] **Step 3: Implement.**
  - `limit_tool_definitions` as specified.
  - The provider guard applies the same rule to `Value`s by `function.name`.
  - `provider_config_for_agent_loop` calls `limit_tool_definitions(tools, DEEPSEEK_MAX_TOOLS)` after its existing `retain` filters.
  - The warning text is exactly: `MCP servers offer {offered} tools, but only {available} fit beside Orca's own {builtin} tools within DeepSeek's limit of 128; {left_out} are left out of requests, starting with {first_three_names}. Use disabled_tools or enabled_tools in an MCP server's configuration to choose which are sent.`
    - `builtin` is the number of tools in `orca_tools::default_tool_registry()`.
    - `available` is `128 - builtin`, saturating at zero.

- [ ] **Step 4: Run the tests to verify they pass.** Use the Step 2 commands. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-provider/src/tool_schema.rs crates/orca-provider/src/deepseek_http.rs crates/orca-runtime/src/tool_invocation.rs crates/orca-runtime/src/mcp_startup.rs
git commit -m "fix(provider): the tool limit keeps Orca's own tools and says which MCP tools are left out"
```

### Task 2: A script stopped by its limits keeps what it logged

**Files:**
- Modify: `crates/orca-runtime/src/script/prelude.js`. `log` also passes each line to a native function.
- Modify: `crates/orca-runtime/src/script/engine.rs`. Keep the lines, bounded, and append them to the wall-clock and memory failures.
- Test: `tests/script_engine_contract.rs`, following its existing tests.

**Interfaces:**
- Produces:
  - The native function's JS name is `__orcaLogLine(text: string)`. It is installed beside the existing call bridge and hidden from the script the way that bridge is.
  - The failure text is `<existing message>\n\nLogged before it was stopped:\n<lines joined by \n>`. It is used only when at least one line was logged.

- [ ] **Step 1: Write the failing tests:**
  - `a_script_stopped_by_the_clock_keeps_its_log`: the wall clock is set to its minimum, and the script is `log("first"); log("second"); while (true) {}`.
    - The outcome is a failure.
    - Its text contains `the script ran longer than`.
    - Its text contains `Logged before it was stopped:\nfirst\nsecond`.
  - `a_script_over_its_memory_keeps_its_log`: memory is set to the minimum (8 MiB). The script logs `before`, then grows an array until the limit.
    - The failure contains `the script used more than 8 MiB`.
    - It contains `Logged before it was stopped:\nbefore`.
  - `kept_log_lines_are_bounded`: the script logs a 1 MiB string 20 times, then loops until the clock. The failure text is no longer than the limit a completed result's output is held to, plus the fixed message.
  - `a_cancelled_script_still_ends_as_interrupted`: the existing cancel behaviour is unchanged.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `env -u NODE_OPTIONS cargo test --test script_engine_contract -- --test-threads=1`
Expected: the new tests FAIL. The lines are missing from the failures.

- [ ] **Step 3: Implement the native log line and the kept-lines buffer.** Bound the buffer with the same byte limit the engine applies to a completed result's text, keeping the first lines that fit.

- [ ] **Step 4: Run the tests to verify they pass.** Use the same command, plus `cargo test -p orca-runtime --lib script`. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src/script/prelude.js crates/orca-runtime/src/script/engine.rs tests/script_engine_contract.rs
git commit -m "fix(script): a script stopped by its time or memory limit keeps what it logged"
```

### Task 3: MCP servers' instructions reach the model; the goal-mode doc matches the request order

**Files:**
- Modify: `crates/orca-mcp/src/client.rs`. Read `instructions` from the initialize result.
- Modify: `crates/orca-mcp/src/registry.rs`. Keep and expose them per server.
- Modify: `crates/orca-core/src/conversation.rs`. Add the new fragment id, its cap and its setter.
- Modify: the runtime call site that refreshes the memory fragment at turn start. Find the callers of `refresh_project_memory_context`; set the `mcp` fragment there too.
- Modify: `docs/goal-mode.md`, the "Internal Context" section.

**Interfaces:**
- Produces:
  - `pub fn server_instructions(&self) -> Vec<(String, String)>` on `McpRegistry`. It returns (server name, non-blank instructions) for connected servers, in configuration order.
  - `pub const MCP_CONTEXT_FRAGMENT_ID: &str = "mcp"` and `pub const MCP_CONTEXT_MAX_TOKENS: usize = 4_096`.
  - `Conversation::replace_mcp_context(&mut self, content: Option<String>)`.
  - The rendered fragment is each server as `Instructions from MCP server '<name>':\n<text>`, joined by blank lines.

- [ ] **Step 1: Write the failing tests:**
  - orca-mcp:
    - `initialize_keeps_server_instructions`: a result with `"instructions": "Use search before fetch."` keeps that text.
    - `initialize_ignores_blank_or_non_string_instructions`: `"   "`, `3`, `null` or a missing field keep nothing.
  - orca-core: `mcp_context_renders_each_server_under_its_heading`.
    - Two servers are rendered under their headings, in order.
    - Text past 4,096 tokens is cut the way the other capped fragments are.
    - `None` removes the fragment.
  - orca-provider: `internal_context_includes_mcp_instructions_last`. A conversation with the `mcp` fragment renders it inside the final system message, the same message `render_internal_context` builds today.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p orca-mcp --lib instructions && cargo test -p orca-core --lib mcp_context && cargo test -p orca-provider --lib internal_context_includes_mcp`
Expected: FAIL.

- [ ] **Step 3: Implement.** Set the fragment at the memory fragment's call sites, from `mcp_registry.server_instructions()`.
- [ ] **Step 4: Correct `docs/goal-mode.md`.**
  - The adapter sends the internal-context fragments as one synthetic system message after the conversation history, the last message of the request.
  - A changed fragment therefore costs only that message, not the cached history prefix.
- [ ] **Step 5: Run the tests to verify they pass.** Use the Step 2 commands, then `env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs && env -u NODE_OPTIONS node scripts/test-validate-runtime-surface-contract.mjs`. Expected: PASS.
- [ ] **Step 6: Commit**

```bash
git add crates/orca-mcp/src crates/orca-core/src/conversation.rs crates/orca-runtime/src crates/orca-provider/src docs/goal-mode.md
git commit -m "feat(mcp): a server's instructions reach the model; the goal-mode doc gives the real request order"
```

### Task 4: Edits keep working on CRLF files

**Files:**
- Modify: `crates/orca-tools/src/edit.rs`, `crates/orca-tools/src/write_file.rs`, and their tests.

**Interfaces:**
- Produces: in `edit.rs`, `fn crlf_majority(text: &str) -> bool`. It returns true when `\r\n` pairs outnumber `\n` bytes not preceded by `\r`. `write_file.rs` reuses it as `crate::edit::crlf_majority`, so expose it at `pub(crate)`.

- [ ] **Step 1: Write the failing tests:**
  - `a_multi_line_edit_matches_a_crlf_file`: file `a\r\nb\r\nc\r\n`, old `a\nb`, new `x\ny`. The file becomes `x\r\ny\r\nc\r\n`.
  - `old_text_with_crlf_still_matches_a_crlf_file`: old `a\r\nb` gives the same result.
  - `uniqueness_is_counted_on_normalized_text`: file `a\r\nb\r\na\r\nb\r\n`, old `a\nb`. The error is `edit old text matched multiple locations`.
  - `an_lf_file_is_edited_as_before`.
  - `a_file_without_crlf_majority_is_edited_byte_exactly`:
    - a file with lone `\r` separators;
    - a file of one line with no ending;
    - for both, the result is exactly what byte matching gives today.
  - `writing_lf_text_over_a_crlf_file_keeps_crlf`: existing `one\r\ntwo\r\n`, content `one\nthree\n`. The file becomes `one\r\nthree\r\n`.
  - `content_with_crlf_is_written_as_given`.
  - `a_new_file_is_written_as_given`.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p orca-tools --lib edit:: && cargo test -p orca-tools --lib write_file::`
Expected: the CRLF tests FAIL.

- [ ] **Step 3: Implement.**
  - Normalize, match and convert back, as the spec says.
  - The change preview shows the text as written.
  - The rewind pre-image capture is untouched.
- [ ] **Step 4: Run the tests to verify they pass.** Use the Step 2 commands, then `cargo test -p orca-tools --lib -- --test-threads=1`. Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add crates/orca-tools/src/edit.rs crates/orca-tools/src/write_file.rs
git commit -m "fix(tools): edit and write_file keep a CRLF file's line endings"
```

### Final checks (after Task 4)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -j 4
env -u NODE_OPTIONS node scripts/validate-windows-platform-boundaries.mjs
env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs
env -u NODE_OPTIONS cargo test -p orca-provider -p orca-mcp -p orca-core -p orca-tools --lib -- --test-threads=1
```

Expected: every command exits 0, and clippy prints no warnings.
