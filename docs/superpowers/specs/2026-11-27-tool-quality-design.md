# Tool quality: repeated calls, parallel network reads, clearer tool guidance

Date: 2026-11-27. Size: small project. It draws on how DeepSeek's harness (dsh) runs tools. The user asked for these learnings to be adopted.

## 1. A reminder when the model repeats the exact same call

**Why.** A model stuck in a loop calls the same tool with the same arguments again and again. Each call costs a request, and nothing tells it to stop.

dsh keeps, per agent, the key (tool name plus JSON arguments with every object's keys sorted).
- At 3, 5 and 8 identical calls in a row it appends a separate reminder message and leaves the tool result untouched.
- A user message resets the count.
- Polling tools are excluded.
- dsh chose to advise rather than block, because polling repeats calls legitimately.

**Change.**
- **The key.** The runtime keeps a per-thread counter over consecutive completed tool calls. The key is the tool name plus its arguments as canonical JSON, keys sorted recursively.
- **What resets it.** A different key resets the count to 1, and so does a new user message.
- **Excluded tools.** `task_wait`, `task_read_output` and `task_list` neither count nor reset. Calls made inside a script are not counted.
- **The reminder.** When the count reaches 3, 5 or 8, the runtime appends one system message after that call's result.
  - The first one says: `You have called {tool} with the same arguments {n} times in a row. The result will not change; use what you already have, change the arguments, or try a different approach.`
  - Later ones add the arguments, cut at 300 characters.
- **Advisory only.** The call itself always runs.

## 2. Network reads run in parallel when no approval is needed

**Why.**
- `web_search`, `web_fetch` and `read_document` carry the `NetworkSearch` capability. `ToolCapabilitySet::is_read_only` (`crates/orca-core/src/tool_types.rs`) excludes it, so these calls run one after another.
- A turn that fetches five pages waits for each in turn, and `web_fetch` may take up to 60 s.
- dsh lets them run together in a pool of up to 10, and keeps approvals and result order in model order.

**Change.**
- `runtime_tool_scheduler.rs` gains a batch for consecutive calls whose capabilities are all read-only or `NetworkSearch`.
- The batch forms only when the session's approval policy already allows every call in it without asking.
  - That is true in auto-edit and full-auto modes, and wherever a permission rule allows it.
  - If any call would ask, the window falls back to running one by one, as today.
- The batch is limited by `tools.max_read_parallel`. Results are recorded in the model's order. Hooks run in the model's order before the batch starts, as for read-only batches.

## 3. Guidance the tool descriptions lack

**Why.** dsh's tool descriptions state rules that Orca's do not. Models then waste calls, or work around a sandbox denial.

**Change.** Add these sentences to the model-facing descriptions, worded to fit each description:
- **bash:**
  - each call starts a fresh shell, so pass `workdir` (or the tool's existing directory argument) instead of `cd`;
  - when a sandbox blocks a command, that is a policy decision: do not retry it another way, and report it or ask;
  - in a command that deletes or overwrites paths built from variables, guard the variables with `${VAR:?}`.
- **edit:** read the file first, unless you just created or edited it in this session.
- **read_file:** use it rather than `cat`, `head`, `tail` or `sed` through bash.

Also remove the stray run of spaces in the bash description ("system from              process start").

This changes the tool schemas' bytes once, so every session's first request after the upgrade misses the prompt cache once. The release note says so.

## Non-goals

- No blocking of repeated calls.
- No parallelism for any call that needs approval.
- No apply_patch here; it is separate work.

## Constraints

- If a test pins prompt or schema sizes, update the pins and report the old and new values.
- The repository is public; describe changes by what they do for a user.
