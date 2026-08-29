# Capability Kernel

Orca now treats every security-sensitive process launch as a capability
decision followed by an execution-broker launch (or a broker-authorized native
platform adapter where the OS requires handle-based spawning). The model, project files,
MCP messages, workflow scripts, and hook output can propose intent, but none
of them can mint a capability directly. Kernel/backend helper probes and test
fixtures are the only remaining direct `Command` calls; they are not exposed
as tool launch surfaces.

## Capability resolution

`CapabilityRequest` is immutable input. `EffectiveCapability::resolve` applies
the intersection of the request, the parent `CapabilityCeiling`, and the
approval-mode ceiling. Boolean capabilities, filesystem roots, denied roots,
and network targets are all intersected; relative paths are rejected. Plan is
a hard read-only ceiling: no permission rule, project file, or interactive
response can turn a write, network, shell, or agent request into an allowed
action.

`UserTrustedIntegration` is not a model-selectable process class. Ordinary
resolution rejects it; only an explicit user-owned launcher (for example the
MCP stdio transport or the danger-full-access shell path) may call
`resolve_user_trusted`. This keeps the advisory broker exception from becoming
a capability-escalation escape hatch.

Child agents use `CapabilitySet::ensure_subset_of`; a child that widens any bit
is rejected before it can start. The broker's final subset check also covers
filesystem roots and target sets, so a forged or stale materialized capability
cannot widen the parent after resolution. MCP stdio integrations declare
capabilities in user-owned configuration and default to read-only.

## Process ownership

`ExecutionBroker` is the process-launch choke point used by runtime shells,
MCP stdio transports, hooks, workflows, subagents, built-in process tools,
verification, notifications, updates, and worktree operations. A
non-dangerous process requires an `Enforced` backend;
`Advisory` and `Unavailable` states are not a fallback to host execution.
Danger-full-access is an explicit user-trusted integration and is surfaced as an
advisory receipt rather than being described as sandboxed. Windows AppContainer
and ConPTY launches are native adapters today; they must consume the same
resolved capability and receipt contract before a future broker adapter removes
that implementation split.

Every broker launch returns a `CapabilityReceipt` containing the request id,
process class, final boolean capabilities, cwd, roots, network targets, backend
name, and enforcement state.
The JSONL permission event includes the command, cwd, and the same capability
context so a client can make a decision from the actual launch boundary.

## Paths and denial evidence

`WorkspaceIdentity` canonicalizes the workspace root and cwd, checks containment,
and detects a root replacement on Unix using the device/inode fingerprint before
and after cwd resolution. Capability resolution also rejects relative and
`ParentDir` paths before any backend canonicalization. A path outside that
identity is rejected.

Sandbox stderr is diagnostic only. Path text emitted by a child process is not a
kernel fact and cannot trigger a filesystem or unsandboxed-shell retry. Future
escalation must carry a `SandboxDenialReceipt` emitted by a backend with an
explicit source (`Kernel` or `Backend`).

Retry authority is fingerprinted from the complete effective runtime settings
and tool catalog. Changing approval mode, permission rules, workspace roots,
network policy, or the active profile therefore invalidates an older retry
grant before any side effect is resumed.

## Configuration authority

Project `.orca/config.toml` is useful for model and presentation defaults, but
the user-owned configuration is authoritative for execution. Project files are
stripped of mode, permission rules/profiles, MCP servers, hooks, external tools,
subagents, workflows, tools, and budget settings before merge.

## Backend policy

Seatbelt workspace-write profiles do not grant global read. Linux sandbox setup
requires a fully enforced Landlock/seccomp ruleset for workspace-write and
read-only (including global-read) profiles; if bubblewrap/Landlock/seccomp
cannot enforce the requested policy it never runs a plain host shell as a
compatibility fallback. An
`ExternalSandbox` command policy is rejected until a broker-owned external
backend is available. macOS also has a restrictive Seatbelt probe so a
privileged/container runtime can be recognized as unable to enforce policy
instead of being treated as a normal host. Landlock roots are opened as
descriptor-backed `PathBeneath` rules and failures are fatal rather than
silently dropped. Windows AppContainer/ConPTY adapters obtain broker
authorization before native handle-based spawning.
