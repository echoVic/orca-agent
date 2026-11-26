# A session written by a newer Orca is "newer", not corrupt

Date: 2026-11-27. Size: small project.

## Intent

Today, when an Orca version reads a session that a later version wrote, it calls the session corrupt and locks it: no resume, no fork, no edits. Users who downgrade, or who run two versions against the same `~/.orca`, find healthy sessions marked broken, and release notes have had to warn them to archive sessions before downgrading. Orca should tell such a session apart from a corrupt one, say plainly that a newer Orca wrote it, and read past records it may skip.

DeepSeek's harness solved the same problem this way:
- a format number in the header that only goes up;
- refusal by name, "written by a newer harness, upgrade", kept apart from corruption errors;
- an `ignorable: true` envelope flag for records an older reader may skip.

Required is the default, because a forgotten flag only refuses too much, while skipping by default would resume a session with records missing.

## How it works today

- `crates/orca-runtime/src/thread_store/types.rs`: `SessionRecord` is a closed serde enum tagged by `type`, with 24 record types. `SessionMeta { schema_version }` is `deny_unknown_fields`.
- `reader.rs`: a line that does not parse becomes `StoredSessionHealth::Quarantined` with a code such as `malformed_record`. Healths are `Healthy`, `RecoverableTail`, `Quarantined` and `InspectionLimited`.
- `history.rs`: `SESSION_SCHEMA_VERSION = 1` is written into `session.meta` but never compared on read.
- `writer.rs`: `rewrite_records_unlocked` rewrites a transcript from typed records.

## Goals

1. **A fifth health, `NewerVersion`** (`as_str` `"newer_version"`). It is used when:
   - a line is a JSON object whose `type` is not one this version knows, and which lacks `"ignorable": true` (code `unknown_record_type`, and the issue names the type);
   - or `session.meta` has a `schema_version` greater than `SESSION_SCHEMA_VERSION` (code `newer_schema_version`).
   Like `Quarantined`, it blocks resume, fork and mutation.
2. **Its message says what to do.** For example: `this session was written by a newer version of Orca (it has a "<type>" record this version does not know); upgrade Orca to open it`. Wherever sessions are listed with their health, the TUI and the CLI show this state as newer, not damaged.
3. **An unknown type marked `"ignorable": true` is skipped.** The session stays `Healthy`, and the records after it are read.
4. **A rewrite keeps every skipped line byte for byte**, in its position among the records around it. This covers `rewrite_records_unlocked` and anything else that writes a transcript back from what it read.
5. **`SessionMeta` accepts fields it does not know.** Drop `deny_unknown_fields`. A field a newer version adds no longer locks the session; a change older readers must refuse raises `schema_version` instead.
6. **Unchanged:**
   - a line that is not JSON;
   - a known type whose fields do not parse;
   - an invalid semantic or asset record;
   - a torn last line.
   These keep their current health and code.
7. **The rule for future record types is written down** where record types are defined (a doc comment on `SessionRecord`) and in the session docs. A new record type that older versions may safely skip must be written with `"ignorable": true`; any other new record type makes older versions refuse the session as newer.

## Non-goals

- No migration machinery, and no change to existing record types or to what is written today.
- No change to how a corrupt session is handled.

## Constraints

- Site docs: `site/src/docs/md/en/session-commands.mdx` and `site/src/docs/md/zh/session-commands.mdx` describe session health and must gain the newer-version state. Both languages say the same thing.
- If session health is part of a JSON output, the change is additive. That covers `orca sessions` listing output, the runtime surface and ACP. Update `docs/harness-contract.md` if it lists the healths, and run `scripts/validate-runtime-surface-contract.mjs` if a pinned surface type changes.

## Left to the builder

- How known types are recognized; it must stay in step with `SessionRecord` by construction or by a test.
- How skipped lines travel to the rewrite path.
