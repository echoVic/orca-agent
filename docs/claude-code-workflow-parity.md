# Claude Code Dynamic Workflows Parity Plan

**Date**: 2026-06-26
**Project**: Orca (blade-deepseek)
**Goal**: Replicate the Claude Code Dynamic Workflows product loop in Orca before extending it with Orca-specific evidence and verifier features.

**Reference snapshot**: Claude Code public docs checked on 2026-06-26:

- [Dynamic workflows](https://code.claude.com/docs/en/workflows)
- [Skills](https://code.claude.com/docs/en/skills)
- [Subagents](https://code.claude.com/docs/en/sub-agents)
- [Agent view](https://code.claude.com/docs/en/agent-view)
- [Agent teams](https://code.claude.com/docs/en/agent-teams)
- [Worktrees](https://code.claude.com/docs/en/worktrees)
- [Hooks](https://code.claude.com/docs/en/hooks)
- [Slash commands in the SDK](https://code.claude.com/docs/en/agent-sdk/slash-commands)

---

## Executive Summary

The target is not a workflow benchmark harness. The target is a product loop:

```text
user intent
-> Orca authors a JavaScript workflow
-> Orca previews phases, script, risks, and expected scale
-> user approves, edits, saves, or cancels
-> workflow runs in the background
-> /workflows exposes live runs, phase detail, agent detail, transcripts, cost, and controls
-> completed runs can be saved as reusable project or user workflows
-> saved workflows appear as slash commands and can be rerun with args
```

The current Orca runtime already has a meaningful base: JavaScript workflow execution, `phase()`, `agent()`, `parallel()`, background launches, workflow run directories, phase records, agent transcripts, evidence bundles, resume/cache, `/workflows`, `/agents`, workflow IPC tools, and named workflow script resolution. The missing product layer is the authoring, approval, reusable-command, and management experience around that runtime.

P0 should therefore focus on making workflow feel like a first-class agentic procedure, not on adding more stress-test assertions.

---

## Strategic Interpretation

The thing to copy is not just a JavaScript runner. Claude Code's workflow value is an ecosystem position:

```text
skills/custom commands define reusable procedures
subagents define specialized workers
agent teams define supervised peer sessions
worktrees isolate long-running source mutations
hooks enforce deterministic lifecycle policy
dynamic workflows move orchestration into runtime-executed code
/workflows and agent view make the background work inspectable and controllable
```

Dynamic workflow is the highest-scale orchestration primitive in that stack. It is used when the plan itself should live outside the main conversation, intermediate results should stay out of the parent context window, and the user wants a script that can be read, approved, saved, rerun, paused, resumed, and inspected.

For Orca, the equivalent should be:

```text
ORCA.md / project instructions
-> skills or saved workflows for repeatable procedures
-> subagents for delegated work
-> workflow JS for high-scale orchestration
-> workflow run manager for observability and control
-> evidence/verifier for DeepSeek-native reliability
```

So the parity target is not "can Orca launch 16 agents". The target is "can Orca convert a high-level user request into a reviewable, reusable, inspectable, background orchestration asset with reliable state-derived reporting".

---

## What Claude Code Workflow Means

Claude Code Dynamic Workflows are valuable because they move orchestration out of the main conversational context and into a reusable script artifact.

Key product properties to replicate:

1. **Generated orchestration**: the assistant can decide that a task should become a workflow and author the JavaScript workflow script.
2. **Preview before execution**: the user sees the phases and raw script before the workflow starts.
3. **Background execution**: many agents can run while the main conversation stays small.
4. **Observable management**: the user can inspect runs, phases, agents, prompts, tool activity, results, tokens, and status.
5. **Reusable asset**: a good workflow can be saved and later invoked like a command.
6. **Scale by design**: intermediate results live in workflow state/script variables rather than flooding the parent session.

Orca should match that loop first. Evidence-first reporting, deterministic verifiers, and benchmark contracts are important, but they are Orca-specific reliability enhancements layered on top.

### Claude Code Capability Matrix

| Claude Code capability | Product meaning | Orca parity target | Current Orca posture |
| --- | --- | --- | --- |
| Ask for `workflow` / `ultracode` | User can opt into workflow in natural language | Detect workflow intent and produce a draft preview, not immediate freeform execution | Keyword detection and workflow runtime exist; authoring loop is incomplete |
| `/effort ultracode` | Agent can choose workflow for substantive tasks | Add an Orca mode such as `workflow_auto` or `effort=ultracode` later | Not present |
| Generated JS workflow | Orchestration is code, not conversation state | Persist script, expose raw source, launch through runtime | Runtime supports JS workflow scripts |
| Approval prompt | User sees phases and can approve/cancel/view script | Draft preview with run/edit/save/cancel | Draft primitives are being added; full UX still incomplete |
| Background run | Session stays responsive while agents run | Keep task registry/background workflow launch | Present |
| `/workflows` progress view | Inspect phases, agents, token totals, elapsed time, results | Run manager with list/detail/phase/agent drill-down | Basic panel exists; needs controls and richer detail |
| Controls: pause/resume/stop/restart/save | User can manage long runs | Add deterministic run controls with cached completed results | Stop/rerun partially present; pause/restart need work |
| Save workflow as command | Successful orchestration becomes reusable asset | Save to `.orca/workflows` or `~/.orca/workflows`; invoke as slash command | Named workflow resolution exists; command UX needs first-class treatment |
| Args to saved workflow | Reuse without editing source | Parse and validate args schema; pass as JS global `args` | Args support exists or is being added; needs UI/contract polish |
| Agent caps | Bound cost and runaway scripts | Keep max concurrent agents and max agents per run | Present |
| No direct FS/shell in script | Script coordinates; agents mutate/read | Enforce JS host as orchestration-only | Mostly aligned |
| Child agents inherit mode/tool policy | Workflows do not bypass safety | Define workflow child approval/tool/MCP policy clearly | Partially present; needs policy doc and tests |
| Intermediate results live in script variables | Parent context stays small | Avoid dumping full JSON into chat; store artifacts in run state | Needs stricter reporting rules |

---

## Orca Design Boundary

Orca is DeepSeek-native, so parity should be behavioral, not textual or architectural cloning.

### Copy the User Contract

- Natural-language workflow opt-in.
- Reviewable generated plan before launch.
- Background execution with visible progress.
- Many subagents coordinated by runtime-owned script state.
- Saved reusable workflows with command-like invocation.
- Inspectable run history, phase state, agent detail, and final output.
- Clear caps for concurrency, total agents, and cost blast radius.

### Do Not Copy Blindly

- Do not require Claude-specific permission modes or model names.
- Do not assume Claude's exact `.claude/` directory semantics; use `.orca/` equivalents.
- Do not make workflow reliability depend on model self-reporting.
- Do not force all workflow reuse through skills if `.orca/workflows` is a cleaner primitive.
- Do not treat hooks as P0 unless a workflow lifecycle policy needs deterministic enforcement.

### Orca Differentiation

- Use `evidence.json`, mailbox state, task-list state, and transcripts as first-class truth sources.
- Let verifier output overrule optimistic final text.
- Add benchmark/evidence contracts after the product loop works.
- Preserve DeepSeek model-routing and budget semantics.

---

## Current Orca Baseline

### Already Present

- Workflow tool and input/output types:
  - `crates/orca-core/src/workflow_types.rs`
  - `crates/orca-core/src/tool_types.rs`
  - `crates/orca-runtime/src/controller.rs`
- JavaScript workflow runtime:
  - `crates/orca-runtime/src/workflow/host.mjs`
  - `crates/orca-runtime/src/workflow/host.rs`
  - `crates/orca-runtime/src/workflow/runner.rs`
- Script resolution and keyword detection:
  - `crates/orca-runtime/src/workflow/script.rs`
- Durable run state, evidence, cache, transcripts, and worker records:
  - `crates/orca-runtime/src/workflow/state.rs`
- Workflow events:
  - `crates/orca-core/src/event_schema.rs`
  - `crates/orca-core/src/event_sink.rs`
- TUI workflow surfaces:
  - `/workflows`
  - `/agents`
  - `crates/orca-tui/src/commands/mod.rs`
  - `crates/orca-tui/src/app.rs`
  - `crates/orca-tui/src/ui.rs`
- Workflow shared state tools:
  - `workflow_send_message`
  - `workflow_read_messages`
  - `workflow_clear_messages`
  - `workflow_create_task_list`
  - `workflow_claim_task`
  - `workflow_complete_task`
  - `workflow_list_tasks`

### Current Product Loop

The runtime primitives for draft persistence, preview data, edit/save/cancel, named workflows, pause/resume, and evidence-backed reporting are present. The user-facing loop now connects them as follows:

- Natural-language orchestration requests are routed through `WorkflowDraft` before launch by the runtime system prompt.
- `WorkflowDraft` results render as a compact TUI preview with phases, estimated agents, concurrency, mutation risk, and expandable raw JavaScript; `WorkflowDraftAction` remains the durable run/edit/save/cancel authority.
- `/tasks` is the unified workspace for workflow runs, ordinary subagents, shell tasks, and monitors. Conversation keeps a bounded activity dock, while `/workflows` stays focused on workflow phases, agents, transcripts, failures, and stop control.
- Child stop, resume, retry, and follow-up use typed, revision-fenced task control. Continuations expose explicit ownership, attempt, checkpoint, resumable, and indeterminate state instead of presenting speculative buttons.
- Keep final reports grounded in durable workflow state and evidence rather than agent-authored claims.

Remaining extensions are saved-workflow catalog browsing, argument editing before rerun, and pause/resume integration with actor-owned workflow ingress receipts. They should extend the existing run manager rather than introduce another task surface.

### Completion Evidence

As of this implementation, the working tree covers the parity loop described in this document:

- `WorkflowDraft` and `WorkflowDraftAction` are model-visible tool surfaces in `crates/orca-tools/src/registry.rs`.
- Draft persistence, edit, save, cancel, and clone-from-run behavior live in `crates/orca-runtime/src/workflow/draft.rs`.
- Runtime launch from `draftId`, resume cache reuse, pause/resume, restart failed, restart phase, concise status lines, and child tool-event capture live in `crates/orca-runtime/src/workflow/runner.rs`.
- TUI workflow preview, task workspace, saved workflow launch, and typed task controls live in `crates/orca-tui/src/ui.rs`, `crates/orca-tui/src/agent_workspace.rs`, `crates/orca-tui/src/surface_client.rs`, and `crates/orca-tui/src/hosted_workflow.rs`.
- Saved workflow slash invocation and collision-safe aliases live in `crates/orca-tui/src/commands/mod.rs`.
- CLI workflow history/control commands live in `src/cli.rs`: `list`, `show`, `source`, `stop`, `pause`, `resume`, `clone`, `restart-failed`, and `restart-phase`.
- Evidence contracts, deterministic verification statuses, required/failed tool checks, MCP failure checks, mutation-policy checks, and concurrency-threshold checks live in `crates/orca-runtime/src/workflow/verifier.rs`.

Local validation uses `cargo fmt --check`, focused workflow/runtime contract tests, and the full `orca-tui` test target. The complete workspace suite remains a CI gate because provider/ACP integration tests may require external services or can block on unavailable supervisors.

---

## Product Principles

1. **Workflow is an artifact**
   - A workflow script is a durable, reviewable asset.
   - It can be generated, approved, run, inspected, saved, and rerun.

2. **Runtime owns orchestration**
   - The main assistant should not manually coordinate dozens of agents in conversation.
   - The workflow script owns loops, fan-out, branches, and intermediate results.

3. **User remains in control**
   - Generated scripts need preview and approval.
   - Risky workflows must be editable or cancellable before execution.

4. **Saved workflows become commands**
   - Project workflows live under `.orca/workflows/`.
   - User workflows live under `~/.orca/workflows/`.
   - Saved workflows should be invocable from the slash menu.

5. **Reliability comes after parity, not instead of parity**
   - P0 is parity with the Claude Code product loop.
   - P1/P2 can add stronger evidence, verifier, and benchmark semantics.

---

## Target User Journeys

### Journey A: Generate and Run a One-Off Workflow

```text
User: 用 workflow 审计认证模块，分 8 个 agent 并行看代码，再汇总风险

Orca:
1. Detects workflow intent.
2. Authors a workflow script using phase(), parallel(), and agent().
3. Shows a preview:
   - name
   - description
   - phases
   - expected agent count
   - max concurrency
   - file mutation risk
   - raw script
4. User chooses Run.
5. Workflow runs in background.
6. Main session receives concise progress and completion notifications.
7. User opens /workflows to inspect details.
```

### Journey B: Edit Before Running

```text
User sees generated workflow preview.
User chooses Edit.
Orca opens the generated script in the composer or an edit panel.
User changes target files, agent count, or phase names.
User approves.
Workflow launches from the edited script.
```

### Journey C: Save Successful Workflow as Command

```text
Workflow completes.
Orca asks whether to save it.
User saves as project workflow "security-audit".
Orca writes .orca/workflows/security-audit.js.
Slash menu now offers /workflow:security-audit or /security-audit.
```

### Journey D: Rerun with Arguments

```text
User: /workflow:security-audit target=crates/orca-runtime/src/auth

Orca resolves the named workflow.
Args are validated.
Workflow launches with args passed to the JS runtime.
```

---

## P0 Scope

P0 delivers a usable Claude Code workflow parity loop.

### P0.1 Workflow Authoring

Add an authoring path where Orca can turn a user task into a workflow script.

Trigger conditions:

- User explicitly says `workflow`, `workflows`, `多 agent`, `并行 agent`, `ultracode`, or asks to run a large multi-phase audit/implementation.
- Existing `contains_workflow_keyword()` remains useful, but it should route to authoring preview when there is no explicit script/name/path.

Expected generated script shape:

```js
export const meta = {
  name: "repo-auth-audit",
  description: "Parallel audit of authentication-related code",
  phases: ["scan", "review", "report"]
};

const scanResults = await phase("scan", async () => parallel([
  agent("Inspect auth middleware and report concrete risks.", { team: "scanner" }),
  agent("Inspect session handling and report concrete risks.", { team: "scanner" }),
  agent("Inspect permission checks and report concrete risks.", { team: "scanner" })
]));

const reviewResults = await phase("review", async () => parallel([
  agent(`Review these findings and identify duplicates:\n${JSON.stringify(scanResults)}`, { team: "reviewer" })
]));

export default await phase("report", async () =>
  agent(`Write a concise final report from:\n${JSON.stringify({ scanResults, reviewResults })}`, {
    team: "reporter"
  })
);
```

Implementation options:

- Minimal P0: the main assistant authors the script as tool input and the runtime stores it as pending workflow draft.
- Stronger P0: add a dedicated `WorkflowDraft` tool or controller path that returns a preview object before launch.

Recommendation: start with a dedicated draft path. It creates cleaner UX and avoids overloading `Workflow` launch semantics.

### P0.2 Preview and Approval

Add a pending workflow draft state with these fields:

```rust
pub struct WorkflowDraft {
    pub draft_id: String,
    pub session_id: String,
    pub cwd: String,
    pub name: String,
    pub description: String,
    pub phases: Vec<String>,
    pub script: String,
    pub estimated_agent_count: Option<u32>,
    pub max_configured_concurrent_agents: u32,
    pub source_mutation_risk: WorkflowSourceMutationRisk,
    pub created_at_ms: i64,
}
```

Preview actions:

- `run`: launches the draft.
- `edit`: lets the user edit the JavaScript before launch.
- `save`: writes a reusable workflow without running.
- `cancel`: discards the draft.

TUI preview should show:

- title/name
- description
- phase list
- expected agent count when statically inferable
- configured concurrency cap
- source mutation risk
- raw script with scroll support

### P0.3 Background Run Notifications

Keep existing background execution, but make notifications product-quality:

- On launch: show workflow name, run id, phase count, transcript dir.
- During run: show phase changes and agent count updates without dumping full JSON.
- On completion: show status, failed agents, cost, duration, and how to inspect details.

Final notifications must never print a full 10KB JSON summary.

### P0.4 `/workflows` Management Surface

Upgrade `/workflows` from a task list view into a run manager.

Required views:

- Recent runs list
  - status
  - name
  - run id short hash
  - current phase
  - agent counts
  - elapsed time
  - estimated cost
- Run detail
  - metadata
  - raw script path
  - launch input
  - phase table
  - agent table
  - failures
  - final summary
- Agent detail
  - prompt
  - team
  - status
  - transcript path
  - output
  - usage/cost

Controls:

- stop
- rerun
- resume from run
- save as reusable workflow

Pause/resume can remain P1 if stop/rerun/resume-from-run are solid in P0.

### P0.5 Reusable Workflow Commands

Treat saved workflows as command-like assets.

Locations:

- Project: `.orca/workflows/<name>.js`
- User: `~/.orca/workflows/<name>.js`

Resolution order:

1. Project workflow
2. User workflow
3. Built-in workflow, if added later

Command surface:

- `/workflow:<name>`
- optional alias `/<name>` only when there is no collision with built-in slash commands

Arguments:

Workflow scripts can export an args schema:

```js
export const args = {
  target: { type: "string", required: true },
  maxAgents: { type: "number", required: false, default: 8 }
};
```

Runtime passes parsed args to the workflow script as the existing `args` input.

### P0.6 Report from State, Not Freeform Confidence

The final report can be written by an agent, but the run status line must be deterministic:

```text
Workflow completed
Agents: 18 completed, 1 failed, 0 running
Phases: 4 completed, 1 failed with fallback
Max observed concurrency: 6 / 16
Inspect: /workflows -> <run id>
```

This avoids repeating the previous failure mode where a freeform assistant said "fully resolved" while the evidence showed unresolved caveats.

---

## P1 Scope

P1 deepens parity after the core loop works.

### P1.1 Script Editing UX

Support a first-class edit loop:

- edit generated JavaScript before launch
- re-parse metadata after edit
- re-render preview
- launch edited script

### P1.2 Run Controls

Add richer controls:

- pause workflow
- resume workflow
- restart failed agent
- restart phase
- clone run as draft

### P1.3 Workflow History and Search

Add workflow-specific history:

- search by workflow name
- search by run id
- filter by failed/completed/running
- show saved workflow source
- show associated session id

### P1.4 Better Saved Workflow Metadata

Support optional metadata:

```js
export const meta = {
  name: "security-audit",
  description: "Parallel security audit",
  phases: ["scan", "review", "report"],
  tags: ["audit", "security"],
  version: "1"
};
```

### P1.5 Workflow Authoring Prompt Library

Add internal authoring instructions so generated workflows:

- keep phase count small and meaningful
- avoid dumping huge intermediate results into final summaries
- use teams consistently
- include explicit final report phase
- prefer worktree isolation for source-mutating implementation workflows

---

## P2 Scope: Orca Differentiation

P2 adds reliability features beyond Claude Code parity.

### P2.1 Evidence Contracts

Add structured workflow contracts:

- required tool calls
- expected tool failures
- expected MCP failures
- expected file mutation policy
- expected concurrency threshold

### P2.2 Deterministic Verifier

Add a deterministic verifier that reads `evidence.json`, `task-lists.json`, `mailbox.json`, and transcripts.

Verifier outputs:

- `proven`
- `not_proven`
- `failed`
- `completed_with_failures`

### P2.3 Concurrency Barrier

Add a barrier primitive for true max-concurrency testing.

Example:

```js
await phase("concurrency", () => parallel(
  Array.from({ length: 16 }, (_, index) =>
    agent(`hold-${index}`, { barrier: "concurrency-16", minHoldMs: 3000 })
  )
));
```

### P2.4 Tool and MCP Event Evidence

Record per-agent tool/MCP calls in `WorkflowEvidenceAgent`.

This makes claims like "tool failure tested" verifiable without reading agent self-reports.

---

## Architecture

### New Concepts

#### Workflow Draft

A workflow draft is a generated script that has not been launched yet.

Responsibilities:

- store generated script
- store parsed metadata
- store preview estimates
- allow run/edit/save/cancel

Suggested storage:

```text
.orca/workflow-sessions/<session-id>/workflow-drafts/<draft-id>/
  draft.json
  script.js
```

#### Workflow Command Registry

A registry that loads saved workflows and exposes them to slash completion.

Responsibilities:

- discover `.orca/workflows/*.js`
- discover `~/.orca/workflows/*.js`
- parse `meta`
- parse optional `args`
- handle command-name collisions

#### Workflow Run Manager

An upgraded TUI/controller layer for listing and operating on workflow runs.

Responsibilities:

- load runs from `WorkflowStateStore`
- map run state into TUI summaries
- expose controls
- avoid printing large JSON blobs

---

## File-Level Implementation Map

### Core Types

- Modify `crates/orca-core/src/workflow_types.rs`
  - Add `WorkflowDraft`
  - Add `WorkflowDraftOutput`
  - Add `WorkflowSourceMutationRisk`
  - Add reusable workflow metadata/args types if shared across runtime and TUI

- Modify `crates/orca-core/src/task_types.rs`
  - Add draft/run summary fields needed by `/workflows`

### Runtime

- Modify `crates/orca-runtime/src/workflow/script.rs`
  - Keep current script/name/path resolution.
  - Add reusable workflow discovery helpers if they fit the existing module.
  - Add metadata extraction for optional `args`.

- Create `crates/orca-runtime/src/workflow/draft.rs`
  - Own draft persistence.
  - Write `draft.json` and `script.js`.
  - Load, update, delete, and launch drafts.

- Modify `crates/orca-runtime/src/workflow/runner.rs`
  - Allow launching from a draft.
  - Ensure final summaries are concise.
  - Preserve existing run/evidence behavior.

- Modify `crates/orca-runtime/src/controller.rs`
  - Add draft creation and draft action handling.
  - Continue using `WorkflowRunner` for actual execution.
  - Route named workflow command invocations to existing workflow launch input.

### Tools

- Modify `crates/orca-tools/src/registry.rs`
  - Add or update model-visible workflow tooling for draft creation if needed.
  - Keep the existing `Workflow` tool for actual launch.

Potential tool split:

```text
WorkflowDraft: create previewable draft
Workflow: launch inline/script/name/path workflow
WorkflowDraftAction: run/edit/save/cancel draft
```

If the tool surface should stay smaller, `Workflow` can accept `mode: "draft" | "run"`, but separate tools are easier to reason about.

### TUI

- Modify `crates/orca-tui/src/commands/mod.rs`
  - Keep `/workflows`.
  - Add dynamic saved workflow slash entries or route `/workflow:<name>`.

- Modify `crates/orca-tui/src/app.rs`
  - Add draft preview state.
  - Add run/edit/save/cancel actions.
  - Add `/workflows` navigation state for list/detail/agent detail.

- Modify `crates/orca-tui/src/ui.rs`
  - Render workflow draft preview.
  - Render improved workflow run manager.
  - Render concise completion notifications.

- Modify `crates/orca-tui/src/types.rs`
  - Add view models for draft preview, workflow run detail, and agent detail if current structs are too narrow.

### Tests

- Modify `tests/workflow_runtime_contract.rs`
  - Draft persistence and launch from draft.
  - Named workflow command resolution.
  - Args parsing and validation.
  - Concise summary regression.

- Modify or add TUI command tests
  - Slash parsing for `/workflow:<name>`.
  - Saved workflow collision behavior.

- Add fixtures under `tests/fixtures/workflows/`
  - simple generated workflow
  - workflow with args
  - workflow with phases and multiple agents

---

## P0 Implementation Milestones

### Milestone 1: Draft Persistence and Preview Data

Deliverable:

- Runtime can create a draft from generated script.
- Draft can be loaded from disk.
- Draft preview includes metadata, phases, script, estimated agent count, concurrency cap, and mutation risk.

Validation:

```bash
cargo test --test workflow_runtime_contract workflow_draft -- --nocapture
```

### Milestone 2: Launch Drafts

Deliverable:

- Draft can be launched without copying script by hand.
- Run stores original draft id in launch input or metadata.
- Existing run/evidence/transcript behavior remains unchanged.

Validation:

```bash
cargo test --test workflow_runtime_contract workflow_draft_launch -- --nocapture
```

### Milestone 3: TUI Preview Approval

Deliverable:

- Generated workflow draft renders before launch.
- User can run, save, cancel.
- Edit can initially be implemented as "copy script into composer" if a full editor is too large for P0.

Validation:

```bash
cargo test -p orca-tui workflow -- --nocapture
```

### Milestone 4: Saved Workflow Commands

Deliverable:

- `.orca/workflows/*.js` and `~/.orca/workflows/*.js` are discovered.
- `/workflow:<name>` launches a saved workflow.
- Name collisions with built-in slash commands are handled deterministically.

Validation:

```bash
cargo test --test workflow_runtime_contract named_workflow -- --nocapture
cargo test -p orca-tui commands -- --nocapture
```

### Milestone 5: `/workflows` Run Manager Upgrade

Deliverable:

- `/workflows` shows recent runs and active runs.
- Run detail shows phases, agents, failures, script path, launch input, cost, and duration.
- Agent detail shows prompt, output, transcript path, and usage.

Validation:

```bash
cargo test -p orca-tui workflow -- --nocapture
cargo test -p orca-runtime workflow -- --nocapture
```

### Milestone 6: Concise Completion Reporting

Deliverable:

- Workflow completion notification never dumps full JSON.
- Completion message includes status, agent counts, failures, observed concurrency, and inspection path.

Validation:

```bash
cargo test -p orca-runtime workflow -- --nocapture
```

---

## Acceptance Criteria

P0 is complete when all of these are true:

1. A user can ask Orca to use workflow for a complex task and receive a generated workflow preview before execution.
2. The preview shows phases and raw JavaScript.
3. The user can run or cancel the draft.
4. The launched workflow runs in the background and records state under `.orca/workflow-sessions/`.
5. `/workflows` can show the run, phases, agents, transcripts, usage, failures, and final status without reading raw JSON files.
6. The user can save a workflow under `.orca/workflows/`.
7. Saved workflows can be invoked by slash command.
8. Completion output is concise and evidence-derived at the status line.
9. Existing workflow runtime contract tests continue to pass.
10. Existing named workflow behavior remains backward compatible.

---

## Non-Goals for P0

- Full deterministic verifier.
- Tool/MCP call evidence contracts.
- Barrier-based concurrency benchmark.
- Full in-terminal JavaScript editor.
- Cloud workflow sharing.
- Plugin marketplace for workflows.
- A web UI.

These are valid later projects, but they should not block Claude Code workflow parity.

---

## Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Orca workflow behave like the Claude Code workflow product loop: generate, preview, edit/save/cancel, run in background, inspect, control, save, and rerun.

**Architecture:** Keep JavaScript workflow execution in `orca-runtime`; keep model-visible tool contracts in `orca-tools`; expose durable state through `TaskRegistry` and TUI view models; make `.orca/workflows` and `~/.orca/workflows` the reusable workflow command registry. Evidence/verifier features layer on top of this loop and must not replace the user-facing workflow lifecycle.

**Tech Stack:** Rust workspace, Node.js workflow host, JSONL events, Ratatui TUI, `.orca/workflow-sessions`, `.orca/workflows`, `serde_json`, existing `WorkflowRunner` / `WorkflowStateStore` / `WorkflowDraftStore`.

### Global Constraints

- Preserve current workflow DSL: `phase`, `agent`, `parallel`, `pipeline`, workflow IPC helpers, `export const meta`, `export const args`, and default export.
- Use `.orca/workflows/<name>.js` for project workflows and `~/.orca/workflows/<name>.js` for user workflows.
- Do not let saved workflow aliases override built-in slash commands; `/workflow:<name>` is always valid.
- Workflow child agents must not bypass approval, tool, MCP, budget, or model-routing policy.
- User-facing completion text must be concise and derived from workflow state; full JSON remains an artifact, not chat output.
- Workflow tests that execute the JS host need bundled Node on `PATH`.

### Task 1: Finish Workflow Draft Contract

**Files:**

- Modify: `crates/orca-core/src/workflow_types.rs`
- Modify: `crates/orca-tools/src/registry.rs`
- Modify: `crates/orca-runtime/src/controller.rs`
- Modify: `crates/orca-runtime/src/workflow/draft.rs`
- Test: `tests/workflow_tool_contract.rs`
- Test: `tests/workflow_runtime_contract.rs`

**Interfaces:**

- Produces: `WorkflowDraft` JSON with `draftId`, `name`, `description`, `phases`, `script`, `estimatedAgentCount`, `maxConfiguredConcurrentAgents`, `sourceMutationRisk`, `scriptPath`.
- Produces: `WorkflowDraftAction` actions `run`, `edit`, `save`, `cancel`.
- Consumes: workflow script text and optional save scope/name.

- [ ] Write failing tests for draft create/edit/save/cancel and metadata reparse after edit.
- [ ] Verify tests fail before implementation when the action or output field is missing.
- [ ] Complete the minimal core/runtime/tool implementation.
- [ ] Run:

```bash
TMPDIR=/tmp PATH="/Users/bytedance/.cache/codex-runtimes/codex-primary-runtime/dependencies/node/bin:$PATH" cargo test --test workflow_tool_contract workflow_draft -- --nocapture
TMPDIR=/tmp PATH="/Users/bytedance/.cache/codex-runtimes/codex-primary-runtime/dependencies/node/bin:$PATH" cargo test --test workflow_runtime_contract workflow_draft -- --nocapture
```

Expected: all draft contract tests pass.

### Task 2: Launch Drafts Through the Normal Runner

**Files:**

- Modify: `crates/orca-runtime/src/workflow/runner.rs`
- Modify: `crates/orca-runtime/src/workflow/state.rs`
- Modify: `crates/orca-runtime/src/controller.rs`
- Test: `tests/workflow_runtime_contract.rs`

**Interfaces:**

- Consumes: `WorkflowInput { draft_id: Some(String), script/name/script_path: None }`.
- Produces: normal workflow run state with the original `draftId`, persisted script path, run id, task id, evidence path, and concise launch output.

- [ ] Write failing tests that launching `draftId` rejects combinations with `script`, `name`, or `scriptPath`.
- [ ] Write failing tests that launching a draft records the draft id in run artifacts.
- [ ] Implement draft resolution inside the existing `WorkflowRunner` launch path.
- [ ] Run:

```bash
TMPDIR=/tmp PATH="/Users/bytedance/.cache/codex-runtimes/codex-primary-runtime/dependencies/node/bin:$PATH" cargo test --test workflow_runtime_contract workflow_draft_launch -- --nocapture
```

Expected: draft launch behaves like a normal workflow run and does not fork a parallel runtime path.

### Task 3: Make Saved Workflows Command-Like

**Files:**

- Modify: `crates/orca-runtime/src/workflow/script.rs`
- Modify: `crates/orca-tui/src/commands/mod.rs`
- Modify: `crates/orca-tui/src/bridge.rs`
- Test: `tests/workflow_script_contract.rs`
- Test: TUI command tests under `crates/orca-tui`

**Interfaces:**

- Consumes: `.orca/workflows/<name>.js`, `~/.orca/workflows/<name>.js`, `/workflow:<name>`, optional `/<name>` alias, and args in either JSON object or `key=value` form.
- Produces: resolved script path, parsed `meta`, parsed `args` schema, launch request.

- [ ] Write failing tests for project-over-user resolution order.
- [ ] Write failing tests for built-in slash command collision handling.
- [ ] Write failing tests for `export const args` defaults and validation.
- [ ] Implement registry/command resolution without hardcoding one workflow name.
- [ ] Run:

```bash
TMPDIR=/tmp PATH="/Users/bytedance/.cache/codex-runtimes/codex-primary-runtime/dependencies/node/bin:$PATH" cargo test --test workflow_script_contract workflow -- --nocapture
cargo test -p orca-tui saved_workflow -- --nocapture
```

Expected: saved workflows are discoverable and invocable as command-like assets.

### Task 4: Upgrade `/workflows` Into a Run Manager

**Files:**

- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/types.rs` if current state structs are too narrow.
- Modify: `crates/orca-tui/src/bridge.rs`
- Test: TUI workflow panel tests under `crates/orca-tui`

**Interfaces:**

- Consumes: workflow task summaries, phase summaries, agent summaries, run artifacts, status line, failure count, cost/duration fields.
- Produces: list view, run detail view, agent detail view, and controls.

- [ ] Write failing tests for rendering phase rows and agent rows for selected workflow.
- [ ] Add list/detail/agent-detail state if the current panel cannot represent drill-down.
- [ ] Add controls for stop, rerun, resume-from-run, and save-as-workflow.
- [ ] Keep pause/resume/restart-agent/restart-phase as P1 unless the run state is already sufficient.
- [ ] Run:

```bash
cargo test -p orca-tui workflow -- --nocapture
```

Expected: `/workflows` can explain what happened without asking the user to inspect raw JSON.

### Task 5: Make Final Reporting Evidence-Derived

**Files:**

- Modify: `crates/orca-runtime/src/workflow/runner.rs`
- Modify: `crates/orca-runtime/src/workflow/report.rs`
- Modify: `crates/orca-runtime/src/workflow/state.rs`
- Test: `tests/workflow_runtime_contract.rs`

**Interfaces:**

- Consumes: `WorkflowRunState`, phase summaries, agent status counts, max observed concurrency, configured concurrency cap.
- Produces: concise status line:

```text
Workflow completed
Agents: 18 completed, 1 failed, 0 running
Phases: 4 completed, 1 failed, 0 with fallback
Max observed concurrency: 6 / 16
Inspect: /workflows -> <run id>
```

- [ ] Write failing tests that completion notifications do not contain a full JSON dump.
- [ ] Write failing tests that failed agents appear in the deterministic status line.
- [ ] Implement status-line generation from run state.
- [ ] Run:

```bash
TMPDIR=/tmp PATH="/Users/bytedance/.cache/codex-runtimes/codex-primary-runtime/dependencies/node/bin:$PATH" cargo test --test workflow_runtime_contract workflow_status -- --nocapture
```

Expected: the assistant cannot claim success when run state records unresolved failures.

### Task 6: Add P1 Run Controls

**Files:**

- Modify: `crates/orca-runtime/src/workflow/state.rs`
- Modify: `crates/orca-runtime/src/workflow/runner.rs`
- Modify: `crates/orca-tui/src/bridge.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Test: `tests/workflow_runtime_contract.rs`

**Interfaces:**

- Produces: pause workflow, resume workflow, restart failed agent, restart phase, clone run as draft.
- Consumes: completed result cache, phase records, agent records, original script path, original launch args.

- [ ] Write failing tests for stop/pause/resume state transitions.
- [ ] Write failing tests for clone-run-as-draft preserving script and args.
- [ ] Add restart failed agent only after agent call inputs are durably stored.
- [ ] Add restart phase only after phase boundary and dependencies are unambiguous.
- [ ] Run focused runtime tests, then TUI workflow tests.

Expected: long workflows can be managed without rerunning successful work unnecessarily.

### Task 7: Add P2 Evidence Contracts

**Files:**

- Modify: `crates/orca-core/src/workflow_types.rs`
- Modify: `crates/orca-runtime/src/workflow/state.rs`
- Modify: `crates/orca-runtime/src/workflow/verifier.rs`
- Modify: `crates/orca-runtime/src/workflow/runner.rs`
- Test: `tests/workflow_runtime_contract.rs`
- Test: `tests/workflow_script_contract.rs`

**Interfaces:**

- Produces: structured evidence for required tool calls, expected tool failures, expected MCP failures, mutation policy, concurrency threshold, barrier/min-hold runs.
- Consumes: per-agent tool/MCP events and workflow-level contract declarations.

- [ ] Write failing verifier tests where agent text claims success but evidence is missing.
- [ ] Record child-agent tool/MCP events rather than inferring from error strings.
- [ ] Implement verifier outputs `proven`, `not_proven`, `failed`, and `completed_with_failures`.
- [ ] Run workflow runtime and script contract tests.

Expected: workflow benchmark results become auditable, not just plausible.

### Final Validation Gate

Before commit/push, run:

```bash
cargo fmt --check
TMPDIR=/tmp PATH="/Users/bytedance/.cache/codex-runtimes/codex-primary-runtime/dependencies/node/bin:$PATH" cargo test --workspace -- --nocapture
git status --short -uall
```

The implementation is not complete until each acceptance criterion in this document has an explicit passing test or a documented manual TUI verification note.

---

## Risks and Decisions

### Risk: Overloading the Existing `Workflow` Tool

If `Workflow` handles inline launch, named launch, draft creation, and draft actions, the schema will become ambiguous.

Decision:

- Prefer a small draft-specific API or tool path.
- Keep `Workflow` as the launcher.

### Risk: Slash Command Collision

Saved workflow names can collide with built-in commands like `/model` or `/resume`.

Decision:

- `/workflow:<name>` is always valid.
- `/<name>` alias is allowed only when there is no built-in collision.

### Risk: Workflow Authoring Hallucinates Unsupported DSL

Generated JavaScript may use non-existent helpers.

Decision:

- Authoring instructions must include the exact DSL surface:
  - `phase`
  - `agent`
  - `parallel`
  - `pipeline`
  - mailbox helpers
  - task-list helpers
- Draft preview must parse metadata before approval.
- Launch must fail fast with a readable script error.

### Risk: Huge Final Summaries

Workflow scripts can export a large object.

Decision:

- Runtime should derive concise summaries from run state.
- Full exported object can remain in run artifacts but should not be printed as the user-facing completion summary.

### Risk: Source Mutation During Preview or Test Runs

Generated workflow may edit source files.

Decision:

- Preview should classify mutation risk from prompt/script heuristics.
- Source-mutating workflow drafts should recommend worktree isolation.
- Hard source-mutation contracts belong to P2.

---

## Recommended Next Step

Start with Milestone 1 and Milestone 2 in one branch:

```text
P0A: workflow draft persistence + launch from draft
```

This creates the missing product primitive without forcing the whole TUI manager redesign into the first patch. Once drafts exist, TUI preview, saved commands, and `/workflows` upgrades become smaller follow-up tasks.
