# Skill Catalog and On-Demand MCP Tools Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement parts 2 and 3 of the spec:
- the model sees a catalog of available skills, and a `$skill` body stays in the conversation instead of vanishing on the next prompt;
- project skills are found from the git root and win over user skills with the same id;
- an MCP server can be set to `tool_loading = "on-demand"`, offering one `load_tools` tool plus a catalog instead of every schema.

Parts 1 and 4 (spill, nested AGENTS.md) are another branch. Don't implement them here.

**Architecture:**
- **Skills.** The catalog is a new internal-context fragment, `skills`. A `$skill` mention appends the body once as a system message recorded in the transcript. Discovery changes in `crates/orca-tools/src/skills.rs`.
- **On-demand MCP.** The setting is configured per server in orca-core's MCP config types. orca-mcp's registry exposes a server's tools as deferred. The runtime registers `mcp__<server>__load_tools`, records loaded names per thread (so a resume re-registers them), and adds loaded tools to the next request's list. The catalog goes into the existing `mcp` context message.

**Tech Stack:** Rust: orca-tools (`skills.rs`, `registry.rs`), orca-core (`conversation.rs` fragments, `mcp_types.rs`), orca-mcp (registry), orca-runtime (`tool_invocation.rs`, `mentions.rs`, controller, session persistence).

**Spec:** `docs/superpowers/specs/2026-11-27-discoverability-and-large-output-design.md` (parts 2 and 3)

## Global Constraints

- **Skill catalog text.**
  - Each entry is `` `name`: description ``, sorted, with the description cut at 500 characters.
  - The closing line is: `When a task matches one of these skills, load it with read_skill before you act.`
  - The fragment is capped at 4,096 tokens. Past the cap the list ends with `({n} more skills; list_skills shows them all)`.
  - When there are no skills, the fragment is absent, so a session without skills sends the same bytes as today.
- **`$skill` bodies.**
  - A body is appended once per skill version as a system message, using the existing record type.
  - A body over 4,096 tokens ends with `[skill text cut at {n} tokens; read_skill {id} shows the rest]`.
- **On-demand MCP.**
  - `tool_loading` defaults to `"eager"`, so today's behaviour and today's bytes are unchanged unless a server opts in.
  - The catalog sits under `On-demand tools from MCP server '<server>':`, with one line per tool: `name — first line of its description`, cut at 160 characters.
  - `load_tools` takes `{"names": [string]}`. It validates every name before loading any, and an unknown name fails the call with the list of valid names.
  - Subagents start with nothing loaded.
  - A server tool literally named `load_tools` is skipped, with a warning.
- **Compatibility.** Anything recorded in sessions uses records Orca 0.5.24 reads. Prefer the existing system-message record, or optional fields.
- **Process.**
  - Commit after each task, and never push.
  - Don't override `TMPDIR` or `HOME`, and never touch `~/.orca`.
  - Use `env -u NODE_OPTIONS` for node, npm and workspace-wide cargo tests.

## Review Focus

- **A skill description containing backticks or text that looks like Orca instructions.** It is rendered as data, escaped, inside the catalog. Test `skill_descriptions_are_rendered_as_data`.
- **A project skill and a user skill with the same id.** The project one is listed and loaded. Test `a_project_skill_wins_over_a_user_skill`.
- **A session started in a sub-crate of a git repository.** It finds the root's `.orca/skills`. Test `skills_are_found_from_the_git_root`.
- **An on-demand server reconnecting with a different tool set.** Loaded names it still offers stay loaded; removed ones disappear from the next request. Test `reconnect_keeps_only_offered_loaded_tools`.
- **Loading tools past the 128-tool limit.** The loaded tools count under the same limit rule as other MCP tools, with the same warning. Test `loaded_tools_respect_the_tool_limit`.

---

### Task 1: Skill catalog, kept skill bodies, discovery fixes

- [ ] **Step 1: Write the failing tests:**
  - `the_skill_catalog_lists_each_skill_sorted`
  - `skill_descriptions_are_rendered_as_data`
  - `the_catalog_is_capped_with_a_count_of_the_rest`
  - `no_skills_means_no_catalog_and_unchanged_request_bytes`
  - `a_mentioned_skill_body_survives_the_next_prompt`
  - `a_long_skill_body_says_where_the_rest_is`
  - `a_project_skill_wins_over_a_user_skill`
  - `skills_are_found_from_the_git_root`
- [ ] **Step 2:** Run each test by name and confirm it fails.
- [ ] **Step 3: Implement.**
  - The catalog refreshes at turn start, like the memory fragment.
  - Update `list_skills`'s description only if needed. Prefer leaving schemas unchanged.
- [ ] **Step 4:** Run `cargo test -p orca-tools --lib skills -- --test-threads=1`, `cargo test -p orca-runtime --lib skill -- --test-threads=1` and `cargo test -p orca-core --lib -- --test-threads=1`, and confirm they pass.
- [ ] **Step 5:** Commit `feat(skills): the model sees which skills exist, and a mentioned skill stays in the conversation`.

### Task 2: On-demand MCP tools

- [ ] **Step 1: Write the failing tests:**
  - `an_eager_server_is_unchanged`, with a byte-identical tool list.
  - `an_on_demand_server_offers_only_load_tools_and_a_catalog`
  - `load_tools_validates_every_name_first`
  - `loaded_tools_appear_in_the_next_request`
  - `loaded_tools_survive_resume`
  - `subagents_start_with_nothing_loaded`
  - `reconnect_keeps_only_offered_loaded_tools`
  - `loaded_tools_respect_the_tool_limit`
  - `a_server_tool_named_load_tools_is_skipped_with_a_warning`
- [ ] **Step 2:** Run each test by name and confirm it fails.
- [ ] **Step 3: Implement.** The loaded-name record must be readable by Orca 0.5.24: use an optional field or an existing record.
- [ ] **Step 4:** Run `cargo test -p orca-mcp --lib -- --test-threads=1` and `cargo test -p orca-runtime --lib mcp -- --test-threads=1`, plus the named tests, and confirm they pass.
- [ ] **Step 5:** Commit `feat(mcp): a server can offer its tools on demand`.

### Task 3: Docs and checks

- [ ] Document the skill catalog, kept `$skill` bodies, the precedence change, and `tool_loading` in the site docs, English and Chinese: the skills and MCP pages.
- [ ] Run these and confirm each passes:
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --locked -j 4`
  - `env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs`
  - `env -u NODE_OPTIONS node scripts/validate-windows-platform-boundaries.mjs`
  - `env -u NODE_OPTIONS npm --prefix site ci && env -u NODE_OPTIONS npm --prefix site run build`
- [ ] Commit `docs: skill catalog and on-demand MCP tools`.
