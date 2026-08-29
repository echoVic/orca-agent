# Capability Kernel Implementation Plan

> **For agentic workers:** This plan is executed inline in the current session. Each task follows a red/green/refactor cycle and must leave an independently testable change.

**Goal:** Replace Orca's split approval/sandbox/process paths with one monotonic capability kernel and one execution broker, then ship the change as a new release and npm patch.

**Architecture:** Every model-facing operation becomes a `CapabilityRequest` resolved into an immutable `EffectiveCapability` by intersection of platform, organization, user, session, turn, and tool ceilings. All child processes, MCP servers, workflows, hooks, terminals, and external tools launch through an `ExecutionBroker` that returns an enforcement receipt. Non-dangerous profiles fail closed when no OS backend can enforce them.

**Tech Stack:** Rust 2024 workspace, serde/JSON, Seatbelt, Landlock/seccomp, Windows sandbox crates, cargo tests, npm packaging scripts.

**Spec:** The final architecture and security review in the current task conversation.

## Global Constraints

- No backward-compatible permission or sandbox wire fields; replace them with the capability request/receipt contract.
- No direct production `Command::spawn`/`ProcessJob::spawn` outside the broker and platform backend implementation.
- No non-dangerous fallback to an unsandboxed shell.
- Project configuration is non-authoritative for security capabilities.
- Every process launch returns an enforcement state and immutable receipt.
- Every production change starts with a failing test.

### Task 1: Freeze capability contracts

**Files:**
- Create: `crates/orca-core/src/capability.rs`
- Modify: `crates/orca-core/src/lib.rs`
- Test: `crates/orca-core/src/capability.rs`

- [x] Add tests for capability intersection, Plan hard ceiling, child subset checks, root/network intersection, and receipt serialization.
- [x] Run the focused capability tests after freezing the contract.
- [x] Implement `CapabilityRequest`, `EffectiveCapability`, `CapabilityCeiling`, `EnforcementState`, `CapabilityReceipt`, and typed path/network/process fields.
- [x] Run the focused tests and `cargo fmt --check`.

### Task 2: Add the execution broker

**Files:**
- Create: `crates/orca-runtime/src/execution_broker.rs`
- Modify: `crates/orca-runtime/src/lib.rs`, `crates/orca-runtime/src/runtime_bash.rs`, `crates/orca-runtime/src/terminal_service.rs`, `crates/orca-runtime/src/shell_session.rs`
- Test: `crates/orca-runtime/src/execution_broker.rs`

- [x] Add a test proving every broker launch carries a receipt and rejects unavailable enforcement for non-dangerous capabilities.
- [ ] Run the focused test to confirm the broker API is absent.
- [x] Implement the broker around existing platform process handles, with an explicit trusted-host class for operations that are not sandboxable.
- [x] Route security-sensitive production launches (Bash, terminal, MCP, hooks, workflows, subagents, built-in process tools, verification, updates, notifications, and worktrees) through the broker and run focused runtime tests.

### Task 3: Make path identity and cwd containment explicit

**Files:**
- Create: `crates/orca-core/src/workspace_identity.rs`
- Modify: `crates/orca-runtime/src/runtime_normal_tool.rs`, `crates/orca-runtime/src/server.rs`, `crates/orca-tools/src/sandbox/mod.rs`
- Test: `crates/orca-core/src/workspace_identity.rs`, `crates/orca-runtime/src/runtime_normal_tool.rs`

- [x] Add tests for outside-workspace cwd, symlinked cwd, `..` traversal, and rename races.
- [x] Implement canonical workspace identity and descriptor-backed cwd launch handles with Unix device/inode replacement detection.
- [x] Separate read, write, metadata, and denied roots and preserve protected metadata denies.
- [x] Run path and sandbox tests on the host platform.

### Task 4: Enforce backend availability

**Files:**
- Modify: `crates/orca-tools/src/sandbox/linux.rs`, `crates/orca-tools/src/sandbox/seatbelt.rs`, `crates/orca-windows-sandbox/src`, `crates/orca-runtime/src/command_exec_sandbox.rs`
- Test: existing Linux/macOS/Windows sandbox suites plus new backend-state tests

- [x] Add tests asserting non-dangerous profiles reject missing bwrap/Landlock/Seatbelt/WFP instead of running plain shell.
- [x] Remove the Linux `strict=false` plain-command fallback.
- [x] Keep absolute backend resolution and expose `Enforced/Unavailable/Advisory` to the broker; restrictive Seatbelt probing is available on macOS.
- [x] Add descriptor-backed Landlock rules and strict backend-state checks. Cross-platform runner remains required for final OS validation.

### Task 5: Remove project execution authority and unify child services

**Files:**
- Modify: `crates/orca-core/src/config/file.rs`, `crates/orca-core/src/config/mod.rs`, `crates/orca-mcp/src/transport.rs`, `crates/orca-runtime/src/runtime_host.rs`, `crates/orca-runtime/src/workflow/host.rs`, `crates/orca-tools/src/external.rs`
- Test: config, MCP, workflow, and external-tool tests

- [ ] Add failing tests proving project files cannot set mode, roots, network, hooks, or MCP commands.
- [x] Move MCP, workflow, hook, and external launches to the broker with explicit capability classes and minimal environments.
- [x] Remove optional runtime config APIs that select default sandbox behavior and reject legacy unsandboxed retries.
- [x] Require explicit user-owned MCP capability configuration; config-digest invalidation remains part of the next persistence pass.

### Task 6: Replace denial heuristics with structured receipts

**Files:**
- Modify: `crates/orca-runtime/src/sandbox_denial.rs`, `crates/orca-runtime/src/runtime_permission.rs`, `crates/orca-runtime/src/runtime_bash.rs`
- Test: denial and permission retry tests

- [x] Add a failing test showing forged stderr cannot create an authority-bearing filesystem request.
- [x] Retain stderr parsing only as explanatory text with explicit non-authority provenance. Backend receipt consumption is the only permitted future escalation path.
- [x] Remove automatic pathless escalation to full access.

### Task 7: Replace permission UI and protocol

**Files:**
- Modify: `crates/orca-runtime/src/runtime_surface/interaction.rs`, `crates/orca-runtime/src/runtime_surface/operation.rs`, `crates/orca-tui/src/types.rs`, `crates/orca-tui/src/operation_controller.rs`, JSONL/ACP protocol modules
- Test: runtime surface and TUI interaction contract tests

- [x] Add protocol coverage for command, cwd, roots, network targets, enforcement state, and provenance.
- [x] Add command/cwd/capability context to permission events and TUI boundary labeling. Legacy persisted settings are rejected at execution rather than widened into host access.
- [x] Include the complete effective runtime settings and tool catalog in the capability fingerprint so persisted retry authority is invalidated when profiles, roots, network policy, or approval settings change.

### Task 8: Verification, documentation, and release

**Files:**
- Modify: `site/src/docs/md/zh/approval-modes.mdx`, `site/src/docs/md/en/approval-modes.mdx`, `site/src/docs/md/zh/configuration.mdx`, `site/src/docs/md/en/configuration.mdx`, `SECURITY.md`, release metadata
- Test: full workspace tests, platform boundary scripts, npm smoke/release verification

- [x] Run static spawn-site audit and the complete host-platform adversarial matrix; cross-target checks remain runner-gated where the required toolchain/SDK is unavailable locally.
- [x] Update security and user documentation to describe capability profiles, enforcement states, project-config limits, and strictest rule semantics.
- [x] Run formatter, workspace check, focused/full core tests, runtime/TUI test compilation, npm stage/version checks, and `git diff --check`; clippy remains warning-noisy in pre-existing code and is not a release gate.
- [ ] Create the release tag, publish the npm patch package, and independently verify tag, GitHub release, registry metadata, installation smoke, and public docs. Local `0.4.6` npm tarball packing and version-sync checks are complete; publication awaits the final commit and CI.
