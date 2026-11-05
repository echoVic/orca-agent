# Orca Harness 证据改进交付

日期：2026-11-04。范围来自 [月度采纳评估](2026-11-04-wiki-orca-recommendations.md) 前五项及 [实施计划](../superpowers/plans/2026-11-04-harness-evidence.md)。基线为 `5f1dafd959f8d2e35fc7589e45cce5e9517fa8b0`，工作树为独立的 `harness-evidence` checkout。本批未提交或推送。

## 行为变化

1. **Headless 完成与恢复证据。** `session.completed` 输出真实持久化状态 `resumable`、恢复命令或不可恢复原因，并聚合当前任务树仍未解决的 blocker。完成写入校验同一文件句柄的会话元数据与已提交消息边界，写入后同步，防止被删除、截断或替换的日志仍宣告可恢复。默认 JSONL 不保存历史；使用 `--save-history`，显式 `--no-history` 继续有效。
2. **工作区变化独立观测。** 开始和结束以内容 SHA-256 比较净变化，覆盖 shell 写入和原有 dirty 文件的再次修改；`writes_applied` 仍只表示成功文件工具回执。扫描限于 8,192 条目、32 MiB 总量、每文件 2 MiB、深度 32、每快照 250 ms，排除构建目录。归因始终 `unknown`。缺失或不完整观测不作删除、添加或无变化的推断。Apple/Linux/Android 以目录句柄及 no-follow 打开和身份校验防止祖先替换越界；其他平台返回 `safe_directory_traversal_unavailable`，不作不安全路径递归。
3. **Provider 冷却及重试预算。** 同进程内的 endpoint 路径与凭证摘要共享 429/503 冷却，解析整数和 HTTP-date `Retry-After`，冷却后的请求入场间隔为 25 ms。一次逻辑请求的 runtime、HTTP 与恢复尝试共用实际发送数预算（默认 5 次）和等待预算（默认 120 秒），并受剩余操作 deadline 约束；超时所需等待不提前发送，取消可打断等待并结算当前及此前失败尝试的已知用量，有限 deadline 溢出不会变成无限等待。凭证明文不留在冷却键或日志。
4. **评测证据及统计。** 记录实际执行参数、观察到的请求模型、fixture/verifier 摘要和独立验收结果；未知响应模型、指纹、schema 与采样值保留 `unknown`。隐藏约束首次发现要求成对且成功的读取工具输出，写入或最终答复不能充当发现证据。多约束场景提供完整及预先精简上下文，外部状态场景通过 SQLite 的独立 verifier 验证。按固定 manifest、split、suite 和实际配置统计重复运行的成功率、方差、Wilson 区间及失败尝试费用。
5. **压缩与延迟观测。** `context.compacted.payload.observation` 记录前后本地 token 估算、耗时、摘要调用和已确认用量/费用；失败、取消和未采用摘要也保留已知用量。本地前缀估算与服务端返回 cache token 分开。`model.response.completed.payload.observation` 记录逻辑 provider 耗时及首个非空 reasoning/text/tool delta 的时间与类型；eval 将两类观测写入 attempt/run/report。自动压缩的 hooks 和全部分层摘要共用一次剩余操作 deadline 与取消边界；结束后先核对取消和 wall-time 预算，再进入主模型。
6. **工具截图旁路。** 非 Flash 模型在丢弃工具图片前调用已有 vision 描述路径，以 User 角色的引用 JSON 记录图片分析、工具调用、观察序号、snapshot ID 和可用尺寸。旧 System 描述降级。观察身份包含完整调用参数、文本和图片摘要，相同观察以独立序号区分；保存记录通过距分析边界的工具距离绑定，前缀裁剪后仍能复用。只有最后一次保留观察对应的一条记录标记 latest，其余标记 historical；恢复后图片过期仍可使用保留文本。无附件且满足完整格式的 User 记录才视为内部分析，兼容原有五字段、刷新后七字段及完整旧文本格式；普通、损坏前缀或带附件的 User 继续参与查询与回退。原 assistant/tool 配对及 Flash 直接看图路径继续有效。

## 审查补充

交叉审查后的修复包括：外层 HeadlessSession 保留内层完成证据、历史追加失败保留 recording intent、同步及异步 blocker 重载与清理、同图片不同观察序号的 current/historical 判定、目录替换为 symlink 的越界回归、完成索引的强制刷新、Windows 完成时拒绝 reparse point。真实重复 Mock 评测还发现临时端口拆分统计组；现在以受控 Mock 脚本及行为配置摘要作为比较标识，保留每次实际请求地址。不同真实 provider 地址、不同 fixture 与未知标识继续分组。

## 验证

最终工作区与独立 TUI 门禁均已执行完成。验证结果如下：

- 全工作区 `cargo nextest run --workspace --all-targets --offline --locked --profile ci --retries 0`：4,799 项运行，4,798 项通过、1 项失败、106 项跳过；测试阶段 1,228.745 秒。唯一失败为 `server_mode_command_exec_uses_session_network_domain_grants` 的本地代理等待超时。本地留存日志：`<local-evidence>/orca-harness-workspace-nextest-final2.log`。相同最终代码与 PATH 随后的独立 `ci-serial --retries 0` 复现通过（10.974 秒），日志：`<local-evidence>/orca-harness-network-grant-recheck.log`；因此不能表述成一次全量零失败，保留时序稳定性缺口。`ci` 默认跳过的 `orca_tui` 与 `tui_pty_contract` 另跑串行门禁，ignored/平台测试仍不计为运行通过。
- 独立 `ci-serial --retries 0` TUI 库门禁：1,848 项通过、24 项跳过；nextest 标记 1 项 `leaky`，表示测试退出后仍有继承输出管道未及时关闭，退出码仍为 0。日志：`<local-evidence>/orca-harness-tui-lib-final.log`。PTY 契约 40 项通过、0 项失败，日志：`<local-evidence>/orca-harness-tui-pty-final.log`。这些是终端契约验证，不是原生 GUI 验证。
- 跨模块 nextest 聚焦回归 65 项通过、0 项失败，覆盖 24 项 image routing、24 项 compaction、2 项自动压缩 deadline、12 项工作区观测及 3 项网络权限契约。完整内部图片记录的识别和目录句柄安全遍历包含在这次验证中；取消重试的已知用量结算及有限 deadline 回归另有独立 GREEN 记录，并在随后全量运行中通过（`<local-evidence>/orca-retry-green.log`）。日志：`<local-evidence>/orca-harness-integration-final.log`。
- 实际 `exec_jsonl` 集成测试 38 项通过，包含保存/禁用历史、blocked 后按广告命令恢复、shell 修改原有 dirty 内容以及截断历史。完成持久化聚焦测试 20 项通过；新增日志 symlink 拒绝测试 1 项通过。
- capability Python 测试 60 项通过。刚构建的实际 Mock CLI 冒烟使用两个场景各两次：成功场景 2 次通过，故意失败场景 2 次独立 verifier 失败，记录 2 次 false completion。两个场景各聚合为一组两次，实际地址和首 delta 观测已进入报告。本地留存 run：`<local-evidence>/orca-eval-real-smoke-final3/20261104-230434-349083000/run.json`，四次首 delta 均为 reasoning。这些是 harness 验证，不能用作云模型能力得分。
- `cargo fmt --all -- --check`、`git diff --check` 通过。HTTP client、execution broker、runtime surface 的三个边界校验器及三个自身测试脚本全部通过；HTTP 校验器自身测试 78 项、execution broker 自身测试 2 项通过。

此前被中断的 `cargo test --workspace` 日志中有 3 项网络权限测试失败，其中 login shell 的 `nvm.sh` 初始化在运行 curl 前超时。最终使用包含 Node 的受控 PATH，并按仓库 CI nextest 配置及 `--retries 0` 验证；这 3 项已在上述独立聚焦运行中通过。中断日志不作为整套工作区通过的依据。 首次最终 nextest 编译另因 `No space left on device` 退出，未开始测试。清理项目 `target/debug/incremental` 可再生缓存后，关闭增量缓存，以 `-j 2` 重跑；没有修改源码或依赖锁文件来绕过错误。受控 PATH 下的 Mock 启动曾超时；使用已有 Python 3.13 显式路径后最终四次 CLI 冒烟完成，此次重跑未修改 Mock 源码。

## 测量边界

验证在原生 macOS 上完成，包括日志与目录 symlink 回归；Windows reparse point 分支未做原生运行验证，Linux/Android 分支未交叉编译或运行。TUI 库的 1 项 `leaky` 尚未定位具体测试，保留为输出管道清理问题；该标记不能证明内存泄漏。未调用付费端点、真实 GUI 或发布流程。预先精简上下文场景验证约束与最终状态，不能证明运行时自动压缩保真。摘要费用单独记录；现有 cost/pass 的累计 usage 流若没有包含摘要调用，费用分子也没有包含它，不能称作全成本。未知用量不填零，估价不是账单；内部发送未返回 usage 时，数值只覆盖已观测用量，不能证明费用完整。共享冷却是进程内控制，不能代表跨进程或账户全局配额。首 delta 包含 reasoning/tool，不能称为首个可见答案 token；数据也不能隔离 Rust overhead 或证明性能收益。

完整复制合法内部分析且无附件的人类输入仍缺少来源元数据可区分，严格格式识别不是密码学来源证明。截图描述是快照证据，执行坐标操作前仍需核对当前窗口与几何。失败或取消时本轮部分描述未持久化，再次尝试可能重复产生 sidecar 费用。非前缀裁剪、重排或旧记录缺少绑定字段时，保守地重新描述也可能增加费用。目录观察有预算及平台限制，可能漏掉净变化；文件被修改后恢复不产生净差异。持久化确认描述完成瞬间的日志，不能承诺其他进程之后不删除或修改它。


## 2026-11-05 发布审查补充

目标版本为 v0.5.17。原实现阶段的结果及平台限制保留在上文；以下发布审查证据另行记录，不把旧运行当成最终发布门禁。

发布审查修复了：完成语义记录追加失败后的内存 assistant/计划年龄漂移；在验证历史之前修剪尾记录；Windows 完成句柄允许路径被替换；已成功恢复的 continuation 前一次 blocked 记录继续阻止完成；GoalRun 失败完成缺少 controller 证据；持久化 blocker 文本未统一脱敏及父任务通知遗漏 question；目录观察工作线程超时后重复启动；截图旁路多响应费用未逐次结算与丢弃图片后遗漏保存描述；Retry-After 的旧 HTTP 日期及摘要取消后的已知用量；评测 verifier 与被测工作区共置、只读输入完整性和比较身份不一致。相应新增回归保留实际 RED 记录。

独立复核 provider 库 259 项通过，runtime 受影响回归 147 项通过，capability Python 93 项通过。目录观察用普通 cargo 并行 8 线程运行 13 项通过。HTTP/execution/runtime/Windows 边界校验器及自身测试、版本/npm 校验脚本、站点 build/SEO 共 14 个门禁通过。Python ResourceWarning 保留；Docker daemon 在本机不可用，未声称容器隔离经过原生验证。

旧发布候选 5421c5d 的 Linux 和 Windows x64 仅价格缓存日期 fixture 失败，Windows ARM64 的重复 resume 在 generation 重启后返回 RuntimeUnavailable。这些失败未作为通过证据；修复后须在新的提交上重新运行 CI。TUI pipe-cleanup 标记已定位为 workspace_status::tests::snapshot_reads_real_branch_and_detached_head，在 v0.5.16 已存在，独立运行未复现；全局 pre-commit watchdog 是线索，不能确认根因或内存泄漏。

重复 resume 已用门控 executor 稳定复现：等待真实 successor generation 启动后，重复请求返回 RuntimeUnavailable，RED 1 项失败（0.445s）。修复仅在当前代未取消、无 pending control、已启动的 RecoveryReplacement 具有对应 predecessor ack 时回放原确认；新增 current-generation interrupt 仍走当前 fence 的新提交。两个门控回归 GREEN（0.405s/0.534s），不把 ARM64 失败归为偶发或增加超时。HTTPS 降级与 JPEG SOF 长度也有独立 RED/GREEN；Windows headless fixture 使用内置 powershell.exe。最终 macOS TUI 1,848 项通过（1 项输出管道清理标记、24 项跳过），PTY 40 项通过；全工作区和新的 Linux/Windows CI 仍在运行。

公开报告中的 `<local-evidence>` 是本地留存标记，文件未上传到仓库，不能作为读者可访问的日志链接。发布 CI 证据以 PR160 和对应 workflow run 为准。绝对路径 verifier 命令兼容性另有回归：修复前 `/usr/bin/diff` 和 `/bin/sh` 被误绑定到快照，3 个新增用例记录 1 项失败、2 项错误，修复后完整 Python 3.13 套件 93 项通过（20.791s）；现在保留绝对系统命令，继续快照相对脚本及声明只读输入。


最终 verifier 审查补充：`env python3 verify.py` 与指向工作区的绝对脚本路径曾遗漏快照及完整性保护，实际替换脚本可产生 false pass。env 组 RED 3 项运行、4 个断言失败（含 2 个不支持参数子用例）；绝对路径组 RED 5 项运行、2 项失败。现在保留绝对系统可执行文件，冻结相对及绝对解释器脚本，解析 env 的 ignore/unset/赋值/`--`，无法支持的 relocation/split 等选项提前失败。完整 Python 3.13 套件 97 项通过（25.389s）。实际构建 CLI 的 Mock 场景各重复两次：成功组 2 次独立验收通过，故意失败组 2 次独立验收失败；两个统计组各两次，实际端点及 reasoning 首 delta（12/9/8/9 ms）保留。这不是付费模型能力测量。


macOS 发布全量第一次运行 4,844 项：4,842 项通过、2 项失败、106 项跳过（957.074s）。`command/exec/read` 测试漏发后续读取请求，stdout/stderr 错开时被动等待超时；实际协议诊断证明第二次 read 可取回完整 stderr。`drop_joins_supervisor_and_kills_process_tree` 原 fixture 在固定 500 ms 后写 marker，不能区分繁忙机器在 Drop 前已写与 Drop 后存活；相同二进制独立复查 1 项通过（1.055s）。修正测试为 stdout/stderr 两阶段门控及关闭后才释放后台子进程，不提高超时、不修改生产终止逻辑；最终运行结果以对应 PR/workflow 为准。

2026-11-05 最终门控修正：macOS 全工作区曾记录 4,842 通过、2 失败（无框架重试）；这两项没有记作成功。流式 command/exec 测试的被动等待不符合后续分块需要再次 read 的协议；stdout/stderr 分别门控后，旧 helper 稳定 RED，恢复主动 read 后相关 41 项通过。进程关闭测试原来的固定子进程延迟可能在 Drop 前到期；现在仅在 Drop 返回后放行写入。临时保留服务而不关闭时回归稳定 RED，正常关闭 GREEN。生产协议和进程终止逻辑未改动，最终全工作区复跑另记。

独立审查补充：同名 fixture 原件与 prepare 后工作区脚本发生快照覆盖，可把 `[7, 0]` 误变成 `[0, 0]`；隐藏 verifier 后的绝对输出路径也可能误当脚本冻结。两项新增回归均先实际 RED，修复 source 身份命名及消费脚本后的解析状态后，完整 Python 套件 99 项通过（56.461s）。此前 97 项记录保留为历史证据。

最终 shell 审查补充：`bash -lc/-ec` 内联绝对命令被误绑定为脚本，`bash -e verify.sh` 则遗漏快照保护并产生实际 false pass。修复前便携回归组 3 项运行、1 项失败及 2 项错误；区分 shell 的 `-c` 与 errexit 后完整 Python 3.13 套件 101 项通过（26.041s）。首版测试硬编码 `/usr/bin/test` 在 macOS 不存在，修正为按实际 executable 查找，未修改生产逻辑绕过测试。关闭进程树 fixture 按 ShellResolver 实际 Posix/GitBash/PowerShell/Cmd 方言选择门控脚本，生产关闭逻辑保持原样，修正后的聚焦关闭回归通过（1.136s）。

最终 macOS 全量运行 4,844 项全部通过、106 项跳过（983.609s），Clippy 及格式检查通过，已有 Clippy warnings 保留。该全量运行发生在最后一次仅测试的 shell 方言选择修正之前；最终平台证据必须来自新的 PR SHA 和合并后的 exact-main CI。


最终 Linux PR 门禁 41974056642 在后台 provider 停止用例失败（1,847 通过、1 失败、22 跳过），保留原始失败。该用例在 v0.5.16 已使用 1,000ms 自然完成窗口；在收到后台 Running 事件后延迟停止动作 1,200ms，稳定复现同一个 `stopped task update: Timeout`（RED 12.288s）。改用已有 release-marker Mock 流，让请求在取消前持续等待，保留延迟停止和重复停止断言，GREEN 1 项通过（2.624s）。此证据确认固定窗口不能保证动作执行时任务仍在运行，不能证明失败 CI 的具体调度原因；生产取消路径未修改。

最后的 Windows 测试审查修正关闭 fixture 的 Cmd 嵌套引号及对 Windows PowerShell 的隐含依赖：PowerShell 子进程使用当前实际 executable，Cmd 用原生 `.cmd` 门控子脚本，不经过 PowerShell。新增原生 Windows 子测试在各可用 pwsh.exe/powershell.exe/cmd.exe 下启动独立测试进程验证关闭，不修改进程环境或生产参数路径。macOS 仅验证 POSIX 分支；其他分支必须由最终 SHA 的 Windows 门禁确认。


本地收尾编译期间可用磁盘空间降到不足 1GiB；仅删除 2026-11-04 中断编译遗留的五组 runtime `.rcgu.o` 可再生对象文件（清单留存在本地），保留当前完成产物、源码和测试日志，继续最终验证。


修正后的完整源码经最后一次 POSIX 关闭回归编译运行通过（`<local-evidence>/orca-v0.5.17-native-shutdown-final.log`）；TUI delayed-stop 门控回归、runtime/Windows 边界校验及版本一致性通过。新增 Windows shell 子进程矩阵尚待最终 PR CI 原生验证，不以 macOS 通过替代。
