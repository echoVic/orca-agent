# File-Defined Subagents

Custom agents provide reusable instructions and a narrower tool policy for the
`subagent` tool. Built-ins (`general`, `explorer`, `code_reviewer`, `test_writer`,
`debugger`, and `documenter`) retain their existing behavior.

## Definition

Create a UTF-8 Markdown file in the user or trusted project agent directory:

```markdown
---
name: audit-api
description: Review API changes for compatibility and correctness
extends: code_reviewer
tools:
  - read_file
  - grep
  - git_status
model: deepseek-v4-pro
---
Review the requested API changes. Report concrete defects with file references.
Do not modify files.
```

Only `name` and `description` are required in the YAML frontmatter. The Markdown
body is required. YAML is parsed with `serde_yaml_ng`, including quoted strings,
inline/block lists, and folded descriptions. LF and CRLF files are supported.
Unknown fields, duplicate keys, wrong types, explicit null values, unknown tools,
and unknown models are errors; they are not silently ignored.

| Field | Validation And Meaning |
| --- | --- |
| `name` | ASCII `[a-z][a-z0-9_-]{0,63}`; case-sensitive; built-ins and their aliases are reserved |
| `description` | Nonempty UTF-8 text, at most 1024 bytes, without control characters; visible in the model's tool catalog |
| `extends` | Optional built-in name/alias or another valid custom agent identifier; defaults to `general` |
| `tools` | Optional YAML list of registered ASCII tool names; intersected with the inherited set; `[]` means no tools |
| `model` | Optional ASCII model ID; a first-party preset or the parent's explicitly selected configured model |
| Body | Nonempty UTF-8 Markdown, at most 64 KiB; only newline and tab control characters are permitted |

The entire file is limited to 128 KiB. Reserved aliases include `reviewer`,
`codereview`, `tester`, `testwriter`, `debug`, `doc`, and `docs`.
Use `>-` for a folded description without a trailing newline. Human text,
including Chinese descriptions and instructions, does not need to be ASCII.

## Discovery And Precedence

1. User definitions are direct `*.md` files in `$ORCA_HOME/agents`, or
   `~/.orca/agents` when `ORCA_HOME` is unset. Orca's scoped home override is
   honored by embedded hosts and tests.
2. Project definitions are direct `*.md` files in `<project>/.orca/agents`.
   The project is the nearest ancestor with `.git`, or the current directory
   when no Git root exists. Both the current directory and the project root
   must be trusted in the user-owned folder trust store. An explicitly
   untrusted nested directory disables project discovery there.
3. Project entries override user entries with the same declared name.
   Directories are processed in lexical filename order; the final catalog is
   sorted by agent name. Discovery is not recursive.
4. Duplicate names disable every copy in that layer. A duplicate project name
   also suppresses the user entry, rather than silently falling back.
5. An invalid entry suppresses an existing entry with its parseable declared
   name or valid filename stem. For syntactically unreadable YAML, only the
   filename stem can identify the override. Name files `<name>.md` to keep
   invalid overrides fail-closed even when YAML cannot be parsed.
6. Invalid inherited definitions, missing parents, cycles, and inheritance
   chains exceeding 16 custom agents are excluded. No fallback is made after
   inheritance resolution fails. Built-in names cannot be overridden.

Definition files and agent directories must not be symlinks. Only regular
files are read; oversized, unreadable, and malformed entries are diagnosed.
Valid unrelated definitions remain usable. The tool catalog notes excluded
entries; requesting an unavailable custom name returns a diagnostic without
starting a child.

The custom-agent catalog is fixed at the session's first turn, including the
instructions and tool policy used to launch children. Editing a definition
after that does not change the session's advertised agents or later launches.
Start a new session to use the edit.

## Calling An Agent

The model sees custom identifiers and descriptions in the `subagent` tool
schema and description. Bodies and filesystem paths are not included in that
catalog. For example:

```json
{
  "description": "Review API compatibility",
  "prompt": "Review the changes in src/api.rs",
  "subagent_type": "audit-api"
}
```

`subagent` has one submission protocol and no model-facing `mode` field. It
quickly returns an accepted `task_id`; capacity pressure returns `queued`
instead of a tool failure. Use `task_wait`, `task_read_output`, `task_list`, and
`task_stop` to observe and control the task. Finite parent budgets use the same
path through a durable child reservation.

## Subagents Started From The TUI

A subagent that the TUI starts runs inside the TUI's own process, on a thread
of its own, instead of as a separate `orca subagent-worker` process. It ends
when the TUI quits. `orca exec` is unchanged: its subagents are detached
workers that finish after it exits. The app server and ACP are unchanged too.

Five kinds of subagent run as before, as a separate process that finishes in
the background after the TUI quits and may ask for a permission on its row:
those started with `"isolation": "worktree"`, custom agent types, those under
a finite parent budget, those continued with `resume_from`, and those started
with `"context": "fork"` when the parent's history could be used. The rest of
this section is about the others.

Main normally waits for the subagents it starts before ending its turn.
In the TUI:

- Open a subagent while it runs or after it ends. Mark its row under the
  conversation with Shift+↑↓ and press Enter, or open `/agents`, select its
  row and press Enter. The hint reads `Enter open conversation`. The first
  message is the task it was given. Its text, tool calls and results follow.
  Esc returns to main and interrupts nothing. If the Agents panel, an expanded
  list of agents, search, shortcuts help or a selection is open, Esc closes
  that first. Press Esc again when the conversation is clear to return.
- Under the subagent's conversation, `Main is running · Esc return` means
  main is still working. If main needs approval or an answer, the line says
  so. Under a finished agent it reads `Main is idle · Esc return` when main
  is idle. The agent you open is marked. Shift+↑↓ moves the mark; Enter
  opens the marked agent without going back to main. A nested agent's row
  in this view opens the Agents panel instead.
- Text sent to a running subagent goes back to the composer with
  `this is an agent's conversation: return to the main conversation to send a message`.
- A finished subagent's view ends with `This agent has finished. Its conversation is read-only.`
  This also holds if it finished on screen or you opened it after a resume.
  Text goes back to the composer with
  `this agent has finished: return to the main conversation to send a message`.
  If the conversation cannot be read, or another task continued it, you see
  `This agent's conversation could not be loaded. Showing its last checkpoint.`
  Only this task's checkpoint is shown. If there is none, you see just the
  checkpoint and read-only notices.
- Where main started its agents, it shows one launch line per batch: `Delegating to 1 agent` or
  `Delegating to N agents in parallel`. Each agent gets one end line:
  `Agent completed: <name>`, `Agent stopped: <name>`, `Agent cancelled: <name>`,
  `Agent failed: <error>` or `Agent blocked: <error>` (the name if there is no
  error). These lines belong to main wherever you were looking. They are
  there when you return or resume. Main's running row and timer continue
  across the visit.
- Esc or Ctrl+C on the running main turn stops its subagents with it. Their
  rows leave the dock and read `cancelled` in the Agents panel.
- `s` on a subagent's row in the Agents panel stops that subagent only. A
  `Stopping "<name>".` notice appears, and the row reads `stopped` once the
  subagent has settled. Main's turn and the other subagents go on. Main is
  told who stopped it, in these words: `the person using Orca stopped this
  subagent from the Agents panel; it was not stopped by you and it did not
  fail. Do not start it again unless they ask.` A `subagent` call of main's
  that was waiting for the stopped one returns it, and a subagent started in
  the background reads `stopped` in main's `task_wait`, with it as the
  result. Its completion notice gives the same line, without the caveat that
  follows a subagent's own report.
- Ctrl+C works the same in main and in a subagent's view. While main runs,
  the first Ctrl+C stops main's turn and its subagents with it. Once none
  are running, the next says `Press Ctrl+C again to quit.` Press it again
  within two seconds to quit. If main is idle but one agent still runs, the
  first says `1 agent is still running. Press Ctrl+C again to quit and stop it.`
  With several it says `N agents are still running. Press Ctrl+C again to quit and stop them.`
  Quitting stops those agents and records them as stopped.
- If Orca was killed while a subagent ran, after `--resume` its row reads
  `failed`. Main shows `Agent failed: …` once. The quit line does not count
  that agent as running.
- A main turn whose subagents are running is not moved to the background.
  Ctrl+B on it, in main's conversation or in a subagent's, shows
  `This turn cannot move to the background while its subagents are running.`,
  and the turn goes on in the foreground. The key is not remembered: once
  the subagents have ended, press it again to move the turn. While main
  waits on a `subagent` call, the line is shown at once, even while the
  subagent is still queued. A Ctrl+B pressed while main runs another tool,
  a `subagent` call that waits for approval included, takes effect when
  main's next reply starts, and is answered with the same line if a
  subagent has started by then. A `subagent` call whose subagent gets no
  conversation of its own is such a tool: its subagent does not count.

Such a subagent never asks the person, or the main agent, anything. What it
may do is fixed when it starts, from the main conversation's permission mode,
rules and sandbox at that moment; a later change to main's mode does not reach
a running subagent:

| Main's mode | The subagent |
| --- | --- |
| full-auto (the TUI's default) | does all of it on the host, as main does |
| auto-edit | writes, runs commands and uses the network inside the sandbox, as main does |
| suggest | reads only: main would ask before each write or command, and the subagent cannot ask |
| plan | reads only, as main |

Where main would be asked (a write or command in suggest, anything outside
the sandbox, a tool with an "always ask" rule), the subagent's call is refused
at once with `This call needs an approval, and a subagent cannot ask for one.`
and the subagent goes on. It is not offered a tool that asks the user a
question, and an MCP server's request for input is declined. The subagent is
told at its start to say in its reply what it could not do, so the main
agent, which can ask, may do it.

### Starting from parent history

`context` accepts `fresh` (the default) or `fork`. A fresh child starts with its
own system prompt and a standalone brief. Use `"context": "fork"` when the
child needs evidence from earlier turns of the parent conversation.

A fork starts with the parent's DeepSeek system prompt, ordered tools, and
completed history. Orca excludes the current turn and appends the task and
role instructions as a user message. The child sends only changes to context
fragments that the inherited history already contains. The parent's plan,
MCP instructions, and request-only context after the cut stay with the parent. The child uses the
parent's model and reasoning effort, so omit `model`. The inherited catalog
can show tools outside the child's role. Orca returns the role's refusal to
the child so it can continue. The child's transcript and result begin with
its own task.

If Orca has no completed parent turn or cannot recover its request prefix, it
starts a fresh child and includes a note. A fork requires a compatible parent
DeepSeek model and endpoint. Use `resume_from` by itself to continue an existing
child. Sibling forks in the same session share one saved copy of the prefix.
Orca keeps a private, redacted copy for queued launches and resumption. A
child started in the parent's process uses the original captured request. If
an inherited instruction source loses trust, the child continues from its own
history with a rebuilt prompt. In Orca
0.5.25, a fork child resumes from its own transcript without the inherited
history. A newer build can restore the saved prefix again after upgrading.

## Scheduling And Capacity

Direct, nested, Workflow, hosted, continued, and recovered children share one
durable execution scope per root task tree:

```toml
[subagents]
delegation = "adaptive" # off | explicit | adaptive
max_depth = 2
max_running = 32
max_queued = 256
max_live_tasks = 512
max_investigation_turns = 6
max_investigation_tool_calls = 8
```

`max_running` is a capacity ceiling, not a target number of children. When it
is full, accepted work remains queued without creating a worker. `max_queued`
bounds work waiting to start; `max_live_tasks` bounds all non-terminal
descendants, including parents that released their execution lease while
waiting for children. A waiting parent must re-enter the fair queue before it
can continue.

A steer from the user ends the parent's `task_wait` or end-of-turn child wait
so the parent can handle it on the next model step. The parent can wait again
afterward. When only child results remain, the parent can end its reply and
Orca resumes the turn as those results arrive.

The retired `max_parallel` key is ignored. Use `delegation = "off"` to disable
new child submission.

Built-in read-only roles (`explorer` and `code_reviewer`) have runtime-owned
evidence limits. After 6 model turns or 8 started tool calls, Orca closes the
unstarted remainder of the current tool batch and asks for one tool-free final
report. That report is capped at 1024 output tokens and a 90-second total
deadline; a truncation or deadline returns an explicit partial report rather
than leaving the task running. A delegated parent enters the same final-report
phase after all direct children settle only when its user request explicitly
says both that the work is read-only and that files must not be edited.

Isolation defaults to `none`: custom agents use the current checkout.
Definitions cannot request isolation or enable worktrees. The existing explicit
`"isolation": "worktree"` call option is unchanged.

## Permissions And Inheritance

Instructions are appended in ancestor-to-descendant order. A definition's
`tools` list can only remove inherited tools, never add them. Omitting it
inherits the full ancestor set; an empty intersection grants no tools.

The final list is also intersected with the admitting parent's explicit tool
policy and any effective custom-parent tool policy. The same list controls
model-visible tools and runtime admission, including tool calls rewritten by
hooks. Tool names are validated against the registered tool catalog, but a
registered tool outside the ancestor's set is not granted. In particular,
custom definitions cannot introduce nested delegation, external tools, or MCP
tools absent from the built-in ancestor's allowlist.

Approval mode, execution profile, permission rules/profiles, workspace roots,
and additional working directories are inherited from the parent unchanged.
Definitions cannot supply permission grants or execution settings.
Parent task budgets and depth limits continue to apply. A subagent that runs
inside the TUI applies them without ever asking; see
[Subagents Started From The TUI](#subagents-started-from-the-tui).

Model selection uses an explicit call override first, then the nearest
definition's model, then the parent. `auto` retains the existing parent-router
semantics. Resolved model selection is frozen at admission.

## Persistence And Resume

Before execution crosses a worker boundary, Orca captures the effective
instructions, description, model, narrowed tools, and parent delegation policy.
This snapshot is stored with the serialized detached launch request and durable
continuation. The child config uses this owned snapshot, never the source path.
The compatibility digest includes the snapshot only for custom agents;
existing built-in continuation hashes are unchanged.

Custom sync calls use the checkpointed child execution path, including when
launched by a hosted parent. Completion returns the existing
`[agent_continuation]` footer. Resume using its `resume_from` value:

```json
{
  "description": "Continue API review",
  "prompt": "Also examine the error responses",
  "resume_from": "<continuation-id>"
}
```

Resume restores the original effective definition after process restart even
if its Markdown file was edited or deleted. It does not rediscover that agent.
Conflicting explicit type/model/isolation options fail closed. A changed
parent delegation policy or a newly narrower parent tool policy also fails
closed; start a new child to adopt a new policy. Old custom continuations that
lack a frozen definition cannot be resumed as newly defined agents.

For a checkpointed custom child, restart the **same recorded parent session**
with `orca exec --continue` or `orca exec --resume <session-id>`, then call
`resume_from`. Each parent turn allocates a new task-tree root. Recovery retains
the immutable source child task and its original committed owning generation,
and binds only the new attempt to the current parent task. This keeps cancellation
and transcript ownership aligned across repeated restarts.

A changed parent-task binding requires digest-verified committed child-start
and parent-admission records in that same session. Before starting a hosted
parent turn, the actor allocates its canonical registry main-session task and
commits it as `AgentLoopTurnStarted.turn.admitted_main_task_id` under the
generation fence. The hosted runner receives that same task through
`with_task_id`; its semantic `turn.started` event uses that canonical ID.
`AgentLoopTurnStarted.turn.task_id` is a separate, potentially synthetic
`typed-user-turn-*` ID and must not be used to infer the registry parent.
If admission or runtime startup fails after allocation, the actor marks the
allocated root failed rather than leaving it queued.

Recovery requires this explicit admission association for the original source
and latest attempt's owning generations and for the current live generation
fence and owner epoch. Their child-start records must identify the same
continuation; the latest must identify the exact preceding attempt.
Task IDs obtained from the global task index are not ownership evidence.
Missing associations (including older records containing only synthetic loop
IDs), another thread/session (including a fork), a substituted parent root, or
a stale generation cannot authorize this recovery.
Checkpoint integrity, frozen compatibility, prompt idempotency, live-owner
exclusion, and compare-and-swap revision checks still apply. Unrecorded callers
and other continuation paths retain exact parent-task matching.

Every resumed attempt uses the same task submission protocol. For a detached
source, the committed child-start and task-upsert batch must identify one
unambiguous admitted parent operation. Its task owner must match the original
task, attempt, and revision. When the latest attempt is detached, its current
binding must additionally match the committed authority digest and exact parent
fence. A superseded detached binding is not used as historical proof.

Inline and detached worker admissions record their continuation at different
safe boundaries. Recovery checks the exact persisted revision for the recorded
worker path instead of accepting an arbitrary preceding attempt. An active
worker still excludes a competing resume. Finite parent budgets are supported
through the same durable child reservation and settlement path.

The freeze covers the agent definition and delegated permissions, not all
project files, memory, hooks, or tools' external state. Those retain their
existing runtime behavior. New launches discover current definitions.
