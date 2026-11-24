# Live Subagent Conversations, Stage 1: A Live Conversation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A subagent started from the TUI runs on a thread of the TUI's process, its turn is recorded on its own surface, and a person who opens it while it runs sees its conversation stream as the main one does.

**Architecture:** The TUI's runtime host says its subagents stay in the process; a child that qualifies for a hosted thread is then not detached. `AgentController::launch` starts the delegated turn through surface admission (the route the prompt queue already uses) instead of a raw `StartTurn`, so the child's surface holds the turn and the existing TUI bridge can follow it. A child kept in the process never asks: calls that would ask are refused as tool results. The TUI's focus check and bridge are loosened for a child that is streaming, the agent list takes the registry's submission order, and quitting stops the children.

**Tech Stack:** Rust 2024, `orca-runtime` (thread actors, surface, task registry), `orca-tui` (ratatui 0.30, crossterm 0.29), `orca-core`, `orca-approval`. Real-terminal checks: Python `pty` + `pyte`.

**Spec:** `docs/superpowers/specs/2026-11-24-live-subagent-conversations-design.md` (stage 1: R1, R2, R3, R5, R6, T1, T2, and "To verify first" items 1 to 4).

## Global Constraints

- Branch `feat/live-subagent-view`. One commit per task, never pushed. `main` is not touched.
- `orca exec`, the app server and ACP keep today's behaviour. `tests/subagent_contract.rs` passes without being edited.
- A child that does not qualify for a thread (isolation, a custom type, a finite budget, `resume_from`) is detached exactly as today.
- What the model sees of the `subagent` tool does not change: no new argument, the call returns a task id at once.
- Strings a person or the model reads are used exactly as written in the task that introduces them.
- No new `#[allow(...)]`, no new clippy warning in touched code, no `unwrap()` outside tests.
- A new direct platform call (`libc`, `std::os::unix`, `windows-sys`) is recorded in `docs/superpowers/specs/2026-07-28-native-windows-platform-foundation.manifest.json`; a new `orca_runtime::surface` export is recorded in the surface contract manifest and digest. `for f in scripts/validate-*.mjs scripts/test-validate-*.mjs; do node "$f"; done` passes at the end of every task that touches `crates/orca-runtime` or `crates/orca-core`.
- Disk is nearly full. Run the named tests, not whole suites, until Task 10. If `df -h /Users/bytedance` shows under 3 GB free, delete `target/debug/incremental` and stale `target/debug/deps/orca_tui-*` artifacts before building.
- The contents of `~/.orca/auth.json` are never printed, logged, copied into a report or committed; temporary copies are deleted when a run ends.
- Where a step gives the behaviour, the names and the test but not every line of the code, the code it touches has to be read first: the line numbers below were right at `d5c959c3` and are a place to start.
- The spike branch `spike/child-turn-on-surface` (`05e57034`) is a reference to read, not code to merge: its names start with `spike_`, it logs to `/tmp`, and it switches on environment variables.

## File Structure

| File | Responsibility in this plan |
|---|---|
| `crates/orca-runtime/src/agent_registry.rs` | The host-wide flag "subagents stay in the process". |
| `crates/orca-runtime/src/runtime_host.rs` | `RuntimeHostHandle::keep_subagents_in_process`; `ThreadCommand::StartDelegatedTurnOnSurface`; `RuntimeStartedTurn.headless_root`; stopping in-process children. |
| `crates/orca-runtime/src/runtime_actor/thread_actor_operation.rs` | Surface admission that takes an output writer and returns the operation's handle; no main-session task for a hosted child. |
| `crates/orca-runtime/src/agent_controller.rs` | Launch on the child's surface with a raw fallback; the child's fixed context; the refusing handlers. |
| `crates/orca-runtime/src/subagent_execution.rs` | The decision to detach. |
| `crates/orca-runtime/src/subagent_placement.rs` (new) | The pure rule: which child is detached. |
| `crates/orca-runtime/src/child_refusal.rs` (new) | The handlers of a child that never asks, and their wording. |
| `crates/orca-runtime/src/tool_execution.rs` | The approval gate turns a refusal-without-asking into a denied tool result and lets the turn go on. |
| `crates/orca-runtime/src/tasks.rs`, `crates/orca-core/src/task_types.rs` | `sequence` on `BackgroundTaskSummary`. |
| `crates/orca-tui/src/workflow_panel.rs` | The list in submission order. |
| `crates/orca-tui/src/hosted_child.rs` | Focus on a child whose revision moved; a bridge that waits for an operation and follows the next. |
| `crates/orca-tui/src/hosted_view.rs`, `operation_controller.rs`, `action_dispatcher.rs` | A message in an agent's view is handed back. |
| `crates/orca-tui/src/global_actions.rs`, `hosted_controller.rs`, the TUI's host start | The quit line and stopping children on quit; telling the host to keep subagents in the process. |
| `docs/subagents.md`, `tests/tui_pty_contract.rs` | What changed for a person. |

---

### Task 1: Check in a terminal what the plan relies on

Nothing is built on `feat/live-subagent-view` in this task. It answers spec items 1 and 2 under "To verify first" and stops the plan if the answer is no.

**Files:**
- Create: `.superpowers/sdd/2026-11-24-live-subagent-conversations-stage-1/task-1-report.md` (git-ignored)
- Read: `.superpowers/research/2026-11-24-spike-child-turn-on-surface.md`, `/tmp/orca-spike/` (the driver and earlier screens, if still there), `/Users/bytedance/.claude/projects/-Users-bytedance-Documents-GitHub-blade-deepseek/memory/feedback_verify_tui_in_real_terminal.md` and `reference_orca_exec_eval_runs.md`

**Interfaces:**
- Produces: two answers the later tasks rely on. (a) In `auto-edit`, a threaded child's file write and shell command run with no prompt and no refusal. (b) The list of code that reads a main-session task record for a thread, so Task 2 knows what leaving it out for a hosted child affects.

- [ ] **Step 1: Build the spike**

```bash
cd /Users/bytedance/Documents/GitHub/blade-deepseek
git status --short            # must be empty
git checkout spike/child-turn-on-surface
cargo build 2>&1 | tail -3
```

Expected: `Finished`. If the working tree is not clean, stop and report.

- [ ] **Step 2: Run it in auto-edit**

Use a temporary `ORCA_HOME` with a copy of the real `auth.json` and no `mode` line in `config.toml` (so the mode is the default, `auto-edit`), an empty temporary working directory, a pty of 150x50 read with `pyte`, model `deepseek-flash`, and the environment `ORCA_SPIKE_INPROC_SUBAGENTS=1 ORCA_SPIKE_LOG=/tmp/orca-stage1/task1-path.log`. Prompt for main:

```text
Start two subagents in parallel. Each must: write a file named after itself (alpha.txt, beta.txt) containing one sentence, run the shell command `ls -a && cat <its file>`, and then reply with one paragraph. Wait for both and tell me what each replied.
```

Capture the screen once a second to `/tmp/orca-stage1/task1/NNN.txt` until main is idle. Open one child with Shift+Down and Enter while it runs.

- [ ] **Step 3: Read what happened**

Record in the report, from the screens and the files:
- the mode shown in the status bar;
- for each child: did the write happen (`alpha.txt` / `beta.txt` exist in the working directory with the sentence), did the shell command run (its output on screen or in the child's session file), was anything refused, was any prompt or `needs approval` shown anywhere;
- the path each child turn took, from `/tmp/orca-stage1/task1-path.log`.

- [ ] **Step 4: Find what reads a main-session task record**

```bash
git checkout feat/live-subagent-view
grep -rn "main_session_task_id\|MainSession" crates/orca-runtime/src --include='*.rs' | grep -v "#\[cfg(test)\]" | grep -v "^.*tests\.rs" > /tmp/orca-stage1/main-session-readers.txt
wc -l /tmp/orca-stage1/main-session-readers.txt
```

Read each non-test site and list in the report: the function, what it does with the record, and whether a hosted child thread (one whose thread extensions hold `AgentThreadPolicy`) reaches it. `runtime_host.rs` near `19943-19954` already clears `main_session_task_id` for a hosted child on the raw start; say whether the surface-admission path (`finish_prepared_surface_admission`, `thread_actor_operation.rs` near `4614-4680`) has an equivalent.

- [ ] **Step 5: Decide**

- If both children wrote and ran their command with no prompt and no refusal: write `GATE: pass` at the top of the report and go on.
- Otherwise write `GATE: fail` with the screen that shows it and **stop the plan**. The permission rule of the spec does not hold as written and the design has to be looked at again before any code.

Delete the temporary `auth.json` copies. Leave `feat/live-subagent-view` checked out and clean. Nothing is committed in this task.

---

### Task 2: The delegated turn is an operation of the child's surface

**Files:**
- Modify: `crates/orca-runtime/src/runtime_host.rs` (`RuntimeThreadHandle` near `3882-3919`, `enum ThreadCommand` near `4967`, `struct RuntimeStartedTurn` near `12581`, the three `match` sites on `ThreadCommand` near `18994`, `20075`, `21373`, the prompt-queue constructor of `RuntimeStartedTurn` near `17358`)
- Modify: `crates/orca-runtime/src/runtime_actor/thread_actor_operation.rs` (`start_runtime_turn_on_surface` at `3910`, `finish_prepared_surface_admission` near `4614-4680`)
- Modify: `crates/orca-runtime/src/agent_controller.rs` (`launch`, near `403-436`)
- Test: the `#[cfg(test)]` module that already tests `AgentController::launch` (find it with `grep -n "fn .*launch" crates/orca-runtime/src/agent_controller.rs crates/orca-runtime/src/subagent_execution.rs | grep test`; the fixtures at `subagent_execution.rs:2371-2602` build a controller on a real host)

**Interfaces:**
- Consumes: Task 1's list of main-session readers.
- Produces:
  - `RuntimeThreadHandle::start_delegated_turn_on_surface<W: HostedOperationWriter>(&self, request: DelegatedTurnStart, writer: W) -> Result<OperationHandle, RuntimeHostError>` (`pub(crate)`)
  - `pub(crate) struct DelegatedTurnStart { pub prompt: String, pub headless_root: bool, pub observer: Arc<dyn EventObserver>, pub approval: Option<Arc<dyn RuntimeApprovalHandler + Send + Sync>>, pub permission: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>> }`
  - `RuntimeStartedTurn` gains `headless_root: bool`.
  - Task 4 passes its refusing handlers through `DelegatedTurnStart.approval` and `.permission`; Task 7 relies on a running child having a foreground surface operation.

- [ ] **Step 1: Write the failing tests**

In the test module found above, using its existing fixture for a host, a scripted provider and a controller, add two tests. The assertions are what matters; build the fixture the way the neighbouring tests do.

```rust
#[test]
fn a_delegated_turn_is_recorded_on_the_childs_surface() {
    // Launch a Sync child whose provider replies with two text deltas.
    // After launch returns:
    //  - read the child thread's surface snapshot (resolve the thread from
    //    the host by the launch's child thread id, then `surface()` and a
    //    fresh attach, as `HostedChildEventBridge::prepare` does);
    //  - assert the snapshot holds one operation, terminal, of kind UserTurn;
    //  - assert the conversation in the snapshot holds a user item whose
    //    text is the delegated prompt and an assistant item whose text is
    //    the two deltas joined.
}

#[test]
fn a_delegated_turn_that_the_surface_refuses_still_runs() {
    // Make admission fail (the simplest way the fixtures allow: start a
    // turn on the child thread first so the surface reports an active
    // operation, or use a thread started without a surface owner).
    // Assert launch still returns the child's reply, i.e. the raw start was
    // used, and that no panic or error reached the caller.
}
```

- [ ] **Step 2: Run them and see the first fail**

Run: `cargo test -p orca-runtime a_delegated_turn_ -- --nocapture`
Expected: `a_delegated_turn_is_recorded_on_the_childs_surface` FAILS on the snapshot assertion (no operation on the child's surface); the second passes already (the raw start is all there is).

- [ ] **Step 3: Carry `headless_root` on `RuntimeStartedTurn`**

In `runtime_host.rs`:

```rust
struct RuntimeStartedTurn {
    turn_id: Option<TurnId>,
    observer: Option<Arc<dyn EventObserver>>,
    interaction_handlers: PromptQueueInteractionHandlers,
    /// A delegated child turn carries its root's headless flag, which the
    /// raw start took from `HostedTurnRequest::with_headless_root`.
    headless_root: bool,
}
```

In its method that builds the `HostedTurnRequest` (the one that applies `turn_id` and `observer`), add before the observer:

```rust
        if self.headless_root {
            request = request.with_headless_root(true);
        }
```

Every existing constructor of `RuntimeStartedTurn` gets `headless_root: false` (the prompt queue's near `17358` and any other the compiler names).

- [ ] **Step 4: Let surface admission take a writer and give back the handle**

In `thread_actor_operation.rs`, keep `start_runtime_turn_on_surface` as a wrapper and move its body into a function with the writer:

```rust
    pub(super) fn start_runtime_turn_on_surface(
        &mut self,
        prompt: &str,
        images: &[orca_core::conversation::ImageInput],
        runtime_started: RuntimeStartedTurn,
    ) -> Result<OperationId, surface::SurfaceClientCommandError> {
        self.start_runtime_turn_on_surface_with_output(prompt, images, runtime_started, None)
    }

    /// A delegated child turn, started as an operation of the child's
    /// surface so a view of the child can follow it. Gives back the handle
    /// the delegating controller settles on.
    pub(super) fn start_delegated_turn_on_surface(
        &mut self,
        prompt: &str,
        runtime_started: RuntimeStartedTurn,
        writer: Box<dyn HostedOperationWriter>,
    ) -> Result<OperationHandle, RuntimeHostError> {
        let operation_id = self
            .start_runtime_turn_on_surface_with_output(prompt, &[], runtime_started, Some(writer))
            .map_err(|error| RuntimeHostError::ThreadStartFailed {
                message: format!("the child's surface did not admit the turn: {error:?}"),
            })?;
        let active = self
            .active
            .as_ref()
            .filter(|active| active.operation_id == operation_id)
            .ok_or(RuntimeHostError::ThreadUnavailable)?;
        Ok(OperationHandle {
            operation_id: active.operation_id,
            initial_generation: GenerationFence::initial(active.operation_id),
            thread: self.handle.clone(),
            completion: active.completion.clone(),
        })
    }
```

`start_runtime_turn_on_surface_with_output` is the old body with one more parameter, `output_writer: Option<Box<dyn HostedOperationWriter>>`, passed where the old body passed `None` to `prepare_surface_admission_from` (the argument before `Some(runtime_started)`). If `OperationHandle`'s fields differ from the four above, build it the way the `ThreadCommand::StartTurn` arm near `runtime_host.rs:20060-20075` does.

- [ ] **Step 5: No main-session task for a hosted child**

In `finish_prepared_surface_admission`, where `main_session_task` is created when `goal_identity.is_none()`:

```rust
        // A hosted child's delegated task is its execution identity. A
        // main-session record for it would sit in the shared registry and
        // never be settled.
        let hosted_child = self.state.as_ref().is_some_and(|state| {
            state
                .thread
                .thread_extensions()
                .get::<crate::agent_controller::AgentThreadPolicy>()
                .is_some()
        });
        let main_session_task = if goal_identity.is_none() && !hosted_child {
```

If Task 1's report names a reader that a hosted child reaches and that needs the record, do not skip it silently: stop and report `BLOCKED` with that reader.

- [ ] **Step 6: The command and the handle method**

In `runtime_host.rs`, add to `enum ThreadCommand`:

```rust
    /// Start a delegated child turn as an operation of the child's surface.
    /// `Err` means nothing started and the caller may start it raw.
    StartDelegatedTurnOnSurface {
        prompt: String,
        started: RuntimeStartedTurn,
        writer: Box<dyn HostedOperationWriter>,
        reply: SyncSender<Result<OperationHandle, RuntimeHostError>>,
    },
```

Handle it at the three sites that match on `ThreadCommand::StartTurn`: where the thread is unavailable reply `Err(RuntimeHostError::ThreadUnavailable)`; where an operation is active reply `Err(RuntimeHostError::OperationActive { operation_id: active.operation_id })`; in the idle handler:

```rust
            ThreadCommand::StartDelegatedTurnOnSurface {
                prompt,
                started,
                writer,
                reply,
            } => {
                let result = self.start_delegated_turn_on_surface(&prompt, started, writer);
                let _ = reply.send(result);
            }
```

Next to `start_turn_with_output` on `RuntimeThreadHandle`:

```rust
pub(crate) struct DelegatedTurnStart {
    pub(crate) prompt: String,
    pub(crate) headless_root: bool,
    pub(crate) observer: Arc<dyn EventObserver>,
    pub(crate) approval: Option<Arc<dyn RuntimeApprovalHandler + Send + Sync>>,
    pub(crate) permission: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
}

    /// Starts a delegated child turn through the child's surface, so the
    /// turn is part of the conversation a view reads.
    pub(crate) fn start_delegated_turn_on_surface<W>(
        &self,
        request: DelegatedTurnStart,
        writer: W,
    ) -> Result<OperationHandle, RuntimeHostError>
    where
        W: HostedOperationWriter,
    {
        let started = RuntimeStartedTurn {
            turn_id: None,
            observer: Some(request.observer),
            interaction_handlers: PromptQueueInteractionHandlers {
                approval: request.approval,
                permission: request.permission,
                user_input: None,
                mcp_elicitation: None,
            },
            headless_root: request.headless_root,
        };
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::StartDelegatedTurnOnSurface {
            prompt: request.prompt,
            started,
            writer: Box::new(writer),
            reply: reply_tx,
        })?;
        receive_reply(reply_rx, "runtime thread")?
    }
```

- [ ] **Step 7: Launch through it, with the raw start as the fallback**

In `AgentController::launch`, replace the block that builds `turn` and calls `child.start_turn_with_output` with:

```rust
        let permission_handler = request.permission_handler.map(|handler| {
            Arc::new(AgentPermissionHandler {
                inner: handler,
                publisher: publisher.clone(),
            }) as Arc<dyn RuntimePermissionRequestHandler + Send + Sync>
        });
        let on_surface = child.start_delegated_turn_on_surface(
            crate::runtime_host::DelegatedTurnStart {
                prompt: request.prompt.clone(),
                headless_root: child_config.tools.headless_exec,
                observer: publisher.clone(),
                approval: request.approval_handler.clone(),
                permission: permission_handler.clone(),
            },
            AgentOutputWriter::new(publisher.clone()),
        );
        let started = match on_surface {
            Ok(operation) => Ok(operation),
            // The child still does its work; it only has no conversation a
            // view can follow.
            Err(_) => {
                let mut turn = HostedTurnRequest::new(request.prompt)
                    .with_headless_root(child_config.tools.headless_exec)
                    .with_event_observer(publisher.clone());
                if let Some(handler) = request.approval_handler {
                    turn = turn.with_approval_handler(handler);
                }
                if let Some(handler) = permission_handler {
                    turn = turn.with_permission_handler(handler);
                }
                child.start_turn_with_output(turn, AgentOutputWriter::new(publisher.clone()))
            }
        };
        let operation = match started {
```

and leave the existing `Ok(operation) => operation, Err(error) => { … }` arms as they are. If `request.prompt` or the handlers are not `Clone`, clone what the types allow and restructure so the fallback still has them; do not change their types.

- [ ] **Step 8: Run the tests**

Run: `cargo test -p orca-runtime a_delegated_turn_ -- --nocapture`
Expected: both PASS.

Run: `cargo test -p orca-runtime agent_controller subagent_ 2>&1 | tail -15` and `cargo test --test subagent_contract 2>&1 | tail -5`
Expected: no failure. A test that fails because it pinned the absence of a surface operation for a child is updated to the new fact and named in the report; any other failure is a regression to fix.

- [ ] **Step 9: Validators and commit**

```bash
for f in scripts/validate-*.mjs scripts/test-validate-*.mjs; do node "$f" >/dev/null || echo "FAILED $f"; done
git add -A crates/orca-runtime
git commit -m "feat(runtime): a delegated child turn is an operation of the child's surface"
```

---

### Task 3: A host can keep its subagents in the process

**Files:**
- Create: `crates/orca-runtime/src/subagent_placement.rs`
- Modify: `crates/orca-runtime/src/lib.rs` (the `mod` list, in rustfmt order), `crates/orca-runtime/src/agent_registry.rs`, `crates/orca-runtime/src/runtime_host.rs` (`impl RuntimeHostHandle`), `crates/orca-runtime/src/agent_controller.rs`, `crates/orca-runtime/src/subagent_execution.rs:814-840`
- Modify: the place the TUI starts its runtime host (find it with `grep -rn "RuntimeHost::start" crates/orca-tui/src | grep -v "mod tests" | grep -v "#\[test\]"` and by reading `crates/orca-tui/src/agent_runtime.rs`)

**Interfaces:**
- Produces:
  - `pub(crate) struct ChildPlacement { pub mode_async: bool, pub registry_shared: bool, pub parent_fence: bool, pub activity_ingress: bool, pub host_keeps_children: bool, pub qualifies_for_thread: bool }` and `pub(crate) fn detaches(placement: ChildPlacement) -> bool`
  - `AgentRegistry::keep_subagents_in_process(&self)`, `AgentRegistry::keeps_subagents_in_process(&self) -> bool`
  - `pub fn RuntimeHostHandle::keep_subagents_in_process(&self)`
  - `AgentController::keeps_subagents_in_process(&self) -> bool`
  - Task 4 reads `AgentController::keeps_subagents_in_process`; Task 9 reads `RuntimeHostHandle`.

- [ ] **Step 1: Write the failing test for the rule**

`crates/orca-runtime/src/subagent_placement.rs`:

```rust
//! Where a subagent runs: detached in a worker process, or in this process.

/// What the decision to detach a child depends on.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ChildPlacement {
    /// The child was submitted in `Async` mode.
    pub(crate) mode_async: bool,
    /// The task registry is shared between processes.
    pub(crate) registry_shared: bool,
    /// The parent has a fence a worker can be bound to.
    pub(crate) parent_fence: bool,
    /// The parent has an activity ingress a worker can report to.
    pub(crate) activity_ingress: bool,
    /// The host asked for its subagents to stay in the process.
    pub(crate) host_keeps_children: bool,
    /// The child can run on a hosted thread of its own.
    pub(crate) qualifies_for_thread: bool,
}

/// `true` when the child is launched as a detached worker process.
pub(crate) fn detaches(placement: ChildPlacement) -> bool {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::{ChildPlacement, detaches};

    const DETACHED_TODAY: ChildPlacement = ChildPlacement {
        mode_async: true,
        registry_shared: true,
        parent_fence: true,
        activity_ingress: true,
        host_keeps_children: false,
        qualifies_for_thread: true,
    };

    #[test]
    fn a_host_that_does_not_ask_detaches_as_before() {
        assert!(detaches(DETACHED_TODAY));
        for placement in [
            ChildPlacement { mode_async: false, ..DETACHED_TODAY },
            ChildPlacement { registry_shared: false, ..DETACHED_TODAY },
            ChildPlacement { parent_fence: false, ..DETACHED_TODAY },
            ChildPlacement { activity_ingress: false, ..DETACHED_TODAY },
        ] {
            assert!(!detaches(placement), "{placement:?}");
        }
    }

    #[test]
    fn a_host_that_keeps_its_children_keeps_those_that_can_have_a_thread() {
        let kept = ChildPlacement { host_keeps_children: true, ..DETACHED_TODAY };
        assert!(!detaches(kept));
    }

    #[test]
    fn a_child_that_cannot_have_a_thread_is_detached_even_then() {
        let no_thread = ChildPlacement {
            host_keeps_children: true,
            qualifies_for_thread: false,
            ..DETACHED_TODAY
        };
        assert!(detaches(no_thread));
    }
}
```

Add `mod subagent_placement;` to `crates/orca-runtime/src/lib.rs` where rustfmt would sort it.

- [ ] **Step 2: Run and see it fail**

Run: `cargo test -p orca-runtime subagent_placement`
Expected: FAIL, `not yet implemented`.

- [ ] **Step 3: Implement the rule**

```rust
pub(crate) fn detaches(placement: ChildPlacement) -> bool {
    let detached_today = placement.mode_async
        && placement.registry_shared
        && placement.parent_fence
        && placement.activity_ingress;
    detached_today && !(placement.host_keeps_children && placement.qualifies_for_thread)
}
```

Run: `cargo test -p orca-runtime subagent_placement`
Expected: 3 passed.

- [ ] **Step 4: The host's flag**

In `agent_registry.rs`, add a field `keeps_subagents_in_process: std::sync::atomic::AtomicBool` to `AgentRegistry` (initialised `false` in every constructor) and:

```rust
    /// The host asked that its subagents run on threads of this process.
    pub(crate) fn keep_subagents_in_process(&self) {
        self.keeps_subagents_in_process
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn keeps_subagents_in_process(&self) -> bool {
        self.keeps_subagents_in_process
            .load(std::sync::atomic::Ordering::Acquire)
    }
```

In `impl RuntimeHostHandle` (`runtime_host.rs` near `4467`):

```rust
    /// Subagents started under this host run on threads of this process
    /// when they can, so an interactive client can follow their
    /// conversations. They end with the process. A host that is not asked
    /// keeps detaching them.
    pub fn keep_subagents_in_process(&self) {
        self.agent_registry.keep_subagents_in_process();
    }
```

In `impl AgentController`:

```rust
    pub(crate) fn keeps_subagents_in_process(&self) -> bool {
        self.registry.keeps_subagents_in_process()
    }
```

Add a test beside the registry's tests:

```rust
    #[test]
    fn a_registry_keeps_subagents_in_process_only_once_asked() {
        let registry = AgentRegistry::default();
        assert!(!registry.keeps_subagents_in_process());
        registry.keep_subagents_in_process();
        assert!(registry.keeps_subagents_in_process());
    }
```

(Use the constructor the neighbouring tests use if `AgentRegistry` has no `Default`.)

- [ ] **Step 5: Use the rule where the child is detached**

In `subagent_execution.rs`, replace the condition of the `if` that calls `launch_async_subagent`:

```rust
    let placement = crate::subagent_placement::ChildPlacement {
        mode_async: request.mode == SubagentMode::Async,
        registry_shared: !task_registry.is_process_local(),
        parent_fence: has_parent_fence,
        activity_ingress: activity_ingress.is_some(),
        host_keeps_children: agent_controller
            .as_ref()
            .is_some_and(|controller| controller.keeps_subagents_in_process()),
        qualifies_for_thread: child_qualifies_for_thread(&request, agent_controller.is_some()),
    };
    if crate::subagent_placement::detaches(placement) {
```

and add, next to `run_subagent_worker`'s own test of the same conditions in `runtime_subagent_call.rs` (`1578-1599`), one function both sites use:

```rust
/// A child runs on a hosted thread of its own when there is a controller to
/// host it and it has no isolation, is not a custom agent, and is not
/// continued from a checkpoint (the hosted path refuses `resume_from`).
pub(crate) fn child_qualifies_for_thread(request: &SubagentRequest, has_controller: bool) -> bool {
    has_controller
        && request.isolation == SubagentIsolation::None
        && !matches!(request.subagent_type, SubagentType::Custom(_))
        && request.resume_from.is_none()
}
```

`run_subagent_worker` keeps its `resumes_custom` test (it is weaker than `resume_from.is_none()` and still needed for children that are not kept). A finite child budget already makes `agent_controller` `None` before this point; confirm that by reading how `agent_controller` reaches `execute_subagent_tool_with_activity_ingress` (`subagent_execution.rs:671` and its callers) and name the line in the report. If it does not, add the budget to `child_qualifies_for_thread` and to the tests below.

Add tests beside `child_qualifies_for_thread`:

```rust
    #[test]
    fn a_plain_child_with_a_controller_qualifies_for_a_thread() { /* isolation None, a built-in type, no resume_from, has_controller true => true */ }

    #[test]
    fn isolation_a_custom_type_or_a_resume_do_not_qualify() { /* each one alone => false; has_controller false => false */ }
```

built with the request constructor the neighbouring tests use.

- [ ] **Step 6: The TUI asks**

Where the TUI obtains the `RuntimeHostHandle` it runs its sessions on, call `host.keep_subagents_in_process()` once, right after the host starts. The attached (ACP) backend, `run_tui_attached`, has no local host and is not touched. Add a test in that module that the host handle the TUI builds reports the flag — if the flag is only readable inside `orca-runtime`, add `pub fn keeps_subagents_in_process(&self) -> bool` on `RuntimeHostHandle` and use it.

- [ ] **Step 7: Run**

```bash
cargo test -p orca-runtime subagent_placement a_registry_keeps child_qualifies 2>&1 | tail -8
cargo test -p orca-runtime qualif 2>&1 | tail -5
cargo test --test subagent_contract 2>&1 | tail -5
cargo test -p orca-tui keep_subagents 2>&1 | tail -5
```

Expected: all pass; `subagent_contract` unchanged and passing (it runs `orca exec`, which never asks).

- [ ] **Step 8: Validators and commit**

```bash
for f in scripts/validate-*.mjs scripts/test-validate-*.mjs; do node "$f" >/dev/null || echo "FAILED $f"; done
git add -A crates
git commit -m "feat(runtime): the TUI's host keeps subagents that can have a thread in the process"
```

---

### Task 4: A child kept in the process never asks

**Files:**
- Create: `crates/orca-runtime/src/child_refusal.rs`
- Modify: `crates/orca-runtime/src/lib.rs` (`mod child_refusal;`), `crates/orca-runtime/src/tool_execution.rs` (the `Err(error) =>` arm of the interactive approval, near `1571-1583`), `crates/orca-runtime/src/agent_controller.rs` (`DELEGATED_CHILD_CONTEXT` at `25`, `launch` at `360-370` and the handlers of Task 2), `crates/orca-runtime/src/runtime_subagent_call.rs` (`run_threaded_agent_worker`, where `permission_handler` is built near `1281-1290` and `approval_handler: None` at `1342`)

**Interfaces:**
- Consumes: `AgentController::keeps_subagents_in_process` (Task 3), `DelegatedTurnStart.approval` / `.permission` (Task 2).
- Produces:
  - `pub(crate) const APPROVAL_REFUSED: &str`
  - `pub(crate) const UNATTENDED_CHILD_CONTEXT: &str`
  - `pub(crate) struct RefusedWithoutAsking;` (an error payload), `pub(crate) fn refused_without_asking(error: &io::Error) -> bool`
  - `pub(crate) struct RefusingApprovalHandler;` implementing `RuntimeApprovalHandler`
  - `pub(crate) struct RefusingPermissionHandler;` implementing `RuntimePermissionRequestHandler`

Exact strings:

```rust
pub(crate) const APPROVAL_REFUSED: &str = "This call needs an approval, and a subagent cannot ask for one. \
    Do not retry it. Go on with what you can do, and say in your reply what you could not do.";

pub(crate) const UNATTENDED_CHILD_CONTEXT: &str = " Your permissions were fixed when you were started. \
    A call that needs an approval is refused. Do not retry it; say in your reply what you could not do.";
```

- [ ] **Step 1: Write the failing tests**

`crates/orca-runtime/src/child_refusal.rs`:

```rust
//! A subagent kept in its parent's process never asks anyone: what would
//! need an answer is refused, and the child reports it.

use std::io;

use orca_core::approval_types::{ApprovalRequest, ApprovalResolution};
use orca_core::tool_types::ToolRequest;

use crate::runtime_approval::RuntimeApprovalHandler;

pub(crate) const APPROVAL_REFUSED: &str = "This call needs an approval, and a subagent cannot ask for one. \
    Do not retry it. Go on with what you can do, and say in your reply what you could not do.";

pub(crate) const UNATTENDED_CHILD_CONTEXT: &str = " Your permissions were fixed when you were started. \
    A call that needs an approval is refused. Do not retry it; say in your reply what you could not do.";

/// Marks an approval that was refused because nobody may be asked, as
/// distinct from a prompt that failed.
#[derive(Debug)]
pub(crate) struct RefusedWithoutAsking;

impl std::fmt::Display for RefusedWithoutAsking {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(APPROVAL_REFUSED)
    }
}

impl std::error::Error for RefusedWithoutAsking {}

pub(crate) fn refused_without_asking(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<RefusedWithoutAsking>())
}

/// Answers every approval a child's tool call would ask for with a refusal.
pub(crate) struct RefusingApprovalHandler;

impl RuntimeApprovalHandler for RefusingApprovalHandler {
    fn resolve_interactive(
        &self,
        _approval: &ApprovalRequest,
        _request: &ToolRequest,
    ) -> io::Result<ApprovalResolution> {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, RefusedWithoutAsking))
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{APPROVAL_REFUSED, RefusedWithoutAsking, refused_without_asking};

    #[test]
    fn a_refusal_is_told_from_a_prompt_that_failed() {
        let refusal = io::Error::new(io::ErrorKind::PermissionDenied, RefusedWithoutAsking);
        assert!(refused_without_asking(&refusal));
        assert_eq!(refusal.to_string(), APPROVAL_REFUSED);

        let failed = io::Error::from_raw_os_error(35);
        assert!(!refused_without_asking(&failed));
        let denied = io::Error::new(io::ErrorKind::PermissionDenied, "no access");
        assert!(!refused_without_asking(&denied));
    }
}
```

Add `mod child_refusal;` to `lib.rs` in rustfmt order. Add `RefusingPermissionHandler` in the same file: read `RuntimePermissionRequestHandler` (`runtime_permission.rs:87`) and `RuntimePermissionResponse` (`:80`), and return the response a person's "deny" produces today (find one with `grep -rn "PermissionResponseDecision::" crates/orca-runtime/src | grep -i den | head`), with no grant and `strict_auto_review: false`. Test it:

```rust
    #[test]
    fn a_permission_request_is_denied_with_nothing_granted() {
        // Build the smallest RuntimePermissionRequest the neighbouring tests
        // build; assert the response's decision is the denied one and its
        // permissions are empty.
    }
```

Then add the test of the gate, in `tool_execution.rs`'s test module, using the fixture its other approval-gate tests use:

```rust
    #[test]
    fn a_refusal_without_asking_is_a_denied_result_and_the_turn_goes_on() {
        // Policy: ApprovalMode::Suggest. Tool request: bash `echo hi`.
        // Approval handler: crate::child_refusal::RefusingApprovalHandler.
        // Run the approval gate.
        // Assert:
        //  - the gate ends the call with RunStatus::Success (not
        //    ApprovalRequired, not Failed);
        //  - the tool result is the one ToolResult::denied builds, and its
        //    text is exactly crate::child_refusal::APPROVAL_REFUSED;
        //  - the text does not contain "interactive approval failed".
    }

    #[test]
    fn a_prompt_that_fails_is_still_a_failed_dispatch() {
        // Same, with a handler that returns Err(io::Error::from_raw_os_error(35)).
        // Assert the result text starts with
        // "interactive approval failed before tool dispatch:".
    }
```

- [ ] **Step 2: Run and see the gate test fail**

Run: `cargo test -p orca-runtime child_refusal a_refusal_without_asking a_prompt_that_fails -- --nocapture`
Expected: `child_refusal` tests pass; `a_refusal_without_asking_is_a_denied_result_and_the_turn_goes_on` FAILS (the result says "interactive approval failed before tool dispatch"); `a_prompt_that_fails_…` passes.

- [ ] **Step 3: The gate**

In `tool_execution.rs`, in the `match self.runtime.resolve_interactive_tool_approval(…)`, add an arm before the final `Err(error) =>`:

```rust
                        // Nobody may be asked on this thread: the call is
                        // refused and the turn goes on, as it does when a
                        // rule denies a call the model can work around.
                        Err(error) if crate::child_refusal::refused_without_asking(&error) => {
                            let result =
                                tool_types::ToolResult::denied(tool_request, error.to_string());
                            return finish_approval_gate_terminal(
                                events,
                                sink,
                                &event_request,
                                emit_deltas,
                                provider_response_ingress,
                                event_error,
                                RunStatus::Success,
                                result,
                            );
                        }
```

If `resolve_interactive_tool_approval` wraps the handler's error so the payload is lost, make it pass the error through unchanged and say so in the report.

Run: `cargo test -p orca-runtime a_refusal_without_asking a_prompt_that_fails`
Expected: both PASS.

- [ ] **Step 4: Give the handlers to a child that is kept**

In `runtime_subagent_call.rs`, `run_threaded_agent_worker`: when `controller.keeps_subagents_in_process()`, the launch request carries

```rust
        approval_handler: Some(Arc::new(crate::child_refusal::RefusingApprovalHandler)),
        permission_handler: Some(Arc::new(crate::child_refusal::RefusingPermissionHandler)),
```

instead of `approval_handler: None` and the `ChildPermissionHandler` built from the parent's handler; otherwise both stay as they are today. `AgentController::launch` wraps the permission handler it is given in `AgentPermissionHandler`, which reports the request on the agent's row: read it, and if it would show `needs approval` for a request that is refused at once, pass the refusing handler through unwrapped for a kept child. Replace the comment above `approval_handler: None` with one that states both cases. The types are those `AgentLaunchRequest` already declares; if `approval_handler`'s type is not `Option<Arc<dyn RuntimeApprovalHandler + Send + Sync>>`, adapt `RefusingApprovalHandler` to it rather than changing the field.

In `agent_controller.rs`, `launch`: the pinned context becomes

```rust
            &format!(
                "{DELEGATED_CHILD_CONTEXT}{}{}",
                request.subagent_type.system_prompt_suffix(),
                if self.keeps_subagents_in_process() {
                    crate::child_refusal::UNATTENDED_CHILD_CONTEXT
                } else {
                    ""
                }
            ),
```

(keeping whatever wraps that `format!` today).

- [ ] **Step 5: A question to the user and an MCP elicitation**

Find what a child's `ask_user`-style tool and an MCP elicitation do when `PromptQueueInteractionHandlers.user_input` and `.mcp_elicitation` are `None` on a turn started by Task 2 (`grep -n "user_input\b\|mcp_elicitation\b" crates/orca-runtime/src/runtime_host.rs | head -30`, then read the handler they fall back to). The requirement: on a child that is kept, each ends that one tool call with a refusal the model reads, and the turn goes on; neither waits and neither ends the turn. If the fallback already does that, pin it with a test named `a_kept_childs_question_is_refused_and_the_turn_goes_on`. If it waits or ends the turn, add refusing handlers for both to `child_refusal.rs`, carry them in `DelegatedTurnStart` (two more `Option` fields) and through the raw fallback, and test the same. If a subagent is not offered such a tool at all, pin that with a test on the child's tool list and say so in the report.

- [ ] **Step 6: The whole path**

Add, beside Task 2's tests:

```rust
#[test]
fn a_kept_child_in_suggest_is_refused_and_still_replies() {
    // Host: keep_subagents_in_process(). Child config: ApprovalMode::Suggest.
    // Scripted provider: first a bash tool call `echo hi`, then, after the
    // tool result, a text reply "could not run echo".
    // Assert:
    //  - launch returns Ok and the reply text is "could not run echo";
    //  - the tool result the provider was sent for the bash call contains
    //    crate::child_refusal::APPROVAL_REFUSED;
    //  - the child's pinned context contains
    //    crate::child_refusal::UNATTENDED_CHILD_CONTEXT.
}

#[test]
fn a_child_of_a_host_that_did_not_ask_keeps_todays_handlers() {
    // Same without keep_subagents_in_process(): the pinned context does not
    // contain UNATTENDED_CHILD_CONTEXT.
}
```

Run: `cargo test -p orca-runtime a_kept_child a_child_of_a_host child_refusal a_refusal_without_asking`
Expected: all PASS.

- [ ] **Step 7: Validators and commit**

```bash
for f in scripts/validate-*.mjs scripts/test-validate-*.mjs; do node "$f" >/dev/null || echo "FAILED $f"; done
git add -A crates/orca-runtime
git commit -m "feat(runtime): a subagent kept in the process is refused what it would have to ask for"
```

---

### Task 5: The list keeps the order agents were started in

**Files:**
- Modify: `crates/orca-core/src/task_types.rs:174-255` (`BackgroundTaskSummary`), `crates/orca-runtime/src/tasks.rs:6791` (`task_summary`), every other constructor of `BackgroundTaskSummary` the compiler names
- Modify: `crates/orca-tui/src/workflow_panel.rs:730-741` (`sort_workflow_tasks_for_panel`) and its tests

**Interfaces:**
- Produces: `BackgroundTaskSummary.sequence: u64` (serialised as `sequence`, `#[serde(default)]`, `0` meaning "not known").

- [ ] **Step 1: Write the failing test**

In `workflow_panel.rs`'s test module, with the helper the neighbouring tests use to build a `BackgroundTaskSummary` (give it `sequence`):

```rust
    #[test]
    fn agents_keep_the_order_they_were_started_in() {
        // a: sequence 1, Running, last_activity_at_ms 10
        // b: sequence 2, Completed, last_activity_at_ms 99
        // c: sequence 3, ApprovalRequired, last_activity_at_ms 50
        let sorted = super::sort_workflow_tasks_for_panel(vec![c.clone(), b.clone(), a.clone()]);
        assert_eq!(ids(&sorted), ["a", "b", "c"]);

        // Activity does not move a row.
        let mut a_later = a.clone();
        a_later.last_activity_at_ms = Some(500);
        let sorted = super::sort_workflow_tasks_for_panel(vec![b.clone(), c.clone(), a_later]);
        assert_eq!(ids(&sorted), ["a", "b", "c"]);
    }

    #[test]
    fn tasks_with_no_sequence_follow_in_creation_order() {
        // x: sequence 0, created_at_ms 5;  y: sequence 0, created_at_ms 3;
        // a: sequence 1, created_at_ms 9.
        let sorted = super::sort_workflow_tasks_for_panel(vec![x, y, a]);
        assert_eq!(ids(&sorted), ["a", "y", "x"]);
    }
```

- [ ] **Step 2: Run and see it fail**

Run: `cargo test -p orca-tui agents_keep_the_order tasks_with_no_sequence`
Expected: does not compile (`sequence` is not a field).

- [ ] **Step 3: The field**

In `task_types.rs`, in `BackgroundTaskSummary`, after `created_at_ms`:

```rust
    /// Submission order within the task registry; `0` when not known.
    /// Rows are listed by it, so a row keeps its place.
    #[serde(default)]
    pub sequence: u64,
```

In `tasks.rs`, `task_summary`: `sequence: record.sequence,`. Every other constructor gets `sequence: 0`.

- [ ] **Step 4: The order**

```rust
pub(crate) fn sort_workflow_tasks_for_panel(
    mut tasks: Vec<BackgroundTaskSummary>,
) -> Vec<BackgroundTaskSummary> {
    // The order tasks were submitted in: a row keeps its place whatever it
    // does. A task with no sequence (an older record) follows, by creation.
    tasks.sort_by(|left, right| {
        (left.sequence == 0)
            .cmp(&(right.sequence == 0))
            .then_with(|| left.sequence.cmp(&right.sequence))
            .then_with(|| left.created_at_ms.cmp(&right.created_at_ms))
            .then_with(|| left.id.cmp(&right.id))
    });
    tasks
}
```

Remove `workflow_task_panel_group` and `workflow_task_activity_ms` if nothing else uses them. Tests that pinned the old order (approval first, latest activity first) are rewritten to the new rule and listed in the report.

- [ ] **Step 5: Run**

```bash
cargo test -p orca-tui workflow_panel 2>&1 | tail -6
cargo test -p orca-core task_types 2>&1 | tail -4
cargo test -p orca-runtime task_summary 2>&1 | tail -4
```

Expected: all pass.

- [ ] **Step 6: Validators and commit**

```bash
for f in scripts/validate-*.mjs scripts/test-validate-*.mjs; do node "$f" >/dev/null || echo "FAILED $f"; done
git add -A crates
git commit -m "feat(tui): agents are listed in the order they were started and keep their place"
```

---

### Task 6: An agent that is writing can be opened

**Files:**
- Modify: `crates/orca-tui/src/hosted_child.rs:814-858` (`child_binding`) and the comment at `614-617`

**Interfaces:**
- Produces: `child_binding` accepts a task whose revision is at or past the one the TUI saw; the second read in `focus_child` still requires the same bound thread.

- [ ] **Step 1: Write the failing tests**

In `hosted_child.rs`'s test module, with the snapshot builder its existing `child_binding` tests use:

```rust
    #[test]
    fn a_child_whose_revision_moved_on_is_still_the_one_selected() {
        // Snapshot: task "child" at revision 7, bound to a running subagent
        // on thread T.
        let binding = super::child_binding(&snapshot, "child", 3).expect("a newer revision is the same task");
        assert_eq!(binding.child_thread_id, thread_t);
    }

    #[test]
    fn a_snapshot_older_than_what_was_seen_is_refused() {
        // Snapshot: task "child" at revision 2.
        let error = super::child_binding(&snapshot, "child", 3).expect_err("older than what was seen");
        assert_eq!(error, "the selected child task is stale (expected revision 3)");
    }
```

- [ ] **Step 2: Run and see the first fail**

Run: `cargo test -p orca-tui a_child_whose_revision_moved_on a_snapshot_older_than`
Expected: the first FAILS with "the selected child task is stale (expected revision 3)"; the second passes.

- [ ] **Step 3: The check**

```rust
    // A child that is writing moves its task's revision several times a
    // second, so the revision seen at the click is behind by the time this
    // runs. The task is the same task: its identity and the thread it is
    // bound to are what must hold, and `focus_child` reads the binding twice
    // to see that the thread did not change. Only a snapshot from before the
    // click is stale.
    if task.revision.get() < expected_revision {
        return Err(format!(
            "the selected child task is stale (expected revision {expected_revision})"
        ));
    }
```

and replace the comment in `focus_child` above the second read with:

```rust
    // The host lookup can race the child's terminal commit. Read the parent
    // surface again and require the task to be bound to the same thread
    // before changing what is on screen.
```

Existing tests that expected a refusal for a newer revision are rewritten to expect the binding and listed in the report.

- [ ] **Step 4: Run and commit**

```bash
cargo test -p orca-tui hosted_child 2>&1 | tail -6
git add crates/orca-tui/src/hosted_child.rs
git commit -m "fix(tui): an agent whose task moved on since the click can still be opened"
```

Expected: all pass.

---

### Task 7: The view follows an agent from before its first turn to after its last

**Files:**
- Modify: `crates/orca-tui/src/hosted_child.rs:212-343` (`run_child_surface_bridge`) and its tests

**Interfaces:**
- Consumes: a running kept child has a foreground surface operation (Task 2).
- Produces: `run_child_surface_bridge` does not return when the child has no operation in flight; it follows each operation the child starts and ends when asked to stop, when the subscription is sealed, or when the event receiver is gone.

Behaviour, replacing "return when `operation_id` is `None`" and "break at the operation's terminal":

1. The bridge keeps an `Option<SurfaceOperationId>`: the operation it is following, if any.
2. Every batch is projected and its events sent, whether or not an operation is being followed. (Today nothing is read without one.)
3. When no operation is followed and a batch carries an operation patch that starts or admits an operation (read `orca_runtime::surface::OperationPatch` for the variants that carry a record which is not terminal), that operation becomes the one followed.
4. When the followed operation's `OperationPatch::Terminal` arrives, the bridge stops following it and goes on reading; it does not detach.
5. The bridge ends, detaching the surface and clearing the focused child surface as today, only when `stop` is set, when sending an event fails, when projection or recovery fails, or on `Sealed`. A `Sealed` after the last followed operation ended is the normal end of a finished child and sends no error; a `Sealed` while an operation is followed keeps today's error line.
6. `recover_child_surface` already takes `&mut operation_id`; change its parameter to the `Option` and let it set it from the recovered snapshot's foreground operation.

- [ ] **Step 1: Write the failing tests**

In `hosted_child.rs`'s test module, with the fixture the existing bridge tests use (a real host and a child thread with a scripted provider, or the fake subscription if that is what they use):

```rust
    #[test]
    fn a_child_opened_before_its_turn_starts_is_followed_once_it_does() {
        // Attach the bridge to a child thread that has no operation yet.
        // Then start a turn on the child whose provider replies "hello".
        // Assert the bridge's event channel receives the assistant text
        // "hello" (as the projected TuiEvents the other bridge tests match
        // on), i.e. the bridge did not return when it found no operation.
    }

    #[test]
    fn the_view_goes_on_to_the_childs_next_turn() {
        // With the bridge attached: run a turn replying "one", wait for its
        // terminal, then run a second turn replying "two".
        // Assert both "one" and "two" arrive, in that order, and that no
        // TuiEvent::Error arrives.
    }
```

- [ ] **Step 2: Run and see them fail**

Run: `cargo test -p orca-tui a_child_opened_before_its_turn the_view_goes_on_to -- --nocapture`
Expected: both FAIL by timing out waiting for the text (the bridge returned).

- [ ] **Step 3: Implement the behaviour above**

Keep the function's shape: one loop over `subscription.recv_timeout(Duration::from_millis(50))`, the same handling of interactions, the same cleanup at the end. Do not add a second thread or a polling read of the snapshot.

- [ ] **Step 4: Run and commit**

```bash
cargo test -p orca-tui hosted_child 2>&1 | tail -6
git add crates/orca-tui/src/hosted_child.rs
git commit -m "feat(tui): an agent's view follows it from before its first turn through each turn after"
```

Expected: all pass, including the tests that were there.

---

### Task 8: A message in an agent's view is handed back

Until stage 3 gives an agent its own queue and steer, a message typed in an agent's conversation goes nowhere it should. This replaces "handed back only while main runs" with "handed back".

**Files:**
- Modify: `crates/orca-tui/src/hosted_view.rs:46-51` (`OutOfStep`), `crates/orca-tui/src/operation_controller.rs:422-436`, `crates/orca-tui/src/action_dispatcher.rs:19-26` and `:225`
- Modify: `crates/orca-tui/src/ui.rs` (`main_behind_child_line`)

**Interfaces:**
- Produces: `OutOfStep::AgentOnScreen`; `message_would_miss_the_conversation_in_view` returns it whenever a child is on screen, whether or not a turn is running.

Exact strings:

```rust
const AGENT_ON_SCREEN: &str =
    "this is an agent's conversation: return to the main conversation to send a message";
```

The row under an agent's conversation, in `main_behind_child_line`: `Main is running · Esc return` (was `Main is running · send after it finishes · Esc return`); the approval and answer variants keep their wording.

- [ ] **Step 1: Write the failing tests**

In `operation_controller.rs`'s test module, beside `a_message_is_never_refused_while_main_is_on_screen`:

```rust
    #[test]
    fn a_message_in_an_agents_view_is_handed_back_whether_or_not_a_turn_runs() {
        // Bind a view and give it a focus the way the neighbouring tests do.
        // With no surface operation active and none armed:
        assert_eq!(
            control.message_would_miss_the_conversation_in_view(),
            Some(crate::hosted_view::OutOfStep::AgentOnScreen)
        );
    }
```

In `action_dispatcher.rs`'s test module, beside `a_refused_message_comes_back_with_its_text`:

```rust
    #[test]
    fn a_message_typed_in_an_agents_view_comes_back_with_its_text() {
        // Same fixture, view focused on a child, no turn running.
        // Send UserAction::Submit { prompt: "hello", .. }.
        // Assert the event is the rejection that carries "hello" back to the
        // composer with the message super::AGENT_ON_SCREEN, and that nothing
        // reached the controller's command channel.
    }
```

In `ui.rs`'s tests for `main_behind_child_line`, change the expected running line to `Main is running · Esc return`.

- [ ] **Step 2: Run and see them fail**

Run: `cargo test -p orca-tui a_message_in_an_agents_view a_message_typed_in_an_agents_view main_behind_child`
Expected: compile error (`AgentOnScreen`), then failures.

- [ ] **Step 3: Implement**

`hosted_view.rs`:

```rust
pub(crate) enum OutOfStep {
    /// A child is on screen; a message typed there has nowhere to go.
    AgentOnScreen,
    /// The controller still operates on a child that is not on screen.
    AgentRuns,
}
```

Remove `MainRuns`: with a child on screen the answer is `AgentOnScreen` whatever the controller does. `out_of_step(on_screen, adopted)` returns `Some(AgentOnScreen)` when `on_screen.is_some()`, `Some(AgentRuns)` when `on_screen.is_none() && adopted.is_some()`, else `None`; update its unit tests.

`operation_controller.rs`:

```rust
    pub(crate) fn message_would_miss_the_conversation_in_view(
        &self,
    ) -> Option<crate::hosted_view::OutOfStep> {
        let (view, turn_active) = {
            let hosted = self.lock_hosted();
            (
                hosted.view.clone(),
                hosted.surface_active.is_some() || hosted.surface_activation_armed,
            )
        };
        let view = view?;
        if view.has_focus() {
            return Some(crate::hosted_view::OutOfStep::AgentOnScreen);
        }
        if !turn_active {
            return None;
        }
        view.out_of_step()
    }
```

`action_dispatcher.rs`: replace the `MAIN_STILL_RUNNING` constant with `AGENT_ON_SCREEN` (string above) and map `OutOfStep::AgentOnScreen => AGENT_ON_SCREEN`; keep `AGENT_STILL_RUNNING`. Update the tests that named `MAIN_STILL_RUNNING`.

`ui.rs`: the running variant of `main_behind_child_line` reads `Main is running · Esc return`.

- [ ] **Step 4: Run and commit**

```bash
cargo test -p orca-tui hosted_view operation_controller action_dispatcher 2>&1 | tail -8
cargo test -p orca-tui main_behind_child 2>&1 | tail -4
git add -A crates/orca-tui
git commit -m "feat(tui): a message typed in an agent's conversation is handed back"
```

Expected: all pass.

---

### Task 9: Quitting says agents are running, and stops them

**Files:**
- Modify: `crates/orca-tui/src/global_actions.rs:44-57` and its tests
- Modify: `crates/orca-tui/src/hosted_controller.rs:1263-1276` (the loop's exit)
- Modify: `crates/orca-runtime/src/runtime_host.rs` (`impl RuntimeHostHandle`), with whatever of the task registry it needs

**Interfaces:**
- Consumes: `RuntimeHostHandle::keep_subagents_in_process` (Task 3).
- Produces: `pub fn RuntimeHostHandle::stop_kept_subagents(&self, wait: Duration) -> usize` — asks every running subagent kept in this process to stop, waits up to `wait` for them to settle, and returns how many were still running when it gave up.

Exact strings, in `global_actions.rs`:

```rust
fn quit_line(running_agents: usize) -> String {
    match running_agents {
        0 => "Press Ctrl+C again to quit.".to_string(),
        1 => "1 agent is still running. Press Ctrl+C again to quit and stop it.".to_string(),
        n => format!("{n} agents are still running. Press Ctrl+C again to quit and stop them."),
    }
}
```

- [ ] **Step 1: Write the failing tests**

In `global_actions.rs`'s test module, beside the test that asserts `Exit(130)` on the second press:

```rust
    #[test]
    fn the_quit_line_counts_agents_that_would_be_stopped() {
        assert_eq!(super::quit_line(0), "Press Ctrl+C again to quit.");
        assert_eq!(
            super::quit_line(1),
            "1 agent is still running. Press Ctrl+C again to quit and stop it."
        );
        assert_eq!(
            super::quit_line(2),
            "2 agents are still running. Press Ctrl+C again to quit and stop them."
        );
    }

    #[test]
    fn the_first_ctrl_c_says_how_many_agents_are_running() {
        // State: idle, with two workflow tasks of TaskType::Subagent, status
        // Running, that have a subagent_child_thread_id, and one Completed.
        // Press Ctrl+C once.
        // Assert the last message is ChatMessage::System with
        // "2 agents are still running. Press Ctrl+C again to quit and stop them."
    }
```

In `runtime_host.rs`'s tests (or the module that tests the task registry with a host), with the fixture Task 2's tests use:

```rust
    #[test]
    fn stopping_kept_subagents_leaves_none_running() {
        // Host: keep_subagents_in_process(). Launch one Async child whose
        // provider blocks until cancelled.
        // let left = host.stop_kept_subagents(Duration::from_secs(5));
        // Assert left == 0, and the child's task record is terminal with the
        // status a person's stop gives today (Stopped or Cancelled — the
        // one `request_stop` produces), not Running and not indeterminate.
    }
```

- [ ] **Step 2: Run and see them fail**

Run: `cargo test -p orca-tui the_quit_line the_first_ctrl_c_says` and `cargo test -p orca-runtime stopping_kept_subagents`
Expected: compile errors for the missing function and method.

- [ ] **Step 3: The line**

In `global_actions.rs`, add `quit_line` and use it where `"Press Ctrl+C again to quit."` is pushed:

```rust
            let running_agents = state
                .workflow_tasks()
                .iter()
                .filter(|task| {
                    task.task_type == orca_core::task_types::TaskType::Subagent
                        && task.status.is_active()
                        && task.subagent_child_thread_id.is_some()
                })
                .count();
            state.push_message(ChatMessage::System {
                text: quit_line(running_agents),
                expanded: false,
            });
```

Only agents with a thread are counted: a detached one goes on after the quit, as today.

- [ ] **Step 4: Stopping them**

`RuntimeHostHandle::stop_kept_subagents`: for every running task of type subagent that is bound to a hosted child thread of this host, request a stop the way `StopTask` does today (follow `TuiSurfaceTaskControl::stop_background_task` in `crates/orca-tui/src/operation_controller.rs` down to the registry call it ends in, and call that), then poll the registry every 50 ms until none is active or `wait` has passed. It does nothing and returns `0` when the host was not asked to keep subagents.

In `hosted_controller.rs`, at the loop's exit, before `control.unbind_view()`:

```rust
    // The agents this TUI started run on its threads and end with it: stop
    // them so their tasks are recorded as stopped, not left as running.
    let _ = host.stop_kept_subagents(Duration::from_secs(2));
```

using the host handle the loop already holds (the one passed to `HostedView::new`).

- [ ] **Step 5: Run**

```bash
cargo test -p orca-tui global_actions 2>&1 | tail -5
cargo test -p orca-runtime stopping_kept_subagents 2>&1 | tail -5
cargo test -p orca-tui the_controller_binds_the_view 2>&1 | tail -4
```

Expected: all pass.

- [ ] **Step 6: Validators and commit**

```bash
for f in scripts/validate-*.mjs scripts/test-validate-*.mjs; do node "$f" >/dev/null || echo "FAILED $f"; done
git add -A crates
git commit -m "feat(tui): quitting says how many agents are running and stops them"
```

---

### Task 10: What a person is told, the contract tests, and the terminal

**Files:**
- Modify: `docs/subagents.md` (the sections at `100-236` that describe where a subagent runs, what it may ask and what a quit does)
- Modify: `tests/tui_pty_contract.rs:551` (`tui_tasks_workspace_stops_one_detached_subagent_without_terminal_spam`)
- Modify: `docs/superpowers/specs/2026-11-24-live-subagent-conversations-design.md` (record the run under "To verify first")
- Create: `.superpowers/sdd/2026-11-24-live-subagent-conversations-stage-1/task-10-report.md`

- [ ] **Step 1: The document**

In `docs/subagents.md`, in the document's own voice and language, state for a person using the TUI:
- a subagent started from the TUI runs inside the TUI and ends when the TUI quits; `orca exec` is unchanged and its subagents finish after it exits;
- such a subagent never asks: the table of the four modes from the spec's "Permissions", and that what it could not do is in its reply;
- subagents with isolation, of a custom type, with a finite budget or continued with `resume_from` run as before;
- its conversation can be opened while it runs; a message cannot be sent to it yet.

- [ ] **Step 2: The pty contract**

`tui_tasks_workspace_stops_one_detached_subagent_without_terminal_spam` starts a plain subagent from the TUI, which is no longer detached. Read the test. Keep what it protects — one agent is stopped from the tasks workspace and the terminal is not flooded — and change what is no longer true: rename it `tui_tasks_workspace_stops_one_subagent_without_terminal_spam`, and replace any assertion that a worker process exists or dies with the assertion that the task ends stopped. If the test relies on the child outliving the TUI, that part is deleted and named in the report.

Run: `cargo test --test tui_pty_contract tui_tasks_workspace_stops_one 2>&1 | tail -6`
Expected: PASS.

- [ ] **Step 3: The suites**

```bash
cargo test -p orca-runtime 2>&1 | tail -6
cargo test -p orca-tui 2>&1 | tail -6
cargo test --test subagent_contract --test subagent_recovery_contract --test tui_pty_contract 2>&1 | tail -8
cargo clippy -p orca-runtime -p orca-tui -p orca-core 2>&1 | grep -c "^warning" 
for f in scripts/validate-*.mjs scripts/test-validate-*.mjs; do node "$f" >/dev/null || echo "FAILED $f"; done
```

A test that fails is run again alone, three times. One that passes alone each time is recorded as load-dependent with its name; one that fails alone is a regression and is fixed before going on. The clippy count is compared with the count at `d5c959c3` for the same command; a new warning in a file this plan touched is fixed.

- [ ] **Step 4: The terminal, auto-edit**

Build `feat/live-subagent-view`. Temporary `ORCA_HOME` with a copy of `auth.json` and no `mode` line, an empty working directory, pty 150x50 read with `pyte`, `deepseek-flash`, no environment switches. Prompt:

```text
Start two subagents in parallel, named alpha and beta. Each must: write a file named after itself (alpha.txt, beta.txt) with one sentence, run `ls -a && cat <its file>`, then write an 800-word essay about its own name in its reply. Wait for both and tell me the first sentence of each essay.
```

One frame a second to `/tmp/orca-stage1/run-auto/NNN.txt`. While all three run: Shift+Down, Enter on the first agent; watch 15 s; type `hello` and Enter; Esc; wait for main to finish. Record, from the frames:

1. No `orca subagent-worker` process exists during the run (`pgrep -f subagent-worker` from the driver).
2. Both files exist with their sentence; no prompt and no `needs approval` appeared.
3. In the agent's view: the task as a user message, the tool calls and results, and two frames a second apart in which the essay grew. Quote the last line of each.
4. `hello` came back to the composer with `this is an agent's conversation: return to the main conversation to send a message`, and the agent went on.
5. The two agent rows kept their order in every frame from when both were listed until main finished.
6. Main's final reply quotes both first sentences.

- [ ] **Step 5: The terminal, suggest**

Same, with `mode = "suggest"` in `config.toml`, frames to `/tmp/orca-stage1/run-suggest/`. Approve main's own `subagent` calls when main asks. Record:

1. Each agent's write and command were refused with a result that contains `This call needs an approval, and a subagent cannot ask for one.`; no prompt for an agent's call appeared; neither agent stopped at the refusal.
2. Each agent's reply says what it could not do, and main's reply reflects it.
3. `alpha.txt` and `beta.txt` do not exist.

- [ ] **Step 6: The terminal, quit**

Auto-edit again with essays of 3000 words, frames to `/tmp/orca-stage1/run-quit/`. Once both agents are running, with main idle or running: Ctrl+C. If main is running the first Ctrl+C interrupts it; press again once it is idle. Record the line shown (`2 agents are still running. Press Ctrl+C again to quit and stop them.`), press Ctrl+C again, and after the process exits record: the exit code, that no process started by this run is left (`pgrep -P <the TUI's pid>` taken before the quit, each pid checked after it), and the two tasks' status in the temporary `ORCA_HOME`'s task registry (stopped, not running).

- [ ] **Step 7: Decide and record**

Delete the `auth.json` copies. Write the report with every numbered observation, pass or fail, and the frame that shows it.

- If every observation of steps 4 to 6 holds: add to the spec, under "To verify first", a paragraph dated today that items 1 to 4 were run and what was seen, then

```bash
git add -A docs tests
git commit -m "docs: subagents of the TUI run inside it; what the stage 1 run showed"
```

- If any does not hold: **do not commit the spec change as passed.** Record what was seen in the report and in the spec as it was seen, commit that, and stop for a decision. Stage 2 is not planned on a stage 1 that the terminal did not confirm.
