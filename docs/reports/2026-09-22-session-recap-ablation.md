# Session recap 消融实验

实验基于 `session-recap-plan` worktree 的当前 dirty 实现，另建了独立实验 worktree：

`/Users/qingyun/.codex/worktrees/recap-ablation-*`

原实现 worktree 和 `tui-visual-unification` 都没有被实验脚本写入。实验使用固定合成 evidence、Mock provider 和只监听 `127.0.0.1` 的本地 SSE fixture，没有读取真实会话、没有调用真实模型、没有消耗 API 额度。

方法是单变量 source mutation：每个变体都从同一份 baseline 重新开始，只移除一个机制；另外保留了一个“active-request gate + success dedupe 同时移除”的交互对照。每个变体运行生产测试二进制里的 probe，结果以 `RECAP_ABLATION_JSON` 输出。完整原始结果和日志在：

- `/Users/qingyun/.codex/worktrees/session-recap-plan/recap-ablation-results/provider2/`
- `/Users/qingyun/.codex/worktrees/session-recap-plan/recap-ablation-results/runtime/`
- `/Users/qingyun/.codex/worktrees/session-recap-plan/recap-ablation-results/tui/`

## 结果

| 机制 | baseline | 去掉机制后的观察 | 结论 |
| --- | --- | --- | --- |
| provider cache | 6 次相同请求实际发 1 次 HTTP 请求；后 5 次约 0.04ms | 6 次 HTTP 请求；后续每次约 32.6ms | 缓存有效，且避免重复 provider 调用 |
| 工具隔离 | wire request 的 tools 数量为 0 | tools 数量为 1 | 必须保留；辅助摘要不能继承会话工具 |
| tool-call response guard | `tool_response_rejected=true` | `false` | 必须保留，避免把工具调用当摘要交付 |
| 输入上限 | 过大 evidence 被拒绝 | `oversized_input_rejected=false` | 必须保留 |
| deadline | 80ms deadline 被执行 | 去掉后 fixture 实际等待约 6.7s | 必须保留 |
| 输出保留上限 | retained output 约 116 tokens | 去掉后约 6000 tokens | 必须保留；当前 wire `max_tokens` 观测为 384000，说明“保留后截断”没有把上限传到 provider 请求本身 |
| evidence redaction | system/reasoning sentinel 均未进入 evidence | 两类 sentinel 都进入 | 必须保留 |
| evidence byte bound | 24KiB 截断 | 长 ASCII 约 72KiB，长 CJK 约 120KiB | 字节上限有效，但 baseline 仍有 token 问题 |
| marker/request/attachment fence | 错 marker、错 request、错 attachment 均 0/20 接受 | 去 marker fence 后错 marker 20/20 被接受；去 transcript isolation 后 20/20 写入 transcript | 三类 fence 和 transcript 隔离都有效 |
| invalidation | turn、completion、compaction、backtrack、new session 都失效旧 recap | 去掉后 5 类都不失效 | 必须保留 |
| auto quiet period | 0/30/179 秒均不可触发，180 秒起可触发 | 去掉后从 0 秒起全部可触发 | 三分钟门槛有效 |
| operation count gate | 两次操作后没有自动 recap，三次后有 | 去掉后两次操作就有 | 三次完成操作门槛有效 |
| strip row cap | 8 行输入显示 3 行 | 去掉后显示 8 行 | 三行上限有效 |

## baseline 暴露的缺陷

1. **自动请求完成后没有清除 controller 的 active request 标记。** hosted baseline 序列为：两次操作 `none`、三次操作 `ready`、同 marker 新 focus `none`、新 marker `none`、手动请求 `ready`。移除 active-request gate 后，同一序列变成 `none → ready → ready → ready → ready`，说明当前 gate 遮住了 success-marker dedupe，同时也阻止了新 marker 的自动请求。这个问题不是“去掉 dedupe 造成的”，而是 baseline 生命周期清理缺失。

2. **runtime 的 24KiB 字节上限没有保证 4096 token 上限。** baseline 的长 ASCII evidence 约 6,150 estimated tokens / 5,466 measured tokens，长 CJK 约 2,055 estimated tokens / 16,385 measured tokens，均超过 provider 的 4096 token 检查，并且最新结论和当前计划在从头截断时可能被丢掉。需要按 provider token counter 做有界、保留最新事实的裁剪。

3. **TUI 自动 recap eligibility 没有排除 child focus。** synthetic probe 在 child target 下仍返回 `child_eligible=true`。当前 gate 检查了 `side_conversation`，但没有检查 `conversation_target.task_id()`。

4. **recap strip 的 prefix 没计入截断宽度。** 20-column probe 中 ready/pending/failed 行都出现 display-width overflow；例如 `short_lines` 的内容宽度减去了 2，但还要加回 `◈`/错误提示前缀。详情面板使用单独外框宽度，短条需要按状态前缀分别计算可用宽度。

## 修复后复测

修复后的同一 baseline probe 结果保存在：

`/Users/qingyun/.codex/worktrees/session-recap-plan/recap-ablation-results/fix-check/`

- provider 请求体的 `max_tokens` 从 384000 降为 120，工具数量仍为 0，缓存、deadline、输入限制和 tool-call guard 均保持有效。
- 长 ASCII/CJK evidence 的 production token count 都在 4096 以内，并保留最新事实和当前计划；system/reasoning sentinel 仍被排除。
- hosted controller 序列变为：两次操作 `none`、三次操作 `ready`、同 marker `none`、新 marker `ready`、手动请求 `ready`。这说明完成通知会释放 active request，同时 success-marker dedupe 仍阻止相同内容重复触发。
- child target 的自动资格变为 `false`。
- 0、1、2、3、4、20、40、80 列下 ready/pending/failed 的 `overflow_rows` 全部为 0，三行上限保持不变。

## 解读边界

这些结果证明了机制对本地可观测行为的影响：请求次数、是否泄漏 sentinel、边界接受率、行数和固定 fixture 的等待时间。它们没有证明真实模型的摘要质量、任务成功率、真实服务 token 成本、跨终端可用性或安全性。provider timing 只是本地 SSE fixture timing；`wire max_tokens=384000` 是本地请求体的直接观测，不等同于远端服务最终行为。

实验脚本、变体定义和复现说明在 `session-recap-plan` 分支的 `scripts/recap-ablation/`（提交 `cbd28aa6`、`f19cc485`）；运行器会记录 source/Cargo.lock/fixture hash，并在 `finally` 中恢复实验 worktree 的变体文件。合入 main 时没有带上这套脚手架：变体按源码原文替换定位，recap 适配新设计体系后这些原文已不存在；机制本身由各 crate 的单元测试覆盖。
