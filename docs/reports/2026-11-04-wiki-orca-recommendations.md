# qingyun-wiki 月度建议对 Orca 的采纳评估

日期：2026-11-04。调研范围：2026-10-04 至 2026-11-04，含首尾，共 32 篇 Twitter 日报、32 篇 AI 工程调研。近期论文条目包含在 AI 工程调研中，未找到另一个覆盖此窗口的独立论文调研目录。

来源固定为 private 仓库 `echoVic/qingyun-wiki` 的 commit `4a4e9c66d8459359632115d6744acdf5ccfc3beb`（2026-11-04T03:03:18Z）。64 篇正文已下载并逐篇校验 Git blob SHA-1，全部匹配。Twitter 32 篇完成全文阅读，工程调研 32 篇按论文与 Orca 相关章节阅读，GPU/训练等无关章节未全文精读。本文所有日报链接均指向该 commit，不随 master 移动。

Orca 核验基线为本地提交 `5f1dafd959f8d2e35fc7589e45cce5e9517fa8b0`，另有品牌修改留在工作区。本文记录采纳前的研究判断。前五项随后按用户批准实施于独立工作树，实施与验证结果见 [交付报告](2026-11-04-harness-evidence-delivery.md)。本批没有 commit、push 或发布。

## 结论与顺序

可采纳，但应围绕现有实现补缺口。建议按以下顺序投入；P0/P1/P2 是本文建议的实施顺序，不是已创建的 issue。

| 顺序 | 建议 | 当前状态与最小范围 |
| --- | --- | --- |
| P0 | headless 完成与恢复契约 | 已有 writes_applied、blocked、resume；补 shell 副作用观测、明确是否可恢复、终态聚合未解决子任务 blocker |
| P1 | provider 共享冷却与统一重试预算 | 已有 Retry-After 整数秒与 jitter；补 HTTP-date、同配额域的共享冷却、内外层总尝试预算 |
| P1 | 扩展已有能力评测的证据 | 记录真实模型配置，加入隐藏约束发现路径、压缩保真、外部状态验证和重复运行统计 |
| P1 | 延迟与压缩经济性观测 | 已有成本和缓存指标；补首 token 延迟、摘要费用、前缀损失估算及任务成功率对照 |
| P1（多模型 GUI） | 工具截图的视觉旁路 | Flash 可直接看工具截图；非 Flash 路径目前丢弃图片，只描述用户附件 |
| P2 | 结构证据检索 | 已有 rg、路径搜索、记忆 BM25；先增强文档标题/模块/符号与来源片段，验证后再引 parser |
| P2 | 工具成本与记忆质量反馈 | 先统计 schema 开销、重复失败、事实过期/矛盾；工具裁剪和新压缩规则独立评测 |
| P2 | skill 来源与变更审计 | 已有工作区信任和路径约束；补内容摘要/版本变更，可选规则扫描作为提示 |

## 1. 优先补 headless 的完成与恢复契约

来源：[11/02 Twitter，三条 Orca 建议](https://github.com/echoVic/qingyun-wiki/blob/4a4e9c66d8459359632115d6744acdf5ccfc3beb/All%20in%20AI/%E8%B0%83%E7%A0%94%E6%97%A5%E6%8A%A5/AI%20Twitter%20%E7%83%AD%E7%82%B9%E6%95%B4%E7%90%86/2026-11-02.md#L193)。

当前已经有 `session.completed.writes_applied`，聚合本任务树成功的 edit/write_file 回执。它证明工具写过文件，既不覆盖 shell 写入，也不证明任务正确。[契约说明](/Users/bytedance/Documents/GitHub/blade-deepseek/docs/harness-contract.md:198)、[回执聚合](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-runtime/src/controller.rs:952)。这条不能再表述成“首次增加写入证据”。

三个可采纳的增量：

1. **shell 副作用观测。** 增加独立的工作区变化证据，保持 writes_applied 的原语义。使用任务涉及路径的开始/结束基线、内容摘要或显式 shell 写入回执；Git status/diff stat 只能作为发现变化的信号，同一文件两次状态和统计相同仍可能内容不同。共享 cwd 中的其他任务也可能修改文件，所以工作区变化不能直接归因给本任务，更不能直接当成功证明。非 Git 目录应限定观察范围，避免每轮全盘哈希。验收至少覆盖：shell 改文件、只读命令、原本已有 dirty 文件再次修改、并行外部修改、修改后恢复。
2. **JSONL 的可恢复状态。** 当前 [exec 配置](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-runtime/src/command/exec.rs:91) 在 JSONL 未传 --save-history 时关闭历史；[终态](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-core/src/event_schema.rs:1311) 只有保存历史时能附可用 session_id。本次 CLI 修复已经支持 resume --json，但默认 JSONL 的恢复缺口仍在。第一步明确输出 resumable 与原因，给有历史的 blocked 输出结构化恢复信息。若要默认保存 blocker checkpoint，需保证它足以恢复工具调用和当前预算/权限状态，并明确保留策略；不能只生成一个不能恢复的 id。显式 --no-history 应继续被尊重。
3. **聚合仍未解决的子任务 blocker。** 已有 [task.status.updated](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-core/src/event_schema.rs:1183)，不必为了同一个状态新造 task.blocked 事件。缺口在 session.completed：调用方目前需要重建任务树或解析子任务错误文本。可增加 task_id、parent_task_id、kind、summary、question、resolution 等结构化结果，聚合与本次任务相关且未解决的 blocker。允许父任务完成独立工作；是否使父任务失败，应由任务契约决定，不能将任何 blocked child 永久算成失败。

验收闭环：JSONL blocked → 获得真实可用恢复信息 → 回答问题 → resume → 获得新终态；有历史和显式无历史分别测试。父任务已解决或替代子任务时，不留下过时 blocker。

## 2. 共享限流与统一重试预算

来源：[10/21 工程调研 O1](https://github.com/echoVic/qingyun-wiki/blob/4a4e9c66d8459359632115d6744acdf5ccfc3beb/All%20in%20AI/%E8%B0%83%E7%A0%94%E6%97%A5%E6%8A%A5/AI%20%E5%B7%A5%E7%A8%8B%E8%B0%83%E7%A0%94/2026-10-21.md#L96)。

原文“退避不读 Retry-After”对当前代码不完整：HTTP 的阻塞与流式路径都已尊重整数秒，[实现](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-provider/src/http_client.rs:65)；[解析器](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-provider/src/http_client.rs:230) 尚不支持 HTTP-date。HTTP 层有最多 3 次 retry，runtime [重试策略](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-runtime/src/provider_retry.rs:6) 另有最多 5 次尝试。尚未看到所有子 Agent、摘要和其他 provider 请求共享配额域的冷却/请求门控。

最小方案：按 endpoint 与账号配额身份划分共享请求域，model 可再细分但不能假定不同模型的账号额度独立。从 429/503 提取冷却截止时间，让所有相关请求遵守；为一次逻辑 provider 操作贯穿内外层记录总尝试数与总等待预算。账号标识不记录明文 key。补 HTTP-date、取消与 deadline 的处理；服务端要求的等待超过本次 deadline 时，应停止/返回状态，不要提前重试。

验收：并行子任务遇到 429 后没有同步重试风暴；总尝试数有明确上限；日期和整数 Retry-After 均生效；用户取消能打断等待。现有 workflow 并发数、只读工具 semaphore 不能替代 provider 配额控制。

## 3. 扩展现有 eval，让优化有证据

来源：[10/29 检索路径建议](https://github.com/echoVic/qingyun-wiki/blob/4a4e9c66d8459359632115d6744acdf5ccfc3beb/All%20in%20AI/%E8%B0%83%E7%A0%94%E6%97%A5%E6%8A%A5/AI%20%E5%B7%A5%E7%A8%8B%E8%B0%83%E7%A0%94/2026-10-29.md#L54)、[10/21 可复现评测](https://github.com/echoVic/qingyun-wiki/blob/4a4e9c66d8459359632115d6744acdf5ccfc3beb/All%20in%20AI/%E8%B0%83%E7%A0%94%E6%97%A5%E6%8A%A5/AI%20%E5%B7%A5%E7%A8%8B%E8%B0%83%E7%A0%94/2026-10-21.md#L100)、[10/26 分数/成本/缓存三元组](https://github.com/echoVic/qingyun-wiki/blob/4a4e9c66d8459359632115d6744acdf5ccfc3beb/All%20in%20AI/%E8%B0%83%E7%A0%94%E6%97%A5%E6%8A%A5/AI%20Twitter%20%E7%83%AD%E7%82%B9%E6%95%B4%E7%90%86/2026-10-26.md#L236)、[10/31 资源轴](https://github.com/echoVic/qingyun-wiki/blob/4a4e9c66d8459359632115d6744acdf5ccfc3beb/All%20in%20AI/%E8%B0%83%E7%A0%94%E6%97%A5%E6%8A%A5/AI%20Twitter%20%E7%83%AD%E7%82%B9%E6%95%B4%E7%90%86/2026-10-31.md#L153)。

Orca 已有 capability fixtures、独立 verifier、保护文件检查与 false_completion 分类，[完成判定](/Users/bytedance/Documents/GitHub/blade-deepseek/scripts/eval/capability/lib/backend.py:366)。长上下文 C 轴和真实仓库 B-L4 也存在。应补测量维度，避免另造一套评测框架。

当前 [run protocol](/Users/bytedance/Documents/GitHub/blade-deepseek/scripts/eval/capability/run.py:46) 记录 source commit、binary hash 与 host，但模型标签写死为 deepseek-flash (effort max)，不足以反映真实请求配置。优先改为从实际执行配置记录 model、reasoning effort、endpoint 身份、requested_model 与 response_model、provider/engine fingerprint（服务端提供时）、采样参数、预算、工具 schema 与 fixture/verifier 版本；策略调优任务与验收留出任务分开，失败尝试费用也计入每个成功任务成本；未知字段明确 unknown，不猜测模型 artifact。

增加三类场景：

- 真实仓库中隐藏文档约束的首次发现路径、到达证据所需工具次数与 token，同时验证最终行为符合约束。
- 压缩前后保留路径、数值、禁止事项、未解决问题与正反证据的多约束任务，比较正确率与每个成功任务成本。
- 文件/GUI/外部应用中的独立终态验证，让“模型说完成”与“状态实际达成”分开统计。

重复运行应报告成功率与方差/置信区间，固定 seed 只能在支持的端点上控制一部分变量。不要采用日报建议的“云模型同 prompt 两次输出完全相同”CI 门槛。资源轴可以先离线测启动到 ready、空闲 RSS、二进制体积；首 token 延迟另记为 provider/端到端延迟，避免把模型快慢归因给 Rust。

## 4. 先观测压缩经济性与 endpoint 延迟，再改变策略

来源：[10/23 PrefixStabilityCost 与能力探测](https://github.com/echoVic/qingyun-wiki/blob/4a4e9c66d8459359632115d6744acdf5ccfc3beb/All%20in%20AI/%E8%B0%83%E7%A0%94%E6%97%A5%E6%8A%A5/AI%20%E5%B7%A5%E7%A8%8B%E8%B0%83%E7%A0%94/2026-10-23.md#L59)、[11/03 EndpointPool 与成本压缩](https://github.com/echoVic/qingyun-wiki/blob/4a4e9c66d8459359632115d6744acdf5ccfc3beb/All%20in%20AI/%E8%B0%83%E7%A0%94%E6%97%A5%E6%8A%A5/AI%20%E5%B7%A5%E7%A8%8B%E8%B0%83%E7%A0%94/2026-11-03.md#L51)。

已有 [cache-aware micro compaction](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-provider/src/context.rs:832)、[PrefixReuseMetrics](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-provider/src/prompt_cache.rs:33)、[成本累计](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-runtime/src/cost.rs:22) 与动态价表。已有 context.compaction.started/context.compacted 的 reason、strategy 和消息计数，[载荷](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-core/src/event_schema.rs:343)。10/16 所说“压缩没有事件”已经过时。当前 [触发策略](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-runtime/src/compaction.rs:303) 仍以输入预算压力为依据。

值得补：本次压缩前后 prompt token 测量、摘要请求费用、未改变前缀的估算下限、之后真实 cache-hit token、首 token 延迟、整次任务耗时与费用。prefix reuse 是本地估算，不等于服务端 KV 命中。流式 chunk 间隔也不等于每个 token 的真实生成间隔，指标应标明口径。

第一阶段将这些结果接入 eval，比较“保留/微压缩/深压缩”对每个成功任务成本与成功率的影响。只有数据支持时才将经济性用于 soft-limit 策略；hard budget、回复预留和 overflow 恢复必须始终优先。日报中“上下文长就保留、高缓存命中就压缩”的简化方向并不普适：压缩可能破坏最有价值的缓存前缀。

能力探测适合显式联网、带请求/费用预算的 opt-in 命令。当前 [doctor](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-runtime/src/diagnostics.rs:1) 的契约是离线只读，默认行为应保持。先测 tool-call/reasoning/SSE/已知上下文设置和缓存；不要为了探测最大上下文无限发大请求。EndpointPool、cache 粘性路由与 MTP 接受率观测暂缓，等多 endpoint 的真实需求及服务端字段出现再做。

## 5. 多模型 GUI：补工具截图视觉旁路

来源：[10/31 工程调研，截图账本与视觉旁路](https://github.com/echoVic/qingyun-wiki/blob/4a4e9c66d8459359632115d6744acdf5ccfc3beb/All%20in%20AI/%E8%B0%83%E7%A0%94%E6%97%A5%E6%8A%A5/AI%20%E5%B7%A5%E7%A8%8B%E8%B0%83%E7%A0%94/2026-10-31.md#L148)。

当前 [图像路由](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-runtime/src/image_routing.rs:68) 对非 Flash 模型移除 tool images；DescribeThenContinue 明确只描述 user images，[代码注释](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-runtime/src/image_routing.rs:84)。[模型能力判定](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-core/src/model.rs:259) 也说明只验证过 Flash 接受工具图片。这是明确缺口。

可复用现有 sidecar 分析/去重/持久化，扩展到工具截图，输出带 observation id、图像尺寸与来源的描述，再交给主模型。把描述当外部观察数据，不作为可信指令；既有分析注入路径的信任边界应一并审查。缓存键需涵盖图片与任务问题，避免复用旧窗口观察；取消、费用与摘要都纳入账本。

但自然语言描述无法保证坐标 grounding 正确：涉及点击的结果仍需当前窗口归属、几何坐标与新观察核验，不能承诺“用文字旁路就等价于原生视觉”。验收应包含非 Flash 真实 GUI 任务成功率、陈旧截图、重复图片去重、侧模型失败与取消。截图张数/缩放后像素可确切统计；image token/费用只在 provider 提供时记实数，否则明确估算。

## 6. 结构证据检索：值得小规模试验

来源：[10/29 工程调研 STAIR/结构单元](https://github.com/echoVic/qingyun-wiki/blob/4a4e9c66d8459359632115d6744acdf5ccfc3beb/All%20in%20AI/%E8%B0%83%E7%A0%94%E6%97%A5%E6%8A%A5/AI%20%E5%B7%A5%E7%A8%8B%E8%B0%83%E7%A0%94/2026-10-29.md#L55)。

路径 mention search 的 [SearchMode](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-file-search/src/types.rs:7) 是 Fuzzy/Browse，匹配的是 File/Directory。它不是唯一代码搜索入口，现有 rg/glob/read 仍能完成内容搜索。项目记忆已有 [FTS5/BM25](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-runtime/src/memory/index.rs:50) 与 [来源/时间/核验提示](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-runtime/src/memory/auto.rs:295)，不能称 Orca 没有检索或记忆索引。

先在独立证据检索路径补目录树、Markdown 标题、模块/符号与精确 source spans，继续保留现有路径选择体验。索引要关联当前 source revision，修改后及时失效；忽略规则和工作区范围沿用现有约束。先用 B-L4 证明隐藏约束更容易找到，且上下文/工具次数下降，再决定是否引入 tree-sitter。暂不引入向量数据库或训练 DSI。

[10/22 的“摘要 + 历史可回查”建议](https://github.com/echoVic/qingyun-wiki/blob/4a4e9c66d8459359632115d6744acdf5ccfc3beb/All%20in%20AI/%E8%B0%83%E7%A0%94%E6%97%A5%E6%8A%A5/AI%20%E5%B7%A5%E7%A8%8B%E8%B0%83%E7%A0%94/2026-10-22.md#L47) 也值得并入此方向。现有 [session index](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-runtime/src/thread_store/session_index.rs:32) 主要做会话列表/标题检索，并非 transcript 内容搜索。对于有记录历史的会话，可以先提供限定当前任务的原文片段回查，返回 session/turn/message 标识与来源，受检索和上下文预算限制。显式关闭历史时不能假装可回查；跨项目历史也不能默认暴露。检索出旧指令应视为历史数据，避免恢复成新授权。

按任务预算决定是否扩检可做实验，但不是直接上线的固定规则。[Selective retrieval 原文](https://arxiv.org/abs/2610.37358) 的小型 QASPER 实验主要保住 evidence Hit@10，摘要明确 utility 优势未经 bootstrap 确认、答案正确性/faithfulness 未明确改善，且使用模型辅助评估。因此需要 Orca 自己的任务级验证。

## 7. 工具与记忆反馈：先测量，避免照搬自动裁剪

来源：[10/31 TokenEconomizer 与工具统计建议](https://github.com/echoVic/qingyun-wiki/blob/4a4e9c66d8459359632115d6744acdf5ccfc3beb/All%20in%20AI/%E8%B0%83%E7%A0%94%E6%97%A5%E6%8A%A5/AI%20%E5%B7%A5%E7%A8%8B%E8%B0%83%E7%A0%94/2026-10-31.md#L55)、[10/29 少数派记忆建议](https://github.com/echoVic/qingyun-wiki/blob/4a4e9c66d8459359632115d6744acdf5ccfc3beb/All%20in%20AI/%E8%B0%83%E7%A0%94%E6%97%A5%E6%8A%A5/AI%20%E5%B7%A5%E7%A8%8B%E8%B0%83%E7%A0%94/2026-10-29.md#L27)。

[TokenEconomizer](https://arxiv.org/abs/2610.22732) 摘要确认跨 Claude Code、Codex、Copilot 的连续任务实验，平均 token 成本下降 18.4–30.9%，任务成功相当。这个数字不能直接作为 Orca 的收益预测，其宿主与干预方式不同。

可先统计每次声明的 schema token、实际使用、失败类别、重复参数/错误与转向后的成功率；同时记录记忆事实被当前源码证实、过期或推翻的比例。父子任务 [已有 tokens/cache/cost/timestamps/retry_count](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-runtime/src/tasks.rs:916)，委派净收益需要组合展示和同任务单 Agent 对照，不能仅凭并行时间估算“节省了多少”。

工具长期未使用不代表将来不需要，自动摘除可能使任务无法完成、改变缓存前缀和模型行为。先提供信息，再对 schema 裁剪和一次性循环转向分别做任务评测。判重需区分同错误原样重试和输入/环境变化后的合理重试，不能将所有重复工具调用视为循环。

[CrossProfile 原文](https://arxiv.org/abs/2610.37624) 对日报解读构成重要纠偏：rarest-first 平均不优于 random；相关度选择会重新引入 decoy；多人署名会造成 social-proof 偏差。因此不直接采用“少数派优先”压缩。应保留支持/反对证据、来源与待核验状态，避免按 agent 数量或稀有度提高可信度；是否更好仍需 Orca 场景实测。

## 8. skill 来源与变更审计：可采纳窄版本

来源：[11/04 Twitter，skills audit 建议](https://github.com/echoVic/qingyun-wiki/blob/4a4e9c66d8459359632115d6744acdf5ccfc3beb/All%20in%20AI/%E8%B0%83%E7%A0%94%E6%97%A5%E6%8A%A5/AI%20Twitter%20%E7%83%AD%E7%82%B9%E6%95%B4%E7%90%86/2026-11-04.md#L267)。

当前 [skill 加载](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-tools/src/skills.rs:94) 已有 User/Project 来源区分、可信工作区 gate 与 allowed_root 路径约束，不能说完全没有边界。可以先为实际加载的 skill 记录规范路径、来源与内容摘要，区分用户主动更新、项目 revision 变化和运行期间意外变化；提示信息接入现有诊断/审计即可。

可选静态扫描作为后续辅助，未知或扫描通过都不能表示“内容可信”，内容哈希只能检测变化，不能证明没有恶意。日报引用的 SkillSpector 漏洞比例、规则集与兼容性尚未独立核实，不作为上线拦截阈值。先核对扫描器真实实现，再用已知 benign/恶意样本、误报率与变更检测测试评估。无需在每次加载时增加一层重复确认。

## 已有能力与暂缓事项

| 日报建议 | 当前判断 |
| --- | --- |
| 首次加 cache-aware compaction / memory search | 已有；补任务保真与经济性评测 |
| 首次加 DeepSeek beta strict 工具参数 | [已有条件应用及拒绝降级](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-provider/src/deepseek_http.rs:545)；可补降级/参数错误率观测 |
| 首次加 reasoning-only 检测 | [已有拒绝与有限恢复测试](/Users/bytedance/Documents/GitHub/blade-deepseek/crates/orca-provider/src/deepseek_http.rs:2257)；trace 导出契约可以单列评估 |
| 压缩 reason/strategy 事件、writes receipts | 已有；增强指标和覆盖范围 |
| 首次采集子 Agent token/cost/time | task records 已有；补汇总与 baseline 对照 |
| STAIR/“RAG 已死” | [原文](https://arxiv.org/abs/2609.03874) 使用 ToC 加训练 DSI，在 18 本书的 SearchTome 上评估；不是无需训练的代码检索证明 |
| 照搬 Concordat 框架 | [原文](https://arxiv.org/abs/2610.37461) 支持运行时契约约束方向；Orca 已有权限、预算与 continuation，适合借鉴违约回归，不必搬 Python 框架 |
| dsh-bridge / 进程内插件 ABI | preview 兼容成本与生态数字尚未独立核实；当前 MCP 已是可用边界，先有具体插件需求 |
| EndpointPool / 分布式 worker / 自研 serving | 当前请求稳定性与观测的回报更直接，先解决单机/单 endpoint |
| GUI 自动生成可信 skill、propose-only | 可作为后续产品实验；已验证操作序列才可提炼，敏感内容需处理，不能自动提升信任 |
| MCP Tasks / 新协议、远程 skills | 可作为已有具体 MCP server 需求时的后续兼容任务；当前代码没有 Tasks 客户端，但日报声称的最新协议版本未单独核对，不能仅改版本字符串或假定现有后台任务已兼容 |
| 轨迹级授权约束 | 优先补权限继承、撤权、运行中切换和恢复后的回归；更大策略 DSL 等有实际需求再评估 |
| 照搬另一 agent 全部用例或参数命名 | 只提取用户实际工作流的契约回归，例如运行中切权限、blocked 后恢复、参数冲突；不以全量行为相同为目标 |

## 证据与局限

- 本文核对的是固定 GitHub 日报、当前 Orca 源码及 5 篇相关 arXiv 摘要。没有对所有 Twitter 原帖、项目 stars、外部实验数字逐一复现。
- 本轮没有新增 live endpoint/GUI/性能实验，所以以上收益是待验证假设。实施验收应同时看任务成功率、每个成功任务成本、耗时和误报完成率。
- 日报以各自采集日的快照为依据，其中 issue 状态、版本和实现描述已经过时。本文采用当前源码判定是否仍有缺口。
- 本次 CLI/权限代码已提交为 5f1dafd；提交前本轮 50 项测试通过，前一轮相关验证结果不在本研究中当作新性能证据。并行品牌改动未纳入该提交。
