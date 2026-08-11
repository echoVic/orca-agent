import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
  type ReactNode,
} from "react";
import {
  applySeoHead,
  canonicalOrigin,
  detectInitialLocale,
  links,
  localeStorageKey,
  releaseVersion,
  type Locale,
  type SeoEntry
} from "./shared";

const npmCommand = "npm install -g @blade-ai/orca";
const curlCommand = "curl -fsSL https://orcaagent.dev/install.sh | sh";
const powershellCommand =
  "& ([scriptblock]::Create((irm https://orcaagent.dev/install.ps1))) -SetupSandbox";

const canonicalUrl = `${canonicalOrigin}/`;

const seoCopy = {
  en: {
    title: "Orca - DeepSeek-native terminal coding agent",
    description:
      "Orca is a DeepSeek-native local terminal coding agent for long-context coding, multi-turn tool use, prefix-cache friendly prompts, persistent goals, resumable history, and approval-aware automation.",
    ogTitle: "Orca - DeepSeek-native terminal coding agent",
    ogDescription:
      "Run DeepSeek-native coding work locally with long context, multi-turn tools, prefix-cache friendly prompts, persistent goals, resumable history, and verifier-gated automation.",
    imageAlt: "Orca terminal coding agent product preview",
    locale: "en_US",
  },
  zh: {
    title: "Orca - DeepSeek 原生终端代码智能体",
    description:
      "Orca 是 DeepSeek 原生的本地终端代码智能体，支持长上下文编码、多轮工具调用、前缀缓存友好提示词、持久 goal、可恢复历史和审批可控自动化。",
    ogTitle: "Orca - DeepSeek 原生终端代码智能体",
    ogDescription:
      "在本地终端运行 DeepSeek 原生代码任务，覆盖长上下文、多轮工具、前缀缓存友好提示词、持久 goal、可恢复历史和 verifier 校验自动化。",
    imageAlt: "Orca 终端代码智能体产品预览",
    locale: "zh_CN",
  },
} as const;

const copy = {
  en: {
    langName: "English",
    aria: {
      home: "Orca home",
      nav: "Main navigation",
      language: "Language",
      tui: "Orca TUI preview",
      commands: "Command examples",
      install: "Install commands",
    },
    nav: {
      features: "Features",
      useCases: "Use cases",
      capabilities: "Capabilities",
      workflow: "Workflow",
      faq: "FAQ",
      docs: "Docs",
      install: "Install",
      changelog: "Changelog",
      github: "GitHub",
    },
    hero: {
      pill: `${releaseVersion} · Rust-native`,
      titlePrefix: "A",
      titleHighlight: "DeepSeek-native",
      titleSuffix: "coding agent, in your terminal",
      subtitle:
        "Orca is a local terminal coding agent built around DeepSeek: long-context coding, multi-turn tool use, prefix-cache friendly prompts, resumable history, persistent goals, and approval-aware automation in one Rust binary.",
      primary: "Get started",
      secondary: "View on GitHub",
      meta: {
        context: "context window",
        turns: "max turns",
        tools: "tool surfaces",
        platforms: "platforms",
        cache: "prefix-cache hit",
      },
    },
    featuresEyebrow: "What you'll notice",
    featuresTitle: "Built around DeepSeek, not around a generic agent shell.",
    features: [
      {
        title: "DeepSeek-native",
        body: "Built around DeepSeek reasoning, max reasoning effort by default, SSE streaming, tool-use semantics, and prefix-cache behavior. One orca exec hands off the task — no context switch.",
      },
      {
        title: "1M context, self-managed",
        body: "A 1M token window with automatic compaction past the 80% threshold, preserving the system prompt and recent turns. Long tasks keep their context.",
      },
      {
        title: "Persistent goal mode",
        body: "Set a long-running objective with /goal; it auto-continues after each successful turn, survives restarts, and only lets the model complete or block after a goal audit.",
      },
      {
        title: "Approval modes",
        body: "Tool specs declare capabilities; reads run directly, while write, shell, network, and agent actions follow your configured approval and sandbox policy.",
      },
      {
        title: "Skills and user input",
        body: "Markdown skills can be explicitly injected with $skill ids, and the TUI can answer structured ask_user_question prompts without ending the turn.",
      },
      {
        title: "Resumable history",
        body: "Local JSONL transcripts support --resume for continuation and --fork for branching.",
      },
    ],
    cacheCard: {
      eyebrow: "Prefix cache",
      title: "Tuned for DeepSeek prefix cache, end-to-end.",
      body: "Nine rounds of real-API tuning. The wire prompt stays append-only at the byte level — system, tools, history, summary baseline — so multi-turn loops, compaction, and resume hold a stable prefix instead of redoing it each turn.",
      stats: [
        { k: "99%", l: "post-compaction main-loop hit (real API)" },
        { k: "92%", l: "non-compacted short-task hit" },
        { k: "0", l: "duplicate remote summary calls (hashed cache)" },
      ],
    },
    quickStart: {
      eyebrow: "Quick Start",
      title: "Install once, then just run orca.",
      subtitle:
        "After installation, start Orca from your terminal. The first run guides you through the DeepSeek API key, then drops you straight into the interactive coding TUI.",
      steps: [
        {
          k: "01",
          title: "Install",
          body: "Use npm for the fastest path, or choose the native curl or PowerShell installer below.",
          code: ["npm install -g @blade-ai/orca"],
        },
        {
          k: "02",
          title: "Run Orca",
          body: "Launch the TUI. If no API key is configured yet, Orca opens the setup flow and saves it for future sessions.",
          code: ["orca"],
        },
        {
          k: "03",
          title: "Start coding",
          body: "Type the task in the interactive terminal. For automated runs later, add verifier commands from the exec workflow.",
          code: ['› fix the failing auth test'],
        },
      ],
    },
    useCasesEyebrow: "Common dev tasks",
    useCasesTitle: "The loops you already run, with more memory and fewer handoffs.",
    useCases: [
      {
        title: "Fix failing tests",
        body: "Trace the failure, edit the right files, run the verifier, and keep the transcript for review.",
      },
      {
        title: "Long refactors",
        body: "Use persistent goals and automatic continuation when one turn is not enough to land the change.",
      },
      {
        title: "Codebase archaeology",
        body: "Search, read, and summarize a repo without losing the path from evidence to conclusion.",
      },
      {
        title: "Release checks",
        body: "Combine command execution, JSONL events, and release notes into an auditable final pass.",
      },
      {
        title: "Reusable workflows",
        body: "Run project or user workflows from .orca/workflows/ when a process deserves a repeatable shape.",
      },
      {
        title: "Approval-aware automation",
        body: "Let reads flow quickly while edits, shell, network, and agent actions respect policy.",
      },
    ],
    capabilitiesEyebrow: "Control surface",
    capabilitiesTitle: "Inspect, resume, and gate every turn.",
    capabilitiesSubtitle:
      "From prompt to tool call to result, Orca keeps coding runs readable, verifiable, and resumable instead of hiding them behind a black box.",
    builtInToolsLabel: "Built-in tools",
    capabilities: [
      {
        title: "Sessions & resume",
        body: "Local JSONL transcripts under ~/.orca/sessions/, with --resume to continue and --fork to branch a new run.",
      },
      {
        title: "Verifier loop",
        body: "Pass --verifier \"cargo test\" to gate a run on a real command; pass/fail is reported with exit code 2 on failure.",
      },
      {
        title: "Sandbox & hooks",
        body: "Permission profiles can scope filesystem and network access, while lifecycle hooks return structured JSON to deny, modify, or inject context.",
      },
      {
        title: "Structured event stream",
        body: "--output-format jsonl emits readable session, reasoning, approval, tool, workflow, and completion events for automation or audit trails.",
      },
    ],
    workflowEyebrow: "Command surface",
    workflowTitle: "One set of verbs across the real dev loop.",
    codeTabs: {
      exec: "orca exec",
      goal: "Persistent goal",
      history: "Resume",
      config: "Config",
    },
    code: {
      execTask: "Hand it a task",
      execRefactor: "Refactor in full-auto",
      execModel: "Pick a model + gate on a verifier",
      execDone: "  ✓ 3 files edited · cargo test passed · exit 0",
      goalSet: "Set a long-running objective; it auto-continues",
      goalEdit: "update + reactivate",
      goalPause: "stop auto-continuation",
      goalResume: "continue when idle",
      goalStored: "  Stored by session id in ~/.orca/goals_1.json — survives restarts.",
      historyBrowse: "Resume or branch a saved conversation",
      historyStored:
        "  Stored under ~/.orca/sessions/YYYY/MM/DD/ for future resume.",
      configMainLoop: "main loop v4-pro, aux tasks v4-flash",
      configReasoning: "max reasoning effort",
      configPriority: "Priority: env vars > CLI args > config file > defaults",
    },
    comparison: {
      eyebrow: "Why teams pick Orca",
      title: "A local DeepSeek workflow, not another chat window.",
      columns: [
        {
          title: "Regular AI chat",
          items: [
            "Context disappears between tasks.",
            "File edits and test runs stay manual.",
            "Reasoning settings are detached from the dev loop.",
          ],
        },
        {
          title: "Generic coding agents",
          items: [
            "DeepSeek behavior is treated as a generic OpenAI-compatible backend.",
            "Long-context and prefix-cache behavior are rarely visible.",
            "Resume, audit, and approval contracts vary by surface.",
          ],
        },
        {
          title: "Orca",
          items: [
            "DeepSeek routing, max reasoning effort, and prefix-cache-friendly prompts are first-class.",
            "Persistent goals, workflows, and verifier gates keep long tasks moving.",
            "Local JSONL transcripts, resume/fork, approval policies, and sandbox hooks stay inspectable.",
          ],
        },
      ],
    },
    faq: {
      eyebrow: "Frequently asked questions",
      title: "Answers before you install.",
      items: [
        {
          q: "Does Orca only work with DeepSeek?",
          a: "Orca is tuned for DeepSeek V4 and gives the best experience there, but the config can point at an OpenAI-compatible endpoint when your team needs that.",
        },
        {
          q: "Will my session history be lost?",
          a: "No. Orca stores local JSONL transcripts under ~/.orca/sessions/ and supports --resume and --fork.",
        },
        {
          q: "Can I keep control over edits and shell commands?",
          a: "Yes. Built-in, MCP, and external tools declare capabilities, and approval modes plus sandbox profiles decide what can run automatically.",
        },
        {
          q: "What makes persistent goals different from a long prompt?",
          a: "A /goal survives restarts, auto-continues successful turns, and requires an explicit complete or blocked audit before Orca stops.",
        },
      ],
    },
    specsEyebrow: "Technical specs",
    specsLabel: "Technical specs",
    specs: {
      context: "Context window, auto-compacted past the 80% threshold.",
      platforms: "Native binaries: macOS, Linux, and Windows, arm64 and x64.",
      tools: "Built-in, MCP, and external tools share one spec-driven registry.",
      rust: "Written in Rust, running as a single local binary.",
    },
    install: {
      eyebrow: "Install",
      title: "Use npm, or install the native binary directly.",
      cardLabel: "Install Orca",
      methodLabel: "Install method",
      copy: "Copy",
      copied: "✓ Copied",
      failed: "Failed",
      platforms:
        "Supported platforms: macOS, Linux, and Windows on arm64/x64. Downloads are available on",
      releases: "GitHub Releases",
    },
    community: {
      qq: "QQ Group 472309526",
      telegram: "Telegram",
    },
    tui: {
      user: "fix the failing auth test",
      reasoning: "reasoning",
      reason1: "locating the failing case, checking the token-expiry comparison…",
      reason2Prefix: "expiry uses",
      reason2Middle: "; the boundary second is wrongly valid — should be",
      approve: "⚑ approval · edit src/auth/token.rs",
      approved: "approved",
      grepResult: "→ 3 assertions matched",
      readResult: "→ read 86 lines",
      editResult: "→ 1 change written",
      bashResult: "→ running 4 tests…",
      ok: "✓ test auth::token_expiry ... ok · 4 passed",
      done: "✓ done · 1 file changed · cargo test passed · exit 0",
      footerBacktrack: "backtrack",
      footerGoal: "goal",
      footerExit: "exit",
      statusContext: "context",
    },
  },
  zh: {
    langName: "中文",
    aria: {
      home: "Orca 首页",
      nav: "主导航",
      language: "语言",
      tui: "Orca TUI 预览",
      commands: "命令示例",
      install: "安装命令",
    },
    nav: {
      features: "特性",
      useCases: "场景",
      capabilities: "能力",
      workflow: "工作流",
      faq: "FAQ",
      docs: "文档",
      install: "安装",
      changelog: "更新日志",
      github: "GitHub",
    },
    hero: {
      pill: `${releaseVersion} · Rust 原生`,
      titlePrefix: "面向终端的",
      titleHighlight: "DeepSeek 原生",
      titleSuffix: "代码智能体",
      subtitle:
        "Orca 是围绕 DeepSeek 构建的本地终端代码智能体：长上下文编码、多轮工具调用、前缀缓存友好提示词、可恢复历史、持久 goal，以及带审批策略的自动化，都内建在一个 Rust 二进制里。",
      primary: "开始使用",
      secondary: "查看 GitHub",
      meta: {
        context: "上下文窗口",
        turns: "最大轮次",
        tools: "工具面",
        platforms: "支持平台",
        cache: "前缀缓存命中",
      },
    },
    featuresEyebrow: "你会注意到",
    featuresTitle: "围绕 DeepSeek 构建，而不是套一层通用智能体外壳。",
    features: [
      {
        title: "DeepSeek 原生",
        body: "围绕 DeepSeek 推理构建，默认启用 max reasoning effort，并适配 SSE 流式输出、工具调用语义和前缀缓存行为。一个 orca exec 就能交付任务，不必切换上下文。",
      },
      {
        title: "1M 上下文，自主管理",
        body: "1M token 上下文窗口，超过 80% 阈值后自动压缩，同时保留系统提示词和最近对话。长任务也能持续推进。",
      },
      {
        title: "持久化 goal 模式",
        body: "用 /goal 设置长期目标；每轮成功后自动继续，跨进程重启保留，并要求模型经过 goal 审计后才能完成或阻塞。",
      },
      {
        title: "审批模式",
        body: "工具规格声明能力；读取直接运行，写入、shell、网络和 agent 操作按你的审批与沙箱策略执行。",
      },
      {
        title: "Skills 与用户输入",
        body: "Markdown skills 可通过 $skill id 显式注入；TUI 能回答结构化 ask_user_question 问题，并继续同一轮任务。",
      },
      {
        title: "可恢复历史",
        body: "本地 JSONL 会话支持用 --resume 继续，也支持用 --fork 分支。",
      },
    ],
    cacheCard: {
      eyebrow: "前缀缓存",
      title: "为 DeepSeek 前缀缓存做了端到端调优。",
      body: "经过九轮真实 API 验证。Wire 层提示词字节级 append-only —— system、tools、history、summary baseline 全部稳定 —— 多轮循环、压缩与 resume 都复用同一段前缀，而不是每轮重发。",
      stats: [
        { k: "99%", l: "压缩后主链路命中（真实 API）" },
        { k: "92%", l: "非压缩短任务命中" },
        { k: "0", l: "重复 remote summary 调用（哈希缓存）" },
      ],
    },
    quickStart: {
      eyebrow: "快速上手",
      title: "安装一次，然后直接运行 orca。",
      subtitle:
        "安装后在终端输入 orca。首次运行会引导你配置 DeepSeek API key，然后直接进入交互式终端开始写代码。",
      steps: [
        {
          k: "01",
          title: "安装",
          body: "npm 是最快路径；直接安装原生二进制时，可在下方选择 curl 或 PowerShell。",
          code: ["npm install -g @blade-ai/orca"],
        },
        {
          k: "02",
          title: "运行 Orca",
          body: "启动 TUI。如果还没有配置 API key，Orca 会打开初始化引导，并为之后的会话保存配置。",
          code: ["orca"],
        },
        {
          k: "03",
          title: "开始交互",
          body: "进入交互式终端后直接输入任务。后续需要自动化运行时，再用 exec workflow 加 verifier。",
          code: ["› 修复失败的 auth 测试"],
        },
      ],
    },
    useCasesEyebrow: "常见开发任务",
    useCasesTitle: "把你每天已经在跑的循环，变成更长记忆、更少切换的流程。",
    useCases: [
      {
        title: "修复失败测试",
        body: "定位失败原因，编辑相关文件，运行 verifier，并留下可复查的会话记录。",
      },
      {
        title: "长任务重构",
        body: "一轮做不完时，用持久 goal 和自动继续把任务稳稳推进到可交付状态。",
      },
      {
        title: "理解大型代码库",
        body: "搜索、读取、归纳仓库证据，让结论能追溯到真实文件和命令输出。",
      },
      {
        title: "发布前检查",
        body: "把命令执行、JSONL 事件和 release note 串起来，形成可审计的最终检查。",
      },
      {
        title: "复用工作流",
        body: "把团队流程放进 .orca/workflows/，让重复任务不再每次重新讲一遍。",
      },
      {
        title: "可审批自动化",
        body: "读取可以快速发生，写入、shell、网络和 agent 行为则按策略执行。",
      },
    ],
    capabilitiesEyebrow: "控制界面",
    capabilitiesTitle: "每一轮都可检查、可恢复、可校验。",
    capabilitiesSubtitle:
      "从提示词到工具调用再到结果，Orca 让代码任务保持可读、可验证、可恢复，而不是藏在黑箱里。",
    builtInToolsLabel: "内置工具",
    capabilities: [
      {
        title: "会话与恢复",
        body: "JSONL 会话记录存放在 ~/.orca/sessions/，可用 --resume 继续，也可用 --fork 分支出新的运行。",
      },
      {
        title: "验证器循环",
        body: "通过 --verifier \"cargo test\" 用真实命令约束运行；失败会以 exit code 2 报告。",
      },
      {
        title: "沙箱与 Hooks",
        body: "Permission profiles 可限制文件系统与网络访问；生命周期 hooks 可返回结构化 JSON 来拒绝、修改或注入上下文。",
      },
      {
        title: "结构化事件流",
        body: "--output-format jsonl 输出可读的会话、推理、审批、工具、工作流和完成事件，便于自动化或审计。",
      },
    ],
    workflowEyebrow: "命令界面",
    workflowTitle: "用同一套动词覆盖真实开发循环。",
    codeTabs: {
      exec: "orca exec",
      goal: "持久 goal",
      history: "恢复会话",
      config: "配置",
    },
    code: {
      execTask: "交给它一个任务",
      execRefactor: "full-auto 模式重构",
      execModel: "指定模型并用 verifier 校验",
      execDone: "  ✓ 修改 3 个文件 · cargo test 通过 · exit 0",
      goalSet: "设置长期目标；它会自动继续",
      goalEdit: "更新并重新激活",
      goalPause: "停止自动继续",
      goalResume: "空闲时继续",
      goalStored: "  按 session id 存在 ~/.orca/goals_1.json — 重启后仍保留。",
      historyBrowse: "恢复或分支一个已保存会话",
      historyStored: "  存放在 ~/.orca/sessions/YYYY/MM/DD/，供后续恢复。",
      configMainLoop: "主循环 v4-pro，辅助任务 v4-flash",
      configReasoning: "max 推理强度",
      configPriority: "优先级：环境变量 > CLI 参数 > 配置文件 > 默认值",
    },
    comparison: {
      eyebrow: "为什么选择 Orca",
      title: "不是再开一个聊天窗口，而是本地 DeepSeek 开发工作流。",
      columns: [
        {
          title: "普通 AI 聊天",
          items: [
            "任务之间上下文容易断掉。",
            "文件修改和测试运行仍然靠手动搬运。",
            "推理强度和开发闭环是分离的。",
          ],
        },
        {
          title: "通用代码智能体",
          items: [
            "DeepSeek 往往只是一个 OpenAI-compatible 后端。",
            "长上下文和前缀缓存行为不够可见。",
            "恢复、审计和审批契约会随入口变化。",
          ],
        },
        {
          title: "Orca",
          items: [
            "DeepSeek 路由、max reasoning effort 和前缀缓存友好提示词是一等能力。",
            "持久 goal、workflow 和 verifier gate 能支撑更长任务。",
            "本地 JSONL、resume/fork、审批策略和沙箱 hooks 都可检查。",
          ],
        },
      ],
    },
    faq: {
      eyebrow: "常见问题",
      title: "安装前先把疑虑说清楚。",
      items: [
        {
          q: "Orca 只能用 DeepSeek 吗？",
          a: "Orca 针对 DeepSeek V4 做了调优，在 DeepSeek 上体验最好；如果团队需要，也可以把配置指向 OpenAI-compatible endpoint。",
        },
        {
          q: "会话历史会不会丢？",
          a: "不会。Orca 在 ~/.orca/sessions/ 下保存本地 JSONL transcript，并支持 --resume 和 --fork。",
        },
        {
          q: "我能控制文件修改和 shell 命令吗？",
          a: "可以。内置、MCP 和 external 工具都会声明能力，approval mode 与 sandbox profile 决定哪些动作能自动运行。",
        },
        {
          q: "持久 goal 和写一个长 prompt 有什么不同？",
          a: "/goal 会跨重启保留，成功后自动继续，并要求 Orca 明确审计为 complete 或 blocked 后才停止。",
        },
      ],
    },
    specsEyebrow: "技术规格",
    specsLabel: "技术规格",
    specs: {
      context: "上下文窗口，超过 80% 阈值后自动压缩。",
      platforms: "原生二进制：macOS、Linux 与 Windows，arm64 / x64。",
      tools: "内置、MCP 和 external 工具共用规格驱动注册表。",
      rust: "Rust 编写，以单个本地二进制运行。",
    },
    install: {
      eyebrow: "安装",
      title: "使用 npm，或直接安装原生二进制。",
      cardLabel: "安装 Orca",
      methodLabel: "安装方式",
      copy: "复制",
      copied: "✓ 已复制",
      failed: "复制失败",
      platforms: "支持平台：macOS、Linux 与 Windows arm64/x64。下载文件位于",
      releases: "GitHub Releases",
    },
    community: {
      qq: "QQ 群 472309526",
      telegram: "Telegram",
    },
    tui: {
      user: "修复失败的 auth 测试",
      reasoning: "推理",
      reason1: "定位失败用例，检查 token 过期时间比较…",
      reason2Prefix: "过期判断使用了",
      reason2Middle: "；边界秒被错误地视为有效，应该改为",
      approve: "⚑ 审批 · edit src/auth/token.rs",
      approved: "已批准",
      grepResult: "→ 匹配到 3 个断言",
      readResult: "→ 读取 86 行",
      editResult: "→ 写入 1 处修改",
      bashResult: "→ 正在运行 4 个测试…",
      ok: "✓ test auth::token_expiry ... ok · 4 passed",
      done: "✓ 完成 · 修改 1 个文件 · cargo test 通过 · exit 0",
      footerBacktrack: "回退",
      footerGoal: "目标",
      footerExit: "退出",
      statusContext: "上下文",
    },
  },
} as const;

const capabilityIcons = [
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7">
    <path d="M21 12a9 9 0 1 1-6.2-8.5" />
    <path d="M21 4v5h-5" />
  </svg>,
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7">
    <path d="M9 11l3 3L22 4" />
    <path d="M21 12v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11" />
  </svg>,
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7">
    <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
    <path d="M14 2v6h6M9 13h6M9 17h6" />
  </svg>,
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7">
    <path d="M4 4h16v12H4z" />
    <path d="M8 20h8M12 16v4M7 9l2 2 6-6" />
  </svg>,
];

const builtinTools = [
  "read_file",
  "glob",
  "edit",
  "grep",
  "bash",
  "write_file",
  "git_status",
  "web_search",
  "subagent",
  "Workflow",
  "update_plan",
  "get_goal",
  "create_goal",
  "update_goal",
  "MCP",
  "external",
];

const installModes = ["npm", "curl", "powershell"] as const;
type InstallMode = (typeof installModes)[number];
type CopyState = "idle" | "copied" | "failed";
type CodeTab = "exec" | "goal" | "history" | "config";

const installTabIds = {
  npm: "install-tab-npm",
  curl: "install-tab-curl",
  powershell: "install-tab-powershell",
} as const;

const installPanelId = "install-panel";

function fallbackCopyText(command: string) {
  const textarea = document.createElement("textarea");

  textarea.value = command;
  textarea.setAttribute("readonly", "");
  textarea.style.position = "fixed";
  textarea.style.top = "0";
  textarea.style.left = "0";
  textarea.style.width = "1px";
  textarea.style.height = "1px";
  textarea.style.padding = "0";
  textarea.style.border = "0";
  textarea.style.outline = "0";
  textarea.style.boxShadow = "none";
  textarea.style.background = "transparent";
  textarea.style.opacity = "0";

  document.body.appendChild(textarea);

  try {
    textarea.focus();
    textarea.select();
    textarea.setSelectionRange(0, textarea.value.length);

    return document.execCommand("copy");
  } finally {
    document.body.removeChild(textarea);
  }
}

async function copyCommandText(command: string) {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(command);
      return true;
    }
  } catch {
    // Fall through to the legacy clipboard path.
  }

  try {
    return fallbackCopyText(command);
  } catch {
    return false;
  }
}

type TuiBlock = {
  kind: "user" | "reason" | "tool" | "approve" | "ok" | "done";
  content: ReactNode;
  ctx: number;
};

function makeTuiBlocks(t: (typeof copy)[Locale]): TuiBlock[] {
  return [
    { kind: "user", ctx: 12, content: <><span className="who">you ›</span> {t.tui.user}</> },
    {
      kind: "reason",
      ctx: 21,
      content: (
        <div className="tb-reason">
          <span className="lbl">{t.tui.reasoning}</span>
          {t.tui.reason1}
        </div>
      ),
    },
    {
      kind: "tool",
      ctx: 30,
      content: (
        <div className="tb-tool">
          <div className="th">
            <span className="ic" />
            grep <span className="arg">"assert_eq" tests/auth.rs</span>
          </div>
          <div className="res">{t.tui.grepResult}</div>
        </div>
      ),
    },
    {
      kind: "tool",
      ctx: 38,
      content: (
        <div className="tb-tool">
          <div className="th">
            <span className="ic" />
            read_file <span className="arg">src/auth/token.rs</span>
          </div>
          <div className="res">{t.tui.readResult}</div>
        </div>
      ),
    },
    {
      kind: "reason",
      ctx: 47,
      content: (
        <div className="tb-reason">
          <span className="lbl">{t.tui.reasoning}</span>
          {t.tui.reason2Prefix} <span style={{ color: "var(--warn)" }}>&lt;=</span>
          {t.tui.reason2Middle} <span style={{ color: "var(--accent-2)" }}>&lt;</span>.
        </div>
      ),
    },
    {
      kind: "approve",
      ctx: 53,
      content: (
        <div className="tb-approve">
          {t.tui.approve}
          <span className="chip">{t.tui.approved}</span>
        </div>
      ),
    },
    {
      kind: "tool",
      ctx: 61,
      content: (
        <div className="tb-tool">
          <div className="th">
            <span className="ic" />
            edit <span className="arg">src/auth/token.rs</span>
          </div>
          <div className="res">{t.tui.editResult}</div>
        </div>
      ),
    },
    {
      kind: "tool",
      ctx: 72,
      content: (
        <div className="tb-tool">
          <div className="th">
            <span className="ic" />
            bash <span className="arg">cargo test auth</span>
          </div>
          <div className="res">{t.tui.bashResult}</div>
        </div>
      ),
    },
    { kind: "ok", ctx: 78, content: <div className="tb-ok">{t.tui.ok}</div> },
    { kind: "done", ctx: 80, content: <div className="tb-done">{t.tui.done}</div> },
  ];
}

function renderCodeTab(tab: CodeTab, t: (typeof copy)[Locale]) {
  const c = t.code;
  const tabs: Record<CodeTab, ReactNode> = {
    exec: (
      <pre>
        <span className="k-com"># {c.execTask}</span>
        {"\n"}
        <span className="k-cmd">orca</span> exec <span className="k-str">"fix this test"</span>
        {"\n\n"}
        <span className="k-com"># {c.execRefactor}</span>
        {"\n"}
        <span className="k-cmd">orca</span> exec <span className="k-flag">--approval-mode</span>{" "}
        full-auto <span className="k-str">"refactor the auth module"</span>
        {"\n\n"}
        <span className="k-com"># {c.execModel}</span>
        {"\n"}
        <span className="k-cmd">orca</span> exec <span className="k-flag">--model</span>{" "}
        deepseek-v4-pro <span className="k-flag">--verifier</span>{" "}
        <span className="k-str">"cargo test"</span>{" "}
        <span className="k-str">"fix the failing test"</span>
        {"\n"}
        <span className="k-out">{c.execDone}</span>
      </pre>
    ),
    goal: (
      <pre>
        <span className="k-com"># {c.goalSet}</span>
        {"\n"}
        <span className="k-cmd">/goal</span> ship the refactor
        {"\n"}
        <span className="k-cmd">/goal</span> edit finish the parser{" "}
        <span className="k-com"># {c.goalEdit}</span>
        {"\n"}
        <span className="k-cmd">/goal</span> pause <span className="k-com"># {c.goalPause}</span>
        {"\n"}
        <span className="k-cmd">/goal</span> resume <span className="k-com"># {c.goalResume}</span>
        {"\n\n"}
        <span className="k-out">{c.goalStored}</span>
      </pre>
    ),
    history: (
      <pre>
        <span className="k-com"># {c.historyBrowse}</span>
        {"\n"}
        <span className="k-cmd">orca</span> <span className="k-flag">--resume</span>
        {"\n"}
        <span className="k-cmd">orca</span> <span className="k-flag">--resume</span> latest
        {"\n"}
        <span className="k-cmd">orca</span> exec <span className="k-flag">--resume</span> latest{" "}
        <span className="k-str">"continue the refactor"</span>
        {"\n"}
        <span className="k-cmd">orca</span> exec <span className="k-flag">--fork</span> latest{" "}
        <span className="k-str">"try another approach"</span>
        {"\n"}
        <span className="k-out">{c.historyStored}</span>
      </pre>
    ),
    config: (
      <pre>
        <span className="k-com"># ~/.orca/config.toml</span>
        {"\n"}
        model = <span className="k-str">"auto"</span>{" "}
        <span className="k-com"># {c.configMainLoop}</span>
        {"\n"}
        reasoning_effort = <span className="k-str">"max"</span>{" "}
        <span className="k-com"># {c.configReasoning}</span>
        {"\n"}
        api_key = <span className="k-str">"sk-..."</span>
        {"\n"}
        base_url = <span className="k-str">"https://api.deepseek.com"</span>
        {"\n\n"}
        <span className="k-com"># {c.configPriority}</span>
      </pre>
    ),
  };

  return tabs[tab];
}

function ctxBar(pct: number) {
  const filled = Math.round(pct / 10);
  return "█".repeat(filled) + "░".repeat(10 - filled);
}

function useTuiAnimation(tuiBlocks: TuiBlock[], tuiUserMsg: string) {
  const [visibleCount, setVisibleCount] = useState(0);
  const [typed, setTyped] = useState("");
  const [phase, setPhase] = useState<"typing" | "streaming">("typing");

  useEffect(() => {
    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    if (reduce) {
      setTyped("");
      setPhase("streaming");
      setVisibleCount(tuiBlocks.length);
      return;
    }

    const timers: number[] = [];
    let cancelled = false;

    function run() {
      setVisibleCount(0);
      setTyped("");
      setPhase("typing");

      // Type the user message into the composer.
      let i = 0;
      const type = () => {
        if (cancelled) return;
        setTyped(tuiUserMsg.slice(0, i));
        if (i <= tuiUserMsg.length) {
          i += 1;
          timers.push(window.setTimeout(type, 46));
        } else {
          timers.push(window.setTimeout(stream, 420));
        }
      };

      // Reveal conversation blocks one by one.
      const stream = () => {
        if (cancelled) return;
        setTyped("");
        setPhase("streaming");
        let n = 0;
        const reveal = () => {
          if (cancelled) return;
          n += 1;
          setVisibleCount(n);
          if (n < tuiBlocks.length) {
            timers.push(window.setTimeout(reveal, 560));
          } else {
            timers.push(window.setTimeout(run, 4200));
          }
        };
        reveal();
      };

      type();
    }

    run();

    return () => {
      cancelled = true;
      timers.forEach((t) => window.clearTimeout(t));
    };
  }, [tuiBlocks, tuiUserMsg]);

  const ctx = visibleCount > 0 ? tuiBlocks[visibleCount - 1].ctx : 8;
  return { visibleCount, typed, phase, ctx };
}

function App() {
  const [mode, setMode] = useState<InstallMode>("npm");
  const [copyState, setCopyState] = useState<CopyState>("idle");
  const [codeTab, setCodeTab] = useState<CodeTab>("exec");
  const [locale, setLocale] = useState<Locale>(detectInitialLocale);
  const resetTimerRef = useRef<number | null>(null);
  const copyRequestRef = useRef(0);
  const tabRefs = useRef<Record<InstallMode, HTMLButtonElement | null>>({
    npm: null,
    curl: null,
    powershell: null,
  });
  const commands: Record<InstallMode, string> = {
    npm: npmCommand,
    curl: curlCommand,
    powershell: powershellCommand,
  };
  const command = commands[mode];
  const t = copy[locale];
  const tuiBlocks = useMemo(() => makeTuiBlocks(t), [t]);
  const tui = useTuiAnimation(tuiBlocks, t.tui.user);

  useEffect(() => {
    window.localStorage.setItem(localeStorageKey, locale);
    applySeoHead(locale, seoCopy[locale] satisfies SeoEntry, canonicalUrl);
  }, [locale]);

  function clearCopyResetTimer() {
    if (resetTimerRef.current !== null) {
      window.clearTimeout(resetTimerRef.current);
      resetTimerRef.current = null;
    }
  }

  function setInstallMode(nextMode: InstallMode) {
    setMode(nextMode);
    copyRequestRef.current += 1;
    setCopyState("idle");
    clearCopyResetTimer();
  }

  useEffect(() => {
    if (copyState === "idle") {
      return;
    }

    clearCopyResetTimer();

    resetTimerRef.current = window.setTimeout(() => {
      setCopyState("idle");
      resetTimerRef.current = null;
    }, 1400);

    return clearCopyResetTimer;
  }, [copyState]);

  async function copyCommand() {
    const requestId = ++copyRequestRef.current;
    const copied = await copyCommandText(command);
    if (requestId !== copyRequestRef.current) {
      return;
    }

    setCopyState(copied ? "copied" : "failed");
  }

  function handleInstallTabKeyDown(
    event: KeyboardEvent<HTMLButtonElement>,
    currentMode: InstallMode,
  ) {
    let nextMode: InstallMode | null = null;

    const currentIndex = installModes.indexOf(currentMode);
    switch (event.key) {
      case "ArrowLeft":
      case "ArrowUp":
        nextMode = installModes[(currentIndex - 1 + installModes.length) % installModes.length];
        break;
      case "ArrowRight":
      case "ArrowDown":
        nextMode = installModes[(currentIndex + 1) % installModes.length];
        break;
      case "Home":
        nextMode = "npm";
        break;
      case "End":
        nextMode = installModes[installModes.length - 1];
        break;
      default:
        return;
    }

    event.preventDefault();
    setInstallMode(nextMode);
    tabRefs.current[nextMode]?.focus();
  }

  const copyLabel =
    copyState === "copied"
      ? t.install.copied
      : copyState === "failed"
        ? t.install.failed
        : t.install.copy;

  return (
    <main>
      <header className="nav">
        <a className="brand" href="#top" aria-label={t.aria.home}>
          <img className="brand-mark" src="/orca-icon.svg" alt="" aria-hidden="true" />
          <span>Orca</span>
        </a>
        <div className="nav-actions">
          <nav aria-label={t.aria.nav}>
            <a href="#features">{t.nav.features}</a>
            <a href="#use-cases">{t.nav.useCases}</a>
            <a href="#capabilities">{t.nav.capabilities}</a>
            <a href="#workflow">{t.nav.workflow}</a>
            <a href="#faq">{t.nav.faq}</a>
            <a href={links.docs}>{t.nav.docs}</a>
            <a href="#install">{t.nav.install}</a>
            <a href={links.changelog}>{t.nav.changelog}</a>
            <a className="nav-cta" href={links.github} rel="noreferrer">
              {t.nav.github}
            </a>
          </nav>
          <div className="locale-switch" role="group" aria-label={t.aria.language}>
            <button
              type="button"
              aria-pressed={locale === "en"}
              aria-label={copy.en.langName}
              onClick={() => setLocale("en")}
            >
              EN
            </button>
            <button
              type="button"
              aria-pressed={locale === "zh"}
              aria-label={copy.zh.langName}
              onClick={() => setLocale("zh")}
            >
              中文
            </button>
          </div>
        </div>
      </header>

      <section className="hero" id="top">
        <div className="hero-copy">
          <span className="pill">
            <span className="dot" />
            {t.hero.pill}
          </span>
          <h1>
            {t.hero.titlePrefix} <span className="grad">{t.hero.titleHighlight}</span>{" "}
            {t.hero.titleSuffix}
          </h1>
          <p className="subtitle">{t.hero.subtitle}</p>

          <div className="actions">
            <a className="primary" href="#install">
              {t.hero.primary}
            </a>
            <a className="secondary" href={links.github} rel="noreferrer">
              {t.hero.secondary}
            </a>
          </div>

          <div className="hero-meta">
            <div>
              <span className="k">1M</span>
              <span className="l">{t.hero.meta.context}</span>
            </div>
            <div>
              <span className="k">99%</span>
              <span className="l">{t.hero.meta.cache}</span>
            </div>
            <div>
              <span className="k">128</span>
              <span className="l">{t.hero.meta.turns}</span>
            </div>
            <div>
              <span className="k">16</span>
              <span className="l">{t.hero.meta.tools}</span>
            </div>
            <div>
              <span className="k">6</span>
              <span className="l">{t.hero.meta.platforms}</span>
            </div>
          </div>
        </div>

        <div className="terminal" aria-label={t.aria.tui}>
          <div className="terminal-bar">
            <span />
            <span />
            <span />
            <span className="terminal-title">orca · TUI — ~/projects/api</span>
          </div>
          <div className="tui-status">
            <span>
              <span className="accent">orca</span>{" "}
              <span className="dim">deepseek-v4-pro</span> ·{" "}
              <span className="dim">full-auto</span>
            </span>
            <span className="tui-ctx">
              {t.tui.statusContext} <span className="bar">{ctxBar(tui.ctx)}</span> {tui.ctx}%
            </span>
          </div>
          <div className="tui-body" aria-hidden="true">
            {tuiBlocks.slice(0, tui.visibleCount).map((block, index) => (
              <div key={index} className={`tb tb-${block.kind}`}>
                {block.content}
              </div>
            ))}
          </div>
          <div className="tui-composer">
            <span className="prompt">›</span>
            <span className="input">{tui.phase === "typing" ? tui.typed : ""}</span>
            <span className="tui-cur" />
          </div>
          <div className="tui-foot">
            <span>
              <span className="key">esc</span> {t.tui.footerBacktrack} ·{" "}
              <span className="key">/goal</span> {t.tui.footerGoal} ·{" "}
              <span className="key">^c</span> {t.tui.footerExit}
            </span>
            <span className="dim">1M · {releaseVersion}</span>
          </div>
        </div>
      </section>

      <section className="features" id="features" aria-labelledby="features-heading">
        <div className="section-heading">
          <p className="eyebrow">{t.featuresEyebrow}</p>
          <h2 id="features-heading">{t.featuresTitle}</h2>
        </div>
        <div className="feature-grid">
          {t.features.map((feature) => (
            <article key={feature.title}>
              <h3>{feature.title}</h3>
              <p>{feature.body}</p>
            </article>
          ))}
        </div>
        <article className="cache-card" aria-labelledby="cache-card-heading">
          <div className="cache-card-copy">
            <p className="eyebrow">{t.cacheCard.eyebrow}</p>
            <h3 id="cache-card-heading">{t.cacheCard.title}</h3>
            <p>{t.cacheCard.body}</p>
          </div>
          <div className="cache-card-stats" aria-hidden="true">
            {t.cacheCard.stats.map((stat) => (
              <div key={stat.l}>
                <span className="num">{stat.k}</span>
                <span className="l">{stat.l}</span>
              </div>
            ))}
          </div>
        </article>
      </section>

      <section className="quick-start" id="quick-start" aria-labelledby="quick-start-heading">
        <div className="section-heading">
          <p className="eyebrow">{t.quickStart.eyebrow}</p>
          <h2 id="quick-start-heading">{t.quickStart.title}</h2>
          <p className="subtitle">{t.quickStart.subtitle}</p>
        </div>
        <div className="quick-grid">
          {t.quickStart.steps.map((step) => (
            <article key={step.k} className="quick-card">
              <span className="step-k">{step.k}</span>
              <h3>{step.title}</h3>
              <p>{step.body}</p>
              <pre>{step.code.join("\n")}</pre>
            </article>
          ))}
        </div>
      </section>

      <section className="use-cases" id="use-cases" aria-labelledby="use-cases-heading">
        <div className="section-heading">
          <p className="eyebrow">{t.useCasesEyebrow}</p>
          <h2 id="use-cases-heading">{t.useCasesTitle}</h2>
        </div>
        <div className="use-case-grid">
          {t.useCases.map((item) => (
            <article key={item.title}>
              <h3>{item.title}</h3>
              <p>{item.body}</p>
            </article>
          ))}
        </div>
      </section>

      <section className="capabilities" id="capabilities" aria-labelledby="capabilities-heading">
        <div className="cap-lead">
          <p className="eyebrow">{t.capabilitiesEyebrow}</p>
          <h2 id="capabilities-heading">{t.capabilitiesTitle}</h2>
          <p className="subtitle">{t.capabilitiesSubtitle}</p>
          <div className="tools-wrap" aria-label={t.builtInToolsLabel}>
            {builtinTools.map((tool) => (
              <span className="tool-chip" key={tool}>
                <span className="tc-dot" />
                {tool}
              </span>
            ))}
          </div>
        </div>
        <div className="cap-rows">
          {t.capabilities.map((cap, index) => (
            <div className="cap-row" key={cap.title}>
              <span className="ci">{capabilityIcons[index]}</span>
              <div>
                <h3>{cap.title}</h3>
                <p>{cap.body}</p>
              </div>
            </div>
          ))}
        </div>
      </section>

      <section className="comparison" id="comparison" aria-labelledby="comparison-heading">
        <div className="section-heading">
          <p className="eyebrow">{t.comparison.eyebrow}</p>
          <h2 id="comparison-heading">{t.comparison.title}</h2>
        </div>
        <div className="comparison-grid">
          {t.comparison.columns.map((column, index) => (
            <article key={column.title} className={index === 2 ? "highlight" : ""}>
              <h3>{column.title}</h3>
              <ul>
                {column.items.map((item) => (
                  <li key={item}>{item}</li>
                ))}
              </ul>
            </article>
          ))}
        </div>
      </section>

      <section className="workflow" id="workflow" aria-labelledby="workflow-heading">
        <div className="section-heading" style={{ marginBottom: 40 }}>
          <p className="eyebrow">{t.workflowEyebrow}</p>
          <h2 id="workflow-heading">{t.workflowTitle}</h2>
        </div>
        <div className="code-panel">
          <div className="code-tabs" role="tablist" aria-label={t.aria.commands}>
            {(Object.keys(t.codeTabs) as CodeTab[]).map((tab) => (
              <button
                key={tab}
                role="tab"
                type="button"
                aria-selected={codeTab === tab}
                onClick={() => setCodeTab(tab)}
              >
                {t.codeTabs[tab]}
              </button>
            ))}
          </div>
          {renderCodeTab(codeTab, t)}
        </div>
      </section>

      <section className="specs" aria-label={t.specsLabel}>
        <p className="eyebrow">{t.specsEyebrow}</p>
        <div className="spec-grid">
          <div>
            <div className="num">
              1<span className="u">M</span>
            </div>
            <p>{t.specs.context}</p>
          </div>
          <div>
            <div className="num">
              6<span className="u">×</span>
            </div>
            <p>{t.specs.platforms}</p>
          </div>
          <div>
            <div className="num">16</div>
            <p>{t.specs.tools}</p>
          </div>
          <div>
            <div className="num">
              100<span className="u">%</span>
            </div>
            <p>{t.specs.rust}</p>
          </div>
        </div>
      </section>

      <section className="faq" id="faq" aria-labelledby="faq-heading">
        <div className="section-heading">
          <p className="eyebrow">{t.faq.eyebrow}</p>
          <h2 id="faq-heading">{t.faq.title}</h2>
        </div>
        <div className="faq-list">
          {t.faq.items.map((item) => (
            <article key={item.q}>
              <h3>{item.q}</h3>
              <p>{item.a}</p>
            </article>
          ))}
        </div>
      </section>

      <section className="install-repeat" id="install" aria-labelledby="install-heading">
        <div>
          <p className="eyebrow">{t.install.eyebrow}</p>
          <h2 id="install-heading">{t.install.title}</h2>
        </div>
        <div className="install-list">
          <div className="install-card" aria-label={t.install.cardLabel}>
            <div className="tabs" role="tablist" aria-label={t.install.methodLabel}>
              <button
                id={installTabIds.npm}
                className={mode === "npm" ? "active" : ""}
                onClick={() => setInstallMode("npm")}
                onKeyDown={(event) => handleInstallTabKeyDown(event, "npm")}
                aria-selected={mode === "npm"}
                aria-controls={installPanelId}
                role="tab"
                tabIndex={mode === "npm" ? 0 : -1}
                type="button"
                ref={(element) => {
                  tabRefs.current.npm = element;
                }}
              >
                npm
              </button>
              <button
                id={installTabIds.curl}
                className={mode === "curl" ? "active" : ""}
                onClick={() => setInstallMode("curl")}
                onKeyDown={(event) => handleInstallTabKeyDown(event, "curl")}
                aria-selected={mode === "curl"}
                aria-controls={installPanelId}
                role="tab"
                tabIndex={mode === "curl" ? 0 : -1}
                type="button"
                ref={(element) => {
                  tabRefs.current.curl = element;
                }}
              >
                curl
              </button>
              <button
                id={installTabIds.powershell}
                className={mode === "powershell" ? "active" : ""}
                onClick={() => setInstallMode("powershell")}
                onKeyDown={(event) => handleInstallTabKeyDown(event, "powershell")}
                aria-selected={mode === "powershell"}
                aria-controls={installPanelId}
                role="tab"
                tabIndex={mode === "powershell" ? 0 : -1}
                type="button"
                ref={(element) => {
                  tabRefs.current.powershell = element;
                }}
              >
                PowerShell
              </button>
            </div>
            <div
              className="command-row"
              id={installPanelId}
              role="tabpanel"
              aria-labelledby={installTabIds[mode]}
              tabIndex={0}
            >
              <code>{command}</code>
              <button
                type="button"
                onClick={copyCommand}
                className="copy"
                data-state={copyState}
              >
                {copyLabel}
              </button>
            </div>
          </div>
          <p>
            {t.install.platforms}{" "}
            <a href={links.releases} rel="noreferrer">
              {t.install.releases}
            </a>
            .
          </p>
        </div>
      </section>

      <footer>
        <a className="foot-brand" href="#top">
          <img className="brand-mark" src="/orca-icon.svg" alt="" aria-hidden="true" />
          <span>Orca</span>
        </a>
        <div className="links">
          <a href={links.github} rel="noreferrer">
            GitHub
          </a>
          <a href={links.npm} rel="noreferrer">
            npm
          </a>
          <a href={links.releases} rel="noreferrer">
            {t.install.releases}
          </a>
          <span>{t.community.qq}</span>
          <a href={links.telegram} rel="noreferrer">
            {t.community.telegram}
          </a>
        </div>
      </footer>
    </main>
  );
}

export default App;
