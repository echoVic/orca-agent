# Orca Computer Use 阶段 3-1：可靠性与恢复设计（macOS）

> 状态：2026-10-24 设计确认（方案 A：驱动修复 + 错误引导 + 单轮失败预算），待写实施计划。
> 来源：阶段 3 的第一个子项目。用户 2026-10-24 选定新顺序：先可靠性，再窗口边界、速度、界面。
> 依据：干净基线 `.superpowers/sdd/2026-10-24-computer-use-eval/report.md`（两次完整实测，harness 见 PR #148）。

## 0. 背景和范围

修好审批（#147）后，干净基线两次运行的结果是：单步 7/8、7/8，多步 5/7、6/7；每次运行有 37–40 个工具错误。错误按来源分组（两次合计 77 个）：

| 来源 | 次数 | 主要任务 |
| --- | ---: | --- |
| GUI 失败后改用 bash：`open`、`open -a`、`osascript`，都被沙箱拦下 | 19 | M5、M3、S3、S4 |
| 其他失败的 bash 绕路（翻 app 包、`sdef`、`plutil` 等） | 16 | S7、M5、S4 |
| 用 `list_apps` 给的窗口 id 去 observe/act，但该窗口没有 AX 窗口 | 15 | M5、S4、S3、M1、M2、M3、S5 |
| System Settings 的 `AXRaise` 返回 -25205 | 14 | M5、S4 |
| 参数错误（如 `"shift+right"`；`keys` 里放了三个 `right`） | 7 | M7、M5、S8、M1、M3 |

其中有几处还会把模型带偏：

- **错误信息误导。** 驱动把三种情况都报成同一句 "the window changed since the last observe; observe again"：没有先 observe、窗口 id 不对、窗口真的变了（`actions.rs` 的 `act` 把 `WindowNotFound` 和"没有元素表"都映射成 `stale()`）。
- **启动后马上 observe 失败。** TextEdit 启动时只显示系统打开面板，这时 TextEdit 自己没有 AX 窗口：`launch_app` 只返回 "launched 文本编辑"，随后 observe 报 "The app has no accessibility window"，Cmd+N 又被报成窗口已变。模型由此转向 shell。
- **失败后不收手。** run 6 的 M5 在 System Settings 上连续失败，一共用了 32 次调用，中途还执行了 `killall "System Settings"`。auto-edit 下 shell 是自动放行的，只是沙箱让这条命令没有生效。

**目标：** 动作更常一次成功；失败时给出明确的下一步，不再走 shell 绕路或乱试；连续失败时及时停下来告诉用户。

**范围内：**

1. `list_apps` 和 `launch_app` 只列出模型真正能用的窗口。
2. 区分错误原因，每种错误都带下一步。
3. 启动就绪：app 还没有窗口时，结果里明说；允许不带窗口发送 app 级快捷键。
4. System Settings 的 AXRaise 回退。
5. 修改工具说明，并兼容常见的按键写法。
6. 单轮失败预算：连续第 3 次失败时提示，第 6 次起停用该 app。

**不做（留给后续子项目）：**

- 模型能不能用用户已打开的窗口、`kill`/`killall` 是否需要审批：子项目 2「窗口边界」。
- 计算器逐键点击、动作后自动观察、大树裁剪：子项目 3「速度」。
- 叙述、截图缩略图、屏幕指示：子项目 4「界面」。
- 沙箱里 bash 失败时追加提示。先看工具说明修改后的效果，再决定要不要做。

**成功标准**（用 harness 跑两次，与 2026-10-24 基线对比；每次实测都要用户离开电脑，并在跑之前征得同意）：

- 每次运行的工具错误 ≤ 15（基线 37、40）。
- "No accessibility window matches the capture window" 降到 0。
- AXRaise 错误每次 ≤ 2；S4 和 M5 两次都通过。
- 失败的 bash `open`/`osascript` 每次 ≤ 5。
- 成功率不低于基线（单步 ≥ 7/8，多步 ≥ 6/7）。S3、M3 的失败属于窗口边界问题，照常计入，但不算本子项目的回归。
- 每项工具调用的中位数不高于基线（9）。

## 1. 只列出可用窗口

**现状。** `driver/macos/apps.rs` 的 `windows()` 用 `SCShareableContent` 取每个 app 在 layer 0 上的全部窗口，其中包括离屏的辅助窗口和状态气泡。基线里每个 app 都有好几个这样的窗口，它们没有标题，也没有 AX 窗口；模型挑中其中一个，就会失败。

**新规则。** 一个窗口要同时满足下面三条才算"可用"：

1. 在 `SCShareableContent` 里位于 layer 0。
2. 能按 frame 匹配到该 app 的某个 AX 窗口（沿用 `frames_match`，误差 ±2 pt）。
3. 满足以下任一条件：
   - 在屏幕上，并且 AX subrole 是 `AXStandardWindow` 或 `AXDialog`，或者尺寸至少 120×60 pt；
   - 对应的 AX 窗口 `AXMinimized` 为真（这种窗口要标注"已最小化"）。

`list_apps`、`launch_app` 的窗口列表都按这个规则过滤。observe 没给 `window` 时，原来取 `AXMainWindow`，取不到就取第一个 AX 窗口；改为：主窗口可用就用主窗口，否则取第一个可用窗口。

**AX 查询的时间预算。** `list_apps` 要查询所有常规 app：

- 每个 app 的 AX 消息超时设为 0.25 s，所有 app 合计不超过 2 s。
- 超时或拒绝回答的 app 退回到"在屏幕上、且至少 120×60 pt"的规则，不做 AX 匹配。
- 没有可用窗口的 app 仍然列出，只是没有窗口行。

**协议。** `protocol.rs` 的 `WindowInfo` 新增 `minimized: bool`，带 `#[serde(default)]`，旧数据可以照常解析。不带窗口的 `key` 和新的错误语义都属于行为变化，所以 `PROTOCOL_VERSION` 从 1 升到 2。驱动是由同一个 orca 可执行文件启动的，版本只会在会话进行中 orca 被更新时才不一致；这时 Hello 握手按现有逻辑报 "Orca was updated; start a new session to use computer use"，不会让新旧两端混着跑。

**输出。** 已最小化的窗口这样写：

```text
  window 812: "draft.txt" (minimized)
```

列出的窗口变少了，也就少把其他 app 的窗口标题发给模型；完整的标题策略留给子项目 2。

## 2. 区分错误原因，带下一步

`act` 和 observe 的窗口解析改成下面这些分支。消息给模型看，所以用英文。

| 情况 | ErrorKind | 消息 |
| --- | --- | --- |
| 给的窗口 id 不是这个 app 的可用窗口 | `WindowNotFound` | `Window {id} is not a window of {app} that Orca can use. Call list_apps for its usable windows, or omit window to use the main window.` |
| 这个窗口还没有 observe 过（没有元素表） | `Stale` | `Window {id} has not been observed yet. Call observe on it first.` |
| observe 过，但 frame 变了 | `Stale` | `the window changed since the last observe; observe again`（不变） |
| app 没有任何 AX 窗口（observe，或带元素/坐标的 act） | `WindowNotFound` | `{app} has no window yet. For an app-level shortcut such as cmd+n, use key without window; to open a file or URL, use launch_app with open.` |

ErrorKind 不变，运行时现有的处理不受影响。

## 3. 启动就绪和不带窗口的快捷键

**`launch_app`。** 现在已经最多等 10 s，直到出现 AX 窗口（`wait_for_accessibility_window`），之后再列一次窗口。改动有两处：

- 窗口列表按 §1 过滤。
- 等完仍没有可用窗口时，结果写成：

```text
launched TextEdit (com.apple.TextEdit); no window yet. For an app-level shortcut such as cmd+n, use key without window; to open a file or URL, use launch_app with open.
```

**不带窗口的 `key`。** 只在同时满足以下两条时允许：没传 `window`，并且该 app 当前没有 AX 窗口。执行步骤：

1. 激活该 app。
2. 确认它已在前台（与现有前台路径同一套检查）。
3. 通过 HID 发送组合键。
4. 恢复原来的前台 app 和指针，结果注明用了前台。

其他约束：

- `[computer_use] foreground_fallback = false` 时直接失败，消息说明这需要前台。
- app 有窗口时仍按原规则，先 observe（用 §2 的新消息提示）。
- 审批、Esc 中断、锁屏检查、用户正在操作时让步，与现有前台动作完全一样。

TextEdit 这类沙箱 app 的打开面板由系统面板服务进程托管。本设计不去 observe 这个面板，只保证模型有路可走：发 Cmd+N 新建文稿，或用 `launch_app` 的 `open`。实施时要先在真机上确认：TextEdit 处在打开面板时，`AXWindows` 是否为空，Cmd+N 能否建出可用窗口。

## 4. System Settings 的 AXRaise 回退

**现状。** `driver/macos/foreground.rs` 在 `activate` 之后无条件执行 `window.perform("AXRaise")?`。System Settings 的窗口返回 -25205，整个动作就失败了，即使这个窗口本来就在最前面。

**新流程：**

1. AXRaise 成功：照旧。
2. AXRaise 失败：读该窗口的 `AXMain`，以及 app 的 `AXFocusedWindow`。窗口已是主窗口或焦点窗口，就继续往下走，并在 outcome 的 note 里记一句 `window already in front (raise unsupported)`。
3. 否则等 150 ms 再执行一次 AXRaise；仍失败，就再查一次主窗口和焦点窗口。
4. 还不行就失败：`Could not bring {app}'s window to the front (AXRaise failed: {code}). Call observe again; if it keeps failing, tell the user.`

后面的 `check_window`、遮挡判断、命中测试和前台恢复全部不变。回退只是跳过"抬窗口"这一步，不能绕过任何一项安全检查。

## 5. 工具说明和按键写法

**工具说明**（`crates/orca-tools/src/registry.rs` 里的 computer 描述）。现在写的是 "Use it only for work that needs a GUI … use shell and file tools for everything else"，这句话把模型推向了 shell（M4 直接用 python 计算，M5、S3 改走 `open`/`osascript`）。改成下面三条意思：

- 用户要用某个 app，或任务需要 GUI 时，用 computer；用户点名了 app，就在那个 app 里完成，不要换成 shell、文件工具或脚本。其他工作仍然用 shell 和文件工具。
- 在 Orca 的沙箱里，shell 的 `open`、`osascript` 和其他控制 app 的命令都够不到 app；要用 `launch_app`，文件和 URL 走它的 `open`。
- 某个 app 上的动作连续失败时，停下来告诉用户哪里失败了，不要去试不相干的绕路。

**`keys` 的说明** 补一个例子：`["shift", "right"]` 是一个组合键，前面放修饰键，最后放一个主键；要按多次就多调用几次。

措辞控制在现有长度附近，净增不超过约 300 个字符。如果有测试固定了提示词或工具 schema 的长度，就在同一个 commit 里一起更新。

**解析**（`crates/orca-computer-use/src/keys.rs` 的 `parse_combo` 调用之前）：

- `keys` 里如果某一项是用 `+` 连起来、且各段都不为空的组合，比如 `"shift+right"`、`"cmd+shift+g"`，先拆成多项再解析。
- 单独一个 `"+"` 仍当作加号键。
- 解析出错时，消息附上正确写法，例如：`"right" must be a modifier before the main key; send ["shift", "right"] once per key press`。

## 6. 单轮失败预算

**放在哪里。** `ComputerUseService` 的单轮状态（`computer_use/mod.rs` 的 `start_turn`/`end_turn`，由 `turn.rs` 的 `TurnGuard` 随轮次创建和释放）。预算按 app 分开计数，一轮结束就清零。这里的"一轮"指用户发出一条消息之后的整个 agent 循环。

**按哪个 app 计数。** 优先用审批后的规范 bundle ID；参数校验阶段就失败、还没解析出 app 的调用，用小写的 `app` 参数。

**什么算失败。** 结果为 failed 的 computer 调用：驱动报错、参数校验失败、超时。

**不算失败：**

- 用户中断（Esc 或停止）；
- 审批被拒，这是用户的决定；
- 预算自己拒掉的调用；
- `list_apps`。

**清零。** 该 app 上任何一次调用成功，计数就清零。

**行为：**

| 连续失败次数 | 行为 |
| --- | --- |
| 1–2 | 照旧 |
| 3–5 | 失败结果后面追加：`{app} has failed {n} times in a row. Stop trying workarounds such as shell commands, scripts or other apps; tell the user what failed and what you tried, unless one specific fix remains.` |
| 第 6 次 | 失败结果后面追加：`Orca stopped computer use on {app} for this turn after 6 failures in a row. Tell the user what failed and what you tried.` |
| 之后 | 该 app 上的任何 computer 调用（包括 observe、launch_app）都不再送进驱动，也不弹审批，直接返回失败，内容就是第 6 次那条停用消息。其他 app 不受影响。 |

**界面。** 拒绝作为普通的工具失败显示，不另做 TUI 提示。用户发出下一条消息时开始新的一轮，预算随之恢复。

## 7. 代码结构

| 文件 | 改动 |
| --- | --- |
| `crates/orca-computer-use/src/driver/macos/apps.rs` | 可用窗口过滤（含 AX 匹配和时间预算）；`launch` 返回过滤后的窗口 |
| `crates/orca-computer-use/src/driver/macos/mod.rs` | `resolve_window` 按可用窗口选主窗口；区分 "app 没有窗口" |
| `crates/orca-computer-use/src/driver/macos/actions.rs` | 拆开 `stale()` 的三种用法；不带窗口的 `key` |
| `crates/orca-computer-use/src/driver/macos/foreground.rs` | AXRaise 回退 |
| `crates/orca-computer-use/src/protocol.rs` | `WindowInfo.minimized` |
| `crates/orca-computer-use/src/keys.rs` | `+` 组合拆分，以及带例子的错误消息 |
| `crates/orca-computer-use/src/fake.rs` | fake 驱动：辅助窗口、最小化、无窗口 app、AXRaise 失败 |
| `crates/orca-runtime/src/computer_use/execute.rs` | 已最小化标注；launch 无窗口引导；接入失败预算 |
| `crates/orca-runtime/src/computer_use/mod.rs` | 单轮的按 app 失败计数 |
| `crates/orca-tools/src/registry.rs` | computer 描述和 `keys` 说明 |
| `site/src/docs/md/{en,zh}/computer-use.mdx` | 写明失败预算、不带窗口的快捷键、可用窗口 |

## 8. 失败和边界情况

- **app 不回答 AX：** 退回到屏幕上 + 尺寸规则，`list_apps` 不会因此变慢超过 2 s。
- **窗口在 `list_apps` 之后才关闭：** observe/act 得到 §2 的 "not a window … Orca can use"，模型重新 `list_apps` 即可。
- **不带窗口的 `key` 时 app 又出现了窗口**（两次 AX 读取之间）：按"有窗口"处理，要求先 observe，不会把键发到没观察过的窗口。
- **AXRaise 回退时窗口是主窗口，但被别的窗口遮住：** 遮挡检查照常拒绝，消息不变。
- **失败预算与重试：** 驱动内部的重试（如 CannotComplete）算一次调用，结果失败才计一次。
- **预算与审批：** 预算停用后不再弹审批；已有的会话授权不受影响，下一轮照常可用。
- **已最小化的窗口：** observe/act 沿用现有行为（前台动作会先把窗口恢复出来）。列表里加标注，只是让模型知道它不在屏幕上。
- **旧会话回放：** 新消息只出现在新结果里，回放和恢复不受影响。

## 9. 测试与验收

**单元测试**（fake 驱动或纯函数，不碰真实 GUI）：

- 可用窗口过滤：离屏辅助窗口被排除；已最小化的保留并标注；小气泡被排除；AX 超时时走退回规则。
- §2 的四种错误各一例，确认 kind 和消息。
- 不带窗口的 `key`：app 无窗口时放行并恢复前台；有窗口时要求先 observe；`foreground_fallback = false` 时失败。
- AXRaise 回退的判定：主窗口/焦点窗口时放行；重试一次；两次都失败才报错；遮挡检查仍然生效。
- `keys` 解析：`"shift+right"`、`"cmd+shift+g"`、单独的 `"+"`，以及错误消息里的例子。
- 失败预算：第 3 次开始提示；第 6 次起停用；成功后清零；Esc 和审批被拒不计数；按 app 分开；轮次结束清零；被停用的调用不进驱动。
- 工具说明：新措辞存在；长度在预算内。

**真机测试**（`crates/orca-computer-use/tests/live_macos.rs`，`--ignored`，需要用户离开电脑）：

- System Settings：对已在前台的窗口做一次 click，在 AXRaise 返回 -25205 时仍然成功。
- TextEdit：启动后没有窗口，提示语正确；不带窗口发 Cmd+N 后出现可用窗口，随后 observe 成功。
- `list_apps` 不再列出离屏辅助窗口。

**验收：** 用 PR #148 的 harness 跑两次完整的 15 项，按 §0 的成功标准逐条对比基线，并写一份同格式的报告。

## 10. 已知限制和假设

- 打开面板和存储面板由系统面板服务托管，本子项目不支持直接 observe 它们，只提供 Cmd+N 和 `launch_app open` 两条出路。
- 可用窗口的 subrole 和尺寸规则是经验值。如果实测中漏掉了真实的工作窗口，再调整；调整要配上反例测试。
- 失败预算的阈值（3 和 6）来自用户的选择。之后可以视实测情况改成配置项，本次不做配置。
- 两次实测的方差很大（例如 S4 一次 2 个调用，一次 25 个），所以成功标准都按两次运行来判断，不看单次。
