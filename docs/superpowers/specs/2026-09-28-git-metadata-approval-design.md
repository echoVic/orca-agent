# auto-edit / suggest 下 git 写操作的单条授权

> 状态：设计已确认（2026-09-28），待写实现计划。基线 `e1a8dfef`（v0.5.3）。
> 目标：在 auto-edit 和 suggest 下，一次 git 写操作的代价是"一次审批"，而不是撞上沙箱的死路；不依赖模型主动申请，不解析命令输出，授权只对被批准的那一条命令生效。

## 0. 背景

auto-edit 的设计是"拿沙箱换免审批"：bash 不逐条询问，直接在 workspace-write 沙箱里运行，工作区可写，但 `.git`、`.agents`、`.codex` 只读、网络关闭（`crates/orca-approval/src/policy.rs:82`，`crates/orca-tools/src/sandbox/mod.rs:19`）。保护 `.git` 是必要的：能写 `.git/config`（`core.fsmonitor`、`core.hooksPath` 等）或 `.git/hooks` 的代码，可以埋下在用户之后于沙箱外运行 git 时执行的命令。

问题在于被挡住之后没有顺畅的放行路径：

- 运行时发起的"沙箱拒绝 → 询问用户 → 重试"流程在 `1fdd88a0`（2026-08-30）被有意关闭（`server.rs` 中 4 个测试标记为 `legacy permission retries require a structured backend denial receipt`）。原因是它依据命令自己的输出（`Operation not permitted`）判断是否为沙箱拒绝，命令可以伪造这段输出，借运行时之口申请更大的权限。替代它的结构化拒绝凭据（`ExecutionBroker::denial_receipt`）目前没有生产代码产出。
- 剩下的路径是模型主动调用 `request_permissions`。DeepSeek 在实际会话中没有这样做（2026-09-28 的 boss-skill 会话里，`git mv` 被拒后模型改用了 `mv`），而 `sandbox_diagnostic` 也没有点名这个工具。

对比：Claude Code 的 acceptEdits 下 bash 仍逐条询问、批准后不进沙箱；Codex 的默认 Auto 模式同样在沙箱里免审批运行、`.git` 只读，但模型会带 `with_escalated_permissions` 申请单条命令在沙箱外运行。两者都把"撞墙"变成了"一次审批"。

## 1. 用户可见行为

| 模式 | 识别为 git 写命令时 |
|---|---|
| auto-edit | 运行前弹权限窗：允许这一次 / 本会话允许 git 写命令 / 拒绝 |
| suggest | 原有的命令审批弹窗多一行"这条命令会写 `<repo>/.git`（config 和 hooks 仍只读）"，批准即授权，不弹第二次 |
| plan | 不变（只读沙箱） |
| full-auto | 不变（不进沙箱） |

- 允许这一次：只有这一次运行时 `.git` 可写，下一条命令恢复只读。
- 本会话允许 git 写命令：同上，并且本会话后续识别出的 git 写命令不再弹窗；每条仍然只对自己生效。
- 拒绝：命令不运行，模型收到 `denied`（"用户拒绝了这条 git 命令写 `.git`"），不会换着法子重试。
- 在 suggest 下，如果命令被权限规则自动放行、没经过审批弹窗，则按 auto-edit 的方式询问。原则：`.git` 写权限只来自"明确提到 `.git` 的那次审批"或本会话开关，普通的命令放行规则不算。
- 没有权限处理器的场景（headless `orca exec`、未接审批的 JSONL 客户端）不弹窗，行为与现在相同，依靠第 5 节的兜底提示。
- Windows 在 v1 中没有快速审批，只有兜底提示（见第 4 节）。

## 2. git 写命令识别

新增纯逻辑模块（`crates/orca-runtime/src/git_write_command.rs`），输入 bash 命令字符串，输出四种判定之一：

| 判定 | 例子 | 处理 |
|---|---|---|
| 需要写 `.git` | `commit` `add` `rm` `mv` `checkout` `switch` `restore` `reset` `merge` `rebase` `cherry-pick` `revert` `stash`（list/show 除外）`branch`/`tag`（创建、删除，列表不算）`am` `apply --index`/`--cached`/`--3way` `gc` `update-index` `update-ref` `bisect` | 走第 3 节的审批 |
| 只读 | `status` `log` `diff` `show` `blame` `grep` `rev-parse` `describe` `ls-files` `config --get`/`--list` `remote -v`、branch/tag 的列表形式 | 照常运行 |
| 拒绝快速审批 | `config` 和 `remote` 的写形式、`submodule` `worktree` `hook` `filter-branch` `sparse-checkout` `init`；带 `-c` `--config-env` `--exec-path` `--git-dir` `--work-tree` 的任何调用；任何 `GIT_*=` 环境变量赋值 | 照常运行（会被沙箱挡住，由兜底提示说明） |
| 无法识别 | 含 `$(…)`、反引号、`eval`、heredoc、`sh -c "git …"` | 照常运行，不猜 |

解析规则：

- 尊重单引号、双引号和反斜杠转义。
- 在未加引号的 `;` `&&` `||` `|` `&`、换行和括号处拆成简单命令。复合命令只要有一段需要写 `.git`、且没有任何一段被拒绝或无法识别，才触发审批。
- 跳过开头的 `NAME=value` 赋值，以及 `command`、`env`、`time`、`nice`、`nohup` 包装；`argv[0]` 必须是 `git` 或以 `/git` 结尾的路径。
- 全局选项中 `--no-pager`、`-p`/`--paginate`、`--no-optional-locks`、`--literal-pathspecs` 可以忽略；`git -C <dir>` 只在目录仍位于工作区内时允许。
- `fetch`、`pull`、`push` 不在 v1 范围：auto-edit 下网络完全关闭，只放开 `.git` 它们依然失败，批准也没有意义。网络授权另议。

## 3. 审批与授权流程

触发条件（同时满足）：

1. 这次 bash 会在 workspace-write 沙箱中运行（`bash_sandbox_for_cwd` 的结果是 `WorkspaceWrite`：auto-edit 或 suggest，且目录已 trust）。
2. 命令的工作目录就是仓库根：`<工作目录>/.git` 是真实目录（不是文件、不是符号链接；复用 `is_safe_metadata_writable_root`）。沙箱保护的正是这个路径；bash 通过 `workdir` 指定子目录时，可写根只有该子目录，仓库根的 `.git` 不在可写范围内，属于第 7 节不覆盖的情况。
3. 第 2 节判定为"需要写 `.git`"。

auto-edit：在 `execute_bash`（`crates/orca-runtime/src/runtime_normal_tool.rs`）调用 `service.exec` 之前，按以下顺序处理：

- 本会话开关已打开：直接授予单条授权，不询问。
- 没有权限处理器：不询问、不授权，照常运行。
- 否则构造一个 `RuntimePermissionRequest`（原因写明命令、目录、"只对这一条命令生效，config 和 hooks 仍只读"），直接调用处理器。**不经过** `PermissionRuntimeState::request_permission`，因为后者会把结果合并进本轮的权限叠加层，变成整轮授权。

suggest：审批闸门（`crates/orca-runtime/src/tool_execution.rs` 中的 `handle_approval`）为识别出的 git 写命令在审批描述中加上 `.git` 说明；审批通过后，这次调用携带"已获得提到 `.git` 的审批"的标记，`execute_bash` 据此直接授予单条授权，不再询问。

"本会话"的语义：

- 现有权限窗的 Session 范围会被持久化为会话级的文件写授权。这一步发生在 actor 处理交互回复时（`thread_actor_interaction.rs` 中 `scope == Session` 的分支），不在调用方，所以只绕开 `PermissionRuntimeState` 不够：请求要带一个独立的种类（如 `RuntimePermissionRequestKind::GitMetadataWrite`），actor 见到这个种类时跳过持久化，调用方把 Session 回复解释为"打开开关"。否则 `.git` 会变回对所有命令开放。这里的"本会话"只记录一个"git 写命令免询问"的开关，不记录任何目录授权。
- v1 中开关只保存在运行时内存里：进程重启或恢复会话后，第一次 git 写命令会重新询问。
- 子 agent 的 bash 走现有的子 agent 权限转发，开关不在 agent 之间共享。

## 4. 沙箱侧

- **一次性授权**：给 `TerminalExecRequest` 增加一个可选的单条授权字段，经 `prepare_shell_command`、`ShellSessionCommand` 传到沙箱构建，只影响这一次执行，不写入 `TurnPermissionOverlay`。
- **授权内容**：`<repo>/.git` 可写，但 `config`、`config.worktree`、`hooks/`、`modules/` 保持只读。commit、add、merge、rebase、stash 等只写 index、objects、refs、logs、HEAD 一类文件，用不到这几项；而让代码在沙箱外执行的入口（hooks、`core.fsmonitor`/`core.hooksPath`/`core.pager`、alias、filter 驱动）都在 config 和 hooks 里，`modules/` 里是各子模块自己的 config 和 hooks。
- **代价**：会写 config 的 git 操作会失败，例如 `git branch --set-upstream-to`、`checkout --track`。它们走兜底提示；确实需要时用 `request_permissions` 或 full-auto。
- **平台**：
  - macOS（Seatbelt）：在放开 `.git` 的规则之后追加这几个路径的 `deny file-write*` 规则，与现有保护 `.git` 的写法一致（后出现的规则优先）。
  - Linux（bwrap）：用现有的只读 bind 把这几个路径重新挂成只读。只读路径嵌套在可写路径中本来就需要 bwrap（`policy_requires_bwrap`），没有 bwrap 时拒绝运行而不是降级。
  - Windows：适配器把 `.git` 整块列为拒绝目录（`crates/orca-windows-sandbox/src/policy.rs:42`），无法表达"部分放开"，v1 不启用快速审批。
- 现有授权方式（`request_permissions`、权限 profile）语义不变，仍是整块放开 `.git`。

## 5. 兜底提示与错误处理

- `sandbox_diagnostic`（`crates/orca-runtime/src/sandbox_denial.rs`）：当被拒路径位于工作区的受保护元数据目录（`.git`、`.agents`、`.codex`）内时，在提示中写出下一步——"如需放行，调用 `request_permissions`，`fileSystem.write: ["<repo>/.git"]`"，点名工具和确切目录。仍标注 non-authoritative，不授予任何权限。覆盖识别器漏掉的命令、被拒绝快速审批的命令和 Windows。
- 权限请求本身出错（如 TUI 断开）：视为未批准，不运行，返回启动前失败并带上错误。
- 用户拒绝：`denied`，`invocation_started = No`。
- 弹窗期间被取消：启动前取消。
- 识别器不得 panic；无法处理的输入一律归为"无法识别"。

## 6. 测试

- **识别器**：表驱动单测覆盖四种判定、复合命令、引号、环境变量、`-C`/`-c`、包装命令，以及各子命令的读写形式。
- **沙箱 profile**：单条授权时 `.git` 可写且 `config`/`hooks`/`modules`/`config.worktree` 的限制存在；macOS 上 Seatbelt 可用时做真实执行测试：临时仓库中 `git commit` 成功，`printf x > .git/hooks/pre-commit` 与 `git config core.fsmonitor x` 失败。
- **端到端**（与 v0.5.3 修复相同的 TUI surface 测试方式，mock 的 bash 调用使用 DeepSeek 形状：action 为 read、无 target）：
  - auto-edit：git commit → 弹权限窗 → 允许一次 → 提交成功；随后一条写 `.git` 的普通命令仍然失败（证明是单条授权）。
  - 拒绝 → 工具返回 denied，没有提交。
  - 本会话允许 → 第二条 git 写命令不弹窗并成功。
  - suggest：审批弹窗提到 `.git`，批准后提交成功，只弹一次。
- **兜底提示**：更新 `sandbox_denial` 单测，断言提示包含 `request_permissions` 和确切目录。

## 7. 不覆盖的情况与已知风险

- git 目录在工作区外：`git worktree` 的链接工作树（`.git` 是指向主仓库的文件）、工作区只是更大仓库的子目录。只有兜底提示。
- bash 通过 `workdir` 在子目录中运行 git 写命令：可写根只有该子目录，不触发审批，只有兜底提示。
- `fetch`、`pull`、`push`：需要网络授权，另议。
- 会写 config 的 git 操作：见第 4 节。
- Windows：只有兜底提示。
- 会话开关不跨进程重启、不跨会话恢复。
- **husky 等工具的 hook 脚本位于工作区内**（`core.hooksPath` 指向 `.husky/`），agent 本来就能修改，用户之后在沙箱外手动 `git commit` 时会执行它们。这与 `.git` 保护无关，本设计不解决；Codex 有同样的问题。
