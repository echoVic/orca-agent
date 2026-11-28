# Live Subagent View Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A running subagent's live conversation opens at once whether or not the main agent is running, and returning shows the main conversation whole.

**Architecture:** Focus and return stop being controller actions. A shared `HostedView` holds what is on screen (the main binding and the child focus); the dispatcher's thread performs focus and return against it with the existing `focus_child` / `return_to_parent`, and the controller only follows the view between actions (`HostedView::adopt`), which is also when the prompt queue is rebound. A message sent from a child's view while the controller still runs the main turn is handed back by the dispatcher.

**Tech Stack:** Rust 2024, `crossbeam_channel`, `ratatui` 0.30, the `orca-runtime` surface (snapshot + subscription), `orca-tui` attachment routing.

**Spec:** `docs/superpowers/specs/2026-11-23-live-subagent-view-and-agents-card-design.md` (Stage 1). Not in this plan, each with its own: read-only replay of finished agents (it needs the saved-session read verified first); the card, and with it the status line naming the agent on screen (Stage 2).

## Global Constraints

- All changes are in `crates/orca-tui`. No new `orca_runtime::surface` export, no new `libc`/`std::os::unix`/`windows-sys` call.
- The controller's operating thread changes only between actions. Nothing in `HostedView::handle` may assign the controller's `thread`, `event_tx` or `session_attachment`.
- `announce_runtime_ready` is called only by the controller thread.
- Lock order: `HostedView` lock, then `AttachmentRouting` or the control's lock. Never take the `HostedView` lock while holding the control's lock.
- A subagent with no `subagent_child_thread_id`, or one that has finished, opens the Agents panel exactly as today.
- Commit messages follow the repo's form, a sentence saying what is now true: `feat(tui): …`, `test(tui): …`.
- Before the last commit: `cargo test -p orca-tui`, `cargo clippy -p orca-tui --all-targets -- -D warnings`, and every `scripts/validate-*.mjs` with its `scripts/test-validate-*.mjs`.

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `src/hosted_view.rs` | create | What is on screen: main binding, child focus, focus/return, adoption |
| `src/lib.rs` | modify | `mod hosted_view;` |
| `src/operation_controller.rs` | modify | Holds the bound view; says when a message would miss the child in view |
| `src/hosted_child.rs` | modify | `focus_child` / `return_to_parent` stop rebinding the runtime |
| `src/hosted_controller.rs` | modify | Loop follows the view instead of owning `child_focus` |
| `src/action_dispatcher.rs` | modify | Focus/return go to the view; messages behind a running main are handed back |
| `src/agent_workspace_actions.rs` | modify | A live child opens its conversation regardless of `AppStatus` |
| `src/types.rs`, `src/state_reducer.rs`, `src/ui.rs` | modify | "Main is running" row under a child's conversation |

All paths below are relative to `crates/orca-tui/`.

---

### Task 1: `HostedView`

**Files:**
- Create: `src/hosted_view.rs`
- Modify: `src/lib.rs:40` (add the module next to `mod hosted_child;`)
- Modify: `src/operation_controller.rs:559-563` (`runtime_thread` loses `#[cfg(test)]`)

**Interfaces:**
- Consumes: `hosted_child::{HostedChildAction, HostedChildFocus, handle_hosted_child_action}` (existing, `pub(crate)`), `TuiSurfaceTaskControl::{runtime_thread, restrict_hosted_settings}`.
- Produces:
  - `pub(crate) enum Adoption { Unchanged, Child, Main }`
  - `pub(crate) fn adoption(on_screen: Option<SessionAttachmentId>, adopted: Option<SessionAttachmentId>) -> Adoption`
  - `HostedView::new(host: RuntimeHostHandle, root_event_tx: mpsc::Sender<TuiEvent>, routing: Arc<Mutex<AttachmentRouting>>, event_tx: mpsc::Sender<TuiEvent>, attachment: SessionAttachmentId) -> HostedView`
  - `HostedView::handle(&self, action: HostedChildAction, control: &TuiSurfaceTaskControl)`
  - `HostedView::adopt(&self, thread: &mut Option<RuntimeThreadHandle>, event_tx: &mut mpsc::Sender<TuiEvent>, attachment: &mut SessionAttachmentId, side_active: bool) -> Adoption`
  - `HostedView::has_focus(&self) -> bool`, `HostedView::awaits_main(&self) -> bool`, `HostedView::take_focus(&self) -> Option<HostedChildFocus>`

- [ ] **Step 1: Write the failing test**

Create `src/hosted_view.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::{Adoption, adoption};
    use crate::protocol::SessionAttachmentId;

    fn id(value: u64) -> SessionAttachmentId {
        SessionAttachmentId::new(value)
    }

    #[test]
    fn the_controller_follows_what_is_on_screen() {
        // Nothing focused, nothing adopted: the controller stays on main.
        assert_eq!(adoption(None, None), Adoption::Unchanged);
        // A child came on screen while the controller was on main.
        assert_eq!(adoption(Some(id(2)), None), Adoption::Child);
        // The controller already operates on the child on screen.
        assert_eq!(adoption(Some(id(2)), Some(id(2))), Adoption::Unchanged);
        // Another child replaced it while the controller was busy.
        assert_eq!(adoption(Some(id(3)), Some(id(2))), Adoption::Child);
        // The view went back to main; the controller still holds the child.
        assert_eq!(adoption(None, Some(id(2))), Adoption::Main);
    }
}
```

Add to `src/lib.rs`, on the line after `mod hosted_child;`:

```rust
mod hosted_view;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p orca-tui hosted_view::`
Expected: FAIL to compile, `unresolved imports super::Adoption, super::adoption`.

- [ ] **Step 3: Write the implementation**

Put this above the test module in `src/hosted_view.rs`:

```rust
//! What is on screen, kept apart from what the controller operates on.
//!
//! The controller takes one action at a time, and a turn keeps it until the
//! turn ends. Showing a child's conversation must not wait for that: the
//! focus and the return are done here, on the thread that asks (the
//! dispatcher's), and the controller only follows between actions
//! ([`HostedView::adopt`]). Until it has followed, the controller still
//! operates on the main conversation and the prompt queue stays bound to it.

use std::sync::{Arc, Mutex, MutexGuard};

use crossbeam_channel as mpsc;
use orca_runtime::runtime_host::{RuntimeHostHandle, RuntimeThreadHandle};

use crate::attachment_routing::AttachmentRouting;
use crate::hosted_child::{HostedChildAction, HostedChildFocus, handle_hosted_child_action};
use crate::operation_controller::TuiSurfaceTaskControl;
use crate::protocol::{SessionAttachmentId, TuiEvent};

/// How the controller's operating thread must move to match the screen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Adoption {
    Unchanged,
    /// Take up the child on screen.
    Child,
    /// Go back to the main conversation.
    Main,
}

/// `on_screen` is the attachment of the child in view, `adopted` that of
/// the child the controller operates on; `None` is the main conversation.
pub(crate) fn adoption(
    on_screen: Option<SessionAttachmentId>,
    adopted: Option<SessionAttachmentId>,
) -> Adoption {
    match (on_screen, adopted) {
        (Some(on_screen), Some(adopted)) if on_screen == adopted => Adoption::Unchanged,
        (Some(_), _) => Adoption::Child,
        (None, Some(_)) => Adoption::Main,
        (None, None) => Adoption::Unchanged,
    }
}

/// The main conversation as the controller last held it.
struct MainBinding {
    thread: Option<RuntimeThreadHandle>,
    event_tx: mpsc::Sender<TuiEvent>,
    attachment: SessionAttachmentId,
}

struct Inner {
    main: MainBinding,
    side_active: bool,
    focus: Option<HostedChildFocus>,
    adopted: Option<SessionAttachmentId>,
}

#[derive(Clone)]
pub(crate) struct HostedView {
    inner: Arc<Mutex<Inner>>,
    host: RuntimeHostHandle,
    root_event_tx: mpsc::Sender<TuiEvent>,
    routing: Arc<Mutex<AttachmentRouting>>,
}

impl std::fmt::Debug for HostedView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostedView").finish_non_exhaustive()
    }
}

impl HostedView {
    pub(crate) fn new(
        host: RuntimeHostHandle,
        root_event_tx: mpsc::Sender<TuiEvent>,
        routing: Arc<Mutex<AttachmentRouting>>,
        event_tx: mpsc::Sender<TuiEvent>,
        attachment: SessionAttachmentId,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                main: MainBinding {
                    thread: None,
                    event_tx,
                    attachment,
                },
                side_active: false,
                focus: None,
                adopted: None,
            })),
            host,
            root_event_tx,
            routing,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Shows a child's conversation, or the main one again, on the calling
    /// thread. It changes what is on screen and nothing the controller
    /// holds.
    pub(crate) fn handle(&self, action: HostedChildAction, control: &TuiSurfaceTaskControl) {
        let mut guard = self.lock();
        let inner = &mut *guard;
        // Where the action starts from: the child in view, or else the main
        // conversation. A turn that started the main thread has not let the
        // controller publish it yet, so the control's bound thread is asked
        // first, unless that one is a child the controller adopted.
        let (mut thread, mut event_tx, mut attachment) = match inner.focus.as_ref() {
            Some(focus) => (
                Some(focus.child_thread.clone()),
                focus.child_event_tx.clone(),
                focus.child_attachment,
            ),
            None => (
                if inner.adopted.is_some() {
                    inner.main.thread.clone()
                } else {
                    control
                        .runtime_thread()
                        .or_else(|| inner.main.thread.clone())
                },
                inner.main.event_tx.clone(),
                inner.main.attachment,
            ),
        };
        if matches!(action, HostedChildAction::Focus { .. }) {
            // Direct settings stay unavailable throughout the change; the
            // controller sets them again as it follows.
            control.restrict_hosted_settings(true);
            if inner.focus.is_some() {
                handle_hosted_child_action(
                    HostedChildAction::Return,
                    &mut thread,
                    &mut inner.focus,
                    inner.side_active,
                    &self.host,
                    &self.root_event_tx,
                    &mut event_tx,
                    &mut attachment,
                    &self.routing,
                    control,
                );
            }
        }
        handle_hosted_child_action(
            action,
            &mut thread,
            &mut inner.focus,
            inner.side_active,
            &self.host,
            &self.root_event_tx,
            &mut event_tx,
            &mut attachment,
            &self.routing,
            control,
        );
    }

    /// Moves the controller's operating thread to what is on screen. Called
    /// by the controller between actions only. While it operates on main it
    /// also records main here, for the next focus to start from.
    pub(crate) fn adopt(
        &self,
        thread: &mut Option<RuntimeThreadHandle>,
        event_tx: &mut mpsc::Sender<TuiEvent>,
        attachment: &mut SessionAttachmentId,
        side_active: bool,
    ) -> Adoption {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let on_screen = inner.focus.as_ref().map(|focus| focus.child_attachment);
        let change = adoption(on_screen, inner.adopted);
        match change {
            Adoption::Child => {
                if inner.adopted.is_none() {
                    // The controller is about to leave main: keep it.
                    inner.main = MainBinding {
                        thread: thread.clone(),
                        event_tx: event_tx.clone(),
                        attachment: *attachment,
                    };
                }
                let focus = inner.focus.as_ref().expect("a child is on screen");
                *thread = Some(focus.child_thread.clone());
                *event_tx = focus.child_event_tx.clone();
                *attachment = focus.child_attachment;
                inner.adopted = on_screen;
            }
            Adoption::Main => {
                *thread = inner.main.thread.clone();
                *event_tx = inner.main.event_tx.clone();
                *attachment = inner.main.attachment;
                inner.adopted = None;
            }
            Adoption::Unchanged => {
                if inner.adopted.is_none() {
                    inner.main = MainBinding {
                        thread: thread.clone(),
                        event_tx: event_tx.clone(),
                        attachment: *attachment,
                    };
                    inner.side_active = side_active;
                }
            }
        }
        change
    }

    pub(crate) fn has_focus(&self) -> bool {
        self.lock().focus.is_some()
    }

    /// A child is on screen and the controller has not taken it up: what
    /// the controller is sent now still goes to the main conversation.
    pub(crate) fn awaits_main(&self) -> bool {
        let inner = self.lock();
        let on_screen = inner.focus.as_ref().map(|focus| focus.child_attachment);
        on_screen.is_some() && on_screen != inner.adopted
    }

    pub(crate) fn take_focus(&self) -> Option<HostedChildFocus> {
        self.lock().focus.take()
    }
}
```

In `src/operation_controller.rs`, delete the `#[cfg(test)]` line directly above `pub(crate) fn runtime_thread(&self)` (line 560) and replace its doc comment with:

```rust
    /// The runtime thread the controller operates on. `None` before the
    /// conversation starts.
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p orca-tui hosted_view::`
Expected: PASS, 1 test. Warnings that `HostedView` methods are unused are expected until Task 3.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-tui/src/hosted_view.rs crates/orca-tui/src/lib.rs crates/orca-tui/src/operation_controller.rs
git commit -m "feat(tui): what is on screen is held apart from what the controller operates on"
```

---

### Task 2: The control carries the view

**Files:**
- Modify: `src/operation_controller.rs:56-89` (`HostedOperationInner`), and next to `read_task_transcript` (line 389)

**Interfaces:**
- Consumes: `hosted_view::HostedView` (Task 1).
- Produces on `TuiSurfaceTaskControl`:
  - `pub(crate) fn bind_view(&self, view: HostedView)`
  - `pub(crate) fn unbind_view(&self)`
  - `pub(crate) fn view(&self) -> Option<HostedView>`
  - `pub(crate) fn message_would_miss_the_child_in_view(&self) -> bool`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module at the end of `src/operation_controller.rs`:

```rust
    #[test]
    fn a_message_is_only_refused_when_a_view_is_bound_and_a_turn_runs() {
        let control = TuiSurfaceTaskControl::isolated_for_test();
        assert!(control.view().is_none());
        assert!(
            !control.message_would_miss_the_child_in_view(),
            "no view, no turn: nothing to miss"
        );

        let host = orca_runtime::runtime_host::RuntimeHost::start_with_background_capacity(1)
            .expect("runtime host");
        let (event_tx, _event_rx) = crossbeam_channel::unbounded();
        let attachment = crate::protocol::SessionAttachmentId::new(1);
        control.bind_view(crate::hosted_view::HostedView::new(
            host.handle(),
            event_tx.clone(),
            std::sync::Arc::new(std::sync::Mutex::new(
                crate::attachment_routing::AttachmentRouting::new(attachment),
            )),
            event_tx,
            attachment,
        ));
        assert!(control.view().is_some());
        assert!(
            !control.message_would_miss_the_child_in_view(),
            "main is on screen"
        );

        control.unbind_view();
        assert!(control.view().is_none());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p orca-tui a_message_is_only_refused_when_a_view_is_bound_and_a_turn_runs`
Expected: FAIL to compile, `no method named view found for struct TuiSurfaceTaskControl`.

- [ ] **Step 3: Write the implementation**

In `HostedOperationInner`, after the `mcp_credentials_path` field and before `shutdown: bool,`:

```rust
    /// What is on screen, once the controller has started. The dispatcher
    /// changes it without waiting for the controller.
    view: Option<crate::hosted_view::HostedView>,
```

In `impl TuiSurfaceTaskControl`, directly after `read_task_transcript`:

```rust
    pub(crate) fn bind_view(&self, view: crate::hosted_view::HostedView) {
        self.lock_hosted().view = Some(view);
    }

    /// Lets go of the view as the controller ends: it holds the runtime
    /// host's handle.
    pub(crate) fn unbind_view(&self) {
        self.lock_hosted().view = None;
    }

    pub(crate) fn view(&self) -> Option<crate::hosted_view::HostedView> {
        self.lock_hosted().view.clone()
    }

    /// Whether a message sent now would go to the main conversation though
    /// a child's is on screen: the controller runs a turn and has not taken
    /// up the child in view.
    pub(crate) fn message_would_miss_the_child_in_view(&self) -> bool {
        let (view, turn_active) = {
            let hosted = self.lock_hosted();
            (hosted.view.clone(), hosted.surface_active.is_some())
        };
        turn_active && view.is_some_and(|view| view.awaits_main())
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p orca-tui a_message_is_only_refused_when_a_view_is_bound_and_a_turn_runs`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-tui/src/operation_controller.rs
git commit -m "feat(tui): the surface control carries the view the dispatcher changes"
```

---

### Task 3: The controller follows the view

**Files:**
- Modify: `src/hosted_child.rs:24-27` (imports), `:506-546` (`handle_hosted_child_action`), `:548-724` (`focus_child`), `:726-804` (`return_to_parent`)
- Modify: `src/hosted_controller.rs:299-500` (loop head and the two child arms), `:1220-1232` (exit)

**Interfaces:**
- Consumes: `HostedView::{new, handle, adopt, has_focus, take_focus}`, `Adoption` (Task 1); `TuiSurfaceTaskControl::{bind_view, unbind_view}` (Task 2).
- Produces: `fn follow_view(...) -> bool` in `hosted_controller.rs` (private). After this task `focus_child` and `return_to_parent` no longer call `announce_runtime_ready`.

- [ ] **Step 1: Write the failing test**

The existing test `hosted_controller_dispatches_rejections_in_fifo_order_and_exits_cleanly` (`src/hosted_controller.rs:1421`) already sends `FocusChildThread` before any conversation and expects `"focus a main conversation before selecting a child"`. Add next to it a test that the controller exits cleanly with a view bound and unbinds it:

```rust
    #[test]
    fn the_controller_binds_the_view_and_lets_it_go_as_it_ends() {
        let _home = crate::test_support::isolate_orca_home();
        let config = Arc::new(Mutex::new(crate::test_support::test_run_config()));
        let preloaded = Arc::new(Mutex::new(None));
        let (action_tx, action_rx) = mpsc::bounded(8);
        let (event_tx, _event_rx) = mpsc::unbounded();
        let controller = TuiSurfaceTaskControl::isolated_for_test();
        let observed = controller.clone();
        let pending = bridge::PendingWorkflowNotifications::new();
        let mut runtime = TuiAgentRuntime::spawn_hosted(
            action_rx,
            event_tx.clone(),
            8,
            controller,
            move |controller, commands, host| {
                hosted_tui_controller_loop(
                    config, preloaded, event_tx, commands, controller, pending, host,
                );
            },
        )
        .expect("hosted controller runtime");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while observed.view().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the controller binds the view as it starts"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        action_tx.send(UserAction::Cancel).expect("cancel action");
        runtime.shutdown().expect("hosted controller shutdown");
        assert!(observed.view().is_none(), "the view is let go at the end");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p orca-tui the_controller_binds_the_view_and_lets_it_go_as_it_ends`
Expected: FAIL after 5 s with `the controller binds the view as it starts`.

- [ ] **Step 3: `hosted_child.rs` stops rebinding the runtime**

Binding the prompt queue to a thread is the controller's, done as it follows the view.

1. In the `use crate::hosted_session::{…}` list (line 24), remove `announce_runtime_ready`.
2. In `focus_child`, delete the line `announce_runtime_ready(&child_thread, event_tx, control);` and replace the comment above it:

```rust
    // Start the relay only after the child reset is queued so hydration and
    // live child events cannot race ahead of the visible focus transition.
    // The prompt queue is not rebound here: the controller does that when
    // it takes up the child, which is not while it runs another turn.
    for interaction in child_interactions {
```

3. In `return_to_parent`, delete this statement:

```rust
    announce_runtime_ready(
        thread.as_ref().expect("parent thread after child return"),
        event_tx,
        control,
    );
```

   Then remove the now unused `control: &TuiSurfaceTaskControl` parameter from `return_to_parent`, and the `control,` argument in its call inside `handle_hosted_child_action`.

- [ ] **Step 4: The loop follows the view**

In `src/hosted_controller.rs`:

1. Add to the imports: `use crate::hosted_view::{Adoption, HostedView};` and remove `HostedChildFocus` and `handle_hosted_child_action` from the `crate::hosted_child` import if nothing else in the file uses them (keep `HostedChildAction` and `shutdown_attached_child_on_controller_exit`).

2. Above `hosted_tui_controller_loop`, add:

```rust
/// Moves what the controller operates on to what is on screen, and binds
/// the prompt queue to it. Only between actions: a running turn keeps the
/// thread it started on. `true` when the operating thread changed.
fn follow_view(
    view: &HostedView,
    thread: &mut Option<RuntimeThreadHandle>,
    event_tx: &mut mpsc::Sender<TuiEvent>,
    session_attachment: &mut SessionAttachmentId,
    side_active: bool,
    control: &TuiSurfaceTaskControl,
    active_recap_request_id: &mut Option<orca_runtime::recap::RecapRequestId>,
) -> bool {
    let previous = thread.clone();
    if view.adopt(thread, event_tx, session_attachment, side_active) == Adoption::Unchanged {
        return false;
    }
    // A recap still being written describes the conversation that left the
    // screen; withdrawing it also stops the provider call.
    if let Some(request_id) = active_recap_request_id.take()
        && let Some(previous) = previous.as_ref()
    {
        let _ = crate::surface_actions::TuiSurfaceActions::new(previous.typed_surface())
            .cancel_recap(request_id);
    }
    if let Some(runtime_thread) = thread.as_ref() {
        announce_runtime_ready(runtime_thread, event_tx, control);
    }
    true
}
```

3. In the loop's set-up, replace `let mut child_focus: Option<HostedChildFocus> = None;` (line 325) with:

```rust
    let view = HostedView::new(
        host.clone(),
        root_event_tx.clone(),
        attachment_routing.clone(),
        event_tx.clone(),
        session_attachment,
    );
    control.bind_view(view.clone());
```

4. At the top of `loop {`, replace the line `control.restrict_hosted_settings(side_parent.is_some() || child_focus.is_some());` with:

```rust
        if follow_view(
            &view,
            &mut thread,
            &mut event_tx,
            &mut session_attachment,
            side_parent.is_some(),
            &control,
            &mut active_recap_request_id,
        ) {
            last_idle_projection_cursor = None;
        }
        control.restrict_hosted_settings(side_parent.is_some() || view.has_focus());
```

5. Directly before `while let Ok(completed_request_id) = recap_completion_rx.try_recv() {` (line 377), so that an action is handled on the thread in view when it was taken:

```rust
        if follow_view(
            &view,
            &mut thread,
            &mut event_tx,
            &mut session_attachment,
            side_parent.is_some(),
            &control,
            &mut active_recap_request_id,
        ) {
            last_idle_projection_cursor = None;
        }
```

6. Replace the two remaining `child_focus.is_some()` (lines 380 and 424) with `view.has_focus()`.

7. Replace the whole `Ok(UserAction::FocusChildThread { … }) => { … }` arm and the `Ok(UserAction::ReturnToParentThread) => { … }` arm (lines 446-494) with:

```rust
            // The dispatcher shows a child without coming here; these only
            // arrive when it found no view bound yet. The loop follows the
            // view at its next turn.
            Ok(UserAction::FocusChildThread {
                task_id,
                expected_revision,
            }) => {
                view.handle(
                    HostedChildAction::Focus {
                        task_id,
                        expected_revision,
                    },
                    &control,
                );
            }
            Ok(UserAction::ReturnToParentThread) => {
                view.handle(HostedChildAction::Return, &control);
            }
```

8. At the end of the function, replace `if let Some(focus) = child_focus {` with `if let Some(focus) = view.take_focus() {`, and add `control.unbind_view();` on the line before `control.release_runtime_thread();`.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p orca-tui hosted_controller:: hosted_child:: hosted_view::`
Expected: PASS, including `the_controller_binds_the_view_and_lets_it_go_as_it_ends` and the existing `hosted_controller_dispatches_rejections_in_fifo_order_and_exits_cleanly`.

Run: `cargo test -p orca-tui`
Expected: PASS. If a test that focused a child and then asserted the prompt queue was bound to it fails, it now needs the controller to have run one loop turn (up to 100 ms, `IDLE_SURFACE_PROJECTION_POLL_INTERVAL`); wait for that in the test rather than restoring the call in `focus_child`.

- [ ] **Step 6: Commit**

```bash
git add crates/orca-tui/src/hosted_child.rs crates/orca-tui/src/hosted_controller.rs
git commit -m "feat(tui): the controller follows the conversation on screen instead of owning the switch"
```

---

### Task 4: The dispatcher shows a child without the controller

**Files:**
- Modify: `src/action_dispatcher.rs:191-200` (head of `route_action`), the `match action` in `route_action`, `:596-598` (`reject_overflowed_action`)

**Interfaces:**
- Consumes: `TuiSurfaceTaskControl::{view, message_would_miss_the_child_in_view}` (Task 2), `HostedView::handle` (Task 1), `hosted_child::HostedChildAction`.
- Produces: `const MAIN_STILL_RUNNING: &str`; `fn reject_action(event_tx: &Sender<TuiEvent>, action: UserAction, message: String)`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module of `src/action_dispatcher.rs`:

```rust
    #[test]
    fn a_focus_before_any_controller_waits_in_the_command_mailbox() {
        let (raw_tx, raw_rx) = mpsc::unbounded();
        let (event_tx, _event_rx) = mpsc::unbounded::<TuiEvent>();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let (mut dispatcher, command_rx) =
            TuiActionDispatcher::spawn(raw_rx, event_tx, control, 1, 1).expect("spawn dispatcher");

        raw_tx
            .send(UserAction::FocusChildThread {
                task_id: "child".to_string(),
                expected_revision: 7,
            })
            .expect("focus action");

        assert!(matches!(
            command_rx.recv_timeout(Duration::from_secs(1)),
            Ok(UserAction::FocusChildThread { task_id, expected_revision: 7 }) if task_id == "child"
        ));
        dispatcher.shutdown().expect("shutdown dispatcher");
    }

    #[test]
    fn a_focus_with_a_view_bound_does_not_wait_behind_the_controller() {
        let (raw_tx, raw_rx) = mpsc::unbounded();
        let (event_tx, event_rx) = mpsc::unbounded::<TuiEvent>();
        let control = TuiSurfaceTaskControl::isolated_for_test();
        let host = orca_runtime::runtime_host::RuntimeHost::start_with_background_capacity(1)
            .expect("runtime host");
        let attachment = crate::protocol::SessionAttachmentId::new(1);
        control.bind_view(crate::hosted_view::HostedView::new(
            host.handle(),
            event_tx.clone(),
            std::sync::Arc::new(std::sync::Mutex::new(
                crate::attachment_routing::AttachmentRouting::new(attachment),
            )),
            event_tx.clone(),
            attachment,
        ));
        let (mut dispatcher, command_rx) =
            TuiActionDispatcher::spawn(raw_rx, event_tx, control, 1, 1).expect("spawn dispatcher");

        // The controller is busy: nobody takes this from the mailbox.
        raw_tx
            .send(UserAction::Submit("a turn the controller runs".to_string()))
            .expect("submit");
        raw_tx
            .send(UserAction::FocusChildThread {
                task_id: "child".to_string(),
                expected_revision: 7,
            })
            .expect("focus action");

        // The view answered, here by saying there is no conversation yet,
        // while the submit still sits in the mailbox.
        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(1)),
            Ok(TuiEvent::OperationRejected(message))
                if message == "focus a main conversation before selecting a child"
        ));
        assert!(matches!(
            command_rx.recv_timeout(Duration::from_secs(1)),
            Ok(UserAction::Submit(prompt)) if prompt == "a turn the controller runs"
        ));
        assert!(command_rx.try_recv().is_err(), "the focus never entered the mailbox");
        dispatcher.shutdown().expect("shutdown dispatcher");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p orca-tui a_focus_`
Expected: `a_focus_before_any_controller_waits_in_the_command_mailbox` PASS (today's behaviour); `a_focus_with_a_view_bound_does_not_wait_behind_the_controller` FAIL: the first `recv_timeout` on `event_rx` times out because the focus went to the mailbox.

- [ ] **Step 3: Write the implementation**

1. Near the other constants at the top of `src/action_dispatcher.rs`:

```rust
/// Said when a message is sent from a child's conversation while the
/// controller still runs the main turn: it would go to the main agent.
const MAIN_STILL_RUNNING: &str =
    "the main agent is still running: return to it to send a message, or wait for it to finish";
```

2. Rename `reject_overflowed_action` to `reject_action`, give it the message, and keep the old name as the overflow case:

```rust
fn reject_overflowed_action(event_tx: &Sender<TuiEvent>, action: UserAction) {
    reject_action(
        event_tx,
        action,
        "TUI command queue is full; command rejected".to_string(),
    );
}

fn reject_action(event_tx: &Sender<TuiEvent>, action: UserAction, message: String) {
    // A plain submit, with the token it carried: the controller never gets it.
```

   and delete the old first line of the body, `let message = "TUI command queue is full; command rejected".to_string();`. The rest of the body is unchanged.

3. In `route_action`, directly after `let controller = surface_control;` and before `match action {`:

```rust
    // A child's conversation is on screen and the controller still runs the
    // main turn: this message would go to the main agent. Hand it back.
    if matches!(
        action,
        UserAction::Submit(_)
            | UserAction::SubmitWithMentions { .. }
            | UserAction::QueuePrompt { .. }
            | UserAction::SubmitNow { .. }
            | UserAction::SubmitQueued { .. }
    ) && controller.message_would_miss_the_child_in_view()
    {
        reject_action(event_tx, action, MAIN_STILL_RUNNING.to_string());
        return true;
    }
```

4. In the `match action`, directly after the `UserAction::ReadTaskTranscript(request) => { … }` arm:

```rust
        // Showing a child's conversation, or the main one again, changes
        // only what is on screen, so it does not wait for the controller,
        // which a running turn keeps until the turn ends.
        UserAction::FocusChildThread {
            task_id,
            expected_revision,
        } => match controller.view() {
            Some(view) => view.handle(
                crate::hosted_child::HostedChildAction::Focus {
                    task_id,
                    expected_revision,
                },
                controller,
            ),
            None => match enqueue_action(
                UserAction::FocusChildThread {
                    task_id,
                    expected_revision,
                },
                command_tx,
                &mut pending.commands,
                backlog_capacity,
            ) {
                EnqueueResult::Queued => {}
                EnqueueResult::Disconnected => return false,
                EnqueueResult::Overflow(action) => reject_overflowed_action(event_tx, action),
            },
        },
        UserAction::ReturnToParentThread => match controller.view() {
            Some(view) => view.handle(crate::hosted_child::HostedChildAction::Return, controller),
            None => match enqueue_action(
                UserAction::ReturnToParentThread,
                command_tx,
                &mut pending.commands,
                backlog_capacity,
            ) {
                EnqueueResult::Queued => {}
                EnqueueResult::Disconnected => return false,
                EnqueueResult::Overflow(action) => reject_overflowed_action(event_tx, action),
            },
        },
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p orca-tui action_dispatcher::`
Expected: PASS, including both `a_focus_` tests and the two `overflowed_submit` tests.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-tui/src/action_dispatcher.rs
git commit -m "feat(tui): a child's conversation is shown without waiting for the turn the controller runs"
```

---

### Task 5: A live child opens its conversation at any time, and main shows under it

**Files:**
- Modify: `src/agent_workspace_actions.rs:13-53` and its tests (`:669-692`)
- Modify: `src/ui.rs:2518` (the hint), `:5495-5527` (`activity_rows`), and the hint test near `:14687`
- Modify: `src/types.rs` (field next to `agent_dock_selected_task_id`, line 786 and 1197)
- Modify: `src/state_reducer.rs:196-262`

**Interfaces:**
- Produces: `opens_live_conversation(task: &BackgroundTaskSummary) -> bool`; `AppState::main_behind_child: Option<SideParentStatus>`.

- [ ] **Step 1: Write the failing tests**

In `src/agent_workspace_actions.rs`, replace the test `a_child_the_running_turn_waits_on_opens_its_activity_right_away` with:

```rust
    #[test]
    fn a_live_child_opens_its_conversation_while_the_main_agent_runs() {
        // The dispatcher shows a child without the controller, so a running
        // turn no longer holds the switch up.
        let mut child = task("child", 1_000);
        child.subagent_child_thread_id = Some("child-thread".to_string());
        let (mut state, rx) = conversation_with(vec![child]);
        state.status = crate::types::AppStatus::Running;
        let tx = state.event_tx.clone();

        assert!(open_agent_task(&mut state, &tx, "child"));

        assert_eq!(state.panel_mode, PanelMode::Conversation);
        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::FocusChildThread {
                task_id,
                expected_revision: 7,
            }) if task_id == "child"
        ));
        assert!(rx.try_recv().is_err(), "no transcript checkpoint is read");
    }

    #[test]
    fn a_running_main_shows_under_a_childs_conversation_and_a_click_returns() {
        let mut child = task("child", 1_000);
        child.subagent_child_thread_id = Some("child-thread".to_string());
        let (mut state, _rx) = conversation_with(vec![child]);
        state.status = crate::types::AppStatus::Running;
        state.update(crate::protocol::TuiEvent::ChildFocusChanged {
            task_id: Some("child".to_string()),
        });
        assert_eq!(
            state.main_behind_child,
            Some(crate::types::SideParentStatus::Running)
        );

        render_once(&mut state);
        let main = clicks_on(&state, &AgentHitTarget::Main);
        assert!(!main.is_empty(), "the row saying main runs returns to main");
        assert_eq!(click(&mut state, main[0]), MouseFlow::ReturnToMain);

        state.update(crate::protocol::TuiEvent::SideParentStatusChanged(
            crate::types::SideParentStatus::NeedsApproval,
        ));
        assert_eq!(
            state.main_behind_child,
            Some(crate::types::SideParentStatus::NeedsApproval)
        );

        state.update(crate::protocol::TuiEvent::ChildFocusChanged { task_id: None });
        assert_eq!(state.main_behind_child, None);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p orca-tui a_live_child_opens_its_conversation_while_the_main_agent_runs a_running_main_shows_under`
Expected: FAIL to compile, `no field main_behind_child on type AppState`.

- [ ] **Step 3: Write the implementation**

1. `src/agent_workspace_actions.rs`: in `open_agent_task` change the condition to `if opens_live_conversation(task) {`, and replace the function and its comment:

```rust
/// Whether opening this agent switches the view to its live conversation:
/// a child still running on a thread of this process. Any other agent opens
/// its panel.
pub(crate) fn opens_live_conversation(task: &BackgroundTaskSummary) -> bool {
    task.subagent_child_thread_id.is_some() && task.status.is_active()
}
```

   Update the doc comment of `open_agent_task` to drop "and only while no turn runs here".

2. `src/ui.rs:2518`: change `opens_live_conversation(task, idle)` to `opens_live_conversation(task)`. If `idle` is then unused in that function, keep the parameter and name it `_idle` only if the compiler warns; do not change the function's signature. In the test near line 14687, both `hint(&child, true)` and `hint(&child, false)` now contain `"Enter open conversation"`; change the second assertion accordingly.

3. `src/types.rs`: after the field `pub(crate) agent_dock_selected_task_id: Option<String>,`:

```rust
    /// What the main conversation is doing while a child's is on screen;
    /// `None` when main itself is on screen.
    pub(crate) main_behind_child: Option<SideParentStatus>,
```

   and in the constructor after `agent_dock_selected_task_id: None,`:

```rust
            main_behind_child: None,
```

   Add to `impl AppState` (next to `set_conversation_target`):

```rust
    /// Called as a child's conversation replaces the main one: what main
    /// was doing is what it is doing behind the child, until it says more.
    pub(crate) fn note_main_behind_child(&mut self) {
        if self.conversation_target.task_id().is_some() {
            return;
        }
        self.main_behind_child = Some(match self.status {
            AppStatus::Idle | AppStatus::Setup | AppStatus::SessionPicker => SideParentStatus::Idle,
            AppStatus::WaitingApproval => SideParentStatus::NeedsApproval,
            AppStatus::WaitingUserInput => SideParentStatus::NeedsInput,
            AppStatus::Running | AppStatus::Compacting => SideParentStatus::Running,
        });
    }
```

4. `src/state_reducer.rs`:

   In the `TuiEvent::SideParentStatusChanged(status)` arm, after the existing `if let Some(side) …` block:

```rust
                if self.conversation_target.task_id().is_some() {
                    self.main_behind_child = Some(status);
                }
```

   In the `TuiEvent::ChildFocusChanged { task_id }` arm, as its first statements:

```rust
                if task_id.is_some() {
                    self.note_main_behind_child();
                } else {
                    self.main_behind_child = None;
                }
```

   In the `TuiEvent::ChildProjectionReset { … }` arm, directly after the early `return;` block and before `let background_tasks = …`:

```rust
                self.note_main_behind_child();
```

   In the `TuiEvent::NewSessionStarted` arm, add `self.main_behind_child = None;`.

5. `src/ui.rs`, in `activity_rows`, directly after the `if … else if let Some(line) = foreground_activity_line(state, theme) { … }` block:

```rust
    if let Some(line) = main_behind_child_line(state, theme) {
        rows.push((line, Some(AgentHitTarget::Main)));
    }
```

   and above `fn foreground_activity_line`:

```rust
/// Under a child's conversation: what the main conversation is doing, when
/// that matters to what the user can do here. A click on it returns.
fn main_behind_child_line(state: &AppState, theme: &Theme) -> Option<Line<'static>> {
    state.conversation_target().task_id()?;
    let (text, color) = match state.main_behind_child? {
        crate::types::SideParentStatus::Running => (
            "Main is running · messages wait for it · Esc return",
            theme.warning,
        ),
        crate::types::SideParentStatus::NeedsApproval => {
            ("Main needs your approval · Esc return", theme.approval)
        }
        crate::types::SideParentStatus::NeedsInput => {
            ("Main needs your answer · Esc return", theme.approval)
        }
        _ => return None,
    };
    Some(Line::from(Span::styled(
        format!(" ● {text}"),
        Style::default().fg(color),
    )))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p orca-tui agent_workspace_actions:: `
Expected: PASS. Then `cargo test -p orca-tui` — expected PASS; a golden frame that shows a focused child with `AppStatus::Running` gains the new row and must be regenerated the way the other goldens in `src/golden` are.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-tui/src/agent_workspace_actions.rs crates/orca-tui/src/ui.rs crates/orca-tui/src/types.rs crates/orca-tui/src/state_reducer.rs
git commit -m "feat(tui): a running subagent's conversation opens while the main agent runs, and says what main is doing"
```

---

### Task 6: See it in a real terminal, then the repo checks

Reducer and dispatcher tests cannot show that the child's stream reaches the screen while the parent turn runs, nor that the parent comes back whole. This task is the gate for the plan.

**Files:**
- Create (not committed): `/tmp/orca-live-view/drive.py`
- Modify: `docs/superpowers/specs/2026-11-23-live-subagent-view-and-agents-card-design.md` (record what was found)

- [ ] **Step 1: Build**

Run: `cargo build -p orca`
Expected: `target/debug/orca` is rebuilt. (If the binary's package has another name, `cargo build` at the workspace root.)

- [ ] **Step 2: Write the driver**

`mkdir -p /tmp/orca-live-view/work` and create `/tmp/orca-live-view/drive.py`:

```python
#!/usr/bin/env python3
"""Drives orca in a pty and prints the screen at each step."""
import fcntl, os, pty, select, shutil, struct, sys, tempfile, termios, time

import pyte

ROWS, COLS = 50, 150
orca, work = os.path.abspath(sys.argv[1]), os.path.abspath(sys.argv[2])
home = tempfile.mkdtemp(prefix="orca-home-")
shutil.copy(os.path.expanduser("~/.orca/auth.json"), os.path.join(home, "auth.json"))

pid, fd = pty.fork()
if pid == 0:
    os.chdir(work)
    os.environ["ORCA_HOME"] = home
    os.environ["TERM"] = "xterm-256color"
    os.execv(orca, [orca])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
screen = pyte.Screen(COLS, ROWS)
stream = pyte.ByteStream(screen)


def pump(seconds):
    end = time.time() + seconds
    while time.time() < end:
        ready, _, _ = select.select([fd], [], [], 0.1)
        if ready:
            try:
                stream.feed(os.read(fd, 65536))
            except OSError:
                return


def text():
    return "\n".join(
        "".join(screen.buffer[y][x].data for x in range(COLS)).rstrip() for y in range(ROWS)
    )


def dump(label):
    print(f"\n===== {label}\n{text()}", flush=True)


def send(data):
    os.write(fd, data.encode())


pump(4)
if "trust" in text().lower():
    send("U")
    pump(2)
dump("started")

send(
    "Start two subagents in parallel. One writes 400 words about tides, the other 400 words "
    "about dunes. When both are done, give me one line on each.\r"
)
pump(30)
dump("main running, agents listed")

send("\x1b[1;2B")  # Shift+Down: pick the first agent in the dock
pump(1)
dump("an agent picked")
send("\r")  # Enter: open it
pump(3)
dump("child conversation, first look")
pump(4)
dump("child conversation, four seconds later")

send("hello\r")
pump(2)
dump("a message sent from the child while main runs")

send("\x1b")  # Esc: back to main
pump(3)
dump("back on main")
pump(90)
dump("main finished")
send("\x03")
pump(1)
```

- [ ] **Step 3: Run it and read the screens**

Run: `python3 /tmp/orca-live-view/drive.py "$PWD/target/debug/orca" /tmp/orca-live-view/work > /tmp/orca-live-view/run.txt; less /tmp/orca-live-view/run.txt`

Expected, screen by screen:

| Screen | Must show |
|---|---|
| main running, agents listed | `Running …` and an `Agents … active` block with two agents |
| an agent picked | `›` on one agent row. If no row is marked, the dock needs `/tasks` first: note it and open the agent with a click instead (`\x1b[<0;COL;ROWM` then `\x1b[<0;COL;ROWm`) |
| child conversation, first look | The child's own conversation, not the Agents panel; the row `Main is running · messages wait for it · Esc return` |
| …four seconds later | More of the child's text than the first look: it streams |
| a message sent… | `hello` back in the composer and `the main agent is still running: return to it…` |
| back on main | The main conversation with everything it showed before, nothing twice |
| main finished | The two one-line summaries, once each; no duplicated or missing paragraph above them |

- [ ] **Step 4: If "back on main" shows text twice or a gap**

That is the risk the spec names: the snapshot the return projects and the running turn's own events overlap. Do not ship. Record the screen in the spec under "To verify first", and stop for a decision: the fix is to have the running turn re-project at its next batch boundary instead of the view reading a snapshot, which is a design change, not a patch.

- [ ] **Step 5: Run it resumed**

Run orca again in the same work directory with `--resume latest` (add the argument to `os.execv` in a copy of the driver; there is no trust prompt on resume, so remove the `U` branch), dump after 5 s, and check the main conversation and the agents are shown as they ended.

- [ ] **Step 6: Repo checks**

```bash
cargo test -p orca-tui
cargo clippy -p orca-tui --all-targets -- -D warnings
for f in scripts/validate-*.mjs; do node "$f" || break; done
for f in scripts/test-validate-*.mjs; do node "$f" || break; done
```

Expected: all pass. A validator that names a new call site in a frozen inventory says which manifest to update; update it as the validator's message directs, in the same commit.

- [ ] **Step 7: Record the result in the spec and commit**

In the spec's "To verify first", replace item 1 with what the run showed (date, the seven screens passed or which failed), and under "The composer" replace the Stage 1 paragraph with:

```markdown
Stage 1: in an agent's conversation, the composer works as it does today
when the main conversation is idle. While the main agent is running, a
message sent there is handed back to the composer with a line saying why,
and a row under the conversation says main is running.
```

Add to "Not in this change" of Stage 1's scope a line saying finished agents open the Agents panel as before until their replay has its own plan.

```bash
git add docs/superpowers/specs/2026-11-23-live-subagent-view-and-agents-card-design.md
git commit -m "docs: what the live subagent view showed in a real terminal"
```
