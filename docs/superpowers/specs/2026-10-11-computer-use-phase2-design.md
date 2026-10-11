# Orca Computer Use 阶段 2 设计（macOS）

> 状态：2026-10-11 逐节确认，待写实施计划。
> 总体设计见 [`2026-09-27-computer-use-design.md`](2026-09-27-computer-use-design.md)（下称"总设计"）。本文写阶段 2 的范围和阶段 2 对总设计的修改；本文没写到的地方照总设计执行。实验 2–4 的结论在总设计第 9 节。

## 0. 范围

阶段 2 是一个补丁版本，做完后 computer use 就可以用了。

- **动作**：`list_apps`、`launch_app`、`observe`、`click`、`type`、`key`、`scroll`。后台优先，后台做不到时短暂切到前台（第 1 节）。
- **默认可用**：没有开关。操作任何应用前都要用户授权（第 4 节）。
- **入口**：只在交互式 TUI 的主会话里。子代理、工作流、ACP、`orca exec` 都拿不到这个工具。
- **系统**：macOS 14 及以上，ScreenCaptureKit 截单个窗口的接口从 14 开始才有。其他系统不注册这个工具。
- **一并处理**阶段 1 留下的图片开销（第 5 节）。

留给阶段 3：`drag`、`set_value`、风险分级细调、15 个任务的验收集。ACP 和 `orca exec` 入口在阶段 3 之后另定。

## 1. 投递：后台优先，必要时切前台

实验 3 表明，在后台只能走无障碍接口，以及先设焦点、再用 `postToPid` 发给进程的键盘事件；鼠标点击和滚轮都发不进后台窗口。各动作的做法：

| 动作 | 后台做法 | 什么时候切前台 |
| --- | --- | --- |
| `click` 元素 | 左键用 AXPress，右键用 AXShowMenu | 元素不支持对应动作；双击；带修饰键 |
| `click` 坐标 | 用 `AXUIElementCopyElementAtPosition` 找到该点的元素，它或它的某个祖先能按下就 AXPress | 找不到能按下的元素（画布类界面）；应用被隐藏；双击、右键或带修饰键 |
| `type` | 用 AX 给目标设焦点（没给 `target` 时用当前焦点），再发 Unicode 键盘事件 | 设焦点失败 |
| `key` | 同 `type`：设焦点，再发组合键 | 组合键在后台无效时。菜单快捷键（如 Cmd+S）能否在后台生效，实施时真机验证；不能的话，带 Cmd 的组合键一律走前台 |
| `scroll` | 暴露了滚动条就设它的 AX 值；否则对滚动方向上的下一个元素执行 AXScrollToVisible | 两种都不支持 |
| `observe`、`list_apps`、`launch_app` | 无障碍接口、NSWorkspace、ScreenCaptureKit | 不需要 |

**什么时候回退**：只有后台方式明确失败才切前台，也就是接口报错、元素不支持该动作或找不到元素。效果只是无法确认时不重试，交给模型看动作后的观察。否则一个其实已经生效的动作会在前台再做一次，比如勾选框被切换两次。这一条替换总设计第 6 节"后台投递未生效"一行。

**前台回退的过程**：

1. 等用户 0.5 秒没有键盘鼠标输入，最多等 3 秒。用户还在操作就放弃，报告"用户正在操作"。
2. 记下当前的前台应用和鼠标位置，把目标应用和窗口提到最前。
3. 用 HID 级事件执行动作。
4. 松开所有可能按下的修饰键和鼠标键，恢复原来的前台应用和鼠标位置。
5. 在结果里注明用了前台。

配置 `[computer_use] foreground_fallback = false` 关闭回退，需要前台的动作直接报告做不到。回退期间 Esc 同样能中止。

## 2. 驱动

组件划分照总设计第 2.1 节：

- `orca-computer-use` crate：`protocol`（所有平台都能编译）、`driver`（仅 macOS）、`fake`（测试用的进程内驱动，实现同一协议）。
- 驱动进程是 `orca` 的隐藏子命令 `orca computer-use-driver`。
- runtime 里的 computer-use 服务、内置的 `computer` 工具、TUI。

**绑定**：用 objc2 系列 crate，全部放在 `cfg(target_os = "macos")` 下。工作区已经依赖 `objc2`、`objc2-app-kit`、`objc2-core-graphics`、`dispatch2`，新增 `objc2-application-services`（无障碍接口）和 `objc2-screen-capture-kit`。不另做 Swift 辅助程序：npm 包里每个平台只有一个二进制，也不必多一套构建和签名。

**进程**：

- 第一次调用 `computer` 时启动。stdin 和 stdout 是管道，一行一个 JSON。stderr 写到 `$ORCA_HOME` 下的日志文件，不继承 TUI 的终端；之前 PTY 测试挂起，就是工作进程占着终端造成的。
- stdin 读到 EOF 就立即退出：移除事件监听，松开所有键和鼠标键，释放会话锁。Orca 退出或崩溃时管道关闭，驱动随之退出。
- 启动时先交换协议版本。会话期间 Orca 被升级、新旧版本对不上时，返回"Orca 已更新，请重新开始会话"。
- 每个无障碍调用设 2 秒的消息超时，卡住的应用不会拖住驱动。请求超时沿用总设计第 6 节（默认 30 秒，`launch_app` 60 秒），超时后 runtime 结束驱动，下次调用时重启。

**协议**：

- runtime 发给驱动：`hello`（协议版本）、`list_apps`、`launch_app`、`observe`、`act`（`click`、`type`、`key`、`scroll`，带 `target`、选项和是否允许前台回退）、`turn`（本轮开始用 computer use，或本轮结束；用来装上和移除 Esc 监听）、`shutdown`。
- 驱动返回：每个请求一个结果。成功时带观察（元素树文字、截图 PNG、截图尺寸、窗口编号），失败时带错误类别（第 3 节）。另有一个 `esc` 通知。

**截图和坐标**：

- 用 ScreenCaptureKit 截单个窗口（`SCScreenshotManager` 配 `desktopIndependentWindow` 过滤器），编码成 PNG。
- 最长边超过 1280 像素才缩小，不放大。
- 驱动记下截图时的窗口位置和缩放比例，用它把坐标目标换算回屏幕坐标。

**元素树**：第一次观察某个应用时，对它设置 `AXManualAccessibility = true`，不区分是不是 Electron 应用，不支持的应用只会返回错误。如果元素树里只有窗口外框，最多轮询 3 秒；仍然没有，就在结果里说明"这个应用没有元素树，请用坐标"。

**切到前台**：对应用设 `AXFrontmost`，对窗口执行 `AXRaise`，恢复原来的前台应用也用这个办法。macOS 14 起，非前台进程发出的普通激活请求可能被系统忽略，窗口管理工具通常这样做。实施时真机验证。

**Esc 监听**：

- 本轮用过 computer use 才装上，本轮结束就移除。
- 吞掉 HID 级的 Esc，并通知 runtime 中止本轮。在 TUI 里按的 Esc 也会被它吞掉，效果一样。
- 驱动在前台回退时自己发的 HID 事件带上标记（`kCGEventSourceUserData`），监听会放行，所以驱动发的 Esc 不会误中止本轮。
- 被系统因超时停用时，重新启用。

**系统权限**：启动时检查辅助功能、屏幕录制，以及事件监听能否创建。缺哪项就说明去系统设置的哪里打开，并提示授予屏幕录制后可能要重启终端。

**会话锁**沿用总设计第 5.5 节。

**CI 校验器**：新增的 macOS 接口代码会改变平台边界校验器的基线，在同一个提交里更新基线。

## 3. `computer` 工具

照总设计第 3 节。修改和补充：

- 工具定义只列阶段 2 的 7 个动作，`drag`、`set_value` 到阶段 3 再加。
- 观察结果的第一行写明截图尺寸和坐标约定。元素位置也用截图像素（总设计没写元素位置的单位）：

  ```text
  Safari "Apple"：截图 1280×832 像素，原点在左上角，坐标都以这张截图为准
  [12] button "保存" (840,52,64,28)
    [13] text field "文件名" = "report.txt" (120,90,300,24)
  ```

- 工具说明告诉模型：
  - 命令行和文件工具能做的事不要用 `computer`；验证自己写的界面正是它的用途；
  - 优先用元素编号，没有元素时才用坐标；
  - 用坐标操作后，看动作后的观察确认结果。
- 动作结果注明是否用了前台，比如 `click (412,300)：该位置没有元素，用了前台（已切回 cmux）`。
- 新的错误类别：需要前台但回退已关闭；用户正在操作，放弃了回退；应用没有元素树。
- `type` 在焦点处插入文字，不替换原有内容；替换留给阶段 3 的 `set_value`，要清空可以先 `key ["cmd","a"]`。开着中文输入法时文字能否直接输入，实施时真机验证；不能的话改为设置 `AXSelectedText` 插入。
- `key` 的键名：`cmd`、`shift`、`option`、`ctrl`、`return`、`escape`、`tab`、`space`、`delete`、方向键、`home`、`end`、`page_up`、`page_down`、`f1`–`f12`，以及字母、数字和符号。
- `scroll` 的 `amount` 在后台是近似的，每格约为可见高度的十分之一。知道滚动位置时（设滚动条的值），结果里报告滚到了哪里。

## 4. 授权

照总设计第 5 节。修改和补充：

- **默认可用，没有开关**（替换总设计第 0 节的"默认状态"和第 2.3 节）。在 macOS 14 及以上的交互式 TUI 里，主会话始终有 `computer` 工具。缺系统权限时，第一次调用就说明缺什么、去哪里打开。
- **完全关闭**：加一条整个工具的 deny 规则。有这条规则时不注册 `computer` 工具，模型看不到它，也就不会反复调用再被拒绝：

  ```toml
  [[permissions.rules]]
  tool = "computer"
  decision = "deny"
  ```

- **规则和风险分级**：
  - 整个工具的规则和带通配符的规则，能放行或禁止普通应用和强提示应用；
  - 密码管理器和金融类应用，只能由 `pattern` 正好等于其 bundle id、不带通配符的规则放开；
  - 运行当前 Orca 的终端，任何规则都放不开；
  - deny 规则仍然优先于 allow 规则。
- **cmux 加入终端清单**（`com.cmuxterm.app`）：它是宿主终端时禁止操作，否则是强提示应用。
- **授权面板的选项**："仅这一次"、"本会话允许该应用"、"总是允许该应用（保存为规则）"、"拒绝"。
  - "总是允许"写入一条 `pattern` 为该应用 bundle id 的规则。
  - 强提示应用照样显示警告。
  - 禁止的应用不弹面板，直接拒绝，并在结果里说明怎样放开；宿主终端则说明不能放开。
- **只给主会话**：子代理和工作流在阶段 2 拿不到 `computer` 工具。异步子代理在独立的工作进程里运行，会各自启动驱动、争抢机器级的锁；授权面板也只在主会话里显示。
- **审批模式**：
  - `list_apps` 和 `observe` 按读取算，plan 模式可以用（`observe` 仍要应用授权）。
  - 其余动作用新增的 `ActionKind::Computer`，plan 模式直接拒绝。
  - 其他模式下，应用授权是唯一的关卡，不再按动作逐个询问。
- **密码框**：向 `[secure]` 字段输入时，驱动返回"需要单独批准"；runtime 询问用户后，带着批准重发（总设计第 5.6 节）。

## 5. 图片开销

阶段 1 推到阶段 2 的几件事（总设计第 4 节的补充）。一个会话会有几十上百张截图，这几件都要做：

1. **实时对话里只保留最近 3 张工具图片的数据**，更早的只留资源引用和"已被更新的观察取代"的说明。请求本来只发最近 3 张，所以模型看到的不变，内存有了上限。压缩快照已经这样做（阶段 1 的裁定 R13）。TUI 预览旧截图时从磁盘上的资源文件读取。
2. **按文件头判断图片类型**：PNG、JPEG、GIF、WebP 的文件头和声明的类型对不上时，以文件头为准；四种都不是就丢弃并注明。驱动的截图总是 PNG，这一条主要针对 MCP 图片。
3. **深度压缩只计入会发出去的图片**。现在它把所有工具图片的 token 都算进去，压掉的历史比需要的多。
4. **恢复会话时不读取工具图片的数据**。现在会逐张读进来再丢掉。

## 6. TUI

- `computer` 工具行显示摘要，比如 `observe Safari：142 个元素，截图 1280×832`、`click [12]`；用了前台时加上"用了前台"。
- 截图可以像 `[Image #N]` 附件一样，把光标移上去按 Enter 预览。
- 操作期间状态栏显示"正在操作 Safari… 按 Esc 中止"，前台回退时加上"暂时切到前台"。
- 授权面板复用现有的审批面板，选项见第 4 节。
- 唯一的配置项是 `[computer_use] foreground_fallback`，默认为 `true`。

## 7. 测试与验收

**自动化测试**：不需要 macOS 权限，所有平台的 CI 都跑。

- **协议和工具定义**：请求和结果序列化后能原样读回；只列 7 个动作；除 `list_apps` 外 `app` 必填；`target` 二选一。
- **授权**：按应用授权；风险分级；只有精确 bundle id 的规则能放开密码管理器和金融类应用；宿主终端（包括 cmux）放不开；plan 模式；整个工具的 deny 规则让工具不注册；子代理和工作流拿不到工具；非 macOS 和 macOS 14 以下不注册。
- **假驱动测 runtime 服务**：超时和重启；过期观察；Esc 中止并松开按键；会话锁冲突；前台回退的触发条件（只在明确失败时回退、关闭时不回退、用户正在操作时放弃）；结果文字（是否用了前台）；观察格式（截图尺寸行，元素位置用截图像素）。
- **图片**：实时对话只保留最近 3 张的数据；压缩只计入会发出去的图片；恢复时不读工具图片；按文件头判断类型。
- **TUI**：工具行摘要、状态栏、授权面板，用黄金帧按现有的视觉约定检查。

**真机测试**：没有 macOS CI runner。像现有的 Seatbelt 测试一样，缺权限时自动跳过，发版前在 Mac 上跑。

- **测试应用**：一个几十行的 Swift 小应用，测试时用 `swiftc` 编译，没有 `swiftc` 就跳过。里面有按钮、输入框、能滚动的长列表，以及一块没有元素树的画布。用它跑 `observe`、`click`（元素和坐标）、`type`、`key`、`scroll` 的完整往返和前台回退。不用系统应用，测试就不受系统语言和版本影响。
- **实施时要在真机确认的事**（前几节提到的）：
  - macOS 14 及以上切到前台和恢复；
  - 后台组合键能否触发 Cmd 快捷键；
  - 开着中文输入法时输入；
  - 空闲判断会不会把驱动自己的事件当成用户操作；
  - 驱动自己的 Esc 能否被放行；
  - Electron 应用的元素树（用环境变量指定一个已安装的 Electron 应用）；
  - 一个 Catalyst 应用。
- **实验里的教训写成测试规则**：开始前先记录状态；退出自己启动的应用；恢复前台应用和鼠标位置；每个否定结论都要有同条件下的正向对照。

**验收**：用真实的二进制和 DeepSeek 跑约 6 个任务，记录结果：

1. 在文本编辑里输入一段话，保存到 `/tmp`；
2. 用计算器算一道题；
3. 在访达里把一个长列表滚到底；
4. 写一个小网页，用浏览器打开并截图检查（用途 A）；
5. 在测试应用的画布上按坐标点击；
6. 在一个 Electron 应用里做一步操作。

15 个任务、单步 90% 和多步 60% 的目标留给阶段 3。

## 8. 发版

阶段 2 发一个补丁版本（目前是 v0.5.11）。打 tag 前更新文档：新增中英文"Computer use（macOS）"页面，写明系统权限、按应用授权、风险分级、Esc、前台回退，以及怎样用 deny 规则关闭；同时更新 README 和发版说明。

## 9. 对总设计的修改

| 总设计 | 改为 |
| --- | --- |
| 第 0 节"默认状态：关闭"、第 2.3 节的开关、第 5.1 节"默认关闭" | 默认可用，没有开关，用整个工具的 deny 规则关闭（本文第 4 节） |
| 第 3 节没写元素位置的单位 | 截图像素，观察的第一行写明截图尺寸（本文第 3 节） |
| 第 3 节和第 6 节：后台投递未生效时切前台重试 | 只有后台方式明确失败才回退，效果无法确认时不重试（本文第 1 节） |
| 第 5.2、5.3 节"禁止（可由规则放开）" | 只认 `pattern` 正好等于 bundle id 的规则（本文第 4 节） |
| 第 5.3 节的终端清单 | 加入 cmux |
| 第 8 节：前台回退在阶段 3 | 前台回退移到阶段 2 |

## 10. 实施时的真机结论

2026-10-12，在 Apple Silicon、macOS 26.6.2（25G83）上执行 Task 13。每次 GUI 检查记录起始前台应用、指针和运行中的应用；检查后退出本次启动的应用并恢复前台与指针。下列结果来自本机，不代表其他 macOS 版本或应用都具有相同表现。

| 项目 | 怎么查及结果 | 因此改动的代码 |
| --- | --- | --- |
| Step 1：切到前台和恢复 | 扩展 `a_canvas_click_uses_the_foreground_and_restores_it`，先用前台画布点击作正向对照，再从 Finder 点击画布和 Password。画布成功，但从 Finder 点击 Password 的激活／恢复步骤复现 Task 8 留下的前台超时：`AXFrontmost` 设置成功，1 秒内实际前台 PID 未变为目标，返回 `The app did not become frontmost`。改动后两种点击都能恢复 Finder 与指针。 | 激活和恢复统一使用 `NSRunningApplication::activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows)`；窗口仍执行 `AXRaise`，仍检查实际前台 PID。替换第 2 节写的 `AXFrontmost` 办法。真机测试的恢复也使用该 API。 |
| Step 2：后台 Cmd 快捷键 | `cmd_a_reaches_the_focused_field` 得到 `abcx`，不是 `x`；新增 `cmd_s_saves_textedit_in_the_background`，在自己的临时 `.txt` 中输入后发 Cmd+S，文件修改时间未改变。同一输入框／文档中的前台 HID 正向对照分别得到 `x` 和文件保存。修复后两条测试都断言 `foreground_used`，并核对选区替换／修改时间和文件内容。 | `actions.rs` 的 `key` 在带 Cmd 时直接返回 `NeedsForeground`，不先投递后台按键；沿用配置控制的前台回退。夹具增加标准 Edit → Select All 菜单，让前台 Cmd+A 正向对照有真实的 AppKit 快捷键处理。 |
| Step 3：中文输入法 | `pending: run with the user`。按控制器 R2，本任务不切换用户的输入法。 | `type` 未改变。 |
| Step 4：空闲和驱动事件 | 新增 `two_foreground_actions_in_a_row_do_not_count_as_user_activity`；第一次点击是正向对照，两次画布点击间隔 100 毫秒。日志记录两次点击，第二次约 482 毫秒完成，没有返回 `UserActive`。 | 无须增加“距自己的最后一次 HID 事件不到 0.5 秒则跳过等待”的例外；保留等用户空闲 0.5 秒、最多 3 秒的实现。 |
| Step 5：Electron | `ORCA_CU_ELECTRON_APP=com.kangfenmao.CherryStudio`，运行 `observe_of_an_electron_app_waits_for_its_tree`。启动后立即观察曾报 `AXWindows` 的 `AXError(-25204)` 或还没有 AX 窗口；测试先等首次捕获窗口，再在 3 秒内重试观察，并记录每次失败。初次就绪观察只有 3 个 button；最终复测通过：`no_element_tree=false`，70 个元素、31 个 button、0 个 text field。记录的是当时界面的可见树，不声称所有 Electron 内容控件都可用。同一驱动观察夹具，button 和 text field 均存在。只观察，未点击或输入 Cherry Studio，结束后退出本次启动的进程。 | 只调整测试的启动就绪检查、增加正向对照和结果记录；没有改 Electron 生产实现。 |
| Step 5：Catalyst / Stocks | 本机 bundle id 是 `com.apple.stocks`。启动后欢迎界面还会异步加载，等待加载后通过驱动对 Continue／继续执行一次按下，随后只观察。欢迎页已关闭；结果为 `no_element_tree=false`、26 个元素、16 个 button、1 个 text field。同一驱动观察夹具，两种角色均存在。退出本次启动的 Stocks。 | 仅新增真机检查；没有改 Catalyst 生产实现。 |
| Step 6：真实 TUI Esc | `cargo build --release --locked`，在从当前 shell 启动的独立 tmux server 中运行 `./target/release/orca`。使用 `/tmp` 下的新 `ORCA_HOME`、工作区和本地 mock provider、dummy key；mock 发一个观察夹具的 `computer` 调用，审批面板按 `1` 允许一次，mock 配置为以每秒约两个 token 输出约一分钟，按 Esc 的检查在输出期间进行。tmux 发送 Esc 与夹具前台的无标记 HID Esc 均使 TUI 显示 `Task cancelled [operation.cancelled_by_user]`，操作终态为 `cancelled`、原因 `user`，两次下一条消息都正常完成。HID Esc 在活动轮次未进入夹具日志；无轮次时同样事件的正向对照能进入，结束后也再次进入。 | Esc 两种路径通过，因此不需要 Task 10 的 `ThreadCommand::InterruptActive` 改动。中断轮次后正常开始并完成下一轮，也覆盖 Task 10 留下的生产轮次释放 turn store 的检查。 |

Step 6 还发现了进程内测试没覆盖的真实驱动进程问题：观察时 CoreGraphics 报 `CGS_REQUIRE_INIT` 并中止进程，崩溃栈位于 ScreenCaptureKit 的 `SCContentFilter` 构造回调。先新增并跑失败的 `real_macos_driver_observes_the_fixture_from_its_own_process`，再在 `run_stdio` 的主线程、启动 worker 前初始化 `NSWorkspace` 和 CoreGraphics（创建一个不投递的 `CGEvent`）。仅初始化 `NSWorkspace` 仍失败；完整修复后，真实驱动进程的夹具观察、截图和 turn 开关测试通过，真实 TUI 的工具调用也成功。进程仍只由 `ExecutionBroker::launch_user_trusted` 启动，stdio 和 stderr 的路由未改变。

Step 7：各代码修复分别提交；本节使用任务简报指定的提交消息记录。提交前执行格式、workspace clippy、两个平台／runtime surface 校验器和修改 crate 的测试。没有推送、切换分支或使用用户的 `~/.orca`、真实 API key。

**修复轮次 1（2026-10-12）**：上表保留初轮的观察记录；以下两项更新生产行为和验证结论。

- 观察和按元素 id 操作的 stale 检查共用一个 label helper，按 `AXTitle`、`AXDescription`、`AXPlaceholderValue` 的顺序读取，并统一使用 R7 的 optional-read 语义。夹具 NSTextView 的 `AXDescription` 返回 Failure 时，按观察中的 id 滚动也成功；TextEdit 的文本区同样可以按 id 输入。角色和位置／大小仍是 required reads，标签变化仍返回 `Stale`。
- `launch` 在已启动进程出现后启用无障碍支持，等到 `AXWindows` 可读且非空再返回；无窗口的应用最多等 10 秒后仍启动成功，等待受原 60 秒 launch deadline 限制，最后一次 AX 调用的 timeout 也不超过剩余时间。打开文件的路径使用同一就绪检查并刷新返回的窗口信息。Electron 测试已删除 harness 的 capture-window wait 和 3 秒 observe retry；Cherry Studio 现在在 `launch_app` 返回后立即观察成功，仍记录元素及控件数量。

Step 3 的真机测试现在是 `typing_with_a_chinese_input_method_enters_the_text_exactly`：通过 Text Input Sources 的 `TISCopyCurrentKeyboardInputSource`／`kTISPropertyInputSourceID` 读取并打印当前 ID，只接受 `com.apple.inputmethod.SCIM` 或 `com.apple.inputmethod.TCIM` 及其子 ID，否则明确 skip；测试不切换输入法。Chinese 分支用现有 `type` 输入 `abc 中文`，核对最后一条输入日志和 Name 字段当前值均完全相同。本轮在 `com.apple.keylayout.ABC` 上只验证了 skip；中文输入实际结果仍是 `pending: run with the user`，`type` 投递实现没有改动。
