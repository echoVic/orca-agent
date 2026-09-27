# Computer Use 阶段 1：实验与图片通道 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 先用三个一次性实验回答设计第 9 节的问题，再打通"工具结果带图片"的通道，让能看图的模型看到 MCP 工具（以及以后的 `computer` 工具）返回的图片。

**Architecture:**
- 图片沿用对话里已有的 `ImageInput` 类型，挂在 `ToolResult` 和 `Message::Tool` 上。
- 持久化复用会话资源目录的通用外置逻辑；恢复会话时显式丢弃旧截图。
- 发请求前，不能看图的模型的图片换成说明文字，能看图的模型只保留最新 3 张，再按实验 1 的结论映射到 DeepSeek 请求。

**Tech Stack:** Rust（orca-core、orca-mcp、orca-tools、orca-runtime、orca-provider）；实验用 curl、python3、Swift 脚本和 macOS 自带的 `screencapture`、`sips`。

**Spec:** `docs/superpowers/specs/2026-09-27-computer-use-design.md`

**范围说明：**
- 本计划覆盖设计第 9 节的三个实验和第 8 节的阶段 1（第 4 节）。
- 与设计的一处出入：第 4 节的 TUI 截图预览推迟到阶段 2。预览需要让 typed surface 的工具结果携带图片，并决定账本里存图片数据还是存资源引用，这个决定和阶段 2 的 observe 流程一起做更合适。阶段 1 只在工具输出里加文字标注 `[N image(s) attached]`，TUI 工具行会直接显示它。

## Global Constraints

- 阶段 1 与平台无关：所有改动都要在 Linux、Windows、macOS 上编译并通过测试。只有实验 2、3 依赖 macOS。
- 新增字段一律可选，为空时不序列化（`#[serde(default, skip_serializing_if = "Vec::is_empty")]`），旧会话历史和旧账本照常读取。
- 只有路由结果为 `ImageRouteDecision::Direct` 的模型才附带图片；其他模型的图片换成说明文字。
- 发给模型时只保留最近 3 张工具图片，更早的替换为说明文字。这只影响请求，不改变持久化内容。
- 工具图片只接受 `image/png`、`image/jpeg`、`image/gif`、`image/webp`，解码后不超过 5 MiB（与 `crates/orca-runtime/src/mentions.rs` 的 `MAX_INLINE_IMAGE_BYTES` 相同）。
- 面向模型的说明文字用英文，与现有工具输出一致。
- 每次提交前：`cargo fmt --all -- --check`；`cargo clippy --workspace --all-targets --locked` 无错误；`node scripts/validate-runtime-surface-contract.mjs` 和 `node scripts/validate-windows-platform-boundaries.mjs` 通过。
- 实验用真实 API key：把 `~/.orca/auth.json` 复制到临时目录并设为 600 权限，用完删除；不要改动原文件的权限。

## Review Focus

1. **恢复会话**：历史里的工具截图不会被重新发给模型，改为留一句说明。由任务 6 的 `a_resumed_session_drops_tool_images_with_a_note` 钉住。
2. **选 Pro 时 MCP 工具返回了图片**：图片不会发给 Pro，模型看到说明文字，也不会触发视觉转述。由任务 7 的 `a_model_that_cannot_see_images_gets_a_note_instead_of_tool_images` 钉住。
3. **一轮里多个工具调用都返回图片，总数超过 3 张**：只发最新 3 张，而且 tool 消息紧跟助手工具调用的顺序不被打乱。由任务 8 的 `only_the_three_newest_tool_images_are_sent` 和配对测试钉住。
4. **不支持的类型、超过 5 MiB 或 base64 损坏的图片**：丢弃并留说明，工具调用本身照常完成，不报错。由任务 4 的三个 `tool_image_*` 测试和任务 5 的 `an_unsupported_mcp_image_leaves_a_note` 钉住。
5. **旧会话历史**（tool 消息没有 `images` 字段）照常加载。由任务 4 的序列化测试和任务 6 的 `history_without_tool_images_loads_unchanged` 钉住。

---

### Task 1: 实验 1——DeepSeek 是否接受 tool 消息带图片

**Files:**
- 临时脚本放在临时目录，不提交。
- Modify: `docs/superpowers/specs/2026-09-27-computer-use-design.md`（第 9 节第 1 条补"结论"）

**Interfaces:**
- Produces：映射方式二选一，写进设计第 9 节，任务 8 按它实现：
  - `in-tool`：图片放进 tool 消息。
  - `trailing-user`：图片放在工具块之后的一条用户消息里。

- [ ] **Step 1：准备临时 key 和测试图片**

  按 Global Constraints 复制 key。用 python3 标准库（`zlib`、`struct`）生成一张 64×64 的纯红色 PNG，转成 base64。

- [ ] **Step 2：请求 A——图片放进 tool 消息**

  `POST https://api.deepseek.com/chat/completions`，`model` 为 `deepseek-flash`，消息序列：
  1. 用户消息："Call the screenshot tool, then tell me the color of the square."
  2. 助手消息：`tool_calls` 里有一个调用，名为 `screenshot`，`id` 为 `call_1`。
  3. tool 消息：`tool_call_id` 为 `call_1`，`content` 为数组 `[{"type":"text","text":"screenshot taken"},{"type":"image_url","image_url":{"url":"data:image/png;base64,<...>"}}]`。

  请求里带上 `screenshot` 工具的定义，参数为空对象。

  Expected：记录 HTTP 状态码，以及回答里有没有说出 red。

- [ ] **Step 3：请求 B——图片放在工具块之后的用户消息里**

  同样的前两条消息。tool 消息的 `content` 改为纯文本 `"screenshot taken"`，之后追加用户消息，`content` 为 `[{"type":"text","text":"Images returned by the tool calls above:"},{"type":"image_url","image_url":{"url":"data:image/png;base64,<...>"}}]`。

  Expected：记录状态码，以及回答里有没有说出 red。

- [ ] **Step 4：定结论并删除临时 key**

  - 请求 A 返回 200 且答出 red：选 `in-tool`。
  - 否则请求 B 答出 red：选 `trailing-user`。
  - 两者都不行：停下来，向用户报告，不进入任务 4。

  在设计第 9 节第 1 条后写"结论：……"，写明两次请求的状态码和回答要点，然后删除临时 key。

- [ ] **Step 5：提交**

```bash
git add docs/superpowers/specs/2026-09-27-computer-use-design.md
git commit -m "docs: record whether DeepSeek takes images in tool messages"
```

### Task 2: 实验 2——Flash 的坐标准确度

**Files:**
- 临时 Swift 脚本和截图放在临时目录，不提交。
- Modify: 设计第 9 节第 2 条

**Interfaces:**
- Produces：命中率，以及按下面规则得出的结论，写进设计，供阶段 2 的计划使用。

- [ ] **Step 1：写一个 Swift 脚本，读出主窗口的元素位置**

  脚本 `ax_frames.swift`：
  - 参数是 bundle id。
  - 用 `AXUIElementCreateApplication` 取得应用的主窗口，遍历元素。
  - 输出 CGWindowID，以及每个可交互元素的角色、标题和屏幕坐标下的 `(x,y,w,h)`。

  终端需要已授予辅助功能和屏幕录制权限。

- [ ] **Step 2：截图并缩小**

  对文本编辑、Finder、系统设置三个应用的主窗口分别执行：
  - `screencapture -l <windowid> -o shot.png`
  - `sips -Z 1280 shot.png`

  记录缩放比例，把元素位置换算到截图像素。

- [ ] **Step 3：让 Flash 给坐标**

  每个应用选 5 个带标题的元素，共 15 个。每次把截图作为用户消息的图片发给 `deepseek-flash`，提问："Return only JSON {\"x\":..,\"y\":..}: the pixel center of the element labelled '<标题>' in this screenshot."

  坐标落在该元素的框内（换算后）算命中。

- [ ] **Step 4：写结论**

  - 命中率 ≥ 80%：像素坐标可以作为通用的备用方式。
  - 50%–80%：像素坐标只用于画布类界面。
  - < 50%：只保留元素编号，像素坐标仅在没有元素树时使用，并在工具说明里提示可能不准。

  把各应用的命中数和结论写进设计第 9 节第 2 条，然后删除临时 key。

- [ ] **Step 5：提交**

```bash
git add docs/superpowers/specs/2026-09-27-computer-use-design.md
git commit -m "docs: record how well Flash locates elements in screenshots"
```

### Task 3: 实验 3——只用公开接口的后台投递

**Files:**
- 临时 Swift 脚本放在临时目录，不提交。
- Modify: 设计第 9 节第 3 条

**Interfaces:**
- Produces：一张"应用类型 × 投递方式 → 是否生效"的表，以及每类应用在阶段 2 默认用哪种方式、是否需要前台回退。

- [ ] **Step 1：写投递探针 `bg_probe.swift`**

  参数是 bundle id 和目标元素的标题。脚本先确认目标应用不在前台（用另一个应用占住前台），再依次尝试四种投递方式：
  1. 对按钮执行 `AXUIElementPerformAction(kAXPressAction)`；
  2. 对输入框用 `AXUIElementSetAttributeValue(kAXValueAttribute)` 直接设值；
  3. 用 `CGEvent` 构造 `abc` 三个按键，`postToPid` 到目标进程；
  4. 用 `CGEvent` 构造一次左键点击，`postToPid` 到元素中心。

  每种方式执行后通过 AX 重新读取，判断是否生效，比如值变了或按下后出现了预期的窗口，最后输出结果表。

- [ ] **Step 2：在三类应用上运行**

  - 文本编辑（AppKit）
  - VS Code（Electron）
  - 播客（Catalyst）

- [ ] **Step 3：写结论**

  把结果表写进设计第 9 节第 3 条，并写明：
  - 每类应用在阶段 2 默认用哪种投递方式；
  - 哪类应用需要前台回退；
  - 是否需要另行讨论私有接口（第一版不引入）。

- [ ] **Step 4：提交**

```bash
git add docs/superpowers/specs/2026-09-27-computer-use-design.md
git commit -m "docs: record which background input methods reach each app kind"
```

### Task 4: 核心类型——工具结果和 tool 消息带图片

**Files:**
- Create: `crates/orca-core/src/tool_images.rs`
- Modify: `crates/orca-core/src/lib.rs`（加 `pub mod tool_images;`）
- Modify: `crates/orca-core/Cargo.toml`（加 `base64 = { workspace = true }`，工作区已有这个依赖；`Cargo.lock` 随之更新）
- Modify: `crates/orca-core/src/conversation.rs`（`Message::Tool` 在第 111 行附近；`add_tool_result`、`add_tool_result_with_terminal` 在第 517–533 行）
- Modify: `crates/orca-core/src/tool_types.rs`（`ToolResult` 在第 755 行附近）
- Modify: 编译器报出的所有构造 `Message::Tool { .. }` 的地方，补 `images: Vec::new()`（已知有 `crates/orca-runtime/src/session.rs`、`thread_store.rs`、`thread_store/types.rs`、`agent_continuation.rs`）

**Interfaces:**
- Produces（`orca_core::tool_images`）：
  - `pub const TOOL_IMAGE_MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];`
  - `pub const MAX_TOOL_IMAGE_BYTES: usize = 5 * 1024 * 1024;`
  - `pub const RESUMED_TOOL_IMAGE_NOTE: &str = "[image omitted: not kept across session resume]";`
  - `pub const TOOL_IMAGE_UNAVAILABLE_NOTE: &str = "[image omitted: the current model cannot see images]";`
  - `pub const SUPERSEDED_TOOL_IMAGE_NOTE: &str = "[image omitted: superseded by a newer one]";`
  - `pub enum ToolImageRejected { UnsupportedType(String), TooLarge { bytes: usize }, InvalidBase64 }`
  - `impl ToolImageRejected { pub fn note(&self) -> String }`，分别返回：
    - `"[image omitted: unsupported type <type>]"`
    - `"[image omitted: <n.n> MiB is over the 5 MiB limit]"`
    - `"[image omitted: invalid base64 data]"`
  - `pub fn tool_image(media_type: &str, base64_data: String) -> Result<ImageInput, ToolImageRejected>`：成功时返回 `ImageInput { source: ImageSource::Base64 { media_type, data }, detail: ImageDetail::High }`。
  - `pub fn drop_tool_images(messages: &mut [Message], note: &str) -> usize`：清空每条 tool 消息的图片；每条原本有图片的消息在 `content` 末尾追加一次 `"\n" + note`；返回丢弃的图片数。用户消息不受影响。
  - `pub fn keep_newest_tool_images(messages: &mut [Message], keep: usize) -> usize`：按消息顺序、再按消息内顺序，只保留最后 `keep` 张工具图片；更早图片所在的 tool 消息追加一次 `SUPERSEDED_TOOL_IMAGE_NOTE`；返回丢弃数。
- Produces（`orca_core::conversation`）：
  - `Message::Tool { tool_call_id: String, content: String, images: Vec<ImageInput>, terminal: Option<ToolTerminal>, pinned: bool }`
  - `impl Message { pub fn images(&self) -> &[ImageInput] }`：用户消息和 tool 消息返回各自的图片，其他返回空切片。
  - `add_tool_result_with_terminal` 把 `result.images` 复制到新消息。
- Produces（`orca_core::tool_types`）：
  - `ToolResult` 新增 `#[serde(default, skip_serializing_if = "Vec::is_empty")] pub images: Vec<ImageInput>`。所有现有构造器都初始化为空。
  - `pub fn with_images(mut self, images: Vec<ImageInput>) -> Self`

- [ ] **Step 1：写失败的测试**

  在 `tool_images.rs` 的测试模块里定义 `const BASE64_1X1_PNG: &str`，内容是任意一张合法 1×1 PNG 的 base64，后面的任务照这个做法各自定义。然后写：

```rust
#[test]
fn tool_image_accepts_supported_types_within_the_limit() {
    let image = tool_image("image/png", BASE64_1X1_PNG.to_string()).unwrap();
    assert!(matches!(image.source, ImageSource::Base64 { ref media_type, .. } if media_type == "image/png"));
    assert_eq!(image.detail, ImageDetail::High);
}

#[test]
fn tool_image_rejects_an_unsupported_type_with_a_note() {
    let rejected = tool_image("image/svg+xml", "PHN2Zz4=".to_string()).unwrap_err();
    assert_eq!(rejected.note(), "[image omitted: unsupported type image/svg+xml]");
}

#[test]
fn tool_image_rejects_an_image_over_5_mib() {
    let data = base64::engine::general_purpose::STANDARD.encode(vec![0u8; MAX_TOOL_IMAGE_BYTES + 1]);
    assert!(matches!(tool_image("image/png", data), Err(ToolImageRejected::TooLarge { .. })));
}
```

  在 `conversation.rs` 的测试模块里：

```rust
#[test]
fn a_tool_result_keeps_its_images_on_the_tool_message() {
    // ToolResult::completed(...).with_images(vec![png]) 经 add_tool_result_with_terminal
    // 之后，最后一条消息是带 1 张图片的 Message::Tool
}

#[test]
fn drop_tool_images_leaves_a_note_and_keeps_user_images() {
    // [user(1 张图片), tool(2 张图片), tool(无图片)] 调用 drop_tool_images(.., "N") 后：
    // 返回 2；user 仍有 1 张；第一条 tool 无图片且 content 以 "\nN" 结尾；
    // 第二条 tool 的 content 不变
}

#[test]
fn keep_newest_tool_images_drops_the_oldest_first() {
    // tool(a,b)、tool(c)、tool(d,e) 调用 keep_newest_tool_images(.., 3) 后：
    // 保留 c、d、e；第一条 tool 无图片并以 SUPERSEDED_TOOL_IMAGE_NOTE 结尾；返回 2
}
```

  在 `tool_types.rs` 的测试模块里：`a_tool_result_without_images_serializes_as_before`。序列化出的 JSON 里没有 `images` 键；一段不含 `images` 的旧 JSON 能反序列化，且 `images` 为空。

- [ ] **Step 2：运行，确认失败**

  Run: `cargo nextest run -p orca-core --lib --locked --retries 0 tool_image drop_tool_images keep_newest a_tool_result`
  Expected：编译失败或测试失败，因为这些函数和字段还不存在。

- [ ] **Step 3：实现上述接口**

  - base64 解码用新加的 `base64` 依赖（`base64::engine::general_purpose::STANDARD`）。
  - 超限判断用解码后的字节数。
  - 注释写清楚两处用途：`drop_tool_images` 用于恢复会话和不能看图的模型；`keep_newest_tool_images` 只在发请求前使用。

- [ ] **Step 4：补齐所有 `Message::Tool` 构造处，运行测试**

  Run: `cargo check --workspace --all-targets --locked`，再运行 Step 2 的命令。
  Expected：编译通过，测试通过。

- [ ] **Step 5：提交**

```bash
git add crates/orca-core crates/orca-runtime
git commit -m "feat(core): let tool results and tool messages carry images"
```

### Task 5: MCP 图片进入工具结果

**Files:**
- Modify: `crates/orca-core/src/mcp_types.rs`（`McpContent` 在第 163 行附近）
- Modify: `crates/orca-mcp/src/client.rs`（`call_tool_inner` 在第 634 行附近，`McpCallOutput` 在第 1060 行附近）
- Modify: `crates/orca-tools/src/registry.rs`（`mcp_call_result_to_tool_result` 在第 2296 行附近）

**Interfaces:**
- Consumes：任务 4 的 `tool_image`、`ToolImageRejected::note`、`ToolResult::with_images`。
- Produces：
  - `McpContent::Image { data: String, #[serde(rename = "mimeType")] mime_type: String }`
  - `McpCallOutput { output: String, images: Vec<ImageInput>, is_error: bool }`
  - `output` 的组成规则：
    1. 文本部分之间用 `\n` 连接；
    2. 每张被拒的图片追加一行说明；
    3. 有被接受的图片时，最后追加一行 `[1 image attached]` 或 `[N images attached]`；
    4. 既没有文本也没有图片时，保持原来的 `(MCP tool returned no text content)`。
  - `mcp_call_result_to_tool_result`：成功结果附带 `images`；`is_error` 的结果不附带图片。

- [ ] **Step 1：写失败的测试**

  用 `crates/orca-mcp/src/client.rs` 测试里的假客户端（第 233 行附近实现 `call_tool` 的那个），让它返回：

```json
{"content":[{"type":"text","text":"screenshot taken"},{"type":"image","data":"<BASE64_1X1_PNG>","mimeType":"image/png"}]}
```

  测试 `an_image_from_an_mcp_tool_reaches_the_tool_output` 断言：
  - `output == "screenshot taken\n[1 image attached]"`
  - `images.len() == 1`，且媒体类型为 `image/png`

  测试 `an_unsupported_mcp_image_leaves_a_note` 使用 `image/svg+xml`，断言：
  - `output == "screenshot taken\n[image omitted: unsupported type image/svg+xml]"`
  - `images` 为空

  在 `crates/orca-tools/src/registry.rs` 的测试里写 `an_mcp_result_with_images_completes_with_them`：`mcp_call_result_to_tool_result` 的成功结果 `images.len() == 1`；`is_error` 为 true 时 `images` 为空。

- [ ] **Step 2：运行，确认失败**

  Run: `cargo nextest run -p orca-mcp -p orca-tools --lib --locked --retries 0 mcp_image an_image_from an_unsupported an_mcp_result`
  Expected：失败。

- [ ] **Step 3：实现上述接口**

- [ ] **Step 4：运行测试，确认通过**

  运行 Step 2 的命令，再运行 `cargo nextest run -p orca-mcp -p orca-tools --all-targets --locked --profile ci --retries 0`。
  Expected：全部通过。

- [ ] **Step 5：提交**

```bash
git add crates/orca-core/src/mcp_types.rs crates/orca-mcp crates/orca-tools
git commit -m "feat(mcp): keep the images an MCP tool returns"
```

### Task 6: 持久化与恢复

**Files:**
- Modify: `crates/orca-runtime/src/thread_store/types.rs`（`StoredMessage::Tool` 在第 363 行附近，`StoredMessageWire::Tool` 在第 528 行附近，`From<StoredMessage> for Message` 在第 645 行附近，`From<&Message> for StoredMessage`）
- Modify: `crates/orca-runtime/src/thread_store/writer.rs`（`transcript_from_records` 在第 660 行附近）
- Test: `crates/orca-runtime/src/thread_store/storage_tests.rs`

**Interfaces:**
- Consumes：任务 4 的 `drop_tool_images`、`RESUMED_TOOL_IMAGE_NOTE`。
- Produces：
  - `StoredMessage::Tool` 新增 `#[serde(default, skip_serializing_if = "Vec::is_empty")] images: Vec<ImageInput>`。
  - `StoredMessageWire::Tool` 新增 `#[serde(default)] images: Vec<ImageInput>`。
  - 两个方向的 `From` 都原样复制图片，保持无损。`thread_store/projection.rs` 的 `normalized_stored_messages` 会做往返转换，所以不能在转换里丢图片。
  - `transcript_from_records` 建好 `messages` 后调用 `drop_tool_images(&mut messages, RESUMED_TOOL_IMAGE_NOTE)`。这是恢复会话的唯一丢弃点。

- [ ] **Step 1：写失败的测试**

  - `a_tool_image_is_stored_as_an_asset_not_inline`：
    - 用 `SessionWriter` 写入一条带 1 张 base64 PNG 的 `Message::Tool`；
    - 会话文件里的原始 JSONL 行不包含那段 base64；
    - 用 `load_images = true` 读回记录后，图片与写入时相同。
  - `a_resumed_session_drops_tool_images_with_a_note`：对同一个会话调用 `load_session`，得到的 `Message::Tool` 没有图片，`content` 以 `"\n[image omitted: not kept across session resume]"` 结尾。
  - `history_without_tool_images_loads_unchanged`：一段手写的旧格式 tool 消息 JSONL 行（没有 `images` 键）能读取，`images` 为空，`content` 不变。

- [ ] **Step 2：运行，确认失败**

  Run: `cargo nextest run -p orca-runtime --lib --locked --retries 0 tool_image resumed_session history_without_tool_images`
  Expected：失败。

- [ ] **Step 3：实现上述接口**

- [ ] **Step 4：运行测试，确认通过**

  运行 Step 2 的命令，再运行 `cargo nextest run -p orca-runtime --lib --locked --profile ci --retries 0 thread_store`。
  Expected：全部通过。

- [ ] **Step 5：提交**

```bash
git add crates/orca-runtime/src/thread_store
git commit -m "feat(runtime): store tool images as assets and drop them on resume"
```

### Task 7: runtime——记录工具图片并按模型路由

**Files:**
- Modify: `crates/orca-runtime/src/session.rs`（`record_tool_result_for_agent` 在第 85 行附近）
- Modify: `crates/orca-runtime/src/runtime_turn_opening.rs`（`has_images` 在第 143 行附近）
- Modify: `crates/orca-runtime/src/child_agent_provider_turn.rs`（第 48 行的 `has_images: false`）
- Modify: `crates/orca-runtime/src/image_routing.rs`（`prepare_image_conversation` 的 `DescribeThenContinue` 分支）

**Interfaces:**
- Consumes：任务 4 的 `Message::images`、`drop_tool_images`、`TOOL_IMAGE_UNAVAILABLE_NOTE`。
- Produces：
  - `record_tool_result_for_agent` 写入的 `Message::Tool` 带上 `result.images` 的副本。
  - 两处 `has_images` 都改为 `messages.iter().any(|message| !message.images().is_empty())`，子 agent 用它自己的对话。
  - `DescribeThenContinue` 分支先对准备发出的对话调用 `drop_tool_images(.., TOOL_IMAGE_UNAVAILABLE_NOTE)`。视觉转述只处理用户消息里的图片，工具图片永远不交给转述。

- [ ] **Step 1：写失败的测试**

  - `session.rs`，`a_tool_results_images_are_recorded_on_its_tool_message`：带 1 张图片的 `ToolResult` 经 `record_tool_result_for_agent` 后，对话最后一条 tool 消息有 1 张图片。
  - 路由，`a_tool_image_makes_the_turn_image_bearing`：对话里只有 tool 消息带图片时，`deepseek-flash` 得到 `Direct`，`deepseek-v4-pro` 得到 `DescribeThenContinue`。
  - `image_routing.rs`，`a_model_that_cannot_see_images_gets_a_note_instead_of_tool_images`：只有工具图片、没有用户图片的对话，以 `DescribeThenContinue` 准备后，tool 消息没有图片，`content` 以 `TOOL_IMAGE_UNAVAILABLE_NOTE` 结尾，`usage` 为 `None`（没有发出转述请求）。

- [ ] **Step 2：运行，确认失败**

  Run: `cargo nextest run -p orca-runtime --lib --locked --retries 0 tool_results_images tool_image_makes cannot_see_images`
  Expected：失败。

- [ ] **Step 3：实现上述接口**

- [ ] **Step 4：运行测试，确认通过**

  运行 Step 2 的命令，再运行 `cargo nextest run -p orca-runtime --lib --locked --profile ci --retries 0 image`。
  Expected：全部通过。

- [ ] **Step 5：提交**

```bash
git add crates/orca-runtime/src
git commit -m "feat(runtime): route turns by the images tools return"
```

### Task 8: 请求映射、只保留最近 3 张、计入 token，并更新文档

**Files:**
- Modify: `crates/orca-provider/src/deepseek_http.rs`（`conversation_to_api_messages` 在第 960 行附近，`Message::Tool` 的映射在第 1051 行附近）
- Modify: `crates/orca-provider/src/context.rs`（`message_tokens_with_counter` 的 `Message::Tool` 分支在第 481 行附近；历史图片预算在第 975 行附近的 `image_costs`）
- Modify: `site/src/docs/md/en/mcp-integration.mdx`、`site/src/docs/md/zh/mcp-integration.mdx`

**Interfaces:**
- Consumes：任务 1 的映射结论；任务 4 的 `keep_newest_tool_images`。
- Produces：
  - `const MAX_REQUEST_TOOL_IMAGES: usize = 3;`
  - `conversation_to_api_messages` 先在副本上调用 `keep_newest_tool_images(.., MAX_REQUEST_TOOL_IMAGES)`，再按实验 1 的结论映射：
    - `in-tool`：tool 消息的 `ApiMessage.images` 填入它的图片（和用户消息一样输出为 `image_url` 内容块）。
    - `trailing-user`：tool 消息只带文字。一段连续 tool 消息的最后一条之后，追加一条 `role: "user"` 的消息，`content` 为 `"Images returned by the tool calls above:"`，`images` 按原顺序放入这段 tool 消息的全部图片。
  - `context.rs`：tool 消息的 token 加上 `images.iter().map(image_tokens).sum::<usize>()`，并计入历史图片预算的 `image_costs`。

- [ ] **Step 1：写失败的测试**

  在 `deepseek_http.rs` 的测试里：
  - 映射测试，按实验 1 的结论写其中一个：
    - `tool_images_are_sent_inside_the_tool_message`：`value[2]["role"] == "tool"`，`value[2]["content"][1]["type"] == "image_url"`。
    - `tool_images_follow_the_tool_block_as_a_user_message`：`value[3]["role"] == "user"`，`value[3]["content"][1]["type"] == "image_url"`，`value[2]["content"]` 是纯字符串。
  - `only_the_three_newest_tool_images_are_sent`：三条 tool 消息共 5 张图片，请求里只有最新 3 张；第一条 tool 消息的文字以 `SUPERSEDED_TOOL_IMAGE_NOTE` 结尾。
  - 仅 `trailing-user` 时写 `tool_messages_still_follow_their_tool_calls`：两个并行工具调用各返回一张图，请求顺序为 assistant（tool_calls）、tool、tool、user（2 张图），中间没有插入其他消息。

  在 `context.rs` 的测试里写 `tool_images_count_toward_message_tokens`：带 1 张 High 图片的 tool 消息，token 数等于同样文字的 tool 消息加 2048。

- [ ] **Step 2：运行，确认失败**

  Run: `cargo nextest run -p orca-provider --lib --locked --retries 0 tool_images three_newest still_follow`
  Expected：失败。

- [ ] **Step 3：实现上述接口**

- [ ] **Step 4：运行测试，确认通过**

  运行 Step 2 的命令，再运行 `cargo nextest run -p orca-provider --all-targets --locked --profile ci --retries 0`。
  Expected：全部通过。

- [ ] **Step 5：更新文档**

  在两份 `mcp-integration.mdx` 里各加一小节"工具返回的图片"（英文版对应标题），写明：
  - 能看图的模型（Flash、auto）会看到 MCP 工具返回的图片；Pro 只看到一句说明。
  - 支持的类型和 5 MiB 上限；不支持的会被丢弃并留说明。
  - 每次请求只带最新 3 张。
  - 恢复会话时不会重新加载旧截图。

  Run: `npm --prefix site run build`，以及 `cargo nextest run -p blade-deepseek --test public_docs_contract --locked --retries 0`。
  Expected：构建成功，文档契约测试通过。

- [ ] **Step 6：提交**

```bash
git add crates/orca-provider site/src/docs/md
git commit -m "feat(provider): send tool images to models that can see them"
```

### Task 9: 阶段收尾验证

**Files:** 无新改动；有失败就回到对应任务修复。

- [ ] **Step 1：全量检查**

  依次运行：
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --locked`（无错误）
  - `node scripts/validate-runtime-surface-contract.mjs`
  - `node scripts/validate-windows-platform-boundaries.mjs`

  Expected：全部通过。

- [ ] **Step 2：全量测试**

  Run:
  - `cargo nextest run --workspace --all-targets --locked --profile ci --no-fail-fast --retries 0`
  - `cargo nextest run -p orca-tui --lib --locked --profile ci-serial --retries 0`
  - `cargo nextest run --test tui_pty_contract --locked --profile ci-serial --retries 0`

  Expected：全部通过。

- [ ] **Step 3：端到端手动验证**

  在 TUI 里用一个会返回截图的 MCP 工具（比如 Pilion 的 `browser_screenshot`），分别在 Flash 和 Pro 下调用一次：
  - Flash 能描述截图内容；
  - Pro 的工具行和回答里能看到"当前模型不能看图"的说明；
  - 工具行显示 `[1 image attached]`。

  这一步使用真实 key，按 Global Constraints 处理。
