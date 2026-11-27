# Delegation: the user can steer while children run; fewer polling requests; workflow task lists that don't lose work; fork children that reuse the cache

Date: 2026-11-27. Size: project in four parts. It draws on DeepSeek's harness (dsh); the user asked for these learnings to be adopted. Parts 1 and 3 start by reproducing the problem, because the survey inferred them from code rather than observing them.

## 1. Steering reaches a parent that is waiting for its children

**Reported, not yet reproduced.**
- The parent's `task_wait` (at most 60 s) ends only when the task changes state, the deadline passes, or the turn is cancelled (`runtime_normal_tool.rs`). The runtime's own wait for children at the end of a turn has the same exit conditions (`runtime_turn_loop.rs`).
- Steering input is read only when a turn opens and before model requests (`runtime_steer.rs`, `provider_turn.rs`).
- So a Ctrl+Enter steer may sit unread until every child settles.

dsh ends any wait as soon as the waiter's inbox gets input: a peer message, a completion, or the user steering.

**Change.**
- First, write a test that steers during a `task_wait` on a long-running child, and during the end-of-turn child wait.
- If the steer is held, make both waits also end when steering input is pending. Then the next model step answers it, and the model may wait again.

## 2. The prompt tells the model it may end its turn while children run

**Today.** When the model ends its turn while children run, Orca already holds the turn and continues it with their `<task-notification>` results (`runtime_turn_loop.rs`). The Delegation section of the prompt (`agent_common.rs`) never says so, and models poll with `task_wait`. A 10-minute child polled at the 30 s default costs about 20 extra parent requests, each returning output tails.

dsh measured the sentence "When only waiting for their results remains, end your turn instead of sleeping or polling; the next result starts a new turn." Over 300 requests per arm it cut sleep calls from 29 to 0, and bash calls from 120 to 7.

**Change.** Add to the Delegation section, in substance: "When only waiting for subagent results remains, end your reply instead of polling; Orca keeps the turn open and continues it with each result as it arrives." Keep the prompt within its size tests.

## 3. Workflow task lists keep every update

**Reported, not yet reproduced.**
- The workflow host (`workflow/host.mjs`) loads `task-lists.json` once, and the Rust runner opens its own copy (`workflow/runner.rs`, `workflow/host.rs`). Each side rewrites the whole file from its own memory, so updates can be lost.
- Claim takes the first pending item. Complete accepts any id, with no status or claimant check.
- There is no release, so a task claimed by a child that crashed stays `running` forever.

**Change.**
- First, write tests that show a lost update between the two writers, completing a task claimed by another child, and a crashed claimant.
- For each that fails:
  - make one side the only writer, with the other asking it;
  - make `complete` require that the caller is the claimant and the task is `running`;
  - release a task back to `pending` when its claimant stops or fails.

## 4. Fork children reuse the parent's prompt cache

**Today.** `RoleContextPolicy` has only `Fresh`, and `Fork` is reserved (`crates/orca-core/src/subagent_types.rs`). Each child also gets its role contract appended to its system prompt and a narrowed tool list (`agent_common.rs`). A child therefore re-reads, at full price, what its parent already holds cached.

**dsh.** `subagent_fork` seeds the child with the parent's completed-turn history.
- The child keeps a byte-identical system prompt and ordered tool list, and enforces role limits at execution time.
- The task and the return guidance are appended after the seed.
- It offers no model choice, because a different model loses the cache.

**Change.**
- The `subagent` tool gains `context: "fresh" | "fork"`, default `"fresh"`.
- A fork child's first request is:
  - the parent's system prompt, tool list and completed-turn history (balanced tool calls, the in-flight turn excluded), byte for byte;
  - then the task as a user message, with the role and return guidance in it.
- It uses the parent's model and reasoning settings. A tool outside the child's role is refused at execution time with a clear message, instead of being hidden.
- Its result excludes the inherited range.
- Report, on a test with a recorded long parent, the cached share of the child's first request.

## Non-goals

- No teams or task boards.
- No child-to-parent messages mid-run.
- No delegation to other agent CLIs.

## Constraints

- Prompt changes stay within the prompt size tests.
- Contract docs are updated where a tool's schema changes (`subagent`'s new `context`).
