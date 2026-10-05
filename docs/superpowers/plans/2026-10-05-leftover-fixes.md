# 遗留问题修复实施计划（v0.5.6 之后）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修掉 v0.5.6 之后留下的已知问题：
- 进程信号；
- 测试污染用户目录；
- MCP 客户端的收尾项；
- 恢复会话时的消息顺序；
- 配置注释。

**Architecture:**
- **Orca home 统一解析。** Orca home 的路径统一由 `orca_core::home::orca_home()` 解析。测试构建（`cfg(test)` 或 `test-utils` feature）下，它退回进程级临时目录。
- **信号处理提前。** 信号处理在资源创建之前装好。exec 把信号交给运行时的中断路径。TUI 把信号变成渲染循环里的事件，走正常退出的路径。
- **MCP 客户端。** 先拆模块、合并 SSE 解码器（不改行为），再逐项修行为。

**Tech Stack:** Rust（workspace crates `orca-core`、`orca-mcp`、`orca-runtime`、`orca-tools`、`orca-provider`、`orca-tui`）、tokio signal、`toml_edit`、nextest；PTY 契约测试。

**Spec:** `docs/superpowers/specs/2026-10-05-leftover-fixes-design.md`

## Global Constraints

- **平台与编译：**
  - 所有 crate 都开着 `#![deny(deprecated)]`。代码要同时能用本机的 Rust 1.95 和 CI 的最新 stable（1.99）编译。不要用 1.95 没有的 API，也不要用 1.99 已弃用的（比如 `AtomicUsize::fetch_update`）。
  - Windows 只在 CI 上编译。只能在 unix 上用的代码加 `#[cfg(unix)]`。在已有条目上方插入新条目时，检查有没有抢走原条目的 `#[cfg]`。
- **环境：** 本机的 `NODE_OPTIONS` 指向一个已经不存在的 preload。所有 `node`、`npm`、`cargo nextest`、`cargo test` 都加 `env -u NODE_OPTIONS` 前缀，否则会启动 node 的测试会失败。
- **测试：**
  - 用 `cargo nextest run`。
  - PTY 测试在 `tests/tui_pty_contract.rs`（`#![cfg(unix)]`），用 `--profile ci-serial` 跑。
  - 多个过滤词写成 `-E 'test(/a|b/)'`。
  - 测试里的 TCP 服务器在 `accept()` 之后调用 `set_nonblocking(false)`。
- **每次提交前：**
  - `cargo fmt --all -- --check`；
  - 直接运行（不接 `tail`）`env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs` 和 `env -u NODE_OPTIONS node scripts/validate-windows-platform-boundaries.mjs`。

  validator 因为固定的文件路径、行号、计数或摘要失败时，在同一个提交里更新清单（`docs/superpowers/specs/*.manifest.json`）或 validator 里的固定值，并在报告里写明。
- **测试不碰真实 home：**
  - 任务 1 完成之前，测试仍可能写到真实的 `~/.orca`，这是已知问题，不需要处理。
  - 任务 1 完成之后，任何测试都不得读写真实的 `~/.orca`。
- **文案：** 代码和面向用户的输出用英文；中英文档都要改。
- **提交：**
  - 每个任务至少一个提交，用 conventional 风格（`fix(scope): …`），正文说明原因和测试。
  - 不推送。

## Review Focus

1. **审阅首屏时收到信号。** 首次运行的工作区审阅界面上收到 SIGTERM，要以 143 干净退出、恢复终端，并且不启动任何 MCP 服务器。由任务 9 的 `tui_sigterm_during_the_workspace_review_exits_cleanly` 钉住。
2. **终端已关闭时收到 SIGHUP。** 写终端会失败（EIO），TUI 仍要在宽限期内退出、结束 MCP 服务器，不能卡住或空转。由任务 9 的 `tui_sighup_with_the_terminal_gone_still_stops_mcp_servers` 钉住。
3. **显式设置的 home 必须生效。** 测试构建下，设置了 `ORCA_HOME` 或按线程覆盖时，必须用它们，不能退回临时目录。由任务 1 的 `orca_home_prefers_the_override_then_the_environment` 钉住。
4. **CRLF 被拆在两次读取之间。** `\r` 和 `\n` 分在两次读取里，只算一个行尾，不能多切出一个空事件。由任务 2 的 `a_crlf_split_between_reads_is_one_line_end` 钉住。
5. **恢复失败时暂存的消息。** 启动时的恢复失败（`Unable to restore saved conversation.`）时，暂存的消息仍要排在命令行提示词之后，发到新会话。由任务 10 的 `held_submissions_follow_the_prompt_when_the_resume_fails` 钉住。

---

### Task 1：Orca home 统一解析，测试构建默认用临时目录

**Files:**
- Create: `crates/orca-core/src/home.rs`，并在 `crates/orca-core/src/lib.rs` 中声明 `pub mod home;`。
- Modify: `crates/orca-core/Cargo.toml`：
  - 增加 feature `test-utils = ["dep:tempfile"]`；
  - `tempfile` 改为 optional 的普通依赖（workspace 版本）。
- Modify: 把以下解析 Orca home 的地方都改为调用 `orca_core::home::orca_home()`：
  - `crates/orca-core/src/config/file.rs:289`（`config_dir`）；
  - `crates/orca-core/src/config/folder_trust.rs:86`（`config_dir`），线程覆盖的 `install_test_orca_home`、`install_host_orca_home`、`current_orca_home_override` 移到 `home.rs`，并更新调用方；
  - `crates/orca-runtime/src/`：
    - `tasks.rs:7037`（`task_sessions_root`，去掉 `cfg(test)` 特判）；
    - `memory.rs:569`；
    - `instructions.rs:111`；
    - `update_check.rs:411`；
    - `thread_store/local.rs:1026`；
    - `mentions.rs:332,443,1074`（只改 `.orca` 那部分；`.agents` 不改）；
  - `crates/orca-runtime/src/history.rs`：`read_test_orca_home` 等测试辅助函数改为基于 `orca_home()`，保留现有行为；
  - `crates/orca-tools/src/`：`skills.rs:69`（只改 `.orca`）、`external.rs:22`；
  - `crates/orca-provider/src/summary_cache.rs:81`；
  - `crates/orca-tui/src/`：`goal_materialization.rs:335`、`input_history.rs:8`（现在完全忽略 `ORCA_HOME`，改后遵从）。
- Do not touch:
  - 沙箱规则：`orca-tools/src/sandbox/*`；
  - `workflow_execution.rs:601`；
  - `goal_store.rs:5512`；
  - `workspace_status.rs:155`。

  它们用的是用户主目录本身，不是 Orca home。
- Modify: 以下 Cargo.toml 的 `[dev-dependencies]` 增加 `orca-core = { path = …, features = ["test-utils"] }`：
  - 根目录的 `Cargo.toml`；
  - `crates/orca-runtime/Cargo.toml`；
  - `crates/orca-tools/Cargo.toml`；
  - `crates/orca-provider/Cargo.toml`；
  - `crates/orca-tui/Cargo.toml`；
  - `crates/orca-mcp/Cargo.toml`；
  - 其他测试会走到 Orca home 的 crate。
- Modify: `.github/workflows/runtime-contract.yml`：
  - 在 "Validate dependency graph and Rust inventory" 之前加一步，`touch "$RUNNER_TEMP/orca-home-marker"`；
  - 在 "Run workspace gate" 之后加一步 "Check that tests left the user's Orca home alone"：

    ```bash
    if [ -e "$HOME/.orca" ] && [ -n "$(find "$HOME/.orca" -newer "$RUNNER_TEMP/orca-home-marker" -print -quit)" ]; then
      echo "tests wrote to $HOME/.orca:"; find "$HOME/.orca" -newer "$RUNNER_TEMP/orca-home-marker" | head -50; exit 1
    fi
    ```
- Test: `crates/orca-core/src/home.rs` 的测试模块；`crates/orca-tui/src/input_history.rs` 的测试模块。

**Interfaces:**
- **Produces:**
  - `pub fn orca_home() -> Option<PathBuf>`：依次取按线程覆盖、`ORCA_HOME`、`~/.orca`。
    - 在 `cfg(any(test, feature = "test-utils"))` 下，没有覆盖、也没有 `ORCA_HOME` 时，返回本进程第一次调用时建好的临时目录，之后每次都返回同一个。
    - 第一次退回临时目录时，同时把 `ORCA_HOME` 设成这个目录，让子进程继承，做法同现有的 `history::isolated_test_orca_home`。
  - `pub fn install_test_orca_home(home: Option<PathBuf>)`、`pub fn install_host_orca_home(home: Option<PathBuf>)`、`pub fn current_orca_home_override() -> Option<PathBuf>`：从 `folder_trust` 移过来，语义不变（`#[doc(hidden)]`）。
- **Consumes:** 无。

- [ ] **Step 1：写失败的测试。**
  - `orca_home_falls_back_to_a_process_temp_dir_in_tests`（home.rs）：持有一个测试内的环境锁，移除 `ORCA_HOME`。断言：
    - `orca_home()` 位于 `std::env::temp_dir()` 之下；
    - 不等于 `dirs::home_dir().join(".orca")`；
    - 两次调用结果相同。
  - `orca_home_prefers_the_override_then_the_environment`：
    - 设置 `ORCA_HOME=/a` 时返回 `/a`；
    - 再 `install_test_orca_home(Some("/b"))` 时返回 `/b`；
    - 清除后恢复原值。
  - `the_input_history_lives_under_the_orca_home`（input_history.rs）：历史文件路径以 `orca_core::home::orca_home()` 开头。
- [ ] **Step 2：确认测试失败。**
  - `env -u NODE_OPTIONS cargo nextest run -p orca-core --lib --locked --retries 0 -E 'test(/orca_home_/)'`：编译失败，因为 `home` 模块还不存在。
  - `env -u NODE_OPTIONS cargo nextest run -p orca-tui --lib --locked --retries 0 -E 'test(/the_input_history_lives_under/)'`：失败。
- [ ] **Step 3：实现。**
  - 写 `home.rs`。
  - 按 Files 改各处调用。
  - 加 feature 和 dev-dependency。
  - 改 CI 工作流。
- [ ] **Step 4：确认不再写真实 home。**
  - 先记下 `ls ~/.orca/task-sessions | wc -l` 和 `ls -la ~/.orca`；
  - 跑 `env -u NODE_OPTIONS cargo nextest run --workspace --all-targets --locked --profile ci --no-fail-fast --retries 0`；
  - 再跑 `env -u NODE_OPTIONS cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0`；
  - 再比对。task-sessions 的条目数不变，`~/.orca` 下没有比开始时间更新的文件。

  有新增的话，找到写入的测试，修好再继续。
- [ ] **Step 5：确认测试通过。** Step 2 的命令，加上各 crate 的 lib 测试。
- [ ] **Step 6：提交。** `fix(test): resolve the Orca home in one place and keep tests out of the user's`

### Task 2：MCP 客户端拆分与 SSE 解码器（不改行为）

**Files:**
- Create:
  - `crates/orca-mcp/src/registry.rs`：从 `client.rs` 移入以下内容：
    - `McpRegistry`、`McpRegistryShared`、`McpSubscribers`、`McpChangeSubscription`；
    - `McpServerState`、`McpServerStatus`、`McpRegistrySnapshot`、`McpRegistryInner`、`McpServerEntry`；
    - `initialize_registry`、`connect_server*`、`ConnectedServer`；
    - `collect_pages`、`MAX_LIST_PAGES`、`incomplete_list`；
    - prompt 与资源列表的辅助函数、`McpPromptExpansion`、`McpResourceListing`、`McpResourceTemplateListing`、`McpCallOutput`；
    - `impl McpRegistry`，以及 `canonical_server_name`、`normalize_schema`、`FIRST_CONNECTION`、`STARTUP_POLL`。
  - `crates/orca-mcp/src/connection_stop.rs`：`McpConnectionStop`、`McpConnectionStopState` 及其 impl。
  - `crates/orca-mcp/src/registry/tests.rs`：原 `client.rs` 的整个 `mod tests`，原样移入。`registry.rs` 里写 `#[cfg(test)] mod tests;`。
  - `crates/orca-mcp/src/sse.rs`：SSE 解码器及其单元测试。
- Modify:
  - `crates/orca-mcp/src/client.rs`：只留下 `McpClient` 及其 impl（请求、重连、重启）、`McpServerCapabilities`、`McpRequestError`（含 `Display`、`From`），以及 `MCP_TOOL_CALL_CANCELLED`、`CANCELLED_STDIO_RECONNECT_TIMEOUT_MS`、`should_reconnect_after_mcp_error`。
  - `crates/orca-mcp/src/lib.rs`：声明新模块。对外导出保持不变：`pub use registry::{…}` 加上 `pub use client::McpRequestError`。`pub mod client` 保留。
  - `crates/orca-mcp/src/transport.rs`：`read_sse_stream`（1958 行附近）、`read_sse_response`（2259）、`parse_sse_or_json_response`（2398）改用 `SseDecoder`，删掉 `sse_event_end`、`sse_event_parts`、`parse_sse_event`。
  - `crates/orca-mcp/src/legacy_sse.rs:614,686`：改用 `SseDecoder`。
  - 清单：
    - `docs/superpowers/specs/2026-07-28-native-windows-platform-foundation.manifest.json`：行 `mcp-client-unix-fixtures` 的路径和计数，随测试移到 `registry/tests.rs`；
    - `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`：行 `mcp.registry` 改指向 `crates/orca-mcp/src/registry.rs` 里 `initialize_registry` 所在的行。

**Interfaces:**
- **Produces（`sse.rs`）：**

  ```rust
  #[derive(Debug, Clone, PartialEq, Eq, Default)]
  pub(crate) struct SseEvent {
      pub(crate) event: Option<String>,
      pub(crate) id: Option<String>,
      /// Every `data:` line, joined with `\n`.
      pub(crate) data: String,
  }
  #[derive(Debug, Default)]
  pub(crate) struct SseDecoder { /* buffered bytes and the event being read */ }
  impl SseDecoder {
      pub(crate) fn new() -> Self;
      /// Takes the next bytes of the stream; returns the events they end, in order.
      pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<SseEvent>, String>;
      /// The event the stream ended inside, without the blank line that ends it.
      pub(crate) fn finish(self) -> Result<Option<SseEvent>, String>;
  }
  ```

  规则按 WHATWG event-stream：
  - 行尾可以是 CRLF、CR 或 LF；
  - 空行结束一个事件；
  - 以 `:` 开头的行是注释；
  - 冒号后的第一个空格去掉；
  - 没有任何 `data` 行的事件不返回；
  - 非 UTF-8 的行报错。

  字节上限仍由调用方按 `MAX_SSE_RESPONSE_BYTES` 检查。
- **Produces（可见性）：** 测试和后续任务要用的 `McpClient` 方法改为 `pub(crate)`：`lock_transport`、`reconnect`、`lock_reconnect_failure`、`take_reconnect_failure`。不加任何新的 `pub`。

- [ ] **Step 1：写解码器的失败测试**（`sse.rs`）。
  - `an_event_split_across_reads_comes_out_whole`：
    - `push(b"data: {\"a\"")` 返回空；
    - 再 `push(b":1}\n\n")` 返回一个事件，`data == "{\"a\":1}"`。
  - `crlf_and_cr_line_ends_end_events`：`push(b"data: x\r\n\r\ndata: y\r\rdata: z\n\n")` 返回三个事件，`data` 依次为 `x`、`y`、`z`。
  - `a_crlf_split_between_reads_is_one_line_end`：
    - `push(b"data: x\r")` 返回空；
    - `push(b"\n\r\n")` 返回恰好一个事件 `x`；
    - 再 `push(b"data: y\n\n")` 返回 `y`。
  - `comment_lines_are_skipped`：`push(b": keepalive\n\ndata: x\n\n")` 只返回 `x`。
  - `data_lines_are_joined_with_newlines`：`push(b"data: a\ndata: b\n\n")` 的 `data == "a\nb"`。
  - `event_and_id_fields_are_kept`：`push(b"event: endpoint\nid: 7\ndata: /messages\n\n")` 返回 `event == Some("endpoint")`、`id == Some("7")`、`data == "/messages"`。
  - `an_event_without_data_is_not_returned`：`push(b"event: ping\n\n")` 返回空。
  - `finish_returns_an_unterminated_event`：`push(b"data: x")` 之后，`finish()` 返回 `Some`，其 `data == "x"`。
- [ ] **Step 2：确认失败。** `env -u NODE_OPTIONS cargo nextest run -p orca-mcp --lib --locked --retries 0 -E 'test(/sse::/)'`：编译失败。
- [ ] **Step 3：实现解码器，替换四处调用。** 先确认 `cargo nextest run -p orca-mcp --lib` 全绿，再进行拆分。
- [ ] **Step 4：拆分模块。**
  - 只移动代码、调整 `use` 和可见性，不改任何逻辑。
  - `client.rs` 与 `registry.rs` 互相引用的地方用 `pub(crate)`。
  - 跑两个 validator，按失败信息更新清单。
- [ ] **Step 5：确认通过。**
  - `env -u NODE_OPTIONS cargo nextest run -p orca-mcp --locked --profile ci --retries 0`：全部通过，测试数与拆分前相同（拆分前 181 个，加上 8 个解码器测试）；
  - `env -u NODE_OPTIONS cargo nextest run -p orca-runtime -p orca-tools --lib --locked --retries 0`；
  - fmt 与两个 validator。
- [ ] **Step 6：提交。** 分两个提交：
  - `refactor(mcp): read every server-sent event stream with one decoder`
  - `refactor(mcp): split the registry and the connection stop out of client.rs`

### Task 3：可取消的等锁，以及准确的重连失败记录

**Files:**
- Modify: `crates/orca-mcp/src/client.rs`：
  - `request_or_cancel`；
  - `restart_closed_stdio_server`；
  - `reconnect`；
  - 新增 `lock_transport_or_cancel`、`note_reconnect`。
- Test: `crates/orca-mcp/src/registry/tests.rs`

**Interfaces:**
- **Consumes:** 任务 2 的模块结构。
- **Produces:**

  ```rust
  /// What a reconnect did when it did not fail.
  #[derive(Debug, PartialEq, Eq)]
  pub(crate) enum Reconnected { Replaced, AlreadyReplaced }
  /// Why a reconnect left no new transport in place.
  #[derive(Debug, PartialEq, Eq)]
  pub(crate) enum ReconnectFailure { Cancelled, CapReached(String), Failed(String) }
  pub(crate) fn reconnect(&self, failed: &Arc<dyn McpTransport>, startup_timeout_cap_ms: Option<u64>,
      should_cancel: &dyn Fn() -> bool) -> Result<Reconnected, ReconnectFailure>;
  /// Keeps the stdio reconnect failure record in step with `outcome`:
  /// Replaced clears it, Failed(e) sets it to e, anything else leaves it.
  pub(crate) fn note_reconnect(&self, outcome: &Result<Reconnected, ReconnectFailure>);
  /// The transport, waiting for it in 25 ms steps while `should_cancel` allows.
  fn lock_transport_or_cancel(&self, should_cancel: &dyn Fn() -> bool)
      -> Result<MutexGuard<'_, Arc<dyn McpTransport>>, McpRequestError>;
  ```

  - `CapReached` 的判定：带上限的重连失败，并且从重连开始已经过了上限（`Instant` 计时）。不匹配错误文字。
  - `Cancelled` 的判定：失败时 `should_cancel()` 为真。
  - 请求路径和重启路径的等锁都改用 `lock_transport_or_cancel`；带取消的 stdio 重连也改用它。

- [ ] **Step 1：写失败的测试。**
  - `a_cancellable_call_waiting_for_the_transport_returns_when_cancelled`：
    - 夹具用 `restarting_server_config(dir, Some(0))`；
    - 测试线程持有 `client.lock_transport()`，模拟一次不可取消的重启；
    - 另一个线程发起 `call_tool_or_cancel`，100 ms 后置位取消。

    断言：
    - 调用在置位后 500 ms 内返回 `"MCP tool call cancelled"`，此时锁仍被持有；
    - 释放锁之后，`started` 文件不存在，即服务器没有收到这次调用。
  - `a_reconnect_that_found_the_transport_replaced_keeps_the_recorded_failure`：
    - 先设 failure 为 `Some("boom")`；
    - `note_reconnect(&Ok(AlreadyReplaced))` 之后仍是 `"boom"`；
    - `Err(Cancelled)`、`Err(CapReached(_))` 也都保持不变；
    - `Ok(Replaced)` 之后为 `None`；
    - `Err(Failed("x"))` 之后为 `"x"`。
  - `a_cancelled_calls_short_restart_that_fails_at_once_fails_the_server`：
    - 夹具用 `restarting_server_config(dir, None)`，第二代启动即退出；
    - 第一次调用在 `started` 出现时取消。

    断言：
    - 返回取消；
    - 紧接着 `server_statuses()[0].state` 是 `Failed { .. }`，不用等下一次调用。
  - 删除被上一个测试取代的 `a_server_that_cannot_start_again_after_a_cancel_fails_the_next_call`。它断言取消后状态仍是 `Ready`，与新行为相反。
- [ ] **Step 2：确认失败。** `env -u NODE_OPTIONS cargo nextest run -p orca-mcp --lib --locked --retries 0 -E 'test(/waiting_for_the_transport|found_the_transport_replaced|short_restart_that_fails_at_once/)'`
- [ ] **Step 3：实现。** 按 Interfaces 改。三处记录失败的地方（短重连、完整重启、重启已关闭的传输）都改为调用 `note_reconnect`，只对 stdio 生效。
- [ ] **Step 4：确认通过。**
  - Step 2 的命令；
  - `env -u NODE_OPTIONS cargo nextest run -p orca-mcp --locked --profile ci --retries 0`；
  - 新测试连跑 10 次（`--retries 0`，循环 10 次）全绿。
- [ ] **Step 5：提交。** `fix(mcp): let a cancel through while a call waits for its server, and record reconnect failures accurately`

### Task 4：stdio 空闲时也应答服务器的请求

**Files:**
- Modify: `crates/orca-mcp/src/transport.rs`：
  - `StdioTransport` 的启动与读取线程；
  - `StdioState`：去掉 `stdin`；
  - `request_with_timeout`（660 行附近）：去掉它自己对 `ping` 和未知请求的应答，保留 elicitation；
  - `write_json_line` 的调用处。
- Test: `crates/orca-mcp/src/transport.rs` 的测试模块（`#[cfg(unix)]`）

**Interfaces:**
- **Produces:**
  - `struct StdioWriter(Arc<Mutex<ChildStdin>>)`：请求和读取线程共用，每次只在写一行时持锁。
  - `in_flight: Arc<AtomicBool>`：`request_with_timeout` 等待期间为真。
  - 读取线程收到服务器请求时：
    - `ping` 回 `{"jsonrpc":"2.0","id":<id>,"result":{}}`；
    - `elicitation/create` 在 `in_flight` 为真时照旧送进响应通道，否则回 `-32601`；
    - 其他请求按现有的 `server_request_reply` 回 `-32601`。

- [ ] **Step 1：写失败的测试。** 夹具脚本收到 `notifications/initialized` 后，主动发一个请求，然后把读到的下一行写进 `$REPLY_FILE`。测试只做 `initialize`，之后不再发任何请求，最多等 2 秒读 `$REPLY_FILE`：
  - `an_idle_stdio_server_gets_its_ping_answered`：请求为 `{"jsonrpc":"2.0","id":"p1","method":"ping"}`，回复解析后等于 `{"jsonrpc":"2.0","id":"p1","result":{}}`。
  - `an_unknown_request_from_an_idle_server_gets_method_not_found`：请求 `x/unknown`，回复的 `error.code == -32601`。
  - `an_elicitation_from_an_idle_server_is_refused`：请求 `elicitation/create`，回复的 `error.code == -32601`。
- [ ] **Step 2：确认失败。** `env -u NODE_OPTIONS cargo nextest run -p orca-mcp --lib --locked --retries 0 -E 'test(/from_an_idle_server|idle_stdio_server/)'`：2 秒内收不到回复。
- [ ] **Step 3：实现。**
- [ ] **Step 4：确认通过。**
  - Step 2 的命令；
  - `env -u NODE_OPTIONS cargo nextest run -p orca-mcp --locked --profile ci --retries 0`，其中原有的 elicitation 与 ping 测试仍要通过。
- [ ] **Step 5：提交。** `fix(mcp): answer a stdio server's requests while no call is in flight`

### Task 5：资源列表被截断时告诉模型

**Files:**
- Modify: `crates/orca-tools/src/registry.rs`：`execute_list_mcp_resources`（2023 行附近）与 `execute_list_mcp_resource_templates`。指定了服务器时，也改用 `list_resources_with_errors_or_cancel` 和 `list_resource_templates_with_errors_or_cancel`，传入 `Some(server)`。
- Modify: `crates/orca-mcp/src/registry.rs`：指定服务器时，`_with_errors` 系列把截断警告放进 `errors`。列表请求本身失败时仍返回 `Err`，不变成 `errors`。
- Modify: `site/src/docs/md/en/mcp-integration.mdx`、`site/src/docs/md/zh/mcp-integration.mdx`：说明截断时模型看到的内容。
- Test: `crates/orca-tools/src/registry.rs` 的测试模块

**Interfaces:**
- **Produces:** 无论是否指定服务器，两个工具的输出都是 `{"resources": [...], "errors": [...]}`（模板工具的键为 `resourceTemplates`，与现在的全量输出一致）。

- [ ] **Step 1：写失败的测试。**
  - `a_cut_short_resource_list_of_one_server_tells_the_model`：用一个实现 `McpTransport` 的测试传输，`resources/list` 永远返回同一个 `nextCursor`。调用 `list_mcp_resources`，参数 `{"server": "<名字>"}`。断言：
    - 输出 JSON 的 `errors` 中有一条包含 `MCP server repeated a list cursor`；
    - `resources` 非空。
  - `a_cut_short_template_list_of_one_server_tells_the_model`：模板工具同理。
- [ ] **Step 2：确认失败。** `env -u NODE_OPTIONS cargo nextest run -p orca-tools --lib --locked --retries 0 -E 'test(/cut_short/)'`
- [ ] **Step 3：实现，并改两份文档。**
- [ ] **Step 4：确认通过。**
  - Step 2 的命令；
  - `env -u NODE_OPTIONS cargo nextest run -p orca-tools -p orca-mcp --lib --locked --retries 0`；
  - `env -u NODE_OPTIONS npm --prefix site run build`。
- [ ] **Step 5：提交。** `fix(mcp): tell the model when one server's resource list was cut short`

### Task 6：登录取消

**Files:**
- Modify: `crates/orca-mcp/src/oauth.rs`：`login`（163 行附近）。`discover`、读取受保护资源元数据、`authorization_server`、`register` 四步改由 `until_cancelled` 执行。
- Modify: `crates/orca-mcp/src/oauth/test_server.rs`：给 `OAuthTestBehavior` 增加 `hold_path: Option<(String, TokenGate)>`，挂起对该路径的请求，直到放行。
- Modify: `crates/orca-mcp/src/oauth.rs`：在 `McpLoginCancel` 上增加 `#[doc(hidden)] pub fn finishing_for_test() -> Self`，用 `cfg(any(test, feature = "test-utils"))` 限定。
- Modify: `crates/orca-tui/src/mcp_dialog_actions.rs:66`（`start_action`）。
- Test: `oauth.rs` 的测试模块；`crates/orca-tui/src/mcp_dialog_actions.rs` 或 `mcp_server_actions.rs` 的测试模块

**Interfaces:**
- **Produces:**
  - `fn until_cancelled<T: Send + 'static>(cancel: &McpLoginCancel, name: &str, step: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String>`：
    - 在名为 `orca-mcp-login-step` 的线程上执行 `step`，以 25 ms 为步长等待结果；
    - `cancel.is_cancelled()` 时立即返回 `Err(cancelled(name))`，结果丢弃；
    - `reqwest::blocking::Client` 克隆进线程。
  - TUI 里，在等待浏览器的登录上按 `l`，如果 `cancel.cancel()` 返回 false 且 `cancel.is_finishing()`，就推送一条系统消息 `login to MCP server <name> is finishing and can no longer be cancelled`，不再显示 `an MCP action for <name> is already running`。

- [ ] **Step 1：写失败的测试。**
  - `a_login_cancelled_during_discovery_stops_at_once`：
    - 测试服务器挂起 `/.well-known/oauth-protected-resource` 的请求；
    - 登录在线程上运行，100 ms 后取消。

    断言：
    - 登录在取消后 1 秒内返回取消错误；
    - `open_browser` 从未被调用；
    - 收尾时放行挂起的请求。
  - `a_login_cancelled_after_the_callback_port_is_bound_frees_it`：
    - 先取一个空闲端口，设为 `oauth_callback_port`；
    - 使用动态注册，`cancel_on: Some(("/register", cancel))`。

    断言：
    - 返回取消；
    - `TcpListener::bind(("127.0.0.1", port))` 成功；
    - `open_browser` 从未被调用。
  - `pressing_l_once_the_browser_is_back_says_the_login_is_finishing`（TUI）：
    - 状态里有一个 `LoggingIn { cancel: McpLoginCancel::finishing_for_test(), .. }`；
    - 选中该服务器后按 `l`。

    断言：
    - 推送的消息含 `is finishing and can no longer be cancelled`；
    - 没有出现 `already running`。
- [ ] **Step 2：确认失败。**
  - `env -u NODE_OPTIONS cargo nextest run -p orca-mcp --lib --locked --retries 0 -E 'test(/cancelled_during_discovery|after_the_callback_port_is_bound/)'`
  - `env -u NODE_OPTIONS cargo nextest run -p orca-tui --lib --locked --retries 0 -E 'test(/once_the_browser_is_back/)'`

  如果端口那个测试在修改前就能通过，在报告里写明。它覆盖的是此前没有测试的分支，不一定要先失败。
- [ ] **Step 3：实现。**
- [ ] **Step 4：确认通过。**
  - Step 2 的命令；
  - `env -u NODE_OPTIONS cargo nextest run -p orca-mcp --locked --profile ci --retries 0`；
  - `env -u NODE_OPTIONS cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0`。
- [ ] **Step 5：提交。** `fix(mcp): stop a login at once when it is cancelled before the browser opens`

### Task 7：prestart 警告不重复，补上启动失败分支的测试

**Files:**
- Modify: `crates/orca-mcp/src/registry.rs`：在首次启动结束那一刻（第一次没有服务器处于 `Starting`）记下各服务器状态的快照。
- Modify: `crates/orca-runtime/src/mcp_startup.rs`：`mcp_startup_warnings` 改用这份快照。
- Test:
  - `crates/orca-runtime/src/mcp_startup.rs` 的测试模块（没有的话就新建）；
  - `crates/orca-tui/src/prestart_mcp.rs` 的测试模块。

**Interfaces:**
- **Produces:** `pub fn startup_statuses(&self) -> Option<Vec<McpServerStatus>>`（`McpRegistry`）：首次启动结束之前为 `None`，之后固定为结束那一刻的状态。之后的重连、登录都不改变它。

- [ ] **Step 1：写失败的测试。**
  - `warnings_after_a_hand_off_leave_out_a_failure_already_reported`：
    - 注册表里有一个服务器，首次启动成功；
    - 启动结束后，`reconnect_server` 让它失败（夹具第二代启动即退出）；
    - 然后 `McpStartupWarnings::watch(&registry)`，模拟交接。

    断言：`on_ended` 报告的警告为空。
  - `a_failed_thread_start_keeps_the_prestarted_servers_and_starts_each_once`（TUI）：
    - 使用 `prestart_mcp.rs` 测试里现有的 harness 和一个会写 pid 的服务器；
    - 第一条消息之前，用一个不存在的会话 id（`00000000-0000-4000-8000-000000000000`）触发 `ResumeSavedSession`，线程启动失败；
    - 然后发送一条消息。

    断言：
    - 新线程启动；
    - 服务器的 pid 文件恰好一行；
    - 服务器显示 `Ready`。
- [ ] **Step 2：确认失败。**
  - `env -u NODE_OPTIONS cargo nextest run -p orca-runtime --lib --locked --retries 0 -E 'test(/leave_out_a_failure_already_reported/)'`
  - `env -u NODE_OPTIONS cargo nextest run -p orca-tui --lib --locked --retries 0 -E 'test(/keeps_the_prestarted_servers_and_starts_each_once/)'`

  第二个测试覆盖此前没有测试的分支，在修改前就通过的话，在报告里写明。
- [ ] **Step 3：实现。**
- [ ] **Step 4：确认通过。**
  - Step 2 的命令；
  - `env -u NODE_OPTIONS cargo nextest run -p orca-mcp -p orca-runtime --lib --locked --retries 0`；
  - orca-tui lib（`ci-serial`）。
- [ ] **Step 5：提交。** `fix(mcp): report a handed-over registry's startup as it ended, not as it stands at hand-off`

### Task 8：`orca exec` 先装信号处理再启动

**Files:**
- Modify: `crates/orca-runtime/src/controller.rs`：
  - 用 `TerminationSignalHandler` 取代 `install_termination_signal_handler`（1446 行附近）；
  - `run_inner`（1557 行附近）在 `RuntimeHost::start()` 之前调用 `install`，线程启动后调用 `attach`；
  - `attach` 时已收到信号的话，不提交本轮提示，直接走中断收尾：关闭线程（随之关闭 MCP 注册表、结束 stdio 服务器）、写出终止记录、返回 `128 + 信号`。
- Modify: `tests/signal_contract.rs`：`spawn_orca_with_a_slow_mcp_server` 增加一个参数，表示写 pid 之前的等待秒数。
- Modify: `site/src/docs/md/en/orca-exec.mdx`、`site/src/docs/md/zh/orca-exec.mdx`：加一小节，内容见 spec §6。
- Test: `controller.rs` 的测试模块（`#[cfg(unix)]`）；`tests/signal_contract.rs`

**Interfaces:**
- **Produces:**

  ```rust
  /// SIGINT/SIGTERM handling for one headless run, in effect before the runtime starts.
  struct TerminationSignalHandler { /* channel to its thread */ }
  impl TerminationSignalHandler {
      /// Registers the handlers on a thread of its own and returns once they are in
      /// effect, or once registering failed, leaving no handler.
      fn install(interrupted: Arc<AtomicI32>, finished: Arc<AtomicBool>) -> Self;
      /// Hands over what stops the run: called at once if a signal already came,
      /// else when one comes.
      fn attach(&self, interrupt: Box<dyn FnOnce() + Send>);
  }
  ```

  宽限期（`INTERRUPT_GRACE_PERIOD`，10 秒）、第二个信号立即退出、stderr 提示，都保持现在的行为。Windows 上沿用现有 `TerminationSignals` 的 Console（Ctrl+C）分支，同样在启动之前装好。

- [ ] **Step 1：写失败的测试。**
  - `the_signal_handler_is_in_effect_once_install_returns`（controller.rs，unix）：
    - `install` 返回后立即 `libc::raise(libc::SIGTERM)`；
    - 1 秒内 `interrupted == 15`，进程还活着；
    - 结束前把 `finished` 置为 true，让宽限计时退出。
  - `a_signal_before_attach_stops_the_run_when_it_is_attached`：
    - `install` 之后先 `raise(SIGTERM)`，等到 `interrupted == 15`；
    - 再 `attach` 一个闭包，闭包置位一个 `AtomicBool`。

    断言：1 秒内被置位。最后置 `finished`。
  - `sigterm_while_mcp_servers_start_stops_them_and_commits_a_terminal`（signal_contract.rs）：
    - 服务器启动后立即写 pid（等待 0 秒），从不回应；
    - 以 JSONL 模式启动 `orca exec`，pid 文件一出现就发 SIGTERM。

    断言：
    - 退出码 143；
    - 1 秒内服务器进程不在了；
    - stdout 的 JSONL 里有终止记录，判断方式与同文件的 `run_interrupted` 相同。
- [ ] **Step 2：确认失败。**
  - `env -u NODE_OPTIONS cargo nextest run -p orca-runtime --lib --locked --retries 0 -E 'test(/signal_handler_is_in_effect|signal_before_attach/)'`：编译失败。
  - `env -u NODE_OPTIONS cargo nextest run --test signal_contract --locked --retries 0 -E 'test(/while_mcp_servers_start/)'`：这个测试受时序影响，修改前不一定失败。实际结果写进报告。
- [ ] **Step 3：实现，并改两份文档。**
- [ ] **Step 4：确认通过。**
  - Step 2 的命令；
  - `env -u NODE_OPTIONS cargo nextest run --test signal_contract --locked --retries 0`，原有的 SIGINT、SIGTERM 测试仍通过；
  - orca-runtime lib。
- [ ] **Step 5：提交。** `fix(runtime): handle termination signals in orca exec before anything starts`

### Task 9：TUI 处理 SIGTERM、SIGHUP、SIGINT

**Files:**
- Create: `crates/orca-tui/src/termination_signals.rs`（内容为 `#[cfg(unix)]`），在 `lib.rs` 中声明。
- Modify:
  - `crates/orca-tui/src/protocol.rs:236`：`TuiEvent` 增加 `TerminationSignal { signal: i32 }`；
  - `crates/orca-tui/src/renderer_event_router.rs`：处理该事件；
  - `crates/orca-tui/src/types.rs`：`AppState` 增加 `terminal_lost: bool`；
  - 绘制路径：`terminal_lost` 时不再绘制；
  - `crates/orca-tui/src/app.rs`：
    - 在启动 hosted 或 ACP runtime 之前安装信号处理；
    - `finish_tui_run` 之后置 `finished`；
    - 第 145 行附近：`terminal_lost` 时不打印续接提示，写 stdout 出错一律忽略。
- Modify: `tests/tui_pty_contract.rs`：`PtyProcess` 增加 `fn pid(&self) -> u32`。
- Modify: `site/src/docs/md/en/terminal-ui.mdx`、`site/src/docs/md/zh/terminal-ui.mdx`：内容见 spec §6。
- Test: `tests/tui_pty_contract.rs`

**Interfaces:**
- **Consumes:** 任务 8 的宽限期和"第二个信号立即退出"的语义（TUI 自己实现一份，常量保持一致：10 秒、25 ms 轮询）。
- **Produces:**
  - `pub(crate) fn install_tui_termination_signals(event_tx: mpsc::Sender<TuiEvent>, finished: Arc<AtomicBool>)`：在自己的线程上注册 SIGINT、SIGTERM、SIGHUP，注册生效后才返回。
    - 第一个信号：发 `TerminationSignal`，开始 10 秒宽限，到期且 `finished` 仍为假时以 `128 + 信号` 退出；
    - 第二个信号：立即以 `128 + 信号` 退出。
  - 渲染循环收到 `TerminationSignal { signal }` 时：
    - 有轮次在运行就先发 `UserAction::Interrupt`；
    - `signal == SIGHUP` 时置 `state.terminal_lost`；
    - 以 `128 + signal` 结束循环，于是走 `finish_tui_run`，与正常退出相同。
  - attach 模式（`orca attach`）走同一条路径：ACP runtime 的 shutdown 只断开它自己的连接，不影响 daemon 的会话。

- [ ] **Step 1：写失败的测试**（PTY）。慢服务器都用现有的 `SlowMcpServer`。
  - `tui_sigterm_quits_like_exit_and_stops_mcp_servers`：
    - `spawn_without_prompt` 并接受工作区，`wait_for_start` 拿到服务器 pid；
    - 然后 `kill -TERM <TUI pid>`。

    断言：
    - 5 秒内以 143 退出；
    - 1 秒内服务器进程不在了；
    - 信号之后的输出里有正常退出时写的终端恢复序列。先用 `arm_idle_exit` 的正常退出确认是哪一段，例如 `\x1b[?2004l`。
  - `tui_sighup_with_the_terminal_gone_still_stops_mcp_servers`：
    - 同样先启动并等服务器起来；
    - `close_io_and_join` 关掉 PTY 主端；
    - 再 `kill -HUP`。

    断言：
    - 5 秒内以 129 退出；
    - 服务器进程不在了。
  - `tui_sigterm_during_a_turn_interrupts_it`：
    - 用同文件里会让轮次持续运行的 mock 提示，即 `tui_cancel_returns_to_idle_through_the_runtime_surface` 用的那种；
    - 轮次开始后发 SIGTERM。

    断言：5 秒内以 143 退出。
  - `tui_sigterm_during_the_workspace_review_exits_cleanly`：
    - 新工作区，配置慢服务器；
    - 审阅界面出现后发 SIGTERM。

    断言：
    - 以 143 退出；
    - `server.started()` 为空。
- [ ] **Step 2：确认失败。** `env -u NODE_OPTIONS cargo nextest run --test tui_pty_contract --locked --profile ci-serial --retries 0 -E 'test(/tui_sigterm|tui_sighup/)'`：进程被信号直接杀掉，`status.code()` 为 `None`。
- [ ] **Step 3：实现，并改两份文档。**
- [ ] **Step 4：确认通过。**
  - Step 2 的命令；
  - 整个 PTY 套件（`--profile ci-serial`）；
  - orca-tui lib（`ci-serial`）。
- [ ] **Step 5：提交。** `fix(tui): quit like an ordinary exit on SIGTERM, SIGHUP and SIGINT`

### Task 10：恢复会话加载完之前，暂存发出的消息

**Files:**
- Modify:
  - `crates/orca-tui/src/types.rs`：`AppState` 增加两个字段：
    - `startup_history_pending: bool`；
    - `held_submissions: Vec<HeldSubmission>`。

    `HeldSubmission` 保存提交路径本来要推送的 `ChatMessage` 和要发送的 `UserAction`。
  - 输入框的提交路径（发出 `UserAction::Submit` 的函数）：`startup_history_pending` 为真时，只清空输入框、把消息显示出来，把这次提交存进 `held_submissions`，不发送。
  - `crates/orca-tui/src/renderer_runtime.rs:79`：`HistoryLoaded` 分支依次做三件事：
    1. 发出待发的命令行提示词（现有逻辑）；
    2. 按顺序推送并发出 `held_submissions`；
    3. 清除标志。
  - `crates/orca-tui/src/app.rs`：`typed_history_startup_eligible(...)` 为真时，置 `state.startup_history_pending = true`，有没有命令行提示词都一样。
- Test: `crates/orca-tui/src/renderer_runtime.rs` 的测试模块；`tests/tui_pty_contract.rs`

**Interfaces:**
- **Consumes:** 现有的 `pending_initial_prompt`。
- **Produces:** 上面描述的两个 `AppState` 字段，没有新的对外接口。

- [ ] **Step 1：写失败的测试。**
  - `held_submissions_follow_the_prompt_once_the_resumed_history_loads`（renderer_runtime.rs）：
    - owner 的待发提示词为 `"cli"`，`startup_history_pending = true`；
    - 经提交路径提交 `"typed"`，断言此时 `action_rx` 为空；
    - 送入 `HistoryLoaded`（带一条历史消息）。

    断言：
    - `action_rx` 依次收到 `Submit("cli")`、`Submit("typed")`；
    - transcript 依次为历史消息、`User("cli")`、`User("typed")`。
  - `held_submissions_follow_the_prompt_when_the_resume_fails`：同上，但 `HistoryLoaded` 不带消息，标签为 `Unable to restore saved conversation.`，顺序相同。
  - `a_message_typed_before_the_resumed_history_loads_runs_after_the_prompt`（PTY）：
    - 用 `record_a_conversation` 记录 `"pty resume seed"`；
    - 在新工作区 `spawn_resumed(..., "latest", "mock_history_echo")`；
    - 审阅界面出现后，一次写入 `b"\rheld message\r"`。

    断言：
    - 出现 `Mock history users: pty resume seed | mock_history_echo`；
    - 没有出现 `pty resume seed | held message`；
    - 最终屏幕上有 `held message`。
- [ ] **Step 2：确认失败。**
  - `env -u NODE_OPTIONS cargo nextest run -p orca-tui --lib --locked --retries 0 -E 'test(/held_submissions_follow/)'`
  - `env -u NODE_OPTIONS cargo nextest run --test tui_pty_contract --locked --profile ci-serial --retries 0 -E 'test(/typed_before_the_resumed_history/)'`
- [ ] **Step 3：实现。**
- [ ] **Step 4：确认通过。**
  - Step 2 的命令；
  - 整个 PTY 套件；
  - orca-tui lib（`ci-serial`）。
- [ ] **Step 5：提交。** `fix(tui): send messages typed before a resumed conversation loads after its prompt`

### Task 11：内联数组里的注释跟着条目走

**Files:**
- Modify: `crates/orca-core/src/config/user_edit.rs`：`push_inline_entry`（522 行附近）、`remove_inline_entry`（545 行附近）。
- Test: `crates/orca-core/src/config/user_edit.rs` 的测试模块

**Interfaces:**
- **Produces:** 无新接口。行为按 spec §5。
- **toml_edit 的约定：** 一个条目同一行、逗号之后的注释，存在下一个元素 prefix 的第一行里；如果它是最后一项，就存在数组的 trailing 里。删除和添加时，要把这部分当作前一个条目的行尾注释处理。

- [ ] **Step 1：写失败的测试。** 都用 `add_user_mcp_server_in` 或 `remove_user_mcp_server_in` 在临时目录上操作。断言只看行与行的关系，不比较条目本身的格式。基础输入：

  ```toml
  mcp_servers = [
    # docs server
    { name = "docs", command = "docs-mcp" }, # stable
    # tracker server
    { name = "tracker", command = "tracker-mcp" }, # beta
    # end of servers
  ]
  ```

  - `removing_an_inline_entry_takes_its_own_comments`：删除 `docs` 后，`# docs server` 和 `# stable` 都不在了。保留下来的几行仍按原来的相对位置排列：
    - `# tracker server` 是 tracker 条目的上一行；
    - `# beta` 在 tracker 条目那一行；
    - `# end of servers` 是 `]` 的上一行。
  - `removing_the_last_inline_entry_keeps_the_closing_comment`：删除 `tracker` 后，`# stable` 仍在 docs 那一行，`# end of servers` 是 `]` 的上一行，`# beta` 与 `# tracker server` 都不在了。
  - `an_added_inline_entry_goes_after_the_last_entrys_comment`：添加 `search` 后：
    - 新条目单独占一行，位于 tracker 条目那一行之后、`# end of servers` 之前；
    - `# beta` 仍在 tracker 那一行；
    - 文件能被 `toml` 解析，服务器依次为 docs、tracker、search。
  - `adding_after_an_entry_without_a_trailing_comma_keeps_its_comment`：输入的最后一项没有逗号，并带行尾注释：

    ```toml
    mcp_servers = [
      { name = "docs", command = "docs-mcp" },
      { name = "tracker", command = "tracker-mcp" } # beta
    ]
    ```

    添加 `search` 后：
    - tracker 那一行变成以 `, # beta` 结尾，也就是逗号在注释之前；
    - 新条目单独占一行；
    - 文件能被解析。
- [ ] **Step 2：确认失败。** `env -u NODE_OPTIONS cargo nextest run -p orca-core --lib --locked --retries 0 -E 'test(/its_own_comments|keeps_the_closing_comment|after_the_last_entrys_comment|without_a_trailing_comma_keeps/)'`
- [ ] **Step 3：实现。**
- [ ] **Step 4：确认通过。**
  - Step 2 的命令；
  - `env -u NODE_OPTIONS cargo nextest run -p orca-core --locked --profile ci --retries 0`，原有的内联数组测试仍通过；
  - `env -u NODE_OPTIONS cargo nextest run --test mcp_cli_contract --locked --retries 0`。
- [ ] **Step 5：提交。** `fix(config): keep each inline MCP server entry's comments with it`

### Task 12：清理 `~/.orca/task-sessions`（由控制会话执行，不派子代理）

在最终评审通过之后执行。这一步会动用户数据，需要用户确认。

- [ ] **Step 1：分类。** 在 `$CLAUDE_JOB_DIR/tmp` 写一个只读脚本，不进仓库，对 `~/.orca/task-sessions` 下的每个目录分类：
  - **真实**：目录名能对上 `~/.orca/sessions` 下记录的会话 id，或者 `sessions-index.sqlite3` 里的会话。读 sqlite 时以只读方式打开，不复制 `sessions-index.sqlite3`。
  - **测试遗留**：对不上任何会话。
  - **不确定**：读不了，或者名字格式不认识。
- [ ] **Step 2：报告。** 向用户报告每一类的数量、示例名字、修改时间范围，以及目标目录 `~/.orca/task-sessions.test-backup-<日期>/`，等用户确认。
- [ ] **Step 3：移动。** 用户确认后，用同一文件系统内的 `rename`，把"测试遗留"类整体移到备份目录。"真实"和"不确定"留在原处。
- [ ] **Step 4：复核并告知用户。** 复核剩余数量与报告一致，然后告诉用户备份所在的位置，由用户决定是否删除。

## 收尾

- **全量验证。** 任务 11 完成后、最终评审之前，由控制会话在本机跑全量检查，所有命令都加 `env -u NODE_OPTIONS`：
  - `cargo fmt --all -- --check`；
  - 两个 validator 及其自测；
  - workspace 的 `ci` profile；
  - orca-tui lib（`ci-serial`）；
  - PTY 套件；
  - `npm --prefix site run build`；
  - `npm --prefix site run check:seo`。
- **CI。** 要跑 Linux 和 Windows CI，需要推一个分支。推送前先征得用户同意。
- **发版。** 本子项目完成后是否发 v0.5.7，由用户决定。
