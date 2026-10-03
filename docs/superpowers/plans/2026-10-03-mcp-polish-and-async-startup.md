# MCP 完善轮实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 发 v0.5.6 之前把 MCP 补完整：
- 启动时在后台并行连接服务器，与 Claude Code、Codex 一致；
- 修掉上一轮留下的已知问题。

**Architecture:**
- `orca-mcp` 的注册表改为非阻塞：服务器以 `Starting` 开始，各自在线程上并行连接，完成后通知订阅者。
- runtime 的会话 actor 订阅注册表变化，有变化就发布 MCP 目录；每轮第一次请求模型前，等待仍在连接的服务器。
- TUI 启动时就建好注册表并开始连接。首条消息启动线程时，把注册表交给线程。在此之前，`/mcp` 和 prompt 命令直接使用这个注册表。

**Tech Stack:** Rust（workspace crates `orca-core`、`orca-mcp`、`orca-runtime`、`orca-tui`、`orca-platform`）、`reqwest`、`tokio`、`toml_edit`、ratatui；测试用 nextest。

**Spec:** `docs/superpowers/specs/2026-10-03-mcp-polish-and-async-startup-design.md`

## Global Constraints

- **平台**：所有改动都要在 Linux、Windows、macOS 上编译并通过测试。
  - 用 shell 脚本充当 MCP 服务器的测试加 `#[cfg(unix)]`。
  - 在已有条目上方插入新条目时，检查有没有抢走原条目的 `#[cfg]`。
- **兼容性**：MCP 部分不考虑向后兼容（spec §0），类型和接口可以直接改。
  - 保留 `transport = "sse"` 的回退和 `[mcp_servers.capabilities]`。
  - 已有会话必须照常打开：已发布版本从未写过 MCP 目录记录，不需要为此做任何事。
- **写用户配置**：只写 `$ORCA_HOME/config.toml`。
  - 用 `toml_edit` 保留原有内容和注释。
  - 用 `orca_platform::fs::atomic_write(.., AtomicWritePolicy::NoFollow)` 原子写入。
  - 解析失败时报错，不覆盖原文件。
- **凭据文件**：`mcp-credentials.json` 只用 `orca_platform::fs::atomic_write_private` 写入。
- **名字**：
  - MCP 工具名为 `mcp__<server>__<tool>`；
  - 服务器名和工具名都按 `orca_core::mcp_types::canonical_mcp_name` 规范化。
- **超时**：
  - 服务器连接过程中的每个请求受各自的 `startup_timeout_ms` 约束，默认 30 秒（`transport::timeout_from_ms`）；
  - 列表分页最多 100 页。
- **文案**：代码和面向用户的输出用英文；中英文档分别更新。
- **每次提交前**：
  - `cargo fmt --all -- --check`；
  - `cargo clippy --workspace --all-targets --locked`，不能有错误；
  - 直接运行（不接 `tail`）`node scripts/validate-runtime-surface-contract.mjs` 和 `node scripts/validate-windows-platform-boundaries.mjs`。

  validator 因为固定的计数、源码行引用或摘要失败时，在同一个提交里更新固定值，并在报告里写明。
- **测试**：
  - 用 `cargo nextest run`，不用 `cargo test`。
  - TUI 的样式只走 `crates/orca-tui/src/chrome.rs`，颜色守卫和 golden frame 有变化时一并更新。
  - 测试里的 TCP 服务器在 `accept()` 之后对连接调用 `set_nonblocking(false)`：macOS 上，接受的连接会继承监听 socket 的非阻塞属性。
- **测试不碰真实 home**：测试一律不读写真实的 `~/.orca`。需要配置目录时，用各 crate 现有的临时 home 辅助函数（如 `with_orca_home`、`isolated_test_orca_home`）或显式的临时路径。

## Review Focus

1. **服务器卡住不回应。** 一个永不回应 `initialize` 的服务器，不能让第一轮永远等下去：它在启动超时后变为 `Failed`，这一轮照常继续。由任务 2 的 `a_hung_server_fails_after_its_startup_timeout_and_the_turn_goes_on` 钉住。
2. **启动时就需要登录。** 一开始就要求登录的远程服务器，启动结束时应是 `NeedsLogin` 而不是 `Failed`，这一轮不带它的工具照常进行。由任务 1 的 `a_server_that_needs_login_ends_startup_as_needs_login` 钉住。
3. **连接中途退出。** 多个服务器还在连接时退出 TUI，所有 stdio 服务器进程都被结束，不残留。由任务 6 的 `quitting_while_servers_are_still_starting_stops_them` 钉住。
4. **首条消息前 `/new`。** 首条消息之前执行 `/new`，新会话接手已经开始的连接，不重复启动服务器。由任务 6 的 `new_before_the_first_message_uses_the_prestarted_servers` 钉住。
5. **登录跨过首条消息。** 首条消息之前开始、之后才完成的登录，会让会话里的服务器连上。注册表是同一个，所以登录后的重连作用于会话。由任务 6 的 `a_login_that_finishes_after_the_first_message_updates_the_thread` 钉住。

---

### Task 1：注册表异步化，凭据路径由配置传入

**Files:**
- Modify: `crates/orca-mcp/src/client.rs`（`initialize_registry`、`McpRegistry`、`McpServerState`、`McpServerEntry`、`reconnect_server`、`server_states`）、`crates/orca-mcp/src/lib.rs`（导出）
- Modify: `crates/orca-core/src/config/mod.rs`（`RunConfig`）、加载配置时组装 `RunConfig` 的地方（设置 `mcp_servers` 的同一处）
- Modify: `initialize_registry` 的其他调用方：
  - `crates/orca-runtime/src/session.rs:203`
  - `crates/orca-runtime/src/child_agent_loop_setup.rs:143`
  - `crates/orca-runtime/src/subagent_async_worker.rs:486,1142`
  - `crates/orca-runtime/src/server.rs:2719`
  - `crates/orca-runtime/src/workflow/runner.rs:2399`
  - `crates/orca-runtime/src/runtime_host.rs:2500`
  - `crates/orca-tools/src/lib.rs:575`
  - `crates/orca-mcp/src/transport.rs:4719`
- Modify: 所有对 `McpServerState` 的穷举匹配，例如 `crates/orca-runtime/src/mcp_catalog.rs:126`：`Starting` 先映射到界面层现有的 `Starting`，任务 2 再统一改名。
- Test: `crates/orca-mcp/src/client.rs` 的测试模块

**Interfaces:**
- **Produces：**
  - `RunConfig::mcp_credentials_path: Option<PathBuf>`
    - 加载配置时设为 `config_dir().map(|dir| dir.join(MCP_CREDENTIALS_FILE))`；
    - `Default` 为 `None`。
  - `pub fn initialize_registry(configs: &[McpServerConfig], credentials_path: Option<PathBuf>) -> McpRegistry`
    - 立即返回，取代旧的单参数版本和 `initialize_registry_with_credentials`。
  - `McpServerState`：新增 `Starting`，取值共 `Starting`、`Ready`、`Failed { message }`、`NeedsLogin`、`Disabled`。
  - `pub struct McpServerStatus { pub name: String, pub state: McpServerState, pub prompts_error: Option<String> }`
  - `McpRegistry` 的新方法：
    - `pub fn server_statuses(&self) -> Vec<McpServerStatus>`，取代 `server_states`；
    - `pub fn subscribe(&self, on_change: Arc<dyn Fn(&McpRegistry) + Send + Sync>) -> McpChangeSubscription`；
    - `pub fn wait_for_startup(&self, should_cancel: &dyn Fn() -> bool) -> bool`：没有 `Starting` 时返回 `true`，被取消时返回 `false`；
    - `pub fn is_starting(&self) -> bool`。
  - `pub struct McpChangeSubscription`：drop 时退订。
- **约定：**
  - 每次服务器状态、工具或 prompts 变化后，先释放写锁，再调用订阅者。
  - 回调拿到的是 `&McpRegistry`。订阅者不要在闭包里捕获注册表的克隆，以免形成循环引用。

- [ ] **Step 1：写失败的测试。** 用 `#[cfg(unix)]` 的 sh 夹具；夹具在 `initialize` 前按参数 `sleep`，并把每次启动的 pid 记到状态目录。
  - `startup_returns_before_servers_connect`：服务器 `initialize` 要睡 1 秒。`initialize_registry` 在 300ms 内返回，`server_statuses()` 里该服务器为 `Starting`。
  - `servers_connect_in_parallel`：两个各睡 1 秒的服务器，`wait_for_startup(&|| false)` 在 1.8 秒内返回 `true`，两个都是 `Ready`。
  - `a_subscriber_hears_each_server_finish`：两个服务器，回调至少被调用 2 次，且回调里读到的 `server_statuses()` 已含新状态。
  - `waiting_for_startup_can_be_cancelled`：`should_cancel` 返回 `true` 后，200ms 内返回 `false`。
  - `a_reconnect_started_while_starting_wins`：夹具第一次启动睡 1 秒、列出工具 `first`，之后的启动立即列出 `second`。启动后立刻 `reconnect_server`，两者都结束后，工具为 `mcp__x__second`。
  - `dropping_the_registry_stops_a_server_that_connects_later`：服务器还是 `Starting` 时 drop 注册表，等连接本该完成之后，夹具记下的 pid 已不存活。
  - `a_server_that_needs_login_ends_startup_as_needs_login`：本地 HTTP 夹具对 `initialize` 回 401，启动结束后状态为 `NeedsLogin`。
  - `a_prompt_list_failure_is_kept_on_the_server_status`：`prompts/list` 回 JSON-RPC 错误，状态为 `Ready`，`prompts_error` 为 `Some`。
  - `a_default_run_config_has_no_credentials_path`：`RunConfig::default().mcp_credentials_path` 为 `None`。
- [ ] **Step 2：确认测试失败。**
  - 运行 `cargo nextest run -p orca-mcp --lib --locked --retries 0 startup_returns servers_connect_in_parallel a_subscriber_hears waiting_for_startup a_reconnect_started dropping_the_registry ends_startup_as_needs_login prompt_list_failure_is_kept`。
  - 运行 `cargo nextest run -p orca-core --lib --locked --retries 0 a_default_run_config_has_no_credentials_path`。
- [ ] **Step 3：实现。**
  - 每个启用的服务器先以 `Starting` 入表，再起一个名为 `orca-mcp-connect-<name>` 的线程去连接。
  - 连接线程只持有注册表的 `Weak`：连接完成后 upgrade 失败，就直接丢弃新连接，从而结束进程。
  - 完成后的替换和 `reconnect_server` 用同一个函数。
  - 每个服务器有代次 `generation: u64`：
    - 初次连接为 1；
    - `reconnect_server` 在开始时加 1；
    - 替换时只接受与当前代次相同的结果，较旧的结果直接丢弃。
  - `prompts/list` 失败的原因存进 `prompts_error`，不再混在 `errors` 里。
  - `wait_for_startup` 每 25ms 检查一次，也可以用条件变量。
- [ ] **Step 4：更新调用方。**
  - 除 `runtime_host.rs:2500` 之外的调用方，在 `initialize_registry(..)` 之后立即调用 `.wait_for_startup(&|| false)`，保持它们原来"连完再继续"的语义（只是连接变成并行）。
  - 凭据路径一律传 `config.mcp_credentials_path.clone()`。
  - `runtime_host.rs:2500` 不等待，由任务 2 处理。
  - 已有测试里直接用 `initialize_registry` 结果的，同样补上等待。
- [ ] **Step 5：确认测试通过。**
  - 先跑 Step 2 的两条命令；
  - 再跑 `cargo nextest run -p orca-mcp -p orca-core --locked --profile ci --retries 0`；
  - 最后跑 `cargo nextest run -p orca-runtime --lib --locked --profile ci --retries 0`。
- [ ] **Step 6：提交。** `feat(mcp): connect MCP servers in parallel without blocking`

### Task 2：runtime 接入（目录随变化发布、首轮等待、状态精简）

**Files:**
- Modify: `crates/orca-runtime/src/runtime_surface/projection.rs:2058-2160`（`SurfaceMcpServerStatus`、`SurfaceMcpCatalogSnapshot`）
- Modify: `crates/orca-runtime/src/mcp_catalog.rs`
- Modify: `crates/orca-runtime/src/runtime_host.rs`（线程启动在 2497-2501 行；actor 的 `run` 在 17450 行附近，参照 `capability_change_rx` 的写法加一个 MCP 变化通道）
- Modify: `crates/orca-runtime/src/agent_loop.rs:88`（在 `RuntimeTurnSetupStep::prepare` 之前等待）
- Modify: `crates/orca-tui/src/surface_projection.rs`（`McpServerStatusView::from_surface` 的映射，只为编译通过）
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.md` 及其摘要固定值
- Test: `crates/orca-runtime/src/mcp_catalog.rs`、`crates/orca-runtime/src/runtime_host.rs` 的测试模块

**Interfaces:**
- **Consumes：** 任务 1 的 `initialize_registry`、`subscribe`、`wait_for_startup`、`server_statuses`。
- **Produces：**
  - `pub enum SurfaceMcpServerStatus { Starting, Ready, Failed { message: DisplayText }, NeedsLogin, Disabled }`
  - `pub struct SurfaceMcpServer { pub name: NonEmptyText, pub status: SurfaceMcpServerStatus, pub prompts_error: Option<DisplayText> }`
  - `SurfaceMcpCatalogSnapshot::servers: Vec<SurfaceMcpServer>`
  - 线程启动不再等待服务器连接。
  - actor 在注册表变化时调用 `publish_mcp_catalog`，只在内容不同时提交。
  - agent loop 在一轮准备之前调用 `mcp_registry.wait_for_startup(&|| cancel.is_cancelled())`，被取消就按现有的取消路径结束这一轮。

- [ ] **Step 1：写失败的测试。**
  - `catalog_reports_starting_failed_and_needs_login`（`mcp_catalog.rs`）：状态一一映射，`prompts_error` 进入 `SurfaceMcpServer`。
  - `thread_start_does_not_wait_for_servers`：线程启动带一个 `initialize` 睡 2 秒的 stdio 服务器，`start_thread` 在 1 秒内返回。
  - `the_catalog_updates_when_a_server_finishes_connecting`：不做任何操作，先读到 `Starting`，再在 3 秒内读到 `Ready` 和它的工具。
  - `the_first_request_sees_a_slow_servers_tools`：mock provider 的一轮调用慢服务器的工具，成功并得到夹具输出。复用 mock provider 已经认识的 `mcp__slow__wait`（`crates/orca-provider/src/lib.rs` 的 `parse_mock_prompt`）：服务器名为 `slow`，工具名为 `wait`。
  - `cancelling_a_turn_while_servers_connect_stops_waiting`：服务器睡 5 秒，一轮开始后 300ms 取消，这一轮在 1 秒内以取消结束。
  - `a_hung_server_fails_after_its_startup_timeout_and_the_turn_goes_on`：`startup_timeout_ms = 300`，服务器永不回应 `initialize`。这一轮在 2 秒内正常完成，目录里该服务器为 `Failed`。
- [ ] **Step 2：确认测试失败。** `cargo nextest run -p orca-runtime --lib --locked --retries 0 catalog_reports_starting thread_start_does_not_wait the_catalog_updates_when the_first_request_sees cancelling_a_turn_while a_hung_server_fails`
- [ ] **Step 3：实现。**
  - 线程启动用 `initialize_registry(&config.mcp_servers, config.mcp_credentials_path.clone())`，不等待。
  - actor 启动时订阅注册表：回调对一个容量为 1 的 tokio 通道 `try_send(())`；`run` 循环收到后调用 `publish_mcp_catalog`。线程结束时 drop 订阅。
  - 状态映射：
    - `Starting` → `Starting`
    - `Ready` → `Ready`
    - `Failed` → `Failed`
    - `NeedsLogin` → `NeedsLogin`
    - `Disabled` → `Disabled`
  - TUI 侧 `from_surface` 对应改名，行为不变。
- [ ] **Step 4：同步契约文档。**
  - 契约文档：更新状态枚举、`SurfaceMcpServer`，以及"连接完成时发布目录"的描述。
  - 按 validator 提示更新摘要和固定值。
- [ ] **Step 5：确认测试通过。**
  - 先跑 Step 2 的命令；
  - 再跑 `cargo nextest run -p orca-runtime --locked --profile ci --retries 0`；
  - 然后跑 `cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0`；
  - 最后跑两个 validator。
- [ ] **Step 6：提交。** `feat(runtime): publish MCP servers as they connect and wait only for those still starting`

### Task 3：状态准确与重连

**Files:**
- Modify: `crates/orca-mcp/src/client.rs`：
  - `mark_needs_login`（767 行附近）；
  - `call_tool_inner`（1049 行附近）；
  - `get_prompt`；
  - 资源请求；
  - `McpClient::call_tool`、`McpClient::reconnect`（1456 行附近）；
  - `reconnect_server`。
- Test: `crates/orca-mcp/src/client.rs` 的测试模块

**Interfaces:**
- **Consumes：** 任务 1 的代次和 `server_statuses`。
- **Produces：**
  - `fn mark_needs_login(&self, server: &str, client: &Arc<McpClient>)`：只有 `client` 仍是该服务器当前的客户端（`Arc::ptr_eq`）时，才标记。
  - `fn mark_ready_after_success(&self, server: &str, client: &Arc<McpClient>)`：当前为 `NeedsLogin` 且客户端相同时，改回 `Ready`。
  - 这两个函数在状态改变时都通知订阅者。

- [ ] **Step 1：写失败的测试。**
  - `a_failure_on_a_replaced_connection_does_not_mark_the_new_one`：旧客户端上的调用返回需要登录时，`reconnect_server` 已经换上了新客户端，状态保持 `Ready`。
  - `a_prompt_or_resource_request_that_needs_login_marks_the_server`：`prompts/get` 和 `resources/list` 收到需要登录，状态变为 `NeedsLogin`。
  - `a_successful_request_clears_needs_login`：标记之后，同一客户端上的一次成功调用让状态回到 `Ready`。
  - `a_tool_call_that_stops_the_server_reconnects_before_returning`：夹具对第一次 `tools/call` 回一个没有 `result` 的响应。这次调用报错，下一次调用直接成功；夹具日志里不出现 "Broken pipe"。
  - `reconnecting_a_stdio_server_stops_the_old_process_first`：夹具启动时检查上一个 pid 是否存活，存活就在状态目录写 `overlap`。分别经 `reconnect_server` 和客户端级重连各重连一次，都不出现 `overlap`。
- [ ] **Step 2：确认测试失败。** `cargo nextest run -p orca-mcp --lib --locked --retries 0 a_failure_on_a_replaced a_prompt_or_resource_request a_successful_request_clears a_tool_call_that_stops reconnecting_a_stdio_server_stops`
- [ ] **Step 3：实现。**
  - 工具调用失败后，如果传输 `is_closed()`，就在返回错误之前调用 `reconnect`，与 `request` 的做法一致。
  - stdio 服务器（`McpTransportKind::Stdio`）重连时：
    - 先取下并 drop 旧客户端或旧传输，再连接新的；
    - `reconnect_server` 先从 `clients` 里移除旧客户端，在锁外 drop；
    - `McpClient::reconnect` 在持有传输锁时，先 `terminate` 旧传输，再连接新的。
  - 远程服务器维持先连新、后断旧。
- [ ] **Step 4：确认测试通过。**
  - 先跑 Step 2 的命令；
  - 再跑 `cargo nextest run -p orca-mcp --locked --profile ci --retries 0`。
- [ ] **Step 5：提交。** `fix(mcp): keep MCP server states accurate across reconnects`

### Task 4：协议补全

**Files:**
- Modify: `crates/orca-mcp/src/transport.rs`：
  - trait `McpTransport` 的列表方法；
  - stdio 请求循环（650-690 行附近）；
  - HTTP 流式读取里的服务器请求（1830 行附近）；
  - 阻塞 SSE 读取 `read_bounded_sse_response`（2040 行附近）。
- Modify: `crates/orca-mcp/src/legacy_sse.rs:728`（`dispatch`）
- Modify: `crates/orca-mcp/src/client.rs`（列表调用改为分页汇总）
- Modify: `crates/orca-core/src/mcp_types.rs`（各 `*ListResult` 增加 `next_cursor`）
- Modify: `crates/orca-mcp/src/oauth.rs:287`（`http_client` 的重定向策略）
- Test: 以上文件的测试模块

**Interfaces:**
- **Produces：**
  - 列表方法都带游标参数：`fn list_tools(&self, cursor: Option<&str>) -> Result<Value, String>`。
    - `list_prompts`、`list_resources`、`list_resource_templates` 及它们的 `_or_cancel` 版本同样增加 `cursor`。
    - 有游标时，请求参数为 `{"cursor": c}`。
  - `#[serde(rename = "nextCursor", default)] pub next_cursor: Option<String>`：加在 `ToolsListResult`、`PromptsListResult`、`ResourcesListResult`、`ResourceTemplatesListResult` 上。
  - `const MAX_LIST_PAGES: usize = 100;`
  - `fn collect_pages<T>(fetch: impl FnMut(Option<&str>) -> Result<(Vec<T>, Option<String>), String>) -> Result<(Vec<T>, Option<String>), String>`：
    - 第二个返回值是警告：`MCP server repeated a list cursor` 或 `MCP list stopped after 100 pages`；
    - 警告记进该服务器的 `errors`。
  - `fn allows_metadata_redirect(previous: &Url, next: &Url) -> bool`：从 https 到 http 返回 `false`。
    - 元数据请求使用自定义重定向策略：调用它，最多跟 10 次。

- [ ] **Step 1：写失败的测试。**
  - `tools_are_listed_across_pages`：三页，共 5 个工具，全部注册。
  - `a_repeated_cursor_stops_listing`：第二页返回与第一页相同的游标。只请求两页，`errors` 里有 `repeated a list cursor`。
  - `listing_stops_after_100_pages`：夹具无限返回新游标。正好请求 100 页，`errors` 里有 `stopped after 100 pages`。
  - `a_server_ping_is_answered`：stdio 夹具在回答 `tools/list` 之前先发 `{"jsonrpc":"2.0","id":"p1","method":"ping"}`，并检查收到 `{"id":"p1","result":{}}`。
  - `an_unknown_server_request_gets_method_not_found`：stdio 上回 `-32601`。
  - `a_server_request_with_our_id_is_not_taken_for_the_response`：服务器先发一个 id 与待回请求相同的 `ping`，再发真正的响应。请求成功，服务器进程没有被结束。
  - `legacy_sse_answers_a_ping`、`http_answers_a_ping_inside_a_response_stream`：两种远程传输各回应一次 `ping`。
  - `a_blocking_sse_read_ends_at_the_matching_response`：HTTP 夹具发出匹配的响应后保持连接、不关流。请求在 1 秒内返回，而超时设为 10 秒。
  - `metadata_redirects_never_drop_to_http`：
    - `allows_metadata_redirect(https://a, http://b) == false`
    - `allows_metadata_redirect(https://a, https://b) == true`
    - `allows_metadata_redirect(http://127.0.0.1, http://127.0.0.1:1) == true`
- [ ] **Step 2：确认测试失败。** `cargo nextest run -p orca-mcp --lib --locked --retries 0 listed_across_pages repeated_cursor stops_after_100 ping_is_answered unknown_server_request with_our_id legacy_sse_answers http_answers_a_ping blocking_sse_read_ends metadata_redirects`
- [ ] **Step 3：实现。**
  - 三种传输对服务器发来的消息按同一规则处理：
    - 有 `method` 且有 `id` 的是请求：`ping` 回 `{}`，`elicitation/create` 维持现状，其他回 `-32601`。
    - 有 `method` 而没有 `id` 的是通知，忽略。
    - 只有不含 `method` 的消息，才按 id 匹配为响应。
  - 阻塞 SSE 改为边读边解析，读到匹配 id 的响应就返回；16 MiB 上限不变。
  - 所有 `McpTransport` 实现（包括测试替身）都更新签名。
- [ ] **Step 4：确认测试通过。**
  - 先跑 Step 2 的命令；
  - 再跑 `cargo nextest run -p orca-mcp -p orca-core --locked --profile ci --retries 0`；
  - 最后跑 `cargo nextest run -p orca-runtime --lib --locked --profile ci --retries 0`。
- [ ] **Step 5：提交。** `fix(mcp): page MCP lists, answer server pings, and end SSE reads at the response`

### Task 5：写配置加锁，支持内联数组

**Files:**
- Modify: `crates/orca-core/src/config/user_edit.rs`（`edit_user_config_in` 在 17 行；`list_user_mcp_servers_in`、`add_user_mcp_server_in`、`remove_user_mcp_server_in`）
- Test: `crates/orca-core/src/config/user_edit.rs`、`crates/orca-runtime/src/command/mcp.rs` 的测试模块

**Interfaces:**
- **Produces：**
  - `edit_user_config_in` 在读、改、写的整个过程中，持有 `orca_platform::fs::ExclusiveFileLock`，锁文件为 `<dir>/config.toml.lock`。
    - 所有写配置的路径都经过它，包括 `persist_user_model_settings`。
  - `mcp_servers` 可以是表数组 `[[mcp_servers]]`，也可以是内联数组 `mcp_servers = [{…}]`：
    - list、get、remove 两种都处理；
    - add 沿用文件已有的写法，文件里还没有时用表数组。

- [ ] **Step 1：写失败的测试。**
  - `concurrent_config_edits_keep_every_change`：8 个线程各调用 10 次 `add_user_allow_rule_in`，工具名各不相同。最后配置里有 80 条规则。flock 对同一进程里不同的打开也互斥。
  - `inline_mcp_servers_round_trip`：配置为 `mcp_servers = [{ name = "a", command = "x" }]`。
    - list 列出 `a`；
    - add `b` 后，文件仍是内联数组，并含 `b`；
    - remove `a` 后只剩 `b`；
    - 注释保留。
  - `orca_mcp_list_reads_an_inline_array`（`command/mcp.rs`）：CLI 的 list 输出含该服务器。
- [ ] **Step 2：确认测试失败。**
  - 运行 `cargo nextest run -p orca-core --lib --locked --retries 0 concurrent_config_edits inline_mcp_servers`。
  - 运行 `cargo nextest run -p orca-runtime --lib --locked --retries 0 orca_mcp_list_reads_an_inline_array`。
- [ ] **Step 3：实现。**
- [ ] **Step 4：确认测试通过。** 先跑 Step 2 的两条命令，再跑 `cargo nextest run -p orca-core --locked --profile ci --retries 0`。
- [ ] **Step 5：提交。** `fix(config): lock config.toml edits and accept an inline mcp_servers array`

### Task 6：TUI 启动即连接

**Files:**
- Create: `crates/orca-tui/src/prestart_mcp.rs`
- Modify: `crates/orca-tui/src/operation_controller.rs`（`TuiSurfaceTaskControl`，在 20 行；`runtime_thread()` 在 304 行）
- Modify: `crates/orca-tui/src/hosted_controller.rs`：在 204-260 行的启动会话处理之后，如果没有线程，就预启动。
- Modify: `crates/orca-tui/src/hosted_session_lifecycle.rs`：在以下几处交接注册表：
  - `ensure_hosted_thread`（27 行）；
  - `start_new_hosted_session`（62 行）；
  - 切换或恢复会话时启动线程的地方（241、289 行附近）。
- Modify:
  - `crates/orca-tui/src/action_dispatcher.rs`、`mcp_server_actions.rs`：线程启动前，`r`/`l`/`o` 作用于预启动的注册表。
  - `slash_command_actions.rs`、`mcp_prompt_actions.rs`：线程启动前用注册表展开 prompt；去掉 R24 的提示。
  - `types.rs`：去掉"只列配置"和 `NotConnectedYet`；`mcp_servers_before_start()` 改为"还没有线程"。
- Modify:
  - `surface_projection.rs`：`McpCatalogView::from_registry`。
  - `protocol.rs`、`state_reducer.rs`：新事件。
  - `ui.rs`：`stream_phase_label`，在 5590 行附近。
- Modify: `site/src/docs/md/{en,zh}/terminal-ui.mdx`、`mcp-integration.mdx`
- Test: 以上 TUI 文件的测试模块

**Interfaces:**
- **Consumes：** 任务 1 的注册表接口；任务 2 的 `SurfaceMcpServerStatus` 和 `SurfaceMcpServer`；`RuntimeThreadStartRequest::with_mcp_registry`（`runtime_host.rs:2478`）。
- **Produces：**
  - `pub(crate) struct PrestartedMcp`：持有 `McpRegistry` 和 `McpChangeSubscription`。
  - `pub(crate) fn start_prestart_mcp(configs: &[McpServerConfig], credentials_path: Option<PathBuf>, event_tx: mpsc::Sender<TuiEvent>) -> Option<PrestartedMcp>`
    - 没有启用的服务器时返回 `None`；
    - 订阅回调发送 `TuiEvent::McpCatalogPrestart(McpCatalogView::from_registry(registry))`。
  - `TuiSurfaceTaskControl` 的新方法：
    - `set_prestart_mcp(PrestartedMcp)`；
    - `prestart_mcp_registry() -> Option<McpRegistry>`；
    - `take_prestart_mcp() -> Option<McpRegistry>`：取出时 drop 订阅。
  - 任何线程启动请求在发出前，先 `take_prestart_mcp()`；有就 `.with_mcp_registry(registry)`。
  - reducer：一旦应用过带 MCP 目录的界面层投影，就忽略之后的 `McpCatalogPrestart`。
  - `stream_phase_label`：这一轮还没有模型输出、且目录里有 `Starting` 的服务器时，返回 `connecting MCP servers`。
  - prompt 命令：
    - 线程启动前：用 `registry.get_prompt` 展开，然后走现有的提交路径（会启动线程）。
    - 服务器还在 `Starting`、目录里还没有它的 prompts 时，输入 `/mcp__<server>__…` 提示 `MCP server <name> is still connecting; try again in a moment.`。

- [ ] **Step 1：写失败的测试。**
  - `before_the_first_message_the_panel_shows_live_server_status`：预启动一个 stdio 夹具，收到事件后打开面板，显示 `connected · 1 tool`。
  - `the_first_message_hands_the_prestarted_registry_to_the_thread`：发第一条消息后，夹具的启动次数仍为 1。
  - `new_before_the_first_message_uses_the_prestarted_servers`：首条消息前执行 `/new`，启动次数仍为 1。
  - `reconnect_before_the_first_message_uses_the_prestarted_registry`：按 `r` 后启动次数为 2，并有重连通知。
  - `a_login_that_finishes_after_the_first_message_updates_the_thread`：用可注入的登录闭包，在首条消息之后才返回成功。会话里的目录随后显示 `Ready`。
  - `a_prompt_command_runs_before_the_first_message`：展开结果作为第一条消息提交，线程随之启动。
  - `a_prompt_for_a_server_still_connecting_says_so`
  - `quitting_before_the_first_message_stops_mcp_servers`：drop 控制器后，夹具 pid 不存活。
  - `quitting_while_servers_are_still_starting_stops_them`：3 个各睡 2 秒的服务器，立即退出，2.5 秒后都不存活。
  - `the_status_line_says_connecting_while_servers_start`
  - 删除或改写 R17、R24 的测试（`not connected yet`、`send a message first`）。
- [ ] **Step 2：确认测试失败。** 分别对每个测试名运行 `cargo nextest run -p orca-tui --lib --locked --retries 0 <name>`。
- [ ] **Step 3：实现。** 退出时，控制器循环结束，`PrestartedMcp` 随之 drop，stdio 服务器进程随之结束。
- [ ] **Step 4：更新文档。** 中英文都改：
  - `terminal-ui`：说明启动即连接、`/mcp` 和 prompt 命令从一启动就可用、状态行的提示；
  - `mcp-integration`：同上，并删去"首条消息前只列配置"的说明。
- [ ] **Step 5：确认测试通过。**
  - 先跑 Step 2 的命令；
  - 再跑 `cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0`；
  - 然后跑 `cargo nextest run --test tui_pty_contract --locked --profile ci-serial --retries 0`；
  - 最后跑两个 validator。
- [ ] **Step 6：提交。** `feat(tui): connect MCP servers at launch and use them before the first message`

### Task 7：可以取消，以及界面细节

**Files:**
- Modify: `crates/orca-mcp/src/oauth.rs`（`McpLoginOptions` 在 72 行；等待回调的循环在 720 行附近）
- Modify: `crates/orca-tui/src/` 下的这些文件：
  - `mcp_dialog_actions.rs`：再按 `l` 取消；
  - `mcp_server_actions.rs`：取消标志；
  - `types.rs`：正在进行的登录持有取消标志；待处理的 prompt 展开；
  - `mcp_prompt_actions.rs`：用 `Esc` 取消；
  - 按键处理文件；
  - `ui.rs`：行文字和详情排版，详情在 1144-1152 行；
  - `approval_actions.rs`：冲突提示。
- Test: 以上文件的测试模块

**Interfaces:**
- **Produces：**
  - `McpLoginOptions::cancel: Arc<AtomicBool>`，默认 `false`。
    - 等待回调的循环每轮检查它；
    - 被取消时返回 `Err("login to MCP server '<name>' cancelled")`，并释放端口。
    - CLI 的 `orca mcp login` 被 Ctrl+C 中断时，进程直接结束，端口随之释放，这一侧不用改。
  - 面板行文字：
    - 等待登录时显示 `waiting for browser login… (l to cancel)`；
    - 对该服务器再按 `l` 就设置取消标志，并提示 `login to MCP server <name> cancelled`。
  - 详情：
    - 每个工具一行，格式为 `read-only · <rule name>`；不是只读的工具只显示规则名。
    - prompt 一行，格式为 `<name> <required> [optional]`。
    - `prompts_error` 有值时显示 `prompts unavailable: <reason>`。
  - prompt 展开进行中按 `Esc`：
    - 记下"已取消"，提示 `MCP prompt /mcp__<server>__<prompt> cancelled`；
    - 结果到达时直接丢弃。
  - 保存"总是允许"后，如果用户配置里有 `deny` 或 `prompt` 规则匹配该工具，就追加提示 `saved, but a stricter rule in your config still applies to <tool>`。

- [ ] **Step 1：写失败的测试。**
  - `a_cancelled_login_stops_waiting_and_frees_the_port`（orca-mcp）：固定 `callback_port`，取消后 500ms 内返回。之后能重新绑定同一端口。
  - `pressing_l_again_cancels_a_waiting_login`
  - `esc_cancels_a_running_prompt_expansion`
  - `details_put_read_only_before_the_rule_name`：在 80 列下渲染，含 `read-only · mcp__github__list_issues`。
  - `details_show_prompt_arguments`：含 `review_pr <pr> [branch]`。
  - `details_show_why_prompts_are_unavailable`
  - `saving_an_allow_rule_under_a_stricter_rule_warns`
- [ ] **Step 2：确认测试失败。**
  - 运行 `cargo nextest run -p orca-mcp --lib --locked --retries 0 a_cancelled_login`。
  - 其余测试，逐个运行 `cargo nextest run -p orca-tui --lib --locked --retries 0 <name>`。
- [ ] **Step 3：实现。**
- [ ] **Step 4：确认测试通过。**
  - 先跑 Step 2 的命令；
  - 再跑 `cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0`；
  - 然后跑 `cargo nextest run -p orca-mcp --locked --profile ci --retries 0`；
  - 最后跑两个 validator。
- [ ] **Step 5：提交。** `feat(tui): cancel MCP logins and prompt expansions, and refine the /mcp details`

### Task 8：端到端、文档措辞与全量检查

**Files:**
- Modify: `tests/mcp_cli_contract.rs`（`HttpMcpFixture`、`a_remote_http_server_added_by_url_works`）
- Modify: `site/src/docs/md/{en,zh}/approval-modes.mdx`（第 86 行的措辞），`mcp-integration.mdx`（状态标签何时更新）

**Interfaces:**
- **Consumes：** 前面各任务的行为。

- [ ] **Step 1：写失败的测试。** `HttpMcpFixture` 记录每个请求的 `Mcp-Session-Id`。`a_remote_http_server_added_by_url_works` 断言：`initialize` 之后的每个 POST 都带 `fixture-session`。
- [ ] **Step 2：运行。** `cargo nextest run -p blade-deepseek --test mcp_cli_contract --locked --retries 0`。如果失败是因为客户端确实没有回传会话 ID，就回到传输层修复。
- [ ] **Step 3：修正文档措辞。** 中英文都改：
  - approval-modes 第 86 行改为 "nothing more than `3` does"；
  - mcp-integration 改为"状态变化时立即更新"。
- [ ] **Step 4：全量检查。** 依次运行：
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --locked`
  - 两个 validator
  - `cargo nextest run --workspace --all-targets --locked --profile ci --no-fail-fast --retries 0`
  - `cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0`
  - `cargo nextest run --test tui_pty_contract --locked --profile ci-serial --retries 0`
  - `npm --prefix site run build`
- [ ] **Step 5：提交。** `test: check the MCP session id end to end and fix MCP doc wording`
