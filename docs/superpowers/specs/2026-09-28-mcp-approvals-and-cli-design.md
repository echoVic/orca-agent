# MCP 审批与 `orca mcp` 命令设计

> 状态：方向已确认（2026-09-28，A–D 都做），待审阅本文。
> 目标：像 Claude Code、Codex 一样，一条命令加好 MCP 服务器；只读工具不用每次批准；需要批准的工具能一次选择、长期生效。不需要写额外配置。

## 0. 现状与参照

Orca 现在：

- **添加服务器**：没有 `orca mcp` 命令，只能在 `~/.orca/config.toml` 里手写 `[[mcp_servers]]`。项目配置不允许定义 MCP 服务器。
- **审批分类**：所有 MCP 调用都按"写"处理（`McpProxyTool` 固定用 `CapabilitySet::filesystem_write()`）。结果是：
  - suggest 模式每次都要问；
  - plan 模式一律拒绝，只读工具也不例外；
  - auto-edit 和 full-auto 直接放行。
- **权限规则**：规则要求调用带目标（`approval_rules.rs` 的 `matches`），而 MCP 调用没有目标，所以规则永远匹配不上 MCP 工具。
- **批准面板**：只能"本会话允许"，没有长期保存。

参照（源码见 `../claude-code`、`../codex`、`../grok-build`）：

| | 添加 | 只读判断 | 长期允许 |
| --- | --- | --- | --- |
| Claude Code | `claude mcp add <名字> -- <命令>` | `readOnlyHint` | 面板写入规则 `mcp__s__t`；`mcp__s`、`mcp__s__*` 表示整个服务器 |
| Codex | `codex mcp add <名字> -- <命令>` | `readOnlyHint`（默认 auto 模式） | 面板可记住本会话，或写入配置 |
| Grok | — | — | 面板"总是允许这个工具 / 这个服务器" |

## 1. A：只读工具不再按写处理

- MCP 客户端解析 `tools/list` 里每个工具的 `annotations.readOnlyHint` 和 `annotations.destructiveHint`，存进 `McpTool`。
- 满足 `readOnlyHint == true` 且 `destructiveHint != true` 的工具按只读处理：动作类型为 `ActionKind::Read`，渲染提示也按只读。其他工具保持现状，按写处理。
- 效果：
  - suggest 模式下只读工具不询问；
  - plan 模式下只读工具可以用；
  - auto-edit 和 full-auto 不变。
- 这个标注由服务器自己声明，Orca 不校验，做法与 Claude Code、Codex 相同。文档写明"是否只读以服务器声明为准"。

## 2. B：权限规则能写 MCP 工具

- `pattern` 改为可省略，省略时等于 `*`。旧配置都写了 `pattern`，行为不变。
- 调用没有目标时，拿空字符串去匹配 `pattern`：只有能匹配空串的规则生效，比如省略 pattern、`*`、`**`。写了具体 pattern 的规则不会命中 MCP 调用。
- `tool` 支持服务器级写法：`mcp__<server>` 和 `mcp__<server>__*` 匹配该服务器的所有工具。其他工具名仍精确匹配。
- 解析 `mcp__<server>__<tool>` 时，第一个 `__` 之后到下一个 `__` 之间是服务器名，所以服务器名不能含 `__`，`orca mcp add` 会拒绝这样的名字。
- 这些不变：
  - 规则只能写在用户配置里；
  - 多条规则同时命中时以最严的为准；
  - 规则不能突破 plan 模式的限制。

```toml
[[permissions.rules]]
tool = "mcp__github__*"
decision = "allow"
```

## 3. C：批准面板可以"总是允许"并保存

- MCP 工具的批准面板新增两个选项，只对 MCP 工具出现：
  - `5` 总是允许这个工具（保存到配置）
  - `6` 总是允许这个服务器（保存到配置）
- 现有选项 `3` 的标签从 "always allow" 改为 "allow for this session"，与文档里的"本会话允许"一致，也避免和"保存"选项混淆。按键 1–4 的含义不变。
- 选中 5 或 6 后：
  1. 往用户配置追加一条 allow 规则：`tool = "mcp__s__t"` 或 `tool = "mcp__s__*"`，不写 pattern。
     - 用 `toml_edit` 保留原有内容和注释，原子写入，与 `persist_user_model_settings` 做法相同。
     - 已有相同的 allow 规则就不重复写。
  2. 本会话立即生效：写入 TUI 的会话允许列表，服务器级按 `mcp__s__` 前缀匹配。
  3. 以后的会话由 B 的规则放行。
- 保存失败时（比如配置文件解析不了），本次调用仍然放行，本会话也有效，状态栏说明保存失败的原因。
- 已有的 deny 规则仍然优先，保存的 allow 规则不会越过它。
- ACP 客户端（如 Pilion）和 app-server 的批准选项不变。

## 4. D：`orca mcp` 命令

```text
orca mcp add <name> [-e KEY=VALUE]... -- <command> [args...]    # stdio
orca mcp add <name> --url <url> [--header "Name: value"]...     # SSE
orca mcp list
orca mcp remove <name>
```

- **写到哪里**：写入用户配置 `~/.orca/config.toml`（遵循 `ORCA_HOME`）的 `[[mcp_servers]]`，只写 `name`、`transport`、`command`、`args`、`env`、`url`、`headers`。用 `toml_edit` 保留原有内容，原子写入。
- **名字**：只允许字母、数字、`-`、`_`，不能含 `__`，不能和已有服务器重名。重名时报错，提示先 `orca mcp remove`。
- **`list`**：列出名字、传输方式、命令或地址、是否停用。不连接服务器，不打印环境变量和请求头的值。
- **`remove`**：删除同名的整个表；找不到时报错。
- **只支持 stdio 和 SSE**：`--url` 即 SSE，因为 Orca 目前没有 streamable HTTP 传输。帮助和文档都写明这一点。
- **生效时机**：命令只改配置，不启动服务器。已经在运行的会话需要重开才能用上新服务器，文档写明。

## 5. 文档

- `mcp-integration`（中英）：
  - 开头改用 `orca mcp add` 举例，手写配置作为第二种方式；
  - 新增"只读工具"和"总是允许"两节。
- `approval-modes`（中英）：
  - 批准选项表加上 5、6，并把 3 的说明改为"本会话允许"；
  - 权限规则一节加上 MCP 写法和"pattern 可省略"。
- `cli-reference`（中英）：加上 `orca mcp` 子命令。

## 6. 测试

- **A**：解析标注；只读、破坏性、无标注三种情况的动作类型；plan 和 suggest 下的实际审批结果。
- **B**：
  - 无目标调用分别匹配省略 pattern、`*` 和具体 pattern；
  - 服务器级写法 `mcp__s` 和 `mcp__s__*`；
  - 不同服务器不误匹配；
  - deny 优先；
  - 旧配置反序列化不变。
- **C**：
  - 选项只对 MCP 工具出现；
  - 选 5、6 后配置里多一条规则，原有内容和注释保留，重复选择不重复写；
  - 会话内立即生效，服务器级覆盖同服务器的其他工具；
  - 保存失败时仍然放行，并有提示。
- **D**：
  - add（stdio、SSE、带 env、带 header）、list、remove 都通过真实配置文件往返；
  - 重名、非法名字、remove 不存在的名字都报错；
  - 原有注释和其他设置保留。
- **端到端**：用 `orca mcp add` 加一个临时 stdio 服务器，用真实 `orca exec` 验证两件事：只读工具在 suggest 下直接运行；非只读工具由规则放行。

## 7. 兼容性

- 已经自报 `readOnlyHint` 的服务器，其工具在 suggest 模式下不再询问、在 plan 模式下可用。发版说明写明。
- 旧的权限规则和 `[[mcp_servers]]` 配置照常读取，行为不变。
- `[mcp_servers.capabilities]` 保持现状，只影响服务器进程的启动，不参与审批。
