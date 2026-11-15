# TB2 预算缩放（A 轴）— v0.5.20（2026-11-16）

执行 [`2026-11-15-tb2-budget-scaling-plan.md`](2026-11-15-tb2-budget-scaling-plan.md) 的结果：v0.5.20 的
TB2 1× 基线与失败集 2×/4× 复跑。结论与 v0.5.14（issue #154）一致：**headline 由 wall-clock 预算主导，
真实能力缺口约 4/87（4.6%），另有一个任务表现为 k=1 抖动**。

## 协议

| 项 | 值 |
|---|---|
| Source | `39ee9edc`（main，含 #165 rewind 修复、#167 费用统计） |
| 二进制 | musl x86_64 静态构建，sha256 `5a68d06eeeee6d2fbd34b8c61a044a2d3696dfd10520f98b1b37b27aec8e2e05` |
| Harness | Harbor（`terminal_bench.orca_agent:OrcaInstalledAgent`），Docker 环境 |
| 模型 | `deepseek-flash`，reasoning effort 默认（`max`） |
| 数据集 | `terminal-bench/terminal-bench-2@sha256:c6fc2e2382c1dbae99b2d5ecd2f4f4a60c3c01e0d84642d69b4afd92e99d078b`（与 v0.5.14 相同 ref） |
| 任务 | 87（排除 `terminal-bench/qemu-startup`、`terminal-bench/qemu-alpine-ssh`，均为数据集镜像缺陷 quarantine） |
| 设置 | 1×：`-k 1 -n 5 --environment-build-timeout-multiplier 2.0`；2×/4×：`--timeout-multiplier 2|4`，`-n 4|3` |
| 运行 | 1× `jobs/full-87-v0520/full-87-v0520-20261115-215809`；2× `jobs/scale-2x-v0520/scale-2.0x-20261116-021600`；4× `jobs/scale-4x-v0520/scale-4.0x-20261116-032212`；dna 复测 `jobs/probe-dna-v0520/dna-probe-v0520` |
| 时间 | 1× 约 4h05m wall；2× 约 55min；4× 约 50min（含长尾 timeout 任务） |

## 1× 基线

| 指标 | 值 |
|---|---|
| trials | 87 |
| scored / passed / zeroed / errored | 81 / **68** / 13 / 11 |
| mean（全部 trial） | **0.7816** |
| mean（仅 scored） | 0.8395 |
| exceptions | AgentTimeoutError 5、AgentSetupTimeoutError 3、VerifierTimeoutError 3 |

与 v0.5.14 同 87 题视图相比：v0.5.14 是 72/87 = 0.8276，v0.5.20 是 68/87 = 0.7816（低 4 题）。
任务级差异：v0.5.20 修复了 6 题（chess-best-move、configure-git-webserver、crack-7z-hash、
large-scale-text-editing、make-mips-interpreter、protein-assembly），但 10 题从通过变为失败，其中 6 题是
1× 的 setup/verifier 超时（见下），其余包含 `dna-assembly`（复测为抖动）。

## 2× / 4× 复跑：19 个失败任务

| task | 1× | 1× exception | 2× | 4× |
|---|---|---|---|---|
| adaptive-rejection-sampler | 0.0 | AgentTimeoutError | 0.0 | 0.0 |
| build-pmars | — | AgentSetupTimeoutError | 1.0 | — |
| caffe-cifar-10 | 0.0 | AgentTimeoutError | 1.0 | — |
| dna-assembly | 0.0 | — | 0.0 | 0.0 |
| extract-moves-from-video | 0.0 | AgentTimeoutError | 1.0 | — |
| filter-js-from-html | 0.0 | — | 0.0 | 0.0 |
| gcode-to-text | 0.0 | — | 1.0 | — |
| git-multibranch | — | AgentSetupTimeoutError | 1.0 | — |
| gpt2-codegolf | 0.0 | AgentTimeoutError | 1.0 | — |
| install-windows-3-11 | 0.0 | — | 1.0 | — |
| log-summary-date-ranges | — | AgentSetupTimeoutError | 1.0 | — |
| make-doom-for-mips | 0.0 | — | 0.0 | 0.0 |
| merge-diff-arc-agi-task | 0.0 | — | 1.0 | — |
| pypi-server | 0.0 | — | 1.0 | — |
| pytorch-model-cli | 0.0 | — | 0.0 | 0.0 |
| pytorch-model-recovery | — | VerifierTimeoutError | 1.0 | — |
| raman-fitting | 0.0 | — | 1.0 | — |
| torch-pipeline-parallelism | — | VerifierTimeoutError | 1.0 | — |
| torch-tensor-parallelism | — | VerifierTimeoutError | 1.0 | — |

- **2×：14/19 通过**，所有 exception 清零；
- **4×：5 个剩余任务全部仍失败**，没有额外通过；
- 等价的 87 题视图：**2× = 82/87 = 0.9425，4× = 82/87 = 0.9425**。

## 分类

| 类别 | 数量 | 任务 |
|---|---|---|
| `time_bound`（2× 通过） | 14 | build-pmars, caffe-cifar-10, extract-moves-from-video, gcode-to-text, git-multibranch, gpt2-codegolf, install-windows-3-11, log-summary-date-ranges, merge-diff-arc-agi-task, pypi-server, pytorch-model-recovery, raman-fitting, torch-pipeline-parallelism, torch-tensor-parallelism |
| `capability`（4× 仍失败） | 4 | adaptive-rejection-sampler, filter-js-from-html, make-doom-for-mips, pytorch-model-cli |
| `flaky` | 1 | dna-assembly |

补充说明：

- 14 个 time_bound 里有 **6 个是 1× 的 setup/verifier 超时**（3 setup + 3 verifier）；2× 能通过是因为
  `--timeout-multiplier` 同时放大了 agent、setup、verifier 的 deadline。它们归为 environment/time 类，
  不是能力提升。
- capability 集合 = v0.5.14 的 5 个任务减去 `configure-git-webserver`（v0.5.20 在 1× 已通过），
  **没有新增能力缺口**。
- `dna-assembly`：v0.5.14 通过（agent 1154s、76 turns）；v0.5.20 1× 失败、2×/4× 失败，但 1× 复测
  **2 次中 1 次通过**（`docs/reports/2026-11-16-tb2-v0520-dna-probe-summary.json`）。失败形态是
  verifier 的引物 overhang 断言（`egfp.left != rc(vector.right)`），不是崩溃；按 `flaky` 处理，
  不计入能力缺口，后续 baseline 建议 `-k 2`。

## 与 v0.5.14 A 轴对照

| 指标 | v0.5.14（#154） | v0.5.20（本轮） |
|---|---|---|
| 1× mean | 0.8182（88 题）/ 0.8276（87 题视图） | 0.7816（87） |
| 失败数 | 16 | 19 |
| 2× 通过 | 9 | 14 |
| 4× 额外通过 | 1（caffe-cifar-10） | 0 |
| 等价 2×/4× mean | 82/88 = 0.9318（4×） | 82/87 = 0.9425（2×/4×） |
| 能力缺口 | 5 题 | **4 题（4.6%）** + 1 flaky |

结论：v0.5.20 在 1× 上低 4 题，主要来自 6 个环境超时；把预算放大后与 v0.5.14 持平略高。
**分数第一杠杆仍然是完成纪律/预算策略，而不是新能力。**

## 凭证扫描（本轮发现并处置）

运行结束后用 `scripts/eval/capability/tools/scan_secrets.py` 扫描 1,245 个 TB2 产物文件，**命中 1 处真实
key**：`jobs/full-87-v0520/…/make-mips-interpreter__dQY3it9/agent/trajectory.jsonl` 中的工具输出包含容器
环境变量清单（`DEEPSEEK_API_KEY=sk-…`）。原因：TB2 adapter 把 key 作为容器环境变量注入，任务脚本打印
`env` 即进入 agent 轨迹；已就地把该值替换为 `<REDACTED_DEEPSEEK_API_KEY>`，复扫 0 命中，仓库内无提交
（`jobs/` 未入库）。已提 **[#178](https://github.com/echoVic/orca-agent/issues/178)**（安全类），建议改用
`~/.orca/auth.json`（0600）+ `ORCA_HOME` 或对轨迹做后端脱敏，并**轮换该 key**。

## 产物

- 本报告；
- 摘要：`docs/reports/2026-11-16-tb2-v0520-baseline-summary.json`、`…-scale-2x-summary.json`、
  `…-scale-4x-summary.json`、`…-dna-probe-summary.json`；
- 原始运行（不入库）：`jobs/full-87-v0520/…`、`jobs/scale-2x-v0520/…`、`jobs/scale-4x-v0520/…`、
  `jobs/probe-dna-v0520/…`；
- Issue：[#177](https://github.com/echoVic/orca-agent/issues/177)（A 轴结果与建议）、
  [#154](https://github.com/echoVic/orca-agent/issues/154)（上一轮，已关闭）。

## 边界

- 每档 k=1（dna 复测 k=2）；capability 判定为「4× 仍失败」，未对 capability 任务做 k=3；
- 数据集 ref 与 v0.5.14 相同 `sha256:c6fc…`，排除两个 quarantine 任务；`qemu-startup` 与
  `qemu-alpine-ssh` 的镜像缺陷不在能力口径内；
- 2×/4× 只放大 timeout（`--timeout-multiplier`），未改变模型、并发或验证器；
- 1× 的 setup/verifier 超时计入 environment/time，而不是 capability；若把 2× 的 setup 放大视为
  「环境修复」，则真实能力缺口仍是 4 题。
