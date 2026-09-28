# MCP 审批与 `orca mcp` 命令 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 服务器标了只读的 MCP 工具不再每次询问；权限规则能允许或禁止 MCP 工具；批准面板可以"总是允许"并保存到配置；新增 `orca mcp add/list/remove` 命令，一条命令加好服务器。

**Architecture:**
- MCP 客户端从 `tools/list` 读出只读标注，存在 `McpTool` 上，工具注册表据此决定动作类型。
- 规则匹配允许调用不带目标，并认识服务器级写法。
- 对用户配置的写入集中在 orca-core 新文件 `config/user_edit.rs` 里，用 `toml_edit` 保留原内容，`orca mcp` 命令和 TUI 的"保存"选项都用它。

**Tech Stack:** Rust（orca-core、orca-mcp、orca-tools、orca-approval、orca-runtime、orca-tui、根包 `blade-deepseek`），clap，toml_edit。

**Spec:** `docs/superpowers/specs/2026-09-28-mcp-approvals-and-cli-design.md`

## Global Constraints

- 所有改动都要在 Linux、Windows、macOS 上编译并通过测试。用 shell 脚本做 MCP 服务器的测试加 `#[cfg(unix)]`。
- 只写用户配置 `$ORCA_HOME/config.toml`（默认 `~/.orca/config.toml`）。项目配置本来就不能定义 `mcp_servers` 和 `permissions`。
- 写配置的做法与 `persist_user_model_settings` 相同，缺一不可：
  - 用 `toml_edit` 保留原有内容和注释；
  - 用 `orca_platform::fs::atomic_write(.., AtomicWritePolicy::NoFollow)` 原子写入；
  - 配置解析失败时报错，且不覆盖原文件。
- MCP 工具名的格式是 `mcp__<server>__<tool>`。服务器名只能用字母、数字、`-`、`_`，不能含 `__`。
- 只读的判定条件：`readOnlyHint == true` 且 `destructiveHint != true`。
- 新增的可序列化字段为默认值时不输出（`skip_serializing_if`），旧数据照常读取。
- 代码和面向用户的输出用英文；中英文档分别更新。
- 每次提交前必须通过：`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked`（无错误）、`node scripts/validate-runtime-surface-contract.mjs`、`node scripts/validate-windows-platform-boundaries.mjs`。
- 测试用 `cargo nextest run`，不用 `cargo test`，因为 orca-runtime 的库测试共用 ORCA_HOME，会互相干扰。

## Review Focus

1. **手写过的配置**：有注释、有其他表。`orca mcp add/remove` 和"保存允许"只改动相关条目，其余内容原样保留。由任务 3 的 `add_keeps_the_rest_of_the_config` 和任务 4 的 `saving_an_allow_rule_keeps_comments` 钉住。
2. **解析失败的配置**：`orca mcp add` 以非零退出并说明原因，文件字节不变。TUI 的"保存允许"在这种情况下本会话照样放行，并提示保存失败。由任务 3 的 `an_unparsable_config_is_never_overwritten` 和任务 4 的 `a_failed_save_still_allows_and_says_why` 钉住。
3. **名字相近的服务器**：`mcp__github__*` 不能匹配 `mcp__githubx__tool`。由任务 2 的 `a_server_rule_matches_only_that_servers_tools` 钉住。
4. **同时标了只读和破坏性的工具**：按写处理，照样询问。由任务 1 的 `read_only_needs_the_hint_and_no_destructive_hint` 钉住。
5. **重复保存**：同一个"总是允许"选两次只写一条规则；手写的等价规则（`pattern = "*"`）也算已存在。由任务 4 的 `saving_an_allow_rule_appends_once` 钉住。

---

### Task 1: 只读 MCP 工具按只读处理

**Files:**
- Modify: `crates/orca-core/src/mcp_types.rs`（`McpTool` 在第 68 行附近，`McpToolDescriptor` 在第 121 行附近）
- Modify: `crates/orca-mcp/src/client.rs`（`connect_server` 里构造 `McpTool`，在第 167 行附近）
- Modify: `crates/orca-tools/src/registry.rs`（`McpProxyTool::new` 在第 2207 行附近）
- Modify: 编译器报出的其他构造 `McpTool { .. }` 的地方，补 `read_only: false`（已知有 `crates/orca-mcp/src/client.rs` 的测试、`crates/orca-tools/src/registry.rs` 的测试、`tests/provider_contract.rs`）
- Modify: `site/src/docs/md/{en,zh}/mcp-integration.mdx`

**Interfaces:**
- Produces（`orca_core::mcp_types`）：
  - `#[derive(Clone, Debug, Default, Deserialize)] pub struct McpToolAnnotations { #[serde(rename = "readOnlyHint", default)] pub read_only_hint: Option<bool>, #[serde(rename = "destructiveHint", default)] pub destructive_hint: Option<bool> }`
  - `McpToolDescriptor` 新增 `#[serde(default)] pub annotations: Option<McpToolAnnotations>`
  - `impl McpToolDescriptor { pub fn is_read_only(&self) -> bool }`
  - `McpTool` 新增 `#[serde(default, skip_serializing_if = "std::ops::Not::not")] pub read_only: bool`
- `McpProxyTool::new`：动作类型为 `if tool.read_only { ActionKind::Read } else { ActionKind::Write }`，`capabilities` 和 `renderer` 分别用同文件已有的 `capability_set_for_action_kind` 和 `renderer_for_action_kind` 求出。

- [ ] **Step 1：写失败的测试**

  `mcp_types.rs` 的测试：

```rust
#[test]
fn read_only_needs_the_hint_and_no_destructive_hint() {
    let parse = |json: &str| serde_json::from_str::<McpToolDescriptor>(json).unwrap();
    assert!(parse(r#"{"name":"a","annotations":{"readOnlyHint":true}}"#).is_read_only());
    assert!(!parse(r#"{"name":"b","annotations":{"readOnlyHint":true,"destructiveHint":true}}"#).is_read_only());
    assert!(!parse(r#"{"name":"c"}"#).is_read_only());
    assert!(!parse(r#"{"name":"d","annotations":{"readOnlyHint":false}}"#).is_read_only());
}
```

  `registry.rs` 的测试，`a_read_only_mcp_tool_is_a_read_action`：
  - `McpProxyTool::new` 传入 `read_only: true` 的 `McpTool`，`spec().capabilities.action_kind() == ActionKind::Read`；
  - 传入 `read_only: false`，得到 `ActionKind::Write`。

  `client.rs` 的测试，`a_tool_marked_read_only_by_its_server_is_read_only`：
  - 用已有的假传输，让 `tools/list` 返回两个工具，只有一个带 `"annotations":{"readOnlyHint":true}`；
  - 注册表列出的 `McpTool` 中，前者 `read_only == true`，后者 `false`。

- [ ] **Step 2：运行，确认失败**

  Run: `cargo nextest run -p orca-core -p orca-mcp -p orca-tools --lib --locked --retries 0 read_only`
  Expected：编译失败或测试失败，因为字段和方法还不存在。

- [ ] **Step 3：实现上述接口**，补齐所有 `McpTool` 构造处。

- [ ] **Step 4：更新文档**

  在两份 `mcp-integration.mdx` 里加一小节"只读工具"（英文 "Read-only tools"），写明三点：
  - 服务器声明为只读的工具（`readOnlyHint`）在 suggest 模式下不再询问，plan 模式下可以用；
  - 是否只读以服务器声明为准，Orca 不校验；
  - 同时标了 `destructiveHint` 的工具照常询问。

- [ ] **Step 5：运行测试，确认通过**

  运行 Step 2 的命令，再运行 `cargo nextest run -p orca-core -p orca-mcp -p orca-tools --all-targets --locked --profile ci --retries 0`。
  Expected：全部通过。

- [ ] **Step 6：提交**

```bash
git add crates/orca-core crates/orca-mcp crates/orca-tools tests site/src/docs/md
git commit -m "feat(mcp): treat tools their server marks read-only as reads"
```

### Task 2：权限规则能匹配 MCP 工具

**Files:**
- Modify: `crates/orca-core/src/approval_rules.rs`（`PermissionRule` 在第 5 行附近，`CompiledPermissionRule::matches` 在第 92 行附近）
- Modify: `crates/orca-core/src/mcp_types.rs`（新增名字解析函数）
- Test: `crates/orca-approval/src/policy.rs` 的测试模块
- Modify: `site/src/docs/md/{en,zh}/approval-modes.mdx`（"Permission rules" / 对应中文小节）

**Interfaces:**
- Produces（`orca_core::mcp_types`）：`pub fn mcp_tool_server(tool: &str) -> Option<&str>`
  - 对 `mcp__<server>__<tool>` 返回 `server`：取 `mcp__` 之后到下一个 `__` 之前的部分，并要求它和后面的工具部分都非空。
  - 其他形式返回 `None`。
- `PermissionRule.pattern` 改为 `#[serde(default = "any_target")] pub pattern: String`，`fn any_target() -> String` 返回 `"*"`。
- 匹配规则：`matches(tool, target)` 等价于 `self.matches_tool(tool) && self.pattern.matches(target.unwrap_or(""))`。
- `matches_tool` 满足以下任一条件即命中：
  - 规则的 `tool` 与调用的工具名完全相同；
  - 规则的 `tool` 是 `mcp__<s>` 或 `mcp__<s>__*`，且 `mcp_tool_server(调用的工具名) == Some(<s>)`。
- Consumed by：任务 4（`mcp_tool_server`、规则语义）、任务 5。

- [ ] **Step 1：写失败的测试**

  `approval_rules.rs` 的测试，规则都从 TOML 解析，比如 `toml::from_str::<PermissionRules>("[[rules]]\ntool = \"mcp__github__create_issue\"\ndecision = \"allow\"\n")`：
  - `a_rule_without_a_pattern_matches_a_call_without_a_target`：`matching_decision("mcp__github__create_issue", None) == Some(Decision::Allow)`。
  - `a_rule_with_a_specific_pattern_does_not_match_a_call_without_a_target`：`pattern = "src/**"` 的规则，对 `None` 目标返回 `None`。
  - `a_server_rule_matches_only_that_servers_tools`：`tool = "mcp__github__*"` 和 `tool = "mcp__github"` 各自满足：
    - 匹配 `mcp__github__create_issue` 和 `mcp__github__list`；
    - 不匹配 `mcp__githubx__tool`、`mcp__gitlab__x`、`bash`。
  - `deny_still_wins_over_a_server_allow`：同时有 `mcp__github__*` allow 和 `mcp__github__delete_repo` deny 时，`delete_repo` 得到 `Decision::Deny`。

  `mcp_types.rs` 的测试，`mcp_tool_server_reads_the_server_segment`：
  - `mcp__github__create_issue` 得到 `Some("github")`；
  - `mcp__a__b__c` 得到 `Some("a")`；
  - `mcp__github` 和 `mcp____x` 得到 `None`；
  - `bash` 得到 `None`。

  `policy.rs` 的测试：
  - `a_rule_can_allow_an_mcp_tool_in_suggest_mode`：suggest 模式，规则 allow `mcp__github__*`，写动作请求，`resolve_for_tool(&req, "mcp__github__create_issue", None)` 得到 `ApprovalDecision::Allow`。
  - `a_rule_cannot_lift_plan_for_an_mcp_tool`：同样的规则在 plan 模式下得到 `Deny`。

- [ ] **Step 2：运行，确认失败**

  Run: `cargo nextest run -p orca-core -p orca-approval --lib --locked --retries 0 without_a_pattern without_a_target server_rule deny_still_wins mcp_tool_server mcp_tool_in_suggest lift_plan`
  Expected：失败。

- [ ] **Step 3：实现上述接口**

- [ ] **Step 4：更新文档**

  `approval-modes.mdx` 的权限规则一节：
  - `pattern` 可以省略，省略时匹配任何目标，也匹配没有目标的调用，比如 MCP 工具；
  - MCP 工具名写成 `mcp__<server>__<tool>`；
  - `mcp__<server>__*`（或 `mcp__<server>`）匹配整个服务器；
  - 加一个 `tool = "mcp__github__*"`、`decision = "allow"` 的例子。

- [ ] **Step 5：运行测试，确认通过**

  运行 Step 2 的命令，再运行 `cargo nextest run -p orca-core -p orca-approval --all-targets --locked --profile ci --retries 0`。
  Expected：全部通过，已有规则测试不变。

- [ ] **Step 6：提交**

```bash
git add crates/orca-core crates/orca-approval site/src/docs/md
git commit -m "feat(approval): let permission rules name MCP tools and servers"
```

### Task 3：`orca mcp add/list/remove`

**Files:**
- Create: `crates/orca-core/src/config/user_edit.rs`，并在 `crates/orca-core/src/config/mod.rs` 里 `pub mod user_edit;`
- Modify: `crates/orca-core/src/config/file.rs`（`persist_user_model_settings_to_dir` 在第 582 行附近，改用新的共用函数）
- Create: `crates/orca-runtime/src/command/mcp.rs`，并在 `crates/orca-runtime/src/command/mod.rs` 里 `pub mod mcp;`
- Modify: `src/cli.rs`（`Command` 枚举在第 60 行附近；分发在第 825 行附近，照 `Trust` 的写法）
- Modify: `tests/cli_architecture_contract.rs`（根帮助命令列表加 `"mcp"`）、`tests/public_docs_contract.rs`（从 `FORBIDDEN_PUBLIC_CLI_CLAIMS` 删掉 `"orca mcp"`）
- Modify: `site/src/docs/md/{en,zh}/cli-reference.mdx`、`site/src/docs/md/{en,zh}/mcp-integration.mdx`

**Interfaces:**
- Produces（`orca_core::config::user_edit`）：
  - `pub(crate) fn edit_user_config_in(dir: &Path, edit: impl FnOnce(&mut toml_edit::DocumentMut) -> io::Result<()>) -> io::Result<PathBuf>`
    - 读取 `dir/config.toml`，文件不存在时从空文档开始；
    - 解析失败时返回错误，文案沿用 `persist_user_model_settings` 的写法；
    - 执行 `edit` 后原子写入，返回文件路径；
    - `edit` 返回错误时不写文件。
  - `pub fn validate_mcp_server_name(name: &str) -> Result<(), String>`，出错文案：`invalid MCP server name '{name}': use letters, digits, '-' and '_', without '__'`
  - `pub fn add_user_mcp_server(server: &McpServerConfig) -> io::Result<PathBuf>`，以及 `add_user_mcp_server_in(dir: &Path, server: &McpServerConfig) -> io::Result<PathBuf>`
    - 名字不合法时报错；
    - 重名时报错，文案：`MCP server '{name}' already exists in {path}; remove it first with 'orca mcp remove {name}'`；
    - 往 `[[mcp_servers]]` 追加一个表，只写这些字段：`name`、`transport`（`"stdio"` 或 `"sse"`）、`command`、`args`、`env`（内联表）、`url`、`headers`（内联表）。空字段不写。
  - `pub fn remove_user_mcp_server(name: &str) -> io::Result<PathBuf>`，以及 `remove_user_mcp_server_in(dir: &Path, name: &str) -> io::Result<PathBuf>`
    - 删除所有同名的表；
    - 一个都没有时报错，文案：`no MCP server named '{name}' in {path}`。
  - `pub fn list_user_mcp_servers() -> io::Result<(PathBuf, Vec<McpServerConfig>)>`，以及 `list_user_mcp_servers_in(dir: &Path) -> io::Result<(PathBuf, Vec<McpServerConfig>)>`；文件不存在时返回空列表。
  - 不带 `_in` 的版本都用 `config_dir()` 作为目录。
  - `mcp_servers` 已存在但不是表数组时报错，文案：`mcp_servers in {path} is not an array of tables; edit it by hand`。
- Produces（`orca_runtime::command::mcp`）：
  - `pub enum McpCommandRequest { Add(McpAddRequest), List, Remove { name: String } }`
  - `pub struct McpAddRequest { pub name: String, pub env: Vec<String>, pub url: Option<String>, pub headers: Vec<String>, pub command: Vec<String> }`
  - `pub fn run(request: McpCommandRequest) -> i32`
  - `pub fn run_in(config_dir: &Path, request: McpCommandRequest, stdout: &mut impl Write, stderr: &mut impl Write) -> i32`
  - 行为：
    - `command` 非空时为 stdio：`command[0]` 是命令，其余是参数。`url` 有值时为 SSE。
    - `-e` 必须是 `KEY=VALUE`，否则报 `invalid --env value '{v}': expected KEY=VALUE`。
    - `--header` 在第一个 `:` 处切开并去掉两边空白，没有 `:` 时报 `invalid --header value '{v}': expected 'Name: value'`。
  - 输出：
    - add 成功：`added MCP server {name} to {path}`
    - remove 成功：`removed MCP server {name} from {path}`
    - list：每行 `{name}\t{transport}\t{target}`，停用的服务器行尾加 `\tdisabled`。`target` 是命令加参数（空格连接）或 url。没有服务器时输出 `no MCP servers configured in {path}`。
    - 失败时 stderr 输出 `orca: {message}`，退出码 1。
- CLI（`src/cli.rs`）：
  - 新增 `/// Add, list, or remove MCP servers in the user config.` `Mcp(McpArgs)`，子命令 `Add(McpAddArgs)`、`List`、`Remove { name: String }`。
  - `McpAddArgs` 的参数：
    - `name`；
    - `-e/--env <KEY=VALUE>`，可重复，与 `--url` 冲突；
    - `--url <URL>`；
    - `--header <'Name: value'>`，可重复，需要 `--url`；
    - `-- <COMMAND>...`，即 `#[arg(last = true)]`。
  - `--url` 和命令二者必须恰好有一个，用 clap 的 ArgGroup 约束。

- [ ] **Step 1：写失败的测试**

  `crates/orca-runtime/src/command/mcp.rs` 的测试（用 `tempfile::tempdir()` 作为 `config_dir` 调 `run_in`）：
  - `add_keeps_the_rest_of_the_config`：
    - 先写入 `"# keep me\nmodel = \"deepseek-flash\"\n"`；
    - 执行 add `docs`，`env = ["API_KEY=secret"]`，`command = ["npx", "-y", "docs-mcp"]`；
    - 退出码 0，stdout 以 `added MCP server docs to` 开头；
    - 文件仍包含 `# keep me` 和 `model = "deepseek-flash"`；
    - 用 `toml::from_str::<orca_core::config::file::FileConfig>` 解析，得到一个服务器：transport stdio、command `npx`、args `["-y","docs-mcp"]`、env `API_KEY=secret`。
  - `add_writes_an_sse_server_with_headers`：`url = Some("https://example.com/sse")`，`headers = ["Authorization: Bearer t"]`，解析后 transport 为 SSE、url 相同、headers 里 `Authorization` 的值为 `Bearer t`。
  - `add_rejects_a_duplicate_or_invalid_name`：
    - 名字 `a__b` 和 `bad name` 都返回 1，stderr 含 `invalid MCP server name`；
    - 重复添加 `docs` 返回 1，stderr 含 `already exists`；
    - 这三种情况下文件字节都不变。
  - `list_hides_env_and_header_values`：先添加带 env 和 header 的服务器，list 输出含 `docs\tstdio\tnpx -y docs-mcp`，但不含 `secret` 和 `Bearer t`。
  - `remove_deletes_the_server_and_reports_a_missing_one`：
    - remove 后 list 输出 `no MCP servers configured in`；
    - 再 remove 一次返回 1，stderr 含 `no MCP server named 'docs'`。
  - `an_unparsable_config_is_never_overwritten`：写入 `"model = ["`，add 返回 1，文件字节不变。
  - `env_and_header_values_must_be_well_formed`：`-e FOO` 和 `--header NoColon` 分别返回 1，并给出上面的文案。

  `tests/cli_architecture_contract.rs`：`root_binary_exposes_the_supported_command_surface` 的命令列表加上 `"mcp"`。

- [ ] **Step 2：运行，确认失败**

  Run:
  - `cargo nextest run -p orca-runtime --lib --locked --retries 0 command::mcp`
  - `cargo nextest run -p blade-deepseek --test cli_architecture_contract --locked --retries 0`

  Expected：失败。

- [ ] **Step 3：实现**
  - 实现 `user_edit.rs` 并改写 `persist_user_model_settings_to_dir` 用 `edit_user_config_in`，它已有的测试保持通过。
  - 实现 `command/mcp.rs`。
  - 在 `src/cli.rs` 接好子命令。

- [ ] **Step 4：更新文档**

  - `cli-reference.mdx`（中英）：
    - 根命令表加 `orca mcp`；
    - 新增 `## orca mcp` 一节，列出三条子命令的用法；
    - 写明 `--url` 只支持 SSE，只写用户配置，改完需要重开会话才生效。
  - `mcp-integration.mdx`（中英）：开头改为先用 `orca mcp add` 举例，手写 `[[mcp_servers]]` 作为第二种方式。
  - `tests/public_docs_contract.rs`：从禁止列表删掉 `"orca mcp"`。

- [ ] **Step 5：运行测试，确认通过**

  运行 Step 2 的命令，再运行：
  - `cargo nextest run -p orca-core --lib --locked --retries 0 config`
  - `cargo nextest run -p blade-deepseek --test public_docs_contract --locked --retries 0`
  - `npm --prefix site run build`

  Expected：全部通过。

- [ ] **Step 6：提交**

```bash
git add crates/orca-core crates/orca-runtime src tests site/src/docs/md
git commit -m "feat(cli): add orca mcp add, list, and remove"
```

### Task 4：批准面板"总是允许"并保存

**Files:**
- Modify: `crates/orca-core/src/config/user_edit.rs`（新增保存规则的函数）
- Modify: `crates/orca-tui/src/types.rs`：
  - `ApprovalOption` 在第 250 行附近；
  - `ApprovalDialog::options_for` 在第 323 行附近；
  - 允许列表的键和判断在第 1642–1665 行附近。
- Modify: `crates/orca-tui/src/approval_actions.rs`：`resolve_approval_option` 和 `permission_decision_for`。
- Modify: 若有 golden frame 显示了批准面板上的 "always allow"，一并更新。
- Modify: `site/src/docs/md/{en,zh}/approval-modes.mdx`（"Answer an approval" 表）、`site/src/docs/md/{en,zh}/mcp-integration.mdx`

**Interfaces:**
- Consumes：
  - 任务 2 的 `orca_core::mcp_types::mcp_tool_server` 和规则语义；
  - 任务 3 的 `edit_user_config_in`。
- Produces（`orca_core::config::user_edit`）：`pub fn add_user_allow_rule(tool: &str) -> io::Result<bool>`，以及 `add_user_allow_rule_in(dir: &Path, tool: &str) -> io::Result<bool>`
  - 往 `[[permissions.rules]]` 追加 `tool = "{tool}"`、`decision = "allow"`，不写 `pattern`。
  - 已存在 `tool` 相同、`decision = "allow"`、且 `pattern` 缺省或为 `"*"` 的规则时不写，返回 `false`；写入时返回 `true`。
- Produces（orca-tui）：
  - `ApprovalOption::AlwaysToolSaved`：按键 `'5'`，标签 `"always allow this tool (saved)"`。
  - `ApprovalOption::AlwaysServerSaved`：按键 `'6'`，标签 `"always allow this server (saved)"`。
  - 两者的 `legacy_key` 与 `key` 相同。
  - `ApprovalOption::AlwaysTool` 的标签改为 `"allow for this session"`。
  - `ApprovalDialog::options_for`：当 `mcp_tool_server(tool)` 为 `Some` 时，返回 `[Once, AlwaysTool, AlwaysToolSaved, AlwaysServerSaved, Deny]`；其他工具保持原样。
  - `AppState::approval_key_mcp_server(server: &str) -> String`，返回 `format!("mcp__{server}__*")`。`approval_is_allowlisted` 在工具属于该服务器时也返回 true。
  - 选中两个"保存"选项后的处理：
    - `AlwaysToolSaved`：把工具键加入会话允许列表，再调用 `add_user_allow_rule(&dialog.tool)`。
    - `AlwaysServerSaved`：加入服务器键，再调用 `add_user_allow_rule(&format!("mcp__{server}__*"))`。
    - 两者的 `permission_decision_for` 都映射为 `TuiPermissionDecision::AllowSession`。
    - 保存失败时往对话里加一条系统消息，写法与 `state_reducer.rs` 处理 `TuiEvent::Notice` 相同（`ChatMessage::System`），文案为 `Allowed for this session, but saving the rule to the user config failed: {error}`。本次调用照常放行。

- [ ] **Step 1：写失败的测试**

  `user_edit.rs` 的测试：
  - `saving_an_allow_rule_appends_once`：
    - 连续两次 `add_user_allow_rule_in(dir, "mcp__github__*")`，依次返回 `true`、`false`，文件里只有一条规则；
    - 手写的 `tool = "mcp__x__y"`、`pattern = "*"`、`decision = "allow"` 规则也视为已存在，调用返回 `false`。
  - `saving_an_allow_rule_keeps_comments`：原文件 `"# mine\n[[permissions.rules]]\ntool = \"bash\"\npattern = \"cargo *\"\ndecision = \"allow\"\n"` 保存后仍含 `# mine` 和原规则，并多了新规则；用 `toml::from_str::<FileConfig>` 解析，得到两条规则。

  orca-tui 的测试（需要写文件的测试，把 `ORCA_HOME` 设为临时目录）：
  - `mcp_tools_offer_saved_allow_options`：
    - `ApprovalDialog::options_for("mcp__github__create_issue", None)` 返回上面那组五个选项；
    - `options_for("bash", Some("ls"))` 不含两个保存选项。
  - `saving_a_server_allow_covers_the_servers_other_tools_this_session`：
    - 对 `mcp__github__create_issue` 选 `AlwaysServerSaved` 后，`approval_is_allowlisted("mcp__github__list_issues", None)` 为 true；
    - `approval_is_allowlisted("mcp__gitlab__x", None)` 为 false；
    - `ORCA_HOME/config.toml` 里有 `tool = "mcp__github__*"` 的规则。
  - `a_failed_save_still_allows_and_says_why`：
    - `ORCA_HOME/config.toml` 预先写入 `"model = ["`；
    - 选 `AlwaysToolSaved` 后，发给 runtime 的是允许，对话里新增的系统消息含 `saving the rule to the user config failed`。
  - 已有的 `always_options_map_to_a_session_scoped_grant` 扩展到两个新选项。

- [ ] **Step 2：运行，确认失败**

  Run:
  - `cargo nextest run -p orca-core --lib --locked --retries 0 allow_rule`
  - `cargo nextest run -p orca-tui --lib --locked --retries 0 saved_allow server_allow failed_save always_options`

  Expected：失败。

- [ ] **Step 3：实现上述接口**

- [ ] **Step 4：更新文档**

  - `approval-modes.mdx` 的批准选项表：
    - 选项 3 写"本会话允许"（英文 "Allow for this session"）；
    - 新增 5、6 两行，说明只对 MCP 工具出现、会把规则写进用户配置、写入失败时本会话照样允许。
  - `mcp-integration.mdx` 加一小节"总是允许"（英文 "Always allow"），给出面板选项和对应的规则写法。

- [ ] **Step 5：运行测试，确认通过**

  运行 Step 2 的命令，再运行：
  - `cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0`
  - `cargo nextest run --test tui_pty_contract --locked --profile ci-serial --retries 0`

  Expected：全部通过。golden frame 有变化时同步更新。

- [ ] **Step 6：提交**

```bash
git add crates/orca-core crates/orca-tui site/src/docs/md
git commit -m "feat(tui): save always-allow choices for MCP tools and servers"
```

### Task 5：端到端验证与收尾

**Files:**
- Create: `tests/mcp_cli_contract.rs`（`#![cfg(unix)]`）

**Interfaces:**
- Consumes：
  - 任务 1 的只读判断；
  - 任务 2 的规则；
  - 任务 3 的 `orca mcp` 命令；
  - 隐藏参数 `--provider mock`：mock 模型会调用提示词里写的工具，参见 `tests/session_server_contract.rs` 的 `mcp__slow__wait`；
  - `--approval-mode suggest` 下需要批准时 exec 以 3 退出，最后状态为 `approval_required`（见 `tests/approval_contract.rs`）。

- [ ] **Step 1：写测试**

  每个测试都用临时 `ORCA_HOME` 和工作目录。

  假服务器仿照 `tests/session_server_contract.rs` 的 `write_slow_mcp_server` 写一个 shell 脚本：
  - `tools/list` 返回两个工具：`lookup` 带 `"annotations":{"readOnlyHint":true}`，`write_note` 不带标注；
  - `tools/call` 返回文本 `fixture called`。

  测试：
  - `mcp_add_list_remove_round_trip_the_user_config`：通过真实二进制执行 `orca mcp add fx -- <脚本>`、`orca mcp list`、`orca mcp remove fx`，退出码和输出与任务 3 定义的一致。
  - `a_read_only_mcp_tool_runs_in_suggest_mode_without_asking`：
    - 先 `orca mcp add fx -- <脚本>`；
    - 再执行 `orca exec --output-format jsonl --provider mock --approval-mode suggest mcp__fx__lookup`；
    - 退出码 0，事件里没有 `approval.requested`，输出含 `fixture called`。
  - `a_write_mcp_tool_still_asks_in_suggest_mode`：同样的命令调 `mcp__fx__write_note`，退出码 3，最后状态为 `approval_required`。
  - `a_permission_rule_allows_an_mcp_tool_in_suggest_mode`：
    - 在 `ORCA_HOME/config.toml` 末尾追加 `[[permissions.rules]]`、`tool = "mcp__fx__*"`、`decision = "allow"`；
    - 调 `mcp__fx__write_note`，退出码 0，输出含 `fixture called`。

- [ ] **Step 2：运行**

  Run: `cargo nextest run -p blade-deepseek --test mcp_cli_contract --locked --retries 0`
  Expected：通过。某项失败时，回到对应任务修复。

- [ ] **Step 3：全量检查**

  依次运行：
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --locked`
  - `node scripts/validate-runtime-surface-contract.mjs`
  - `node scripts/validate-windows-platform-boundaries.mjs`
  - `cargo nextest run --workspace --all-targets --locked --profile ci --no-fail-fast --retries 0`
  - `cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0`
  - `cargo nextest run --test tui_pty_contract --locked --profile ci-serial --retries 0`
  - `npm --prefix site run build`

  Expected：全部通过。

- [ ] **Step 4：提交**

```bash
git add tests/mcp_cli_contract.rs
git commit -m "test: cover orca mcp and MCP approvals end to end"
```
