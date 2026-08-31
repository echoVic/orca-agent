export type Locale = "en" | "zh";

export const localeStorageKey = "orca-site-locale";
export const canonicalOrigin = "https://orcaagent.dev";
export const socialImageUrl = `${canonicalOrigin}/orca-social.png`;

export const releaseVersion = "v0.4.7";

export const releases = [
  {
    version: "v0.4.7",
    date: "2026-08-31",
    title: "Observable subagents and fenced permission recovery",
    body: "Unifies synchronous and asynchronous child activity through one durable relay, renders expandable child tasks with transcript access, and closes detached permission races with actor-authenticated decisions, deterministic identities, cancellation linearization, terminal cleanup, and strict task/subagent authority pairing.",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.4.7",
  },
  {
    version: "v0.4.6",
    date: "2026-08-29",
    title: "Capability kernel and fail-closed execution",
    body: "Freezes capability intersection and Plan ceilings, routes shell and MCP stdio launches through an execution broker, rejects workspace escapes and backend fallbacks, makes project execution settings non-authoritative, and keeps sandbox denial text explanatory instead of authority-bearing.",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.4.6",
  },
  {
    version: "v0.4.5",
    date: "2026-08-29",
    title: "Direct TUI owner imports",
    body: "Completes the TUI state-owner migration by removing the stale types compatibility facade. Protocol, transcript, interaction, viewport, and surface projection values now have one import path enforced by an architecture contract, so new code cannot silently aggregate dedicated owner types back into types.rs.",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.4.5",
  },
  {
    version: "v0.4.4",
    date: "2026-08-29",
    title: "TUI state ownership convergence",
    body: "Moves TUI protocol values into protocol.rs, AppState event reduction into state_reducer.rs, and independent transcript, interaction, and viewport state into focused owners. Tests follow those boundaries, while the thin AppState aggregate remains the composition root and types keeps compatibility re-exports for existing in-tree consumers.",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.4.4",
  },
  {
    version: "v0.4.3",
    date: "2026-08-25",
    title: "Queue running state and busy-submit auto-queueing",
    body: "Shows the running queue head in the TUI queue preview, and auto-queues submissions made while the session is busy through the durable prompt queue instead of rejecting them. A backgrounded turn with no background capacity is queued the same way instead of failing the handoff, so the session completes successfully after the queued turn runs. Queue pause/delete interactions and task interruption are wired into the hosted operation controller.",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.4.3",
  },
  {
    version: "v0.4.2",
    date: "2026-08-23",
    title: "Linear surface commit recovery",
    body: "Indexes prepared surface commit batches by commit id while scanning the recorded surface JSONL log, so recovery scales linearly with the number of commit records instead of quadratically. A bounded-time regression test recovers 25,000 prepared/committed pairs within five seconds.",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.4.2",
  },
  {
    version: "v0.4.1",
    date: "2026-08-23",
    title: "Faster streaming surfaces and hardened sandbox metadata",
    body: "Batches provider step commits with deferred surface refresh, caches token counts, shards the reducer state, and coalesces consecutive MessageDelta/ReasoningDelta events before renderer dispatch to cut UI render passes. Sandbox metadata handling hardens: Linux/bubblewrap re-binds nested protected metadata read-only, symlinked metadata roots cannot widen grants, session grants pass a safety guard before recording, and reserved session-metadata sources are ignored at the thread boundary so forged configuration cannot mint metadata escalation authority. Workflow active-state classification switches to exclusion with reported status sequences.",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.4.1",
  },
  {
    version: "v0.4.0",
    date: "2026-08-22",
    title: "DeepSeek vision, image paste, and runtime controllers",
    body: "Adds end-to-end multimodal support for DeepSeek's deepseek-v4-flash-vision-exp model across clipboard paste, dragged or pasted image paths, TUI file mentions, ACP image blocks, durable prompt queues, conversation history, continuation checkpoints, and OpenAI-compatible provider requests. Atomic [Image #N] attachments survive editing, queueing, and rejected submissions; clipboard reads stay off the renderer thread and preserve paste-before-send ordering across macOS, Linux, Windows, and WSL. RuntimeHost now delegates generation context, interaction routing, operation recovery, Goal control, and task/workflow ownership to focused controllers; stable ContextWindowId epochs make compaction boundaries explicit and restart-safe.",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.4.0",
  },
  {
    version: "v0.3.26",
    date: "2026-08-21",
    title: "Reusable session-scoped shell permission capability",
    body: "Makes the unsandboxed shell permission an explicit, reusable capability: it lives in the turn permission overlay with delta and merge semantics, bash consumes it before prompting, and a grant is recorded only on allow. Session-scoped allow responses persist the capability into thread settings and JSONL metadata, restore it into every new turn and command_exec operation, and validate grant deltas against the requested profile, so a sandbox denial can never trigger repeated approval requests or silently widen authority.",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.26",
  },
  {
    version: "v0.3.25",
    date: "2026-08-20",
    title: "Checkpointable agent continuation and large Goal pastes",
    body: "Adds runtime-owned, checkpointable continuation lineages for synchronous subagents, async workers, and Workflow children, with leased attempts, digest-verified checkpoints, replay boundaries, cold recovery, and fail-closed indeterminate side effects. A durable prompt queue now projects the same lifecycle across TUI, ACP, JSONL, and Headless. The TUI also adopts Codex-style paste chips and materializes active Goal pastes under ORCA_HOME, including bounded objective files and transactional cleanup.",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.25",
  },
  {
    version: "v0.3.24",
    date: "2026-08-19",
    title: "Durable interactions across every runtime surface",
    body: "Unifies tool approval, permission requests, user input, and MCP elicitation behind one durable interaction broker across TUI, ACP, JSONL, and Headless. Restartable intents, exact response fencing, pre-side-effect permission retry, durable invocation receipts, and stable continuation operation identities make crash recovery fail closed without replaying ambiguous side effects. The obsolete RuntimePendingInteractionStore compatibility API is removed.",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.24",
  },
  {
    version: "v0.3.23",
    date: "2026-08-18",
    title: "Unified interactive execution and proactive terminal supervision",
    body: "Adds thread-owned exec_command and write_stdin sessions with optional PTY, raw terminal control input, bounded incremental output, and task-based process-tree control. A single-owner background supervisor now settles natural exits and external stop requests without another poll, releases session resources, and injects exactly-once bounded completion notifications before the next model turn unless the terminal was already observed.",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.23",
  },
  {
    version: "v0.3.22",
    date: "2026-08-17",
    title: "Interactive configuration, structured questions, and terminal-native editing",
    body: "Adds an interactive /config panel for session model, reasoning, and approval settings; presents ask_user_question choices as an owned selection dialog with multi-select and custom-answer support; and separates editor shortcuts from global, idle, and running actions so readline and Vim keys edit drafts without triggering transcript search, help, scrolling, backtracking, or background-task actions.",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.22",
  },
  {
    version: "v0.3.21",
    date: "2026-08-17",
    title: "Reliable TUI state ownership and legacy task recovery",
    body: "Keeps interactive state consistent across Side switching, queued input, approvals, terminal startup, and renderer wakeups by giving each concern one projection or lifecycle owner. Restarted sessions now reconcile legacy active and terminal workflow tasks into the current durable task model instead of losing or duplicating their visible state.",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.21",
  },
  {
    version: "v0.3.20",
    date: "2026-08-13",
    title: "Durable project memory with bounded, verified recall",
    body: "Recorded root turns can now asynchronously extract a bounded set of durable project facts after transcript and verifier success. The project-scoped ledger uses leased, fenced jobs for crash recovery and retry; recall is provenance-bearing internal context backed by a repairable SQLite FTS index with lexical fallback. Manual /remember remains separate, and auto_memory = false disables automatic capture and recall.",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.20",
  },
  {
    version: "v0.3.19",
    date: "2026-08-13",
    title: "Durable execution-budget accounting across restart and suspension",
    body: "Makes budget usage a versioned journal fact instead of an in-memory suspension snapshot. Restarts recover the original operation budget and cumulative turns, tools, cost, and wall deadline; provider settlement is idempotent by response identity; concurrent children split the parent's actual remaining additive capacity and share one wall-clock deadline; and every runtime surface projects the same durable budget terminal.",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.19",
  },
  {
    version: "v0.3.16",
    date: "2026-08-13",
    title: "Execution budget protocol hardening: durable boundaries, child leases, truthful terminals",
    body: "Three review rounds close every execution-budget protocol gap: budget stops commit the real durable resume boundary before any resumable terminal, child-agent leases reserve and settle per child with usage receipts, suspended and stateless exchanges never claim resumability or success they cannot back, and the operation journal records the committed tool outcome with dimension-specific stop reasons.",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.16",
  },
  {
    version: "v0.3.15",
    date: "2026-08-12",
    title: "High-parallelism test reliability, fenced goal schema initialization",
    body: "Eliminates the high-parallelism test hangs and flaky failures under --test-threads=16, fences goal schema initialization against concurrent opens, and makes default-parallelism local verification reliable again.",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.15",
  },
  {
    version: "v0.3.14",
    date: "2026-08-10",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.14",
  },
  {
    version: "v0.3.13",
    date: "2026-08-10",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.13",
  },
  {
    version: "v0.3.12",
    date: "2026-08-10",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.12",
  },
  {
    version: "v0.3.11",
    date: "2026-08-09",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.11",
  },
  {
    version: "v0.3.10",
    date: "2026-08-08",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.10",
  },
  {
    version: "v0.3.9",
    date: "2026-08-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.9",
  },
  {
    version: "v0.3.8",
    date: "2026-08-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.8",
  },
  {
    version: "v0.3.7",
    date: "2026-08-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.7",
  },
  {
    version: "v0.3.6",
    date: "2026-08-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.6",
  },
  {
    version: "v0.3.5",
    date: "2026-08-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.5",
  },
  {
    version: "v0.3.4",
    date: "2026-08-05",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.4",
  },
  {
    version: "v0.3.3",
    date: "2026-08-03",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.3",
  },
  {
    version: "v0.3.2",
    date: "2026-08-03",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.2",
  },
  {
    version: "v0.3.1",
    date: "2026-08-03",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.1",
  },
  {
    version: "v0.3.0",
    date: "2026-08-02",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.3.0",
  },
  {
    version: "v0.2.56",
    date: "2026-07-29",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.56",
  },
  {
    version: "v0.2.55",
    date: "2026-07-27",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.55",
  },
  {
    version: "v0.2.54",
    date: "2026-07-26",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.54",
  },
  {
    version: "v0.2.53",
    date: "2026-07-25",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.53",
  },
  {
    version: "v0.2.52",
    date: "2026-07-25",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.52",
  },
  {
    version: "v0.2.51",
    date: "2026-07-25",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.51",
  },
  {
    version: "v0.2.50",
    date: "2026-07-19",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.50",
  },
  {
    version: "v0.2.49",
    date: "2026-07-18",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.49",
  },
  {
    version: "v0.2.48",
    date: "2026-07-18",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.48",
  },
  {
    version: "v0.2.47",
    date: "2026-07-18",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.47",
  },
  {
    version: "v0.2.46",
    date: "2026-07-18",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.46",
  },
  {
    version: "v0.2.45",
    date: "2026-07-17",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.45",
  },
  {
    version: "v0.2.44",
    date: "2026-07-17",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.44",
  },
  {
    version: "v0.2.43",
    date: "2026-07-17",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.43",
  },
  {
    version: "v0.2.42",
    date: "2026-07-17",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.42",
  },
  {
    version: "v0.2.36",
    date: "2026-07-17",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.36",
  },
  {
    version: "v0.2.35",
    date: "2026-07-17",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.35",
  },
  {
    version: "v0.2.34",
    date: "2026-07-17",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.34",
  },
  {
    version: "v0.2.33",
    date: "2026-07-16",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.33",
  },
  {
    version: "v0.2.32",
    date: "2026-07-16",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.32",
  },
  {
    version: "v0.2.31",
    date: "2026-07-16",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.31",
  },
  {
    version: "v0.2.30",
    date: "2026-07-16",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.30",
  },
  {
    version: "v0.2.29",
    date: "2026-07-15",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.29",
  },
  {
    version: "v0.2.28",
    date: "2026-07-15",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.28",
  },
  {
    version: "v0.2.27",
    date: "2026-07-15",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.27",
  },
  {
    version: "v0.2.26",
    date: "2026-07-15",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.26",
  },
  {
    version: "v0.2.25",
    date: "2026-07-15",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.25",
  },
  {
    version: "v0.2.24",
    date: "2026-07-15",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.24",
  },
  {
    version: "v0.2.23",
    date: "2026-07-14",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.23",
  },
  {
    version: "v0.2.22",
    date: "2026-07-14",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.22",
  },
  {
    version: "v0.2.21",
    date: "2026-07-13",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.21",
  },
  {
    version: "v0.2.20",
    date: "2026-07-12",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.20",
  },
  {
    version: "v0.2.19",
    date: "2026-07-12",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.19",
  },
  {
    version: "v0.2.18",
    date: "2026-07-12",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.18",
  },
  {
    version: "v0.2.17",
    date: "2026-07-12",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.17",
  },
  {
    version: "v0.2.16",
    date: "2026-07-11",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.16",
  },
  {
    version: "v0.2.15",
    date: "2026-07-11",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.15",
  },
  {
    version: "v0.2.14",
    date: "2026-07-10",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.14",
  },
  {
    version: "v0.2.13",
    date: "2026-07-10",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.13",
  },
  {
    version: "v0.2.12",
    date: "2026-07-10",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.12",
  },
  {
    version: "v0.2.11",
    date: "2026-07-10",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.11",
  },
  {
    version: "v0.2.10",
    date: "2026-07-10",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.10",
  },
  {
    version: "v0.2.9",
    date: "2026-07-10",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.9",
  },
  {
    version: "v0.2.8",
    date: "2026-07-10",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.8",
  },
  {
    version: "v0.2.7",
    date: "2026-07-10",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.7",
  },
  {
    version: "v0.2.6",
    date: "2026-07-10",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.6",
  },
  {
    version: "v0.2.5",
    date: "2026-07-10",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.5",
  },
  {
    version: "v0.2.4",
    date: "2026-07-10",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.4",
  },
  {
    version: "v0.2.3",
    date: "2026-07-09",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.3",
  },
  {
    version: "v0.2.2",
    date: "2026-07-09",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.2",
  },
  {
    version: "v0.2.1",
    date: "2026-07-09",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.1",
  },
  {
    version: "v0.2.0",
    date: "2026-07-09",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.2.0",
  },
  {
    version: "v0.1.191",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.191",
  },
  {
    version: "v0.1.190",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.190",
  },
  {
    version: "v0.1.189",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.189",
  },
  {
    version: "v0.1.188",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.188",
  },
  {
    version: "v0.1.187",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.187",
  },
  {
    version: "v0.1.186",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.186",
  },
  {
    version: "v0.1.185",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.185",
  },
  {
    version: "v0.1.184",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.184",
  },
  {
    version: "v0.1.183",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.183",
  },
  {
    version: "v0.1.182",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.182",
  },
  {
    version: "v0.1.181",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.181",
  },
  {
    version: "v0.1.180",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.180",
  },
  {
    version: "v0.1.179",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.179",
  },
  {
    version: "v0.1.178",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.178",
  },
  {
    version: "v0.1.177",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.177",
  },
  {
    version: "v0.1.176",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.176",
  },
  {
    version: "v0.1.175",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.175",
  },
  {
    version: "v0.1.174",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.174",
  },
  {
    version: "v0.1.173",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.173",
  },
  {
    version: "v0.1.172",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.172",
  },
  {
    version: "v0.1.171",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.171",
  },
  {
    version: "v0.1.170",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.170",
  },
  {
    version: "v0.1.169",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.169",
  },
  {
    version: "v0.1.168",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.168",
  },
  {
    version: "v0.1.167",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.167",
  },
  {
    version: "v0.1.166",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.166",
  },
  {
    version: "v0.1.165",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.165",
  },
  {
    version: "v0.1.164",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.164",
  },
  {
    version: "v0.1.163",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.163",
  },
  {
    version: "v0.1.162",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.162",
  },
  {
    version: "v0.1.161",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.161",
  },
  {
    version: "v0.1.160",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.160",
  },
  {
    version: "v0.1.159",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.159",
  },
  {
    version: "v0.1.158",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.158",
  },
  {
    version: "v0.1.157",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.157",
  },
  {
    version: "v0.1.156",
    date: "2026-07-07",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.156",
  },
  {
    version: "v0.1.155",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.155",
  },
  {
    version: "v0.1.154",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.154",
  },
  {
    version: "v0.1.153",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.153",
  },
  {
    version: "v0.1.152",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.152",
  },
  {
    version: "v0.1.151",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.151",
  },
  {
    version: "v0.1.150",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.150",
  },
  {
    version: "v0.1.149",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.149",
  },
  {
    version: "v0.1.148",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.148",
  },
  {
    version: "v0.1.147",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.147",
  },
  {
    version: "v0.1.146",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.146",
  },
  {
    version: "v0.1.145",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.145",
  },
  {
    version: "v0.1.144",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.144",
  },
  {
    version: "v0.1.143",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.143",
  },
  {
    version: "v0.1.142",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.142",
  },
  {
    version: "v0.1.141",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.141",
  },
  {
    version: "v0.1.140",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.140",
  },
  {
    version: "v0.1.139",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.139",
  },
  {
    version: "v0.1.138",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.138",
  },
  {
    version: "v0.1.137",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.137",
  },
  {
    version: "v0.1.136",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.136",
  },
  {
    version: "v0.1.135",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.135",
  },
  {
    version: "v0.1.134",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.134",
  },
  {
    version: "v0.1.133",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.133",
  },
  {
    version: "v0.1.132",
    date: "2026-07-06",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.132",
  },
  {
    version: "v0.1.131",
    date: "2026-07-05",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.131",
  },
  {
    version: "v0.1.130",
    date: "2026-07-05",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.130",
  },
  {
    version: "v0.1.129",
    date: "2026-07-05",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.129",
  },
  {
    version: "v0.1.128",
    date: "2026-07-05",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.128",
  },
  {
    version: "v0.1.127",
    date: "2026-07-05",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.127",
  },
  {
    version: "v0.1.126",
    date: "2026-07-05",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.126",
  },
  {
    version: "v0.1.125",
    date: "2026-07-05",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.125",
  },
  {
    version: "v0.1.124",
    date: "2026-07-05",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.124",
  },
  {
    version: "v0.1.123",
    date: "2026-07-05",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.123",
  },
  {
    version: "v0.1.122",
    date: "2026-07-04",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.122",
  },
  {
    version: "v0.1.121",
    date: "2026-07-04",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.121",
  },
  {
    version: "v0.1.120",
    date: "2026-07-04",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.120",
  },
  {
    version: "v0.1.119",
    date: "2026-07-04",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.119",
  },
  {
    version: "v0.1.118",
    date: "2026-07-04",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.118",
  },
  {
    version: "v0.1.117",
    date: "2026-07-04",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.117",
  },
  {
    version: "v0.1.116",
    date: "2026-07-04",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.116",
  },
  {
    version: "v0.1.115",
    date: "2026-07-04",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.115",
  },
  {
    version: "v0.1.114",
    date: "2026-07-04",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.114",
  },
  {
    version: "v0.1.113",
    date: "2026-07-03",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.113",
  },
  {
    version: "v0.1.112",
    date: "2026-07-03",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.112",
  },
  {
    version: "v0.1.111",
    date: "2026-07-03",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.111",
  },
  {
    version: "v0.1.110",
    date: "2026-07-03",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.110",
  },
  {
    version: "v0.1.109",
    date: "2026-07-03",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.109",
  },
  {
    version: "v0.1.108",
    date: "2026-07-03",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.108",
  },
  {
    version: "v0.1.107",
    date: "2026-07-03",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.107",
  },
  {
    version: "v0.1.106",
    date: "2026-07-03",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.106",
  },
  {
    version: "v0.1.105",
    date: "2026-07-03",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.105",
  },
  {
    version: "v0.1.104",
    date: "2026-07-03",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.104",
  },
  {
    version: "v0.1.103",
    date: "2026-07-03",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.103",
  },
  {
    version: "v0.1.102",
    date: "2026-07-03",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.102",
  },
  {
    version: "v0.1.101",
    date: "2026-07-02",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.101",
  },
  {
    version: "v0.1.100",
    date: "2026-07-02",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.100",
  },
  {
    version: "v0.1.99",
    date: "2026-07-02",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.99",
  },
  {
    version: "v0.1.98",
    date: "2026-07-02",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.98",
  },
  {
    version: "v0.1.97",
    date: "2026-07-02",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.97",
  },
  {
    version: "v0.1.96",
    date: "2026-07-02",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.96",
  },
  {
    version: "v0.1.95",
    date: "2026-07-02",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.95",
  },
  {
    version: "v0.1.94",
    date: "2026-07-02",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.94",
  },
  {
    version: "v0.1.93",
    date: "2026-07-02",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.93",
  },
  {
    version: "v0.1.92",
    date: "2026-07-02",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.92",
  },
  {
    version: "v0.1.91",
    date: "2026-07-02",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.91",
  },
  {
    version: "v0.1.90",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.90",
  },
  {
    version: "v0.1.89",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.89",
  },
  {
    version: "v0.1.88",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.88",
  },
  {
    version: "v0.1.87",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.87",
  },
  {
    version: "v0.1.86",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.86",
  },
  {
    version: "v0.1.85",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.85",
  },
  {
    version: "v0.1.84",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.84",
  },
  {
    version: "v0.1.83",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.83",
  },
  {
    version: "v0.1.82",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.82",
  },
  {
    version: "v0.1.81",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.81",
  },
  {
    version: "v0.1.80",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.80",
  },
  {
    version: "v0.1.79",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.79",
  },
  {
    version: "v0.1.78",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.78",
  },
  {
    version: "v0.1.77",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.77",
  },
  {
    version: "v0.1.76",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.76",
  },
  {
    version: "v0.1.75",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.75",
  },
  {
    version: "v0.1.74",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.74",
  },
  {
    version: "v0.1.73",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.73",
  },
  {
    version: "v0.1.72",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.72",
  },
  {
    version: "v0.1.71",
    date: "2026-07-01",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.71",
  },
  {
    version: "v0.1.70",
    date: "2026-06-30",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.70",
  },
  {
    version: "v0.1.69",
    date: "2026-06-30",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.69",
  },
  {
    version: "v0.1.68",
    date: "2026-06-30",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.68",
  },
  {
    version: "v0.1.67",
    date: "2026-06-30",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.67",
  },
  {
    version: "v0.1.66",
    date: "2026-06-30",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.66",
  },
  {
    version: "v0.1.65",
    date: "2026-06-30",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.65",
  },
  {
    version: "v0.1.64",
    date: "2026-06-30",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.64",
  },
  {
    version: "v0.1.63",
    date: "2026-06-30",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.63",
  },
  {
    version: "v0.1.62",
    date: "2026-06-30",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.62",
  },
  {
    version: "v0.1.61",
    date: "2026-06-30",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.61",
  },
  {
    version: "v0.1.59",
    date: "2026-06-29",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.59",
  },
  {
    version: "v0.1.58",
    date: "2026-06-29",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.58",
  },
  {
    version: "v0.1.57",
    date: "2026-06-29",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.57",
  },
  {
    version: "v0.1.56",
    date: "2026-06-29",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.56",
  },
  {
    version: "v0.1.55",
    date: "2026-06-29",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.55",
  },
  {
    version: "v0.1.53",
    date: "2026-06-29",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.53",
  },
  {
    version: "v0.1.52",
    date: "2026-06-29",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.52",
  },
  {
    version: "v0.1.51",
    date: "2026-06-29",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.51",
  },
  {
    version: "v0.1.50",
    date: "2026-06-29",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.50",
  },
  {
    version: "v0.1.49",
    date: "2026-06-29",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.49",
  },
  {
    version: "v0.1.48",
    date: "2026-06-29",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.48",
  },
  {
    version: "v0.1.47",
    date: "2026-06-29",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.47",
  },
  {
    version: "v0.1.46",
    date: "2026-06-29",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.46",
  },
  {
    version: "v0.1.45",
    date: "2026-06-29",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.45",
  },
  {
    version: "v0.1.44",
    date: "2026-06-29",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.44",
  },
  {
    version: "v0.1.43",
    date: "2026-06-29",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.43",
  },
  {
    version: "v0.1.42",
    date: "2026-06-27",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.42",
  },
  {
    version: "v0.1.41",
    date: "2026-06-26",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.41",
  },
  {
    version: "v0.1.40",
    date: "2026-06-26",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.40",
  },
  {
    version: "v0.1.39",
    date: "2026-06-26",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.39",
  },
  {
    version: "v0.1.38",
    date: "2026-06-26",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.38",
  },
  {
    version: "v0.1.37",
    date: "2026-06-26",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.37",
  },
  {
    version: "v0.1.36",
    date: "2026-06-26",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.36",
  },
  {
    version: "v0.1.35",
    date: "2026-06-25",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.35",
  },
  {
    version: "v0.1.34",
    date: "2026-06-25",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.34",
  },
  {
    version: "v0.1.33",
    date: "2026-06-25",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.33",
  },
  {
    version: "v0.1.32",
    date: "2026-06-25",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.32",
  },
  {
    version: "v0.1.31",
    date: "2026-06-25",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.31",
  },
  {
    version: "v0.1.30",
    date: "2026-06-25",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.30",
  },
  {
    version: "v0.1.29",
    date: "2026-06-24",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.29",
  },
  {
    version: "v0.1.28",
    date: "2026-06-24",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.28",
  },
  {
    version: "v0.1.27",
    date: "2026-06-23",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.27",
  },
  {
    version: "v0.1.26",
    date: "2026-06-22",
    url: "https://github.com/echoVic/orca-agent/releases/tag/v0.1.26",
  },
] as const;

export type ReleaseEntry = (typeof releases)[number];
export type ReleaseVersion = ReleaseEntry["version"];

export const links = {
  github: "https://github.com/echoVic/orca-agent",
  npm: "https://www.npmjs.com/package/@blade-ai/orca",
  releases: "https://github.com/echoVic/orca-agent/releases/latest",
  telegram: "https://t.me/+11No1w5ZbTMyZTQ1",
  home: "/",
  changelog: "/changelog/",
  terminalCodingAgent: "/terminal-coding-agent/",
  deepseekCodingAgent: "/deepseek-coding-agent/",
  githubWorkflows: "/github/",
  mcp: "/mcp/",
  docs: "/docs/",
} as const;

export function detectInitialLocale(): Locale {
  if (typeof window === "undefined") {
    return "en";
  }
  const stored = window.localStorage.getItem(localeStorageKey);
  if (stored === "en" || stored === "zh") {
    return stored;
  }
  return window.navigator.language.toLowerCase().startsWith("zh") ? "zh" : "en";
}

function setMetaAttribute(
  selector: string,
  attributeName: "content" | "href",
  value: string,
  createElement: () => HTMLMetaElement | HTMLLinkElement,
) {
  const existing = document.head.querySelector<HTMLMetaElement | HTMLLinkElement>(selector);
  const element = existing ?? createElement();
  element.setAttribute(attributeName, value);
  if (!existing) {
    document.head.appendChild(element);
  }
}

export function setNamedMeta(name: string, content: string) {
  setMetaAttribute(`meta[name="${name}"]`, "content", content, () => {
    const meta = document.createElement("meta");
    meta.setAttribute("name", name);
    return meta;
  });
}

export function setPropertyMeta(property: string, content: string) {
  setMetaAttribute(`meta[property="${property}"]`, "content", content, () => {
    const meta = document.createElement("meta");
    meta.setAttribute("property", property);
    return meta;
  });
}

export function setCanonicalLink(href: string) {
  setMetaAttribute('link[rel="canonical"]', "href", href, () => {
    const link = document.createElement("link");
    link.setAttribute("rel", "canonical");
    return link;
  });
}

export type SeoEntry = {
  title: string;
  description: string;
  ogTitle: string;
  ogDescription: string;
  imageAlt: string;
  locale: string;
};

export function applySeoHead(locale: Locale, seo: SeoEntry, canonicalUrl: string) {
  document.documentElement.lang = locale === "zh" ? "zh-CN" : "en";
  document.title = seo.title;
  setCanonicalLink(canonicalUrl);
  setNamedMeta("description", seo.description);
  setNamedMeta("twitter:title", seo.ogTitle);
  setNamedMeta("twitter:description", seo.ogDescription);
  setNamedMeta("twitter:image", socialImageUrl);
  setNamedMeta("twitter:image:alt", seo.imageAlt);
  setPropertyMeta("og:title", seo.ogTitle);
  setPropertyMeta("og:description", seo.ogDescription);
  setPropertyMeta("og:url", canonicalUrl);
  setPropertyMeta("og:image", socialImageUrl);
  setPropertyMeta("og:image:alt", seo.imageAlt);
  setPropertyMeta("og:locale", seo.locale);
  setPropertyMeta("og:locale:alternate", locale === "zh" ? "en_US" : "zh_CN");
}
