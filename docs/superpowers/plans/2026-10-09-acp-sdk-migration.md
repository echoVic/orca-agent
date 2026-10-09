# ACP SDK 迁移实施计划（子项目 3）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `agent-client-protocol` 从 `=0.10.4`（schema 0.11.4，开 `unstable`）升级到 `=3.2.0`（schema 1.10.2，不开 feature）。ACP v1 线上流量不变；Orca 的行为除"与 spec 不同的地方"第 1、2 条以外不变。

**Architecture:**
- 第一个提交在旧 SDK 上录线上流量基准：11 个场景用原始 JSON-RPC 驱动 daemon 自己的传输层，把 daemon 写出的每一帧去掉主机相关的值，存成 golden 文件。之后每个提交都必须原样通过。
- 接着在旧 SDK 上把 Orca 这边的结构改好，每步一个提交：
  - 任务 2：客户端的 `ClientHandler` 和 `AgentHandle`；
  - 任务 3：`OrcaAcpAgent` 的固有方法；
  - 任务 4：旧会话模型 API 的本地类型。
- 任务 5 换版本：
  - 类型路径；
  - 客户端内部换成 builder 加 `Send` 适配层；
  - `session/list` 目录过滤的解码；
  - `preserve_order` 下四处恢复排序输出；
  - 更新参数探针。
- 任务 6：实测、Linux 复核、文档。

**Tech Stack:** Rust 2024 workspace（`orca-runtime`、`orca-tui`、`orca-provider`、`orca-tools`），tokio current_thread + `LocalSet`，`agent-client-protocol` 3.2.0 / `agent-client-protocol-schema` 1.10.2，serde_json 1.0.151，nextest，Python 探针（`scripts/eval/`），Docker（OrbStack，镜像 `rust:1.99-bookworm`）。

**Spec:** `docs/superpowers/specs/2026-10-09-acp-sdk-migration-design.md`

## 写计划前的验证（2026-10-09）

在 HEAD（1acaf49e）的一次性 worktree 里把任务 1–5 完整做了一遍。计划里的代码块就是跑通的那份。结果：

- **基准：**
  - 在 0.10.4 上录的 11 个 golden，在 3.2.0 上原样通过。
  - 在 Linux Docker（bwrap 不能用、只有 Landlock 的主机）上也通过。
  - macOS 上连跑 5 次、`cargo test` 单进程跑 1 次，都稳定。
- **3.2.0 上的测试：**
  - 换版本后、修 `preserve_order` 之前，`--profile ci` 全量 3957 个测试挂 5 个，全是 JSON 键顺序（见下文第 2 条）。
  - 四处修复后，这 5 个不改期望就通过。任务 1–5 全部做完后，全量 3976 个测试全过。
  - TUI 库测试（serial）1785 个、PTY 合约 33 个全过。
- **探针：**
  - `daemon_probe.py` 全过。
  - `acp_param_probe.py` 在任务 5 更新期望前挂 2 项（见下文第 1 条），更新后全过。
- **验证器：** 4 个仓库验证脚本和 hygiene 测试都通过，清单不用改。
- **之后合入的 PR #125。**
  - 计划写完后，#124 的修复（PR #125）合入 main：plain `--mode=acp` 不再在回答流完后把整段再发一遍，推理改为 `agent_thought_chunk`。
  - 任务 1 在它之后录基准，所以基准记录的是修好后的行为。在合入后的 main 上录过一次，连跑 3 次稳定。
  - PR #125 只改了 `agent.rs` 里 assistant 文本的发送和 `tests/acp_agent.rs`，任务 2–5 的步骤不受影响：
    - 新代码里的 `agent_client_protocol::ContentChunk` 由任务 5 的路径替换处理；
    - 新测试用方法调用，不是 `Agent::…`，任务 3 要改的仍是那 7 处。

## 与 spec 不同的地方（执行前请确认第 1、2 条）

1. **宽松解码（需要你确认）。**
   - schema 1.x 解码时，可选字段用 `DefaultOnError`，列表用 `VecSkipError`：
     - 可选字段类型不对，就当没传；
     - 列表里坏掉的项跳过，不再整条请求报 -32602。
   - schema 0.11 会拒绝这些请求。对 Orca 的影响都是少给，不是多给：
     - `initialize` 的 `clientCapabilities: 42` 当作没有客户端能力。`acp_param_probe.py` 的第 6 项因此失败，连带"hostile input 之后还能 initialize"那项。
     - `session/new`、`session/load` 的 `mcpServers`、`additionalDirectories` 里格式不对的项被跳过，会话照常建。
   - 必填字段类型不对、`params` 不是对象，仍然是 -32602。基准固定了这些。
   - **本计划按"接受"写。** 这是 ACP schema 1.x 自己规定的解码方式，结果都是 fail-safe。
     - 任务 5 把探针那一项换成必填字段类型错误，并加一个单元测试固定新行为；
     - 任务 6 在 `docs/acp-daemon.md` 写明。
   - 另一个选择是在 Orca 里给这些字段补严格校验：代码多，又和协议本身的规定相反，不推荐。
2. **serde_json 的 `preserve_order`（需要你确认）。**
   - schema 从 0.13.7 起（包括所有 1.x）都打开 serde_json 的 `preserve_order`。Cargo feature 合并后，整个 Orca 的 `serde_json::Map` 从按键排序变成保留插入顺序，这是升级绕不开的。
   - 3.2 上全量测试因此挂了 5 个：
     - `jsonl_surface_differential`：JSONL server 的输出要和 v0.2.50 逐字节一致；
     - 3 个 tool schema 测试：发给 DeepSeek 的 schema 的键顺序和 `required` 顺序；
     - `registry` 的 MCP 资源列表：给模型的工具结果。
   - **本计划按"在四处恢复排序"写。** 用 `Value::sort_all_objects`（没有 `preserve_order` 时它什么也不做），改这四处：
     - JSONL server 的事件信封；
     - DeepSeek 的 tool schema；
     - MCP 资源列表和资源模板列表；
     - 工作流的两个摘要 `input_hash`、`digest_value`：旧运行记录里的摘要是按排序算的。
   - 改完后，这 5 个测试不改期望就通过，发给模型的字节也和旧版一样。
   - 其余用 `serde_json::Value` 拼的 JSON（ACP 线上、日志、会话文件等）变成插入顺序。JSON 对象本身无序，按 JSON 解析的读者不受影响。
3. **`session/list` 的 `additionalDirectories`。**
   - schema 1.10.2 的 `ListSessionsRequest` 删掉了这个 unstable 字段。
   - 为了照旧拒绝非空过滤，而不是悄悄列出所有目录的会话，任务 5 在 supervisor 里单独解码它。基准固定了这条错误。
   - 试过 `#[serde(flatten)]` 包一层，1.10.2 的类型不支持（"can only flatten structs and maps"）。
4. **提交顺序。**
   - spec 第 6 节是：2 旧模型 API，3 固有方法，4 客户端。本计划改成：2 客户端，3 固有方法，4 旧模型 API。
     - 客户端先改：旧的"SDK 对 SDK"线上测试要在 `Agent` trait 还在的时候，换成"生产客户端对 daemon 传输层"的测试。
     - 固有方法先于旧模型 API：`set_session_model` 就不会出现固有方法和 trait 方法同名的中间状态。
   - 换版本的提交照旧只剩类型路径和连接内部，外加上面 1–3 条必需的改动。
5. **`tests/acp_agent.rs` 的线上测试。**
   - 它原来用 SDK 的 `AgentSideConnection` 包 `OrcaAcpAgent`。生产环境从不这样用，3.x 也没有这个类型。
   - 任务 2 把它换成 supervisor 测试 `the_attach_client_round_trips_a_prompt_with_the_daemon_connection`：生产客户端 `Connection::connect_streams` 对生产传输层 `run_connection`。
   - 这同样证明"新 SDK 的客户端和 Orca 的 agent 互通"（spec 第 5 节第 2 条）。
6. **3.2 客户端的两个行为（spec 第 7 节的风险，已核实）：**
   - 丢弃一个还没收到响应的请求（超时、abort）时，3.2 会发 `$/cancel_request {"requestId": …}`。daemon 对没有 id 的未知方法不回复，基准固定了这一点；任务 5 的客户端测试固定 3.2 会发它。
   - `connect_with` 不会因为对端 EOF 自己结束。主函数要等 `incoming_closed()`；对端 EOF 时，未完成的请求先失败。任务 2 的断线测试在新旧 SDK 上都要过。

## Global Constraints

- **依赖：**
  - 根 `Cargo.toml` 写 `agent-client-protocol = "=3.2.0"`，不开任何 feature。
  - 执行时若已有 3.2.x 补丁版，改用补丁版，并重看它的 CHANGELOG。
- **类型路径：**
  - 消息类型从 `agent_client_protocol::schema::v1` 引入；
  - `ProtocolVersion` 从 `agent_client_protocol::schema` 引入；
  - `Error`、`ErrorCode`、`Result` 仍在 crate 根。
- **线上格式：**
  - 任务 1 的基准在之后每个任务结束时都要原样通过。
  - 只有在提交说明里写清原因时才能重新生成 golden；本计划没有任何一步需要重新生成。
- **范围：**
  - 不加新的协议功能；
  - agent 侧保留 Orca 自己的 JSON-RPC 层；
  - TUI 客户端不改成 `Send`；
  - 不去掉旧会话模型 API。
- **平台与编译：**
  - 所有 crate 开着 `#![deny(deprecated)]`。代码要同时能用本机的 Rust 1.95 和 CI 的最新 stable 编译。
  - Windows 只在 CI 上编译；只能在 unix 上用的代码加 `#[cfg(unix)]`。
- **环境：** 所有 `cargo nextest`、`cargo test`、`node` 命令加 `env -u NODE_OPTIONS` 前缀。
- **zsh：** 在 zsh 里，`$files` 这样的变量不会按空格拆开，文件列表直接写在命令里。
- **测试：**
  - 用 `cargo nextest run`；
  - PTY 合约测试用 `--profile ci-serial`；
  - 多个过滤词写成 `-E 'test(/a|b/)'`。
- **每次提交前：**
  - 运行 `cargo fmt --all -- --check`；
  - 直接运行（不接 `tail`）`env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs` 和 `env -u NODE_OPTIONS node scripts/validate-windows-platform-boundaries.mjs`。
  - 验证器因为固定的路径、行号或计数失败时，在同一个提交里更新清单或固定值，并在提交说明里写明。
- **提交：**
  - 在本地 `main` 上提交，每个任务一个，用 conventional 风格；
  - 正文写原因和验证；
  - 不推送。
- **文案：** 代码和面向用户的输出用英文。

## Review Focus

1. **daemon 在一轮对话中途断开。**
   - 客户端要让这轮的 prompt 以错误结束，并报告连接已关闭，TUI 才会提示"未确认"并重连，且不重发 prompt。
   - 由任务 2 的 `a_daemon_hangup_fails_the_pending_prompt_and_closes_the_connection`（新旧 SDK 都要过）和 TUI 已有的 `wire_permissions_commit_only_after_peer_acceptance_and_prompt_result` 钉住。
2. **紧挨着 prompt 响应前到达的更新。**
   - TUI 看到这轮结束之前，这些更新必须已经处理完；否则收尾时会漏掉最后的文本或终态。
   - 由任务 5 的 `updates_sent_before_a_response_are_handled_before_it_returns` 钉住：处理函数故意变慢；去掉屏障时这个测试会失败，已验证。
3. **按 Esc 或切换模型时请求被丢弃。**
   - 3.2 会给 daemon 发 `$/cancel_request`。daemon 必须不回复、继续服务。
   - 由基准 `stdio_session_lifecycle`（发 `$/cancel_request` 后没有任何回复）和任务 5 的 `a_request_dropped_while_outstanding_is_cancelled_on_the_wire` 钉住。
4. **升级前录的工作流运行，升级后恢复。**
   - 恢复时要命中已缓存的 agent 调用，不能因为键顺序变了而重跑。
   - 由任务 5 的 `input_hash_ignores_the_order_of_option_keys` 钉住：其中固定了按排序算出的摘要值；去掉排序时这个测试会失败，已验证。
5. **旧版 Orca TUI 或只认旧 API 的编辑器连到新 daemon。**
   - `session/new`、`session/load` 仍要带 `models`，`session/set_model` 仍可用。
   - 由基准 `daemon_session_settings` 和任务 4 的 `legacy_model` 测试钉住。

---
### Task 1：线上流量基准（旧 SDK）

**Files:**
- Create: `crates/orca-runtime/src/acp/supervisor/tests/wire_baseline.rs`
- Modify: `crates/orca-runtime/src/acp/supervisor.rs`：在 `mod tests` 里声明这个子模块。
- Create（测试生成）：`crates/orca-runtime/src/golden/acp_wire/*.json`，共 11 个。

**Interfaces:**
- Consumes：
  - 都是 `supervisor.rs` 里 `mod tests` 已有的：`test_config(cwd: PathBuf) -> RunConfig`、`TEST_TIMEOUT`；
  - 执行器 `CompleteWithMessageExecutor`、`WaitForCancelExecutor`、`ReadTextFileExecutor { content_tx }`、`WriteTextFileExecutor { outcome_tx }`、`TerminalObserveExecutor { outcome_tx }`、`StandardInteractionExecutor { behaviors, outcome_tx }`；
  - `run_connection`、`run_shared_connection`、`crate::acp::shared::SharedSessions`、`RuntimeHost::start()`、`RuntimeHost::start_with_executor(..)`。
- Produces：
  - 11 个测试，名字都以 `wire_baseline_` 开头；
  - 11 个 golden 文件。
  - 之后每个任务都用这条命令确认基准不变：`env -u NODE_OPTIONS cargo nextest run -p orca-runtime --lib -E 'test(/wire_baseline/)'`。

**基准怎么比：**
- 每个场景记下 daemon 写给每个连接的全部帧，分三类：
  - 响应：按到达顺序；
  - daemon 发给客户端的请求：按到达顺序；
  - 通知：排序后比较，因为它们由不同的任务写出。
- 比较前做两件事：
  - 屏蔽主机相关的值：工作目录换成 `<cwd>`，临时目录换成 `<tmp>`，UUID 换成 `<uuid>`，RFC 3339 时间换成 `<time>`，`*_unix_ms` 数字换成 `<unix-ms>`，去掉 `_meta` 里的 `orca.dev/readiness`（沙箱提示随主机变化，是 Orca 自己的元数据）；
  - 所有对象的键排好序，这样 golden 文件不受 serde_json 的 map 顺序影响。
- `session/list` 的结果只保留本场景目录下的会话，并去掉翻页游标：同一进程里别的测试也会往测试 home 里存会话。
- 整个模块只在 unix 上编译。Windows 上的路径和沙箱提示不同，而线上格式本身和平台无关；Windows CI 照常跑其余的 ACP 测试。

- [ ] **Step 1：写基准测试模块**

新建 `crates/orca-runtime/src/acp/supervisor/tests/wire_baseline.rs`，内容如下：

```rust
//! The ACP wire baseline for the SDK migration (sub-project 3).
//!
//! Each scenario drives the daemon's own transport with literal JSON-RPC
//! frames, so changing the ACP SDK cannot change the driver. Everything the
//! daemon writes is compared with `src/golden/acp_wire/<scenario>.json` once
//! host-dependent values are masked. Responses and the daemon's requests to
//! the client keep their order; notifications compare as a sorted list,
//! because separate tasks write them. With `ORCA_UPDATE_GOLDEN` set the files
//! are rewritten instead; review the diff like any other change.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

use super::*;

fn client_info() -> Value {
    json!({"name": "wire-baseline", "version": "0.0.0"})
}

fn run(scenario: impl Future<Output = ()>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&runtime, scenario);
}

/// A daemon (shared sessions) or stdio (one connection) ACP endpoint.
struct Endpoint {
    host: RuntimeHost,
    config: RunConfig,
    shared: Option<crate::acp::shared::SharedSessions>,
    connections: Vec<tokio::task::JoinHandle<Result<(), RpcFacadeError>>>,
}

impl Endpoint {
    fn stdio(host: RuntimeHost, cwd: &Path) -> Self {
        Self {
            host,
            config: test_config(cwd.to_path_buf()),
            shared: None,
            connections: Vec::new(),
        }
    }

    fn daemon(host: RuntimeHost, cwd: &Path) -> Self {
        Self {
            shared: Some(crate::acp::shared::SharedSessions::default()),
            ..Self::stdio(host, cwd)
        }
    }

    fn connect(&mut self) -> WirePeer {
        let (client, server) = tokio::io::duplex(1 << 20);
        let (client_read, client_write) = tokio::io::split(client);
        let (server_read, server_write) = tokio::io::split(server);
        let surface = self.host.surface_handle();
        let config = self.config.clone();
        self.connections.push(match &self.shared {
            Some(shared) => tokio::task::spawn_local(run_shared_connection(
                surface,
                config,
                server_read,
                server_write,
                shared.clone(),
            )),
            None => {
                tokio::task::spawn_local(run_connection(surface, config, server_read, server_write))
            }
        });
        WirePeer {
            write: client_write,
            read: BufReader::new(client_read),
            frames: Vec::new(),
            methods: HashMap::new(),
        }
    }

    async fn shut_down(self) {
        for connection in self.connections {
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
        }
        self.host.shutdown().unwrap();
    }
}

/// One client connection; records every frame the daemon writes to it.
struct WirePeer {
    write: WriteHalf<DuplexStream>,
    read: BufReader<ReadHalf<DuplexStream>>,
    frames: Vec<Value>,
    /// Method of each request this peer sent, by its id.
    methods: HashMap<String, String>,
}

impl WirePeer {
    async fn send(&mut self, frame: Value) {
        let mut bytes = serde_json::to_vec(&frame).unwrap();
        bytes.push(b'\n');
        self.write.write_all(&bytes).await.unwrap();
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    async fn start(&mut self, id: Value, method: &str, params: Value) {
        self.methods.insert(id.to_string(), method.to_string());
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
    }

    /// Sends a request and reads until its response.
    async fn request(&mut self, id: Value, method: &str, params: Value) -> Value {
        self.start(id.clone(), method, params).await;
        self.response(&id).await
    }

    async fn response(&mut self, id: &Value) -> Value {
        self.until(|frame| frame.get("method").is_none() && frame["id"] == *id)
            .await
    }

    async fn reply(&mut self, request: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": request["id"], "result": result}))
            .await;
    }

    async fn until(&mut self, done: impl Fn(&Value) -> bool) -> Value {
        loop {
            let frame = self
                .next()
                .await
                .expect("daemon closed the connection early");
            if done(&frame) {
                return frame;
            }
        }
    }

    /// Like `until`, but a frame that already arrived counts.
    async fn seen(&mut self, done: impl Fn(&Value) -> bool) {
        if !self.frames.iter().any(&done) {
            self.until(done).await;
        }
    }

    async fn next(&mut self) -> Option<Value> {
        let mut line = String::new();
        let read = tokio::time::timeout(TEST_TIMEOUT, self.read.read_line(&mut line))
            .await
            .expect("ACP frame timeout")
            .unwrap();
        if read == 0 {
            return None;
        }
        let frame: Value = serde_json::from_str(&line).unwrap();
        self.frames.push(frame.clone());
        Some(frame)
    }

    /// Hangs up, then records whatever the daemon still writes.
    async fn hang_up(&mut self) {
        self.write.shutdown().await.unwrap();
        while self.next().await.is_some() {}
    }

    /// Answers the daemon's requests to the client until `prompt_id`'s response.
    async fn serve_client(&mut self, prompt_id: &Value, permission: &str) -> Value {
        loop {
            let frame = self
                .until(|frame| {
                    frame.get("id").is_some()
                        && (frame.get("method").is_some() || frame["id"] == *prompt_id)
                })
                .await;
            let Some(method) = frame["method"].as_str() else {
                return frame;
            };
            let result = match method {
                "fs/read_text_file" => json!({"content": "second line\nthird line\n"}),
                "fs/write_text_file" => json!({}),
                "terminal/create" => json!({"terminalId": "terminal-1"}),
                "terminal/output" => json!({
                    "output": "hello",
                    "truncated": false,
                    "exitStatus": {"exitCode": 0, "signal": null},
                }),
                "terminal/wait_for_exit" => json!({"exitCode": 0, "signal": null}),
                "terminal/kill" | "terminal/release" => json!({}),
                "session/request_permission" => {
                    let option = frame["params"]["options"]
                        .as_array()
                        .and_then(|options| {
                            options.iter().find(|option| option["kind"] == permission)
                        })
                        .unwrap_or_else(|| panic!("no {permission} option in {frame}"));
                    json!({"outcome": {"outcome": "selected", "optionId": option["optionId"]}})
                }
                other => panic!("unexpected daemon request {other}: {frame}"),
            };
            self.reply(&frame, result).await;
        }
    }
}

async fn initialize(peer: &mut WirePeer, id: i64, capabilities: Value) -> Value {
    peer.request(
        json!(id),
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": capabilities,
            "clientInfo": client_info(),
        }),
    )
    .await
}

async fn new_session(peer: &mut WirePeer, id: i64, cwd: &Path) -> String {
    let response = peer
        .request(
            json!(id),
            "session/new",
            json!({"cwd": cwd, "mcpServers": []}),
        )
        .await;
    response["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new failed: {response}"))
        .to_string()
}

fn prompt(session: &str, text: &str) -> Value {
    json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]})
}

/// Replaces values that depend on the host or the run, and sorts object keys
/// so the golden files read the same whichever map order serde_json uses.
struct Mask {
    roots: Vec<(String, &'static str)>,
}

impl Mask {
    fn new(cwd: &Path) -> Self {
        let temp = std::env::temp_dir();
        let mut roots = vec![
            (cwd.display().to_string(), "<cwd>"),
            (temp.display().to_string(), "<tmp>"),
        ];
        if let Ok(canonical) = temp.canonicalize() {
            roots.push((canonical.display().to_string(), "<tmp>"));
        }
        for (root, _) in &mut roots {
            while root.len() > 1 && root.ends_with(std::path::MAIN_SEPARATOR) {
                root.pop();
            }
        }
        // Longer roots first: the cwd lives inside the temp directory.
        roots.sort_by_key(|(root, _)| std::cmp::Reverse(root.len()));
        Self { roots }
    }

    fn value(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.text(text)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.value(item)).collect())
            }
            Value::Object(entries) => {
                let mut entries = entries.clone();
                // Readiness warnings depend on the host's sandbox, and they are
                // Orca's own metadata rather than anything the SDK shapes.
                if let Some(Value::Object(meta)) = entries.get_mut("_meta") {
                    meta.remove("orca.dev/readiness");
                    if meta.is_empty() {
                        entries.remove("_meta");
                    }
                }
                let mut keys = entries.keys().collect::<Vec<_>>();
                keys.sort();
                let mut sorted = Map::new();
                for key in keys {
                    let value = if key.ends_with("_unix_ms") && entries[key].is_number() {
                        Value::from("<unix-ms>")
                    } else {
                        self.value(&entries[key])
                    };
                    sorted.insert(key.clone(), value);
                }
                Value::Object(sorted)
            }
            other => other.clone(),
        }
    }

    fn text(&self, text: &str) -> String {
        let mut text = text.to_string();
        let mut rooted = false;
        for (root, token) in &self.roots {
            if text.contains(root.as_str()) {
                text = text.replace(root.as_str(), token);
                rooted = true;
            }
        }
        if rooted {
            text = text.replace('\\', "/");
        }
        mask_ids_and_times(&text)
    }
}

/// UUIDs become `<uuid>`; RFC 3339 timestamps become `<time>`.
fn mask_ids_and_times(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        if let Some(length) = uuid_at(&bytes[index..]) {
            output.push_str("<uuid>");
            index += length;
        } else if let Some(length) = timestamp_at(&bytes[index..]) {
            output.push_str("<time>");
            index += length;
        } else {
            let next = text[index..].chars().next().unwrap();
            output.push(next);
            index += next.len_utf8();
        }
    }
    output
}

fn uuid_at(bytes: &[u8]) -> Option<usize> {
    const SHAPE: &[u8; 36] = b"xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx";
    let candidate = bytes.get(..SHAPE.len())?;
    candidate
        .iter()
        .zip(SHAPE)
        .all(|(byte, shape)| match shape {
            b'-' => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
        .then_some(SHAPE.len())
}

/// `YYYY-MM-DDTHH:MM:SS`, an optional fraction, then an optional `Z` or `±HH:MM`.
fn timestamp_at(bytes: &[u8]) -> Option<usize> {
    const SHAPE: &[u8; 19] = b"dddd-dd-ddTdd:dd:dd";
    let candidate = bytes.get(..SHAPE.len())?;
    let matches = candidate
        .iter()
        .zip(SHAPE)
        .all(|(byte, shape)| match shape {
            b'd' => byte.is_ascii_digit(),
            _ => byte == shape,
        });
    if !matches {
        return None;
    }
    let mut length = SHAPE.len();
    if bytes.get(length) == Some(&b'.') {
        length += 1;
        while bytes.get(length).is_some_and(u8::is_ascii_digit) {
            length += 1;
        }
    }
    match bytes.get(length..) {
        Some([b'Z', ..]) => length += 1,
        Some([b'+' | b'-', h1, h2, b':', m1, m2, ..])
            if [h1, h2, m1, m2].iter().all(|digit| digit.is_ascii_digit()) =>
        {
            length += 6
        }
        _ => {}
    }
    Some(length)
}

/// What each peer received, masked. Other tests in the same process can save
/// sessions in the shared test home, so a `session/list` result keeps only
/// this scenario's directory and drops the page cursor that depends on them.
fn transcript(peers: &[&WirePeer], cwd: &Path) -> Value {
    let mask = Mask::new(cwd);
    Value::Array(
        peers
            .iter()
            .map(|peer| {
                let mut responses = Vec::new();
                let mut requests = Vec::new();
                let mut notifications = Vec::new();
                for frame in &peer.frames {
                    let mut frame = mask.value(frame);
                    match (frame.get("method").is_some(), frame.get("id").is_some()) {
                        (false, _) => {
                            let method = peer.methods.get(&frame["id"].to_string());
                            if method.is_some_and(|method| method == "session/list")
                                && let Some(result) = frame["result"].as_object_mut()
                            {
                                result.remove("nextCursor");
                                if let Some(sessions) =
                                    result.get_mut("sessions").and_then(Value::as_array_mut)
                                {
                                    sessions.retain(|session| session["cwd"] == "<cwd>");
                                }
                            }
                            responses.push(frame);
                        }
                        (true, true) => requests.push(frame),
                        (true, false) => notifications.push(frame),
                    }
                }
                notifications.sort_by_key(Value::to_string);
                // Keys in sorted order, so the file reads the same whichever map
                // order serde_json uses.
                json!({
                    "clientRequests": requests,
                    "notifications": notifications,
                    "responses": responses,
                })
            })
            .collect(),
    )
}

fn assert_wire_golden(name: &str, actual: Value) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/golden/acp_wire")
        .join(format!("{name}.json"));
    let text = format!("{}\n", serde_json::to_string_pretty(&actual).unwrap());
    if std::env::var_os("ORCA_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).expect("write wire golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "wire golden `{name}` is missing at {}; rerun with ORCA_UPDATE_GOLDEN=1 set \
             (e.g. `ORCA_UPDATE_GOLDEN=1 cargo nextest run -p orca-runtime --lib -E \
             'test(/wire_baseline/)'`) to generate it",
            path.display()
        )
    });
    let expected: Value = serde_json::from_str(&expected).expect("wire golden is JSON");
    assert!(
        expected == actual,
        "wire golden `{name}` differs from what the daemon sent; review and rerun with \
         ORCA_UPDATE_GOLDEN=1 to accept. The daemon sent:\n{text}"
    );
}

fn workspace() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().canonicalize().unwrap();
    (directory, cwd)
}

#[test]
fn wire_baseline_stdio_session_lifecycle() {
    run(async {
        let (_directory, cwd) = workspace();
        let host = RuntimeHost::start_with_executor(Arc::new(CompleteWithMessageExecutor)).unwrap();
        let mut endpoint = Endpoint::stdio(host, &cwd);
        let mut peer = endpoint.connect();
        initialize(
            &mut peer,
            1,
            json!({"fs": {"readTextFile": true, "writeTextFile": true}, "terminal": true}),
        )
        .await;
        peer.request(json!(2), "authenticate", json!({"methodId": "none"}))
            .await;
        let session = new_session(&mut peer, 3, &cwd).await;
        peer.request(json!(4), "session/list", json!({"cwd": cwd}))
            .await;
        // A string id, then a cancel for it after its response: nothing answers a notification.
        peer.request(
            json!("prompt-1"),
            "session/prompt",
            prompt(&session, "complete"),
        )
        .await;
        peer.notify("$/cancel_request", json!({"requestId": "prompt-1"}))
            .await;
        peer.request(json!(5), "orca.dev/unknown", json!({})).await;
        initialize(&mut peer, 11, json!({})).await;
        peer.request(
            json!(6),
            "session/set_mode",
            json!({"sessionId": session, "modeId": "plan"}),
        )
        .await;
        peer.request(
            json!(7),
            "session/load",
            json!({"sessionId": session, "cwd": cwd, "mcpServers": []}),
        )
        .await;
        let queue = peer
            .request(
                json!(8),
                "orca.dev/session/queue/list",
                json!({"sessionId": session}),
            )
            .await;
        let revision = queue["result"]["revision"].clone();
        let paused = peer
            .request(
                json!(9),
                "orca.dev/session/queue/pause",
                json!({"sessionId": session, "expectedRevision": revision}),
            )
            .await;
        peer.request(
            json!(10),
            "orca.dev/session/queue/add",
            json!({
                "sessionId": session,
                "expectedRevision": paused["result"]["revision"],
                "input": "queued later",
            }),
        )
        .await;
        peer.hang_up().await;
        endpoint.shut_down().await;
        assert_wire_golden("stdio_session_lifecycle", transcript(&[&peer], &cwd));
    });
}

#[test]
fn wire_baseline_stdio_malformed_params() {
    run(async {
        let (_directory, cwd) = workspace();
        let host = RuntimeHost::start_with_executor(Arc::new(CompleteWithMessageExecutor)).unwrap();
        let mut endpoint = Endpoint::stdio(host, &cwd);
        let mut peer = endpoint.connect();
        peer.request(json!(1), "initialize", json!("not-an-object"))
            .await;
        peer.request(
            json!(2),
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}, "clientInfo": client_info()}),
        )
        .await;
        peer.request(json!(3), "authenticate", json!("not-an-object"))
            .await;
        peer.request(json!(4), "authenticate", json!({"methodId": 42}))
            .await;
        peer.request(json!(5), "session/new", json!("not-an-object"))
            .await;
        peer.request(
            json!(6),
            "session/new",
            json!({"cwd": 42, "mcpServers": []}),
        )
        .await;
        peer.request(json!(7), "session/load", json!("not-an-object"))
            .await;
        peer.request(
            json!(8),
            "session/load",
            json!({"sessionId": 42, "cwd": cwd, "mcpServers": []}),
        )
        .await;
        let session = new_session(&mut peer, 9, &cwd).await;
        peer.request(json!(10), "session/prompt", json!("not-an-object"))
            .await;
        peer.request(json!(11), "session/prompt", json!({"sessionId": session}))
            .await;
        peer.request(json!(12), "session/set_mode", json!({})).await;
        peer.request(
            json!(13),
            "session/set_config_option",
            json!({"sessionId": 42}),
        )
        .await;
        peer.request(
            json!(14),
            "session/set_config_option",
            json!({"sessionId": session, "configId": "reasoning", "value": 5}),
        )
        .await;
        peer.request(json!(15), "session/set_model", json!({}))
            .await;
        peer.request(json!(16), "session/list", json!({"cursor": 5}))
            .await;
        peer.request(
            json!(17),
            "session/list",
            json!({"cwd": cwd, "cursor": "x"}),
        )
        .await;
        peer.request(
            json!(18),
            "session/list",
            json!({"cwd": cwd, "additionalDirectories": [cwd]}),
        )
        .await;
        peer.request(
            json!(19),
            "orca.dev/session/queue/list",
            json!("not-an-object"),
        )
        .await;
        peer.request(
            json!(20),
            "orca.dev/session/queue/pause",
            json!({"sessionId": session}),
        )
        .await;
        peer.hang_up().await;
        endpoint.shut_down().await;
        assert_wire_golden("stdio_malformed_params", transcript(&[&peer], &cwd));
    });
}

#[test]
fn wire_baseline_stdio_cancel() {
    run(async {
        let (_directory, cwd) = workspace();
        let host = RuntimeHost::start_with_executor(Arc::new(WaitForCancelExecutor)).unwrap();
        let mut endpoint = Endpoint::stdio(host, &cwd);
        let mut peer = endpoint.connect();
        initialize(&mut peer, 1, json!({})).await;
        let session = new_session(&mut peer, 2, &cwd).await;
        peer.start(json!(3), "session/prompt", prompt(&session, "wait"))
            .await;
        peer.notify("session/cancel", json!({"sessionId": session}))
            .await;
        peer.response(&json!(3)).await;
        peer.hang_up().await;
        endpoint.shut_down().await;
        assert_wire_golden("stdio_cancel", transcript(&[&peer], &cwd));
    });
}

/// Runs one prompt whose executor calls back into the client, answering
/// every daemon request with a canned result.
async fn client_capability_scenario(name: &str, host: RuntimeHost, permission: &str) {
    let (_directory, cwd) = workspace();
    let mut endpoint = Endpoint::stdio(host, &cwd);
    let mut peer = endpoint.connect();
    initialize(
        &mut peer,
        1,
        json!({"fs": {"readTextFile": true, "writeTextFile": true}, "terminal": true}),
    )
    .await;
    let session = new_session(&mut peer, 2, &cwd).await;
    peer.start(json!(3), "session/prompt", prompt(&session, "go"))
        .await;
    peer.serve_client(&json!(3), permission).await;
    peer.hang_up().await;
    endpoint.shut_down().await;
    assert_wire_golden(name, transcript(&[&peer], &cwd));
}

#[test]
fn wire_baseline_stdio_reads_a_file_through_the_client() {
    run(async {
        let (content_tx, _content_rx) = std::sync::mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(ReadTextFileExecutor { content_tx }))
            .unwrap();
        client_capability_scenario("stdio_read_text_file", host, "allow_once").await;
    });
}

#[test]
fn wire_baseline_stdio_writes_a_file_through_the_client() {
    run(async {
        let (outcome_tx, _outcome_rx) = std::sync::mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(WriteTextFileExecutor { outcome_tx }))
            .unwrap();
        client_capability_scenario("stdio_write_text_file", host, "allow_once").await;
    });
}

#[test]
fn wire_baseline_stdio_runs_a_terminal_through_the_client() {
    run(async {
        let (outcome_tx, _outcome_rx) = std::sync::mpsc::sync_channel(1);
        let host =
            RuntimeHost::start_with_executor(Arc::new(TerminalObserveExecutor { outcome_tx }))
                .unwrap();
        client_capability_scenario("stdio_terminal", host, "allow_once").await;
    });
}

#[test]
fn wire_baseline_stdio_asks_the_client_for_permission() {
    run(async {
        let (outcome_tx, _outcome_rx) = std::sync::mpsc::sync_channel(1);
        let host = RuntimeHost::start_with_executor(Arc::new(StandardInteractionExecutor {
            behaviors: Mutex::new(vec![StandardInteractionBehavior::ToolApproval]),
            outcome_tx,
        }))
        .unwrap();
        client_capability_scenario("stdio_permission", host, "allow_once").await;
    });
}

#[test]
fn wire_baseline_stdio_prompt_with_tools() {
    run(async {
        let (_directory, cwd) = workspace();
        let mut endpoint = Endpoint::stdio(RuntimeHost::start().unwrap(), &cwd);
        let mut peer = endpoint.connect();
        initialize(&mut peer, 1, json!({})).await;
        let session = new_session(&mut peer, 2, &cwd).await;
        peer.request(
            json!(3),
            "session/prompt",
            prompt(&session, "plan Inspect references"),
        )
        .await;
        peer.hang_up().await;
        endpoint.shut_down().await;
        assert_wire_golden("stdio_prompt_with_tools", transcript(&[&peer], &cwd));
    });
}

#[test]
fn wire_baseline_daemon_session_settings() {
    run(async {
        let (_directory, cwd) = workspace();
        let host = RuntimeHost::start_with_executor(Arc::new(CompleteWithMessageExecutor)).unwrap();
        let mut endpoint = Endpoint::daemon(host, &cwd);
        let mut owner = endpoint.connect();
        initialize(&mut owner, 1, json!({})).await;
        let session = new_session(&mut owner, 2, &cwd).await;
        owner
            .request(
                json!(3),
                "session/set_model",
                json!({"sessionId": session, "modelId": "deepseek-v4-pro"}),
            )
            .await;
        owner
            .request(
                json!(4),
                "session/set_mode",
                json!({"sessionId": session, "modeId": "plan"}),
            )
            .await;
        owner
            .request(
                json!(5),
                "session/set_config_option",
                json!({"sessionId": session, "configId": "reasoning", "value": "high"}),
            )
            .await;
        owner
            .request(
                json!(6),
                "session/set_config_option",
                json!({"sessionId": session, "configId": "model", "value": " padded "}),
            )
            .await;
        owner
            .request(
                json!(7),
                "session/set_model",
                json!({"sessionId": session, "modelId": " padded "}),
            )
            .await;
        owner
            .request(json!(8), "session/list", json!({"cwd": cwd}))
            .await;
        let mut second = endpoint.connect();
        initialize(&mut second, 1, json!({})).await;
        second
            .request(
                json!(2),
                "session/load",
                json!({"sessionId": session, "cwd": cwd, "mcpServers": []}),
            )
            .await;
        owner.hang_up().await;
        second.hang_up().await;
        endpoint.shut_down().await;
        assert_wire_golden(
            "daemon_session_settings",
            transcript(&[&owner, &second], &cwd),
        );
    });
}

#[test]
fn wire_baseline_daemon_prompt_projection() {
    run(async {
        let (_directory, cwd) = workspace();
        let mut endpoint = Endpoint::daemon(RuntimeHost::start().unwrap(), &cwd);
        let mut peer = endpoint.connect();
        initialize(
            &mut peer,
            1,
            json!({"_meta": {"orca.dev/projection": {"version": 1}}}),
        )
        .await;
        let session = new_session(&mut peer, 2, &cwd).await;
        peer.seen(|frame| frame["params"]["_meta"]["orca.dev/projection"]["phase"] == "ready")
            .await;
        peer.request(json!(3), "session/prompt", prompt(&session, "mock_usage"))
            .await;
        peer.request(
            json!(4),
            "session/prompt",
            prompt(&session, "plan Inspect references"),
        )
        .await;
        peer.hang_up().await;
        endpoint.shut_down().await;
        assert_wire_golden("daemon_prompt_projection", transcript(&[&peer], &cwd));
    });
}

#[test]
fn wire_baseline_daemon_declined_permission_is_a_refusal() {
    run(async {
        let (_directory, cwd) = workspace();
        let mut endpoint = Endpoint::daemon(RuntimeHost::start().unwrap(), &cwd);
        let mut peer = endpoint.connect();
        initialize(&mut peer, 1, json!({})).await;
        let session = new_session(&mut peer, 2, &cwd).await;
        // Full-auto would grant the permission without asking the client.
        peer.request(
            json!(3),
            "session/set_mode",
            json!({"sessionId": session, "modeId": "suggest"}),
        )
        .await;
        let extra = cwd.join("extra");
        peer.start(
            json!(4),
            "session/prompt",
            prompt(
                &session,
                &format!(
                    "request_permissions_then_bash {} :: printf hi",
                    extra.display()
                ),
            ),
        )
        .await;
        peer.serve_client(&json!(4), "reject_once").await;
        peer.hang_up().await;
        endpoint.shut_down().await;
        assert_wire_golden("daemon_declined_permission", transcript(&[&peer], &cwd));
    });
}
```

- [ ] **Step 2：在 supervisor 的测试模块里声明它**

在 `crates/orca-runtime/src/acp/supervisor.rs` 的 `mod tests` 里，找到

```rust
    use crate::runtime_permission::RuntimePermissionRequest;
    use crate::thread::RuntimeThread;
```

在其后加：

```rust

    // Host paths and sandbox warnings differ on Windows; the wire shapes do not.
    #[cfg(unix)]
    mod wire_baseline;
```

因为 `tests` 是 `supervisor.rs` 里的内联模块，子模块文件放在 `src/acp/supervisor/tests/` 下。子模块用 `use super::*;` 就能拿到 `tests` 里的执行器和工具函数。

- [ ] **Step 3：运行，确认因为没有 golden 而失败**

Run：`env -u NODE_OPTIONS cargo nextest run -p orca-runtime --lib -E 'test(/wire_baseline/)'`

Expected：11 个 FAIL，每个都报 ``wire golden `<name>` is missing at ...; rerun with ORCA_UPDATE_GOLDEN=1 set``。

- [ ] **Step 4：生成 golden**

Run：`ORCA_UPDATE_GOLDEN=1 env -u NODE_OPTIONS cargo nextest run -p orca-runtime --lib -E 'test(/wire_baseline/)'`

Expected：11 个 PASS。`crates/orca-runtime/src/golden/acp_wire/` 下有 11 个文件：
- `daemon_declined_permission.json`、`daemon_prompt_projection.json`、`daemon_session_settings.json`；
- `stdio_cancel.json`、`stdio_malformed_params.json`、`stdio_permission.json`、`stdio_prompt_with_tools.json`、`stdio_read_text_file.json`、`stdio_session_lifecycle.json`、`stdio_terminal.json`、`stdio_write_text_file.json`。

- [ ] **Step 5：审一遍 golden**

先打印摘要：

```sh
python3 - <<'PY'
import glob, json
for path in sorted(glob.glob("crates/orca-runtime/src/golden/acp_wire/*.json")):
    for index, peer in enumerate(json.load(open(path))):
        print(f"== {path.rsplit('/', 1)[1]} peer{index}")
        for frame in peer["responses"]:
            error = frame.get("error")
            result = frame.get("result")
            if error:
                detail = f"ERR {error['code']} {str(error.get('data'))[:80]}"
            elif isinstance(result, dict):
                detail = "OK " + ",".join(sorted(result))
            else:
                detail = f"OK {result}"
            print(f"   response {frame['id']}: {detail}")
        for frame in peer["clientRequests"]:
            print(f"   daemon request {frame['id']}: {frame['method']}")
        kinds = {}
        for frame in peer["notifications"]:
            kind = frame["params"].get("update", {}).get("sessionUpdate", frame["method"])
            kinds[kind] = kinds.get(kind, 0) + 1
        print("   notifications:", kinds)
PY
grep -rlE '/Users/|/private/|/var/folders|/home/|/tmp/' crates/orca-runtime/src/golden/acp_wire || echo "no host paths"
```

逐条核对以下内容（这是 0.10.4 上录到的结果）：
- `stdio_session_lifecycle`：
  - 12 个响应。字符串 id `"prompt-1"` 原样回显，`stopReason` 为 `end_turn`。
  - `$/cancel_request` 没有任何回复。
  - `orca.dev/unknown` 回 -32601，`data` 为 `unsupported ACP method 'orca.dev/unknown'`。
  - 第二次 `initialize`（id 11）回 -32600。
  - 非 daemon 模式下 `session/set_mode` 回 -32601。
  - 队列的 list、pause、add 回快照，`revision` 依次为 0、1、2。
- `stdio_malformed_params`：
  - 除 id 2 和 id 9 外都是 -32602。
  - 第 18 条的 `data` 是 `additional directory filtering is unsupported`。
- `stdio_terminal`：daemon 依次发了 `terminal/create`、`terminal/output`、`terminal/wait_for_exit`、`terminal/kill`、`terminal/release`，id 是 -1 到 -5。
- `stdio_read_text_file`、`stdio_write_text_file`：各有一个 `fs/*` 请求，id 为 -1。
- `stdio_permission`：一个 `session/request_permission`，id 为 0。
- `stdio_cancel`：prompt 的 `stopReason` 是 `cancelled`。
- `stdio_prompt_with_tools`：有 `tool_call`、`tool_call_update`，一条 `agent_message_chunk`（`Mock completed after tool execution.`）和两条 `agent_thought_chunk`（推理）。回答只发一遍，推理不混进回答（#124）。
- `daemon_session_settings`：
  - 第一个连接：`session/new` 的结果有 `configOptions`、`models`、`modes`、`sessionId`；`session/set_model` 回 `{}`；首尾带空格的模型名（id 6、7）回 -32602；`session/list` 列出 1 个会话。
  - 第二个连接：`session/load` 的结果有 `configOptions`、`models`、`modes`。
- `daemon_prompt_projection`：
  - 通知里有 `usage_update`、`agent_thought_chunk`、`plan`、`tool_call`、`tool_call_update`，以及带 `orca.dev/projection` 元数据的 `session_info_update`。
  - `usage_update` 的 `used` 是 120。
- `daemon_declined_permission`：
  - daemon 发了一个 `session/request_permission`；
  - 拒绝后 prompt 的 `stopReason` 是 `refusal`（#76）。
- 文件里没有主机路径，`grep` 打印 `no host paths`。

有不符的地方，先查清原因再继续，不要改 golden 去迁就。

- [ ] **Step 6：确认基准稳定、和主机无关**

```sh
for run in 1 2 3 4 5; do
  env -u NODE_OPTIONS cargo nextest run -p orca-runtime --lib -E 'test(/wire_baseline/)' 2>&1 | grep -E 'Summary|FAIL'
done
env -u NODE_OPTIONS cargo test -p orca-runtime --lib wire_baseline 2>&1 | grep 'test result'
```

Expected：
- 5 次都是 `11 tests run: 11 passed`，没有 FAIL；
- `cargo test` 打印 `test result: ok. 11 passed`。
- 同一进程跑全部测试时，测试 home 里会有别的会话；`session/list` 的过滤保证结果不变。

再在 Linux 上跑一次。这个容器里 bwrap 不能用，只有 Landlock，会出现 macOS 上没有的沙箱提示，用来证明屏蔽规则够用。脚本：

```sh
mkdir -p /tmp/acp-linux && cat > /tmp/acp-linux/run.sh <<'SH'
set -e
export CARGO_TARGET_DIR=/target
cd /src
cargo test -p orca-runtime --lib wire_baseline 2>&1 | grep -E '^test |test result|panicked|differs'
SH
docker run --rm -v "$PWD":/src -v orca-linux-target:/target \
  -v orca-linux-cargo:/usr/local/cargo/registry -v /tmp/acp-linux:/scripts \
  rust:1.99-bookworm bash /scripts/run.sh
```

Expected：`test result: ok. 11 passed`。卷 `orca-linux-target`、`orca-linux-cargo` 已经存在时，编译是增量的。

- [ ] **Step 7：格式和验证器**

Run：
- `cargo fmt --all -- --check`
- `env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs`
- `env -u NODE_OPTIONS node scripts/validate-windows-platform-boundaries.mjs`

Expected：
- fmt 没有输出；
- 两个验证器分别打印 `runtime surface contract validated` 和 `windows platform boundary contract passed`。

- [ ] **Step 8：提交**

```bash
git add crates/orca-runtime/src/acp/supervisor.rs crates/orca-runtime/src/acp/supervisor/tests/wire_baseline.rs crates/orca-runtime/src/golden/acp_wire
git commit -F - <<'MSG'
test(acp): record the ACP wire baseline before the SDK migration

Eleven scenarios drive the daemon's own transport with literal JSON-RPC
frames and compare everything it writes with golden files, so the move to
agent-client-protocol 3.2.0 can show the wire did not change. They cover
initialize, authenticate, session new/load/list, the settings methods
including the legacy session/set_model and the models field, prompts with
text, tool calls, plans and usage, cancel, permissions (allowed and
declined, #76), fs and terminal requests, the orca.dev queue methods,
$/cancel_request, unknown methods and the malformed-params replies of #80.

Host paths, UUIDs, times and sandbox readiness warnings are masked and keys
are sorted. Notifications compare as a sorted list because separate tasks
write them. Unix only: Windows paths and sandbox warnings differ, the wire
shapes do not.

Stable over five nextest runs, one cargo test run and on a Landlock-only
Linux container.
MSG
```

---
### Task 2：客户端的 `ClientHandler` 和 `AgentHandle`（旧 SDK）

**Files:**
- Modify（整个文件重写）：`crates/orca-runtime/src/acp/client.rs`
- Modify：`crates/orca-tui/src/acp_client.rs`：import、`impl ClientHandler for TuiClient`、`control_request` 的参数和三处测试调用。
- Modify：`crates/orca-runtime/tests/acp_agent.rs`：删掉 `WireTestClient`、`wire_connection_pair`、`acp_wire_round_trip_projects_typed_prompt_updates`，收窄 import。
- Modify：`crates/orca-runtime/src/acp/supervisor.rs`：在 `mod tests` 里加 `AttachTexts` 和 `the_attach_client_round_trips_a_prompt_with_the_daemon_connection`。

**Interfaces:**
- Consumes：0.10.4 的 `ClientSideConnection`、`Client` trait；`super::daemon::connect`；`run_connection`。
- Produces（`orca_runtime::acp::client`，任务 4、5 和 TUI 都依赖这些名字）：

```rust
#[async_trait::async_trait(?Send)]
pub trait ClientHandler {
    async fn request_permission(&self, request: RequestPermissionRequest)
        -> agent_client_protocol::Result<RequestPermissionResponse>;
    async fn session_notification(&self, notification: SessionNotification)
        -> agent_client_protocol::Result<()>;
}

#[derive(Clone)]
pub struct AgentHandle { /* private */ }
impl AgentHandle {
    pub async fn initialize(&self, request: InitializeRequest) -> agent_client_protocol::Result<InitializeResponse>;
    pub async fn new_session(&self, request: NewSessionRequest) -> agent_client_protocol::Result<NewSessionResponse>;
    pub async fn load_session(&self, request: LoadSessionRequest) -> agent_client_protocol::Result<LoadSessionResponse>;
    pub async fn prompt(&self, request: PromptRequest) -> agent_client_protocol::Result<PromptResponse>;
    // SDK type here; Orca's own type from task 4 on.
    pub async fn set_session_model(&self, request: SetSessionModelRequest) -> agent_client_protocol::Result<SetSessionModelResponse>;
    pub async fn set_session_config_option(&self, request: SetSessionConfigOptionRequest) -> agent_client_protocol::Result<SetSessionConfigOptionResponse>;
    pub async fn cancel(&self, notification: CancelNotification) -> agent_client_protocol::Result<()>;
}

pub struct Connection { pub agent: AgentHandle, /* private */ }
impl Connection {
    pub async fn connect(socket: &Path, client: Rc<dyn ClientHandler>, capabilities: ClientCapabilities) -> io::Result<Self>;
    pub(crate) async fn connect_streams<R, W>(read: R, write: W, client: Rc<dyn ClientHandler>, capabilities: ClientCapabilities) -> io::Result<Self>
    where R: AsyncRead + Unpin + Send + 'static, W: AsyncWrite + Unpin + Send + 'static;
    pub fn is_closed(&self) -> bool;
    pub async fn attach(&self, cwd: &Path, selector: &str) -> io::Result<SessionId>;
    pub async fn attach_with_metadata(&self, cwd: &Path, selector: &str) -> io::Result<AttachedSession>;
}
```

`connect` 和 `connect_streams` 都要在 `LocalSet` 里调用。`Rc<TuiClient>` 在调用处会自动转成 `Rc<dyn ClientHandler>`。

- [ ] **Step 1：先写客户端测试**

`crates/orca-runtime/src/acp/client.rs` 末尾的 `#[cfg(test)] mod tests` 换成下面 Step 3 文件里的 `mod tests` 部分：
- 原有的 `readiness_metadata_extracts_only_string_warnings` 保留；
- 新增 `set_model_and_cancel_keep_their_wire_shape` 和 `a_daemon_hangup_fails_the_pending_prompt_and_closes_the_connection`。

它们用一个脚本化的"daemon"（duplex 的另一端）驱动 `Connection::connect_streams`。

- [ ] **Step 2：运行，确认编译失败**

Run：`env -u NODE_OPTIONS cargo nextest run -p orca-runtime --lib -E 'test(/^acp::client::/)'`

Expected：编译失败，报 `ClientHandler` 和 `connect_streams` 找不到。

- [ ] **Step 3：重写 `client.rs`**

整个文件替换为：

```rust
//! Standard ACP SDK connection used by the hosted TUI and headless attach.

use std::io;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use agent_client_protocol::{
    Agent as _, CancelNotification, Client, ClientCapabilities, ClientSideConnection, ContentBlock,
    Implementation, InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, ProtocolVersion,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse, SessionId,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModelRequest, SetSessionModelResponse, StopReason,
};
use tokio::io::{AsyncRead, AsyncWrite};

/// What an attach client does with the daemon's requests and notifications.
#[async_trait::async_trait(?Send)]
pub trait ClientHandler {
    async fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> agent_client_protocol::Result<RequestPermissionResponse>;

    async fn session_notification(
        &self,
        notification: SessionNotification,
    ) -> agent_client_protocol::Result<()>;
}

/// The SDK's client role, played by a [`ClientHandler`].
struct SdkClient(Rc<dyn ClientHandler>);

#[async_trait::async_trait(?Send)]
impl Client for SdkClient {
    async fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> agent_client_protocol::Result<RequestPermissionResponse> {
        self.0.request_permission(request).await
    }

    async fn session_notification(
        &self,
        notification: SessionNotification,
    ) -> agent_client_protocol::Result<()> {
        self.0.session_notification(notification).await
    }
}

/// Requests and notifications an attach client sends to the daemon.
#[derive(Clone)]
pub struct AgentHandle {
    connection: Rc<ClientSideConnection>,
}

impl AgentHandle {
    pub async fn initialize(
        &self,
        request: InitializeRequest,
    ) -> agent_client_protocol::Result<InitializeResponse> {
        self.connection.initialize(request).await
    }

    pub async fn new_session(
        &self,
        request: NewSessionRequest,
    ) -> agent_client_protocol::Result<NewSessionResponse> {
        self.connection.new_session(request).await
    }

    pub async fn load_session(
        &self,
        request: LoadSessionRequest,
    ) -> agent_client_protocol::Result<LoadSessionResponse> {
        self.connection.load_session(request).await
    }

    pub async fn prompt(
        &self,
        request: PromptRequest,
    ) -> agent_client_protocol::Result<PromptResponse> {
        self.connection.prompt(request).await
    }

    pub async fn set_session_model(
        &self,
        request: SetSessionModelRequest,
    ) -> agent_client_protocol::Result<SetSessionModelResponse> {
        self.connection.set_session_model(request).await
    }

    pub async fn set_session_config_option(
        &self,
        request: SetSessionConfigOptionRequest,
    ) -> agent_client_protocol::Result<SetSessionConfigOptionResponse> {
        self.connection.set_session_config_option(request).await
    }

    pub async fn cancel(
        &self,
        notification: CancelNotification,
    ) -> agent_client_protocol::Result<()> {
        self.connection.cancel(notification).await
    }
}

pub struct Connection {
    pub agent: AgentHandle,
    io: tokio::task::JoinHandle<agent_client_protocol::Result<()>>,
}

pub struct AttachedSession {
    pub session_id: SessionId,
    pub startup_warnings: Vec<String>,
}

const MAX_READINESS_WARNINGS: usize = 16;
const MAX_READINESS_WARNING_BYTES: usize = 8 * 1024;

pub fn readiness_warnings(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Vec<String> {
    let Some(readiness) = meta.and_then(|meta| meta.get(super::READINESS_META)) else {
        return Vec::new();
    };
    if readiness.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
        return Vec::new();
    }
    readiness
        .get("startupWarnings")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter(|warning| warning.len() <= MAX_READINESS_WARNING_BYTES)
        .take(MAX_READINESS_WARNINGS)
        .map(str::to_string)
        .collect()
}

impl Connection {
    #[cfg(unix)]
    pub async fn connect(
        socket: &Path,
        client: Rc<dyn ClientHandler>,
        capabilities: ClientCapabilities,
    ) -> io::Result<Self> {
        let stream = super::daemon::connect(socket).await?;
        let (read, write) = stream.into_split();
        Self::connect_streams(read, write, client, capabilities).await
    }

    #[cfg(not(unix))]
    pub async fn connect(
        _socket: &Path,
        _client: Rc<dyn ClientHandler>,
        _capabilities: ClientCapabilities,
    ) -> io::Result<Self> {
        Err(super::daemon::unsupported())
    }

    /// Runs the connection on a `LocalSet` and initializes it.
    pub(crate) async fn connect_streams<R, W>(
        read: R,
        write: W,
        client: Rc<dyn ClientHandler>,
        capabilities: ClientCapabilities,
    ) -> io::Result<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
        let (connection, io_task) = ClientSideConnection::new(
            SdkClient(client),
            write.compat_write(),
            read.compat(),
            |future| {
                tokio::task::spawn_local(future);
            },
        );
        let connection = Self {
            agent: AgentHandle {
                connection: Rc::new(connection),
            },
            io: tokio::task::spawn_local(io_task),
        };
        tokio::time::timeout(
            Duration::from_secs(10),
            connection.agent.initialize(
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_capabilities(capabilities)
                    .client_info(Implementation::new(
                        "orca-hosted-client",
                        env!("CARGO_PKG_VERSION"),
                    )),
            ),
        )
        .await?
        .map_err(io::Error::other)?;
        Ok(connection)
    }

    pub fn is_closed(&self) -> bool {
        self.io.is_finished()
    }

    /// `new` creates a session; all other selectors must be exact session IDs.
    /// Never retry a prompt automatically: a lost response is not a failed turn.
    pub async fn attach(&self, cwd: &Path, selector: &str) -> io::Result<SessionId> {
        self.attach_with_metadata(cwd, selector)
            .await
            .map(|attached| attached.session_id)
    }

    pub async fn attach_with_metadata(
        &self,
        cwd: &Path,
        selector: &str,
    ) -> io::Result<AttachedSession> {
        tokio::time::timeout(Duration::from_secs(30), async {
            if selector == "new" {
                self.agent
                    .new_session(NewSessionRequest::new(cwd.to_path_buf()))
                    .await
                    .map(|response| AttachedSession {
                        startup_warnings: readiness_warnings(response.meta.as_ref()),
                        session_id: response.session_id,
                    })
            } else {
                let id = SessionId::new(selector);
                self.agent
                    .load_session(LoadSessionRequest::new(id.clone(), cwd.to_path_buf()))
                    .await
                    .map(|response| AttachedSession {
                        startup_warnings: readiness_warnings(response.meta.as_ref()),
                        session_id: id,
                    })
            }
        })
        .await?
        .map_err(io::Error::other)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.io.abort();
    }
}

struct HeadlessClient;

#[async_trait::async_trait(?Send)]
impl ClientHandler for HeadlessClient {
    async fn request_permission(
        &self,
        _request: RequestPermissionRequest,
    ) -> agent_client_protocol::Result<RequestPermissionResponse> {
        // Non-interactive attachment never silently grants permission.
        Ok(RequestPermissionResponse::new(
            RequestPermissionOutcome::Cancelled,
        ))
    }

    async fn session_notification(
        &self,
        note: SessionNotification,
    ) -> agent_client_protocol::Result<()> {
        use std::io::Write;
        if let SessionUpdate::AgentMessageChunk(chunk) = note.update
            && let ContentBlock::Text(text) = chunk.content
        {
            let mut stdout = io::stdout().lock();
            stdout
                .write_all(text.text.as_bytes())
                .map_err(agent_client_protocol::Error::into_internal_error)?;
            stdout
                .flush()
                .map_err(agent_client_protocol::Error::into_internal_error)?;
        }
        Ok(())
    }
}

pub fn run_headless(socket: &Path, cwd: &Path, selector: &str, prompt: String) -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    tokio::task::LocalSet::new().block_on(&runtime, async {
        let connection =
            Connection::connect(socket, Rc::new(HeadlessClient), ClientCapabilities::new()).await?;
        let attached = connection.attach_with_metadata(cwd, selector).await?;
        for warning in &attached.startup_warnings {
            eprintln!("orca: warning: {warning}");
        }
        let id = attached.session_id;
        eprintln!("orca: attached ACP session {id}");
        let response = connection
            .agent
            .prompt(PromptRequest::new(id, vec![prompt.into()]))
            .await
            .map_err(io::Error::other)?;
        println!();
        if response.stop_reason == StopReason::EndTurn {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "ACP turn stopped: {:?}",
                response.stop_reason
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

    use super::*;

    #[test]
    fn readiness_metadata_extracts_only_string_warnings() {
        let meta = serde_json::Map::from_iter([(
            super::super::READINESS_META.to_string(),
            serde_json::json!({
                "version": 1,
                "startupWarnings": ["shell unavailable", 7, null],
            }),
        )]);

        assert_eq!(
            readiness_warnings(Some(&meta)),
            vec!["shell unavailable".to_string()]
        );
        assert!(readiness_warnings(None).is_empty());

        let incompatible = serde_json::Map::from_iter([(
            super::super::READINESS_META.to_string(),
            serde_json::json!({
                "version": 2,
                "startupWarnings": ["must be ignored"],
            }),
        )]);
        assert!(readiness_warnings(Some(&incompatible)).is_empty());
    }

    /// Records the text of every agent message chunk.
    #[derive(Default)]
    struct Recorder {
        texts: RefCell<Vec<String>>,
    }

    #[async_trait::async_trait(?Send)]
    impl ClientHandler for Recorder {
        async fn request_permission(
            &self,
            _request: RequestPermissionRequest,
        ) -> agent_client_protocol::Result<RequestPermissionResponse> {
            Ok(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ))
        }

        async fn session_notification(
            &self,
            notification: SessionNotification,
        ) -> agent_client_protocol::Result<()> {
            if let SessionUpdate::AgentMessageChunk(chunk) = notification.update
                && let ContentBlock::Text(text) = chunk.content
            {
                self.texts.borrow_mut().push(text.text);
            }
            Ok(())
        }
    }

    /// The daemon end of a connection, scripted by the test.
    struct Daemon {
        read: BufReader<ReadHalf<DuplexStream>>,
        write: WriteHalf<DuplexStream>,
    }

    impl Daemon {
        async fn recv(&mut self) -> Value {
            let mut line = String::new();
            let read = tokio::time::timeout(Duration::from_secs(5), self.read.read_line(&mut line))
                .await
                .expect("client frame timeout")
                .unwrap();
            assert_ne!(read, 0, "client closed the connection");
            serde_json::from_str(&line).unwrap()
        }

        async fn send(&mut self, frame: Value) {
            let mut bytes = serde_json::to_vec(&frame).unwrap();
            bytes.push(b'\n');
            self.write.write_all(&bytes).await.unwrap();
        }
    }

    /// Connects `client` to a scripted daemon that answers `initialize`.
    async fn connected(client: Rc<dyn ClientHandler>) -> (Connection, Daemon) {
        let (client_end, daemon_end) = tokio::io::duplex(64 * 1024);
        let (client_read, client_write) = tokio::io::split(client_end);
        let (daemon_read, daemon_write) = tokio::io::split(daemon_end);
        let mut daemon = Daemon {
            read: BufReader::new(daemon_read),
            write: daemon_write,
        };
        let connecting = tokio::task::spawn_local(Connection::connect_streams(
            client_read,
            client_write,
            client,
            ClientCapabilities::new(),
        ));
        let initialize = daemon.recv().await;
        assert_eq!(initialize["method"], "initialize");
        assert_eq!(
            initialize["params"]["clientInfo"]["name"],
            "orca-hosted-client"
        );
        daemon
            .send(
                json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1}}),
            )
            .await;
        (connecting.await.unwrap().unwrap(), daemon)
    }

    fn run_local(test: impl std::future::Future<Output = ()>) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&runtime, test);
    }

    #[test]
    fn set_model_and_cancel_keep_their_wire_shape() {
        run_local(async {
            let (connection, mut daemon) = connected(Rc::new(Recorder::default())).await;
            let agent = connection.agent.clone();
            let set_model = tokio::task::spawn_local(async move {
                agent
                    .set_session_model(SetSessionModelRequest::new("s-1", "deepseek-v4-pro"))
                    .await
            });
            let request = daemon.recv().await;
            assert_eq!(request["method"], "session/set_model");
            assert_eq!(
                request["params"],
                json!({"sessionId": "s-1", "modelId": "deepseek-v4-pro"})
            );
            daemon
                .send(json!({"jsonrpc": "2.0", "id": request["id"], "result": {}}))
                .await;
            set_model.await.unwrap().unwrap();

            connection
                .agent
                .cancel(CancelNotification::new("s-1"))
                .await
                .unwrap();
            assert_eq!(
                daemon.recv().await,
                json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "s-1"}})
            );
        });
    }

    #[test]
    fn a_daemon_hangup_fails_the_pending_prompt_and_closes_the_connection() {
        run_local(async {
            let (connection, mut daemon) = connected(Rc::new(Recorder::default())).await;
            let agent = connection.agent.clone();
            let prompt = tokio::task::spawn_local(async move {
                agent
                    .prompt(PromptRequest::new(
                        "s-1",
                        vec![ContentBlock::from("hi".to_string())],
                    ))
                    .await
            });
            assert_eq!(daemon.recv().await["method"], "session/prompt");
            drop(daemon);
            let result = tokio::time::timeout(Duration::from_secs(5), prompt)
                .await
                .expect("the prompt settles")
                .unwrap();
            assert!(
                result.is_err(),
                "a lost turn is not a finished one: {result:?}"
            );
            tokio::time::timeout(Duration::from_secs(5), async {
                while !connection.is_closed() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the connection closes");
        });
    }
}
```

要点：
- `SdkClient` 把 0.10.4 的 `Client` trait 转给 `ClientHandler`；
- `AgentHandle` 包着 `Rc<ClientSideConnection>`，调用 SDK 的 `Agent` trait 方法（所以要 `use agent_client_protocol::Agent as _`）；
- 连接、attach、超时和 `HeadlessClient` 的行为都不变。

- [ ] **Step 4：运行客户端测试**

Run：`env -u NODE_OPTIONS cargo nextest run -p orca-runtime --lib -E 'test(/^acp::client::/)'`

Expected：3 个 PASS。

- [ ] **Step 5：TUI 改用 `ClientHandler` 和 `AgentHandle`**

`crates/orca-tui/src/acp_client.rs` 顶部的两个 import 改为：

```rust
use agent_client_protocol::{
    CancelNotification, ClientCapabilities, ContentBlock, ImageContent, PermissionOption,
    PermissionOptionKind, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionConfigKind, SessionConfigOption, SessionId, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionModelRequest, StopReason, ToolCallContent,
    ToolCallStatus, ToolCallUpdateFields,
};
```

```rust
use orca_runtime::acp::{
    PROJECTION_META,
    client::{AgentHandle, ClientHandler, Connection, readiness_warnings},
};
```

其余改动：
- `impl Client for TuiClient {` 改为 `impl ClientHandler for TuiClient {`，上面的 `#[async_trait::async_trait(?Send)]` 保留。
- `async fn control_request(agent: &impl Agent, session: SessionId, action: UserAction)` 的第一个参数改为 `agent: &AgentHandle`。
- `run` 里的 `control_request(agent.as_ref(), session, action).await` 改为 `control_request(&agent, session, action).await`。
- 测试 `model_picker_uses_standard_requests_and_reports_denial` 里三处 `control_request(connection.agent.as_ref(),` 改为 `control_request(&connection.agent,`。
- `Connection::connect(&options.socket, client.clone(), capabilities)` 和三个测试里的 `Connection::connect(&path, client.clone(), ClientCapabilities::new())` 不用改。

- [ ] **Step 6：把 SDK 对 SDK 的线上测试换成生产客户端对 daemon 传输层**

在 `crates/orca-runtime/tests/acp_agent.rs` 里：

1. 删掉 `struct WireTestClient` 和它的 `impl Client`、`fn wire_connection_pair`、`#[test] fn acp_wire_round_trip_projects_typed_prompt_updates`：从 `struct WireTestClient` 上面那行 `#[derive(Clone, Default)]` 一直删到 `struct OrcaHomeGuard` 之前。
2. 删掉 `use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};`。
3. SDK 的 import 改为：

```rust
use agent_client_protocol::{
    Agent, AudioContent, CancelNotification, ClientCapabilities, ContentBlock, EmbeddedResource,
    EmbeddedResourceResource, FileSystemCapabilities, InitializeRequest, LoadSessionRequest,
    NewSessionRequest, PromptRequest, ProtocolVersion, ResourceLink, SessionId,
    SessionNotification, SessionUpdate, StopReason, TextResourceContents,
};
```

（`Agent` 留到任务 3 再去掉，这里的直接调用还要用它。）

在 `crates/orca-runtime/src/acp/supervisor.rs` 的 `mod tests` 里，紧挨在 `#[test] fn production_connection_routes_standard_interactions_and_fails_unnegotiated_extensions_closed()` 之前，加：

```rust
    /// Collects the attach client's agent message text.
    #[derive(Default)]
    struct AttachTexts(RefCell<Vec<String>>);

    #[async_trait::async_trait(?Send)]
    impl crate::acp::client::ClientHandler for AttachTexts {
        async fn request_permission(
            &self,
            _request: RequestPermissionRequest,
        ) -> agent_client_protocol::Result<RequestPermissionResponse> {
            Ok(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ))
        }

        async fn session_notification(
            &self,
            notification: agent_client_protocol::SessionNotification,
        ) -> agent_client_protocol::Result<()> {
            if let agent_client_protocol::SessionUpdate::AgentMessageChunk(chunk) =
                notification.update
                && let ContentBlock::Text(text) = chunk.content
            {
                self.0.borrow_mut().push(text.text);
            }
            Ok(())
        }
    }

    #[test]
    fn the_attach_client_round_trips_a_prompt_with_the_daemon_connection() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let host =
                RuntimeHost::start_with_executor(Arc::new(CompleteWithMessageExecutor)).unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let daemon = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let texts = Rc::new(AttachTexts::default());
            let connection = crate::acp::client::Connection::connect_streams(
                client_read,
                client_write,
                texts.clone(),
                ClientCapabilities::new(),
            )
            .await
            .expect("attach client connects");
            let session = connection
                .attach(cwd.path(), "new")
                .await
                .expect("session/new");
            let response = connection
                .agent
                .prompt(PromptRequest::new(
                    session,
                    vec![ContentBlock::from("complete".to_string())],
                ))
                .await
                .expect("prompt");
            assert_eq!(
                response.stop_reason,
                agent_client_protocol::StopReason::EndTurn
            );
            tokio::time::timeout(TEST_TIMEOUT, async {
                while texts.0.borrow().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the update reaches the client");
            assert_eq!(*texts.0.borrow(), ["typed update"]);
            drop(connection);
            tokio::time::timeout(TEST_TIMEOUT, daemon)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            host.shutdown().unwrap();
        });
    }
```

这里用轮询等更新到达：0.10.4 在单独的任务里处理通知，可能晚于 prompt 的响应。任务 5 的屏障会让它立即成立。

- [ ] **Step 7：运行 ACP 相关测试和基准**

Run：
- `env -u NODE_OPTIONS cargo nextest run -p orca-runtime -p orca-tui --lib -E 'test(/^acp::/) | test(/acp_client::/)'`
- `env -u NODE_OPTIONS cargo nextest run -p orca-runtime --test acp_agent --test acp_rpc_facade`

Expected：
- 第一条全部 PASS，其中包括 11 个 `wire_baseline_*`、3 个 `acp::client::tests::*`、`the_attach_client_round_trips_a_prompt_with_the_daemon_connection`，以及 TUI 的 19 个 `acp_client::tests::*`；
- 第二条全部 PASS（`acp_agent` 少了一个测试）。

- [ ] **Step 8：格式、验证器、提交**

Run：
- `cargo fmt --all -- --check`
- `env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs`
- `env -u NODE_OPTIONS node scripts/validate-windows-platform-boundaries.mjs`

Expected：都通过。

```bash
git add crates/orca-runtime/src/acp/client.rs crates/orca-runtime/src/acp/supervisor.rs crates/orca-runtime/tests/acp_agent.rs crates/orca-tui/src/acp_client.rs
git commit -F - <<'MSG'
refactor(acp): give the attach client its own handler and agent handle

The hosted TUI and headless attach implemented the SDK's Client trait and
called the SDK's Agent trait on a ClientSideConnection. agent-client-protocol
3.x has neither trait, so the attach client now depends on Orca's own
ClientHandler (permission requests and session notifications) and
AgentHandle (the requests and the cancel it sends), with
Connection::connect_streams for any byte stream. Behavior is unchanged;
the SDK still runs underneath.

The old wire test paired the SDK's client with the SDK's
AgentSideConnection, which production never uses. It is replaced by a
round trip from the production client to the daemon's own transport.
New client tests pin the set_model and cancel frames and that a daemon
hangup fails the pending prompt and closes the connection.
MSG
```

---

### Task 3：`OrcaAcpAgent` 的 ACP 方法改为固有方法（旧 SDK）

**Files:**
- Modify：`crates/orca-runtime/src/acp/agent.rs`：模块注释、SDK import、`impl Agent for OrcaAcpAgent`。
- Modify：`crates/orca-runtime/src/acp/supervisor.rs`：SDK import 和分发里的调用。
- Modify：`crates/orca-runtime/tests/acp_agent.rs`：SDK import 和 7 处 `Agent::…(&agent, …)` 调用。

**Interfaces:**
- Produces：`impl OrcaAcpAgent` 上的公开方法，签名和原来 trait 里的一样，只是变成 `pub async fn`：
  - `initialize`、`authenticate`、`new_session`、`load_session`、`prompt`；
  - `set_session_model`、`set_session_mode`、`set_session_config_option`；
  - `list_sessions`、`cancel`。
- 任务 4、5 会改其中几个的参数类型。

这是纯重构：没有新测试，靠已有测试和基准守住。

- [ ] **Step 1：改 `agent.rs`**

- 第 1 行 `//! ACP [`Agent`] implementation projected onto the runtime-owned typed surface.` 改为 `//! The ACP agent, projected onto the runtime-owned typed surface.`（`Agent` 不再是 import 进来的名字，文档链接会断）。
- 顶部 `use agent_client_protocol::{ Agent, AgentCapabilities, …` 去掉 `Agent, `。
- 把

```rust
#[async_trait::async_trait(?Send)]
impl Agent for OrcaAcpAgent {
    async fn initialize(&self, args: InitializeRequest) -> Result<InitializeResponse, Error> {
```

改为

```rust
impl OrcaAcpAgent {
    pub async fn initialize(&self, args: InitializeRequest) -> Result<InitializeResponse, Error> {
```

- 这个 impl 块里其余 9 个 `async fn` 都改成 `pub async fn`：`authenticate`、`new_session`、`load_session`、`prompt`、`set_session_model`、`set_session_mode`、`set_session_config_option`、`list_sessions`、`cancel`。函数体不动。

- [ ] **Step 2：改 supervisor 的分发**

- `crates/orca-runtime/src/acp/supervisor.rs` 顶部 `use agent_client_protocol::{ Agent, AuthenticateRequest, …` 去掉 `Agent, `。
- `handle_inbound` 里的调用改为方法调用：
  - `Agent::initialize(agent.as_ref(), args)` 改为 `agent.initialize(args)`；
  - `authenticate`、`new_session`、`load_session`、`list_sessions`、`set_session_model`、`set_session_mode`、`set_session_config_option` 同样改；
  - `Agent::cancel(agent.as_ref(), args).await.map_err(…)` 改为 `agent.cancel(args).await.map_err(…)`。
- 改完 `grep -n "Agent::" crates/orca-runtime/src/acp/supervisor.rs` 应当没有输出。

- [ ] **Step 3：改集成测试**

- `crates/orca-runtime/tests/acp_agent.rs` 的 SDK import 去掉 `Agent, `。
- 7 处调用改为方法调用：
  - `Agent::new_session(&agent, NewSessionRequest::new(…))` 改为 `agent.new_session(NewSessionRequest::new(…))`；
  - `Agent::prompt(&agent, PromptRequest::new(…))` 改为 `agent.prompt(PromptRequest::new(…))`；
  - `Agent::cancel(&agent, CancelNotification::new(…))` 改为 `agent.cancel(CancelNotification::new(…))`。
- 改完 `grep -n "Agent::" crates/orca-runtime/tests/acp_agent.rs` 应当没有输出。

- [ ] **Step 4：编译和测试**

Run：
- `cargo check -p orca-runtime -p orca-tui --lib --tests`
- `env -u NODE_OPTIONS cargo nextest run -p orca-runtime -p orca-tui --lib -E 'test(/^acp::/) | test(/acp_client::/)'`
- `env -u NODE_OPTIONS cargo nextest run -p orca-runtime --test acp_agent --test acp_rpc_facade`

Expected：
- 编译没有警告；
- 测试全部 PASS，包括 11 个 `wire_baseline_*`。

- [ ] **Step 5：格式、验证器、提交**

Run：`cargo fmt --all -- --check`，以及 Global Constraints 里的两个验证器。Expected：都通过。

```bash
git add crates/orca-runtime/src/acp/agent.rs crates/orca-runtime/src/acp/supervisor.rs crates/orca-runtime/tests/acp_agent.rs
git commit -F - <<'MSG'
refactor(acp): make the agent's ACP methods inherent

agent-client-protocol 3.x drops the Agent trait. Orca's daemon and stdio
connection already decode requests in their own JSON-RPC layer and only
called the trait's methods, so the methods become inherent on
OrcaAcpAgent with the same signatures. No behavior change; the wire
baseline and the ACP tests pass unchanged.
MSG
```

---
### Task 4：旧的会话模型 API 改用本地类型（旧 SDK）

**Files:**
- Create：`crates/orca-runtime/src/acp/legacy_model.rs`
- Modify：`crates/orca-runtime/src/acp/mod.rs`：声明 `pub mod legacy_model;`。
- Modify：`crates/orca-runtime/src/acp/settings.rs`：`ModelInfo`、`SessionModelState` 改从本地模块引入。
- Modify：`crates/orca-runtime/src/acp/agent.rs`：
  - `new_session`、`load_session` 拆出带 `models` 的版本；
  - `set_session_model` 改用本地类型。
- Modify：`crates/orca-runtime/src/acp/supervisor.rs`：`session/new`、`session/load` 走带 `models` 的版本，`session/set_model` 用本地类型解码。
- Modify：`crates/orca-runtime/src/acp/client.rs`：`AgentHandle::set_session_model` 收本地类型，在 0.10.4 上转成 SDK 自带的那份再发出。
- Modify：`crates/orca-tui/src/acp_client.rs`：`SetSessionModelRequest` 改从本地模块引入。

**Interfaces:**
- Produces（`orca_runtime::acp::legacy_model`）：
  - `ModelId(pub Arc<str>)`（`Display`、`From<String>`、`From<&str>`）；
  - `ModelInfo::new(model_id, name)`；
  - `SessionModelState::new(current_model_id, available_models)`；
  - `SetSessionModelRequest::new(session_id, model_id)`，字段 `session_id`、`model_id`、`meta`；
  - `SetSessionModelResponse`（`Default`，字段 `meta`）；
  - `WithModels<T> { pub response: T, pub models: Option<SessionModelState> }`（`Serialize`，`response` 用 `flatten` 展开）。
- Produces（`OrcaAcpAgent`）：
  - `pub(super) async fn new_session_with_models(&self, NewSessionRequest) -> Result<WithModels<NewSessionResponse>, Error>`；
  - `pub(super) async fn load_session_with_models(&self, LoadSessionRequest) -> Result<WithModels<LoadSessionResponse>, Error>`；
  - `pub async fn set_session_model(&self, legacy_model::SetSessionModelRequest) -> Result<legacy_model::SetSessionModelResponse, Error>`。
  - `new_session`、`load_session` 的公开签名不变，返回时丢掉 `models`。只有 daemon（共享会话）才会带 `models`。

线上形状按 0.11.4 的 `unstable_session_model` 抄：camelCase；`description`、`_meta` 为 `None` 时省略；`ModelId` 是透明的字符串。

- [ ] **Step 1：写本地类型和它们的测试**

新建 `crates/orca-runtime/src/acp/legacy_model.rs`：

```rust
//! The session model API that ACP schema 0.11 shipped as `unstable_session_model`.
//!
//! Schema 1.x dropped it for the `model` config option. Orca still reports
//! `models` from `session/new` and `session/load` and still accepts
//! `session/set_model`, in the 0.11 wire shapes, so clients written against
//! the old API keep working.

use std::sync::Arc;

use agent_client_protocol::SessionId;
use serde::{Deserialize, Serialize};

type Meta = serde_json::Map<String, serde_json::Value>;

/// A unique identifier for a model.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(pub Arc<str>);

impl std::fmt::Display for ModelId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<String> for ModelId {
    fn from(value: String) -> Self {
        Self(value.into())
    }
}

impl From<&str> for ModelId {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}

/// Information about a selectable model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub model_id: ModelId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

impl ModelInfo {
    pub fn new(model_id: impl Into<ModelId>, name: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            name: name.into(),
            description: None,
            meta: None,
        }
    }
}

/// The set of models and the one currently active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModelState {
    pub current_model_id: ModelId,
    pub available_models: Vec<ModelInfo>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

impl SessionModelState {
    pub fn new(current_model_id: impl Into<ModelId>, available_models: Vec<ModelInfo>) -> Self {
        Self {
            current_model_id: current_model_id.into(),
            available_models,
            meta: None,
        }
    }
}

/// `session/set_model` parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionModelRequest {
    pub session_id: SessionId,
    pub model_id: ModelId,
    #[serde(skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

impl SetSessionModelRequest {
    pub fn new(session_id: impl Into<SessionId>, model_id: impl Into<ModelId>) -> Self {
        Self {
            session_id: session_id.into(),
            model_id: model_id.into(),
            meta: None,
        }
    }
}

/// `session/set_model` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionModelResponse {
    #[serde(skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Meta>,
}

/// A `session/new` or `session/load` result with the legacy `models` field.
#[derive(Debug, Serialize)]
pub struct WithModels<T> {
    #[serde(flatten)]
    pub response: T,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<SessionModelState>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_model_state_keeps_the_schema_0_11_shape() {
        let state = SessionModelState::new(
            "auto",
            vec![
                ModelInfo::new("auto", "auto"),
                ModelInfo::new("deepseek-v4-pro", "deepseek-v4-pro"),
            ],
        );
        assert_eq!(
            serde_json::to_value(&state).unwrap(),
            json!({
                "currentModelId": "auto",
                "availableModels": [
                    {"modelId": "auto", "name": "auto"},
                    {"modelId": "deepseek-v4-pro", "name": "deepseek-v4-pro"},
                ],
            })
        );
    }

    #[test]
    fn set_model_keeps_the_schema_0_11_shape() {
        let wire = json!({"sessionId": "s-1", "modelId": "deepseek-flash", "_meta": {"k": 1}});
        let request: SetSessionModelRequest = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(request.model_id.to_string(), "deepseek-flash");
        assert_eq!(serde_json::to_value(&request).unwrap(), wire);
        let missing = serde_json::from_value::<SetSessionModelRequest>(json!({"sessionId": "s-1"}))
            .unwrap_err();
        assert_eq!(missing.to_string(), "missing field `modelId`");
        assert_eq!(
            serde_json::to_value(SetSessionModelResponse::default()).unwrap(),
            json!({})
        );
    }

    #[test]
    fn models_ride_beside_the_session_response() {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Response {
            session_id: &'static str,
        }
        let with = WithModels {
            response: Response { session_id: "s-1" },
            models: Some(SessionModelState::new("auto", Vec::new())),
        };
        assert_eq!(
            serde_json::to_value(&with).unwrap(),
            json!({
                "sessionId": "s-1",
                "models": {"currentModelId": "auto", "availableModels": []},
            })
        );
        let without = WithModels {
            response: Response { session_id: "s-1" },
            models: None,
        };
        assert_eq!(
            serde_json::to_value(&without).unwrap(),
            json!({"sessionId": "s-1"})
        );
    }
}
```

`crates/orca-runtime/src/acp/mod.rs` 在 `pub mod daemon;` 后加一行 `pub mod legacy_model;`。

- [ ] **Step 2：运行新测试**

Run：`env -u NODE_OPTIONS cargo nextest run -p orca-runtime --lib -E 'test(/^acp::legacy_model::/)'`

Expected：3 个 PASS。这几个测试只依赖 serde，在 0.10.4 和 3.2.0 上都应该通过。

- [ ] **Step 3：`settings.rs` 改用本地类型**

`crates/orca-runtime/src/acp/settings.rs` 的 import

```rust
use agent_client_protocol::{
    Error, ModelInfo, SessionConfigOption, SessionConfigSelectOption, SessionMode,
    SessionModeState, SessionModelState,
};
use orca_core::approval_types::ApprovalMode;
```

改为

```rust
use agent_client_protocol::{
    Error, SessionConfigOption, SessionConfigSelectOption, SessionMode, SessionModeState,
};
use orca_core::approval_types::ApprovalMode;

use super::legacy_model::{ModelInfo, SessionModelState};
```

`models()` 和 `options()` 的函数体不用动：用到的字段名和方法本地类型都有。

- [ ] **Step 4：agent 拆出带 `models` 的 `session/new`、`session/load`**

在 `crates/orca-runtime/src/acp/agent.rs` 的 `use crate::surface::{` 之前加：

```rust
use super::legacy_model::{SetSessionModelRequest, SetSessionModelResponse, WithModels};
```

`new_session` 改成一个薄包装加带 `models` 的实现。把

```rust
    pub async fn new_session(&self, args: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        self.negotiated_client_capabilities()?;
```

改为

```rust
    pub async fn new_session(&self, args: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        Ok(self.new_session_with_models(args).await?.response)
    }

    /// `session/new` as it goes on the wire: a daemon session also reports the
    /// legacy `models`.
    pub(super) async fn new_session_with_models(
        &self,
        args: NewSessionRequest,
    ) -> Result<WithModels<NewSessionResponse>, Error> {
        self.negotiated_client_capabilities()?;
```

这个函数里共享会话分支的返回

```rust
            return Ok(NewSessionResponse::new(id)
                .models(super::settings::models(&settings))
                .modes(super::settings::modes(
                    &settings,
                    self.base_config.approval_mode,
                ))
                .config_options(super::settings::options(
                    &settings,
                    self.base_config.approval_mode,
                ))
                .meta(startup_warnings_meta(&startup_warnings)));
```

改为

```rust
            return Ok(WithModels {
                response: NewSessionResponse::new(id)
                    .modes(super::settings::modes(
                        &settings,
                        self.base_config.approval_mode,
                    ))
                    .config_options(super::settings::options(
                        &settings,
                        self.base_config.approval_mode,
                    ))
                    .meta(startup_warnings_meta(&startup_warnings)),
                models: Some(super::settings::models(&settings)),
            });
```

最后的 `Ok(NewSessionResponse::new(session_id).meta(startup_warnings_meta(&startup_warnings)))` 改为

```rust
        Ok(WithModels {
            response: NewSessionResponse::new(session_id)
                .meta(startup_warnings_meta(&startup_warnings)),
            models: None,
        })
```

`load_session` 照同样的方式改：

```rust
    pub async fn load_session(
        &self,
        args: LoadSessionRequest,
    ) -> Result<LoadSessionResponse, Error> {
        Ok(self.load_session_with_models(args).await?.response)
    }

    /// `session/load` as it goes on the wire: a daemon session also reports
    /// the legacy `models`.
    pub(super) async fn load_session_with_models(
        &self,
        args: LoadSessionRequest,
    ) -> Result<WithModels<LoadSessionResponse>, Error> {
        self.negotiated_client_capabilities()?;
```

它的共享会话分支返回

```rust
            return Ok(WithModels {
                response: LoadSessionResponse::new()
                    .modes(super::settings::modes(
                        &settings,
                        self.base_config.approval_mode,
                    ))
                    .config_options(super::settings::options(
                        &settings,
                        self.base_config.approval_mode,
                    ))
                    .meta(startup_warnings_meta(&startup_warnings)),
                models: Some(super::settings::models(&settings)),
            });
```

最后的 `Ok(LoadSessionResponse::new().meta(startup_warnings_meta(&startup_warnings)))` 改为

```rust
        Ok(WithModels {
            response: LoadSessionResponse::new().meta(startup_warnings_meta(&startup_warnings)),
            models: None,
        })
```

`set_session_model` 的签名改为

```rust
    pub async fn set_session_model(
        &self,
        args: SetSessionModelRequest,
    ) -> Result<SetSessionModelResponse, Error> {
```

末尾的 `Ok(agent_client_protocol::SetSessionModelResponse::new())` 改为 `Ok(SetSessionModelResponse::default())`。函数体其余部分不动（`args.model_id.to_string()` 用的是 `ModelId` 的 `Display`）。

- [ ] **Step 5：supervisor 走带 `models` 的版本**

`crates/orca-runtime/src/acp/supervisor.rs` 的 `handle_inbound` 里：
- `"session/new"` 分支的 `agent.new_session(args).await` 改为 `agent.new_session_with_models(args).await`；
- `"session/load"` 分支的 `agent.load_session(args).await` 改为 `agent.load_session_with_models(args).await`；
- `"session/set_model"` 分支的 `decode::<agent_client_protocol::SetSessionModelRequest>(params)` 改为 `decode::<super::legacy_model::SetSessionModelRequest>(params)`。

`response_completion` 对任何 `Serialize` 都适用，不用改。

- [ ] **Step 6：客户端发本地类型**

在 `crates/orca-runtime/src/acp/client.rs` 里：
- SDK import 里去掉 `SetSessionModelRequest`、`SetSessionModelResponse`；
- 在 `use tokio::io::{AsyncRead, AsyncWrite};` 后加：

```rust

use super::legacy_model::{SetSessionModelRequest, SetSessionModelResponse};
```

- `AgentHandle::set_session_model` 改为：

```rust
    pub async fn set_session_model(
        &self,
        request: SetSessionModelRequest,
    ) -> agent_client_protocol::Result<SetSessionModelResponse> {
        // SDK 0.10 carries its own copy of the unstable type.
        self.connection
            .set_session_model(
                agent_client_protocol::SetSessionModelRequest::new(
                    request.session_id,
                    request.model_id.0.to_string(),
                )
                .meta(request.meta),
            )
            .await
            .map(|response| SetSessionModelResponse {
                meta: response.meta,
            })
    }
```

`crates/orca-tui/src/acp_client.rs`：
- SDK import 里去掉 `SetSessionModelRequest`；
- `orca_runtime::acp` 的 import 改为：

```rust
use orca_runtime::acp::{
    PROJECTION_META,
    client::{AgentHandle, ClientHandler, Connection, readiness_warnings},
    legacy_model::SetSessionModelRequest,
};
```

`control_request` 里的 `SetSessionModelRequest::new(session.clone(), model)` 不用改：本地类型的 `new` 签名一样。

- [ ] **Step 7：编译和测试**

Run：
- `cargo check -p orca-runtime -p orca-tui --lib --tests`
- `env -u NODE_OPTIONS cargo nextest run -p orca-runtime -p orca-tui --lib -E 'test(/^acp::/) | test(/acp_client::/)'`
- `env -u NODE_OPTIONS cargo nextest run -p orca-runtime --test acp_agent --test acp_rpc_facade`

Expected：
- 没有警告，全部 PASS；
- 其中 `wire_baseline_daemon_session_settings` 证明 `models` 仍在 `session/new`、`session/load` 的结果里，`session/set_model` 仍可用；
- `set_model_and_cancel_keep_their_wire_shape` 证明客户端发出的帧没变。

- [ ] **Step 8：格式、验证器、提交**

Run：`cargo fmt --all -- --check`，以及 Global Constraints 里的两个验证器。Expected：都通过。

```bash
git add crates/orca-runtime/src/acp/legacy_model.rs crates/orca-runtime/src/acp/mod.rs crates/orca-runtime/src/acp/settings.rs crates/orca-runtime/src/acp/agent.rs crates/orca-runtime/src/acp/supervisor.rs crates/orca-runtime/src/acp/client.rs crates/orca-tui/src/acp_client.rs
git commit -F - <<'MSG'
refactor(acp): keep the legacy session model API in Orca's own types

ACP schema 1.x removed the unstable session model API (models on
session/new and session/load, and session/set_model) in favour of the
model config option. Orca serves both, and clients built on the old API,
including earlier Orca TUIs, still use it. legacy_model.rs now holds the
four types in their schema 0.11 wire shape; the agent reports models
beside the SDK response through WithModels, and the supervisor and the
attach client use the local set_model types.

The wire baseline is unchanged; new tests pin the 0.11 shapes.
MSG
```

---
### Task 5：升级到 agent-client-protocol 3.2.0

**Files:**
- Modify：`Cargo.toml`、`Cargo.lock`
- Modify（类型路径）：
  - `crates/orca-runtime/src/acp/{agent,supervisor,observer,settings,legacy_model}.rs`
  - `crates/orca-tui/src/acp_client.rs`
  - `crates/orca-runtime/tests/acp_agent.rs`
- Modify（整个文件重写）：`crates/orca-runtime/src/acp/client.rs`
- Modify：`crates/orca-runtime/src/acp/legacy_model.rs`：加 `JsonRpcRequest`、`JsonRpcResponse` 派生。
- Modify：`crates/orca-runtime/src/acp/supervisor.rs`、`crates/orca-runtime/src/acp/agent.rs`：`session/list` 的目录过滤单独解码。
- Modify（`preserve_order`）：
  - `crates/orca-runtime/src/protocol/events.rs`
  - `crates/orca-provider/src/tool_schema.rs`
  - `crates/orca-tools/src/registry.rs`
  - `crates/orca-runtime/src/workflow/state.rs`
  - `crates/orca-runtime/src/workflow/runner.rs`
- Modify：`scripts/eval/acp_param_probe.py`
- Test：
  - `client.rs` 新增 2 个测试；
  - `supervisor.rs` 新增 `a_wrong_typed_optional_field_reads_as_absent`；
  - `workflow/state.rs` 新增 `input_hash_ignores_the_order_of_option_keys`。

**Interfaces:**
- Consumes：任务 2 的 `ClientHandler`、`AgentHandle`、`Connection` 的公开签名，本任务一个都不改；任务 4 的 `legacy_model` 类型。
- 3.2 的 API：
  - `agent_client_protocol::Client.builder()`、`.on_receive_request(async move |req: RequestPermissionRequest, responder: Responder<RequestPermissionResponse>, _cx| …, on_receive_request!())`、`.on_receive_notification(async move |n: SessionNotification, _cx| …, on_receive_notification!())`、`.connect_with(ByteStreams::new(write, read), async move |cx: ConnectionTo<Agent>| …)`；
  - `cx.send_request(r).block_task().await`、`cx.send_notification(n)`、`cx.incoming_closed().await`；
  - `Responder::respond_with_result`；
  - 派生宏 `agent_client_protocol::{JsonRpcRequest, JsonRpcResponse}`，配 `#[request(method = "...", response = T)]`。
- Produces：
  - `OrcaAcpAgent::list_sessions(&self, args: ListSessionsRequest, additional_directories: Vec<PathBuf>)`；
  - `crate::workflow::state::sorted_json(&Value) -> String`（`pub(crate)`）。

**客户端内部（3.2）：**
- SDK 的处理函数必须是 `Send` 的，所以它们只把收到的权限请求（连同 responder）和会话通知放进一个无界通道。
- `LocalSet` 上的一个泵按到达顺序处理：通知逐个交给 `ClientHandler`；每个权限请求起一个本地任务，结果经 responder 回复。
- `AgentHandle` 的每个请求拿到响应后，往同一个通道发一个屏障，等泵处理到它再返回。这样 daemon 在响应之前发出的更新，客户端都已经处理完。
- `connect_with` 的主函数把 `ConnectionTo<Agent>` 交出去，然后等 `incoming_closed()`；所以 daemon 断开时 `is_closed()` 会变成 true。
- `Connection` 被丢弃时 abort 驱动任务和泵。

- [ ] **Step 1：确认版本并改依赖**

Run：`cargo search agent-client-protocol --limit 1`

如果最新版是 3.2.x 且高于 3.2.0：
- 下面所有 `=3.2.0` 都换成它；
- 先读它的 CHANGELOG（`~/.cargo/registry/src/*/agent-client-protocol-<版本>/CHANGELOG.md`），确认没有动 builder、`block_task`、`incoming_closed`、`$/cancel_request`。

根 `Cargo.toml` 的

```toml
agent-client-protocol = { version = "=0.10.4", features = ["unstable"] }
```

改为

```toml
agent-client-protocol = "=3.2.0"
```

- [ ] **Step 2：改类型路径**

在仓库根目录运行（文件逐个列出，zsh 不拆变量）：

```sh
perl -0pi -e 's/agent_client_protocol::(?!Error\b|ErrorCode\b|Result\b|schema\b)([A-Z]\w*)/agent_client_protocol::schema::v1::$1/g; s/use agent_client_protocol::\{/use agent_client_protocol::schema::v1::{/g; s/agent_client_protocol::schema::v1::ProtocolVersion/agent_client_protocol::schema::ProtocolVersion/g' \
  crates/orca-runtime/src/acp/agent.rs \
  crates/orca-runtime/src/acp/supervisor.rs \
  crates/orca-runtime/src/acp/observer.rs \
  crates/orca-runtime/src/acp/settings.rs \
  crates/orca-runtime/src/acp/legacy_model.rs \
  crates/orca-tui/src/acp_client.rs \
  crates/orca-runtime/tests/acp_agent.rs
```

这条命令做三件事：
- 把 `agent_client_protocol::Name` 改成 `agent_client_protocol::schema::v1::Name`，`Error`、`ErrorCode`、`Result` 除外；
- 把 `use agent_client_protocol::{` 改成 `use agent_client_protocol::schema::v1::{`；
- 把带路径的 `ProtocolVersion` 改到 `schema` 下。

`schema::v1` 也导出 `Error`，所以 `use … ::{…, Error, …}` 里的 `Error` 不用挑出来。`shared.rs` 只用 `agent_client_protocol::Error`，不用改。`client.rs` 在 Step 6 整个替换。

- [ ] **Step 3：修 `ProtocolVersion` 的 import**

`ProtocolVersion` 在 `schema` 下，不在 `schema::v1` 下：
- `crates/orca-runtime/src/acp/agent.rs`：
  - 顶部 import 列表里删掉 `ProtocolVersion, `（在 `PromptResponse, ProtocolVersion, RequestPermissionOutcome,` 一行）；
  - 在 `use agent_client_protocol::schema::v1::{` 之前加 `use agent_client_protocol::schema::ProtocolVersion;`。
- `crates/orca-runtime/src/acp/supervisor.rs` 的 `mod tests`：
  - 从 `use agent_client_protocol::schema::v1::{ CancelNotification, … }` 里删掉 `ProtocolVersion,`；
  - 在 `use super::*;` 后加 `use agent_client_protocol::schema::ProtocolVersion;`。
- `crates/orca-runtime/tests/acp_agent.rs`：
  - 从 SDK import 列表删掉 `ProtocolVersion, `；
  - 在它之前加 `use agent_client_protocol::schema::ProtocolVersion;`。

- [ ] **Step 4：`legacy_model` 的请求类型可以直接发送**

`crates/orca-runtime/src/acp/legacy_model.rs`：

```rust
/// `session/set_model` parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionModelRequest {
```

改为

```rust
/// `session/set_model` parameters.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, agent_client_protocol::JsonRpcRequest,
)]
#[request(method = "session/set_model", response = SetSessionModelResponse)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionModelRequest {
```

```rust
/// `session/set_model` result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionModelResponse {
```

改为

```rust
/// `session/set_model` result.
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    agent_client_protocol::JsonRpcResponse,
)]
#[serde(rename_all = "camelCase")]
pub struct SetSessionModelResponse {
```

Step 2 已经把 `use agent_client_protocol::SessionId;` 改成了 `use agent_client_protocol::schema::v1::SessionId;`。

- [ ] **Step 5：`session/list` 的目录过滤单独解码**

schema 1.10.2 的 `ListSessionsRequest` 没有 `additional_directories` 了。

`crates/orca-runtime/src/acp/agent.rs` 的 `list_sessions`：
- 签名改为

```rust
    pub async fn list_sessions(
        &self,
        args: agent_client_protocol::schema::v1::ListSessionsRequest,
        additional_directories: Vec<PathBuf>,
    ) -> Result<agent_client_protocol::schema::v1::ListSessionsResponse, Error> {
```

- 函数体里的 `if !args.additional_directories.is_empty() {` 改为 `if !additional_directories.is_empty() {`。检查顺序不变：先能力，再工作区，再目录，再游标。

`crates/orca-runtime/src/acp/supervisor.rs`：
- `fn decode` 之后加：

```rust
/// Schema 1.x dropped the unstable `additionalDirectories` filter from
/// `session/list`. Orca still refuses a non-empty one rather than list every
/// directory's sessions.
fn decode_list_sessions(
    params: Value,
) -> Result<
    (
        agent_client_protocol::schema::v1::ListSessionsRequest,
        Vec<std::path::PathBuf>,
    ),
    serde_json::Error,
> {
    #[derive(serde::Deserialize)]
    struct Filter {
        #[serde(default, rename = "additionalDirectories")]
        additional_directories: Vec<std::path::PathBuf>,
    }
    let request = decode(params.clone())?;
    let filter = decode::<Filter>(params)?;
    Ok((request, filter.additional_directories))
}
```

- `"session/list"` 分支改为：

```rust
            "session/list" => {
                let result = match decode_list_sessions(params) {
                    Ok((args, additional_directories)) => {
                        agent.list_sessions(args, additional_directories).await
                    }
                    Err(error) => {
                        Err(agent_client_protocol::Error::invalid_params().data(error.to_string()))
                    }
                };
                Ok(response_completion(facade, request_id, result))
            }
```

先解码 SDK 的请求，非对象参数的报错文字就和以前一样。不要改用 `#[serde(flatten)]`：1.10.2 的这个类型不能被 flatten，会报 "can only flatten structs and maps"。

- [ ] **Step 6：客户端换成 builder 加 `Send` 适配层**

`crates/orca-runtime/src/acp/client.rs` 整个替换为下面的内容。测试部分在任务 2 的基础上做了三处改动：
- `Recorder` 多一个 `delay`；
- 新增 `updates_sent_before_a_response_are_handled_before_it_returns`；
- 新增 `a_request_dropped_while_outstanding_is_cancelled_on_the_wire`。

```rust
//! Standard ACP SDK connection used by the hosted TUI and headless attach.
//!
//! The SDK runs its handlers on `Send` tasks; attach clients keep their state
//! on a `LocalSet`. The handlers only queue what arrives, and one local pump
//! hands it to the [`ClientHandler`] in arrival order.

use std::io;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ClientCapabilities, ContentBlock, Implementation, InitializeRequest,
    InitializeResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionId, SessionNotification,
    SessionUpdate, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason,
};
use agent_client_protocol::{Agent, ByteStreams, ConnectionTo, JsonRpcRequest, Responder};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

use super::legacy_model::{SetSessionModelRequest, SetSessionModelResponse};

/// What an attach client does with the daemon's requests and notifications.
#[async_trait::async_trait(?Send)]
pub trait ClientHandler {
    async fn request_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> agent_client_protocol::Result<RequestPermissionResponse>;

    async fn session_notification(
        &self,
        notification: SessionNotification,
    ) -> agent_client_protocol::Result<()>;
}

enum Incoming {
    Permission(
        RequestPermissionRequest,
        Responder<RequestPermissionResponse>,
    ),
    Notification(SessionNotification),
    /// Answered once everything queued before it has been handled.
    Barrier(oneshot::Sender<()>),
}

/// Requests and notifications an attach client sends to the daemon.
#[derive(Clone)]
pub struct AgentHandle {
    connection: ConnectionTo<Agent>,
    incoming: mpsc::UnboundedSender<Incoming>,
}

impl AgentHandle {
    pub async fn initialize(
        &self,
        request: InitializeRequest,
    ) -> agent_client_protocol::Result<InitializeResponse> {
        self.request(request).await
    }

    pub async fn new_session(
        &self,
        request: NewSessionRequest,
    ) -> agent_client_protocol::Result<NewSessionResponse> {
        self.request(request).await
    }

    pub async fn load_session(
        &self,
        request: LoadSessionRequest,
    ) -> agent_client_protocol::Result<LoadSessionResponse> {
        self.request(request).await
    }

    pub async fn prompt(
        &self,
        request: PromptRequest,
    ) -> agent_client_protocol::Result<PromptResponse> {
        self.request(request).await
    }

    pub async fn set_session_model(
        &self,
        request: SetSessionModelRequest,
    ) -> agent_client_protocol::Result<SetSessionModelResponse> {
        self.request(request).await
    }

    pub async fn set_session_config_option(
        &self,
        request: SetSessionConfigOptionRequest,
    ) -> agent_client_protocol::Result<SetSessionConfigOptionResponse> {
        self.request(request).await
    }

    pub async fn cancel(
        &self,
        notification: CancelNotification,
    ) -> agent_client_protocol::Result<()> {
        self.connection.send_notification(notification)
    }

    async fn request<R: JsonRpcRequest>(
        &self,
        request: R,
    ) -> agent_client_protocol::Result<R::Response> {
        let result = self.connection.send_request(request).block_task().await;
        // The daemon sends a turn's updates before the turn's response. Let the
        // client handle everything that arrived first.
        let (done, handled) = oneshot::channel();
        if self.incoming.send(Incoming::Barrier(done)).is_ok() {
            let _ = handled.await;
        }
        result
    }
}

pub struct Connection {
    pub agent: AgentHandle,
    driver: tokio::task::JoinHandle<agent_client_protocol::Result<()>>,
    pump: tokio::task::JoinHandle<()>,
}

pub struct AttachedSession {
    pub session_id: SessionId,
    pub startup_warnings: Vec<String>,
}

const MAX_READINESS_WARNINGS: usize = 16;
const MAX_READINESS_WARNING_BYTES: usize = 8 * 1024;

pub fn readiness_warnings(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Vec<String> {
    let Some(readiness) = meta.and_then(|meta| meta.get(super::READINESS_META)) else {
        return Vec::new();
    };
    if readiness.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
        return Vec::new();
    }
    readiness
        .get("startupWarnings")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter(|warning| warning.len() <= MAX_READINESS_WARNING_BYTES)
        .take(MAX_READINESS_WARNINGS)
        .map(str::to_string)
        .collect()
}

impl Connection {
    #[cfg(unix)]
    pub async fn connect(
        socket: &Path,
        client: Rc<dyn ClientHandler>,
        capabilities: ClientCapabilities,
    ) -> io::Result<Self> {
        let stream = super::daemon::connect(socket).await?;
        let (read, write) = stream.into_split();
        Self::connect_streams(read, write, client, capabilities).await
    }

    #[cfg(not(unix))]
    pub async fn connect(
        _socket: &Path,
        _client: Rc<dyn ClientHandler>,
        _capabilities: ClientCapabilities,
    ) -> io::Result<Self> {
        Err(super::daemon::unsupported())
    }

    /// Starts the connection on a `LocalSet` and initializes it.
    pub(crate) async fn connect_streams<R, W>(
        read: R,
        write: W,
        client: Rc<dyn ClientHandler>,
        capabilities: ClientCapabilities,
    ) -> io::Result<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
        let (incoming, mut queued) = mpsc::unbounded_channel();
        let (opened, connection) = oneshot::channel();
        let permissions = incoming.clone();
        let notifications = incoming.clone();
        let driver = tokio::task::spawn_local(
            agent_client_protocol::Client
                .builder()
                .on_receive_request(
                    async move |request: RequestPermissionRequest,
                                responder: Responder<RequestPermissionResponse>,
                                _cx| {
                        let _ = permissions.send(Incoming::Permission(request, responder));
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_notification(
                    async move |notification: SessionNotification, _cx| {
                        let _ = notifications.send(Incoming::Notification(notification));
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_with(
                    ByteStreams::new(write.compat_write(), read.compat()),
                    async move |cx: ConnectionTo<Agent>| {
                        let _ = opened.send(cx.clone());
                        // The connection ends when the daemon hangs up or the
                        // `Connection` is dropped (which aborts this task).
                        cx.incoming_closed().await;
                        Ok(())
                    },
                ),
        );
        let pump = tokio::task::spawn_local(async move {
            while let Some(message) = queued.recv().await {
                match message {
                    Incoming::Notification(notification) => {
                        let _ = client.session_notification(notification).await;
                    }
                    Incoming::Permission(request, responder) => {
                        let client = Rc::clone(&client);
                        tokio::task::spawn_local(async move {
                            let result = client.request_permission(request).await;
                            let _ = responder.respond_with_result(result);
                        });
                    }
                    Incoming::Barrier(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        let connection = match connection.await {
            Ok(connection) => connection,
            Err(_) => {
                pump.abort();
                return Err(io::Error::other("ACP connection closed before it opened"));
            }
        };
        let connection = Self {
            agent: AgentHandle {
                connection,
                incoming,
            },
            driver,
            pump,
        };
        tokio::time::timeout(
            Duration::from_secs(10),
            connection.agent.initialize(
                InitializeRequest::new(ProtocolVersion::V1)
                    .client_capabilities(capabilities)
                    .client_info(Implementation::new(
                        "orca-hosted-client",
                        env!("CARGO_PKG_VERSION"),
                    )),
            ),
        )
        .await?
        .map_err(io::Error::other)?;
        Ok(connection)
    }

    pub fn is_closed(&self) -> bool {
        self.driver.is_finished()
    }

    /// `new` creates a session; all other selectors must be exact session IDs.
    /// Never retry a prompt automatically: a lost response is not a failed turn.
    pub async fn attach(&self, cwd: &Path, selector: &str) -> io::Result<SessionId> {
        self.attach_with_metadata(cwd, selector)
            .await
            .map(|attached| attached.session_id)
    }

    pub async fn attach_with_metadata(
        &self,
        cwd: &Path,
        selector: &str,
    ) -> io::Result<AttachedSession> {
        tokio::time::timeout(Duration::from_secs(30), async {
            if selector == "new" {
                self.agent
                    .new_session(NewSessionRequest::new(cwd.to_path_buf()))
                    .await
                    .map(|response| AttachedSession {
                        startup_warnings: readiness_warnings(response.meta.as_ref()),
                        session_id: response.session_id,
                    })
            } else {
                let id = SessionId::new(selector);
                self.agent
                    .load_session(LoadSessionRequest::new(id.clone(), cwd.to_path_buf()))
                    .await
                    .map(|response| AttachedSession {
                        startup_warnings: readiness_warnings(response.meta.as_ref()),
                        session_id: id,
                    })
            }
        })
        .await?
        .map_err(io::Error::other)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.driver.abort();
        self.pump.abort();
    }
}

struct HeadlessClient;

#[async_trait::async_trait(?Send)]
impl ClientHandler for HeadlessClient {
    async fn request_permission(
        &self,
        _request: RequestPermissionRequest,
    ) -> agent_client_protocol::Result<RequestPermissionResponse> {
        // Non-interactive attachment never silently grants permission.
        Ok(RequestPermissionResponse::new(
            RequestPermissionOutcome::Cancelled,
        ))
    }

    async fn session_notification(
        &self,
        note: SessionNotification,
    ) -> agent_client_protocol::Result<()> {
        use std::io::Write;
        if let SessionUpdate::AgentMessageChunk(chunk) = note.update
            && let ContentBlock::Text(text) = chunk.content
        {
            let mut stdout = io::stdout().lock();
            stdout
                .write_all(text.text.as_bytes())
                .map_err(agent_client_protocol::Error::into_internal_error)?;
            stdout
                .flush()
                .map_err(agent_client_protocol::Error::into_internal_error)?;
        }
        Ok(())
    }
}

pub fn run_headless(socket: &Path, cwd: &Path, selector: &str, prompt: String) -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    tokio::task::LocalSet::new().block_on(&runtime, async {
        let connection =
            Connection::connect(socket, Rc::new(HeadlessClient), ClientCapabilities::new()).await?;
        let attached = connection.attach_with_metadata(cwd, selector).await?;
        for warning in &attached.startup_warnings {
            eprintln!("orca: warning: {warning}");
        }
        let id = attached.session_id;
        eprintln!("orca: attached ACP session {id}");
        let response = connection
            .agent
            .prompt(PromptRequest::new(id, vec![prompt.into()]))
            .await
            .map_err(io::Error::other)?;
        println!();
        if response.stop_reason == StopReason::EndTurn {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "ACP turn stopped: {:?}",
                response.stop_reason
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

    use super::*;

    #[test]
    fn readiness_metadata_extracts_only_string_warnings() {
        let meta = serde_json::Map::from_iter([(
            super::super::READINESS_META.to_string(),
            serde_json::json!({
                "version": 1,
                "startupWarnings": ["shell unavailable", 7, null],
            }),
        )]);

        assert_eq!(
            readiness_warnings(Some(&meta)),
            vec!["shell unavailable".to_string()]
        );
        assert!(readiness_warnings(None).is_empty());

        let incompatible = serde_json::Map::from_iter([(
            super::super::READINESS_META.to_string(),
            serde_json::json!({
                "version": 2,
                "startupWarnings": ["must be ignored"],
            }),
        )]);
        assert!(readiness_warnings(Some(&incompatible)).is_empty());
    }

    /// Records the text of every agent message chunk, taking `delay` over each.
    #[derive(Default)]
    struct Recorder {
        texts: RefCell<Vec<String>>,
        delay: Duration,
    }

    #[async_trait::async_trait(?Send)]
    impl ClientHandler for Recorder {
        async fn request_permission(
            &self,
            _request: RequestPermissionRequest,
        ) -> agent_client_protocol::Result<RequestPermissionResponse> {
            Ok(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ))
        }

        async fn session_notification(
            &self,
            notification: SessionNotification,
        ) -> agent_client_protocol::Result<()> {
            tokio::time::sleep(self.delay).await;
            if let SessionUpdate::AgentMessageChunk(chunk) = notification.update
                && let ContentBlock::Text(text) = chunk.content
            {
                self.texts.borrow_mut().push(text.text);
            }
            Ok(())
        }
    }

    /// The daemon end of a connection, scripted by the test.
    struct Daemon {
        read: BufReader<ReadHalf<DuplexStream>>,
        write: WriteHalf<DuplexStream>,
    }

    impl Daemon {
        async fn recv(&mut self) -> Value {
            let mut line = String::new();
            let read = tokio::time::timeout(Duration::from_secs(5), self.read.read_line(&mut line))
                .await
                .expect("client frame timeout")
                .unwrap();
            assert_ne!(read, 0, "client closed the connection");
            serde_json::from_str(&line).unwrap()
        }

        async fn send(&mut self, frame: Value) {
            let mut bytes = serde_json::to_vec(&frame).unwrap();
            bytes.push(b'\n');
            self.write.write_all(&bytes).await.unwrap();
        }
    }

    /// Connects `client` to a scripted daemon that answers `initialize`.
    async fn connected(client: Rc<dyn ClientHandler>) -> (Connection, Daemon) {
        let (client_end, daemon_end) = tokio::io::duplex(64 * 1024);
        let (client_read, client_write) = tokio::io::split(client_end);
        let (daemon_read, daemon_write) = tokio::io::split(daemon_end);
        let mut daemon = Daemon {
            read: BufReader::new(daemon_read),
            write: daemon_write,
        };
        let connecting = tokio::task::spawn_local(Connection::connect_streams(
            client_read,
            client_write,
            client,
            ClientCapabilities::new(),
        ));
        let initialize = daemon.recv().await;
        assert_eq!(initialize["method"], "initialize");
        assert_eq!(
            initialize["params"]["clientInfo"]["name"],
            "orca-hosted-client"
        );
        daemon
            .send(
                json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1}}),
            )
            .await;
        (connecting.await.unwrap().unwrap(), daemon)
    }

    fn run_local(test: impl std::future::Future<Output = ()>) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&runtime, test);
    }

    #[test]
    fn set_model_and_cancel_keep_their_wire_shape() {
        run_local(async {
            let (connection, mut daemon) = connected(Rc::new(Recorder::default())).await;
            let agent = connection.agent.clone();
            let set_model = tokio::task::spawn_local(async move {
                agent
                    .set_session_model(SetSessionModelRequest::new("s-1", "deepseek-v4-pro"))
                    .await
            });
            let request = daemon.recv().await;
            assert_eq!(request["method"], "session/set_model");
            assert_eq!(
                request["params"],
                json!({"sessionId": "s-1", "modelId": "deepseek-v4-pro"})
            );
            daemon
                .send(json!({"jsonrpc": "2.0", "id": request["id"], "result": {}}))
                .await;
            set_model.await.unwrap().unwrap();

            connection
                .agent
                .cancel(CancelNotification::new("s-1"))
                .await
                .unwrap();
            assert_eq!(
                daemon.recv().await,
                json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "s-1"}})
            );
        });
    }

    #[test]
    fn a_daemon_hangup_fails_the_pending_prompt_and_closes_the_connection() {
        run_local(async {
            let (connection, mut daemon) = connected(Rc::new(Recorder::default())).await;
            let agent = connection.agent.clone();
            let prompt = tokio::task::spawn_local(async move {
                agent
                    .prompt(PromptRequest::new(
                        "s-1",
                        vec![ContentBlock::from("hi".to_string())],
                    ))
                    .await
            });
            assert_eq!(daemon.recv().await["method"], "session/prompt");
            drop(daemon);
            let result = tokio::time::timeout(Duration::from_secs(5), prompt)
                .await
                .expect("the prompt settles")
                .unwrap();
            assert!(
                result.is_err(),
                "a lost turn is not a finished one: {result:?}"
            );
            tokio::time::timeout(Duration::from_secs(5), async {
                while !connection.is_closed() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the connection closes");
        });
    }

    #[test]
    fn updates_sent_before_a_response_are_handled_before_it_returns() {
        run_local(async {
            let recorder = Rc::new(Recorder {
                delay: Duration::from_millis(20),
                ..Recorder::default()
            });
            let (connection, mut daemon) = connected(recorder.clone()).await;
            let agent = connection.agent.clone();
            let prompt = tokio::task::spawn_local(async move {
                agent
                    .prompt(PromptRequest::new(
                        "s-1",
                        vec![ContentBlock::from("hi".to_string())],
                    ))
                    .await
            });
            let request = daemon.recv().await;
            assert_eq!(request["method"], "session/prompt");
            for text in ["one", "two"] {
                daemon
                    .send(
                        json!({"jsonrpc": "2.0", "method": "session/update", "params": {
                            "sessionId": "s-1",
                            "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": {"type": "text", "text": text},
                            },
                        }}),
                    )
                    .await;
            }
            daemon
                .send(json!({"jsonrpc": "2.0", "id": request["id"], "result": {"stopReason": "end_turn"}}))
                .await;
            let response = prompt.await.unwrap().unwrap();
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            assert_eq!(*recorder.texts.borrow(), ["one", "two"]);
        });
    }

    #[test]
    fn a_request_dropped_while_outstanding_is_cancelled_on_the_wire() {
        run_local(async {
            let (connection, mut daemon) = connected(Rc::new(Recorder::default())).await;
            let agent = connection.agent.clone();
            let prompt = tokio::task::spawn_local(async move {
                agent
                    .prompt(PromptRequest::new(
                        "s-1",
                        vec![ContentBlock::from("hi".to_string())],
                    ))
                    .await
            });
            let request = daemon.recv().await;
            assert_eq!(request["method"], "session/prompt");
            prompt.abort();
            // The daemon answers no notification, so it ignores this one.
            assert_eq!(
                daemon.recv().await,
                json!({
                    "jsonrpc": "2.0",
                    "method": "$/cancel_request",
                    "params": {"requestId": request["id"]},
                })
            );
        });
    }
}
```

- [ ] **Step 7：编译、格式**

Run：
- `cargo check --workspace --all-targets`
- `cargo fmt --all`

Expected：
- 只剩一条早就有的警告：`tests/support/server_test_client.rs` 里 `ProcessJob::spawn` 已弃用；main 上也有，不是本任务引入的。
- `Cargo.lock` 被更新：`agent-client-protocol` 3.2.0、`agent-client-protocol-derive` 3.2.0、`agent-client-protocol-schema` 1.10.2，另外新增 `serde_with`、`futures-concurrency` 等。
- 用 `git diff Cargo.lock | grep -E '^[-+]name = '` 看一眼增删的包。

- [ ] **Step 8：ACP 测试和基准**

Run：
- `env -u NODE_OPTIONS cargo nextest run -p orca-runtime -p orca-tui --lib -E 'test(/^acp::/) | test(/acp_client::/)'`
- `env -u NODE_OPTIONS cargo nextest run -p orca-runtime --test acp_agent --test acp_rpc_facade`

Expected：全部 PASS，其中包括：
- 11 个 `wire_baseline_*`（基准原样通过，这是本计划的核心检查）；
- 5 个 `acp::client::tests::*`；
- 3 个 `acp::legacy_model::tests::*`；
- TUI 的 19 个 `acp_client::tests::*`。

基准如果失败，读失败信息里打印出的实际帧，找出差异，修代码，不要重新生成 golden。

- [ ] **Step 9：确认新测试真的有约束力**

临时删掉 `AgentHandle::request` 里的屏障（`let (done, handled) = oneshot::channel();` 到 `}` 那四行），然后运行：

`env -u NODE_OPTIONS cargo nextest run -p orca-runtime --lib -E 'test(/updates_sent_before_a_response/)'`

Expected：FAIL，`left: []`、`right: ["one", "two"]`。恢复那四行，再跑一次，PASS。

- [ ] **Step 10：全量测试，确认 `preserve_order` 造成的失败**

Run：`env -u NODE_OPTIONS cargo nextest run --workspace --all-targets --profile ci --no-fail-fast --retries 0`

Expected：恰好下面 5 个 FAIL，原因都是 JSON 键顺序：
- `blade-deepseek::jsonl_surface_differential released_v0_2_50_submit_wire_remains_byte_stable_after_identity_normalization`
- `blade-deepseek::provider_contract tool_schema_lowering_is_generic_and_preserves_deepseek_wire_shape`
- `orca-provider deepseek_http::tests::strict_tools_apply_only_on_beta_endpoint_to_eligible_definitions`
- `orca-provider tool_schema::tests::strict_lowering_uses_definition_metadata_instead_of_tool_names`
- `orca-tools registry::tests::the_list_of_one_server_has_the_shape_of_the_list_of_all_servers`

如果还有别的失败，先查清是不是同一个原因；是的话按 Step 12 的方式在它的输出处排序，并写进提交说明。

- [ ] **Step 11：写两个固定新行为的测试**

在 `crates/orca-runtime/src/acp/supervisor.rs` 的 `mod tests` 里，紧挨在 `/// Collects the attach client's agent message text.` 之前，加：

```rust
    #[test]
    fn a_wrong_typed_optional_field_reads_as_absent() {
        // ACP schema 1.x reads an optional field of the wrong type as absent:
        // `clientCapabilities: 42` is a client with no capabilities, where
        // schema 0.11 refused the request.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, async {
            let host =
                RuntimeHost::start_with_executor(Arc::new(CompleteWithMessageExecutor)).unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_read, mut client_write) = tokio::io::split(client);
            let (server_read, server_write) = tokio::io::split(server);
            let connection = tokio::task::spawn_local(run_connection(
                host.surface_handle(),
                test_config(cwd.path().to_path_buf()),
                server_read,
                server_write,
            ));
            let mut client_read = BufReader::new(client_read);
            write_request(
                &mut client_write,
                1,
                "initialize",
                json!({"protocolVersion": 1, "clientCapabilities": 42}),
            )
            .await;
            let initialized = read_response(&mut client_read, 1).await;
            assert_eq!(initialized["result"]["protocolVersion"], 1, "{initialized}");
            client_write.shutdown().await.unwrap();
            tokio::time::timeout(TEST_TIMEOUT, connection)
                .await
                .expect("connection shutdown")
                .expect("connection task")
                .expect("clean connection");
            host.shutdown().unwrap();
        });
    }
```

在 `crates/orca-runtime/src/workflow/state.rs` 文件末尾加：

```rust

#[cfg(test)]
mod input_hash_tests {
    use super::*;

    #[test]
    fn input_hash_ignores_the_order_of_option_keys() {
        let sorted: Value = serde_json::from_str(r#"{"a":1,"b":{"c":2,"d":3}}"#).unwrap();
        let shuffled: Value = serde_json::from_str(r#"{"b":{"d":3,"c":2},"a":1}"#).unwrap();
        assert_eq!(input_hash("p", &shuffled), input_hash("p", &sorted));
        // A run recorded before serde_json kept insertion order hashed the
        // sorted text; resuming it must find the same cached calls.
        assert_eq!(
            input_hash("p", &shuffled),
            "089522e635b5f0168bf302aef3a77f1403668f34a9f8bea0050885363587d479"
        );
    }
}
```

（这个摘要值是 `sha256(b"p\0" + b'{"a":1,"b":{"c":2,"d":3}}')`。）

Run：`env -u NODE_OPTIONS cargo nextest run -p orca-runtime --lib -E 'test(/input_hash_ignores|a_wrong_typed_optional_field/)'`

Expected：
- `a_wrong_typed_optional_field_reads_as_absent` PASS（固定 schema 1.x 的宽松解码）；
- `input_hash_ignores_the_order_of_option_keys` FAIL（还没排序）。

- [ ] **Step 12：在四处恢复排序输出**

`Value::sort_all_objects` 按 `BTreeMap` 的顺序重排所有嵌套对象；没有 `preserve_order` 时它什么也不做，所以这些改动对旧行为是精确还原。

1. `crates/orca-runtime/src/protocol/events.rs`，`ServerEventEnvelope::serialize` 的结尾：

```rust
        let mut map = serializer.serialize_map(Some(object.len()))?;
        for (key, value) in object {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
```

改为

```rust
        // The released wire has sorted keys. serde_json keeps insertion order
        // instead once a dependency turns on `preserve_order` (the ACP SDK does).
        value.sort_all_objects();
        value.serialize(serializer)
    }
```

并删掉文件顶部已经不用的 `use serde::ser::SerializeMap;`。

2. `crates/orca-provider/src/tool_schema.rs`：

```rust
fn deepseek_tool_schema(definition: &ProviderToolDefinition) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": definition.name,
            "description": definition.description,
            "parameters": definition.input_schema,
        }
    })
}
```

改为

```rust
/// DeepSeek has always received tool schemas with sorted keys, and a prompt
/// cache prefix depends on the exact bytes. serde_json keeps insertion order
/// once a dependency turns on `preserve_order` (the ACP SDK does), so sort.
fn deepseek_tool_schema(definition: &ProviderToolDefinition) -> Value {
    let mut tool = json!({
        "type": "function",
        "function": {
            "name": definition.name,
            "description": definition.description,
            "parameters": definition.input_schema,
        }
    });
    tool.sort_all_objects();
    tool
}
```

`deepseek_strict_tools_schema_for_endpoint` 里 `function.insert("strict".to_string(), Value::Bool(true));` 之后加一行 `tool.sort_all_objects();`。原因：`require_all_properties` 新插入的 `additionalProperties`、`required` 和 `strict` 也要排序；`required` 本身按已排序的 `properties` 生成，所以顺序也和以前一样。

3. `crates/orca-tools/src/registry.rs`：
   - `execute_list_mcp_resources` 里，把 `serde_json::to_value(&listing.resources).and_then(|resources| {` 改为

```rust
    let output = serde_json::to_value(&listing.resources).and_then(|mut resources| {
        // The model has always seen these keys sorted.
        resources.sort_all_objects();
```

   - `execute_list_mcp_resource_templates` 里同样处理 `resource_templates`：闭包参数改成 `mut resource_templates`，开头加同样的注释和 `resource_templates.sort_all_objects();`。

4. `crates/orca-runtime/src/workflow/state.rs`：把 `input_hash` 改为下面的样子，并在它后面加 `sorted_json`：

```rust
pub fn input_hash(prompt: &str, opts: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prompt.as_bytes());
    hasher.update(b"\0");
    hasher.update(sorted_json(opts).as_bytes());
    orca_core::hex::lower(&hasher.finalize())
}

/// `value` as JSON with every object's keys sorted, so a digest does not
/// depend on the order the keys arrived in. serde_json keeps insertion order
/// once a dependency turns on `preserve_order` (the ACP SDK does); recorded
/// digests were made with sorted keys.
pub(crate) fn sorted_json(value: &Value) -> String {
    let mut value = value.clone();
    value.sort_all_objects();
    serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string())
}
```

`crates/orca-runtime/src/workflow/runner.rs` 的 `digest_value` 改为：

```rust
fn digest_value(value: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(super::state::sorted_json(value).as_bytes());
    orca_core::hex::lower(&hasher.finalize())
}
```

Run：

```sh
env -u NODE_OPTIONS cargo nextest run --workspace --all-targets --no-fail-fast -E 'test(/released_v0_2_50_submit_wire_remains_byte_stable|tool_schema_lowering_is_generic_and_preserves_deepseek_wire_shape|strict_tools_apply_only_on_beta_endpoint_to_eligible_definitions|strict_lowering_uses_definition_metadata_instead_of_tool_names|the_list_of_one_server_has_the_shape_of_the_list_of_all_servers|input_hash_ignores/)'
```

Expected：6 个全部 PASS，那 5 个测试的期望一个字都没改。

- [ ] **Step 13：更新参数探针**

`scripts/eval/acp_param_probe.py` 的第 3 节：

```python
        # 3. Malformed params on known methods: -32602 + id echo. Both shapes a real
        #    client produces: the whole `params` value of the wrong JSON kind, and a
        #    single field of the wrong type (schema/version skew).
        client_info = {"name": "acp-param-probe", "version": "0.0.1"}
        malformed: list[tuple[int, str, object]] = [
            (3, "initialize", "not-an-object"),
            (4, "session/new", "not-an-object"),
            (5, "session/prompt", "not-an-object"),
            (6, "initialize", {"protocolVersion": 1, "clientCapabilities": 42, "clientInfo": client_info}),
            (7, "session/new", {"cwd": 42, "mcpServers": []}),
            (8, "session/load", {"sessionId": 42, "cwd": work, "mcpServers": []}),
            (9, "authenticate", {"methodId": 42}),
            (15, "orca.dev/session/queue/list", "not-an-object"),
            (16, "session/set_config_option", {"sessionId": 42}),
        ]
        for request_id, method, params in malformed:
            if isinstance(params, str):
                shape = "params not an object"
            else:
                bad = next(key for key, value in params.items() if key != "protocolVersion")
                shape = f"{bad} wrong type"
            bridge.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
```

改为

```python
        # 3. Malformed params on known methods: -32602 + id echo. Both shapes a real
        #    client produces: the whole `params` value of the wrong JSON kind, and a
        #    required field of the wrong type. (ACP schema 1.x reads an optional field
        #    of the wrong type as absent, so `clientCapabilities: 42` is not an error.)
        client_info = {"name": "acp-param-probe", "version": "0.0.1"}
        malformed: list[tuple[int, str, str, object]] = [
            (3, "initialize", "params not an object", "not-an-object"),
            (4, "session/new", "params not an object", "not-an-object"),
            (5, "session/prompt", "params not an object", "not-an-object"),
            (6, "initialize", "protocolVersion wrong type", {"protocolVersion": "one", "clientInfo": client_info}),
            (7, "session/new", "cwd wrong type", {"cwd": 42, "mcpServers": []}),
            (8, "session/load", "sessionId wrong type", {"sessionId": 42, "cwd": work, "mcpServers": []}),
            (9, "authenticate", "methodId wrong type", {"methodId": 42}),
            (15, "orca.dev/session/queue/list", "params not an object", "not-an-object"),
            (16, "session/set_config_option", "sessionId wrong type", {"sessionId": 42}),
        ]
        for request_id, method, shape, params in malformed:
            bridge.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
```

（`protocolVersion: "one"` 在 0.11 上反而被当成版本 0 接受，在 1.x 上被拒绝。所以这一项只在本任务之后成立。）

Run：
- `cargo build --bin orca`
- `env -u NODE_OPTIONS python3 scripts/eval/acp_param_probe.py --binary target/debug/orca`
- `env -u NODE_OPTIONS python3 scripts/eval/daemon_probe.py --binary target/debug/orca`

Expected：两个都打印 `verdict: PASS`。
- `daemon_probe.py` 里包括 `attach new + prompt`（无界面 attach 走的就是新客户端）、`acp permission decline` 的 `refusal`、`session reuse`。

- [ ] **Step 14：全量和 TUI**

Run：
- `env -u NODE_OPTIONS cargo nextest run --workspace --all-targets --profile ci --no-fail-fast --retries 0`
- `env -u NODE_OPTIONS cargo nextest run -p orca-tui --lib --profile ci-serial --retries 0`
- `env -u NODE_OPTIONS cargo nextest run --test tui_pty_contract --profile ci-serial --retries 0`

Expected：三条都没有 FAIL。

- [ ] **Step 15：格式、验证器、提交**

Run：`cargo fmt --all -- --check`，以及 Global Constraints 里的两个验证器。Expected：都通过。

```bash
git add Cargo.toml Cargo.lock crates/orca-runtime/src/acp crates/orca-runtime/src/protocol/events.rs crates/orca-runtime/src/workflow/state.rs crates/orca-runtime/src/workflow/runner.rs crates/orca-runtime/tests/acp_agent.rs crates/orca-tui/src/acp_client.rs crates/orca-provider/src/tool_schema.rs crates/orca-tools/src/registry.rs scripts/eval/acp_param_probe.py
git commit -F - <<'MSG'
build(deps): move the ACP SDK to agent-client-protocol 3.2.0

agent-client-protocol 0.10.4 (schema 0.11.4, with the unstable feature)
becomes 3.2.0 (schema 1.10.2, no features). The ACP v1 wire is unchanged:
the wire baseline recorded on 0.10.4 passes as it was.

- Message types come from agent_client_protocol::schema::v1.
- The attach client runs on the 3.x builder. Its handlers are Send, so they
  only queue permission requests and notifications for a pump on the
  LocalSet that hands them to the ClientHandler in order. Each request
  waits for the pump to drain what arrived before its response, so a turn's
  updates are handled before its result. The connection ends when the
  daemon hangs up. A request dropped while outstanding now sends
  $/cancel_request, which the daemon ignores like any notification it does
  not know.
- legacy_model's set_model types derive JsonRpcRequest/JsonRpcResponse.
- session/list decodes the additionalDirectories filter itself, since the
  schema dropped it, and still refuses a non-empty one.
- The schema turns on serde_json's preserve_order for the whole build.
  Where Orca's output order is part of a contract it is sorted again: the
  JSONL server envelope (released byte-stable wire), the DeepSeek tool
  schemas (prompt-cache prefix), the MCP resource lists the model reads and
  the workflow input/output digests that recorded runs compare. Other JSON
  keeps insertion order.
- Schema 1.x reads an optional field of the wrong type as absent and skips
  bad list items, where 0.11 refused the request; acp_param_probe now tests
  a required field of the wrong type instead.

Full workspace, TUI (serial) and PTY suites pass; acp_param_probe and
daemon_probe pass.
MSG
```

---
### Task 6：实测、Linux 复核、文档

**Files:**
- Modify：`docs/acp-daemon.md`：新增 "Protocol" 一节；"Verification" 一节补上线上流量基准。
- 仓库外：更新记忆文件 `orca-acp-sdk-migration-progress.md`，不提交。

**Interfaces:** 无新代码。用的是任务 5 编出的 `target/debug/orca` 和 `scripts/eval/mock_provider.py`。

- [ ] **Step 1：无界面 attach 实测**

```sh
cargo build --bin orca
work=$(mktemp -d); home=$(mktemp -d); sock="$home/acp/daemon.sock"
python3 scripts/eval/mock_provider.py --port 8799 --log /tmp/acp-live-requests.jsonl &
provider_pid=$!
export ORCA_HOME="$home" ORCA_API_KEY=live ORCA_BASE_URL=http://127.0.0.1:8799
unset DEEPSEEK_API_KEY
target/debug/orca daemon --socket "$sock" --cwd "$work" > /tmp/acp-live-daemon.log 2>&1 &
daemon_pid=$!
sleep 2
target/debug/orca attach new --cwd "$work" --socket "$sock" --exec "FIMODE=clean live headless attach"; echo "exit=$?"
```

Expected：
- stderr 有 `orca: attached ACP session <uuid>`；
- stdout 是模拟供应商的回答；
- `exit=0`；
- `/tmp/acp-live-daemon.log` 里没有报错。

daemon 和模拟供应商留着给下一步用。

- [ ] **Step 2：TUI attach 实测（tmux）**

```sh
tmux new-session -d -s acp-live -x 160 -y 48 \
  "env ORCA_HOME=$home ORCA_API_KEY=live ORCA_BASE_URL=http://127.0.0.1:8799 target/debug/orca attach new --cwd $work --socket $sock"
sleep 3; tmux capture-pane -p -t acp-live | tail -20
```

逐项操作，每次操作后用 `tmux capture-pane -p -t acp-live` 看屏幕：

1. **权限，拒绝：**
   - `tmux send-keys -t acp-live 'FIMODE=tool_script FITOOL=request_permissions' Enter` → 出现权限对话框；
   - 选拒绝 → 这一轮结束，没有卡住，输入框可用。
2. **权限，允许：** 再发一次同样的提示词，选允许一次 → 这一轮正常完成。
3. **Esc 打断：**
   - `tmux send-keys -t acp-live 'FIMODE=stall_once live interrupt' Enter`，等 2 秒，`tmux send-keys -t acp-live Escape`；
   - 几秒内状态回到空闲；输入框那一行没有被别的文字盖住（这是上一轮修过的问题）；
   - 再发 `FIMODE=clean after interrupt` 能正常完成。
4. **切换模型：**
   - 发 `/model`，在菜单里选 `deepseek-v4-pro`；
   - 状态栏显示新模型；
   - `/tmp/acp-live-requests.jsonl` 最后一条请求的 `model` 是 `deepseek-v4-pro`。这需要再发一条 `FIMODE=clean model check`。
5. **断线重连：**
   - `kill $daemon_pid` → TUI 提示 `ACP disconnected; reloading the authoritative session. The last prompt will not be resent.`；
   - 立刻用 Step 1 的同一条命令重启 daemon（记下新的 `daemon_pid`）→ TUI 重新载入同一个会话；
   - 如果重试次数用完，TUI 以状态 1 退出并说明原因，这也是预期行为。

结束后：`tmux kill-session -t acp-live`，然后 `kill $daemon_pid $provider_pid 2>/dev/null`。

任何一项不符，回到任务 5 查原因：先看客户端适配层，再看 TUI 调用处。

在 Zed 里实测是可选的，由你决定（spec 第 5 节）。

- [ ] **Step 3：Linux 复核**

```sh
mkdir -p /tmp/acp-linux && cat > /tmp/acp-linux/run.sh <<'SH'
set -e
export CARGO_TARGET_DIR=/target
cd /src
cargo test -p orca-runtime --lib acp:: 2>&1 | grep -E 'test result|FAILED|panicked'
cargo test -p orca-runtime --lib input_hash_ignores 2>&1 | grep -E 'test result|FAILED'
cargo test -p orca-runtime --test acp_agent 2>&1 | grep -E 'test result|FAILED'
cargo test -p orca-provider --lib tool_schema 2>&1 | grep -E 'test result|FAILED'
SH
docker run --rm -v "$PWD":/src -v orca-linux-target:/target \
  -v orca-linux-cargo:/usr/local/cargo/registry -v /tmp/acp-linux:/scripts \
  rust:1.99-bookworm bash /scripts/run.sh
```

Expected：
- 每行都是 `test result: ok`；
- 没有 `FAILED`；
- `acp::` 包括 11 个基准场景。

- [ ] **Step 4：文档**

在 `docs/acp-daemon.md` 的 `## Ownership and Recovery` 之前插入：

```markdown
## Protocol

- The daemon speaks ACP v1 through `agent-client-protocol` 3.2 (schema 1.10).
  It still reports `models` from `session/new` and `session/load` and accepts
  `session/set_model`: schema 1.x removed that session model API, and clients
  built on it keep working.
- Requests are decoded as schema 1.x specifies. An optional field of the wrong
  type reads as absent, and an invalid entry in a list such as `mcpServers` is
  skipped. A required field of the wrong type, or `params` that are not an
  object, is `-32602 Invalid params`. `session/list` still refuses a non-empty
  `additionalDirectories` filter.

```

在 `## Verification` 第一段之后插入：

```markdown
`acp::supervisor::tests::wire_baseline` drives eleven scenarios with literal
JSON-RPC frames and compares everything the daemon writes with
`crates/orca-runtime/src/golden/acp_wire/`. Regenerate the files with
`ORCA_UPDATE_GOLDEN=1` only for an intended wire change, and review the diff.

```

- [ ] **Step 5：提交文档**

```bash
git add docs/acp-daemon.md
git commit -F - <<'MSG'
docs(acp): describe the daemon's protocol version and wire baseline

The daemon now speaks ACP through agent-client-protocol 3.2 (schema 1.10).
Say that it keeps the legacy session model API, how schema 1.x decodes a
malformed optional field or list entry, and how the wire baseline guards
the daemon's output.
MSG
```

- [ ] **Step 6：更新记忆**

`~/.claude/projects/-Users-qingyun-Documents-GitHub-blade-deepseek/memory/orca-acp-sdk-migration-progress.md` 改为"已在本地 main 完成"，写上：
- 各提交号；
- 两处行为差异的决定；
- 实测结果；
- 待用户决定的事：推送并跑 CI（Linux、Windows）、是否发 v0.5.9。

发版说明要写的点：
- ACP SDK 3.2；
- schema 1.x 的宽松解码；
- JSON 键顺序：JSONL server、tool schema、MCP 列表照旧排序，其余改为插入顺序；
- 丢弃请求时发 `$/cancel_request`。

- [ ] **Step 7：清理**

```sh
rm -rf /tmp/acp-linux
git worktree list
git status --short
```

Expected：
- 没有多余的 worktree；
- `git status --short` 没有输出；
- `jobs/` 下的探针日志在 `.gitignore` 里，不会出现在 `git status` 中。

Docker 卷 `orca-linux-target`、`orca-linux-cargo` 留到发版清理时再删。
