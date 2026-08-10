import { useEffect, useState } from "react";
import {
  type Locale,
  type SeoEntry,
  applySeoHead,
  canonicalOrigin,
  detectInitialLocale,
  links,
  localeStorageKey,
  releaseVersion,
  releases,
} from "../shared";

const canonicalUrl = `${canonicalOrigin}/changelog/`;

const seoCopy: Record<Locale, SeoEntry> = {
  en: {
    title: "Orca changelog",
    description:
      "Every Orca release in one place: what shipped, when, and a link to the full GitHub release notes.",
    ogTitle: "Orca changelog",
    ogDescription:
      "Every Orca release in one place: what shipped, when, and a link to the full GitHub release notes.",
    imageAlt: "Orca terminal coding agent product preview",
    locale: "en_US",
  },
  zh: {
    title: "Orca 更新日志",
    description: "Orca 每一个版本：发了什么、何时发布，附 GitHub Release 完整说明链接。",
    ogTitle: "Orca 更新日志",
    ogDescription: "Orca 每一个版本：发了什么、何时发布，附 GitHub Release 完整说明链接。",
    imageAlt: "Orca 终端代码智能体产品预览",
    locale: "zh_CN",
  },
};

const copy = {
  en: {
    langName: "English",
    aria: {
      home: "Orca home",
      language: "Language",
    },
    nav: {
      home: "Home",
      install: "Install",
      github: "GitHub",
    },
    header: {
      eyebrow: "Changelog",
      title: "Every Orca release, in order.",
      subtitle:
        "Versions follow semver; each entry links to the full GitHub Release notes for verification commands, breaking changes, and migration tips.",
      latestLabel: "latest",
      readNotes: "Release notes",
    },
    related: {
      eyebrow: "Guides",
      title: "Follow the release history into the main workflows.",
      links: [
        {
          title: "Terminal coding agent",
          body: "Use Orca as a local terminal coding agent for verifier-gated repository work.",
          href: links.terminalCodingAgent,
        },
        {
          title: "DeepSeek coding agent",
          body: "See how DeepSeek-native reasoning, prefix cache behavior, and local history fit together.",
          href: links.deepseekCodingAgent,
        },
        {
          title: "GitHub workflows",
          body: "Apply Orca to issue triage, pull-request prep, release checks, and codebase archaeology.",
          href: links.githubWorkflows,
        },
      ],
    },
    summaries: {
      "v0.3.13":
        "Makes headless resume a first-class CLI capability. orca exec resume <SESSION_ID> continues a saved session with a fresh budget scope, resume --last picks the most recent session, and --resume-at <MESSAGE_ID> restores only up to a durable message boundary. session.completed now carries the durable session_id, text-mode exits print the exact resume command, and a budget-exhausted run persists a typed session.checkpoint (status, reason, budget consumed, last committed message, task plan, resumable) before the terminal projection — uncommitted tool calls remain indeterminate on restore, so Orca promises resumable, not exactly-once, execution.",
      "v0.3.12":
        "Adds runtime-owned Side Conversations for quick questions without disturbing the main task. /side creates a separate disposable child from an atomic parent snapshot; Ctrl+/ switches between parent and Side while the parent keeps running, and Ctrl+C closes and joins only the Side. Side history, memory, goals, and transcript output never merge into the durable parent. TUI response projection also fences provider responses by turn and item identity so older or partial streams cannot overwrite the current response.",
      "v0.3.11":
        "Strengthens the reliability boundary across headless, TUI, MCP, server, and automatic-memory workflows. Headless max-turn trajectories now preserve terminal truth and compare streamed projections with persisted records; terminal waiters cancel and join cleanly; MCP SSE elicitation validates requests and terminal IDs, cancels in-flight response posts, and covers decline, malformed, and wire cancellation paths; inherited network grants ask once and persist the retry; JSONL tool approvals remain runtime-owned; automatic memory writes are lock-governed and cancellation-safe; and the cross-surface contract fixtures now use deterministic local resources.",
      "v0.3.10":
        "Makes TUI interaction lifecycle runtime-owned end to end. Approvals, user input, cancellation, terminal settlement, and replay now use one typed surface rail instead of parallel client state. Goal tool Allow and Deny settle durably with exact usage; Deny pauses without executing the tool, and explicit resume starts a fresh fenced run. Session browsing ignores symlinks, FIFOs, devices, and other non-regular transcript entries, preventing special files from stalling the picker or transcript reader.",
      "v0.3.9":
        "Upgrades Plan mode from a permission-only gate to a full plan-then-execute workflow. The agent now performs read-only exploration, emits a formal proposed plan, and presents a bottom-anchored approval dialog. Approving restores the previous mode and executes the plan automatically; rejecting stays in Plan for further iteration. Per-turn mode context injection ensures Plan constraints apply immediately when switching modes mid-session. PageUp/PageDown and scroll review long plans inline.",
      "v0.3.8":
        "Improves long-running and delegated work reliability. The context footer shows the remaining percentage, while /status reports remaining and total token counts; repeated compactions and queued steer input recover without restoring discarded context or losing user intent; asynchronous subagent results are durable and pageable; background task completion is announced proactively; stdio MCP reconnects after transport failures; and concurrent JSONL writers fail before reusing an event-sequence reservation. Workflow runs can enforce a shared tokenBudget, while /skills opens a searchable picker and slash settings reflect committed runtime state.",
      "v0.3.7":
        "Completes session-context and TUI projection reliability. Recorded sessions restore provider prompt occupancy before the first resumed turn; revision-aware projection prevents an older surface snapshot from replacing a newer context footer; reasoning, message, and plan streams preserve their opened order during hydration; and already-streamed completed responses are no longer rendered twice. The delegated execution, transcript locking, retry diagnostics, and task-registry guarantees from v0.3.6 remain included.",
      "v0.3.6":
        "Strengthens runtime reliability across delegated work and durable task state. Synchronous, asynchronous, and workflow child agents now inherit one serialized execution-policy snapshot; plaintext and compressed transcript paths share a stable cross-process lock during append and rewrite; provider compaction retries and truncated tool output remain visible in task summaries; and session completion publishes the complete task registry before the terminal event. The composer remains available while an activity line shows running or approval-blocked background work.",
      "v0.3.5":
        "Adds one canonical ask_user_question tool for one to four structured questions, with described choices, previews, multi-select answers, custom responses, and cancellation through the runtime-owned TUI interaction broker. Goal mode now starts provider work correctly in optimized builds, /workflows remains available during foreground turns, and unknown slash commands are rejected instead of reaching the model. Terminal-Bench now reports the mounted binary version, preserves JSONL trajectories for Harbor without extending its closed AgentContext model, documents supported filters, and keeps generated benchmark artifacts out of Git.",
      "v0.3.4":
        "Fixes the context meter and compaction policy for large model windows. The TUI now derives the remaining percentage of the full context window from provider-reported prompt usage instead of displaying an estimate against the old 96k compaction budget. Automatic compaction now triggers at 80% of the model window, keeps a 90% hard safety ceiling, and retains a fixed recent-context budget of about 48k tokens before summarizing older history. New sessions now default to sandboxed auto-edit; suggest, full-auto, and plan remain available explicitly. Existing absolute compaction overrides remain compatible.",
      "v0.3.3":
        "Closes the Orca runtime and architecture audit. Goal persistence and terminal replies now settle in order without blocking Tokio actors, operation cancellation owns spawned task trees, and session switch, fork, rename, and stale-event handling are transactionally fenced. Runtime surfaces use an explicit facade, tool schemas are provider-neutral, root MCP ownership lives in RuntimeHost, and transcript streaming, reflow, search, deduplication, usage, and projection state now have bounded or revision-checked behavior.",
      "v0.3.2":
        "Fixes a crash when Orca is launched through the npm wrapper (node stdio:inherit sets O_NONBLOCK on inherited terminal fds). Orca now clears non-blocking flags at startup and wraps stdout with an EAGAIN/EINTR retry writer, eliminating os error 35 during rapid terminal resize on macOS. Also adds reasoning effort low for DeepSeek V4, with correct thinking configuration and default max_tokens in API requests.",
      "v0.3.1":
        "Orca now has a complete session lifecycle in the TUI. /resume is the single saved-session entry point, with resume, fork, rename, archive, delete and session-ID copy actions in one picker. /new starts a clean conversation, /fork preserves the current context in a new durable session, /rename updates the active session, /status reports the current runtime state and /copy copies a finalized assistant response. Recoverable operations use an explicit Continue or Cancel prompt, and exit prints the exact orca --resume command for returning later. Reasoning effort now supports low alongside medium and high, matching DeepSeek V4 parameter requirements with correct thinking configuration and default max_tokens. The npm wrapper startup crash caused by inherited O_NONBLOCK terminal file descriptors is fixed: Orca clears non-blocking flags at startup and wraps stdout with an EAGAIN/EINTR retry writer.",
      "v0.3.0":
        "Orca now ships native Windows x64 and ARM64 support across the CLI, TUI, shell sessions, sandboxing, updates, persistence, npm packages and GitHub release archives. PowerShell 7, Windows PowerShell and cmd.exe resolve through explicit dialect-aware commands; ConPTY provides interactive terminal sessions; AltGr input, clipboard access, process-tree cleanup, atomic replacement and cross-process locks follow Windows semantics. The PowerShell installer verifies checksums, installs the runtime plus sandbox helpers and can provision, repair or remove the per-workspace sandbox capability. Native x64 and ARM64 CI run the platform contracts and full workspace tests before release.",
      "v0.2.56":
        "The CLI binary is now limited to argument parsing and library forwarding: configuration, launch, update, history, trust, workflow, protocol and worker ownership live in orca-runtime and orca-tui. Stateless JSONL submissions now own their complete turn lifecycle without requiring a persisted thread, including exact EOF cancellation and settlement. macOS Seatbelt execution now uses the absolute system binary, parameterized path rules, protected metadata write roots and fail-closed enforcement, while trust and command-output failures propagate instead of being reported as success.",
      "v0.2.55":
        "ACP and the JSONL server now complete the runtime-owned typed-surface convergence begun in v0.2.54. ACP session admission, prompt binding, replay, terminal flush, cancellation, capability settlement and bounded transport supervision are owned by the runtime. JSONL thread, control, permission, user-input and MCP routes now use one surface adapter with durable request identity, EOF settlement and restart recovery. Release gates exercise TUI, ACP and server paths against the real binary, while publication verifies exact archives, npm tarballs, checksums, registry integrity, package aliases, binary identity and clean installation.",
      "v0.2.54":
        "The production TUI now completes its runtime-owned typed surface migration. The app loop, agent runtime and action dispatcher hold one typed surface control while RuntimeHost owns prompt admission, durable batches, operation and generation fences, interactions, cancellation, terminal finalization, workflow task state and restart recovery. Assistant and tool output is projected only after durable commit, interaction answers are persisted before waiters wake, manual compaction cannot report success before its terminal receipt, and restart restores the exact snapshot and pending ownership without redefining turn semantics in the renderer. ACP convergence follows this TUI release; JSONL compatibility remains later work.",
      "v0.2.53":
        "Goal Mode now completes its first runtime-owned TUI vertical loop. Set-and-run, resume, pause and cancellation flow through typed commands; Goal state, outer-turn progress, continuation decisions and terminal results are committed durably before TUI projection or waiter wake. Restart recovery preserves the exact owner lease, operation and generation fences, pending mutation receipts, progress barriers and repeated-gap streaks. MaxInnerTurns remains resumable, plan-only work counts as progress, cost-budget exhaustion pauses as UsageLimit, and exact retry digests bind usage, progress, verification, continuation and terminal semantics.",
      "v0.2.52":
        "Goal continuation now distinguishes an advanced turn, a resumable interruption, and a true blocker. Reaching the inner-turn limit preserves a MaxInnerTurns reason, emits soft-landing reminders, and continues with a structured handoff containing the objective, budget state, open gap, current task plan, and bounded assistant checkpoint. Cost-budget exhaustion, cancellation, approval, verification failure, and other blocking outcomes still pause. A separate durable watchdog counts only substantive tool or plan progress, preserves progress barriers across SQLite recovery, pauses after three repeated model-fixable gaps, and caps eight consecutive inner-turn interruptions.",
      "v0.2.51":
        "Ordinary TUI turns now run end to end through a runtime-owned typed surface: prompt admission, atomic durable commit, assistant and tool projection, approval and permission decisions, cancellation, terminal cleanup, snapshot replay, and restart recovery share one RuntimeHost truth. Recovered controls remain bound to the original operation and generation, interaction responses are durable before waiters wake, and production tests cover real Record-to-Resume history plus PTY terminal restoration. ACP typed prompt, replay, permission, and bounded RPC bridges are included without changing Goal, workflow, or JSONL compatibility.",
      "v0.2.50":
        "Goal Mode no longer has a fixed outer-turn or continuation ceiling. RuntimeHost admits the next turn from semantic state, cancellation, pending interactions, workflow ownership, progress, and token budget; continuation_count remains only persisted ledger and event telemetry. This removes the false Paused(NoProgress) terminal that could appear after 64 otherwise valid turns without weakening budget, stall, or user-control boundaries.",
      "v0.2.49":
        "Goal Mode now has one runtime owner for lifecycle, continuation admission, cancellation, recovery, usage, and persistence. Terminal model claims are typed intents audited at turn end, SQLite replaces direct JSON mutation with migration and crash recovery, and role-safe context plus semantic events keep TUI and ACP projections consistent. Five real DeepSeek scenarios verify completion, rejected completion, genuine blocking, cancellation, and resume with no stale continuation or in-flight run.",
      "v0.2.48":
        "ACP initialization now reports the Orca binary release version from RunConfig instead of the internal orca-runtime crate version. Integration coverage also isolates ORCA_HOME, verifies that each session keeps its requested working directory, and exercises cancellation arriving before the hosted operation handle is installed.",
      "v0.2.47":
        "Orca now exposes an additive Agent Client Protocol adapter over stdio with --mode=acp. ACP sessions and prompts project directly onto RuntimeHost threads and hosted turns, EventEnvelope streams become standard session/update notifications, and cancellation reaches the active Generation Fence through OperationHandle. The existing internal JSONL server protocol remains unchanged.",
      "v0.2.46":
        "Goal Mode control tools now execute through the runtime that advertised them. get_goal, create_goal, and update_goal use the recorded session and live extension context before any normal-tool worker boundary, with the old thread-local callback removed. Invalid model arguments remain recoverable, while missing control-plane ownership or persistence failures stop one turn, atomically stall an active goal, and clear stale Goal context. A billed DeepSeek gate verifies one non-goal tool, exactly one terminal update_goal call, and zero eligible continuations.",
      "v0.2.45":
        "Approval modes now match their execution boundaries: auto-edit runs autonomously inside the workspace sandbox, while full-auto combines automatic approval with danger-full-access and no post-failure sandbox escape prompt.",
      "v0.2.44":
        "macOS Sequoia sandbox shell resolution is fixed. sandbox-exec invocations now use /bin/sh instead of bare sh, bypassing the /private/var/select/sh kernel lookup that macOS 15 blocks inside a seatbelt sandbox. This eliminates the spurious Unsandboxed Shell Required approval prompt that appeared on every tool call in full-auto mode.",
      "v0.2.43":
        "Linux fail-closed enforcement is now scoped to strict restricted-read policies, so untrusted-folder and strict read-only modes still refuse to run when neither bubblewrap nor Landlock can enforce them. Non-strict capability modes (workspace write and global read-only) keep their established Landlock-plus-seccomp or plain compatibility fallback when a policy needs bwrap-only features and no bwrap is on PATH, matching the reference agents' fail-open-for-built-ins behavior. The release test runner no longer installs bubblewrap, so CI exercises the Landlock plus seccomp fallback path directly.",
      "v0.2.42":
        "Linux command isolation now prefers bubblewrap for mount, namespace, capability, and network boundaries, then falls back to Landlock plus seccomp when the policy is expressible. Strict restricted-read policies refuse to run when neither backend can enforce them. Folder trust persists user decisions outside the repository: untrusted folders do not load project config, instructions, skills, or named workflows and receive a read-only, no-network default, while explicit capability modes, permission rules, and network-proxy grants keep their existing authority. Runtime lifecycle tests now use an explicit danger-full-access capability profile, proving that this established override remains authoritative without weakening the Linux fail-closed default.",
      "v0.2.36":
        "Foreground subagents now have one runtime-owned invocation lifetime for admission, child cancellation, worker join, panic classification, schema validation, worktree completion, usage, and exactly-once terminal selection. Interrupting a synchronous delegation waits for child cleanup before the turn settles or the next prompt starts, while a child panic becomes an indeterminate result instead of escaping RuntimeHost. Async delegation now projects its durable task to the TUI immediately without creating an unmatched foreground lifecycle; atomic PID adoption prevents a fast worker from being overwritten, and foreground interrupt remains independent from explicit task_stop cancellation. The inline single-child loop, scoped batch runtime, duplicate formatting, stale adoption path, and source-shape ownership tests are removed; cross-process leases and stale-owner takeover remain P1.4.",
      "v0.2.35":
        "Sequential normal tools now run as runtime-owned child lifetimes. RuntimeToolCallRuntime owns admission, started state, cancellation policy, the worker, join, panic classification, permission deltas, and the exactly-once terminal while bounded typed bridges carry output, approval, and MCP elicitation. Interrupting bash, external tools, or MCP calls waits for cleanup before the turn settles or the next prompt starts; WaitForTerminal preserves an observed mutation, and a worker panic after start becomes indeterminate. Turn permission grants are merged before later sibling calls. The borrowed normal executor, fallback owner, inline path, and source-shape tests are removed; subagents remain the explicit P1.2c boundary.",
      "v0.2.34":
        "Interrupting a TUI or server turn now reaches parallel read-only tool calls that have already started, waits for every worker and transport to clean up, and only then publishes the operation terminal or admits the next prompt. RuntimeToolCallRuntime owns each invocation's concurrency permit, cancellation, started state, blocking task, join, panic classification, and exactly-once terminal while preserving provider order. MCP resource list, template, and read requests are cancellable; stdio reconnects after cancellation and SSE remains reusable. The old orca-tools batch scheduler is removed. Normal tools and subagents remain explicit P1.2 follow-ups.",
      "v0.2.33":
        "A submitted prompt now has one admission owner and one durable user row. Hosted turn preparation persists the identified user message before committing it to model context, while agent-loop bootstrap explicitly distinguishes owned child-agent conversations from borrowed hosted sessions and never replays borrowed initial history. Live thread reads, turn pagination, item pagination, cold ThreadStore reads, restart, and resume now expose one user item per logical turn with stable turn and item ids. Existing duplicate history remains readable without a projection-time deduplication layer.",
      "v0.2.32":
        "Completed DeepSeek responses now have one durable canonical fact. Agent-message, reasoning, and proposed-plan ids are allocated before streaming and stay identical through live projection, approval suspension and continuation, restart, pagination, and resume. Runtime persists one typed model.response.completed event instead of a second assistant record, and model replay plus ThreadStore history reduce that same event. Legacy combined assistant records remain readable through one isolated reducer, while malformed current completions fail closed. The real DeepSeek gate now compares complete persisted item objects and internal/external ids across processes.",
      "v0.2.31":
        "Recorded conversations now keep opaque turn and item identities across reload, resume, compaction, repair, pagination, rename, archive, and compression. Logical turn ids are separate from runtime task ids, so concurrent first turns cannot collide in server control routing. New records use typed UUIDv7 identities, tool and workflow items retain their domain ids, and legacy histories remain readable through one isolated fallback. A real DeepSeek gate now records a turn, restarts the process, resumes the same thread, and proves both context continuity and stable prior ids.",
      "v0.2.30":
        "The production TUI now runs foreground turns, interrupted streams, approvals, user input, MCP elicitation, background providers, and saved workflows through one process-owned RuntimeHost. RuntimeHost owns cancellation, joins, terminal events, usage commits, and shutdown cleanup; the duplicate TUI provider/tool/workflow loops and TaskSupervisor are removed. A cancelled live DeepSeek stream releases the foreground operation and the next submit starts cleanly, while repeated idle Goal refreshes no longer duplicate the same notice.",
      "v0.2.29":
        "Runtime now has a process-owned RuntimeHost and one bounded ThreadActor per conversation, with typed operation handles and completion terminals so headless and TUI turns share one ownership boundary. Structured @ mention bindings now resolve exact files, skills, plugins, and MCP resources, recover rejected submissions, and keep mention search and history isolated from user data. TUI selection, clipboard, input history, status formatting, and submission hints are also refined.",
      "v0.2.28":
        "Server turn cancellation is now generation-owned. Interrupt permanently cancels the current DeepSeek execution; resume waits for it to return, starts a fresh scope on the same logical turn id, and never appends the original prompt twice. Permission, user-input, and MCP waiters are cancellation-aware and generation-fenced, stale steer and responses are rejected, and a replaced generation cannot publish stale cancellation errors or a stale terminal event. The first interaction request id remains compatible; resumed generations receive an internal generation suffix. A real DeepSeek gate interrupts after the first stream delta and verifies one successful terminal event.",
      "v0.2.27":
        "Every submitted TUI turn, manual compaction, Goal operation, and approved background continuation now receives a fresh one-shot cancellation scope with a stable operation id. Esc and Ctrl+C cancel only the active scope, so interrupting a DeepSeek stream cannot be undone by a later reset or leave the next turn born cancelled. All production TUI reset calls are gone, and an agent-loop behavior test cancels a delayed first turn before proving that a second submit gets a different scope, produces output, and completes successfully. CLI arguments, TUI keys and flows, server JSONL, persisted records, and DeepSeek behavior remain compatible. The server turn/resume reset path remains an explicit actor-owned follow-up rather than a permanent compatibility layer.",
      "v0.2.26":
        "The TUI now admits runtime events through a 256-item mailbox and user actions through a 64-item mailbox with blocking backpressure, so a slow or paused renderer cannot grow an unbounded queue and admitted output, approvals, errors, and terminal state keep FIFO order. Runtime compaction and approved background continuation project the original typed EventEnvelope through EventObserver instead of serializing JSONL into a partial-frame buffer and parsing it back. Provider streaming, mention catalog refresh, and silent child-agent event disposal also use explicit bounded ownership, with the silent drain thread joined before return. CLI/server JSONL, persisted records, DeepSeek behavior, TUI keys, and interaction flows remain compatible.",
      "v0.2.25":
        "Network-restricted TUI bash and server command sessions now own every proxy connection through one managed supervisor. Admission is capped at 32 concurrent connections; excess clients receive a bounded connection-limit response instead of spawning another worker. Request and header framing is bounded before parsing, network-block reports use a fixed-capacity non-blocking queue, and DNS lookup, upstream connect, and socket I/O have explicit deadlines. Stopping a command, cancelling a turn, or dropping the proxy stops admission, aborts and awaits every active connection, closes both ends of CONNECT tunnels, and joins the supervisor thread. CLI flags, permission profiles, proxy environment variables, TUI flows, server/JSONL shapes, and persisted records remain compatible.",
      "v0.2.24":
        "Every accepted tool invocation now reaches one truthful terminal result, including interruption, pre-execution rejection, and siblings that never started. Incomplete legacy history is repaired as indeterminate without replaying old calls. Ordinary process stdout and stderr are capped at 1 MiB per stream, while file reads and exact edits reject non-regular, binary, invalid UTF-8, growing, and oversized inputs before unsafe admission. External tools, MCP servers, workflows, async subagents, verifier commands, server turns, shells, and search managers now have explicit cleanup or reaper ownership; MCP and WorkflowHost transports and shutdown paths are bounded; observed completion wins cancellation and timeout races; and internal worker API keys no longer persist or appear in process arguments. Windows descendant-tree parity, a total WorkflowHost deadline, and a managed-proxy connection ceiling remain follow-up work.",
      "v0.2.23":
        "The Orca TUI gains native-feeling mouse text interaction. Drag over the transcript to select text with an editor-style theme-aware highlight that preserves syntax foregrounds; releasing the button copies the selection to the system clipboard through OSC 52 (VS Code, iTerm2, kitty, WezTerm, and SSH sessions) with a pbcopy fallback on macOS, and a transient `copied N chars to clipboard` notice appears on the status line. Selections are anchored in content space rather than screen space, so streaming output and scrolling never shift what was selected. Soft-wrapped prose copies back as one line with the wrap-dropped whitespace restored, while hard-split long words such as URLs stay unbroken. Double-click selects the word under the cursor and copies it immediately; dragging onto the transcript's first or last row auto-scrolls on the animation timer so the selection keeps growing while the pointer sits still; and a floating `Jump to bottom` pill appears whenever auto-follow is disarmed and re-arms it on click. The obsolete `shift+drag to copy` status hint is gone, and the mouse wheel keeps its existing scroll behavior.",
      "v0.2.22":
        "Orca's `@` mention now spans multiple workspace roots and every candidate kind at once. `orca-file-search` sessions accept one or more roots, so equal relative paths from different roots stay distinct while browse, fuzzy, exclude, Git-ignore, cancellation, and million-path bounds keep working unchanged. Files, Skills, Plugins, MCP Resources, and Resource Templates now share one typed `MentionCandidate`/`MentionTarget` model with stable ids derived from the full target rather than display text, so same-name results never collapse. Selecting a candidate records a hidden atomic binding: it rebases across earlier edits, invalidates on overlapping edits, and expands the exact selected root, Skill path, plugin manifest, or MCP resource at submission time instead of re-resolving visible text. The Codex-compatible `fuzzyFileSearch/*` app-server contract now takes explicit multi-root input, and a new thread-bound `mention/search/*` contract discovers and expands candidates against a live thread's own workspace roots and MCP registry. TUI and app-server submissions share the same expansion and validation code, legacy unbound `@file`/`$skill` input remains valid, and search-session reapers are retained and joined on stop/shutdown so late output can never race a stopped session.",
      "v0.2.21":
        "DeepSeek turns that end without visible content or a tool call now receive one bounded semantic recovery request instead of an identical replay. The request-local correction preserves valid tool-call reasoning and tool results, never persists the incomplete response, avoids replaying recovery reasoning already shown in the TUI, and preserves usage reported by both attempts. Foreground budgeted requests use serialized admission and later-turn preflight, while detached completions persist priced usage and redacted diagnostics through the task-correlated background_task.provider_response record without overwriting global session completion. Resume accounting now uses session.usage_baseline, and Goal tokens count input plus output without adding cache hits twice. The release also updates the tag-driven GitHub Actions pipeline to current v5 artifact, checkout, and Node setup actions.",
      "v0.2.20":
        "The TUI now keeps long sessions compact across the full interaction loop. Large pastes stay collapsed in the composer and transcript while the complete prompt reaches the model; Goal objectives and notices, task-plan steps, and tool targets use display-width-aware ellipses; long approval content cannot push decisions away; slash and file menus follow the selected row; and the responsive footer preserves permission mode and context before optional metadata. Permission modes now use blue, violet, red, and teal semantic accents.",
      "v0.2.19":
        "macOS Seatbelt now lets sandboxed test runners signal only their own child workers, so Vitest, Tinypool, Jest, and similar pools can clean up after failures and shutdown. The incident behind this fix left 10 Node workers using 40.51 GiB while Orca native used about 30.2 MiB and its npm wrapper about 11.8 MiB; it was not an Orca transcript-heap leak. Workspace-write and read-only profiles keep unrelated processes outside the signal boundary.",
      "v0.2.18":
        "Unknown or malformed DeepSeek function names now become recorded, failed tool results that the model can correct inside the same agent turn instead of terminal provider errors that pause Goal mode. Orca preserves the original call id, name, and raw arguments across streaming and non-streaming responses, but never infers or executes bash from command-shaped names; registry validation rejects them before approval, hooks, task creation, or execution.",
      "v0.2.17":
        "Active Goal activity time now stays cumulative across automatic continuations by combining persisted completed-turn time with the current turn delta. /goal resume preserves the objective, budget, token usage, elapsed time, and creation timestamp; same- and cross-session restoration use one atomic migration with an occupied-target guard and unchanged state on failure. Restored history projects the preserved Goal before TurnStarted so the first running frame includes the persisted base.",
      "v0.2.16":
        "TUI context compaction now has a visible, interruptible lifecycle. Automatic soft-limit, hard-limit, and prompt-too-long recovery show Compacting context before hooks or remote summary work; manual /compact uses the same state. Ctrl+C, Esc, and Ctrl+G cancel hooks and the streaming DeepSeek summary, while completion appears only after persistence and post-compact hooks finish. DeepSeek header waits, retry delays, error-body reads, and SSE reads now race cancellation through a joined provider worker, including between events from one SSE frame. Malformed or prematurely ended streams fail explicitly and retry only before visible output; malformed JSON for a known tool is preserved, validated before approval, hooks, task creation, or execution, and returned as a corrective tool failure. Older compacted events remain compatible.",
      "v0.2.15":
        "TUI resume now drops legacy reasoning-only assistant turns before replaying history to DeepSeek. New provider responses that contain reasoning but no visible content or tool calls are retried instead of being persisted, while valid reasoning attached to tool calls remains intact. The release gate now exercises this malformed-history recovery against the real DeepSeek API.",
      "v0.2.14":
        "Server-mode MCP elicitation now matches the TUI interaction path. When an stdio MCP tool sends elicitation/create during a turn, Orca emits a turn-scoped mcp_elicitation_request event, accepts mcp_elicitation/respond by requestId, writes accept/decline back to the MCP server, and then continues the original tool call. turn/interrupt now cancels unanswered MCP elicitation prompts, MCP transport cancellation returns promptly across stdio and SSE paths, and server shell/list stops reaping command/exec-owned backing shells.",
      "v0.2.13":
        "Runtime task output now flows through a bounded, UTF-8-safe task-output store. Long-running TUI bash and command/exec sessions avoid unbounded stdout/stderr retention, command/exec streaming keeps cumulative output caps correct after retained-output rebases, and terminal command paths evict retained output after completion, stop, or permission denial.",
      "v0.2.12":
        "TUI scroll performance gets a full overhaul so long sessions stay responsive. A frame scheduler coalesces wheel events and caps per-batch processing, message rendering flows through a version-based cache instead of redrawing the whole transcript each frame, and a virtual viewport renders only the visible messages. Scroll offsets widen to usize so sessions past 65,535 lines scroll correctly, and the bottom status line drops the scroll: N/total indicator.",
      "v0.2.11":
        "TUI keyboard handling now runs through a context-aware shortcut resolver. Global, composer, running-turn, and approval-dialog keys keep the same behavior, but the resolver, tests, and shortcut help path now share one binding boundary so future keymap and task-control changes are easier to verify.",
      "v0.2.10":
        "TUI compacted-context notices now keep the runtime compaction reason and strategy. Long DeepSeek sessions can show when Orca compacted near the token limit, at the hard limit, or after prompt-too-long recovery, including the collapsed message count, instead of only showing a generic before/after message total.",
      "v0.2.9":
        "TUI automatic compaction and prompt-too-long retry recovery now run through the runtime compaction boundary. The visible context meter, compacted-context notice, and failed compaction errors keep the same TUI shape, but the main TUI loop no longer owns context-pressure decisions or retry state, reducing drift from server and child-agent compaction paths.",
      "v0.2.8":
        "Command/exec sandbox and permission-profile resolution now lives behind a focused server module instead of the generic server loop. TUI bash and server command/exec still share the same sandbox behavior and JSON wire shapes, while the permission-profile boundary is easier to test before future network, filesystem, and task-control changes.",
      "v0.2.7":
        "Core now owns the reusable thread-item projection types for user, persisted, assistant-message, proposed-plan, and reasoning transcript items. Runtime projection still emits the same TUI/server JSON, but live streams, active steer messages, and resumed history now share one typed item boundary before serialization, reducing drift in the transcript cards users see.",
      "v0.2.6":
        "TUI proposed plans now render as their own scrollback message instead of leaking <proposed_plan> tags into assistant text. The server and TUI share one UTF-8-safe parser, so split tags and Chinese streaming text keep the same tested behavior across local TUI and server projection.",
      "v0.2.5":
        "Server command/exec network-policy denials now use the same runtime permission evaluation boundary as TUI bash. Requestable blocked hosts still open the existing permission-request and retry flow, while configured denylist hosts now surface a clear policy-denial error instead of falling through as an unpromptable missing request.",
      "v0.2.4":
        "TUI bash network-policy denials are now explicit. Runtime permission policy returns a structured Request or Deny evaluation for network blocks, so requestable hosts still open the normal approval flow while configured denylist blocks end as a clear denied tool result instead of being represented as a missing prompt.",
      "v0.2.3":
        "TUI MCP tool calls can now surface real stdio elicitation requests instead of silently dropping them. When an MCP server sends elicitation/create during a tool call, Orca projects the URL or form request through the runtime pending-interaction store, shows a TUI waiting-input prompt keyed by the runtime id, writes accept or decline back to the server, and then continues the original tool call.",
      "v0.2.2":
        "DeepSeek tool-call compatibility gets hardened. update_goal and update_plan normalize the status aliases and boolean status flags DeepSeek emits before validation, the glob and update_goal JSON Schemas support nullable/anyOf parameters, and tool validation errors now list the allowed and required properties. The system prompt stops inlining full tool schemas in favor of concise examples, and the line adds reasoning-content replay, a tool-count cap, and empty-response retry for DeepSeek turns.",
      "v0.2.1":
        "Server interactive responses now stay with their focused processors. permission/respond resolves pending permission grants inside the permission processor, user_input/respond resolves runtime user-input waiters inside the user-input processor, and ownership tests guard that the generic server module does not reclaim those response paths.",
      "v0.2.0":
        "Permission approval dialogs now name the requested risk directly. Network blocks, filesystem write grants, and unsandboxed shell retries keep their runtime permission kind through pending interactions, so the TUI modal can show Network Permission Required, Filesystem Permission Required, or Unsandboxed Shell Required instead of a generic approval title. request_permissions also reaches the runtime permission handler instead of being intercepted as an ordinary tool approval, making TUI and server permission grants more reliable.",
      "v0.1.191":
        "RuntimeProviderResponseStep now consumes RuntimeProviderResponseInput directly instead of flattening the response handoff. Child-agent executors travel through RuntimeProviderResponseExecutors, so provider final-message and tool-turn dispatch keep one named response boundary while behavior stays unchanged.",
      "v0.1.190":
        "RuntimeProviderTurnStep now receives provider-call state through a named RuntimeProviderTurnInput. Provider-turn I/O refs live behind RuntimeProviderTurnIo, so conversation, history, events, sink, and cost tracking are handed into provider execution as one boundary while provider behavior stays unchanged.",
      "v0.1.189":
        "RuntimeProviderResponseInput now carries provider-response I/O refs through a named RuntimeProviderResponseIo bundle. RuntimeTurnKernel assembles events, sink, conversation, history, cost tracking, and background workflow handles as one handoff, and provider response handling destructures that bundle only at the execution boundary.",
      "v0.1.188":
        "RuntimeProviderCycleInput now reuses RuntimeStepCapabilitySnapshot for provider-cycle capability refs. runtime_turn_iteration assembles that bundle once, provider_turn passes it into RuntimeStepContext without expanding flat capability fields, and provider/tool behavior stays unchanged while the provider-cycle boundary gets smaller.",
      "v0.1.187":
        "RuntimeStepSnapshot now carries request-scoped capability refs through a named RuntimeStepCapabilitySnapshot. Tool dispatch reads instructions, memory, MCP registry, hooks, cancel state, task registry, workflow IPC, and interaction handlers through that capability bundle, keeping provider/tool behavior unchanged while shrinking the next Codex-style step-context boundary.",
      "v0.1.186":
        "Runtime request_user_input handling now shares RuntimeTurnInteractionState with permission requests. ThreadTurnRequest can carry a runtime user-input handler, the provider/tool cycle passes it through RuntimeStepSnapshot and ToolExecutionContext, and RuntimeToolRouter treats request_user_input as a runtime special dispatch instead of a normal tool fallback.",
      "v0.1.185":
        "Runtime turn-scoped interaction handlers now flow through RuntimeTurnInteractionState. AgentLoopContext, runtime_turn_loop, and runtime_turn_iteration carry the permission-request handler through one grouped boundary, preserving current approval behavior while preparing request-user-input and other interaction waiters to share the same turn-owned surface.",
      "v0.1.184":
        "RuntimeStepContext now carries request-scoped runtime inputs through a named RuntimeStepSnapshot. Provider response handling reads final-response settings through the snapshot, and tool dispatch splits the step context into snapshot plus extension binding before routing normal, readonly, workflow, and subagent tool turns.",
      "v0.1.183":
        "Runtime directives now flow through a named RuntimeCapabilitySnapshot. Switch-model, allowed-tool, and runtime system-message patches share one capability contract, and RuntimeDirectiveState exposes that snapshot so later skill, hook, MCP, and tool-policy work can derive from the same state surface.",
      "v0.1.182":
        "RuntimeTurnKernel now owns turn-loop state assembly as an instance handoff. RuntimeTurnState creates the kernel from scoped extension stores and asks that instance to build RuntimeTurnLoopState, while shared extension stores keep the kernel-owned borrow boundary compatible with the loop state it hands forward.",
      "v0.1.181":
        "RuntimeTurnKernel now assembles the lifecycle-owned RuntimeTurnLoopState. RuntimeTurnState no longer expands loop runtime and extension-state fields itself, continuing the turn-state consolidation started around provider response handling.",
      "v0.1.180":
        "RuntimeTurnKernel now assembles the provider-response input object itself. Provider response handling no longer exposes kernel-owned sampling state or step-context binding as separate fields, tightening the turn-state handoff while preserving behavior.",
      "v0.1.179":
        "RuntimeTurnKernel now retains the extension stores used by its reducer and binds provider-response RuntimeStepContext extensions through the same kernel. Provider response handling no longer wires extension stores directly, tightening the Codex-style turn-state boundary without changing behavior.",
      "v0.1.178":
        "RuntimeTurnKernel now owns the per-sampling request state together with the runtime turn reducer. Provider response handling constructs tool-dispatch state through that kernel before passing it into tool turns, preserving behavior while giving the next Codex-style turn-state consolidation a named boundary.",
      "v0.1.177":
        "command/exec/list snapshots now include the backing shellId, taskId, requestedTerminalMode, and effectiveTerminalMode. Reconnecting app-server clients can restore active command/exec display and PTY or pipe semantics with the same task identity used by shell/list.",
      "v0.1.176":
        "Server-mode clients can now call command/exec/list to recover active command/exec process handles. The listed snapshots include processId, command, cwd, running status, stream output settings, output cap, and stdout/stderr byte counters, and completed processes are drained out before the next list response.",
      "v0.1.175":
        "Server-mode command/exec/read now accepts outputBytesCap. Read requests can tighten the active streaming process output cap before the normal pre-dispatch drain, so bounded polling returns UTF-8-safe deltas with capReached metadata instead of leaking a larger burst.",
      "v0.1.174":
        "Server-mode command/exec now accepts command/exec/read for client-driven drains of long-running streaming process handles. The read request acknowledges the active process and reuses the existing outputDelta stream, cap metadata, and completion path.",
      "v0.1.173":
        "Server-mode shell/read now accepts outputBytesCap for bounded incremental shell output. stdout/stderr deltas and shell_updated or shell_completed responses are truncated on UTF-8 boundaries and include capReached metadata when the read budget is reached.",
      "v0.1.172":
        "Server-mode clients can now query shell/capabilities before starting shell or command/exec PTY work. The response reports the current platform, whether native PTY and PTY resize are available, accepted terminal modes, pipe fallback behavior, and the processId requirement for streaming command/exec sessions.",
      "v0.1.171":
        "Bash sandbox recovery now handles pathless macOS sandbox denials such as GitHub HTTPS credential prompts: runtime, JSONL command/exec, and the TUI can ask to re-run the command without the filesystem sandbox after approval. Shell task session state also moves to ORCA_HOME with migration from legacy project .orca/task-sessions directories.",
      "v0.1.170":
        "RuntimeSamplingRequestState now records normal tool results and owns the approval-required and subagent-failure terminal folding for single-tool turns. Normal tool execution borrows its permission overlay and records its result through the same request state, leaving tool_turn to delegate without changing user-visible behavior.",
      "v0.1.169":
        "RuntimeSamplingRequestState now produces clamped RuntimeToolDispatchWindow values for readonly and subagent batches. Tool turns no longer read raw cursor positions or slice batch windows directly, and a stalled batch collector still advances over the current request instead of risking a stuck dispatch loop.",
      "v0.1.168":
        "RuntimeSamplingRequestState now owns the tool-dispatch cursor as well as the per-sampling permission overlay. Tool turns read and advance the current request through sampling state instead of keeping a separate ToolRequestCursor, so Codex-style request-scoped runtime state has one clearer owner without changing tool execution behavior.",
      "v0.1.167":
        "RuntimeSamplingRequestState now owns the per-sampling permission overlay. Provider response handling creates the sampling state and tool turns borrow its overlay instead of allocating local permission state, giving the next Codex-style request-state split a concrete home without changing tool behavior.",
      "v0.1.166":
        "Runtime turn-loop input construction now lives behind a focused run_agent_turn_loop entrypoint. agent_loop passes a RuntimeAgentTurnLoopInput launch object instead of constructing the wide RuntimeTurnLoopInput directly, leaving runtime_turn_loop as the owner of the internal turn-loop handoff.",
      "v0.1.165":
        "RuntimeTurnLoopState now owns the directive-resolved loop policy surface. agent_loop no longer destructures loop state or reads directive accessors directly; lifecycle resolves tool policy, runtime system messages, model override, cost/cancel/task refs, and grouped extension context for each turn-loop iteration.",
      "v0.1.164":
        "RuntimeTurnState now hands agent_loop a lifecycle-owned RuntimeTurnLoopState, separating runtime directives from the mutable turn-loop runtime refs. RuntimeTurnLoopInput owns that grouped runtime handoff and derives RuntimeExtensionContext at the iteration boundary, so agent_loop no longer destructures extension registry/thread/turn fields or reconstructs extension context from raw parts.",
      "v0.1.163":
        "RuntimeTurnState now exposes the grouped RuntimeExtensionContext boundary and owns the registry/store composition helper used by agent_loop. The loop still passes the same registry, thread store, and turn store into RuntimeTurnLoopInput, but extension-context construction no longer lives directly in the loop body.",
      "v0.1.162":
        "Runtime turn-loop, iteration, and provider-cycle inputs now carry one RuntimeExtensionContext instead of parallel extension registry, thread store, and turn store fields. agent_loop creates the grouped context once, and provider turns reuse it when building RuntimeStepContext, preserving lifecycle notifications while narrowing the runtime extension boundary.",
      "v0.1.161":
        "RuntimeStepContext and RuntimeNormalToolTurnContext now carry a grouped RuntimeExtensionContext instead of parallel extension registry, thread store, and turn store fields. Provider turns still build the same stores once, while normal tool execution receives the same lifecycle data through a narrower extension boundary.",
      "v0.1.160":
        "ToolExecutionContext now carries grouped RuntimeExtensionStores instead of reconstructing them from parallel thread/turn references. Tool lifecycle contributors, goal progress recording, and router dispatch keep the same behavior while the normal tool execution entrypoint has a smaller extension-store API.",
      "v0.1.159":
        "Permission-sensitive tool contexts now pass grouped RuntimeExtensionStores instead of parallel thread/turn extension references. RuntimeTurnReducer can be constructed from that grouped store boundary, so request_permissions, bash auto-escalation, router overlay transfer, and direct runtime-tool actor compatibility keep the same behavior while the runtime state API gets smaller.",
      "v0.1.158":
        "Permission reduction is now consistently instance-owned by RuntimeTurnReducer. The old static permission reducer accessor is gone, while request_permissions, bash auto-escalation, router overlay transfer, and direct runtime-tool actor calls keep their existing behavior through turn/thread extension stores.",
      "v0.1.157":
        "Permission overlay requests and merges now reduce through RuntimeTurnReducer. request_permissions, bash auto-escalation, and router overlay transfer keep the same behavior while permission state mutation leaves the direct call sites.",
      "v0.1.156":
        "Runtime directives now reduce through RuntimeTurnReducer. Model switches, allowed-tool replacement, and injected runtime system messages keep the same behavior while RuntimeTurnState no longer writes directive state directly.",
      "v0.1.155":
        "RuntimeTurnReducer now owns completed-tool goal progress recording. TUI completed normal tools route through that reducer instead of writing goal progress directly, keeping the live thread extension guard in one runtime-owned state boundary.",
      "v0.1.154":
        "Goal terminal updates now consult live runtime thread extension state. TUI turns record completed normal tools into the same goal progress store, and update_goal refuses complete or blocked claims until the live thread has observed real non-goal tool progress.",
      "v0.1.153":
        "RuntimeThread now owns the thread-scoped extension store and hands it to each RuntimeTurnState, while every RuntimeThread turn receives a fresh turn extension store id. Goal and future runtime contributors can now keep stable thread-level state across headless, TUI, and server turns without leaking turn-local data.",
      "v0.1.152":
        "RuntimeTurnState now owns the default extension registry plus thread and turn extension stores, installs the goal tool lifecycle contributor, and threads that state through the provider/tool turn path into normal tool execution. Goal progress can now observe real normal-tool completions from the live runtime path without changing CLI, TUI, server, JSONL, or goal storage behavior.",
      "v0.1.151":
        "ToolExecutionContext can now carry an extension registry plus thread and turn extension stores, and ToolExecutionActor notifies lifecycle contributors around normal tool execution. This makes completed, blocked, aborted, and not-implemented tool outcomes visible at the extension boundary without changing CLI, TUI, server, or JSONL wire behavior.",
      "v0.1.150":
        "Orca now has a runtime extension contributor kernel with typed per-scope ExtensionData and ordered tool lifecycle contributors. Goal tool progress has the first real contributor seed, so future goal, memory, task, and tool lifecycle work can move out of the main runtime loop without changing CLI or app-server behavior.",
      "v0.1.149":
        "Realtime server item projection now lives behind a shared RuntimeEventProjector reducer. Assistant messages, proposed plans, reasoning, command execution, MCP/dynamic tools, file changes, and workflow lifecycle events keep the same app-server wire shape while ServerRequestWriter no longer owns the projection state maps.",
      "v0.1.148":
        "Async subagent worker entrypoints now use grouped AsyncSubagentWorkerInput, AsyncSubagentWorkerContext, AsyncSubagentLaunchContext, and spawn context objects instead of long argument lists or clippy allowances. CLI worker startup, parent async launch, task registry state, worktree handoff, async completion payloads, subagent contracts, and TUI async subagent behavior stay unchanged.",
      "v0.1.147":
        "Child-agent runtime constructor inputs now flow through a grouped ChildAgentRuntimeContext instead of a long cwd/events/sink/instructions/memory/MCP/hooks/cancel/lifecycle/executor argument list. Existing orca_runtime::agent_child re-exports remain available, while child-agent loop setup, provider turns, compaction, response folding, tool execution, sync and async subagent contracts, workflow child agents, and TUI child-agent delegation behavior stay unchanged.",
      "v0.1.146":
        "Child-agent loop runner inputs now flow through a grouped ChildAgentLoopContext instead of a long request/cwd/instructions/memory/hooks/cost-tracker argument list. Existing orca_runtime::agent_child re-exports remain available, while loop setup, provider turns, compaction, response folding, tool execution, subagent contracts, workflow child agents, and TUI child-agent delegation behavior stay unchanged.",
      "v0.1.145":
        "Child-agent prompt entrypoint inputs now flow through a grouped ChildAgentPromptContext instead of a long argument list. Existing orca_runtime::agent_child re-exports remain available, while prompt-to-request construction, model override, child cost tracking, subagent contracts, workflow child agents, and TUI child-agent delegation behavior stay unchanged.",
      "v0.1.144":
        "Child-agent public entrypoints now live in a focused child_agent_entrypoints module instead of the agent_child facade. Existing orca_runtime::agent_child imports remain available through re-exports, while model override, child cost tracking, prompt-to-request construction, subagent contracts, workflow child agents, and TUI child-agent delegation behavior stay unchanged.",
      "v0.1.143":
        "Child-agent behavior tests now live in a focused child_agent_tests module instead of the agent_child facade. Existing orca_runtime::agent_child imports remain available through re-exports, while child-agent setup, provider turns, response folding, loop running, subagent contracts, and TUI child-agent delegation behavior stay unchanged.",
      "v0.1.142":
        "Child-agent request, result, runtime, and executor types now live in a focused child_agent_types module. Existing orca_runtime::agent_child imports remain available through re-exports, while loop setup, provider turns, response folding, loop running, subagent contracts, and TUI child-agent delegation behavior stay unchanged.",
      "v0.1.141":
        "Child-agent loop orchestration now lives in a focused child_agent_loop_runner module. Existing orca_runtime::agent_child imports remain available through re-exports, while loop setup, provider turns, response folding, tool-result folding, subagent contracts, and TUI child-agent delegation behavior stay unchanged.",
      "v0.1.140":
        "Child-agent provider response folding, tool request extraction, tool execution context, and tool-result folding now live in a focused child_agent_response_folding module. Existing orca_runtime::agent_child imports remain available through re-exports, while child-agent loop orchestration, provider turns, subagent contracts, and TUI child-agent delegation behavior stay unchanged.",
      "v0.1.139":
        "Child-agent model routing, provider hook execution, provider call dispatch, child compaction, and prompt-too-long retry/failure handling now live in a focused child_agent_provider_turn module. Existing orca_runtime::agent_child imports remain available through re-exports, while provider response folding, tool request extraction, tool result folding, subagent contracts, and TUI child-agent delegation behavior stay unchanged.",
      "v0.1.138":
        "Child-agent loop setup, provider bootstrap, conversation seed, approval policy seed, and child turn-budget state now live in a focused child_agent_loop_setup module. Existing orca_runtime::agent_child imports remain available through re-exports, while child-agent model routing, provider turns, tool folding, subagent contracts, and TUI delegation behavior stay unchanged.",
      "v0.1.137":
        "Runtime approval decisions, approval handlers, and config-backed interactive approval handling now live in a focused runtime_approval module. Existing orca_runtime::lifecycle imports remain available through re-exports, while approval policy resolution and interactive approval behavior stay unchanged.",
      "v0.1.136":
        "Runtime permission requests now live in a focused runtime_permission module. The existing orca_runtime::lifecycle imports remain available through re-exports, while request_permissions execution and turn permission overlay behavior keep the same runtime shape.",
      "v0.1.135":
        "Runtime request_user_input handling now lives in a focused runtime_user_input module. The existing orca_runtime::lifecycle imports remain available through re-exports, while request parsing, handler dispatch, answer completion, and cancellation failure behavior keep the same runtime and TUI shape.",
      "v0.1.134":
        "Runtime ORCA_HOME-scoped tests now share a poison-tolerant test environment lock helper. If one test panics while holding the shared environment mutex, later history, server, session, thread-store, and workflow-host tests can recover the lock instead of cascading into misleading PoisonError failures.",
      "v0.1.133":
        "Sandbox bash command construction now uses grouped WorkspaceWriteSandboxCommandContext and ReadOnlySandboxCommandContext inputs, with the macOS Seatbelt profile helpers also receiving focused profile contexts. Shell sessions, command/exec, bash sandboxing, network and filesystem policy flags, Unix socket allowlists, and non-interactive process preparation keep the same behavior while the sandbox API no longer exposes long argument lists.",
      "v0.1.132":
        "Runtime bash sandbox execution and one-shot shell spawning now use focused RuntimeBashSandboxContext and RuntimeBashOnceContext inputs. The model-visible bash flow still owns permission-profile sandboxing, network and filesystem permission retries, cancellation, task-registry handoff, output truncation, and diagnostics while runtime_bash.rs no longer needs internal long-argument helper escape hatches.",
      "v0.1.131":
        "Readonly tool-turn batch execution now lives in a focused runtime_readonly_tool_turn module with grouped readonly batch and tool-turn contexts. The main tool_turn dispatcher still owns request cursoring, child-tool policy checks, subagent batching, readonly batch selection, and normal tool turns while readonly hook gating, parallel execution, and result recording keep the same runtime behavior.",
      "v0.1.130":
        "Async subagent worker launch and completion now live in a focused subagent_async_worker module. The parent subagent executor still owns sync and batch execution, while worker spawn, task-registry usage writes, worktree handoff, and async result payloads keep the same CLI and subagent_status behavior.",
      "v0.1.129":
        "Server shell-session state now lives in a focused shell_manager module. Optional runtime shell session storage, lazy task-registry-backed initialization, shell CRUD/read/kill/reap/wait calls, and command/exec drain compatibility keep the same server shell protocol shape.",
      "v0.1.128":
        "Server pending-permission request state now lives in a focused permission_manager module. Runtime request_permissions waits, command/exec permission retry records, pending request removal, and permission request handler registration keep the same server permission protocol shape.",
      "v0.1.127":
        "Server active-turn lifecycle state now lives in a focused active_turn_manager module. Active turn controls, running turn handles, finished-turn reclamation, thread-specific reclaim waits, and session permission metadata merge keep the same server turn protocol shape.",
      "v0.1.126":
        "Server command/exec active process state now lives in a focused command_exec_manager module. Buffered output, streaming deltas, stdin writes, PTY resize, termination, output caps, sandbox diagnostics, and permission retry behavior keep the same server protocol shape.",
      "v0.1.125":
        "RuntimeToolActorContext now lives in a focused runtime_tool_actor module. Existing orca_runtime::lifecycle imports remain available through a re-export, while approval, hook, user-input, normal-tool, permission-overlay, and active-task behavior keep the same runtime shape.",
      "v0.1.124":
        "Runtime lifecycle state machine types now live in a focused runtime_lifecycle module. The existing orca_runtime::lifecycle imports remain available through re-exports, while task/turn ids, status mapping, event payloads, and RuntimeTurnRunner behavior stay unchanged.",
      "v0.1.123":
        "Runtime turn setup now lives in a focused runtime_turn_setup module. Agent loop still delegates through RuntimeTurnSetupStep, and the new module owns context budget setup, tool approval policy construction, and provider config composition while lifecycle.rs keeps actor/lifecycle primitives.",
      "v0.1.122":
        "Runtime conversation bootstrap now lives in a focused runtime_conversation_bootstrap module. Agent loop still delegates through RuntimeConversationBootstrapStep, and the new module owns RuntimePreparedConversation, borrowed-or-owned conversation storage, session bootstrap composition, and initial history recording while lifecycle.rs keeps actor/lifecycle primitives.",
      "v0.1.121":
        "Runtime steer application now lives in a focused runtime_steer module with a grouped RuntimeSteerInput boundary. RuntimeTurnOpeningStep and RuntimeProviderTurnStep still drain pending steer inputs into the conversation and history before the model call, while lifecycle.rs keeps ThreadSteerHandle storage and sheds another reducer slice.",
      "v0.1.120":
        "Runtime model-route orchestration now lives in a focused runtime_model_route module with a grouped RuntimeModelRouteInput boundary. RuntimeTurnOpeningStep still composes compaction, turn start, model routing, and steering in the same order, while lifecycle.rs keeps the actor/lifecycle primitives and sheds another reducer slice without adding a new long-argument surface.",
      "v0.1.119":
        "Runtime turn-start orchestration now lives in a focused runtime_turn_start module instead of lifecycle.rs. RuntimeTurnOpeningStep still composes compaction, turn start, model routing, and steering in the same order, while lifecycle.rs keeps the actor/lifecycle primitives and sheds another lower-level reducer slice.",
      "v0.1.118":
        "Runtime turn-opening orchestration now lives in a focused runtime_turn_opening module with a grouped RuntimeTurnOpeningInput boundary. RuntimeTurnIterationStep still composes opening and provider-cycle execution in the same order, while lifecycle.rs keeps the lower-level start/model-route/steer steps and sheds another reducer-sized layer.",
      "v0.1.117":
        "Runtime turn-iteration orchestration now lives in a focused runtime_turn_iteration module instead of lifecycle.rs. The outer runtime_turn_loop still delegates through RuntimeTurnIterationStep, provider-cycle behavior still lives in provider_turn, and lifecycle.rs keeps the opening/start/model-route pieces while getting smaller for the next reducer-style split.",
      "v0.1.116":
        "Runtime turn-loop orchestration now lives in a focused runtime_turn_loop module instead of lifecycle.rs. Agent loop still delegates through RuntimeTurnLoopStep with the same grouped input/executor objects and the same iteration retry/return behavior, while lifecycle.rs gets smaller for the next Codex/package-3-inspired reducer split.",
      "v0.1.115":
        "Shell-session bash execution now receives one grouped RuntimeBashInvocationContext instead of a long execute_bash_with_shell_session argument list. RuntimeNormalToolExecutor still owns the bash branch, permission overlays, cancellation, output truncation, task registry handoff, and network/filesystem permission retries keep the same behavior, while the bash boundary gets smaller for the next shell/session and async-subagent slices.",
      "v0.1.114":
        "Filesystem sandbox denials now recover more clearly across server command/exec and model-visible bash. Orca diagnoses macOS Seatbelt write blocks such as nested .git/index.lock failures, explains when they are sandbox scope issues rather than stale locks, requests a turn-scoped filesystem write grant when an approval handler is available, and retries the original command with the granted root.",
      "v0.1.113":
        "Tool-turn dispatch now receives one grouped RuntimeToolTurnsContext from provider response handling instead of a long run_tool_turns call. RuntimeStepContext, events, sink, conversation, history writer, tool requests, cost tracking, background workflow state, and child executors still flow unchanged while the provider-to-tool boundary gets smaller.",
      "v0.1.112":
        "Normal tool-turn execution now receives one grouped RuntimeNormalToolTurnContext instead of a long run_normal_tool_turn argument list. Tool execution, approval, result recording, plan-state recording, permission overlays, workflow/background state, and child executor handoff keep the same runtime behavior while the tool-turn boundary gets smaller.",
      "v0.1.111":
        "Tool approval gate inputs now move through one grouped ToolApprovalGateContext instead of a long handle_approval argument list. Config, events, sink, tool request, invocation, policy, strict auto-review, and delta emission still flow unchanged, while approval allow/ask/deny behavior and tool-call item emission keep the same public shape.",
      "v0.1.110":
        "Historical projected tool completions now rebuild through the shared complete_projected_tool_item helper in tool_item_projection.rs instead of thread_store/projection.rs calling MCP, dynamic, commandExecution, and fileChange completed-item constructors directly. Realtime and persisted history stay behavior-compatible while the remaining tool-item schema drift has one smaller ownership point.",
      "v0.1.109":
        "Runtime normal-tool routing now passes a grouped RuntimeNormalToolInvocation from the router into lifecycle actors instead of calling the long roots/cancel method directly. Bash shell-session execution, MCP/external fallback, permission overlays, cancellation, and output truncation keep the same behavior while the common tool path gets a smaller call surface for later shell and async-subagent work.",
      "v0.1.108":
        "Normal tool invocation now funnels through one runtime_normal_tool helper instead of letting lifecycle.rs instantiate the executor directly. RuntimeTaskActor and RuntimeToolActorContext still preserve the same bash, MCP, external, cancellation, and permission-overlay behavior, but the next shell-session and async subagent slices have a smaller call surface to build on.",
      "v0.1.107":
        "Tool-call argument streaming now reports progress end to end: a new tool.call.progress event and ToolCallProgress provider step flow through runtime and server, and the TUI renders received-bytes progress with cache-friendly updates. Adds an SSE streaming idle-timeout guard and fixes environment-variable proxy configuration and hook-timeout output handling.",
      "v0.1.106":
        "The normal-tool fallback path is now injectable through a focused RuntimeNormalToolFallbackExecutor boundary. MCP, TOML external, and built-in tool execution still use the same default orca-tools path, but the runtime can now test fallback context handoff without hardcoding that implementation.",
      "v0.1.105":
        "Normal tool execution now lives behind a focused RuntimeNormalToolExecutor boundary. The shell-session bash branch and the MCP/external/built-in fallback path move out of lifecycle.rs, while CLI, TUI, server, workflow, permission, and model-visible tool behavior stay unchanged.",
      "v0.1.104":
        "Runtime tool invocation dispatch now lives behind a focused RuntimeToolRouter boundary. ToolExecutionActor keeps invocation prep, approval, hooks, and result finalization, while workflow, subagent, task, permission, workflow IPC, and normal-tool routing move into the router without changing model-visible behavior.",
      "v0.1.103":
        "Runtime turn execution now carries cleaner grouped inputs: turn iteration, provider cycle, provider response, and tool turns share request-scoped context boundaries. This Codex/package-3-inspired slice reduces repeated runtime state plumbing while preserving CLI, TUI, server, tool, workflow, and history behavior.",
      "v0.1.102":
        "TUI child-agent execution now flows through runtime-owned request construction, model/cost setup, loop orchestration, provider handling, tool request extraction, and tool-result folding while TUI keeps only the interactive tool adapter. This keeps the new reasoning-effort configuration intact across child provider calls.",
      "v0.1.101":
        "Reasoning effort is now configurable (high or max, default max) via env vars, config file, and CLI arguments, carried on DeepSeek API requests. The TUI /model command becomes a two-step picker — choose the model, then the reasoning effort — with deferred apply, clean Esc cancellation, and a status bar that shows both.",
      "v0.1.100":
        "TUI polish: inline scrolling now detects real overflow via rendered-line-info, keeps auto-follow armed until content actually overflows, fixes CJK-aware wrap heights, moves memory extraction off the render thread, adds a live activity bar, and debounces inertial mouse scroll right after a turn completes.",
      "v0.1.99":
        "Runtime-special tool dispatch and small executors now live in a focused runtime_special module, keeping request_permissions, workflow IPC, subagent status, task list/stop, and workflow draft preview behavior intact while shrinking lifecycle.rs.",
      "v0.1.98":
        "Server submit-family dispatch now routes through a focused submit processor, preserving legacy submit, thread-bound turns, thread/start, thread/resume, and thread/fork behavior while leaving the generic router as a pure operation-family dispatcher.",
      "v0.1.97":
        "Server permission/respond dispatch now routes through a focused permission processor, preserving turn/session grants, strict auto-review, filesystem overlays, and network allow/deny behavior while shrinking the generic router.",
      "v0.1.96":
        "Server command/exec dispatch now routes through a focused command-exec processor, preserving buffered, streaming, stdin, resize, terminate, sandbox, and permission-profile behavior while shrinking the generic router.",
      "v0.1.95":
        "Server shell-session dispatch now routes through a focused shell processor, preserving shell start, write, update, close, resize, list, read, and kill behavior while shrinking the generic router.",
      "v0.1.94":
        "Server turn-control dispatch now routes through a focused turn processor, keeping interrupt, resume, and steer behavior intact while shrinking the generic router.",
      "v0.1.93":
        "Synchronous server thread query and metadata operations now route through a focused thread processor, shrinking the generic router while preserving thread/read, list, search, turns, items, and metadata behavior.",
      "v0.1.92":
        "Server-mode operation dispatch now lives behind a focused router boundary, preserving every existing wire method while opening the next request-processor refactor path.",
      "v0.1.91":
        "Runtime permission requests now share one overlay merge path for file-system grants, network domain grants, and strict auto-review, keeping request_permissions and bash retry behavior aligned.",
      "v0.1.90":
        "Model-visible bash now inherits the active permission profile's managed network policy, turns eligible proxy blocks into permission requests, and retries after a turn-scoped network allow.",
      "v0.1.89":
        "Streaming command/exec processes now share the managed network permission flow: eligible proxy blocks request a session-scoped allow, then restart the same processId and stream output after the grant.",
      "v0.1.88":
        "Command/exec can now turn managed network proxy blocks into a network permission request and retry the command after a session-scoped allow response, while denylist blocks remain final diagnostics.",
      "v0.1.87":
        "Managed command/exec network blocks now include the normalized blocked host in proxy diagnostics, giving clients a stable attribution hook for upcoming automatic network permission prompts.",
      "v0.1.86":
        "Session-scoped request_permissions network denials now override permission-profile allow entries, so interactive deny decisions can tighten later command/exec proxy policy.",
      "v0.1.85":
        "Session-scoped request_permissions network domain grants now persist on server threads and feed command/exec's managed proxy, so later commands inherit interactive allowlist decisions.",
      "v0.1.84":
        "Permission-profile Unix socket allowlists now flow into command/exec sandboxing on macOS, allowing configured AF_UNIX socket paths without enabling full network access.",
      "v0.1.83":
        "The managed command/exec network proxy now checks resolved socket addresses before connecting, blocking DNS names that resolve to local, private, reserved, or otherwise non-public targets.",
      "v0.1.82":
        "The managed command/exec network proxy now blocks local and private IP targets unless they are explicitly allowlisted, matching Codex's local-network guard while keeping allowlisted loopback workflows working.",
      "v0.1.81":
        "Permission-profile network blocks now preserve Codex-style proxy reasons, so command/exec clients can distinguish denylist hits from allowlist misses instead of seeing only a generic policy 403.",
      "v0.1.80":
        "The TUI conversation session now owns RuntimeThread instead of rebuilding InteractiveSession and RuntimeSessionLifecycle locally, completing the first headless/server/TUI runtime-state convergence pass while preserving TUI behavior.",
      "v0.1.79":
        "Headless exec now creates and runs long-lived agent state through RuntimeThread, aligning CLI turns with server-mode ownership while preserving JSONL sequencing, session hooks, history, verifier, and npm behavior.",
      "v0.1.78":
        "Server-mode threads now store their long-lived agent state through RuntimeThread, removing duplicated session/lifecycle/executor assembly while preserving thread projection, resume/fork, cancellation, and permission behavior.",
      "v0.1.77":
        "RuntimeThread now groups the runtime-owned interactive session and lifecycle state behind one boundary, creating the next convergence point for server, TUI, and headless execution without changing public behavior.",
      "v0.1.76":
        "The runtime protocol boundary now uses a small facade backed by focused command_exec, events, permissions, shell, thread, turn, and wire modules, preserving the public protocol API while making the next server dispatch split easier.",
      "v0.1.75":
        "ThreadStore now has a focused storage facade backed by separate types, local JSONL, writer, projection, pagination, and live-thread modules, preserving the public runtime API while shrinking the monolithic store file.",
      "v0.1.74":
        "Permission-profile network domain policies now run through a managed loopback HTTP proxy for command/exec, so allowed hosts can pass and denied hosts return a policy 403.",
      "v0.1.73":
        "Permission-profile filesystem globs now support configurable scan depth through glob_scan_max_depth / globScanMaxDepth, with inherited profile defaults and child-profile overrides.",
      "v0.1.72":
        "Permission profiles now expand bounded read/write/read-write filesystem globs into concrete command sandbox roots, keeping Codex-style split filesystem policies usable without weakening broad-glob safety checks.",
      "v0.1.71":
        "Runtime compaction now lives in a dedicated module, keeping prompt-budget hooks, summary persistence, and prompt-too-long recovery out of the lifecycle orchestration module.",
      "v0.1.70":
        "TUI history splits into native terminal scrollback for settled transcript output and a live bottom viewport for streaming content, plans, input, status, and modal/full-panel states.",
      "v0.1.69":
        "Tool-turn execution now lives in a dedicated runtime module, separating provider tool schema/invocation preparation from cursoring, batching, execution, and result folding.",
      "v0.1.68":
        "TUI tool approval gating now lives in the runtime interaction adapter, keeping approval request construction, preview generation, and interactive waits out of bridge orchestration.",
      "v0.1.67":
        "TUI runtime approval and request-user-input handlers now live in a dedicated interaction adapter module, and the site build includes the server prerender entry used by crawler-visible HTML generation.",
      "v0.1.66":
        "TUI runtime event projection now lives in a dedicated module, keeping EventEnvelope-to-TuiEvent mapping and workflow notification prompt shaping out of bridge orchestration.",
      "v0.1.65":
        "Persisted edit and write_file history items now project as Codex-style fileChange items, aligning thread-read history with realtime server streams.",
      "v0.1.64":
        "Persisted commandExecution history items now use shared projection builders while preserving command metadata placeholders and failed-command diagnostics.",
      "v0.1.63":
        "Realtime commandExecution lifecycle items now use shared projection builders, closing another app-server item-shape drift point.",
      "v0.1.62":
        "Realtime agent, plan, and reasoning lifecycle items now use shared projection builders, further tightening the app-server protocol boundary.",
      "v0.1.61":
        "Realtime fileChange and workflow lifecycle items now use shared projection builders, and the tag release gate runs server-heavy Rust tests serially on CI.",
      "v0.1.59":
        "MCP/dynamic completed-item projection is shared across realtime streams and history, and CI stdio MCP fixtures now launch through /bin/sh to avoid Linux ETXTBSY release flakes.",
      "v0.1.58":
        "MCP and dynamic tool completed-item construction now uses shared projection builders across realtime streams and persisted history, with failed command projection guarded against output-shape regression.",
      "v0.1.57":
        "Realtime streams and persisted history now share MCP and dynamic tool started-item builders, keeping first-class tool-call item shape aligned at creation time.",
      "v0.1.56":
        "Realtime and persisted tool item projections now share exit-code error normalization and completed-status checks, reducing the remaining mcpToolCall/dynamicToolCall schema drift.",
      "v0.1.55":
        "Realtime server streams and persisted thread projections now share MCP tool parsing, JSON argument parsing, MCP result shaping, and camelCase tool error helpers, with CI JSONL polling hardened for active background turns.",
      "v0.1.53":
        "Realtime mcpToolCall and dynamicToolCall item errors now include exitCode when tool completion reports one, keeping server streams aligned with persisted thread item projections.",
      "v0.1.52":
        "MCP initialize capabilities are now cached per server, so all-server resource/template discovery skips tools-only servers while explicit server filters still report that server's real error.",
      "v0.1.51":
        "MCP resource and template discovery now includes registry-level startup errors in all-server results, so failed MCP servers stay visible alongside healthy resource context.",
      "v0.1.50":
        "MCP resource templates are now model-visible through list_mcp_resource_templates, with resources/templates/list wired through stdio/SSE and partial per-server error reporting.",
      "v0.1.49":
        "MCP resource discovery now returns available resources even when another server fails, with per-server errors surfaced in the list_mcp_resources result.",
      "v0.1.48":
        "MCP resource tools ship with a hardened server-mode JSONL test harness, so noisy child-process output no longer flakes task_stop shell-session release coverage.",
      "v0.1.47":
        "MCP resources are now model-visible through read-only list_mcp_resources and read_mcp_resource tools, with stdio/SSE resources/list and resources/read support wired through the shared registry.",
      "v0.1.46":
        "Structured hook JSON stdout now validates declared actions and required string fields, so typoed or malformed hook outputs fail visibly instead of being silently injected or ignored.",
      "v0.1.45":
        "Tool argument validation now evaluates JSON Schema oneOf and anyOf branches before execution, keeping runtime rejection behavior aligned with advertised provider schemas.",
      "v0.1.44":
        "Model-facing file discovery now supports fuzzy path queries through glob mode=fuzzy, while preserving existing glob pattern behavior and list_files compatibility.",
      "v0.1.43":
        "Runtime turn orchestration now lives behind lifecycle-owned turn opening, provider cycle, iteration, loop, and loop-input boundaries, shrinking the agent loop entrypoint while preserving behavior.",
      "v0.1.42":
        "Claude Code-style workflow parity loop: generated drafts, edit/save/run controls, reusable workflow commands, evidence-bound reports, and process-tree timeout cleanup.",
      "v0.1.41":
        "Workflow concurrency control rewrite (Promise.allSettled with fail-fast), structured failure taxonomy (tool/MCP/token/schema), concurrency metrics in evidence bundles, and stress-test coverage.",
      "v0.1.40":
        "Workflow evidence bundles with standardized reporting (Markdown + JSON), automatic evidence capture at lifecycle checkpoints, and contract validation tests.",
      "v0.1.39":
        "Workflow child task list tools, typed output schemas for subagents, team tool allowlists, durable IPC state, and agent lifecycle observability.",
      "v0.1.38":
        "History/session persistence now flows through a dedicated SessionStore boundary, with runtime session/controller call sites aligned to the same entry point.",
      "v0.1.37":
        "Shell execution now honors the configurable timeout, with timeout-aware child process waiting shared by bash and external tools.",
      "v0.1.36":
        "Workflow agent runs now support worktree isolation, async handle recovery, and continue-on-failure phase fallback in the TUI workflow view.",
      "v0.1.35":
        "Bracketed paste support in TUI input; textarea soft-wrap rendering rewritten with accurate height calculation.",
      "v0.1.34":
        "Add a reusable real API release gate that verifies provider summary costs, CLI JSONL output, and server-mode streaming before tagging.",
      "v0.1.33":
        "Centralize runtime tool invocation records, approval request construction, and hook-modified request validation across built-in, MCP, and external tools.",
      "v0.1.32":
        "Add a typed runtime protocol boundary for server submissions and events while preserving the existing flat JSON wire format.",
      "v0.1.31":
        "Runtime-owned interactive sessions now centralize conversation, history, instructions, memory, hooks, MCP, cost tracking, and workflow task state before the protocol split.",
      "v0.1.30":
        "Workflow DSL and multi-stage runtime overhaul; TUI now shows workflow/task progress, elapsed time, notifications, and clearer approval choices.",
      "v0.1.29":
        "Refactor TUI session preloading for clarity; extract goal session ID helper; add unit tests for session restoration and goal control flow.",
      "v0.1.28":
        "Drop legacy deepseek-chat / deepseek-reasoner; tool arguments are JSON-Schema validated before any call; TUI text-wrap rewritten for wide chars and ANSI.",
      "v0.1.27":
        "Kill the cache-compaction storm: wire-equivalent gating + 60% hysteresis, persist inherited summary state across --continue and --fork.",
      "v0.1.26":
        "Update check falls back to npm registry (no rate limit); table rendering rewritten with progressive degradation down to narrow terminals.",
    },
    foot: {
      releases: "GitHub Releases",
      qq: "QQ Group 472309526",
      telegram: "Telegram",
    },
  },
  zh: {
    langName: "中文",
    aria: {
      home: "Orca 首页",
      language: "语言",
    },
    nav: {
      home: "首页",
      install: "安装",
      github: "GitHub",
    },
    header: {
      eyebrow: "更新日志",
      title: "Orca 历次发布。",
      subtitle:
        "版本遵循 semver；每条记录都链接到 GitHub Release 的完整说明，含校验命令、breaking change 与迁移提示。",
      latestLabel: "最新",
      readNotes: "查看发布说明",
    },
    related: {
      eyebrow: "相关指南",
      title: "从更新记录继续了解主要工作流。",
      links: [
        {
          title: "Terminal coding agent",
          body: "把 Orca 作为本地终端代码智能体，用 verifier-gated 流程处理真实仓库任务。",
          href: links.terminalCodingAgent,
        },
        {
          title: "DeepSeek coding agent",
          body: "了解 DeepSeek 原生推理、前缀缓存行为和本地历史如何协同。",
          href: links.deepseekCodingAgent,
        },
        {
          title: "GitHub workflows",
          body: "把 Orca 用在 issue triage、PR 准备、发布检查和代码库考古上。",
          href: links.githubWorkflows,
        },
      ],
    },
    summaries: {
      "v0.3.13":
        "让 headless resume 成为一等 CLI 能力。orca exec resume <SESSION_ID> 以全新预算范围继续保存的会话，resume --last 恢复最近会话，--resume-at <MESSAGE_ID> 只恢复到持久化消息边界为止。session.completed 现在携带持久的 session_id，文本模式退出时打印确切的 resume 命令，预算耗尽的运行会在 terminal projection 之前持久化类型化 session.checkpoint（status、reason、已消耗预算、最后提交消息、任务计划、resumable）。恢复时未提交的工具调用仍标记为 indeterminate——Orca 承诺可恢复，而非 exactly-once 执行。",
      "v0.3.12":
        "新增 runtime-owned Side Conversation，用来临时提问而不打断主任务。/side 会从父会话的原子快照创建独立、可丢弃的 child；Ctrl+/ 在父会话与 Side 之间切换，父任务仍可继续运行，Ctrl+C 只关闭并回收 Side。Side 的历史、memory、Goal 和 transcript 都不会合并回持久化父会话。TUI response projection 也会按 turn 与 item identity 隔离 provider response，避免旧流或部分流覆盖当前回复。",
      "v0.3.11":
        "增强 headless、TUI、MCP、server 与自动 memory 工作流之间的可靠性边界。headless max-turn trajectory 现在保留 terminal truth，并比较 streamed projection 与持久化记录；terminal waiter 会在取消时可靠 join；MCP SSE elicitation 会校验 request 和 terminal id，取消 in-flight response POST，并覆盖 decline、malformed 与 wire cancellation；继承的 network grant 会只询问一次并持久化 retry；JSONL tool approval 继续由 runtime 统一管理；自动 memory 写入受锁保护且可安全取消；跨 surface contract fixture 也改用确定性的本地资源。",
      "v0.3.10":
        "TUI 交互生命周期现在端到端归 runtime 所有。审批、用户输入、取消、终态结算与 replay 统一通过一条 typed surface rail，不再维护并行的客户端状态。Goal 工具 Allow 与 Deny 会持久、精确地结算 usage；Deny 不执行工具并暂停 Goal，显式 resume 则启动新的 fenced run。会话浏览会忽略符号链接、FIFO、设备及其他非普通 transcript 文件，避免特殊文件卡住 picker 或历史读取。",
      "v0.3.9":
        "Plan 模式从纯权限开关升级为正式的 plan-then-execute 工作流。Agent 在 Plan 模式下进行只读探索，完成后输出正式 proposed plan，底部弹出审批栏。批准后自动恢复进入 Plan 前的模式并执行计划；拒绝则留在 Plan 继续修改。每回合注入 mode context 确保 mid-session 切换模式即时生效。PageUp/PageDown 和滚轮可回看长计划。",
      "v0.3.8":
        "增强长会话与委派任务的可靠性。context footer 现在只显示剩余百分比，/status 提供剩余与总 token 数；重复 compaction 与排队中的 steer input 在恢复时不会重新带回已丢弃上下文，也不会丢失用户意图；异步 subagent 结果可持久化并分页读取；后台任务完成后会主动通知；stdio MCP 在传输故障后自动重连；并发 JSONL 写入器会在复用事件序号前失败。Workflow run 可通过共享 tokenBudget 限制总用量，/skills 提供可搜索 picker，斜杠设置也统一读取已提交的 runtime state。",
      "v0.3.7":
        "完成 session context 与 TUI projection reliability 收口。恢复历史会话时会在首个 resumed turn 前还原 provider prompt 占用；revision-aware projection 不会让旧 surface snapshot 覆盖更新的 context footer；reasoning、message、plan stream 在 hydration 时保留打开顺序；已经流式展示的 completed response 不会再渲染一遍。v0.3.6 的 delegated execution、transcript lock、retry diagnostics 与 task registry 保证继续包含在本版本中。",
      "v0.3.6":
        "增强 runtime reliability，统一 delegated work 与 durable task 状态。同步、异步和 workflow 子 agent 现在继承同一份序列化执行策略快照；同一 thread transcript 的明文与压缩路径在 append 和 rewrite 时共用稳定的跨进程锁；provider 压缩重试与被截断的 tool 输出会保留在任务摘要中；session 完成事件前会先发布完整 task registry。前台 composer 仍可继续使用，activity line 会持续展示运行中或等待审批的后台任务。",
      "v0.3.5":
        "新增唯一规范名称 ask_user_question，一次可提出 1-4 个结构化问题，支持带说明的选项、preview、多选、自定义答案和取消，并复用 runtime-owned TUI 交互 broker。修复优化构建中 Goal mode 未发起 provider 调用、前台任务期间 /workflows 不可用，以及未知斜杠命令被错误发送给模型的问题。Terminal-Bench 现在从挂载二进制读取版本，在不扩展 Harbor 封闭 AgentContext 模型的前提下保留 JSONL trajectory，使用受支持的过滤参数，并阻止生成的 benchmark 产物进入 Git。",
      "v0.3.4":
        "修复大模型窗口下的 context 指示器与自动压缩策略。TUI 现在根据 provider 回报的真实 prompt token 计算完整模型窗口的剩余百分比，不再拿本地估算值对照旧的 96k 压缩预算。自动压缩默认在模型窗口 80% 时触发，以 90% 作为硬安全线，并固定保留约 48k token 的近期上下文后总结更早历史。新会话现在默认使用沙箱内自主执行的 auto-edit；suggest、full-auto 和 plan 仍可显式选择。现有绝对压缩阈值覆盖仍保持兼容。",
      "v0.3.3":
        "完成 Orca runtime 与架构审计整改。Goal 持久化和 terminal reply 现在按顺序结算且不会阻塞 Tokio actor，operation 取消会收拢其创建的任务树；session 切换、fork、rename 与陈旧事件都受事务和 attachment fence 约束。Runtime surface 改为显式 facade，tool schema 与 provider 解耦，根 MCP 生命周期归 RuntimeHost 所有；transcript streaming、reflow、search、去重、usage 与 projection state 也具备有界或 revision 校验语义。",
      "v0.3.2":
        "修复通过 npm 包装器启动时的崩溃问题（node stdio:inherit 会在继承的终端 fd 上设置 O_NONBLOCK）。Orca 现在在启动时清除非阻塞标记，并为 stdout 添加 EAGAIN/EINTR 重试写入器，消除 macOS 上快速调整终端窗口大小时的 os error 35。同时新增 DeepSeek V4 低思考强度支持，正确发送 thinking 配置和默认 max_tokens。",
      "v0.3.1":
        "Orca 的 TUI 现在具备完整的会话生命周期交互。/resume 是唯一的已保存会话入口，同一个选择器内支持恢复、分叉、重命名、归档、删除和复制 Session ID；/new 开启空白会话，/fork 将当前上下文复制到新的持久化会话，/rename 修改当前会话名称，/status 查看运行状态，/copy 复制已完成的 Assistant 回复。可恢复任务改为显式 Continue 或 Cancel，退出时会打印可直接返回当前会话的 orca --resume 命令。",
      "v0.3.0":
        "Orca 现在原生支持 Windows x64 与 ARM64，覆盖 CLI、TUI、Shell Session、沙箱、更新、持久化、npm 包和 GitHub Release。PowerShell 7、Windows PowerShell 与 cmd.exe 使用各自的命令方言，ConPTY 提供交互终端，AltGr 输入、剪贴板、进程树清理、原子替换和跨进程锁按 Windows 语义实现。PowerShell 安装器会校验 checksum，安装主程序与沙箱 helper，并支持配置、修复或移除按工作区绑定的沙箱 capability。发布前由原生 x64 和 ARM64 runner 执行平台契约与完整工作区测试。",
      "v0.2.56":
        "CLI 二进制现在只负责参数解析和转发：配置、启动、更新、历史、信任、workflow、协议与 worker 生命周期都下沉到 orca-runtime 和 orca-tui。无状态 JSONL submit 不再依赖已持久化 thread，由 runtime 完整拥有 turn，并在 EOF 时精确取消和结算。macOS Seatbelt 改用系统绝对路径、参数化路径规则、受保护 metadata 写入根目录与 fail-closed 强制；信任和命令输出失败也会向上返回，不再误报成功。",
      "v0.2.55":
        "ACP 与 JSONL server 已完成 v0.2.54 开始的 runtime-owned typed surface 收敛。ACP 的 session 准入、prompt 绑定、replay、terminal flush、取消、capability settlement 与有界 transport supervision 现在统一由 runtime 拥有；JSONL 的 thread、control、permission、user-input 和 MCP 路由则收敛到同一个 surface adapter，并保留 durable request identity、EOF settlement 与重启恢复。发布门禁会用真实二进制覆盖 TUI、ACP 和 server，同时校验 release archive、npm tarball、checksum、registry integrity、package alias、binary identity 与干净安装的一致性。",
      "v0.2.54":
        "生产 TUI 已完成 runtime-owned typed surface 迁移收口。App loop、agent runtime 与 action dispatcher 只持有一份 typed surface control；prompt 准入、durable batch、operation/generation fence、交互、取消、terminal finalization、workflow task 状态和重启恢复都由 RuntimeHost 统一拥有。Assistant 与 tool 输出只在持久化提交后投影，交互响应会先落盘再唤醒 waiter，manual compaction 不会在拿到 terminal receipt 前误报成功，重启则从精确 snapshot 恢复原有 owner，而不再由 renderer 重新定义 turn 语义。ACP convergence 将在本次 TUI 发布后继续，JSONL compatibility 更后置。",
      "v0.2.53":
        "Goal Mode 完成了首个由 runtime 真正拥有的 TUI 纵向闭环。Set-and-run、resume、pause 与取消都通过 typed command；Goal 状态、outer-turn 进展、continuation 决策和 terminal 结果会先持久化，再投影到 TUI 或唤醒 waiter。重启恢复保留精确 owner lease、operation/generation fence、待确认 mutation receipt、进展屏障与重复 gap streak。MaxInnerTurns 仍可续轮，plan-only 工作会计为进展，成本预算耗尽映射为 UsageLimit 暂停，exact retry digest 同时绑定 usage、progress、verification、continuation 和 terminal 语义。",
      "v0.2.52":
        "Goal continuation 现在区分已推进、可恢复中断和真正阻塞。触达 inner-turn 上限会保留 MaxInnerTurns 原因，提前注入软着陆提醒，并通过结构化交接继续下一轮；交接包含目标、预算状态、未解决 gap、当前 task plan 和有界 assistant checkpoint。成本预算耗尽、取消、审批、验证失败等阻塞结果仍会暂停。独立的 durable watchdog 只把实质工具执行或结构化计划变化计为进展，在 SQLite 恢复后保留进展屏障，连续三次重复同一 model-fixable gap 或连续八次 inner-turn 中断时暂停。",
      "v0.2.51":
        "普通 TUI turn 现在端到端运行在 runtime-owned typed surface 上：prompt 准入、原子 durable commit、assistant 与 tool 投影、审批与权限响应、取消、terminal 清理、snapshot replay 和重启恢复共用同一份 RuntimeHost 事实。恢复后的控制仍绑定原 operation 与 generation，交互响应会先持久化再唤醒 waiter；生产测试覆盖真实 Record-to-Resume 历史与 PTY 终端恢复。本版本同时包含 ACP typed prompt、replay、permission 与有界 RPC bridge，但不改变 Goal、workflow 或 JSONL compatibility。",
      "v0.2.50":
        "Goal Mode 不再设置固定 outer-turn 或 continuation 上限。RuntimeHost 只根据语义状态、取消、待处理交互、workflow 所有权、进展与 token budget 决定是否续轮；continuation_count 继续持久化，但只用于账本与事件观测。这消除了 Goal 在 64 个正常 turn 后被错误映射为 Paused(NoProgress) 的终态，同时保留预算、stall 与用户控制边界。",
      "v0.2.49":
        "Goal Mode 现在由一个 runtime owner 统一管理生命周期、continuation 准入、取消、恢复、usage 与持久化。模型的终态声明改为类型化 intent，并在 turn 结束时审计；SQLite 取代直接 JSON 写入，同时提供迁移与崩溃恢复；role-safe context 和语义事件让 TUI 与 ACP 投影保持一致。五组真实 DeepSeek 场景验证了完成、拒绝完成、真实阻塞、取消与恢复，且没有陈旧 continuation 或未关闭 run。",
      "v0.2.48":
        "ACP 初始化现在从 RunConfig 上报 Orca 二进制发布版本，不再误用内部 orca-runtime crate 的版本。集成测试同时隔离 ORCA_HOME，验证每个 session 保留请求的工作目录，并覆盖 hosted operation handle 安装前到达的取消请求。",
      "v0.2.47":
        "Orca 新增通过 --mode=acp 启动的 stdio Agent Client Protocol 适配层。ACP session 与 prompt 直接投影到 RuntimeHost thread 和 hosted turn，EventEnvelope 流转换为标准 session/update 通知，取消则通过 OperationHandle 抵达活动 Generation Fence。现有内部 JSONL server 协议保持不变。",
      "v0.2.46":
        "Goal Mode 控制工具现在由展示它们的同一个 runtime 执行。get_goal、create_goal 和 update_goal 会在普通工具 worker 边界之前使用已记录 session 与 live extension context，旧的 thread-local callback 已删除。模型参数错误仍可在同一轮自纠；缺少控制面 owner 或持久化失败会结束一次 turn、原子 stall active Goal，并清除陈旧 Goal context。真实 DeepSeek gate 已验证先执行一个普通工具，再且仅调用一次终态 update_goal，后续 continuation 为零。",
      "v0.2.45":
        "权限模式现在与执行边界一致：auto-edit 在工作区沙箱内自主执行，full-auto 同时启用自动批准与 danger-full-access，不再在沙箱失败后弹出越界授权提示。",
      "v0.2.44":
        "macOS Sequoia 沙箱 shell 解析问题已修复。sandbox-exec 调用现在使用 /bin/sh 而非裸 sh，绕过了 macOS 15 在 seatbelt 沙箱内拦截的 /private/var/select/sh 内核查找。这消除了 full-auto 模式下每次工具调用都会弹出的多余 \"Unsandboxed Shell Required\" 审批提示。",
      "v0.2.43":
        "Linux 的 fail-closed 强制现在仅限于严格的受限只读策略：未信任目录与严格只读模式在 bubblewrap 和 Landlock 都无法执行时仍会拒绝运行。非严格 capability mode（workspace write 与全局只读）在策略需要仅 bwrap 才能表达的特性、且 PATH 上没有 bwrap 时，保持其既有的 Landlock 加 seccomp 或纯 shell 兼容回退，与参考 agent 对内置 profile 的 fail-open 行为一致。release 测试运行器不再安装 bubblewrap，因此 CI 直接验证 Landlock 加 seccomp 的回退路径。",
      "v0.2.42":
        "Linux 命令隔离现在优先使用 bubblewrap 提供挂载、namespace、capability 与网络边界；当策略可表达时则回退到 Landlock 加 seccomp。严格的受限只读策略在两种后端都无法执行时会拒绝运行。folder trust 会把用户决定持久化在仓库外：未信任目录不会加载 project config、instructions、skills 或命名 workflow，并采用只读、无网络默认值；显式 capability mode、权限规则和网络代理授权仍保持原有优先级。runtime lifecycle 测试现在使用显式的 danger-full-access capability profile，证明该既有覆盖仍有最高优先级，同时不会放宽 Linux 的 fail-closed 默认值。",
      "v0.2.36":
        "前台 subagent 现在拥有一条由 runtime 统一管理的调用生命周期，覆盖准入、子取消、worker join、panic 分类、schema 校验、worktree 收尾、usage 与 exactly-once terminal。中断同步委派时，会等待子任务完成清理后再结束 turn 或接收下一次提交；子任务 panic 会变成 indeterminate 结果，不再逃逸为 RuntimeHost panic。异步委派会立即把 durable task 投影到 TUI，同时不再创建无法闭合的前台 lifecycle；原子 PID adoption 避免快速 worker 的新状态被父进程旧快照覆盖，前台 interrupt 也继续与显式 task_stop 取消相互独立。旧的单子任务 inline loop、batch scoped runtime、重复格式化、陈旧 adoption 路径和源码形状测试已删除；跨进程 lease 与 stale-owner takeover 留给 P1.4。",
      "v0.2.35":
        "顺序执行的普通工具现在由 runtime 作为子生命周期统一拥有。RuntimeToolCallRuntime 负责准入、started 状态、取消策略、worker、join、panic 分类、权限增量和 exactly-once terminal；输出、审批与 MCP elicitation 通过有界类型化桥接传递。中断 bash、外部工具或 MCP 调用时，会先等待进程和 transport 完成清理，再结束 turn 或接收下一次提交；WaitForTerminal 会保留已经观察到的变更结果，启动后的 worker panic 会标记为 indeterminate。同一 turn 内获得的权限会在后续 sibling 调用前合并。旧的借用式 normal executor、fallback owner、inline 路径和源码形状测试已删除；subagent 仍是明确的 P1.2c 后续边界。",
      "v0.2.34":
        "中断 TUI 或 server turn 现在会传递到已经启动的并行只读工具调用，等待所有 worker 和 transport 完成清理后，才发布 operation terminal 或接收下一次提交。RuntimeToolCallRuntime 统一拥有每次调用的并发 permit、取消、started 状态、blocking task、join、panic 分类和 exactly-once terminal，同时保持 provider 顺序。MCP resource list、template 和 read 请求均可取消；stdio 会在取消后重连，SSE transport 可继续复用。旧的 orca-tools batch scheduler 已删除；普通工具和 subagent 仍是明确的 P1.2 后续切片。",
      "v0.2.33":
        "每次提交的 prompt 现在只有一个准入所有者和一条持久化 user 记录。Hosted turn preparation 会先持久化带身份的 user message，再把它提交到模型上下文；agent loop bootstrap 则显式区分独立 child-agent conversation 与借用的 hosted session，借用路径不会再重复写入初始历史。实时 thread read、turn 分页、item 分页、冷启动 ThreadStore、重启与 resume 都只会为每个逻辑 turn 返回一个 user item，并保持 turn/item id 稳定。已有重复历史继续可读，不增加投影时去重层。",
      "v0.2.32":
        "每个已完成的 DeepSeek 响应现在只有一个可持久化的 canonical 事实。Agent message、reasoning 与 proposed plan 的 id 会在流式输出前分配，并在实时投影、审批暂停与续接、进程重启、分页和 resume 后保持不变。Runtime 只持久化一个类型化 model.response.completed 事件，不再额外写入第二份 assistant 记录；模型 replay 与 ThreadStore 历史都归约同一事件。旧版合并 assistant 记录继续通过唯一隔离的 reducer 读取，格式错误的当前 completion 则 fail closed。真实 DeepSeek gate 会跨进程比较完整持久化 item 对象及内外部 id。",
      "v0.2.31":
        "已记录会话的 turn 与 item 现在使用不透明稳定身份，在重新加载、resume、compaction、兼容修复、分页、重命名、归档和压缩后都不会改变。逻辑 turn id 与 runtime task id 已彻底分离，并发 thread 即使都处于第一轮，也不会在 server 控制路由中发生碰撞。新记录使用类型化 UUIDv7 身份，tool 与 workflow item 保留各自领域 id；旧历史通过唯一隔离的 fallback 继续可读。真实 DeepSeek gate 会先记录一轮、重启进程、resume 同一 thread，再同时验证上下文连续与旧 id 稳定。",
      "v0.2.30":
        "生产 TUI 现在通过同一个进程级 RuntimeHost 执行前台 turn、流式中断、审批、用户输入、MCP elicitation、后台 provider 与已保存 workflow。RuntimeHost 统一拥有取消、join、终态事件、usage 提交和 shutdown 清理；重复的 TUI provider/tool/workflow loop 与 TaskSupervisor 已删除。真实 DeepSeek 长流被取消后会释放前台 operation，下一次提交可以干净启动；空闲状态下重复的 Goal 刷新也不会重复显示同一条通知。",
      "v0.2.29":
        "Runtime 现在提供进程级 RuntimeHost，并为每个会话使用一个有界 ThreadActor，通过类型化 operation handle 与 completion terminal 统一 headless 和 TUI turn 的所有权边界。结构化 @ mention binding 现在可以解析文件、skill、plugin 和 MCP resource，恢复被拒绝的提交，并让 mention search 与输入历史和用户数据隔离。TUI 的选择、剪贴板、输入历史、状态栏格式和提交提示也得到优化。",
      "v0.2.28":
        "Server turn cancellation 现在由 generation 独占。Interrupt 会永久取消当前 DeepSeek 执行；resume 等待旧 generation 返回后，在同一个逻辑 turn id 上用新的 scope 重启，并且不会重复追加原始 prompt。Permission、user-input 和 MCP waiter 都支持取消并按 generation 隔离；过期的 steer 与响应会被拒绝，被替换的 generation 不能发布过期取消错误或终态。第一代 interaction request id 保持兼容，续接 generation 会增加内部后缀。真实 DeepSeek gate 会在首个 stream delta 后 interrupt，再验证同一 turn 只产生一个成功终态。",
      "v0.2.27":
        "每次 TUI 提交、手动 compaction、Goal 操作和已批准的后台续轮现在都会获得新的 one-shot cancellation scope，并带有稳定的 operation id。Esc 与 Ctrl+C 只取消当前 scope，因此中断 DeepSeek 流后，后续 reset 不会把旧操作重新激活，也不会让下一轮一开始就处于已取消状态。生产 TUI 中的 reset 调用已全部删除；agent-loop 行为测试会取消延迟中的第一轮，再证明第二次提交拿到不同 scope、正常产生输出并成功完成。CLI 参数、TUI 快捷键与流程、server JSONL、持久化记录和 DeepSeek 行为保持兼容。Server turn/resume 的 reset 路径仍是明确的 actor-owned 后续边界，不会被当作永久兼容层。",
      "v0.2.26":
        "TUI 现在通过容量为 256 的 mailbox 接收 runtime event，并通过容量为 64 的 mailbox 接收用户操作；满载时采用阻塞背压，因此终端渲染变慢或暂停时不会无限积压，已准入的输出、审批、错误与终态仍保持 FIFO 顺序。Runtime compaction 和已批准的后台续轮现在通过 EventObserver 直接投影原始的类型化 EventEnvelope，不再先写入带 partial-frame buffer 的 JSONL writer 再反序列化。Provider stream、mention catalog 刷新和静默 child-agent 事件丢弃也都具有明确的有界所有权，静默 drain 线程会在返回前完成 join。CLI/server JSONL、持久化记录、DeepSeek 行为、TUI 快捷键和交互流程保持兼容。",
      "v0.2.25":
        "启用网络权限策略的 TUI bash 与 server command 现在由同一个托管 supervisor 拥有全部代理连接。并发连接上限为 32；超过上限的客户端会收到有界的 connection-limit 响应，不再创建额外 worker。请求行与 header 会在解析前执行字节和数量准入，网络阻断报告使用固定容量的非阻塞队列，DNS、上游连接与 socket I/O 都有明确时限。停止命令、取消 turn 或销毁代理时，会先停止准入，再取消并等待全部活动连接，关闭 CONNECT tunnel 两端，最后 join supervisor 线程。CLI 参数、权限 profile、代理环境变量、TUI 流程、server/JSONL 形状和持久化记录保持兼容。",
      "v0.2.24":
        "每个已接受的工具调用现在都会得到一个真实且唯一的终态结果，包括中断、执行前拒绝和未启动的同批调用；旧历史中缺失的终态会修复为 indeterminate，绝不会重放旧调用。普通进程的 stdout 与 stderr 分别限制为 1 MiB，文件读取和精确编辑会在准入前拒绝非普通文件、二进制、非法 UTF-8、持续增长及超限输入。外部工具、MCP server、workflow、异步 subagent、验证命令、server turn、shell 和搜索管理器都有明确的清理或 reaper 所有者；MCP 与 WorkflowHost 传输和关闭路径具备边界；已观察到的完成结果优先于取消和超时竞争；内部 worker API key 不再持久化，也不会出现在进程参数中。Windows 后代进程树清理、WorkflowHost 总时限及托管代理连接上限仍是后续工作。",
      "v0.2.23":
        "Orca TUI 迎来接近原生体验的鼠标文本交互。在 transcript 上拖拽即可选中文本，编辑器风格的选区高亮按主题取色并保留语法前景色；松开鼠标即通过 OSC 52 写入系统剪贴板（VS Code、iTerm2、kitty、WezTerm 及 SSH 会话可用，macOS 额外以 pbcopy 兜底），状态栏同时浮现短暂的 `copied N chars to clipboard` 提示。选区锚定在内容坐标而非屏幕坐标，流式输出和滚动不会移动已选内容。软换行的段落复制回来仍是一行，折行处被丢弃的空白会补回，而被硬切断的长词（如 URL）保持完整。双击选中光标下的单词并立即复制；拖到 transcript 首行或末行时按动画节拍持续自动滚动，指针静止选区也能继续增长；脱离自动跟随时浮现 `Jump to bottom` 按钮，点击即回到底部并恢复跟随。过时的 `shift+drag to copy` 状态栏提示已移除，滚轮滚动行为保持不变。",
      "v0.2.22":
        "Orca 的 `@` mention 现在一次覆盖多个工作区根目录和所有候选类型。`orca-file-search` 会话可以接受多个根目录，来自不同根目录的相同相对路径保持独立身份，browse、fuzzy、exclude、Git-ignore、取消与百万路径级别的边界行为不变。文件、Skill、Plugin、MCP Resource 与 Resource Template 现在共用同一套带类型的 `MentionCandidate`/`MentionTarget` 模型，稳定 id 来自完整 target 而非展示文本，同名结果不会再互相覆盖。选中候选项会记录一个隐藏的原子绑定：它会随更早的编辑重新定位，遇到重叠编辑会失效，并在提交时展开精确选中的根目录、Skill 路径、插件 manifest 或 MCP 资源，而不是重新解析可见文本。兼容 Codex 的 `fuzzyFileSearch/*` app-server 协议现在接受显式多根输入，新增的 `mention/search/*` 协议则绑定到具体 thread，按该 thread 自身的工作区根目录和 MCP registry 发现并展开候选项。TUI 与 app-server 提交共用同一套展开与校验代码，未绑定的旧版 `@file`/`$skill` 输入继续有效，搜索会话的 reaper 在停止或关闭时会被保留并 join，避免已停止的会话再输出迟到内容。",
      "v0.2.21":
        "DeepSeek turn 如果结束时既没有可见正文也没有工具调用，Orca 现在会发起一次有上限、会改变请求语义的恢复请求，而不是原样重放。临时纠正指令会保留已经有效的 tool-call reasoning 与 tool result，不会把不完整响应写入历史，也不会在 TUI 重复展示已经出现过的 recovery reasoning，并保留两次请求上报的 usage。启用预算时，前台请求会串行准入并在后续 turn 开始前预检；detached completion 则通过任务关联的 background_task.provider_response 持久化定价后的 usage 与脱敏诊断，不会覆盖全局 session completion。恢复会话使用 session.usage_baseline 延续累计核算，Goal token 只计算 input 加 output，不再重复加入 cache hit。本次发布同时把 tag 驱动的 GitHub Actions 流程升级到 v5 的 checkout、artifact 与 Node setup actions。",
      "v0.2.20":
        "TUI 现在会在完整交互链路中控制长内容占用。大段粘贴在输入框和 transcript 中保持折叠，但提交给模型的正文不变；Goal 目标与状态、Task Plan 步骤和工具 target 会按终端显示宽度省略；长审批内容不会再把决策选项挤出；Slash 与文件菜单会跟随当前选择滚动；响应式底栏优先保留权限模式和 context。权限模式分别使用蓝、紫、红、青绿语义色。",
      "v0.2.19":
        "macOS Seatbelt 现在允许沙箱内的测试运行器只向自己的子 worker 发信号，因此 Vitest、Tinypool、Jest 等 worker pool 能在失败或退出时正常清理。触发本次修复的事故中，10 个遗留 Node worker 合计占用 40.51 GiB，而 Orca native 约 30.2 MiB、npm wrapper 约 11.8 MiB；这不是 Orca transcript heap 泄漏。workspace-write 与 read-only profile 仍无权影响无关进程。",
      "v0.2.18":
        "DeepSeek 生成未知或畸形 function name 时，Orca 现在会把它记录为可失败的 tool result，并在同一个 agent turn 内返回给模型纠正，不再直接触发 provider terminal error 并暂停 Goal。流式与非流式响应都会保留原始 call id、名称和 raw arguments，但绝不会从命令形名称推断或执行 bash；registry validation 会在审批、hook、任务创建和执行前拒绝该调用。",
      "v0.2.17":
        "活动 Goal 的计时现在会跨自动续轮累计：已持久化的完整 turn 用时与当前 turn 增量合并显示。/goal resume 会保留目标、预算、token 用量、累计时间和创建时间；同 session 与跨 session 恢复使用一次原子迁移，目标 session 已占用时拒绝覆盖，失败时保持原状态。恢复历史后会在 TurnStarted 前投影 Goal，因此第一帧 running 状态就包含已持久化的时间基数。",
      "v0.2.16":
        "TUI 上下文压缩现在有可见、可中断的完整生命周期。自动 soft-limit、hard-limit、prompt-too-long recovery 与手动 /compact 都会在阻塞工作前显示 Compacting context；Ctrl+C、Esc、Ctrl+G 可取消 hook 与 DeepSeek 流式摘要。等待响应头、重试等待、错误响应体和 SSE 响应体读取都会与取消竞争，兼容同步 facade 会在返回前 join provider worker，同一 SSE frame 内的后续事件也会立即停止交付。畸形或提前结束的 SSE 会显式失败，只有尚未产生可见输出时才重试；已知工具的非法 JSON 参数会保留，并在审批、hook、任务创建或执行前完成 schema 校验，作为可纠正的 tool failure 返回。只有持久化和 post-compact hook 完成后才会回到 idle，旧版 compacted 事件仍兼容。",
      "v0.2.15":
        "TUI 恢复会话时会先移除旧历史中只有 reasoning、没有可见正文或工具调用的 assistant turn，再把历史重放给 DeepSeek。新的同类无效响应会触发重试而不会写入历史；与有效工具调用关联的 reasoning 仍会完整保留。发布门禁也新增了真实 DeepSeek API 的畸形历史恢复验证。",
      "v0.2.14":
        "Server-mode MCP elicitation 现在与 TUI 路径保持一致。stdio MCP tool 在 turn 中发送 elicitation/create 时，Orca 会发出稳定的 mcp_elicitation_request 事件，按 requestId 接收 mcp_elicitation/respond，把 accept/decline 写回 MCP server，然后继续原来的 tool call。Server shell/list 也不再回收 command/exec 拥有的 backing shell，command_exec_completed 会继续由 command/exec 控制路径发出。",
      "v0.2.13":
        "Runtime 任务输出现在通过有上限、UTF-8 safe 的 task-output store 保留。长时间运行的 TUI bash 和 command/exec 会话不再无限持有 stdout/stderr；retained output 被裁剪后，command/exec 流式输出仍会继续遵守累计输出上限；完成、停止或权限拒绝的终端命令路径会清理已保留输出。",
      "v0.2.12":
        "TUI 滚动性能迎来全面重构，长会话保持流畅。帧调度器会合并滚轮事件并限制每批处理数量，消息渲染改走基于版本的缓存，而不是每帧重绘整个 transcript，虚拟视口只渲染可见区域的消息。滚动偏移改为 usize，超过 65,535 行的会话也能正确滚动，底部状态栏去掉了 scroll: N/total 指示。",
      "v0.2.11":
        "TUI 键盘处理现在走 context-aware shortcut resolver。Global、composer、running-turn 和 approval-dialog 快捷键行为保持不变，但 resolver、测试和快捷键帮助路径共享同一个绑定边界，后续 keymap 与任务控制改动会更容易验证。",
      "v0.2.10":
        "TUI 的 compacted-context notice 现在会保留 runtime compaction reason 与 strategy。长 DeepSeek 会话里，用户能看到 Orca 是接近 token limit、到达 hard limit，还是为了 prompt-too-long recovery 而压缩，并显示被折叠的消息数量，不再只有泛化的前后消息总数。",
      "v0.2.9":
        "TUI 自动压缩和 prompt-too-long retry recovery 现在都走 runtime compaction boundary。可见的 context meter、compacted-context notice 和压缩失败错误仍保持同样的 TUI 形态，但主 TUI loop 不再自己拥有 context-pressure 决策和 retry state，减少它与 server、child-agent 压缩路径之间的漂移。",
      "v0.2.8":
        "Command/exec sandbox 与 permission-profile resolution 现在归属到 focused server module，不再压在泛化的 server loop 里。TUI bash 和 server command/exec 仍然共享同一套 sandbox 行为与 JSON wire shape，但 permission-profile 边界更容易测试，为后续网络、文件系统和任务控制改动降低回归风险。",
      "v0.2.7":
        "Core 现在拥有 user、persisted、assistant-message、proposed-plan 和 reasoning transcript item 的可复用投影类型。Runtime projection 仍然输出相同的 TUI/server JSON，但 live stream、active steer message 与 resumed history 会先经过同一个 typed item 边界再序列化，减少用户看到的 transcript card 漂移。",
      "v0.2.6":
        "TUI proposed plan 现在会作为独立的 scrollback 消息显示，不再把 <proposed_plan> 标签泄漏进普通 assistant 文本。Server 和 TUI 共用同一个 UTF-8 safe parser，所以拆分标签和中文流式文本在本地 TUI 与 server projection 中保持同一套经过测试的行为。",
      "v0.2.5":
        "Server command/exec 的网络策略拒绝现在和 TUI bash 使用同一个 runtime permission evaluation 边界。可申请的 blocked host 仍然走原来的 permission request 与 retry 流程；配置为 denylist 的 host 会显示清晰的 policy-denial error，不再作为不可提示的缺失请求悄悄落下。",
      "v0.2.4":
        "TUI bash 的网络策略拒绝现在会明确显示。Runtime permission policy 会为 network block 返回结构化的 Request 或 Deny evaluation：可申请的 host 仍然走原来的审批流程，而配置为 denylist 的 host 会变成清晰的 denied tool result，不再只是一个缺失的 prompt。",
      "v0.2.3":
        "TUI MCP 工具调用现在能显示真实 stdio elicitation 请求，而不是静默丢掉。MCP server 在 tool call 中发送 elicitation/create 时，Orca 会把 URL 或表单请求投影到 runtime pending-interaction store，通过 runtime id 显示 TUI waiting-input prompt，把 accept 或 decline 写回 server，然后继续原来的工具调用。",
      "v0.2.2":
        "DeepSeek 工具调用兼容性得到加固。update_goal 与 update_plan 会在校验前把 DeepSeek 生成的 status 别名和布尔状态标志归一化，glob 与 update_goal 的 JSON Schema 支持 nullable/anyOf 参数，工具校验错误也会列出允许和必填的属性。System prompt 不再内联完整工具 Schema，改为展示简洁示例，同时该版本为 DeepSeek turn 新增了 reasoning-content 重放、工具数量上限与空响应重试。",
      "v0.2.1":
        "Server 交互响应现在归属到各自的 focused processor。permission/respond 会在 permission processor 里解析 pending permission grant，user_input/respond 会在 user-input processor 里解析 runtime user-input waiter，并且 ownership tests 会防止这些响应路径重新退回泛化的 server 模块。",
      "v0.2.0":
        "权限审批弹窗现在会直接说清楚请求风险。Network block、filesystem write grant 和 unsandboxed shell retry 的 runtime permission kind 会穿过 pending interaction 传到 TUI，所以弹窗可以显示 Network Permission Required、Filesystem Permission Required 或 Unsandboxed Shell Required，而不是泛泛的 Approval Required。",
      "v0.1.191":
        "RuntimeProviderResponseStep 现在直接消费 RuntimeProviderResponseInput，而不是把 response handoff 重新展开成长参数列表。Child-agent executors 会通过 RuntimeProviderResponseExecutors 传递，所以 provider final-message 与 tool-turn dispatch 继续共享一个命名 response 边界，行为保持不变。",
      "v0.1.190":
        "RuntimeProviderTurnStep 现在通过命名的 RuntimeProviderTurnInput 接收 provider-call 状态。Provider-turn I/O 引用统一放在 RuntimeProviderTurnIo 后面，所以 conversation、history、events、sink 和 cost tracking 会作为一个边界交给 provider execution，provider 行为保持不变。",
      "v0.1.189":
        "RuntimeProviderResponseInput 现在通过命名的 RuntimeProviderResponseIo bundle 承载 provider-response I/O 引用。RuntimeTurnKernel 会把 events、sink、conversation、history、cost tracking 和 background workflow handles 作为一次性 handoff 组装起来，provider response handling 只在执行边界解构这组 bundle。",
      "v0.1.188":
        "RuntimeProviderCycleInput 现在复用 RuntimeStepCapabilitySnapshot 来承载 provider-cycle capability 引用。runtime_turn_iteration 只组装一次这组 bundle，provider_turn 会把它交给 RuntimeStepContext，而不再展开成一串 flat capability 字段；provider/tool 行为不变，但 provider-cycle 边界更小。",
      "v0.1.187":
        "RuntimeStepSnapshot 现在通过命名的 RuntimeStepCapabilitySnapshot 承载 request-scoped capability 引用。Tool dispatch 会从这组 capability bundle 读取 instructions、memory、MCP registry、hooks、cancel state、task registry、workflow IPC 与交互 handler，保持 provider/tool 行为不变，同时继续收紧 Codex 风格的 step-context 边界。",
      "v0.1.186":
        "Runtime request_user_input 现在和 permission request 共用 RuntimeTurnInteractionState。ThreadTurnRequest 可以携带 runtime user-input handler，provider/tool cycle 会通过 RuntimeStepSnapshot 和 ToolExecutionContext 继续传递它，RuntimeToolRouter 也会把 request_user_input 当成 runtime special dispatch，而不是落回普通工具占位失败。",
      "v0.1.185":
        "Runtime turn 级交互处理器现在流经 RuntimeTurnInteractionState。AgentLoopContext、runtime_turn_loop 和 runtime_turn_iteration 会通过一个 grouped boundary 传递 permission-request handler，保持现有审批行为不变，同时为后续 request_user_input 和其他交互等待状态共用同一个 turn-owned surface 做准备。",
      "v0.1.184":
        "RuntimeStepContext 现在通过命名的 RuntimeStepSnapshot 承载 request-scoped runtime 输入。Provider response handling 会从 snapshot 读取 final-response 设置，tool dispatch 也会先把 step context 拆成 snapshot 与 extension binding，再路由 normal、readonly、workflow 和 subagent tool turn。",
      "v0.1.183":
        "Runtime directive 现在会流经命名的 RuntimeCapabilitySnapshot。模型切换、工具 allowlist 和 runtime system message patch 共享同一个 capability contract，RuntimeDirectiveState 也会暴露这份 snapshot，方便后续 skill、hook、MCP 和 tool policy 都从同一层状态派生。",
      "v0.1.182":
        "RuntimeTurnKernel 现在以实例方法拥有 turn-loop state 组装。RuntimeTurnState 会用 scoped extension stores 创建 kernel，再由这个实例构建 RuntimeTurnLoopState；共享 extension stores 让 kernel 的借用边界和交给 loop 的状态保持一致。",
      "v0.1.181":
        "RuntimeTurnKernel 现在会组装 lifecycle-owned RuntimeTurnLoopState。RuntimeTurnState 不再自行展开 loop runtime 和 extension-state 字段，延续 provider response handling 周边的 turn-state 收口。",
      "v0.1.180":
        "RuntimeTurnKernel 现在会自己组装 provider-response input 对象。Provider response handling 不再把 kernel-owned sampling state 或 step-context binding 作为独立字段暴露出来，在不改变行为的前提下继续收紧 turn-state handoff。",
      "v0.1.179":
        "RuntimeTurnKernel 现在会保留 reducer 使用的 extension stores，并通过同一个 kernel 绑定 provider-response RuntimeStepContext 的 extensions。Provider response handling 不再直接拼接 extension stores，在不改变行为的前提下继续收紧 Codex 风格 turn-state 边界。",
      "v0.1.178":
        "RuntimeTurnKernel 现在同时持有每次 sampling request 的状态和 runtime turn reducer。Provider response handling 会先通过这个 kernel 构造 tool-dispatch state，再交给 tool turn；用户可见行为不变，但下一步 Codex 风格 turn-state 收口有了明确边界。",
      "v0.1.177":
        "command/exec/list 快照现在包含底层 shellId、taskId、requestedTerminalMode 和 effectiveTerminalMode。重连后的 app-server 客户端可以用和 shell/list 一致的 task identity 恢复 active command/exec 的展示，以及 PTY 或 pipe 语义。",
      "v0.1.176":
        "Server mode 客户端现在可以调用 command/exec/list 来恢复 active command/exec process handle。列表快照包含 processId、command、cwd、running 状态、stream output 设置、output cap，以及 stdout/stderr 已发送字节计数；已完成的进程会在下一次 list 响应前被 drain 出列表。",
      "v0.1.175":
        "Server mode 的 command/exec/read 现在支持 outputBytesCap。read 请求会在常规 pre-dispatch drain 前收紧 active streaming process 的输出上限，让有界轮询返回 UTF-8 安全的 delta 和 capReached 元数据，而不是泄露更大的输出突发。",
      "v0.1.174":
        "Server mode 的 command/exec 现在支持 command/exec/read，客户端可以主动拉取长运行 streaming process handle 的输出。read 请求会确认活动进程，并复用现有 outputDelta 流、cap 元数据和完成事件路径。",
      "v0.1.173":
        "Server mode 的 shell/read 现在支持 outputBytesCap，用于给增量 shell 输出设置字节预算。stdout/stderr delta 以及 shell_updated / shell_completed 响应会按 UTF-8 边界截断，并在触达预算时带上 capReached 元数据。",
      "v0.1.172":
        "Server mode 现在可以在启动 shell 或 command/exec PTY 任务前查询 shell/capabilities。响应会报告当前平台、原生 PTY 与 PTY resize 是否可用、接受的 terminal mode、pipe fallback 行为，以及 streaming command/exec session 必须提供 processId 的约束。",
      "v0.1.171":
        "Bash sandbox recovery 现在能处理 macOS 上没有具体路径的 sandbox 拒绝，例如 GitHub HTTPS 凭据读取失败：runtime、JSONL command/exec 和 TUI 都会在用户同意后用无文件系统沙箱方式重跑命令。Shell task session 状态也迁移到 ORCA_HOME，并会从旧的项目 .orca/task-sessions 目录迁移历史记录。",
      "v0.1.170":
        "RuntimeSamplingRequestState 现在负责记录 normal tool result，并持有 approval-required 与 subagent-failure 的 terminal folding。Normal tool execution 会从同一个 request state 借 permission overlay 并通过它记录结果；tool_turn 只负责委托，用户可见行为不变。",
      "v0.1.169":
        "RuntimeSamplingRequestState 现在会为 readonly 与 subagent batch 生成 clamped RuntimeToolDispatchWindow。Tool turn 不再读取 raw cursor position，也不再直接切 batch slice；即便 batch collector 没有推进，也会至少消费当前 request，避免调度循环卡住。",
      "v0.1.168":
        "RuntimeSamplingRequestState 现在同时拥有 tool-dispatch cursor 和每次 sampling request 的 permission overlay。Tool turn 通过 sampling state 读取并推进当前 request，不再保留独立的 ToolRequestCursor；Codex 风格 request-scoped runtime state 有了更清晰的单一归属，同时不改变工具执行行为。",
      "v0.1.167":
        "RuntimeSamplingRequestState 现在拥有每次 sampling request 的 permission overlay。Provider response handling 会创建 sampling state，tool turn 借用其中的 overlay，不再分配局部 permission state；这为下一步 Codex 风格 request-state 拆分提供了真实承载点，同时不改变工具行为。",
      "v0.1.166":
        "Runtime turn-loop input construction 现在收进 focused run_agent_turn_loop 入口。agent_loop 只传 RuntimeAgentTurnLoopInput launch object，不再直接构造宽大的 RuntimeTurnLoopInput；内部 turn-loop handoff 由 runtime_turn_loop 统一持有。",
      "v0.1.165":
        "RuntimeTurnLoopState 现在拥有 directive-resolved loop policy surface。agent_loop 不再拆开 loop state，也不再直接读取 directive accessor；lifecycle 会在每次 turn-loop iteration 前解析 tool policy、runtime system messages、model override、cost/cancel/task refs，以及 grouped extension context。",
      "v0.1.164":
        "RuntimeTurnState 现在把 lifecycle-owned RuntimeTurnLoopState 交给 agent_loop，将 runtime directive 与可变 turn-loop runtime refs 分开。RuntimeTurnLoopInput 持有这个 grouped runtime handoff，并在 iteration 边界派生 RuntimeExtensionContext，因此 agent_loop 不再拆开 extension registry/thread/turn 字段，也不再从 raw parts 重建 extension context。",
      "v0.1.163":
        "RuntimeTurnState 现在暴露 grouped RuntimeExtensionContext boundary，并拥有 agent_loop 使用的 registry/store 组合 helper。Loop 仍把同一套 registry、thread store 和 turn store 传给 RuntimeTurnLoopInput，但 extension-context 构造不再直接散落在 loop body 中。",
      "v0.1.162":
        "Runtime turn-loop、iteration 与 provider-cycle input 现在携带单一 RuntimeExtensionContext，不再暴露平行的 extension registry、thread store 和 turn store 字段。agent_loop 只创建一次 grouped context；provider turn 在构造 RuntimeStepContext 时复用它，保持 lifecycle notification 行为不变，同时继续收窄 runtime extension boundary。",
      "v0.1.161":
        "RuntimeStepContext 与 RuntimeNormalToolTurnContext 现在携带 grouped RuntimeExtensionContext，不再暴露平行的 extension registry、thread store 和 turn store 字段。Provider turn 仍只构造一次相同 stores；normal tool execution 通过更窄的 extension boundary 接收同样的 lifecycle 数据。",
      "v0.1.160":
        "ToolExecutionContext 现在直接携带 grouped RuntimeExtensionStores，不再从平行的 thread/turn 引用重新组装。Tool lifecycle contributor、goal progress recording 和 router dispatch 保持原行为，同时 normal tool execution entrypoint 的 extension-store API 更小。",
      "v0.1.159":
        "Permission-sensitive tool context 现在传递分组后的 RuntimeExtensionStores，不再暴露平行的 thread/turn extension 引用。RuntimeTurnReducer 可以直接从这个 grouped store boundary 构造；request_permissions、bash 自动权限升级、router overlay transfer 和直接 runtime-tool actor 兼容路径保持原行为，同时 runtime state API 更小。",
      "v0.1.158":
        "Permission reduction 现在统一由 RuntimeTurnReducer 实例持有。旧的静态 permission reducer accessor 已移除；request_permissions、bash 自动权限升级、router overlay transfer 和直接 runtime-tool actor 调用仍通过 turn/thread extension stores 保持原有行为。",
      "v0.1.157":
        "Permission overlay 的请求与合并现在通过 RuntimeTurnReducer 归约。request_permissions、bash 自动权限升级和 router overlay transfer 保持原有行为，同时 permission state mutation 不再散落在直接调用点。",
      "v0.1.156":
        "Runtime directive 现在通过 RuntimeTurnReducer 归约。模型切换、allowed-tool 替换与 runtime system message 注入保持原有行为，同时 RuntimeTurnState 不再直接写 directive state。",
      "v0.1.155":
        "RuntimeTurnReducer 现在拥有 completed-tool 的 goal progress recording。TUI 已完成的 normal tool 会先进入这个 reducer，而不是直接写 goal progress，从而把 live thread extension guard 收束到一个 runtime-owned state boundary。",
      "v0.1.154":
        "Goal terminal update 现在会读取 live runtime thread extension state。TUI turn 会把已完成的 normal tool 写入同一个 goal progress store；在 live thread 尚未观察到真实 non-goal tool progress 前，update_goal 会拒绝 complete 或 blocked 声明。",
      "v0.1.153":
        "RuntimeThread 现在拥有 thread-scoped extension store，并会把它传给每个 RuntimeTurnState；每个 RuntimeThread turn 仍然获得新的 turn extension store id。Goal 和后续 runtime contributor 现在可以跨 headless、TUI、server turn 保留稳定的 thread-level 状态，同时不泄漏 turn-local 数据。",
      "v0.1.152":
        "RuntimeTurnState 现在拥有默认 extension registry 以及 thread/turn extension store，并会安装 goal tool lifecycle contributor，再沿 provider/tool turn path 传入 normal tool execution。Goal progress 现在可以从真实 runtime 路径观察 normal-tool completion，同时不改变 CLI、TUI、server、JSONL 或 goal storage 行为。",
      "v0.1.151":
        "ToolExecutionContext 现在可以携带 extension registry 以及 thread/turn extension store，ToolExecutionActor 会在 normal tool execution 前后通知 lifecycle contributor。Completed、blocked、aborted 与 not-implemented outcome 已经能进入 extension boundary，同时不改变 CLI、TUI、server 或 JSONL wire 行为。",
      "v0.1.150":
        "Orca 新增 runtime extension contributor kernel，提供 typed per-scope ExtensionData 和有序 tool lifecycle contributor。Goal tool progress 现在有了第一个真实 contributor seed，后续 goal、memory、task 与 tool lifecycle 可以逐步移出主 runtime loop，同时不改变 CLI 或 app-server 行为。",
      "v0.1.149":
        "Realtime server item projection 现在由共享的 RuntimeEventProjector reducer 承载。Assistant message、proposed plan、reasoning、command execution、MCP/dynamic tool、file change 与 workflow lifecycle event 继续保持同一 app-server wire shape，同时 ServerRequestWriter 不再持有 projection state map。",
      "v0.1.148":
        "Async subagent worker entrypoint 现在改用 AsyncSubagentWorkerInput、AsyncSubagentWorkerContext、AsyncSubagentLaunchContext 和 spawn context 分组传递输入，不再暴露长参数列表，也不再用 clippy allow 压住设计问题。CLI worker 启动、父级 async launch、task registry 状态、worktree handoff、async completion payload、subagent contract 与 TUI async subagent 行为保持不变。",
      "v0.1.147":
        "Child-agent runtime constructor 输入现在通过分组的 ChildAgentRuntimeContext 传递，不再暴露 cwd/events/sink/instructions/memory/MCP/hooks/cancel/lifecycle/executor 长参数列表。既有 orca_runtime::agent_child re-export 仍保持可用，child-agent loop setup、provider turn、compaction、response folding、tool execution、同步和异步 subagent contract、workflow child agent 与 TUI child-agent delegation 行为保持不变。",
      "v0.1.146":
        "Child-agent loop runner 输入现在通过分组的 ChildAgentLoopContext 传递，不再暴露 request/cwd/instructions/memory/hooks/cost-tracker 长参数列表。既有 orca_runtime::agent_child re-export 仍保持可用，loop setup、provider turn、compaction、response folding、tool execution、subagent contract、workflow child agent 与 TUI child-agent delegation 行为保持不变。",
      "v0.1.145":
        "Child-agent prompt entrypoint 输入现在通过分组的 ChildAgentPromptContext 传递，不再暴露长参数列表。既有 orca_runtime::agent_child re-export 仍保持可用，prompt-to-request 构造、model override、child cost tracking、subagent contract、workflow child agent 与 TUI child-agent delegation 行为保持不变。",
      "v0.1.144":
        "Child-agent public entrypoints 现在移到独立的 child_agent_entrypoints 模块，不再由 agent_child facade 承载。既有 orca_runtime::agent_child 导入仍通过 re-export 保持可用，model override、child cost tracking、prompt-to-request 构造、subagent contract、workflow child agent 与 TUI child-agent delegation 行为保持不变。",
      "v0.1.143":
        "Child-agent behavior tests 现在移到独立的 child_agent_tests 模块，不再由 agent_child facade 承载。既有 orca_runtime::agent_child 导入仍通过 re-export 保持可用，child-agent setup、provider turn、response folding、loop running、subagent contract 与 TUI child-agent delegation 行为保持不变。",
      "v0.1.142":
        "Child-agent request、result、runtime 与 executor 类型现在移到独立的 child_agent_types 模块。既有 orca_runtime::agent_child 导入仍通过 re-export 保持可用，loop setup、provider turn、response folding、loop running、subagent contract 与 TUI child-agent delegation 行为保持不变。",
      "v0.1.141":
        "Child-agent loop orchestration 现在移到独立的 child_agent_loop_runner 模块。既有 orca_runtime::agent_child 导入仍通过 re-export 保持可用，loop setup、provider turn、response folding、tool-result folding、subagent contract 与 TUI child-agent delegation 行为保持不变。",
      "v0.1.140":
        "Child-agent provider response folding、tool request extraction、tool execution context 与 tool-result folding 现在移到独立的 child_agent_response_folding 模块。既有 orca_runtime::agent_child 导入仍通过 re-export 保持可用，child-agent loop orchestration、provider turn、subagent contract 与 TUI child-agent delegation 行为保持不变。",
      "v0.1.139":
        "Child-agent model routing、provider hook execution、provider call dispatch、child compaction 与 prompt-too-long retry/failure handling 现在移到独立的 child_agent_provider_turn 模块。既有 orca_runtime::agent_child 导入仍通过 re-export 保持可用，provider response folding、tool request extraction、tool result folding、subagent contract 与 TUI child-agent delegation 行为保持不变。",
      "v0.1.138":
        "Child-agent loop setup、provider bootstrap、conversation seed、approval policy seed 与 child turn-budget state 现在移到独立的 child_agent_loop_setup 模块。既有 orca_runtime::agent_child 导入仍通过 re-export 保持可用，child-agent model routing、provider turn、tool folding、subagent contract 与 TUI delegation 行为保持不变。",
      "v0.1.137":
        "Runtime approval decision、approval handler 和基于配置的 interactive approval 处理现在移到独立的 runtime_approval 模块。既有 orca_runtime::lifecycle 导入仍通过 re-export 保持可用，approval policy resolution 与 interactive approval 行为保持不变。",
      "v0.1.136":
        "Runtime permission request/response/handler 与 turn permission overlay 现在移到独立的 runtime_permission 模块。既有 orca_runtime::lifecycle 导入仍通过 re-export 保持可用，request_permissions 执行和 turn permission overlay 行为保持同一运行时形状。",
      "v0.1.135":
        "Runtime request_user_input 处理现在移到独立的 runtime_user_input 模块。既有 orca_runtime::lifecycle 导入仍通过 re-export 保持可用，请求解析、handler 派发、回答完成和取消失败行为都保持同一 runtime 与 TUI 形状。",
      "v0.1.134":
        "Runtime 里依赖 ORCA_HOME 的测试现在统一使用可恢复 poisoned mutex 的 test env lock helper。如果某个测试在持有共享环境锁时 panic，后续 history、server、session、thread-store 与 workflow-host 测试会恢复锁，而不是级联报出误导性的 PoisonError。",
      "v0.1.133":
        "Sandbox bash command 构造现在改用 WorkspaceWriteSandboxCommandContext 与 ReadOnlySandboxCommandContext 聚合输入，macOS Seatbelt profile helper 也改为接收聚焦的 profile context。Shell session、command/exec、bash sandbox、网络与文件系统策略开关、Unix socket allowlist、非交互式进程准备都保持同一行为，同时 sandbox API 不再暴露长参数列表。",
      "v0.1.132":
        "运行时 bash 的 sandbox 执行和一次性 shell 启动现在改用 RuntimeBashSandboxContext 与 RuntimeBashOnceContext 聚合输入。模型可见 bash 仍保持 permission-profile 沙箱、网络和文件系统权限重试、取消、task registry 交接、输出截断和诊断行为不变，同时 runtime_bash.rs 不再需要内部长参数 helper 的逃逸口。",
      "v0.1.131":
        "Readonly tool-turn batch 执行现在移到独立的 runtime_readonly_tool_turn 模块，并使用分组后的 readonly batch / tool-turn context。主 tool_turn dispatcher 继续负责 request cursor、child-tool policy 检查、subagent batch、readonly batch 选择和 normal tool turn，readonly hook gate、并行执行与结果记录保持同一运行时行为。",
      "v0.1.130":
        "Async subagent worker 的启动和完成写回现在移到独立的 subagent_async_worker 模块。父级 subagent executor 继续负责同步和 batch 执行，worker spawn、task registry usage 写入、worktree handoff 和 async result payload 保持同一 CLI 与 subagent_status 行为。",
      "v0.1.129":
        "Server shell-session 状态现在移到独立的 shell_manager 模块。可选 runtime shell session 存储、带 task registry 的 lazy 初始化、shell CRUD/read/kill/reap/wait 调用，以及 command/exec drain 兼容借用都保持同一 server shell protocol 形状。",
      "v0.1.128":
        "Server pending-permission request 状态现在移到独立的 permission_manager 模块。Runtime request_permissions 等待、command/exec permission retry record、pending request removal，以及 permission request handler registration 都保持同一 server permission protocol 形状。",
      "v0.1.127":
        "Server active-turn lifecycle 状态现在移到独立的 active_turn_manager 模块。Active turn control、running turn handle、finished-turn reclaim、按 thread 等待 reclaim，以及 session permission metadata merge 都保持同一 server turn protocol 形状。",
      "v0.1.126":
        "Server command/exec 的 active process 状态现在移到独立的 command_exec_manager 模块。Buffered output、streaming delta、stdin write、PTY resize、terminate、output cap、sandbox 诊断和 permission retry 行为保持同一 server protocol 形状。",
      "v0.1.125":
        "RuntimeToolActorContext 现在移到独立的 runtime_tool_actor 模块。既有 orca_runtime::lifecycle 导入仍通过 re-export 保持可用，审批、hook、request-user-input、normal-tool、permission overlay 和 active-task 行为保持同一运行时形状。",
      "v0.1.124":
        "Runtime lifecycle 状态机类型现在移到独立的 runtime_lifecycle 模块。既有 orca_runtime::lifecycle 导入仍通过 re-export 保持可用，task/turn id、状态映射、事件 payload 和 RuntimeTurnRunner 行为保持不变。",
      "v0.1.123":
        "Runtime turn setup 现在从 lifecycle.rs 移到独立的 runtime_turn_setup 模块。Agent loop 仍通过 RuntimeTurnSetupStep 委托执行，新模块负责 context budget setup、工具审批 policy 构造和 provider config 组合，lifecycle.rs 则继续保留 actor/lifecycle 原语。",
      "v0.1.122":
        "Runtime conversation bootstrap 现在从 lifecycle.rs 移到独立的 runtime_conversation_bootstrap 模块。Agent loop 仍通过 RuntimeConversationBootstrapStep 委托执行，新模块负责 RuntimePreparedConversation、borrowed/owned conversation 存储、session bootstrap 组合和初始 history 记录，lifecycle.rs 则继续保留 actor/lifecycle 原语。",
      "v0.1.121":
        "Runtime steer application 现在从 lifecycle.rs 移到独立的 runtime_steer 模块，并通过分组后的 RuntimeSteerInput 传参。RuntimeTurnOpeningStep 和 RuntimeProviderTurnStep 仍会在模型调用前把待处理 steer input 注入 conversation 和 history，lifecycle.rs 保留 ThreadSteerHandle 存储，同时再拆掉一个 reducer 切片。",
      "v0.1.120":
        "Runtime model-route 编排现在从 lifecycle.rs 移到独立的 runtime_model_route 模块，并通过分组后的 RuntimeModelRouteInput 传参。RuntimeTurnOpeningStep 仍按原顺序组合 compaction、turn start、model routing 和 steering，lifecycle.rs 保留 actor/lifecycle 原语，同时再拆掉一个 reducer 切片，并避免新增长参数调用面。",
      "v0.1.119":
        "Runtime turn-start 编排现在从 lifecycle.rs 移到独立的 runtime_turn_start 模块。RuntimeTurnOpeningStep 仍按原顺序组合 compaction、turn start、model routing 和 steering，lifecycle.rs 则保留 actor/lifecycle 原语，同时再拆掉一个更底层的 reducer 切片。",
      "v0.1.118":
        "Runtime turn-opening 编排现在从 lifecycle.rs 移到独立的 runtime_turn_opening 模块，并通过分组后的 RuntimeTurnOpeningInput 传参。RuntimeTurnIterationStep 仍按原顺序组合 opening 与 provider-cycle 执行，lifecycle.rs 则继续保留更底层的 start/model-route/steer 步骤，同时再少一层 reducer 大小的职责。",
      "v0.1.117":
        "Runtime turn-iteration 编排现在从 lifecycle.rs 移到独立的 runtime_turn_iteration 模块。外层 runtime_turn_loop 仍通过 RuntimeTurnIterationStep 委托执行，provider-cycle 行为仍归 provider_turn，lifecycle.rs 继续保留 opening/start/model-route 这些步骤，同时为下一轮 reducer 风格拆分继续变小。",
      "v0.1.116":
        "Runtime turn-loop 编排现在从 lifecycle.rs 移到独立的 runtime_turn_loop 模块。agent loop 仍然通过 RuntimeTurnLoopStep 委托执行，分组后的 input/executor 对象和 iteration 重试/返回行为保持不变，同时 lifecycle.rs 进一步变小，为后续参考 Codex/package 3 的 reducer 拆分铺路。",
      "v0.1.115":
        "shell-session bash 执行现在统一接收分组后的 RuntimeBashInvocationContext，不再暴露 execute_bash_with_shell_session 的长参数列表。RuntimeNormalToolExecutor 仍然拥有 bash 分支，permission overlay、取消、输出截断、task registry 交接以及网络/文件系统权限重试行为保持不变，同时 bash 边界为后续 shell/session 和 async-subagent 切片继续收窄。",
      "v0.1.114":
        "文件系统 sandbox 拒绝现在在 server command/exec 和模型可见 bash 两条路径上都能更清楚地恢复。Orca 会诊断 macOS Seatbelt 写入阻断，例如嵌套工作区里的 .git/index.lock 失败，并说明这通常是 sandbox 范围问题而不是 stale lock；当存在审批处理器时，会请求 turn-scoped 文件系统写入授权，并用授权后的 root 重试原命令。",
      "v0.1.113":
        "工具回合 dispatch 现在从 provider response 处理处接收分组后的 RuntimeToolTurnsContext，不再暴露 run_tool_turns 的长参数调用。RuntimeStepContext、events、sink、conversation、history writer、tool requests、cost tracking、background workflow 状态和 child executors 的透传保持不变，同时 provider 到 tool-turn 的边界继续收窄。",
      "v0.1.112":
        "普通工具回合执行现在统一接收分组后的 RuntimeNormalToolTurnContext，不再让 run_normal_tool_turn 暴露长参数列表。工具执行、审批、结果记录、plan-state 记录、permission overlay、workflow/background 状态以及 child executor 交接都保持相同 runtime 行为，同时 tool-turn 边界继续收窄。",
      "v0.1.111":
        "工具审批 gate 的输入现在统一通过 ToolApprovalGateContext 传递，不再让 handle_approval 暴露长参数列表。config、events、sink、tool request、invocation、policy、strict auto-review 与 delta emission 的透传保持不变，同时 approval allow/ask/deny 行为和 tool-call item emission 继续保持相同的公开形状。",
      "v0.1.110":
        "历史投影工具完成态现在统一由 tool_item_projection.rs 里的 complete_projected_tool_item 重建，不再让 thread_store/projection.rs 直接调用 MCP、dynamic、commandExecution 和 fileChange completed-item 构造器。实时流和持久化 history 行为保持兼容，同时剩余 tool-item schema drift 又少了一个分散所有权点。",
      "v0.1.109":
        "runtime 普通工具路由现在从 router 向 lifecycle actor 传递分组后的 RuntimeNormalToolInvocation，不再直接调用带 roots/cancel 的长参数方法。bash shell-session、MCP/external fallback、permission overlay、取消和输出截断行为保持不变，同时公共工具路径为后续 shell 与 async-subagent 工作留下更窄的调用面。",
      "v0.1.108":
        "普通工具 invocation 现在统一经过 runtime_normal_tool 里的单一 helper，不再让 lifecycle.rs 直接实例化 executor。RuntimeTaskActor 和 RuntimeToolActorContext 仍保持同样的 bash、MCP、external、取消与 permission-overlay 行为，但后续 shell-session 和 async subagent 切片会有更窄的调用面可继续推进。",
      "v0.1.107":
        "工具调用参数的流式接收现在支持端到端进度上报：新增 tool.call.progress 事件与 ToolCallProgress provider 步骤，贯穿 runtime 与 server，TUI 以缓存友好的方式展示已接收字节进度。同时新增 SSE 流式空闲超时保护，并修复环境变量代理配置与 hook 超时输出处理的问题。",
      "v0.1.106":
        "普通工具 fallback 路径现在通过独立 RuntimeNormalToolFallbackExecutor 边界注入。MCP、TOML external 和 built-in 工具仍然走默认 orca-tools 实现，但 runtime 已经可以直接测试 fallback context 的透传，不再把具体实现硬编码在执行器里。",
      "v0.1.105":
        "普通工具执行现在进入独立 RuntimeNormalToolExecutor 边界。shell-session bash 分支，以及 MCP/external/built-in fallback 路径都从 lifecycle.rs 移出，同时 CLI、TUI、server、workflow、permission 与模型可见工具行为保持不变。",
      "v0.1.104":
        "runtime tool invocation dispatch 现在进入独立 RuntimeToolRouter 边界。ToolExecutionActor 只保留 invocation 准备、审批、hook 与结果收尾；workflow、subagent、task、permission、workflow IPC 和普通工具路由都移到 router，模型可见行为保持不变。",
      "v0.1.103":
        "runtime turn 执行现在使用更清晰的分组输入边界：turn iteration、provider cycle、provider response 与 tool turns 共享 request-scoped context。这个参考 Codex/package 3 的架构切片减少了重复的 runtime 状态传递，同时保持 CLI、TUI、server、tool、workflow 与 history 行为不变。",
      "v0.1.102":
        "TUI child-agent 执行现在通过 runtime 统一负责 request 构造、model/cost setup、loop 编排、provider 处理、tool request 提取与 tool-result folding；TUI 只保留交互式 tool adapter，并且新的 reasoning-effort 配置会继续传入 child provider 调用。",
      "v0.1.101":
        "推理强度现在可配置（high 或 max，默认 max），支持通过环境变量、配置文件和 CLI 参数设置，并在 DeepSeek API 请求中携带。TUI 的 /model 命令改为两步选择——先选模型，再选推理强度——选择过程中不立即应用，按 Esc 可完整取消，状态栏同时显示模型与推理强度。",
      "v0.1.100":
        "TUI 体验优化：inline 滚动现在通过 rendered-line-info 判断真实溢出，内容未溢出时保持自动跟随，修复 CJK 混排的换行高度计算，将内存提取移出渲染线程，新增实时活动指示栏，并在回合结束后对惯性鼠标滚动做防抖处理。",
      "v0.1.99":
        "runtime-special 工具分发和小型 executor 现在进入独立 runtime_special 模块，保持 request_permissions、workflow IPC、subagent status、task list/stop、workflow draft preview 行为不变，同时缩小 lifecycle.rs。",
      "v0.1.98":
        "server 的 submit-family dispatch 现在进入独立 submit processor，保持 legacy submit、thread-bound turn、thread/start、thread/resume、thread/fork 行为不变，同时让通用 router 只保留 operation-family 分发职责。",
      "v0.1.97":
        "server 的 permission/respond dispatch 现在进入独立 permission processor，保持 turn/session 授权、strict auto-review、文件系统 overlay 与网络 allow/deny 行为不变，同时继续缩小通用 router。",
      "v0.1.96":
        "server 的 command/exec dispatch 现在进入独立 command-exec processor，保持 buffered、streaming、stdin、resize、terminate、sandbox 与 permission-profile 行为不变，同时继续缩小通用 router。",
      "v0.1.95":
        "server 的 shell-session dispatch 现在进入独立 shell processor，保持 start、write、update、close、resize、list、read、kill 行为不变，同时继续缩小通用 router。",
      "v0.1.94":
        "server 的 turn-control dispatch 现在进入独立 turn processor，保持 interrupt、resume、steer 行为不变，同时继续缩小通用 router。",
      "v0.1.93":
        "server 里的同步 thread 查询和 metadata 操作现在进入独立 thread processor，缩小通用 router，同时保持 thread/read、list、search、turns、items 和 metadata 行为不变。",
      "v0.1.92":
        "server 模式的 operation dispatch 现在进入独立 router 边界，在保持所有现有 wire method 不变的同时，为后续 request processor 重构铺路。",
      "v0.1.91":
        "runtime 权限请求现在统一走同一个 overlay 合并路径，覆盖文件系统授权、网络域名授权和 strict auto-review，让 request_permissions 与 bash 重试行为保持一致。",
      "v0.1.90":
        "模型可见 bash 现在会继承 active permission profile 的托管网络策略：符合条件的代理阻断会转成权限请求，并在 turn 级网络 allow 后重试。",
      "v0.1.89":
        "streaming command/exec 进程现在也接入托管网络权限流：符合条件的代理阻断会请求 session 级 allow，授权后用同一个 processId 重启并继续流式输出。",
      "v0.1.88":
        "command/exec 现在可以把托管网络代理阻断转成网络权限请求，并在收到 session 级 allow 后重试命令；denylist 阻断仍保持为最终诊断。",
      "v0.1.87":
        "command/exec 托管网络代理的阻断诊断现在会包含规范化后的被拦截 host，为后续自动网络权限提示提供稳定归因点。",
      "v0.1.86":
        "session 级 request_permissions 网络拒绝现在会覆盖 permission profile 的 allow 条目，让交互式 deny 决策能收紧后续 command/exec 的代理策略。",
      "v0.1.85":
        "session 级 request_permissions 网络域名授权现在会持久化到 server thread，并传入 command/exec 的托管代理，让后续命令继承交互式 allowlist 决策。",
      "v0.1.84":
        "permission profile 中的 Unix socket allowlist 现在会传入 macOS command/exec 沙箱，允许显式配置的 AF_UNIX socket 路径，同时不需要开启完整网络访问。",
      "v0.1.83":
        "command/exec 的托管网络代理现在会在连接前检查 DNS 解析后的 socket 地址，阻止解析到本地、私网、保留地址或其他非公网目标的域名。",
      "v0.1.82":
        "command/exec 的托管网络代理现在默认阻止本地和私网 IP 目标，除非显式 allowlist；这对齐 Codex 的 local-network guard，同时保留已 allowlist 的 loopback 工作流。",
      "v0.1.81":
        "权限 profile 的网络拦截现在保留 Codex 风格的 proxy reason，command/exec 客户端可以区分 denylist 命中和 allowlist 未命中，而不是只看到泛化的 policy 403。",
      "v0.1.80":
        "TUI conversation session 现在直接拥有 RuntimeThread，不再本地重建 InteractiveSession 和 RuntimeSessionLifecycle，完成 headless/server/TUI 第一轮 runtime state ownership 收敛，同时保持 TUI 行为不变。",
      "v0.1.79":
        "Headless exec 现在也通过 RuntimeThread 创建并运行长期 agent state，让 CLI turn 与 server-mode 共享同一所有权边界，同时保留 JSONL 顺序、session hook、history、verifier 和 npm 行为。",
      "v0.1.78":
        "Server-mode thread 现在通过 RuntimeThread 保存长期 agent state，不再重复拼 session/lifecycle/executor，同时保持 thread projection、resume/fork、cancel 和权限行为不变。",
      "v0.1.77":
        "RuntimeThread 现在把 runtime-owned interactive session 和 lifecycle state 收到同一个边界里，为 server、TUI、headless 后续收敛提供新的承载点，同时不改变公开行为。",
      "v0.1.76":
        "Runtime protocol 边界现在变成小 facade，并由 command_exec、events、permissions、shell、thread、turn、wire 等专门模块支撑；公开 protocol API 保持不变，同时为下一步拆 server dispatch 铺路。",
      "v0.1.75":
        "ThreadStore 现在拆成清晰的存储 facade：types、local JSONL、writer、projection、pagination 和 live-thread 各自成模块，在保持公开 runtime API 不变的同时拆掉原来的巨型 store 文件。",
      "v0.1.74":
        "权限 profile 的 network domain policy 现在会通过 command/exec 的本地 HTTP 代理执行：允许的 host 可访问，被 deny 的 host 返回 policy 403。",
      "v0.1.73":
        "权限 profile 的文件系统 glob 现在支持通过 glob_scan_max_depth / globScanMaxDepth 配置扫描深度，并支持父 profile 默认值与子 profile 覆盖。",
      "v0.1.72":
        "权限 profile 现在会把有界 read/write/read-write 文件系统 glob 展开成具体 command sandbox roots，在保留过宽 glob 安全拒绝的同时补齐 Codex 风格 split filesystem policy。",
      "v0.1.71":
        "Runtime compaction 现在迁到专门模块，prompt budget hooks、summary 持久化和 prompt-too-long 恢复不再混在 lifecycle 编排里。",
      "v0.1.70":
        "TUI 历史拆成两层：已定稿 transcript 输出进入终端原生 scrollback，底部 live viewport 保留流式内容、计划、输入框、状态栏和模态/全屏面板。",
      "v0.1.69":
        "Tool-turn 执行现在迁到专门的 runtime 模块，provider 工具 schema / invocation 准备与游标、批处理、执行、结果折叠边界分开。",
      "v0.1.68":
        "TUI tool approval gate 现在由 runtime interaction adapter 负责，`bridge` 不再直接持有 approval request 构造、preview 生成和交互等待逻辑。",
      "v0.1.67":
        "TUI runtime approval 和 request_user_input handler 现在迁到专门的 interaction adapter 模块；站点构建也补齐了用于生成爬虫可见 HTML 的 server prerender entry。",
      "v0.1.66":
        "TUI runtime event projection 现在迁到专门模块，`bridge` 不再直接持有 EventEnvelope 到 TuiEvent 的映射和 workflow notification prompt 组装。",
      "v0.1.65":
        "持久化 edit / write_file history item 现在投影为 Codex 风格 fileChange item，让 thread-read 历史与实时 server stream 保持一致。",
      "v0.1.64":
        "持久化 commandExecution history item 现在也由共享 projection builder 构造，同时保留命令元数据占位字段和失败命令诊断语义。",
      "v0.1.63":
        "实时 commandExecution lifecycle item 现在也由共享 projection builder 构造，继续消除 app-server item shape 漂移点。",
      "v0.1.62":
        "实时 agent / plan / reasoning lifecycle item 现在也由共享 projection builder 构造，继续收紧 app-server protocol 边界。",
      "v0.1.61":
        "实时 fileChange / workflow lifecycle item 现在由共享 projection builder 构造，tag 发布关口也会在 CI 串行运行 server-heavy Rust 测试。",
      "v0.1.59":
        "MCP / dynamic completed-item projection 已在实时 stream 与 history 间共享；CI stdio MCP fixture 改为通过 /bin/sh 启动，避开 Linux ETXTBSY 发布抖动。",
      "v0.1.58":
        "MCP / dynamic tool completed-item 构造现在由实时 stream 与持久化 history 共享 projection builder，并补上失败 command projection 的输出形状回归守卫。",
      "v0.1.57":
        "实时 stream 与持久化 history 现在共享 MCP / dynamic tool started-item builder，让一等工具调用 item 从创建阶段就保持形状一致。",
      "v0.1.56":
        "实时与持久化 tool item projection 现在共享 exit-code 错误归一化和 completed 状态检查，继续减少 mcpToolCall / dynamicToolCall 的 schema drift。",
      "v0.1.55":
        "实时 server stream 与持久化 thread projection 现在共享 MCP tool 解析、JSON 参数解析、MCP result shaping 和 camelCase tool error helper，并加固后台 turn 活跃写入时的 CI JSONL 轮询测试。",
      "v0.1.53":
        "实时 mcpToolCall / dynamicToolCall item error 现在会在工具完成事件提供 exit_code 时携带 exitCode，与持久化 thread item 投影保持一致。",
      "v0.1.52":
        "MCP initialize capabilities 现在会按 server 缓存；all-server resource/template 发现会跳过 tools-only server，显式 server 查询仍返回该 server 的真实错误。",
      "v0.1.51":
        "MCP resource / template 发现现在会在 all-server 结果里带上 registry 级启动错误，让失败的 MCP server 和健康资源上下文一起可见。",
      "v0.1.50":
        "MCP resource templates 现在通过 list_mcp_resource_templates 暴露给模型，stdio/SSE 已接入 resources/templates/list，并支持按 server 聚合部分失败错误。",
      "v0.1.49":
        "MCP resource 发现现在会保留可用 server 的资源，并把失败 server 的错误聚合到 list_mcp_resources 结果里，不再因为单点失败丢掉全部上下文。",
      "v0.1.48":
        "MCP resource 工具随更稳的 server-mode JSONL 测试 harness 一起发布，子进程噪声不再让 task_stop shell-session 覆盖在 CI 中偶发失败。",
      "v0.1.47":
        "MCP resources 现在通过只读的 list_mcp_resources / read_mcp_resource 暴露给模型，stdio/SSE 的 resources/list 与 resources/read 也接入了统一工具注册表。",
      "v0.1.46":
        "结构化 hook JSON stdout 现在会校验声明的 action 与必需字符串字段，拼错或格式错误的 hook 输出会显式失败，不再被静默注入或忽略。",
      "v0.1.45":
        "工具参数执行前校验现在支持 JSON Schema 的 oneOf / anyOf 分支，runtime 拒绝行为与暴露给模型的 provider schema 更一致。",
      "v0.1.44":
        "模型侧文件发现补齐 fuzzy path query：`glob` 可通过 mode=fuzzy 按路径片段/首字母查找，同时保留原有 glob pattern 行为和 list_files 兼容入口。",
      "v0.1.43":
        "Runtime turn 编排继续内聚到 lifecycle 边界：turn opening、provider cycle、iteration、loop 与 loop input 都由 runtime 持有，agent loop 入口更薄且行为保持兼容。",
      "v0.1.42":
        "补齐 Claude Code 风格 workflow 闭环：生成草稿、编辑/保存/运行控制、可复用 workflow 命令、证据绑定报告，以及进程树级超时清理。",
      "v0.1.41":
        "重写工作流并发控制（Promise.allSettled + 首错快速失败）、结构化失败分类（工具/MCP/令牌/Schema）、证据包并发指标及压力测试覆盖。",
      "v0.1.40":
        "新增工作流证据包（Evidence Bundle）与标准化报告生成（Markdown + JSON），生命周期各节点自动写入证据，配套合约校验测试。",
      "v0.1.39":
        "工作流子任务列表工具、subagent 强类型输出 schema、团队工具白名单、持久化 IPC 状态及 agent 生命周期可观测性。",
      "v0.1.38":
        "历史 / 会话持久化现在经过专门的 SessionStore 边界，runtime 的 session/controller 调用点也统一到了同一入口。",
      "v0.1.37":
        "Shell 执行现在会遵守可配置超时，bash 和外部工具共享统一的超时等待子进程逻辑。",
      "v0.1.36":
        "工作流 agent 运行现在支持 worktree 隔离、异步句柄恢复，以及在 TUI 工作流视图中继续执行失败后续 phase。",
      "v0.1.35":
        "TUI 输入框支持括号粘贴（Bracketed Paste）；重写文本区域软换行渲染，修复高度计算不准确问题。",
      "v0.1.34":
        "新增可重复执行的真实 API 发布闸门，发版前统一验证 provider summary 成本、CLI JSONL 输出和 server-mode 流式事件。",
      "v0.1.33":
        "统一 runtime 工具调用记录、审批请求构造与 hook 修改后的请求校验，覆盖内置工具、MCP 工具和外部工具。",
      "v0.1.32":
        "新增 runtime 侧强类型 protocol 边界，server submission 与 event 映射不再散落在松散 JSON 中，同时保持现有扁平 JSON wire 格式兼容。",
      "v0.1.31":
        "交互会话状态改由 runtime 统一持有，集中管理 conversation、历史、instructions、memory、hooks、MCP、成本统计和 workflow task 状态，为 protocol 拆分打基础。",
      "v0.1.30":
        "重构 workflow DSL 与多阶段运行时；TUI 现在展示 workflow/task 进度、运行时长、通知和更清晰的审批选项。",
      "v0.1.29":
        "重构 TUI 会话预加载逻辑，提取 goal session ID 辅助函数，新增会话恢复与目标控制流的单元测试。",
      "v0.1.28":
        "移除旧版 deepseek-chat / deepseek-reasoner；工具参数在调用前按 JSON Schema 校验；重写 TUI 文本换行，支持宽字符与 ANSI 段。",
      "v0.1.27":
        "终结缓存压缩风暴：按真实 wire 提示词触发 + 60% 压缩滞后，--continue 与 --fork 现在会持久化继承的 summary 状态。",
      "v0.1.26":
        "版本更新检查优先走 npm registry（无限流），表格渲染重写为渐进降级，窄终端也能读。",
    },
    foot: {
      releases: "GitHub Releases",
      qq: "QQ 群 472309526",
      telegram: "Telegram",
    },
  },
} as const;

function Changelog() {
  const [locale, setLocale] = useState<Locale>(detectInitialLocale);
  const t = copy[locale];

  useEffect(() => {
    window.localStorage.setItem(localeStorageKey, locale);
    applySeoHead(locale, seoCopy[locale], canonicalUrl);
  }, [locale]);

  return (
    <main>
      <header className="nav">
        <a className="brand" href={links.home} aria-label={t.aria.home}>
          <img className="brand-mark" src="/orca-icon.svg" alt="" aria-hidden="true" />
          <span>Orca</span>
        </a>
        <div className="nav-actions">
          <nav aria-label="Main navigation">
            <a href={links.home}>{t.nav.home}</a>
            <a href={`${links.home}#install`}>{t.nav.install}</a>
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

      <section className="changelog-hero">
        <span className="pill">
          <span className="dot" />
          {releaseVersion} · {t.header.latestLabel}
        </span>
        <p className="eyebrow">{t.header.eyebrow}</p>
        <h1>{t.header.title}</h1>
        <p className="subtitle">{t.header.subtitle}</p>
      </section>

      <section className="search-paths" aria-labelledby="changelog-guides-heading">
        <div className="section-heading">
          <p className="eyebrow">{t.related.eyebrow}</p>
          <h2 id="changelog-guides-heading">{t.related.title}</h2>
        </div>
        <div className="search-path-grid">
          {t.related.links.map((link) => (
            <a href={link.href} key={link.href}>
              <h3>{link.title}</h3>
              <p>{link.body}</p>
              <span>{link.href}</span>
            </a>
          ))}
        </div>
      </section>

      <section className="changelog-page" aria-labelledby="changelog-heading">
        <h2 id="changelog-heading" className="visually-hidden">
          {t.header.eyebrow}
        </h2>
        <ol className="changelog-list">
          {releases.map((release, idx) => (
            <li key={release.version} className="changelog-item">
              <a
                href={release.url}
                rel="noreferrer"
                aria-label={`${release.version} ${t.header.readNotes}`}
              >
                <div className="changelog-meta">
                  <span className="changelog-version">{release.version}</span>
                  {idx === 0 ? (
                    <span className="changelog-latest">{t.header.latestLabel}</span>
                  ) : null}
                  <time className="changelog-date" dateTime={release.date}>
                    {release.date}
                  </time>
                </div>
                <p className="changelog-summary">{t.summaries[release.version]}</p>
                <span className="changelog-link">{t.header.readNotes}</span>
              </a>
            </li>
          ))}
        </ol>
      </section>

      <footer>
        <a className="foot-brand" href={links.home}>
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
            {t.foot.releases}
          </a>
          <span>{t.foot.qq}</span>
          <a href={links.telegram} rel="noreferrer">
            {t.foot.telegram}
          </a>
        </div>
      </footer>
    </main>
  );
}

export default Changelog;
