# MCP 能力补全设计

> 状态：A–D 已确认（2026-09-28）；用户要求对标顶尖产品、一次做全，E–I 待审阅本文。
> 目标：Orca 的 MCP 能力与 Claude Code、Codex 看齐，涵盖以下几方面：
> - 一条命令加好本地或远程服务器；
> - 远程服务器按规范连接，需要时走 OAuth 登录；
> - 只读工具不用每次批准，需要批准的工具可以一次选择、长期生效；
> - TUI 里能查看和管理服务器；
> - 服务器提供的 prompts 能当斜杠命令用；
> - 可以按工具启用或禁用。

## 0. 现状与参照

Orca 现在的情况：

- **添加服务器**：没有 `orca mcp` 命令，只能在 `~/.orca/config.toml` 里手写 `[[mcp_servers]]`。项目配置不允许定义 MCP 服务器。
- **审批**：
  - 所有 MCP 调用都按"写"处理，因为 `McpProxyTool` 固定用 `CapabilitySet::filesystem_write()`。结果是 suggest 模式每次都问，plan 模式一律拒绝。
  - 权限规则要求调用带目标，MCP 调用没有目标，所以规则匹配不上。
  - 批准面板只能"本会话允许"。
- **远程传输**：`transport = "sse"` 实际是简化版 streamable HTTP：每个请求 POST，读回 SSE 或 JSON 响应。它缺三样东西：
  - 请求头 `Accept: application/json, text/event-stream`；
  - 会话 ID `Mcp-Session-Id`；
  - 协议版本头。

  旧版 SSE（先 GET 事件流、再往它给的地址 POST）不支持。初始化时声明的协议版本是 `2024-11-05`。
- **认证**：只有静态请求头，没有 OAuth。
- **界面**：没有 `/mcp`。界面层的 MCP 目录（`SurfaceMcpCatalogSnapshot`）已经定义，但 runtime 从不填充。
- **其他**：MCP prompts 和按工具启用/禁用都没有。

参照（源码见 `../claude-code`、`../codex`、`../grok-build`）：

| 能力 | Claude Code | Codex | Grok |
| --- | --- | --- | --- |
| 添加 | `claude mcp add <名字> -- <命令>`，`--transport http\|sse` | `codex mcp add/list/get/remove/login/logout` | — |
| 远程传输 | streamable HTTP、旧版 SSE | streamable HTTP | HTTP |
| 认证 | OAuth（`/mcp` 里登录） | OAuth、`bearer_token_env_var` | OAuth（HttpAuth） |
| 只读判断 | `readOnlyHint` | `readOnlyHint` | — |
| 长期允许 | 面板写入规则 `mcp__s__t`；`mcp__s` 或 `mcp__s__*` 表示整个服务器 | 面板记住本会话，或写入配置 | 面板"总是允许这个工具 / 服务器" |
| 管理界面 | `/mcp` | `/mcp` | — |
| prompts | 斜杠命令 `/mcp__s__p` | — | — |
| 按工具筛选 | — | `enabled_tools` / `disabled_tools` | — |

## 1. A：只读工具不再按写处理

- MCP 客户端解析 `tools/list` 里每个工具的 `annotations.readOnlyHint` 和 `annotations.destructiveHint`，存进 `McpTool`。
- 满足 `readOnlyHint == true` 且 `destructiveHint != true` 的工具按只读处理（`ActionKind::Read`），渲染提示也按只读。其他工具保持现状，按写处理。
- 效果：只读工具在 suggest 模式下不询问，plan 模式下可以用。auto-edit 和 full-auto 不变。
- 是否只读由服务器声明，Orca 不校验，与 Claude Code、Codex 一致。文档要写明这一点。

## 2. B：权限规则能写 MCP 工具

- `pattern` 可以省略。省略时规则覆盖这个工具的所有调用，不论目标：任何路径（包括嵌套路径和绝对路径）、任何命令、没有目标的调用。它不是 glob `*`，`*` 在路径里不跨目录。（裁定 R30 修正了原来"省略时等于 `*`"的说法：那样写的 `write_file` deny 规则会漏掉几乎所有调用。）旧配置都写了 `pattern`，行为不变；显式写的 `*` 仍按 glob 匹配。
- MCP 调用的目标是工具自己的名字（`mcp__<server>__<tool>`），所以 MCP 规则不写 pattern。调用没有目标时，用空字符串去匹配 `pattern`，写了具体 pattern 的规则不会命中。
- `tool` 支持服务器级写法：`mcp__<server>` 或 `mcp__<server>__*` 匹配这个服务器的所有工具。其他工具名仍然精确匹配。
- 规则里 `mcp__` 名字的服务器段和工具段，按工具命名的方式规范化（转成小写，连续的其他字符换成一个 `_`），所以 `mcp__GitHub__deleteRepo` 匹配 `mcp__github__deleterepo`（裁定 R31）。`/mcp` 的详情显示每个工具在规则里的名字。
- 解析 `mcp__<server>__<tool>` 时，第一个 `__` 之后到下一个 `__` 之前是服务器名。因此服务器名不能含 `__`，`orca mcp add` 会拒绝这样的名字。
- 这几条不变：规则只能写在用户配置里；多条规则同时命中时以最严的为准；规则不能突破 plan 模式的限制。

```toml
[[permissions.rules]]
tool = "mcp__github__*"
decision = "allow"
```

## 3. C：批准面板"总是允许"并保存

- MCP 工具的批准面板新增两个选项，只对 MCP 工具出现：
  - `5`：总是允许这个工具（保存到配置）；
  - `6`：总是允许这个服务器（保存到配置）。
- 现有选项 `3` 的标签从 "always allow" 改为 "allow for this session"。1–4 的按键含义不变。
- 选中 5 或 6 后：
  1. 往用户配置追加一条 allow 规则：`tool = "mcp__s__t"` 或 `tool = "mcp__s__*"`，不写 pattern。
     - 用 `toml_edit` 修改，保留原有内容和注释，原子写入，做法与 `persist_user_model_settings` 相同。
     - 已有相同的 allow 规则时不重复写。
  2. 本会话立即生效：TUI 的会话允许列表里加入这一项，服务器级按 `mcp__s__` 前缀匹配。
  3. 之后的会话由 B 的规则放行。
- 保存失败（比如配置文件解析不了）时，本次调用仍然放行，本会话也有效，并在对话里说明保存失败的原因。
- 已有的 deny 规则仍然优先。
- ACP 客户端（如 Pilion）和 app-server 的批准选项不变。

## 4. D：`orca mcp` 命令

```text
orca mcp add <name> [-e KEY=VALUE]... -- <command> [args...]            # stdio
orca mcp add <name> --url <url> [--transport http|sse]                  # 远程，默认 http
             [--header "Name: value"]... [--bearer-token-env-var NAME]
             [--client-id ID] [--callback-port PORT]
orca mcp list [--json]
orca mcp get <name> [--json]
orca mcp remove <name>
orca mcp login <name>
orca mcp logout <name>
```

- **写哪里**：写入用户配置 `~/.orca/config.toml`（遵循 `ORCA_HOME`）的 `[[mcp_servers]]`。只写命令行里给出的字段，用 `toml_edit` 保留原有内容，原子写入。
- **名字**：只允许字母、数字、`-`、`_`，不能含 `__`，不能和已有服务器重名。重名时报错，提示先 `orca mcp remove`。规范化后（见 B）为空的名字，或与已有服务器规范化后相同的名字（如 `GitHub` 与 `github`），同样报错，并指出冲突的服务器（裁定 R32）。
- **list、get**：
  - 不连接服务器，不打印环境变量、请求头和 token 的值，只显示名字或键名。
  - `list` 每行显示：名字、传输方式、命令或地址、登录状态（只看本地保存的 token：已登录 / 未登录 / 无需登录），停用的服务器加 `disabled`。
  - `get` 额外显示：超时设置、`bearer_token_env_var`、`enabled_tools`、`disabled_tools`。
  - `--json` 输出同样的内容，方便脚本使用。
- **remove**：删除所有同名的表，同时删除它保存的 OAuth token。找不到时报错。
- **login、logout**：见 F。
- **生效时机**：命令只改配置或凭据，不连接运行中的会话。改动在新会话里生效；已开的 TUI 会话可以在 `/mcp` 里重连（见 G）。

## 5. E：远程传输按 MCP 规范补全

- **传输名**：配置 `transport` 新增 `"http"`，即 streamable HTTP。
  - `"sse"` 保留，做法与 MCP 规范给客户端的向后兼容建议相同：先按 streamable HTTP 发初始化请求，收到 400、404 或 405 时改走旧版 SSE。
  - 现有 `sse` 配置照常工作。
- **streamable HTTP**：
  - 每个请求用 POST，带 `Accept: application/json, text/event-stream`。响应可能是 JSON，也可能是 SSE 流，两种都处理，沿用现有的 SSE 读取和 16 MiB 上限。
  - 初始化响应的 `Mcp-Session-Id` 在之后每个请求里带上。之后的请求都带 `MCP-Protocol-Version: <协商结果>`。
  - 通知收到 202 算成功。
  - 带会话 ID 的请求收到 404，说明会话已过期：重新初始化，并把原请求重试一次。
  - 连接关闭时发 DELETE 结束会话，失败不报错。
  - 不实现服务器主动推送用的 GET 流。服务器发给客户端的请求（如 elicitation）仍在 POST 的响应流里处理，与现在相同。
- **旧版 SSE（2024-11-05）**：
  1. GET 服务器地址，打开事件流；
  2. 从 `endpoint` 事件取得 POST 地址。相对地址按原地址解析，并且必须同源，否则报错；
  3. 请求 POST 到该地址，响应从事件流里按 id 取回，由一个后台读线程负责分发；
  4. 事件流断开时，按现有规则重连。
- **协议版本**：初始化时声明 `2025-06-18`，接受服务器返回 `2025-06-18`、`2025-03-26` 或 `2024-11-05`。其他版本报错，并说明服务器要求的版本。

## 6. F：认证

- **`bearer_token_env_var = "NAME"`**：连接远程服务器时读取这个环境变量，发送 `Authorization: Bearer <值>`。变量未设置时连接失败，报错说明缺哪个变量。token 因此不会写进配置。
- **OAuth**（按 MCP 授权规范：OAuth 2.1 + PKCE）：
  - **何时触发**：远程服务器返回 401，而且配置里没有静态 `Authorization` 头，也没有 `bearer_token_env_var`。
  - **发现**：
    1. 先读 `WWW-Authenticate` 里的 `resource_metadata`，取得受保护资源元数据（RFC 9728）；没有这一项时，请求服务器同源的 `/.well-known/oauth-protected-resource`。
    2. 再取授权服务器元数据（RFC 8414 的 `/.well-known/oauth-authorization-server`），找不到时改用 OpenID 发现。
  - **客户端 ID**：配置里有 `oauth_client_id` 就用它，否则做动态注册（RFC 7591）。
  - **授权**：
    - 使用授权码流程，附带 PKCE（S256）、`state` 和 `resource`（RFC 8707）。
    - 本机回环地址 `http://127.0.0.1:<端口>/callback` 接收回调。默认用随机端口；`oauth_callback_port`（即 `--callback-port`）可以固定端口，给要求预先注册回调地址的服务器用。
    - 自动打开浏览器，同时把授权地址打印出来，没有图形界面时可以手动打开。
    - 等待回调最多 5 分钟。
  - **保存**：
    - 存在 `$ORCA_HOME/mcp-credentials.json`，权限 600，原子写入，与 `auth.json` 保存 API key 的做法一致。
    - 按"服务器名 + 地址"保存，地址变了旧 token 就作废。
    - 保存的内容：access token、refresh token、过期时间、token 端点、客户端 ID，以及动态注册拿到的客户端信息。
  - **使用与刷新**：
    - 请求带 `Authorization: Bearer <access token>`。
    - 快过期或收到 401 时，用 refresh token 刷新一次。刷新失败就把服务器标为"需要登录"，这时工具调用报错，提示运行 `orca mcp login <name>` 或在 `/mcp` 里登录。
  - **命令**：
    - `orca mcp login <name>` 走完上面的流程；对 stdio 服务器或配置了静态认证的服务器，报错说明无需登录。
    - `orca mcp logout <name>` 删除保存的 token。
  - **不做**：client secret（MCP 服务器普遍支持动态注册或公共客户端）；系统钥匙串（以后可以加）。

## 7. G：TUI `/mcp` 面板

- **打开**：输入 `/mcp` 打开面板。
- **服务器列表**：列出所有服务器和状态：
  - 已连接（N 个工具）；
  - 连接失败（显示原因）；
  - 需要登录；
  - 已停用。
- **详情**：选中服务器后显示它的工具（只读工具带标记）和 prompts。
- **操作**：
  - `r`：重新连接。
  - `l`：登录。在 TUI 进程里走 F 的流程，完成后自动重连。
  - `o`：退出登录，删除 token 后重连，服务器会变为"需要登录"。
  - `Esc`：关闭面板。
- **数据来源**：runtime 在会话启动时、以及每次重连后，填充界面层已有的 MCP 目录（`SurfaceMcpCatalogSnapshot`），内容包括服务器状态、工具、prompts。
- **会话中途重连**：`McpRegistry` 改为可以替换单个服务器的连接和工具列表。下一次模型请求时，就能看到更新后的工具。重连通过界面层新增的命令触发。
- **daemon 会话**：`orca attach` 挂接的 daemon 会话暂不支持 `/mcp`，给出提示：在本机运行 `orca mcp login` 后，用 `orca attach new` 开新的 daemon 会话；已经打开的会话要等 daemon 重启、重新挂接后才用上这次登录（裁定 R34：重新挂接会沿用会话原来的连接）。

## 8. H：MCP prompts 变成斜杠命令

- **获取**：连接时，对声明了 `prompts` 能力的服务器调用 `prompts/list`，结果放进 MCP 目录。
- **菜单显示**：TUI 斜杠菜单列出 `/mcp__<server>__<prompt>`。说明取 prompt 的 description，参数提示取它的 arguments。
- **参数**：例如 `/mcp__github__review_pr 123 main`，按位置对应 prompt 的参数，最后一个参数接收剩下的全部文本。缺少必填参数时，在对话里提示用法，不发送。
- **执行**：runtime 调用 `prompts/get`，把返回的消息合成一条用户消息，作为这一轮的输入发出。合成方式：
  - 文本按顺序用空行连接；
  - 图片作为图片附件；
  - 嵌入资源取其文本内容。
- **错误**：服务器报错时在对话里说明，不发送。

## 9. I：按工具启用或禁用

- 服务器配置新增 `enabled_tools`（白名单）和 `disabled_tools`（黑名单），写服务器给出的原始工具名。
- 先按白名单筛选，再去掉黑名单中的工具，做法与 Codex 相同。
- 被筛掉的工具不注册，模型看不到。`orca mcp get` 和 `/mcp` 会显示这两项。

## 10. 文档

- **`mcp-integration`（中英）**：
  - 开头用 `orca mcp add` 举例，手写配置作为第二种方式；
  - 远程服务器：`http` 与 `sse` 的区别、`bearer_token_env_var`、OAuth 登录；
  - 新增小节：只读工具、总是允许、`/mcp`、prompts、按工具筛选。
- **`approval-modes`（中英）**：批准选项表加上 5、6，3 改为"本会话允许"；权限规则一节加上 MCP 写法，并说明 pattern 可以省略。
- **`cli-reference`（中英）**：加上 `orca mcp` 的全部子命令。
- **`terminal-ui`（中英）**：加上 `/mcp` 和 MCP prompt 命令。

## 11. 测试

- **A**：
  - 解析标注；
  - 只读、破坏性、无标注三种情况下的动作类型；
  - plan 和 suggest 模式下的实际审批结果。
- **B**：
  - 无目标调用分别对省略 pattern、`*` 和具体 pattern 的匹配；
  - 服务器级写法；
  - 名字相近的服务器不误匹配；
  - deny 优先；
  - 旧配置不变。
- **C**：
  - 选项只对 MCP 工具出现；
  - 保存后配置里多一条规则，原有内容和注释保留，重复保存不重复写；
  - 会话内立即生效；
  - 保存失败时仍然放行，并给出提示。
- **D**：
  - 每个子命令都通过真实配置文件往返；
  - 重名、非法名字、不存在的名字都报错；
  - `list` 和 `get` 不泄露密钥类的值；
  - 配置解析失败时不覆盖文件。
- **E**：
  - 用本地 HTTP 测试服务器验证：缺 `Accept` 头时服务器回 406；会话 ID 往返；通知收到 202；会话过期后重新初始化；JSON 和 SSE 两种响应。
  - 旧版 SSE：`endpoint` 事件、POST 请求、从事件流取回响应、跨源地址被拒绝。
  - `sse` 在初始化收到 405 时回退到旧版 SSE。
  - 协议版本的协商。
- **F**：
  - 用本地测试授权服务器和资源服务器走完整流程：401、元数据发现、动态注册、授权回调（测试直接请求授权地址，模拟浏览器）、换 token、刷新；
  - 凭据文件权限为 600；
  - logout 删除 token；
  - `bearer_token_env_var` 生效，变量缺失时报错。
- **G**：
  - 面板列出各种状态；
  - 重连后工具列表更新；
  - 登录后重连；
  - daemon 会话里给出提示。
- **H**：
  - 通过测试服务器取 prompts 列表并执行 prompt；
  - 斜杠菜单列出 prompt 命令；
  - 按位置映射参数；
  - 缺少参数时给出用法提示。
- **I**：白名单、黑名单及两者组合。
- **端到端**：用真实二进制和 mock 模型验证四件事：`orca mcp add` 本地服务器；只读工具直接运行；非只读工具由规则放行；远程测试服务器通过 `--url` 连接。

## 12. 兼容性

- 已经自报 `readOnlyHint` 的服务器，其工具在 suggest 模式下不再询问，在 plan 模式下可以使用。发版说明里要写明。
- `transport = "sse"` 的服务器现在会带上 `Accept` 头和会话 ID，对不接受 streamable HTTP 的服务器会回退到旧版 SSE。正常情况下只会更容易连上。
- 初始化时声明的协议版本改为 `2025-06-18`，只支持旧版本的服务器会协商回旧版本。
- 旧的权限规则和 `[[mcp_servers]]` 配置照常读取。
- `[mcp_servers.capabilities]` 保持现状，只影响服务器进程的启动，不参与审批。
