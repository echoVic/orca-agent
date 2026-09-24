# Session Recap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 `tui-visual-unification` 当前代码和刚审计的视觉改动（现已落在 `025d814c`）之上，增加一个只读、可取消、不会污染 transcript 的 session recap：先交付 `/recap` 和 composer 上方的短 recap 条，再交付 focus 回归后的自动生成。

**Architecture:** TUI 负责 eligibility、显示和输入优先级；runtime/provider 负责从 `SurfaceSnapshot` 构造有界证据、调用无工具的辅助摘要模型、缓存、取消和 worker 生命周期。runtime host 在已有 actor/config 边界内提供只读 recap handle，hosted controller 只桥接结果，renderer 只接收 typed result。recap 结果通过带 session attachment 与 content marker 的 typed event 回到 reducer。短条是 activity chrome 的临时行，详情才是 popup；不新增 `ChatMessage`、`OperationKind` 或 compaction mutation。

**Tech Stack:** Rust 2024，ratatui 0.29，crossbeam channels，现有 `RuntimeSurfaceThreadHandle`/`TuiSurfaceActions`，`orca-provider` 的 summary cache 和 `CancelToken`，`unicode-width` 与 `display_text::truncate_to_display_width`。

**Spec:** `docs/superpowers/specs/2026-09-22-session-recap-design.md`

## Global Constraints

- 视觉统一 worktree `/Users/qingyun/Documents/GitHub/blade-deepseek/.claude/worktrees/tui-visual-unification` 保持只读；实施 recap 的新 worktree 是 `/Users/qingyun/.codex/worktrees/session-recap-plan/blade-deepseek`，分支 `session-recap-plan`，基线为 `025d814c`。
- 视觉统一的 9 个文件已经在 `025d814c` 中；实施 recap 时不得 reset、checkout、clean、覆盖、回退或代提交用户正在开发的视觉 worktree。
- recap 不进入 `ChatMessage`、`TranscriptRenderCache`、scrollback、搜索索引或 runtime context；不发送 `UserAction::Submit`，不复用 `manual_compact`。
- 不新增全局 `Recapping` 状态；审批、计划、问卷、child focus、slash/mention popup、队列和 Esc/取消顺序保持现有语义。
- renderer 不做同步网络调用；每个 session/content marker 只能有一个 inflight 请求，所有异步结果必须带 request id、attachment 和 marker 并在 reducer 处再次校验。
- provider 请求无工具、输入最多 4,096 tokens、输出最多 120 tokens、软超时 8 秒、可取消；真实 recap usage 与主 operation usage 分开。
- 短条最多 3 个 visual rows，空间不足时先让 recap 让步，不挤 composer、queue/search 或 status。面板内容宽度按 `panel_block` 外框减 4。
- 每个任务完成后运行针对性测试；最终运行 `cargo fmt --all`、`cargo test -p orca-tui --lib -- --test-threads=1`、`cargo clippy -p orca-tui --all-targets -- -D warnings`。计划本身不要求现在提交或 push。

## Task 0: Freeze the visual-unification baseline and add contract fixtures

**Files:**

- Create: `crates/orca-tui/src/recap_contract_tests.rs`
- Modify: `crates/orca-tui/src/lib.rs` (test module registration only)
- Reference only: `crates/orca-tui/src/chrome.rs`, `ui.rs`, `transcript_view.rs`, `transcript_state_tests.rs`

- [ ] Record `git rev-parse HEAD`, `git status --short`, and `git show --stat 025d814c` in the implementation notes; preserve the visual-unification commit and do not stage the two plan documents while drafting.
- [ ] Add red contract tests for a `RecapContentMarker` that changes on a new completed user operation but not on usage/settings-only projection refreshes.
- [ ] Add a red test that a recap result is rejected when `SessionAttachmentId` or marker differs from the pending request.
- [ ] Add a red layout fixture for a 20-column area proving the future recap strip cannot reduce composer/status below their current minimum heights.
- [ ] Run the focused tests to establish the expected compile failures before adding implementation types:

```bash
cargo test -p orca-tui recap_contract --lib -- --test-threads=1
```

## Task 1: Extract a provider-owned, display-only summary request

**Files:**

- Modify: `crates/orca-provider/src/context.rs`
- Modify: `crates/orca-provider/src/lib.rs` (export the narrow display-summary entry point)
- Modify: `crates/orca-provider/src/summary_cache.rs` only if the cache key needs a display-purpose namespace
- Tests: `crates/orca-provider/src/context.rs` and/or `crates/orca-provider/src/display_summary_tests.rs`

**Interfaces:**

```rust
pub struct DisplaySummaryEvidence {
    /// Canonical hex digest; provider crate must not depend on orca-runtime's
    /// `Sha256Digest` type.
    pub digest: String,
    pub text: String,
    pub input_tokens: usize,
}

pub struct DisplaySummaryResult {
    pub text: String,
    pub usage: Option<Usage>,
}

pub fn request_display_summary(
    provider_kind: ProviderKind,
    provider_config: &ProviderConfig,
    evidence: &DisplaySummaryEvidence,
    cancel: &CancelToken,
    deadline: Instant,
) -> Result<DisplaySummaryResult, DisplaySummaryError>;
```

- [ ] Factor the no-tool request path out of `context::request_summary` without changing compaction behavior, summary baseline/delta state, or existing cache keys.
- [ ] Use the resolved provider/api key/base URL and `auxiliary_model()`; preserve custom base URLs and never prompt for setup from this path.
- [ ] Build the system/user prompt so evidence is explicitly data, not instructions; reject tool calls, empty assistant content, deadline expiry, cancellation, and output over the retained bound.
- [ ] Keep telemetry purpose separate (`display_recap`) and do not add usage to context or the active operation.
- [ ] Test cache hit, custom base URL propagation, no-tool enforcement, cancellation, timeout, empty/error response, Unicode output bound, and no mutation of a cloned `Conversation`.
- [ ] Run:

```bash
cargo test -p orca-provider display_summary -- --test-threads=1
```

## Task 2: Add runtime recap evidence, worker, and typed result

**Files:**

- Create: `crates/orca-runtime/src/recap.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/projection.rs` only for a stable, redacted evidence extractor near `SurfaceSnapshot`
- Modify: `crates/orca-runtime/src/runtime_surface/host.rs` and the runtime host actor to expose a typed read-only recap handle with actor-owned provider config
- Modify: `crates/orca-runtime/src/runtime_host.rs` (`ThreadCommand`, `RuntimeThreadHandle`, `ThreadActor` dispatch and shutdown drain)
- Tests: `crates/orca-runtime/src/recap.rs` and projection tests

**Interfaces:**

```rust
pub struct RecapRequest {
    pub request_id: RecapRequestId,
    pub source: RecapSourceFence, // runtime-owned SurfaceCursor + content marker
    pub trigger: RecapTrigger,
    pub evidence: RecapEvidence,
}

pub enum RecapResult {
    Ready { request_id: RecapRequestId, source: RecapSourceFence, text: String, usage: RecapUsage },
    Failed { request_id: RecapRequestId, source: RecapSourceFence, error: SafeDiagnosticText },
    Skipped { request_id: RecapRequestId, reason: RecapSkipReason },
}
```

- [ ] Implement `SurfaceSnapshot::recap_evidence()` (or a sibling pure function) that selects bounded user/assistant conclusions, plan/goal/interaction state, and safe tool terminal labels while excluding reasoning, raw outputs, images, secrets, active streams, and system instructions. Join tool name/target through `SurfaceSnapshot::tools` by call id; `SurfaceItem::ToolResultMessage` alone does not carry those fields.
- [ ] Define `RecapContentMarker` from thread/incarnation plus the latest completed user operation/item and evidence digest; usage/settings-only changes must not invalidate it.
- [ ] Keep the runtime-owned worker read-only: it builds evidence and calls the provider with actor-owned config, but must not commit surface events, append history, mutate context, or create a `ManualCompaction`/other `OperationKind`.
- [ ] Add the recap handle inside `orca-runtime` so TUI does not gain a direct `orca-provider` dependency and cannot access a full `RuntimeThreadSnapshot` or provider registry. Return only typed handle/result values.
- [ ] Add `ThreadCommand::RequestRecap` and `ThreadCommand::CancelRecap`, typed methods on `RuntimeThreadHandle`/`RuntimeSurfaceThreadHandle`, and actor-side handling that clones its private `RunConfig`; update `drain_closed_thread_commands` so shutdown cannot wait on a recap reply.
- [ ] Implement one supervised worker per runtime thread with an explicit cancel handle, deadline, and deduplication key `(source fence, marker, provider_fingerprint, trigger class)`.
- [ ] Ensure shutdown, attachment replacement, and cancellation join the worker within the existing supervisor deadline; stale result delivery is harmless because the TUI rechecks fences.
- [ ] Test redaction/bounds, operation-count selection, marker stability, one-inflight dedupe, cancellation, worker shutdown, and no surface-ledger events.
- [ ] Run:

```bash
cargo test -p orca-runtime recap -- --test-threads=1
```

## Task 3: Bridge the worker through the TUI surface facade and protocol

**Files:**

- Modify: `crates/orca-tui/src/surface_actions.rs`
- Modify: `crates/orca-tui/src/surface_client.rs` or the existing hosted runtime owner where typed surface reads are dispatched
- Modify: `crates/orca-tui/src/protocol.rs`
- Modify: `crates/orca-tui/src/renderer_runtime.rs`
- Modify: `crates/orca-tui/src/attachment_routing.rs`
- Tests: `crates/orca-tui/src/surface_boundary_tests.rs`, `surface_client.rs` tests, protocol routing tests

- [ ] Add one TUI-facing `request_recap`/`cancel_recap` facade method that delegates to the runtime recap handle; presentation code must not receive a raw `RuntimeSurfaceThreadHandle` or provider registry.
- [ ] Let the hosted controller bridge the runtime handle's typed result to `TuiEvent`; it must not call `orca-provider` or hold a duplicate provider worker.
- [ ] Send only typed `RecapReady`/`RecapFailed`/`RecapSkipped` events containing request id, `SessionAttachmentId`, marker, text/error and usage. Do not send `SurfaceSnapshot` or provider response through `TuiEvent`.
- [ ] Route events through `accept_attached_tui_event` before reducer handling; old attachment or old incarnation events must be dropped.
- [ ] Add surface boundary tests proving recap is read-only and every new public action/event has an explicit routing branch.
- [ ] Keep error messages out of `TuiEvent::Notice` so `state_reducer` does not turn them into `ChatMessage::System` transcript rows.
- [ ] Run:

```bash
cargo test -p orca-tui surface_boundary --lib -- --test-threads=1
cargo test -p orca-tui attachment --lib -- --test-threads=1
```

## Task 4: Add reducer-owned recap state and eligibility

**Files:**

- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/state_reducer.rs`
- Modify: `crates/orca-tui/src/runtime_event_actions.rs`
- Modify: `crates/orca-tui/src/state_integration_tests.rs`

**Interfaces:**

```rust
pub(crate) enum RecapState {
    Hidden,
    Pending { request_id: RecapRequestId, session: SessionAttachmentId, marker: RecapContentMarker, trigger: RecapTrigger },
    Ready { session: SessionAttachmentId, marker: RecapContentMarker, text: String, usage: RecapUsage, detail_open: bool },
    Failed { session: SessionAttachmentId, marker: RecapContentMarker, message: String },
}
```

- [ ] Add `recap` to `AppState` with default `Hidden`; do not add `AppStatus::Recapping`.
- [ ] On `TurnStarted`, `CompactionStarted`, `SessionProjectionReset`, `ChildProjectionReset`, `Backtracked`, new attachment, or changed content marker, cancel/invalidate pending and ready recap as specified; preserve a ready result only across usage/settings-only refreshes with the same marker.
- [ ] On `RecapReady`, accept only matching request/session/marker; on `RecapFailed`, show the message only for a manual trigger; stale/cancelled results become no-ops.
- [ ] Define `can_request_manual_recap()` and `can_request_auto_recap()` from existing `AppStatus`, approval/plan/user-input fields, child focus, background workflow activity, terminal focus, history mode, and completed user-operation count. Do not count every `TurnStarted`.
- [ ] Test all invalidation fences, pending-to-ready transitions, manual failure visibility, automatic failure quietness, and no transcript/message revision changes.
- [ ] Run:

```bash
cargo test -p orca-tui state_reducer --lib -- --test-threads=1
cargo test -p orca-tui state_integration --lib -- --test-threads=1
```

## Task 5: Implement the unified recap strip and detail view

**Files:**

- Create: `crates/orca-tui/src/recap_view.rs`
- Modify: `crates/orca-tui/src/lib.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/chrome.rs` only for a genuinely shared token/helper
- Tests: `crates/orca-tui/src/recap_view.rs`, `ui.rs` layout/golden tests

- [ ] Implement `short_lines`, `detail_lines`, `strip_rect`, and `detail_rect` using explicit `Rect` inputs and `truncate_to_display_width`; hard CJK/emoji/URL tokens must not overflow or split a grapheme.
- [ ] In `ui::render`, calculate recap rows separately before `desired_activity_height`; pass the same geometry to `render_activity` and any hit-test. Do not change `activity_lines`’ existing test-facing signature.
- [ ] Reserve at most three rows and make recap the first content dropped when the activity area cannot fit; assert composer and status rectangles remain unchanged in narrow/overflow fixtures.
- [ ] Render the detail popup with `chrome::dialog_rect`, `panel_block`, and `block.inner(rect)`; use `outer - 4` for content width and preserve rounded border, theme, hint line, and selection marker.
- [ ] Keep the strip unfocused; only the detail popup may register a hit area. It must not steal ordinary typing, Esc, approval, plan, questionnaire, search, or child-focus events.
- [ ] Add golden/structural tests for wide, 40-column, 20-column, zero-height, CJK, emoji, long-token, panel-padding, and overflow cases. Verify recap never enters transcript cache or scrollback.
- [ ] Run:

```bash
cargo test -p orca-tui recap_view --lib -- --test-threads=1
cargo test -p orca-tui ui --lib -- --test-threads=1
```

## Task 6: Add `/recap` command and preserve input priority

**Files:**

- Modify: `crates/orca-tui/src/commands/mod.rs`
- Modify: `crates/orca-tui/src/idle_submit_actions.rs`
- Modify: `crates/orca-tui/src/action_dispatcher.rs` or the existing local command dispatcher
- Modify: `crates/orca-tui/src/key_event_actions.rs` / `status_key_actions.rs` only where the current priority chain requires a recap-detail branch
- Modify: `crates/orca-tui/src/shortcuts.rs` if the command hint list is generated there
- Tests: command parser, idle submit, input priority, popup hit-test tests

- [ ] Register `/recap` as strict no-argument builtin; unknown arguments produce the existing command error instead of a provider request.
- [ ] Handle the command locally when idle, request/reuse the recap, clear only the composer command text, and never enqueue or submit it as a user turn.
- [ ] For non-idle/modal states, preserve existing gates; do not add a broad “recap catches all input” branch.
- [ ] Add detail open/close and click behavior only after approval/plan/user-input/search/child Esc handling; short strip remains non-focusable.
- [ ] Test parser rejection, idle request, cache reuse, modal priority, Esc behavior, mouse geometry and “no `ChatMessage` pushed”.
- [ ] Run:

```bash
cargo test -p orca-tui commands --lib -- --test-threads=1
cargo test -p orca-tui idle_submit --lib -- --test-threads=1
cargo test -p orca-tui input_event_actions --lib -- --test-threads=1
```

## Task 7: Add focus-return automatic generation

**Files:**

- Modify: `crates/orca-tui/src/input_event_actions.rs`
- Modify: `crates/orca-tui/src/terminal_session.rs` only if focus events need a separate recap wake signal
- Modify: `crates/orca-tui/src/renderer_frame.rs` or `renderer_loop.rs` only to schedule a dirty redraw, never a new high-frequency timer
- Modify: `crates/orca-tui/src/app.rs` / hosted controller owner to submit the worker request off the renderer thread
- Modify: `crates/orca-tui/src/workspace_config.rs` and `orca-core/src/config/file.rs` only if the auto toggle is made persistent
- Tests: focus/auto eligibility and no-repeat tests

- [ ] On `FocusGained`, after existing focus routing, check `can_request_auto_recap()` using current time, terminal focus, last completed user operation, session attachment, background attention and marker history; the hosted controller asks runtime for a handle, while the renderer only marks the frame dirty when a result arrives.
- [ ] Require at least three completed user operations and three minutes since completion; use the existing focus event rather than a new interval timer. The current scheduler remains bounded by its existing frame interval.
- [ ] Deduplicate by session/marker/focus cycle; never auto retry a failed request until a new completed operation or new focus cycle satisfies the policy.
- [ ] Keep auto failures quiet and preserve an older ready recap only when its marker still matches.
- [ ] Add tests for three-turn/three-minute thresholds, focus loss/gain, waiting approval, child focus, noninteractive mode, same-marker no-repeat, and manual-after-auto-failure retry.
- [ ] Run:

```bash
cargo test -p orca-tui focus --lib -- --test-threads=1
cargo test -p orca-tui auto_recap --lib -- --test-threads=1
```

## Task 8: Full verification and review of the dirty visual branch

- [ ] Run `git diff --check` and verify the worktree still contains only the intended two untracked plan/spec documents; inspect `git show --check 025d814c` separately for the visual-unification commit.
- [ ] Run the focused provider/runtime/TUI tests from Tasks 1–7.
- [ ] Run the existing visual-unification checks in the target worktree:

```bash
cargo fmt --all -- --check
cargo test -p orca-tui --lib -- --test-threads=1
cargo clippy -p orca-tui --all-targets -- -D warnings
```

- [ ] Inspect a `ratatui::TestBackend` frame for a 20-column and a normal terminal: composer/status stay fixed, recap is omitted first, and detail popup geometry matches its hit-test.
- [ ] Verify `git status --short` contains only the intended recap files and that `HEAD` remains `025d814c`; do not stage, commit, push, merge, or switch branches as part of this plan.
- [ ] If implementation is later requested, commit recap slices separately with conventional messages (`feat(provider): …`, `feat(runtime): …`, `feat(tui): …`) so the existing visual-unification work remains reviewable.
