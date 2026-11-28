# Watching a Subagent Live, and the Agents Card Design

- Date: 2026-11-23
- Status: its Design, To verify first and Testing sections are replaced by
  `2026-11-24-live-subagent-conversations-design.md`, written after item 1
  under "To verify first" was run in a real terminal on 2026-11-24 and
  failed in part (see there). The card's look described here still holds.
- Baseline: `main` at `11d90cee` (v0.5.23)
- Reference: `openai/codex` at `9ee4539162`, `codex-rs/tui/src/app/`

## Purpose

Today a person can open a subagent's live conversation only while the main
conversation is idle. While the main agent is running, which is when its
subagents are running too, opening one shows a checkpoint snapshot or the
last twelve activity lines instead. A subagent that has finished cannot be
opened as a conversation at all.

This change makes a subagent's conversation open the same way at any time:
the conversation view switches to it, shows what it has done, and follows it
live, as the main conversation does. A card floating over the top right of
the conversation lists the agents and is the way in and out.

## What the person sees

### The card

The card floats over the conversation; the layout underneath does not change
and the conversation is not rewrapped.

Collapsed, it is one row at the top right, shown only while the conversation
has subagents:

```
                                          ⠋ 2 agents · ✓ 3
```

Expanded, it is a bordered card about 34 columns wide and at most half the
conversation's height:

```
┌ Changes ───────────────────────┐
│ main                  +64 -3   │
│ Agents                         │
│ › Main                         │
│ ⠋ reviewer · bash cargo test   │
│ ● planner · needs approval     │
│ ✓ 3 finished                   │
│ 1 background task running      │
└────────────────────────────────┘
```

- A click on the collapsed row, or `/tasks`, expands it. Esc, or a click on
  its title row, collapses it. A click outside it does not close it. In an
  agent's conversation Esc returns to the main conversation first, as the
  Esc order has it today.
- `›` marks the conversation on screen. A click on any agent row opens that
  agent; a click on `Main` returns. Shift+Up/Down and Enter do the same from
  the keyboard, as they do in the dock today.
- `✓ 3 finished` opens to list the finished agents; each opens like a
  running one.
- Agents keep the order they were first seen in; finishing does not move
  one.
- A click inside the card never reaches the conversation under it. The
  wheel scrolls the card while the pointer is over it and its list
  overflows, and the conversation otherwise.
- The card is drawn under approvals, the slash and mention menus, dialogs
  and the image viewer. It is not drawn where the composer's image preview
  is.
- When the terminal is too narrow for the card to leave 40 columns of
  conversation visible beside it, the card is not used and the dock under
  the conversation stays as it is today.
- With the card in use, the dock keeps only the foreground line
  (`Running 12s · … · Esc interrupt`); its agent rows are in the card.

### An agent's conversation

Opening an agent replaces the conversation view with that agent's
conversation: its history first, then what it does as it does it. The
status line names it (`Main › reviewer`). Esc, or `Main` in the card,
returns to the main conversation, which is rebuilt as it stands and goes on
live if the main agent is still running.

- A finished agent opens the same view, read-only, and says so.
- If the agent on screen finishes, the view stays on it and becomes
  read-only; it does not jump back.
- If the main agent needs an approval or an answer while an agent is on
  screen, the status line and the card's `Main` row say so; the request is
  shown on return, as it is today after a side conversation.
- An agent that does not run on a thread of this process has no
  conversation to attach to. It opens the checkpoint transcript in the
  Agents panel, as today, and its card row is marked `snapshot`.

### The composer

Stage 1: in an agent's conversation, the composer works as it does today
when the main conversation is idle. While the main agent is running, a
message sent there is handed back to the composer with a line saying why,
and a row under the conversation says main is running. The same holds the
other way round: while the controller still runs a turn on an agent that
is no longer on screen, a message sent from the main conversation is
handed back to the composer with a line saying why.

Stage 3: the composer always sends to the conversation on screen.

## How it works today

- The layout is one vertical stack (`main_layout`, `ui.rs:378`). The dock
  (`activity_rows`, `ui.rs:5495`) lists `Main` and active subagents; hit
  targets are rectangles recorded each frame (`AgentHitArea`).
- `open_agent_task` (`agent_workspace_actions.rs:13`) sends
  `FocusChildThread` only when `opens_live_conversation` holds: the task
  has a child thread, is active, and the app is idle. Otherwise it opens
  the Agents panel on a checkpoint.
- `focus_child` (`hosted_child.rs:548`) resolves the child's live thread,
  switches the active attachment, projects the child's snapshot and starts
  `HostedChildEventBridge`, a read-only subscription that owns no operation
  control.
- `hosted_tui_controller_loop` (`hosted_controller.rs:299`) takes one
  action at a time. A submitted turn runs to its end inside
  `handle_hosted_submitted_turn` (`hosted_submission.rs:164`), holding the
  loop's `thread` and `event_tx`. A focus sent meanwhile waits for it.
- Events carry an attachment id. `AttachmentRouting`
  (`attachment_routing.rs`) is shared behind a mutex, passes only the
  active attachment's events, and keeps a hidden parent's approvals and
  questions to replay on return. Side conversations already rely on this
  with the parent running (`SideParentStatus::Running`).
- `host.resolve_live_thread` finds only threads in the host's live table.
  A child thread is a saved session whose id is the task's
  `subagent_child_thread_id` (`runtime_host.rs:2455`).

## Design

### Stage 1: viewing is observation

What is on screen and what the controller operates on become two things.

- **View target.** A shared `ViewTarget` (main, or a task id with its
  attachment and bridge) replaces the controller loop's local
  `child_focus`. Focus and return are handled by a small view worker with
  its own action channel, not by the controller loop, so they never wait
  for a turn. The dispatcher sends `FocusChildThread` and
  `ReturnToParentThread` there.
- **Focus** is `focus_child` minus the two assignments that change the
  controller's operating thread: resolve and recheck the binding, allocate
  an attachment, prepare the bridge, switch the active attachment, project
  the snapshot, start the bridge. The running turn keeps sending under the
  parent's attachment; routing withholds those events and keeps its
  interactions.
- **Return** is `return_to_parent` as it is: read the parent's snapshot,
  switch with parent events deferred, project, release.
- **Operating thread.** When the controller loop takes its next action and
  no turn is running, it adopts the view target as its operating thread,
  which gives today's behaviour for an idle app. While a turn runs the
  operating thread does not change; the renderer disables the composer in
  an agent's view for that time.
- **Finished agents.** Focus on a task whose thread is not live opens its
  saved session read-only: the session's surface snapshot is projected and
  no bridge starts. If the snapshot cannot be read, the view shows the
  checkpoint transcript in the same place; if there is none, a line saying
  the conversation is not available.
- **Entry.** `opens_live_conversation` drops its `idle` condition and its
  `is_active` condition. The test
  `a_child_the_running_turn_waits_on_opens_its_activity_right_away` is
  replaced by one that expects the focus.

### Stage 2: the card

- `render()` draws the card after the conversation and before the modal
  layers, records its rectangle in `ViewportState`, and pushes its rows'
  `AgentHitArea`s. `AgentHitTarget` gains the collapsed row, the title row
  and the finished group.
- `handle_mouse_event` tests the card before the transcript, as it does
  the recap detail.
- Rows come from `workflow_tasks()` ordered by first sight, kept in
  `AppState`; finished subagents stay in the list for the session.
- Changes shows the branch from `WorkspaceStatus` and the totals of
  `git diff --shortstat HEAD`, read with the existing 500 ms limit when a
  turn ends and when the card opens.
- `tasks_dock_expanded` becomes the card's expanded flag where the card is
  in use; `/tasks`, the Esc order and the automatic reveal on a background
  approval keep their meaning.

### Stage 3: input per conversation

A message typed in an agent's conversation goes to that agent while the
main agent runs. This needs the controller to run turns for more than one
thread at a time. It has its own design; nothing in stages 1 and 2 depends
on it.

## Taken from Codex

- Events are kept per thread whatever is on screen; switching replays the
  target and then follows it (`thread_events.rs`, `select_agent_thread` in
  `session_lifecycle.rs:696`). Orca's surface snapshot and subscription
  already give this without a buffer in the TUI.
- Three ways a thread opens: live, replay only, closed.
- First-seen order that survives finishing (`agent_navigation.rs`).
- Requests from threads not on screen are surfaced
  (`surface_pending_inactive_thread_interactive_requests`).
- Input goes to the thread on screen, with a draft per thread (stage 3).

Codex's TUI shows its agents in a split pane; the floating card is its
desktop app's. The card here follows Orca's recap detail.

## To verify first

These were read, not run. Each is the first task of its stage's plan.

1. Run on 2026-11-24 against `67bd26a8`, in a pty read with `pyte`
   (150x50), `deepseek-flash`, the main agent starting two subagents that
   write essays; four runs and one `--resume latest`. Result: failed in
   part.
   - The `subagent` tool's default children (async: the tool has no mode
     argument) run as detached `orca subagent-worker` processes. Their
     `subagent_child_thread_id` stays unset, so opening a running one shows
     the Agents panel ("No transcript checkpoint yet"), as before. The live
     view was reached only with children the model launched with
     `"mode": "sync"`, which run on threads of this process.
   - Passed: the dock lists both agents with `Running …` while main runs;
     Shift+Down marks one with `›`; Enter switches the view to that child,
     with `Main is running · send after it finishes · Esc return` under
     it; `hello` sent there is handed back to the composer with `the main
     agent is still running: return to it to send a message, or wait for
     it to finish`, and the child stays shown as running; staying on a
     child, that row goes when main finishes and the view stays on the
     finished child; a finished agent opens the Agent Transcript panel;
     `--resume latest` shows main and both agents as they ended.
   - Failed: the child's conversation shows only "Inherited conversation
     context." for its whole run (32 s watched) and never its reply. The
     child's session file has no surface commit for that turn, so the
     bridge has nothing to show; a turn the controller itself runs on the
     child does stream. After a sibling is opened from a child's view the
     `Main is running` row is gone although main still runs, and
     Shift+Down there marks no row (opened with a click).
   - Return while main runs: no paragraph doubled or lost, and the rest of
     the turn (two agent notices, the two summary lines) arrived once. But
     main's `Running … · Esc interrupt` row was gone from the return until
     the turn ended, about 70 s, while main was still running and then
     writing; the `Delegating to 2 agents in parallel` line was gone (no
     rebuild has it, resume neither); and `Inherited conversation
     context.` was appended under main. `Agent completed` notices land in
     whatever conversation is on screen, so main lacks them after return.
2. Whether a finished child's thread is still in the host's live table, and
   whether its saved session yields a surface snapshot without starting a
   thread (`RuntimeSurfaceHostHandle::load_saved_session`,
   `runtime_surface/host.rs:76`).

If 1 fails, the fallback is to poll the view channel from inside the turn's
drain loop. If 2 fails, finished agents open their checkpoint transcript in
the conversation view.

## Not in this change

- Sending to an agent while the main agent runs (stage 3).
- Finished agents as conversations: in stage 1 they open the Agents panel
  as before, until their replay has its own plan.
- A diff view opened from Changes.
- Workflow agents and background tasks as conversations.
- A split-pane layout.

## Testing

- Routing: with a parent turn running, focus passes child events and
  withholds parent ones; return releases the parent's kept approvals once.
- View worker: focus, return, focus on another child, a child that
  finishes while on screen, a stale revision, a child of another parent.
- Finished agent: opens read-only; falls back in the order above.
- Card: collapsed and expanded frames at several sizes, the narrow
  fallback, click targets, clicks not reaching the transcript, wheel,
  layering under each modal.
- In a real terminal (pty with `pyte`): the main agent starts two
  subagents; open one while all three run and watch it stream; return and
  check the main conversation; open a finished one; `--resume latest` and
  open one again. The live path and the rebuilt path are both looked at.
