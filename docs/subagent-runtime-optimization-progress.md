# Orca 子代理运行时优化实施进度

更新：2026-09-14。对照 [优化方案](subagent-runtime-optimization-plan.md)。本文件区分代码已经接通、测试已经验证和真实模型效果边界；不能作为发布成功的证明。

## 本轮修复

### 1. 提交、排队与统一容量

- 默认 `delegation = "adaptive"`，`max_running = 32`、`max_queued = 256`、`max_live_tasks = 512`、`max_depth = 2`。达到执行上限正常接受并排队；队列和存活任务数量有独立边界。
- 模型可见的 `subagent` 不再选择 sync/async；默认快速返回已接受的 `task_id`。提示词、schema、解析默认值保持一致。内部同步执行仍用于已准入 worker 和测试。
- 普通后台提交、内部批次、Workflow 子调用、托管子线程和同进程 UI continue 使用发起方的同一个 TaskRegistry / ExecutionScope。托管子线程不再创建重复的 MainSession 任务记录。
- 排队闭包由每个 scope 的单个 dispatcher 管理，获得 lease 后才创建执行 worker。新任务队列不为每个条目预先创建线程或进程。
- accepted 回执前先持久化不含凭据的启动意图、工作目录、角色、深度、批次身份和冻结委派策略。重启后，尚未开始执行的意图按原 task id 重新入队；已经写入 execution-start 但没有终态的任务标记 `continuation_indeterminate`，不会自动重放可能产生过外部副作用的执行。
- 准入使用进程内仲裁锁和持久 scope 文件锁；提交 sequence 是持久单调序号。分支轮转游标持久化，使用分支身份，独立打开同一登记表也能延续公平顺序。
- 读取持久状态失败时拒绝新准入，容量视图标记 `available: false`；不把读取失败解释为空闲。仲裁回调 panic 后恢复线程局部锁状态。

### 2. 安全等待、取消与期限

- 有活跃后代不等于父代理已经让出 lease。只有工具调用全部结算、写入安全检查点、登记 Dependency 等待后，父代理才能释放容量；唤醒后必须重新排队并取得 lease。
- `task_wait` 统一观察命令和子代理，支持 any/all、终态/状态变化、游标；等待超时不取消任务。未知任务返回错误。
- 等待图拒绝自身、祖先以及兄弟间循环；登记等待与检查环在同一个 scope 决策锁内完成，覆盖并发登记。
- 直接 `request_pause` / `request_resume` 拒绝 Subagent，不能通过改状态释放或绕过 lease；Workflow 主任务原有暂停接口保留。
- 新任务 deadline 包含排队时间，继承祖先最早期限，不因同一任务恢复重置。排队到期不启动；正在执行的任务先请求停止，资源静默后才释放容量。
- 执行 worker panic 后标记 indeterminate，保留资源占用；父任务不会把无法结算的子任务误报为成功。普通失败、取消和预算停止先停止并等待已启动子任务结算。
- 托管子任务直接使用 canonical task 的 cancel token；`task_stop` 无需轮询桥接即可中断真实 child，父取消只单向传播给子。已 stop 的准入记录不会被 AgentTurnAdmission 当作新 continuation 复活。
- 命令统一由 `bash` 启动，后续使用任务工具；`yield_time_ms` 只让出调用，`timeout_ms` 才是执行期限，不再有通用的 120 秒执行硬切断。

### 3. 子任务预算与终态账单

- 普通有限预算后台委派需要父操作签发的持久 reservation。先绑定已接受任务，再启动 worker；启动前放弃可归还零消耗，已开始但缺失账单时保留预留，不能按零消耗退款。
- 跨进程文件账本串行校验“已消耗 + 未结算预留 + 新预留”。同一工具调用重放绑定同一任务；后续模型轮次复用相同 provider tool id 不会误用旧 reservation。
- 子任务实际账单先结算，再发布任务终态。父 journal 持久记入消费后才确认账单，恢复时不重复累计。迟到费用即使在 BudgetStop 之后也必须计入。
- 分配时保留父任务收尾预算。Workflow 启动也必须取得持久父 reservation；每个实际 agent 调用按剩余额度和剩余 agent 槽位动态取得 turn、tool-call 和费用上界。临时预留不足时等待结算，确定耗尽才失败；启动前失败结算为零，真实执行汇总精确账单，panic 或缺失账单保留预留。

### 4. 结果和指导交付

- 子结果 outbox 只领取 Pending / 可重试 Claimed，按直接父任务过滤；InBand 结果不二次注入。
- 父对话持久写入结果并完成安全检查点后才 ack。写失败释放 claim，独立 writer / 重启重放按持久通知身份去重。
- 结果版本随结果事实变化；旧 claim / revision 的 ack 不会确认新结果。
- `subagent_message` 在每个安全模型边界读取、写入会话并保存检查点，再确认已消费；不再只在用户新轮次开始时处理。
- 父任务自然结束前等待已接受的子任务并摄入结果；收到新结果后给模型一次整合机会，避免父 owner 提前关闭。
- `task_list` / `task_wait` / `task_read_output` / `task_send_input` / `task_stop` 是统一任务控制面，`subagent_status` 已移除。

### 5. 恢复、角色与工具权限

- continuation 在入队前只读校验来源、父任务所有权、检查点与配置；实际启动时再次校验并 CAS 提交。伪造任务绑定不会先生成一个接受成功的后台任务。
- 托管子线程使用确切的父 TaskRegistry，UI continue 新建受同一 scope 管理的执行 attempt，不能开私有容量池。
- General 可以在允许深度内委派；角色工具目录与 runtime 硬门禁一致。可执行命令的角色具备观察、输入和停止自己命令所需的任务工具。
- Workflow IPC 工具只在 Workflow 上下文中加入，再与团队授权工具交集；普通 General 不会因此获得 Workflow 控制能力。
- 线程元数据持久保存 root thread、深度、角色、task id 和 TaskRegistry session。冷启动 resume 会验证 task 与 child thread 的确切绑定，再恢复原父 scope 和角色门禁。
- 排队启动时重新读取当前父权限，并要求当前权限快照与提交时一致。权限撤销和权限扩大都不会让旧任务悄悄以不同能力启动；调用方必须按当前配置重新提交。

### 6. 故障边界和资源驻留

- 入队、执行开始、panic、预算回执、结果领取和父会话摄入都使用持久身份收敛。下面的矩阵给出每个边界的恢复规则和直接验证，不用一个笼统的“可恢复”描述覆盖不同风险。
- 256 个排队任务只保存意图，不创建 worker。安全等待会释放 execution lease，但当前 RuntimeThread 的调用栈仍驻留；现有 UI continue 依赖这个活动 handle。没有透明的 actor 重载协议前不主动关闭它。冷启动身份恢复已经完成，为将来安全淘汰提供前提，但“关闭后由 UI 自动重开”不能在尚未接通时冒充完成。

| 故障边界 | 恢复规则 | 直接验证 |
|---|---|---|
| 持久准入 / 多 owner 竞争 | 文件锁内提交唯一 sequence 和容量事实 | `persistent_admission_serializes_independent_registry_instances` |
| accepted 后、execution-start 前 owner 丢失 | 原 task id 重建队列并执行一次 | `restart_requeues_an_admitted_launch_that_never_started` |
| execution-start 后 owner 丢失或 worker panic | 标记 indeterminate，保留资源/预算，不自动重放 | `restart_never_replays_a_launch_after_execution_started`、`panicked_worker_retains_indeterminate_reservation` |
| Workflow worker 启动前失败 | 父 reservation 结算零消耗 | `workflow_failure_before_worker_start_refunds_parent_budget` |
| Workflow 子执行完成或缺失回执 | 精确汇总实际账单；缺失账单不退款 | `workflow_child_execution_settles_its_receipt_into_the_parent_ledger`、`workflow_parent_budget_settles_exact_aggregate_and_keeps_missing_receipts` |
| 通知领取后 owner 丢失 | claim 过期重领；旧 ack 不能吞掉新 claim | `result_outbox_survives_owner_loss_between_claim_and_ack` |
| 父会话已经写入、尚未 ack | 持久 delivery id 去重，重开 writer 不重复摄入 | `system_delivery_is_deduplicated_across_independently_opened_writers` |

### 7. 真实模型测试驱动的修复

- Explorer 的实际工具集不包含 `bash`，系统提示词现在按生效工具集生成，只展示 `read_file`、`glob`、`grep` 等确实可用的工具；无 shell 的角色也不再收到“所有命令都使用 bash”的指导。
- 普通并行调查明确走直接 `subagent`，只有用户明确要求 Workflow/ultracode/可复用编排或存在阶段依赖时才先生成 WorkflowDraft，避免广域审查被错误路由。
- 委派指导要求每个孩子承担一个最小可用证据范围，把 32 视为容量上限而不是派单目标；成功孩子的精确证据不再由主代理重复读取。面向人的普通证据报告不附输出 schema，减少无收益的格式重试。
- provider 偶尔会把结构化对象多编码成一层 JSON 字符串。runtime 只在外层值不满足 schema、内层 JSON 可以无损解析且通过同一 schema 时解开这一层；合法字符串结果保持不变，普通 prose 仍不会绕过 schema。
- 单个并行子代理的参数或普通执行失败现在作为该工具结果返回模型，不再终止整个 turn 或取消尚未结算的兄弟调用；审批、取消、不确定终态以及 continuation 所有权/fence/lease 身份冲突仍保持终止语义。
- 内置 Explorer / CodeReviewer 达到 6 个模型回合或 8 次已开始工具调用后，runtime 关闭未开始的同批调用、保存安全检查点，并执行一次无工具、最多 1024 token 的最终报告。最终报告使用 90 秒总截止时间，不会因 SSE 持续输出思考片段而绕过超时；长度截断或截止时间到达会成为可交付的 `partial` 结果。
- 明确同时要求“只读”和“禁止修改”的父任务，在直属子代理全部终结并交付后进入同一受限汇总阶段。实现任务不触发该规则，因此仍可先委派调查再修改代码。委派指导同时约束一个命名分支只提交一次，不把普通失败或部分结果解释成自动重派许可。
- 真实基线在父进程超时/失败后留下过孤立 worker。基准脚本为每个 case 建立独立进程组，超时时递归终止后代，并支持按 variant/case 单独复跑；本轮最终检查没有基准 worker 残留。

## 验证记录

测试使用隔离 `ORCA_HOME`，不写用户实际任务历史。以下为本轮最终通过的回归；表中定向复测与大套件有重叠，不应直接相加。

| 验证 | 结果 |
|---|---|
| `cargo check --workspace --all-targets` | 通过；测试辅助代码有一处既存 `ProcessJob::spawn` deprecated 警告 |
| `cargo fmt --all -- --check` / `git diff --check` | 通过 |
| `orca-runtime --lib` 筛选大套件 | 1,348 通过；另有 160 项宿主/网络/已知时序模块按名称排除；本轮发现并修复托管 child stop 的准入/执行竞态，`subagent_execution::tests` 37 项并行运行全部通过 |
| 最新 scope 压力与 panic 回归 | 26 通过，包含后补的两个 panic 测试 |
| 最新系统提示词回归 | 11 通过，明确只展示实际可用工具、不再传 mode、不重复提交、等待不取消执行 |
| 队列恢复、冷启动身份、权限复核、outbox owner-loss | 精确复跑全部通过；`restart_` 过滤共 20 通过 |
| Workflow runner 预算与恢复 | 26 通过；包含临时预留等待、启动前零结算、实际子回执聚合与缺失回执保留 |
| `orca-core --lib` / `orca-provider --lib` | 287 / 192 通过；provider 包含最终报告截止时间、截断 partial 与上下文摘要截断回退的分离契约 |
| `orca-tools --lib`（排除 Seatbelt 模块） | 196 通过，45 项 Seatbelt 模块测试未纳入此轮重跑 |
| `orca-tui --lib` | 沙箱全量 1,339 通过、3 条本地 loopback 用例被宿主拒绝、24 ignored；ACP 18 项在允许 loopback 后全部通过，覆盖这 3 条 |
| `runtime_lifecycle_contract` / `subagent_contract` / `subagent_recovery_contract` | 53 / 13 / 2 通过 |
| `tool_contract` / `workflow_types_contract` / `workflow_runtime_contract` | 17 / 5 / 45 通过 |
| `budget_lease_contract` / `budget_resume_contract` | 6 / 6 通过 |
| `subagent_delegation_contract` / `subagent_observability_contract` / `task_output_store` | 26 / 18 / 15 通过 |
| `scripts/subagent-runtime-real-model-benchmark.sh` | 真实 DeepSeek A/B 已运行；支持 `BENCH_VARIANTS`、`BENCH_CASES`、逐 case 180/600 秒护栏、断点跳过和进程树清理 |

环境边界：另行运行过包含 Seatbelt 的工具包测试，其中 232 通过、9 失败；失败集中在宿主拒绝 `/usr/bin/sandbox-exec`、signal 6。未绕过生产安全策略。runtime 大套件中的 20ms host 探针、Workflow 子进程 EOF/回收和后台审批重试会在高并发下偶发失败，相关用例精确复跑通过；因此记录为“大套件有时序抖动”，不宣称无条件全 workspace 通过。`suggest` 的 CLI 契约分别验证正常审批拒绝和沙箱不可用时的执行前拒绝；本机实际覆盖的是后者。

- 实际 dispatcher：100 个阻塞任务，32 个执行、68 个排队，释放一个只补一个，全部完成。
- 实际模型循环的等待 helper：32 个父代理释放 lease、后代运行、父代理重新准入并完成，不超过 32。
- UI continue：与已有子任务共享容量，超限排队，完成后下一次 continue 使用新 attempt。
- 重启：未开始的 accepted 启动意图按原身份重排；execution-start 后丢失 owner 不重放；冷启动继续恢复原角色、深度和父 scope；冻结权限变化拒绝启动。
- 预算：并发预留、Workflow 动态份额与等待、启动前零结算、缺失账单保留、父 journal 重启去重、工具 id 重放/复用、预算停止后迟到费用入账、子任务先结账后发布终态。
- 交付：claim 后 owner 丢失可重领，旧 ack 不吞新 claim；父 transcript 用持久 delivery id 跨 writer 去重。
- 两条 CLI 恢复集成测试通过，覆盖同步/后台切换、自定义角色恢复、跨会话拒绝、伪造来源和任务绑定拒绝。

## 真实 DeepSeek A/B

测试固定源码为 `/private/tmp/orca-baseline`、模型为 `deepseek-flash`、审批为 `full-auto`；baseline 使用旧二进制，candidate 使用本工作区构建。指标来自 JSONL 最终 usage 和实际工具事件。前三项的 baseline 使用 600 秒护栏，candidate 最终复跑使用 180 秒护栏；第四项是单文件控制，第五项是修订后明确不使用 schema 的四路受控并发。超时记为 124，不能把未完成输出当成质量通过。

| 场景 | baseline | candidate | 结论 |
|---|---:|---:|---|
| 1. 八区域架构审查 | 600s 超时；769,766 in / 10,217 out；52 tools；5 subagents | 149s 完成；877,173 in / 26,005 out；46 tools；8 subagents | 八个分支各启动一次并全部完成；父汇总正常退出 |
| 2. 四测试域缺口调查 | 70s 失败；546,457 in / 11,823 out；32 tools；7 subagents | 70s 完成；454,065 in / 10,692 out；32 tools；4 subagents | 四个测试域各启动一次；相对 baseline 输入 -16.9%、输出 -9.6%，且从失败变为完成 |
| 3. 五崩溃窗口追踪 | 600s 超时；10,394,037 in / 65,124 out；134 tools；6 subagents | 105s 完成；405,274 in / 19,800 out；32 tools；4 subagents | 模型将五个窗口合并为四个独立分支；输入 -96.1%、输出 -69.6%、工具 -76.1%，并在期限内完成 |
| 4. 单文件控制 | 4s；19,496 in / 299 out；1 tool | 2s；22,080 in / 229 out；1 tool | 均完成，无需委派；candidate 更快但输入略增 |
| 5. 四文件、四 Explorer | 46s；175,524 in / 6,278 out；29 tools；4 subagents；1 tool error | 28s；32,575 in / 1,829 out；5 tools；4 subagents；0 errors | 均完成；candidate 耗时 -39.1%、输入 -81.4%、输出 -70.9%、工具 -82.8% |

场景 5 证明新的提交、并发运行、`task_wait` 和结果汇总可以在真实模型下端到端工作，并且恰好启动四个子代理。最终源码复跑只有四次 `subagent` 和一次 `task_wait`，父代理没有重复读取孩子已经覆盖的文件。baseline 则使用 14 次 `subagent_status`、一次 `sleep` 命令和额外搜索，且出现一次无效 glob 参数。

三类广域压力场景最终都在 180 秒内完成。真实测试先暴露两段独立长尾：只读孩子达到工具上限后，最终报告仍可被持续 SSE 思考流拖住；孩子全部完成后，父代理又会重复读取已经委派的范围。前者由 6 回合/8 工具证据预算、1024-token 报告和 90 秒总截止时间闭合，后者由显式只读父任务的终态汇总阶段闭合。中间复测确实出现过 case 1 的 300 秒超时以及 case 2/3 的 240 秒超时，表中只列最终代码对应的干净复跑，不把这些失败隐藏成通过。

这些结果证明 P1 的只读广域调查收敛已经接通，但不是所有自然语言任务的通用质量保证。当前停止条件只覆盖 runtime 可确定的内置只读孩子，以及用户同时明确“只读”和“禁止修改”的父任务；实现任务、通用 General 长推理和 provider 级全局输出策略仍保持原有语义。后续评测应继续记录每个孩子的首 token、有效证据时间、完整/partial 比例和父级重复读取率，而不是再提高 32 的容量上限。

活动等待上下文的卸载仍是 P2 前置架构工作：先实现 RuntimeThread/actor 的透明重载和 UI handle 重绑定，再启用淘汰。当前实现保留活动 handle，避免破坏 continue；终态历史不占 execution lease。更深嵌套、丰富历史继承和带来源的只读调查产物复用仍按方案 P2 评测后决定；不实现按相同 prompt 命中的最终结果缓存。
