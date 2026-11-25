# Live Subagent Conversations Design

- Date: 2026-11-24
- Status: direction approved in conversation; not yet reviewed as written
- Baseline: `feat/live-subagent-view` at `292eaafd` (`main` at `11d90cee`,
  v0.5.23, plus the view-switching work of the 2026-11-23 plan)
- Replaces the Design, To verify first and Testing sections of
  `2026-11-23-live-subagent-view-and-agents-card-design.md`. The card is
  carried over here unchanged in look.
- Evidence: a real-terminal run of the baseline (2026-11-24, recorded in the
  2026-11-23 spec), a throwaway spike on `spike/child-turn-on-surface`
  (`05e57034`, `812fbca1`, four runs in a pty read with `pyte`), and
  readings of Codex (`9ee4539162`), Claude Code and DeepSeek Harness
  (0.2.0-rc.2).

## Purpose

A person using the TUI opens any subagent and sees its conversation as they
see the main one: what it has done, then what it does as it does it. They
can send it a message while it runs, stop it without stopping anything
else, and return to the main conversation, which has gone on meanwhile.

The baseline has the switching but not the conversation. A subagent started
from the TUI runs as a separate process that keeps no conversation the TUI
can attach to, and the kind that does run in this process starts its turn
in a way that records nothing a view can follow. This design changes where
a TUI's subagents run and how their turn starts, and then finishes the
view.

## What the person sees

### An agent's conversation

Opening an agent replaces the conversation view with that agent's
conversation: the task it was given as the first message, then its text as
it is written, its tool calls and their results as they happen. The row
under it says what main is doing (`Main is running · Esc return`, or that
main needs an approval or an answer). Esc returns to main, which is shown
as it stands and goes on live.

Esc in an agent's conversation only returns. It never interrupts the agent
and never interrupts main.

### Sending a message to an agent

While an agent runs, the composer in its conversation sends to it, with the
keys the main conversation has while a turn runs:

- Enter queues the message. The agent takes it when its current turn ends,
  before it reports to the main agent.
- Ctrl+Enter puts the message into the running turn; the agent sees it
  before its next request. This carries text only, as it does in main; a
  message with an image is queued.

Both work while the main agent is running. Queued messages are listed above
the composer and can be edited or removed, as main's are.

### Stopping an agent

Stopping is its own action and stops that agent only: `/stop` in the
agent's conversation, or `s` on its row in the Agents panel as today. The
main agent is told the agent was stopped. Messages still queued for it are
handed back to the composer.

### A finished agent

A finished agent opens the same view, read-only, and says so. A message
sent there is handed back to the composer with a line saying the agent has
finished. If the agent on screen finishes, the view stays on it and becomes
read-only.

### The list of agents

Agents are listed in the order they were started and keep their place;
activity and finishing do not move a row. `Agent completed`, `failed`,
`stopped` notices are part of the main conversation, wherever the person
was looking when they happened, and are there after a return and after a
resume.

### Permissions

An agent never asks the person anything, and never asks the main agent.
What it may do is fixed when it starts, from the main conversation's
permission mode, rules and sandbox at that moment:

| Main's mode | The agent |
|---|---|
| full-auto (the TUI's default) | does all of it on the host, as main does |
| auto-edit | writes, runs commands and uses the network inside the sandbox, as main does |
| suggest | reads only: main would ask before each write or command, and the agent cannot ask |
| plan | reads only, as main |

Where main would be asked — a write or command in suggest, anything outside
the sandbox, a tool with an "always ask" rule — the agent's call is refused
at once and the agent goes on. It is told at its start to say in its reply
what it could not do, so the main agent, which can ask, may do it.

### Quitting

Quitting the TUI ends its agents. With agents running, the first Ctrl+C
says so (`2 agents are still running. Press Ctrl+C again to quit and stop
them.`), and quitting stops them so they are recorded as stopped.

### The card

As in the 2026-11-23 spec: a card floating over the top right of the
conversation, one row collapsed, about 34 columns expanded, listing Changes,
`Main`, the agents and a finished group; `›` marks the conversation on
screen; a click or Enter on a row opens it; the dock under the conversation
keeps only the foreground line while the card is in use, and is used as
today when the terminal is too narrow. One row changes: an agent with no
conversation is marked `snapshot`, and only such a row can say
`needs approval`.

### Agents that keep a snapshot

Four kinds of subagent cannot run on a thread of their own: those started
with isolation, custom agent types, those with a finite budget, and those
continued with `resume_from`. They are left exactly as they are today: a
separate process that finishes in the background after a quit and may ask
for a permission on its row. They open the checkpoint transcript in the
Agents panel, cannot be sent a message, and are marked `snapshot`. What is
said above about permissions and quitting is about the others.

## Three changes in behaviour

These are about subagents that get a conversation; the four kinds that keep
a snapshot, and everything under `orca exec`, are unchanged.

1. A TUI's subagents end when the TUI quits. Today they finish in the
   background.
2. A subagent of the TUI no longer asks for a permission. Today a request
   to go beyond the sandbox is shown on its row.
3. A finished subagent cannot be continued by typing in its conversation.
   Today, with main idle, a message there starts a new turn on it.

## How it works today

Established on 2026-11-24. "Run" means seen in a real terminal; the rest was
read in the code.

- The `subagent` tool has no mode argument and the parser defaults to
  `Async` (`subagent.rs:76-80`). An `Async` child whose parent has a
  persistent task registry, a fence and an activity ingress is launched as
  a detached `orca subagent-worker` (`subagent_execution.rs:817-840`). A
  TUI parent meets all three. The worker exists so that a child outlives
  the `orca exec` that started it (`tests/subagent_contract.rs:209`).
- The worker hosts no thread and no surface. It sends back activity facts
  without text, checkpoints at settled tool boundaries, and a result. Its
  task has no `subagent_child_thread_id`. (Run: opening one shows the
  Agents panel.)
- A child that stays in the process runs on a hosted thread when it has no
  isolation, is not a custom type, is not a custom resume and has an
  unlimited budget (`runtime_subagent_call.rs:1578-1599`). Its turn is
  started by `AgentController::launch` through `start_turn_with_output`, a
  raw `ThreadCommand::StartTurn` (`agent_controller.rs:405-436`,
  `runtime_host.rs:3882-3919`), which leaves the operation without a
  surface operation, so no surface commit is written. (Run: the child's
  view shows only "Inherited conversation context." for its whole turn.)
- `start_runtime_turn_on_surface`
  (`runtime_actor/thread_actor_operation.rs:3910`) starts a turn the thread
  begins by itself as an operation of its surface. The prompt queue and the
  wake for finished children use it (`runtime_host.rs:17321`, `:17424`).
- A thread has a prompt queue that it drains by itself, and a running
  operation can be steered (`steer_operation`, `runtime_host.rs:3981`). The
  main conversation's Enter and Ctrl+Enter use these.
- A child resolves each tool call against its own configuration's mode and
  rules (`orca-approval/src/policy.rs:43-112`). A call that resolves to
  "ask" goes to an interactive prompt that cannot work on a child's thread.
  (Run, suggest mode: the call fails with "interactive approval failed …
  os error 35" and the child goes on.) Requests to go beyond the sandbox
  travel a separate permission handler to the row in the dock.
- The agent list is sorted by latest activity
  (`workflow_panel.rs:730-741`). `TaskRecord.sequence` is a durable
  submission order (`tasks.rs:1013-1017`) that `BackgroundTaskSummary` does
  not carry.
- `Agent completed` notices are made by the TUI from changes in the task
  list and pushed into the conversation on screen
  (`workflow_panel.rs:611-657`).
- A hosted child's thread is shut down only on error paths, so a child that
  finished stays in the host's live table.

The spike changed two things: the delegated turn was started on the child's
surface, and `Async` children stayed in the process. Run, four times, eight
child turns:

- Every turn took the surface path. The child sessions held 569 to 913
  surface commits where they had held none.
- A child opened while it ran showed the task as a user message, the tool
  call and its result, and the essay growing frame by frame; nothing was
  doubled or missing against its final result. The usage shown in each view
  was that thread's own. Main received both results.
- Opening a child while it wrote was refused twice with "child focus became
  stale": the focus rechecks the task's revision, which a child that is
  streaming changes several times a second.
- With the child's own surface answering approvals, an approval was denied
  after 1.3 s because nobody was attached, and the whole child failed with
  "agent requires permission".

## Design

### Runtime

**R1. Where a child runs.** A host can say that its subagents stay in the
process; the TUI's host says so and no other host does. Under such a host
an `Async` child that qualifies for a thread (no isolation, not a custom
type, an unlimited budget, no `resume_from`) is not detached: it goes
through `enqueue_accepted_subagent` to `run_subagent_worker` and runs on a
hosted thread. A child that does not qualify is detached as today. The
flag is the host's, not the configuration's, so a resume or a new session
keeps it. What the model sees does not change: the call returns a task id
at once and results come through `task_wait` and the finished-children
wake. Admission limits are unchanged.

**R2. The delegated turn is an operation of the child's surface, under a
host that keeps its subagents.** There `AgentController::launch` starts
the turn through a sibling of
`start_runtime_turn_on_surface` that takes the output writer, carries the
event observer, the handlers and `headless_root`, and gives back the
`OperationHandle` the controller settles on. If the surface cannot admit
the turn, the raw start is used as before; the child then works and has no
live conversation. A hosted child does not get a main-session task record
from this admission. Any other host starts the turn raw, as before this
work: a turn on the surface has an activity ingress and a parent fence,
and those decide where the child's own subagents run.

**R3. A child never asks.** A child kept in the process by R1 is given
handlers that answer without a prompt:

- a tool call that resolves to "ask" is denied as that one call's result,
  with a line saying it needs an approval a subagent cannot ask for and
  should not be retried;
- a request to go beyond the sandbox, a question to the user and an MCP
  elicitation are refused the same way.

A refusal is a tool result and the turn goes on; it does not end the child
as "requires permission". The child's fixed context gains: "Your
permissions were fixed when you were started. A call that needs an approval
is refused. Do not retry it; say in your reply what you could not do." The
mode, rules and sandbox are those of the parent when the child starts;
later changes to main's mode do not reach a running child. Detached
children keep the signed mailbox.

**R4. A child finishes when its queue is empty.** After the delegated
operation ends, the controller does not settle the task while the child's
thread has queued messages: the thread drains its queue as it does for
main, the controller follows each turn it starts, and the result handed to
the parent is the last turn's reply, with usage summed. Closing is one step
in the child's actor: it finds the queue empty and closes it, and a message
that arrives afterwards is refused with "the agent has finished". Stopping
the task closes the queue and returns what was in it.

**R5. Order.** `BackgroundTaskSummary` carries the record's `sequence`.

**R6. Quitting.** The host can stop its running in-process children and
wait for them briefly; the TUI calls it on quit, so their tasks end as
stopped and not as indeterminate.

### TUI

**T1. Opening a busy agent.** The focus accepts a child whose task revision
moved, provided the task is the same one bound to the same thread.

**T2. Following an agent.** The bridge follows the child from one operation
to the next, waits when the child has no operation yet, and ends when the
task is finished. Projection of the child starts from its surface snapshot,
so a child opened late shows what it has already done. Only the
conversation on screen is held in the TUI; nothing is buffered for agents
that are not being looked at.

**T3. Messages.** With a running agent on screen, the dispatcher sends
Enter to the child thread's queue and Ctrl+Enter to its running operation,
through the view, without the controller; queue edits go the same way. The
controller therefore no longer operates on a child's thread: its adoption
of the view's child, and the half of the message guard that covered a turn
it ran there, are removed. What remains of the guard: a message in a
finished or snapshot agent's view is handed back with a line.

**T4. Esc and stop.** Esc in an agent's view returns. `/stop` there sends
the existing `StopTask` for that agent. The agent's status row reads
`Running 12s · writing · /stop` and no longer offers `Esc interrupt`.

**T5. Finished agents.** A finished agent whose thread is live opens from
that thread's surface snapshot with no bridge. After a resume its thread is
not live; it is loaded from its saved session and shown read-only. If that
fails, the checkpoint transcript is shown in the same place.

**T6. Notices.** The notices are derived when main's surface is projected,
from the completion facts main's surface already holds for its children,
and are not pushed into the conversation on screen.

**T7. Defects seen in the two runs**, each fixed with a test and looked at
in the terminal:

- main's `Running … · Esc interrupt` row is gone after a visit to a child;
- "Inherited conversation context." is appended to main on return, and
  shown in a child's view at all;
- the `Delegating to N agents` line is lost by a rebuild of main;
- `Main is running` is gone after opening a sibling from a child's view;
- Shift+Down marks no row inside a child's view;
- a child's timer restarts when it is opened again;
- the agent list goes when main finishes while agents still run;
- `/agents` from a child's view says the transcript was not found.

**T8. The card.** As designed on 2026-11-23, with rows from R5's order.

## Taken from others

- All three run a subagent in the main process and give it a conversation
  of its own; none runs its default subagents as separate processes.
- DeepSeek Harness: the permission rule (R3) and its wording; queue and
  steer as two deliveries with a stop that is neither; a child that is done
  only when its inbox is empty (R4); one conversation component for main
  and child.
- Codex: order of first appearance that never changes; input and interrupt
  that act on the thread on screen; a switch that shows a snapshot and then
  follows.
- Claude Code: Esc in an agent's view does not interrupt main; the notice
  of completion belongs to the parent's conversation and is given once;
  nothing is held for agents that are not on screen.

Not taken: asking the person from a child (Codex, and Claude Code's
teammates), and continuing a finished agent.

## Stages

Each stage ends with a run in a real terminal (pty read with `pyte`), and
the next does not start until that run shows what the stage promised.

1. **A live conversation.** R1, R2, R3, R5, R6, T1, T2. Until stage 3 a
   message sent in an agent's view is handed back. Run: in auto-edit, main
   starts two agents that each write a file, run a command and write an
   essay; open one while all three run and watch it stream; the list keeps
   its order; quit with agents running. In suggest, an agent's write is
   refused and its reply says so.
2. **A view that holds.** T5, T6, T7. Run: return to a running main and
   find its rows and lines intact; open a sibling; open a finished agent;
   `--resume latest` and open one again.
3. **Messages and stop.** R4, T3, T4. Run: with main running, queue a
   message to an agent and steer another; the reply main receives reflects
   both; `/stop` an agent and see main told.
4. **The card.** T8. Run: the frames at several sizes, clicks, the narrow
   fallback.

## To verify first

Each is the first task of its stage's plan, and is answered by running, not
by reading.

1. Stage 1: in auto-edit, a threaded child's write and shell command run
   without any prompt (read in `policy.rs`; the spike ran only full-auto
   and suggest). Run on 2026-11-24 on the spike build, in a workspace not
   trusted: both children wrote their file and ran `ls -a && cat`, nothing
   was asked or refused, and their sessions record `auto-edit permits
   write` and `auto-edit permits shell`. The TUI starts in full-auto
   (`command/launch.rs`, `prepare_interactive`); auto-edit was reached with
   Shift+Tab.
2. Stage 1: what else expects the main-session task record that R2 leaves
   out for a hosted child (the spike skipped it on a guess).
3. Stage 1: which seam says a host is interactive, and that app-server and
   ACP hosts are unaffected.
4. Stage 1: a denial from R3's handlers reaches the model as a tool result
   and the turn goes on (the spike saw the turn end instead when a surface
   handler denied).
5. Stage 2: a finished child's saved session yields a conversation without
   taking ownership of the thread.
6. Stage 2: main's surface holds a completion fact for each in-process
   child, and still does after a resume.
7. Stage 3: a hosted child's thread drains its own prompt queue, and the
   controller can follow the operations it starts.

If 5 fails, a finished agent shows its checkpoint transcript after a
resume. If 6 fails, the runtime records the notice on main's surface. If 7
fails, Enter in an agent's view steers, and queueing waits for its own
design.

Run on 2026-11-25 at the end of stage 1 (`a05a69d6`, `deepseek-flash`, a
150x50 pty read with `pyte`, frames under `/tmp/orca-stage1/`), items 1 to 4
hold. 1: in auto-edit both kept children wrote their file and ran
`ls -a && cat` with no prompt, and no `orca subagent-worker` process existed
at any point. 2: the task registry held a main-session record for each of
main's turns and a `subagent` record for each child, but no main-session
record for a child, and the Agents panel, Esc, `s`, quit and `--resume` all
worked without one. 3: a host keeps its children in its process only
when it asks to; `server_runtime_contract`, `acp_agent` and
`subagent_delegation_contract` pass. 4: in suggest, each child's write and
shell call came back as a tool result, `This call needs an approval, and a
subagent cannot ask for one.`; neither child stopped there, each reply said
what it could not do, main's reply said so too, and neither file exists.
Also seen: a child's conversation streams while it runs and a message sent
in it is handed back; the rows keep their order; `s` in the Agents panel
stops one child (`stopped`, no `s stop`) while main and the other go on;
Esc on main stops both (`cancelled`, `0 active`): their rows left within
about a second and main's turn ended within about three, during `sleep 20`
too, and the next prompt is answered with no row coming back; Ctrl+C on main
does the same, its rows leaving after about two seconds and main's turn
ending after about five; the Ctrl+C after that says `Press Ctrl+C again to
quit.`, the TUI exits 130, no process is left, both records are `stopped`,
and after a resume neither is shown running. Seen and left to stage 2:
`Inherited conversation context.` shows in a child's view and on main;
after returning from a child's view main's running row and its local
notices (`Stopping …`) are gone, and `Main is running` is lost when a child
is opened again from its own view; `Agent completed` is shown in the
child's own view; a cancelled child's panel history ends `completed`; a
child stopped with `s` that main waited on with `task_wait` reads
`agent cancelled`, and main started it again both times.

## Not in this change

- Asking the person, or the main agent, on a subagent's behalf.
- Continuing a finished subagent.
- Conversations for isolated, custom, budgeted and resumed subagents.
- Subagents of `orca exec`, the app server and ACP: unchanged.
- A diff view opened from Changes; a split-pane layout.

## Testing

- Runtime: an `Async` child of an interactive host runs in the process and
  one of a headless host is detached; a delegated turn writes stream
  commits to the child's surface and falls back when admission fails; a
  call that resolves to "ask" is refused as a tool result and the turn
  completes; the queue drains before the task settles, and a message after
  closing is refused; quitting leaves tasks stopped.
- TUI: focus on a child whose revision moves; the bridge across two
  operations and from before the first; Enter and Ctrl+Enter reach the
  child with main running; a message to a finished agent is handed back;
  Esc returns and interrupts nothing; notices appear in main after a
  rebuild; each defect in T7.
- The contracts that fix today's behaviour are updated where it changes:
  `tests/tui_pty_contract.rs:551` (stopping a detached child from the TUI)
  and `docs/subagents.md`. `tests/subagent_contract.rs:209` must keep
  passing untouched.
- In a real terminal: the run named for each stage.
