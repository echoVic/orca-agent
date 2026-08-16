# TUI Pending Interaction Input Admission

Status: Implemented on local `main`

## Context

At audited base `f80270bf7`, foreground user-input and MCP-elicitation answers
share `idle_submit_actions::handle_idle_submit`. Three ordering defects exist
before the runtime-owned interaction responder is reached:

1. `handle_slash_command` runs before the `WaitingUserInput` branch, so an
   answer equal to a known slash command can mutate TUI/session state instead
   of answering the pending interaction.
2. `McpElicitationRequested` projects the mode into the transcript but stores
   only the interaction key in `pending_input`. The submit path therefore
   cannot distinguish Form from URL admission.
3. MCP content is parsed as JSON only in
   `TuiSurfaceTaskControl::respond_surface_interaction`, after the TUI has
   entered Running, consumed `pending_input`, and cleared the composer. Invalid
   Form JSON then produces `OperationRejected` while the runtime interaction
   remains bound, leaving no usable retry surface.

The same early empty-input guard also prevents URL-mode acceptance with an
empty composer even though the runtime representation can carry an empty JSON
object.

## Decision

Keep pending-interaction input admission in the renderer submit owner, before
any optimistic state transition:

- bypass slash-command parsing whenever status is `WaitingUserInput`;
- retain the current MCP elicitation mode as a private `AppState` projection
  companion to the existing public `PendingTuiInput` key;
- require syntactically valid JSON for non-empty MCP answers before dispatch;
- preserve the pending interaction, Waiting state, composer text, and all
  mention/paste state when JSON validation fails;
- admit an empty URL-mode submission as accepted content `{}`;
- keep empty Form and ordinary user-input submissions as no-ops.

The runtime surface remains the only response mutation owner. It still parses
the dispatched JSON, applies interaction/operation/route fences, commits the
answer, removes the bound interaction only after commit, and owns response
errors and terminal settlement.

## Frozen Behavior

### Ordinary User Input

1. A non-empty answer is sent once as
   `RespondToInteraction { key, UserInput(exact_trimmed_text) }`.
2. Known and unknown slash-looking text is literal interaction input; it does
   not execute a slash command or emit an invalid-command error.
3. Empty input remains unhandled and preserves the pending key and composer.

### MCP Form

1. Non-empty syntactically valid JSON is sent once with the exact trimmed JSON
   text as accepted MCP content.
2. Invalid JSON emits no `UserAction`, stays `WaitingUserInput`, retains the
   exact pending key/mode and composer text, and appends the existing parser
   error prefix `invalid typed MCP elicitation content:`.
3. Empty input remains unhandled and preserves state.

### MCP URL

1. Empty input is a handled acceptance and sends accepted content `{}` for the
   exact pending interaction key.
2. Non-empty valid JSON preserves the exact trimmed JSON text.
3. Non-empty invalid JSON follows the same retry-preserving rejection as Form.

### Successful Admission

After a user-input or MCP response is admitted, preserve the existing optimistic
transition: enter Running, consume the pending key/mode, scroll to bottom,
clear pending paste/mention/atomic-token state, and reset the composer. The
action dispatcher still handles `RespondToInteraction` on its prioritized path
without entering the bounded command mailbox.

## Lifecycle And Compatibility

- Interaction revision, response-route epoch, operation fence, permission
  grant, runtime retries, disconnect behavior, and committed/deferred/
  uncommitted outcomes remain in the runtime surface and
  `TuiSurfaceTaskControl`.
- Interrupt, cancellation, terminal completion, session reset, Side attachment
  fencing, and restart behavior are unchanged. Session reset and terminal
  completion clear both the pending key and private MCP mode projection.
- No `UserAction`, `TuiEvent`, runtime-surface, server/JSONL, app-server, ACP,
  CLI/slash syntax, transcript schema, persistence, or public Rust API changes.
  The mode companion is crate-private and leaves the public
  `PendingTuiInput::McpElicitation(TuiInteractionKey)` shape unchanged.
- The lower runtime JSON parse remains as defense in depth; renderer admission
  does not replace runtime authority or validate the requested schema.

## Test Strategy

1. Add RED submit-owner tests for a known slash command as literal user input,
   invalid Form JSON retry preservation, and empty URL acceptance as `{}`.
2. Add reducer coverage proving Form/URL mode projection and reset/terminal
   cleanup stay aligned with `pending_input`.
3. Keep the existing foreground approval, permission, user-input, MCP event,
   dispatcher-priority, canonical runtime response, interrupt, Side, restart,
   and PTY suites as downstream evidence.
4. Update the runtime-surface manifest references without weakening the frozen
   interaction mutation inventory.
5. Run focused submit/reducer/dispatcher tests, compiler check, full serial
   TUI, PTY, runtime/Windows validators and self-tests, formatter, and diff
   checks. Request independent review before integration.

## Acceptance Criteria

1. Waiting interaction text cannot execute a slash command.
2. Invalid MCP JSON cannot consume the pending interaction or composer.
3. Empty URL-mode acceptance dispatches `{}` exactly once, while empty Form
   and user input remain no-ops.
4. Mode projection cannot outlive or become detached from its pending MCP key.
5. Runtime response ownership and all external/public contracts remain
   unchanged.
6. Full TUI and PTY suites pass after rebase and on integrated local `main`.
7. Independent review has no unresolved Critical or Important finding.

## Implementation Evidence

- At audited base `f80270bf7`, all three submit-owner regressions failed for
  the intended reasons: empty URL input returned unhandled, invalid Form JSON
  dispatched and consumed state, and `/new` routed away from the interaction.
  After renderer admission was implemented, all seven `idle_submit_actions`
  tests pass.
- `AppState` now retains MCP mode in a crate-private companion projection.
  Focused reducer tests prove UserInput clears it, MCP preserves Form/URL, and
  both terminal completion and session reset clear mode with the pending key.
- The existing exact valid MCP payload test, waiting user-input test, six
  dispatcher tests, and three canonical runtime approval/permission/user-input
  fence tests pass. Response mutation remains in `respond_surface_interaction`.
- The `RespondToInteraction` manifest references now include the current
  pending-input producer and prioritized dispatcher owner. Path-specific
  anchors plus negative self-tests reject deletion of either production path
  while enum and test references remain. Manifest SHA is `faa212d8f9d78c72fe1df2b0825e155dafd34b0bcd3f5b6594b1776737b4b1ca`.
- Current source sizes are `idle_submit_actions.rs` 454 lines, `types.rs` 8,839
  lines, `action_dispatcher.rs` 569 lines, and `app.rs` 8,826 lines.
- The initial and post-rebase topic gates pass: full serial TUI 1,100/1,100,
  root-package PTY 6/6, compiler check, runtime and Windows validators plus
  their self-tests, formatter, and diff checks. The fetched `origin/main` had no
  commits beyond local `main`, so the topic rebase was a no-op.
- Local `main` was fast-forwarded to the single semantic commit. Integrated-root
  verification passes full serial TUI 1,100/1,100 in 270.43 seconds and PTY 6/6
  in 9.61 seconds, plus both validators/self-tests, formatter, and diff checks.
  The completed slice worktree and branch were then removed.
- Independent CodeRabbit review covered all nine source, contract, roadmap,
  spec, and plan files. It raised no code issue and no Critical or Important
  issue. Its sole Minor asked the status to distinguish implementation from
  unfinished integration; the status now does so. A follow-up service review
  was rate-limited after the included-review quota, so local diff and document
  checks verify that one-line correction.

## Residual Boundary

Runtime response failures after renderer admission still use the existing
`OperationRejected` path. A future slice may add atomic retry presentation for
committed-path transport failures, but must not duplicate response mutation or
introduce a second interaction broker.
