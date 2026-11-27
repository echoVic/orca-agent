# Context that changes rarely is sent once; a resumed session keeps its prompt

Date: 2026-11-27. Size: project. It draws on DeepSeek's harness (dsh); the user asked for these learnings to be adopted.

## Intent

DeepSeek bills a cached prompt prefix at about 1/50 of the uncached price on flash, and 1/30 on pro (`crates/orca-runtime/src/pricing/rates.rs`). Two habits of Orca's request building send bytes uncached that need not be.

1. **The overlay is rebuilt every request.** Every request ends with one system message rebuilt from the internal-context fragments: runtime, date, mode, goal, plan, skill, memory, and the new mcp fragment (`render_internal_context`, `crates/orca-provider/src/deepseek_http.rs`). It sits after the history and is never saved, so all of it is uncached on every request. The fragment caps add up to about 18K tokens. Memory is refreshed every turn and skill text every prompt, but most of these fragments change rarely.
2. **Resume rebuilds the system prompt.** It is rebuilt from the current cwd, OS, shell, memory, AGENTS.md, mode and the installed version's prompt text (`history.rs`, `session.rs`, `agent_common.rs`). If any of that changed (an upgrade, an AGENTS.md edit, another mode), the first request after resume misses the cache from its first token. A 400K-token pro session then pays about 30 times more for that request, and waits for the whole history to be processed again.

dsh fixes both the same way:
- **Context that changes is appended to history, and only when it changes.** It is appended as a durable message that says it replaces earlier versions, and it is sent again if compaction removed it.
- **The system prompt is node zero of the log, and each named section is recorded.** On every assembly, including the first after a restart, each section is compared with the recorded text, and only changed sections are appended, with "replaces" or "no longer applies" notices. The leading prompt is rebuilt only after compaction, which has already changed the prefix.

## Goals

1. **Fragments other than `plan` are appended when they change.**
   - At each model request the runtime renders the fragments as today. For each one other than `plan`, it compares the text with the last version it appended to history in this session.
   - Changed fragments are appended together, before the request, as one system message recorded in the transcript. Each fragment appears under `Current <kind> context (this replaces any earlier version):`.
   - A fragment that disappeared is written as `The <kind> context no longer applies.`
   - Unchanged fragments add nothing.
2. **`plan` stays at the tail** as today, because it changes often and its value is in being current.
3. **After compaction**, the current versions of all appended fragments are appended again after the summary, because the summary may have dropped them.
4. **Resume rebuilds the "last appended" state from the transcript**, so a resumed session sends nothing new for unchanged fragments.
5. **The system prompt is recorded in sections, and resume reuses it.**
   - The session's system prompt is recorded with its named sections when the session starts.
   - On resume or fork, Orca builds the prompt it would build today, section by section, and compares it with the recorded sections.
   - The recorded prompt is sent unchanged. Each changed section is appended as one system message after the history: `The "<section>" instructions changed; they now read:` followed by the new text. A removed section is appended as `The "<section>" instructions no longer apply.`
   - A rebuild of the leading prompt happens only at compaction.
6. **Measure it.** Report, on a terminal_bench subset or a recorded long session, the uncached prompt tokens per request before and after.

## Non-goals

- No change to which fragments exist or to their caps.
- No change to the prompt's wording.

## Constraints

- Requests must stay valid for DeepSeek's thinking mode. A system message mid-history is already used for notices today.
- Transcript records added for these messages follow the session-format rule: a new record type that older versions may skip is written with `"ignorable": true` (from the session newer-version work). Prefer reusing the existing system-message record.
- A session written before this change resumes exactly as today: no recorded sections means the prompt is rebuilt.
