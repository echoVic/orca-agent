# Capability evaluation — v0.5.20（2026-11-15）

本轮目标：在 v0.5.20 上重跑能力评测，为 v0.5.15–v0.5.20 新增的 rewind、重试与冷却、harness 证据、
DeepSeek 搜索/抓取/OCR 补题，再用失败项决定下一步方向。

## 协议

| 项 | 值 |
|---|---|
| Source | `d227f7b5`（v0.5.20 tag） |
| 二进制 | host release 构建（2m14s，sha256 `d347db36…`）+ musl 静态构建（2m48s，Docker `messense/rust-musl-cross`） |
| 模型 | 请求 `deepseek-flash`，reasoning effort `max`（`--model` / `--reasoning-effort` 显式传入；响应侧模型 provider 未暴露，记为 unknown） |
| Profile | `standard`，每场景 1 次尝试（k=1） |
| 运行 | `20261115-174239-095333000`（B/C/D/E/G/H/I/J/K/L，41 场景）+ `20261115-175330-139467000`（F，3 场景） |
| 花费 | 观测到 41 场景合计 **$0.104**（runtime 上报的 `estimated_cost_usd`；不含 K 轴 DeepSeek web/OCR 的按次计费，见边界） |
| 环境 | macOS aarch64，Docker/OrbStack 已启动；`reap_harbor.sh` 未涉及（本轮无 Harbor） |
| 未覆盖 | A 轴（需要一次 TB2 全量 Harbor run，~3.5h，本轮未跑）；G 轴的桌面 live 子集（独立 harness，最近一次 2026-10-30 测过 S1/M2/M7 3/3，本轮未重跑） |

## 结果：44 场景，43 通过（97.7%）

| 轴 | 场景 | 结果 | 说明 |
|---|---|---|---|
| B 长程工程 | 9 | **9/9** | L1–L5 合成 + docker 变体 + 真实仓库单函数（L4-real，135s）+ **新增真实仓库双函数/隐藏测试 L6**（72s） |
| C 上下文 | 5 | **5/5** | 50k/150k/300k 检索；`C-multi-constraint-full/compacted` 改用真实模型后 7 项约束全对（12.2s/15.2s） |
| D 故障恢复 | 7 | **7/7** | 1–6 叠加 host-safe 故障；`D-7-honest-blocker`（只读工作区）现在返回结构化 `blocked`，修复了旧版假成功（#156 已评论） |
| E 编排 | 3 | **3/3** | 5/10/25 fan-out；修复了验证器在 macOS `/tmp` → `/private/tmp` 路径不匹配的 harness bug |
| F goal 长跑 | 3 | **3/3** | 1/3/6 次 pause→resume（单独运行：goal 不支持显式 model 覆盖） |
| G 外部状态 | 1 | **1/1** | `G-1-external-state` 改用真实模型后通过（12.5s），独立 SQLite 验证 |
| H 泛化 | 2 | **2/2** | Python / 改名 Rust 换皮 |
| **I rewind（新）** | 4 | **3/4** | 会话回滚/dry-run/prune/rollback 通过；**code/both 恢复失败**（见下） |
| **J 重试与冷却（新）** | 3 | **3/3** | 429/503 + `Retry-After: 1` 重试成功（冷却间隔 1.0s）；持续 429 在 5 次发送上限后干净失败（4.4s） |
| **K DeepSeek web/OCR（新）** | 4 | **4/4** | web_search、web_fetch（example.com 标题）、read_document OCR（标记 `ORCA-OCR-MARKER-2026` + 磁盘缓存）、`doctor --online` |
| **L harness 证据（新）** | 3 | **3/3** | 工作区净变化证据、resume/complete 证据、失败时的 resume 证据、`blocked` 状态与 blocker 列表 |

## 唯一失败：headless 会话没有文件 checkpoint（issue #163）

`I-1-rewind-code-restore`：`orca exec --save-history` + 一个成功的 `write_file` 工具调用，会话结束时
`session.completed` 报告 `status: success`、工作区净变化 `file.txt` 改变，但 transcript 里**没有
`session.snapshot`** 记录，`$ORCA_HOME/rewind` 里也没有对象。随后：

```
orca rewind --last --mode both
→ orca: no file snapshot recorded for message '…'; use --mode conversation
```

同一会话的 `--mode conversation` 正常（`kept_messages: 1`，追加 `session.rewind`），`--dry-run`、
`--prune`、`--rollback` 契约也都正常。根因在代码里很清楚：checkpoint 捕获只在交互路径
（`crates/orca-runtime/src/controller.rs:2275` 的 `begin_turn` / `2355` 的 `finish_rewind_turn`）注册，
headless 入口（`crates/orca-runtime/src/runtime_host.rs:23478` 的 `run_headless_session`）没有等价调用，
而 `docs/releases/v0.5.19.md` 宣称「File checkpointing is on for sessions that record history」并给出
headless rewind 的用法。已提 **[issue #163]**（含复现、代码指针、回归建议）。

## 本轮为评测量身修复的 harness 问题（不是模型能力）

1. **`exec_flags` 未接通**：scenario 解析了 `exec_flags`，但没有传入 `Scenario` 构造，`--save-history`
   从未生效；修复后 I/L 轴才看到真实行为。
2. **验证器环境不足**：新增 `CAPABILITY_HOME`、`CAPABILITY_ATTEMPT_DIR`、`CAPABILITY_PROVIDER_LOG`、
   `CAPABILITY_AGENT_STDOUT/STDERR`，使 I/J/L/K 的断言能观察到会话、provider 日志和证据字段。
3. **E 轴路径规范化**：验证器用字符串比较 `cwd`，macOS 上 `/tmp` 与 `/private/tmp` 不等导致全挂；
   改 `os.path.realpath`。
4. **C-multi oracle 过严**：上下文写的是 `Forbidden: network; delete archives`，oracle 只接受 JSON 数组，
   模型忠实写出分号字符串被判失败；oracle 现在接受两种表示（断言仍保持 7 字段精确）。
5. **C-multi / G-1 的 provider 从 mock 改为 deepseek**：mock 不是求解器，这两组是真实能力场景；
   改后均通过。
6. 新增 **`tools/scan_secrets.py`**：跑完扫描 run 目录中的凭证（exact key、Authorization、`sk-` token）。

## 凭证扫描（按用户要求，跑完必扫）

对 clean 运行与复跑目录共 **28,598 个文件**扫描（`scripts/eval/capability/tools/scan_secrets.py`）：

```
exact key hits: 0
authorization bearer hits: 18 (review)
unrelated sk- token hits: 24 (review)
```

- 精确 key：**0**；
- 18 条 Bearer / 24 条 `sk-` 命中全部来自 B-L4-real/B-L6-real 复制进来的 Orca 仓库自身的测试与文档
  fixture（`sk-test-…`、`sk-orca-…`、`token-from-environment` 等），不是本轮泄漏；
- 结论：**本轮评测产物没有出现真实 key 明文**；扫描器已入库，后续每轮都跑。

边界：扫描器按内容匹配，不解码压缩/二进制；`provider-requests.jsonl` 由 harness 允许列表提取，不含
Authorization。K 轴的 DeepSeek web/OCR 按次费用（$0.75/1k 搜索、$0.225/1k 页 OCR）不在
`estimated_cost_usd` 内，所以本轮 $0.104 只是 token 侧观测。

## 用结果定方向

1. **修 #163（唯一产品失败，优先级最高）**：把 checkpoint 捕获移到 headless/交互共用的 turn 路径，
   或让 headless rewind 显式拒绝并给出可操作提示；回归用 `I-1-rewind-code-restore`。
2. **补 K 轴的按次计费观测**：web/OCR 调用费目前不进预算与报告，K 轴要在工具结果/证据里记录调用次数，
   让 `--max-usd` 与 cost-per-pass 覆盖 DeepSeek extras。
3. **下一档难度**：B 已到「真实仓库双函数 + 隐藏测试 + 既有测试全绿」，下一档应是真实 PR 规模的多文件
   改动（含评审级验收测试）；D 已到只读工作区，下一档是 docker 破坏性故障（磁盘满、只读设备）；
   C 已到 300k 检索与多约束，下一档是运行时真实压缩事件（长会话跨压缩 + 埋点事实）。
4. **稳定性**：本轮 k=1；唯一的语义抖动来自格式（分号字符串 vs 数组），k=3 的 deep profile 应在
   I/J/K/L 新轴上先跑一轮再谈趋势。
5. **TB2 对照**：A 轴（2×/4× 预算）需要在 v0.5.20 上重跑才有「时间天花板」对比；这属于下一轮的成本
   决定（一次全量 3.5h + 复跑）。

## 产物

- 运行：`/tmp/eval-v0520-clean/20261115-174239-095333000`（41 场景）、`…/20261115-175330-139467000`（F）。
- 入库：`docs/reports/capability-scoreboard.json` 两条记录、自动报告
  `2026-11-15-capability-20261115-174239-095333000.md` / `…-175330-139467000.md`。
- 场景：新增 `I-*`（4）、`J-*`（3）、`K-*`（4）、`L-*`（3）、`B-l6-real-multi`；`C-multi-*`、`G-1` 改为真实模型。
- 工具：`scripts/eval/capability/tools/scan_secrets.py`；mock provider 新增 retry/write/blocker 模式。
