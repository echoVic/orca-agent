# Orca `/rewind`：文件检查点与对话回退设计

> 状态：Proposed（未实现）
> 日期：2026-11-07
> 前置：Orca 2026-11 功能缺口盘点（P0 第 1 项；该盘点位于工作区的 `reports/orca-feature-gaps-2026-11.md`，不在本仓库）
> 参考：Claude Code 2026-03-31 npm sourcemap 快照（`~/Documents/GitHub/claude-code`，非官方源码；仅作设计参考）、Codex、DSH
> 目标版本：v0.6.x（M 级工作量）

## 0. 结论

1. **存储模型抄 CC 的分层**：元数据进 transcript JSONL（`session.snapshot`），文件内容进 `$ORCA_HOME/rewind/sessions/<session_id>/objects/<sha256>` 内容寻址 blob。JSONL 只增不重写，恢复不依赖 git。
2. **捕获模型抄 CC 的「每 turn 首触记前像」**：每个用户消息（turn）在写工具执行前记录一次该文件的前像（`Option<pre-image>`，`null` 表示当时文件不存在），同一 turn 同文件只记一次；turn 末用已有的有界 workspace 观察补记「无法归因的改动」（bash/外部进程）。
3. **恢复模型抄 CC 的「撤销日志」而非全量工作区快照**：恢复目标 = 某条用户消息提交时刻；只需回放「该消息之后所有被触碰文件的前像」，未碰过的文件一律不动。这同时解决文件创建/删除的往返（tombstone）。
4. **对话回退不重写日志**：新增 `session.rewind` 记录，载入时按记录折叠（fold）截断消息、重建 summary/compaction 状态。它与 CC 的 parentUuid 链目标一致（append-only + 有效头），但适配 Orca 的线性 transcript；`--resume-at` 继续作为无状态边界读取使用。
5. **5 种出口按代码证据实现 6 个模式**：只恢复代码 / 只回退对话 / 两者 / 总结到此处（summarize up_to）/ 总结从此处（summarize from）/ 取消。注意：CC 快照里 `Summarize up to here` 被 `"external" === 'ant'` 门控（死代码），见 §2.5。
6. **不做**：全工作区内容快照、git 集成、bash 写前拦截、跨 session/跨 worktree 文件迁移、全局跨 session blob 去重。这些要么代价过高，要么与 Orca 的隔离模型冲突，见 §4。

---

## 1. 背景

`docs/harness-contract.md:293-295` 目前的承诺是：

> The checkpoint is audit data — execution always resumes from the transcript's committed messages, and uncommitted side effects are never claimed as exactly-once (restore repairs them as indeterminate). **File rewind is not promised**: Orca does not snapshot external workspace state.

现实后果：agent 用 `edit`/`write_file` 改坏一批文件后，用户只能靠 git（可能脏工作区、可能没有 commit）、或手工撤销。`session.checkpoint` 只在预算停止时写（`operation_context.rs:459-504`），且 reader 直接忽略（`writer.rs:802`），不能还原任何文件。`--resume-at`（`writer.rs:669-704`）只回对话、不碰文件。

这是「敢让 agent 全自动跑」的信任前提，也是本次 P0 第一项。

---

## 2. 参考实现证据（Claude Code 快照）

> 该仓库是 2026-03-31 从 npm sourcemap 泄露后构建修复的 TS 快照，之后只有 ByteDance 的构建修复提交。它是很好的设计参考，但不能当作最新 CC 行为。以下行号均在该快照内核对过。

### 2.1 数据模型与落盘

- `src/utils/fileHistory.ts:31-52`：
  - `FileHistoryBackup = { backupFileName: string | null, version: number, backupTime: Date }`；`null` 是 **tombstone**，表示「该版本时文件不存在」。
  - `FileHistorySnapshot = { messageId: UUID, trackedFileBackups: Record<path, FileHistoryBackup>, timestamp }`。
  - 内存里**只有文件名，没有文件内容**。
- blob 落盘：`{claudeConfigDir}/file-history/{sessionId}/{sha256(path)[:16]}@v{n}`（`fileHistory.ts:725-741`），整文件 `copyFile` + `chmod` 保留权限（`:748-798`），二进制天然支持，**没有单文件大小上限**。每次版本是完整拷贝，不是 delta。
- 会话 JSONL 里只追加元数据：`{type:'file-history-snapshot', messageId, snapshot, isSnapshotUpdate}`（`types/logs.ts:188-193`、`sessionStorage.ts:1085-1099`）。重启后重建（`fileHistory.ts:888-917`）；跨 session resume 时把 blob 复制/硬链到新 session 目录（`:922-1046`）。
- `MAX_SNAPSHOTS = 100`（`fileHistory.ts:54`），环形丢弃旧快照（`:309-311`）。

### 2.2 捕获时机

- **每个用户消息一次**：prompt submit 时、query 真正运行前（`handlePromptSubmit.ts:525-539`、`QueryEngine.ts:641-655`，SDK 路径需 `persistSession`）。
- **每个写工具执行前**：`fileHistoryTrackEdit` 在 `FileWriteTool.ts:255-264`、`FileEditTool.ts:431-440`、`NotebookEditTool.ts:311-317`、`BashTool.tsx:392-394`（仅模拟 sed 的编辑路径）被调用。
- 去重：若该文件已在最近快照里被 tracking，直接复用、不写新 blob（`fileHistory.ts:114-118`），并**回溯插入当前消息快照**（`:132-158`），这样 turn 中第一次编辑的前像会记在当前消息上。
- `fileHistoryMakeSnapshot` 对已跟踪文件重新 stat，只有 mtime/size/mode/content 变化才生成新版本，否则复用上一版本；ENOENT 记 `backupFileName: null`（`:241-275`）。

### 2.3 恢复算法

- `fileHistoryRewind`（`:347-397`）→ `applySnapshot`（`:537-591`）：对目标快照里记录的每个文件，取对应 backup；`null` → `unlink`（容忍 ENOENT），否则 copyFile + chmod 写回。
- 恢复前做 `checkOriginFileChanged`（`:600-672`）：先 stat mode+size，再用 mtime 短路，最后比内容。blob 缺失只记日志、静默跳过，**从不抛错**。
- 路径校验：**没有**。任何记录下来的绝对路径都会原样写回（无 cwd containment、无 symlink/realpath 检查）。这是我们要明确改掉的地方。
- 已知限制（唯一的用户可见警告）：`MessageSelector.tsx:345-350` 提示「Rewinding does not affect files edited manually or via bash」。

### 2.4 对话回退

- `rewindConversationTo`（`REPL.tsx:3661-3707`）是**内存截断**，不是重写日志：`setMessages(prev.slice(0, messageIndex))`，再 bump `conversationId` 让行重新挂载，重置 microcompact/context-collapse 状态，恢复目标消息的 permissionMode。
- JSONL 是 append-only：被回退的分支留在磁盘上，载入时由 `buildConversationChain` 从最新 leaf 沿 `parentUuid` 回溯，自动丢弃孤儿（`sessionStorage.ts:3226-3239`）；`useLogMessages.ts:48-57` 把「同头缩短」当作正常情况，不再二次截断。
- 恢复后 `restoreMessageSync`（`REPL.tsx:3712-3739`）把目标用户消息的原文 + 粘贴图片**填回输入框**，等用户重新发送。
- 选到已被压缩/snip 掉的消息时明确 no-op 并提示（`REPL.tsx:4923-4930`）。
- 结论：CC 的 resume 游标是 parentUuid 链；对话框只是「有效视图」。**对话截断与文件恢复互相独立**，`/rewind` 只是编排两者。

### 2.5 选择器与 5 出口的真实状态

- `/rewind`（别名 `/checkpoint`）本身是纯 UI 命令，`/rewind/index.ts:6`、`rewind/rewind.ts:4-13`：打开 `MessageSelector` 后返回 `{type:'skip'}`，不写 transcript；headless 走 SDK 控制请求 `rewind_files`（`cli/print.ts:4525-4566`、`controlSchemas.ts:308-327`）。
- 选择器是**两步**（`MessageSelector.tsx`）：先选消息，再在确认页选动作。
  - 行 = 用户消息（过滤 tool_result/synthetic/isMeta/compact summary 等，`:767-792`），另加一行合成的 `(current)`（`:59-67`）。
  - 每行三行高：截断的 prompt 一行 + 每消息 diff 统计「N files changed +X -Y / No code changes / ⚠ No code restore」（`:359-390`）。行内统计来自 transcript 重放 tool result 的 structuredPatch（`:722-766`），确认页数字来自快照（权威）。
  - 可视窗口 `MAX_VISIBLE_MESSAGES = 7`（`:45`），键盘 up/down/k/j/ctrl+p/ctrl+n/enter（`:275-284`）；Esc 在确认页回到列表（`:269-272`）。
- 确认页选项（`:93-134`）：
  1. Restore code and conversation
  2. Restore conversation
  3. Restore code
  4. Summarize from here
  5. Never mind
  6. **Summarize up to here —— 在 `:121` 被 `if ("external" === 'ant')` 门控，此快照中不可达**（死代码）。
- 动作映射（`:177-246`）：both = 先 code 后 conversation；code = 只 `fileHistoryRewind`；conversation = 只 `rewindConversationTo`；summarize = `partialCompactConversation(direction)`，**从不碰文件**。
- summarize 语义（`compact.ts:772-808` + `REPL.tsx:4943-4965`）：
  - `direction='from'`：summarize `messages[pivot..]`，保留 `[0..pivot)`，结果 = kept + summary；并 prefill 输入框（可从 pivot 重新发）。
  - `direction='up_to'`：summarize `messages[0..pivot)`，保留 `[pivot..]`（并剥掉旧 compact boundary/summary），结果 = summary + kept。
- 门控：`fileHistoryEnabled()`（`fileHistory.ts:63-78`）= 全局配置 `fileCheckpointingEnabled !== false`（默认 true）且未设 `CLAUDE_CODE_DISABLE_FILE_CHECKPOINTING`；非交互 session 需 opt-in。`/rewind` 本身无远端 feature gate。

### 2.6 可抄 / 不抄

| CC 做法 | 我们 | 原因 |
|---|---|---|
| 元数据在 JSONL、内容在磁盘 CAS | 抄 | transcript 小、resume 快、审计可见 |
| 每 turn 首触记前像 + tombstone | 抄 | 撤销日志最小、创建/删除往返自然 |
| 版本整文件拷贝、保留 mode | 抄（+zstd） | 简单、二进制安全；压缩是纯收益 |
| turn 末不校验、bash 改动不还原 | 抄语义 + 补告警 | Orca 已有有界 hash 观察，可诚实列出「不可还原」路径 |
| 恢复无路径校验 | 不抄 | 安全边界必须收紧 |
| 100 快照环形 + 按 mtime 剪 blob | 抄上限，改为 mark-sweep | 避免误删仍被最新快照引用的 blob |
| append-only + 有效视图截断 | 抄目标，换实现 | Orca 无 parentUuid；用 `session.rewind` fold |
| 恢复后 prefill 输入框 | 抄 | 体验一致，用户可重发 |
| 不碰 git | 抄 | 工作区可能脏/无仓库；git 集成是独立项 |

---

## 3. Orca 现状（缺口事实）

### 3.1 已有的

- `session.checkpoint`（`thread_store/types.rs:280-303`）：字段 `session_id/status/reason/budget_consumed/last_committed_message_id/resumable/task_plan/recorded_at`；唯一写入点 `operation_context.rs:481-494`（**仅 budget_exhausted**）；reader 忽略它（`writer.rs:802`）。写入用非 fsync 的 `write_record`。
- 会话持久化：JSONL，`$ORCA_HOME/sessions/<YYYY>/<MM>/<DD>/session-<ts>-<session_id>.jsonl`（`history.rs:453-471`）；`SessionRecord` 是 `type` 标签枚举（`types.rs:192-231`）；每 transcript 一把 `<path>.lock`（`writer.rs:1206-1216`）；`write_record` 不 fsync（`writer.rs:171-196`），`write_durable_record` 才 `sync_data`（`:198-216`）。
- 已有 CAS 实现：transcript 图片 assets，`{transcript}.jsonl.assets/<sha256>`，`put_blob` no-clobber + 文件/目录 fsync + 摘要校验（`thread_store/assets.rs:13-130`）。**可复用其实现模式，但不能复用目录**（assets 绑定单条记录，随 transcript 压缩/归档迁移）。
- 有界工作区哈希观察：`workspace_observation.rs:26 / :102`，SHA-256、2 MiB/file、32 MiB total、8192 entries、250 ms、排除 `.git/target/node_modules/.orca`、单进程同时只允许一个 observer（`:7-11`、`:50-100`）。**只出 hash，不出内容**。
- 变更溯源：`tool_execution.rs:1383-1391` 只对成功 `Edit|WriteFile` 计 `applied_file_writes`；bash 变更不可见。工具直接 `std::fs::write`（`write_file.rs:25`、`edit.rs:34`），**没有中央写 choke point**。
- 预算/压缩：`budget_controller.rs`、`operation_context.rs`；`SessionRecord::ManualCompactionSnapshot` 在载入时**直接重置消息列表**（`writer.rs:754-768`）——这是「记录类型可在 fold 中改变状态」的既有先例。
- worktree：只用于子代理隔离，`<repo>/.orca/worktrees/subagent-<uuid>`，`git worktree add --detach`，干净才删除（`worktree.rs:22-62`）；沙箱不做路径映射（`orca-tools/src/sandbox/mod.rs:95-102`）。
- Esc 空输入回退：`Conversation::backtrack_last_user`（`conversation.rs:737-750`）→ 内存截断 + TUI `remove_after_last_user`（`types.rs:2026`）；**不写任何持久记录**（`hosted_context.rs:95-110`）。推断：resume 时 transcript 仍含被回退的消息，它们会重新回到上下文（代码中未见持久化截断）。

### 3.2 缺的

1. 文件内容快照、恢复/撤销 API、任何 rewind 概念。
2. `session.checkpoint` 之外的 per-turn 检查点；checkpoint 不带文件状态。
3. 对话回退的持久语义（见 3.1 最后一条）。
4. headless 侧没有「回到某个 item」的文件操作（`--resume-at` 只回对话）。
5. 事件/协议无 `session.rewind`；TUI 无选择器；slash 目录无 `/rewind`（`orca-tui/src/commands/mod.rs:2-47`）。

### 3.3 由此得到的两个设计约束

- **必须先有写工具拦截点**，否则捕获不到前像。Orca 的写工具在 runtime 分发（`tool_execution.rs:559 execute_tool_with_approval`、`:980 execute`），不在 `orca-tools` 内部；拦截点应放在 runtime 的 write-class 工具分发前，而不是改每个工具。
- **对话回退必须可持久**，而 CC 的 parentUuid 链在 Orca 不存在；折中是「append-only + `session.rewind` 记录 + 载入 fold」，与 `ManualCompactionSnapshot` 的既有做法同构。

---

## 4. 目标与非目标

### 目标

1. `/rewind` 在 TUI 里选一个历史用户消息，可恢复代码、回退对话、两者、或双向总结。
2. 文件恢复与 git 无关；不破坏未跟踪/用户手工创建的文件；创建/删除可往返。
3. 恢复是**用户显式操作**，有 dry-run、预览、失败可见、部分失败可报告。
4. 对话回退持久：重启/resume 后不会复活已回退消息，且能继续对话。
5. headless/CI 可用：`orca rewind --dry-run --json`，事件进 JSONL。
6. 与现有 `session.checkpoint`、budget、worktree、compaction、resume 语义不冲突，旧 transcript 完全兼容。

### 非目标（本期）

- 全工作区逐 turn 内容快照（成本 32 MiB × N，且大部分文件没变）。
- bash/外部进程写的前像捕获（需要 bash 前全量拷贝或 overlayfs/COW；先诚实报告「不可还原」）。
- git 集成（checkout/reset/stash）、跨 worktree 迁移文件、把子代理 worktree 的改动合回父 session。
- 跨 session 全局 blob 去重（先 per-session，GC 简单）。
- 云端/多机同步、加密存储。
- Esc 空输入回退的语义改造（见 §16 Q4）。

---

## 5. 总体架构

```
                      ┌───────────────────────────────┐
                      │            TUI / CLI          │
                      │  /rewind  ->  RewindState     │
                      │  orca rewind --dry-run        │
                      └──────────────┬────────────────┘
                                     │ UserAction::RewindToCheckpoint
                                     ▼
┌────────────────────────────────────────────────────────────────────┐
│ orca-runtime                                                        │
│  ┌──────────────┐   ┌──────────────────┐   ┌────────────────────┐  │
│  │ TurnCapture  │   │ RewindCoordinator│   │ Transcript fold    │  │
│  │ (per turn)   │   │ (plan/apply)     │   │ (session.rewind)   │  │
│  └──────┬───────┘   └────────┬─────────┘   └─────────┬──────────┘  │
│         │ pre-image         │ plan/apply            │ load/replay  │
│         ▼                   ▼                       ▼              │
│  ┌──────────────────────────────────────────────────────────────┐  │
│  │ orca-rewind (new crate): RewindStore, Planner, Applier,     │  │
│  │ Journal                                                     │  │
│  └───────────────┬──────────────────────────────┬───────────────┘  │
└──────────────────┼──────────────────────────────┼──────────────────┘
                   │ CAS blobs                     │ SessionRecord append
                   ▼                               ▼
     $ORCA_HOME/rewind/sessions/<sid>/objects   $ORCA_HOME/sessions/.../*.jsonl
```

职责划分：

- **orca-core**：共享类型 `RewindMode`、`RewindReport`、`RewindTarget`；沿用 `ConversationItemId`（`thread_identity.rs:9`）。
- **orca-rewind**（新 crate，无 runtime 依赖）：`RewindStore`（CAS：put/read/verify/GC）、`Planner`（撤销日志 → 计划）、`Applier`（两阶段、原子写、journal）、路径安全工具。
- **orca-runtime**：`TurnCapture`（turn 生命周期 + 写工具拦截）、`RewindCoordinator`（目标校验、调用 planner/applier、写 `session.rewind`/`session.snapshot` 记录、发事件）、transcript fold（载入时应用 rewind 标记）。
- **orca-tui**：`/rewind` 命令、`RewindState` 两阶段选择器、确认页、`RewindCompleted` reducer。
- **orca-core/event_schema**：`session.rewind` 事件类型 + 各投影。

---

## 6. 快照存储格式

### 6.1 目录布局

```
$ORCA_HOME/rewind/
└── sessions/
    └── <session_id>/
        ├── objects/
        │   └── <2-hex>/<sha256>          # 内容寻址 blob（zstd 或 raw）
        ├── journals/
        │   └── <rewind_op_id>.jsonl      # 两阶段恢复日志
        └── pre/                          # 恢复前备份（仅被覆盖文件，用于失败回滚）
            └── <2-hex>/<sha256>
```

- **为什么不复用 `{transcript}.jsonl.assets`**：assets 由单条 `session.record_with_assets` 行 envelope 引用，随 transcript 压缩（`.jsonl.zst`）与 archive 一起迁移/清理；rewind blob 需要独立保留策略、独立 `/storage` 统计与清理，且被多条快照记录共享。复用 `assets.rs` 的 **实现模式**（no-clobber publish、`sync_all`、digest 校验、64 MiB 上限），目录独立。
- 路径 key 一律 **workspace 相对路径**（`/` 分隔，UTF-8；非 UTF-8 路径记录 `limitation: non_utf8_path` 并跳过）。绝对路径只出现在快照记录的 `root` 字段。
- blob 文件名 = `sha256(content)`，天然去重；跨快照同内容不重复落盘。

### 6.2 `session.snapshot` 记录

每个 turn 结束追加一条（仅当该 turn 触碰过文件或检测到未归因改动；纯对话 turn 不写，节省 JSONL 体积——选择器行由 transcript 消息生成，快照按 `at_message_id` join）。

```jsonc
{"type":"session.snapshot","schema_version":1,
 "snapshot_id":"snap_019f...","session_id":"...",
 "at_message_id":"item_019f...",          // 该 turn 的用户消息 item id（ConversationItemId）
 "turn_id":"turn_...",
 "ordinal":12,                            // 该 turn 在会话中的用户消息序号（0 起、单调；不是快照计数）
 "root":"/Users/x/repo",                  // 规范化后的工作区根
 "workspace_fingerprint":"dev:16777234,ino:98765",  // WorkspaceIdentity 风格，防同路径替换
 "complete":true,
 "start_digest":"sha256:...",             // 有界观察 digest（可选；关闭观察时缺省）
 "end_digest":"sha256:...",
 "files":{                                // 本 turn 触碰过的文件：pre = 写前状态
   "src/a.rs":{"pre":{"sha256":"ab12...","size":812,"mode":420,"encoding":"zstd"},
                 "post_sha256":"cd34..."}, // 最后一次写成功后的内容摘要（可选）
   "src/new.rs":{"pre":null,               // tombstone：写前不存在
                 "post_sha256":"ef56..."}
 },
 "change_stats":{"files":2,"insertions":41,"deletions":7,"omitted":false},
 "untracked":["docs/x.md"],               // digest 检出但无 pre-image（bash/外部写）
 "limitations":["observation_budget"],    // 或 outside_root / too_large / symlink / not_regular / non_utf8_path
 "recorded_at":"2026-11-07T10:00:00Z"}
```

设计要点：

- **`pre` 是「写前状态」，`post_sha256` 是「写后摘要」**：前者用于回放撤销，后者用于判断「当前磁盘是否仍是 agent 最后写的状态」，从而在恢复前诚实报告 `untracked_change`。
- **同 turn 同文件只记一次 `pre`**（首次触碰时记录）。后续同 turn 再写只更新 `post_sha256`。跨 turn 每次触碰都记新的 `pre`（即上一 turn 的结束状态）。
- `at_message_id` 是 join 键：选择器行 = transcript 里的用户消息；`snapshot_by_message` 缺省 = 该 turn 无文件变化或快照被淘汰。
- `complete:false` 表示观察不完整（worker busy / budget / 平台不支持）；`untracked` 只用于提示，绝不用于恢复。
- `change_stats` 来自本 turn 工具结果里的 `FileChangePreview` 统计（`tool_types.rs:595-604`、`file_admission.rs:356`）；二进制/超限时 `omitted:true`。确认页的 +X/-Y 由 planner 用 pre-image 现算（权威），行内统计用它。

### 6.3 blob 编码与上限

| 项 | 值 | 理由 |
|---|---|---|
| 编码 | ≤4 KiB raw；>4 KiB 且 UTF-8 文本 → zstd(level 3)；二进制 raw | 文本压缩比高；二进制不浪费 CPU |
| 单文件上限 | 16 MiB（等于 `MAX_EDIT_FILE_BYTES`，`file_admission.rs:13`） | 超过的编辑本来就被工具拒绝；更大文件记 `too_large` 跳过 |
| 单 turn 捕获上限 | 64 MiB / 512 个文件 | 防病态脚本把 ORCA_HOME 写爆 |
| 单 session 保留 | 最近 100 个快照（可配 `[rewind] max_checkpoints`） | 与 CC 一致 |
| 单 session blob 上限 | 512 MiB（默认，可配）；全局 2 GiB | mark-sweep 到上限 |
| 权限 | unix mode 保留；Windows 只读属性尽量保留（无法保留 ACL） | 恢复后可直接运行 |

- 二进制安全：按字节存/还原，不做 UTF-8 假设（`read_text_file_with_limit` 只用于 diff 预览）。
- 符号链接/特殊文件：不跟随、不捕获，记 `symlink`/`not_regular` limitation；恢复时不覆盖 symlink。
- 排除目录同观察器：`.git`、`target`、`node_modules`、`.orca`（`workspace_observation.rs:11`）。这保证不读 `.git`（避免把 hooks/config 落盘）也避免恢复污染 git 元数据。

### 6.4 去重、GC 与保留

- **去重**：blob 按内容 hash；`put` 已存在则跳过（复用 no-clobber 语义）。
- **保留**：每 session 保留最近 `max_checkpoints` 条 `session.snapshot` 记录（按 turn `ordinal` 排序取最后 N 条）。更早的记录仍留在 JSONL（历史审计），但标记为「已淘汰」：用一条 `session.snapshot_retention {evicted_through_ordinal:N}` 水位记录表达；restore 遇到淘汰快照返回 `snapshot_evicted`，TUI 行显示 `⚠ 不可还原`，但「只回退对话」仍可用。
- **GC**：mark-sweep。扫描 transcript 中未淘汰快照记录引用的 hash 集合，删除 `objects/` 下不在集合中的 blob；执行时机 = session 打开时低频、`/storage`、显式 `orca storage prune`。默认只清理「最后使用时间超过 7 天」的孤儿，避免刚写完还没 append 记录的窗口。
- 与 `orca storage`（`src/cli.rs:73`、`StorageArgs`）集成：`orca storage` 报告 rewind 占用；`orca storage prune --rewind` 立即 GC；删除 session/archive 时同步清理 `rewind/sessions/<id>`。
- 恢复失败回滚用的 `pre/` 目录随 journal 一起在成功后删除；失败则保留并在下次启动提示「可回滚」。

---

## 7. 捕获时机

### 7.1 turn 生命周期

| 时机 | 动作 | 失败策略 |
|---|---|---|
| turn 开始（`session.rs:705 begin_runtime_turn`，TurnStart） | 建 `TurnCapture { at_message_id, turn_id, root, start_digest }`；`start_digest` = 有界观察（若开启） | 观察失败记 limitation，不阻塞 turn |
| 写工具执行前（`tool_execution.rs:559/980` 分发前） | 若工具 spec 是 write-class 且能解析出 root 内路径 → `capture_pre_image(path)` | 读失败/超限 → 记 limitation 并继续工具执行（工具本身会报错或成功） |
| 写工具成功后 | 更新该文件 `post_sha256`；更新 change_stats | —— |
| turn 结束（正常/取消/失败/预算停止，terminal 之前） | `end_digest` → diff → `untracked`；append `session.snapshot`（`write_durable_record`，fsync）；预算停止时再 append `session.checkpoint`（带 `snapshot_id`）→ journal/terminal | 快照写失败：**不阻塞 turn**，但发 `TuiEvent::Error`（提示「本次检查点未保存」），并确保不会 write 一条残缺记录 |

与现有 commit 顺序的关系（`operation_context.rs:459-504`）：预算停止路径现在是「checkpoint → journal checkpoint → terminal」。改为「snapshot → checkpoint（带 snapshot_id）→ journal → terminal」，保证任何声称可恢复的 checkpoint 都有对应文件状态。

### 7.2 写工具拦截

- 位置：`orca-runtime` 的 write-class 分发前（`tool_execution.rs:559 execute_tool_with_approval`、`:980 execute`），用 `ToolSpec` 的 action kind（`tool_types.rs`、`runtime_permission.rs` 现有判定）识别 `Edit | WriteFile` 和外部/MCP 的写类工具。
- 路径解析：
  - `edit` / `write_file`：从 `raw_arguments.path` 解析（与 `plan_write`/`plan_edit` 相同的 workspace containment 规则），相对 root 规范化。
  - 外部工具（`~/.orca/tools/*.toml`，`action_kind=write`）：若 `target` 字段可解析为 root 内路径则捕获；否则记 `untracked_roots` limitation。
  - MCP 写工具：同上，尽力而为。
- 捕获实现：走 `orca_rewind::RewindStore::put_file_pre_image(path)`：`symlink_metadata` → 必须是普通文件 → 有界读取（≤16 MiB）→ 计算 sha256 → 存 blob（必要时 zstd）→ 返回 `FilePreImage{sha256,size,mode,encoding}`。ENOENT → tombstone `pre: None`。
- **不在 `orca-tools` 内部改每个工具**：避免把 runtime 关注点下沉到工具 crate，也保证未来新工具只要经过 registry 就自动纳管。

### 7.3 bash / 外部写

- 不做写前拦截（成本见 §4 非目标）。
- turn 末有界观察：把 `end_digest` 与 `start_digest` 比较，得到「非工具写」变更路径列表；这些路径：
  - 若本 turn 未被写工具触碰过 → 记入 `untracked`，不参与恢复；
  - 若被写工具触碰过、但当前 hash ≠ `post_sha256` → 记入 `untracked`（agent 写完后被外部改回/覆盖）。
- TUI/CLI 在恢复前明确提示：`untracked` 路径与「未观察到的 bash 改动」不在还原范围（对齐 CC 的警告文案，但给出实际路径）。
- 观察成本：每个 turn 两次（start/end），各 ≤250 ms 且单进程串行（`workspace_observation.rs:50-100`）。可通过 `[rewind] observe_untracked = false` 关闭（关闭后 `untracked` 不可用，仅提示「bash 改动无法还原」）。

### 7.4 子代理与 worktree

- 同进程子代理（无隔离）：与父操作共享 `Arc<TurnCapture>`（按 root+session 绑定）。子代理的写工具同样经过 runtime 分发 → 前像记入**父 turn** 的快照（与 `writes_applied` 语义一致，`harness-contract.md` 的 writes_applied 覆盖当前 task tree）。
- async 子代理（独立 worker/session）：若在同一 root，写入同样落入父 capture（worker 与父共享 runtime host 时）；若在 worktree（`SubagentIsolation::Worktree`，`worktree.rs:22`），root 不同 → 由子 session 自己的 `TurnCapture` 记录（子 session 的 transcript 也在 `sessions/` 下）。父 session 不记录子 worktree 的文件。
- worktree 清理：`WorktreeGuard::finish` 只有在干净时才删（`worktree.rs:45-62`）；worktree 一旦删除，子 session 的快照恢复返回 `root_missing`，UI 提示「隔离工作区已清理，只能回退对话」。`rewind/` 存储不因 worktree 删除而清理，交给 GC。

### 7.5 成本

- 每个 turn 最多多两次有界 hash 观察（≤500 ms 上限，通常远小于）；写文件时多一次读（编辑器本来就要读旧内容做 diff，`plan_write`/`plan_edit` 已经读过，但捕获走独立路径，避免工具失败时无前像）。
- JSONL 每 turn 多一条记录（通常 <2 KiB）；磁盘 blob 只存被写文件的旧版本，且 zstd 后通常很小。
- 关闭方式：`[rewind] enabled = false` 或 `ORCA_DISABLE_FILE_HISTORY=1`；headless 默认关闭（对齐 CC：非交互需 opt-in，见 §11）。

---

## 8. 恢复语义

### 8.1 目标与状态定义

- **target** = 某条用户消息的 `ConversationItemId`（`item_...`）。语义：**该消息提交时的文件状态**（即该 turn 开始前）。
- 有效上下文内的消息才可选：被 `session.rewind` 回退掉、或被 compaction 覆盖掉的旧消息，选择器不展示（或展示但禁用，见 §10），避免「选到却无法回退」。
- 文件状态对某路径 P 的求法（撤销日志反推）：
  1. 取 `ordinal >= target_ordinal` 的所有快照记录中**最早**触碰 P 的那条，其 `pre` 即 P 在目标时刻的状态。
  2. 全无触碰 → P 不动。
  - 因此**不需要全量 manifest**；也天然支持「文件在目标时刻不存在」（tombstone）与「之后新建」（删除）。

### 8.2 六种模式

| 模式 | 文件 | 对话 | 恢复后输入框 | 备注 |
|---|---|---|---|---|
| `RestoreBoth`（默认） | 恢复 | 截断到 target | 填回 target 的 prompt | 先文件后对话（CC 同） |
| `RestoreCode` | 恢复 | 不动 | 不动 | 可单独用 |
| `RestoreConversation` | 不动 | 截断到 target | 填回 prompt | CC 同 |
| `SummarizeFrom` | 不动 | `[0..target)` + summary(`target..`) | 填回 prompt | 从 target 重新开始 |
| `SummarizeUpTo` | 不动 | summary(`0..target`) + `[target..]` | 不动 | 把旧历史压成摘要 |
| `Cancel` | 不动 | 不动 | 不动 | Never mind |

- UI 文案按 CC 的中文习惯：只恢复代码 / 只回退对话 / 两者都恢复 / 总结到此处 / 总结从此处 / 取消。
- 不可用模式要灰显并给出原因（无快照 → 只回退对话可用；快照被淘汰 → 同上；root 不匹配/缺失 → 只有对话；目标已被压缩 → 全部禁用并提示）。
- `SummarizeUpTo` 在 CC 快照中是 dead code（§2.5）。我们实现它，但灰度顺序放在最后（§15），因为它是唯一「保留未来消息」的语义，最容易被误用。

### 8.3 文件恢复计划算法

```
plan(target):
  fold = transcript_fold(session)                 // 得到有序快照列表 + 消息序
  reject_if target not in effective context
  reject_if any snapshot in plan has root != session.root   // 不做路径映射
  paths = ordered_map<rel_path, (pre_image, source_snapshot)>
  for snap in snapshots where snap.ordinal >= target_ordinal (按 ordinal 升序):
      for (path, entry) in snap.files:
          if path not in paths:
              paths[path] = (entry.pre, snap)
  for each (path, (pre, snap)):
      abs = root.join(path)                       // 校验 containment / 不跟随 symlink
      expected = last post_sha256 recorded for path among snapshots > snap
      current  = hash(abs) or Missing/NotRegular
      action = if pre is null: Delete else Write(pre)
      flags  = untracked_change if current != expected
      stats  = if text and size <= 4 MiB: diff(pre, current)  // 复用 file_admission 的 diff 限制
  return Plan{ writes, deletes, skipped, stats, warnings }
```

- 删除顺序在写入之后；写用「同目录临时文件 + fsync + rename」，失败不破坏原文件。
- 覆盖前把当前文件内容存 `pre/<sha256>`（≤16 MiB；超过则不备份并在计划里标 `no_rollback`），用于部分失败时的显式回滚。
- 缺失 blob：`skipped` + warning（不中断其余文件；与 CC 一致但**可见**）。
- untracked 覆盖策略（统一规则）：计划照常生成，但凡是 `untracked_change` 的路径——
  - TUI：确认页默认勾选「覆盖这些文件」，必须用户显式确认（逐项列出路径）；
  - CLI：默认**拒绝**（fail closed），需 `--force` 才覆盖；
  - `root_mismatch` / `rewind_record_inconsistent` / journal 未完成：`--force` 也不能绕过，必须人工处理。
- 幂等：对已是目标状态的文件，planner 跳过（hash 相同），报告 `unchanged`。

### 8.4 对话回退记录与载入折叠

新增记录（`thread_store/types.rs`，与 `Checkpoint` 并列）：

```jsonc
{"type":"session.rewind","schema_version":1,
 "session_id":"...","rewind_id":"rw_019f...",
 "at_message_id":"item_019f...",
 "kept_messages":12,          // 折叠到该记录位置后的消息条数（权威）
 "kept_compactions":1,        // 同上，compaction 记录条数
 "kept_summaries":2,          // 同上，summary 记录条数
 "conversation":{"kind":"truncate"},                    // 或 "summarize_from"/"summarize_up_to"
 "summary":null,              // summarize 模式下为生成好的摘要文本（自包含）
 "summary_usage":null,        // 生成摘要的 usage（审计/成本）
 "files":{"snapshot_id":"snap_...","restored":3,"deleted":1,
          "skipped":[],"untracked":["docs/x.md"]},        // 仅当同时恢复文件
 "recorded_at":"2026-11-07T10:01:00Z"}
```

- **为什么把摘要文本内联进记录**：Orca 的 transcript 是线性的，fold 无法可靠地「回填更早位置」；把 summary 直接放进 rewind 记录让载入折叠自洽、与 `ManualCompactionSnapshot`（`writer.rs:754-768`）同构，也不需要重写历史。
- **为什么存 kept counts**：Orca 没有 parentUuid；按「截止记录位置的计数」截断是 O(1) 且与 compaction 重置兼容。`at_message_id` 仅用于 UI/校验（若计数与 id 对不上，fail closed 并报 `rewind_record_inconsistent`）。
- 载入折叠（`transcript_from_records` 增加一个 `Rewind` 分支）：
  - `truncate`：`messages.truncate(kept_messages)`；`compactions.truncate(kept_compactions)`；`summaries.truncate(kept_summaries)`。
  - `summarize_from`：同上截断到目标之前（kept 计数已按 `[0..target)` 计算），再把 `summary` 构造为一条 summary 系统消息追加到末尾；`summary_state` 同步重建。
  - `summarize_up_to`：`messages.drain(0..kept_messages)` 后再在头部插入 summary 消息；`compactions`/`summaries` 清空。
  - usage **不回滚**：usage 记录不截断（省钱记录是真实花费），与 `--resume-at` 的「读到哪算到哪」不同；文档里写明差异。
- 运行中应用：内存截断复用一个新方法 `Conversation::rewind_to(&mut self, counts)`（复用 `backtrack_last_user` 的 truncate + `refresh_retained_tool_images` 逻辑，`conversation.rs:737-750`）。协调器顺序（与崩溃语义对齐）：**计算 plan → 应用文件 → append 记录（durable）→ 应用内存截断 → 发布事件**。append 失败时文件已改，不做自动回滚，只报告 `rewind_record_failed` 并保留 journal（下次启动提示可回滚）；不把 append 前移，否则会出现「记录声称成功但文件没改」的更坏状态。
- `--resume-at` 与新记录共存：载入先按 rewind 折叠，再按 `--resume-at` 边界截断；两者都未知 id → fail closed（`writer.rs:684-689` 语义）。文档说明「rewind 之后再用 resume-at 指向被回退的消息」会失败，符合预期。

### 8.5 两种总结模式

- 复用 compaction 的摘要器与 hooks（`compaction.rs:763-816` 的 `PreCompact`/`PostCompact`、`OnBudgetWarning` 路径），但增加显式范围参数：`partial_compact(conversation, range, direction)`，产出 `summary` + `summary_state` + usage，不直接改 transcript。
- 流程：确认 → 运行摘要（可取消，TUI 显示 `compacting` 状态）→ 成功后在 `session.rewind` 记录里写 `kind`/`summary`/`summary_usage` → 应用内存 → 事件。失败/取消 → 什么都不写、内存不动。
- 成本：摘要请求按当前会话模型与预算记账（真实 token）；`/rewind` 本身不消耗 token。预算耗尽时摘要失败，UI 提示可改用「只恢复代码/只回退对话」。
- 与自动 compaction 的区别：自动 compaction 由 token 压力触发、从头压缩；部分总结由用户选 pivot，只压缩一侧。二者共用摘要器，但要**避免重复压缩**：planner 在生成前检查 `range` 是否为 0 条（CC 会报 "Nothing to summarize..."，`compact.ts:802-808`）。

### 8.6 校验与拒绝

| 检查 | 失败结果 |
|---|---|
| target 在有效上下文内 | `target_not_in_context`（提示选择更近的消息） |
| 快照存在且未淘汰 | `snapshot_missing` / `snapshot_evicted`（降级为只回退对话） |
| 快照 root == 当前 session root（canonical + dev/ino 指纹） | `root_mismatch` / `root_missing`（不迁移、不降级到别处） |
| 无 active turn（`ensure_no_active_turn` 同款语义） | `busy`（提示先停止） |
| 无并发 restore 锁 | `another_restore_in_progress` |
| journal 无未完成项 | `pending_rewind_recovery`（先提示恢复/放弃） |

### 8.7 部分失败与回滚

- 文件逐个原子应用；每完成一个写 journal（append + fsync）。
- 中途失败（权限/磁盘满/文件被占用）：停止，不继续；报告 `partial`，列出已完成/未完成；保留 journal 与 `pre/`。
- 显式回滚（`orca rewind --rollback <op-id>` 或 TUI 提示）：用 `pre/` 把已改文件写回，然后删除 journal。不给自动回滚，避免二次误操作。
- blob 缺失/损坏（sha256 不匹配）：跳过该文件并报告；不写坏内容。

### 8.8 恢复后的输入框

- `RestoreConversation` / `RestoreBoth` / `SummarizeFrom`：TUI 把 target 用户消息的正文（含图片引用，若仍可解析）填回 composer，光标在末尾，等用户确认/编辑后发送（对齐 CC `REPL.tsx:3712-3739`）。
- 图片/附件：transcript 的 assets（`thread_store/assets.rs`）仍可读取时恢复附件引用；否则仅文本 + 提示。

---

## 9. 与现有机制的关系

### 9.1 与 `session.checkpoint`

- **保留**其为预算审计记录，语义不变（reader 继续忽略）。新增可选字段 `snapshot_id: Option<String>`（`#[serde(default, skip_serializing_if)]`，旧记录反序列化兼容）。
- 新增记录类型 `session.snapshot` / `session.rewind` 属于 transcript 格式扩展：**新版本写出的会话旧版本无法载入（fail closed）**，这是既有策略（`SessionRecord` 是 tag 枚举）；不提供降级渲染，文档中标注最低版本 v0.6.x。`session.checkpoint` 的加字段则双向兼容，不 bump 最低版本。
- 顺序改为 `session.snapshot` → `session.checkpoint`（带 snapshot_id）→ operation journal checkpoint → terminal；这样任何 resumable checkpoint 都有文件状态可查。
- `/rewind` 不复用 checkpoint 记录类型，避免把审计语义与用户回退语义混在一起；两者在 UI 上都叫「检查点」，但数据模型分开。

### 9.2 与 `resume` / `--resume-at`

- 文件回退是**显式操作**，不会自动把文件恢复到某个 resume 点；`--resume-at` 仍只截断对话（`session.rs:320-338`、`writer.rs:669-704`）。
- `session.rewind` 记录在载入 fold 中生效，早于 `--resume-at` 的边界截断；resume 后继续 append 到原 transcript、保持 session id（`session.rs:371-389`）。
- 文档更新 `harness-contract.md:293-295`：从「File rewind is not promised」改为「文件状态由实现该特性的 session 快照承诺，见 `docs/design/rewind-file-checkpoints.md`；无快照的会话仍不承诺」。

### 9.3 与 budget

- 文件快照不计入预算；`/rewind` 的文件+截断部分不消耗 token，不新开 operation。
- 两种 summarize 模式消耗 token：走当前会话预算，失败（`budget_exhausted`）时提示改用其它模式。
- 预算停止后仍允许 `/rewind`（session idle）：这正是它最有用的场景之一。
- usage 不回滚（§8.4）；恢复后继续对话时，`session.usage_baseline`/resume 的新预算 scope 逻辑不变（`writer.rs:1838-1856`、`budget_resume_contract.rs:9-10`）。
- 与 `--resume-at` 的差异写进文档：`--resume-at` 截断 transcript 后 usage baseline 只覆盖截断后的记录（推断：尾部 usage 归因丢失），`/rewind` 保留完整 usage。

### 9.4 与 worktree

- 每个 session 记录自己的 root；恢复只在原 root 内进行，**没有路径映射**（沙箱也不做映射，`sandbox/mod.rs:95-102`）。
- 子代理 worktree 的 session：恢复目标是 worktree 路径；worktree 被 `finish` 删除后 → `root_missing`，只允许对话回退。
- 未来用户级 `/worktree` 命令应复用同一 root 绑定模型（快照按 session root 记录），不需要改本设计。

### 9.5 与 compaction / summary

- 自动 compaction 仍按原策略运行；`session.snapshot` 与 compaction 无关。
- 手动/自动 compaction 在 transcript 里会重置消息列表（`ManualCompactionSnapshot`，`writer.rs:754-768`）；`session.rewind` 的 kept counts 在 fold 顺序中应用，二者兼容。
- 若 target 消息已被 compaction 覆盖（不在 `messages` 视图里），拒绝并提示（同 CC）。
- 部分总结复用摘要器，但要保证不与自动 compaction 并发：`/rewind` 要求 idle，天然互斥。

### 9.6 与 hooks

- 本期不强依赖 hooks；建议在 P1 hooks 扩容时加 `pre_rewind` / `post_rewind`（payload：target、mode、plan 摘要；`pre_rewind` 只注入 context、不否决），复用 `hooks.rs:71-100` 与 `hook_types.rs:5-15`。**若不做**，事件（§11.3）已足够给外部编排消费。

### 9.7 与权限 / sandbox / trust

- 恢复是**用户显式命令**，不经过工具审批；但：
  - 必须通过路径 containment（§12.3），拒绝 root 外、`..`、symlink 逃逸；
  - 必须通过 folder trust（`folder_trust.rs`）——未信任目录不允许写入；
  - 不触碰 `.git`/排除目录；不执行任何恢复出来的脚本/命令；
  - 记录 mode 但不改 ownership/ACL；Windows 只读属性尽量保留。
- 不把 `/rewind` 建模成工具（不暴露给模型），避免 agent 自己回退用户工作区。

---

## 10. TUI 交互

### 10.1 命令与状态

- 新 `SlashCommand::Rewind`（`orca-tui/src/commands/mod.rs:2-47`），解析 `"rewind" | "checkpoint"`（parse 臂 `:157-226`），`all_commands()` 加一行（`:234-262`，驱动 `/` 菜单与补全），dispatch 到 `slash_command_actions.rs:234` 的新臂（参照 `:459 SlashCommand::Resume => open_session_picker`）。
- 前置条件：session idle（无 active turn）。v1 在 turn 运行时直接提示「先停止当前 turn」，不做 CC 式「先取消再回退」；理由：文件恢复要求确定的工作区状态。
- 状态：`AppState.rewind: Option<RewindState>`，放在 `types.rs:269-272`（mcp panel 状态旁，不新建 `AppStatus`）。选择器状态机（列表 → 模式 → 确认）镜像 `SessionPickerPhase`（`types.rs:209-229`）：
  ```rust
  enum RewindPhase {
      PickCheckpoint { selected: usize, scroll: usize },
      PickMode { checkpoint: RewindCheckpointView, selected: RewindMode, show_preview: bool },
      Confirm { checkpoint: RewindCheckpointView, mode: RewindMode, plan: Option<RewindPlan> },
  }
  ```
- 数据来源：新增只读 host 调用 `RuntimeSurfaceHostHandle::list_rewind_checkpoints`（参照 `list_saved_session_page`，`runtime_surface/host.rs:58`；session picker 的调用模式 `session_picker_actions.rs:492`），返回 `Vec<RewindCheckpointView>{message_id, ordinal, prompt_preview, timestamp, file_stats, available_modes, warnings}`；失败时回退到「从当前 transcript 内存消息生成行、无文件数据」。
- 提交动作：`UserAction::RewindToCheckpoint { checkpoint_message_id, mode }`（`protocol.rs:636-804`），经 `action_dispatcher.rs` → `hosted_controller.rs` → runtime；完成后 `TuiEvent::RewindCompleted { prompt: Option<String>, report: RewindReport }`，在 `state_reducer.rs` 按 `TuiEvent::Backtracked` 的方式处理（`:1085`，用 `remove_after_last_user`/`truncate_messages` 截断视图 + Notice），并 prefill composer。

### 10.2 步骤 1：检查点列表

- 居中有界对话框（`chrome::dialog_rect`、`panel_block`，`chrome.rs:31/142`；参照 `/mcp` 面板 `types.rs:269-272`、`mcp_panel.rs:28`，不新增 AppStatus）。
- 行 = 用户消息（turn），最近的在最下；每行两行高：
  - 第一行：`#ordinal  相对时间  prompt 首行（截断）`
  - 第二行：`N files changed +X −Y` / `No code changes` / `⚠ 无文件快照（仅对话可回退）` / `⚠ 快照已淘汰`
- 窗口化 `skip/take`（`mcp_panel.rs:97-99`），可视 9 行（比 CC 的 7 略多，Orca 终端更常用 80×24 以上；`popup_window` 上限 12，`ui.rs:5943`）。
- v1 不展示合成的「(现在)」行：它没有可回退内容，展示只会增加歧义（决策见 §16 Q5）。
- 键位见 §10.6。

### 10.3 步骤 2：动作菜单

- 选择检查点后进入模式选择，列表样式复用 `ApprovalOption` 的 `key()`/`label()` 模式（`types.rs:333-402`）：
  ```
  恢复代码和对话   (默认)
  只回退对话
  只恢复代码
  总结到此处
  总结从此处
  取消
  ```
- 每项一行说明后果（「代码恢复到该消息提交时；对话截断到该消息」等）；不可用项灰显 + 原因（`snapshot_missing`、`root_missing`、`target_not_in_context`）。
- `Tab` 切换右侧/下方 diff 预览（参照 session picker 的 Tab actions，`session_picker_actions.rs:182`）：显示该 turn 的文件统计与前几个文件的 unified diff（`diff_highlight::render_unified_diff`，`ui.rs:1387`；滚动参照审批面板 `ui.rs:6480-6545` 的 "… lines N–M of T · PgUp/PgDn"）。
- `1..6` 直接选模式 + Enter 确认；Esc 返回列表。

### 10.4 步骤 3：确认与预览

- 变更类模式（所有恢复/总结）进入确认页，内容：
  - 目标行摘要（时间、prompt）。
  - 文件计划：`将写入 3 个、删除 1 个、跳过 1 个（blob 缺失）` + 可展开列表 + diff。
  - 警告：`untracked`（bash/外部改动，不在还原范围，列出路径）；`no_rollback`（>16 MiB 不备份）；`root_missing`；`snapshot_evicted`。
  - 总结模式额外显示：使用的模型、预计消耗、「将调用一次摘要请求」。
- 确认页默认高亮「应用」，但必须显式 `y`/Enter 才执行（不做自动执行）。
- 确认后：关闭对话框，状态栏/Notice 显示进行中（文件恢复通常瞬完；总结显示 `compacting` 复用现有 compaction 状态 UI）。
- 部分失败：保留对话框并显示错误 + 已完成列表；给出「重试剩余」「放弃」「回滚」（若有 `pre/`）三个选项（v1 至少显示错误与已完成项，回滚可 CLI）。

### 10.5 完成 / 错误 / 渲染

- 成功文件恢复：`TuiEvent::Notice` 或 transcript 系统行：`已恢复 4 个文件（+41 −7），删除 1 个`。
- 成功对话回退：视图截断（复用 `truncate_messages`/`remove_after_last_user`），插入一行分隔：`── 已回退到此处（消息 #12）──`；composer 填入原 prompt。
- 总结成功：Notice `对话已总结（Ctrl+O 查看历史）`（对齐 CC 文案），不截断用户可见历史（CC fullscreen 'from' 保留 scrollback）；实际上下文按 summary 重建。
- 错误：`TuiEvent::Error`（`state_reducer.rs:802`），保留对话框供重试。
- 渲染用现有 primitives（`chrome.rs:31/49/73/142`：`panel_block`/`hint_line`/`option_line`/`dialog_rect`），不新建全局状态。

### 10.6 键位表

| 上下文 | 键 | 行为 |
|---|---|---|
| 列表 | ↑/↓、j/k、Ctrl+P/N | 移动选择 |
| 列表 | PgUp/PgDn、Home/End、g/G | 翻页/首尾（窗口化，同 `mcp_dialog_actions.rs:27-57`） |
| 列表 | Enter | 进入模式选择 |
| 列表 | Esc / Ctrl+C | 关闭；Ctrl+C 需二次确认（参照 CC `:393-398`） |
| 列表 | Tab | 预览该 turn 的 diff |
| 模式 | ↑/↓、1–6 | 选择模式 |
| 模式 | Enter | 进入确认（变更类）或直接完成（取消） |
| 模式 | Esc | 回列表 |
| 确认 | y / Enter | 执行 |
| 确认 | Esc / n | 取消 |
| 任意 | Ctrl+C 两次 | 强制关闭（不执行） |

---

## 11. CLI 与对外契约

### 11.1 `orca rewind`

新顶层 `Command` variant（`src/cli.rs:68-98`，与 `Doctor`/`Storage` 并列）：

```text
orca rewind --session <id|latest> [--at <message-id>] [--mode both|code|conversation|summarize-from|summarize-up-to]
            [--dry-run] [--json] [--force] [--rollback <rewind-op-id>]
```

- `--session latest` 等价 `--last`；缺省 `--session latest`。
- `--at` 缺省 = 最近一个用户消息（即「撤销上一轮」）。未知 id → 退出码 1，stderr `no saved message matches '<id>'`（复用既有语义）。
- `--dry-run`：不出错就打印计划与统计；`--json` 输出结构化报告（见 11.2）。
- `--mode` 缺省 `both`；`summarize-*` 需要模型调用，headless 下同样计费。
- 退出码沿用 `harness-contract.md`：`0` 成功；`1` 失败/拒绝；`130` 取消。审批不适用（用户显式命令）。
- 原子性：CLI 与 TUI 共用 `RewindCoordinator`；CLI 也要求 session 无 active turn（daemon 下通过 session 锁判断），否则 `busy`。

### 11.2 JSON 报告

```jsonc
{"schema_version":1,"session_id":"...","target":{"message_id":"item_...","ordinal":12},
 "mode":"both","dry_run":true,
 "files":{"changed":3,"deleted":1,"unchanged":5,"skipped":[{"path":"a.bin","reason":"blob_missing"}]},
 "stats":{"insertions":41,"deletions":7,"omitted":false},
 "warnings":[{"kind":"untracked_change","paths":["docs/x.md"]},
             {"kind":"root_mismatch","recorded":"/a","current":"/b"}],
 "conversation":{"kind":"truncate","kept_messages":12},
 "applied":{"rewind_id":"rw_...","journal":"..."}}
```

### 11.3 事件、投影与协议

- `event_schema.rs:147-263` 新增 `EventType::SessionRewind`（`"session.rewind"`），payload：
  `{target_message_id, mode, files:{changed,deleted,skipped}, conversation:{kind,kept_messages}, duration_ms, applied}`。
- 因为文件/对话状态变更属于语义事件，`is_semantic` 的判定需要讨论：建议**不**进 transcript 消息流（避免污染模型上下文），只作 stream/JSONL 事件；`session.rewind` 记录本身在 transcript 里已可审计。**不要**把它做成 `SemanticEvent`（`writer.rs:774-779` 会把它转成消息）。
- 需要更新的投影/协议（沿现有 runtime surface 投影链）：`runtime_surface/projection.rs`、`runtime_surface/commands.rs`（`SurfaceCommand`）、`server/surface_adapter.rs`、`orca-tui/src/surface_projection.rs`、`surface_client.rs`、`diagnostics.rs`、`protocol/{wire,thread}.rs`；重新生成 `crates/orca-runtime/src/golden/acp_wire/*.json`（`ORCA_UPDATE_GOLDEN`）。
- TUI 边界：新 `UserAction` 要同步 `surface_boundary_tests.rs:8-63 CURRENT_ACTIONS`、`docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`、`hosted_controller.rs:150-167/:385-405` 的 guard 列表、`TUI_ENTRYPOINTS`（`surface_boundary_tests.rs:130-174`）。

### 11.4 文档更新

- `docs/harness-contract.md`：§Session 的「File rewind is not promised」改为条件承诺；新增 `session.snapshot`/`session.rewind` 记录说明、`orca rewind` 命令、JSON 报告、事件。
- `docs/public-cli-manifest.json`：只登记 CLI 变更，`/rewind` 属 TUI，不加入。
- 站点文档：`site/src/docs/md/{en,zh}/terminal-ui.mdx`（命令列表）、`README.md:163-166` 的命令表；TUI 内 `/` 菜单由 `commands/mod.rs:234` 自动带出。
- `docs/harness-contract.md` 的 `writes_applied`/`workspace_observation` 文本与预算 checkpoint 示例顺带校正（示例里的 `max_inner_turns` 在代码里并不存在）。

---

## 12. 崩溃、并发与安全

### 12.1 两阶段 journal

- 恢复前：`journals/<op>.jsonl` 写 `{"kind":"intent","plan_digest":...,"paths":[...]}` + fsync。
- 每个文件成功：append `{"kind":"applied","path":...,"action":...}` + fsync。
- 结束后：append `{"kind":"committed","rewind_id":...}`；成功后删除 journal；失败保留。
- 启动/打开 session 时若发现未完成 journal：不自动重放（避免二次写入），通过 `TuiEvent::Error`/`orca doctor` 报告 `pending_rewind_recovery`，提供 `orca rewind --rollback <op>` 或手动丢弃。
- 为什么不放进 transcript：journal 是操作日志，不属于对话历史；放 transcript 会让每条文件恢复都重放给模型。

### 12.2 锁

- 复用 transcript 的 `<path>.lock`（`writer.rs:1206-1216`）保证同一 session 串行。
- 新增 **root 级恢复锁**：`$ORCA_HOME/rewind/locks/<sha256(canonical_root)>.lock`，防止 daemon 下两个 session 同时恢复同一工作区；拿不到 → `another_restore_in_progress`。
- apply 前对每个目标路径做一次「计划时 hash vs 当前 hash」的 CAS 校验；不一致 → 拒绝并提示重新生成计划（`concurrent_workspace_change`）。

### 12.3 路径安全

- 只接受 workspace 相对路径；`root.join(rel)` 后：
  - 拒绝 `Component::ParentDir`、绝对路径、Windows 前缀；
  - 逐级用 `symlink_metadata` 检查，任一层是 symlink → 拒绝（或仅允许最终组件是普通文件）；
  - 写入用「同目录临时文件 + rename」，避免跟随目标 symlink；
  - 读取 blob 后校验 sha256 + size（复用 `assets.rs:65-87` 的校验模式）。
- 不写 `.git` / 排除目录；恢复路径若落在排除目录 → 跳过并报告（防止历史快照包含用户手工放入的 `.git` 文件）。

### 12.4 隐私与企业策略

- 文件内容会落盘到 `$ORCA_HOME/rewind`。这是与 CC 相同的取舍，但必须可关闭：
  - 配置 `[rewind] enabled`、环境 `ORCA_DISABLE_FILE_HISTORY=1`；
  - headless 默认关闭（CC 的交互默认/非交互 opt-in 保持一致）；
  - 文档写明「检查点包含源码内容，位于 ~/.orca/rewind，删除 session 或 `orca storage prune --rewind` 可清理」；
  - 不读排除目录，降低把密钥/构建产物落盘的概率；不加密（与 `sessions/` 一致，依赖磁盘加密）。
- 企业策略（与 P0 插件项的作用域/允许列表同属一类）：后续可在 managed config 里加 `rewind.disabled`/`rewind.max_bytes`；本期只用 config/env。

---

## 13. 实现拆分与工作量

| 模块 | 文件 | 内容 | 估 |
|---|---|---|---|
| `orca-core` | `tool_types.rs`/新 `rewind.rs` | `RewindMode/RewindTarget/RewindReport`、`ConversationItemId` 复用、序列化契约 | S |
| 新 crate `crates/orca-rewind` | `store.rs` `planner.rs` `applier.rs` `journal.rs` `path.rs` `Cargo.toml` | CAS 存储、撤销日志 planner、原子 applier、journal、路径安全；单测为主 | M |
| `orca-runtime` | `thread_store/types.rs`（+2 记录）、`writer.rs`（append + fold）、`session.rs`（TurnCapture、rewind_to）、`tool_execution.rs`（写前拦截）、`operation_context.rs`（顺序/snapshot_id）、`runtime_host.rs`/`runtime_surface/host.rs`（coordinator + list 调用） | 深度集成 | M |
| `orca-runtime` 事件/协议 | `event_schema.rs` + 投影 6 处 + golden | 事件与 wire | S |
| `orca-runtime` 部分总结 | `compaction.rs` | `partial_compact(range, direction)` 复用摘要器 | M |
| `orca-tui` | `commands/mod.rs`、`slash_command_actions.rs`、`types.rs`、`ui.rs`、`*_actions.rs`、`state_reducer.rs`、`surface_classification`/`surface_projection` | 选择器 + 确认页 + reducer | M |
| CLI | `src/cli.rs`、`crates/orca-runtime/src/command/rewind.rs` | `orca rewind` + JSON | S |
| 文档/测试 | harness-contract、site、README、tests | 见 §14 | S |

总体：**M**（约 2–3 周单人；关键路径是 runtime 捕获 + fold 语义）。

---

## 14. 测试计划

### 单元（`orca-rewind`）

- CAS：put/read/verify、digest 损坏、重复内容去重、zstd/raw 往返、mode 保留。
- planner：turn 顺序、同文件多 turn 取最早 pre、tombstone 删除/重建、未触碰文件不动、blob 缺失跳过、`untracked_change` 标记、路径逃逸/符号链接拒绝、`unchanged` 幂等。
- applier：原子写、删除顺序、部分失败 + journal、回滚（`pre/`）、并发 CAS 失败。
- journal：崩溃点（intent 后 / 半 apply / committed 前）状态机。

### 集成（`crates/orca-runtime/tests/rewind_contract.rs`）

- 用假 provider 跑一个 turn：写 2 个文件（新建 + 修改）→ 快照记录存在、blob 可读 → rewind code → 文件往返。
- 对话回退：turn1/turn2 → rewind 到 turn1 → reload（模拟 resume）后只有 turn1 消息；继续对话追加正常。
- 两种 summarize：假 provider 返回固定摘要 → 记录形态、fold 后消息/summary 状态、usage 保留。
- budget 停止路径：`session.snapshot` 先于 `session.checkpoint`，checkpoint 带 snapshot_id。
- 兼容：旧 transcript（无新记录）载入不变；带未知 schema 字段 fail-closed / 忽略策略。
- 崩溃注入：参照 `writer.rs:1253 inject_manual_compaction_snapshot_failure_once` 做快照写失败注入。

### TUI

- `app_integration_tests.rs`：`/rewind` 打开列表（模板：`slash_menu_tab_opens_resume_picker_like_enter`，`:7602-7688`）；列表→模式→确认的键位流；Esc 回退层级；不可用项灰显。
- 渲染快照：`mcp_panel.rs:396-420` 模式加 golden（`ui.rs:17247 assert_golden`）。
- `tests/tui_pty_contract.rs`：`/rewind\r` + 截图断言 + `arm_idle_exit`（模板参照 `/side`，`:929-940`）。
- `tests/tui-e2e/orca-extended.sh`：黑盒加一条 `/rewind` 流程（`:96-98` 风格）。

### 契约

- `budget_resume_contract.rs` 扩展；`history_contract.rs`；golden wire 重新生成；`surface_boundary_tests.rs` 的 CURRENT_ACTIONS/entrypoints。

### 安全/隐私

- 路径逃逸用表驱动攻击样本（`..`、绝对路径、symlink 目录、Windows UNC/前缀）。
- `ORCA_DISABLE_FILE_HISTORY` 下不产生任何 `rewind/` 文件；`/storage` 统计与清理。

---

## 15. 里程碑与灰度

1. **M0（存储 + 记录）**：`orca-rewind` CAS、`session.snapshot` 记录、TurnCapture + 写工具拦截、`[rewind] enabled` 默认 **false**；单测 + runtime 集成。
2. **M1（计划 + CLI dry-run）**：planner、`orca rewind --dry-run --json`；只读，先验证正确性。
3. **M2（应用 + 对话回退）**：applier/journal、`session.rewind` fold、esc 视图截断、CLI 实际执行；默认仍 false，内部 dogfood。
4. **M3（TUI）**：`/rewind` 选择器 3 个恢复模式（both/code/conversation）+ 取消 + 确认页；默认 **true**（交互会话）。
5. **M4（总结模式）**：`partial_compact`、`summarize_from/up_to`；先 from，up_to 最后。
6. **M5（运维）**：保留/GC、`/storage` 集成、headless opt-in（`rewind` 命令默认可用，但捕获默认关闭）、文档与站点。

灰度开关：`[rewind] enabled`（捕获）、`[rewind] modes.summarize`（总结模式）、`[rewind] observe_untracked`。任一异常可退回「只提示、不捕获」。

---

## 16. 未决问题

1. **粒度**：确认按「用户消息（turn）」而非「每条 assistant 消息」。CC 经验表明 turn 粒度足够且成本可控；更细粒度需要为每个 tool call 存前像（同 turn 去重后差别不大），暂不做。
2. **bash 覆盖**：是否值得在 M5 之后做「bash 前后有界内容捕获」？(a) 全量拷贝太重；(b) Linux 可考虑 `unshare`/overlay 但跨平台不可行；(c) 解析命令 `git`/`rm`/`mv` 只覆盖已知命令。建议不做，保持「诚实不可还原 + 提示」。
3. **`SummarizeUpTo` 是否值得做**：CC 快照里是 dead code，但语义有价值（「把旧历史压掉，保留最近的未来」）。建议实现但放最后；若产品认为易混淆，可只留 UI 文案而不暴露。
4. **Esc 空输入回退统一**：现在 `Backtrack` 只改内存（resume 复活）。是否把它也写成 `session.rewind{kind:truncate}`？会改变现有行为（更正确但需迁移测试）。建议在 M2 一并做，或至少在文档里标注已知限制。
5. **「(现在)」行**：CC 有合成行用于确认状态。Orca v1 不展示；若用户需要「查看未保存的检查点」，后续再加。
6. **跨 session 去重/共享 blob**：per-session 足够；若磁盘占用成问题再考虑全局 CAS。
7. **二进制文件 diff/统计**：确认页对二进制只显示「已修改」，不做行统计；需要产品确认文案。
8. **worktree 子代理的 rewind 入口**：`/rewind` 在子代理 session 里是否可见、以及 worktree 被删后的 UX，需要和 Agent Teams 设计一起定。
9. **事件是否进 transcript**：当前建议 stream-only（不进模型上下文）；若后续需要「会话内系统行持久化」，再评估 `session.rewind` 记录的展示投影。

---

## 17. 证据索引

### 参考实现（CC 2026-03-31 快照）

- `src/utils/fileHistory.ts:31-52`（类型/tombstone）、`:54`（100 上限）、`:63-78`（门控）、`:114-158`（首触去重 + 回溯插入）、`:241-311`（逐轮 stat/环形）、`:347-397`（rewind）、`:537-672`（apply/OriginFileChanged）、`:725-798`（blob 路径/拷贝/chmod）、`:847-862`（v1 fallback）、`:888-1046`（重建/跨 session 复制）
- `src/commands/rewind/{index.ts,rewind.ts}`、`src/components/MessageSelector.tsx:45/59-67/93-134/121/177-246/269-284/345-357/359-390/722-766/767-792`
- `src/screens/REPL.tsx:3661-3739`（对话截断 + prefill）、`:4911-4990`（动作映射/summarize）、`:4923-4930`（已被压缩的 no-op）
- `src/services/compact/compact.ts:772-808`（from/up_to 语义）、`src/utils/sessionStorage.ts:1085-1099/3226-3239`、`src/hooks/useLogMessages.ts:48-57`、`src/types/logs.ts:188-193`
- `src/tools/FileWriteTool.ts:255-264`、`FileEditTool.ts:431-440`、`NotebookEditTool.ts:311-317`、`BashTool.tsx:392-394`、`src/screens/REPL.tsx`/`handlePromptSubmit.ts:525-539`、`QueryEngine.ts:641-655`
- `src/entrypoints/agentSdkTypes.ts:261-262`（fork 不带 file history）、`src/cli/print.ts:4525-4566` + `controlSchemas.ts:308-327`（rewind_files dry-run）

### Orca 现状

- `docs/harness-contract.md:293-295`（不承诺 file rewind）、`:185-217`（resume/`resume-at`/checkpoint 示例）、`:264-267`（resume 新预算 scope、usage 保留）
- `crates/orca-runtime/src/thread_store/types.rs:87-115`（SessionMeta/cwd/roots）、`:192-231`（SessionRecord tag）、`:280-303`（session.checkpoint）、`:224-227`（context.summary）、`:226-227`（ManualCompactionSnapshot）
- `crates/orca-runtime/src/thread_store/writer.rs:171-216`（write/durable）、`:669-704`（read_transcript_until）、`:754-768`（ManualCompactionSnapshot 载入重置）、`:1206-1216`（文件锁）、`:1592-1597`（append_checkpoint）、`:1295-1305`（last_committed_message_id）、`:1838-1856`（usage_baseline）
- `crates/orca-runtime/src/operation_context.rs:431-504`（预算停止 commit 顺序/checkpoint）、`:605-613`（reason）
- `crates/orca-runtime/src/session.rs:320-338`（ResumeAt 截断）、`:371-389`（resume 保持 session id）、`:705-713`（begin_runtime_turn）
- `crates/orca-tools/src/write_file.rs:11-36`、`crates/orca-tools/src/edit.rs:13-41`、`crates/orca-tools/src/file_admission.rs:13-19/356`
- `crates/orca-runtime/src/tool_execution.rs:559/980/1383-1391`；`crates/orca-runtime/src/workspace_observation.rs:7-11/26/102`；`crates/orca-runtime/src/thread_store/assets.rs:13-130`（CAS 模式）
- `crates/orca-runtime/src/worktree.rs:22-62`；`crates/orca-tools/src/sandbox/mod.rs:95-102`
- `crates/orca-core/src/conversation.rs:737-750`（backtrack）；TUI：`crates/orca-tui/src/hosted_context.rs:95-110`、`state_reducer.rs:1085`、`types.rs:2026`
- `crates/orca-core/src/hook_types.rs:5-38`；`crates/orca-runtime/src/hooks.rs:71-100`
- `crates/orca-core/src/event_schema.rs:147-263`；`crates/orca-runtime/src/runtime_surface/host.rs:58/465`；`crates/orca-tui/src/commands/mod.rs:2-47/157-226/234-262`；`slash_command_actions.rs:234/459`；`session_picker_actions.rs:148-220/492`；`types.rs:209-229/269-272/333-402/531-546`；`mcp_panel.rs:28/97-99`；`ui.rs:1192/1387/5943/6082/6480-6545`；`chrome.rs:31/49/73/142`；`protocol.rs:236ff/493/636-804`；`surface_boundary_tests.rs:8-63/130-174`；`tests/tui_pty_contract.rs:929-940`；`tests/tui-e2e/orca-extended.sh:96-98`
- `src/cli.rs:68-98/199-201`；`parse_exec`/`resolve_history_mode`（`command/exec.rs:214-249`）；`thread_identity.rs:9/87`；`history.rs:453-471`；`home.rs:59-85`

---

## 18. 实现状态（v0.6 落地记录）

本节记录实际实现与本章设计之间的差异；本节之前的内容保持为设计意图，实现以代码为准。

### 已落地（phase 1 + phase 2）

- `crates/orca-rewind`：CAS blob 存储（sha256、zstd/raw、mode 保留、去重、GC）、撤销日志 planner（tombstone、untracked 检测、行级 stats、路径安全/排除目录）、两阶段 journal applier（原子写、备份、partial 报告、rollback）、`plan_diff_previews`（TUI 确认页的 unified diff），62 个单测。
- transcript：`session.snapshot` / `session.rewind` / `session.snapshot_retention` 记录；durable append；载入 fold 支持 `truncate`、`summarize_from`（截断 + 末尾 pinned summary）、`summarize_up_to`（drain + 清空 compaction/summary + system prompt 后插入 summary），未知 kind fail closed；`read_snapshots` / `read_effective_user_messages` / `read_transcript_before_message` / `read_transcript_split_at_message` / `read_snapshot_retention`。
- runtime 捕获：turn 开始建 `RewindTurn`（thread extension，覆盖交互 `BacktrackableUser` 与 headless `PinnedUser`）；写工具 dispatch 前记前像；turn 末 post 摘要 + 有界观察 untracked + `line_diff_stats` change_stats + append snapshot；按 ordinal 保留最近 100 个检查点并追加 retention 水位。
- runtime 协调器：`InteractiveSession::{rewind_checkpoints, rewind_to_checkpoint}`；文件恢复（plan → CAS 校验 → journal apply）与对话回退（truncate / summarize_from / summarize_up_to）；summarize 用 `orca_provider::summarize_range`（新增强制范围摘要 API，不受上下文压力门槛限制），dry-run 不调用模型；失败不写记录、不动内存；pending journal 检测与拒绝。
- host/CLI：`RuntimeThreadHandle` / `RuntimeSurfaceThreadHandle::{list_rewind_checkpoints, rewind_checkpoint}`、`ThreadCommand` 两条空闲命令；`orca rewind` 支持 `--at/--mode(both|code|conversation|summarize-from|summarize-up-to)/--dry-run/--json/--force/--rollback OP_ID/--prune`（CLI summarize 也调用真实摘要器）。
- TUI `/rewind`（别名 `/checkpoint`）：检查点列表（含 `snapshot_evicted` / pending journal 不可用原因）→ 模式菜单（summarize 已启用）→ 确认页（文件计划、skip、warnings、summarize 提示、unified diff 预览：Tab 折叠/展开、PgUp/PgDn 滚动、binary/truncated 标记）→ apply 等待显示 Compacting → `RewindCompleted` reducer 截断/不截断视图 + Notice + prompt 回填 composer；模式页 Tab 可预览 dry-run。PTY 冒烟测试覆盖 `/rewind` 打开与 Esc 关闭。
- 文档：`docs/harness-contract.md`（条件承诺、记录/CLI/retention/rollback/prune/summarize 语义）、zh/en CLI reference 与 terminal-ui、README、public CLI manifest、surface 边界 manifest、TUI golden。

### 剩余偏差 / 已知限制

1. **`[rewind]` 配置已落地**：`enabled`（默认 true）、`observe_untracked`（默认 true）、`max_checkpoints`（默认 100，最小 1）；env `ORCA_DISABLE_FILE_HISTORY` / `ORCA_REWIND_DISABLE_OBSERVE` 仍优先且只能关不能开（见 `site/src/docs/md/*/configuration.mdx` 与 `docs/harness-contract.md`）。
2. **GC 已自动 + 可手动**：写 retention watermark 后自动对未淘汰快照引用集做 best-effort GC；`orca rewind --prune` 仍可手动清理。`orca storage` 未单独展示 rewind 占用。
3. **`session.rewind` 事件已实现但线路可见性有限**：事件通过 attached surface observer 发布，并在 recorded session 的 transcript 事件日志里 journal；ACP `session/update` 与 JSONL server 线尚没有 rewind 请求通路，因此没有端到端 JSONL/ACP rewind 场景（见 `docs/harness-contract.md` 的说明）。
4. **root 校验**：restore 以目标 snapshot 的 `root` 为准并校验参与快照一致，且比较 `session meta.cwd` 检测 session cwd 漂移（不一致 fail closed）；只有 target 快照缺失时才走“无 snapshot 只回退对话”的降级。
5. **子代理写入捕获**：主 thread 的 turn 捕获挂在 thread extension 上；同进程子代理共享该 extension 才计入父 turn，否则进入 `untracked`；worktree 隔离子代理由子 session 自己记录。
6. **CLI summarize 成本已折算**：`execute_summarize` 用 `CostTracker`（book rates + `[models.*.pricing]` 覆盖 + 峰时倍率）把摘要 usage 折算 `estimated_cost_usd`；摘要模型为 provider config 实际使用的模型。
7. **TUI 细节**：Ctrl+C 单击关闭对话框（设计 §10.6 为两次）；选择器数据走异步 `UserAction`/`TuiEvent` 往返；previews 仅 dry-run 填充、最多 3 个文件显示。
8. **对话回退实现方式**：truncate/summarize 都通过截断 transcript + `resume_conversation` 重建内存 conversation（保留 system prompt；下一 turn 重新注入 pinned context），与 fold 语义一致。
9. **2027 年中国节假日**：`pricing/rates.rs` 的 `WEEKDAY_HOLIDAYS` 目前只覆盖 2026 年；国务院公布 2027 年放假安排后需补录（与 rewind 无关，发布检查项）。

### 验证（phase 2）

- `cargo test -p orca-rewind`：**62 passed / 0 failed**。
- `cargo test -p orca-runtime --lib rewind`：**37 passed / 0 failed**（capture/summarize/retention/previews/pending journal/split/fold/CLI）。
- `cargo test -p orca-tui --lib rewind`：**25 passed / 0 failed**（含 summarize/review/preview/golden）。
- `cargo test -p orca-agent --bin orca public_cli_manifest_matches_visible_clap_surface`：通过。
- `cargo test -p orca-agent --test tui_pty_contract tui_rewind_opens_the_checkpoint_picker_and_esc_closes_it -- --exact`：通过。
- `cargo check --workspace --all-targets`：通过。
- `cargo test -p orca-runtime --lib -- --test-threads=2`：2137 passed / 1 failed（`shell_session::tests::registration_failure_reaps_spawned_shell_process_group` 在满载下进程组回收竞态；**单跑通过**，文件未被本特性修改，属既有 flake）。
- `cargo test -p orca-tui --lib -- --test-threads=2`：**1885 passed / 0 failed**。
