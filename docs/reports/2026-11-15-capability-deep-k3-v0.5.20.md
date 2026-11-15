# Capability evaluation — deep profile k=3, v0.5.20（2026-11-15）

上一轮（PR #164）在 v0.5.20 上用 `standard` k=1 跑出 44 场景 / 43 通过，唯一失败是 headless rewind 的
checkpoint 捕获（#163，修复 PR #165）。本轮在**修复版**上把全部轴按 `deep` profile（每场景 3 次）重跑，
用来区分「行为稳定」和「k=1 偶然通过」，并把仍然存在的问题先提 issue。

## 协议

| 项 | 值 |
|---|---|
| Source | `82d645af`（`fix/headless-rewind-checkpoints` rebase 到 main `42f09ee1`，含 #163 修复） |
| 二进制 | host release（含修复）+ musl 静态（含修复，用于 docker 场景） |
| 模型 | `deepseek-flash`，reasoning effort `max`（B–L 显式传入；F 不支持覆盖，用默认路由） |
| Profile | `deep`，每场景 3 次尝试（k=3） |
| 运行 | `20261115-190713-436015000`（B/C/D/E/G/H/I/J/K/L，41 场景 ×3）+ `20261115-195152-684207000`（F，3 ×3） |
| 规模 | **44 场景、132 次尝试**，全部通过；总尝试耗时约 40 分钟 |
| 花费 | **$0.327** token 侧观测（不含 K 轴 web/OCR 按次费，见 #166） |
| 未覆盖 | A 轴（需要先有一次 v0.5.20 的 TB2 Harbor 全量运行，再跑 2×/4× 复跑，约 3.5h+）；G 轴桌面 live 子集（独立 harness，最近 2026-10-30 测过 3/3，本轮未重跑） |

## 结果：132/132 次尝试通过（0 失败）

| 轴 | 场景 | 尝试 | 结果 | 备注 |
|---|---|---|---|---|
| B 长程工程 | 9 | 27 | **27/27** | 含 docker 变体、真实仓库双函数 + 隐藏测试（最慢一次 325.8s） |
| C 上下文 | 5 | 15 | **15/15** | 50k/150k/300k + 多约束 full/compacted 各 3/3（格式断言已接受文档原样写法） |
| D 故障恢复 | 7 | 21 | **21/21** | 含只读工作区 `blocked` 3/3（#156 的假成功不再出现） |
| E 编排 | 3 | 9 | **9/9** | 5/10/25 fan-out 各 3/3 |
| F goal 长跑 | 3 | 9 | **9/9** | 1/3/6 次 pause→resume 各 3/3 |
| G 外部状态 | 1 | 3 | **3/3** | 独立 SQLite 验收 3/3 |
| H 泛化 | 2 | 6 | **6/6** | Python / 改名 Rust 换皮 |
| I rewind | 4 | 12 | **12/12** | **#163 修复后 I-1 连续 3/3 通过**：snapshot 写入、文件恢复、`session.rewind` 追加 |
| J 重试与冷却 | 3 | 9 | **9/9** | 429/503 `Retry-After` 冷却间隔稳定 ≥1.0s；持续 429 在 5 次发送上限后干净失败 |
| K DeepSeek web/OCR | 4 | 12 | **12/12** | web_search / web_fetch / read_document（含缓存）/ doctor --online 各 3/3 |
| L harness 证据 | 3 | 9 | **9/9** | workspace 证据、失败时 resume 证据、blocked + blockers 各 3/3 |

Wilson 区间（aggregate 输出）：每个场景 3/3 → 95% 下界约 0.44，说明「单场景 k=3 全过」仍不能证明
确定性；但 132/132 无一次失败，至少排除了 k=1 那次唯一的 rewind 失败是抖动，也排除了新轴（I/J/K/L）
的明显不稳定。

## 与 k=1 的差异

- **I-1 从 fail → 3/3 pass**：确认 #163 的修复在三次重复运行中稳定（不是偶然）。
- **D-7 三次都是 `blocked` + 结构化 blocker**：确认 #156 描述的行为已修复。
- **C-multi full/compacted 各 3/3**：上一次 k=1 记录里曾因 oracle 只接受数组而失败；oracle 接受文档原样
  的 `network; delete archives` 后稳定通过，说明那是断言问题而非模型波动。
- E 轴 3/3：修复 macOS `/tmp` vs `/private/tmp` 的验证器路径比较后稳定。

## 仍然开着的 issue

- **#163**（headless rewind 无文件 checkpoint）：已由 **PR #165** 修复，本报告是其 k=3 黑盒验证；
  linux-full / native-x64 / macos-tui-e2e 已绿，native-arm64 当时仍在队列。
- **#166**（本轮新提，[Eval]）：评测的花费统计不含 DeepSeek web/OCR 的按次计费，`--max-usd` 与
  cost-per-pass 对 K 轴偏低；需要 harness 记录 extras 调用并在预算/报告里分开计价。
- **#169**（本轮新提，[Eval]）：`restarted_runtime_terminalizes_an_inflight_typed_workflow_and_its_task`
  在负载高的 CI 上偶发失败（PR #165 一次失败、同代码复跑通过），属于恢复/终态断言的时序 flake。
- **#156**：D-7 已在 v0.5.20 返回结构化 `blocked`，已在该 issue 下补证据，等待确认后关闭。

## 边界与下一步

1. **A 轴仍未覆盖**：需要一次 v0.5.20 的 TB2 全量运行作为基线，才能跑 2×/4× 预算倍率；这是纯成本决定
   （~3.5h + 复跑），不是 harness 缺口。
2. **K 轴计价**：按 #166 补 extras 记账后，deep profile 的 `cost_per_pass` 才有完整口径。
3. **下一档难度**：B 已到真实仓库双函数 + 隐藏测试 + 既有测试全绿；下一档是真实 PR 规模多文件改动 +
   评审级验收测试；D 下一档是 docker 破坏性故障（磁盘满/只读设备）；C 下一档是运行时真实压缩事件。
4. **G 桌面 live**：本轮只跑了外部状态（G-1），桌面 live 子集未重跑；需要时单独跑
   `scripts/eval/computer_use/`。
