# 依赖升级设计（子项目 2）

> 状态：待审阅（2026-10-08）。
> 前提：v0.5.6 之后的三个子项目之二。子项目 1（遗留问题修复）已作为 v0.5.7 发布，子项目 3 是 ACP SDK 迁移。
> 2026-10-05 已定：TUI 栈一起升级；网站全部升到最新；ACP 单独做。2026-10-08 补充：一并修复 bash 状态停在 `still running` 的问题；reqwest 0.13 用系统证书库 + ring。
> 做法：在本地 main 上分阶段提交，每一步都要编译通过、全量测试通过；最后只发一版，发不发由用户决定。
> 版本：以执行时 crates.io / npm 上最新的正式版为准，不用预发布版。下文的版本号是 2026-10-08 查到的。

## 0. 顺序

1. bash 状态修复（第 1 节）。它和升级无关，先单独做，免得和升级的改动混在一起。
2. Rust 兼容更新（第 2 节）。
3. Rust 大版本（第 3 节），每个小节一个提交：toml → reqwest/hickory/TLS → rusqlite → 其余。
4. TUI 栈（第 4 节），一个提交。
5. 网站（第 5 节），一个提交。

第 3 步和第 4 步完成后，各推一次 `ci/dependency-upgrades` 分支跑 Linux 和 Windows CI（见第 8 节）。

## 1. bash 状态停在 `still running`

**现状：**
- bash 调用最多等一个 yield 窗口就返回（`execute_bash`，`crates/orca-runtime/src/runtime_normal_tool.rs`）。命令没跑完时，结果 JSON 里是 `state: "running"`、`return_reason: "yield_elapsed"`，命令作为后台 shell 任务继续运行。
- TUI 把这份 JSON 存进那一行（`ChatMessage::ToolCall` 的 `output`），渲染时由 `terminal_output_display`（`crates/orca-tui/src/terminal_output.rs`）算出 `still running` 标签。图标看的是工具调用本身的状态，所以是 ✓。
- 同一个任务后来的结果都到不了那一行：
  - `task_read_output` 和 `task_wait` 的结果被 reducer 直接丢弃（`is_panel_owned_tool_progress_name`，`crates/orca-tui/src/state_reducer.rs`）；
  - 任务结束时，runtime 只在下一个轮次边界给模型插一条 `<task-notification>` 系统消息（`drain_terminal_notifications`），不会更新 TUI 的那一行。
- 结果：命令早已跑完，那一行仍然显示 `still running`。从 v0.5.0（d4e781f8）起就是这样。

**要求：**
1. **跑完后更新标签。** 规则和 `terminal_output_display` 处理直接跑完的结果一样：
   - 正常退出：不加标签；
   - 否则显示 `exit N`、`timed out`、`cancelled`，或其他终止原因（例如 `interrupted`）。
2. **用 `task_id` 关联。** 结果 JSON 里 `task_id` 相同的行都属于这个任务，包括 `task_send_input` 的行。
3. **最终状态的来源。** 取最新、最具体的一个：
   - `task_read_output` 的结果，格式和 bash 的结果相同；
   - `task_wait` 的结果：`tasks` 数组里的每一项，字段是 `status`、`termination`、`exit_code`，和 bash 结果的字段名不同；
   - TUI 已经在接收的任务快照（`BackgroundTaskSummary`，`TaskType::Shell`）。快照只有状态、没有退出码，所以只有快照时这样显示：
     - `Completed`：不加标签；
     - `Failed`：`failed`；
     - `Stopped`：`stopped`；
     - `Cancelled`：`cancelled`。

     之后拿到带退出码的结果，再换成精确的标签。
   - 一旦拿到结束状态，就不会再变回 `still running`。几个结束状态之间，带退出码或终止原因的结果优先于只有状态的快照。
4. **这两个工具照样不在对话里显示。** reducer 先用它们的结果更新对应的行，然后照旧返回。
5. **恢复会话。** 回放历史时走同样的处理；任务快照里有这个任务，就用快照里的状态。对于从历史恢复出来的行，如果它仍是 running、历史里没有后续结果、任务快照里也没有这个任务，显示 `state unknown`。
6. **只改标签。** 图标和那一行显示的输出不变，也不把后续输出补进那一行。
7. **改动范围：** 只动 orca-tui（reducer、`terminal_output.rs`、渲染），runtime 不改。

## 2. Rust 兼容更新

- `cargo update`：148 个包，只改 `Cargo.lock`。
- 每个 crate 都开了 `#![deny(deprecated)]`，新冒出来的弃用会变成编译错误，要改用新的 API。CI 用最新的 stable（2026-10-05 是 1.99），本机是 1.95，两边都要通过，所以要用两边都接受的写法。
- 本机 1.95 不会让任何依赖卡在旧版本：按 MSRV 解析和不限制 MSRV 的结果完全一样。

## 3. Rust 大版本

### 3.1 toml 0.8 → 1.1，toml_edit 0.22 → 0.25

**变化：**
- toml 0.9 换了新的解析器和写入器。`to_string_pretty` 的输出格式、报错信息的文字都可能变。用它写文件的有 `crates/orca-core/src/config/folder_trust.rs` 和 `crates/orca-runtime/src/onboarding.rs`。
- toml 0.9 起，反序列化默认不再按文档里的顺序给出键，要保留顺序得开 `preserve_order`。
- `impl FromStr for Value` 改成只解析单个值，不再解析整个文档。Orca 没用到。
- 能解析 TOML 1.1 的写法了，比如多行内联表、尾逗号。
- toml_edit 的变化：
  - 0.23 起，`Array::push` 和 `Array::insert` 新增条目的空白和注释，改到渲染时才决定；
  - 0.24 把 `InlineTable::preamble` 换成了 `trailing`；
  - 0.25 起，`Time` 的秒和纳秒变成 `Option`。

**要求：**
1. **用户现有的文件照常能读。** 包括 `config.toml`、folder trust、onboarding 等，用现有格式的样例做测试。
2. **用户降级后也能读新写出的文件。** dev 依赖里用别名引入 toml 0.8，测试中用它把新版写出的内容再解析一遍。
3. **不开 `preserve_order`。** 计划阶段逐处确认，没有哪里依赖文档顺序反序列化。2026-10-08 初查：没有 IndexMap 这类依赖顺序的容器。
4. **子项目 1 的行为不变：** `orca mcp add` / `remove` 时，注释跟着自己的条目走，现有的往返测试全部通过。测试里写死的输出文本如果要按新格式更新，在提交说明里逐条说明原因。

### 3.2 reqwest 0.12 → 0.13，hickory-resolver 0.25 → 0.26，TLS

**变化（reqwest 0.13.0 起）：**
- 默认 TLS 后端是 rustls，默认加密实现是 aws-lc。`rustls-tls` 改名为 `rustls`；想换别的加密实现，用 `rustls-no-provider`。
- 去掉了内置根证书相关的 feature，默认用 `rustls-platform-verifier`，也就是系统证书库。
- `form` 和 `query` 改成了 feature，默认不开。
- 0.13.4 起依赖 hickory-resolver 0.26，所以两者必须一起升；0.13.5 起，hickory 解析改成优先 IPv6（`Ipv6AndIpv4`）。

**决定（2026-10-08 用户选定）：** 用系统证书库，加密实现继续用 ring。

**要求：**
1. **feature：** workspace 里 reqwest 的 feature 改为 `blocking`、`hickory-dns`、`json`、`stream`、`rustls-no-provider`、`form`、`query`。继续 `default-features = false`，也就是 http2、charset、system-proxy 都不开，和现在一样。
2. **统一入口：** 在 orca-mcp 里新增 `http` 模块。用 reqwest 的四个 crate（orca-mcp、orca-provider、orca-tools、orca-runtime）都依赖 orca-mcp，所以放在这里。
   - 它提供构建 async 和 blocking 客户端的函数。
   - 第一次调用时，把 ring 装成整个进程默认的 rustls `CryptoProvider`，只装一次；如果已经有别处装过，就沿用。
3. **所有构建客户端的地方都走统一入口。** 包括直接写 `Client::new()` 的地方和测试代码。新增边界校验脚本 `scripts/validate-http-client-boundary.mjs`，带自己的测试，接进 `runtime-contract.yml`：在统一入口以外直接构建 reqwest 客户端，校验就失败。
4. **代理行为不变：**
   - 现在会读 `HTTPS_PROXY` 这类环境变量代理的客户端，照旧读；
   - `web_search` 只在测试构建里用 `no_proxy()`，这一点也不变。
5. **hickory 0.26：** 适配 `crates/orca-runtime/src/network_proxy.rs` 里解析器的构建和查询。
6. **真实请求：** 用真实 API key 在这台 Mac 上发请求，确认 TLS 和 DNS 正常（包括优先 IPv6）。升级前后各测 3 次首个请求的连接耗时，取中位数对比。升级后如果多出 300 ms 以上，先查明原因（比如 IPv6 优先），报告用户后再决定怎么处理。API key 的用法：把 `~/.orca/auth.json` 复制到临时目录、权限设成 600，用完删掉，不改原文件的权限。
7. **文档和发布说明：** HTTPS 证书改由系统证书库验证。
   - 公司代理的自签 CA，装进系统证书库就能用；
   - 精简版 Linux 需要先安装 ca-certificates。

### 3.3 rusqlite 0.32 → 0.40

**变化：**
- 0.35 起，`execute` 和 `prepare` 遇到一段 SQL 里有多条语句会报错。以前是静默只执行第一条。
- 0.38 起：
  - u64/usize 默认没有 `ToSql` / `FromSql`，打开 `fallible_uint` feature 可以恢复；
  - 最低支持的 SQLite 版本是 3.34.1；
  - 语句缓存变成 feature，默认开着。
- 0.40 的改动都在 VTab 上，Orca 没用到。
- 自带的 SQLite 从 3.46 升到 3.53.2。

**要求：**
1. **打开 `fallible_uint`。** u64/usize 的读写行为和现在一样：超出 i64 范围时报错。
2. **逐处检查 SQL。** 检查 orca-runtime 五个源文件里传给 `execute` / `prepare` 的 SQL，一段里有多条语句的改用 `execute_batch`。补测试覆盖建表和迁移路径。这五个文件是 `memory.rs`、`memory/index.rs`、`goal_store.rs`、`task_output/persistence.rs`、`thread_store/session_index.rs`。
3. **现有的库文件照常能读写。** 包括 session index、goals、memory index、task output，测试里用现有格式生成库文件，不碰真实的 `~/.orca`。

### 3.4 其余

| 依赖 | 变化 | Orca 要做的 |
|---|---|---|
| sha2 0.10 → 0.11 | 摘要结果换成 hybrid-array，不能再用 `{:x}` 格式化 | 在 orca-core 加一个小写十六进制函数，替换 orca-runtime 和 orca-provider 里格式化摘要的地方；用测试固定几个已知输入的 SHA-256 值，资产校验路径也要覆盖 |
| pulldown-cmark 0.12 → 0.13 | 新增上标、下标、WikiLink 语法，对应选项默认不开 | TUI 里匹配 `Tag` / `TagEnd` 的地方补上新分支；不打开新选项 |
| base64 0.22 → 0.23 | 新增 SIMD 引擎等，用法不变 | 无 |
| dirs 6 → 7 | 只改了 Windows 上的 `preference_dir` | 无，Orca 只用 `home_dir` |
| shlex 1 → 2 | 删除了早已弃用的 `quote` / `join` | 无，Orca 只用 `split` 和 `try_quote` |
| zstd 0.13 → 0.14 | 字典相关的 API 改为借用 | 无，Orca 只用流式 API |

## 4. TUI 栈（一个提交）

**版本：**
- ratatui 0.30.2：显式加上 `crossterm_0_29` feature，保留 `scrolling-regions` 和 `unstable-rendered-line-info`；
- crossterm 0.29；
- ratatui-image 11.1。12 还只是 RC，不用；
- tui-textarea 0.7 换成 ratatui-textarea 0.9.3；
- unicode-width 跟着从 0.2.0 升到 0.2.2。ratatui 0.29 把它固定在 0.2.0，0.30 放开了。

**为什么只能一个提交：** ratatui-textarea 从 0.8 起只支持 ratatui 0.30（依赖 ratatui-core 0.1），而 tui-textarea 不支持 0.30，拆开做的话中间状态编译不过。

**要改的代码**（对照 ratatui 0.30 的 BREAKING-CHANGES）：
1. **`Backend` trait：** 新要求一个关联的 `Error` 类型和 `clear_region` 方法。要改的实现：`CapabilityBackend`（`crates/orca-tui/src/capability_backend.rs`），测试里的 `RecordingBackend` 和 `FailingBackend`，以及 `ui.rs` 里的 `RecordingBackend`。
2. **`TestBackend`：** 错误类型变成 `Infallible`，测试里的错误处理跟着改。
3. **改名：** `layout::Alignment` 改叫 `HorizontalAlignment`，共 7 处。
4. **输入框：**
   - 29 个文件里的 `tui_textarea` 换成 `ratatui_textarea`；
   - placeholder 改用 0.9 的接口，有 4 处，包括 `composer_textarea.rs`；
   - 不启用 0.9 新增的软换行。
5. **图片预览**（`crates/orca-tui/src/image_preview.rs`）：按 ratatui-image 10 和 11 的 API 调整。例如 `FontSize` 变成了结构体，`Resize::render_area` 改叫 `size_for`，`area()` 改成 `size()`。
6. **其余编译错误：** 按 ratatui 0.30 的迁移说明处理，不顺带改行为。

**接受的行为变化**（写进发布说明）：
- 按单词移动或删除时，下划线算单词的一部分。Alt+B/F、Ctrl+W 和 vim 模式的 `w` / `b` 都是这样，和 Vim 本身的规则一致。
- 修正中文这类宽字符的光标滚动位置。
- 修正撤销 / 重做后光标越界导致的 panic。
- unicode-width 0.2.2：加入了 Unicode 16 和 17 新字符的宽度，调整了 CJK 歧义宽度字符的处理。
- ratatui-image 11 在终端不回应能力查询时，会改用 ioctl 读字体尺寸，图片大小算得更准。

**不受影响：** crossterm 0.29 唯一的不兼容改动是 `KeyModifiers` 的 Display 在修饰键之间加了 `+`，Orca 没用到。

**要求：**
1. **视觉守卫照常通过：** 4 个 golden frame、颜色守卫、鲸鱼图的字符宽度守卫。哪个 golden frame 有变化，就逐个看实际渲染结果并说明原因，不能直接覆盖基线。
2. **校验器基线：** 校验器里记着 TUI 同名调用点等基线。有变化的话，在同一个提交里更新并说明原因。

## 5. 网站（site/，一个提交）

**版本：**
- vite 7.3.5 → 8.3.3、@vitejs/plugin-react 5.2.0 → 6.1.2，这两个和现在一样写死精确版本；
- typescript ^5.9.3 → ^7.0.2；
- @mermaid-js/mermaid-cli ^11.16.0 → ^12.0.0；
- react、react-dom、@types/react、@types/react-dom 升到 19.3；
- highlight.js 升到 11.12。

其余依赖已经是最新版。

**要求：**
1. **删掉 `overrides` 里的 esbuild 和 postcss。** 它们是 85c4c04a 为打安全补丁钉的，只有 vite 7 依赖。vite 8 不再依赖 esbuild，并且要求 postcss ≥ 8.5.28，比钉的 8.5.25 还新。删完用 `npm ls esbuild postcss` 和 `npm audit` 确认没有把漏洞带回来。
2. **vite 8 改用 Rolldown 打包：**
   - `vite.config.ts` 里的 `build.rollupOptions` 按 vite 8 的写法调整；
   - `@mdx-js/rollup` 插件和 `scripts/prerender.mjs`（用的是 `createServer`）都要照常工作。
3. **TypeScript 7：** `npm run build` 里的 `tsc --noEmit` 直接换成 7。万一不兼容就退回 5.9，在提交说明和发布说明里写明原因。这是 2026-10-05 定的规则，对其他大版本同样适用。
4. **mermaid-cli：** 没有任何脚本调用它，只在手动重画 `site/public/docs-assets/diagram-*.svg` 时用。升级后确认 `mmdc` 能运行。
5. **Node 版本：** vite 8 要求 ^20.19 或 ≥ 22.12，mermaid-cli 12 要求 ≥ 22.13。Pages 用的是 Node 24，本机是 22.22，都满足。
6. **页面不变：** 升级前后各截一组图对比：首页、文档、更新日志，中英文各一套。截图方法是 vite preview 加无头 Chrome。

## 6. 文档

- **`configuration.mdx`（中英文）：** 新增"网络与证书"一节。
  - Orca 用系统证书库验证 HTTPS；
  - 公司代理的自签 CA，装进系统证书库就能用；
  - 精简版 Linux 需要先安装 ca-certificates；
  - `HTTPS_PROXY` 这类环境变量代理照常生效。
- **发布说明**（发版时写 `docs/releases/v0.5.8.md`）：
  - 证书改用系统证书库；
  - 按单词移动时下划线算单词的一部分；
  - bash 状态 `still running` 的修复；
  - 依赖升级概要。
- **发版前按惯例更新：** docs、网站文档、中英文 README，以及 GitHub Release 正文。

## 7. 测试

第 1 节先写会失败的测试，再修。升级各步的测试都要在对应的提交里通过。

用来对比升级前后行为的测试，要在对应依赖升级之前先写好并通过，包括：toml 样例、pulldown-cmark 样例、SHA-256 固定值、现有格式的库文件。

**bash 状态**（reducer 单元测试）：
- 快照显示任务完成，标签消失；
- `task_read_output` 带 exit 2，显示 `exit 2`；
- `task_wait` 一次返回多个任务的结果；
- 不相关的 `task_id` 不会动到别的行；
- 任务还在跑时，保持 `still running`；
- 恢复会话后状态正确，包括查不到任务时显示 `state unknown`。

**toml：**
- 用现有格式的样例做读取测试；
- 新写出的内容，用 toml 0.8 再解析一遍；
- 子项目 1 的注释往返测试全部通过。

**reqwest：**
- 一个集成测试在全新的进程里，第一次通过统一入口构建客户端，不 panic；
- 环境变量代理的测试；
- 边界校验脚本和它自己的测试；
- 第 3.2 节第 6 条的真实请求，以及升级前后的连接耗时对比。

**rusqlite：**
- 建表和迁移路径（覆盖多条语句的情况）；
- u64/usize 的读写；
- 用现有格式的库文件做读写测试。

**sha2：** 几个已知输入的 SHA-256 值，以及资产校验路径。

**pulldown-cmark：** 现有的 markdown 渲染测试全部通过；再补一个含 `^` 和 `~` 的样例，渲染结果要和升级前一样。

**TUI：**
- golden frame、颜色守卫、鲸鱼图宽度守卫、PTY 测试全量、校验脚本。
- **Ghostty 真终端实测**（用 `~/.claude/orca-ghostty-harness` 自动跑），覆盖：
  - 多行输入；
  - 粘贴（bracketed paste）；
  - 输入历史；
  - 光标移动；
  - 中文文本的宽度和光标位置；
  - Esc 和 Ctrl+C；
  - 图片预览（kitty 协议）。
- **用户手测：** 中文输入法的组字过程没法自动化。最后给用户一份 3–5 项的清单，内容包括中文输入法、从真实剪贴板粘贴、图片预览。

**网站：** `npm run build`、`npm run check:seo`，以及升级前后的截图对比。

**全量：**
- 所有 node、npm、`cargo test`、`cargo nextest` 命令都加 `env -u NODE_OPTIONS` 前缀，跑这几项：
  - workspace 的 `ci` profile；
  - orca-tui lib（ci-serial）；
  - PTY；
  - 校验脚本。
- 收尾前，在这台 Mac 上跑全量测试，包括 macOS 沙箱测试。
- Linux 和 Windows CI 通过（见第 8 节）。

## 8. 流程

- **在哪做：** 在本地 main 上提交，每步单独提交，第 3 节每个小节一个提交。
- **开工前：** 确认 `target/` 已经清理过。升级后几乎全部要重编，旧产物用不上了。
- **CI：** Windows 只能在 CI 上编译。第 3 节和第 4 节完成后，各推一次 `ci/dependency-upgrades` 分支，跑 Linux 和 Windows CI。用户已在 2026-10-08 同意，但只推这个分支，不碰 main。
- **收尾：**
  - 对整个分支做一轮评审；
  - 跑全量测试；
  - 用户按清单手测。
- **发版：** 推 main 和打 tag 都要用户同意，发不发 v0.5.8 由用户决定。

## 9. 不在本轮

- **ACP SDK：** `agent-client-protocol` 继续固定在 `=0.10.4`，它的 schema 依赖也不动。最新版已经是 3.1.0，子项目 3 再重新评估目标版本。
- **不需要升级的依赖：** nucleo 固定的 git 版本；qwertty 已经是最新版。
- **工具链：** GitHub Actions 里各个 action 的版本，以及 Rust 工具链。
- **新功能：**
  - 输入框的软换行（ratatui-textarea 0.9 的新功能）；
  - 在 bash 那一行补上后续输出。
- **reqwest：** http2 和系统代理设置（system-proxy）继续不开。
- **第三方许可清单：** zstd 改成了 BSD-3-Clause，但 Orca 目前不发布第三方许可清单，本轮不新增。
