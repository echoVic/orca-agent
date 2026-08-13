# Memory

Orca has two local memory paths with different ownership:

- `/remember ...` writes an explicit user fact. `/remember project: ...` writes
  an explicit project fact. These Markdown files are loaded with bounded size at
  session startup.
- Automatic memory extracts a small set of durable project facts after a
  successful recorded root turn. Later turns retrieve only candidates relevant
  to the new prompt.

Automatic memory is enabled by default. Disable it in user or trusted-project
`config.toml`:

```toml
auto_memory = false
```

Disabling automatic memory stops its worker, capture, and recall for newly
started sessions. It does not delete existing files and does not disable manual
`/remember` facts.

## What Is Captured

Automatic extraction considers only the completed turn's user and assistant
messages. System context and tool output are excluded. The source is redacted
and bounded to 32 KiB before it is sent to the configured provider's auxiliary
model. A candidate must be explicitly classified as one of:

- `user`: a stable user preference;
- `feedback`: confirmed guidance about how the agent should work;
- `project`: a non-obvious project decision that cannot simply be recovered
  from the current repository or Git history;
- `reference`: a stable external reference needed in later work.

Transient task state, code summaries, raw tool output, credentials, private
values, and unverified claims are rejected by the extractor prompt. Candidates
that still match the local secret detector are rejected before persistence.

A turn is eligible only after all of these are true:

1. automatic memory is enabled;
2. session history is persistent;
3. the root turn finishes successfully, including its verifier;
4. the transcript completion record commits durably.

Failed, cancelled, verifier-failed, history-disabled, subagent, and intermediate
tool turns do not create automatic candidates.

## Capture and Recovery

Eligible turns first create a durable extraction job. The foreground result is
not coupled to the auxiliary model call. A session-owned worker claims jobs with
a lease, renews it by wall clock even when the provider emits no streaming
increments, retries failures up to three times, and resumes pending or expired
work when a later session starts. Cancellation releases the lease without
consuming a retry. Empty, malformed, unclassified, or over-limit provider output
is a failed extraction attempt rather than a successful zero-candidate commit;
explicit `NOTHING` is the only no-candidate success response.

Transient job-store or lock failures reschedule the worker after thirty seconds
instead of leaving pending work dormant until another session starts. Project
metadata is re-read after lock acquisition so corruption that appears during
lock contention is preserved and reported rather than overwritten.

Jobs are idempotent by session, turn, and redacted-source digest. Candidate
writes deduplicate only normalized exact facts and atomically replace the
authoritative ledger under a cross-process lock. Near-duplicate corrections are
retained with their own provenance instead of being silently discarded. A
worker must still own an unexpired fenced lease when it publishes candidates;
an old worker that returns after takeover cannot write to the ledger. The
human-readable Markdown view and SQLite FTS5 search index are derived
projections. If either publication fails, the ledger remains committed; a later
write repairs both, and recall also rebuilds a missing, stale, or corrupt index
from the ledger.

Before a new turn selects memory, it places a five-second bounded barrier behind
worker commands already queued in the session. If auxiliary work is still
running or backlogged, the turn uses the last committed ledger instead of
blocking indefinitely, while the worker continues in the background. Provider
suspension keeps the original turn snapshot. After a process restart, exact
messages are reconstructed from durable transcript records carrying the same
`turn_id` rather than from the whole session.

## Recall

At the beginning of each new turn, Orca searches normalized candidate tokens
with the derived SQLite FTS5 index, ordered by BM25 relevance and record time.
If the index is unavailable or cannot be repaired, recall falls back to bounded
in-process lexical overlap plus recency; automatic memory therefore remains
usable without its cache. It injects at most six relevant entries and 3,072
bytes as internal context, including category, age, session id, and turn id.
The context is not appended to the conversation transcript.

Recalled entries are historical hints, not current truth. The system prompt
marks them as claims rather than instructions and requires verification against
the repository, Git history, and external state before relying on them. A
provider continuation keeps the recall snapshot chosen for the original turn
instead of changing context halfway through a response.

## Storage

The root is `$ORCA_HOME/memory` when `ORCA_HOME` is set, otherwise
`~/.orca/memory`.

```text
memory/
├── user.md
└── projects/
    └── <sha256-project-id>/
        ├── project.json
        ├── memory.md
        ├── candidates.jsonl
        ├── auto-memory.md
        ├── index.sqlite3
        └── jobs/
            └── <job-id>.json
```

- `project.json` identifies the hashed directory with its normalized Git-origin
  or canonical-path identity and last-seen working directory. It is not sent to
  the model.
- `memory.md` contains explicit project facts from `/remember project:`.
- `candidates.jsonl` is the authoritative automatic-memory ledger.
- `auto-memory.md` is a human-readable projection with provenance comments.
- `index.sqlite3` is a derived FTS5 cache keyed by the candidate-ledger
  fingerprint. It can be deleted and is rebuilt on the next write or recall.
- `jobs/*.json` stores bounded redacted extraction inputs, lease state, attempts,
  errors, and extractor provenance for crash recovery and audit.

Git clones and worktrees with the same normalized `origin` share project memory.
Repositories without an origin use their canonical repository root; non-Git
directories use their canonical path. User info, query parameters, and fragments
are removed from origin URLs before identity hashing and metadata publication.

The candidate ledger retains 128 candidates, its Markdown projection shows the
newest 64, and the job directory retains the newest 128 committed jobs. At most
64 active jobs are admitted per project; exhausted jobs no longer consume that
capacity.

## Inspect or Delete

All files are local and human-inspectable. To find the current project, inspect
`projects/*/project.json` and match `last_seen_cwd` or `project_identity`.

For a safe full project-memory deletion:

1. set `auto_memory = false` and exit all Orca processes using the project;
2. inspect `project.json` to confirm the exact hashed directory;
3. move that one project directory to Trash or another recoverable location;
4. restart Orca.

Deleting only `auto-memory.md` or `index.sqlite3` does not clear memory because
both are derived from `candidates.jsonl`. Deleting only `candidates.jsonl`
leaves recoverable job evidence and is not a complete reset. Delete `user.md`
separately only when the intent is to remove explicit user-wide facts.

Orca deliberately does not expose an unversioned `/memory clear` shortcut. The
runtime's typed memory mutation surface requires revisioned, idempotent deletion
semantics before destructive clearing can be safely added.
