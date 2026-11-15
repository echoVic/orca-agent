# TB2 预算缩放（A 轴）执行计划与预算 — v0.5.20

> 执行结果：[`2026-11-16-tb2-v0520-budget-scaling.md`](2026-11-16-tb2-v0520-budget-scaling.md)（87 题 1× → 2×/4×，capability 4 + flaky 1）。

日期：2026-11-15 · 目标：回答「v0.5.20 在 TB2 上剩下的失败，有多少是 wall-clock 预算问题，多少是真实
能力缺口」。这是能力评测 A 轴的独立运行计划；不在 capability harness（B–L）内，也不与 TB2 mean 合并。

## 背景与先例

| 项 | v0.5.14 基线（`docs/reports/2026-10-28-tb2-v0514-summary.json`） |
|---|---|
| Harness | Harbor 0.20.0，`terminal_bench.orca_agent:OrcaInstalledAgent` |
| 模型 | `deepseek-flash`，reasoning effort `max` |
| 任务 | 88 × 1 trial，并发 5，`qemu-startup` 按 quarantine 排除 |
| 结果 | mean **0.8182**（72/88），6 × `AgentTimeoutError`，0 环境启动失败 |
| 观测 | 88 题 agent 时间合计 **11.1 小时**（单题最长 3,203.6s），轨迹约 65MB |

上一轮 A 轴（v0.5.14）的做法：取 16 个失败/超时任务，在 2× / 4× 预算下复跑，结果 **9 个在 2× 通过、
1 个在 4× 通过、1 个环境失败、5 个在 4× 仍失败**（capability gap，见 issue #154）。这说明
`0.8182` 主要是 wall-clock 约束，真实能力缺口约 5/88。

## 目标与验收

1. 在 v0.5.20（main 上任一含 rewind 修复的版本）复现 88 题 1× 基线；
2. 对每个失败任务，在 2× 和 4× wall budget 下复跑，形成逐题 transition 表；
3. 按设计稿验收：**每个失败要么在更高预算下通过（`time_bound`），要么在 4× 仍失败且失败原因一致
   （`capability`）**；环境类失败单独归类并注明是否重跑；
4. 输出：A 轴报告 + ledger 条目（`capability` block 之外单独列 TB2/A），真实能力缺口提 issue。

## 阶段与命令

### 阶段 0：准备（约 15 分钟）

- 从目标 commit 构建 musl 静态二进制：
  `docker run --rm -v "$PWD:/src" -w /src messense/rust-musl-cross:x86_64-musl cargo build --release --locked --target x86_64-unknown-linux-musl --bin orca`
- 确认 Harbor 0.20.0 环境、Docker/OrbStack、API key（`~/.orca/auth.json`）；
- 清理磁盘：单轮约 65MB 轨迹 + 镜像；建议预留 ≥ 20GB；
- 建立 Harbor job：`-k 1 -n 5 --environment-build-timeout-multiplier 2.0`，排除 `qemu-startup`。

### 阶段 1：1× 基线（wall 约 2.5–3.5 小时）

- 88 题 × 1 trial，并发 5；预计 agent 时间 ~11 小时、wall 2.5–3.5 小时；
- 产物：trials JSON（task / trial / reward / exception / agent_seconds / turns / trajectory_bytes）；
- 与 v0.5.14 的 72/88、6 个 timeout 对照，标出新增失败 / 修复项；
- 检查是否出现 rewind 修复后的回归（预期无：TB2 不用 rewind 场景，但二进制更新需确认无崩溃）。

### 阶段 2：预算缩放（wall 约 1.5–2.5 小时）

- 失败集合 = reward < 1 或 exception 的任务（预期 10–16 个）；
- 对每个任务在 **2×** 与 **4×** wall budget 下复跑（任务 timeout 覆写或 Harbor 配置；
  若某任务不允许覆写，则用等价的本地合成预算敏感任务，并在报告中标注）；
- 主结果 k=1；对「2× 通过、4× 边界」的任务补 k=2，避免把抖动算成 `time_bound`；
- 并发保持 5，4× 任务可能跑满 timeout，按队列串行度控制总时长。

### 阶段 3：分类与产出（约 1 小时）

对每个失败任务标注：

| 分类 | 判定 |
|---|---|
| `time_bound` | 2× 或 4× 通过 |
| `environment` | 环境启动失败/基础设施异常，附重跑结果 |
| `capability` | 4× 仍失败且失败信息与 1× 同类 |
| `flaky` | 同预算重复结果不一致（需要 k=2 证据） |

产出：
- `docs/reports/<date>-tb2-v0520-budget-scaling.md`（Δpass、逐题 transition 表、wall 分布）；
- `evaluation-ledger.json` 新条目（含 `tb2_mean`、`tb2_mean_scored_only`、A 轴分类计数）；
- 真实能力缺口 / 产品问题 → 先提 issue；复现脚本与 job 路径写进报告。

## 预算

### 时间

| 阶段 | 估计 |
|---|---|
| 准备 | 15 min |
| 1× 基线（88 题） | 2.5–3.5 h |
| 2× / 4× 复跑（10–16 题 ×2） | 1.5–2.5 h（4× timeout 主导） |
| 分类 + 报告 | 1 h |
| **合计** | **约 5–7 小时 wall**（建议安排无人值守时段） |

### API 花费

v0.5.14 的 trials 未记录 token 成本；参照 capability round（0.66 agent-hours ≈ $0.33）与 TB2 更长
上下文/更多轮次，基线估计 **$10–25**，2×/4× 复跑再 **$3–8**；合计 **$13–33**，以运行后轨迹 usage 为准。
建议在 Harbor/agent 侧设总花费上限，并在报告中写「观测成本，非账单」。

### 资源

- Docker 并发 5 + 环境构建（`environment_build_timeout_multiplier 2.0`）；
- 磁盘：单轮 ~65MB 轨迹，含镜像与环境建议 ≥ 20GB；
- 机器保持不休眠；TB2 任务里有长时 CPU 任务（caffe-cifar-10、make-doom-for-mips 等）。

## 风险与对策

| 风险 | 对策 |
|---|---|
| Harbor/环境启动长时间排队 | 先跑 1 题 smoke，确认镜像可用再全量 |
| 4× timeout 任务拖长 wall | 单独队列，先 2× 后 4×，按单题 timeout 排序 |
| 速率限制/429 | 复用 v0.5.17 的重试与冷却；并发可降到 3 |
| 结果不可比（模型/设置差异） | 固定 `deepseek-flash`、effort `max`、`-k 1 -n 5`、排除同一 quarantine 列表 |
| 预算超支 | 设 agent max cost / 总 spend guard；每阶段结束记录观测花费 |

## 不在本轮范围

- 不改 TB2 任务集、不重跑 quarantine 的 `qemu-startup`；
- 不把 TB2 数字并入 capability B–L 的 pass rate；
- 不在本计划内做 8× 预算或 k=3 全量（成本过高；如需要，仅对 `time_bound` 边界题补 k=2）。
