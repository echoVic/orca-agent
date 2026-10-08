# 依赖升级实施计划（子项目 2）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 Orca 的依赖全部升到最新正式版（Rust 兼容更新、Rust 大版本、TUI 栈、网站），并修好 bash 那一行在命令跑完后仍显示 `still running` 的问题。

**Architecture:**
- **still running：** 只改 orca-tui。命令后来的结束状态按 `task_id` 记在 `AppState` 里（`CommandEnds`），再写回那一行存着的结果 JSON。渲染照旧由 `terminal_output_display` 读这份 JSON，渲染代码不用改。
- **升级：** 在本地 main 上按影响面从小到大分步提交，每一步都要编译通过、全量测试通过。TUI 栈只能一步做完（ratatui-textarea 只支持 ratatui 0.30）。
- **TLS：** reqwest 0.13 用 `rustls-no-provider`。所有 HTTP 客户端都从 `orca_mcp::http` 构建，它在第一次构建前把 ring 装成进程默认的加密实现；证书由系统证书库验证。

**Tech Stack:** Rust workspace（orca-core、orca-mcp、orca-provider、orca-tools、orca-runtime、orca-tui）、nextest、ratatui 0.30、reqwest 0.13 + rustls、rusqlite 0.40、toml 1.1；网站：vite 8（Rolldown）、TypeScript 7、React 19.3。

**Spec:** `docs/superpowers/specs/2026-10-08-dependency-upgrades-design.md`

## Global Constraints

- **版本：**
  - 以执行时 crates.io / npm 上最新的正式版为准，不用预发布版。动手前用 `cargo info <crate>` 或 `env -u NODE_OPTIONS npm view <pkg> version` 查一次。spec 里的版本号是 2026-10-08 查到的下限。
  - 不动：`agent-client-protocol = "=0.10.4"`（以及它固定的 schema 版本）、nucleo 的 git 版本、`qwertty = "=0.1.6"`。
  - 某个大版本实在升不了：保留原来的大版本，在提交说明里写明原因，并告诉控制会话。
- **平台与编译：**
  - 所有 crate 都开着 `#![deny(deprecated)]`。代码要同时能用本机的 Rust 1.95 和 CI 的最新 stable（1.99）编译。
  - Windows 只在 CI 上编译。只能在 unix 上用的代码加 `#[cfg(unix)]`。在已有条目上方插入新条目时，检查有没有抢走原条目的 `#[cfg]`。
- **环境：** 本机的 `NODE_OPTIONS` 指向一个已经不存在的 preload。所有 `node`、`npm`、`npx`、`cargo nextest`、`cargo test` 都加 `env -u NODE_OPTIONS` 前缀。
- **测试：**
  - 用 `cargo nextest run`。PTY 测试在 `tests/tui_pty_contract.rs`（`#![cfg(unix)]`），用 `--profile ci-serial` 跑。
  - 多个过滤词写成 `-E 'test(/a|b/)'`。
  - 测试里的 TCP 服务器在 `accept()` 之后调用 `set_nonblocking(false)`。
  - 测试不读写真实的 `~/.orca`。
- **全量检查**（每个任务提交前都跑；命令直接运行，不要接 `tail`）：

  ```bash
  cargo fmt --all -- --check
  env -u NODE_OPTIONS node scripts/test-validate-runtime-surface-contract.mjs
  env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs
  env -u NODE_OPTIONS node scripts/test-validate-windows-platform-boundaries.mjs
  env -u NODE_OPTIONS node scripts/validate-windows-platform-boundaries.mjs
  env -u NODE_OPTIONS node scripts/test-validate-execution-broker-boundary.mjs
  env -u NODE_OPTIONS node scripts/validate-execution-broker-boundary.mjs
  env -u NODE_OPTIONS cargo nextest run --workspace --all-targets --locked --profile ci --no-fail-fast --retries 0
  env -u NODE_OPTIONS cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0
  env -u NODE_OPTIONS cargo nextest run --test tui_pty_contract --locked --profile ci-serial --retries 0
  ```

  从 Task 4 起，再加上 `env -u NODE_OPTIONS node scripts/test-validate-http-client-boundary.mjs` 和 `env -u NODE_OPTIONS node scripts/validate-http-client-boundary.mjs`。
- **校验器和基线：**
  - validator 因为固定的文件路径、行号、计数或摘要失败时，在同一个提交里更新清单（`docs/superpowers/specs/*.manifest.json`）或 validator 里的固定值，并在提交说明里写明。
  - golden frame（`crates/orca-tui/src/golden/*.txt`）有变化时：先看 diff、查清原因、写进提交说明，再用 `ORCA_UPDATE_GOLDEN=1` 重新生成。不能直接覆盖。
  - 测试里写死的报错文字或输出格式，因为依赖升级而变化时：新文字同样说清了问题（行列号、键名还在）才能更新期望值，并在提交说明里逐条列出。
- **Cargo.lock：** 改了 `Cargo.toml` 之后，第一次构建不加 `--locked`，让它更新锁文件；之后的测试都加 `--locked`。
- **磁盘：** 每个任务结束时看一眼 `df -h .`。可用空间低于 50 GB 就停下来告诉控制会话，由控制会话问用户要不要 `cargo clean`。
- **文案：** 代码和面向用户的输出用英文；中英文档都要改。
- **提交：**
  - 每个任务至少一个提交，用 conventional 风格（依赖升级用 `chore(deps): …`），正文说明原因、行为变化和测试。
  - 实现任务不推送。只有控制会话会把当前 main 推到 `ci/dependency-upgrades` 分支（用户 2026-10-08 已同意，只限这个分支）。不推 main，不打 tag。
- **真实 API key：** 把 `~/.orca/auth.json` 复制到临时目录，权限设为 600，用完删除。不改原文件的权限。不复制 `sessions-index.sqlite3`。
- **不碰用户的东西：** 不杀用户的 orca 进程，不碰用户已经开着的 "Orca" Ghostty 窗口。

## Review Focus

1. **大段输出的 bash 行。** 输出有几 MB、含中文和转义序列的 bash 行，状态被改写后，输出必须原样不变（`output` 字段逐字节相同）。由 Task 1 的 `writing_the_end_keeps_the_output_as_it_was` 钉住。
2. **恢复会话后任务列表晚到。** `HistoryLoaded` 先到、任务快照后到时，先显示的 `state unknown` 要被快照里的真实结束状态替换掉。由 Task 1 的 `a_task_list_after_the_restore_replaces_state_unknown` 钉住。
3. **设了代理时的本机服务。** 用户设了 `HTTP_PROXY`，`NO_PROXY` 里列了本机地址时，本机的 MCP 服务器仍要直连，不能被送进代理。由 Task 4 的 `the_environment_proxy_is_used_and_no_proxy_hosts_are_reached_directly` 钉住。
4. **按单词删除。** 输入框里 `foo_bar`、中文后面跟标识符时，Ctrl+W 按新的规则把 `foo_bar` 当成一个词删掉。由 Task 7 的 `word_deletion_keeps_an_identifier_with_underscores_together` 钉住。
5. **宽字符不超宽。** 带 emoji 组合序列、国旗、CJK 和歧义宽度字符的消息，升级 unicode-width 之后渲染出的每一行仍不超过给定宽度。由 Task 7 的 `messages_with_emoji_sequences_and_cjk_stay_within_the_width` 钉住。

---

## 开工前（控制会话）

- [ ] 确认 `target/` 已经清理过（用户在 Orca 里清理），`df -h .` 的可用空间足够。
- [ ] `git status` 干净，HEAD 是规格提交 52847023 之后的 main。

### Task 1：bash 那一行跟上命令的结束

**Files:**
- Modify: `crates/orca-tui/src/terminal_output.rs`
  - `TerminalPayload` 的 `_task_id` 改成 `task_id: String`（去掉 `#[serde(rename = "task_id")]`）；
  - 新增 `EndSource`、`CommandEnd`、`reported_task`、`reported_ends`、`task_list_end`、`unknown_end`、`with_end`，代码见 Step 3。
- Create: `crates/orca-tui/src/command_ends.rs`，在 `crates/orca-tui/src/lib.rs` 的 `mod clipboard_image;` 之后加 `mod command_ends;`。
- Modify: `crates/orca-tui/src/types.rs`：`AppState` 加字段 `pub(crate) command_ends: crate::command_ends::CommandEnds`，`AppState::new` 里写 `command_ends: Default::default(),`。
- Modify: `crates/orca-tui/src/state_reducer.rs`：`ToolCompleted`、`HistoryLoaded`、`NewSessionStarted` 三个分支。
- Modify: `crates/orca-tui/src/workflow_panel.rs`：`apply_workflow_tasks_update` 的最后一行之后、`apply_background_tasks_update` 末尾（`self.agent_workspace.reconcile(...)` 之后）各加一行 `self.learn_command_ends_from_tasks();`。
- Test: `crates/orca-tui/src/command_ends.rs` 和 `crates/orca-tui/src/terminal_output.rs` 的测试模块。
- Do not touch: runtime 的任何代码；渲染代码（`ui.rs` 里读 `terminal_output_display` 的地方）。

**Interfaces:**
- **Produces**（只在 orca-tui 内部用）：
  - `terminal_output::EndSource`：`Unknown < TaskList < Record`，表示一个结束状态有多可靠。
  - `terminal_output::CommandEnd { pub(crate) source: EndSource, state, return_reason, termination_reason, exit_code }`。
  - `terminal_output::reported_task(content: &str) -> Option<(String, Option<EndSource>)>`：一行结果对应的任务，以及它已知的结束状态（`None` 表示还在跑）。
  - `terminal_output::reported_ends(content: &str) -> Vec<(String, CommandEnd)>`：一个结果报告的结束状态（bash、`task_send_input`、`task_read_output` 的结果，或 `task_wait` 的 `tasks` 数组）。
  - `terminal_output::task_list_end(task: &BackgroundTaskSummary) -> Option<(String, CommandEnd)>`；`terminal_output::unknown_end() -> CommandEnd`；`terminal_output::with_end(content: &str, end: &CommandEnd) -> Option<String>`。
  - `AppState::learn_command_ends_from_result(&mut self, tool: &str, output: &str)`、`learn_command_ends_from_tasks(&mut self)`、`show_known_command_end(&mut self, index: usize)`、`settle_restored_command_rows(&mut self)`。
- **Consumes:** 无。

- [ ] **Step 1：写失败的测试。** 放在新文件 `command_ends.rs` 的测试模块里（先只建一个空的 `CommandEnds` 让模块能声明，测试会因为方法不存在而编译失败）。

  ```rust
  #[cfg(test)]
  mod tests {
      use crossbeam_channel as mpsc;
      use orca_core::task_types::BackgroundTaskSummary;
      use serde_json::json;

      use crate::protocol::TuiEvent;
      use crate::terminal_output::terminal_output_display;
      use crate::transcript_state::ChatMessage;
      use crate::types::AppState;

      fn state() -> AppState {
          let (tx, _rx) = mpsc::unbounded();
          AppState::new(tx, "0.0.0-test".to_string(), "mock".to_string(), "/tmp".to_string())
      }

      /// A shell result as the runtime writes it. `fields` override a call
      /// that returned while its command still ran.
      fn shell_result(task_id: &str, fields: serde_json::Value) -> String {
          let mut payload = json!({
              "task_id": task_id,
              "state": "running",
              "return_reason": "yield_elapsed",
              "termination_reason": null,
              "exit_code": null,
              "output": "4.0K\t.\n",
              "next_cursor": 6,
              "output_gap": null,
              "effective_deadline_ms": null,
              "deadline_source": null,
              "terminal": "pipe",
              "eof": false,
          });
          for (key, value) in fields.as_object().unwrap() {
              payload[key] = value.clone();
          }
          payload.to_string()
      }

      fn running(task_id: &str) -> String {
          shell_result(task_id, json!({}))
      }

      fn ended(task_id: &str, state: &str, exit_code: i64) -> String {
          shell_result(task_id, json!({
              "state": state,
              "return_reason": "terminal_observed",
              "termination_reason": "exited",
              "exit_code": exit_code,
              "eof": true,
          }))
      }

      /// Call `id` of `tool`, requested and completed with `output`.
      fn tool(state: &mut AppState, id: &str, tool: &str, output: String) {
          state.update(TuiEvent::ToolRequested {
              id: id.to_string(),
              name: tool.to_string(),
              target: None,
          });
          state.update(TuiEvent::ToolCompleted {
              id: id.to_string(),
              name: tool.to_string(),
              status: "completed".to_string(),
              output,
              diff: None,
              kind: None,
          });
      }

      /// The note the row of call `id` shows after its tool name.
      fn note(state: &AppState, id: &str) -> Option<String> {
          let output = state
              .transcript
              .messages
              .iter()
              .find_map(|message| match message {
                  ChatMessage::ToolCall { id: row, output, .. } if row == id => Some(output.clone()),
                  _ => None,
              })
              .expect("the row")
              .expect("an output");
          terminal_output_display(&output).expect("a shell result").note
      }

      fn shell_task(id: &str, status: &str) -> BackgroundTaskSummary {
          serde_json::from_value(json!({
              "id": id,
              "type": "shell",
              "status": status,
              "description": "du -sh .",
              "createdAtMs": 1,
          }))
          .expect("task summary")
      }

      fn restored(id: &str, tool: &str, output: String) -> ChatMessage {
          ChatMessage::ToolCall {
              id: id.to_string(),
              name: tool.to_string(),
              target: None,
              status: "completed".to_string(),
              output: Some(output),
              diff: None,
              kind: None,
              expanded: false,
          }
      }

      fn restore(state: &mut AppState, messages: Vec<ChatMessage>) {
          state.update(TuiEvent::HistoryLoaded {
              messages,
              plan: None,
              label: "Resumed saved conversation.".to_string(),
          });
      }

      #[test]
      fn a_task_list_showing_the_command_done_clears_still_running() {
          let mut state = state();
          tool(&mut state, "call-1", "bash", running("task-1"));
          assert_eq!(note(&state, "call-1").as_deref(), Some("still running"));
          state.update(TuiEvent::WorkflowTasksUpdated(vec![shell_task("task-1", "completed")]));
          assert_eq!(note(&state, "call-1"), None);
      }

      #[test]
      fn a_read_of_the_output_shows_the_exit_code() {
          let mut state = state();
          tool(&mut state, "call-1", "bash", running("task-1"));
          let rows = state.transcript.messages.len();
          tool(&mut state, "call-2", "task_read_output", ended("task-1", "failed", 2));
          assert_eq!(note(&state, "call-1").as_deref(), Some("exit 2"));
          assert_eq!(state.transcript.messages.len(), rows, "the read itself is not shown");
      }

      #[test]
      fn a_wait_settles_every_task_it_waited_on() {
          let mut state = state();
          tool(&mut state, "call-1", "bash", running("task-1"));
          tool(&mut state, "call-2", "bash", running("task-2"));
          tool(&mut state, "call-3", "task_wait", json!({
              "wait": "terminal",
              "return_reason": "terminal",
              "tasks": [
                  {"session_id": "shell-1", "task_id": "task-1", "status": "completed",
                   "termination": "exited", "exit_code": 0, "output": "", "deadline_reached": false},
                  {"session_id": "shell-2", "task_id": "task-2", "status": "stopped",
                   "termination": "timed_out", "exit_code": null, "output": "", "deadline_reached": true},
              ],
          }).to_string());
          assert_eq!(note(&state, "call-1"), None);
          assert_eq!(note(&state, "call-2").as_deref(), Some("timed out"));
      }

      #[test]
      fn rows_of_other_tasks_keep_their_state() {
          let mut state = state();
          tool(&mut state, "call-1", "bash", running("task-1"));
          state.update(TuiEvent::WorkflowTasksUpdated(vec![shell_task("task-9", "completed")]));
          assert_eq!(note(&state, "call-1").as_deref(), Some("still running"));
      }

      #[test]
      fn a_command_the_task_list_still_runs_keeps_still_running() {
          let mut state = state();
          tool(&mut state, "call-1", "bash", running("task-1"));
          state.update(TuiEvent::WorkflowTasksUpdated(vec![shell_task("task-1", "running")]));
          assert_eq!(note(&state, "call-1").as_deref(), Some("still running"));
      }

      #[test]
      fn the_command_record_wins_over_the_task_list() {
          let mut state = state();
          tool(&mut state, "call-1", "bash", running("task-1"));
          state.update(TuiEvent::WorkflowTasksUpdated(vec![shell_task("task-1", "failed")]));
          assert_eq!(note(&state, "call-1").as_deref(), Some("failed"));
          tool(&mut state, "call-2", "task_read_output", ended("task-1", "failed", 3));
          assert_eq!(note(&state, "call-1").as_deref(), Some("exit 3"));
          // Neither a later task list nor a stale read takes it back.
          state.update(TuiEvent::WorkflowTasksUpdated(vec![shell_task("task-1", "failed")]));
          tool(&mut state, "call-3", "task_read_output", running("task-1"));
          assert_eq!(note(&state, "call-1").as_deref(), Some("exit 3"));
      }

      #[test]
      fn an_end_seen_before_the_row_arrives_is_shown_on_it() {
          let mut state = state();
          state.update(TuiEvent::WorkflowTasksUpdated(vec![shell_task("task-1", "completed")]));
          tool(&mut state, "call-1", "bash", running("task-1"));
          assert_eq!(note(&state, "call-1"), None);
      }

      #[test]
      fn restored_rows_show_how_their_commands_ended() {
          let mut state = state();
          state.update(TuiEvent::WorkflowTasksUpdated(vec![shell_task("task-3", "failed")]));
          restore(&mut state, vec![
              restored("call-1", "bash", running("task-1")),
              restored("call-2", "task_read_output", ended("task-1", "completed", 0)),
              restored("call-3", "bash", running("task-2")),
              restored("call-4", "bash", running("task-3")),
          ]);
          assert_eq!(note(&state, "call-1"), None);
          assert_eq!(note(&state, "call-3").as_deref(), Some("state unknown"));
          assert_eq!(note(&state, "call-4").as_deref(), Some("failed"));
      }

      #[test]
      fn a_task_list_after_the_restore_replaces_state_unknown() {
          let mut state = state();
          restore(&mut state, vec![restored("call-1", "bash", running("task-1"))]);
          assert_eq!(note(&state, "call-1").as_deref(), Some("state unknown"));
          state.update(TuiEvent::WorkflowTasksUpdated(vec![shell_task("task-1", "completed")]));
          assert_eq!(note(&state, "call-1"), None);
      }

      /// The note of the latest bash row; `None` before there is one.
      #[cfg(unix)]
      fn latest_bash_note(state: &AppState) -> Option<Option<String>> {
          state.transcript.messages.iter().rev().find_map(|message| match message {
              ChatMessage::ToolCall { name, output: Some(output), .. } if name == "bash" => {
                  terminal_output_display(output).map(|display| display.note)
              }
              _ => None,
          })
      }

      #[cfg(unix)]
      #[test]
      fn a_bash_row_shows_how_its_command_ended_after_outliving_the_call() {
          use crate::test_support::hosted_tui::{Tui, config};

          let home = crate::test_support::isolate_orca_home();
          let mut config = config(home.path(), Vec::new());
          config.approval_mode = orca_core::approval_types::ApprovalMode::FullAuto;
          let mut tui = Tui::start(config);
          tui.send("bash_wait 200 :: sleep 2; echo done");
          tui.until("the call to return while its command runs", |state| {
              latest_bash_note(state) == Some(Some("still running".to_string()))
          });
          tui.until("the row to show its command ended", |state| {
              latest_bash_note(state) == Some(None)
          });
          tui.quit();
      }
  }
  ```

  在 `terminal_output.rs` 的测试模块里再加（Review Focus 1，以及两个解析函数）：

  ```rust
  #[test]
  fn writing_the_end_keeps_the_output_as_it_was() {
      let output = format!(
          "{}\x1b[31m红色\x1b[0m tab\there\r\nlast line without newline",
          "构建日志 build log 0123456789\n".repeat(40_000)
      );
      let running = payload(serde_json::json!({
          "state": "running", "return_reason": "yield_elapsed",
          "termination_reason": null, "exit_code": null, "output": output,
      }));
      let (_, end) = task_list_end(&serde_json::from_value(serde_json::json!({
          "id": "task-1", "type": "shell", "status": "completed",
          "description": "build", "createdAtMs": 1,
      })).unwrap()).unwrap();
      let written = with_end(&running, &end).unwrap();
      let before: serde_json::Value = serde_json::from_str(&running).unwrap();
      let after: serde_json::Value = serde_json::from_str(&written).unwrap();
      assert_eq!(after["output"], before["output"]);
      assert_eq!(terminal_output_display(&written).unwrap().note, None);
  }

  #[test]
  fn ends_are_read_from_a_result_and_from_each_waited_task() {
      let read = payload(serde_json::json!({ "exit_code": 7, "state": "failed" }));
      let ends = reported_ends(&read);
      assert_eq!(ends.len(), 1);
      assert_eq!(ends[0].0, "task-1");
      assert_eq!(ends[0].1.source, EndSource::Record);
      assert!(reported_ends(&payload(serde_json::json!({
          "state": "running", "termination_reason": null, "exit_code": null,
      })))
      .is_empty());
      let wait = serde_json::json!({ "tasks": [
          { "task_id": "a", "status": "running", "termination": "running", "exit_code": null },
          { "task_id": "b", "status": "failed", "termination": "interrupted", "exit_code": null },
          { "id": "agent-1", "type": "subagent", "status": "completed" },
      ]}).to_string();
      let ends = reported_ends(&wait);
      assert_eq!(ends.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["b"]);
      assert_eq!(
          terminal_output_display(&with_end(&read, &ends[0].1).unwrap()).unwrap().note.as_deref(),
          Some("interrupted")
      );
  }
  ```

- [ ] **Step 2：确认测试失败。**
  - `env -u NODE_OPTIONS cargo nextest run -p orca-tui --lib --locked --retries 0 -E 'test(/command_ends::tests|writing_the_end_keeps|ends_are_read_from/)'`：编译失败，因为方法和函数还不存在。
- [ ] **Step 3：实现 `terminal_output.rs` 的部分。** 文件顶部加 `use orca_core::task_types::{BackgroundTaskSummary, TaskStatus, TaskType};`，然后加：

  ```rust
  /// How a shell command that outlived its call is known to have ended,
  /// least to most: a restored row nothing reports on, a task list's
  /// status, the command's own record (exit code and why it ended).
  #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
  pub(crate) enum EndSource {
      Unknown,
      TaskList,
      Record,
  }

  /// What a shell result says once its command has ended: the fields
  /// `terminal_output_display` reads, and where they came from.
  #[derive(Clone, Debug, Eq, PartialEq)]
  pub(crate) struct CommandEnd {
      pub(crate) source: EndSource,
      state: String,
      return_reason: String,
      termination_reason: Option<String>,
      exit_code: Option<i64>,
  }

  /// `return_reason` of a result rewritten from a task list's status.
  const TASK_LIST_REASON: &str = "task_list";
  /// `return_reason` of a restored result nothing reports on any more.
  const RESTORED_REASON: &str = "restored_unknown";

  fn is_ended(state: &str) -> bool {
      matches!(state, "completed" | "failed" | "stopped" | "cancelled")
  }

  fn known_end(payload: &TerminalPayload) -> Option<EndSource> {
      if payload.return_reason == RESTORED_REASON {
          Some(EndSource::Unknown)
      } else if !is_ended(&payload.state) {
          None
      } else if payload.return_reason == TASK_LIST_REASON {
          Some(EndSource::TaskList)
      } else {
          Some(EndSource::Record)
      }
  }

  /// The task a shell result reports on, and how its end is known: `None`
  /// while the result still has the command running.
  pub(crate) fn reported_task(content: &str) -> Option<(String, Option<EndSource>)> {
      let payload: TerminalPayload = serde_json::from_str(content.trim()).ok()?;
      let known = known_end(&payload);
      Some((payload.task_id, known))
  }

  /// The ends a result reports: a `bash`, `task_send_input` or
  /// `task_read_output` result (all the same envelope), or each task a
  /// `task_wait` result lists. Commands still running are left out.
  pub(crate) fn reported_ends(content: &str) -> Vec<(String, CommandEnd)> {
      let Ok(value) = serde_json::from_str::<serde_json::Value>(content.trim()) else {
          return Vec::new();
      };
      if let Some(tasks) = value.get("tasks").and_then(serde_json::Value::as_array) {
          return tasks.iter().filter_map(waited_end).collect();
      }
      let Ok(payload) = serde_json::from_value::<TerminalPayload>(value) else {
          return Vec::new();
      };
      let Some(source) = known_end(&payload) else {
          return Vec::new();
      };
      vec![(
          payload.task_id,
          CommandEnd {
              source,
              state: payload.state,
              return_reason: payload.return_reason,
              termination_reason: payload.termination_reason,
              exit_code: payload.exit_code,
          },
      )]
  }

  /// One task of a `task_wait` result. A shell task appears as the terminal
  /// service records it; other tasks have no `termination` and are skipped.
  fn waited_end(task: &serde_json::Value) -> Option<(String, CommandEnd)> {
      #[derive(Deserialize)]
      struct WaitedShell {
          task_id: String,
          status: String,
          termination: String,
          #[serde(default)]
          exit_code: Option<i64>,
          #[serde(default)]
          deadline_reached: bool,
      }
      let task = WaitedShell::deserialize(task).ok()?;
      if !is_ended(&task.status) {
          return None;
      }
      let return_reason = if task.deadline_reached {
          "deadline_exceeded"
      } else {
          "terminal_observed"
      };
      Some((
          task.task_id,
          CommandEnd {
              source: EndSource::Record,
              state: task.status,
              return_reason: return_reason.to_string(),
              termination_reason: Some(task.termination),
              exit_code: task.exit_code,
          },
      ))
  }

  /// What a task list says about a shell task that has ended. It carries no
  /// exit code, so a failed command reads `failed` until its record says more.
  pub(crate) fn task_list_end(task: &BackgroundTaskSummary) -> Option<(String, CommandEnd)> {
      if task.task_type != TaskType::Shell {
          return None;
      }
      let (state, termination, exit_code) = match task.status {
          TaskStatus::Completed => ("completed", "exited", Some(0)),
          TaskStatus::Failed => ("failed", "failed", None),
          TaskStatus::Stopped => ("stopped", "stopped", None),
          TaskStatus::Cancelled => ("cancelled", "cancelled", None),
          _ => return None,
      };
      Some((
          task.id.clone(),
          CommandEnd {
              source: EndSource::TaskList,
              state: state.to_string(),
              return_reason: TASK_LIST_REASON.to_string(),
              termination_reason: Some(termination.to_string()),
              exit_code,
          },
      ))
  }

  /// A restored row's command that nothing reports on any more.
  pub(crate) fn unknown_end() -> CommandEnd {
      CommandEnd {
          source: EndSource::Unknown,
          state: "unknown".to_string(),
          return_reason: RESTORED_REASON.to_string(),
          termination_reason: Some("state unknown".to_string()),
          exit_code: None,
      }
  }

  /// `content`, a shell result, saying how its command ended. The output and
  /// every other field stay as they were.
  pub(crate) fn with_end(content: &str, end: &CommandEnd) -> Option<String> {
      let mut value: serde_json::Value = serde_json::from_str(content.trim()).ok()?;
      let fields = value.as_object_mut()?;
      fields.insert("state".to_string(), end.state.clone().into());
      fields.insert("return_reason".to_string(), end.return_reason.clone().into());
      fields.insert(
          "termination_reason".to_string(),
          end.termination_reason.clone().into(),
      );
      fields.insert("exit_code".to_string(), end.exit_code.into());
      serde_json::to_string(&value).ok()
  }
  ```

  `terminal_output_display` 不用改：改写后的 `state` 不再是 `running`，它会按 `exit_code` / `termination_reason` 算出 `exit N`、`failed`、`stopped`、`cancelled`、`interrupted` 或 `state unknown`。
- [ ] **Step 4：实现 `command_ends.rs`。**

  ```rust
  //! How shell commands that outlived their calls ended. A shell row keeps the
  //! result its call returned; when the command was still running then, what
  //! is learned later — a read of its output, a wait on it, the task list —
  //! is kept here by task id and written into every row of that task.

  use std::collections::{HashMap, HashSet};

  use crate::terminal_output::{
      CommandEnd, EndSource, reported_ends, reported_task, task_list_end, unknown_end, with_end,
  };
  use crate::transcript_state::ChatMessage;
  use crate::types::AppState;

  /// The most telling end known for each task.
  #[derive(Debug, Default)]
  pub(crate) struct CommandEnds {
      known: HashMap<String, CommandEnd>,
  }

  impl CommandEnds {
      /// Keeps `end` when it tells more than what is known; says whether it did.
      fn learn(&mut self, task_id: &str, end: &CommandEnd) -> bool {
          if self
              .known
              .get(task_id)
              .is_some_and(|known| known.source >= end.source)
          {
              return false;
          }
          self.known.insert(task_id.to_string(), end.clone());
          true
      }

      pub(crate) fn clear(&mut self) {
          self.known.clear();
      }
  }

  /// Tools whose results are shell results, or report on shell tasks.
  fn reports_on_commands(tool: &str) -> bool {
      matches!(tool, "bash" | "task_send_input" | "task_read_output" | "task_wait")
  }

  impl AppState {
      /// Learns what the result of `tool` says about how commands ended.
      pub(crate) fn learn_command_ends_from_result(&mut self, tool: &str, output: &str) {
          if !reports_on_commands(tool) {
              return;
          }
          for (task_id, end) in reported_ends(output) {
              self.learn_command_end(&task_id, &end);
          }
      }

      /// Learns which shell tasks the task list shows ended.
      pub(crate) fn learn_command_ends_from_tasks(&mut self) {
          let ends: Vec<_> = self.workflow_tasks().iter().filter_map(task_list_end).collect();
          for (task_id, end) in ends {
              self.learn_command_end(&task_id, &end);
          }
      }

      /// Shows the end already known for the task of the row at `index`: for
      /// a row that arrives after its command was seen to end.
      pub(crate) fn show_known_command_end(&mut self, index: usize) {
          let Some((task_id, row_knows)) = self.command_row_task(index) else {
              return;
          };
          let Some(end) = self.command_ends.known.get(&task_id).cloned() else {
              return;
          };
          if row_knows.is_none_or(|known| known < end.source) {
              self.write_command_end(index, &end);
          }
      }

      /// Settles a restored transcript: what later results in it say, then
      /// what the task list says. A row still running whose task the list does
      /// not have shows `state unknown`: its command ran in an earlier Orca,
      /// and nothing says how it ended.
      pub(crate) fn settle_restored_command_rows(&mut self) {
          self.command_ends.clear();
          let reported: Vec<_> = self
              .transcript
              .messages
              .iter()
              .filter_map(|message| match message {
                  ChatMessage::ToolCall {
                      name,
                      output: Some(output),
                      ..
                  } if reports_on_commands(name) => Some(reported_ends(output)),
                  _ => None,
              })
              .flatten()
              .collect();
          for (task_id, end) in reported {
              self.command_ends.learn(&task_id, &end);
          }
          let listed: Vec<_> = self.workflow_tasks().iter().filter_map(task_list_end).collect();
          for (task_id, end) in listed {
              self.command_ends.learn(&task_id, &end);
          }
          let in_task_list: HashSet<String> = self
              .workflow_tasks()
              .iter()
              .map(|task| task.id.clone())
              .collect();
          for index in 0..self.transcript.messages.len() {
              self.show_known_command_end(index);
              if let Some((task_id, None)) = self.command_row_task(index)
                  && !in_task_list.contains(&task_id)
              {
                  let end = unknown_end();
                  self.command_ends.learn(&task_id, &end);
                  self.write_command_end(index, &end);
              }
          }
      }

      /// The task the shell row at `index` reports on, and how its end is known.
      fn command_row_task(&self, index: usize) -> Option<(String, Option<EndSource>)> {
          match self.transcript.messages.get(index)? {
              ChatMessage::ToolCall {
                  name,
                  output: Some(output),
                  ..
              } if reports_on_commands(name) => reported_task(output),
              _ => None,
          }
      }

      fn learn_command_end(&mut self, task_id: &str, end: &CommandEnd) {
          if !self.command_ends.learn(task_id, end) {
              return;
          }
          let rows: Vec<usize> = (0..self.transcript.messages.len())
              .filter(|&index| {
                  self.command_row_task(index).is_some_and(|(id, row_knows)| {
                      id == task_id && row_knows.is_none_or(|known| known < end.source)
                  })
              })
              .collect();
          for index in rows {
              self.write_command_end(index, end);
          }
      }

      fn write_command_end(&mut self, index: usize, end: &CommandEnd) {
          self.mutate_message(index, |message| {
              if let ChatMessage::ToolCall {
                  output: Some(output),
                  ..
              } = message
                  && let Some(updated) = with_end(output, end)
              {
                  *output = updated;
              }
          });
      }
  }
  ```
- [ ] **Step 5：接上 reducer。**
  - `state_reducer.rs` 的 `TuiEvent::ToolCompleted`：
    - 把 `if is_panel_owned_tool_progress_name(&name) { return; }` 改成先学再返回：

      ```rust
      if is_panel_owned_tool_progress_name(&name) {
          // Not shown, but a read of a command's output, or a wait on it,
          // can say how a command that outlived its call ended.
          self.learn_command_ends_from_result(&name, &output);
          return;
      }
      ```
    - 在算出 `message_index` 之后、`if status == "completed"` 之前加：

      ```rust
      self.learn_command_ends_from_result(&name, &output);
      self.show_known_command_end(message_index);
      ```
  - `TuiEvent::HistoryLoaded`：在 `self.replace_messages(messages);` 之后加 `self.settle_restored_command_rows();`。
  - `TuiEvent::NewSessionStarted`：加 `self.command_ends.clear();`。
  - `workflow_panel.rs`：按 Files 里说的两处加 `self.learn_command_ends_from_tasks();`。
- [ ] **Step 6：确认测试通过。**
  - Step 2 的命令；
  - `env -u NODE_OPTIONS cargo nextest run -p orca-tui --lib --locked --retries 0 -E 'test(/terminal_output::/)'`，原有的 `terminal_output` 测试仍通过；
  - 端到端测试 `a_bash_row_shows_how_its_command_ended_after_outliving_the_call` 如果卡在第二个 `until`（20 秒内那一行没更新），说明空闲时的任务快照没有送到 TUI。不要改 runtime，先查 `apply_surface_projection_state` 在空闲时有没有收到这个 shell 任务的状态，把结论报告给控制会话。
- [ ] **Step 7：全量检查**（见 Global Constraints）。
- [ ] **Step 8：提交。** `fix(tui): show how a shell command ended after it outlived its call`

### Task 2：Rust 兼容更新

**Files:**
- Modify: `Cargo.lock`。
- Modify: 新弃用的 API 用在哪里，就改哪里。

**Interfaces:** 无。

- [ ] **Step 1：更新。** `cargo update`，记下 "Locking N packages" 的 N（2026-10-08 是 148）。
- [ ] **Step 2：确认剩下的只有后面任务要处理的大版本。** `cargo update --dry-run --verbose 2>&1 | grep Unchanged`。允许出现的只有：
  - 后面任务要升的：toml、toml_edit、reqwest、hickory-resolver、rusqlite、sha2、base64、dirs、shlex、pulldown-cmark、zstd、ratatui、crossterm、ratatui-image、unicode-width；
  - ACP 和它的 schema；
  - 被上游依赖固定住的传递依赖（例如 generic-array）。在提交说明里列出这类依赖。
- [ ] **Step 3：编译并修弃用。** `env -u NODE_OPTIONS cargo build --workspace --all-targets --locked`。新的弃用改用替代 API，要求 1.95 和 1.99 都接受。
- [ ] **Step 4：全量检查。** 如果 syntect / two-face 的更新让 golden frame 变了，按 Global Constraints 处理。
- [ ] **Step 5：提交。** `chore(deps): update compatible Rust dependencies`。正文写：更新了多少个包、几个值得注意的版本（tokio、clap、rustls、hyper 等）、修了哪些弃用。

### Task 3：toml 1.1 与 toml_edit 0.25

**Files:**
- Modify: `Cargo.toml`（workspace）：`toml = "1.1"`、`toml_edit = "0.25"`（按执行时最新的次版本写）。
- Modify: `crates/orca-core/Cargo.toml` 和 `crates/orca-runtime/Cargo.toml` 的 `[dev-dependencies]` 加 `toml_v08 = { package = "toml", version = "0.8" }`。
- Modify: 编译报错的地方。
- Test:
  - `crates/orca-core/src/config/folder_trust.rs`
  - `crates/orca-core/src/config/user_edit.rs`
  - `crates/orca-core/src/config/file.rs`
  - `crates/orca-runtime/src/onboarding.rs`

**Interfaces:** 无新接口。

- [ ] **Step 1：在升级之前写好对照测试，并确认它们在 toml 0.8 上通过。**
  - `folder_trust.rs`：
    - `a_trust_file_in_the_v0_5_7_format_still_loads`：先用当前代码的 `save_path` 写一份信任文件，条目包括 trusted 和 untrusted，路径里有空格和中文。把写出的原文贴进测试，作为常量 `TRUST_FILE_V0_5_7`。测试用 `load_path` 读这个常量写成的文件，断言每个路径的信任级别。
    - `a_saved_trust_file_reads_back_with_toml_0_8`：`save_path` 写出后，`toml_v08::from_str::<toml_v08::Table>(&text)` 能解析，`load_path` 读回的条目和写入的一样。
  - `onboarding.rs`：照上面的写法，给 acknowledgement store 写 `an_acknowledgement_file_in_the_v0_5_7_format_still_loads` 和 `a_saved_acknowledgement_file_reads_back_with_toml_0_8`。
  - `user_edit.rs`：`an_edited_config_reads_back_with_toml_0_8`。基础输入用子项目 1 那份带注释的多行内联数组配置，依次执行 `add_user_mcp_server_in` 和一次权限规则的添加，结果文件能被 `toml_v08` 解析，也能被当前的配置加载器读出这几个服务器和规则。
  - `file.rs`：`a_config_in_the_v0_5_7_format_still_parses`。从现有测试里挑覆盖面最大的配置样例，合成一份常量 `CONFIG_V0_5_7`，至少包含：
    - `model`、`reasoning_effort`；
    - 内联数组形式和 `[[mcp_servers]]` 表数组形式的服务器；
    - 权限规则、permission profile、hooks。

    断言解析出的关键字段。
  - 跑 `env -u NODE_OPTIONS cargo nextest run -p orca-core -p orca-runtime --lib --locked --retries 0 -E 'test(/v0_5_7_format|with_toml_0_8/)'`，全部通过。
- [ ] **Step 2：升级。** 改 workspace 的版本号，`env -u NODE_OPTIONS cargo build --workspace --all-targets` 更新锁文件。
- [ ] **Step 3：修编译错误。** 按 toml 0.9 / 1.x 和 toml_edit 0.23–0.25 的 CHANGELOG 改，不顺带改行为。已知的变化有：
  - `Serializer::new` 改收 `&mut Buffer`；
  - `impl FromStr for Value` 只解析单个值；
  - `InlineTable::preamble` 改名为 `trailing`；
  - `Time` 的秒和纳秒变成 `Option`。

  初查 Orca 没用到这些。
- [ ] **Step 4：确认通过。**
  - Step 1 的命令；
  - `env -u NODE_OPTIONS cargo nextest run -p orca-core --locked --profile ci --retries 0`（包括子项目 1 的注释往返测试）；
  - `env -u NODE_OPTIONS cargo nextest run --test mcp_cli_contract --locked --retries 0`；
  - `cargo tree -i toml@0.8 -e normal` 不应有输出（toml 0.8 只剩 dev 依赖）。如果有第三方依赖还要 0.8，在提交说明里写上。
- [ ] **Step 5：确认没有依赖文档顺序的反序列化。** `grep -rn 'IndexMap\|indexmap\|preserve_order' --include='*.rs' --include='Cargo.toml' crates src` 没有结果；再看一遍 `crates/orca-core/src/config/` 里反序列化成 map 的字段，确认用的是 `HashMap` 或 `BTreeMap`（顺序本来就不按文档）。结论写进提交说明。
- [ ] **Step 6：全量检查。**
- [ ] **Step 7：提交。** `chore(deps): move to toml 1.1 and toml_edit 0.25`

### 控制会话：升级前的真实请求基线（Task 4 之前）

- [ ] 在 Task 3 提交后的 HEAD 上 `env -u NODE_OPTIONS cargo build --bin orca`。
- [ ] **功能：** 按 Global Constraints 的规则，把 auth.json 复制到一个临时 `ORCA_HOME`，在一个临时工作目录里跑 `ORCA_HOME=<临时home> target/debug/orca exec 'Reply with OK.'`，确认得到正常回复。用完删掉复制的 auth.json。
- [ ] **耗时：** 在另一个空的临时 `ORCA_HOME` 里，跑 3 次 `ORCA_API_KEY=sk-orca-latency-probe target/debug/orca exec 'hi'`，用 `python3 -c` 包一层计时。确认每次输出里都是服务器返回的 401（说明请求确实发出去了，不是本地就失败）。记下 3 次的中位数。

### Task 4：reqwest 0.13、hickory 0.26、系统证书库 + ring

**Files:**
- Modify: `Cargo.toml`（workspace）：
  - `reqwest = { version = "0.13.5", default-features = false, features = ["blocking", "form", "hickory-dns", "json", "query", "rustls-no-provider", "stream"] }`；
  - `hickory-resolver = "0.26.3"`；
  - 新增 `rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }`。
- Modify: `crates/orca-mcp/Cargo.toml`：`[dependencies]` 加 `rustls = { workspace = true }`。
- Create: `crates/orca-mcp/src/http.rs`；在 `crates/orca-mcp/src/lib.rs` 的 `mod connection_stop;` 之后加 `pub mod http;`。
- Modify: 所有构建 reqwest 客户端的地方（2026-10-08 的清单，执行时再 grep 一次）：
  - `crates/orca-runtime/src/update_check.rs:274,299`；
  - `crates/orca-tools/src/web_search.rs:234`；
  - `crates/orca-mcp/src/oauth.rs:470,1288,1341`；
  - `crates/orca-mcp/src/oauth/test_server.rs:279`；
  - `crates/orca-mcp/src/transport.rs:1280,1690,2207,4267,4742`；
  - `crates/orca-mcp/src/legacy_sse.rs:113,585`；
  - `crates/orca-provider/src/http_client.rs:31,39`；
  - `crates/orca-provider/src/streaming.rs:540,711`。
- Modify: `crates/orca-runtime/src/network_proxy.rs`（hickory 0.26）。
- Create: `scripts/validate-http-client-boundary.mjs`、`scripts/test-validate-http-client-boundary.mjs`。
- Modify: `.github/workflows/runtime-contract.yml`：在 "Validate reviewed artifacts and current inventories" 这一步的 `node scripts/validate-execution-broker-boundary.mjs` 之后加两行：

  ```yaml
            node scripts/test-validate-http-client-boundary.mjs
            node scripts/validate-http-client-boundary.mjs
  ```
- Create: `crates/orca-mcp/tests/http_entry.rs`、`crates/orca-mcp/tests/http_proxy_env.rs`（每个文件是一个独立的测试进程）。
- Modify: `site/src/docs/md/zh/configuration.mdx`、`site/src/docs/md/en/configuration.mdx`。

**Interfaces:**
- **Produces:**
  - `orca_mcp::http::client_builder() -> reqwest::ClientBuilder`
  - `orca_mcp::http::blocking_client_builder() -> reqwest::blocking::ClientBuilder`
  - `orca_mcp::http::client() -> reqwest::Client`（原来 `reqwest::Client::new()` 的替代）
  - `orca_mcp::http::blocking_client() -> reqwest::blocking::Client`（原来 `reqwest::blocking::Client::new()` 的替代）
- **Consumes:** 无。

- [ ] **Step 1：在 reqwest 0.12 上先做统一入口（纯重构）。**
  - 写 `crates/orca-mcp/src/http.rs`：

    ```rust
    //! Every HTTP client Orca builds starts here.
    //!
    //! reqwest is built with `rustls-no-provider`: TLS goes through rustls,
    //! certificates are checked against the operating system's trust store
    //! (`rustls-platform-verifier`), and the cryptography comes from the
    //! provider the process installed. These builders install ring, once,
    //! before the first client.

    use std::sync::Once;

    /// A builder for an async client.
    pub fn client_builder() -> reqwest::ClientBuilder {
        install_crypto_provider();
        reqwest::Client::builder()
    }

    /// A builder for a blocking client.
    pub fn blocking_client_builder() -> reqwest::blocking::ClientBuilder {
        install_crypto_provider();
        reqwest::blocking::Client::builder()
    }

    /// An async client with reqwest's defaults, as `reqwest::Client::new()`.
    pub fn client() -> reqwest::Client {
        client_builder().build().expect("failed to build HTTP client")
    }

    /// A blocking client with reqwest's defaults, as
    /// `reqwest::blocking::Client::new()`.
    pub fn blocking_client() -> reqwest::blocking::Client {
        blocking_client_builder()
            .build()
            .expect("failed to build blocking HTTP client")
    }

    fn install_crypto_provider() {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            // Another library may have installed a provider first; reqwest
            // then uses that one, which is as good.
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
    }
    ```
  - 加 `rustls` 依赖（workspace 和 orca-mcp）。
  - 把 Files 里列的每一处改成走统一入口：
    - `reqwest::Client::new()` 换成 `orca_mcp::http::client()`；
    - `reqwest::blocking::Client::new()` 换成 `orca_mcp::http::blocking_client()`；
    - `Client::builder()`、`BlockingClient::builder()`、`reqwest::Client::builder()` 换成对应的 `*_builder()`，后面的链式调用不变；
    - orca-mcp 自己的代码里写 `crate::http::…`。
  - 写两个集成测试文件：

    `crates/orca-mcp/tests/http_entry.rs`：

    ```rust
    //! The HTTP client entry in a process of its own: nothing else has
    //! installed a rustls crypto provider here, so building a client works
    //! only because the entry installs one.

    #[test]
    fn a_fresh_process_builds_clients_through_the_entry() {
        orca_mcp::http::client_builder().build().expect("async client");
        orca_mcp::http::blocking_client_builder()
            .build()
            .expect("blocking client");
        let _ = orca_mcp::http::client();
        let _ = orca_mcp::http::blocking_client();
    }
    ```

    `crates/orca-mcp/tests/http_proxy_env.rs`：

    ```rust
    //! The proxy the environment names is used, and `NO_PROXY` hosts are
    //! reached directly. A process of its own, since it sets proxy variables.

    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    /// Answers one request with `body` and reports its request line.
    fn serve_once(body: &'static str) -> (u16, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("address").port();
        let (seen, requests) = mpsc::channel();
        thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            stream.set_nonblocking(false).expect("blocking stream");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut request_line = String::new();
            reader.read_line(&mut request_line).expect("request line");
            let mut header = String::new();
            while reader.read_line(&mut header).is_ok_and(|read| read > 2) {
                header.clear();
            }
            let _ = seen.send(request_line.trim_end().to_string());
            let mut stream = stream;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        });
        (port, requests)
    }

    #[test]
    fn the_environment_proxy_is_used_and_no_proxy_hosts_are_reached_directly() {
        let (proxy_port, proxied) = serve_once("via proxy");
        let (direct_port, direct) = serve_once("direct");
        // SAFETY: this test binary runs this one test, before anything in it
        // reads the environment.
        unsafe {
            for name in ["http_proxy", "https_proxy", "all_proxy", "ALL_PROXY", "no_proxy"] {
                std::env::remove_var(name);
            }
            std::env::set_var("HTTP_PROXY", format!("http://127.0.0.1:{proxy_port}"));
            std::env::set_var("NO_PROXY", "127.0.0.1");
        }
        let client = orca_mcp::http::blocking_client();

        let body = client
            .get("http://orca-proxy-probe.invalid/path")
            .send()
            .expect("proxied request")
            .text()
            .expect("body");
        assert_eq!(body, "via proxy");
        assert_eq!(
            proxied.recv().expect("the proxy saw a request"),
            "GET http://orca-proxy-probe.invalid/path HTTP/1.1"
        );

        let body = client
            .get(format!("http://127.0.0.1:{direct_port}/"))
            .send()
            .expect("direct request")
            .text()
            .expect("body");
        assert_eq!(body, "direct");
        assert_eq!(direct.recv().expect("the server saw a request"), "GET / HTTP/1.1");
    }
    ```
  - 写校验脚本 `scripts/validate-http-client-boundary.mjs`：

    ```js
    #!/usr/bin/env node

    import { execFileSync } from "node:child_process";
    import { readFileSync } from "node:fs";
    import path from "node:path";
    import { fileURLToPath } from "node:url";

    const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
    // reqwest 0.13 panics on a client built before a crypto provider is
    // installed; the entry installs one.
    const entry = "crates/orca-mcp/src/http.rs";

    function fail(message) {
      throw new Error(`http client boundary: ${message}`);
    }

    function trackedRustSources() {
      return execFileSync("git", ["ls-files", "crates", "-z"], { cwd: repoRoot })
        .toString()
        .split("\0")
        .filter((file) => file.endsWith(".rs"));
    }

    // The names a file gives reqwest's client types: `use reqwest::Client;`,
    // `use reqwest::blocking::{Client as BlockingClient, …};`, …
    function importedClientNames(source) {
      const names = [];
      for (const [, body] of source.matchAll(/\buse\s+reqwest::([^;]+);/g)) {
        for (const [, kind, alias] of body.matchAll(/\b(Client|ClientBuilder)\b(?:\s+as\s+(\w+))?/g)) {
          names.push(alias ?? kind);
        }
      }
      return names;
    }

    export function validateHttpClientBoundary({ sourceOverrides = new Map() } = {}) {
      const sources = new Set([...trackedRustSources(), ...sourceOverrides.keys()]);
      for (const relativePath of [...sources].sort()) {
        if (relativePath === entry) {
          continue;
        }
        const source = sourceOverrides.has(relativePath)
          ? sourceOverrides.get(relativePath)
          : readFileSync(path.join(repoRoot, relativePath), "utf8");
        if (!source.includes("reqwest")) {
          continue;
        }
        const qualified = /\breqwest::(?:blocking::)?(?:Client|ClientBuilder)::(?:new|builder)\s*\(/.test(source);
        const imported = importedClientNames(source).some((name) =>
          new RegExp(`\\b${name}::(?:new|builder)\\s*\\(`).test(source),
        );
        if (qualified || imported) {
          fail(`direct reqwest client in ${relativePath}; build it with orca_mcp::http`);
        }
      }
      return true;
    }

    if (import.meta.url === `file://${process.argv[1]}`) {
      validateHttpClientBoundary();
      console.log("http client boundary passed");
    }
    ```

    和它的测试 `scripts/test-validate-http-client-boundary.mjs`：

    ```js
    #!/usr/bin/env node

    import assert from "node:assert/strict";
    import test from "node:test";

    import { validateHttpClientBoundary } from "./validate-http-client-boundary.mjs";

    test("a qualified reqwest client outside the entry is rejected", () => {
      assert.throws(
        () =>
          validateHttpClientBoundary({
            sourceOverrides: new Map([
              ["crates/orca-runtime/src/forged.rs", "fn f() { let _ = reqwest::blocking::Client::new(); }"],
            ]),
          }),
        /direct reqwest client/,
      );
    });

    test("an imported or aliased reqwest client outside the entry is rejected", () => {
      assert.throws(
        () =>
          validateHttpClientBoundary({
            sourceOverrides: new Map([
              [
                "crates/orca-provider/src/forged.rs",
                "use reqwest::blocking::{Client as BlockingClient, Response};\nfn f() { BlockingClient::builder(); }",
              ],
            ]),
          }),
        /direct reqwest client/,
      );
    });

    test("the entry, and files that only name the client type, are allowed", () => {
      assert.doesNotThrow(() =>
        validateHttpClientBoundary({
          sourceOverrides: new Map([
            ["crates/orca-mcp/src/http.rs", "reqwest::Client::builder()"],
            ["crates/orca-provider/src/typed.rs", "use reqwest::Client;\nfn f(client: &Client) {}"],
          ]),
        }),
      );
    });

    validateHttpClientBoundary();
    console.log("http client boundary validator tests passed");
    ```
  - 在 `runtime-contract.yml` 里接上两行（见 Files）。
- [ ] **Step 2：确认 0.12 上全部通过。**
  - `env -u NODE_OPTIONS cargo nextest run -p orca-mcp --test http_entry --test http_proxy_env --locked --retries 0`；
  - `env -u NODE_OPTIONS node scripts/test-validate-http-client-boundary.mjs`；
  - `env -u NODE_OPTIONS node scripts/validate-http-client-boundary.mjs`；
  - `env -u NODE_OPTIONS cargo nextest run -p orca-mcp -p orca-provider -p orca-tools --locked --profile ci --retries 0`。
- [ ] **Step 3：升级。** 按 Files 改 workspace 的 reqwest、hickory-resolver，`env -u NODE_OPTIONS cargo build --workspace --all-targets` 更新锁文件。
- [ ] **Step 4：修编译错误。**
  - reqwest 0.13 改了名、但保留旧名的 TLS 方法（例如 `use_rustls_tls`）只是软弃用，Orca 没用到。
  - `network_proxy.rs`：按 hickory 0.26 的 API 改解析器的构建（现在是 `TokioResolver::builder_tokio()` 加 `builder.build()`）和 `lookup_ip`。
    - 初始化失败仍走现有的 `failed to initialize DNS resolver: {error}` 错误路径；
    - 查询的超时（`DNS_LOOKUP_TIMEOUT`）和错误处理不变。
  - 其余按编译器提示和 reqwest、hickory 的 CHANGELOG 改，不顺带改行为。
- [ ] **Step 5：确认依赖树。**
  - `cargo tree -i aws-lc-rs` 和 `cargo tree -i aws-lc-sys` 都应报"没有这个包"（不能引入 aws-lc）；
  - `cargo tree -i hickory-resolver` 只有 0.26；
  - `cargo tree -i reqwest` 只有 0.13。
- [ ] **Step 6：文档。** 在中英文 `configuration.mdx` 的"环境变量覆盖 / Environment variable overrides"一节之后加一节。

  中文：

  ```markdown
  ## 网络与证书

  Orca 用操作系统的证书库验证 HTTPS 证书：macOS 用钥匙串，Windows 用系统证书存储，Linux 用系统的 CA 证书包。

  - 公司的代理或网关用自己的 CA 时，把这张 CA 装进系统证书库就行，Orca 不需要额外设置。
  - 精简版的 Linux（比如最小化的容器镜像）要先安装 `ca-certificates`，否则 HTTPS 请求会因为找不到根证书而失败。
  - `HTTPS_PROXY`、`HTTP_PROXY` 和 `NO_PROXY` 环境变量照常生效。
  ```

  英文：

  ```markdown
  ## Network and certificates

  Orca checks HTTPS certificates against the operating system's trust store: the keychain on macOS, the system certificate store on Windows, and the system CA bundle on Linux.

  - If your company's proxy or gateway uses its own CA, install that CA in the system trust store. Orca needs no setting of its own.
  - On a minimal Linux system, such as a slim container image, install `ca-certificates` first. Otherwise HTTPS requests fail because no root certificates are found.
  - The `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY` environment variables work as before.
  ```

  标题层级和所在位置按两份文件现有的结构调整，两边保持一致。
- [ ] **Step 7：确认通过。**
  - Step 2 的四条命令；
  - `env -u NODE_OPTIONS cargo nextest run --test public_docs_contract --test provider_contract --test mcp_cli_contract --locked --retries 0`；
  - `env -u NODE_OPTIONS npm --prefix site run build`；
  - `env -u NODE_OPTIONS npm --prefix site run check:seo`。
- [ ] **Step 8：全量检查**（从这一步起包括新的校验脚本）。
- [ ] **Step 9：提交。** `chore(deps): move to reqwest 0.13 with the system trust store and ring`。正文写明三个行为变化：
  - 证书改由系统证书库验证；
  - 加密实现仍是 ring；
  - hickory 解析优先 IPv6。

### 控制会话：升级后的真实请求对比（Task 4 之后）

- [ ] 在 Task 4 的提交上重新 `env -u NODE_OPTIONS cargo build --bin orca`，重复"升级前的真实请求基线"的两步。
- [ ] **功能：** 正常回复。
- [ ] **耗时：** 3 次中位数和升级前比较。多出 300 ms 以上就先查原因（比如 IPv6 优先），向用户报告，等用户决定怎么处理之后再继续 Task 5。

### Task 5：rusqlite 0.40

**Files:**
- Modify: `Cargo.toml`（workspace）：`rusqlite = { version = "0.40.2", features = ["bundled", "fallible_uint"] }`。
- Create: `crates/orca-runtime/tests/fixtures/sqlite-v0.5.7/` 下的四个库文件：
  - `sessions-index.sqlite3`；
  - `goals.sqlite3`；
  - `memory-index.sqlite3`；
  - `task-output.sqlite3`。
- Modify: 编译报错的地方。
- Test: 以下四个文件各自的测试模块：
  - `crates/orca-runtime/src/thread_store/session_index.rs`
  - `crates/orca-runtime/src/goal_store.rs`
  - `crates/orca-runtime/src/memory/index.rs`
  - `crates/orca-runtime/src/task_output/persistence.rs`

**Interfaces:** 无新接口。

- [ ] **Step 1：检查多条语句的 SQL。** 运行：

  ```bash
  python3 - <<'PY'
  import re
  files = [
      "crates/orca-runtime/src/memory.rs",
      "crates/orca-runtime/src/memory/index.rs",
      "crates/orca-runtime/src/goal_store.rs",
      "crates/orca-runtime/src/task_output/persistence.rs",
      "crates/orca-runtime/src/thread_store/session_index.rs",
  ]
  call = re.compile(r'\.(execute|prepare|prepare_cached|query_row)\(\s*(r#?"|")(.*?)("#|")', re.S)
  for path in files:
      text = open(path).read()
      for match in call.finditer(text):
          statements = [s for s in match.group(3).split(";") if s.strip()]
          if len(statements) > 1:
              line = text[: match.start()].count("\n") + 1
              print(f"{path}:{line}: {match.group(1)} with {len(statements)} statements")
  PY
  ```

  2026-10-08 的结果是没有。另外人工看一遍把 SQL 先放进变量、再传给 `execute` / `prepare` 的地方，例如 `session_index.rs:60`。有多条语句的，改用 `execute_batch`。
- [ ] **Step 2：在升级之前生成库文件夹具。** 在每个 store 的测试模块里临时加一个测试，用这个 store 正常的创建和写入函数写出一个库，复制到 `crates/orca-runtime/tests/fixtures/sqlite-v0.5.7/`。每个库的内容：
  - **session index：** 两个会话，标题之一是中文。
  - **goals：** 一个 goal，带一次状态变化。
  - **memory index：** 两条记忆。
  - **task output：** 一个 shell 任务的输出，分成至少两块，覆盖 usize 的偏移量。

  跑一次生成后，**删掉这些临时测试**，只把生成的库文件加进提交。
- [ ] **Step 3：写读取测试，并确认在 0.32 上通过。** 每个 store 一个测试，命名为 `a_v0_5_7_<store>_database_still_reads_and_writes`（例如 `a_v0_5_7_session_index_database_still_reads_and_writes`）：
  - 把夹具复制到临时目录；
  - 用 store 正常的打开函数打开；
  - 断言 Step 2 写入的内容都能读出；
  - 再写一条新记录，关闭后重新打开，新旧记录都在。

  夹具路径写成 `concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sqlite-v0.5.7/…")`。运行 `env -u NODE_OPTIONS cargo nextest run -p orca-runtime --lib --locked --retries 0 -E 'test(/database_still_reads_and_writes/)'`。
- [ ] **Step 4：升级并修编译错误。** 改 workspace 版本，构建。`fallible_uint` 让 u64/usize 的 `ToSql` / `FromSql` 和以前一样。其余按 rusqlite 0.33–0.40 的发布说明改，Orca 没用到 VTab。
- [ ] **Step 5：确认通过。**
  - Step 3 的命令；
  - `env -u NODE_OPTIONS cargo nextest run -p orca-runtime --locked --profile ci --retries 0`（其中 `runtime_host`、`runtime_surface_domain` 也直接用 rusqlite）。
    - 各 store 现有的测试会在临时目录里新建库，覆盖建表路径；
    - Step 3 的测试覆盖打开旧库的路径。
- [ ] **Step 6：全量检查。**
- [ ] **Step 7：提交。** `chore(deps): move to rusqlite 0.40 (SQLite 3.53)`

### Task 6：sha2 0.11 与其余小版本

**Files:**
- Modify: `Cargo.toml`（workspace）：
  - `sha2 = "0.11"`、`base64 = "0.23"`、`dirs = "7"`、`shlex = "2"`；
  - `pulldown-cmark = { version = "0.13", default-features = false }`；
  - `zstd = "0.14"`。
- Create: `crates/orca-core/src/hex.rs`；在 `crates/orca-core/src/lib.rs` 的 `pub mod home;` 之前加 `pub mod hex;`。
- Modify: 11 处把摘要格式化成十六进制的地方：
  - orca-runtime：
    - `image_routing.rs:303`（`analysis_key`）
    - `mentions.rs:586`（`stable_id`）
    - `thread_store/assets.rs:82,182`（`externalize` 和校验）
    - `workflow/runner.rs:4425`（`digest_value`）
    - `workflow/state.rs:808`（`input_hash`）
    - `thread_store/local.rs:647`（`storage_identity_for_path`）
    - `thread_store/writer.rs:509`（`source_fingerprint`）
    - `workflow/script.rs:1105`（`sha256_hex`）
  - orca-provider：
    - `prompt_cache.rs:144`（`hash_json`）
    - `summary_cache.rs:36`（`summary_key`）
- Test：各函数所在文件的测试模块；`crates/orca-core/src/hex.rs`；`crates/orca-tui/src/ui.rs`（markdown）。

**Interfaces:**
- **Produces:** `orca_core::hex::lower(bytes: &[u8]) -> String`：小写十六进制，每个字节两位。
- **Consumes:** 无。

- [ ] **Step 1：在升级之前写好固定值测试，并确认通过。** 期望值用当前代码先算出来，再贴进测试（这是对照测试，用来保证升级前后算出的值完全一样）。
  - `workflow/script.rs`：`sha256_hex_is_the_lowercase_digest`：`sha256_hex(b"abc")` 等于 `"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"`。
  - `thread_store/assets.rs`：`an_externalized_asset_records_the_sha256_of_its_bytes`：外置一个内容为 `abc` 的资产，记录里的 `sha256` 等于上面那个值；把它读回来，校验通过。
  - `thread_store/local.rs`：`the_storage_identity_of_a_path_is_stable`：`storage_identity_for_path(Path::new("/orca/pin/workspace"))` 等于当前代码算出的值。
  - `summary_cache.rs`：`summary_keys_are_stable`：用一组固定参数调用 `summary_key`，等于当前值。
  - `prompt_cache.rs`：`json_hashes_are_stable`：`hash_json(b"orca-pin", &serde_json::json!({ "a": 1 }))` 等于当前值。
  - `ui.rs`：`markdown_with_carets_and_tildes_renders_as_before`：`render_markdown("x^2^ and H~2~O, ~~gone~~, [[Wiki]]", 80, &theme)` 每一行的文字，等于当前代码的输出。
  - 运行 `env -u NODE_OPTIONS cargo nextest run -p orca-runtime -p orca-provider -p orca-tui --lib --locked --retries 0 -E 'test(/lowercase_digest|sha256_of_its_bytes|storage_identity_of_a_path|summary_keys_are_stable|json_hashes_are_stable|carets_and_tildes/)'`，全部通过。
- [ ] **Step 2：加 `hex` 模块并替换 11 处。**

  ```rust
  //! Lower-case hexadecimal, the way digests are written in stored records
  //! and keys.

  /// `bytes` as lower-case hexadecimal, two digits a byte.
  pub fn lower(bytes: &[u8]) -> String {
      const DIGITS: &[u8; 16] = b"0123456789abcdef";
      let mut text = String::with_capacity(bytes.len() * 2);
      for byte in bytes {
          text.push(char::from(DIGITS[usize::from(byte >> 4)]));
          text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
      }
      text
  }

  #[cfg(test)]
  mod tests {
      #[test]
      fn bytes_become_two_lower_case_digits_each() {
          assert_eq!(super::lower(&[0x00, 0x0f, 0xab, 0xff]), "000fabff");
          assert_eq!(super::lower(&[]), "");
      }
  }
  ```

  替换写法：
  - `format!("{:x}", hasher.finalize())` → `orca_core::hex::lower(&hasher.finalize())`；
  - 带前缀的 `format!("{kind}:{:x}", hasher.finalize())` → `format!("{kind}:{}", orca_core::hex::lower(&hasher.finalize()))`；
  - `storage-{:x}` 同理；
  - `assets.rs:82` 的比较改成 `orca_core::hex::lower(&Sha256::digest(&bytes)) != asset.sha256`。

  这一步还在 sha2 0.10 上，Step 1 的测试应仍然通过。
- [ ] **Step 3：升级。** 改 workspace 版本号，构建。
- [ ] **Step 4：修编译错误。** 2026-10-08 用 sha2 0.11 实测过：
  - 摘要转 `[u8; 32]` 的 `.into()`、解引用成切片、`extend_from_slice` 都不用改；
  - 只有 `{:x}` 不再能用（Step 2 已经换掉）。

  其余几个：
  - pulldown-cmark 0.13 的新标签：Orca 匹配 `Tag` / `TagEnd` 的地方都有 `_ =>` 分支，不用补，确认能编译即可；也不打开新的语法选项；
  - base64、dirs、shlex、zstd：Orca 用到的 API 都没变。如果还有报错，按各自的 CHANGELOG 改，不改行为。
- [ ] **Step 5：确认通过。**
  - Step 1 的命令；
  - `env -u NODE_OPTIONS cargo nextest run -p orca-core --lib --locked --retries 0 -E 'test(/hex::/)'`；
  - `cargo tree -i sha2@0.10 -e normal` 如果还有第三方依赖用 0.10，在提交说明里写上。
- [ ] **Step 6：全量检查。**
- [ ] **Step 7：提交。** `chore(deps): move to sha2 0.11, base64 0.23, dirs 7, shlex 2, pulldown-cmark 0.13 and zstd 0.14`

### 控制会话：第一次推 CI 分支（Task 6 之后）

- [ ] `git push origin HEAD:refs/heads/ci/dependency-upgrades`。
- [ ] `gh workflow run runtime-contract.yml --ref ci/dependency-upgrades` 和 `gh workflow run windows-ci.yml --ref ci/dependency-upgrades`。用 Monitor 盯着，watch 脚本放进文件里用 `/bin/bash` 跑。
- [ ] 全部通过才继续 Task 7。
  - 如果只在 Windows 上失败：在本地 main 上加修复提交（派一个修复任务），再推同一个分支、重新跑。
  - 如果失败和这次的依赖升级无关：报告用户。

### Task 7：TUI 栈（一个提交）

**Files:**
- Modify: `Cargo.toml`（workspace）：
  - `crossterm = "0.29"`；
  - `ratatui = "0.30.2"`；
  - `ratatui-image = { version = "=11.1.0", default-features = false, features = ["crossterm"] }`（12 还是 RC，不用）；
  - 删掉 `tui-textarea`，加 `ratatui-textarea = "0.9.3"`；
  - `unicode-width = "0.2.2"`。
- Modify: `crates/orca-tui/Cargo.toml`：
  - ratatui 的 features 改为 `["crossterm_0_29", "scrolling-regions", "unstable-rendered-line-info"]`；
  - `tui-textarea = { workspace = true }` 换成 `ratatui-textarea = { workspace = true }`。
- Modify: orca-tui 里用到 `tui_textarea` 的 29 个文件，执行时用 `grep -rl tui_textarea crates/orca-tui/src` 重新列一次。
- Modify:
  - `crates/orca-tui/src/capability_backend.rs`（`CapabilityBackend` 和测试里的两个 backend）；
  - `crates/orca-tui/src/ui.rs`（测试里的 `RecordingBackend`；`Alignment`）；
  - `crates/orca-tui/src/transcript_view.rs`（`Alignment`）；
  - `crates/orca-tui/src/renderer_frame.rs`、`crates/orca-tui/src/presentation.rs`、`crates/orca-tui/src/renderer_loop.rs`（泛型 `B: Backend`）；
  - `crates/orca-tui/src/image_preview.rs`（ratatui-image 11）。
- Test：
  - `crates/orca-tui/src/composer_input_actions.rs`（Review Focus 4）；
  - `crates/orca-tui/src/ui.rs`（Review Focus 5，以及已有的 golden frame）。

**Interfaces:** 无新接口。

- [ ] **Step 1：在升级之前写两个测试。**
  - `composer_input_actions.rs` 的测试模块（用已有的 `editor_fixture`、`handle_composer_editor_shortcut`、`textarea_text`）：

    ```rust
    #[test]
    fn word_deletion_keeps_an_identifier_with_underscores_together() {
        for (text, left) in [("cargo test foo_bar", "cargo test "), ("提交 foo_bar", "提交 ")] {
            let (mut state, config, theme, mut vim, mut textarea) =
                editor_fixture(text, text.len(), false);
            let ctrl_w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);
            assert!(handle_composer_editor_shortcut(
                &Event::Key(ctrl_w),
                &ctrl_w,
                &mut state,
                &config,
                &mut textarea,
                &mut vim,
                &theme,
            ));
            assert_eq!(textarea_text(&textarea), left, "{text:?}");
        }
    }
    ```
  - `ui.rs` 的测试模块（golden 测试旁边）：

    ```rust
    #[test]
    fn messages_with_emoji_sequences_and_cjk_stay_within_the_width() {
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let text = "构建完成 👩‍💻 🇨🇳 ™ ½ — 修改了 `foo_bar.rs`，共 12 处：👍🏽 ✔️ ⚠️ ".repeat(3);
        for width in [20, 41, 80] {
            for message in [ChatMessage::Assistant(text.clone()), ChatMessage::User(text.clone())] {
                for line in build_lines_for_messages(std::slice::from_ref(&message), &theme, width, 0, false) {
                    assert!(line.width() <= width, "width {width}: {line:?}");
                }
            }
        }
    }
    ```

    `build_lines_for_messages`、`ChatMessage`、`Theme` 按测试模块里已有的路径引用。
  - 运行 `env -u NODE_OPTIONS cargo nextest run -p orca-tui --lib --locked --retries 0 -E 'test(/keeps_an_identifier_with_underscores|emoji_sequences_and_cjk/)'`：
    - 第一个测试应该失败：tui-textarea 0.7 把下划线当分隔符，只删掉 `bar`。它钉住的是升级后的新行为。
    - 第二个应该通过：它是守卫，升级后也要通过。
- [ ] **Step 2：改依赖并换 import。**
  - 按 Files 改两个 `Cargo.toml`；
  - `grep -rl tui_textarea crates/orca-tui/src | xargs sed -i '' 's/tui_textarea/ratatui_textarea/g'`；
  - `env -u NODE_OPTIONS cargo build -p orca-tui --all-targets` 更新锁文件。
- [ ] **Step 3：修编译错误。** 对照 ratatui 的 `BREAKING-CHANGES.md` 里 v0.30.0 一节：
  - **`Backend` trait：** 新要求关联的 `type Error: core::error::Error`，所有方法返回 `Result<_, Self::Error>`。
    - `CapabilityBackend<B>` 写 `type Error = B::Error;`，各方法的返回类型从 `io::Result<…>` 改成 `Result<…, Self::Error>`，转发逻辑不变。
    - 已弃用的 `get_cursor` / `set_cursor` 在 0.30.2 的 trait 里还在，保留现有的 `#[allow(deprecated)]` 转发和对应测试。
    - 测试里的 `RecordingBackend`、`FailingBackend` 和 `ui.rs` 里的 `RecordingBackend` 写 `type Error = io::Error;`。
  - **`TestBackend`：** 错误类型变成 `Infallible`。泛型 `B: Backend`、返回 `io::Result` 的函数（`renderer_frame.rs`、`presentation.rs`、`renderer_loop.rs`）加约束 `B::Error: std::error::Error + Send + Sync + 'static`，在 `?` 之前写 `.map_err(io::Error::other)`。不要为了编译而去掉测试对这些函数的调用。
  - **改名：** `Alignment` 改成 `HorizontalAlignment`。0.30 里 `Alignment` 只是没弃用的别名，改名是为了跟上游的叫法一致，共 7 处：
    - `ui.rs` 的 import、2264、4801、5093、16784 行附近；
    - `transcript_view.rs` 的 10、48、52、336、1756 行附近。
  - **图片预览**（`image_preview.rs`）：按 ratatui-image 10/11 的 CHANGELOG 改。
    - `FontSize` 从 `(u16, u16)` 变成 `FontSize { width, height }`；
    - `Resize::render_area` 改名为 `size_for`；
    - `area()` 改成 `size()`。

    现有的协议选择、`Resize::Fit(Some(FilterType::Lanczos3))` 的效果和缓存方式都不变。
  - **输入框：** ratatui-textarea 0.9.3 仍有 `set_placeholder_text`、`set_placeholder_style`，而且没弃用，不用改。不调用 `set_wrap_mode`（默认就不折行，保持现状）。
  - **其余：** 按编译器提示改，不顺带改行为。
- [ ] **Step 4：确认依赖树。**
  - `cargo tree -i crossterm` 只有 0.29；
  - `cargo tree -i unicode-width` 只有 0.2.2；
  - `cargo tree -i ratatui` 只有 0.30；
  - `cargo tree -i tui-textarea` 报"没有这个包"。
- [ ] **Step 5：确认测试。**
  - Step 1 的命令：两个测试都通过。
  - `env -u NODE_OPTIONS cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0`，其中包括：
    - 4 个 golden（`golden_welcome`、`golden_transcript_with_tools_thinking_and_notices`、`golden_approval_dialog`、`golden_help_panel`）；
    - 鲸鱼图宽度守卫 `welcome_whale_art_rows_stay_within_the_art_column_width`；
    - 主题和颜色相关的测试。
  - golden 有变化时：
    - 先看 `crates/orca-tui/src/golden/<name>.txt` 和实际输出的差异，查清是哪个改动造成的（unicode-width、布局，还是 ratatui 的渲染）；
    - 写进提交说明，再用 `ORCA_UPDATE_GOLDEN=1 env -u NODE_OPTIONS cargo test -p orca-tui --lib golden_ -- --test-threads=1` 重新生成；
    - 报告里列出每个变化的 golden 和原因。
  - `env -u NODE_OPTIONS cargo nextest run --test tui_pty_contract --locked --profile ci-serial --retries 0`。
- [ ] **Step 6：全量检查。** 两个校验器里 TUI 同名调用点的基线有变化时，按 Global Constraints 处理。
- [ ] **Step 7：提交。** `chore(deps): move the TUI to ratatui 0.30, crossterm 0.29 and ratatui-textarea`。正文写行为变化：
  - 按单词移动或删除时，下划线算单词的一部分；
  - 修正宽字符的光标滚动位置；
  - 修正撤销 / 重做后光标越界导致的 panic；
  - unicode-width 0.2.2；
  - ratatui-image 在终端不回应能力查询时用 ioctl 读字体尺寸。

### 控制会话：Ghostty 实测，第二次推 CI 分支（Task 7 之后）

- [ ] **准备 harness。** 把 `~/.claude/orca-ghostty-harness/` 复制到 `$CLAUDE_JOB_DIR/tmp/gh/`，在里面 `python3 -m venv venv && venv/bin/pip install pyte`，再跑一次 `venv/bin/python gh.py calibrate`。
  - 用 Task 7 提交后构建的 `target/debug/orca`；
  - 每个场景用一个临时 `ORCA_HOME` 和一个临时工作目录；
  - 用 mock provider：`gh.py start NAME --home H --cwd W -- --provider mock --mode full-auto`。执行前用 `target/debug/orca --help` 核对参数名。
- [ ] **逐项实测**，每项用 `gh.py screen` 或 `gh.py grid` 看结果：
  1. **多行输入：** 输入 `line one`，按 Shift+Enter，再输入 `line two`。输入框显示两行，按 Enter 后作为一条消息发出。
  2. **粘贴：** `gh.py text NAME $'pasted one\npasted two'`。两行原样进入输入框，不会被当成两次提交。
  3. **输入历史：** 发出 `first`，在空输入框里按 Up，输入框显示 `first`。
  4. **光标移动：** 输入 `foo_bar baz`，按两次 Alt+B，再输入 `X`。输入框是 `Xfoo_bar baz`（下划线算单词的一部分）。
  5. **中文宽度：** 输入 `中文测试 abc`，按 Home，再输入 `X`。输入框是 `X中文测试 abc`，边框对齐，没有错位或残影。
  6. **Esc 和 Ctrl+C：**
     - 发出 `mock_stream_delay_ms 10000`，运行中按 Esc，这一轮被中断，状态回到空闲；
     - 空闲时在空输入框里按 Ctrl+C，行为和升级前一样。用 `ORCA_BIN` 指向升级前构建的二进制，对比一次。
  7. **图片预览：** 用 `gh.py text` 粘贴一张 PNG 的绝对路径（路径里带空格）。输入框显示图片预览，发出后对话里有缩略图，Ghostty 里用 kitty 协议显示出图片。
  8. **still running：** 发出 `bash_wait 200 :: sleep 3; echo done`。那一行先显示 `still running`，3 秒后标签消失。
- [ ] **收尾和问题处理。**
  - 用 `gh.py close NAME` 关闭每个窗口。不要碰用户自己开着的 "Orca" 窗口。
  - 发现问题时：先判断是不是升级带来的（用升级前的二进制对比），再派修复任务。
- [ ] **推 CI。** `git push origin HEAD:refs/heads/ci/dependency-upgrades`，重新触发两个 workflow，全部通过才继续 Task 8。

### Task 8：网站

**Files:**
- Modify: `site/package.json`、`site/package-lock.json`、`site/vite.config.ts`。

**Interfaces:** 无。

- [ ] **Step 1：记下升级前的样子。**
  - `env -u NODE_OPTIONS npm --prefix site run build`。
  - 用 Python 的 `html.parser` 把 `site/dist/**/*.html` 每个页面的可见文字抽出来，存成 `$CLAUDE_JOB_DIR/tmp/site-text-before.json`，以页面路径为键。
  - 截图：
    - `cd site && env -u NODE_OPTIONS npx vite preview --port 4199 --strictPort`；
    - 用无头 Chrome 加 CDP 截 6 张图：首页、`/docs/`、`/changelog/`，中英文各一套。语言通过 `localStorage["orca-site-locale"]` 设成 `"en"` 或 `"zh"` 再重新导航；加 `--force-prefers-reduced-motion`，让首页的终端动画直接显示最终状态；
    - 存到 `$CLAUDE_JOB_DIR/tmp/site-before/`；
    - 停掉 preview 服务。
- [ ] **Step 2：升级。** 在 `site/` 下执行：

  ```bash
  env -u NODE_OPTIONS npm install --save-exact vite@latest @vitejs/plugin-react@latest
  env -u NODE_OPTIONS npm install typescript@latest react@latest react-dom@latest highlight.js@latest
  env -u NODE_OPTIONS npm install --save-dev @mermaid-js/mermaid-cli@latest @types/react@latest @types/react-dom@latest
  ```

  装完核对版本，不应低于 spec 第 5 节列的版本：vite 8.3.3、@vitejs/plugin-react 6.1.2、typescript 7.0.2、mermaid-cli 12.0.0，以及 react、react-dom、两个 @types 的 19.3、highlight.js 的 11.12。
- [ ] **Step 3：删掉 overrides。** 删掉 `package.json` 里的整个 `"overrides"`（esbuild 0.28.1 和 postcss 8.5.25），再 `env -u NODE_OPTIONS npm install`。然后：
  - `env -u NODE_OPTIONS npm ls esbuild postcss`：esbuild 不再出现，或者只被别的包带进来；postcss ≥ 8.5.28；
  - `env -u NODE_OPTIONS npm audit`：没有 high 或 critical。
- [ ] **Step 4：改 vite 配置。** `vite.config.ts` 里的 `build.rollupOptions` 改名为 `build.rolldownOptions`（vite 8 里 `rollupOptions` 只是已弃用的别名），内容不变。
- [ ] **Step 5：构建和检查。**
  - `env -u NODE_OPTIONS npm --prefix site run build`。这一步包括 `tsc --noEmit`（TypeScript 7）、`vite build` 和预渲染。
  - `env -u NODE_OPTIONS npm --prefix site run check:seo`；
  - `cd site && env -u NODE_OPTIONS npx mmdc --version` 能运行；
  - `env -u NODE_OPTIONS node --test tests/pages_workflow_contract.test.mjs`；
  - `env -u NODE_OPTIONS cargo nextest run --test public_docs_contract --locked --retries 0`。

  如果 TypeScript 7 不兼容（`tsc` 报错，而且不是代码本身的问题）：退回 `typescript@^5.9.3`，在提交说明里写明原因和报错，并报告控制会话。其他大版本也按同样的规则处理。
- [ ] **Step 6：和升级前对比。**
  - 用同样的方法抽出页面文字，和 `site-text-before.json` 比较，每个页面的可见文字一致；
  - 用同样的方法截 6 张图，存到 `site-after/`，逐张和 `site-before/` 对比看（用 Read 工具看图），版式、字体、颜色没有可见的变化。

  有差异就说明原因，属于回归的修掉。
- [ ] **Step 7：提交。** `chore(deps): move the site to vite 8, TypeScript 7 and React 19.3`

## 收尾（控制会话）

- [ ] **全量验证。** 在这台 Mac 上跑：
  - Global Constraints 里的全量检查（包括 http 校验脚本）。这台 Mac 上的全量会跑到 macOS 沙箱测试；
  - `env -u NODE_OPTIONS npm --prefix site run build`；
  - `env -u NODE_OPTIONS npm --prefix site run check:seo`。
- [ ] **整体评审。** 按 subagent-driven-development 的流程，对 52847023 之后的所有提交做一次整体评审，处理评审意见。
- [ ] **CI。** 最终的 HEAD 推到 `ci/dependency-upgrades`，Linux 和 Windows 都通过。
- [ ] **用户手测。** 给用户这份清单：
  1. **中文输入法：** 用拼音输入法在输入框里打一句中文。组字时候选框位置正确；上屏后光标位置正确；文本完整发出。
  2. **粘贴：** 从剪贴板粘贴一段多行文本，里面有中文和 `foo_bar` 这样的标识符。文本原样进入输入框，没有被拆成多次提交。
  3. **图片：** 把一张图片的路径粘贴进输入框。预览正常，发出后对话里的缩略图正常。
  4. **按单词删除：** 在 `foo_bar baz` 上用 Alt+←/→ 和 Ctrl+W，确认 `foo_bar` 被当成一个词。
  5. **still running：** 让 Orca 运行一个超过等待窗口的命令（例如 `sleep 20; echo done`）。跑完后那一行不再显示 `still running`。
- [ ] **发版。** 问用户要不要发 v0.5.8。要发的话：
  - 先更新发布文档：`docs/releases/v0.5.8.md`、网站更新日志、中英文 README，以及 GitHub Release 正文；
  - 发布说明写明：证书改用系统证书库（加密实现仍是 ring）；按单词移动时下划线算单词的一部分；still running 的修复；依赖升级概要；
  - 再按发布流程推 main、打 tag。推 main 和打 tag 都要用户同意。
- [ ] **更新持久记忆。** 改 `orca-dependency-upgrades-progress`：记下做完了什么、发版状态，以及下一步是子项目 3（ACP SDK 迁移，目标版本要重新评估，最新已经是 3.1.0）。
