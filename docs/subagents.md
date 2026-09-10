# File-Defined Subagents

Custom agents provide reusable instructions and a narrower tool policy for the
`subagent` tool. Built-ins (`general`, `code_reviewer`, `test_writer`, `debugger`,
and `documenter`) retain their existing behavior.

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

`mode` defaults to `sync`. `"mode": "async"` uses the existing detached-worker
path, requiring persistent task ownership and an actor-owned operation fence.
Use `subagent_status` or `task_list` to observe it. A cost-budgeted parent
continues to require sync mode.

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
Parent task budgets and depth limits continue to apply.

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

Sync and async attempts can resume in either execution mode. For a detached
source, the committed child-start and task-upsert batch must identify one
unambiguous admitted parent operation. Its task owner must match the original
task, attempt, and revision. When the latest attempt is detached, its current
binding must additionally match the committed authority digest and exact parent
fence. A superseded detached binding is not used as historical proof.

The original sync admission records continuation revision 1, after acquiring
its lease. Original async admission records revision 0, before spawning and
acquiring the worker. Recovery checks the exact revision for each mode rather
than accepting an arbitrary preceding attempt. An active worker still excludes
a competing resume, and the existing cost-budget restriction on async mode
continues to apply.

The freeze covers the agent definition and delegated permissions, not all
project files, memory, hooks, or tools' external state. Those retain their
existing runtime behavior. New launches discover current definitions.
