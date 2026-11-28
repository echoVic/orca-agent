# Live Subagent Conversations, Stage 2: A View That Holds — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A person can leave the main conversation for an agent's and come back to find main as it stands (running row, lines, notices), can open a finished agent's conversation now and after `--resume`, and never sees an agent's row stay "running" after the agent is gone.

**Architecture:** Returning to main stops pretending main is a freshly loaded, idle history: main is re-projected from its surface snapshot as the running conversation it is. The lines that belong to main because of its agents (`Delegating to N agents`, `Agent completed: …`) are derived from the facts main's surface holds whenever main's history is built, instead of being pushed into whatever conversation is on screen. A finished agent opens read-only from its live thread's snapshot, or from its saved session after a resume. The runtime settles a kept agent's row from its registry record when the agent's end never reached the surface. Actions taken from an agent's view that concern main's tasks act on main's thread.

**Tech Stack:** Rust 2024, `orca-runtime` (thread actors, surface, task registry), `orca-tui` (ratatui 0.30, crossterm 0.29), `orca-core`. Real-terminal checks: Python `pty` + `pyte` (driver `/tmp/orca-stage1/t10/drive.py`).

**Spec:** `docs/superpowers/specs/2026-11-24-live-subagent-conversations-design.md` (stage 2: T5, T6, T7, and "To verify first" items 5 and 6; plus the items stage 1's final review sent here).

**Research this plan argues from:** `.superpowers/sdd/2026-11-26-live-subagent-conversations-stage-2/research.md` (code paths with file:line at `17de2db8`, by reading; sections A–F are cited below as "research A5", "research C0", …). Stage 1's ledger and final review: `.superpowers/sdd/2026-11-24-live-subagent-conversations-stage-1/progress.md`, `final-review.md`.

## Global Constraints

- Branch `feat/live-subagent-view`. One commit per task (a fix after review is a new commit; nothing is amended). Implementers do not push; the controller does.
- `orca exec`, the app server and ACP keep today's behaviour unless a task says otherwise in so many words. `tests/subagent_contract.rs` passes without being edited.
- Subagents that keep a snapshot (isolation, custom types, a finite budget, `resume_from`) are left exactly as they are: detached, opened as a checkpoint transcript in the Agents panel.
- What the model sees does not change in this stage: no tool argument, no tool result text, no notice text sent to the model.
- Strings a person reads are used exactly as written in the task that introduces them. Existing strings are not reworded unless the task says so.
- Live display and the display rebuilt from a snapshot (return, resume) are two paths. Every task that changes what a conversation shows proves both, and says in its report which test proves which.
- No new `#[allow(...)]`, no new clippy warning in touched code (`cargo clippy -p orca-runtime -p orca-tui -p orca-core 2>&1 | grep -c "^warning"` is 549 before this stage), no `unwrap()` outside tests.
- A new direct platform call (`libc`, `std::os::unix`, `windows-sys`) is recorded in `docs/superpowers/specs/2026-07-28-native-windows-platform-foundation.manifest.json`; a new `orca_runtime::surface` export, variant or field is recorded in the surface contract manifest and digests. `for f in scripts/validate-*.mjs scripts/test-validate-*.mjs; do node "$f" >/dev/null || echo "FAILED $f"; done` prints nothing at the end of every task. `scripts/` is edited only where a task names the line; anything else a validator pins is reported to the controller first.
- One cargo at a time, in the foreground, with a timeout. `crates/orca-runtime/tests/runtime_surface_interaction.rs` is run with `--test-threads=1` unless the between-stages fix for its lock has landed (the ledger says).
- Disk: if `df -h /` shows under 10 GB free, stop cargo and run `find target/debug/deps -type f -name '*.rcgu.o' -mmin +180 -delete`.
- The contents of `~/.orca/auth.json` are never printed, logged, copied into a report or committed; temporary copies are deleted when a run ends. Only Tasks 1 and 10 may use the real key, and Task 1 only if the mock cannot show a thing.
- Where a step gives the behaviour, the names and the test but not every line of the code, the code it touches is read first: line numbers are from `17de2db8` and are a place to start. Where a test is given as a name plus assertions in comments, real assertions are written for every commented one.
- A kept-host test that panics can hang its run; such a run is reported with the test's name, not waited out.

## Rulings made while planning (the controller's, recorded in the ledger)

- **Ctrl+C is the same key in every view.** In an agent's view, while main runs behind it, the first Ctrl+C interrupts main's turn (and with it its agents), as it does in main's view; the quit line follows on the next press. Esc in an agent's view still only returns.
- **The timer in an agent's view** counts from the agent's start, not from the moment the view was opened and not from its current turn.
- **`Stopping <label>.` stays** as the immediate answer to `s`; it is a transient line and need not survive a rebuild. What survives is the derived `Agent stopped: <name>`.
- **A cancelled agent is not an error:** its row is drawn in the muted colour of a stopped one and carries no error line.
- **The word that ends an agent's activity history** follows its status in every host (`completed`, `failed`, `cancelled`, `blocked`); today it is always `completed`.

## Amendments after Task 1 (2026-11-27, the controller's)

Task 1 ran at `d22a4710`, after the between-stages work B-E: owner stamps on task records, and recovery that skips live owners. Its report is `.superpowers/sdd/2026-11-26-live-subagent-conversations-stage-2/task-1-report.md` (A1–A9). Where these amendments differ from a task's text below, they win.

- **Task 3: the premise changes.**
  - **What A9a found.** After `kill -9` and `--resume latest`, the agent's registry record is still `running`, with no error. Resume opens the registry with `attach_for_cwd`, which runs no recovery (`session.rs:454`, `tasks.rs:1738`, `:1868`), so `mark_interrupted_if_active` never runs.
  - **Ruling: attach runs owner-aware recovery for records whose owner stamp names a dead owner.** It uses B-E's session-qualified stamp and its non-blocking liveness probe, with the same rule as open-time recovery.
    - Owner-less records stay untouched on attach, as today (B-E ruling Q3).
    - Records with a live owner, or a held lease, are left alone exactly as open-time recovery leaves them.
    - Make it its own commit inside Task 3: `fix(runtime): a resume ends the running records of owners that are gone`.
    - Add a registry-level test: attach fails a dead owner's running record and leaves a live owner's alone.
    - Say whether this changes what a reader of a dead owner's shell sees after a resume. Update ADR 0006's residuals to match.
  - **Then** `settle_stranded_kept_subagents` settles the row from the record, which has now ended.
  - **The test** `a_resumed_thread_settles_the_agents_it_lost` starts from the real state: a host dropped without settling, leaving a `running` record stamped by an owner that is gone, followed by an actual resume. It does not start from a seeded `Failed` record.
  - Cost if wrong: a record whose owner's lock probe wrongly reads "dead" (`flock` on NFS) is failed on resume, as open-time recovery already does today.
- **Task 4: cover both returns A8 found.**
  - (a) The agent still runs: on return, main's running row and `Delegating to N agents` are missing.
  - (b) A queued main turn started during the visit: the running row is back but its streamed text is not, although it is on disk.
  - **`started_at_ms`.** Take it from main's admitted task, not from `terminal.committed_at`, which reads 0 (A5). A5 also shows that the timed main tasks can be missing from the live surface until a resume reconciles them. So test what the live surface actually holds, and say which source the live path uses when the task is not there.
- **Task 6.**
  - Join reopened main tasks by `agent_loop_turns[].admitted_main_task_id`, not by `task_id` (A5).
  - Place a line by its owning operation where an item's time is unknown.
  - The mock's "two agents in one response" records two batches of one, not one batch of two. A batch of two needs a fixture that builds one.
- **Task 7: as planned.** A3: the saved child session holds its final reply both before quit and after a resume. So the checkpoint fallback is only for a load that fails, as written.
- **Task 8.**
  - **The draft guard.** A6 explains "Shift+Down marks no row": dock cycling is off while the composer holds text (`key_event_actions.rs:1708`).
    - If Shift+↑↓ does something in a composer with text, keep the guard, and say what it does. Behaviour 1 then reads "with an empty composer, a press always marks a row", and a test pins that a draft leaves the dock alone.
    - If Shift+↑↓ does nothing there, drop the guard.
  - **Either way, an open marks the agent on screen.** A6 shows an open starts from no mark.
  - **A7.** Main's natural completion waits for its kept agents. Behaviour 2 is therefore the "pin the reason" case: a test that the parent's turn does not end while its kept agents run, and nothing built.
- **Task 10.**
  - The overlap the mock cannot make goes into Task 10's real-terminal run: main's still-open stream on screen, a visit to a live agent, then return. Task 4's tests do not stand in for it.
  - The gate also re-checks a crash and resume against the registry file, not only against the dock.
- **Tasks 2, 5 and 9: as planned.**

## File Structure

| File | Responsibility in this plan |
|---|---|
| `crates/orca-runtime/src/agent_controller.rs` | The record names the child's thread from launch (Task 2). |
| `crates/orca-runtime/src/runtime_actor/thread_actor_generation.rs` | `settle_stranded_kept_subagents` (Task 3); the history word per status (Task 9). |
| `crates/orca-runtime/src/runtime_host.rs` | Call sites of the settling: thread open and the idle drain (Task 3). |
| `crates/orca-tui/src/protocol.rs` | `TuiEvent::ConversationReprojected`; `started_at_ms` on `TuiEvent::TurnStarted` (Task 4). |
| `crates/orca-tui/src/hosted_session.rs`, `hosted_child.rs`, `state_reducer.rs`, `types.rs` | Return re-projects a running main; no label; timers (Task 4). A finished agent's view (Task 7). |
| `crates/orca-tui/src/hosted_view.rs`, `operation_controller.rs`, `hosted_controller.rs`, `action_dispatcher.rs`, `global_actions.rs` | Main's thread for actions from an agent's view; Ctrl+C; shutdown (Task 5). Read-only focus without adoption (Task 7). |
| `crates/orca-tui/src/subagent_lines.rs` (new) | The one place that words `Delegating to N agents` and `Agent <status>: <name>` and decides where they sit in main's history (Task 6). |
| `crates/orca-tui/src/surface_projection.rs`, `workflow_panel.rs` | Derivation of those lines from main's surface; nothing pushed into a child's view (Task 6). |
| `crates/orca-tui/src/agent_workspace_actions.rs` | Which agent opens a conversation (Task 7). |
| `crates/orca-tui/src/ui.rs`, `background_tasks.rs`, `surface_client.rs` | Small things (Task 9). |
| `docs/subagents.md`, `tests/tui_pty_contract.rs`, the spec's run note | What changed for a person; the gate (Task 10). |

Tasks that share a file run in the order given; none runs in parallel with another.

---

### Task 1: Run the probes the plan relies on

Nothing is committed in this task. It answers spec items 5 and 6 under "To verify first" and the questions the research could not answer by reading, and it stops the plan where an answer contradicts a later task.

**Files:**
- Create: `.superpowers/sdd/2026-11-26-live-subagent-conversations-stage-2/task-1-report.md` (git-ignored); frames and saved homes under `/tmp/orca-stage2/p<N>/`
- Read: the research file's section "Verify first: probes" (P1–P7) and "Where I was unsure"; `/tmp/orca-stage1/t10/drive.py`; `tests/tui_pty_contract.rs:551-773` (how the pty tests start the TUI with the mock provider and which mock prompts exist); `/Users/bytedance/.claude/projects/-Users-bytedance-Documents-GitHub-blade-deepseek/memory/feedback_verify_tui_in_real_terminal.md`

**Interfaces:**
- Produces: answers A1–A9 below, each with the frame or file that shows it. Later tasks name the answer they depend on.

- [ ] **Step 1: Build and a first check.** `cargo build -p orca` (the binary the pty tests use; say which path). Start the TUI under the mock provider in a pty (fresh `ORCA_HOME`, a temp workspace), with a prompt that starts one kept agent that streams until released (`subagent async mock_stream_release_marker <file>`). **A1:** does selecting the running agent open its live conversation (not the Agents panel)? If the mock cannot open a live view, find out why (`tui_click_on_a_running_background_agent_opens_its_transcript`, `tests/tui_pty_contract.rs:717`, expects the panel) and say what a probe needs instead; the real key is the fallback for this one answer.
- [ ] **Step 2: P1, a finished agent and a resume (spec item 5).** Two agents in one batch; release; both rows end; quit. Record: each child's session file under `sessions/` (path, `session.meta.parent_id`, message count, `session.completed`); in the task registry file, each record's `subagent_child_thread_id` and whether it has continuation fields. Then `--resume latest`: **A2:** main's session opens (not a child's). `/agents`, select a finished agent: **A3:** what is shown (its conversation from the saved session, a checkpoint, or "unavailable")?
- [ ] **Step 3: P2, the facts on main's surface after a resume (spec item 6).** In the resumed TUI of step 2: **A4:** do the dock and the Agents panel show both agents ended? From main's saved session, the last snapshot record: does it hold the two subagents with a terminal status and `child_thread_id`, and their tasks with `completed_at`? **A5:** are the main-session tasks of past turns (with `started_at`/`completed_at`) and the operation history kept there, after 3 turns and after the resume? (Task 6 places lines by them.)
- [ ] **Step 4: P3, Shift+Down in an agent's view.** Batch of two, running. Open A from the dock by click; from A's view open B by click; Shift+Down twice, a frame after each. Repeat opening by Shift+Down and Enter. **A6:** the exact sequence after which a press marks no row, or "not reproduced" with the sequences tried (at least the two above and one with the composer holding text).
- [ ] **Step 5: P4, the agent list when main ends first.** Find a mock prompt whose parent turn ends while its agent still runs (read `crates/orca-provider/src/lib.rs` around the mock prompts; under stage 1 a main turn waits for its agents, so this may only be reachable by a wake turn or not at all). **A7:** is "main idle, a kept agent running" reachable; if so, what do the dock and the Agents panel show in main's view and in the agent's view over 30 s?
- [ ] **Step 6: P5, main's streamed text across a visit.** Main streams slowly (`mock_stream_delay_ms`) with a kept agent running; open the agent mid-stream; Esc back. **A8:** what of main is on screen right after the return: the text streamed before the visit, the text streamed during it, the running row, the `Delegating` line?
- [ ] **Step 7: P6 and P7.** P6: one kept agent running; `kill -9` the TUI; resume. **A9a:** the row (running, spinner?), `s` on it, the quit line, and the record's status and error in the registry file. P7: queue a slow message while main runs so the runtime starts that turn itself; quit during it. **A9b:** how long the process lives after the last Ctrl+C and what the session file records for that turn.
- [ ] **Step 8: Report.** One section per answer with the evidence. Then a list "what this changes in the plan": for each of Tasks 2–10, "as planned" or what must change. The spec's fallbacks apply: if item 5 fails, Task 7 shows the checkpoint transcript after a resume; if item 6 fails (A4 or A5 says the facts are not there after a resume), stop and tell the controller before Task 6. Delete every temporary copy of a key and list the paths.

---

### Task 2: The record names the agent's thread from the start

**Files:**
- Modify: `crates/orca-runtime/src/agent_controller.rs:349-366` (`AgentController::launch`, right after `start_agent_thread`)
- Test: `crates/orca-runtime/src/runtime_host.rs` tests (beside `a_person_can_stop_one_kept_subagent`, ~`:34757`)

**Interfaces:**
- Consumes: `TaskRegistry::bind_subagent_thread(&agent_id, &thread_id)` (`tasks.rs:4748-4755`; idempotent for the same id, refuses a different one).
- Produces: a kept agent's registry record has `subagent_child_thread_id` from the moment its thread exists, not from its exit. Task 7 (the saved session is found by this id) and `stop_kept_subagents`' lookup read it.

Why: research A3. Today the bind happens when `launch` returns, which for a kept agent is when it has ended.

- [ ] **Step 1: The failing test.**

```rust
    #[test]
    fn a_running_kept_subagents_record_names_its_thread() {
        // Keeping host; parent turn "subagent async mock_stream_release_marker <file>".
        // Wait until the parent's surface shows the row Running with a child thread id.
        // Then, while the agent still runs:
        //   the registry record of that task has subagent_child_thread_id == that id.
        // Release the marker; after the end the record still names the same id.
    }
```

- [ ] **Step 2: Run it and see it fail** (`cargo test -p orca-runtime --lib a_running_kept_subagents_record_names_its_thread`): the record's id is `None` while running.
- [ ] **Step 3: Bind at launch.** After the child's thread is started, bind it to the record under the registry id the request carries. Trace that the id in hand is the registry task id on every launch path that reaches this line (a fresh launch, a recovered queued launch); where it is not, bind only when it is and say so. The later bind in `runtime_subagent_call.rs` (~`:1378`) stays. A bind that is refused is not fatal to the launch: log as its neighbours log and go on.
- [ ] **Step 4: Run.** The new test; `cargo test -p orca-runtime --lib kept_subagent 2>&1 | tail -5`; `cargo test --test subagent_contract 2>&1 | tail -3`.
- [ ] **Step 5: Validators, fmt, commit.** `fix(runtime): a running subagent's record names its thread`

---

### Task 3: An agent that is gone is not shown running

**Files:**
- Modify: `crates/orca-runtime/src/runtime_actor/thread_actor_generation.rs` (beside `settle_stranded_detached_subagents`, `:1940-1996`)
- Modify: `crates/orca-runtime/src/runtime_host.rs` (call sites: `drain_subagent_relays_while_idle`, ~`:18157-18194`, before both early returns; and once when a thread is opened or resumed, after the registry's recovery)
- Maybe: `crates/orca-runtime/src/runtime_surface/commit.rs` (~`:6729-6756`), `reducer.rs` (~`:1814-1821`) if the permit for a stopped generation does not admit the settling patch
- Test: `runtime_host.rs` tests

**Interfaces:**
- Consumes: `settled_subagent_batch(task, subagent, &record, continuation, settled_at)`, `settled_subagent_record`, `SUBAGENT_SETTLE_GRACE` (all used by the detached settling).
- Produces: `settle_stranded_kept_subagents` — for each row of the thread's surface that is `Running`, owned by `Generation{fence}` where that generation is not the actor's running one, and whose registry record has ended (or does not exist), commit the row's end from the record. Idempotent.

Why: research D. A kept agent's end is sent to its parent's surface once. If it is lost (the process was killed, the quit's wait ran out, the mailbox was full), the registry record ends at the next open (`mark_interrupted_if_active` → `Failed`, "execution remains indeterminate"), but the row stays running for good: spinner, `1 active`, counted on the quit line, `s` refused. Task 1's A9a shows it.

Behaviour:
- Status from the record: completed → completed; failed → failed with the record's error; blocked → blocked; stopped or cancelled → the row ends as a cancelled/stopped agent does today when its end arrives in time (read what `Subagent::Completed` carries for each and produce the same pair of task status and subagent status; say which pair in the report).
- A row whose generation is the running one is never touched (its agent may still report).
- A row whose record is still running in this process (a kept agent that is alive: `is_running_kept_subagent` or the equivalent) is never touched.
- Other hosts: only kept rows are Generation-owned and outlive their generation, so no host flag is needed; if reading shows another kind of row that fits the filter, gate on the host and say so.

- [ ] **Step 1: Failing tests.**

```rust
    #[test]
    fn an_agent_whose_end_was_lost_is_settled_from_its_record_when_idle() {
        // Keeping host. Parent turn starts a kept agent; the agent's end is kept from the
        // surface (use the forced-order mock from the between-stages work or end the record
        // directly in the registry while swallowing the publisher's terminal — say which).
        // Parent's generation has ended; the row is still Running.
        // Run the idle drain (or wait for it): the row is no longer Running, its status
        // matches the record, the dock has no active row, and a second drain commits nothing
        // (cursor unchanged).
    }

    #[test]
    fn a_resumed_thread_settles_the_agents_it_lost() {
        // A saved session whose surface has a kept row Running and whose registry record is
        // Failed with the indeterminate error (what kill -9 leaves; build it by running a
        // host, dropping it without settling, and opening a new host on the same home).
        // Open the thread: the row is Failed, carries the record's error, and is not counted
        // as running.
    }

    #[test]
    fn a_live_kept_agent_of_an_ended_generation_is_left_alone() {
        // The state stage 1's fix E covers: generation stopped, agent still unwinding.
        // The settling does nothing; the agent's own end then lands once.
    }
```

- [ ] **Step 2: See them fail** for the reason given (row stays `Running`).
- [ ] **Step 3: Implement** `settle_stranded_kept_subagents` and the two calls. If the stopped-generation permit refuses the patch, extend that arm for exactly this patch and say what else it now admits.
- [ ] **Step 4: Run.** The three tests; the detached settling's tests (`cargo test -p orca-runtime --lib settle 2>&1 | tail -5`); stage 1's kept tests (`kept_subagent`, `an_agent_`); `cargo test -p orca-runtime --test runtime_surface_interaction -- --test-threads=1 2>&1 | tail -3`.
- [ ] **Step 5: Validators, fmt, clippy count, commit.** `fix(runtime): a kept subagent whose end was lost is settled from its record`

---

### Task 4: Returning to main shows main as it stands

**Files:**
- Modify: `crates/orca-tui/src/protocol.rs:263-266` (`TurnStarted`), `:391-395` (a new event beside `HistoryLoaded`)
- Modify: `crates/orca-tui/src/hosted_session.rs:552-574` (`project_hosted_thread_attached`), `hosted_child.rs:506-532` (`project_hosted_child_attached`), `:774-846` (`return_to_parent`), `:256-262` (the child bridge's hydration), `hosted_side.rs` (the three callers keep their label)
- Modify: `crates/orca-tui/src/state_reducer.rs:303-319` (`HistoryLoaded`), `types.rs:866-876` (`note_main_behind_child`), `:1780-1805` (`enter_running`, `set_status`), `surface_projection.rs:1128-1140` (`hydrate_open_streams`)
- Test: `state_integration_tests.rs`, `hosted_session.rs` and `hosted_child.rs` tests, `tests/tui_pty_contract.rs`

**Interfaces:**
- Produces:
  - `TuiEvent::ConversationReprojected { messages: Vec<ChatMessage>, plan: Option<(Option<String>, Vec<PlanItem>)> }` — replaces the conversation on screen with one built from a surface snapshot of a conversation that may be running. The reducer replaces the messages and the plan; it adds no label and does not touch the status. `HistoryLoaded` and its other senders are unchanged.
  - `TuiEvent::TurnStarted { turn: u32, task: Option<TuiTaskLifecycle>, started_at_ms: Option<u64> }` — `Some(ms)` (wall clock) when the turn began before this event was made; the running clock then counts from it. Every existing sender passes `None`.
  - `project_hosted_thread_attached` takes a `label: Option<&str>`: side conversations pass their label and keep `HistoryLoaded`; the return to main and the child focus pass `None` and send `ConversationReprojected`.

Why: research C0. A return rebuilds main as a loaded history: the reducer sets the status to `Idle`, appends the side conversation's label, and nothing tells it main's turn is running. That one path is defects 1, 2, 4 and 6 of T7, and A8 of Task 1 shows what else it drops.

Behaviour:
1. After Esc from an agent's view while main's turn runs: main's status is `Running`, the row reads `Running <N>s · … · Esc interrupt` with N counted from the turn's real start, main's open assistant text is there (what was streamed before and during the visit), and `Inherited conversation context.` is nowhere. Main idle → idle, as today.
2. An agent's view never shows `Inherited conversation context.`.
3. Opening a sibling from an agent's view keeps `Main is running · Esc return` under it. `note_main_behind_child` takes main's state from main's snapshot (an open foreground operation → running; a pending approval or question → needs approval / needs input), not from the status of whatever was on screen.
4. An agent's `Running <N>s` counts from the agent's start (its row's `started_at_ms`), whenever and however often it is opened.
5. Side conversations (`/btw` and the like) are unchanged, label included.

- [ ] **Step 1: Failing state-level tests** (in `state_integration_tests.rs`, with the event sequences the real paths send — take them from `return_to_parent` and `focus_child`, not from guessing):

```rust
#[test]
fn returning_from_an_agent_keeps_a_running_main_running() {
    // Main: TurnStarted, some assistant text, a kept agent row Running.
    // Focus the agent (ChildProjectionReset + its history event + ChildFocusChanged).
    // Return with main's snapshot having an open foreground operation that began 40 s ago.
    // Then: status == Running; the running row's seconds >= 40; main's assistant text is the
    // snapshot's; no System message reads "Inherited conversation context.";
    // main_behind_child == None.
}

#[test]
fn an_agents_view_has_no_inherited_context_line() { /* focus only; no such System message */ }

#[test]
fn opening_a_sibling_keeps_main_shown_as_running() {
    // Main running; focus agent A; then the sibling sequence for agent B (return + focus).
    // main_behind_child == Some(Running) and the line under the conversation draws.
}

#[test]
fn an_agents_timer_counts_from_its_start_however_often_it_is_opened() {
    // Row started 30 s ago. Open, return, open again: the running row reads >= 30 s both times.
}

#[test]
fn a_side_conversation_keeps_its_label() { /* the existing behaviour, pinned */ }
```

- [ ] **Step 2: See them fail**, each for its reason.
- [ ] **Step 3: The events and the reducer.** Add `ConversationReprojected` and `started_at_ms`. `enter_running` with `Some(ms)` sets the clock to now minus the elapsed wall time (saturating; a start in the future counts from now).
- [ ] **Step 4: The return.** `return_to_parent` builds a `TuiSurfaceProjection` from main's snapshot as the child bridge does for a child, sends `SessionProjectionReset`, `ConversationReprojected`, then `hydrate_open_streams()`'s events with the turn's start taken from the snapshot (the main-session task of the running turn; if the snapshot does not carry a start for it, the operation's; say which field). The order with the release of deferred parent events and `ChildFocusChanged{None}` stays as it is unless a test shows it must move.
- [ ] **Step 5: The child focus.** No label; the hydrated `TurnStarted` carries the row's start.
- [ ] **Step 6: Run.** The five tests; `cargo test -p orca-tui --lib hosted_child hosted_session state_integration 2>&1 | tail -6`; the existing order tests (`hosted_session_attached_projection_resets_before_history`, `hosted_child_projection_sets_target_before_merging_child_tasks`) updated to the new event where they pinned the label, and listed.
- [ ] **Step 7: A pty contract test** `tui_return_from_an_agent_keeps_the_running_main` in `tests/tui_pty_contract.rs`: main runs slowly with a kept agent held by a marker; open the agent; Esc; the screen has `Esc interrupt` on main's running row and has no `Inherited conversation context.`. Release the marker so the test ends.
- [ ] **Step 8: Validators, fmt, clippy count, commit.** `fix(tui): returning from an agent shows main as it stands`

---

### Task 5: From an agent's view, actions on main's tasks reach main

**Files:**
- Modify: `crates/orca-tui/src/hosted_view.rs:67-71` (`MainBinding`), `:200-248` (adoption)
- Modify: `crates/orca-tui/src/operation_controller.rs:367-401` (`stop_background_task`, `read_task_transcript`), `:693-708` (`mcp_servers`), `:746-772` (`shutdown`), `:199-254` (`interrupt_current`)
- Modify: `crates/orca-tui/src/hosted_controller.rs:445-478` (the refusal), `:1166-1178` (the transcript arm), `action_dispatcher.rs:487-509`
- Modify: `crates/orca-tui/src/global_actions.rs:27-69`, `:109-131`
- Maybe: `scripts/validate-runtime-surface-contract.mjs` — only the baseline line the controller approves after seeing what moved
- Test: unit tests in the same files; `state_integration_tests.rs`

**Interfaces:**
- Produces: `OperationController::main_runtime()` (or the name its neighbours suggest) — the thread of the main conversation, whatever conversation is on screen and whichever thread the controller has adopted. Tasks 7 and 9 use it.

Why: research C8, E, F2, F6. Once the controller has adopted an agent's thread, `queue_runtime` is that thread: `/agents` reads, `s` and `/mcp` ask the agent's surface about things that live on main's, `s` is refused with the side-conversation sentence, and a main turn the runtime started by itself is not ended at quit.

Behaviour:
1. In an agent's view, opening a transcript in the Agents panel, `s` on a row, and `/mcp` act on main's thread and work as they do from main's view. (`/mcp` in an agent's view shows main's servers.)
2. The commands that stay refused in an agent's view (new session, model, compact, goals, rewind, …; the list at `hosted_controller.rs:445-472` minus `StopTask`) are refused with exactly: `this command is unavailable in an agent's conversation; return to main first`. A side conversation keeps its own sentence.
3. Ctrl+C (and SIGTERM/SIGHUP's quit) in an agent's view treats main as running when `main_behind_child` says running, needs approval or needs input: the first press sends the interrupt, and the interrupt reaches main's turn — the one the TUI started (as today) or one main's thread started by itself. Esc is untouched.
4. At quit, `shutdown` ends a turn main's thread started by itself (a queued message's turn, a wake for finished agents) before the thread is shut down, as it ends one the TUI started.
5. If a validator counts a new mutation site, stop: tell the controller the site and the count, and wait for the ruling. Do not edit `scripts/` before it.

- [ ] **Step 1: Failing unit tests** (with a view bound to a child: `HostedView::show_child_for_test` and the fixtures of `a_return_with_a_view_bound_does_not_wait_behind_the_controller`, `action_dispatcher.rs:914`):

```rust
#[test] fn a_transcript_read_from_an_agents_view_asks_mains_thread() { /* returns main's record, not NotFound */ }
#[test] fn a_stop_from_an_agents_view_reaches_mains_task() { /* the stop is sent on main's surface; no refusal */ }
#[test] fn mcp_from_an_agents_view_lists_mains_servers() { }
#[test] fn a_refused_command_in_an_agents_view_says_so_in_its_own_words() { /* the exact sentence; side conversation keeps the old one */ }
#[test] fn ctrl_c_in_an_agents_view_interrupts_a_running_main() {
    // status Idle (a finished agent on screen), main_behind_child Running:
    // the first Ctrl+C sends Interrupt and prints no quit line; with main idle it prints the quit line.
}
#[test] fn shutdown_ends_a_turn_main_started_by_itself() {
    // Main's thread runs a queued slow prompt it started itself; a child is adopted.
    // shutdown(): the turn ends cancelled before the thread is shut down.
}
```

- [ ] **Step 2: See them fail.**
- [ ] **Step 3: Implement** the accessor and move the five call sites to it; the refusal text; the key layer; `interrupt_current` and `shutdown` reaching a self-started turn of main's thread.
- [ ] **Step 4: Run.** The tests; `cargo test -p orca-tui --lib operation_controller action_dispatcher global_actions hosted_view 2>&1 | tail -6`; pty: `cargo test --test tui_pty_contract tui_sigterm_during_a_turn_interrupts_it tui_cancel_returns_to_idle 2>&1 | tail -4`.
- [ ] **Step 5: Validators** (see behaviour 5), fmt, clippy count, commit. `fix(tui): actions taken in an agent's view reach main's tasks, and Ctrl+C there stops main`

---

### Task 6: Main's lines about its agents are derived, not pushed

**Files:**
- Create: `crates/orca-tui/src/subagent_lines.rs`
- Modify: `crates/orca-tui/src/surface_projection.rs:706-804` (`history_messages_from_surface_snapshot`, `history_messages_from_surface_items`)
- Modify: `crates/orca-tui/src/workflow_panel.rs:539-657` (`apply_workflow_tasks_update`, `emit_subagent_terminal_notices`), `state_reducer.rs:1425-1453` (`sync_subagent_transcript_messages`)
- Test: unit tests in `subagent_lines.rs` and `surface_projection.rs`; `state_integration_tests.rs`; `tests/tui_pty_contract.rs`

**Interfaces:**
- Consumes: Task 4's `ConversationReprojected` (the rebuilt history is where the derived lines appear); Task 1's A4 and A5.
- Produces, in `subagent_lines.rs`:
  - `pub(crate) fn delegating_line(batch_size: usize) -> String` — the existing wording, unchanged (`Delegating to 1 agent`, `Delegating to N agents`; copy it from `sync_subagent_transcript_messages`).
  - `pub(crate) fn agent_ended_line(status: TaskStatus, name: &str, error: Option<&str>) -> Option<String>` — `Agent completed: <name>`, `Agent failed: <error or name>`, `Agent blocked: <error or name>`, `Agent cancelled: <name>` exactly as today, and new: `Agent stopped: <name>`. `None` for a status that is not an end.
  - a function that, given main's snapshot (items, tasks, subagents), returns the lines with the position each takes among the items. Both the rebuilt path and the live path call the two wording functions, so the text cannot differ.

Why: research B. The lines are pushed into whatever conversation is on screen when a task list changes: an agent's own `Agent completed` shows in its own view; a rebuild of main (return, resume) has none of them; a person's `s` makes no line at all.

Behaviour:
1. Whenever main's history is built from its surface (first open, return, resume), it contains one `Delegating to N agents` line per batch and one `Agent <status>: <name>` line per ended agent, including `stopped`.
2. Place: `Delegating…` directly after the item of main's turn that started the batch; an agent's ended line after the last item of main that precedes the agent's end in time, and before main's next item. If A5 says the snapshot does not keep what this needs, fall back to "after the last item of the operation that owns the agent", and say so in the report.
3. Live, with main on screen: the same line is appended once when the batch starts and when an agent ends (as today, plus `stopped`).
4. Live, with an agent on screen: nothing is appended to the agent's conversation. The lines are in main when the person returns (by 1).
5. No line is shown twice: not after a return, not after a resume, not when a late update repeats a status.
6. The model sees none of this: these are TUI lines, as today.

- [ ] **Step 1: Failing unit tests** for the wording (every status, the singular and plural) and for the derivation: a snapshot with two turns, a batch of two started in turn 1, one agent completed during turn 1, one stopped after turn 2 began → the lines and their positions.
- [ ] **Step 2: Failing state-level tests.**

```rust
#[test] fn an_agent_that_ends_on_screen_adds_nothing_to_its_own_conversation() { }
#[test] fn the_return_to_main_shows_what_its_agents_did_while_away() {
    // Agent ends while its view is open; return: main has "Agent completed: <name>" once,
    // and "Delegating to 1 agent" once, in place.
}
#[test] fn a_stopped_agent_is_announced_in_main() { /* "Agent stopped: <name>", live and rebuilt */ }
#[test] fn a_rebuild_does_not_repeat_a_line() { /* return twice; one line each */ }
```

- [ ] **Step 3: See them fail.**
- [ ] **Step 4: Implement** `subagent_lines.rs`, the derivation in the history builders, and the live path through the same wording functions with the "main is on screen" rule. Remove what the old pushers no longer need (`announced_*` sets stay only if the live path still needs them to avoid a repeat; say which).
- [ ] **Step 5: Run.** The tests; the neighbours the research lists (`surface_projection_announces_group_once_without_inline_child_messages`, `task_snapshot_keeps_running_subagent_out_of_main_conversation`, `child_projection_reset_preserves_parent_agent_dock`, `parent_task_updates_refresh_agent_dock_while_child_is_focused`, `a_wake_for_finished_agents_keeps_the_answer_before_it`) pass, edited only where they pinned a pushed line, and listed.
- [ ] **Step 6: A pty contract test** `tui_resume_shows_what_the_agents_did`: a batch of mock agents ends; quit; resume; the screen has `Delegating to` and `Agent completed:` for it.
- [ ] **Step 7: Validators, fmt, clippy count, commit.** `fix(tui): main's lines about its agents come from its surface, after a return and a resume too`

---

### Task 7: A finished agent opens its conversation

**Files:**
- Modify: `crates/orca-tui/src/agent_workspace_actions.rs:13-51` (`open_agent_task`, `opens_live_conversation`)
- Modify: `crates/orca-tui/src/hosted_child.rs:596-772` (`focus_child`), `:871-913` (`child_binding`), `:65-179` (`HostedChildEventBridge::prepare`)
- Modify: `crates/orca-tui/src/hosted_view.rs:130-248` (focus state, adoption), `action_dispatcher.rs:17-23`, `:224-227` (the guard), `operation_controller.rs:422-439`
- Maybe: `crates/orca-runtime/src/runtime_surface/host.rs:76-80` (`load_saved_session` is already exported; no change expected)
- Test: unit tests in those files; a runtime test in `crates/orca-runtime`; `state_integration_tests.rs`; `tests/tui_pty_contract.rs`

**Interfaces:**
- Consumes: Task 2 (the record and the row name the thread), Task 4 (`ConversationReprojected`), Task 5 (`main_runtime`; a read-only focus is not adopted), Task 1's A2, A3.
- Produces: a focus that is read-only (`HostedChildFocus` says so; the name is the implementer's). Stage 3 will rely on "the controller never adopts an agent's thread" becoming true for every focus; here it becomes true for the read-only one.

Why: research A. A finished agent never opens a conversation: the open requires an active row, `child_binding` refuses a row that is not running, and after a resume no agent thread is live.

Behaviour (spec, "A finished agent" and T5):
1. Selecting a finished agent that has a thread (dock or Agents panel, Enter or click) opens its conversation: the task it was given, its text, tool calls and results, as they were when it ended.
   - Thread live in this process: from its surface snapshot. No bridge is started and the controller does not adopt the thread.
   - Thread not live (after a resume): from its saved session, loaded by the thread id without taking ownership (`RuntimeSurfaceHostHandle::load_saved_session`), shown the same way.
   - If that load fails: the checkpoint transcript (what `ReadTaskTranscript` returns) is shown in the same place, with the first line `This agent's conversation could not be loaded. Showing its last checkpoint.`
2. The view says it is read-only: its last line is `This agent has finished. Its conversation is read-only.` The row under the conversation still says what main is doing and `Esc return`; no `Running` row is drawn for the agent.
3. A message submitted there is handed back to the composer with exactly: `this agent has finished: return to the main conversation to send a message`.
4. If the agent on screen finishes, the view stays on it: the bridge ends, the line of 2 is appended once, and a message then gets the sentence of 3 (today it gets the running agent's sentence).
5. A finished agent with no thread, and every snapshot agent, opens the Agents panel as today.
6. Esc returns to main as from a running agent's view (Task 4's return).

- [ ] **Step 1: Failing tests.**

```rust
// agent_workspace_actions.rs
#[test] fn a_finished_agent_with_a_thread_opens_its_conversation() { /* FocusChildThread, not the panel */ }
#[test] fn a_finished_agent_without_a_thread_opens_the_panel() { }

// hosted_child.rs
#[test] fn a_finished_agent_is_bound_read_only_from_its_snapshot() {
    // child_binding accepts an ended row whose thread resolves; no bridge thread; the view's focus is read-only;
    // the controller's thread is still main's.
}
#[test] fn a_finished_agent_after_a_resume_is_read_from_its_saved_session() {
    // The thread does not resolve live; the saved session is loaded by thread id; messages shown.
}
#[test] fn a_finished_agent_whose_session_cannot_be_loaded_shows_its_checkpoint() { /* the first line, exactly */ }

// state_integration_tests.rs
#[test] fn a_finished_agents_view_says_it_is_read_only_and_hands_a_message_back() { /* both strings, exactly, once */ }
#[test] fn an_agent_that_finishes_on_screen_becomes_read_only() { }
```

A runtime test in `crates/orca-runtime` (where the kept-agent tests are): `a_finished_agents_session_is_read_without_taking_its_thread` — a kept agent ends; with the parent's thread still owned and running, its session is loaded by thread id; `meta.parent_id` is the parent; the parent's next turn runs normally afterwards.

- [ ] **Step 2: See them fail.**
- [ ] **Step 3: Implement**, in this order: the open decision; `child_binding` and the read-only focus (live thread); the saved-session path; the checkpoint fallback; the guard's third case; the on-screen finish.
- [ ] **Step 4: Run.** The tests; `cargo test -p orca-tui --lib hosted_child hosted_view agent_workspace_actions action_dispatcher 2>&1 | tail -6`; stage 1's focus tests named in research A6 pass unedited.
- [ ] **Step 5: A pty contract test** `tui_a_finished_agent_opens_its_conversation_now_and_after_a_resume`: a mock agent ends; open it; its reply text and the read-only line are on screen; type a message and Enter: the finished sentence; Esc; quit; resume; open it again: the same reply text and line.
- [ ] **Step 6: Validators, fmt, clippy count, commit.** `feat(tui): a finished agent opens its conversation, read-only, after a resume too`

---

### Task 8: The marker and the list in an agent's view

**Files:**
- Modify: as Task 1's A6 and A7 show; expected `crates/orca-tui/src/types.rs:1561-1622` (`reset_session_projection` clears the dock selection), `state_reducer.rs` (`ChildProjectionReset`), `key_event_actions.rs:1708-1724`, `hosted_controller.rs:417-424` (the idle poll projects only the adopted thread)
- Test: `state_integration_tests.rs`; a controller-level test if A7 is reachable

**Interfaces:**
- Consumes: Task 1's A6 (the sequence that loses the marker) and A7 (whether "main idle, an agent running" is reachable and what the list does); Task 4.

Behaviour:
1. In an agent's view the dock marks (`›`) the agent on screen from the moment the view opens, and Shift+↑↓ moves the mark from there; a press always marks a row when there are rows. Returning to main clears the mark as today.
2. The list of agents does not go while any agent runs, in main's view and in an agent's, whether or not main's turn has ended. If A7 found the state unreachable, pin that with a test of the reason (main waits for its agents) and build nothing for it.
3. If A6 is "not reproduced" and reading still finds no path, build 1's "the open marks the agent on screen" (it is what a person needs either way), and say that the 2026-11-24 sighting is not explained.

- [ ] **Step 1: Failing state-level tests** from A6's sequence: `the_agent_on_screen_is_marked_in_the_dock`, `shift_down_in_an_agents_view_moves_the_mark` (after a sibling open by click and by Enter), and for A7 `the_agent_list_stays_while_an_agent_runs_after_main_ended` or the test of the reason.
- [ ] **Step 2: See them fail. Step 3: Implement. Step 4: Run** (`cargo test -p orca-tui --lib state_integration workflow_panel 2>&1 | tail -5`).
- [ ] **Step 5: Validators, fmt, clippy count, commit.** `fix(tui): the agent on screen is marked in the list, and the list stays while agents run`

---

### Task 9: Small things

One commit each where the file sets differ; all in one task because none needs its own review.

**Files and changes:**
1. `crates/orca-tui/src/ui.rs:4444-4451` (`task_status_color`): `Cancelled` takes the muted colour `Stopped` has. A cancelled agent's row (dock and Agents panel) shows no error line: the task's `agent cancelled` error is not drawn for a `Cancelled` row. Test beside `ui.rs:11207` and `:14022`.
2. `crates/orca-runtime/src/runtime_actor/thread_actor_generation.rs:345-347`: the word that ends an agent's activity history follows the `Completed` payload's status: `completed`, `failed`, `cancelled`, `blocked`. This changes the word in every host; if a contract test pins `completed` for an agent that did not complete, correct the test and name it. Unit test on that function for each status.
3. `crates/orca-tui/src/surface_client.rs` (`stop_subagent_until_not_stale`, `stop_task`), `background_tasks.rs:98-101`, `action_dispatcher.rs:463`: a stop that finds the agent already ended prints `<label> had already finished.` (the label quoted as `Stopping <label>.` quotes it) instead of `Stopping <label>.`. `stop_task`'s text is pinned by SHA-256 in `scripts/validate-runtime-surface-contract.mjs` (`BASELINE_HARMLESS_ASSOCIATED_FUNCTION_SHA256`): this change moves that one hash; update that one line and show the scripts diff is that line only.
4. Comments: `crates/orca-runtime/src/runtime_actor/thread_actor_operation.rs` ~`:9584` drops "or a finite budget" (stage 1's final wave found a config budget does not make a hosted turn's child threadless); `crates/orca-tui/src/hosted_controller.rs:1264-1272` is rewrapped under 100 columns and, after Task 5, no longer says the keys do not send the interrupt.

- [ ] Each: failing test where there is behaviour, change, run, validators, fmt, commit (`fix(tui): …`, `fix(runtime): …`, `docs(…): …`).

---

### Task 10: What a person is told, and the terminal

**Files:**
- Modify: `docs/subagents.md` ("Subagents Started From The TUI"): a finished agent's conversation (read-only, the two sentences, after a resume); what the main conversation shows about its agents; Ctrl+C in an agent's view; an agent that was lost is shown failed after a resume.
- Modify: the spec, a run note for stage 2 after the one of 2026-11-25 (what was run, on which commit, what held; what was seen and left to stage 3).
- Create: `.superpowers/sdd/2026-11-26-live-subagent-conversations-stage-2/task-10-report.md`; frames under `/tmp/orca-stage2/run-*/`

- [ ] **Step 1: The suites.** `cargo test --workspace --no-fail-fast` once, logged, with a timeout; every failure run again alone; each that still fails is checked against `17de2db8` (and against the ledger's list of known ones). A failure this stage caused fails the gate unless the test pinned behaviour a task changed on purpose (name the task, correct the test). Clippy count; validators.
- [ ] **Step 2: The terminal, with the real key** (auto-edit, two agents that each write a file, run a command and write an essay of 3000 words; a 150x50 pty read with `pyte`). The spec's run for stage 2, each observation with its frame:
  1. Open an agent while all three run; Esc back: main's running row with its real seconds, its streamed text, `Delegating to` lines, no `Inherited conversation context.`; main goes on live.
  2. From an agent's view open its sibling: `Main is running · Esc return` stays; the timer of each agent does not restart; the dock marks the agent on screen; Shift+↑↓ moves the mark.
  3. Let one agent finish while its view is open: the read-only line appears once, no `Agent completed` in its own view; a message is handed back with the finished sentence; Esc: main has `Agent completed: …` once.
  4. `s` on the other from the Agents panel opened in an agent's view: it works; main has `Agent stopped: …`.
  5. When main has replied: open a finished agent from the dock or the panel: its whole conversation, read-only.
  6. Ctrl+C in an agent's view while main runs (a second run): main is interrupted, its agents end, then the quit line, then quit.
  7. `--resume latest`: main's lines about its agents are there; open a finished agent: the same conversation, read-only; no row is shown running.
  8. `kill -9` with an agent running (a third run); resume: the agent's row is failed, not running; the quit line does not count it.
- [ ] **Step 3: Decide.** Every observation of step 2 holds, or the gate fails and the report says which and why. Things seen that belong to stage 3 or 4 are recorded as seen.
- [ ] **Step 4: Commit** the documents (`docs: what stage 2 changes for a person`), and report: the gate's verdict, the suites, what was not verified.

---

## Self-review of this plan against the spec

| Spec | Task |
|---|---|
| T5 finished agents (live thread; saved session after resume; checkpoint fallback; read-only, says so; on-screen finish) | 7 (needs 2, 4, 5) |
| T6 notices derived when main's surface is projected | 6 |
| T7.1 running row gone after a visit; T7.2 `Inherited conversation context.`; T7.4 `Main is running` gone after a sibling; T7.6 timer restarts | 4 |
| T7.3 `Delegating to N agents` lost by a rebuild | 6 |
| T7.5 Shift+Down marks no row; T7.7 the list goes while agents run | 8 (after probes) |
| T7.8 `/agents` from a child's view: transcript not found | 5 |
| "To verify first" 5 and 6 | 1 |
| Stage 1 final review, to stage 2: I2 | 3; item 12 | 2; item 13 | 9; W1 and the key layer and the self-started turn at quit | 5; R2 | 9 |
| The stage's run in a real terminal | 10 |

Not in this stage: messages to an agent and `/stop` (stage 3: R4, T3, T4; the controller's remaining actions on a viewed agent's thread go with T3); the card (stage 4).
