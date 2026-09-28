# MCP 能力补全 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 Orca 的 MCP 能力与 Claude Code、Codex 看齐：
- 一条命令加好本地或远程服务器；
- 远程服务器按规范连接，需要时用 OAuth 登录；
- 只读工具不再每次询问，需要批准的工具可以一次选择、长期生效；
- 提供 `/mcp` 面板，MCP prompts 可以当斜杠命令用，也可以按工具启用或禁用。

**Architecture:**
- **orca-core**：配置类型、规则匹配，以及集中写用户配置和凭据文件的代码（`config/user_edit.rs`、`config/mcp_credentials.rs`）。
- **orca-mcp**：
  - 传输：stdio、streamable HTTP、旧版 SSE，外加一层自动回退；
  - OAuth 流程；
  - 可以按服务器重连的注册表。
- **orca-runtime**：把注册表的状态填进界面层已有的 MCP 目录，并提供两个界面命令：重连服务器、展开 prompt。
- **orca-tui**：`/mcp` 面板、prompt 斜杠命令、批准面板上的保存选项。
- **根包 CLI**：`orca mcp`。

**Tech Stack:** Rust，clap，toml_edit，reqwest（blocking），工作区已有的 serde、sha2、base64、url、uuid。

**Spec:** `docs/superpowers/specs/2026-09-28-mcp-approvals-and-cli-design.md`

## Global Constraints

- **平台**：所有改动都要在 Linux、Windows、macOS 上编译并通过测试。
  - 用 shell 脚本充当 MCP 服务器的测试加 `#[cfg(unix)]`。
  - 在已有条目上方插入新条目时，检查没有抢走原条目的 `#[cfg]`。
- **写用户配置**：只写 `$ORCA_HOME/config.toml`（默认 `~/.orca/config.toml`）。
  - 用 `toml_edit` 保留原有内容和注释，用 `orca_platform::fs::atomic_write(.., AtomicWritePolicy::NoFollow)` 原子写入。
  - 配置解析失败时报错，不覆盖原文件。
- **凭据文件**：`$ORCA_HOME/mcp-credentials.json`，只用 `orca_platform::fs::atomic_write_private(.., AtomicWritePolicy::NoFollow)` 写入，与 `auth.json` 相同。
- **名字格式**：
  - MCP 工具名为 `mcp__<server>__<tool>`；
  - 服务器名只能含字母、数字、`-`、`_`，不能含 `__`。
- **只读判定**：`readOnlyHint == true && destructiveHint != true`。
- **协议版本**：初始化时声明 `2025-06-18`，接受 `2025-06-18`、`2025-03-26`、`2024-11-05`。
- **序列化兼容**：新增的可序列化字段为默认值时不输出（`skip_serializing_if`），旧配置、旧会话、旧账本照常读取。
- **文案**：代码和面向用户的输出用英文；中英文档分别更新。
- **每次提交前**：
  - `cargo fmt --all -- --check`；
  - `cargo clippy --workspace --all-targets --locked`，不能有错误；
  - 直接运行（不接 `tail`）`node scripts/validate-runtime-surface-contract.mjs` 和 `node scripts/validate-windows-platform-boundaries.mjs`。

  validator 因为固定的计数失败时，在同一个提交里更新固定值，并在报告里写明。
- **测试**：用 `cargo nextest run`，不用 `cargo test`，因为 orca-runtime 的库测试共用 ORCA_HOME。TUI 的样式只走 `chrome.rs` 的边框、标记和提示，颜色守卫和 golden frame 有变化时一并更新。

## Review Focus

1. **手写过的配置**：`orca mcp add/remove` 和"保存允许"只改动相关条目，注释和其他表原样保留。配置解析失败时，文件字节保持不变。由任务 4 的 `add_keeps_the_rest_of_the_config`、`an_unparsable_config_is_never_overwritten` 和任务 9 的 `saving_an_allow_rule_keeps_comments` 钉住。
2. **登录中途放弃**：用户关掉浏览器、回调一直不来时，到超时后给出明确错误，释放回环端口，不挂住。由任务 7 的 `login_times_out_when_no_callback_arrives` 钉住。
3. **远程会话在使用中过期**：服务器回 404 后透明地重新初始化，并把原请求重试一次，工具调用照常完成。由任务 5 的 `an_expired_http_session_is_reinitialized_once` 钉住。
4. **密钥不外泄**：`list` 和 `get`（包括 `--json`）不输出环境变量值、请求头的值和 token；凭据文件权限为 600。由任务 4 的 `list_and_get_hide_secret_values`、任务 8 的 `status_output_never_contains_tokens` 和任务 7 的 `credentials_are_written_privately` 钉住。
5. **名字相近的服务器**：`mcp__github__*` 不能匹配 `mcp__githubx__tool`。由任务 2 的 `a_server_rule_matches_only_that_servers_tools` 钉住。

---

### Task 1：只读 MCP 工具按只读处理（设计 A）

**Files:**
- Modify: `crates/orca-core/src/mcp_types.rs`（`McpTool` 在第 68 行附近，`McpToolDescriptor` 在第 121 行附近）
- Modify: `crates/orca-mcp/src/client.rs`（`connect_server` 里构造 `McpTool`，在第 167 行附近）
- Modify: `crates/orca-tools/src/registry.rs`（`McpProxyTool::new`，在第 2207 行附近）
- Modify: 编译器报出的所有 `McpTool { .. }` 构造处，补上 `read_only: false`
- Modify: `site/src/docs/md/{en,zh}/mcp-integration.mdx`

**Interfaces（Produces，均在 `orca_core::mcp_types`）:**
- `#[derive(Clone, Debug, Default, Deserialize)] pub struct McpToolAnnotations`，字段：
  - `#[serde(rename = "readOnlyHint", default)] pub read_only_hint: Option<bool>`
  - `#[serde(rename = "destructiveHint", default)] pub destructive_hint: Option<bool>`
- `McpToolDescriptor` 新增 `#[serde(default)] pub annotations: Option<McpToolAnnotations>`，并提供 `pub fn is_read_only(&self) -> bool`。
- `McpTool` 新增 `#[serde(default, skip_serializing_if = "std::ops::Not::not")] pub read_only: bool`。
- `McpProxyTool::new`：动作类型为 `if tool.read_only { ActionKind::Read } else { ActionKind::Write }`，`capabilities` 和 `renderer` 用同一文件里的 `capability_set_for_action_kind` 和 `renderer_for_action_kind`。

- [ ] **Step 1：写失败的测试**
  - `mcp_types.rs` 里的 `read_only_needs_the_hint_and_no_destructive_hint`：
    - `{"name":"a","annotations":{"readOnlyHint":true}}` 为 true；
    - 同时带 `"destructiveHint":true` 为 false；
    - 不带 annotations 为 false；
    - `"readOnlyHint":false` 为 false。
  - `registry.rs` 里的 `a_read_only_mcp_tool_is_a_read_action`：`read_only: true` 时得到 `ActionKind::Read`，`false` 时得到 `ActionKind::Write`。
  - `client.rs` 里的 `a_tool_marked_read_only_by_its_server_is_read_only`：让现有的假传输在 `tools/list` 里给两个工具，只有一个带只读标注。注册表中前者 `read_only == true`，后者为 `false`。
- [ ] **Step 2：确认测试失败**：`cargo nextest run -p orca-core -p orca-mcp -p orca-tools --lib --locked --retries 0 read_only`
- [ ] **Step 3：实现**，并补齐所有构造处。
- [ ] **Step 4：更新文档**：两份 `mcp-integration.mdx` 各加一节"只读工具"（英文 "Read-only tools"），写明三点：
  - 服务器声明为只读的工具在 suggest 模式下不询问，在 plan 模式下可用；
  - 是否只读以服务器的声明为准；
  - 标了 `destructiveHint` 的工具照常询问。
- [ ] **Step 5：确认测试通过**：先跑 Step 2 的命令，再跑 `cargo nextest run -p orca-core -p orca-mcp -p orca-tools --all-targets --locked --profile ci --retries 0`
- [ ] **Step 6：提交**：`feat(mcp): treat tools their server marks read-only as reads`

### Task 2：权限规则能匹配 MCP 工具（设计 B）

**Files:**
- Modify: `crates/orca-core/src/approval_rules.rs`（`PermissionRule` 在第 5 行附近，`CompiledPermissionRule::matches` 在第 92 行附近）
- Modify: `crates/orca-core/src/mcp_types.rs`
- Test: `crates/orca-approval/src/policy.rs`
- Modify: `site/src/docs/md/{en,zh}/approval-modes.mdx`

**Interfaces:**
- Produces `orca_core::mcp_types::mcp_tool_server(tool: &str) -> Option<&str>`：
  - 对 `mcp__<server>__<tool>` 返回 `server`，即 `mcp__` 之后到下一个 `__` 之前的部分；
  - 服务器部分或工具部分为空时返回 `None`，其他形式也返回 `None`。
- `PermissionRule.pattern` 改为 `#[serde(default = "any_target")] pub pattern: String`，`any_target()` 返回 `"*"`。
- `matches(tool, target)` 改为 `matches_tool(tool) && pattern.matches(target.unwrap_or(""))`。
- `matches_tool` 在以下两种情况下成立：
  - 与调用的工具名完全相同；
  - 规则的 `tool` 是 `mcp__<s>` 或 `mcp__<s>__*`，而且 `mcp_tool_server(调用的工具名) == Some(<s>)`。

- [ ] **Step 1：写失败的测试**（规则从 TOML 解析）
  - `a_rule_without_a_pattern_matches_a_call_without_a_target`：`tool = "mcp__github__create_issue"` 的 allow 规则，对 `("mcp__github__create_issue", None)` 得到 `Some(Allow)`。
  - `a_rule_with_a_specific_pattern_does_not_match_a_call_without_a_target`：`pattern = "src/**"` 的规则对无目标调用得到 `None`。
  - `a_server_rule_matches_only_that_servers_tools`：`mcp__github__*` 和 `mcp__github` 两种写法都匹配 `mcp__github__create_issue`、`mcp__github__list`，都不匹配 `mcp__githubx__tool`、`mcp__gitlab__x`、`bash`。
  - `deny_still_wins_over_a_server_allow`：`delete_repo` 的 deny 优先于服务器级 allow。
  - `mcp_tool_server_reads_the_server_segment`：
    - `mcp__github__create_issue` → `Some("github")`
    - `mcp__a__b__c` → `Some("a")`
    - `mcp__github`、`mcp____x`、`bash` → `None`
  - `policy.rs` 里两个测试，规则都是 allow `mcp__github__*`、请求都是写动作、目标为 `None`：
    - `a_rule_can_allow_an_mcp_tool_in_suggest_mode`：suggest 模式下得到 `Allow`；
    - `a_rule_cannot_lift_plan_for_an_mcp_tool`：plan 模式下得到 `Deny`。
- [ ] **Step 2：确认测试失败**：`cargo nextest run -p orca-core -p orca-approval --lib --locked --retries 0 without_a_pattern without_a_target server_rule deny_still_wins mcp_tool_server mcp_tool_in_suggest lift_plan`
- [ ] **Step 3：实现**
- [ ] **Step 4：更新文档**：权限规则一节写明四点：
  - `pattern` 可以省略；
  - MCP 工具名写成 `mcp__<server>__<tool>`；
  - `mcp__<server>__*` 表示整个服务器；
  - 附一个 `tool = "mcp__github__*"`、`decision = "allow"` 的例子。
- [ ] **Step 5：确认测试通过**：先跑 Step 2 的命令，再跑 `cargo nextest run -p orca-core -p orca-approval --all-targets --locked --profile ci --retries 0`
- [ ] **Step 6：提交**：`feat(approval): let permission rules name MCP tools and servers`

### Task 3：新配置字段和按工具筛选（设计 I）

**Files:**
- Modify: `crates/orca-core/src/mcp_types.rs`（`McpTransportKind`、`McpServerConfig`）
- Modify: `crates/orca-mcp/src/client.rs`（`connect_server`）、`crates/orca-mcp/src/transport.rs`（`connect`）
- Modify: 编译器报出的对 `McpTransportKind` 的 match，其中包括 runtime 界面层声明 `SurfaceMcpTransport`

**Interfaces（Produces）:**
- `McpTransportKind` 新增 `Http`，序列化为 `"http"`。在任务 5 之前，`connect` 里的 `Http` 与 `Sse` 走同一个传输。
- 界面层的 `SurfaceMcpTransport` 新增 `Http { url, headers }`，与 `Sse` 同形。
- `McpServerConfig` 新增以下字段，都加 `#[serde(default, skip_serializing_if = "Option::is_none")]`：
  - `bearer_token_env_var: Option<String>`
  - `oauth_client_id: Option<String>`
  - `oauth_callback_port: Option<u16>`
  - `enabled_tools: Option<Vec<String>>`
  - `disabled_tools: Option<Vec<String>>`
- `pub fn tool_is_enabled(config: &McpServerConfig, tool_name: &str) -> bool`：
  - `enabled_tools` 为 `Some` 时只放行列表里的工具；
  - 然后排除 `disabled_tools` 里的工具；
  - 用服务器给出的原始工具名比较。
- `connect_server` 在注册工具前用 `tool_is_enabled` 过滤。

- [ ] **Step 1：写失败的测试**
  - `tool_filters_apply_the_allow_list_then_the_deny_list`：
    - 两者都没有时全部放行；
    - 只有白名单 `[a,b]` 时放行 a、b；
    - 白名单 `[a,b]` 加黑名单 `[b]` 时只放行 a；
    - 只有黑名单 `[c]` 时放行 c 以外的全部。
  - `filtered_tools_are_not_registered`（client.rs，假传输给出 a、b、c，配置 `disabled_tools = ["b"]`）：注册表只有 a 和 c。
  - `new_server_fields_round_trip_and_stay_absent_when_unset`：TOML 带 `transport = "http"` 和上面五个字段时能解析；全部为 None 时序列化结果不含这些键。
- [ ] **Step 2：确认测试失败**：`cargo nextest run -p orca-core -p orca-mcp --lib --locked --retries 0 tool_filters filtered_tools new_server_fields`
- [ ] **Step 3：实现**，并补齐所有 match。
- [ ] **Step 4：确认测试通过**：先跑 Step 2 的命令，再跑 `cargo nextest run -p orca-core -p orca-mcp -p orca-runtime --lib --locked --profile ci --retries 0 mcp`
- [ ] **Step 5：提交**：`feat(mcp): add http transport, auth, and tool filter settings`

### Task 4：`orca mcp add/list/get/remove`（设计 D，不含登录）

**Files:**
- Create: `crates/orca-core/src/config/user_edit.rs`，在 `config/mod.rs` 里加 `pub mod user_edit;`
- Modify: `crates/orca-core/src/config/file.rs`：`persist_user_model_settings_to_dir` 改用新的公共函数，原有测试保持通过。
- Create: `crates/orca-runtime/src/command/mcp.rs`，在 `command/mod.rs` 里加 `pub mod mcp;`
- Modify: `src/cli.rs`：`Command` 枚举和分发，照 `Trust` 的写法。
- Modify: `tests/cli_architecture_contract.rs`（根帮助命令列表加 `"mcp"`），`tests/public_docs_contract.rs`（从 `FORBIDDEN_PUBLIC_CLI_CLAIMS` 里删掉 `"orca mcp"`）
- Modify: `site/src/docs/md/{en,zh}/cli-reference.mdx`、`site/src/docs/md/{en,zh}/mcp-integration.mdx`

**Interfaces（Produces）:**

`orca_core::config::user_edit`：
- `pub(crate) fn edit_user_config_in(dir: &Path, edit: impl FnOnce(&mut toml_edit::DocumentMut) -> io::Result<()>) -> io::Result<PathBuf>`
  - 文件不存在时从空文档开始；
  - 解析失败时报错，文案沿用 `persist_user_model_settings`；
  - `edit` 返回错误时不写入。
- `pub fn validate_mcp_server_name(name: &str) -> Result<(), String>`，错误文案：`invalid MCP server name '{name}': use letters, digits, '-' and '_', without '__'`
- `pub fn add_user_mcp_server(server: &McpServerConfig) -> io::Result<PathBuf>`，以及 `add_user_mcp_server_in(dir, server)`
  - 重名时报错：`MCP server '{name}' already exists in {path}; remove it first with 'orca mcp remove {name}'`
  - 只写非空字段；`env` 和 `headers` 写成内联表。
- `pub fn remove_user_mcp_server(name: &str) -> io::Result<PathBuf>`，以及 `remove_user_mcp_server_in`
  - 删除所有同名的表；
  - 一个都没有时报错：`no MCP server named '{name}' in {path}`
- `pub fn list_user_mcp_servers() -> io::Result<(PathBuf, Vec<McpServerConfig>)>`，以及 `list_user_mcp_servers_in`
- 以上函数遇到 `mcp_servers` 存在但不是表数组时报错：`mcp_servers in {path} is not an array of tables; edit it by hand`

`orca_runtime::command::mcp`：
- `pub enum McpCommandRequest { Add(McpAddRequest), List { json: bool }, Get { name: String, json: bool }, Remove { name: String } }`
  - 任务 8 会再加上 `Login` 和 `Logout`。
- `pub struct McpAddRequest`，字段：
  - `name: String`
  - `transport: Option<String>`
  - `env: Vec<String>`
  - `url: Option<String>`
  - `headers: Vec<String>`
  - `bearer_token_env_var: Option<String>`
  - `client_id: Option<String>`
  - `callback_port: Option<u16>`
  - `command: Vec<String>`
- `pub fn run(request: McpCommandRequest) -> i32`
- `pub fn run_in(config_dir: &Path, request: McpCommandRequest, stdout: &mut impl Write, stderr: &mut impl Write) -> i32`

规则：
- `command` 不为空时是 stdio 服务器。
- 给了 `url` 时，传输方式由 `--transport` 决定，只能是 `http` 或 `sse`，默认 `http`。
- 参数格式错误的文案：
  - `-e` 必须是 `KEY=VALUE`，否则报 `invalid --env value '{v}': expected KEY=VALUE`；
  - `--header` 在第一个 `:` 处拆分，否则报 `invalid --header value '{v}': expected 'Name: value'`；
  - `--transport` 只能是 `http` 或 `sse`。

输出：
- add 成功：`added MCP server {name} to {path}`；remove 成功：`removed MCP server {name} from {path}`。
- list：每行 `{name}\t{transport}\t{target}`，停用的服务器行尾加 `\tdisabled`。`target` 是命令加参数，或者 url。没有服务器时输出 `no MCP servers configured in {path}`。
- get：以 `key: value` 形式逐行输出：
  - name、transport、command、args、url；
  - env 和 headers 只列键名；
  - bearer_token_env_var、oauth_client_id、oauth_callback_port、startup/tool 超时、enabled_tools、disabled_tools、disabled。
- `--json`：list 输出对象数组，get 输出单个对象，字段与文本输出相同，同样不含任何值。
- 失败：stderr 输出 `orca: {message}`，退出码为 1。

CLI：
- `Mcp(McpArgs)` 的说明写 `/// Add, list, show, or remove MCP servers in the user config.`
- `add` 的参数：
  - `name`；
  - `-e/--env`，与 `--url` 冲突；
  - `--url`、`--transport`；
  - `--header`，需要 `--url`；
  - `--bearer-token-env-var`、`--client-id`、`--callback-port`，这三个都需要 `--url`；
  - `-- <COMMAND>...`（`last = true`）。
- `--url` 和命令二者恰好给一个，用 ArgGroup 约束。

- [ ] **Step 1：写失败的测试**（`command/mcp.rs`，用临时目录调 `run_in`）
  - `add_keeps_the_rest_of_the_config`：
    - 原文件是 `"# keep me\nmodel = \"deepseek-flash\"\n"`，执行 add `docs`，`-e API_KEY=secret`，命令 `npx -y docs-mcp`；
    - 退出码 0，文件仍含 `# keep me` 和 model 那一行；
    - 解析 `FileConfig` 后得到一个 stdio 服务器，命令、参数、环境变量都与输入一致。
  - `add_writes_a_remote_server`：
    - `--url https://example.com/mcp --header "Authorization: Bearer t" --bearer-token-env-var GH` 得到 transport `Http`，url、headers、bearer_token_env_var 都与输入一致；
    - 加上 `--transport sse` 时得到 `Sse`。
  - `add_rejects_a_duplicate_or_invalid_name`：`a__b`、`bad name` 和重名都返回 1，给出上面的错误文案，且文件字节不变。
  - `list_and_get_hide_secret_values`：
    - list 输出含 `docs\tstdio\tnpx -y docs-mcp`，get 输出含 `env: API_KEY`；
    - 两者的文本和 `--json` 输出都不含 `secret` 和 `Bearer t`。
  - `remove_deletes_the_server_and_reports_a_missing_one`
  - `an_unparsable_config_is_never_overwritten`：原文件为 `"model = ["`，add 返回 1，文件字节不变。
  - `env_and_header_values_must_be_well_formed`
  - `tests/cli_architecture_contract.rs` 的命令列表加上 `"mcp"`。
- [ ] **Step 2：确认测试失败**：
  - `cargo nextest run -p orca-runtime --lib --locked --retries 0 command::mcp`
  - `cargo nextest run -p blade-deepseek --test cli_architecture_contract --locked --retries 0`
- [ ] **Step 3：实现**
- [ ] **Step 4：更新文档**
  - `cli-reference` 的根命令表加 `orca mcp`，并新增 `## orca mcp` 一节，写明：
    - 各子命令的用法，`login` 和 `logout` 在任务 8 补上；
    - `--transport` 的默认值；
    - 只写用户配置；
    - 改动在新会话生效。
  - `mcp-integration` 开头改用 `orca mcp add` 举例。
  - 从禁止列表里删掉 `"orca mcp"`。
- [ ] **Step 5：确认测试通过**：先跑 Step 2 的两条命令，再跑：
  - `cargo nextest run -p orca-core --lib --locked --retries 0 config`
  - `cargo nextest run -p blade-deepseek --test public_docs_contract --locked --retries 0`
  - `npm --prefix site run build`
- [ ] **Step 6：提交**：`feat(cli): add orca mcp add, list, get, and remove`

### Task 5：按规范实现 streamable HTTP（设计 E 第一部分）

**Files:**
- Modify: `crates/orca-mcp/src/transport.rs`：
  - `SseTransport` 在第 804 行附近，就地改名为 `StreamableHttpTransport`；
  - `initialize` 在第 832 行附近；
  - stdio 的 `initialize` 在第 319 行附近；
  - `request_sse_with_client` 在第 1400 行附近，以及异步请求路径。

**Interfaces（Produces）:**
- `pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";`
- `pub const SUPPORTED_MCP_PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];`
- stdio 和 HTTP 的初始化都声明 `MCP_PROTOCOL_VERSION`。服务器返回的版本不在支持列表中时，报 `MCP server '{name}' requires protocol version {v}, which Orca does not support`。
- HTTP 请求：
  - 每个请求都带 `Accept: application/json, text/event-stream`；
  - 初始化响应头里的 `Mcp-Session-Id` 保存下来，之后每个请求都带上；
  - 初始化之后的请求带 `MCP-Protocol-Version: <协商结果>`；
  - 通知收到 200 或 202 都算成功；
  - 带会话 ID 的请求收到 404 时：清空会话，重新 `initialize` 并发 `notifications/initialized`，原请求重试一次，重试仍失败就报错；
  - 传输对象被丢弃时发一次 DELETE（带会话 ID，超时 2 秒，忽略失败）。
- `connect`：`Http` 和 `Sse` 都用 `StreamableHttpTransport`，任务 6 改为 `Sse` 带回退。

- [ ] **Step 1：写失败的测试**（本地 `TcpListener` 充当 HTTP 服务器，参照本文件已有的原始 HTTP 测试）
  - `http_requests_accept_json_and_event_streams`：`Accept` 头缺少任意一种类型时，服务器回 406；有了该请求头后初始化成功。
  - `http_session_id_is_sent_after_initialize`：初始化响应带 `Mcp-Session-Id: s1`，之后的 `tools/list` 请求头含 `Mcp-Session-Id: s1`。
  - `http_requests_carry_the_negotiated_protocol_version`：服务器回 `2025-03-26`，之后的请求带 `MCP-Protocol-Version: 2025-03-26`。
  - `http_notifications_accept_202`
  - `an_expired_http_session_is_reinitialized_once`：
    - 第一次 `tools/call` 返回 404，服务器随后收到第二次 `initialize` 和重试的 `tools/call`；
    - 结果正常；
    - 连续两次 404 时报错。
  - `json_and_event_stream_responses_both_work`
  - `an_unsupported_protocol_version_is_reported`：服务器回 `1999-01-01`，报上面那条错误。
  - `stdio_declares_the_current_protocol_version`：用 stdio 脚本记录收到的初始化请求，声明的版本为 `2025-06-18`。
- [ ] **Step 2：确认测试失败**：`cargo nextest run -p orca-mcp --lib --locked --retries 0 http_ json_and_event unsupported_protocol stdio_declares`
- [ ] **Step 3：实现**
- [ ] **Step 4：确认测试通过**：先跑 Step 2 的命令，再跑 `cargo nextest run -p orca-mcp --all-targets --locked --profile ci --retries 0`，已有测试全部通过。
- [ ] **Step 5：提交**：`feat(mcp): speak streamable HTTP by the spec`

### Task 6：旧版 SSE 和自动回退（设计 E 第二部分）

**Files:**
- Create: `crates/orca-mcp/src/legacy_sse.rs`，在 `lib.rs` 里声明。
- Modify: `crates/orca-mcp/src/transport.rs`（`connect`）

**Interfaces（Produces）:**
- `pub(crate) struct LegacySseTransport`，实现 `McpTransport`：
  - `GET <url>`，带 `Accept: text/event-stream`，打开事件流；
  - 第一个 `endpoint` 事件的数据按原 url 解析出 POST 地址。协议、主机、端口任一与原 url 不同就报 `MCP server '{name}' sent an endpoint on another origin: {endpoint}`；
  - 每个请求 POST 到该地址，收到 2xx 即可；
  - 响应从事件流里的 `message` 事件按 id 取回：后台读线程通过 `HashMap<id, Sender>` 分发，每个请求各自计超时；
  - 事件流上的 `elicitation/create` 请求复用 `handle_elicitation_create_request` 处理，应答通过 POST 发回；
  - 读线程在事件流断开时，让所有等待中的请求失败，失败原因写明流已断开，文案为 `MCP SSE event stream closed`。
  - 把这条文案加进 `crates/orca-mcp/src/client.rs` 的 `should_reconnect_after_mcp_error`，使下一次调用自动重连。
- `connect`：`Sse` 返回一层包装。它的 `initialize` 先按 streamable HTTP 初始化，收到 HTTP 400、404 或 405 时改用 `LegacySseTransport` 初始化，之后所有调用都交给成功的那一个。

- [ ] **Step 1：写失败的测试**（本地服务器同时提供 GET 事件流和 POST 端点）
  - `legacy_sse_reads_responses_from_the_event_stream`：通过事件流完成 `initialize`、`tools/list` 和 `tools/call`。
  - `legacy_sse_rejects_a_cross_origin_endpoint`
  - `sse_falls_back_to_legacy_when_post_is_not_allowed`：对原 url 的 POST 返回 405，后续请求全部经由旧版 SSE。
  - `sse_keeps_using_streamable_http_when_it_works`：服务器支持 streamable HTTP，从未收到 GET。
  - `a_closed_event_stream_fails_pending_requests`
- [ ] **Step 2：确认测试失败**：`cargo nextest run -p orca-mcp --lib --locked --retries 0 legacy_sse sse_falls_back sse_keeps closed_event_stream`
- [ ] **Step 3：实现**
- [ ] **Step 4：确认测试通过**：先跑 Step 2 的命令，再跑 `cargo nextest run -p orca-mcp --all-targets --locked --profile ci --retries 0`
- [ ] **Step 5：提交**：`feat(mcp): support legacy SSE servers and fall back to them`

### Task 7：认证核心，包括 bearer 环境变量、凭据存储和 OAuth 流程（设计 F 第一部分）

**Files:**
- Create: `crates/orca-core/src/config/mcp_credentials.rs`，在 `config/mod.rs` 里声明。
- Create: `crates/orca-mcp/src/oauth.rs`，在 `lib.rs` 里声明。
- Modify: `crates/orca-mcp/src/transport.rs`：给 HTTP 和旧版 SSE 加上认证。
- Modify: `crates/orca-platform/src/process.rs`：新增打开网址的函数。
- Modify: `crates/orca-mcp/Cargo.toml`：加入工作区已有的 `serde`、`sha2`、`base64`、`url`、`uuid`。

**Interfaces（Produces）:**

`orca_core::config::mcp_credentials`：
- `#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)] pub struct McpCredential`，字段：
  - `server_url: String`
  - `access_token: String`
  - `refresh_token: Option<String>`
  - `expires_at: Option<u64>`（Unix 秒）
  - `token_endpoint: String`
  - `client_id: String`
  - `resource: String`
  - `scope: Option<String>`
- 文件为 JSON 对象，以服务器名为键。函数：
  - `pub fn mcp_credentials_path() -> Option<PathBuf>`，返回 `config_dir()/mcp-credentials.json`；
  - `pub fn load_mcp_credential(path: &Path, server: &str, server_url: &str) -> io::Result<Option<McpCredential>>`，`server_url` 不一致时返回 `None`；
  - `pub fn save_mcp_credential(path: &Path, server: &str, credential: &McpCredential) -> io::Result<()>`；
  - `pub fn delete_mcp_credential(path: &Path, server: &str) -> io::Result<bool>`。

`orca_mcp`：
- `pub const MCP_AUTH_REQUIRED: &str = "MCP server requires login";`
- `pub fn is_auth_required(error: &str) -> bool`，即 `error.starts_with(MCP_AUTH_REQUIRED)`。

`orca_mcp::oauth`：
- `pub struct McpLoginOptions`，字段：
  - `pub credentials_path: PathBuf`
  - `pub open_browser: Box<dyn FnOnce(&str) -> io::Result<()> + Send>`
  - `pub callback_timeout: Duration`（默认 300 秒）
- `pub fn login(server: &McpServerConfig, options: McpLoginOptions) -> Result<(), String>`
- `pub(crate) fn refresh(server: &McpServerConfig, credentials_path: &Path, credential: &McpCredential) -> Result<McpCredential, String>`：成功后写回凭据文件。

登录流程：
1. **发现**：向服务器 url 发一次无认证的 `initialize`，期望收到 401。
2. **受保护资源元数据**：取 `WWW-Authenticate` 里的 `resource_metadata`；没有时用同源的 `/.well-known/oauth-protected-resource`。
3. **授权服务器元数据**：先按 RFC 8414 取 `/.well-known/oauth-authorization-server`（issuer 带路径时插在路径前），再尝试 `/.well-known/openid-configuration`。
4. **客户端 ID**：有 `oauth_client_id` 就用它；否则向 `registration_endpoint` POST 以下内容做动态注册：
   `{"client_name":"Orca","redirect_uris":[<回调>],"grant_types":["authorization_code","refresh_token"],"response_types":["code"],"token_endpoint_auth_method":"none"}`
5. **授权地址**：参数为 `response_type=code`、`client_id`、`redirect_uri`、`code_challenge`（S256）、`code_challenge_method=S256`、`state`、`resource=<服务器 url>`，以及 `scope`（资源元数据的 `scopes_supported` 用空格连接，没有就省略）。
   - verifier 用两个 uuid v4 的 simple 形式拼接；
   - state 用一个 uuid v4。
6. **回调**：在 `127.0.0.1:<oauth_callback_port 或 0>` 监听，只接受 `GET /callback`：
   - `state` 不一致时报错；
   - 带 `error` 参数时报错；
   - 否则回一个简短页面 `Login complete. You can close this window.`。
   - 到 `callback_timeout` 仍未收到回调，报 `timed out waiting for the browser login for MCP server '{name}'` 并释放端口。
7. **换取 token**：`grant_type=authorization_code` 加上 `code`、`redirect_uri`、`client_id`、`code_verifier`、`resource`，结果存入凭据文件。
8. **刷新**：`grant_type=refresh_token` 加上 `refresh_token`、`client_id`、`resource`。

传输层的认证：
- 配置了静态 `Authorization` 头时，照原样发送。
- 否则有 `bearer_token_env_var` 时，发送 `Authorization: Bearer $VAR`。变量缺失时连接失败，报 `environment variable {VAR} for MCP server '{name}' is not set`。
- 都没有时，从凭据文件加载 token。token 在 60 秒内就要过期、或者请求收到 401 时，有 refresh token 就刷新一次并重试；否则报 `{MCP_AUTH_REQUIRED}: run 'orca mcp login {name}'`。

`orca_platform::process`：`pub fn open_url(url: &str) -> io::Result<()>`，启动后不等待：
- macOS 用 `open`；
- Linux 和其他 Unix 用 `xdg-open`；
- Windows 用 `cmd /C start "" <url>`。

- [ ] **Step 1：写失败的测试**
  - `orca-core`：
    - `credentials_round_trip_and_delete`；
    - `a_credential_for_another_url_is_ignored`；
    - `credentials_are_written_privately`：`#[cfg(unix)]`，文件权限为 `0o600`。
  - `orca-mcp`：写一个本地测试夹具，在一个 `TcpListener` 上按路径分发，同时充当资源服务器和授权服务器。测试里的 `open_browser` 在新线程里 GET 授权地址，然后跟随 302 请求回调地址。
    - `login_discovers_registers_and_stores_tokens`：
      - 依次经过 401、资源元数据、授权服务器元数据、动态注册、授权、换 token；
      - 凭据文件里 `access_token == "at-1"`、`refresh_token == Some("rt-1")`；
      - 夹具记录到的授权请求带有 `code_challenge_method=S256` 和 `resource`。
    - `login_uses_a_configured_client_id_without_registering`
    - `callback_rejects_a_mismatched_state`
    - `login_times_out_when_no_callback_arrives`：`callback_timeout` 设为 200 毫秒，报超时错误，之后能重新绑定同一个端口。
    - `an_expired_token_is_refreshed_once`：凭据已过期，请求带上刷新得到的 `at-2` 后成功。
    - `a_failed_refresh_reports_login_required`：错误满足 `is_auth_required`。
    - `bearer_token_env_var_sets_the_authorization_header`（设置测试进程自己的环境变量），以及 `a_missing_bearer_env_var_is_reported`。
- [ ] **Step 2：确认测试失败**：`cargo nextest run -p orca-core -p orca-mcp --lib --locked --retries 0 credential login_ callback_rejects expired_token failed_refresh bearer_token`
- [ ] **Step 3：实现**
- [ ] **Step 4：确认测试通过**：
  - 先跑 Step 2 的命令；
  - 再跑 `cargo nextest run -p orca-core -p orca-mcp -p orca-platform --all-targets --locked --profile ci --retries 0`；
  - 最后跑 `node scripts/validate-windows-platform-boundaries.mjs`，`open_url` 触动了平台边界计数时更新 manifest。
- [ ] **Step 5：提交**：`feat(mcp): log in to remote servers with OAuth`

### Task 8：`orca mcp login/logout` 和登录状态（设计 F 第二部分）

**Files:**
- Modify: `crates/orca-runtime/src/command/mcp.rs`、`src/cli.rs`
- Modify: `site/src/docs/md/{en,zh}/cli-reference.mdx`、`site/src/docs/md/{en,zh}/mcp-integration.mdx`

**Interfaces（Produces）:**
- `McpCommandRequest` 新增 `Login { name: String }` 和 `Logout { name: String }`。
- `run_in_with_browser(config_dir, credentials_path, request, stdout, stderr, open_browser: Box<dyn FnOnce(&str) -> io::Result<()> + Send>) -> i32` 供测试使用。`run` 用 `orca_platform::process::open_url`，并先打印：
  - `Opening your browser to log in to MCP server {name}.`
  - `If it does not open, visit: {url}`
- 各种结果：
  - 登录成功：`logged in to MCP server {name}`；
  - stdio 服务器，或已配置静态认证的服务器：报 `MCP server '{name}' does not use OAuth login`；
  - `logout` 成功：`logged out of MCP server {name}`；
  - 没有保存的 token 时：`MCP server '{name}' is not logged in`。
- `list` 每行末尾加一列认证状态，`get` 加一行 `auth: ...`，取值：
  - `oauth: logged in`
  - `oauth: not logged in`
  - `bearer: $VAR`
  - `static header`
  - `-`（stdio）
- `remove` 同时删除该服务器保存的 token。

- [ ] **Step 1：写失败的测试**
  - `login_stores_a_credential_and_logout_removes_it`：复用任务 7 的夹具，并由测试注入浏览器函数。
  - `status_output_never_contains_tokens`：凭据文件预先写入 token，list、get 和 `--json` 的输出都不含它。
  - `remove_also_deletes_the_saved_token`
  - `login_refuses_servers_without_oauth`
- [ ] **Step 2：确认测试失败**：`cargo nextest run -p orca-runtime --lib --locked --retries 0 command::mcp`
- [ ] **Step 3：实现**
- [ ] **Step 4：更新文档**
  - `cli-reference` 补上 `login` 和 `logout`。
  - `mcp-integration` 加一节"远程服务器与登录"（英文 "Remote servers and login"），内容包括：
    - `http` 与 `sse` 的区别；
    - `bearer_token_env_var`；
    - OAuth 登录和 token 的存放位置；
    - 登录后，已开的会话在 `/mcp` 里重连。
- [ ] **Step 5：确认测试通过**：先跑 Step 2 的命令，再跑 `cargo nextest run -p blade-deepseek --test public_docs_contract --locked --retries 0` 和 `npm --prefix site run build`
- [ ] **Step 6：提交**：`feat(cli): add orca mcp login and logout`

### Task 9：批准面板"总是允许"并保存（设计 C）

**Files:**
- Modify: `crates/orca-core/src/config/user_edit.rs`
- Modify: `crates/orca-tui/src/types.rs`：`ApprovalOption` 在第 250 行附近，`options_for` 在第 323 行附近，允许列表在第 1642–1665 行附近。
- Modify: `crates/orca-tui/src/approval_actions.rs`
- Modify: 批准面板对应的 golden frame（如果有）；`site/src/docs/md/{en,zh}/approval-modes.mdx`、`mcp-integration.mdx`

**Interfaces:**
- Consumes：任务 2 的 `mcp_tool_server`；任务 4 的 `edit_user_config_in`。
- Produces：
  - `pub fn add_user_allow_rule(tool: &str) -> io::Result<bool>`，以及 `add_user_allow_rule_in(dir, tool)`：
    - 往 `[[permissions.rules]]` 追加 `tool = "{tool}"`、`decision = "allow"`，不写 pattern；
    - 已有 tool 相同、decision 为 allow、pattern 缺省或为 `"*"` 的规则时，不写入并返回 `false`。
  - 两个新的批准选项，`legacy_key` 与 `key` 相同：
    - `ApprovalOption::AlwaysToolSaved`：按键 `'5'`，标签 `"always allow this tool (saved)"`；
    - `ApprovalOption::AlwaysServerSaved`：按键 `'6'`，标签 `"always allow this server (saved)"`。
  - `AlwaysTool` 的标签改为 `"allow for this session"`。
  - `options_for`：`mcp_tool_server(tool)` 为 `Some` 时返回 `[Once, AlwaysTool, AlwaysToolSaved, AlwaysServerSaved, Deny]`。
  - `AppState::approval_key_mcp_server(server) -> String` 返回 `format!("mcp__{server}__*")`。同服务器的工具在 `approval_is_allowlisted` 里返回 true。
  - 选中保存选项后：
    1. 先加入会话允许列表；
    2. 再调用 `add_user_allow_rule`，工具级传工具名，服务器级传 `mcp__{server}__*`；
    3. 两个选项在 `permission_decision_for` 里都映射为 `AllowSession`。
  - 保存失败时，按 `state_reducer.rs` 处理 `TuiEvent::Notice` 的方式推入一条 `ChatMessage::System`，内容为 `Allowed for this session, but saving the rule to the user config failed: {error}`。

- [ ] **Step 1：写失败的测试**（写文件的测试把 `ORCA_HOME` 设为临时目录）
  - `saving_an_allow_rule_appends_once`：
    - 连续保存两次，依次返回 true、false；
    - 手写的等价规则（`pattern = "*"`）也算已存在。
  - `saving_an_allow_rule_keeps_comments`：原文件带注释和一条 bash 规则，保存后仍在；解析后得到两条规则。
  - `mcp_tools_offer_saved_allow_options`：MCP 工具得到上面那组 5 个选项；`options_for("bash", Some("ls"))` 不含保存选项。
  - `saving_a_server_allow_covers_the_servers_other_tools_this_session`：
    - 同服务器的其他工具为 true，其他服务器为 false；
    - 配置文件里多了 `tool = "mcp__github__*"`。
  - `a_failed_save_still_allows_and_says_why`：配置文件为 `"model = ["`，仍然放行，系统消息含 `saving the rule to the user config failed`。
  - 已有的 `always_options_map_to_a_session_scoped_grant` 扩展到两个新选项。
- [ ] **Step 2：确认测试失败**：
  - `cargo nextest run -p orca-core --lib --locked --retries 0 allow_rule`
  - `cargo nextest run -p orca-tui --lib --locked --retries 0 saved_allow server_allow failed_save always_options`
- [ ] **Step 3：实现**
- [ ] **Step 4：更新文档**
  - `approval-modes` 的选项表：选项 3 写"本会话允许"；新增 5、6，说明只对 MCP 工具出现、保存到用户配置、保存失败时本会话照样允许。
  - `mcp-integration` 加一节"总是允许"（英文 "Always allow"）。
- [ ] **Step 5：确认测试通过**：先跑 Step 2 的两条命令，再跑 `cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0` 和 `cargo nextest run --test tui_pty_contract --locked --profile ci-serial --retries 0`
- [ ] **Step 6：提交**：`feat(tui): save always-allow choices for MCP tools and servers`

### Task 10：注册表可重连，runtime 填充 MCP 目录（设计 G 的 runtime 部分）

**Files:**
- Modify: `crates/orca-mcp/src/client.rs`：`McpRegistry` 在第 18 行附近，`initialize_registry` 在第 103 行附近，`reconnect` 在第 1052 行附近。
- Modify: `crates/orca-runtime/src/runtime_surface/projection.rs`（`SurfaceMcpServerStatus` 在第 2058 行附近）、`runtime_surface/commands.rs`（`RuntimeSurfaceClientHandle` 和派发 trait）
- Modify: `crates/orca-runtime/src/runtime_host.rs`：`ThreadSurfaceDispatcher` 在第 6006 行附近，`ThreadCommand` 在第 4913 行附近，处理在第 18992 行附近，都照 `SurfaceReadTaskTranscript` 的写法。线程启动时的目录快照在第 11099 行附近。
- Modify: 线程 actor 里提交界面事件的地方，参照 `thread_actor_operation.rs` 第 3604 行附近提交 `PinnedContext` 的写法。

**Interfaces（Produces）:**

orca-mcp：
- `pub enum McpServerState { Ready, Failed { message: String }, NeedsLogin, Disabled }`
- `McpRegistry` 的内部状态改为可以替换：`Arc<RwLock<McpRegistryInner>>`；`tools()` 改为返回 `Vec<McpTool>`；已有调用方随之调整。
- `McpRegistry::server_states(&self) -> Vec<(String, McpServerState)>`：
  - 按配置顺序列出所有服务器，包括连接失败的和停用的；
  - `is_auth_required` 的失败记为 `NeedsLogin`。
- `McpRegistry::reconnect_server(&self, name: &str) -> Result<(), String>`：
  - 用保存的配置重新连接；
  - 只替换这个服务器的客户端、工具和查找表；
  - 失败时更新它的状态并返回错误；
  - 其他服务器不受影响。

界面层：
- `SurfaceMcpServerStatus` 新增 `AuthRequired`。
- `pub enum SurfaceMcpServerAction { Reconnect }`
- `RuntimeSurfaceClientHandle::mcp_server_control(&self, request_id: SurfaceRequestId, server: NonEmptyText, action: SurfaceMcpServerAction) -> Result<SurfaceMcpServerStatus, SurfaceClientCommandError>`：
  - 所需权限与 `update_settings` 相同；
  - actor 调用 `reconnect_server`，再提交 `McpCatalogPatch::Reconciled`，返回新状态。
- 线程启动、界面上线后，提交一次 `Reconciled`：
  - `servers` 来自 `server_states()`：`Ready` 映射为 `Ready`，`Failed` 映射为 `Degraded { message }`，`NeedsLogin` 映射为 `AuthRequired`，`Disabled` 映射为 `Disabled`；
  - `tools` 来自注册表；
  - resources 仍为空。

- [ ] **Step 1：写失败的测试**
  - orca-mcp：
    - `reconnecting_a_server_replaces_only_its_tools`：用两个 stdio 脚本服务器。第一个重连后工具列表改变，第二个的工具不变。
    - `a_failed_server_is_listed_with_its_error`
    - `a_server_that_needs_login_reports_needs_login`：远程夹具返回 401。
    - `disabled_servers_are_listed`
  - orca-runtime：
    - `the_mcp_catalog_lists_servers_and_tools_after_start`
    - `reconnect_updates_the_catalog`
    - `the_next_request_sees_reconnected_tools`：重连后，下一轮的工具注册表包含新工具，因为工具注册表每次调用都从注册表重建。
- [ ] **Step 2：确认测试失败**：`cargo nextest run -p orca-mcp -p orca-runtime --lib --locked --retries 0 reconnect failed_server needs_login disabled_servers mcp_catalog reconnected_tools`
- [ ] **Step 3：实现**
- [ ] **Step 4：确认测试通过**：先跑 Step 2 的命令，再跑 `cargo nextest run -p orca-mcp -p orca-runtime --lib --locked --profile ci --retries 0`，以及两个 validator。
- [ ] **Step 5：提交**：`feat(runtime): reconnect MCP servers and publish their status`

### Task 11：MCP prompts 的获取与展开（设计 H 的 runtime 部分）

**Files:**
- Modify: `crates/orca-core/src/mcp_types.rs`、`crates/orca-mcp/src/transport.rs`、`crates/orca-mcp/src/legacy_sse.rs`、`crates/orca-mcp/src/client.rs`（`McpServerCapabilities` 在第 38 行附近）
- Modify: 界面层的 `projection.rs`（`SurfaceMcpCatalogSnapshot` 在第 2123 行附近）、`commands.rs`，以及 `runtime_host.rs`

**Interfaces（Produces）:**

orca-core：
- `McpPrompt { server, name, description: Option<String>, arguments: Vec<McpPromptArgument> }`
- `McpPromptArgument { name, description: Option<String>, required: bool }`
- 以及 `prompts/list` 和 `prompts/get` 结果的反序列化类型。

orca-mcp：
- `McpTransport` 新增两个方法：
  - `fn list_prompts(&self) -> Result<Value, String>`
  - `fn get_prompt(&self, name: &str, arguments: Value) -> Result<Value, String>`
  - 默认实现返回 `Err("prompts are not supported by this transport")`；stdio、HTTP、旧版 SSE 都要实现。
- `McpServerCapabilities` 新增 `prompts: bool`。连接声明了 `prompts` 能力的服务器时调用 `prompts/list`。
- `McpRegistry::prompts(&self) -> Vec<McpPrompt>`
- `McpRegistry::get_prompt(&self, server: &str, prompt: &str, arguments: &BTreeMap<String, String>) -> Result<McpPromptExpansion, String>`
  - `pub struct McpPromptExpansion { pub text: String, pub images: Vec<ImageInput> }`
  - 按消息顺序处理：文本内容用 `"\n\n"` 连接；图片经 `tool_image` 校验后放进 images，被拒的图片把说明附进文本；嵌入资源取 `resource.text`。

界面层：
- `SurfaceMcpPrompt { server: NonEmptyText, name: NonEmptyText, description: Option<DisplayText>, arguments: Vec<SurfaceMcpPromptArgument> }`
- `SurfaceMcpPromptArgument { name: NonEmptyText, description: Option<DisplayText>, required: bool }`
- `SurfaceMcpCatalogSnapshot` 新增 `#[serde(default, skip_serializing_if = "Vec::is_empty")] prompts: Vec<SurfaceMcpPrompt>`。
- `RuntimeSurfaceClientHandle::expand_mcp_prompt(&self, request_id, server: NonEmptyText, prompt: NonEmptyText, arguments: Vec<(String, String)>) -> Result<Result<SurfaceMcpPromptExpansion, DisplayText>, SurfaceClientCommandError>`
  - `SurfaceMcpPromptExpansion { text: String, images: Vec<ImageInput> }`
  - 所需权限与 `read_task_transcript` 相同。

- [ ] **Step 1：写失败的测试**
  - `prompts_are_listed_for_servers_that_offer_them`
  - `a_prompt_expands_to_text_and_images`：两条消息，一条文本、一条 PNG。
  - `an_unknown_prompt_is_an_error`
  - `the_catalog_includes_prompts`
  - `expand_mcp_prompt_returns_the_expansion`
  - `an_old_catalog_without_prompts_still_deserializes`
- [ ] **Step 2：确认测试失败**：`cargo nextest run -p orca-mcp -p orca-runtime --lib --locked --retries 0 prompt`
- [ ] **Step 3：实现**
- [ ] **Step 4：确认测试通过**：先跑 Step 2 的命令，再跑 `cargo nextest run -p orca-mcp -p orca-runtime --lib --locked --profile ci --retries 0`，以及两个 validator。
- [ ] **Step 5：提交**：`feat(mcp): list MCP prompts and expand them on request`

### Task 12：TUI `/mcp` 面板（设计 G 的 TUI 部分）

**Files:**
- Modify: `crates/orca-tui/src/commands/mod.rs`（`SlashCommand` 和 `all_commands`）、`slash_command_actions.rs`（参照 `SlashCommand::Config` 打开对话框的写法）
- Modify: `crates/orca-tui/src/surface_projection.rs`（`SurfaceProjectionState`）、`state_reducer.rs`（`apply_surface_projection_state`）、`types.rs`（`AppState`、对话框类型）
- Modify: `crates/orca-tui/src/ui.rs`（渲染）、按键处理文件、`protocol.rs`（`UserAction`）、`action_dispatcher.rs`
- Modify: `crates/orca-tui/src/cli.rs`（`run_attached` 在第 70 行附近，给挂接会话打上标记）
- Modify: `site/src/docs/md/{en,zh}/terminal-ui.mdx`、`mcp-integration.mdx`

**Interfaces:**
- Consumes：任务 10 的 `mcp_server_control`、目录快照和 `AuthRequired`；任务 7 的 `oauth::login` 和 `delete_mcp_credential`；任务 11 的 prompts。
- Produces：
  - `SlashCommand::Mcp`，解析 `/mcp`。`all_commands` 加入 `("/mcp", "Manage MCP servers")`。
  - TUI 侧的视图类型，由目录快照映射而来：
    - `McpCatalogView { servers: Vec<McpServerView>, tools: Vec<McpToolView>, prompts: Vec<McpPromptView> }`
    - `McpServerView { name: String, status: McpServerStatusView }`，其中 `McpServerStatusView` 取值为 `Connected`、`Failed(String)`、`NeedsLogin`、`Disabled`、`Starting`
    - `McpToolView { server: String, name: String, read_only: bool }`
    - `McpPromptView { server: String, name: String, description: Option<String>, arguments: Vec<(String, bool)> }`，元组依次是参数名和是否必填。
  - `SurfaceProjectionState` 新增 `mcp_catalog: McpCatalogView`，`AppState` 保存它。
  - `AppState::mcp_dialog: Option<McpDialog { selected: usize, showing_details: bool }>`
  - 渲染：
    - 每个服务器一行，状态文字为以下之一：
      - `connected · {n} tools`
      - `failed: {message}`
      - `needs login`
      - `disabled`
    - 详情里列出工具（只读的标 `read-only`）和 prompts。该服务器在 TUI 自己的配置里设置了 `enabled_tools` 或 `disabled_tools` 时，详情里也显示这两项。
    - 底部按键提示为 `↑↓ select · Enter details · r reconnect · l log in · o log out · Esc close`，用 `chrome.rs` 的提示样式。
  - 三个 `UserAction`：
    - `McpReconnect { server }`：调用 `mcp_server_control(Reconnect)`，结果以 `TuiEvent::Notice` 显示。
    - `McpLogin { server }`：在工作线程里跑 `oauth::login`，用配置里的服务器信息和 `open_url`，完成后自动重连。等待期间显示 `waiting for browser login for {server}…`。
    - `McpLogout { server }`：删除 token 后重连。
  - 挂接会话：`AppState` 记录该会话来自 `run_attached`。在挂接会话里执行 `/mcp` 只给出提示：`MCP management is not available in an attached daemon session; run 'orca mcp login <name>' on this machine, then reattach.`

- [ ] **Step 1：写失败的测试**
  - `slash_mcp_opens_the_panel_with_each_server_status`：目录里有四种状态，渲染文本分别含对应状态文字。
  - `details_mark_read_only_tools_and_list_prompts`
  - `r_sends_a_reconnect_for_the_selected_server`，以及 `l_and_o_send_login_and_logout`
  - `attached_sessions_explain_mcp_is_unavailable`
  - `the_catalog_from_the_projection_reaches_app_state`
- [ ] **Step 2：确认测试失败**：`cargo nextest run -p orca-tui --lib --locked --retries 0 slash_mcp details_mark r_sends l_and_o attached_sessions catalog_from_the_projection`
- [ ] **Step 3：实现**
- [ ] **Step 4：更新文档**：`terminal-ui` 加上 `/mcp` 和按键说明；`mcp-integration` 指向 `/mcp`。
- [ ] **Step 5：确认测试通过**：
  - 先跑 Step 2 的命令；
  - 再跑 `cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0` 和 `cargo nextest run --test tui_pty_contract --locked --profile ci-serial --retries 0`；
  - 最后跑两个 validator，有固定值变化时同步更新。
- [ ] **Step 6：提交**：`feat(tui): add the /mcp panel`

### Task 13：MCP prompt 斜杠命令（设计 H 的 TUI 部分）

**Files:**
- Modify: `crates/orca-tui/src/commands/mod.rs`（动态命令列表，参照 skills 和 workflows 的做法）、斜杠菜单构建处、`slash_command_actions.rs`、`protocol.rs`、`action_dispatcher.rs`
- Modify: `site/src/docs/md/{en,zh}/terminal-ui.mdx`、`mcp-integration.mdx`

**Interfaces:**
- Consumes：任务 11 的目录 prompts 和 `expand_mcp_prompt`，任务 12 的 `AppState` 目录。
- Produces：
  - 斜杠菜单：从目录 prompts 生成 `/mcp__{server}__{prompt}` 条目，说明为 prompt 的 description，后面加上参数名（如 `<pr> [branch]`）。
  - `SlashCommand::McpPrompt { server: String, prompt: String, args: String }`：只在输入与目录里的某个 prompt 相符时解析成这个命令。
  - 参数映射 `pub(crate) fn map_prompt_arguments(prompt: &McpPromptView, args: &str) -> Result<Vec<(String, String)>, String>`：
    - 按空白切分，依次对应声明的参数，最后一个参数接收剩下的全部文本；
    - 缺少必填参数时返回 `usage: /mcp__{server}__{prompt} <a> [b]`。
  - 执行：`UserAction::RunMcpPrompt { server, prompt, arguments }` 调用 `expand_mcp_prompt`：
    - 成功时，把展开结果当成用户输入提交：文本加图片附件，走与输入框提交相同的 `SubmitWithMentions` 路径；
    - 失败时推入错误消息，不发送。

- [ ] **Step 1：写失败的测试**
  - `prompt_commands_appear_in_the_slash_menu`
  - `positional_arguments_map_with_the_last_taking_the_rest`：参数为 `a b`，输入 `1 two words` 得到 `a=1`、`b=two words`。
  - `a_missing_required_argument_shows_usage`
  - `running_a_prompt_submits_its_expansion`：展开结果被提交为用户消息，图片成为附件。
  - `an_unknown_mcp_command_is_not_parsed_as_a_prompt`
- [ ] **Step 2：确认测试失败**：`cargo nextest run -p orca-tui --lib --locked --retries 0 prompt_commands positional_arguments missing_required running_a_prompt unknown_mcp_command`
- [ ] **Step 3：实现**
- [ ] **Step 4：更新文档**：`terminal-ui` 和 `mcp-integration` 加上 prompt 命令的用法和参数规则。
- [ ] **Step 5：确认测试通过**：先跑 Step 2 的命令，再跑 `cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0`，以及两个 validator。
- [ ] **Step 6：提交**：`feat(tui): run MCP prompts as slash commands`

### Task 14：端到端验证与收尾

**Files:**
- Create: `tests/mcp_cli_contract.rs`（`#![cfg(unix)]`）

**Interfaces（Consumes）:**
- 前面各任务的命令和行为。
- 隐藏参数 `--provider mock`：mock 模型会调用提示词里写出的工具。
- `--approval-mode suggest` 下需要批准时，exec 以 3 退出，最后的状态为 `approval_required`（见 `tests/approval_contract.rs`）。
- 用 shell 脚本写 MCP 服务器，参照 `tests/session_server_contract.rs` 的 `write_slow_mcp_server`。

- [ ] **Step 1：写测试**（每个测试都用临时的 `ORCA_HOME` 和工作目录）

  脚本服务器 `fx` 提供两个工具：`lookup`（只读标注）和 `write_note`。调用任一工具都返回 `fixture called`。

  - `mcp_commands_round_trip_the_user_config`：通过真实二进制依次执行 add、list、get、remove，逐项核对输出和退出码。
  - `a_read_only_mcp_tool_runs_in_suggest_mode_without_asking`：运行 `exec --output-format jsonl --provider mock --approval-mode suggest mcp__fx__lookup`，退出码 0，没有 `approval.requested`，输出含 `fixture called`。
  - `a_write_mcp_tool_still_asks_in_suggest_mode`：退出码 3，最后的状态为 `approval_required`。
  - `a_permission_rule_allows_an_mcp_tool_in_suggest_mode`：追加 `tool = "mcp__fx__*"` 的 allow 规则后，`write_note` 退出码 0。
  - `a_remote_http_server_added_by_url_works`：
    - 测试进程起一个线程，用 `TcpListener` 提供 streamable HTTP 服务：检查 `Accept` 头，响应带会话 ID，提供一个只读工具；
    - `orca mcp add web --url http://127.0.0.1:<port>/mcp`；
    - 在 suggest 模式下调用该工具成功。
- [ ] **Step 2：运行**：`cargo nextest run -p blade-deepseek --test mcp_cli_contract --locked --retries 0`，任何失败都回到对应任务修复。
- [ ] **Step 3：全量检查**，依次运行：
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --locked`
  - 两个 validator
  - `cargo nextest run --workspace --all-targets --locked --profile ci --no-fail-fast --retries 0`
  - `cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0`
  - `cargo nextest run --test tui_pty_contract --locked --profile ci-serial --retries 0`
  - `npm --prefix site run build`
- [ ] **Step 4：提交**：`test: cover orca mcp and MCP approvals end to end`
