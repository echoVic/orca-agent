# MCP 完善轮设计：启动时异步连接与遗留问题

> 状态：待审阅（2026-10-03）。
> 前提：用户要求 MCP 能力在发 v0.5.6 之前尽量完善，并明确本轮 MCP 重构不需要考虑向后兼容。
> 范围：
> - 上一轮设计（`docs/superpowers/specs/2026-09-28-mcp-approvals-and-cli-design.md`）实现后留下的已知问题；
> - 启动时在后台并行连接 MCP 服务器。

## 0. 不做兼容意味着什么

**可以直接改的：**
- MCP 注册表的接口、MCP 目录，以及界面层的 MCP 类型，都可以直接改，不保留旧形态。
- 界面层的服务器状态收敛为五种：`Starting`、`Ready`、`Failed { message }`、`NeedsLogin`、`Disabled`。
  - 去掉 `Stopped`；
  - `Degraded` 改名为 `Failed`；
  - `AuthRequired` 改名为 `NeedsLogin`。

  已发布的版本从未写入过 MCP 目录记录，所以改动不影响任何已有会话。

**直接删除、不保留的：**
- 会话启动前"只列配置"的 `/mcp` 视图；
- "先发一条消息"的提示。

这两样由第 1 节取代。

**不再作为设计约束的：** 旧版 Orca 能否读取新数据。发版说明里写一句即可。

**仍然保留的：**
- `transport = "sse"` 先试 streamable HTTP、再回退旧版 SSE。这是 MCP 规范建议客户端具备的健壮性，不是兼容层。
- `[mcp_servers.capabilities]`。

## 1. 启动时异步连接

**参照：**
- Claude Code（`src/services/mcp/useManageMCPConnections.ts`）：界面一挂载，就把所有服务器标成 pending，然后并行连接、不等待，每个服务器连上或失败后各自更新状态。
- Codex（`codex-rs/codex-mcp/src/connection_manager.rs`）：会话一创建，就用 `JoinSet` 并行启动所有服务器，逐个发出 `McpStartupUpdate`（Starting、Ready、Failed），全部结束后发 `McpStartupComplete`。

**现状：**
- 新会话的 runtime 线程要到第一条消息才启动。启动时 `initialize_registry` 用一个循环逐个、同步地连接服务器，所以第一条消息要等所有服务器依次连完。
- 第一条消息之前，`/mcp` 只能列出配置，prompt 命令也不存在。

**方案（选定）：**

1. **注册表不阻塞。**
   - `initialize_registry` 立即返回。每个启用的服务器以 `Starting` 状态开始，各自在线程上并行连接。
   - 连接成功或失败后，服务器替换为 `Ready`、`NeedsLogin` 或 `Failed`。替换走与重连相同的逻辑。
   - 注册表提供变化订阅：任何服务器的状态、工具或 prompts 变化时，都通知订阅者。
   - 注册表提供 `wait_for_startup(deadline)`：一直等到没有服务器处于 `Starting`，或者到达截止时间。
2. **目录随连接更新。** 会话 actor 订阅注册表的变化，有变化就发布 MCP 目录，只在内容不同时才提交。这样不再依赖某个操作结束才刷新，后台轮次也就不再是特例。
3. **每轮只等仍在连接的服务器。**
   - 一轮第一次请求模型之前，如果还有服务器处于 `Starting`，就等它们这次连接结束，成功或失败都算。这个等待是有上限的：服务器连接过程中的每个请求（`initialize`、`tools/list`、`prompts/list`）都受它自己的 `startup_timeout_ms` 约束，默认 30 秒。
   - 这样模型第一次请求时看到的工具与现在一致。
   - 等待期间，TUI 状态行显示 `connecting MCP servers…`，`Esc` 照常取消这一轮。
   - `orca exec`、ACP 和 daemon 会话同样适用，区别只是连接变成并行。
4. **TUI 在启动时就开始连接。**
   - 新会话在 TUI 启动时就在后台创建注册表并开始连接。
   - 第一条消息启动 runtime 线程时，通过 `RuntimeThreadStartRequest::with_mcp_registry` 把这个注册表交给线程，不重新连接。
   - 线程启动之前：
     - `/mcp` 直接读取这个注册表，显示真实的状态、工具和 prompts；
     - `r`、`l`、`o` 直接作用于它，因为登录和重连都只需要注册表；
     - prompt 命令按注册表里的 prompts 出现；
     - 运行 prompt 命令时，直接用这个注册表展开，再像第一条消息那样提交；提交时启动线程。
   - 首条消息之前，无论用哪种方式启动线程（第一条消息、`/new`、恢复会话），都接手这个注册表。之后新开的会话由 runtime 按第 1 条异步连接。
5. **退出时结束服务器进程。** 线程启动前就退出 TUI 时，关闭注册表，结束所有 stdio 服务器进程。

**未选的方案：**
- **TUI 启动时就创建 runtime 线程。** 每次打开都会在历史里留下一个空的 "New conversation"，而会话选择器不会隐藏空会话。
- **只在第一条消息时改成并行连接。** 改动最小，但第一条消息之前 `/mcp` 和 prompt 命令仍然不可用。

## 2. 状态准确

1. **旧连接不覆盖新连接。**
   - 只有失败的调用所用的连接仍是该服务器的当前连接时，才把服务器标为 `NeedsLogin`。判断方法是比较连接的指针。
   - 每次重连带一个代次，较旧的重连结果不能覆盖较新的。
2. **`NeedsLogin` 的进出。**
   - `prompts/get` 和资源请求遇到"需要登录"，同样标记 `NeedsLogin`。
   - 该服务器上任何请求成功后，清除 `NeedsLogin`，回到 `Ready`。
3. **显示 prompt 列表的错误。** `prompts/list` 失败时，服务器保持 `Ready`，但目录里记下原因，`/mcp` 详情显示 `prompts unavailable: <原因>`。
4. **工具调用后立即重连。** 工具调用之后，如果 stdio 传输已经关闭（`is_closed()`），就在返回本次错误之前完成重连，和其他请求的做法一样，下一次调用因此能成功。本次失败的调用不重试。

## 3. 可以取消

1. **取消登录。**
   - 等待浏览器登录时，对同一个服务器再按一次 `l` 就取消。
   - 取消后停止回调监听、释放端口，提示 `login to MCP server <name> cancelled`。
   - 等待时行内显示 `waiting for browser login… (l to cancel)`。
   - `orca mcp login` 用 Ctrl+C 中断时，也要释放端口。
2. **取消 prompt 展开。**
   - prompt 展开进行中按 `Esc` 取消。结果到达时丢弃，并提示 `MCP prompt /mcp__<server>__<prompt> cancelled`。
   - 服务器端的请求照常完成，不另行中止。

## 4. 协议补全

1. **分页。**
   - `tools/list`、`prompts/list`、`resources/list` 和 `resources/templates/list` 都跟随 `nextCursor` 往下取，最多取 100 页。
   - 超过 100 页时保留已经取到的结果，并记一条错误。
   - 游标重复时立即停止。
2. **服务器发来的请求。** 三种传输（stdio、streamable HTTP、旧版 SSE）现在都不回应。改为：
   - `ping` 回空结果；
   - `elicitation/create` 维持现状；
   - 其他请求回 JSON-RPC 错误 `-32601`。

   stdio 上，服务器请求的 id 与正在等待的请求 id 相同时，不再被当成响应。
3. **SSE 响应流尽早结束。** 读到与请求 id 匹配的响应，就结束这次读取，不再等服务器关闭流。
4. **OAuth 元数据不跟随降级重定向。** 获取受保护资源元数据和授权服务器元数据时，不跟随从 https 跳到 http 的重定向，遇到时报错。

## 5. 进程与配置

1. **stdio 服务器重连：先停后启。** 先结束旧进程，再启动新进程，固定端口的服务器重连时不再冲突。新进程启动失败时，服务器显示 `Failed`。远程服务器保持现在的做法：先建立新连接，再断开旧连接。
2. **写配置加锁。**
   - 写 `config.toml` 的地方包括：`orca mcp add`、`orca mcp remove`、保存"总是允许"的规则，以及已有的模型设置保存。
   - 它们都持有跨进程文件锁 `config.toml.lock`，与凭据文件用同一套锁。
3. **内联数组写法。**
   - `orca mcp` 的所有子命令都同时支持两种写法：`[[mcp_servers]]` 和 `mcp_servers = [{…}]`。
   - `add` 沿用文件已经在用的写法。

## 6. 界面与文档

1. **`/mcp` 详情。**
   - 每个工具一行：`read-only` 标记在前，规则名在后，80 列时也不会被截掉。
   - prompt 显示参数，例如 `review_pr <pr> [branch]`。
2. **保存"总是允许"时提示冲突。**
   - 如果用户配置里有更严格的规则（`deny` 或 `prompt`）覆盖这个工具，就提示：`saved, but a stricter rule in your config still applies to <tool>`。
   - 本次调用仍然放行，本会话内也仍然有效。
3. **文档。**
   - 新增一段"启动时异步连接"的说明。
   - 改写 `/mcp`、prompt 命令在第一条消息之前的行为。
   - 修正两处措辞：
     - `approval-modes.mdx` 第 86 行，"allow no less than `3` does"；
     - `mcp-integration` 里状态标签在何时更新。

   中英文都改。

## 7. 测试

1. **凭据路径来自配置。** MCP 凭据文件的路径改由 `RunConfig` 携带：加载配置时填入 `$ORCA_HOME/mcp-credentials.json`，而测试默认构造的配置没有这个路径。所以测试不会再读写真实的 `~/.orca/mcp-credentials.json`。
2. **端到端：会话 ID 回传。** HTTP 测试服务器断言：初始化之后的每个请求都带回了 `Mcp-Session-Id`。
3. **本轮新增的关键测试：**
   - **并行连接：** 两个各需 1 秒才能完成初始化的服务器，总共约 1 秒连完，而不是 2 秒。
   - **不阻塞：** `initialize_registry` 立即返回，此时服务器为 `Starting`。
   - **首轮等待：** 一轮的第一次模型请求能看到慢服务器的工具；`Esc` 可以取消等待。
   - **目录自动更新：** 服务器连上后，目录自动更新，不需要任何操作。
   - **启动前的 `/mcp`：** 线程启动之前，`/mcp` 显示真实状态，`r`、`l`、`o` 可用，prompt 命令可用。
   - **退出：** 线程启动前退出 TUI，stdio 服务器进程被结束。
   - **交接注册表：** 第一条消息启动线程时，复用已经连好的连接，不重新连接。
   - **状态准确：** 旧连接上的失败不覆盖新连接；`NeedsLogin` 在请求成功后被清除；旧的重连结果不覆盖新的。
   - **取消：** 取消登录后端口被释放；取消展开后结果被丢弃。
   - **分页：** 多页结果、重复游标、页数上限。
   - **旧版 SSE：** `ping` 得到回应。
   - **SSE 尽早结束：** 读到匹配的响应后结束读取，此时服务器不关流也不会卡住。
   - **OAuth：** 遇到从 https 跳到 http 的重定向时报错。
   - **stdio 重连：** 先停后启。
   - **配置锁：** 两个进程同时写配置时不丢更新。
   - **内联数组：** 内联数组写法的 list、get、add、remove 都能往返。
   - **界面：** 详情的排版，以及冲突提示。

## 8. 发版说明增补

- MCP 服务器在启动时就在后台并行连接，`/mcp` 和 prompt 命令从一启动就可用。第一条消息只等还没连上的服务器。
- 登录和 prompt 展开可以取消。
- 列表会跟随分页往下取。
- stdio 服务器重连时先停旧进程。
- 写配置有锁保护。
- 支持内联数组写法。
- 其余状态和界面的修正。
- 兼容性：本轮不考虑向后兼容，v0.5.5 及更早版本不能读取本版本写下的部分会话和配置。

## 9. 不在本轮

- **严格检查 stdio 服务器的协议版本。** 维持现状，符合 MCP 规范；发版说明里已经写了。
- **`turn_diagnostic_seen` 的问题。** 这是与 MCP 无关的旧问题。
- **ACP 客户端（如 Pilion）提供的 MCP 服务器的登录。**
- **把 `/mcp` 面板代码从 `ui.rs` 挪到独立模块。** 这是纯重构，本轮不做。
