# ACP SDK 迁移设计（子项目 3）

> 状态：待审阅（2026-10-09）。
> 前提：v0.5.6 之后的三个子项目之三。子项目 1（遗留问题修复）已作为 v0.5.7 发布，子项目 2（依赖升级）已作为 v0.5.8 发布。开始本项目前，先修了 issue #118–#120 和 Esc 打断后的几个问题（本地 main 上 7 个提交，e717107f..b86893e7，未推送）。
> 2026-10-09 已定：只做 SDK 迁移。换成新 SDK 的 API，Orca 的行为和 ACP v1 线上格式都不变，不加新的协议功能。目标版本 `agent-client-protocol` 3.2.0，精确固定。
> 做法：在本地 main 上分阶段提交，每一步都要编译通过、测试通过；发不发版由用户决定。
> 版本：3.2.0（2026-10-08 发布）和它固定的 `agent-client-protocol-schema` 1.10.2 是 2026-10-09 查到的最新正式版。执行时若已有 3.2.x 补丁版，改用补丁版并重看它的 CHANGELOG。

## 0. 背景

当前固定的是 `agent-client-protocol = { version = "=0.10.4", features = ["unstable"] }`（根 `Cargo.toml`），对应 schema 0.11.4。此后 SDK 发了三个大版本：

- 0.11 换了编程模型：消息类型移到 `schema` 下；`ClientSideConnection` / `AgentSideConnection` 和 `Client` / `Agent` 两个 trait 没了，换成 builder 加回调；出站调用改成 `send_request` / `send_notification`；不再需要 `LocalSet`。
- 2.0（2026-07-23）：传输层改成带批次的 `TransportFrame`，处理函数和路由的 API 改名，MCP-over-ACP 换了类型。CHANGELOG 写明稳定的 v1 线上 schema 不变。
- 3.0（2026-10-06）：`ConnectionDriver`、协议无关的原始 JSON-RPC 错误、默认不开任何 Cargo feature、MCP-over-ACP 改为按请求绑定；同时修了一批连接排空和关闭的问题。3.1、3.2 只加了预备请求和取消句柄。

1.0、2.0、3.0 都保持稳定的 v1 线上格式，变的是 Rust API。

Orca 用到的 SDK 面不大：

- agent 侧（daemon、stdio bridge）用的是 Orca 自己的 JSON-RPC 层（`acp/rpc_facade.rs`、`acp/supervisor.rs`），只从 SDK 取消息类型和 `Error`。
- SDK 的连接只用在 `acp/client.rs`（TUI attach 和无界面 attach）和 `crates/orca-runtime/tests/acp_agent.rs`。

2026-10-09 盘点了 Orca 引用的 SDK 名字，共 100 个（附录 A）：

- 94 个在 3.2 / schema 1.10.2 里仍然存在，其中 `Agent`、`Client` 变成了角色标记类型，不再是 trait。
- 4 个以前要开 unstable 的类型已经转为稳定，线上格式没变（字段名、类型、`None` 省略都一样）：`UsageUpdate`、`Cost`、`SessionConfigOptionValue`、`SessionAdditionalDirectoriesCapabilities`。
- 删掉了 6 个：两个连接类型，以及 unstable 的会话模型 API 那 4 个类型（第 3 节）。

## 1. 依赖和类型

**要求：**

- 根 `Cargo.toml`：`agent-client-protocol = "=3.2.0"`，不开任何 feature。
  - 3.x 默认没有 feature。
  - Orca 用到的条目全是稳定的，不需要 `unstable`。
  - attach 走 Unix socket，不需要 `stdio` / `process`。
  - schema 生成用不到，不需要 `schemars`。
- 消息类型从 `agent_client_protocol::schema::v1` 引入，`ProtocolVersion` 从 `agent_client_protocol::schema` 引入；`Error`、`ErrorCode`、`Result` 仍在 crate 根。Orca 调用的 `Error::invalid_params`、`internal_error`、`method_not_found`、`auth_required`、`into_internal_error` 都还在。
- 稳定类型上新加的 unstable 字段（fork、notices、compaction、subagents、end-turn token usage、MCP-over-ACP、LLM providers、NES）Orca 都没用到。例如 `PromptResponse` 只设 `stop_reason`，用量走 `UsageUpdate` 通知。
- 看一眼 `Cargo.lock` 新增的传递依赖（futures-concurrency、rustc-hash、uuid、tracing、serde_with 等）。SDK 用 `tracing` 打日志，Orca 不装 subscriber，所以不会多出输出。

## 2. agent 侧

**现状：**

- `OrcaAcpAgent` 实现了 0.10.4 的 `Agent` trait（`crates/orca-runtime/src/acp/agent.rs:2898`，`#[async_trait(?Send)]`）。
- supervisor 按方法名解码参数后，调用 `Agent::initialize`、`authenticate`、`new_session`、`load_session`、`list_sessions`、`set_session_model`、`set_session_mode`、`set_session_config_option`、`cancel`（`crates/orca-runtime/src/acp/supervisor.rs:498–609`）。

**要求：**

- 3.x 没有这个 trait，改成 `OrcaAcpAgent` 的固有方法，签名只换类型路径；supervisor 直接调用这些方法。
- 传输、分发、错误码映射都不动。

## 3. 旧的会话模型 API：线上格式不变

**现状：**

- schema 1.10.2 删掉了 unstable 的会话模型 API：`ModelInfo`、`SessionModelState`、`SetSessionModelRequest`、`SetSessionModelResponse`（0.11.4 里在 `unstable_session_model` 下）。协议改用稳定的 config option（category 为 model）来选模型。
- Orca 两条路都提供：
  - `session/new`、`session/load` 的响应里带 `models`（`agent.rs:2957`、`agent.rs:3014`，由 `settings::models` 生成）；
  - 接受 `session/set_model`（`agent.rs:3073`，`supervisor.rs:537`）；
  - 同时提供 `model` 这个 config option（`settings.rs:144`，`agent.rs:3112`）。
- TUI attach 改模型用 `set_session_model`（`crates/orca-tui/src/acp_client.rs:1103`），读当前模型看 `model` config option（`acp_client.rs:86`）。
- `docs/acp-daemon.md` 写明改模型用 `session/set_model`。

**要求：**

- 线上格式不变。
- 新增一个小模块（例如 `acp/legacy_model.rs`），按 0.11.4 的 serde 形状定义这 4 个类型：字段名、camelCase、`_meta`、`None` 省略都和原来一样。
- 请求类型实现 3.x 的 `JsonRpcRequest`（方法名 `session/set_model`），客户端可以用 `send_request` 发。
- `session/new`、`session/load` 的响应照旧带 `models`：把 SDK 的响应类型和本地的 `models` 字段拼成一个对象序列化（`#[serde(flatten)]` 包一层，或在 supervisor 写结果 JSON 时加上），以第 5 节的基准流量为准。
- `session/set_model` 照旧接受，TUI 照旧用它改模型。

**为什么保留：** 还在用旧 API 的客户端，以及连到新 daemon 的旧版 Orca TUI，都不受影响。去掉它会改变线上行为，不在本轮。`docs/acp-daemon.md` 仍然准确，不用改。

## 4. 客户端：TUI attach 和无界面 attach

**现状：**

- `acp/client.rs` 的 `Connection` 包着 0.10.4 的 `ClientSideConnection`（`Rc`），在 `LocalSet` 上用 `spawn_local` 跑 IO。
- 三个客户端实现了 SDK 的 `Client` trait，都只有 `request_permission` 和 `session_notification` 两个方法：
  - `HeadlessClient`（`crates/orca-runtime/src/acp/client.rs:143`）；
  - `TuiClient`（`crates/orca-tui/src/acp_client.rs:527`）；
  - 测试里的 `WireTestClient`（`crates/orca-runtime/tests/acp_agent.rs:54`）。
- 出站调用走 `Agent` trait 的方法（`initialize`、`new_session`、`load_session`、`prompt`、`cancel`、`set_session_model`、`set_session_mode`、`set_session_config_option` 等）；TUI 的 `control_request`（`acp_client.rs:1072`）参数是 `&impl Agent`。
- `TuiClient` 的状态是 `Rc`、`RefCell`、`Cell`，不是 `Send`。

**3.2 的用法：**

- 连接写成 `Client.builder().on_receive_request(…).on_receive_notification(…).connect_with(transport, |cx: ConnectionTo<Agent>| async { … })`，只在这个闭包里有效。
- 出站请求是 `cx.send_request(req).block_task().await`。
- `ConnectionTo<Agent>` 实现了 `Clone`。
- 处理函数和它返回的 future 都必须是 `Send + 'static`。
- 传输用 `ByteStreams::new(write, read)`，读写端是 futures-io 的类型；和现在一样，用 tokio-util 的 compat 包装 Unix socket 的两半。

**要求：**

1. `Connection` 对外的 API 不变（`connect`、`attach`、`attach_with_metadata`），内部改成：起一个任务跑 `connect_with`，闭包把 `ConnectionTo<Agent>` 交给 `Connection`，然后等连接关闭（对端 EOF，或 `Connection` 被关闭、丢弃）。`Connection` 被丢弃时连接随之结束，和现在 abort IO 任务的效果一样。
2. 新增 Orca 自己的句柄类型（例如 `AgentHandle`），方法名沿用现在调用的那些，内部用 `send_request(…).block_task()` 实现。TUI 的 `control_request` 等调用处改用这个句柄，调用写法基本不变。
3. 新增 Orca 自己的 `ClientHandler` trait（`?Send`，方法同上两个），三个客户端改为实现它。
4. `Send` 适配层：
   - builder 上注册的处理函数是 `Send` 的，只做转发：收到的权限请求连同它的 responder、收到的会话通知，经通道交给 `LocalSet` 上的 `ClientHandler`。
   - 通知按收到的顺序逐个处理。
   - 每个权限请求起一个本地任务处理，和现在一样可以并发；结果经 responder 回复。
   - 连接关闭时通道随之关闭，`LocalSet` 那边还没处理完的权限请求按现在断线的规则 fail closed。
5. Orca 自己需要 `LocalSet` 的地方照旧保留（TUI 的 attach 逻辑、无界面 attach）。TUI 客户端的逻辑不改，不改成 `Send`。

## 5. 验证

1. **线上流量基准（第一个提交，在旧 SDK 上做）**
   - 新测试用原始 JSON-RPC 驱动 daemon，不经过 SDK 客户端，所以迁移影响不到驱动方。
   - 记录 daemon 的每个响应和通知，存成基准文件，和测试一起提交。
   - 覆盖：
     - initialize、authenticate；
     - session/new、session/load、session/list；
     - session/set_mode、session/set_model、session/set_config_option；
     - 带流式更新的 session/prompt（文本、工具调用、plan、usage），以及 session/cancel；
     - daemon 发给客户端的请求（权限、fs 读写、terminal）；
     - `orca.dev/*` 扩展方法；
     - #76–#80 修过的错误参数回复。
   - 比较时解析成 JSON，把 id、时间戳、路径、会话 id 归一化后再比。
   - 迁移后这个测试必须原样通过。
2. **现有测试**
   - `tests/acp_agent.rs` 改用 3.2 的 SDK 客户端。这样它还能证明新 SDK 的客户端和 Orca 的 agent 互通。
   - TUI 的 ACP 客户端测试（断线、权限范围、重连等）、`acp_rpc_facade` 测试、runtime 的其余 ACP 测试都要通过。
   - `scripts/eval/acp_param_probe.py`、`scripts/eval/daemon_probe.py` 用新二进制跑。
3. **实测**
   - `orca daemon` 加 `orca attach new --exec`（无界面）。
   - 在 tmux 里驱动 TUI attach：Esc 打断、权限请求、切换模型。
   - Zed 实测可选，由用户决定。
4. **全量和 CI**
   - 本机：全量 nextest、TUI 库测试（serial）、PTY 合约测试。
   - Linux、Windows CI，以及仓库的几个验证脚本。

## 6. 提交顺序

每个提交都要编译通过、测试通过。为此先在旧 SDK 上把 Orca 这边的结构改好，最后换版本的那个提交只剩类型路径和连接内部。

1. 线上流量基准测试和基准文件（旧 SDK）。
2. 旧会话模型 API 改用本地类型：agent 的 `models`、`session/set_model` 改用第 3 节的模块（旧 SDK）。基准不变。客户端发 `session/set_model` 到第 5 步才改用本地类型，因为 `JsonRpcRequest` 只在新 SDK 里有。
3. `OrcaAcpAgent` 的 trait 实现改成固有方法，supervisor 直接调用（旧 SDK）。
4. 客户端：加 `AgentHandle` 和 `ClientHandler`，三个客户端和 TUI 的调用处改用它们（旧 SDK，内部仍是 `ClientSideConnection`）。
5. 升级到 3.2.0：
   - 改类型路径；
   - 客户端内部换成 builder 加 `Send` 适配层，`AgentHandle` 发 `session/set_model` 改用本地类型；
   - 去掉 `unstable`；
   - `tests/acp_agent.rs` 改用新客户端。

   基准必须不变。
6. 实测、全量测试、CI；如有需要，补文档。

## 7. 风险

- **3.x 很新。** 3.0、3.1、3.2 是连着三天发的，API 可能还会动。用 `=3.2.0` 精确固定；执行时如有补丁版再评估。
- **连接的生命周期。** 3.0 改了排空和关闭：`connect_with` 的闭包返回后连接才关；对端 EOF 会让未完成的请求失败，之后的请求立即失败。这要和 TUI 现在的断线处理（不重发 prompt、权限 fail closed）对上，由 TUI 的断线测试覆盖。
- **请求取消。** 稳定版 schema 里有请求取消；3.2 加了显式取消句柄，CHANGELOG 提到对已发出请求的"自动取消"。要确认：TUI 在超时后丢弃一个请求时，新 SDK 会不会给 daemon 发取消通知；会的话，daemon 要能安静地处理它。必要时用 `detach` 之类的写法保持现在的行为。
- **顺序。** 2.0 起，SDK 给"选择有序消费的响应回调"加了屏障。Orca 用 `block_task()` 等响应，不注册这种回调，应该不受影响。仍要用测试确认 prompt 的响应不会排到它最后一批通知前面。
- **Windows。** attach 和 daemon 的平台差异以 CI 为准。

## 8. 不在本轮

- ACP 草案 v2。
- 新的协议功能：session/delete、fork、elicitation、subagent 更新、notices、LLM providers 等。
- SDK 的 `stdio`、`process`、`schemars` feature。
- agent 侧改用 SDK 的连接；Orca 自己的 JSON-RPC 层保留。
- 去掉旧的会话模型 API。
- 把 TUI 客户端改成 `Send`。

## 附录 A：用到的 SDK 名字（2026-10-09 盘点）

扫描范围：仓库里所有 Rust 文件中的 `use agent_client_protocol::{…}` 和 `agent_client_protocol::Name` 路径。对照的是 0.10.4 / schema 0.11.4 和 3.2.0 / schema 1.10.2 的源码。

- **删掉的：**
  - `ClientSideConnection`、`AgentSideConnection`：被 builder 取代；
  - `ModelInfo`、`SessionModelState`、`SetSessionModelRequest`、`SetSessionModelResponse`：unstable 的会话模型 API，见第 3 节。
- **含义变了的：** `Agent`、`Client`，现在是角色标记类型，不再是 trait。
- **由 unstable 转为稳定、线上格式不变的：**
  - `UsageUpdate`、`Cost`（原 `unstable_session_usage`）；
  - `SessionConfigOptionValue`（原 `unstable_boolean_config`）；
  - `SessionAdditionalDirectoriesCapabilities`（原 `unstable_session_additional_directories`）。
- **其余 88 个：** 稳定类型，只是移到 `schema::v1` 下，或者原本就在 crate 根（`Error`、`ErrorCode`、`Result`）。
