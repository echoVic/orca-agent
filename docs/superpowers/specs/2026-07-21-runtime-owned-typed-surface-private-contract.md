# Runtime-Owned Typed Surface Private Contract

- Date: 2026-07-21
- Status: approved for Phase 0A written review
- Contract version: `runtime-surface-private-v1`
- Baseline: `main@50b7698d1` (`v0.2.50`)
- Approved parent design: `89979d6062246ec4a9b98032ee62cc6d45c1e3ba`, blob
  `bf80af4ce1fde3607af3fd75874abddfaf79b450`, SHA-256
  `9fbe4c57fc8776e1bd0e87853b71640f2027d5be243a903be9fea88006f7dab0`
- Machine manifest:
  `2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`

## Purpose And Normative Order

This is the Phase 0A companion artifact required by the approved
runtime-owned typed surface design. It freezes the private implementation
contract from which Phase 0B exhaustive inventories and RED behavior tests are
written. It does not authorize production cutover.

The normative order is:

1. the approved parent design owns authority, durability, ordering, recovery,
   migration, and deletion invariants;
2. this document owns exact private types, state transitions, command payloads,
   results, and adapter dispositions;
3. the machine manifest owns exhaustive baseline source, action, entrypoint,
   command, mapping, and test-vector membership;
4. Phase 4A later owns the public ACP extension JSON schema and wire fixtures;
5. the released `v0.2.50` JSONL contract remains authoritative for its public
   field names, error shapes, and per-request ordering.

The document and manifest are one reviewed artifact bundle. A change to either
file changes `runtime-surface-private-v1` and requires both files to pass the
Phase 0A consistency checks again.

The words MUST, MUST NOT, REQUIRED, and ONLY are normative. Type definitions
use Rust-like notation. Every enum is closed. No type in this contract contains
`serde_json::Value`, an untagged open union, a wildcard semantic variant, or an
adapter-defined terminal outcome.
Within this notation, `Enum::Variant` in a field position denotes the closed
payload refinement type of that exact variant; implementations factor it into a
named Rust struct rather than treating an enum constructor as a Rust type.

## Closed Data Rules

### Primitive wrappers

```text
NonEmptyText(String)        // UTF-8, not empty after trim
DisplayText(String)         // UTF-8; presentation content, may be empty
SafeDiagnosticText(String)  // UTF-8; runtime-sanitized, <=4096 bytes, no input/secret content
CanonicalPath(PathBuf)      // absolute, normalized, root-checked by runtime
CanonicalUri(String)        // parsed URI with normalized scheme/authority
CanonicalMime(String)       // lower-case type/subtype without parameters
CanonicalDomainName(String) // lower-case IDNA ASCII host; no scheme/port/path/wildcard
FiniteF64(f64)              // rejects NaN and both infinities at construction
UnixMillis(i64)
Rfc3339Timestamp(String)    // canonical UTC RFC3339; preserves serialized precision
DurationMillis(u64)
MonotonicTick(u64)
ByteOffset(u64)
ByteCount(u64)
SequenceNumber(u64)
Revision(u64)               // nonzero after the first committed fact
Sha256Digest([u8; 32])
OpaqueToken([u8; 32])       // constant-time equality, never logged
UuidV7([u8; 16])            // UUID version 7; generated identities are canonical
Uuid([u8; 16])              // existing released UUID identity
Unit = ()

// Closed collection/value aliases used throughout this Rust-like contract.
Set<T>(ordered, finite set of T)
NonEmptyVec<T>(Vec<T>)      // constructor rejects an empty vector
NonEmptySet<T>(Set<T>)      // constructor rejects an empty set
Denied = Unit               // closed unit value for forbidden object extras

HostMonotonicClockId(UuidV7)
MonotonicInstant {
  clock_id: HostMonotonicClockId,
  tick: MonotonicTick,
}
```

`UnixMillis` and `Rfc3339Timestamp` are observational metadata only. They never
authorize expiry, takeover, revocation, or admission. Every authoritative
deadline uses an injected host monotonic clock. A `MonotonicInstant` is
comparable only with another instant from the same clock id; mismatch, overflow,
or a lost issuing host fails closed instead of falling back to wall time.

Every newly allocated durable identity below is UUIDv7. Existing session,
Goal, task, workflow, tool-call, and provider identities are parsed into their
dedicated wrapper and retain their released string form at compatibility
boundaries. An identity wrapper cannot be constructed from an empty or
ill-formed string.

```text
SurfaceThreadId(Uuid)
SurfaceOperationId(UuidV7)
SurfaceTurnId(Uuid)
SurfaceItemId(Uuid)
SurfaceStreamId(UuidV7)
SurfaceToolCallId(NonEmptyText)
SurfaceTaskId(NonEmptyText)
SurfaceWorkflowRunId(NonEmptyText)
SurfaceWorkflowResultId(NonEmptyText)
SurfaceSubagentId(NonEmptyText)
SurfaceGoalId(NonEmptyText)
SurfaceGoalRunId(NonEmptyText)
SurfaceGoalOuterTurnId(NonEmptyText)
SurfaceGoalIntentId(NonEmptyText)
SurfaceInteractionId(UuidV7)
SurfaceAttachmentId(UuidV7)
SurfaceResponseId(UuidV7)
SurfaceResponseReceiptId(UuidV7)
SurfaceResponseToken(OpaqueToken)
SurfaceResponseGrantToken(OpaqueToken)
SurfaceEventId(UuidV7)
SurfaceRequestId(UuidV7)
SurfaceCommitId(UuidV7)
SurfaceSettlementId(UuidV7)
SurfaceFinalizeIntentId(UuidV7)
SurfaceAdmissionLeaseId(UuidV7)
SurfaceInputCorrelationId(UuidV7)
SurfaceBackgroundOwnerToken(OpaqueToken)
SurfacePublisherPermitId(OpaqueToken)
SurfaceCapabilityCallId(UuidV7)
SurfaceRemoteTerminalId(NonEmptyText)
SurfaceCatalogEntryId(NonEmptyText)
SurfaceGenerationId(u64)    // starts at zero and is never reused per operation
SurfaceConnectionId(UuidV7)
```

`SurfaceGenerationId` starts at zero for the first generation of an operation
and increases by exactly one. It is never reused within that operation. The
following revisions are independent monotonically increasing
domains and MUST NOT be compared across domains:

```text
ThreadOwnerEpoch(u64)
HostIncarnation(UuidV7)
SurfaceIncarnation(UuidV7)
DurableRevision(Revision)
LiveRevision(Revision)
SessionCatalogRevision(Revision)
McpCatalogRevision(Revision)
InputCatalogRevision(Revision)
WorkflowCatalogRevision(Revision)
SessionMetadataRevision(Revision)
SettingsRevision(Revision)
TrustRevision(Revision)
PolicyEpoch(Revision)
MemoryRevision(Revision)
PinnedContextRevision(Revision)
SessionHealthRevision(Revision)
GoalRevision(Revision)
GoalObjectiveRevision(u32)
GoalCatalogRevision(Revision)
GoalOwnerEpoch(Revision)
TaskRevision(Revision)
WorkflowRevision(Revision)
SubagentRevision(Revision)
InteractionRevision(Revision)
ResponseRouteEpoch(Revision)
CapabilityRevision(Revision)
PlanRevision(Revision)
UsageRevision(Revision)
ContextRevision(Revision)
PinnedFileRevision(Revision)
PinnedUserRevision(Revision)
PinnedSystemRevision(Revision)
ProjectRootMemoryRevision(Revision)
BootstrapCredentialRevision(Revision)
HostLifecycleRevision(Revision)

PinnedContextSourceRevision =
  Memory(MemoryRevision)
  | File(PinnedFileRevision)
  | User(PinnedUserRevision)
  | System(PinnedSystemRevision)

HostRevisionWitness =
  Memory(MemoryRevision)
  | FolderTrust(TrustRevision)
  | RuntimeSettings(SettingsRevision)
  | SessionCatalog(SessionCatalogRevision)
  | SessionMetadata(SessionMetadataRevision)
  | HostLifecycle(HostLifecycleRevision)
```

`GoalRevision` is the per-Goal row/state revision. `GoalObjectiveRevision` is
incremented only when objective text changes. `GoalCatalogRevision` orders the
Goal store as a whole. None is interchangeable with another.

```text
SurfaceUnavailableReason =
  HostShuttingDown
  | ThreadClosing
  | ProjectionDegraded
  | CapacityExceeded
  | RuntimeUnavailable

OptionalProcessLocalCancel(opaque, process-local, non-authoritative)
ZeroizingProcessLocalSecret(opaque bytes, process-local, zeroized on drop)
SurfaceBoundCaller(
  opaque, process-local, unforgeable attachment capability,
  bound optional connection identity
)
SurfaceHostBoundCaller(
  opaque, process-local, unforgeable host capability,
  bound optional connection identity
)

AcpRequestId = String(NonEmptyText) | Integer(i64)
```

### Fences and scopes

```text
SurfaceOperationFence {
  thread_id: SurfaceThreadId,
  thread_owner_epoch: ThreadOwnerEpoch,
  operation_id: SurfaceOperationId,
  generation_id: SurfaceGenerationId,
}

SurfaceBackgroundFence {
  operation_fence: SurfaceOperationFence,
  background_owner_token: SurfaceBackgroundOwnerToken,
}

SurfaceGoalFence {
  goal_id: SurfaceGoalId,
  goal_revision: GoalRevision,
  goal_owner_epoch: GoalOwnerEpoch,
}

SurfaceTaskFence {
  task_id: SurfaceTaskId,
  task_revision: TaskRevision,
  background_owner: Option<SurfaceBackgroundFence>,
}

SurfaceWorkflowFence {
  workflow_run_id: SurfaceWorkflowRunId,
  workflow_revision: WorkflowRevision,
  parent: Option<SurfaceOperationFence>,
}

SurfaceScope =
  Thread
  | Operation { operation_id: SurfaceOperationId }
  | Generation { fence: SurfaceOperationFence }
  | Background { fence: SurfaceBackgroundFence }
  | Goal {
      goal_id: SurfaceGoalId,
      causative_generation: Option<SurfaceOperationFence>,
    }
```

Scope validation happens before persistence and again before reduction. A
publisher permit is closed to one scope class and identity. A stale scope or
permit is `Uncommitted(StalePublisherPermit)` and cannot append or publish.

### Commit classes and cursors

```text
CommitClass =
  Recorded {
    thread_owner_epoch: ThreadOwnerEpoch,
    durable_revision: DurableRevision,
    commit_id: SurfaceCommitId,
  }
  | Ephemeral {
      incarnation: SurfaceIncarnation,
      live_revision: LiveRevision,
      commit_id: SurfaceCommitId,
    }

CursorSourceRevision =
  Recorded { durable_revision: DurableRevision }
  | Ephemeral { live_revision: LiveRevision }

SurfaceCursor {
  thread_id: SurfaceThreadId,
  incarnation: SurfaceIncarnation,
  next_seq: SequenceNumber,
  source_revision: CursorSourceRevision,
}
```

A complete batch beginning at sequence `S` with `N` events has
`cursor_before.next_seq == S` and `cursor_after.next_seq == S + N`. Event
ordinals `0..N-1` are runtime-private positions and are never attachable
cursors. Both boundary cursors have the same thread and incarnation; the after
cursor carries the source revision containing the complete batch. Cursor source
variants never convert implicitly, and no public cursor may point inside a
batch.

## Capabilities

```text
SurfaceCapability =
  ReadSnapshot
  | ReadCatalog
  | SubmitOperation
  | ControlBoundOperation
  | ControlAnyVisibleOperation
  | LegacyCancelCurrent
  | LegacyInterruptResume
  | LegacyJsonlControl
  | RespondGrantedInteraction
  | ManageTask
  | ManageWorkflow
  | ManageGoal
  | ManageThreadSettings
  | ManagePinnedContext
  | RepairThread
  | ReadSessionCatalog
  | ManageSessionCatalog
  | ManageSessionLifecycle
  | ManageMemory
  | ReadHostPolicy
  | ManageFolderTrust
  | ReadHostSettings
  | ManageHostSettings
  | ShutdownHost
```

```text
SurfaceAttachmentGrant {
  attachment_id: SurfaceAttachmentId,
  host_incarnation: HostIncarnation,
  role: Tui | Acp | Jsonl | InternalCompatibility,
  capabilities: NonEmptySet<SurfaceCapability>,
  granted_at: SurfaceCursor,
  expires_at: Option<MonotonicInstant>,
}
```

`ControlBoundOperation` authorizes only an operation allocated by that
attachment or explicitly transferred to it by runtime. A snapshot does not by
itself grant control. `ControlAnyVisibleOperation` is a local host policy grant,
not an adapter inference. Interaction response authority always additionally
requires the exact route epoch and grant token.
Grant expiry is evaluated only by the issuing host's injected monotonic clock;
host-incarnation or clock-id mismatch makes the grant unusable. It cannot be
extended or revived from a serialized wall-clock timestamp.

```text
SurfacePublisherPermit =
  ActorControl {
    permit_id: SurfacePublisherPermitId,
    thread_id: SurfaceThreadId,
    owner_epoch: ThreadOwnerEpoch,
  }
  | Generation {
      permit_id: SurfacePublisherPermitId,
      fence: SurfaceOperationFence,
    }
  | Background {
      permit_id: SurfacePublisherPermitId,
      fence: SurfaceBackgroundFence,
    }
  | Goal {
      permit_id: SurfacePublisherPermitId,
      goal_fence: SurfaceGoalFence,
      receipt_digest: Sha256Digest,
    }
  | Finalizer {
      permit_id: SurfacePublisherPermitId,
      operation_id: SurfaceOperationId,
      finalize_intent_id: SurfaceFinalizeIntentId,
      owner_epoch: ThreadOwnerEpoch,
    }
  | Recovery {
      permit_id: SurfacePublisherPermitId,
      current_owner_epoch: ThreadOwnerEpoch,
      historical_fence: SurfaceOperationFence,
    }
```

Actor control cannot publish Terminal. Generation and background permits cannot
publish outside their exact fences. Goal requires a matching post-commit store
receipt. Only Finalizer can publish `OperationPatch::Terminal`.
Recovery is issued only after the process-lifetime lease is acquired and its
owner epoch advanced. It may publish only a recovery Stop for the exact
historical fence plus its atomic suspension/finalization disposition; it cannot
execute, admit, respond to interactions, or publish a success terminal.

```text
ProcessLeaseWitness(opaque, process-local, non-cloneable)

ThreadOwnershipLease {
  thread_id: SurfaceThreadId,
  host_incarnation: HostIncarnation,
  owner_epoch: ThreadOwnerEpoch,
  witness: ProcessLeaseWitness,
}

PolicyOwnerLease {
  lease_id: UuidV7,
  host_incarnation: HostIncarnation,
  observed_policy_epoch: PolicyEpoch,
  governed_roots: NonEmptySet<CanonicalPath>,
  witness: ProcessLeaseWitness,
  diagnostic_expires_at: UnixMillis,
}
```

Both leases are exclusive process-lifetime OS-lock witnesses and never enter a
surface event, snapshot, replay capsule, or adapter payload. A thread takeover
requires acquisition of the same exclusive lock and a durable owner-epoch CAS
before recovery can begin. A policy-owner removal from a revocation
acknowledgement set likewise requires acquiring that owner's lock and advancing
the durable policy-owner record. Heartbeats and expiry timestamps are diagnostic
eligibility signals only: clock advance or rollback, parse failure, an
unprobeable lock, or an expired row does not prove that an owner or governed
resource stopped. Such uncertainty remains a typed pending acknowledgement.

## Input And Operation Intent

```text
SurfaceInputBindingKind =
  File
  | Directory
  | Skill
  | Plugin
  | Workflow
  | McpResource
  | McpResourceTemplate
  | McpTool

SurfaceInputBinding {
  kind: SurfaceInputBindingKind,
  identity: SurfaceCatalogEntryId,
  observed_catalog_revision: InputCatalogRevision,
  observed_settings_revision: SettingsRevision,
  label: NonEmptyText,
}

SurfaceLegacyMentionTarget =
  File {
    root: SurfaceLegacyPath,
    path: SurfaceLegacyPath,
    kind: File | Directory,
  }
  | Skill { id: DisplayText, path: SurfaceLegacyPath }
  | Plugin { name: DisplayText, manifest_path: SurfaceLegacyPath }
  | Resource { server: DisplayText, uri: SurfaceLegacyUri }
  | ResourceTemplate { server: DisplayText, uri_template: SurfaceLegacyUri }

SurfaceLegacyPath(DisplayText)
SurfaceLegacyUri(DisplayText)

SurfaceInputBindingRequest =
  ExactCatalog {
    kind: SurfaceInputBindingKind,
    identity: SurfaceCatalogEntryId,
    observed_catalog_revision: InputCatalogRevision,
    observed_settings_revision: SettingsRevision,
    label: NonEmptyText,
  }
  | LegacyJsonlMention {
      name: DisplayText,
      visible: DisplayText,
      start: ByteOffset,
      end: ByteOffset,
      target: SurfaceLegacyMentionTarget,
    }

SurfaceInputRequestBlock =
  Text { text: DisplayText }
  | Binding { binding: SurfaceInputBindingRequest }
  | ResourceLink {
      uri: CanonicalUri,
      name: NonEmptyText,
      description: Option<DisplayText>,
      mime: Option<CanonicalMime>,
    }
  | EmbeddedText {
      uri: CanonicalUri,
      mime: CanonicalMime,
      text: DisplayText,
      digest: Sha256Digest,
    }

SurfaceInputBlock =
  Text { text: DisplayText }
  | Binding { binding: SurfaceInputBinding }
  | ResourceLink {
      uri: CanonicalUri,
      name: NonEmptyText,
      description: Option<DisplayText>,
      mime: Option<CanonicalMime>,
    }
  | EmbeddedText {
      uri: CanonicalUri,
      mime: CanonicalMime,
      text: DisplayText,
      digest: Sha256Digest,
    }

SurfaceInput {
  blocks: NonEmptyVec<SurfaceInputBlock>,
  canonical_text: DisplayText,
  bindings_digest: Sha256Digest,
}

SurfaceInputRequest {
  blocks: NonEmptyVec<SurfaceInputRequestBlock>,
}
```

`canonical_text` is runtime-produced after decoding and is not a client-owned
resource expansion. A binding is resolved only after its generation Started
barrier. Unsupported images, audio, blobs, malformed typed resources, or unknown
content fail before Requested unless a frozen legacy disposition explicitly
says `LegacyAcceptedDropped`. A `SurfaceLegacyPath` or `SurfaceLegacyUri` is raw
released-wire data: relative, nonexistent, out-of-root, and malformed values
remain representable until the post-Started resolver canonicalizes and
authorizes them. Failure there is an input-resolution terminal, not a
pre-Requested decoder error.

`OperationRequestIntent`, `SteerOperation`, and Goal run input accept only
`SurfaceInputRequest`. The runtime reconstructs `SurfaceInput.canonical_text`
and `.bindings_digest` from those blocks after catalog/settings validation;
callers cannot submit, replace, or validate those derived fields. Compatibility
decoders that receive legacy canonical fields discard them before private
ingress and report `InvalidInput` if their digest was advertised as an
authoritative precondition.
`SurfaceInputBindingRequest` values are descriptors, not authority. Runtime
freezes the matching catalog/settings revisions during reservation, then after
the generation Started barrier resolves either the exact catalog identity or
the closed legacy target and constructs `SurfaceInputBinding`. A forged label,
stale exact revision, mismatched kind/root, or changed legacy target is a typed
input-resolution failure; the adapter never performs a lookup-then-start fill.

For `turn/start`, the released JSONL decoder preserves its text assembly
exactly: text items are joined with one `\n`; legacy mentions preserve their
computed visible text and byte offsets; and skill/mention targets become
`LegacyJsonlMention` descriptors only after the released `retain_valid`
normalization. That normalization first drops any empty/out-of-range/non-UTF-8-
boundary span or visible-text mismatch, then stable-sorts survivors by start
offset and drops every survivor whose start is before the previously retained
end. Dropped bindings are compatibility no-ops, not later resolution failures.
If input is missing or every accepted
image/local-image/incomplete-skill member is `LegacyAcceptedDropped`, it emits
one `Text { text: "" }` request block so the nonempty private request remains
wire-compatible. Released `turn/steer` is intentionally different: it keeps
only the assembled visible prompt as one `Text` block and discards all mention
binding authority, exactly matching `prompt_from_turn_start_params`; steer never
creates a `LegacyJsonlMention` descriptor.

```text
OperationOrigin =
  TuiUser
  | TuiWorkflowNotification { result_id: SurfaceWorkflowResultId }
  | AcpPrompt {
      connection_id: SurfaceConnectionId,
      session_id: NonEmptyText,
      inbound_seq: SequenceNumber,
      rpc_request_id: AcpRequestId,
    }
  | JsonlThreadTurn {
      connection_id: SurfaceConnectionId,
      rpc_id_digest: Sha256Digest,
      legacy_turn_id: LegacyTurnId,
    }
  | JsonlStatelessSubmit {
      connection_id: SurfaceConnectionId,
      rpc_id_digest: Sha256Digest,
    }
  | RuntimeWorkflowResult { result_id: SurfaceWorkflowResultId }

OperationIngressCorrelation =
  TuiUser
  | TuiWorkflowNotification { result_id: SurfaceWorkflowResultId }
  | AcpPrompt {
      session_id: NonEmptyText,
      inbound_seq: SequenceNumber,
      rpc_request_id: AcpRequestId,
    }
  | JsonlThreadTurn {
      rpc_id_digest: Sha256Digest,
      legacy_turn_id: LegacyTurnId,
    }
  | JsonlStatelessSubmit { rpc_id_digest: Sha256Digest }

SurfaceInternalOriginPermit(
  opaque, process-local, runtime-owned, bound to one workflow-result admission
)

LastUserTurn = MostRecent

OperationKind =
  UserTurn
  | GoalRun {
      goal_id: SurfaceGoalId,
      goal_run_id: SurfaceGoalRunId,
      initial_objective_revision: GoalObjectiveRevision,
    }
  | ManualCompaction { reason: Manual }
  | Backtrack { target: LastUserTurn }
  | StandaloneWorkflow { workflow: SurfaceCatalogEntryId }
  | WorkflowResultFollowup { result_id: SurfaceWorkflowResultId }

BusyDisposition = Queue | NotAdmittedImmediately

InterruptSettlement =
  SuspendUntilExplicitControl
  | TerminalizeCancelledAtInterruptedStopUnlessResumeQueued

LegacyVisibility =
  PublishAfterAdmitted
  | JsonlBindingsResolvedBeforeTurnStarted

OperationRequestIntent {
  correlation: OperationIngressCorrelation,
  kind: OperationKind,
  input: Option<SurfaceInputRequest>,
  replayability: ReplayabilityRequest,
  settings_preparation: OperationSettingsPreparation,
}

ReplayabilityRequest =
  CaptureReplayableCapsule
  | NonReplayable { reason: HistoryDisabled | Redacted | SecretInput | Missing }

OperationIntent {
  origin: OperationOrigin,
  kind: OperationKind,
  initial_replayability: Replayability,
  busy_disposition: BusyDisposition,
  interrupt_settlement: InterruptSettlement,
  legacy_visibility: LegacyVisibility,
  settings_revision: SettingsRevision,
  policy_epoch: PolicyEpoch,
  required_capabilities: Set<SurfaceCapability>,
  capability_fingerprint: Sha256Digest,
  settings_receipt: OperationSettingsPreparationReceipt,
}

OperationSettingsPreparation =
  UseCurrent {
    expected_settings_revision: SettingsRevision,
    expected_policy_epoch: PolicyEpoch,
  }
  | ApplyThreadOverridesBeforeRequested {
      expected_settings_revision: SettingsRevision,
      expected_policy_epoch: PolicyEpoch,
      patches: NonEmptyVec<RuntimeSettingsPatch>,
    }

OperationSettingsPreparationReceipt =
  Current {
    settings_revision: SettingsRevision,
    policy_epoch: PolicyEpoch,
  }
  | ThreadOverridesCommitted {
      previous_settings_revision: SettingsRevision,
      settings_revision: SettingsRevision,
      policy_epoch: PolicyEpoch,
      patches_digest: Sha256Digest,
      host_commit_id: SurfaceCommitId,
      thread_settings_cursor: SurfaceCursor,
    }

Replayability =
  Replayable {
    capsule_digest: Sha256Digest,
    request: Option<SurfaceInputRequest>,
    request_digest: Option<Sha256Digest>,
    cwd: CanonicalPath,
    workspace_roots: Vec<CanonicalPath>,
    settings_revision: SettingsRevision,
    policy_epoch: PolicyEpoch,
    tool_schema_digest: Sha256Digest,
  }
  | NonReplayable {
      reason: HistoryDisabled | Redacted | SecretInput | Missing,
      live_capsule: LiveOperationCapsule,
    }

LiveOperationCapsule =
  Available {
    incarnation: SurfaceIncarnation,
  }
  | Unavailable

LiveCapsuleStatus =
  Current
  | NotCurrent {
      descriptor: Stale { incarnation: SurfaceIncarnation } | Unavailable,
    }

ReplayabilityClass = Replayable | NonReplayable(LiveCapsuleStatus)

FinalizerPhaseClass =
  Admitted
  | SuspendedResumeStarting { generation_id: SurfaceGenerationId }

AdmittedInput =
  PendingUser {
    item_id: SurfaceItemId,
    presentation: SurfaceInputPresentation,
    correlation_id: SurfaceInputCorrelationId,
  }
  | NotApplicable
```

`OperationRequestIntent.input` exists only before the Requested commit.
`OperationIntent` never duplicates it. A Replayable intent durably freezes the
typed request descriptor in `Replayability::Replayable.request`; its resolved
`SurfaceInput` does not exist until the post-Started resolution transition. The
request, its canonical request digest, and capsule digest are committed together.
`request` and `request_digest` are either both present or both absent. They are
absent exactly for an operation whose `AdmittedInput` is `NotApplicable`.
A NonReplayable intent stores no executable content or
content-revealing digest: `LiveOperationCapsule::Available` names a
process-local registry entry whose bytes are non-serializable and never enter a
snapshot/event. Rematerialization preserves the historical descriptor but never
rehydrates the registry entry; `Available` with a non-current incarnation is
evaluated as unavailable. Missing that entry before Started fails the operation
without reconstructing forbidden input.
`LiveCapsuleStatus` is a pure classifier of the descriptor and current surface
incarnation. Finalizer and recovery tables match only the closed
`ReplayabilityClass` plus `FinalizerPhaseClass`; they do not embed inequality
guards or implicit process state in record patterns.

Clients submit an `OperationRequestIntent`; they never submit an
`OperationOrigin`, required capability set, or the prepared `OperationIntent`
stored in an `OperationRecord`. `RuntimeSurfaceClientHandle` accepts only the
correlation variant permitted by its bound role, injects its bound connection
identity, and rejects a role mismatch before Requested. TUI cannot claim ACP or
JSONL correlation, one transport connection cannot nominate another, and no
attachment handle can construct the runtime-only workflow-result origin. That
origin requires an exact `SurfaceInternalOriginPermit` held by `ThreadActor`.
Attachment-submitted `ReserveOperation` may construct only `OperationKind::UserTurn`.
`WorkflowResultFollowup` requires the runtime-only workflow-result permit;
GoalRun, ManualCompaction, Backtrack, and StandaloneWorkflow are constructed only
by their dedicated Goal, maintenance, or workflow coordinator commands. No wire
or attachment payload can deserialize those privileged kinds.
Automatic Goal continuation never enters reservation preparation: it appends a
new generation under the existing operation through the Goal coordinator batch.
Recovery likewise retains the existing operation and uses `ResumeOperation` or
the closed recovery finalizer mapping; neither can commit a new `Requested`.

The private ingress derives the final required capability set plus busy,
interrupt, and visibility behavior from the validated closed `OperationOrigin`:

| Origin | Busy | Interrupt | Visibility |
| --- | --- | --- | --- |
| TUI, ACP, Goal, workflow | `Queue` | `SuspendUntilExplicitControl` | `PublishAfterAdmitted` |
| JSONL thread turn | `NotAdmittedImmediately` | `TerminalizeCancelledAtInterruptedStopUnlessResumeQueued` | `JsonlBindingsResolvedBeforeTurnStarted` |
| JSONL stateless submit | `NotAdmittedImmediately` | terminalize unless resume queued | JSONL binding barrier |

This is runtime-owned compatibility policy. Adapters cannot override the table.
For `NotAdmittedImmediately`, the actor checks the foreground, reservation, and
finalization slots in mailbox order before allocating an operation or emitting
`Requested`. A busy check returns `Uncommitted(OperationActive)` with no
operation id, FIFO entry, item, or terminal fact. This is the exact released
JSONL busy-error branch. Once a reservation commits, ordinary reservation
finalization and `NotAdmitted` semantics apply; an adapter never performs a
separate precheck.

Reservation preparation is one actor/coordinator sequence with this order:

1. validate the request shape, handle-bound role/connection correlation, caller
   capability, input content, expected settings revision, expected policy epoch,
   and origin-derived compatibility values;
2. for `NotAdmittedImmediately`, inspect the foreground, reservation, and
   finalization slots in mailbox order. A busy result exits here with no settings
   mutation and no operation fact;
3. for `UseCurrent`, freeze the observed settings and policy revisions into
   `OperationSettingsPreparationReceipt::Current`;
4. for `ApplyThreadOverridesBeforeRequested`, validate the complete patch vector
   against the expected revisions, commit the host receipt and thread Settings
   cursor through one coordinator batch, and only accept the resulting
   `ThreadOverridesCommitted` receipt when both acknowledgements are present;
5. inject the validated `OperationOrigin`, derive the mandatory capability set
   from the origin, operation kind, input, settings, and negotiated attachment
   grant, then derive the final `OperationIntent`, replayability capsule,
   settings revision, and policy epoch and commit `Operation::Requested` with
   that intent;
6. if the settings batch commits but its host or thread acknowledgement is
   missing, return `Deferred` with no operation value and the exact missing
   acknowledgement. Retrying the same request probes that batch before it can
   allocate `Requested`; it never applies the patches twice.

`OperationIntent.settings_revision` and `.policy_epoch` therefore always refer
to the committed preparation receipt, never to caller-supplied values. A
`ReplayabilityRequest` is likewise resolved only after preparation; a replayable
capsule carries the final settings/policy revisions in `Replayability`.
`OperationIntent.initial_replayability` is the admission receipt for generation
zero only. Each `GenerationRecord.replayability` is the sole executable capsule
for that generation and Started/input resolution/resume compare only that exact
fence and capsule digest. Generation zero must equal the initial receipt; a Goal
continuation carries its newly prepared request/digest, while a generic recovery
replacement must be byte-identical to its stopped predecessor capsule.
Generation capability ownership is identical: generation zero's required set
and fingerprint equal `OperationIntent`; a Goal continuation recomputes and
freezes its own set before GenerationReserved; a recovery replacement copies the
exact predecessor set/fingerprint.

## Operation And Generation Domain

```text
ReservationLease {
  lease_id: SurfaceAdmissionLeaseId,
  operation_id: SurfaceOperationId,
  reservation_sequence: SequenceNumber,
  issuing_host_incarnation: HostIncarnation,
  issued_at: MonotonicInstant,
  duration: DurationMillis,       // exactly 30_000 in v1
}

OperationPhase =
  Requested
  | Admitted
  | Suspended { cause: SuspensionCause }
  | Finalizing { finalize_intent_id: SurfaceFinalizeIntentId }
  | FinalizingDegraded { finalize_intent_id: SurfaceFinalizeIntentId }
  | Terminal

GenerationPhase = Reserved | Started | Transferred | Stopped

TerminalizationCause =
  UserCancel
  | GoalPause
  | HostShutdown
  | ThreadClose

SuspensionCause =
  Interrupted { generation_id: SurfaceGenerationId }
  | RecoveryRequired { generation_id: SurfaceGenerationId }
  | ProviderSuspended { generation_id: SurfaceGenerationId }

SuspendedFinalizationCause =
  Terminalization(TerminalizationCause)
  | ResumeStartCommitFailure { message: SafeDiagnosticText }
  | RecoveryAbortNonReplayable { last_generation: SurfaceGenerationId }

PendingControlIntent =
  Interrupt { generation_fence: SurfaceOperationFence }
  | Terminalize {
      operation_id: SurfaceOperationId,
      cause: TerminalizationCause,
    }
  | ResumeStarting { generation_fence: SurfaceOperationFence }
  | ResumeAfterInterruptedStop { generation_fence: SurfaceOperationFence }
  | BackgroundOnStart {
      operation_id: SurfaceOperationId,
      reservation_sequence: SequenceNumber,
    }
```

```text
NotStartedReason =
  ReservationExpired
  | Cancelled { cause: TerminalizationCause }
  | Interrupted
  | RuntimeRestart
  | StartCommitFailure { message: SafeDiagnosticText }
  | MissingLiveInputCapsule
  | AdmissionRejected { reason: AdmissionRejectionReason }
  | Shutdown { reason: HostShutdown | ThreadClose }

GenerationStopReason =
  Completed { status: GenerationCompletionStatus }
  | Cancelled { cause: TerminalizationCause }
  | InterruptedResumable
  | ProviderSuspended
  | RuntimeRestart
  | ProjectionFailure { message: SafeDiagnosticText }
  | ExecutionFailed {
      class: GenerationExecutionFailureClass,
      message: SafeDiagnosticText,
    }
  | Panicked { message: SafeDiagnosticText }
  | NotStarted { reason: NotStartedReason }

GenerationCompletionStatus =
  Success
  | VerificationFailed { message: SafeDiagnosticText }
  | BudgetExhausted { budget: OperationBudget }

GenerationExecutionFailureClass =
  Provider
  | Tool
  | Hook
  | Workflow
  | InputResolution
  | ClientCapabilityUnavailable
  | LegacyApprovalRequired
  | RuntimeInvariant
  | ExternalEffectAmbiguous
  | RemoteResourceCleanupAmbiguous

SurfaceGoalGenerationIdentity {
  goal_id: SurfaceGoalId,
  goal_run_id: SurfaceGoalRunId,
  operation_fence: SurfaceOperationFence,
  goal_outer_turn_id: SurfaceGoalOuterTurnId,
  logical_turn_id: SurfaceTurnId,
  canonical_input_item_id: SurfaceItemId,
  outer_turn_origin: User | Resume | Continuation | WorkflowNotification,
  attempt: Initial | RecoveryReplacement,
  predecessor_fence: Option<SurfaceOperationFence>,
  objective_revision: GoalObjectiveRevision,
  outer_turn_count: u32,
}

GenerationStartedWitness {
  started_commit_id: SurfaceCommitId,
  settings_revision: SettingsRevision,
  policy_epoch: PolicyEpoch,
  durable_replayability_digest: Sha256Digest,
  capability_fingerprint: Sha256Digest,
}

InputResolutionErrorCode =
  MalformedLegacyTarget
  | StaleCatalog
  | KindMismatch
  | OutsideWorkspace
  | TargetNotFound
  | ReadFailed
  | UnsupportedMime
  | RuntimeUnavailable

SurfaceInputPresentation =
  Visible { text: DisplayText }
  | Redacted

SurfaceResolvedInputFact =
  Replayable {
    input: SurfaceInput,
    request_digest: Sha256Digest,
  }
  | NonReplayable {
      presentation: SurfaceInputPresentation,
      live_capsule_incarnation: SurfaceIncarnation,
    }

GenerationInputState =
  NotApplicable
  | Pending {
      input_item_id: SurfaceItemId,
      presentation: SurfaceInputPresentation,
      correlation_id: SurfaceInputCorrelationId,
    }
  | Resolved {
      input_item_id: SurfaceItemId,
      fact: SurfaceResolvedInputFact,
    }
  | Failed {
      input_item_id: SurfaceItemId,
      code: InputResolutionErrorCode,
    }

GenerationRecord {
  fence: SurfaceOperationFence,
  logical_turn_id: SurfaceTurnId,
  input: GenerationInputState,
  predecessor: Option<SurfaceOperationFence>,
  attempt: Initial | RecoveryReplacement,
  goal_identity: Option<SurfaceGoalGenerationIdentity>,
  replayability: Replayability,
  required_capabilities: Set<SurfaceCapability>,
  capability_fingerprint: Sha256Digest,
  phase: GenerationPhase,
  started_witness: Option<GenerationStartedWitness>,
  stop_reason: Option<GenerationStopReason>,
}

SurfaceAgentLoopTurn {
  turn_id: SurfaceTurnId,
  fence: SurfaceOperationFence,
  ordinal: u32, // starts at one and increments within the operation
  task_id: SurfaceTaskId,
  task_status: Running,
}

OperationRecord {
  operation_id: SurfaceOperationId,
  request_id: SurfaceRequestId,
  intent: OperationIntent,
  phase: OperationPhase,
  reservation: ReservationLease,
  ready_for_admission: bool,
  initial_logical_turn_id: Option<SurfaceTurnId>,
  initial_input_item_id: Option<SurfaceItemId>,
  generations: Vec<GenerationRecord>,
  agent_loop_turns: Vec<SurfaceAgentLoopTurn>,
  pending_control: Option<PendingControlIntent>,
  finalization: Option<OperationFinalizationRecord>,
  terminal: Option<OperationTerminalRecord>,
}
```

`SurfaceResolvedInputFact::Replayable` is legal only when the exact generation
carries `Replayability::Replayable { request: Some, request_digest: Some }`; the fact's
request digest must equal that durable request digest, and its input (including
the sole bindings digest) may then be recorded.
`NonReplayable` never contains input or a content-derived digest and its
incarnation must equal the live capsule. `Redacted`, `SecretInput`, and `Missing`
always use `SurfaceInputPresentation::Redacted`. `HistoryDisabled` may use
`Visible` only with an Ephemeral commit in the current incarnation. The paired
Operation and Item patches carry an identical fact, so no durable/public patch
can bypass these confidentiality rules.

// Closed reducer transition table. Any source/target pair not listed here is
// `SurfaceReducerErrorCode::IllegalTransition`.
OperationTransition =
  Requested -> Admitted
  | Requested -> Terminal(NotAdmitted)
  | Admitted -> Suspended
  | Admitted -> Finalizing
  | Suspended -> Admitted(GenerationStarted under matching ResumeStarting)
  | Suspended -> Suspended(SuspensionRebasedAfterUnstartedResume)
  | Suspended -> Finalizing(SuspendedFinalizationCause)
  | Finalizing -> Terminal
  | Finalizing -> FinalizingDegraded
  | FinalizingDegraded -> Terminal(RetryFinalization)
  | FinalizingDegraded -> Terminal(RetryProjectionTerminal)

GenerationTransition =
  Reserved -> Started
  | Reserved -> Stopped(NotStarted)
  | Started -> Stopped
  | Started -> Transferred
  | Transferred -> Stopped

OperationGenerationInvariant =
  SameOperationThreadAndOwnerEpoch
  | FirstGenerationIsZeroReserved
  | GenerationIdsContiguous
  | PredecessorStoppedBeforeSuccessorReserved
  | NoGenerationTransitionAfterStopped
  | SuspendedNamesExactStoppedGeneration
  | TerminalHasExactlyOneRecord
  | LiveOperationHasNoTerminalRecord
  | BackgroundFenceMatchesTransferredGeneration
  | StartedFreezesSettingsPolicyReplayability
  | AgentLoopOrdinalStrictlyIncreases
  | TerminalUsageMatchesRecord
  | GoalGenerationIdentityMatchesGeneration
  | GoalPredecessorAuthorizesAtMostOneSuccessor
  | JoinFailedOnlyFromOperationJoinSettlement
  | InputResolutionAfterStartedBeforeExecution

Reservation expiry compares `issued_at + duration` only against the injected
clock with the same clock id while `issuing_host_incarnation` is current. The
monotonic deadline is never reconstructed from wall time. After host-incarnation
loss, Requested follows the explicit `RuntimeRestart` reservation-finalizer row;
it is never reclassified by elapsed wall time.

The first `Admitted` patch carries generation id `0`, phase `Reserved`, no
`started_witness`, and no `stop_reason`. Its generation input is `Pending`
exactly for `AdmittedInput::PendingUser` and `NotApplicable` exactly for
`AdmittedInput::NotApplicable`. The exhaustive operation-kind table is:

| Operation kind | First-generation input |
| --- | --- |
| UserTurn | PendingUser |
| GoalRun | PendingUser |
| WorkflowResultFollowup | PendingUser |
| ManualCompaction | NotApplicable |
| Backtrack | NotApplicable |
| StandaloneWorkflow | NotApplicable |

Every later generation id is exactly
the previous id plus one and its predecessor fence is in the same operation and
already `Stopped`. `Suspended` and `RecoveryRequired` patches set
`OperationPhase::Suspended` with the exact stopped generation and cause; they
cannot be emitted from a live or merely Reserved generation. `ResumeOperation`
and `ResumeAfterInterruptedStop` carry that exact fence. `Terminal` is emitted
only after all generations are stopped and the operation finalizer has fixed
one terminal record. If `OperationTerminal::Succeeded` carries usage, it must
equal `OperationTerminalRecord.usage`; for every other terminal, the record's
usage is the final aggregate used by all adapters. `GenerationStarted` freezes
the operation settings revision, policy epoch, exact
`GenerationRecord.replayability`, and capability fingerprint in one
`GenerationStartedWitness`. Generation zero additionally equals
`OperationIntent.initial_replayability`. Its settings/policy values equal
the prepared intent, its capability fingerprint equals
`GenerationRecord.capability_fingerprint` (and generation zero therefore equals
`OperationIntent.capability_fingerprint`), and its durable replayability digest
is the canonical digest of only stable fields: the Replayable capsule digest or
the NonReplayable reason. It excludes live-capsule availability/incarnation.
These rules
apply equally to Replayable and NonReplayable operations.
Requested carries only the generation-zero replayability receipt; it has no
input gate. First Admitted creates the generation's `Pending` input and paired
pending UserMessage, or `NotApplicable` with no Item. Only a matching Started
generation can commit Resolved/Failed,
and Resolved carries the same item id/input/digest as its paired Item patch.
`AgentLoopTurnStarted` must use a Started generation with a Resolved or
NotApplicable input gate, the next ordinal, and a task in `Running` state.
For every Goal generation, `goal_identity` is present and its operation fence,
logical turn, canonical input, predecessor, attempt, and outer-turn count
match the enclosing `GenerationRecord` and the Goal patch committed in the same
coordinator batch. Its `goal_id` and `goal_run_id` equal
`OperationIntent.kind::GoalRun`; generation zero's `objective_revision` equals
`initial_objective_revision`, while every continuation generation equals the
objective revision in its own admitting Goal-store receipt. That run is the
current run in the matching receipt, and the receipt's owner/revision fence
authorizes the same Goal. `outer_turn_origin` matches the admitting Goal decision (`User`,
`Resume`, `Continuation`, or `WorkflowNotification`) rather than being a free
display label. A Goal generation cannot be `NotApplicable`; its identity's
canonical input item equals the item id in that generation's Pending, Resolved,
or Failed state. Recovery replacements preserve that id, while a continuation
successor allocates a new id. Non-Goal generations have no `goal_identity`. One predecessor
fence may authorize at most one successor identity; exact replay is idempotent
and any changed field is `StaleIdentity`. `ResumeStarting` may reserve the next
generation while the operation remains `Suspended`; the phase changes to
`Admitted` only when that generation's Started commit succeeds.

```text
TurnRequestBudgetScope = AgentLoop | Subagent

OperationBudget =
  ModelTokens { limit: Option<u64>, observed: Option<u64> }
  | TurnRequests {
      scope: TurnRequestBudgetScope,
      limit: u64,
      observed: u64,
    }
  | GoalTokenBudget {
      goal_id: SurfaceGoalId,
      limit: i64,
      observed: i64,
    }
  | WorkflowTokenBudget {
      workflow_run_id: SurfaceWorkflowRunId,
      limit: u64,
      observed: u64,
    }
  | MonetaryBudgetUsdMicros { limit: u64, observed: u64 }

AdmissionRejectionReason =
  ConfigurationConflict
  | PolicyConflict

NotAdmittedReason =
  CancelledBeforeAdmission
  | ReservationExpired
  | ConfigurationConflict
  | PolicyConflict
  | RuntimeRestart
  | HostShutdown
  | ThreadClose

CancelReason = User | GoalPause

InteractionCancelReason =
  OperationCancelled { reason: CancelReason }
  | HostShutdown
  | ThreadClose
  | CapabilityUnavailable
  | ExpiryAuthorityUnavailable {
      deadline: InteractionExpiryDeadline,
      failure: InteractionExpiryAuthorityFailure,
    }

FailureClass =
  Provider
  | Tool
  | Hook
  | Workflow
  | Verification
  | InputResolution
  | ClientCapabilityUnavailable
  | LegacyApprovalRequired
  | RuntimeInvariant
  | Persistence
  | ExternalEffectAmbiguous
  | RemoteResourceCleanupAmbiguous

OperationTerminal =
  NotAdmitted { reason: NotAdmittedReason }
  | Succeeded { usage: UsageTotals }
  | Cancelled { reason: CancelReason }
  | BudgetExhausted { budget: OperationBudget }
  | Failed { class: FailureClass, message: SafeDiagnosticText }
  | Panicked { message: SafeDiagnosticText }
  | JoinFailed { message: SafeDiagnosticText }
  | AbortedByRuntimeRestart {
      last_generation: SurfaceGenerationId,
    }
  | Shutdown { reason: HostShutdown | ThreadClose }

OperationTerminalRecord {
  operation_id: SurfaceOperationId,
  finalize_intent_id: SurfaceFinalizeIntentId,
  terminal: OperationTerminal,
  usage: UsageTotals,
  source_diagnostic_digest: Option<Sha256Digest>,
  settlement_receipts: Vec<SurfaceSettlementReceipt>,
  committed_at: UnixMillis,
}

SurfaceSettlementReceipt {
  settlement_id: SurfaceSettlementId,
  receipt_digest: Sha256Digest,
}

FinalizationStartedAtCursor {
  operation_id: SurfaceOperationId,
  finalize_intent_id: SurfaceFinalizeIntentId,
  terminal_commit_id: SurfaceCommitId,
  event_id: SurfaceEventId,
  cursor: SurfaceCursor,
  commit_class: CommitClass,
  batch_digest: Sha256Digest,
}

OperationFinalizationRecord {
  finalize_intent_id: SurfaceFinalizeIntentId,
  terminal_commit_id: SurfaceCommitId,
  started_at: FinalizationStartedAtCursor,
  selected_cause: OperationFinalizationCause,
  suspended_cause: Option<SuspendedFinalizationCause>,
  expected_settlements: Vec<SurfaceSettlementId>,
  settled: Vec<SurfaceSettlementReceipt>,
}

FinalizationDegradedCause =
  MissingFinalization {
    terminal_commit_id: SurfaceCommitId,
    missing_settlements: NonEmptyVec<SurfaceSettlementId>,
    missing_set_digest: Sha256Digest,
  }
  | TerminalProjectionPending {
      terminal_commit_id: SurfaceCommitId,
      terminal_event_id: SurfaceEventId,
      durable_revision: DurableRevision,
      terminal_digest: Sha256Digest,
    }

ReservationFinalizerReason =
  ReservationExpired
  | AdmissionRejected { reason: AdmissionRejectionReason }
  | CancelledBeforeAdmission
  | RuntimeRestart
  | HostShutdown
  | ThreadClose

ReservationFinalizerSource {
  reason: ReservationFinalizerReason,
}

OperationJoinSettlementSource {
  operation_id: SurfaceOperationId,
  finalize_intent_id: SurfaceFinalizeIntentId,
  settlement_id: SurfaceSettlementId,
  settlement_receipt_digest: Sha256Digest,
  message: SafeDiagnosticText,
}

OperationFinalizerSource =
  GenerationStop { reason: GenerationStopReason }
  | Reservation { source: ReservationFinalizerSource }
  | OperationJoinSettlement { source: OperationJoinSettlementSource }

OperationFinalizationCause =
  Terminalization(TerminalizationCause)
  | GenerationStop(GenerationStopReason)
  | Reservation(ReservationFinalizerReason)
  | OperationJoinSettlement(OperationJoinSettlementSource)
  | Suspended(SuspendedFinalizationCause)

AdmissionRejectionTerminalMapping =
  ConfigurationConflict -> NotAdmittedReason::ConfigurationConflict
  | PolicyConflict -> NotAdmittedReason::PolicyConflict

JoinFailedTerminalMapping =
  OperationFinalizerSource::OperationJoinSettlement { source }
    -> Terminal(JoinFailed { message: source.message })

LiveGenerationStopDisposition =
  Completed(Success) when OperationKind=GoalRun and
      GoalContinuationDecision=Admitted { successor, .. }
      -> ContinueGoal {
           successor,
           atomic_batch: GenerationStopped + OuterTurnFinished
                         + ContinuationDecided(Admitted)
                         + GenerationReserved(successor),
         }; no FinalizationStarted or Terminal
  | Completed(Success) when OperationKind!=GoalRun
      -> Terminal(Succeeded { usage: final_operation_usage })
  | Completed(Success) when OperationKind=GoalRun and
      GoalContinuationDecision=Stopped { terminal, .. }
      -> Terminal(terminal) after GoalContinuationStopReason binding validation
  | Completed(VerificationFailed { message })
      -> Terminal(Failed { class: Verification, message })
  | Completed(BudgetExhausted { budget }) -> Terminal(BudgetExhausted { budget })
  | Cancelled { cause: UserCancel } -> Terminal(Cancelled { reason: User })
  | Cancelled { cause: GoalPause } -> Terminal(Cancelled { reason: GoalPause })
  | Cancelled { cause: HostShutdown } -> Terminal(Shutdown { reason: HostShutdown })
  | Cancelled { cause: ThreadClose } -> Terminal(Shutdown { reason: ThreadClose })
  | InterruptedResumable when ReplayabilityClass=Replayable or
      ReplayabilityClass=NonReplayable(Current)
      -> OperationPhase::Suspended; no Terminal
  | InterruptedResumable when ReplayabilityClass=NonReplayable(NotCurrent)
      -> Terminal(AbortedByRuntimeRestart { last_generation: current })
  | ProviderSuspended when ReplayabilityClass=Replayable or
      ReplayabilityClass=NonReplayable(Current)
      -> OperationPhase::Suspended; no Terminal
  | ProviderSuspended when ReplayabilityClass=NonReplayable(NotCurrent)
      -> Terminal(AbortedByRuntimeRestart { last_generation: current })
  | RuntimeRestart
      -> Terminal(AbortedByRuntimeRestart { last_generation: current })
  | ProjectionFailure { message }
      -> Terminal(Failed { class: Persistence, message })
  | ExecutionFailed { class, message } -> Terminal(Failed { class, message })
  | Panicked { message } -> Terminal(Panicked { message })
  | NotStarted { reason: Cancelled { cause } } when
      FinalizerPhaseClass=Admitted
      -> Terminal(TerminalizationCauseTerminalMapping(cause))
  | NotStarted { reason: Cancelled { cause } } when
      FinalizerPhaseClass=SuspendedResumeStarting(current_generation)
      -> Finalizing(Terminalization(cause))
  | NotStarted { reason: Interrupted } when FinalizerPhaseClass=Admitted and
      (ReplayabilityClass=Replayable or ReplayabilityClass=NonReplayable(Current))
      -> Suspended(Interrupted(current_generation))
  | NotStarted { reason: Interrupted } when
      FinalizerPhaseClass=SuspendedResumeStarting(current_generation) and
      (ReplayabilityClass=Replayable or ReplayabilityClass=NonReplayable(Current))
      -> Suspended(SuspensionRebasedAfterUnstartedResume(
           Interrupted(current_generation)))
  | NotStarted { reason: Interrupted } when
      ReplayabilityClass=NonReplayable(NotCurrent) and
      FinalizerPhaseClass=SuspendedResumeStarting(current_generation)
      -> Finalizing(RecoveryAbortNonReplayable {
           last_generation: current_generation,
         })
  | NotStarted { reason: Interrupted } when
      ReplayabilityClass=NonReplayable(NotCurrent) and
      FinalizerPhaseClass=Admitted
      -> Terminal(AbortedByRuntimeRestart { last_generation: current_generation })
  | NotStarted { reason: RuntimeRestart } when
      FinalizerPhaseClass=Admitted and ReplayabilityClass=Replayable
      -> Suspended(RecoveryRequired(current_generation))
  | NotStarted { reason: RuntimeRestart } when
      FinalizerPhaseClass=Admitted and
      ReplayabilityClass=NonReplayable(Current)
      -> Suspended(RecoveryRequired(current_generation))
  | NotStarted { reason: RuntimeRestart } when
      FinalizerPhaseClass=SuspendedResumeStarting(current_generation) and
      ReplayabilityClass=Replayable
      -> Suspended(SuspensionRebasedAfterUnstartedResume(
           RecoveryRequired(current_generation)))
  | NotStarted { reason: RuntimeRestart } when
      FinalizerPhaseClass=SuspendedResumeStarting(current_generation) and
      ReplayabilityClass=NonReplayable(Current)
      -> Suspended(SuspensionRebasedAfterUnstartedResume(
           RecoveryRequired(current_generation)))
  | NotStarted { reason: RuntimeRestart } when
      ReplayabilityClass=NonReplayable(NotCurrent) and
      FinalizerPhaseClass=SuspendedResumeStarting(current_generation)
      -> Finalizing(RecoveryAbortNonReplayable {
           last_generation: current_generation,
         })
  | NotStarted { reason: RuntimeRestart } when
      ReplayabilityClass=NonReplayable(NotCurrent) and
      FinalizerPhaseClass=Admitted
      -> Terminal(AbortedByRuntimeRestart { last_generation: current_generation })
  | NotStarted { reason: ReservationExpired }
      -> Terminal(NotAdmitted { reason: ReservationExpired }) when no generation has Started
  | NotStarted { reason: AdmissionRejected { reason } }
      -> Terminal(NotAdmitted {
           reason: AdmissionRejectionTerminalMapping(reason),
         }) when no generation has Started
  | NotStarted { reason: StartCommitFailure { message } } when
      FinalizerPhaseClass=Admitted
      -> Terminal(Failed { class: Persistence, message })
  | NotStarted { reason: StartCommitFailure { message } } when
      FinalizerPhaseClass=SuspendedResumeStarting(current_generation)
      -> Finalizing(ResumeStartCommitFailure { message })
  | NotStarted { reason: MissingLiveInputCapsule } when
      FinalizerPhaseClass=Admitted
      -> Terminal(Failed {
           class: RuntimeInvariant,
           message: "non-replayable operation input capsule is unavailable before generation start",
         })
  | NotStarted { reason: MissingLiveInputCapsule } when
      FinalizerPhaseClass=SuspendedResumeStarting(current_generation)
      -> Finalizing(RecoveryAbortNonReplayable {
           last_generation: current_generation,
         })
  | NotStarted { reason: Shutdown { reason } } when
      FinalizerPhaseClass=Admitted
      -> Terminal(Shutdown { reason })
  | NotStarted { reason: Shutdown { reason } } when
      FinalizerPhaseClass=SuspendedResumeStarting(current_generation)
      -> Finalizing(Terminalization(reason))

ReservationFinalizerDisposition =
  ReservationFinalizerSource { reason: ReservationExpired }
    -> Terminal(NotAdmitted { reason: ReservationExpired }) without a GenerationStopped patch
  | ReservationFinalizerSource { reason: AdmissionRejected { reason } }
    -> Terminal(NotAdmitted {
         reason: AdmissionRejectionTerminalMapping(reason),
       }) without a GenerationStopped patch
  | ReservationFinalizerSource { reason: CancelledBeforeAdmission }
    -> Terminal(NotAdmitted { reason: CancelledBeforeAdmission }) without a GenerationStopped patch
  | ReservationFinalizerSource { reason: RuntimeRestart }
    -> Terminal(NotAdmitted { reason: RuntimeRestart }) without a GenerationStopped patch
  | ReservationFinalizerSource { reason: HostShutdown }
    -> Terminal(NotAdmitted { reason: HostShutdown }) without a GenerationStopped patch
  | ReservationFinalizerSource { reason: ThreadClose }
    -> Terminal(NotAdmitted { reason: ThreadClose }) without a GenerationStopped patch

SuspendedFinalizationTerminalMapping =
  ResumeStartCommitFailure { message }
    -> Terminal(Failed { class: Persistence, message })
  | RecoveryAbortNonReplayable { last_generation }
    -> Terminal(AbortedByRuntimeRestart { last_generation })
  | Terminalization(UserCancel) -> Terminal(Cancelled { reason: User })
  | Terminalization(GoalPause) -> Terminal(Cancelled { reason: GoalPause })
  | Terminalization(HostShutdown) -> Terminal(Shutdown { reason: HostShutdown })
  | Terminalization(ThreadClose) -> Terminal(Shutdown { reason: ThreadClose })

TerminalizationCauseTerminalMapping =
  UserCancel -> Cancelled { reason: User }
  | GoalPause -> Cancelled { reason: GoalPause }
  | HostShutdown -> Shutdown { reason: HostShutdown }
  | ThreadClose -> Shutdown { reason: ThreadClose }

MaterializationCause =
  SameProcessProjectionReset { retained_incarnation: SurfaceIncarnation }
  | ColdOwnerTakeover {
      new_incarnation: SurfaceIncarnation,
      new_owner_epoch: ThreadOwnerEpoch,
    }

PostMaterializationRecoveryDisposition =
  Requested
    -> AppendReservationTerminal(NotAdmitted(RuntimeRestart))
  | Admitted { latest: Reserved, replayability: Replayable }
    -> AppendStopped(NotStarted(RuntimeRestart)) then
       AppendSuspended(RecoveryRequired(latest.generation_id))
  | Admitted {
      latest: Reserved,
      replayability: NonReplayable(Current),
      materialization: SameProcessProjectionReset,
    }
    -> AppendStopped(NotStarted(RuntimeRestart)) then
       AppendSuspended(RecoveryRequired(latest.generation_id))
  | Admitted { latest: Reserved, replayability: NonReplayable(NotCurrent) }
    -> AppendStopped(NotStarted(RuntimeRestart)) then
       AppendFinalizing(RecoveryAbortNonReplayable {
         last_generation: latest.generation_id,
       })
  | Admitted {
      latest: Started | Transferred,
      unavailable_interaction:
        Expired | Cancelled(CapabilityUnavailable | ExpiryAuthorityUnavailable),
      matching_operation_and_generation_fence: true,
      generation_stopped_or_finalization_started: false,
    }
    -> AppendStopped(ExecutionFailed(ClientCapabilityUnavailable)) then
       AppendFinalizing(GenerationStop(same reason))
  | Admitted {
      latest: Started | Transferred,
      matching_terminal_unavailable_interaction: false,
    }
    -> AppendStopped(RuntimeRestart) then
       AppendFinalizing(GenerationStop(RuntimeRestart))
  | Suspended { replayability: Replayable }
    -> ExposeRecoveryRequired; append no historical transition
  | Suspended {
      replayability: NonReplayable(Current),
      materialization: SameProcessProjectionReset,
    }
    -> ExposeRecoveryRequired; append no historical transition
  | Suspended { replayability: NonReplayable(NotCurrent), last_generation }
    -> AppendFinalizing(RecoveryAbortNonReplayable { last_generation })
  | SuspendedResumeStarting { replacement: Reserved, replayability: Replayable }
    -> AppendStopped(NotStarted(RuntimeRestart)) then
       AppendSuspensionRebase(RecoveryRequired(replacement.generation_id))
  | SuspendedResumeStarting {
      replacement: Reserved,
      replayability: NonReplayable(Current),
      materialization: SameProcessProjectionReset,
    }
    -> AppendStopped(NotStarted(RuntimeRestart)) then
       AppendSuspensionRebase(RecoveryRequired(replacement.generation_id))
  | SuspendedResumeStarting {
      replacement: Reserved,
      replayability: NonReplayable(NotCurrent),
    }
    -> AppendStopped(NotStarted(RuntimeRestart)) then
       AppendFinalizing(RecoveryAbortNonReplayable {
         last_generation: replacement.generation_id,
       })
  | Finalizing
    -> ReconcileOriginalFinalizer
  | FinalizingDegraded
    -> ExposeOriginalFinalizationOrProjectionRepair
  | Terminal
    -> NoOp
```

`NonReplayable(Current)` is constructible only for
`SameProcessProjectionReset` with the exact retained incarnation. Cold load or
owner takeover always classifies a non-replayable capsule as `NotCurrent`; an
attempt to construct the Current row is `IllegalTransition`. Requested has no
generation; every valid Admitted record has exactly one latest generation in
Reserved, Started, or Transferred phase before this table runs. A Stopped latest
generation without an atomically paired Suspended, successor Reserved, or
FinalizationStarted disposition is invalid durable history and fails
materialization rather than selecting an implicit recovery branch.

`LiveGenerationStopDisposition` runs only in the owner actor when it handles a
new stop. A Goal `Admitted` decision is legal only for
`Completed(Success)` and commits the stop, exact Goal outer-turn settlement,
decision, and successor reservation in one coordinator batch; the operation
remains Admitted. Every non-successful Goal stop requires
`GoalContinuationDecision::Stopped`. Every other stop plus its selected
Suspended, suspension-rebase, or FinalizationStarted patch is likewise one
coordinator batch, so no crash can leave an Admitted operation with a stopped
generation and no successor/phase disposition. A stopped Goal decision's same
batch also contains the exact outer-turn settlement,
`ContinuationDecided(Stopped)`, and final Goal state/store receipt.
`ReservationFinalizerDisposition` is selected only from
`OperationFinalizerSource::Reservation` and never fabricates a
GenerationStopped patch. The pure reducer and materializer never classify
live-capsule status, consult the current incarnation, or terminalize while
replaying historical stop records; they replay the explicit patches already in
the log byte-for-byte. Only after one complete snapshot is materialized may
`PostMaterializationRecoveryDisposition` inspect current process-local capsule
availability and append a new fenced recovery batch. Replaying the same durable
history therefore has the same result in every incarnation.
The terminal-unavailable Started/Transferred row is evaluated before the generic
runtime-restart row. It is constructible only when the durable interaction's
operation and generation fence match the latest generation and neither
GenerationStopped nor FinalizationStarted exists. The generic row explicitly
excludes that witness. This makes a crash between the interaction and post-join
barriers resume the already-selected unavailable cause without rerunning work or
reclassifying it as RuntimeRestart.

The replayable/current-incarnation-live forms of `InterruptedResumable` and
`ProviderSuspended`, plus the explicitly conditioned replayable/live
`NotStarted(Interrupted|RuntimeRestart)` rows and an atomically admitted Goal
successor, are the only stopped results that deliberately do not enter
finalization. An initial Reserved attempt creates
Suspended; a failed unstarted resume uses the one suspension-rebase transition
and clears ResumeStarting. Every non-replayable resume-replacement failure uses
`SuspendedFinalizationTerminalMapping`, so it cannot append a Terminal directly
from Suspended. Every other row has exactly one terminalizer path. A
`NotAdmitted` terminal can therefore be produced only while no generation has
Started; an admitted-but-Reserved generation may still settle as
`NotAdmitted` without any external side effect. A finalizer must reject that
terminal after `GenerationStarted`. `ReservationFinalizerSource` is a
host/operation reservation path, not a `GenerationStopReason`, so it never fabricates a
generation record. `InvalidInput`, the JSONL `OperationActive` busy rejection,
and reservation-capacity rejection
before Requested are `Uncommitted` ingress/admission errors and are not operation
terminals. `OperationJoinSettlementSource` is the sole operation-level source
for `JoinFailed`; it carries the fixed finalizer message and cannot be emitted by
an adapter. It enters the finalizer only through
`OperationFinalizerSource::OperationJoinSettlement` and the one
`JoinFailedTerminalMapping`; neither a generation stop nor reservation source
can produce `JoinFailed`. `LiveGenerationStopDisposition`,
`ReservationFinalizerDisposition`, and `JoinFailedTerminalMapping` are the three
disjoint exhaustive mappings for `OperationFinalizerSource`; none can consume
another source variant.
The join source's operation id and finalize intent must match the finalizer
permit, and the receipt digest for its settlement id must be present in the
terminal record before the mapping is legal.

`final_operation_usage` is the operation record's aggregate after applying the
stopping generation's `usage_delta`; the same value is copied into
`OperationTerminal::Succeeded.usage` and `OperationTerminalRecord.usage`.
Failed, Panicked, and JoinFailed terminal records carry
`source_diagnostic_digest=Some(SHA256(sanitized_message))`; every other terminal
uses None. All terminal-producing source constructors sanitize and bound their
diagnostic before the batch preflight, so a terminal/finalizer batch cannot be
made oversized by provider/tool/panic text.
Every `GenerationExecutionFailureClass` lifts to the identically named
`FailureClass`. Verification and persistence cannot use that generic source:
they arise only from `VerificationFailed`, `ProjectionFailure`, or
`StartCommitFailure`, each of which supplies the exact terminal message. This
makes every stop source disjoint and every terminal payload constructible.

`LiveOperationCapsule` is process-local evidence only. Its durable descriptor is
not rewritten: `Available` with an incarnation different from the current one
is evaluated exactly like `Unavailable` after restart. Recovery of
a non-replayable Reserved generation first commits
`Stopped(NotStarted(RuntimeRestart))`; the closed mapping above then finalizes
`AbortedByRuntimeRestart`. A previously resumable stopped generation with no
durable replay capsule follows the same terminal row. Thus no non-replayable
operation can remain stranded in `Suspended` after rematerialization.

`ModelTokens` is used only for an actual provider token/length limit. The main
and child 128-turn ceilings map to `TurnRequests`; Goal charged-token limits map
to `GoalTokenBudget`; workflow agent limits map to `WorkflowTokenBudget`; USD
guards map to `MonetaryBudgetUsdMicros`. No adapter may collapse these back to a
generic budget string.

## Snapshot, Attach, Replay, And Wait DTOs

```text
SurfaceSnapshot {
  cursor: SurfaceCursor,
  thread: SurfaceThreadSnapshot,
  foreground_operation: Option<OperationRecord>,
  queued_operations: Vec<OperationRecord>,
  background_operations: Vec<SurfaceBackgroundOperation>,
  operation_history: Vec<OperationRecord>,
  items: Vec<SurfaceItem>,
  assistant_streams: Vec<SurfaceAssistantStream>,
  tools: Vec<SurfaceToolView>,
  plan: SurfacePlanSnapshot,
  usage: SurfaceUsageSnapshot,
  context: SurfaceContextSnapshot,
  interactions: Vec<SurfaceInteractionView>,
  tasks: Vec<SurfaceTask>,
  workflows: Vec<SurfaceWorkflow>,
  subagents: Vec<SurfaceSubagent>,
  goal: Option<SurfaceGoal>,
  settings: SurfaceSettingsSnapshot,
  mcp_catalog: SurfaceMcpCatalogSnapshot,
  pinned_context: SurfacePinnedContextSnapshot,
  session_health: SurfaceSessionHealth,
}

SnapshotAtCursor {
  snapshot: Arc<SurfaceSnapshot>,
  cursor: SurfaceCursor, // exactly snapshot.cursor
}

SurfaceAttachmentCapabilities {
  grant: SurfaceAttachmentGrant,
  interaction_kinds: Set<SurfaceInteractionKind>,
  acp_capability_revision: Option<CapabilityRevision>,
}

SurfaceAttachAuthority {
  host_incarnation: HostIncarnation,
  thread_id: SurfaceThreadId,
  role: Tui | Acp | Jsonl | InternalCompatibility,
  maximum_capabilities: NonEmptySet<SurfaceCapability>,
  required_capabilities: NonEmptySet<SurfaceCapability>,
  maximum_interaction_kinds: Set<SurfaceInteractionKind>,
}

AttachDeniedReason = RoleMismatch | MissingRequiredCapability

SurfaceSubscriptionHandle(
  opaque Stream<SurfaceSubscriptionItem>, process-local, non-serializable
)

FreshSurfaceAttachment {
  attachment_id: SurfaceAttachmentId,
  client: RuntimeSurfaceClientHandle,
  baseline: SnapshotAtCursor,
  subscription: SurfaceSubscriptionHandle,
  capabilities: SurfaceAttachmentCapabilities,
}

CursorSurfaceAttachment {
  attachment_id: SurfaceAttachmentId,
  client: RuntimeSurfaceClientHandle,
  from: SurfaceCursor,
  head: SurfaceCursor,
  replay: Vec<SurfaceCommitBatch>,
  subscription: SurfaceSubscriptionHandle,
  capabilities: SurfaceAttachmentCapabilities,
}

RuntimeSurfaceClientHandle(
  opaque, cloneable, process-local, bound to exactly one attachment id,
  capability grant, thread id, host incarnation, and optional connection id
)
```

Only `RuntimeSurfaceClientHandle` can enqueue an attachment-authorized
`SurfaceCommand`. It injects `SurfaceBoundCaller`; neither an adapter nor a
command payload can construct, deserialize, replace, or nominate an attachment
identity. The unbound host/thread facade can attach and read public snapshots,
but cannot submit an attachment-authorized command.

```text
FreshAttachRequest {
  request_id: SurfaceRequestId,
  role: Tui | Acp | Jsonl | InternalCompatibility,
  requested_capabilities: Set<SurfaceCapability>,
  interaction_capabilities: Set<SurfaceInteractionKind>,
}

CursorAttachRequest {
  request_id: SurfaceRequestId,
  cursor: SurfaceCursor,
  role: Tui | Acp | Jsonl | InternalCompatibility,
  requested_capabilities: Set<SurfaceCapability>,
  interaction_capabilities: Set<SurfaceInteractionKind>,
}

AttachResult =
  FreshAttached { attachment: FreshSurfaceAttachment }
  | CursorAttached { attachment: CursorSurfaceAttachment }
  | Denied { reason: AttachDeniedReason }
  | SnapshotRequired { required: SnapshotRequired }
  | InvalidCursor { error: InvalidCursor }
  | ThreadClosed { thread_id: SurfaceThreadId }
  | Unavailable { reason: SurfaceUnavailableReason }

SnapshotRequiredReason =
  StaleIncarnation
  | ExpiredSuffix
  | ReplayHole
  | SlowSubscriber
  | ProjectionReset

SnapshotRequired {
  reason: SnapshotRequiredReason,
  retained_from: Option<SurfaceCursor>,
  head: SurfaceCursor,
}

InvalidCursorReason = WrongThread | FutureSequence | ImpossibleSourceRevision
                      | NotBatchBoundary

InvalidCursor {
  reason: InvalidCursorReason,
  supplied: SurfaceCursor,
  expected_thread: SurfaceThreadId,
  head: SurfaceCursor,
}
```

`FreshAttachRequest` may return only `FreshAttached` or an error branch;
`CursorAttachRequest` may return only `CursorAttached` or an error branch. A
cursor attachment's `head` is the exclusive end of its replay, not a runtime
snapshot. The caller's state at the supplied cursor plus every replay event is
the state at `head`; returning a current snapshot on this branch would duplicate
facts and is forbidden.
For both variants, the outer `attachment_id`, the id bound inside
`RuntimeSurfaceClientHandle`, and `capabilities.grant.attachment_id` are exactly
equal. The `RuntimeSurfaceHandle` facade is bound to one host-issued
`SurfaceAttachAuthority`; request role is required to equal its role, and the
returned capability/interaction sets are exactly the intersection of the
request, authority maxima, and current host/thread policy. The result must still
contain every authority-required capability, including ReadSnapshot, or attach
returns Denied and registers nothing. Request fields never widen the ceiling.
Fresh requires `grant.granted_at == baseline.cursor`. Cursor requires
`from == CursorAttachRequest.cursor`, `grant.granted_at == head`, and replay
cursors across complete batches contiguous from `from` to `head` (an empty replay requires
`from == head`). The subscription begins at `head`; any mismatched id, grant
cursor, or replay boundary is an internal construction error and no attachment
is registered.
Subscriptions and cursor replay deliver only complete `SurfaceCommitBatch`
values. A client applies every event in a batch atomically before rendering its
batch head; no per-envelope subscription API exposes a partial coordinator
commit.

```text
SurfaceSubscriptionItem =
  Batch { batch: SurfaceCommitBatch }
  | Gap { required: SnapshotRequired }
  | Sealed { reason: ThreadClosed | HostShutdown }

DetachRequest {
  request_id: SurfaceRequestId,
}

DetachRevocationReceipt {
  request_id: SurfaceRequestId,
  attachment_id: SurfaceAttachmentId,
  revoked_grant_digest: Sha256Digest,
  affected_route_epochs: Vec<(SurfaceInteractionId, ResponseRouteEpoch)>,
  route_commit_id: Option<SurfaceCommitId>,
  route_cursor: Option<SurfaceCursor>,
}

DetachResult =
  Detached { receipt: DetachRevocationReceipt }
  | AlreadyDetached { receipt: DetachRevocationReceipt }
  | Deferred {
      receipt: DetachRevocationReceipt,
      mutation: DeferredMutation,
    }
  | StaleAttachment { request_id: SurfaceRequestId, attachment_id: SurfaceAttachmentId }

WaitOperationTerminalRequest {
  request_id: SurfaceRequestId,
  operation_id: SurfaceOperationId,
  caller_cancel: OptionalProcessLocalCancel,
}

OperationTerminalAtCursor {
  operation_id: SurfaceOperationId,
  terminal: OperationTerminal,
  cursor: SurfaceCursor,
  commit_class: CommitClass,
  batch_digest: Sha256Digest,
}

WaitOperationTerminalResult =
  Terminal { value: OperationTerminalAtCursor }
  | TerminalCommitFailure {
      operation_id: SurfaceOperationId,
      finalize_intent_id: SurfaceFinalizeIntentId,
      commit_id: SurfaceCommitId,
      repair: RetryFinalizationToken,
    }
  | TerminalProjectionFailure {
      operation_id: SurfaceOperationId,
      finalize_intent_id: SurfaceFinalizeIntentId,
      terminal_commit_id: SurfaceCommitId,
      terminal_event_id: SurfaceEventId,
      repair: RetryProjectionToken,
    }
  | UnknownOperation { operation_id: SurfaceOperationId }
  | WrongThread { operation_id: SurfaceOperationId }
  | WaitCancelled { operation_id: SurfaceOperationId }
```

Detach first CASes the attachment and all of its response grants to revoked.
Only the CAS winner may compute affected interaction routes; it rotates each
route epoch/grant set and commits those route patches before success. The
receipt has `route_commit_id=None`, `route_cursor=None`, and an empty affected
set together, or all three describe the exact containing route batch. Projection
failure after durable route commit returns Deferred with the grant already
revoked and the exact projection repair; it cannot return Detached early or
restore the old grant. Detached requires the route cursor barrier when routes
changed. AlreadyDetached replays the byte-identical receipt. Detach never
cancels an operation, resolves an interaction, or claims a replacement route.

A projection reset is represented only by
`Gap(SnapshotRequired { reason: ProjectionReset, .. })`, after which that lane
closes. `Sealed` is reserved for actual thread/host closure and is observed only
after every already-admitted complete batch. The same reset can never be encoded
as either a gap or a terminal seal at adapter discretion.

Cancelling a waiter never cancels its operation. A waiter reconstructed after
restart reads the durable operation ledger. It returns the existing Terminal,
the existing finalization or terminal-projection repair token, or
`UnknownOperation`; it never creates
another completion rail. Thread close and host shutdown cannot complete while
an owned operation lacks Terminal; either typed failure keeps the close barrier
deferred. A close
attempt with an unresolved terminal barrier is `Deferred`, not a terminal-less
wait result.

## Event Envelope And Reducer Contract

```text
SurfaceEventEnvelope {
  ordinal: u32,
  event_id: SurfaceEventId,
  commit_class: CommitClass,
  scope: SurfaceScope,
  event: SurfaceEvent,
}

SurfaceCommitBatch {
  cursor_before: SurfaceCursor,
  cursor_after: SurfaceCursor,
  commit_class: CommitClass,
  event_count: u32,
  batch_digest: Sha256Digest,
  events: NonEmptyVec<SurfaceEventEnvelope>,
}

SurfaceCommitBatchPreflightResult =
  Ready {
    event_count: u32,
    canonical_encoded_bytes: u64,
    batch_digest: Sha256Digest,
  }
  | Rejected {
      code: CommitBatchTooLarge,
      observed_event_count: u64,
      observed_canonical_encoded_bytes: u64,
      event_limit: 1_024,
      byte_limit: 8_388_608,
    }

AppliedTransitionRecord {
  event_id: SurfaceEventId,
  commit_id: SurfaceCommitId,
  event_digest: Sha256Digest,
  batch_cursor_after: SurfaceCursor,
}

SurfaceAppliedTransitionIndex(
  runtime-private exact index keyed by event_id + commit_id
)

AppliedBatchRecord {
  commit_class: CommitClass,
  event_count: u32,
  batch_digest: Sha256Digest,
  cursor_before: SurfaceCursor,
  cursor_after: SurfaceCursor,
}

SurfaceAppliedBatchIndex(
  runtime-private exact index keyed by commit_id
)

SurfaceReducerState {
  snapshot: SurfaceSnapshot,
  applied: SurfaceAppliedTransitionIndex,
  applied_batches: SurfaceAppliedBatchIndex,
}

SurfaceEvent =
  Operation(OperationPatch)
  | Item(ItemPatch)
  | Assistant(AssistantPatch)
  | Tool(ToolPatch)
  | Plan(SurfacePlanSnapshot)
  | Usage(SurfaceUsageSnapshot)
  | Context(SurfaceContextSnapshot)
  | Interaction(InteractionPatch)
  | Task(TaskPatch)
  | Workflow(WorkflowPatch)
  | Subagent(SubagentPatch)
  | Goal(GoalPatchEnvelope)
  | Settings(SettingsPatch)
  | McpCatalog(McpCatalogPatch)
  | PinnedContext(PinnedContextPatch)
  | Session(SessionPatch)
```

`commit_class.commit_id` is the batch identity used by append reconciliation.
Every envelope in one batch has the batch's complete identical `CommitClass`
(including owner/incarnation and source revision), ordinals are exactly
`0..event_count-1`, `event_count == events.len()`, and the event family/nested
patch discriminant is the closed `transition_kind`; operation/generation
identity agrees with scope and embedded fences. The batch boundary cursors span
exactly `event_count` event sequences and no intermediate cursor is public. The
applied index stores the exact canonical
event digest, not merely the cursor, and is runtime-private rather than part of
a client snapshot. `event_digest` is the versioned canonical digest of `(scope,
event family, nested discriminant, complete event payload)`. It excludes cursors
and `CommitClass`, so rematerialization may assign current live cursors while a
scope or payload reclassification still conflicts. `batch_digest` is the
versioned canonical digest of the ordered event digests, event count, and
complete CommitClass; changing order, membership, or any commit-class field is
a different batch.

The v1 canonical batch encoder covers both boundary cursors, complete
CommitClass, event count, batch digest, and every ordered envelope. Its exact
encoded byte length is the byte-budget value. `preflight_commit_batch` computes
that value and the canonical digest before any coordinator WAL prepare,
authoritative store/in-memory mutation, receipt, cursor advance, or reducer call.
More than 1,024 events or 8,388,608 encoded bytes returns the sole
`Rejected(CommitBatchTooLarge)` branch with no fact or receipt. Streaming deltas
and progress are split into independently meaningful batches before preflight;
an indivisible attempted fact remains uncommitted. An asynchronous generation or
background publisher then enters the ordinary bounded runtime-failure/finalizer
path as a separate commit. Terminal/finalizer constructors use a fixed bounded
diagnostic plus a private digest rather than retrying an oversized payload.

The reducer is a total function:

```text
SurfaceReduceResult =
  Applied { state: SurfaceReducerState }
  | AlreadyApplied { cursor: SurfaceCursor, commit_id: SurfaceCommitId }
  | Rejected { error: SurfaceReducerError }

SurfaceReduceMode = Live | Rematerialization

reduce_batch(mode: SurfaceReduceMode,
       state: &SurfaceReducerState,
       batch: &SurfaceCommitBatch)
  -> SurfaceReduceResult

SurfaceReducerErrorCode =
  CursorMismatch
  | ScopeMismatch
  | CommitClassMismatch
  | StaleRevision
  | IllegalTransition
  | MissingIdentity
  | DuplicateTransition
  | InvalidOrdering
  | PartialBatchReplay
  | GoalReceiptMismatch

SurfaceReducerErrorLocation =
  Batch { commit_id: SurfaceCommitId }
  | Event { event_id: SurfaceEventId, ordinal: u32 }

SurfaceReducerError {
  code: SurfaceReducerErrorCode,
  location: SurfaceReducerErrorLocation,
  message: DisplayText,
}
```

It preflights the complete batch and rejects a cursor mismatch, scope mismatch, stale revision, illegal state
transition, missing referenced identity, duplicate non-idempotent transition,
or invalid ordering before mutating any field. Cross-event invariants are
evaluated against the batch's final candidate state, so no intermediate partial
snapshot is published. Reapplying a batch returns `AlreadyApplied` only in
`Rematerialization` mode and only when its exact `AppliedBatchRecord` matches
and every ordinal/event record is present with the same digest. A changed
CommitClass is `CommitClassMismatch`; changed count, digest, boundary, order, or
membership is `DuplicateTransition`; a batch record without every event record,
or event records without the batch record, is `PartialBatchReplay`. `Live`
always rejects an already-applied batch. The coordinator WAL stores
`{ commit_id, event_count, batch_digest, state: Prepared | Committed }`; only a
synced `Committed` marker may materialize or fan out the batch. A short-write
prefix remains an incomplete prepared commit and cannot be reinterpreted as a
smaller valid batch. A one-event mutation is represented by
a batch of length one; there is no public unary commit/reduce path.
Batch-boundary, count, digest, commit-class, and partial-replay failures use the
`Batch` location because no single event owns them. A field, scope, identity, or
transition failure attributable to one envelope uses `Event` with its exact
ordinal. A reducer error never fabricates an event id for a batch-level failure.

## Operation Patches

```text
OperationPatch =
  Requested {
    operation: OperationRecord,
  }
  | ReservationQueueChanged {
      operation_id: SurfaceOperationId,
      reservation_sequence: SequenceNumber,
      ready_for_admission: bool,
      queue_position: u32,
    }
  | Admitted {
      operation_id: SurfaceOperationId,
      logical_turn_id: SurfaceTurnId,
      input: AdmittedInput,
      first_generation: GenerationRecord,
    }
  | InputBindingsResolved {
      fence: SurfaceOperationFence,
      input_item_id: SurfaceItemId,
      fact: SurfaceResolvedInputFact,
    }
  | InputBindingsFailed {
      fence: SurfaceOperationFence,
      input_item_id: SurfaceItemId,
      code: InputResolutionErrorCode,
      message: SafeDiagnosticText,
    }
  | ControlIntentCommitted {
      operation_id: SurfaceOperationId,
      request_id: SurfaceRequestId,
      intent: PendingControlIntent,
    }
  | GenerationReserved { generation: GenerationRecord }
  | GenerationStarted {
      fence: SurfaceOperationFence,
      witness: GenerationStartedWitness,
    }
  | AgentLoopTurnStarted { turn: SurfaceAgentLoopTurn }
  | ModelRouteSelected {
      fence: SurfaceOperationFence,
      requested_model: NonEmptyText,
      actual_model: NonEmptyText,
      reason: NonEmptyText,
    }
  | VerificationStarted {
      fence: SurfaceOperationFence,
      verification_id: UuidV7,
      command: NonEmptyText,
    }
  | VerificationCompleted {
      fence: SurfaceOperationFence,
      verification_id: UuidV7,
      result: SurfaceVerificationResult,
    }
  | GenerationStopped {
      fence: SurfaceOperationFence,
      reason: GenerationStopReason,
      usage_delta: UsageTotals,
    }
  | GenerationTransferred {
      fence: SurfaceOperationFence,
      background_fence: SurfaceBackgroundFence,
      task_id: Option<SurfaceTaskId>,
    }
  | Suspended {
      operation_id: SurfaceOperationId,
      cause: SuspensionCause,
    }
  | SuspensionRebasedAfterUnstartedResume {
      operation_id: SurfaceOperationId,
      previous_cause: SuspensionCause,
      replacement_fence: SurfaceOperationFence,
      rebased_cause: SuspensionCause,
    }
  | RecoveryRequired {
      operation_id: SurfaceOperationId,
      last_generation: SurfaceGenerationId,
    }
  | FinalizationStarted {
      operation_id: SurfaceOperationId,
      finalize_intent_id: SurfaceFinalizeIntentId,
      terminal_commit_id: SurfaceCommitId,
      selected_cause: OperationFinalizationCause,
      suspended_cause: Option<SuspendedFinalizationCause>,
      expected_settlements: Vec<SurfaceSettlementId>,
    }
  | FinalizationSettlementRecorded {
      operation_id: SurfaceOperationId,
      finalize_intent_id: SurfaceFinalizeIntentId,
      receipt: SurfaceSettlementReceipt,
    }
  | FinalizationDegraded {
      operation_id: SurfaceOperationId,
      finalize_intent_id: SurfaceFinalizeIntentId,
      cause: FinalizationDegradedCause,
      last_error: DisplayText,
    }
  | Terminal { record: OperationTerminalRecord }
```

Reducer effects are exact:

- `Requested` inserts one operation into the reservation FIFO.
- `ReservationQueueChanged` changes only queue projection fields.
- `Admitted` removes the reservation from the FIFO, fixes turn/input identity,
  installs the foreground operation, and inserts its Reserved generation.
  `PendingUser` additionally requires exactly one `ItemPatch::Added` for the
  same item id, presentation, correlation id, and turn in the same coordinator
  batch. `NotApplicable` forbids an Item patch and sets
  `GenerationInputState::NotApplicable`.
- `GenerationReserved` for a new Goal outer turn carries that generation's own
  `Pending` input and replayability capsule and is paired with exactly one new
  pending UserMessage. A generic recovery replacement copies the predecessor's
  exact replayability capsule and `Pending`, `Resolved`, or `NotApplicable`
  input state, emits no second Item transition, and cannot copy `Failed`.
- `InputBindingsResolved` changes the exact generation input-resolution gate and must
  be paired in the same coordinator batch with `ItemPatch::InputResolved` for
  the same exact generation/item and identical `SurfaceResolvedInputFact`.
  `InputBindingsFailed` is paired with `ItemPatch::InputResolutionFailed` and
  the exact item id, failure code, and byte-identical `SafeDiagnosticText`;
  it also requires in that same atomic batch
  `GenerationStopped { fence, reason: ExecutionFailed {
  class: InputResolution, message: same }, usage_delta: zero }` and the matching
  `FinalizationStarted` intent. Neither
  resolution transition may run before GenerationStarted. The input-failure
  batch is therefore crash-complete and always finalizes as
  `Failed(InputResolution)`, never as a later runtime-restart abort.
- generation patches mutate only their matching generation and the derived
  foreground/background view.
- `AgentLoopTurnStarted` appends exactly the next ordinal under an already
  Started matching generation whose own input gate is Resolved (or NotApplicable).
  It is not the generation side-effect barrier. No provider/model/tool/hook
  side effect may start while the gate is Pending or Failed.
- `GenerationTransferred` inserts the background view and releases foreground
  only at its containing batch's `cursor_after`.
- `Suspended` and `RecoveryRequired` retain foreground ownership. A
  `SuspensionRebasedAfterUnstartedResume` transition is legal only when a
  replacement generation was Reserved under `ResumeStarting`, then stopped as
  `NotStarted(Interrupted|RuntimeRestart)`. It atomically clears ResumeStarting
  and moves the suspension witness from `previous_cause` to `rebased_cause` on
  that exact `replacement_fence`; both causes name the corresponding stopped
  generations and neither may change operation, turn, or input identity.
- `FinalizationStarted` creates one `OperationFinalizationRecord` with a fixed
  terminal commit id, current-incarnation `FinalizationStartedAtCursor`, and
  immutable `selected_cause` plus unique expected settlement ids. The selected
  cause must equal the closed source-to-terminal mapping and can never be
  replaced by cancel, close, shutdown, or repair. `suspended_cause` is present exactly when the source
  phase is Suspended and fixes the only legal Suspended-to-Finalizing mapping; it
  is absent for all other source phases. `FinalizationSettlementRecorded` accepts exactly one
  expected id and rejects a second digest for it; exact replay is idempotent.
  `FinalizationDegradedCause::MissingFinalization.missing_settlements` equals the
  nonempty ordered set difference `expected - settled` and is repaired only by
  `RetryFinalization`. `TerminalProjectionPending` proves that the exact
  Terminal is already durable and is repaired only by `RetryProjection`; it
  cannot settle another store or append a second Terminal. Terminal is legal
  only when the settled ids equal the complete expected set and its typed receipt
  vector is in expected order with byte-identical digests.
- `Terminal` removes only the matching reservation, foreground, or background
owner selected by its finalizer mode, retains the terminal in operation
history, and updates admission availability at the containing batch's
`cursor_after`.

No operation patch may create or mutate Goal, task, workflow, interaction, or
settings facts without the corresponding typed patch in the same coordinator
batch.

## Item And Assistant Patches

```text
SurfaceItemOrigin =
  UserInput
  | GoalContinuation
  | WorkflowNotification
  | RuntimeContext
  | ProviderResponse
  | ToolResult
  | HistoryMaterialization

SurfaceUserInputState =
  Pending {
    presentation: SurfaceInputPresentation,
    correlation_id: SurfaceInputCorrelationId,
  }
  | Resolved { fact: SurfaceResolvedInputFact }
  | ResolutionFailed {
      presentation: SurfaceInputPresentation,
      correlation_id: SurfaceInputCorrelationId,
      code: InputResolutionErrorCode,
      message: SafeDiagnosticText,
    }

SurfaceItem =
  UserMessage {
    id: SurfaceItemId,
    turn_id: SurfaceTurnId,
    input: SurfaceUserInputState,
    pinned: bool,
    origin: SurfaceItemOrigin,
  }
  | SystemMessage {
      id: SurfaceItemId,
      content: DisplayText,
      pinned: bool,
      origin: SurfaceItemOrigin,
    }
  | AssistantMessage {
      id: SurfaceItemId,
      turn_id: SurfaceTurnId,
      text: DisplayText,
      pinned: bool,
    }
  | AssistantReasoning {
      id: SurfaceItemId,
      turn_id: SurfaceTurnId,
      summary: DisplayText,
      content: DisplayText,
      pinned: bool,
    }
  | AssistantPlan {
      id: SurfaceItemId,
      turn_id: SurfaceTurnId,
      text: DisplayText,
      pinned: bool,
    }
  | ToolResultMessage {
      id: SurfaceItemId,
      turn_id: SurfaceTurnId,
      tool_call_id: SurfaceToolCallId,
      content: DisplayText,
      terminal: SurfaceToolTerminal,
      pinned: bool,
    }

ItemRemovalReason = Compacted | Backtracked | ForkExcluded | RecoveryRepair

ItemPatch =
  Added { item: SurfaceItem }
  | InputResolved {
      item_id: SurfaceItemId,
      fact: SurfaceResolvedInputFact,
    }
  | InputResolutionFailed {
      item_id: SurfaceItemId,
      code: InputResolutionErrorCode,
      message: SafeDiagnosticText,
    }
  | Removed {
      item_id: SurfaceItemId,
      reason: ItemRemovalReason,
    }
```

Resolved conversation Items are ordered by durable conversation order, not
event arrival time. A pending UserMessage is an operation-audit projection only:
it has `NoLegacyProjection`, is excluded from conversation/context/history
materialization, and cannot feed a later model. `InputResolved` atomically
promotes it into conversation order in the same coordinator batch as the
operation resolution fact. `ResolutionFailed` remains audit-only and is omitted
from `thread/read`, turns, items, future model context, and JSONL events.
`Added` requires an absent id. `InputResolved` and `InputResolutionFailed`
require the same existing UserMessage in `Pending`; each is first-transition-
wins and every later resolution patch is illegal except exact rematerialization.
`Removed` requires an existing id and records a
closed mutation reason. Backtrack and compaction emit one ordered `Removed`
patch per removed identity in the same coordinator batch; they do not assign a
replacement item vector directly.

Pending presentation is derived by runtime and is part of every generation
input-creation batch: initial Admitted or later Goal GenerationReserved.
Replayable input with a present request uses the canonical visible
request presentation. `NonReplayable { reason: Redacted | SecretInput |
Missing, .. }` always uses `Redacted`. `HistoryDisabled` may use `Visible` only
in an Ephemeral batch whose incarnation equals the live capsule. The operation
patch and pending Item must carry byte-identical presentation/correlation
values; no adapter may supply or upgrade them. Recovery replacement copies the
predecessor presentation byte-for-byte and cannot recompute or upgrade it.

```text
AssistantChannel = Message | Reasoning | Plan

SurfaceAssistantStream {
  stream_id: SurfaceStreamId,
  fence: SurfaceOperationFence,
  turn_id: SurfaceTurnId,
  item_id: SurfaceItemId,
  channel: AssistantChannel,
  next_offset: ByteOffset,
  text: DisplayText,
  state: Open | Completed | Discarded,
}

SurfaceRawToolCall {
  id: SurfaceToolCallId,
  name: NonEmptyText,
  raw_arguments: DisplayText,
  arguments_digest: Sha256Digest,
}

SurfaceCompletedModelResponse {
  response_id: UuidV7,
  turn_id: SurfaceTurnId,
  message_item: Option<SurfaceItem::AssistantMessage>,
  reasoning_item: Option<SurfaceItem::AssistantReasoning>,
  plan_item: Option<SurfaceItem::AssistantPlan>,
  tool_calls: Vec<SurfaceRawToolCall>,
}

AssistantDiscardReason =
  GenerationCancelled
  | GenerationInterrupted
  | ProviderFailed
  | RuntimeRestart
  | ProjectionRepair

AssistantPatch =
  StreamOpened {
    stream: SurfaceAssistantStream,
  }
  | Delta {
      stream_id: SurfaceStreamId,
      offset: ByteOffset,
      text: DisplayText,
    }
  | ResponseCompleted {
      response: SurfaceCompletedModelResponse,
    }
  | StreamDiscarded {
      stream_id: SurfaceStreamId,
      reason: AssistantDiscardReason,
    }
```

`Delta.offset` MUST equal the stream's `next_offset`; the reducer appends and
advances it by the UTF-8 byte length. `ResponseCompleted` is the durable
completed-model fact. It closes matching streams and is the only constructor for
ProviderResponse AssistantMessage/Reasoning/Plan items; `ItemPatch::Added` may
not independently create those variants. Each declared tool call requires
exactly one `ToolPatch::Requested` in the same batch with the same response id,
turn, tool-call id, name, raw arguments, and digest. ResponseCompleted itself
does not create a tool view; ToolPatch::Requested is that sole constructor.
For each present completed assistant channel, any matching Open stream must use
the same response/turn/item/channel and its accumulated UTF-8 text must equal
the exact completed item field byte-for-byte. There is at most one open stream
per channel/item. A nonempty open channel with a missing or mismatched completed
item is rejected; it is never silently overwritten or closed.
Token deltas remain ephemeral. After restart, completed items rematerialize but
token-perfect open streams do not.

## Tool And Capability Patches

```text
SurfaceToolAction = Read | Write | Network | Agent | Shell

SurfaceToolRequest {
  tool_call_id: SurfaceToolCallId,
  source_response_id: Option<UuidV7>,
  turn_id: SurfaceTurnId,
  name: NonEmptyText,
  action: SurfaceToolAction,
  target: Option<DisplayText>,
  raw_arguments: DisplayText,
  arguments_digest: Sha256Digest,
}

SurfaceFileChange =
  UnifiedDiff {
    path: CanonicalPath,
    text: DisplayText,
    digest: Sha256Digest,
  }
  | PreviewOmitted {
      path: CanonicalPath,
      input_bytes: ByteCount,
      maximum_bytes: ByteCount,
    }

ToolInvocationStarted = Yes | No | Unknown
ToolTerminalSource = Observed | CompatibilityRepair

`CompatibilityRepair` is the released `terminalSource="compatibility_repair"`
value used for repaired or synthesized legacy tool results. It is not split
into additional private variants; the repair reason belongs in the typed health
ledger, preserving the exact compatibility field.

SurfaceToolResultKind =
  Success
  | Failed
  | Denied
  | Cancelled
  | TimedOut
  | InvalidArguments
  | ExternalEffectAmbiguous
  | ObservationUnavailable
  | CleanupAmbiguous

SurfaceToolTerminal {
  kind: SurfaceToolResultKind,
  source: ToolTerminalSource,
  invocation_started: ToolInvocationStarted,
}

SurfaceToolResult {
  tool_call_id: SurfaceToolCallId,
  name: NonEmptyText,
  terminal: SurfaceToolTerminal,
  output: Option<DisplayText>,
  error: Option<DisplayText>,
  exit_code: Option<i32>,
  truncated: bool,
  file_change: Option<SurfaceFileChange>,
}

SurfaceToolView {
  request: SurfaceToolRequest,
  state: Requested | Running | Completed,
  arguments_bytes: ByteCount,
  output_bytes: ByteCount,
  streamed_output: DisplayText,
  streamed_output_truncated: bool,
  result: Option<SurfaceToolResult>,
  capability_calls: Vec<SurfaceCapabilityCall>,
  terminal_leases: Vec<SurfaceRemoteTerminalLease>,
}
```

`SurfaceToolView` has one closed state/result pairing: Requested and Running
require `result=None`; Completed requires `result=Some` with matching tool-call
id and name, and no later state transition is legal. Success requires no error;
Denied/InvalidArguments require `invocation_started=No`; ambiguous external
effect/cleanup results cannot claim `No`. `exit_code` is legal only for Shell,
and `file_change` only for Write. `CompatibilityRepair` requires the matching
typed health/recovery proof and cannot be selected by a live adapter.

```text
SurfaceCapabilityCallKind =
  ReadTextFile
  | WriteTextFile
  | TerminalCreate
  | TerminalOutput
  | TerminalWaitForExit
  | TerminalKill
  | TerminalRelease

SurfaceTerminalExitStatus {
  exit_code: Option<u32>,
  signal: Option<NonEmptyText>,
}

AcpCapabilityText(String)       // UTF-8, <= ACP_CAPABILITY_TEXT_BYTE_LIMIT
AcpCapabilityIdentifier(String) // nonempty UTF-8, <= ACP_CAPABILITY_IDENTIFIER_BYTE_LIMIT

CapabilityCallResult =
  ReadTextFile { content: AcpCapabilityText, content_digest: Sha256Digest }
  | WriteTextFileAcknowledged
  | TerminalCreated { terminal_id: SurfaceRemoteTerminalId }
  | TerminalOutputObserved {
      output: AcpCapabilityText,
      truncated: bool,
      exit_status: Option<SurfaceTerminalExitStatus>,
    }
  | TerminalExitObserved { exit_status: SurfaceTerminalExitStatus }
  | TerminalKillAcknowledged
  | TerminalReleaseAcknowledged
  | RemoteError { code: AcpCapabilityIdentifier, message: SafeDiagnosticText }

ExternalEffectKind = FileWrite | TerminalCreate | TerminalKill | TerminalRelease

SurfaceCapabilityCallState =
  Prepared
  | DeliveryPossible
  | WrittenAwaitingResponse
  | Completed {
      result: CapabilityCallResult,
      response_digest: Sha256Digest,
    }
  | FailedBeforeWrite { error: SafeDiagnosticText }
  | ObservationUnavailable { error: SafeDiagnosticText }
  | ExternalEffectAmbiguous {
      effect_kind: ExternalEffectKind,
      error: SafeDiagnosticText,
    }

SurfaceCapabilityCall {
  call_id: SurfaceCapabilityCallId,
  acp_session_id: NonEmptyText,
  fence: SurfaceOperationFence,
  capability_revision: CapabilityRevision,
  policy_epoch: PolicyEpoch,
  kind: SurfaceCapabilityCallKind,
  arguments_digest: Sha256Digest,
  owning_tool_call_id: SurfaceToolCallId,
  state: SurfaceCapabilityCallState,
}

SurfaceRemoteTerminalLeaseState =
  Live {
    terminal_id: SurfaceRemoteTerminalId,
    owner_fence: SurfaceOperationFence,
  }
  | KillPending {
      terminal_id: SurfaceRemoteTerminalId,
      owner_fence: SurfaceOperationFence,
    }
  | ReleasePending {
      terminal_id: SurfaceRemoteTerminalId,
      owner_fence: SurfaceOperationFence,
    }
  | Released
  | IdentityUnknown { create_call_id: SurfaceCapabilityCallId }
  | CleanupAmbiguous {
      terminal_id: Option<SurfaceRemoteTerminalId>,
      owner_fence: SurfaceOperationFence,
    }

SurfaceRemoteTerminalLease {
  lease_id: UuidV7,
  owning_tool_call_id: SurfaceToolCallId,
  state: SurfaceRemoteTerminalLeaseState,
}
```

```text
ToolPatch =
  Requested { request: SurfaceToolRequest }
  | ArgumentsProgress {
      tool_call_id: SurfaceToolCallId,
      arguments_bytes: ByteCount,
    }
  | OutputDelta {
      tool_call_id: SurfaceToolCallId,
      offset: ByteOffset,
      chunk: DisplayText,
    }
  | Completed { result: SurfaceToolResult }
  | CapabilityCallChanged { call: SurfaceCapabilityCall }
  | RemoteTerminalLeaseChanged { lease: SurfaceRemoteTerminalLease }
```

`ArgumentsProgress` and `OutputDelta` are ephemeral. Pre-request
`ArgumentsProgress` is a no-snapshot absolute progress event validated by the
live publisher; it may precede the complete model response and therefore does
not require an existing Tool view. `ToolPatch::Requested` atomically consumes
that live progress lane, initializes `arguments_bytes` to the UTF-8 length of
the final raw arguments, and verifies the final value is not below the last
observed progress. No progress lane is reconstructed after restart. Requested, Completed,
capability-call state, and terminal-lease state are recorded for recorded
threads. `WriteTextFile`, `TerminalCreate`, `TerminalKill`, and
`TerminalRelease` MUST commit `DeliveryPossible` before a writer receives a
byte permit. No state at or after `DeliveryPossible` automatically retries a
side-effecting call.

For a provider-declared call, `source_response_id` is required and the paired
ResponseCompleted equality above is enforced. Non-provider runtime tools require
None and a separately typed runtime source. `ToolPatch::Completed` is the sole
tool-view terminal transition and is paired with exactly one
`ItemPatch::Added(ToolResultMessage)` whose turn/tool-call id/content/terminal
match the result projection; that Item cannot be added independently.

`OutputDelta.offset` MUST equal the current `output_bytes`. The reducer appends
the UTF-8 chunk to `streamed_output`, advances `output_bytes`, and preserves the
runtime truncation flag. ACP standard `ToolCallUpdate.content` is projected from
the complete reduced `streamed_output` at the containing batch's `cursor_after`
because ACP collection
updates replace rather than append; the adapter never owns a second output
accumulator. `ArgumentsProgress` has no standard ACP frame and is retained only
as typed surface progress until a complete tool request exists.

The method settlement matrix is exact:

| Method after possible delivery loses observation | Terminal capability-call state | Lease effect |
| --- | --- | --- |
| ReadTextFile, TerminalOutput, TerminalWaitForExit | `ObservationUnavailable` | none |
| WriteTextFile | `ExternalEffectAmbiguous(FileWrite)` | none |
| TerminalCreate without decoded id | `ExternalEffectAmbiguous(TerminalCreate)` | `IdentityUnknown(create_call_id)` |
| TerminalKill | `ExternalEffectAmbiguous(TerminalKill)` | `CleanupAmbiguous(known id)` |
| TerminalRelease | `ExternalEffectAmbiguous(TerminalRelease)` | `CleanupAmbiguous(known id)` |

Every row settles the capability call exactly once before the owning tool can
settle. The decoded result variant MUST match the call kind; a mismatch is a
protocol failure, not `Success`. Only a successful `TerminalRelease` response
moves a lease to `Released`.

## Plan, Usage, Context, And Settings Facts

```text
SurfacePlanStatus = Pending | InProgress | Completed
SurfacePlanPriority = Low | Medium | High

SurfacePlanItem {
  step: NonEmptyText,
  priority: SurfacePlanPriority,
  status: SurfacePlanStatus,
}

SurfacePlanSnapshot {
  revision: PlanRevision,
  explanation: Option<DisplayText>,
  items: Vec<SurfacePlanItem>,
  causative_generation: Option<SurfaceOperationFence>,
}

UsageTotals {
  input_tokens: u64,
  output_tokens: u64,
  cache_tokens: u64,
  estimated_cost_usd_micros: u64,
}

SurfaceUsageSnapshot {
  revision: UsageRevision,
  thread_total: UsageTotals,
  active_operation: Option<(SurfaceOperationId, UsageTotals)>,
  goal: Option<(SurfaceGoalId, GoalUsage)>,
  workflow: Vec<(SurfaceWorkflowRunId, UsageTotals)>,
}
```

Floating-point `estimated_cost_usd` is converted once at typed ingress to
nonnegative integer micros using round-half-away-from-zero. Every later
projection uses the integer value.

```text
CompactionState =
  Idle
  | Running {
      operation_id: SurfaceOperationId,
      reason: Manual | Automatic,
      before_messages: u64,
    }
  | Completed {
      operation_id: SurfaceOperationId,
      reason: Manual | Automatic,
      strategy: NonEmptyText,
      before_messages: u64,
      after_messages: u64,
      collapsed_messages: u64,
      status_text: DisplayText,
    }

ProviderReplayHealth =
  None
  | Available { state_digest: Sha256Digest }
  | Invalidated { reason: DisplayText }

SurfaceContextFragmentKind = Runtime | Goal | Plan | Skill
SurfaceContextFragmentOrigin = System | GoalRuntime | Model | User

SurfaceContextFragment {
  id: NonEmptyText,
  kind: SurfaceContextFragmentKind,
  origin: SurfaceContextFragmentOrigin,
  content: DisplayText,
  max_tokens: u64,
}

SurfaceContextSnapshot {
  revision: ContextRevision,
  used_tokens: u64,
  limit_tokens: u64,
  compaction: CompactionState,
  fragments: Vec<SurfaceContextFragment>,
  provider_replay: ProviderReplayHealth,
}
```

Provider continuation bytes remain runtime-private. The surface exposes only a
digest and health state. No adapter receives a provider DTO or uses replay
health to resume execution.

```text
SurfaceReasoningEffort = Low | Medium | High | Max
SurfaceApprovalMode = Suggest | AutoEdit | FullAuto | Plan
SurfacePermissionDecision = Allow | Prompt | Deny
SurfaceNetworkDomainAccess = Allow | Deny

SurfaceActivePermissionProfile {
  id: NonEmptyText,
  extends: Option<NonEmptyText>,
}

SurfacePermissionRule {
  tool: NonEmptyText,
  pattern: NonEmptyText,
  decision: SurfacePermissionDecision,
}

SurfacePermissionRuleSet {
  ordered_rules: Vec<SurfacePermissionRule>,
  digest: Sha256Digest,
}

SurfaceAdditionalWorkingDirectory {
  path: CanonicalPath,
  source: NonEmptyText,
}

SurfaceNetworkDomainPermission {
  domain: CanonicalDomainName,
  access: SurfaceNetworkDomainAccess,
}

SurfaceNetworkPermissions {
  enabled: Option<bool>,
  domains: Vec<SurfaceNetworkDomainPermission>,
}

SurfaceRuntimeSettings {
  model: NonEmptyText,
  reasoning_effort: SurfaceReasoningEffort,
  approval_mode: SurfaceApprovalMode,
  cwd: CanonicalPath,
  workspace_roots: Vec<CanonicalPath>,
  active_permission_profile: Option<SurfaceActivePermissionProfile>,
  permission_rules: SurfacePermissionRuleSet,
  additional_working_directories: Vec<SurfaceAdditionalWorkingDirectory>,
  network_permissions: SurfaceNetworkPermissions,
  policy_epoch: PolicyEpoch,
}

SurfaceSettingsSnapshot {
  host_revision: SettingsRevision,
  thread_revision: SettingsRevision,
  effective: SurfaceRuntimeSettings,
  pending: Option<SurfaceRuntimeSettings>,
  frozen_generation_revision: Option<(SurfaceOperationFence, SettingsRevision)>,
}

SettingsPatch =
  Committed {
    previous_revision: SettingsRevision,
    snapshot: SurfaceSettingsSnapshot,
  }
  | PendingChanged {
      thread_revision: SettingsRevision,
      pending: Option<SurfaceRuntimeSettings>,
    }
```

`Committed` replaces only the typed settings sub-snapshot after validating the
expected revision. It never mutates the configuration captured by an active
generation. Model routing is an `OperationPatch::ModelRouteSelected` fact under
the exact generation scope and cannot change host or thread defaults.

## Durable Interaction Patches

```text
SurfaceInteractionKind =
  ToolApproval
  | PermissionRequest
  | UserInput
  | McpElicitation
  | BackgroundApproval

InteractionExpiryDeadline {
  issuing_host_incarnation: HostIncarnation,
  expires_at: MonotonicInstant,
  observed_expires_at: Option<UnixMillis>,
}

InteractionExpiryAuthorityFailure =
  ClockIdMismatch {
    expected: HostMonotonicClockId,
    observed: HostMonotonicClockId,
  }
  | TickArithmeticOverflow { clock_id: HostMonotonicClockId }
  | IssuingHostLost {
      clock_id: HostMonotonicClockId,
      issuing_host_incarnation: HostIncarnation,
    }

InteractionUnavailableDisposition =
  FailOperation
  | AwaitCapableAttachment { deadline: InteractionExpiryDeadline }

BrokerInteractionResponseRoute =
  Unassigned {
    epoch: ResponseRouteEpoch,
  }
  | Exclusive {
      epoch: ResponseRouteEpoch,
      attachment_id: SurfaceAttachmentId,
      grant_token: SurfaceResponseGrantToken,
    }
  | SharedFirstCommitWins {
      epoch: ResponseRouteEpoch,
      grants: NonEmptyVec<(SurfaceAttachmentId, SurfaceResponseGrantToken)>,
    }

SurfaceInteractionRoute =
  Unassigned {
    epoch: ResponseRouteEpoch,
  }
  | Exclusive {
      epoch: ResponseRouteEpoch,
      attachment_id: SurfaceAttachmentId,
    }
  | SharedFirstCommitWins {
      epoch: ResponseRouteEpoch,
      attachments: NonEmptySet<SurfaceAttachmentId>,
    }
```

`BrokerInteractionResponseRoute`, response tokens, and grant tokens are
runtime-private broker state and never enter a snapshot or event. The surface
projects only `SurfaceInteractionRoute`. A bound client handle resolves its
current broker grant internally when submitting a response; possession of
another attachment's public id or route view grants no authority.
`SurfaceInteractionView.recovery_disposition` is the sole durable unavailable
authority and route changes cannot modify it. Route projection is exact:
Unassigned and Exclusive preserve epoch/attachment id, while the unique keys of
a Shared broker grant map equal the public attachment set. Every change strictly
advances the route epoch and rotates all grant tokens.

```text
AuthorityFingerprint {
  operation_id: SurfaceOperationId,
  request_digest: Sha256Digest,
  tool_digest: Sha256Digest,
  cwd: CanonicalPath,
  workspace_roots_digest: Sha256Digest,
  policy_epoch: PolicyEpoch,
  executable_generation: Sha256Digest,
  artifact_generation: Sha256Digest,
  capability_digest: Sha256Digest,
}

SurfacePermissionPathLabel(DisplayText)
SurfacePermissionDomainPattern(DisplayText)

SurfaceFileSystemPermissionProfile {
  read: Option<Vec<SurfacePermissionPathLabel>>,
  write: Option<Vec<SurfacePermissionPathLabel>>,
}

SurfaceShellPermissionProfile { unsandboxed: bool }

SurfacePermissionNetworkProfile {
  enabled: Option<bool>,
  domains: Vec<(SurfacePermissionDomainPattern, Allow | Deny)>,
}

SurfacePermissionProfile {
  file_system: Option<SurfaceFileSystemPermissionProfile>,
  network: Option<SurfacePermissionNetworkProfile>,
  shell: Option<SurfaceShellPermissionProfile>,
}

PermissionGrantScope = Turn | Session

SurfaceSchemaInteger =
  Negative(i64)      // constructor requires value < 0
  | NonNegative(u64)

SurfaceSchema =
  String {
    title: Option<DisplayText>,
    description: Option<DisplayText>,
    enum_values: Vec<DisplayText>,
    min_length: Option<u64>,
    max_length: Option<u64>,
  }
  | Integer {
      title: Option<DisplayText>,
      description: Option<DisplayText>,
      minimum: Option<SurfaceSchemaInteger>,
      maximum: Option<SurfaceSchemaInteger>,
      enum_values: Vec<SurfaceSchemaInteger>,
    }
  | Number {
      title: Option<DisplayText>,
      description: Option<DisplayText>,
      minimum: Option<FiniteF64>,
      maximum: Option<FiniteF64>,
    }
  | Boolean {
      title: Option<DisplayText>,
      description: Option<DisplayText>,
    }
  | Array {
      title: Option<DisplayText>,
      description: Option<DisplayText>,
      items: Box<SurfaceSchema>,
      min_items: Option<u64>,
      max_items: Option<u64>,
    }
  | Object {
      title: Option<DisplayText>,
      description: Option<DisplayText>,
      properties: Vec<SurfaceSchemaProperty>,
      additional_properties: Denied,
    }
  | Unsupported {
      schema_digest: Sha256Digest,
      unsupported_keywords: NonEmptyVec<NonEmptyText>,
    }

SurfaceSchemaProperty {
  name: DisplayText,
  required: bool,
  schema: Box<SurfaceSchema>,
}

```

An unsupported typed interpretation remains identified by digest, while the
exact requested schema is preserved separately as closed `SurfaceDataValue` for
released-wire projection and capable extensions. It is never exposed as an open
JSON value. A client without opaque-content capability cannot answer it as a
form; runtime reroutes to a capable extension or applies the already-persisted
unavailable disposition.
`SurfaceDataValue` is recursive so every supported nested Array/Object schema
has a representable answer. In strict mode runtime validates it recursively
against the persisted schema before committing it. Object fields are unique and
ordered by schema-property order; an unknown field, missing required field, or
scalar/array/object kind mismatch is `InvalidInput` and does not consume the
interaction.
`SurfaceSchemaInteger` is canonical: negative integers use `Negative`, while
zero and positive integers use `NonNegative`; bounds and enum members compare by
their mathematical integer value. This preserves the full JSON `i64`/`u64`
range without an overlapping representation.

Every schema and settings collection is normalized before commit. Object
property names, form field names, enum values, workspace roots,
additional-directory canonical paths, and network domains are unique in their
respective collection. Permission-rule order is semantic, but an identical
`tool + pattern + decision` triple cannot occur twice. Numeric bounds are
finite and ordered; minimum length/item counts cannot exceed maxima. Violating
any rule is `InvalidInput`, never last-write-wins reduction.

Permission response labels intentionally do not reuse `CanonicalPath` or
`CanonicalDomainName`. They losslessly preserve released relative/empty paths,
`glob:` labels, `:root`, `:minimal`, `:workspace_roots[/subpath]`, `:tmpdir`,
`/tmp`, unknown special labels, and exact domain patterns including `*.` and
`**.`. Runtime later derives a canonical effective grant under the persisted
policy epoch; the compatibility decoder never rewrites these labels into a
different private authority value.

```text
SurfaceInteractionRequest =
  ToolApproval {
    tool: SurfaceToolRequest,
    description: DisplayText,
    preview: Option<DisplayText>,
    authority: AuthorityFingerprint,
  }
  | PermissionRequest {
      tool_call_id: SurfaceToolCallId,
      reason: Option<DisplayText>,
      permissions: SurfacePermissionProfile,
      authority: AuthorityFingerprint,
    }
  | UserInput {
      question: NonEmptyText,
      suggestions: Vec<DisplayText>,
    }
  | McpElicitation {
      server_name: NonEmptyText,
      server_request_id: NonEmptyText,
      message: DisplayText,
      request: SurfaceMcpElicitationRequest,
    }
  | BackgroundApproval {
      task: SurfaceTaskFence,
      tool: SurfaceToolRequest,
      authority: AuthorityFingerprint,
    }

SurfaceMcpElicitationRequest =
  Form {
    requested_schema: Option<SurfaceDataValue>,
    supported_schema: Option<SurfaceSchema>,
  }
  | Url {
      raw_url: Option<DisplayText>,
      requested_schema: Option<SurfaceDataValue>,
    }

SurfacePermissionClientDecision =
  Allow {
    scope: PermissionGrantScope,
    permissions: SurfacePermissionProfile,
    strict_auto_review: bool,
  }
  | Deny {
      scope: PermissionGrantScope,
      permissions: SurfacePermissionProfile,
      strict_auto_review: bool,
    }

SurfaceUserInputDecision = Answer(DisplayText) | Cancel

SurfaceMcpElicitationDecision =
  Accept { content: SurfaceDataValue }
  | Decline

SurfaceClientInteractionAnswer =
  ToolApproval {
    decision: Allow | Deny,
  }
  | PermissionRequest {
      decision: SurfacePermissionClientDecision,
    }
  | UserInput { decision: SurfaceUserInputDecision }
  | McpElicitation {
      decision: SurfaceMcpElicitationDecision,
    }
  | BackgroundApproval {
      decision: Allow | Deny,
    }

BrokerInteractionAnswerPolicy =
  NativeStrict
  | LegacyJsonlV0250PermissionProfile {
      connection_id: SurfaceConnectionId,
      policy_epoch: PolicyEpoch,
    }
  | LegacyJsonlV0250McpOpaqueContent {
      connection_id: SurfaceConnectionId,
    }

ApplicableAuthorityFingerprint =
  NotApplicable
  | Persisted { authority: AuthorityFingerprint }

BoundInteractionResponse {
  response_id: SurfaceResponseId,
  answer: SurfaceClientInteractionAnswer,
  policy: BrokerInteractionAnswerPolicy,
  authority: ApplicableAuthorityFingerprint,
}

ValidatedInteractionResponse {
  interaction_id: SurfaceInteractionId,
  response_id: SurfaceResponseId,
  answer: SurfaceClientInteractionAnswer,
  policy: BrokerInteractionAnswerPolicy,
  authority: ApplicableAuthorityFingerprint,
  route_epoch: ResponseRouteEpoch,
  operation_fence: SurfaceOperationFence,
}
```

Wire/TUI clients construct only `SurfaceClientInteractionAnswer`; they cannot
supply or deserialize a response id, answer policy, route/grant token, or
authority fingerprint. The bound handle injects the stable response id and the
policy persisted with the broker request. For every Allow that can authorize a
later side effect it also injects the persisted request fingerprint; the actor
re-derives current authority and constructs `ValidatedInteractionResponse` only
when they match. A changed component returns `WrongAuthorityFingerprint` and
does not consume the interaction. Deny/UserInput/MCP responses use
`NotApplicable`.

The `kind` tag of an answer MUST equal the request kind. Tool/background
approval has no synthetic cross-request scope; permission persistence uses its
separate Turn/Session scope. The request variant,
`SurfaceInteractionView.kind`, answer variant, and every route grant are one
closed discriminant and must agree; a mismatch is `WrongInteractionKind`.
With `NativeStrict`, an Allow profile must be a subset of the requested profile.
With `LegacyJsonlV0250PermissionProfile`, any normalized well-formed response
profile is preserved, including a different or broader profile, exactly matching
the released JSONL behavior; route, fence, policy epoch, authority, and closed
shape checks still apply. The legacy policy is capability-bound to the exact
JSONL connection and request and cannot be selected by another client. An
empty/default profile is valid in both modes. Permission scope is the released
`Turn | Session`, and `strict_auto_review` is retained for both Allow and Deny.
User-input suggestions are advisory: every `Answer`, including the empty string,
is accepted, while `Cancel` is distinct from `Answer("")`.
For MCP, Form versus Url is fixed by the request variant; both may carry the
exact closed JSON `requested_schema`, and both accept closed JSON content. A Url
request preserves the released optional raw string without canonicalizing or
rejecting it, and its message may be empty. A missing released `contentJson`
normalizes to `Object([])` when accepted; an omitted released `accepted` decodes
to false and produces Decline regardless of content. `NativeStrict` recursively validates a supported Form
schema. `LegacyJsonlV0250McpOpaqueContent` accepts any bounded
`SurfaceDataValue`, including one that does not match a supported schema, and is
capability-bound to the exact released JSONL connection/request. Unsupported or
URL schemas remain lossless closed data rather than a digest-only public
placeholder. A
`BackgroundApproval` resolution is accepted only against the task fence already
bound to the persisted request; the response cannot nominate or replace it.

```text
SurfaceInteractionSafeProjection =
  ToolApproval { allowed: bool }
  | PermissionRequest {
      decision: Allow | Deny,
      scope: PermissionGrantScope,
      strict_auto_review: bool,
    }
  | UserInput { answered: bool }
  | McpElicitation { accepted: bool }
  | BackgroundApproval { allowed: bool }

SurfaceInteractionResolutionReceipt {
  response_id: SurfaceResponseId,
  receipt_id: SurfaceResponseReceiptId,
  kind: SurfaceInteractionKind,
  safe_projection: SurfaceInteractionSafeProjection,
}

BrokerInteractionRequestRecord {
  thread_id: SurfaceThreadId,
  interaction_id: SurfaceInteractionId,
  fence: SurfaceOperationFence,
  kind: SurfaceInteractionKind,
  request: SurfaceInteractionRequest,
  response_token: SurfaceResponseToken,
  answer_policy: BrokerInteractionAnswerPolicy,
  recovery_disposition: InteractionUnavailableDisposition,
}

BrokerResponsePayload =
  ReplayablePrivate { encrypted_reference: OpaqueToken }
  | LiveOnly { incarnation: SurfaceIncarnation }

BrokerInteractionResponseRecord {
  receipt: SurfaceInteractionResolutionReceipt,
  payload: BrokerResponsePayload,
  keyed_response_digest: OpaqueToken,
}

BrokerInteractionWaitResult =
  Resolved { response: BrokerInteractionResponseRecord }
  | Cancelled { reason: InteractionCancelReason }
  | Expired { deadline: InteractionExpiryDeadline }

SurfaceInteractionLifecycle =
  Requested
  | Resolved {
      receipt: SurfaceInteractionResolutionReceipt,
    }
  | Cancelled { reason: InteractionCancelReason }
  | Expired { deadline: InteractionExpiryDeadline }
  | Transferred { background_fence: SurfaceBackgroundFence }

SurfaceInteractionView {
  interaction_id: SurfaceInteractionId,
  revision: InteractionRevision,
  fence: SurfaceOperationFence,
  kind: SurfaceInteractionKind,
  request: SurfaceInteractionRequest,
  route: SurfaceInteractionRoute,
  lifecycle: SurfaceInteractionLifecycle,
  recovery_disposition: InteractionUnavailableDisposition,
}

InteractionPatch =
  Requested { interaction: SurfaceInteractionView }
  | RouteChanged {
      interaction_id: SurfaceInteractionId,
      expected_revision: InteractionRevision,
      next_revision: InteractionRevision,
      route: SurfaceInteractionRoute,
    }
  | Resolved {
      interaction_id: SurfaceInteractionId,
      expected_revision: InteractionRevision,
      next_revision: InteractionRevision,
      receipt: SurfaceInteractionResolutionReceipt,
    }
  | Cancelled {
      interaction_id: SurfaceInteractionId,
      expected_revision: InteractionRevision,
      next_revision: InteractionRevision,
      reason: InteractionCancelReason,
    }
  | Expired {
      interaction_id: SurfaceInteractionId,
      expected_revision: InteractionRevision,
      next_revision: InteractionRevision,
      deadline: InteractionExpiryDeadline,
    }
  | Transferred {
      interaction_id: SurfaceInteractionId,
      expected_revision: InteractionRevision,
      next_revision: InteractionRevision,
      background_fence: SurfaceBackgroundFence,
      route: SurfaceInteractionRoute,
    }
```

For recorded threads every patch is coordinator-wrapped and durable. The first
valid response commit wins. A malformed, foreign, stale, or wrong-kind response
does not advance revision or remove a waiter. Detach revokes a grant; it does
not create a lifecycle patch. Any reassignment rotates the route epoch and all
grant tokens.
Only `InteractionExpiryDeadline.expires_at` authorizes expiry. The optional
wall-clock value is display metadata. Reaching the same-clock deadline commits
an interaction-only batch containing `InteractionPatch::Expired`, returns its
matching `ThreadLocalCursor` acknowledgement, then wakes the process-local
waiter exactly once with `BrokerInteractionWaitResult::Expired`. After the
generation returns and every child/join settlement completes, the actor commits
a second batch containing
`OperationPatch::GenerationStopped(ExecutionFailed(ClientCapabilityUnavailable))`
followed by `OperationPatch::FinalizationStarted(GenerationStop(same reason))`,
with one matching Operation `ThreadLocalCursor` acknowledgement per patch. The
finalizer then publishes the unique failed terminal through its normal terminal
barrier; an expired waiter cannot remain blocked or resume tool execution.
If the issuing monotonic clock is lost,
has a different clock id, or cannot be compared without overflow, recovery
commits `Cancelled(ExpiryAuthorityUnavailable)` with the exact deadline and
failure witness under the persisted `AwaitCapableAttachment` disposition, then
fails the owning operation as `Failed(ClientCapabilityUnavailable)`. It never
fabricates an `Expired` patch from wall time or reclassifies the persisted
disposition as `FailOperation`.
`BrokerInteractionRequestRecord.answer_policy` is durable and runtime-private.
`NativeStrict` is legal for every kind; the permission legacy policy is legal
only for a JSONL-origin PermissionRequest, and the MCP legacy policy only for a
JSONL-origin McpElicitation. Their connection id must equal the bound route's
connection and cannot survive reassignment to another role/connection. Any
kind/origin/policy mismatch is `InvalidInput` before response consumption.
The full response body exists only in the runtime-private broker record and
execution waiter. It never enters `SurfaceInteractionView`, a surface patch,
snapshot, replay batch, public digest, or peer-client subscription. The safe
projection discriminant must match the receipt kind. Broker idempotency compares
the private keyed digest; low-entropy UserInput/MCP content is not exposed by a
public content digest. A LiveOnly payload that is unavailable after restart
follows the typed recovery disposition instead of reconstructing response data.
`InteractionCancelReason::CapabilityUnavailable` closes the interaction under
its persisted `FailOperation` disposition and enters finalization as
`Failed(ClientCapabilityUnavailable)`; it never becomes an operation
`Cancelled` terminal.
`InteractionCancelReason::ExpiryAuthorityUnavailable` is legal only for a
persisted `AwaitCapableAttachment` disposition with a byte-identical deadline
and exact clock-id mismatch, overflow, or issuing-host-loss witness. It enters
the same failed operation terminal without changing the persisted disposition.
Both unavailable cancellation variants wake the waiter with the byte-identical
`BrokerInteractionWaitResult::Cancelled` only after the interaction lifecycle
patch commits in its own batch with the matching Interaction cursor. Only after
the generation returns and all child/join settlements complete may the actor
commit the second, ordered `GenerationStopped(ExecutionFailed(
ClientCapabilityUnavailable))` plus `FinalizationStarted(GenerationStop(same
reason))` batch and its two Operation cursor acknowledgements. A crash between
barriers recovers from the durable unavailable lifecycle and deterministically
completes this same post-join failure batch without rerunning the interaction or
generation.

## Task, Workflow, And Subagent Patches

```text
SurfaceTaskType = MainSession | Workflow | Subagent | Shell | Monitor
SurfaceTaskStatus =
  Queued | Running | Paused | Stopping | Stopped | Completed | Failed
  | ApprovalRequired | Cancelled

SurfaceTask {
  task_id: SurfaceTaskId,
  revision: TaskRevision,
  task_type: SurfaceTaskType,
  status: SurfaceTaskStatus,
  backgrounded: bool,
  description: DisplayText,
  created_at: UnixMillis,
  started_at: Option<UnixMillis>,
  completed_at: Option<UnixMillis>,
  parent_operation: Option<SurfaceOperationId>,
  background_fence: Option<SurfaceBackgroundFence>,
  workflow_run_id: Option<SurfaceWorkflowRunId>,
  subagent_id: Option<SurfaceSubagentId>,
  pending_interaction_id: Option<SurfaceInteractionId>,
  usage: Option<UsageTotals>,
  result: Option<DisplayText>,
  error: Option<DisplayText>,
}

TaskPatch =
  Upserted {
    expected_revision: Option<TaskRevision>,
    task: SurfaceTask,
  }
  | StatusChanged {
      task_id: SurfaceTaskId,
      expected_revision: TaskRevision,
      next_revision: TaskRevision,
      status: SurfaceTaskStatus,
      completed_at: Option<UnixMillis>,
      result: Option<DisplayText>,
      error: Option<DisplayText>,
    }
  | OwnershipChanged {
      task_id: SurfaceTaskId,
      expected_revision: TaskRevision,
      next_revision: TaskRevision,
      backgrounded: bool,
      background_fence: Option<SurfaceBackgroundFence>,
    }
  | Reconciled {
      source_revision: TaskRevision,
      tasks: Vec<SurfaceTask>,
    }

TaskStatusTransition =
  Absent -> Queued
  | Absent -> Running
  | Queued -> Running
  | Queued -> Paused
  | Queued -> Stopping
  | Queued -> Stopped
  | Queued -> Failed
  | Queued -> Cancelled
  | Running -> Paused
  | Running -> Stopping
  | Running -> Stopped
  | Running -> Completed
  | Running -> Failed
  | Running -> ApprovalRequired
  | Running -> Cancelled
  | ApprovalRequired -> Running
  | ApprovalRequired -> Stopping
  | ApprovalRequired -> Stopped
  | ApprovalRequired -> Failed
  | ApprovalRequired -> Cancelled
  | Paused -> Running
  | Paused -> Stopping
  | Paused -> Stopped
  | Paused -> Failed
  | Paused -> Cancelled
  | Stopping -> Stopped
  | Stopping -> Failed
  | Stopping -> Cancelled
```

`Reconciled` has two closed commit authorities. A fresh recorded-thread import
requires an opaque receipt issued while `TaskRegistry` holds the session lock;
the coordinator verifies the session/thread identity, canonical receipt digest,
publication horizon, exact receipt-derived additions, and byte-identical
retention of every current surface task. Ordinary actor authority rejects this
patch. Recovery may replay an already prepared reconciliation without reopening
the registry only when the ledger batch is append-only and every new row is a
revision-one, non-backgrounded, operation-free, interaction-free MainSession in
Completed, Stopped, or Cancelled state. The recovery authority cannot create a
fresh batch, active/failed/approval task, or rich workflow/subagent identity.
Its input is only the single immutable prepared batch returned by
`JsonlSurfaceCommitLedger::recover_batches`: stored-batch decoding reruns batch
preflight, recomputes and matches the canonical digest and event count, and
checks the recorded cursor chain before the coordinator records that exact
batch as `recovered_prepared` and evaluates the constrained shape. No public or
fresh commit path accepts a caller-supplied prepared reconciliation identity.

Already committed reconciliation batches follow the ordinary commit-id
idempotency rule. Ledger recovery materializes each committed batch from the
validated committed prefix and never sends it through prepared authority. An
exact retry with the same commit id and canonical digest returns the existing
`CommitProbe::Present` receipt without a second reducer application or append;
a conflicting identity/digest is rejected. A later cold start also subtracts
surface task ids before building a fresh batch, so an already imported legacy
row consumes no new commit.

Legacy active-task adoption has a separate closed commit authority. For a
recorded thread, `TaskRegistry` may issue an opaque receipt while holding the
session lock over safe registry-only MainSession rows in exact Running state.
The coordinator admits every missing receipt row, and no other row, as one
canonical five-event group: `OperationRequested`, `OperationAdmitted`,
`GenerationStarted`, `TaskPatch::Upserted`, then `GenerationTransferred`. The
operation is non-replayable with an unavailable capsule, the generation and
task share the same fresh operation identity, and the transfer installs the
exact background fence recorded on the task. The fixed capability fingerprint
is the SHA-256 of `orca.runtime.legacy-active-task-adoption.v1`; current thread
owner, settings, and policy revisions are required. Ordinary actor authority
and terminal reconciliation authority cannot commit this shape. Because the
legacy row has no typed operation id, both the startup producer and fresh or
prepared authority reject the entire adoption when the recovered snapshot
already contains any foreground, queued, historical, or background operation
lineage; a Running compatibility mirror is otherwise indistinguishable from a
truly registry-only row.

Cold startup subtracts task ids already present in the recovered surface,
commits all remaining receipt-backed groups in one batch, and then applies the
existing non-replayable runtime-restart recovery. That recovery terminalizes
the operation as `AbortedByRuntimeRestart`; only the subsequent terminal task
reconciliation changes the adopted task to Stopped and then mirrors Stopped to
the legacy registry. An append failure exhausts bounded semantic retries and
leaves the registry row Running. Prepared-ledger recovery may reuse the active
adoption authority only for the same exact append-only five-event shape; it
cannot create a replayable continuation, approval, failed/retryable task, rich
task graph, or omission/replacement of current state. Already committed and
repeated-start batches follow ordinary commit-id and surface-task idempotency.

The reducer independently requires unique ids, source revision coverage, and
byte-identical retention of every current task. It permits constrained terminal
history creation only in the shape above; omission or replacement of an
existing task is `IllegalTransition`. Live `Upserted(expected_revision=None)` is
creation-only and may construct only Queued or Running; each later changed task
must prove one listed edge. Stopped, Completed, Failed, and Cancelled are
absorbing. `OwnershipChanged` changes no status and is legal only for a
nonterminal task with the exact background fence. Every omitted edge is
`IllegalTransition`.

```text
SurfaceWorkflowStatus =
  Queued | Running | Paused | Stopping | Stopped | Completed | Failed
  | Cancelled | AsyncLaunched

SurfaceWorkflowAgentStatus = Pending | Running | Cached | Completed | Failed | Cancelled

SurfaceWorkflowPhase {
  name: NonEmptyText,
  status: SurfaceWorkflowStatus,
  started_at: Option<UnixMillis>,
  completed_at: Option<UnixMillis>,
  agent_count: u32,
  summary: Option<DisplayText>,
  error: Option<DisplayText>,
}

SurfaceWorkflowAgent {
  agent_id: SurfaceSubagentId,
  phase: NonEmptyText,
  status: SurfaceWorkflowAgentStatus,
  attempt: u32,
  output: Option<DisplayText>,
  error: Option<DisplayText>,
  usage: Option<UsageTotals>,
}

SurfaceWorkflowResult {
  result_id: SurfaceWorkflowResultId,
  tool_use_id: Option<SurfaceToolCallId>,
  status: Success | Failed,
  content: DisplayText,
  acknowledged_by_operation: Option<SurfaceOperationId>,
}

SurfaceWorkflow {
  workflow_run_id: SurfaceWorkflowRunId,
  task_id: SurfaceTaskId,
  revision: WorkflowRevision,
  name: NonEmptyText,
  status: SurfaceWorkflowStatus,
  phases: Vec<SurfaceWorkflowPhase>,
  agents: Vec<SurfaceWorkflowAgent>,
  result: Option<SurfaceWorkflowResult>,
  error: Option<DisplayText>,
  parent: Option<SurfaceOperationFence>,
}

WorkflowPatch =
  Started { workflow: SurfaceWorkflow }
  | Resumed {
      fence: SurfaceWorkflowFence,
      next_revision: WorkflowRevision,
    }
  | PhaseStarted {
      fence: SurfaceWorkflowFence,
      next_revision: WorkflowRevision,
      phase: SurfaceWorkflowPhase,
    }
  | PhaseCompleted {
      fence: SurfaceWorkflowFence,
      next_revision: WorkflowRevision,
      phase: SurfaceWorkflowPhase,
    }
  | AgentStarted {
      fence: SurfaceWorkflowFence,
      next_revision: WorkflowRevision,
      agent: SurfaceWorkflowAgent,
    }
  | AgentCached {
      fence: SurfaceWorkflowFence,
      next_revision: WorkflowRevision,
      agent: SurfaceWorkflowAgent,
    }
  | AgentCompleted {
      fence: SurfaceWorkflowFence,
      next_revision: WorkflowRevision,
      agent: SurfaceWorkflowAgent,
    }
  | AgentFailed {
      fence: SurfaceWorkflowFence,
      next_revision: WorkflowRevision,
      agent: SurfaceWorkflowAgent,
    }
  | AgentCancelled {
      fence: SurfaceWorkflowFence,
      next_revision: WorkflowRevision,
      agent: SurfaceWorkflowAgent,
    }
  | Paused {
      fence: SurfaceWorkflowFence,
      next_revision: WorkflowRevision,
      reason: DisplayText,
    }
  | Stopping {
      fence: SurfaceWorkflowFence,
      next_revision: WorkflowRevision,
      reason: DisplayText,
    }
  | Stopped {
      fence: SurfaceWorkflowFence,
      next_revision: WorkflowRevision,
      reason: DisplayText,
    }
  | AsyncLaunched {
      fence: SurfaceWorkflowFence,
      next_revision: WorkflowRevision,
    }
  | Completed {
      fence: SurfaceWorkflowFence,
      next_revision: WorkflowRevision,
    }
  | Failed {
      fence: SurfaceWorkflowFence,
      next_revision: WorkflowRevision,
      error: DisplayText,
    }
  | Cancelled {
      fence: SurfaceWorkflowFence,
      next_revision: WorkflowRevision,
      reason: DisplayText,
    }
  | ResultReady {
      fence: SurfaceWorkflowFence,
      next_revision: WorkflowRevision,
      result: SurfaceWorkflowResult,
    }
  | ResultAcknowledged {
      fence: SurfaceWorkflowFence,
      next_revision: WorkflowRevision,
      result_id: SurfaceWorkflowResultId,
      operation_id: SurfaceOperationId,
    }

WorkflowRunStatusTransition =
  Absent -> Queued
  | Absent -> Running
  | Queued -> Running
  | Queued -> Failed
  | Queued -> Cancelled
  | Running -> Paused
  | Running -> Stopping
  | Running -> Completed
  | Running -> Failed
  | Running -> Cancelled
  | Running -> AsyncLaunched
  | Paused -> Running
  | Paused -> Stopping
  | Paused -> Failed
  | Paused -> Cancelled
  | Stopping -> Stopped
  | Stopping -> Failed
  | Stopping -> Cancelled
  | AsyncLaunched -> Running
  | AsyncLaunched -> Stopping
  | AsyncLaunched -> Completed
  | AsyncLaunched -> Failed
  | AsyncLaunched -> Cancelled

WorkflowPhaseStatusTransition =
  Absent -> Running
  | Running -> Completed
  | Running -> Failed
  | Running -> Stopped
  | Running -> Cancelled

WorkflowAgentAttemptTransition =
  Absent -> Pending
  | Absent -> Running
  | Pending -> Running
  | Pending -> Cached
  | Pending -> Cancelled
  | Running -> Completed
  | Running -> Failed
  | Running -> Cancelled
```

Workflow result acknowledgement is runtime-owned. `ResultReady` is committed
before any source acknowledgement; admitting its follow-up operation records
`acknowledged_by_operation` through `ResultAcknowledged` under the same
idempotency identity. Workflow Started creates only Queued or Running. Each
phase identity and each `(agent_id, attempt)` is independent; retry allocates a
strictly larger attempt and cannot reopen a terminal attempt. Stopped,
Completed, Failed, and Cancelled workflow runs; Completed, Failed, Stopped, and
Cancelled phases; and Cached, Completed, Failed, and Cancelled agent attempts
are absorbing. `ResultReady` and `ResultAcknowledged` do not change run status.
`Stopping` realizes only `Running | Paused | AsyncLaunched -> Stopping`;
`Stopped` realizes only `Stopping -> Stopped`. A stop request cannot skip the
Stopping receipt even when the runner settles immediately.
Every patch must realize exactly one listed edge or an exact receipt-backed
rematerialization; every omitted edge is `IllegalTransition`.

```text
SurfaceSubagentStatus = Running | Completed | Failed | Cancelled

SurfaceSubagent {
  subagent_id: SurfaceSubagentId,
  revision: SubagentRevision,
  description: DisplayText,
  status: SurfaceSubagentStatus,
  activity: Option<DisplayText>,
  turn: Option<u32>,
  usage: Option<UsageTotals>,
  output: Option<DisplayText>,
  error: Option<DisplayText>,
  parent: SurfaceOperationFence,
}

SubagentPatch =
  Started {
    expected_revision: None,
    subagent: SurfaceSubagent where status=Running,
  }
  | Progress {
      subagent_id: SurfaceSubagentId,
      expected_revision: SubagentRevision,
      next_revision: SubagentRevision,
      parent: SurfaceOperationFence,
      activity: DisplayText,
      turn: Option<u32>,
      usage: Option<UsageTotals>,
    }
  | Completed {
      subagent_id: SurfaceSubagentId,
      expected_revision: SubagentRevision,
      next_revision: SubagentRevision,
      parent: SurfaceOperationFence,
      status: Completed | Failed | Cancelled,
      output: Option<DisplayText>,
      error: Option<DisplayText>,
      usage: Option<UsageTotals>,
    }

SubagentStatusTransition =
  Absent -> Running
  | Running -> Running(Progress)
  | Running -> Completed
  | Running -> Failed
  | Running -> Cancelled
```

Subagent progress is ephemeral. Start and completion are durable when the
parent thread is recorded. Progress still advances the current-incarnation
revision even when its event is ephemeral, so late or duplicate progress cannot
mutate a completed record. Completed, Failed, and Cancelled are absorbing; a
retry allocates a new subagent id. Every expected/next revision is contiguous,
the parent fence is immutable, and every omitted edge is `IllegalTransition`.

## Goal Patches

```text
SurfaceEvidenceKind = Test | File | Command | Observation | External

SurfaceEvidenceItem {
  kind: SurfaceEvidenceKind,
  summary: NonEmptyText,
  target: Option<DisplayText>,
}

SurfaceBlockerKind =
  UserDecision | MissingAuthority | ExternalState | EnvironmentContradiction
  | UnverifiableRequirement

SurfaceBlocker {
  kind: SurfaceBlockerKind,
  summary: NonEmptyText,
  fingerprint: NonEmptyText,
  evidence: Vec<SurfaceEvidenceItem>,
}

SurfaceGoalState =
  Active
  | Paused { reason: User | NoProgress | Backoff | Infrastructure
                     | WaitingForWorkflow | Recovery | UsageLimit,
             message: DisplayText }
  | Blocked { blocker: SurfaceBlocker }
  | BudgetLimited
  | Complete { evidence: Vec<SurfaceEvidenceItem> }

GoalUsage {
  charged_input_tokens: i64,
  output_tokens: i64,
  cache_tokens: i64,
  verifier_tokens: i64,
  cost_micros: i64,
  elapsed_seconds: i64,
}

SurfaceGoalRun {
  goal_run_id: SurfaceGoalRunId,
  run_origin: User | Resume | WorkflowNotification,
  operation_id: SurfaceOperationId,
  phase: SurfaceGoalRunPhase,
}

SurfaceGoalRunPhase =
  Preparing
  | InFlight { outer_turn: SurfaceGoalOuterTurnReceipt }
  | Settled { last_outer_turn: Option<SurfaceGoalOuterTurnReceipt> }

SurfaceGoalNoLiveRun =
  NoCurrentRun
  | Quiescent { run: SurfaceGoalRun where phase=Settled }

SurfaceGoal {
  goal_id: SurfaceGoalId,
  thread_id: SurfaceThreadId,
  goal_revision: GoalRevision,
  goal_owner_epoch: GoalOwnerEpoch,
  catalog_revision: GoalCatalogRevision,
  objective: NonEmptyText,
  objective_revision: GoalObjectiveRevision,
  state: SurfaceGoalState,
  token_budget: Option<i64>,
  usage: GoalUsage,
  current_run: Option<SurfaceGoalRun>,
  last_transition: Option<SurfaceGoalTransition>,
}

SurfaceGoalStoreReceipt {
  goal_id: SurfaceGoalId,
  goal_revision: GoalRevision,
  objective_revision: GoalObjectiveRevision,
  catalog_revision: GoalCatalogRevision,
  goal_owner_epoch: GoalOwnerEpoch,
  row_state: SurfaceGoalReceiptState,
  store_commit_id: SurfaceCommitId,
  receipt_digest: Sha256Digest,
}

SurfaceGoalRunReceipt = SurfaceGoalRun

SurfaceGoalClosedRunReceipt {
  run: SurfaceGoalRun,
  close_reason: Recovery,
  store_commit_id: SurfaceCommitId,
  receipt_digest: Sha256Digest,
}

SurfaceGoalOuterTurnReceipt {
  outer_turn_id: SurfaceGoalOuterTurnId,
  origin: User | Resume | Continuation | WorkflowNotification,
  outer_turn_count: u32,
}

SurfaceGoalReceiptState =
  Present {
    state: SurfaceGoalState,
    current_run: Option<SurfaceGoalRunReceipt>,
  }
  | Removed { tombstone_revision: GoalRevision }

SurfaceGoalTransition {
  previous: SurfaceGoalState,
  next: SurfaceGoalState,
  reason_code: NonEmptyText,
}
```

```text
SurfaceGoalIntent =
  Complete {
    intent_id: SurfaceGoalIntentId,
    reason: NonEmptyText,
    evidence: NonEmptyVec<SurfaceEvidenceItem>,
  }
  | Blocked {
      intent_id: SurfaceGoalIntentId,
      reason: NonEmptyText,
      blocker: SurfaceBlocker,
      evidence: Vec<SurfaceEvidenceItem>,
    }

SurfaceGoalIntentAck =
  DeferredToTurnEnd { intent_id: SurfaceGoalIntentId, pending_depth: u32 }
  | Rejected { code: GoalIntentRejectionCode,
               message: DisplayText }
  | AlreadyPending { intent_id: SurfaceGoalIntentId }
  | BlockedAgainstInactive { state: SurfaceGoalState }

GoalIntentRejectionCode =
  NoActiveOuterTurn
  | TerminalIntentPending
  | MissingEvidence
  | MissingBlocker
  | StaleIdentity

GoalContinuationStopReason =
  GoalInactive {
    state: SurfaceGoalState where Paused | Blocked | BudgetLimited | Complete,
  }
  | PredecessorNotSuccessful {
      status: Failed | Cancelled | ApprovalRequired | BudgetExhausted,
      terminal: OperationTerminal,
    }
  | TerminalizingControl { cause: TerminalizationCause }
  | QueuedUserInput { item_id: SurfaceItemId }
  | PendingInteraction { interaction_id: SurfaceInteractionId }
  | WorkflowOwned { workflow_run_id: SurfaceWorkflowRunId }
  | PlanModeDisallowsContinuation
  | VerificationPending
  | BudgetLimited { budget: OperationBudget::GoalTokenBudget }
  | RuntimeFailure { class: FailureClass, message: SafeDiagnosticText }

GoalContinuationDecision =
  Admitted {
    reason: Progress | GapFeedback,
    successor: SurfaceGoalGenerationIdentity,
  }
  | Stopped {
      reason: GoalContinuationStopReason,
      outer_turn_count: u32,
      goal_state: SurfaceGoalState,
      terminal: OperationTerminal,
    }

GoalContinuationCoordinatorResult =
  Applied { commit_id: SurfaceCommitId }
  | AlreadyApplied {
      predecessor: SurfaceGoalGenerationIdentity,
      decision: GoalContinuationDecision,
      commit_id: SurfaceCommitId,
      acknowledgements: NonEmptyVec<MutationCommitAck>,
    }
  | StaleIdentity

SurfaceGoalVerification =
  Achieved { evidence: Vec<SurfaceEvidenceItem> }
  | NotAchieved { gaps: Vec<SurfaceGoalGap> }
  | Blocked { blocker: SurfaceBlocker }
  | Indeterminate { message: DisplayText }

SurfaceGoalGap {
  summary: NonEmptyText,
  fingerprint: NonEmptyText,
  model_fixable: bool,
}
```

```text
GoalPatch =
  Created { goal: SurfaceGoal }
  | Edited {
      goal_id: SurfaceGoalId,
      previous_revision: GoalRevision,
      goal: SurfaceGoal,
    }
  | Removed {
      goal_id: SurfaceGoalId,
      previous_revision: GoalRevision,
      tombstone_revision: GoalRevision,
    }
  | RunStarted {
      goal_id: SurfaceGoalId,
      goal_run: SurfaceGoalRun,
    }
  | OuterTurnStarted {
      identity: SurfaceGoalGenerationIdentity,
    }
  | IntentRequested {
      goal_id: SurfaceGoalId,
      outer_turn_id: SurfaceGoalOuterTurnId,
      intent: SurfaceGoalIntent,
    }
  | IntentAcknowledged {
      goal_id: SurfaceGoalId,
      outer_turn_id: SurfaceGoalOuterTurnId,
      intent: SurfaceGoalIntent,
      ack: SurfaceGoalIntentAck,
    }
  | OuterTurnFinished {
      identity: SurfaceGoalGenerationIdentity,
      status: Success | Failed | Cancelled | ApprovalRequired | BudgetExhausted,
      usage: GoalUsage,
      next_action: Continue | Verify | Pause | Blocked | BudgetLimited | Complete,
    }
  | VerificationCompleted {
      identity: SurfaceGoalGenerationIdentity,
      result: SurfaceGoalVerification,
    }
  | Transitioned {
      goal_id: SurfaceGoalId,
      transition: SurfaceGoalTransition,
    }
  | ContinuationDecided {
      goal_id: SurfaceGoalId,
      predecessor: SurfaceGoalGenerationIdentity,
      decision: GoalContinuationDecision,
    }
  | Paused {
      goal_id: SurfaceGoalId,
      goal_run_id: Option<SurfaceGoalRunId>,
      outer_turn_id: Option<SurfaceGoalOuterTurnId>,
      state: SurfaceGoalState,
    }
  | Recovered {
      goal_id: SurfaceGoalId,
      stale_run: SurfaceGoalClosedRunReceipt,
      recovery_message: DisplayText,
      discarded_continuation: true,
    }
  | Completed {
      goal_id: SurfaceGoalId,
      goal_run_id: Option<SurfaceGoalRunId>,
      evidence: Vec<SurfaceEvidenceItem>,
      usage: GoalUsage,
    }

GoalPatchEnvelope {
  receipt: SurfaceGoalStoreReceipt,
  patch: GoalPatch,
}
```

Only `GoalPatchEnvelope` may publish a Goal change. Its receipt MUST match the
Goal publisher permit, patch identity, post-state revision, catalog revision,
owner epoch, and current-run state before reduction. A present receipt names the
exact optional `SurfaceGoalRun` witness stored after the patch. A Removed
patch requires `tombstone_revision == receipt.goal_revision`,
`receipt.row_state == Removed(tombstone_revision)`, and a strictly advancing
catalog revision; that tombstone is the deletion/replay witness. A client never
derives `Edited`, `Removed`, or `Transitioned` from before/after reads.
For Recovered, `stale_run.store_commit_id == receipt.store_commit_id`, its
receipt digest verifies the complete closed run, and its goal/run/operation
identity equals the pre-transaction current run; the post-state current run is
None. The same stale run cannot receive a second close receipt.
`OuterTurnStarted`, `OuterTurnFinished`, and `VerificationCompleted` carry the
same complete `SurfaceGoalGenerationIdentity` as the matching
`GenerationRecord`. `ContinuationDecided::Admitted.successor` is committed in
the same coordinator batch as that successor's `GenerationReserved`; its
required predecessor's `operation_fence` equals
`successor.predecessor_fence=Some(predecessor.operation_fence)`. Initial User,
Resume, and WorkflowNotification admissions use `RunStarted` plus the initial
generation admission and never construct `ContinuationDecided`. The first started
outer turn has `outer_turn_count=1`, matching the released Goal store; every
continuation is exactly predecessor count plus one. A
RecoveryReplacement preserves the count and outer-turn/input identities. The
reducer rejects any partial or mismatched relation and indexes predecessor fence
to the complete decision for first-commit-wins idempotency. Before live
reduction, the actor/coordinator compares that index: the same predecessor plus
byte-identical decision, successor when present, Goal receipt, and acknowledgement
vector returns `GoalContinuationCoordinatorResult::AlreadyApplied`; any changed
field returns `StaleIdentity` and emits no patch. This command-level retry is
distinct from `SurfaceReduceMode::Rematerialization` replay.
For every Goal generation, `OperationIntent::GoalRun` must repeat the same
`goal_id` and `goal_run_id`. Generation zero's identity must repeat its
`initial_objective_revision`; every continuation identity instead repeats the
objective revision in that continuation's admitting Goal-store receipt. The
current receipt must be `Present { state: Active, current_run: Some(SurfaceGoalRun {
goal_run_id, operation_id, run_origin, phase: InFlight { outer_turn: {
outer_turn_id, origin, outer_turn_count } } }) }` and
the identity's operation fence must name that operation. A stale run receipt or
field mismatch is rejected before generation reservation.

Goal patch post-state pairing is closed:

| Patch | Required receipt post-state |
| --- | --- |
| Created | Present with the exact state and no run, or the exact Preparing initial run |
| Edited | Present Active; a prior current run may be absent or Settled but never Preparing/InFlight, and a Settled run is preserved exactly |
| RunStarted | Present Active with the exact Preparing run |
| OuterTurnStarted / IntentRequested / IntentAcknowledged | Present Active with the exact InFlight run/outer-turn/count named by the patch |
| OuterTurnFinished / VerificationCompleted | Present with the exact Settled run and last outer-turn/count named by the patch |
| ContinuationDecided::Admitted | Present Active with the exact InFlight successor run/outer turn |
| ContinuationDecided::Stopped | Exact `decision.goal_state`; a Paused state may retain the terminalizing InFlight run only until the paired operation terminal, while Blocked/BudgetLimited/Complete have `current_run=None` |
| Paused | Present Paused; `current_run` may be the exact Preparing/InFlight run only while its operation terminalizes, otherwise it is None; Paused never retains Settled |
| Recovered | Present Paused with `reason=Recovery`, message equal to `recovery_message`, and `current_run=None`; `stale_run` is the exact closed pre-recovery witness and is not current |
| Transitioned | Present state equals `transition.next`; Active preserves the exact accompanying run witness, while Blocked/BudgetLimited/Complete require `current_run=None`; Paused follows the Paused row |
| Completed | Present Complete with `current_run=None` |
| Removed | Removed with the exact tombstone revision |

`Transitioned` may move a non-Complete state to the exact runtime-selected next
state; a runtime transition cannot downgrade Complete. Preparing and InFlight
are the only live execution phases; Settled is quiescent but still belongs to
the current unfinished Goal run. `SurfaceGoalNoLiveRun` is the exact guard used
below. Edit requires that guard, commits
Active, and preserves a Settled run. Attached Resume returns Complete unchanged;
otherwise it requires that guard, closes any Settled prior run in the
same coordinator intent, and commits Active through a fresh run admission.
SetAndRun has the same guard and closes any Settled prior run
before committing the new Preparing/InFlight run. Clear requires the exact fence
and that guard, closes any Settled run, then commits Removed. Pause may
temporarily retain the exact Preparing/InFlight run only while its operation is
terminalizing; its final settlement clears `current_run`. Recovery always closes
the stale run and never auto-admits a continuation. Token
budgets are absent or positive and every GoalUsage component is nonnegative at
construction.

`SurfaceGoalRun.run_origin` is immutable for the lifetime of a run and can never
be `Continuation`; only the outer-turn/generation origin uses that value.
`GoalContinuationDecision::Stopped` is always paired with
`GenerationStopped`, the exact outer-turn/verifier settlement, the selected Goal
state/store receipt, and `FinalizationStarted` in one coordinator batch. It
never creates a Suspended operation. Its terminal binding is closed:

| Stop reason | Required Goal state | Required operation terminal |
| --- | --- | --- |
| GoalInactive(Paused/Blocked/Complete) | the same inactive state | Succeeded with final operation usage |
| GoalInactive(BudgetLimited) or BudgetLimited | BudgetLimited | BudgetExhausted with the exact GoalTokenBudget |
| PredecessorNotSuccessful | state selected by the exact predecessor settlement | the byte-identical terminal required by that generation/verifier mapping |
| TerminalizingControl(UserCancel/GoalPause) | Paused(User) | Cancelled(User/GoalPause respectively) |
| TerminalizingControl(HostShutdown/ThreadClose) | Paused(Infrastructure) | Shutdown with the same cause |
| QueuedUserInput | Paused(User) | Succeeded with final operation usage |
| PendingInteraction | Paused(Infrastructure) | Succeeded with final operation usage after the interaction is closed by its persisted unavailable disposition |
| WorkflowOwned | Paused(WaitingForWorkflow) | Succeeded with final operation usage |
| PlanModeDisallowsContinuation or VerificationPending | Paused(NoProgress) | Succeeded with final operation usage |
| RuntimeFailure | Paused(Infrastructure) | Failed with the exact class/message |

Any other reason/state/terminal combination is `GoalReceiptMismatch`.

## Catalog And Pinned Context Patches

```text
SurfaceMcpServerStatus = Starting | Ready | Degraded { message: DisplayText }
                         | Stopped | Disabled

SurfaceMcpTool {
  id: SurfaceCatalogEntryId,
  server: NonEmptyText,
  name: NonEmptyText,
  schema_name: NonEmptyText,
  description: Option<DisplayText>,
  input_schema: SurfaceSchema,
}

SurfaceMcpResource {
  id: SurfaceCatalogEntryId,
  server: NonEmptyText,
  uri: CanonicalUri,
  name: NonEmptyText,
  description: Option<DisplayText>,
  mime: Option<CanonicalMime>,
}

SurfaceMcpResourceTemplate {
  id: SurfaceCatalogEntryId,
  server: NonEmptyText,
  uri_template: NonEmptyText,
  name: NonEmptyText,
  description: Option<DisplayText>,
  mime: Option<CanonicalMime>,
}

SurfaceMcpCatalogDiagnosticCode =
  EmptyName | EmptySchemaName | InvalidUri | InvalidUriTemplate | InvalidMime
  | InvalidSchema

SurfaceMcpCatalogDiagnostic {
  server: NonEmptyText,
  entry_kind: Tool | Resource | ResourceTemplate,
  source_index: u64,
  code: SurfaceMcpCatalogDiagnosticCode,
  source_digest: Sha256Digest,
}

SurfaceMcpCatalogSnapshot {
  revision: McpCatalogRevision,
  servers: Vec<(NonEmptyText, SurfaceMcpServerStatus)>,
  tools: Vec<SurfaceMcpTool>,
  resources: Vec<SurfaceMcpResource>,
  resource_templates: Vec<SurfaceMcpResourceTemplate>,
  diagnostics: Vec<SurfaceMcpCatalogDiagnostic>,
}

McpCatalogPatch =
  Reconciled {
    previous_revision: McpCatalogRevision,
    snapshot: SurfaceMcpCatalogSnapshot,
  }
  | ServerStatusChanged {
      previous_revision: McpCatalogRevision,
      next_revision: McpCatalogRevision,
      server: NonEmptyText,
      status: SurfaceMcpServerStatus,
    }
```

`Reconciled` performs an identity diff after validating the exact prior
revision. Runtime keeps unsupported raw MCP schemas private; the surface carries
only the closed schema or `Unsupported` descriptor. A baseline descriptor whose
name/URI/template/MIME/schema cannot construct the closed typed entry is omitted
from that entry vector, contributes one stable diagnostic per failed field, and
marks its server `Degraded`; the catalog revision still reconciles atomically.
Diagnostics are sorted by server/kind/source index/code and reveal only a source
digest, so malformed source is never silently dropped or promoted to binding
authority. A later valid descriptor removes its diagnostics through the next
ordinary Reconciled diff.

```text
SurfacePinnedContextEntry {
  id: SurfaceCatalogEntryId,
  kind: Memory | File | User | System,
  label: NonEmptyText,
  content: DisplayText,
  content_digest: Sha256Digest,
  source_revision: PinnedContextSourceRevision,
}

SurfacePinnedContextSnapshot {
  revision: PinnedContextRevision,
  entries: Vec<SurfacePinnedContextEntry>,
}

PinnedContextPatch =
  Added {
    previous_revision: PinnedContextRevision,
    next_revision: PinnedContextRevision,
    entry: SurfacePinnedContextEntry,
  }
  | Removed {
      previous_revision: PinnedContextRevision,
      next_revision: PinnedContextRevision,
      entry_id: SurfaceCatalogEntryId,
    }
  | Reconciled {
      previous_revision: PinnedContextRevision,
      snapshot: SurfacePinnedContextSnapshot,
    }
```

`SurfacePinnedContextEntry.kind` and `source_revision` are one closed pair:
Memory uses `PinnedContextSourceRevision::Memory`, File uses `File`, User uses
`User`, and System uses `System`. A mismatched pair is `InvalidInput` at command
ingress and `SurfaceReducerErrorCode::IllegalTransition` during reduction; no
source revision converts across those domains.

## Session Patches And Health

```text
ThreadPersistence =
  RecordedCatalogued
  | EphemeralNonCataloguedOneShot {
      close_after: FirstOperationCompletionPolicy,
    }
  | EphemeralAttached

FirstOperationCompletionPolicy = Terminal | NotAdmitted

SurfaceThreadSnapshot {
  thread_id: SurfaceThreadId,
  owner_epoch: ThreadOwnerEpoch,
  persistence: ThreadPersistence,
  title: DisplayText,
  metadata_revision: SessionMetadataRevision,
  created_at: UnixMillis,
  updated_at: UnixMillis,
  cwd: CanonicalPath,
  workspace_roots: Vec<CanonicalPath>,
  closed: bool,
}

SurfaceBackgroundOperation {
  operation_id: SurfaceOperationId,
  fence: SurfaceBackgroundFence,
  task_id: Option<SurfaceTaskId>,
  transferred_at: SurfaceCursor,
  finalizing_degraded: bool,
}

SurfaceHealthIssueId =
  Mutation(SurfaceSettlementId)
  | Projection(SurfaceCommitId)
  | StartCommit(SurfaceCommitId)
  | Finalization(SurfaceFinalizeIntentId)
  | BackgroundFinalization(SurfaceFinalizeIntentId)
  | Capability(SurfaceCapabilityCallId)
  | RemoteTerminal(UuidV7)
  | Ownership(ThreadOwnerEpoch)

SurfaceHealthIssue =
  MutationDegraded { settlement_id: SurfaceSettlementId }
  | ProjectionDegraded { commit_id: SurfaceCommitId, fact_family: SurfaceFactFamily }
  | StartCommitDegraded { fence: SurfaceOperationFence, commit_id: SurfaceCommitId }
  | FinalizingDegraded {
      operation_id: SurfaceOperationId,
      finalize_intent_id: SurfaceFinalizeIntentId,
      cause: FinalizationDegradedCause,
    }
  | BackgroundFinalizingDegraded {
      operation_id: SurfaceOperationId,
      finalize_intent_id: SurfaceFinalizeIntentId,
      cause: FinalizationDegradedCause,
    }
  | CapabilityObservationUnavailable { call_id: SurfaceCapabilityCallId }
  | ExternalEffectAmbiguous { call_id: SurfaceCapabilityCallId }
  | RemoteTerminalIdentityUnknown { lease_id: UuidV7 }
  | RemoteTerminalCleanupAmbiguous { lease_id: UuidV7 }
  | OwnershipLost { stale_epoch: ThreadOwnerEpoch }

SurfaceSessionHealth {
  revision: SessionHealthRevision,
  accepting_admission: bool,
  issues: Vec<(SurfaceHealthIssueId, SurfaceHealthIssue)>,
  closing: bool,
  closed: bool,
}

SurfaceHealthClearProof {
  issue_id: SurfaceHealthIssueId,
  resolving_commit_id: SurfaceCommitId,
  receipt_digest: Sha256Digest,
}
```

```text
SurfaceVerificationResult {
  command: NonEmptyText,
  success: bool,
  exit_code: Option<i32>,
  stdout: DisplayText,
  stderr: DisplayText,
}

SessionPatch =
  Materialized { thread: SurfaceThreadSnapshot }
  | OwnerEpochChanged {
      previous: ThreadOwnerEpoch,
      next: ThreadOwnerEpoch,
    }
  | MetadataChanged {
      previous_revision: SessionMetadataRevision,
      next_revision: SessionMetadataRevision,
      title: DisplayText,
      updated_at: UnixMillis,
    }
  | HealthIssueAdded {
      previous_revision: SessionHealthRevision,
      next_revision: SessionHealthRevision,
      id: SurfaceHealthIssueId,
      issue: SurfaceHealthIssue,
    }
  | HealthIssueCleared {
      previous_revision: SessionHealthRevision,
      next_revision: SessionHealthRevision,
      id: SurfaceHealthIssueId,
      proof: SurfaceHealthClearProof,
    }
  | RuntimeFault {
      class: FailureClass,
      message: DisplayText,
      causative_generation: Option<SurfaceOperationFence>,
    }
  | Closing {
      reason: HostShutdown | ThreadClose,
      barrier_id: SurfaceSettlementId,
      closing_commit_id: SurfaceCommitId,
      plan_digest: Sha256Digest,
    }
  | Closed {
      reason: HostShutdown | ThreadClose,
      barrier_id: SurfaceSettlementId,
      closing_commit_id: SurfaceCommitId,
      plan_digest: Sha256Digest,
    }
```

Legacy `Error` maps to `RuntimeFault` only when runtime typed ingress classifies
it as an authoritative runtime fault. Presentation diagnostics stay outside the
surface. Legacy `SessionCompleted` never enters this reducer as terminal
authority; it is emitted only by compatibility adapters from a committed
`OperationPatch::Terminal`.

## Mutation Result Algebra

```text
MutationTarget =
  Host { host_incarnation: HostIncarnation }
  | Thread { thread_id: SurfaceThreadId }
  | Operation {
      thread_id: SurfaceThreadId,
      operation_id: SurfaceOperationId,
    }
  | Generation { fence: SurfaceOperationFence }
  | Interaction {
      thread_id: SurfaceThreadId,
      interaction_id: SurfaceInteractionId,
    }
  | Goal { goal_id: SurfaceGoalId }
  | Task { thread_id: SurfaceThreadId, task_id: SurfaceTaskId }
  | Workflow {
      thread_id: SurfaceThreadId,
      workflow_run_id: SurfaceWorkflowRunId,
    }
  | Memory { scope: User | Project { root: CanonicalPath } }
  | FolderTrust { path: CanonicalPath }
  | RuntimeSettings {
      host_incarnation: HostIncarnation,
      thread_id: Option<SurfaceThreadId>,
    }
  | SessionCatalog { thread_id: Option<SurfaceThreadId> }
  | SessionMetadata { thread_id: SurfaceThreadId }
```

```text
MutationDisposition = Accepted | Queued | AlreadyApplied

SurfaceFactFamily =
  Operation | Item | Assistant | Tool | Plan | Usage | Context | Interaction
  | Task | Workflow | Subagent | Goal | Settings | McpCatalog
  | PinnedContext | Session

HostDomainKind =
  Memory | FolderTrust | RuntimeSettings | SessionCatalog | SessionMetadata
  | HostLifecycle

PolicyRevocationBarrierPlan {
  canonical_path: CanonicalPath,
  trust_revision: TrustRevision,
  policy_epoch: PolicyEpoch,
  expected_owner_leases: Vec<UuidV7>,
  expected_resources: Vec<NonEmptyText>,
  plan_digest: Sha256Digest,
}

MutationAckRequirement =
  ThreadCursor {
    thread_id: SurfaceThreadId,
    family: SurfaceFactFamily,
    event_id: SurfaceEventId,
    commit_id: SurfaceCommitId,
  }
  | ThreadRemoteOwner {
      thread_id: SurfaceThreadId,
      thread_owner_epoch: ThreadOwnerEpoch,
      durable_revision: DurableRevision,
      commit_id: SurfaceCommitId,
    }
  | HostReceipt {
      host_incarnation: HostIncarnation,
      domain: HostDomainKind,
      target: MutationTarget,
      revision: HostRevisionWitness,
      commit_id: SurfaceCommitId,
      receipt_digest: Sha256Digest,
    }
  | GoalStoreReceipt {
      goal_id: SurfaceGoalId,
      store_commit_id: SurfaceCommitId,
      receipt_digest: Sha256Digest,
    }
  | OperationTerminal {
      thread_id: SurfaceThreadId,
      thread_owner_epoch: ThreadOwnerEpoch,
      operation_id: SurfaceOperationId,
      terminal_commit_id: SurfaceCommitId,
    }
  | PolicyRevocationBarrier {
      plan: PolicyRevocationBarrierPlan,
    }

The RuntimeSettings HostReceipt and Settings ThreadCursor are both required, in
that order,
when `OperationSettingsPreparation::ApplyThreadOverridesBeforeRequested` is
used. ThreadRemoteOwnerAck is the alternative to
the local interaction cursor only when the durable route owner is another
process; it is never an extra duplicate witness.
The RuntimeSettings HostReceipt followed by the Settings ThreadCursor is required when CreateThread,
LoadThread, or ForkThread carries nonempty settings overrides.
The McpCatalog ThreadCursor is additionally required
when CreateThread or LoadThread carries nonempty MCP declarations. Empty vectors
require none of those conditional witnesses.

MutationCommitAck =
  ThreadLocalCursor {
    cursor: SurfaceCursor,
    family: SurfaceFactFamily,
    event_id: SurfaceEventId,
    commit_class: CommitClass,
  }
  | ThreadRemoteOwnerAck {
      thread_id: SurfaceThreadId,
      thread_owner_epoch: ThreadOwnerEpoch,
      durable_revision: DurableRevision,
      commit_id: SurfaceCommitId,
    }
  | GoalStoreCommitAck {
      goal_id: SurfaceGoalId,
      receipt: SurfaceGoalStoreReceipt,
    }
  | OperationTerminalAck {
      thread_id: SurfaceThreadId,
      thread_owner_epoch: ThreadOwnerEpoch,
      operation_id: SurfaceOperationId,
      value: OperationTerminalAtCursor,
    }
  | PolicyRevocationBarrierAck {
      plan: PolicyRevocationBarrierPlan,
      settled_owner_leases: Vec<UuidV7>,
      settled_resources: Vec<NonEmptyText>,
      proof: Sha256Digest,
    }
  | HostCommitAck {
      host_incarnation: HostIncarnation,
      target: MutationTarget,
      revision: HostRevisionWitness,
      commit_id: SurfaceCommitId,
      receipt_digest: Sha256Digest,
      receipt: HostDomainReceipt,
    }

SurfaceMemoryReceipt {
  scope: User | Project { root: CanonicalPath },
  record_id: SurfaceCatalogEntryId,
  memory_revision: MemoryRevision,
  display_path: CanonicalPath,
}

SurfaceFolderTrustReceipt {
  canonical_path: CanonicalPath,
  old_effective_level: Trusted | Untrusted,
  new_effective_level: Trusted | Untrusted,
  trust_revision: TrustRevision,
  policy_epoch: PolicyEpoch,
  reload_required: bool,
  reconciliation_proof: Option<Sha256Digest>,
}

SurfaceRuntimeSettingsReceipt {
  host_revision: SettingsRevision,
  thread_revision: Option<SettingsRevision>,
  effective: SurfaceRuntimeSettings,
  pending: Option<SurfaceRuntimeSettings>,
}

SurfaceSessionCatalogReceipt {
  catalog_revision: SessionCatalogRevision,
  thread_id: Option<SurfaceThreadId>,
  action: Created | Opened | Loaded | Forked | Closed | Removed,
}

SurfaceSessionMetadataReceipt {
  thread_id: SurfaceThreadId,
  metadata_revision: SessionMetadataRevision,
  title: DisplayText,
}

SurfaceHostShutdownReceipt {
  host_incarnation: HostIncarnation,
  lifecycle_revision: HostLifecycleRevision,
  barrier_id: SurfaceSettlementId,
  shutdown_commit_id: SurfaceCommitId,
  stage: Last,
  closed_at: UnixMillis,
}

HostDomainReceipt =
  Memory(SurfaceMemoryReceipt)
  | FolderTrust(SurfaceFolderTrustReceipt)
  | RuntimeSettings(SurfaceRuntimeSettingsReceipt)
  | SessionCatalog(SurfaceSessionCatalogReceipt)
  | SessionMetadata(SurfaceSessionMetadataReceipt)
  | HostLifecycle(SurfaceHostShutdownReceipt)
```

```text
HostReceiptIdentityPair =
  Memory {
    target: MutationTarget::Memory(scope),
    revision: HostRevisionWitness::Memory(memory_revision),
    receipt: HostDomainReceipt::Memory(scope, memory_revision),
  }
  | FolderTrust {
      target: MutationTarget::FolderTrust(path),
      revision: HostRevisionWitness::FolderTrust(trust_revision),
      receipt: HostDomainReceipt::FolderTrust(path, trust_revision),
    }
  | RuntimeSettings {
      target: MutationTarget::RuntimeSettings(host_incarnation, thread_id),
      revision: HostRevisionWitness::RuntimeSettings(settings_revision),
      receipt: HostDomainReceipt::RuntimeSettings(settings_revision),
    }
  | SessionCatalog {
      target: MutationTarget::SessionCatalog(thread_id),
      revision: HostRevisionWitness::SessionCatalog(catalog_revision),
      receipt: HostDomainReceipt::SessionCatalog(thread_id, catalog_revision),
    }
  | SessionMetadata {
      target: MutationTarget::SessionMetadata(thread_id),
      revision: HostRevisionWitness::SessionMetadata(metadata_revision),
      receipt: HostDomainReceipt::SessionMetadata(thread_id, metadata_revision),
    }
  | HostLifecycle {
      target: MutationTarget::Host(host_incarnation),
      revision: HostRevisionWitness::HostLifecycle(lifecycle_revision),
      receipt: HostDomainReceipt::HostLifecycle(
        host_incarnation, lifecycle_revision, stage=Last),
    }
```

An acknowledgement requirement is satisfied only by a witness whose complete
identity matches. `ThreadLocalCursor.cursor` equals the complete containing
batch's `cursor_after`, and its `commit_class` equals the batch CommitClass;
multiple family/event acknowledgements from one batch share that same cursor.
The `event_id` proves membership in the batch and never exposes an internal
event cursor. ThreadCursor otherwise compares thread/family/event/commit;
ThreadRemoteOwner compares thread/epoch/durable revision/commit. HostReceipt
must be one exact `HostReceiptIdentityPair`, including host, domain, target,
typed revision, commit, and the canonical digest of the complete receipt
payload. GoalStoreReceipt requires
`receipt.receipt_digest == requirement.receipt_digest`,
`receipt.store_commit_id == requirement.store_commit_id`, and matching Goal.
OperationTerminal requires matching thread, owner epoch, and operation,
`value.cursor.thread_id == thread_id`, and an exact applied-batch lookup by
`terminal_commit_id`: `value.cursor`, `value.commit_class`, and
`value.batch_digest` equal that `AppliedBatchRecord`'s `cursor_after`, complete
CommitClass, and digest. For a recorded commit the CommitClass owner epoch must
also equal the requirement. A `PolicyRevocationBarrierPlan` sorts and deduplicates
its two expected collections; `plan_digest` is the canonical digest of every
other field. PolicyRevocationBarrier compares the complete byte-identical plan,
settled subjects, and proof. Settled subjects are sorted unique subsets of their
matching expected collections. The state pending list is exactly the nonempty
tagged union `expected - settled`; requirement, acknowledgement, state, output,
and repair token carry the same original plan.
For HostLifecycle, `receipt.shutdown_commit_id == ack.commit_id ==
requirement.commit_id`, the barrier id matches the immutable shutdown plan, and
`stage` is `Last`; no separate host commit-class tag may weaken this identity.
The coordinator rejects missing, duplicate, out-of-order, or mismatched
witnesses before returning `Committed`; a `Deferred` value lists the exact
unproved requirements and retains the unique proved witnesses as an
order-preserving subsequence of the barrier plan. Parallel thread work need not
finish as a prefix; HostLifecycle remains last and cannot be proved early.
`HostCommitAck.revision` must be the matching `HostRevisionWitness` for its
`HostDomainReceipt`; a catalog revision cannot satisfy a settings or memory
receipt, and generic `Revision` values are never accepted at this boundary.

```text
DeferredMutationState =
  MutationDegraded { settlement_id: SurfaceSettlementId }
  | ProjectionDegraded {
      durable_commit_id: SurfaceCommitId,
      fact_family: SurfaceFactFamily,
    }
  | OwnerAckPending {
      thread_owner_epoch: ThreadOwnerEpoch,
      durable_revision: DurableRevision,
    }
  | StartCommitDegraded {
      generation_fence: SurfaceOperationFence,
      started_commit_id: SurfaceCommitId,
    }
  | FinalizingDegraded {
      operation_id: SurfaceOperationId,
      finalize_intent_id: SurfaceFinalizeIntentId,
      cause: FinalizationDegradedCause,
    }
  | MemoryPinPending {
      scope: User | Project { root: CanonicalPath },
      record_id: SurfaceCatalogEntryId,
      memory_revision: MemoryRevision,
      thread_id: SurfaceThreadId,
      thread_owner_epoch: ThreadOwnerEpoch,
    }
  | PolicyRevocationPending {
      plan: PolicyRevocationBarrierPlan,
      pending: NonEmptyVec<PolicyRevocationSubject>,
    }
  | ShutdownDeferred {
      plan: ShutdownBarrierPlan,
      missing: NonEmptyVec<ShutdownMissing>,
    }

ShutdownBarrierPlan =
  CloseThread {
    request_id: SurfaceRequestId,
    host_incarnation: HostIncarnation,
    thread: ShutdownThreadPlan,
    barrier_id: SurfaceSettlementId,
    closing_commit_id: SurfaceCommitId,
    plan_digest: Sha256Digest,
  }
  | ShutdownHost {
      request_id: SurfaceRequestId,
      host_incarnation: HostIncarnation,
      threads: Vec<ShutdownThreadPlan>,
      barrier_id: SurfaceSettlementId,
      closing_commit_id: SurfaceCommitId,
      final_host_lifecycle: MutationAckRequirement::HostReceipt,
      plan_digest: Sha256Digest,
    }

ShutdownOperationSourcePhase =
  Requested
  | AdmittedReserved
  | AdmittedStarted
  | Suspended
  | BackgroundOwned
  | Finalizing
  | FinalizingDegraded

ShutdownSelectedCause =
  ExistingWinning { cause: OperationFinalizationCause }
  | Requested { cause: HostShutdown | ThreadClose }

ShutdownOperationPlan =
  ExistingTerminal {
    operation_id: SurfaceOperationId,
    finalize_intent_id: SurfaceFinalizeIntentId,
    terminal_commit_id: SurfaceCommitId,
    requirement: MutationAckRequirement::OperationTerminal,
  }
  | PlannedFinalization {
      operation_id: SurfaceOperationId,
      source_phase: ShutdownOperationSourcePhase,
      finalize_intent_id: SurfaceFinalizeIntentId,
      terminal_commit_id: SurfaceCommitId,
      selected_cause: ShutdownSelectedCause,
      expected_settlements: Vec<SurfaceSettlementId>,
      requirement: MutationAckRequirement::OperationTerminal,
    }

ShutdownThreadPlan =
  Recorded {
    thread_id: SurfaceThreadId,
    owner_epoch: ThreadOwnerEpoch,
    operations: Vec<ShutdownOperationPlan>,
    session_closed: MutationAckRequirement::ThreadCursor,
    catalog_closed: MutationAckRequirement::HostReceipt,
  }
  | Ephemeral {
      thread_id: SurfaceThreadId,
      owner_epoch: ThreadOwnerEpoch,
      persistence: EphemeralNonCataloguedOneShot {
                     close_after: FirstOperationCompletionPolicy,
                   }
                   | EphemeralAttached,
      operations: Vec<ShutdownOperationPlan>,
      session_closed: MutationAckRequirement::ThreadCursor,
    }

ShutdownBarrierRecord {
  plan: ShutdownBarrierPlan,
  settled: Vec<MutationCommitAck>,
  state: Closing | Closed { retained_output: RetainedShutdownOutput },
}

ShutdownThreadRequirement =
  OperationTerminal(MutationAckRequirement::OperationTerminal)
  | SessionClosed(MutationAckRequirement::ThreadCursor)
  | CatalogClosed(MutationAckRequirement::HostReceipt)

ShutdownMissing =
  Thread {
    thread_id: SurfaceThreadId,
    owner_epoch: ThreadOwnerEpoch,
    requirement: ShutdownThreadRequirement,
  }
  | HostLifecycle {
      requirement: MutationAckRequirement::HostReceipt,
    }

ShutdownScope =
  CloseThread { thread_id: SurfaceThreadId, owner_epoch: ThreadOwnerEpoch }
  | ShutdownHost { host_incarnation: HostIncarnation }

PolicyRevocationSubject =
  OwnerLease(UuidV7)
  | Resource(NonEmptyText)
```

`ShutdownOperationPlan.requirement` is byte-identical to its operation id,
owner epoch, and terminal commit id. `PlannedFinalization` additionally fixes
one finalize intent and selected cause before any work is signalled; an
`ExistingWinning` cause must equal the already committed control/finalizer cause,
while a Requested cause must equal the enclosing CloseThread or ShutdownHost
scope. Satisfying a `PlannedFinalization` OperationTerminal requirement also
loads the terminal's `OperationFinalizationRecord` and proves that finalize
intent, terminal commit, selected cause, and expected settlements equal the
plan; a cursor/commit-id match alone is insufficient.
`ShutdownThreadRequirement.flatten()` is a bijection to the embedded
`MutationAckRequirement`. Every `ShutdownMissing::Thread` wrapper has the exact
same thread id/owner epoch as its planned thread and embedded requirement;
`HostLifecycle` wraps exactly the plan's `final_host_lifecycle`. A wrapper,
embedded requirement, or plan mismatch is a construction error; HostLifecycle
is illegal for CloseThread. A CloseThread plan contains exactly one thread whose
id/owner epoch equal the command scope and never contains HostLifecycle. A
ShutdownHost plan contains its required final HostLifecycle witness. Thread ids
and per-thread operation ids are sorted and unique. `plan_digest` is the
canonical digest of the variant and all fields except itself; it is immutable
across partial progress. The outer request/target/commit, deferred plan, and
repair token must agree on request, host, scope, barrier, closing commit, and
plan digest. Before signalling work, the host lifecycle store durably commits
the complete `ShutdownBarrierRecord` in Closing state. Repair loads that record
by barrier/closing commit and never re-enumerates threads, operations, causes,
settlements, or commit ids. Closed stores the exact retained typed output. Any
mismatch is rejected before probing a witness.

```text
SurfaceMutationErrorCode =
  InvalidRequest
  | InvalidInput
  | CommitBatchTooLarge
  | InvalidContent
  | UnsupportedContent
  | UnsupportedOperation
  | CapabilityDenied
  | WrongHost
  | WrongThread
  | WrongAttachment
  | WrongOwnerEpoch
  | UnknownOperation
  | UnknownGeneration
  | UnknownInteraction
  | UnknownTask
  | UnknownWorkflow
  | UnknownGoal
  | NoActiveGoal
  | UnknownSession
  | StaleFence
  | StaleRevision
  | StaleLease
  | StaleResponseRoute
  | WrongInteractionKind
  | WrongResponseToken
  | WrongAuthorityFingerprint
  | IllegalState
  | OperationAlreadyTerminal
  | OperationActive
  | OperationNotInterrupted
  | OperationNotSteerable
  | AdmissionClosed
  | CapacityExceeded
  | ThreadOwnedElsewhere
  | ThreadClosed
  | HostShuttingDown
  | CommitFailed
  | StoreUnavailable
  | RuntimeUnavailable
  | StalePublisherPermit

SurfaceMutationRevision =
  Thread { cursor: SurfaceCursor }
  | Host { host_incarnation: HostIncarnation, revision: HostRevisionWitness }
  | SessionCatalog { revision: SessionCatalogRevision }
  | McpCatalog { thread_id: SurfaceThreadId, revision: McpCatalogRevision }
  | InputCatalog { revision: InputCatalogRevision }
  | WorkflowCatalog { revision: WorkflowCatalogRevision }
  | SessionMetadata {
      thread_id: SurfaceThreadId,
      revision: SessionMetadataRevision,
    }
  | Settings {
      host_incarnation: HostIncarnation,
      thread_id: Option<SurfaceThreadId>,
      revision: SettingsRevision,
    }
  | Trust {
      canonical_path: CanonicalPath,
      revision: TrustRevision,
      policy_epoch: PolicyEpoch,
    }
  | Memory {
      scope: User | Project { root: CanonicalPath },
      revision: MemoryRevision,
    }
  | ProjectRootMemory {
      root: CanonicalPath,
      revision: ProjectRootMemoryRevision,
    }
  | Plan { thread_id: SurfaceThreadId, revision: PlanRevision }
  | Usage { thread_id: SurfaceThreadId, revision: UsageRevision }
  | Context { thread_id: SurfaceThreadId, revision: ContextRevision }
  | Goal {
      goal_id: SurfaceGoalId,
      revision: GoalRevision,
      owner_epoch: GoalOwnerEpoch,
    }
  | Task { thread_id: SurfaceThreadId, revision: TaskRevision }
  | Workflow {
      thread_id: SurfaceThreadId,
      workflow_run_id: SurfaceWorkflowRunId,
      revision: WorkflowRevision,
    }
  | Interaction {
      thread_id: SurfaceThreadId,
      interaction_id: SurfaceInteractionId,
      revision: InteractionRevision,
      route_epoch: ResponseRouteEpoch,
    }
  | PinnedContext { thread_id: SurfaceThreadId, revision: PinnedContextRevision }

SurfaceMutationError {
  code: SurfaceMutationErrorCode,
  message: DisplayText,
  winning_request_id: Option<SurfaceRequestId>,
  current_revision: Option<SurfaceMutationRevision>,
}
```

```text
CommittedMutation {
  request_id: SurfaceRequestId,
  target: MutationTarget,
  disposition: MutationDisposition,
  acknowledgements: NonEmptyVec<MutationCommitAck>,
}

DeferredMutation {
  request_id: SurfaceRequestId,
  target: MutationTarget,
  commit_id: SurfaceCommitId,
  committed_acknowledgements: Vec<MutationCommitAck>,
  missing_acknowledgements: NonEmptyVec<MutationAckRequirement>,
  repair: DeferredRepair,
}

InvalidMutationError(SurfaceMutationError where code in {
  InvalidRequest, InvalidInput, CommitBatchTooLarge, InvalidContent, UnsupportedContent,
  UnsupportedOperation, CapabilityDenied, WrongThread, WrongAttachment,
  UnknownOperation, UnknownGeneration, UnknownInteraction, UnknownTask,
  UnknownWorkflow, UnknownGoal, NoActiveGoal, UnknownSession,
  WrongInteractionKind, WrongResponseToken, WrongAuthorityFingerprint,
  IllegalState, OperationAlreadyTerminal, OperationNotInterrupted,
  OperationNotSteerable,
})

StaleMutationError(SurfaceMutationError where code in {
  WrongHost, WrongOwnerEpoch, StaleFence, StaleRevision, StaleLease,
  StaleResponseRoute, StalePublisherPermit,
})

UnavailableMutationError(SurfaceMutationError where code in {
  OperationActive, AdmissionClosed, CapacityExceeded, ThreadOwnedElsewhere,
  ThreadClosed, HostShuttingDown, StoreUnavailable, RuntimeUnavailable,
})

CommitFailedMutationError(SurfaceMutationError where code=CommitFailed)

UncommittedMutation =
  Invalid {
    request_id: SurfaceRequestId,
    target: Option<MutationTarget>,
    error: InvalidMutationError,
  }
  | Stale {
      request_id: SurfaceRequestId,
      target: Option<MutationTarget>,
      error: StaleMutationError,
    }
  | Unavailable {
      request_id: SurfaceRequestId,
      target: Option<MutationTarget>,
      error: UnavailableMutationError,
    }
  | CommitFailed {
      request_id: SurfaceRequestId,
      target: Option<MutationTarget>,
      error: CommitFailedMutationError,
    }

RuntimeSurfaceMutationResult =
  Committed(CommittedMutation)
  | Deferred(DeferredMutation)
  | Uncommitted(UncommittedMutation)

MutationReply<T> =
  Committed { mutation: CommittedMutation, value: T }
  | Deferred {
      mutation: DeferredMutation,
      partial: DeferredCommandValue<T>,
    }
  | Uncommitted { mutation: UncommittedMutation }

DeferredCommandValue<T> = NoValue | Provisional { value: T }

RetainedMutationReplay<T> {
  request_id: SurfaceRequestId,
  canonical_command_digest: Sha256Digest,
  target: MutationTarget,
  value: T,
  acknowledgements: NonEmptyVec<MutationCommitAck>,
}
```

`T` is a closed command-specific output. `Committed` contains exactly the
command row's required acknowledgement set in barrier order and no duplicate
acknowledgement identity. `Deferred.committed_acknowledgements` is the monotonic
unique order-preserving proved subsequence for the same request and commit;
`missing_acknowledgements` is its exact nonempty complement. A command row also
fixes whether `NoValue` or a named `Provisional<T>` is legal. `Uncommitted`
contains no cursor, receipt, or durability claim.
`DeferredRepair` is the sole state/token representation. Its variant fields
must match byte-for-byte on every duplicated identity; there is no separately
selectable `state + retry` pair.
For every variant, the outer request/target/commit equals the token identity.
Thread/Host Mutation compare settlement and expected commit; Projection compares
durable commit, family, and event; RemoteOwner compares owner epoch and durable
revision; Start compares fence and Started commit; Finalization compares
operation, finalize intent, terminal commit, and missing-set digest; MemoryPin
compares scope, record, memory revision, pin thread, and owner epoch; Policy
compares the complete immutable barrier plan and expected commit; Shutdown
compares host, scope, barrier, closing commit, and immutable barrier-plan digest. Any mismatch is
an internal construction failure, not another Deferred value.
The four uncommitted code sets are disjoint and their union is exactly
`SurfaceMutationErrorCode`; construction fails for any cross-disposition pair.

Every mutation that reaches a semantic commit retains one
`RetainedMutationReplay<T>` in its owning idempotency domain for the same window
as the request-id record. The digest covers the complete canonical command,
including caller binding and every fence, but excludes transport framing. After
a repair proves the missing acknowledgements, a byte-identical original command
lookup runs before ordinary current-state validation and returns
`MutationReply::Committed` with the retained command-specific `T`, exact ordered
acknowledgement vector, and the same process-local waiter identity or its durable
reconstruction. It never reruns the external mutation, allocates another
generation, changes a winning cause, or creates a second receipt. Reusing the
request id with a different command digest or target is `InvalidRequest`.

Repair commands themselves establish only the requirements named by their
tokens. Generic thread/host settlement and projection repair may return the
repaired mutation algebra, after which original-command replay returns `T`.
`RetryFinalization` returns its already closed terminal value directly.
CloseThread and ShutdownHost are the sealed-handle exception defined below:
their retained outputs are returned directly by `ReconcileHostMutation` once
the immutable shutdown barrier completes.

## Repair Tokens

```text
ReconcileMutationToken {
  request_id: SurfaceRequestId,
  target: MutationTarget,
  settlement_id: SurfaceSettlementId,
  expected_commit_id: SurfaceCommitId,
}

RetryStartCommitToken {
  request_id: SurfaceRequestId,
  thread_owner_epoch: ThreadOwnerEpoch,
  fence: SurfaceOperationFence,
  started_commit_id: SurfaceCommitId,
}

RetryProjectionSelector =
  Local { fact_family: SurfaceFactFamily, event_id: SurfaceEventId }
  | Remote { durable_revision: DurableRevision }

RetryProjectionToken {
  request_id: SurfaceRequestId,
  target: MutationTarget,
  durable_commit_id: SurfaceCommitId,
  expected_thread_owner_epoch: ThreadOwnerEpoch,
  selector: RetryProjectionSelector,
}

RetryFinalizationToken {
  request_id: SurfaceRequestId,
  thread_id: SurfaceThreadId,
  operation_id: SurfaceOperationId,
  finalize_intent_id: SurfaceFinalizeIntentId,
  terminal_commit_id: SurfaceCommitId,
  expected_thread_owner_epoch: ThreadOwnerEpoch,
  missing_set_digest: Sha256Digest,
}

ReconcileHostMutationToken {
  Settlement {
    request_id: SurfaceRequestId,
    target: MutationTarget,
    settlement_id: SurfaceSettlementId,
    host_incarnation: HostIncarnation,
    expected_commit_id: SurfaceCommitId,
  }
  | Shutdown {
      request_id: SurfaceRequestId,
      host_incarnation: HostIncarnation,
      scope: ShutdownScope,
      barrier_id: SurfaceSettlementId,
      closing_commit_id: SurfaceCommitId,
      barrier_plan_digest: Sha256Digest,
    }
}

ReconcileMemoryMutationToken {
  request_id: SurfaceRequestId,
  scope: User | Project { root: CanonicalPath },
  memory_revision: MemoryRevision,
  record_id: SurfaceCatalogEntryId,
  pin_thread_id: SurfaceThreadId,
  expected_thread_owner_epoch: ThreadOwnerEpoch,
  expected_commit_id: SurfaceCommitId,
}

ReconcileFolderTrustRevocationToken {
  request_id: SurfaceRequestId,
  expected_commit_id: SurfaceCommitId,
  plan: PolicyRevocationBarrierPlan,
}

DeferredRepair =
  ThreadMutation {
    state: DeferredMutationState::MutationDegraded,
    token: ReconcileMutationToken,
  }
  | HostMutation {
      state: DeferredMutationState::MutationDegraded,
      token: ReconcileHostMutationToken::Settlement,
    }
  | Projection {
      state: DeferredMutationState::ProjectionDegraded,
      token: RetryProjectionToken { selector: Local },
    }
  | TerminalProjection {
      state: DeferredMutationState::FinalizingDegraded {
        cause: FinalizationDegradedCause::TerminalProjectionPending,
      },
      token: RetryProjectionToken { selector: Local },
    }
  | RemoteOwner {
      state: DeferredMutationState::OwnerAckPending,
      token: RetryProjectionToken { selector: Remote },
    }
  | Start {
      state: DeferredMutationState::StartCommitDegraded,
      token: RetryStartCommitToken,
    }
  | Finalization {
      state: DeferredMutationState::FinalizingDegraded {
        cause: FinalizationDegradedCause::MissingFinalization,
      },
      token: RetryFinalizationToken,
    }
  | MemoryPin {
      state: DeferredMutationState::MemoryPinPending,
      token: ReconcileMemoryMutationToken,
    }
  | Policy {
      state: DeferredMutationState::PolicyRevocationPending,
      token: ReconcileFolderTrustRevocationToken,
    }
  | Shutdown {
      state: DeferredMutationState::ShutdownDeferred,
      token: ReconcileHostMutationToken::Shutdown,
    }
```

Every repair token is opaque to external wire clients even though its private
fields are exact here. Every `DeferredMutation` returns the complete matching
`DeferredRepair`; clients never infer owner epochs, commit ids, or fences
from the display state. Repair commands accept only the original token and never
allocate a new semantic request id. `ShutdownDeferred` uses the
`ReconcileHostMutationToken::Shutdown` composite token, whose scope, barrier,
and immutable plan digest cover the complete expected thread/owner/operation/
acknowledgement plan. Progress never changes token identity; current missing work
is always `expected - proved`. It remains valid for a zero-thread host whose
only missing witness is final HostLifecycle. Each pending operation contributes
its matching `OperationTerminal` requirement. Recorded threads, but never
ephemeral threads, contribute a SessionCatalog receipt. Canonical barrier order
is thread id, then per-thread operation id terminals, Session Closed,
recorded-only catalog Closed, and finally HostLifecycle. The flattened outer
`missing_acknowledgements` is exactly `ShutdownDeferred.missing`; neither is a
second authority. `PolicyRevocationPending.pending` and
`FinalizationDegradedCause::MissingFinalization.missing_settlements` are
nonempty by construction; an empty set must reconcile directly to `Committed`.
`TerminalProjectionPending` carries no settlement set and cannot be paired with
`RetryFinalization`.

For policy repair,
`FolderTrustMutationOutput.barrier_plan == PolicyRevocationPending.plan ==
ReconcileFolderTrustRevocationToken.plan ==` the requirement/ack plan, and the
token's `expected_commit_id` equals the outer commit. Output/state `pending` is
the exact plan-minus-settled view. For shutdown repair, the token's
`closing_commit_id` equals the plan and outer deferred commit; the token scope,
barrier id, host incarnation, and plan digest are byte-identical to that plan.

## Thread Command Payloads

```text
BackgroundTarget =
  ReservedOperation {
    operation_id: SurfaceOperationId,
    admission_lease_id: SurfaceAdmissionLeaseId,
  }
  | ActiveGeneration { fence: SurfaceOperationFence }

ResumeSourceWitness =
  DurableReplay { replayability_digest: Sha256Digest }
  | LiveCapsule { incarnation: SurfaceIncarnation }

InteractionSelector =
  Exact {
    interaction_id: SurfaceInteractionId,
    expected_revision: InteractionRevision,
    kind: SurfaceInteractionKind,
    response_token: SurfaceResponseToken,
    response_route_epoch: ResponseRouteEpoch,
    response_grant_token: SurfaceResponseGrantToken,
    operation_fence: SurfaceOperationFence,
  }
  | OpaqueRequestId {
      opaque_request_id: NonEmptyText,
      expected_kind: SurfaceInteractionKind,
    }

TaskControlAction =
  Stop { fence: SurfaceTaskFence }
  | Foreground { fence: SurfaceTaskFence }

WorkflowControlAction =
  Launch {
    catalog_entry_id: SurfaceCatalogEntryId,
    observed_catalog_revision: WorkflowCatalogRevision,
    args: Vec<(NonEmptyText, DisplayText)>,
    parent: Option<SurfaceOperationFence>,
  }
  | Pause { fence: SurfaceWorkflowFence }
  | Resume { fence: SurfaceWorkflowFence }
  | Stop { fence: SurfaceWorkflowFence }

GoalRunInput =
  Supplied { request: SurfaceInputRequest }
  | DerivedFromGoal {
      goal_id: SurfaceGoalId,
      objective_revision: GoalObjectiveRevision,
      goal_receipt_digest: Sha256Digest,
    }

GoalMutationAction =
  SetAndRun {
    expected_goal: None | Exact(SurfaceGoalFence),
    objective: NonEmptyText,
    token_budget: Option<i64>,
    input: GoalRunInput,
  }
  | Edit {
      fence: SurfaceGoalFence,
      objective: NonEmptyText,
      token_budget: Keep | Set(Option<i64>),
    }
  | Clear { fence: SurfaceGoalFence }
  | ResumeAndRun {
      fence: SurfaceGoalFence,
      input: GoalRunInput,
    }

RuntimeSettingsPatch =
  SetModel { model: NonEmptyText }
  | SetReasoning { effort: SurfaceReasoningEffort }
  | SetApprovalMode { mode: SurfaceApprovalMode }
  | SetCwd { cwd: CanonicalPath }
  | SetWorkspaceRoots { roots: Vec<CanonicalPath> }
  | SetActivePermissionProfile {
      profile: Option<SurfaceActivePermissionProfile>,
    }
  | ReplacePermissionRules { rules: Vec<SurfacePermissionRule> }
  | ReplaceAdditionalWorkingDirectories {
      directories: Vec<SurfaceAdditionalWorkingDirectory>,
    }
  | ReplaceNetworkPermissions { permissions: SurfaceNetworkPermissions }
  | ApplyPermissionUpdate { update: SurfacePermissionUpdate }

SurfaceSettingsDestination = Session | UserSettings | ProjectSettings | LocalSettings

SurfacePermissionRuleSelector {
  tool: NonEmptyText,
  pattern: Option<NonEmptyText>, // None selects every pattern for this tool
}

SurfacePermissionUpdate =
  AddRules {
    destination: SurfaceSettingsDestination,
    decision: SurfacePermissionDecision,
    rules: NonEmptyVec<SurfacePermissionRuleSelector>,
  }
  | ReplaceRules {
      destination: SurfaceSettingsDestination,
      decision: SurfacePermissionDecision,
      rules: Vec<SurfacePermissionRuleSelector>,
    }
  | RemoveRules {
      destination: SurfaceSettingsDestination,
      decision: SurfacePermissionDecision,
      rules: NonEmptyVec<SurfacePermissionRuleSelector>,
    }
  | SetMode {
      destination: SurfaceSettingsDestination,
      mode: SurfaceApprovalMode,
    }
  | AddDirectories {
      destination: SurfaceSettingsDestination,
      directories: NonEmptyVec<SurfaceAdditionalWorkingDirectory>,
    }
  | RemoveDirectories {
      destination: SurfaceSettingsDestination,
      paths: NonEmptyVec<CanonicalPath>,
    }

PinnedContextAction =
  Add {
    expected_revision: PinnedContextRevision,
    entry: SurfacePinnedContextEntry,
    memory_receipt: Option<(SurfaceCatalogEntryId, MemoryRevision)>,
  }
  | Remove {
      expected_revision: PinnedContextRevision,
      entry_id: SurfaceCatalogEntryId,
    }
  | Clear { expected_revision: PinnedContextRevision }

McpCatalogFamily = Tools | Resources | ResourceTemplates

McpCatalogCursor {
  thread_id: SurfaceThreadId,
  revision: McpCatalogRevision,
  family: McpCatalogFamily,
  offset: u64,
  cursor_authenticator: OpaqueToken,
}

McpCatalogQuery =
  ListTools { cursor: Option<McpCatalogCursor>, limit: u32 }
  | ListResources { cursor: Option<McpCatalogCursor>, limit: u32 }
  | ListResourceTemplates { cursor: Option<McpCatalogCursor>, limit: u32 }
  | Lookup { id: SurfaceCatalogEntryId }
```

`DerivedFromGoal` is not an adapter prompt shortcut. Before reservation the Goal
coordinator validates the exact post-commit receipt and deterministically
constructs one `SurfaceInputRequest` from the objective revision under the
contract-versioned codec, then freezes its request digest in the generation
capsule. A stale receipt cannot fall back to a caller-composed prompt.

```text
SurfaceCommand =
  ReserveOperation {
    request_id: SurfaceRequestId,
    caller: SurfaceBoundCaller,
    intent: OperationRequestIntent,
  }
  | AdmitReserved {
      request_id: SurfaceRequestId,
      caller: SurfaceBoundCaller,
      operation_id: SurfaceOperationId,
      admission_lease_id: SurfaceAdmissionLeaseId,
    }
  | CancelOperation {
      request_id: SurfaceRequestId,
      caller: SurfaceBoundCaller,
      operation_id: SurfaceOperationId,
    }
  | CancelSessionCurrent {
      request_id: SurfaceRequestId,
      caller: SurfaceBoundCaller,
      legacy_rpc_id_digest: Sha256Digest,
    }
  | InterruptGeneration {
      request_id: SurfaceRequestId,
      caller: SurfaceBoundCaller,
      fence: SurfaceOperationFence,
    }
  | PauseGoalOperation {
      request_id: SurfaceRequestId,
      caller: SurfaceBoundCaller,
      goal_fence: SurfaceGoalFence,
    }
  | ResumeOperation {
      request_id: SurfaceRequestId,
      caller: SurfaceBoundCaller,
      operation_id: SurfaceOperationId,
      expected_last_generation: SurfaceGenerationId,
      resume_source: ResumeSourceWitness,
    }
  | SteerOperation {
      request_id: SurfaceRequestId,
      caller: SurfaceBoundCaller,
      fence: SurfaceOperationFence,
      input: SurfaceInputRequest,
    }
  | TransferBackground {
      request_id: SurfaceRequestId,
      caller: SurfaceBoundCaller,
      target: BackgroundTarget,
    }
  | RespondInteraction {
      request_id: SurfaceRequestId,
      caller: SurfaceBoundCaller,
      selector: InteractionSelector,
      response: BoundInteractionResponse,
    }
  | ReconcileMutation {
      token: ReconcileMutationToken,
    }
  | RetryStartCommit {
      token: RetryStartCommitToken,
    }
  | RetryProjection {
      token: RetryProjectionToken,
    }
  | RetryFinalization {
      token: RetryFinalizationToken,
    }
  | ManualCompact {
      request_id: SurfaceRequestId,
      caller: SurfaceBoundCaller,
      expected_context_revision: ContextRevision,
    }
  | Backtrack {
      request_id: SurfaceRequestId,
      caller: SurfaceBoundCaller,
      expected_cursor: SurfaceCursor,
      target: LastUserTurn,
    }
  | TaskControl {
      request_id: SurfaceRequestId,
      caller: SurfaceBoundCaller,
      action: TaskControlAction,
    }
  | WorkflowControl {
      request_id: SurfaceRequestId,
      caller: SurfaceBoundCaller,
      action: WorkflowControlAction,
    }
  | GoalMutation {
      request_id: SurfaceRequestId,
      caller: SurfaceBoundCaller,
      action: GoalMutationAction,
    }
  | SettingsMutation {
      request_id: SurfaceRequestId,
      caller: SurfaceHostBoundCaller,
      host_incarnation: HostIncarnation,
      expected_thread_revision: SettingsRevision,
      patch: RuntimeSettingsPatch,
    }
  | McpCatalogQuery {
      request_id: SurfaceRequestId,
      caller: SurfaceBoundCaller,
      expected_revision: Option<McpCatalogRevision>,
      query: McpCatalogQuery,
    }
  | PinnedContextMutation {
      request_id: SurfaceRequestId,
      caller: SurfaceBoundCaller,
      action: PinnedContextAction,
    }
```

`SettingsMutation` is callable only by the owning host facade while completing
an `UpdateRuntimeSettings` composite. An attachment cannot bypass the host
revision by constructing it directly.

For `RespondInteraction`, the attachment-bound client handle injects the
`SurfaceBoundCaller`, current response route/grant token, and the runtime-issued
response identity, persisted answer policy, and applicable persisted authority
fingerprint. Wire adapters may provide only the opaque request id or typed
client answer; they cannot supply, replace, or deserialize those authority
values. The actor compares every injected value with current broker/request
state before constructing `ValidatedInteractionResponse`. A retry reuses the
original `request_id` and response identity through the same bound handle.

## Thread Command Outputs

```text
OperationWaiterHandle(opaque, cloneable, process-local)

ReservedOperationOutput {
  operation_id: SurfaceOperationId,
  lease: ReservationLease,
  requested_cursor: SurfaceCursor,
  waiter: OperationWaiterHandle,
}

AdmissionOutput =
  Queued {
    operation_id: SurfaceOperationId,
    queue_position: u32,
    lease: ReservationLease,
    waiter: OperationWaiterHandle,
  }
  | Admitted {
      operation_id: SurfaceOperationId,
      first_generation: SurfaceOperationFence,
      admitted_cursor: SurfaceCursor,
      waiter: OperationWaiterHandle,
    }

CancelOperationOutput =
  CancelledBeforeAdmission {
    terminal: OperationTerminalAtCursor,
  }
  | Accepted {
    operation_id: SurfaceOperationId,
    accepted_cursor: SurfaceCursor,
    waiter: OperationWaiterHandle,
  }
  | AlreadyTerminal { terminal: OperationTerminalAtCursor }
  | FinalizationPending {
      operation_id: SurfaceOperationId,
      finalize_intent_id: SurfaceFinalizeIntentId,
      finalization_cursor: FinalizationStartedAtCursor,
      waiter: OperationWaiterHandle,
    }

CancelSessionCurrentResult =
  NoCurrentOperation {
    request_id: SurfaceRequestId,
    thread_id: SurfaceThreadId,
  }
  | Resolved { mutation: MutationReply<CancelOperationOutput> }

InterruptOutput {
  fence: SurfaceOperationFence,
  accepted_cursor: SurfaceCursor,
  settlement: SuspendUntilExplicitControl
              | TerminalizeCancelledAtInterruptedStopUnlessResumeQueued,
  waiter: OperationWaiterHandle,
}

PauseGoalOutput {
  goal: SurfaceGoal,
  goal_receipt: SurfaceGoalStoreReceipt,
  goal_cursor: SurfaceCursor,
  operation: None
             | CancelledBeforeAdmission {
                 terminal: OperationTerminalAtCursor,
               }
             | Cancelling {
                 operation_id: SurfaceOperationId,
                 accepted_cursor: SurfaceCursor,
                 waiter: OperationWaiterHandle,
               },
}

ResumeTransitionRole = ResumeStarting | GenerationReserved | GenerationStarted

ResumeTransitionReceipt {
  role: ResumeTransitionRole,
  event_id: SurfaceEventId,
  cursor: SurfaceCursor,
  commit_class: CommitClass,
}

ResumeOperationOutput {
  operation_id: SurfaceOperationId,
  generation: SurfaceOperationFence,
  resume_starting: ResumeTransitionReceipt,
  generation_reserved: ResumeTransitionReceipt,
  generation_started: ResumeTransitionReceipt,
  waiter: OperationWaiterHandle,
}

SteerOutput {
  fence: SurfaceOperationFence,
  input_item_id: SurfaceItemId,
  committed_cursor: SurfaceCursor,
}

TransferBackgroundOutput =
  QueuedOnStart {
    operation_id: SurfaceOperationId,
    intent_cursor: SurfaceCursor,
  }
  | HandedOff {
      background_fence: SurfaceBackgroundFence,
      handoff_cursor: SurfaceCursor,
      waiter: OperationWaiterHandle,
    }

RespondInteractionOutput {
  interaction_id: SurfaceInteractionId,
  attempted_response_id: SurfaceResponseId,
  disposition: Resolved {
                 receipt: SurfaceInteractionResolutionReceipt,
               }
               | AlreadyResolved {
                   winning_receipt: SurfaceInteractionResolutionReceipt,
                 },
  projected_cursor: Option<SurfaceCursor>,
}

MaintenanceOperationOutput {
  operation_id: SurfaceOperationId,
  admitted_cursor: SurfaceCursor,
  waiter: OperationWaiterHandle,
}

TaskControlOutput { task: SurfaceTask, cursor: SurfaceCursor }

WorkflowControlOutput {
  workflow: SurfaceWorkflow,
  operation_id: Option<SurfaceOperationId>,
  cursor: SurfaceCursor,
  waiter: Option<OperationWaiterHandle>,
}

GoalMutationOutput {
  goal: Option<SurfaceGoal>,
  goal_receipt: SurfaceGoalStoreReceipt,
  change_cursor: SurfaceCursor,
  operation_id: Option<SurfaceOperationId>,
  waiter: Option<OperationWaiterHandle>,
}

SettingsMutationOutput {
  settings: SurfaceSettingsSnapshot,
  cursor: SurfaceCursor,
}

McpCatalogPage {
  revision: McpCatalogRevision,
  values: Tools(Vec<SurfaceMcpTool>)
          | Resources(Vec<SurfaceMcpResource>)
          | ResourceTemplates(Vec<SurfaceMcpResourceTemplate>)
          | Entry(SurfaceCatalogEntry),
  next_cursor: Option<McpCatalogCursor>,
}

SurfaceCatalogEntry =
  McpTool(SurfaceMcpTool)
  | McpResource(SurfaceMcpResource)
  | McpResourceTemplate(SurfaceMcpResourceTemplate)

PinnedContextMutationOutput {
  snapshot: SurfacePinnedContextSnapshot,
  cursor: SurfaceCursor,
}
```

`RespondInteractionOutput.attempted_response_id` equals the bound response id.
For Resolved, the receipt response id equals it and the receipt kind/safe
projection equal the request. For AlreadyResolved, `winning_receipt` is the
byte-identical persisted winner and may carry a different response id; the
attempted answer body is never returned. `projected_cursor` is present only for
the local-owner committed projection and equals its containing batch head;
remote-owner and projection-deferred branches use None.

`McpCatalogQuery::Lookup` returns `Found(McpCatalogPage::Entry(entry))` on a
hit and the outer `SurfaceReadResult::NotFound` on a miss. `Entry` is never
optional, so there is one normative miss encoding.

`ManualCompact` and `Backtrack` are short runtime maintenance operations. They
allocate a normal globally unique operation and generation, return its admitted
cursor and waiter before waiting, commit Started before changing conversation
state, emit generation-scoped Item/Context facts, and commit one Terminal. This
preserves the approved Item scope rule without using a stale historical
generation. Restored composer input is read from the typed post-terminal
snapshot; there is no client-created second terminal.

## Thread Command Contract Matrix

All mutation commands use their `request_id` as the primary idempotency key in
the named target domain. The listed additional identity is part of the key and
cannot be changed on retry. `Target` is the exact `MutationTarget`; `Acks` are
the ordered, non-duplicated acknowledgements required for `Committed`; and
`Deferred` names the only legal partial value/state. A command that has not
listed a disposition cannot return it as a normal result. Every mutating row has
one additional common structural precommit error,
`Invalid(CommitBatchTooLarge)`, under the exact preflight rule above; it is
omitted from the per-row semantic error cell and can never accompany a fact,
receipt, or Deferred value.

| Command | Target | Required capability and fence | Legal source state | Normal dispositions | Result | Acks | Legal deferred value/state | Closed uncommitted errors | Emitted authoritative facts |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| ReserveOperation | `Thread -> Operation` | `SubmitOperation`; bound caller/thread grant | admission open; reservation capacity; actor-ordered busy policy | Accepted | `MutationReply<ReservedOperationOutput>` | no overrides: `ThreadCursor(Operation)`; `ApplyThreadOverridesBeforeRequested`: `HostReceipt(RuntimeSettings)` then `ThreadCursor(Settings)` then `ThreadCursor(Operation)` | `NoValue / ProjectionDegraded` or settings preparation `NoValue / MutationDegraded` | InvalidInput, InvalidContent, UnsupportedContent, CapabilityDenied, WrongAttachment, OperationActive, AdmissionClosed, CapacityExceeded, ThreadClosed, StaleRevision | Operation Requested, or no fact for pre-Requested busy rejection |
| AdmitReserved | `Operation` | bound operation + exact lease | matching Requested, unexpired lease | Queued, Accepted, AlreadyApplied | `MutationReply<AdmissionOutput>` | Queued: `ThreadCursor(Operation)`; Accepted: `ThreadCursor(Operation)` then `ThreadCursor(Item)` | `NoValue / ProjectionDegraded` | UnknownOperation, StaleLease, IllegalState, AdmissionClosed, StaleRevision | queue change, or Admitted carrying the first Reserved generation plus user Item; never a duplicate GenerationReserved patch |
| CancelOperation | `Operation` | bound/visible control grant + operation id | any known operation from Requested through Terminal | Accepted, AlreadyApplied | `MutationReply<CancelOperationOutput>` | Requested: `ThreadCursor(Operation)` then `OperationTerminal`; first live cancel: control `ThreadCursor(Operation)`; Finalizing/FinalizingDegraded: existing `FinalizationStartedAtCursor`; Terminal: existing `OperationTerminal` | first live cancel only: `NoValue / ProjectionDegraded` for the control projection; finalizing/terminal replay cannot defer | UnknownOperation, CapabilityDenied, WrongThread, IllegalState | Requested synchronously commits `Terminal(NotAdmitted(CancelledBeforeAdmission))`; a first live cancel commits terminalizing control; Finalizing and Terminal emit no new fact or cause |
| CancelSessionCurrent | `Thread` then resolved `Operation` | `LegacyCancelCurrent`; handle-bound connection identity | any live actor state; no-current is a read-only actor lookup | Accepted, AlreadyApplied | `CancelSessionCurrentResult` | resolved branch uses the exact `CancelOperation` branch witnesses, including `OperationTerminal` for Requested; no-current has none | resolved later-phase branch uses the exact CancelOperation deferred policy; no-current and Requested-direct branches cannot defer | WrongAttachment, CapabilityDenied, ThreadClosed | exact resolved CancelOperation result/fact, or typed no-current lookup with no fact |
| InterruptGeneration | `Generation` | bound control + exact generation fence | Reserved or Started; duplicate Suspended is AlreadyApplied | Accepted, AlreadyApplied | `MutationReply<InterruptOutput>` | `ThreadCursor(Operation)` | `NoValue / ProjectionDegraded` | UnknownGeneration, StaleFence, IllegalState, CapabilityDenied | interrupt control, Stopped, then Suspended or legacy Terminal path |
| PauseGoalOperation | `Goal` plus active `Operation` when present | ManageGoal + Goal fence | matching Goal in any non-Complete state | Accepted, AlreadyApplied | `MutationReply<PauseGoalOutput>` | `GoalStoreReceipt`, then `ThreadCursor(Goal)`; Requested operation then adds `ThreadCursor(Operation)` + `OperationTerminal`; later active operation adds control `ThreadCursor(Operation)` | later active operation only: `NoValue / ProjectionDegraded` for Goal/control projection; terminal settlement/finalization failure is reported by the operation waiter/health repair, not this acceptance command | UnknownGoal, StaleRevision, WrongOwnerEpoch, IllegalState | Goal Paused; Requested operation directly commits `Terminal(NotAdmitted(CancelledBeforeAdmission))` with no control intent, while later phases commit the exact GoalPause terminalizing intent |
| ResumeOperation | `Operation` | bound control + operation/last generation + exact `ResumeSourceWitness` | `Suspended{cause=Interrupted|RecoveryRequired|ProviderSuspended}` with matching durable replay or current-incarnation live capsule | Accepted, AlreadyApplied | `MutationReply<ResumeOperationOutput>` | `ThreadCursor(Operation)` for ResumeStarting, Reserved, then Started | `NoValue / StartCommitDegraded` | UnknownOperation, StaleFence, InvalidInput, IllegalState, OperationAlreadyTerminal | ResumeStarting, next Reserved, Started |
| SteerOperation | `Generation` | bound control + exact Started fence | Started, not terminalizing or transferred | Accepted | `MutationReply<SteerOutput>` | `ThreadCursor(Operation)`, then `ThreadCursor(Item)` | `NoValue / ProjectionDegraded` | UnknownGeneration, StaleFence, InvalidInput, IllegalState | canonical steer Item plus actor delivery fact |
| TransferBackground | `Operation` or `Generation` | bound control + lease or exact fence | Requested/Reserved/Started foreground | Queued, Accepted, AlreadyApplied | `MutationReply<TransferBackgroundOutput>` | `ThreadCursor(Operation)` | `NoValue / ProjectionDegraded` | UnknownOperation, StaleLease, StaleFence, CapacityExceeded, IllegalState | BackgroundOnStart or Transferred |
| RespondInteraction | `Interaction` | `RespondGrantedInteraction` + exact/opaque selector + response id | matching Requested/Transferred interaction and live route grant | Accepted, AlreadyApplied | `MutationReply<RespondInteractionOutput>` | `ThreadCursor(Interaction)` when the owner is local, or `ThreadRemoteOwnerAck` when the owner is remote | local: `NoValue / ProjectionDegraded`; remote: `NoValue / OwnerAckPending` | InvalidInput, UnknownInteraction, WrongInteractionKind, WrongResponseToken, StaleResponseRoute, WrongAuthorityFingerprint, WrongAttachment | Resolved with safe receipt or idempotent AlreadyResolved with the winning safe receipt |
| ReconcileMutation | original `MutationTarget` | repair capability + exact token | matching MutationDegraded | Accepted, AlreadyApplied | `RuntimeSurfaceMutationResult` | missing host/thread receipt named by token | `NoValue / MutationDegraded` | InvalidRequest, StaleRevision, IllegalState | missing receipt and projected fact only |
| RetryStartCommit | `Generation` | repair capability + exact token | matching StartCommitDegraded | Accepted, AlreadyApplied | `RuntimeSurfaceMutationResult` | `ThreadCursor(Operation)` | `NoValue / StartCommitDegraded` | InvalidRequest, StaleFence, WrongOwnerEpoch, IllegalState | same Started or Stopped + finalizer path |
| RetryProjection | original `MutationTarget` | repair capability + exact token | ProjectionDegraded, OwnerAckPending, or FinalizingDegraded(TerminalProjectionPending) | Accepted, AlreadyApplied | `RuntimeSurfaceMutationResult` | repaired `ThreadCursor` or remote owner ack; terminal repair requires `ThreadCursor(Operation)` plus `OperationTerminal` | `NoValue / ProjectionDegraded`, `NoValue / OwnerAckPending`, or `NoValue / FinalizingDegraded(TerminalProjectionPending)` | InvalidRequest, StaleRevision, WrongOwnerEpoch, IllegalState | same fact projection/ack only; terminal repair projects the already-durable Terminal and appends no fact |
| RetryFinalization | `Operation` | repair capability + exact token | matching FinalizingDegraded(MissingFinalization); terminal commit id is fixed | Accepted, AlreadyApplied | `MutationReply<OperationTerminalAtCursor>` | `ThreadCursor(Operation)` and `OperationTerminal` | `NoValue / FinalizingDegraded(MissingFinalization)` | InvalidRequest, StaleRevision, WrongOwnerEpoch, IllegalState | missing settlement and Terminal only; never FinalizationStarted |
| ManualCompact | `Thread -> Operation` | SubmitOperation; current `ContextRevision` | idle/admissible thread | Accepted | `MutationReply<MaintenanceOperationOutput>` | `ThreadCursor(Operation)` | `Never`; later start/finalization failure is reported only by the operation waiter/terminal barrier | StaleRevision, OperationActive, AdmissionClosed | maintenance operation, Context/Item patches, Terminal |
| Backtrack | `Thread -> Operation` | SubmitOperation; expected cursor | idle/admissible thread with a user turn | Accepted | `MutationReply<MaintenanceOperationOutput>` | `ThreadCursor(Operation)` | `Never`; later start/finalization failure is reported only by the operation waiter/terminal barrier | StaleRevision, OperationActive, InvalidInput, AdmissionClosed | maintenance operation, Item removals, Terminal |
| TaskControl | `Task` | ManageTask + Task fence | Stop: nonterminal; Foreground: exact background owner | Accepted, AlreadyApplied | `MutationReply<TaskControlOutput>` | `ThreadCursor(Task)` | `NoValue / ProjectionDegraded` | UnknownTask, StaleRevision, StaleFence, IllegalState | Task StatusChanged/OwnershipChanged |
| WorkflowControl | `Workflow` plus optional `Operation` | ManageWorkflow + catalog/revision/parent fence | action-specific matching workflow state | Accepted, AlreadyApplied | `MutationReply<WorkflowControlOutput>` | `ThreadCursor(Workflow)`; Launch also `ThreadCursor(Task)` then `ThreadCursor(Operation)` | `NoValue / ProjectionDegraded` | UnknownWorkflow, StaleRevision, StaleFence, InvalidInput, IllegalState | Workflow patch; Launch also operation/Task facts |
| GoalMutation | `Goal` plus optional `Operation` | ManageGoal + expected Goal fence | action-specific Goal state | Accepted, AlreadyApplied | `MutationReply<GoalMutationOutput>` | `GoalStoreReceipt`, then `ThreadCursor(Goal)`; run variants also `ThreadCursor(Operation)` | `NoValue / ProjectionDegraded` | UnknownGoal, StaleRevision, WrongOwnerEpoch, InvalidInput, IllegalState | post-commit Goal changes; run variants also operation facts |
| SettingsMutation | `Thread` | host-only thread settings capability + `SurfaceHostBoundCaller` + host incarnation/revision | live thread; no conflicting mutation | Accepted, AlreadyApplied | `MutationReply<SettingsMutationOutput>` | `HostReceipt(RuntimeSettings)` then `ThreadCursor(Settings)` | `Provisional(SettingsMutationOutput) / ProjectionDegraded` | WrongHost, StaleRevision, InvalidInput, CapabilityDenied, ThreadClosed | Settings Committed/PendingChanged |
| McpCatalogQuery | `Thread` | ReadCatalog + optional catalog revision | attached thread | Found | `SurfaceReadResult<McpCatalogPage>` | none | n/a | InvalidRequest, CapabilityDenied, NotFound, StaleRevision, ThreadClosed, StoreUnavailable, RuntimeUnavailable | no mutation |
| PinnedContextMutation | `Thread` plus memory receipt when supplied | `ManagePinnedContext`; memory-backed Add additionally requires `ManageMemory` | live thread; expected revision; memory receipt required when memory-backed | Accepted, AlreadyApplied | `MutationReply<PinnedContextMutationOutput>` | non-memory: `ThreadCursor(PinnedContext)`; memory-backed Add: `HostReceipt(Memory)` then `ThreadCursor(PinnedContext)` | non-memory: `NoValue / ProjectionDegraded`; memory-backed: `Provisional(PinnedContextMutationOutput) / MemoryPinPending` | StaleRevision, InvalidInput, CapabilityDenied, ThreadClosed | Pinned Added/Removed/Reconciled |

`CancelOperation` is explicitly idempotent after Terminal: it returns
`AlreadyApplied` with `CancelOperationOutput::AlreadyTerminal` and the existing
terminal cursor, never rewrites the outcome. `RetryFinalization` uses the stable
`RetryFinalizationToken.terminal_commit_id`; it probes/settles the existing
finalizer and appends only the missing Terminal. Close/shutdown commands below
are the only commands allowed to wait on a set of operation terminals.
When a new cancel first observes Finalizing or FinalizingDegraded, it returns
`AlreadyApplied(FinalizationPending)` with the record's byte-identical
`FinalizationStartedAtCursor` and waiter; it creates no control intent and does
not change the selected cause. When the request id and canonical command are the
winning cancel's exact retry, idempotency replay runs before phase validation
and returns the original `Accepted` value, cursor, acknowledgements, and waiter
even if the operation has since entered Finalizing. A mismatched request with a
reused id is `InvalidRequest`.
For a Requested operation, cancellation instead returns
`CancelOperationOutput::CancelledBeforeAdmission` after the reservation
finalizer synchronously commits the terminal and both required witnesses. It
never publishes `ControlIntentCommitted`, never enters `FinalizingDegraded`, and
never leaves a pending control on the Requested record.
For `ResumeOperation`, ResumeStarting and GenerationReserved are two distinct
event receipts in one coordinator batch and therefore share one batch-head
cursor/CommitClass while retaining different event ids and roles. If that batch
cannot be established, no successor is visible and the ordinary precommit
mutation failure path applies. GenerationStarted is the independent execution
barrier; only its missing acknowledgement can produce StartCommitDegraded.
The three named output fields have fixed roles respectively `ResumeStarting`,
`GenerationReserved`, and `GenerationStarted`; each field's event id, cursor,
and complete CommitClass is byte-identical to its corresponding
`MutationCommitAck::ThreadLocalCursor(family=Operation)` and containing event.
`ResumeOperationOutput.generation` equals the successor fence in both Reserved
and Started events, and every receipt agrees on the output operation id/fence.
Committed and AlreadyApplied return the same three exact receipts. A Deferred
StartCommit result proves the first two and names only GenerationStarted as
missing.

## Read Result Algebra

```text
SurfaceReadRevision =
  Host { host_incarnation: HostIncarnation, revision: HostRevisionWitness }
  | SessionCatalog { revision: SessionCatalogRevision }
  | McpCatalog { thread_id: SurfaceThreadId, revision: McpCatalogRevision }
  | InputCatalog { revision: InputCatalogRevision }
  | WorkflowCatalog { revision: WorkflowCatalogRevision }
  | Thread { cursor: SurfaceCursor }
  | Session { token: SessionReadToken }

SurfaceReadErrorCode =
  InvalidRequest
  | InvalidCursor
  | CapabilityDenied
  | NotFound
  | StaleRevision
  | ThreadOwnedElsewhere
  | ThreadClosed
  | StoreUnavailable
  | RuntimeUnavailable

SurfaceReadErrorClass = NotFound | Invalid | Stale | Unavailable

SurfaceReadError {
  class: SurfaceReadErrorClass,
  code: SurfaceReadErrorCode,
  message: DisplayText,
  current_revision: Option<SurfaceReadRevision>,
}

SurfaceReadResult<T> =
  Found {
    request_id: SurfaceRequestId,
    revision: SurfaceReadRevision,
    value: T,
  }
  | NotFound {
      request_id: SurfaceRequestId,
      error: SurfaceReadError,
    }
  | Invalid {
      request_id: SurfaceRequestId,
      error: SurfaceReadError,
    }
  | Stale {
      request_id: SurfaceRequestId,
      error: SurfaceReadError,
    }
  | Unavailable {
      request_id: SurfaceRequestId,
      error: SurfaceReadError,
}
```

The outer result tag and error class are one closed algebra: `NotFound` carries
only `class=NotFound` with `NotFound`; `Invalid` carries `InvalidRequest`,
`InvalidCursor`, or `CapabilityDenied`; `Stale` carries only `StaleRevision`;
and `Unavailable` carries `ThreadOwnedElsewhere`, `ThreadClosed`,
`StoreUnavailable`, or `RuntimeUnavailable`. A reducer rejects any other
class/code pairing before
returning the result, so an adapter never has to guess whether a code is a
lookup miss, validation failure, stale cursor, or unavailable store.

No read result silently falls back from a failed live snapshot to a different
store revision. A caller may issue a new read after `Unavailable` or `Stale`,
but it cannot combine values from two revisions into one response.

## Session Catalog DTOs

```text
SessionSortKey = CreatedAt | UpdatedAt | RecencyAt
SortDirection = Ascending | Descending
SessionListArchiveFilter = ActiveOnly | ArchivedOnly
SessionSearchArchiveFilter = ActiveOnly | ActiveAndArchived

SessionRelationFilter =
  DirectChildrenOf { parent_thread_id: SurfaceThreadId }
  | DescendantsOf { ancestor_thread_id: SurfaceThreadId }

SessionSetFilter<T> =
  Any
  | Match(NonEmptySet<T>)

SurfacePageLimit =
  ClientBounded { value: u32 }
  | LegacyJsonl { wire_value: u64, effective: NonZeroU64 }

SessionListFilter {
  cwd: Vec<CanonicalPath>,
  providers: SessionSetFilter<NonEmptyText>,
  models: SessionSetFilter<NonEmptyText>,
  relation: Option<SessionRelationFilter>,
  archived: SessionListArchiveFilter,
}

SessionCatalogCursor {
  catalog_revision: SessionCatalogRevision,
  sort_key: SessionSortKey,
  direction: SortDirection,
  query_digest: Sha256Digest,
  last_value_digest: Sha256Digest,
  last_thread_id: SurfaceThreadId,
  cursor_authenticator: OpaqueToken,
}

LegacyJsonlPageCursor {
  wire_value: DisplayText,
  effective_offset: u64,
}

SurfaceSessionPageCursor =
  Typed(SessionCatalogCursor)
  | LegacyJsonl(LegacyJsonlPageCursor)

SessionPageRequest {
  filters: SessionListFilter,
  search_term: Option<NonEmptyText>,
  sort_key: SessionSortKey,
  direction: SortDirection,
  cursor: Option<SurfaceSessionPageCursor>,
  limit: SurfacePageLimit, // ClientBounded is 1..=100 here
}

SessionSearchRequest {
  query: NonEmptyText,
  archived: SessionSearchArchiveFilter,
  sort_key: SessionSortKey,
  direction: SortDirection,
  cursor: Option<SurfaceSessionPageCursor>,
  limit: SurfacePageLimit, // ClientBounded is 1..=100 here
}

SurfaceSessionSummary {
  thread_id: SurfaceThreadId,
  title: DisplayText,
  cwd: CanonicalPath,
  provider: NonEmptyText,
  model: Option<NonEmptyText>,
  created_at: Rfc3339Timestamp,
  updated_at: Rfc3339Timestamp,
  parent_thread_id: Option<SurfaceThreadId>,
  forked: bool,
  archived: bool,
  approval_mode: Option<SurfaceApprovalMode>,
  active_permission_profile: Option<SurfaceActivePermissionProfile>,
  permission_rule_count: u64,
  runtime_workspace_roots: Vec<CanonicalPath>,
  additional_working_directories: Vec<SurfaceAdditionalWorkingDirectory>,
  network_permissions: SurfaceNetworkPermissions,
  message_count: u64,
  turn_count: u64,
  metadata_revision: SessionMetadataRevision,
  running: bool,
}

SurfaceSessionSummaryPage {
  catalog_revision: SessionCatalogRevision,
  data: Vec<SurfaceSessionSummary>,
  next_cursor: Option<SurfaceSessionPageCursor>,
  backwards_cursor: Option<SurfaceSessionPageCursor>,
}

SurfaceSessionSearchHit {
  thread: SurfaceSessionSummary,
  snippet: DisplayText,
}

SurfaceSessionSearchPage {
  catalog_revision: SessionCatalogRevision,
  data: Vec<SurfaceSessionSearchHit>,
  next_cursor: Option<SurfaceSessionPageCursor>,
  backwards_cursor: Option<SurfaceSessionPageCursor>,
}

For released JSONL list/page calls, a missing or empty provider/model array
decodes to `Any`, and a nonempty array to `Match`; the released matcher
explicitly treats empty arrays as no filter. A missing or empty `cwd` filter is
likewise the existing empty vector and applies no cwd filter.
An empty `thread/list searchTerm` decodes to `None` and performs no text filter.
If both released `parentThreadId` and `ancestorThreadId` are present,
`parentThreadId` wins and the decoder constructs only `DirectChildrenOf`.

`LegacyJsonl.wire_value` preserves every released 64-bit `usize` value. Its
`effective` value is `max(1, wire_value)` and has no compatibility-layer upper
cap; this freezes the released `page_vec` behavior for zero and large limits.
The runtime may stream or internally aggregate bounded store reads under one
catalog/read token, but may not truncate, reject, or change next/backwards
cursor semantics. `ClientBounded` retains the private 1..=100 list/search and
1..=500 thread-page limits for non-legacy callers.

SessionReadToken {
  thread_id: SurfaceThreadId,
  durable_revision: DurableRevision,
  metadata_revision: SessionMetadataRevision,
  snapshot_digest: Sha256Digest,
}

SurfaceSessionMetadata {
  summary: SurfaceSessionSummary,
  runtime_workspace_roots: Vec<CanonicalPath>,
  active_permission_profile: Option<SurfaceActivePermissionProfile>,
  permission_rules: SurfacePermissionRuleSet,
  additional_working_directories: Vec<SurfaceAdditionalWorkingDirectory>,
  network_permissions: SurfaceNetworkPermissions,
}

// Compatibility history is a typed read projection, not the live SurfaceItem
// ledger. `SurfaceDataValue` is the closed JSON data algebra used for released
// tool/workflow payloads; its object keys are sorted and unique, and it never
// carries a runtime event or terminal discriminator.
SurfaceDataValue =
  Null
  | Boolean(bool)
  | Integer(i64)
  | Unsigned(u64)
  | Number(FiniteF64)
  | String(DisplayText)
  | Array(Vec<SurfaceDataValue>)
  | Object(Vec<SurfaceDataProperty>)

SurfaceDataProperty {
  name: DisplayText,
  value: Box<SurfaceDataValue>,
}

`Integer` is used only for negative JSON integers and `Unsigned` for zero and
positive JSON integers, including values above `i64::MAX`, without conversion
to a lossy float. `DisplayText` permits the valid empty JSON object key. Object keys
remain bytewise sorted and unique, including the empty key.

SurfaceHistoryId(NonEmptyText)
SurfaceHistoryRole = System | User | Assistant | Tool
SurfaceHistoryStatus =
  InProgressSnakeCase       // `in_progress`
  | InProgressCamelCase     // `inProgress`
  | Running
  | Completed
  | Failed
  | NotImplementedSnakeCase // `not_implemented`
  | Cancelled
  | Indeterminate
  | Denied

SurfaceHistoryToolKind =
  Success
  | Empty
  | NoMatches
  | Truncated
  | PermissionDenied
  | InvalidInput
  | RuntimeError
  | Cancelled
  | Indeterminate

SurfaceHistoryMessage =
  System {
    role: System,
    content: DisplayText,
  }
  | User {
    role: User,
    content: DisplayText,
  }
  | Assistant {
    role: Assistant,
    content: Option<DisplayText>,
    reasoning_content: Option<DisplayText>,
    tool_calls: Vec<SurfaceDataValue>,
  }
  | Tool {
    role: Tool,
    tool_call_id: SurfaceHistoryId,
    content: DisplayText,
  }

FileChangeKind = Edit | Write

SurfaceHistoryFileChange {
  path: Option<DisplayText>,
  kind: FileChangeKind,
  diff: SurfaceDataValue,
}

SurfaceHistoryItem =
  PersistedMessage { message: SurfaceHistoryMessage }
  | UserMessage { content: DisplayText }
  | AgentMessage { id: SurfaceHistoryId, text: DisplayText }
  | Plan { id: SurfaceHistoryId, text: DisplayText }
  | Reasoning {
      id: SurfaceHistoryId,
      summary: DisplayText,
      content: DisplayText,
    }
  | CommandExecution {
      id: SurfaceHistoryId,
      tool: NonEmptyText,
      command: Option<DisplayText>,
      cwd: Option<CanonicalPath>,
      process_id: Option<SurfaceHistoryId>,
      source: Option<NonEmptyText>,
      status: SurfaceHistoryStatus,
      command_actions: Vec<SurfaceDataValue>,
      aggregated_output: Option<DisplayText>,
      error: Option<SurfaceDataValue>,
      exit_code: Option<i32>,
      truncated: Option<bool>,
      duration_ms: Option<u64>,
      kind: Option<SurfaceHistoryToolKind>,
      terminal_source: Option<ToolTerminalSource>,
      invocation_started: Option<ToolInvocationStarted>,
    }
  | ToolResult {
      tool_call_id: SurfaceHistoryId,
      content: DisplayText,
      status: Option<SurfaceHistoryStatus>,
      error: Option<SurfaceDataValue>,
      exit_code: Option<i32>,
      truncated: Option<bool>,
      kind: Option<SurfaceHistoryToolKind>,
      terminal_source: Option<ToolTerminalSource>,
      invocation_started: Option<ToolInvocationStarted>,
    }
  | McpToolCall {
      id: SurfaceHistoryId,
      server: NonEmptyText,
      tool: NonEmptyText,
      status: SurfaceHistoryStatus,
      arguments: SurfaceDataValue,
      result: SurfaceDataValue,
      error: SurfaceDataValue,
      truncated: Option<bool>,
      kind: Option<SurfaceHistoryToolKind>,
      terminal_source: Option<ToolTerminalSource>,
      invocation_started: Option<ToolInvocationStarted>,
    }
  | DynamicToolCall {
      id: SurfaceHistoryId,
      namespace: Option<NonEmptyText>,
      tool: NonEmptyText,
      status: SurfaceHistoryStatus,
      arguments: SurfaceDataValue,
      content_items: Option<Vec<SurfaceDataValue>>,
      success: Option<bool>,
      error: Option<SurfaceDataValue>,
      truncated: Option<bool>,
      kind: Option<SurfaceHistoryToolKind>,
      terminal_source: Option<ToolTerminalSource>,
      invocation_started: Option<ToolInvocationStarted>,
    }
  | FileChange {
      id: SurfaceHistoryId,
      status: SurfaceHistoryStatus,
      changes: NonEmptyVec<SurfaceHistoryFileChange>,
      error: Option<SurfaceDataValue>,
      kind: Option<SurfaceHistoryToolKind>,
      terminal_source: Option<ToolTerminalSource>,
      invocation_started: Option<ToolInvocationStarted>,
    }
  | WorkflowStarted {
      id: SurfaceHistoryId,
      workflow_name: NonEmptyText,
      task_id: SurfaceHistoryId,
      status: Running,
      task: SurfaceDataValue,
    }
  | WorkflowTerminal {
      id: SurfaceHistoryId,
      workflow_name: NonEmptyText,
      task_id: SurfaceHistoryId,
      status: SurfaceHistoryStatus,
      result: SurfaceDataValue,
      error: SurfaceDataValue,
      task: SurfaceDataValue,
    }

The `SurfaceHistoryMessage` variant fixes its wire role literal; a caller cannot
construct `System { role: User }`. `WorkflowStarted.status` is exactly
`running`; `WorkflowTerminal.status` is any closed terminal status other than
`Running` or either in-progress spelling. A raw unmatched `tool_result` record
uses `ToolResult` and is never silently dropped or promoted to a tool-call item.

TurnItemsView = NotLoaded | Summary | Full

The released `Summary` view still carries the projected item vector for the
current wire version; only `NotLoaded` carries an empty `items` vector. The
typed query preserves that distinction and never replaces `Summary` with a
count-only DTO.

ThreadPageQuery =
  Messages {
    direction: SortDirection,
  }
  | Turns {
      direction: SortDirection,
      items_view: TurnItemsView,
    }
  | Items {
      turn: ThreadItemTurnFilter,
      direction: SortDirection,
    }

ThreadItemTurnFilter =
  Any
  | Exact(SurfaceHistoryId)
  | MatchNone

ThreadPageCursor {
  read_token: SessionReadToken,
  query_digest: Sha256Digest,
  next_ordinal: u64,
  cursor_authenticator: OpaqueToken,
}

SurfaceThreadPageCursor =
  Typed(ThreadPageCursor)
  | LegacyJsonl(LegacyJsonlPageCursor)

SurfaceHistoryTurn {
  thread_id: SurfaceHistoryId,
  turn_id: SurfaceHistoryId,
  index: u64,
  role: SurfaceHistoryRole,
  items_view: TurnItemsView,
  items: Vec<SurfaceHistoryItem>,
}

SurfaceHistoryItemEntry {
  thread_id: SurfaceHistoryId,
  turn_id: SurfaceHistoryId,
  item_id: SurfaceHistoryId,
  index: u64,
  item: SurfaceHistoryItem,
}

SurfaceThreadPage =
  Messages {
    read_token: SessionReadToken,
    data: Vec<SurfaceHistoryMessage>,
    next_cursor: Option<SurfaceThreadPageCursor>,
    backwards_cursor: Option<SurfaceThreadPageCursor>,
  }
  | Turns {
      read_token: SessionReadToken,
      data: Vec<SurfaceHistoryTurn>,
      next_cursor: Option<SurfaceThreadPageCursor>,
      backwards_cursor: Option<SurfaceThreadPageCursor>,
  }
  | Items {
      read_token: SessionReadToken,
      data: Vec<SurfaceHistoryItemEntry>,
      next_cursor: Option<SurfaceThreadPageCursor>,
      backwards_cursor: Option<SurfaceThreadPageCursor>,
  }

SurfaceSessionReadBundle {
  metadata: SurfaceSessionMetadata,
  read_token: SessionReadToken,
  messages: Vec<SurfaceHistoryMessage>,
  turns: Vec<SurfaceHistoryTurn>,
}

ReadSessionMetadataOutput {
  metadata: SurfaceSessionMetadata,
  read_token: SessionReadToken,
}
```

`ReadSession` obtains one `SessionReadToken` and reads every requested field at
that revision while holding the logical session read guard or from an immutable
revision snapshot. It is the only mapping for released JSONL `thread/read`.
The released wire always contains `messages` and `turns` arrays; a false
`includeMessages` or `includeTurns` flag produces the corresponding empty
vector, never `null` or an omitted field.
Typed list/search cursors bind the exact sort and filter digest. Typed
thread-page cursors bind the exact `ThreadPageQuery`; changing direction, item
view, or turn filter with one is `InvalidCursor`.
`LegacyJsonlPageCursor` instead freezes the released offset codec: a non-decimal,
empty, or overflowing wire string has `effective_offset=0`; valid decimal is
used as the offset. After ordering/filtering, paging uses
`start=min(offset,len)`, `page_size=max(limit,1)`,
`end=min(start.saturating_add(page_size),len)`, and
`data=ordered[start..end]`. `next=Some(decimal(end))` exactly when `end < len`,
otherwise null. `backwards=Some(decimal(start))` exactly when `len > 0`,
otherwise null; therefore a cursor beyond the end returns empty data but a
backwards value equal to `len` for a nonempty source. It carries no cross-request revision/query authority.
Runtime still performs each request against one current catalog/read snapshot
and returns unknown-thread/store errors before producing an empty page. The
adapter neither stores a token map nor upgrades legacy cursors into typed ones.
For released `thread/items/list`, a missing `turnId` decodes to `Any`, a
nonempty value to `Exact`, and `turnId:""` to `MatchNone`. `MatchNone` still
opens the named thread and fixes its read token before returning an empty page,
so an unknown thread retains its released error instead of being adapter-short-
circuited.

## Thread Creation And Catalog Inputs

```text
SecretReference = Environment { name: NonEmptyText }
                  | HostSecretStore { key: NonEmptyText }

SurfaceMcpValue =
  LiteralNonSecret { value: DisplayText }
  | Secret { reference: SecretReference }
  | EphemeralSecret { value: ZeroizingProcessLocalSecret }

SurfaceMcpTransport =
  Stdio {
    command: NonEmptyText,
    args: Vec<SurfaceMcpValue>,
    env: Vec<(NonEmptyText, SurfaceMcpValue)>,
  }
  | Sse {
      url: CanonicalUri,
      headers: Vec<(NonEmptyText, SurfaceMcpValue)>,
    }

SurfaceMcpServerDeclaration {
  name: NonEmptyText,
  transport: SurfaceMcpTransport,
  startup_timeout: DurationMillis,
  tool_timeout: DurationMillis,
  disabled: bool,
}

SurfaceThreadCreateSpec {
  title: DisplayText,
  persistence: ThreadPersistence,
  settings_overrides: Vec<RuntimeSettingsPatch>,
  mcp_servers: Vec<SurfaceMcpServerDeclaration>,
  parent_thread_id: Option<SurfaceThreadId>,
}

OpenThreadMode = LiveOnly | LiveOrMaterialize

SessionMetadataPrecondition =
  Exact { revision: SessionMetadataRevision }
  | LegacyLastWriteWins

SessionMetadataPatch = SetTitle { title: DisplayText }

MemoryScope =
  User { expected_memory_revision: Option<MemoryRevision> }
  | Project {
      canonical_root: CanonicalPath,
      expected_root_revision: ProjectRootMemoryRevision,
      expected_memory_revision: Option<MemoryRevision>,
    }

RuntimeSettingsTarget =
  HostDefaults
  | Thread { thread_id: SurfaceThreadId }
  | HostDefaultsAndThread { thread_id: SurfaceThreadId }

RuntimeSettingsExpectedRevision {
  host: SettingsRevision,
  thread: Option<SettingsRevision>,
}

InputCatalogCursor {
  revision: InputCatalogRevision,
  context_digest: Sha256Digest,
  query_digest: Sha256Digest,
  offset: u64,
  cursor_authenticator: OpaqueToken,
}

InputCatalogQuery =
  Search {
    query: DisplayText,
    kinds: Set<File | Directory | Skill | Plugin | Workflow | McpResource
               | McpResourceTemplate | McpTool>,
    cursor: Option<InputCatalogCursor>,
    limit: u32,
  }
  | Lookup { id: SurfaceCatalogEntryId }

InputCatalogContext =
  HostDefaults {
    host_incarnation: HostIncarnation,
    settings_revision: SettingsRevision,
  }
  | Thread {
      thread_id: SurfaceThreadId,
      settings_revision: SettingsRevision,
    }

JsonlTurnControlAction =
  Interrupt
  | Resume
  | Steer { input: SurfaceInputRequest }

JsonlResolvedTurnControlStatus = Interrupted | Resumed | Steered

JsonlTurnControlWireAction = Interrupt | Resume | Steer

LegacyTurnId(DisplayText) // released decoder preserves an empty/missing id

JsonlResolvedTurnControlWireEcho {
  legacy_turn_id: LegacyTurnId,
  action: JsonlTurnControlWireAction,
  status: JsonlResolvedTurnControlStatus,
  legacy_input: Option<DisplayText>,
}

JsonlIdleTurnControlWireEcho {
  legacy_turn_id: LegacyTurnId,
  action: JsonlTurnControlWireAction,
  status: Idle,
  legacy_input: Option<DisplayText>,
}

JsonlTurnControlledOutput {
    operation_id: SurfaceOperationId,
    echo: JsonlResolvedTurnControlWireEcho,
    committed_cursor: SurfaceCursor,
    input_item_id: Option<SurfaceItemId>,
}

JsonlTurnControlResult =
  Idle {
      request_id: SurfaceRequestId,
      echo: JsonlIdleTurnControlWireEcho,
    }
  | Resolved { mutation: MutationReply<JsonlTurnControlledOutput> }

SurfaceInputCatalogEntry {
  id: SurfaceCatalogEntryId,
  kind: SurfaceInputBindingKind,
  label: NonEmptyText,
  description: Option<DisplayText>,
  catalog_revision: InputCatalogRevision,
}

SurfaceInputCatalogPage {
  revision: InputCatalogRevision,
  data: Vec<SurfaceInputCatalogEntry>,
  next_cursor: Option<InputCatalogCursor>,
}
```

The echo pairing is closed: resolved Interrupt/Interrupted and Resume/Resumed
require `legacy_input=None`; resolved Steer/Steered requires the exact assembled
visible `DisplayText` and no structured mention binding. An unknown turn returns
only `JsonlIdleTurnControlWireEcho(status=Idle)` with the original action and,
for Steer, exact original visible input. A resolved output cannot contain Idle.
The adapter retains only RPC correlation and never rereads the semantic request
to build `turn_controlled`.

Typed session, thread-page, MCP, and input catalog cursor authenticators are
runtime-signed opaque encodings of every preceding claim field in their exact
cursor type and a domain-separation purpose. Legacy JSONL page cursors are the
sole unsigned exception. Cross-thread/family MCP reuse, cross-context/query
Input reuse, typed session/page query reuse, stale revision, or a tampered token
is rejected before reading a page; adapters never decode an offset and silently
continue against a different snapshot.

Secrets are accepted only through `SurfaceMcpValue::Secret` or a zeroizing
process-local value consumed during host creation. `LiteralNonSecret` is an
explicit assertion and may be persisted. Before a recorded declaration commits,
an ephemeral secret is atomically stored and replaced by a secret reference or
the whole mutation remains uncommitted. Secret bytes never enter snapshots,
events, manifests, content-revealing digests, or durable request capsules.

Creation and fork callers submit only closed override patches. They cannot
submit an effective settings snapshot, `SettingsRevision`, or `PolicyEpoch`.
The host applies creation overrides to current host defaults and fork overrides
to the revision-bound source settings, validates trust/capabilities, and derives
the effective revisions. `LoadThread.settings_overrides` is the atomic released
JSONL resume path: stored settings load first, overrides and MCP declarations
commit through the coordinator, and only then may the live handle be returned.
No adapter performs a read-modify-write or mutates a loaded config before the
host receipt.

## Host Command Payloads

Every `SurfaceHostCommand` is submitted through a
`RuntimeSurfaceHostHandle`; its `SurfaceHostBoundCaller`, host incarnation,
role, capabilities, and optional connection identity are injected out of band
and cannot be supplied in the payload. Payload incarnation or thread fields are
stale-state preconditions, never caller credentials. Connection-scoped commands
are unavailable on an unbound handle.

```text
SurfaceHostCommand =
  ListSessions {
    request_id: SurfaceRequestId,
    page: SessionPageRequest,
  }
  | SearchSessions {
      request_id: SurfaceRequestId,
      search: SessionSearchRequest,
    }
  | ReadSessionMetadata {
      request_id: SurfaceRequestId,
      thread_id: SurfaceThreadId,
    }
  | ReadSession {
      request_id: SurfaceRequestId,
      thread_id: SurfaceThreadId,
      include_messages: bool,
      include_turns: bool,
    }
  | ReadThreadPage {
      request_id: SurfaceRequestId,
      thread_id: SurfaceThreadId,
      query: ThreadPageQuery,
      read_token: Option<SessionReadToken>,
      cursor: Option<SurfaceThreadPageCursor>,
      limit: SurfacePageLimit, // ClientBounded is 1..=500 here
    }
  | CreateThread {
      request_id: SurfaceRequestId,
      spec: SurfaceThreadCreateSpec,
    }
  | OpenThread {
      request_id: SurfaceRequestId,
      thread_id: SurfaceThreadId,
      mode: OpenThreadMode,
      expected_settings_digest: Option<Sha256Digest>,
    }
  | LoadThread {
      request_id: SurfaceRequestId,
      thread_id: SurfaceThreadId,
      expected_settings_digest: Option<Sha256Digest>,
      settings_overrides: Vec<RuntimeSettingsPatch>,
      mcp_servers: Vec<SurfaceMcpServerDeclaration>,
    }
  | ForkThread {
      request_id: SurfaceRequestId,
      source_thread_id: SurfaceThreadId,
      source_read_token: SessionReadToken,
      title: Option<DisplayText>,
      settings_overrides: Vec<RuntimeSettingsPatch>,
    }
  | ResolveRunningThread {
      request_id: SurfaceRequestId,
      thread_id: SurfaceThreadId,
      mode: LiveOnly,
    }
  | ResumeLatestActiveGoal {
      request_id: SurfaceRequestId,
      expected_goal_store_revision: Option<GoalCatalogRevision>,
    }
  | UpdateSessionMetadata {
      request_id: SurfaceRequestId,
      thread_id: SurfaceThreadId,
      precondition: SessionMetadataPrecondition,
      patch: SessionMetadataPatch,
    }
  | QueryInputCatalog {
      request_id: SurfaceRequestId,
      context: InputCatalogContext,
      expected_revision: Option<InputCatalogRevision>,
      query: InputCatalogQuery,
    }
  | ControlJsonlTurn {
      request_id: SurfaceRequestId,
      expected_thread_id: Option<SurfaceThreadId>,
      legacy_turn_id: LegacyTurnId,
      action: JsonlTurnControlAction,
    }
  | RememberMemory {
      request_id: SurfaceRequestId,
      scope: MemoryScope,
      note: NonEmptyText,
      pin_to_thread: Option<SurfaceThreadId>,
    }
  | ReconcileMemoryMutation {
      token: ReconcileMemoryMutationToken,
    }
  | ReadFolderTrust {
      request_id: SurfaceRequestId,
      path: CanonicalPath,
    }
  | SetFolderTrust {
      request_id: SurfaceRequestId,
      path: CanonicalPath,
      expected_trust_revision: TrustRevision,
      level: Trusted | Untrusted,
    }
  | ReconcileFolderTrustRevocation {
      token: ReconcileFolderTrustRevocationToken,
    }
  | ReadRuntimeSettings {
      request_id: SurfaceRequestId,
      thread_id: Option<SurfaceThreadId>,
    }
  | UpdateRuntimeSettings {
      request_id: SurfaceRequestId,
      target: RuntimeSettingsTarget,
      expected: RuntimeSettingsExpectedRevision,
      patch: NonEmptyVec<RuntimeSettingsPatch>,
    }
  | ReconcileHostMutation {
      token: ReconcileHostMutationToken,
    }
  | CloseThread {
      request_id: SurfaceRequestId,
      thread_id: SurfaceThreadId,
      expected_owner_epoch: Option<ThreadOwnerEpoch>,
    }
  | ShutdownHost {
      request_id: SurfaceRequestId,
      host_incarnation: HostIncarnation,
    }
```

`ReadSession`, `UpdateSessionMetadata`, `QueryInputCatalog`, and
`ControlJsonlTurn` are Phase 0A additions to the parent command-name list. They
close released JSONL coherence/metadata/control and TUI discovery gaps without
giving an adapter store or active-turn map access. The parent design is updated
in the same Phase 0A commit.

## Host Command Outputs

```text
RuntimeSurfaceHostHandle(
  opaque, cloneable, process-local, bound to one host incarnation, grant,
  and optional connection identity
)
RuntimeSurfaceHandle(
  opaque, cloneable, process-local, thread-bound attach facade carrying one
  host-issued SurfaceAttachAuthority
)

CreateThreadMaterialization = Created
ForkThreadMaterialization = Forked { source_thread_id: SurfaceThreadId }

ThreadSettingsReceipt =
  Unchanged {
    host_revision: SettingsRevision,
    thread_revision: Option<SettingsRevision>,
  }
  | Committed { receipt: SurfaceRuntimeSettingsReceipt }

CreateThreadOutput =
  Recorded {
    surface: RuntimeSurfaceHandle,
    thread: SurfaceThreadSnapshot,
    materialization: CreateThreadMaterialization,
    catalog_receipt: SurfaceSessionCatalogReceipt,
    settings_receipt: ThreadSettingsReceipt,
  }
  | Ephemeral {
    surface: RuntimeSurfaceHandle,
    thread: SurfaceThreadSnapshot,
    materialization: CreateThreadMaterialization,
    settings_receipt: ThreadSettingsReceipt,
  }

OpenThreadMaterialization = AttachedLive | MaterializedLive

OpenThreadOutput {
  surface: RuntimeSurfaceHandle,
  thread: SurfaceThreadSnapshot,
  materialization: OpenThreadMaterialization,
  catalog_receipt: SurfaceSessionCatalogReceipt,
  settings_receipt: ThreadSettingsReceipt,
}

LoadThreadMaterialization =
  LoadedCold { recovery: Clean | RecoveryRequired | FinalizationReconciled }

LoadThreadOutput {
  surface: RuntimeSurfaceHandle,
  thread: SurfaceThreadSnapshot,
  materialization: LoadThreadMaterialization,
  catalog_receipt: SurfaceSessionCatalogReceipt,
  settings_receipt: ThreadSettingsReceipt,
}

ForkThreadOutput {
  surface: RuntimeSurfaceHandle,
  thread: SurfaceThreadSnapshot,
  materialization: ForkThreadMaterialization,
  catalog_receipt: SurfaceSessionCatalogReceipt,
  settings_receipt: ThreadSettingsReceipt,
}

ResolveRunningThreadOutput {
  surface: RuntimeSurfaceHandle,
  thread: SurfaceThreadSnapshot,
}

ResumeLatestGoalOutput {
  surface: RuntimeSurfaceHandle,
  goal: SurfaceGoal,
  goal_receipt: SurfaceGoalStoreReceipt,
  goal_cursor: SurfaceCursor,
  operation_id: SurfaceOperationId,
  operation_cursor: SurfaceCursor,
  waiter: OperationWaiterHandle,
  catalog_receipt: SurfaceSessionCatalogReceipt,
}

MemoryPinResult =
  NotRequested
  | Committed { thread_id: SurfaceThreadId, cursor: SurfaceCursor }
  | Pending { thread_id: SurfaceThreadId }

MemoryMutationOutput {
  memory_receipt: SurfaceMemoryReceipt,
  pin: MemoryPinResult,
}

FolderTrustRead {
  canonical_path: CanonicalPath,
  matched_ancestor: CanonicalPath,
  effective_level: Trusted | Untrusted,
  trust_revision: TrustRevision,
  policy_epoch: PolicyEpoch,
}

FolderTrustMutationOutput {
  receipt: SurfaceFolderTrustReceipt,
  barrier_plan: PolicyRevocationBarrierPlan,
  pending: Vec<PolicyRevocationSubject>,
}

RuntimeSettingsRead {
  host_revision: SettingsRevision,
  thread_revision: Option<SettingsRevision>,
  effective: SurfaceRuntimeSettings,
  pending: Option<SurfaceRuntimeSettings>,
}

RuntimeSettingsMutationOutput {
  receipt: SurfaceRuntimeSettingsReceipt,
  thread_cursor: Option<SurfaceCursor>,
}

SessionMetadataMutationOutput {
  metadata: SurfaceSessionMetadata,
  receipt: SurfaceSessionMetadataReceipt,
  thread_cursor: Option<SurfaceCursor>,
}

`SessionMetadataMutationOutput.thread_cursor` is present exactly when the
thread was attached at commit time; a cold catalog-only update is complete with
the HostReceipt alone. `MemoryPinResult::Pending` is legal only inside the
memory-backed `Deferred(MemoryPinPending)` branch, and its thread id equals the
deferred state/token. A non-memory pin never uses that nested value and uses
`ProjectionDegraded`/`RetryProjection` on projection failure.

ClosedThreadReceipt =
  Recorded {
    thread_id: SurfaceThreadId,
    operation_terminals: Vec<OperationTerminalAtCursor>,
    closed_cursor: SurfaceCursor,
    catalog_receipt: SurfaceSessionCatalogReceipt,
  }
  | Ephemeral {
      thread_id: SurfaceThreadId,
      persistence: EphemeralNonCataloguedOneShot {
                     close_after: FirstOperationCompletionPolicy,
                   }
                   | EphemeralAttached,
      operation_terminals: Vec<OperationTerminalAtCursor>,
      closed_cursor: SurfaceCursor,
    }

CloseThreadOutput = ClosedThreadReceipt

ShutdownHostOutput {
  host_incarnation: HostIncarnation,
  host_receipt: SurfaceHostShutdownReceipt,
  closed_threads: Vec<ClosedThreadReceipt>,
}

RetainedShutdownOutput =
  CloseThread { output: CloseThreadOutput }
  | ShutdownHost { output: ShutdownHostOutput }

ReconcileHostMutationOutput =
  Settlement { result: RuntimeSurfaceMutationResult }
  | CloseThread { result: MutationReply<CloseThreadOutput> }
  | ShutdownHost { result: MutationReply<ShutdownHostOutput> }
```

A committed close/shutdown output is the sorted exact value projection of its
immutable `ShutdownBarrierPlan` and acknowledgement vector. For each planned
thread there is exactly one `ClosedThreadReceipt` with the same id, persistence,
and operation order. Every `operation_terminals` member is byte-identical to the
matching `OperationTerminalAck.value`; `closed_cursor` is byte-identical to the
planned Session `ThreadLocalCursor`; a Recorded receipt contains the exact
catalog `HostCommitAck.receipt` and digest, while an Ephemeral receipt contains
none. `ShutdownHostOutput.host_incarnation` equals the plan,
`ShutdownHostOutput.closed_threads` is in plan thread order, and its
`host_receipt` is byte-identical to the final HostLifecycle ack; CloseThread
returns the sole planned thread receipt. Missing, extra, duplicate, out-of-order,
wrong-persistence, or receipt/ack mismatches are construction errors and cannot
return `Committed`.
`ReconcileHostMutationToken::Shutdown.scope` selects exactly one of the latter
two `ReconcileHostMutationOutput` branches. If repair remains incomplete, that
branch carries the same Deferred shutdown plan and exact missing complement. If
it completes, the host returns the byte-identical retained output from the
durable `ShutdownBarrierRecord` even when the original thread surface or host
handle is sealed. It never reconstructs an output from a new registry scan.

An ephemeral noncatalogued one-shot thread is automatically closed and removed
by the host after its first operation reaches Terminal or NotAdmitted. Adapter
detach, write failure, or EOF cannot leave it discoverable or orphaned. It never
emits a catalog receipt or `thread_started` on the released stateless JSONL
wire.

## Host Command Contract Matrix

Host commands use the same request-id idempotency rule as thread commands. A
host mutation is `Committed` only after every acknowledgement in its row is
present. In particular, a close or shutdown cannot return a closed output while
any owned operation lacks its `OperationTerminal` acknowledgement. As in the
thread matrix, every mutating host row additionally admits only the common
precommit `Invalid(CommitBatchTooLarge)` structural error; read rows do not.

| Command | Target | Required capability / precondition | Normal dispositions | Result | Required acknowledgements | Legal deferred value/state | Closed failure outcomes | Authoritative effect |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| ListSessions | `SessionCatalog` | ReadSessionCatalog; valid page cursor | Found | `SurfaceReadResult<SurfaceSessionSummaryPage>` | none | n/a | InvalidCursor, StaleRevision, CapabilityDenied, StoreUnavailable, RuntimeUnavailable | none |
| SearchSessions | `SessionCatalog` | ReadSessionCatalog; nonempty query | Found | `SurfaceReadResult<SurfaceSessionSearchPage>` | none | n/a | InvalidRequest, InvalidCursor, StaleRevision, CapabilityDenied, StoreUnavailable, RuntimeUnavailable | none |
| ReadSessionMetadata | `SessionCatalog` | ReadSessionCatalog; canonical id | Found | `SurfaceReadResult<ReadSessionMetadataOutput>` | none | n/a | NotFound, CapabilityDenied, StoreUnavailable, RuntimeUnavailable | none |
| ReadSession | `SessionCatalog` | ReadSessionCatalog; canonical id | Found | `SurfaceReadResult<SurfaceSessionReadBundle>` | none | n/a | NotFound, CapabilityDenied, StoreUnavailable, RuntimeUnavailable | none; one coherent read token |
| ReadThreadPage | `SessionCatalog` | ReadSessionCatalog; matching token/cursor | Found | `SurfaceReadResult<SurfaceThreadPage>` | none | n/a | InvalidCursor, StaleRevision, NotFound, CapabilityDenied, StoreUnavailable, RuntimeUnavailable | none |
| CreateThread | `SessionCatalog(thread)` or `Thread` for ephemeral | ManageSessionLifecycle; valid spec/trust/settings/MCP | Accepted, AlreadyApplied | `MutationReply<CreateThreadOutput>` | in barrier order: nonempty settings overrides add `HostReceipt(RuntimeSettings)` then `ThreadCursor(Settings)`; nonempty MCP declarations add `ThreadCursor(McpCatalog)`; then recorded threads require `HostReceipt(SessionCatalog)` + `ThreadCursor(Session)`, while ephemeral threads require `ThreadCursor(Session)` | `NoValue / MutationDegraded` or `ProjectionDegraded` | InvalidInput, InvalidContent, UnsupportedContent, CapabilityDenied, StaleRevision, RuntimeUnavailable, ThreadClosed | registry winner, owner lease, optional catalog record, Session Materialized |
| OpenThread | `SessionCatalog(thread)` | ManageSessionLifecycle; mode rules | Accepted, AlreadyApplied | `MutationReply<OpenThreadOutput>` | `HostReceipt(SessionCatalog)` + materialization `ThreadCursor(Session)` | `NoValue / ProjectionDegraded` | UnknownSession, ThreadOwnedElsewhere, InvalidInput, CapabilityDenied, RuntimeUnavailable | attach live winner or materialize only in LiveOrMaterialize; output uses one `OpenThreadMaterialization` variant |
| LoadThread | `SessionCatalog(thread)` | ManageSessionLifecycle; durable record exists | Accepted, AlreadyApplied | `MutationReply<LoadThreadOutput>` | in barrier order: nonempty settings overrides add `HostReceipt(RuntimeSettings)` then `ThreadCursor(Settings)`; nonempty MCP declarations add `ThreadCursor(McpCatalog)`; then `HostReceipt(SessionCatalog)` + `ThreadCursor(Session)` | `NoValue / FinalizingDegraded` or `ProjectionDegraded` | UnknownSession, ThreadOwnedElsewhere, InvalidInput, CapabilityDenied, StoreUnavailable, RuntimeUnavailable | acquire/advance owner epoch, recovery, materialize |
| ForkThread | `SessionCatalog(new_thread)` | ManageSessionLifecycle; exact source read token | Accepted, AlreadyApplied | `MutationReply<ForkThreadOutput>` | in barrier order: nonempty settings overrides add `HostReceipt(RuntimeSettings)` then `ThreadCursor(Settings)`; then `HostReceipt(SessionCatalog)` + `ThreadCursor(Session)` | `NoValue / MutationDegraded` or `ProjectionDegraded` | UnknownSession, StaleRevision, InvalidInput, CapabilityDenied, StoreUnavailable, RuntimeUnavailable | new id/catalog record, forkable history only |
| ResolveRunningThread | `SessionCatalog(thread)` | ManageSessionLifecycle; LiveOnly | Found | `SurfaceReadResult<ResolveRunningThreadOutput>` | none | n/a | NotFound, ThreadOwnedElsewhere, CapabilityDenied, RuntimeUnavailable | no cold load and no mutation except attachment grant |
| ResumeLatestActiveGoal | `Goal` + `Operation` + `SessionCatalog(thread)` | ManageSessionLifecycle + ManageGoal | Accepted, AlreadyApplied | `MutationReply<ResumeLatestGoalOutput>` | `GoalStoreReceipt` + `ThreadCursor(Goal)` + `ThreadCursor(Operation)` + `HostReceipt(SessionCatalog)` | `NoValue / FinalizingDegraded` or `ProjectionDegraded` | NoActiveGoal, UnknownGoal, StaleRevision, ThreadOwnedElsewhere, OperationActive, CapabilityDenied, RuntimeUnavailable | one Goal/session/operation coordinator intent |
| UpdateSessionMetadata | `SessionMetadata(thread)` | ManageSessionCatalog; exact revision or frozen legacy LWW | Accepted, AlreadyApplied | `MutationReply<SessionMetadataMutationOutput>` | `HostReceipt(SessionMetadata)`; add `ThreadCursor(Session)` only when the thread is attached | `Provisional(SessionMetadataMutationOutput) / ProjectionDegraded` | UnknownSession, StaleRevision, InvalidInput, CapabilityDenied, StoreUnavailable, RuntimeUnavailable | catalog metadata receipt and optional live Session patch when attached |
| QueryInputCatalog | `SessionCatalog` or `Thread` context | ReadCatalog; optional revision | Found | `SurfaceReadResult<SurfaceInputCatalogPage>` | none | n/a | NotFound, StaleRevision, InvalidRequest, CapabilityDenied, ThreadClosed, RuntimeUnavailable | none |
| ControlJsonlTurn | resolved `Operation`, or read-only Idle lookup | LegacyJsonlControl; handle-bound connection identity and optional exact thread id | Idle, Accepted, AlreadyApplied | `JsonlTurnControlResult` | Idle: none; resolved Interrupt/Resume: `ThreadCursor(Operation)`; resolved Steer: `ThreadCursor(Operation)` then `ThreadCursor(Item)` | resolved branch only: `NoValue / ProjectionDegraded` | OperationAlreadyTerminal, StaleFence, OperationNotInterrupted, OperationNotSteerable, WrongThread | unknown turn returns Idle with no mutation; resolved branch commits actor-ordered interrupt/resume/steer |
| RememberMemory | `Memory` plus optional `Thread` pin | ManageMemory; user scope fences `MemoryRevision`, project scope also fences `ProjectRootMemoryRevision`; pin additionally requires ManagePinnedContext | Accepted, AlreadyApplied | `MutationReply<MemoryMutationOutput>` | always `HostReceipt(Memory)`; pin also `ThreadCursor(PinnedContext)` | `Provisional(MemoryMutationOutput) / MemoryPinPending` | StaleRevision, InvalidInput, StoreUnavailable, UnknownSession, CapabilityDenied | memory receipt plus optional pin; never append note twice |
| ReconcileMemoryMutation | `Memory` plus exact pin thread | ManageMemory + ManagePinnedContext when token has pin | Accepted, AlreadyApplied | `MutationReply<MemoryMutationOutput>` | `HostReceipt(Memory)` + `ThreadCursor(PinnedContext)` | `NoValue / MemoryPinPending` | InvalidRequest, StaleRevision, StoreUnavailable, UnknownSession, CapabilityDenied | retry pin only; never append note again |
| ReadFolderTrust | `FolderTrust(path)` | ReadHostPolicy; canonical path | Found | `SurfaceReadResult<FolderTrustRead>` | none | n/a | InvalidRequest, CapabilityDenied, NotFound, StoreUnavailable, RuntimeUnavailable | none |
| SetFolderTrust | `FolderTrust(path)` | ManageFolderTrust; exact trust revision | Accepted, AlreadyApplied | `MutationReply<FolderTrustMutationOutput>` | `HostReceipt(FolderTrust)` + `PolicyRevocationBarrier(exact plan)` | `Provisional(FolderTrustMutationOutput) / PolicyRevocationPending` | StaleRevision, InvalidInput, StoreUnavailable, CapabilityDenied, RuntimeUnavailable | trust receipt/policy epoch and immutable revocation plan; removal waits for owner/resource ack |
| ReconcileFolderTrustRevocation | `FolderTrust(path)` | ManageFolderTrust; exact token | Accepted, AlreadyApplied | `MutationReply<FolderTrustMutationOutput>` | `PolicyRevocationBarrier(exact plan)` | `NoValue / PolicyRevocationPending` | InvalidRequest, StaleRevision, StoreUnavailable, CapabilityDenied, RuntimeUnavailable | owner ack/resource cleanup only |
| ReadRuntimeSettings | `RuntimeSettings(host, thread?)` | ReadHostSettings; optional known thread | Found | `SurfaceReadResult<RuntimeSettingsRead>` | none | n/a | NotFound, CapabilityDenied, RuntimeUnavailable, StoreUnavailable | none |
| UpdateRuntimeSettings | `RuntimeSettings(host, thread?)` | ManageHostSettings; exact host/thread revisions | Accepted, AlreadyApplied | `MutationReply<RuntimeSettingsMutationOutput>` | `HostReceipt(RuntimeSettings)` + optional `ThreadCursor(Settings)` | `Provisional(RuntimeSettingsMutationOutput) / ProjectionDegraded` | StaleRevision, InvalidInput, UnknownSession, CapabilityDenied, ThreadClosed, RuntimeUnavailable | host receipt plus optional thread Settings cursor |
| ReconcileHostMutation | original host `MutationTarget` or shutdown barrier | matching host domain capability/token | Accepted, AlreadyApplied | `ReconcileHostMutationOutput` | missing host/thread receipt named by token, or complete shutdown barrier | Settlement returns the original mutation algebra; Shutdown returns the exact typed CloseThread/ShutdownHost `MutationReply`, with `NoValue / ShutdownDeferred` until complete | InvalidRequest, StaleRevision, WrongHost, CapabilityDenied, RuntimeUnavailable | missing host receipt/projection or the same immutable close/shutdown barrier only; completed shutdown returns retained output |
| CloseThread | `SessionCatalog(thread)` for RecordedCatalogued or `Thread(thread)` for ephemeral, plus all owned `Operation`s | ManageSessionLifecycle; optional owner epoch | Accepted, AlreadyApplied | `MutationReply<CloseThreadOutput>` | every `OperationTerminal`, then `ThreadCursor(Session)`; add `HostReceipt(SessionCatalog)` only for RecordedCatalogued | `NoValue / ShutdownDeferred` (pending terminal/close identities) | UnknownSession, WrongOwnerEpoch, ThreadOwnedElsewhere, CapabilityDenied, ThreadClosed, RuntimeUnavailable | close admission/interactions/work, commit every terminal and Session Closed, plus recorded-only catalog receipt |
| ShutdownHost | `Host(host_incarnation)` + all owned threads/operations | ShutdownHost; exact incarnation | Accepted, AlreadyApplied | `MutationReply<ShutdownHostOutput>` | for each thread, every `OperationTerminal` then `ThreadCursor(Session)`; add `HostReceipt(SessionCatalog)` only for RecordedCatalogued; after every thread, final `HostReceipt(HostLifecycle)` last | `NoValue / ShutdownDeferred` (pending thread/terminal/host identities) | WrongHost, HostShuttingDown, CapabilityDenied, RuntimeUnavailable | close all owned work and surfaces; recorded threads close catalog entries; HostLifecycle Closed receipt is committed only after every thread barrier |

`ReadFolderTrust` and `ReadRuntimeSettings` are read commands: missing targets
return `SurfaceReadResult::NotFound`, never the mutation-only
`UnknownSession`. `SearchSessions` returns the search-hit DTO, not the list page.
`ControlJsonlTurn` maps its closed error set to the released `turn_controlled`
or correlated error shapes in the JSONL section below.

Close and shutdown first commit the closing barrier, then resolve every pending
interaction, join/settle every owned operation, and wait for each real
`OperationTerminalAtCursor`. If any terminal persistence, projection, owner ack,
or required recorded-thread catalog receipt is missing, the host returns
`Deferred(ShutdownDeferred)` and
keeps admission closed; it never returns a terminal-less `CloseThreadOutput` or
`ShutdownHostOutput`, and it never restores the TUI terminal on that response.

`LegacyLastWriteWins` is available only to the released JSONL
`thread/metadata/update` decoder, which binds one stable request identity from
connection, RPC id, and normalized patch. No other client receives it.

## Bootstrap Credential Boundary

TUI setup credential persistence is classified but is not runtime/session
authority. It uses a separate closed `BootstrapCredentialService`:

```text
StoreProviderCredential {
  request_id: SurfaceRequestId,
  provider: NonEmptyText,
  secret: ZeroizingProcessLocalSecret,
}

StoreProviderCredentialResult =
  Committed {
    credential_revision: BootstrapCredentialRevision,
    provider: NonEmptyText,
  }
  | Uncommitted { error: InvalidInput | StoreUnavailable | PermissionDenied }
```

The service writes the secret store atomically and returns before the TUI
updates setup presentation. It exposes no runtime handle, surface event, or
secret value. A later `CreateThread` resolves the credential by provider under
host control. Direct TUI file/config credential writes are forbidden after the
Phase 3 cutover.
`BootstrapCredentialRevision` belongs only to this service and cannot satisfy a
host settings, session, memory, or thread revision precondition.

## Fixed Runtime And Transport Budgets

These constants are part of private contract v1 and use injected clocks in
tests:

```text
SURFACE_RESERVATION_LEASE_MS = 30_000
SURFACE_COMMIT_BATCH_EVENT_LIMIT = 1_024
SURFACE_COMMIT_BATCH_BYTE_LIMIT = 8_388_608      // 8 MiB canonical encoding
SURFACE_RETAINED_EVENT_LIMIT = 8_192
SURFACE_RETAINED_BYTE_LIMIT = 33_554_432       // 32 MiB
SURFACE_SUBSCRIBER_EVENT_LIMIT = 1_024
SURFACE_SUBSCRIBER_BYTE_LIMIT = 8_388_608      // 8 MiB

ACP_MAX_INBOUND_LINE_BYTES = 8_388_608         // 8 MiB including newline
ACP_MAX_OUTBOUND_FRAME_BYTES = 8_388_608        // 8 MiB including newline
ACP_INGRESS_MESSAGE_LIMIT = 64
ACP_INGRESS_BYTE_LIMIT = 16_777_216             // 16 MiB
ACP_OUTGOING_MESSAGE_LIMIT = 256
ACP_OUTGOING_BYTE_LIMIT = 33_554_432            // 32 MiB
ACP_LOAD_GATE_MESSAGE_LIMIT = 4_096
ACP_LOAD_GATE_BYTE_LIMIT = 67_108_864            // 64 MiB
ACP_PROMPT_GATE_MESSAGE_LIMIT = 1_024
ACP_PROMPT_GATE_BYTE_LIMIT = 16_777_216          // 16 MiB
ACP_WRITE_FLUSH_DEADLINE_MS = 30_000
ACP_REVERSE_REQUEST_DEADLINE_MS = 120_000
ACP_CAPABILITY_CALL_DEADLINE_MS = 60_000
ACP_CAPABILITY_RESULT_CANONICAL_BYTE_LIMIT = 4_194_304 // 4 MiB, including tag/fields
ACP_CAPABILITY_TEXT_BYTE_LIMIT = 4_194_304
ACP_CAPABILITY_IDENTIFIER_BYTE_LIMIT = 4_096
ACP_TERMINAL_KILL_DEADLINE_MS = 10_000
ACP_TERMINAL_RELEASE_DEADLINE_MS = 10_000
ACP_SUPERVISOR_JOIN_DEADLINE_MS = 5_000
ACP_TOMBSTONE_TTL_MS = 300_000
ACP_TOMBSTONE_LIMIT = 4_096

JSONL_REQUEST_TOMBSTONE_TTL_MS = 300_000
JSONL_REQUEST_TOMBSTONE_LIMIT = 4_096
JSONL_LIVE_REQUEST_LIMIT = 1_024
JSONL_REPAIR_AUTHORITY_LIMIT = 1_024
JSONL_COMMITTED_REPAIR_DRAIN_DEADLINE_MS = 5_000
JSONL_SUPERVISOR_JOIN_DEADLINE_MS = 5_000
```

Byte budgets count encoded UTF-8 bytes plus framing. A single message must fit
the per-frame limit and the aggregate lane budget. A load baseline exceeding a
gate budget fails with `BaselineTooLarge`; it is never partially declared
complete. Test clocks advance deadlines without wall-clock sleeps.

## ACP Private Projection Contract

Phase 0A freezes semantic dispositions, not the Phase 4A public JSON field
schema. The closed disposition is:

```text
AcpProjectionDisposition =
  StandardExact
  | StandardPlusExtensionMeta
  | ExtensionOnly
  | NoWireRetained
  | PromptTerminal
  | UnsupportedBeforeReservation

AcpClientProjectionProfile = StandardOnly | OrcaSurfaceV1

AcpStandardCapabilitySet {
  session_usage: bool,
  session_model: bool,
  session_modes: bool,
  session_info: bool,
  file_read: bool,
  file_write: bool,
  terminal: bool,
}
```

The projection decision is a total function of typed fact, negotiated standard
capability bits, and profile. Each cell below is one member of the closed enum;
there are no compound or implementation-decides dispositions.

| Typed fact/field | StandardOnly | OrcaSurfaceV1 | Exact wire rule |
| --- | --- | --- | --- |
| Operation nonterminal | NoWireRetained | ExtensionOnly | no adapter operation mirror |
| Operation Terminal | PromptTerminal | PromptTerminal | extension metadata rides the correlated response only after cursor flush |
| Item materialization, including SystemMessage | NoWireRetained | ExtensionOnly | standard conversation frames come from Assistant/Tool facts, not duplicate items |
| Assistant Message delta | StandardExact | StandardPlusExtensionMeta | `AgentMessageChunk` |
| Assistant Reasoning delta | StandardExact | StandardPlusExtensionMeta | `AgentThoughtChunk` |
| Assistant Plan text delta/completion | NoWireRetained | ExtensionOnly | never parse free text into ACP Plan |
| Tool Requested | StandardExact | StandardPlusExtensionMeta | `ToolCall`; raw input omitted unless a later closed typed argument value exists |
| Tool ArgumentsProgress | NoWireRetained | ExtensionOnly | byte count has no ACP standard field |
| Tool OutputDelta | StandardExact | StandardPlusExtensionMeta | `ToolCallUpdate.content` contains full reduced output at the containing batch cursor |
| Tool Completed | StandardExact | StandardPlusExtensionMeta | final status/content/diff from typed result |
| CapabilityCall/RemoteTerminalLease | NoWireRetained | ExtensionOnly | transport never invents tool or operation terminal state |
| Plan items | StandardExact | StandardPlusExtensionMeta | full ACP Plan; priority/status are typed, baseline missing priority normalizes to Medium at ingress |
| Plan explanation | NoWireRetained | ExtensionOnly | ACP Plan has no explanation field |
| Live Usage totals | NoWireRetained | ExtensionOnly | prompt terminal usage is mapped separately |
| Context used/limit with session_usage=true | StandardExact | StandardPlusExtensionMeta | exact `UsageUpdate(used,size)` |
| Context used/limit with session_usage=false | NoWireRetained | ExtensionOnly | no undeclared standard frame |
| Context compaction/provider replay | NoWireRetained | ExtensionOnly | provider continuation never crosses the boundary |
| ToolApproval | StandardExact | StandardPlusExtensionMeta | project exactly `AllowOnce` and `RejectOnce`; the selected option becomes Allow/Deny for this interaction only, and the bound handle injects authority; no Thread/Operation/Always scope is fabricated |
| PermissionRequest | NoWireRetained | ExtensionOnly | ACP permission options cannot encode exact filesystem/network/profile scope |
| UserInput/McpElicitation | NoWireRetained | ExtensionOnly | logical reverse requests use the same runtime broker |
| BackgroundApproval | NoWireRetained | ExtensionOnly | no standard mapping unless a future contract adds an exact closed subset |
| Task/Workflow/Subagent/Goal | NoWireRetained | ExtensionOnly | never fabricate chat text or generic tools |
| Settings model with session_model=true | StandardExact | StandardPlusExtensionMeta | model state/update |
| Settings mode with session_modes=true | StandardExact | StandardPlusExtensionMeta | current mode/update |
| Settings fields without matching capability | NoWireRetained | ExtensionOnly | one committed settings revision remains authoritative |
| McpCatalog/PinnedContext | NoWireRetained | ExtensionOnly | retained for load/snapshot |
| Session title/time with session_info=true | StandardExact | StandardPlusExtensionMeta | `SessionInfoUpdate` |
| Session health/recovery/fault | NoWireRetained | ExtensionOnly | a prompt-correlated failure is produced only by terminal mapping |

An extensionless client receives every representable standard update and the
correct PromptResponse terminal. Extension-only state remains in runtime and in
future load materialization; absence of a wire frame is `NoWireRetained`, not a
drop from the surface.

### ACP content ingress

- Text blocks retain original order and text. Adjacent blocks are not joined by
  an adapter-invented newline; runtime canonicalization owns separators.
- ResourceLink requires URI, root, capability, and authority validation before
  Requested.
- Embedded text requires supported text MIME and encoding and becomes an exact
  `EmbeddedText` input block.
- Image, audio, blob, unknown content, malformed URI, unsupported MIME, or an
  unsupported schema returns `UnsupportedBeforeReservation` unless the runtime
  model capability explicitly adds a closed type in a later contract version.
- New/load validates declared MCP servers and additional directories before
  returning. It cannot ignore request fields.

### ACP ordering gates

```text
AcpPromptBindingState =
  Decoded
  | Reserved { operation_id: SurfaceOperationId }
  | Bound { operation_id: SurfaceOperationId }
  | TerminalGated { terminal_cursor: SurfaceCursor }
  | ResponseWriting
  | Completed
  | TransportRetired {
      reason: AcpTransportRetireReason,
      operation_id: Option<SurfaceOperationId>,
      request_id_tombstone: AcpRequestTombstone,
    }

AcpTransportRetireReason =
  ClientCancelled | RuntimeTerminal | InteractionClosed | Oversize
  | WriteFailed | WriteTimedOut | ConnectionClosed | SupervisorShutdown

AcpRequestClass = Prompt | InteractionReverse | CapabilityCall

AcpRequestTombstone {
  request_id: AcpRequestId,
  class: AcpRequestClass,
  operation_id: Option<SurfaceOperationId>,
  interaction_id: Option<SurfaceInteractionId>,
  capability_call_id: Option<SurfaceCapabilityCallId>,
  reason: AcpTransportRetireReason,
  retired_at: MonotonicInstant,
  expires_at: MonotonicInstant,
  observed_retired_at: Option<UnixMillis>,
  observed_expires_at: Option<UnixMillis>,
}

AcpLoadGateState =
  BaselineStreaming { cursor: SurfaceCursor }
  | BaselineFlushed { cursor: SurfaceCursor }
  | ResponseWriting
  | ResponseCommitted
  | Live
```

The read loop assigns `inbound_seq` before dispatch. A later-read cancel cannot
run before the prompt reaches Reserved and Bound. A PromptResponse is eligible
only after runtime Terminal, every prior update through its cursor is physically
`write_all + flush` acknowledged, and the prompt binding enters
`ResponseWriting`. A load response is eligible only after the complete
capability-valid baseline is flushed; live frames wait for `ResponseCommitted`.
ACP tombstone lookup and eviction compare only same-clock monotonic instants.
Clock mismatch, overflow, or issuing-host loss seals the connection-local
tombstone set until supervisor recovery; wall-clock display metadata never
authorizes reuse or eviction of a request id.

### ACP frame and connection failure

Oversize and write failures follow the parent design's directionality table.
The private stable error names are:

```text
RequestTooLarge
ResponseTooLarge
BaselineTooLarge
SurfaceSnapshotRequired
UnsupportedContent
OrcaInvalidInput
OrcaBusy
OrcaCapacityExceeded
ClientCapabilityUnavailable
ConnectionWriteFailed
ConnectionWriteTimedOut
OrcaBudgetExhausted
OrcaOperationFailed
OrcaOperationPanicked
OrcaOperationJoinFailed
OrcaRuntimeRestarted
OrcaNotAdmitted
OrcaTerminalDegraded
```

These are semantic error identities, not yet public JSON codes. Phase 4A fixes
their exact public representation. A failed or timed-out `write_all` or `flush`
seals the connection, cancels its token, fails all physical acknowledgements and
local waiters, settles every capability call exactly once, detaches its
attachments, and joins or aborts every owned task within the fixed deadline.
Every retired request creates exactly one tombstone whose class-specific
identity field is populated; the other two are absent. Duplicate or late
responses consult the tombstone and cannot cross the interaction/capability
ledgers or reopen a prompt binding.

### ACP capability calls

Capability calls use the Tool patches and state matrix in this contract. An
interaction reverse response and a capability response are disjoint request-id
classes. Oversize, unknown, or duplicate responses never cross those ledgers.
After `DeliveryPossible`, a side-effecting method is never automatically
retried. Runtime-owned tool settlement may fail an operation from an ambiguous
capability fact; ACP transport may not choose or publish the terminal.

Before constructing `CapabilityCallResult`, runtime canonical-encodes the
complete decoded variant and requires its size to be at most
`ACP_CAPABILITY_RESULT_CANONICAL_BYTE_LIMIT`; the inbound line limit is not this
proof. Identifier and text constructors apply their stricter component limits
first. An oversized result never enters `Completed` and is never silently
truncated by the adapter. ReadTextFile, TerminalOutput, and TerminalWaitForExit
settle as `ObservationUnavailable`; WriteTextFile settles as
`ExternalEffectAmbiguous(FileWrite)`; TerminalCreate also records
`IdentityUnknown(call_id)`; TerminalKill and TerminalRelease record
`CleanupAmbiguous`. These are the same post-delivery rows as an unobservable
response, so a size failure cannot trigger an automatic side-effect retry.

### ACP pre-reservation prompt failures

These failures occur before an operation is reserved and therefore never
construct `OperationTerminal::NotAdmitted`:

```text
AcpPreReservationPromptFailure::InvalidInput
  -> OrcaInvalidInput RPC rejection
AcpPreReservationPromptFailure::OperationActive
  -> OrcaBusy RPC rejection
AcpPreReservationPromptFailure::CapacityExceeded
  -> OrcaCapacityExceeded RPC rejection
```

### ACP terminal mapping

```text
OperationTerminal::Succeeded             -> PromptResponse::EndTurn
OperationTerminal::Cancelled             -> PromptResponse::Cancelled
OperationTerminal::BudgetExhausted(ModelTokens) -> PromptResponse::MaxTokens
OperationTerminal::BudgetExhausted(TurnRequests{AgentLoop})
                                           -> PromptResponse::MaxTurnRequests
OperationTerminal::BudgetExhausted(TurnRequests{Subagent}
  | GoalTokenBudget | WorkflowTokenBudget | MonetaryBudgetUsdMicros
  | ToolCalls | WallTimeMs)
                                           -> OrcaSurfaceV1 exact terminal metadata;
                                              StandardOnly OrcaBudgetExhausted
OperationTerminal::Failed {
  class: Provider | Tool | Hook | Workflow | Verification | InputResolution
       | ClientCapabilityUnavailable | LegacyApprovalRequired | RuntimeInvariant
       | Persistence | ExternalEffectAmbiguous | RemoteResourceCleanupAmbiguous
}                                         -> OrcaOperationFailed
OperationTerminal::Panicked              -> OrcaOperationPanicked
OperationTerminal::JoinFailed            -> OrcaOperationJoinFailed
OperationTerminal::AbortedByRuntimeRestart -> OrcaRuntimeRestarted
OperationTerminal::Shutdown              -> PromptResponse::Cancelled
OperationTerminal::NotAdmitted(CancelledBeforeAdmission)
                                           -> PromptResponse::Cancelled
OperationTerminal::NotAdmitted(
  ReservationExpired | ConfigurationConflict | PolicyConflict
  | RuntimeRestart | HostShutdown | ThreadClose
)                                          -> OrcaNotAdmitted RPC rejection;
                                              no fake turn
FinalizingDegraded/TerminalCommitFailure/TerminalProjectionFailure
                                           -> OrcaTerminalDegraded;
                                               no PromptResponse terminal
```

Background transfer is not a PromptResponse. The prompt binding remains open
until the background operation's real Terminal or transport retirement.

## JSONL Compatibility Contract

### Supervisor and routing ownership

```text
JsonlSupervisorState =
  Open
  | IngressClosed {
      trigger: JsonlSupervisorCloseTrigger,
      repair_plan: JsonlCommittedRepairPlan,
    }
  | RoutesRetired {
      trigger: JsonlSupervisorCloseTrigger,
      committed_repairs: JsonlCommittedRepairSettlementSet,
    }
  | ServicesSettled {
      evidence: JsonlAssessedCloseEvidence,
    }
  | RuntimeShutdownPending {
      evidence: JsonlAssessedCloseEvidence,
    }
  | Closed { result: JsonlSupervisorCloseResult }

JsonlNonIoCloseTrigger =
  EndOfFile
  | NormalInputClose
  | SupervisorShutdown

JsonlSupervisorIoFailure =
  ReadFailed { error: SafeDiagnosticText }
  | EncodeFailed { error: SafeDiagnosticText }
  | WriteFailed { error: SafeDiagnosticText }
  | FlushFailed { error: SafeDiagnosticText }

JsonlSupervisorCloseTrigger =
  NonIo { trigger: JsonlNonIoCloseTrigger }
  | Io { failure: JsonlSupervisorIoFailure }

JsonlSupervisorCloseResult =
  Clean {
    shutdown: ShutdownHostOutput,
    evidence: JsonlAssessedNonIoCloseEvidence::Healthy,
  }
  | CleanupDegraded {
      shutdown: ShutdownHostOutput,
      evidence: JsonlAssessedNonIoCloseEvidence::Degraded,
    }
  | ShutdownFailed {
      error: SafeDiagnosticText,
      evidence: JsonlAssessedNonIoCloseEvidence,
    }
  | IoFailed {
      shutdown_health: Option<SafeDiagnosticText>,
      evidence: JsonlAssessedIoCloseEvidence,
    }

JsonlSupervisorCloseEvidence =
  NonIo {
    trigger: JsonlNonIoCloseTrigger,
    committed_repairs: JsonlCommittedRepairSettlementSet,
    services: JsonlServiceSettlements,
  }
  | Io {
      failure: JsonlSupervisorIoFailure,
      committed_repairs: JsonlCommittedRepairSettlementSet,
      services: JsonlServiceSettlements,
    }

JsonlAssessedNonIoCloseEvidence =
  Healthy { evidence: JsonlSupervisorCloseEvidence::NonIo }
  | Degraded {
      evidence: JsonlSupervisorCloseEvidence::NonIo,
      issues: NonEmptyVec<JsonlCloseHealthIssue>,
    }

JsonlAssessedIoCloseEvidence =
  Healthy { evidence: JsonlSupervisorCloseEvidence::Io }
  | Degraded {
      evidence: JsonlSupervisorCloseEvidence::Io,
      issues: NonEmptyVec<JsonlCloseHealthIssue>,
    }

JsonlAssessedCloseEvidence =
  NonIo { evidence: JsonlAssessedNonIoCloseEvidence }
  | Io { evidence: JsonlAssessedIoCloseEvidence }

JsonlServiceKind = CommandExec | Shell | FileSearch | MentionSearch

JsonlServiceSettlementState =
  Joined
  | AbortedAfterDeadline
  | CleanupUnconfirmed {
      health: SafeDiagnosticText,
    }

JsonlServiceSettlements {
  command_exec: JsonlServiceSettlementState,
  shell: JsonlServiceSettlementState,
  file_search: JsonlServiceSettlementState,
  mention_search: JsonlServiceSettlementState,
}

JsonlCommittedRepairOwner =
  ThreadPermission | DirectUserInput | DirectMcpElicitation

JsonlCommittedRepairKey {
  connection_id: SurfaceConnectionId,
  opaque_request_id: NonEmptyText,
  owner: JsonlCommittedRepairOwner,
  retirement_sequence: JsonlRetirementSequence,
}

JsonlCommittedRepairExpected {
  key: JsonlCommittedRepairKey,
  committed_settlement_digest: Sha256Digest,
  repair_digest: Sha256Digest,
}

JsonlConnectionRepairAuthorityBudget(
  opaque, connection-scoped, capacity=JSONL_REPAIR_AUTHORITY_LIMIT)
JsonlRepairAuthorityPermit(
  opaque, non-cloneable, connection-scoped, moves with durable repair ownership)
DurableDeferredRepairRecordId(SurfaceSettlementId)
JsonlRepairExecutionLease(
  opaque, process-local, non-authoritative, bound to one durable record)

JsonlDurableRepairRecordReceipt {
  record_id: DurableDeferredRepairRecordId,
  target: RuntimeDeferredRepairTarget,
  repair_digest: Sha256Digest,
  store_commit_id: SurfaceCommitId,
  state: Pending | PendingRecovery | FailedAttempt,
  proof: OpaqueToken,
}

JsonlDurableRepairSettlementAck =
  Completed {
    record_id: DurableDeferredRepairRecordId,
    store_commit_id: SurfaceCommitId,
    proof: OpaqueToken,
  }
  | RetainedForRecovery {
      record_id: DurableDeferredRepairRecordId,
      store_commit_id: SurfaceCommitId,
      next_owner_epoch: Option<ThreadOwnerEpoch>,
      proof: OpaqueToken,
    }

JsonlCommittedRepairPlanEntry {
  expected: JsonlCommittedRepairExpected,
  committed_settlement: JsonlCommittedRequestSettlement,
  durable_record: JsonlDurableRepairRecordReceipt,
  execution_lease: JsonlRepairExecutionLease,
}

JsonlCommittedRepairPlan {
  entries: Vec<JsonlCommittedRepairPlanEntry>,
  plan_digest: Sha256Digest,
}

JsonlClosePlannedEntry {
  key: JsonlCommittedRepairKey,
  plan_digest: Sha256Digest,
  entry_ordinal: u64,
}

RuntimeDeferredRepairTarget =
  JsonlCommittedRequest { key: JsonlCommittedRepairKey }
  | JsonlInteractionUnavailable {
      connection_id: SurfaceConnectionId,
      thread_id: SurfaceThreadId,
      interaction_id: SurfaceInteractionId,
      owner: JsonlCommittedRepairOwner,
      route_epoch: ResponseRouteEpoch,
      disposition: InteractionUnavailableDisposition,
    }

RuntimeDurableRepairTransferReceipt {
  host_incarnation: HostIncarnation,
  record: JsonlDurableRepairRecordReceipt,
  transfer_commit_id: SurfaceCommitId,
  proof: OpaqueToken,
}

JsonlCommittedRepairSettlement =
  Completed {
    key: JsonlCommittedRepairKey,
    tombstone: JsonlRequestTombstone,
    repair_ack: JsonlDurableRepairSettlementAck::Completed,
  }
  | RetainedPending {
      key: JsonlCommittedRepairKey,
      tombstone: JsonlRequestTombstone,
      durable_record: JsonlDurableRepairRecordReceipt,
      transfer: RuntimeDurableRepairTransferReceipt,
      repair_ack: JsonlDurableRepairSettlementAck::RetainedForRecovery,
    }
  | FailedRetained {
      key: JsonlCommittedRepairKey,
      tombstone: JsonlRequestTombstone,
      durable_record: JsonlDurableRepairRecordReceipt,
      transfer: RuntimeDurableRepairTransferReceipt,
      repair_ack: JsonlDurableRepairSettlementAck::RetainedForRecovery,
      health: SafeDiagnosticText,
    }

JsonlCommittedRepairSettlementSet {
  plan_digest: Sha256Digest,
  expected: Vec<JsonlCommittedRepairExpected>,
  settlements: Vec<JsonlCommittedRepairSettlement>,
}

JsonlCloseHealthIssue =
  CommittedRepairTimedOut {
    key: JsonlCommittedRepairKey,
    repair_digest: Sha256Digest,
  }
  | CommittedRepairFailed {
      key: JsonlCommittedRepairKey,
      repair_digest: Sha256Digest,
      message: SafeDiagnosticText,
    }
  | ServiceAbortedAfterDeadline { service: JsonlServiceKind }
  | ServiceCleanupUnconfirmed {
      service: JsonlServiceKind,
      message: SafeDiagnosticText,
    }

JsonlRouteOwner =
  RuntimeSurfaceHost
  | RuntimeSurfaceThread
  | OpaquePermissionRouter
  | DirectUserInputResponder
  | DirectMcpElicitationResponder
  | CommandExecService
  | ShellService
  | FileSearchService
  | MentionSearchService
```

`Open` is constructible only after the runtime host installs one
`JsonlConnectionRepairAuthorityBudget` for the connection; budget installation
failure rejects the connection before ingress and creates no supervisor. The only legal supervisor
transitions are the listed sequence from Open through Closed; a stage may stay
in place while bounded join/repair progresses but may not skip backward or start
another shutdown rail. Entry to `IngressClosed`
first forbids new admission, resolves every finite `Routed` response-commit
versus retirement CAS, then freezes exactly the remaining `CommittedPending`
entries and their non-cloneable repair authorities into one
`JsonlCommittedRepairPlan`. `RoutesRetired` retires only permission/direct
response routes and drains that frozen plan under one absolute
`JSONL_COMMITTED_REPAIR_DRAIN_DEADLINE_MS` deadline. `ServicesSettled` then
detaches adapter surface attachments and cancels and settles the four services
under one absolute `JSONL_SUPERVISOR_JOIN_DEADLINE_MS` deadline. It does not
close a runtime actor or discard the supervisor's retained host shutdown handle.
The close matrix is exact:

| Trigger | Primary result | Route retirement | Service action | Runtime action | Wire after trigger |
| --- | --- | --- | --- | --- | --- |
| EndOfFile / NormalInputClose | Clean when shutdown succeeds and cleanup health is empty; CleanupDegraded when shutdown succeeds with cleanup health; otherwise ShutdownFailed | retire every permission/direct response route, preserve committed receipts, and revoke grants | detach adapter surfaces; cancel and settle command-exec, shell, file-search, and mention-search | call ShutdownHost exactly once and wait for the retained complete output | none |
| ReadFailed | IoFailed(ReadFailed); shutdown failure is secondary health | same | same | same | none |
| EncodeFailed | IoFailed(EncodeFailed); shutdown failure is secondary health | CAS the affected uncommitted Writing route to transport-retired, then retire every permission/direct response route; preserve committed receipts | same | same | same | none |
| WriteFailed | IoFailed(WriteFailed); shutdown failure is secondary health | same; Published entries remain semantically committed | same | same | none |
| FlushFailed | IoFailed(FlushFailed); shutdown failure is secondary health | same; frame_may_have_been_observed=true | same | same | none |
| SupervisorShutdown | Clean when shutdown succeeds and cleanup health is empty; CleanupDegraded when shutdown succeeds with cleanup health; otherwise ShutdownFailed | retire every permission/direct response route, preserve committed receipts, and revoke grants | same | same | none |

For each `CommittedPending` entry the close-owned drain either completes its
existing private repair or reaches the shared repair deadline; it never creates
a new semantic response. `Completed`, `RetainedPending`, and `FailedRetained`
all carry a tombstone whose settlement is the original
`PermissionCommitted` or `DirectInteractionCommitted` receipt.
`RetainedPending` records deadline exhaustion, and `FailedRetained` records a
repair failure; neither may be rewritten as `TransportRetired`. Before either
variant can tombstone the route, the router durably records the exact deferred
repair under its admission-reserved `JsonlRepairAuthorityPermit`, then transfers
that durable record to the host recovery owner. The matching
`RuntimeDurableRepairTransferReceipt` and
`JsonlDurableRepairSettlementAck::RetainedForRecovery` are part of the
settlement. A digest or process-local execution lease without those receipts is
not repair authority and cannot construct a settlement. `ShutdownHost` includes
every accepted durable record in its immutable repair barrier and cannot commit
its final HostLifecycle receipt while dropping one.
Their health is recorded in `JsonlCloseHealthIssue`, while runtime remains
authoritative for later projection reconciliation.

The repair plan and settlement set use canonical order
`(retirement_sequence, owner_rank, opaque_request_id UTF-8 bytes)`. Plan keys are
unique. `plan_digest` covers every expected identity and digest but never opaque
authority bytes. ThreadPermission requires Permission authority;
DirectUserInput requires DirectInteraction(UserInput), and
DirectMcpElicitation requires DirectInteraction(McpElicitation). Authority target,
committed receipt, and expected key must agree before the plan can be frozen.
The settlement-set constructor requires the byte-identical
plan digest and exactly one matching settlement for every expected key, with no
missing, duplicate, extra, or reordered member. Each tombstone, committed
settlement digest, repair digest, and handoff target must match that expected
entry. An empty plan is legal, but cannot weaken service coverage.

Each of CommandExec, Shell, FileSearch, and MentionSearch is cancelled exactly
once and occupies its dedicated field in `JsonlServiceSettlements`. A service
that joins before the shared deadline is `Joined`; after the deadline the
supervisor aborts it and records `AbortedAfterDeadline` only when cancellation
is observed, otherwise it records `CleanupUnconfirmed`. There is no empty,
missing, duplicate, extra, or reordered service collection. These settlements
and all committed-repair settlements are carried without loss into
`RuntimeShutdownPending` and the final close evidence.
The immutable close trigger is carried with them. One private assessment
constructor derives cleanup health from the complete evidence; callers cannot
supply an arbitrary issue vector. Its issues are a sorted exact bijection: each
`RetainedPending`, `FailedRetained`,
`AbortedAfterDeadline`, or `CleanupUnconfirmed` settlement contributes exactly
its matching `JsonlCloseHealthIssue`, while `Completed` and `Joined` contribute
none. `Clean` therefore requires every repair to be `Completed` and all four
fixed service fields to be `Joined`. I/O failure identity and diagnostic exist
once inside `JsonlAssessedIoCloseEvidence`; `IoFailed` has no second kind/error
field that could disagree with it. Non-I/O result variants accept only the
matching assessed non-I/O refinement.
Cleanup degradation never skips, retries, or duplicates the sole `ShutdownHost`
call. Result precedence is closed: an I/O trigger always yields `IoFailed`; for a
non-I/O trigger a shutdown failure yields `ShutdownFailed`; otherwise any close
health yields `CleanupDegraded`; only an empty health set yields `Clean`.

Routing is selected once before a processor may inspect or remove mutable state:

| Released input | Sole owner | Forbidden fallback |
| --- | --- | --- |
| thread/session discovery, create, resume, fork, metadata | RuntimeSurfaceHost | direct catalog/store access |
| turn/start, stateless submit, thread-bound mutations | RuntimeSurfaceThread obtained from host | adapter-owned active-turn/session maps |
| turn/interrupt, turn/resume, turn/steer | RuntimeSurfaceHost::ControlJsonlTurn | lookup then direct actor control |
| permission/respond | OpaquePermissionRouter | direct thread responder or command-exec probe |
| user_input/respond | DirectUserInputResponder | permission router or pending-manager removal |
| mcp_elicitation/respond | DirectMcpElicitationResponder | permission router or pending-manager removal |
| command/exec operations | CommandExecService | SurfaceOperation fabrication |
| shell operations | ShellService | command-exec or thread fallback |
| fuzzy file search | FileSearchService | direct filesystem query from router |
| mention search | MentionSearchService | file-search fallback after dispatch |

Unknown, duplicate, or malformed input is answered only by its selected owner;
the router never probes a second ledger. Once `IngressClosed` is entered, no row
may accept a new frame or emit a response.

### Session and host mapping

| Released request | Private command | Released response |
| --- | --- | --- |
| thread/list | ListSessions with exact filters/sort/search | thread_list |
| thread/search | SearchSessions with exact sort/archive mode | thread_search |
| thread/read | ReadSession at one read token | thread_read |
| thread/turns/list | ReadThreadPage(Turns{direction,items_view}) | thread_turns_list |
| thread/items/list | ReadThreadPage(Items{turn_id,direction}) | thread_items_list |
| thread/metadata/update | UpdateSessionMetadata(SetTitle, LegacyLastWriteWins) | thread_metadata_updated |
| thread/start | CreateThread(RecordedCatalogued) | thread_started |
| thread/resume | LoadThread(settings_overrides,MCP declarations) | thread_started with same id |
| thread/fork | ForkThread(settings_overrides) | thread_started with new id |
| turn/start with threadId | ResolveRunningThread(LiveOnly), apply typed permission overrides before Requested, reserve one operation | existing streamed turn events |
| op:submit or turn/start without threadId | CreateThread(EphemeralNonCataloguedOneShot), reserve one operation | turn events only; no thread_started/catalog record |
| turn/interrupt | ControlJsonlTurn(Interrupt) | turn_controlled or exact existing error |
| turn/resume | ControlJsonlTurn(Resume) | turn_controlled or exact existing error |
| turn/steer | ControlJsonlTurn(Steer) | turn_controlled then steer item, or exact existing error |
| thread/close | no command; preserve unsupported-method error | error |
| server EOF, normal close, read/encode/write/flush error, or supervisor shutdown | the sole `JsonlServerSupervisor` close machine ending in `ShutdownHost` | no response after close begins |

`JsonlServerSupervisor` owns the sole connection and the non-surface services
that remain for compatibility. Every close trigger first closes ingress, then
retires every opaque-router/request id and direct user-input/MCP responder,
detaches surface attachments, cancels and bounded-joins command-exec, shell, and
search services, and finally calls `ShutdownHost` and waits for its complete
barrier before process exit. An ephemeral one-shot thread is included in that
barrier and cannot remain discoverable. No individual processor starts a second
shutdown rail. Direct user-input/MCP request frames use the same
Registered/Writing/Published physical-ack and retirement CAS as the permission
router; write failure revokes their bound response grant and uses the persisted
interaction unavailable disposition.

The failure priority is closed: a read/encode/write/flush error remains the primary
returned I/O error and a shutdown failure is attached as secondary health
evidence; EOF/normal close returns a shutdown failure when one occurs, otherwise
success. Once close begins no later request frame is accepted or emitted. A
physical writer failure uses the opaque-router CAS rules above before this
supervisor sequence, so a committed response is never compensated and an
uncommitted route cannot remain live.

`turn/start` never implicitly loads a cold recorded thread. A second turn on a
busy thread uses the runtime-owned `NotAdmittedImmediately` policy and preserves
the existing `thread has an active turn` error instead of joining the general
reservation FIFO.

The compatibility decoder keeps the released wire names and defaults exactly:
`thread/list` and `thread/search` use `searchTerm` (the private search DTO may
call the value `query`), default `sortDirection` to descending,
`sortKey` to `updatedAt`, and `limit` to `50`; `thread/turns/list` and
`thread/items/list` default direction to ascending, `itemsView` to `full`, and
`limit` to `50`. Unknown enum strings take those same decoder defaults rather
than becoming a new typed variant. An empty `thread/search` `searchTerm`
retains the released correlated error text `thread search term must not be
empty` and does not allocate a surface command or operation.

A `thread/metadata/update` request without `title` fails decoder preflight with
the exact released message `thread metadata patch did not include any supported
fields`; it does not allocate `UpdateSessionMetadata` or mutate a revision.

Released `PermissionProfileOverride` and `permissionUpdates` decode only into
closed `RuntimeSettingsPatch` values. On `thread/resume` they are part of the
atomic load/settings receipt. On `turn/start` they use
`ApplyThreadOverridesBeforeRequested`: the Settings fact and host receipt must
commit before Requested, and the resulting settings revision/policy epoch are
frozen into the operation. Failure emits the existing correlated error and no
operation fact. JSONL never mutates `RunConfig` or session metadata itself.
Presence is distinct from omission: `runtimeWorkspaceRoots: []` emits
`SetWorkspaceRoots { roots: [] }` and explicitly clears the roots, while an
omitted field emits no patch. An empty `permissionUpdates` array is consumed as
the released no-op and emits no patch or Settings revision; it is never treated
as invalid input.

### JSONL turn lifecycle

One released turn is one `SurfaceOperation`. Its released `turnId` is fixed at
admission. `turn/resume` never creates another operation; it can only request a
replacement generation under the same operation, turn, and input identity.

For the JSONL interrupt policy:

1. `ControlJsonlTurn(Interrupt)` resolves the live turn and commits the exact
   `InterruptGeneration` intent in one actor-ordered command; only then
   may the adapter emit `turn_controlled(status=interrupted)`.
2. A `ControlJsonlTurn(Resume)` actor-ordered before interrupted Stopped processing commits
   idempotent `ResumeAfterInterruptedStop`; the adapter then emits
   `turn_controlled(status=resumed)` and runtime starts the next generation.
3. If no resume is already actor-ordered when Stopped is processed, runtime
   enters its Cancel finalizer immediately and commits Terminal(Cancelled).
4. There is no timer, grace task, adapter flag, or adapter terminal inference.
5. After Terminal, resume returns the released not-active error.

`turn_completed` is emitted only from `OperationTerminalAtCursor` and is the
final event for that request id:

```text
Succeeded -> success
Failed { class: Verification } -> verification_failed
Failed { class: LegacyApprovalRequired } -> approval_required
Failed { class: Provider | Tool | Hook | Workflow | InputResolution
       | ClientCapabilityUnavailable | RuntimeInvariant | Persistence
       | ExternalEffectAmbiguous | RemoteResourceCleanupAmbiguous } -> failed
Panicked | JoinFailed | AbortedByRuntimeRestart -> failed
Cancelled -> cancelled
Shutdown -> cancelled
BudgetExhausted(ModelTokens | TurnRequests{AgentLoop} | TurnRequests{Subagent}
              | GoalTokenBudget | WorkflowTokenBudget | MonetaryBudgetUsdMicros)
              -> budget_exhausted
NotAdmitted -> existing correlated error; suppress turn_started/turn_completed
```

NotAdmitted, pre-start validation, unknown thread, busy, and classified input
resolution failures retain the existing single `error` shape with no
`turn_started` or `turn_completed`. Internally committed facts are not erased.

`LegacyApprovalRequired` is assigned only by typed ingress for a baseline
terminal `RunStatus::ApprovalRequired` source that cannot become a live durable
interaction. A real pending approval is always an Interaction plus a waiting or
Suspended operation and is never mislabeled terminal for compatibility.

### JSONL binding visibility

Structured bindings resolve only after typed Generation Started. For
`LegacyVisibility::JsonlBindingsResolvedBeforeTurnStarted`, the adapter delays
the first legacy `turn_started` until `OperationPatch::InputBindingsResolved`.
Every released `turn_started` is projected from
`OperationPatch::AgentLoopTurnStarted`, including its ordinal and agent-task
metadata; `GenerationStarted` is only the once-per-generation execution barrier.
Later agent-loop iterations under the same generation therefore retain their
own released `turn_started` event. On binding failure
it waits for runtime Terminal, emits only the existing correlated error, and
suppresses both legacy start and completion for that classified compatibility
case. This is a frozen presentation mapping, not terminal authority.

The following currently accepted input members remain explicit compatibility
degradation in v1 and are never represented as supported typed input:

```text
image item                -> LegacyAcceptedDropped
localImage item           -> LegacyAcceptedDropped
incomplete skill item     -> LegacyAcceptedDropped
untagged string params.input -> LegacyAcceptedDropped
```

A future public wire version may reject them. Phase 5 cannot silently change
the `v0.2.50` differential corpus while claiming compatibility.

### JSONL gap behavior

The exact correlated gap message is:

```text
thread surface snapshot required; reconnect and resume the thread
```

With an open correlated RPC, the adapter writes and flushes one existing
`{id,event:"error",message}` frame using that literal, permanently retires the
binding, and emits no later frame with that id. Without an open RPC, it emits no
new event, drains already-admitted pre-gap writes, seals the connection, and
requires a new connection plus `thread/resume`. JSONL never gains a cursor or
invented gap event.

### JSONL opaque interaction ids

```text
JsonlCommandExecServiceFence {
  connection_id: SurfaceConnectionId,
  service_request_id: SurfaceRequestId,
  policy_epoch: PolicyEpoch,
}

JsonlBoundInteractionResponder(opaque, process-local, attachment-bound)
JsonlOpaquePermissionRouter(opaque, process-local, host-owned, connection-scoped)
JsonlPermissionPhysicalWriteAck(OpaqueToken)
JsonlPermissionRouterRepair(
  opaque, process-local, owns exactly one non-cloneable DeferredRepair,
  never returned to the wire adapter)

JsonlOpaquePermissionRoute =
  ThreadInteraction {
    thread_id: SurfaceThreadId,
    interaction_id: SurfaceInteractionId,
    responder: JsonlBoundInteractionResponder,
  }
  | CommandExecPermission {
      fence: JsonlCommandExecServiceFence,
    }

JsonlPermissionPublicationState =
  Registered
  | Writing { frame_digest: Sha256Digest }
  | Published {
      frame_digest: Sha256Digest,
      physical_ack: JsonlPermissionPhysicalWriteAck,
    }

JsonlPermissionResolutionReceipt {
  opaque_request_id: NonEmptyText,
  response_id: SurfaceResponseId,
  decision: Allow | Deny,
  scope: PermissionGrantScope,
  strict_auto_review: bool,
}

JsonlDirectInteractionKind = UserInput | McpElicitation

JsonlDirectInteractionResolutionReceipt {
  opaque_request_id: NonEmptyText,
  kind: JsonlDirectInteractionKind,
  receipt: SurfaceInteractionResolutionReceipt,
}

JsonlRetirementSequence(u64)

JsonlRetiredRequestOwner =
  ThreadPermission
  | CommandExecPermission
  | DirectUserInput
  | DirectMcpElicitation

JsonlLiveAdmissionFailureReason =
  LiveLimitReached
  | RetirementSequenceExhausted
  | OpaqueIdExhausted

JsonlLiveRequestAdmission {
  connection_id: SurfaceConnectionId,
  opaque_request_id: NonEmptyText,
  owner: JsonlRetiredRequestOwner,
  retirement_sequence: JsonlRetirementSequence,
  repair_authority_permit: JsonlRepairAuthorityPermit,
}

JsonlInteractionAdmissionRejectionIdentity {
  connection_id: SurfaceConnectionId,
  thread_id: SurfaceThreadId,
  interaction_id: SurfaceInteractionId,
  owner: JsonlCommittedRepairOwner,
  route_epoch: ResponseRouteEpoch,
  disposition: InteractionUnavailableDisposition,
}

RuntimeInteractionRecoveryWitness {
  identity: JsonlInteractionAdmissionRejectionIdentity,
  thread_owner_epoch: ThreadOwnerEpoch,
  interaction_revision: InteractionRevision,
  durable_revision: DurableRevision,
  request_commit_id: SurfaceCommitId,
  proof: OpaqueToken,
}

JsonlInteractionAdmissionRejectionOutcome =
  Applied {
    acknowledgements: NonEmptyVec<MutationCommitAck>,
  }
  | DeferredToRuntime {
      transfer: RuntimeDurableRepairTransferReceipt,
    }
  | RecoveryRetained {
      witness: RuntimeInteractionRecoveryWitness,
      error: SurfaceMutationError,
    }

JsonlCommandExecFenceStage = FailedBeforeExecution

JsonlCommandExecFenceFailureReceipt {
  fence: JsonlCommandExecServiceFence,
  service_revision: Revision,
  failure_id: SurfaceSettlementId,
  stage: JsonlCommandExecFenceStage::FailedBeforeExecution,
  proof: OpaqueToken,
}

JsonlOwnerSettlement =
  Interaction {
    identity: JsonlInteractionAdmissionRejectionIdentity,
    outcome: JsonlInteractionAdmissionRejectionOutcome,
  }
  | CommandExec {
      receipt: JsonlCommandExecFenceFailureReceipt,
    }

JsonlLiveRequestAdmissionResult =
  Admitted { admission: JsonlLiveRequestAdmission }
  | Rejected {
      reason: JsonlLiveAdmissionFailureReason,
      settlement: JsonlOwnerSettlement,
    }

JsonlTransportRetireReason =
  EncodeFailed | WriteFailed | FlushFailed | ConnectionClosed

JsonlRetiredRequestSettlement =
  PermissionCommitted {
    receipt: JsonlPermissionResolutionReceipt,
    keyed_response_digest: OpaqueToken,
  }
  | DirectInteractionCommitted {
      receipt: JsonlDirectInteractionResolutionReceipt,
      keyed_response_digest: OpaqueToken,
    }
  | TransportRetired {
      reason: JsonlTransportRetireReason,
      frame_may_have_been_observed: bool,
      owner_settlement: JsonlOwnerSettlement,
    }

JsonlRequestTombstone {
  connection_id: SurfaceConnectionId,
  opaque_request_id: NonEmptyText,
  owner: JsonlRetiredRequestOwner,
  settlement: JsonlRetiredRequestSettlement,
  retired_at: MonotonicInstant,
  expires_at: MonotonicInstant,
  retirement_sequence: JsonlRetirementSequence,
}

JsonlOpaquePermissionEntryState =
  Routed {
    admission: JsonlLiveRequestAdmission,
    route: JsonlOpaquePermissionRoute,
    publication: JsonlPermissionPublicationState,
  }
  | CommittedPending {
      admission: JsonlLiveRequestAdmission,
      owner: JsonlOpaquePermissionRoute::ThreadInteraction,
      publication: Writing | Published,
      receipt: JsonlPermissionResolutionReceipt,
      keyed_response_digest: OpaqueToken,
      repair: JsonlPermissionRouterRepair,
    }
  | Tombstoned { tombstone: JsonlRequestTombstone }

JsonlOpaquePermissionRespondResult =
  Committed { receipt: JsonlPermissionResolutionReceipt }
  | DeferredCommitted { receipt: JsonlPermissionResolutionReceipt }
  | AlreadyCommitted { receipt: JsonlPermissionResolutionReceipt }
  | AlreadyConsumed
  | RetryableUncommitted { error: SurfaceMutationError }
  | UnknownRequest

JsonlDirectInteractionPhysicalWriteAck(OpaqueToken)
JsonlDirectInteractionRepair(
  opaque, process-local, owns exactly one non-cloneable DeferredRepair,
  never returned to the wire adapter)

JsonlDirectInteractionRoute {
  connection_id: SurfaceConnectionId,
  thread_id: SurfaceThreadId,
  interaction_id: SurfaceInteractionId,
  kind: JsonlDirectInteractionKind,
  responder: JsonlBoundInteractionResponder,
}

JsonlDirectInteractionPublicationState =
  Registered
  | Writing { frame_digest: Sha256Digest }
  | Published {
      frame_digest: Sha256Digest,
      physical_ack: JsonlDirectInteractionPhysicalWriteAck,
    }

JsonlDirectInteractionEntryState =
  Routed {
    admission: JsonlLiveRequestAdmission,
    route: JsonlDirectInteractionRoute,
    publication: JsonlDirectInteractionPublicationState,
  }
  | CommittedPending {
      admission: JsonlLiveRequestAdmission,
      route: JsonlDirectInteractionRoute,
      publication: Writing | Published,
      receipt: JsonlDirectInteractionResolutionReceipt,
      keyed_response_digest: OpaqueToken,
      repair: JsonlDirectInteractionRepair,
    }
  | Tombstoned { tombstone: JsonlRequestTombstone }

JsonlDirectInteractionRespondResult =
  Committed { receipt: JsonlDirectInteractionResolutionReceipt }
  | DeferredCommitted { receipt: JsonlDirectInteractionResolutionReceipt }
  | AlreadyCommitted { receipt: JsonlDirectInteractionResolutionReceipt }
  | AlreadyConsumed
  | RetryableUncommitted { error: SurfaceMutationError }
  | UnknownRequest
```

Released request ids remain adapter correlation only. Every
`permission/respond` MUST call `JsonlOpaquePermissionRouter.respond`; it never
calls a thread responder directly. The router chooses exactly one registered
owner. For a thread route it invokes the bound responder, whose runtime path
uses `InteractionSelector::OpaqueRequestId` and atomically validates attachment,
thread, kind, token, route, operation fence, answer policy, and authority before
consuming anything. A failed validation does not tombstone a still-valid
request. The non-thread command/exec permission service receives the same host
policy epoch and is never a thread interaction or SurfaceOperation.

The permission and direct ledgers share one connection admission lock and live
counter. Before encoding any request frame, one atomic registration first
expires tombstones at the injected monotonic `now`, verifies that the sum of
`Routed` and `CommittedPending` entries across both live maps is strictly below
`JSONL_LIVE_REQUEST_LIMIT`, checked-reserves the next collision-free opaque id
and the next connection-scoped `JsonlRetirementSequence`, reserves one
non-cloneable permit from the connection repair-authority budget, and
inserts the exact route with that admission. The sequence and repair-authority permit are
therefore available before any close race can require a tombstone or authority
transfer. Tombstones do not count toward the live limit.

At the limit no route or request frame is created. A ThreadPermission,
DirectUserInput, or DirectMcpElicitation rejection revokes any transient grant
and asks runtime to apply that interaction's already-persisted unavailable
disposition. The rejection can return only with a typed
`JsonlOwnerSettlement`: `Applied` carries the complete commit
acknowledgements, `DeferredToRuntime` proves runtime retained the exact
`DeferredRepair`, and `RecoveryRetained` is the failure branch whose unforgeable
witness proves the durable broker request, disposition, revision, and current
runtime recovery owner remain intact after the connection is sealed. A
CommandExecPermission rejection returns the exact service fence failure receipt
at `FailedBeforeExecution`, so no command side effect can start. Settlement
identity must match the rejected owner and interaction or fence; a bare reason,
missing acknowledgement, digest-only repair, or mismatched settlement cannot
construct `Rejected`.

Retirement-sequence or opaque-id exhaustion uses the same owner-specific
settlement and additionally seals ingress for that connection. A runtime
application error without either a deferred handoff or recovery witness returns
no rejection result and enters supervisor close while the runtime broker retains
the original interaction. Existing live entries are unchanged and remain able
to settle. The connection nonce plus checked opaque-id allocator never reuses an
id, including after admission failure, tombstone expiry, or eviction.

The writer CASes Routed/Registered to Writing, then to Published only after the
complete request frame receives `write_all + flush` acknowledgement. Response
lookup never removes a route before its owner returns a committed, deferred, or
terminal result. A response may consume only Writing (the peer may have observed
a complete partially flushed frame) or Published; Registered is not
response-eligible. A thread-owner Deferred atomically changes the route to
`CommittedPending`; the
semantic response is already consumed, the safe receipt and private keyed digest
are fixed, and only the router retains/runs the repair until projection/owner
acknowledgement reaches Tombstoned. No repair token crosses the adapter boundary.
The admission's repair-authority permit is released on an ordinary committed or
transport-retired tombstone. If a response becomes `CommittedPending`, that
permit is atomically consumed to create the unique durable repair record; only a
process-local `JsonlRepairExecutionLease` remains in the adapter. A deferred
unavailable disposition during route retirement follows the same durable-record
path before its tombstone. Closing or dropping the router cannot drop, clone, or
recreate the durable repair authority.
Wrong thread/kind/authority, stale policy, invalid answer, or store failure
returns `RetryableUncommitted` and leaves a Published route consumable.

Encoding/write/flush failure or connection retirement races a response commit
only while the entry is `Routed`. If retirement wins, the router first revokes
the route grant and constructs the exact `JsonlOwnerSettlement`: an interaction
must carry `Applied`, `DeferredToRuntime`, or `RecoveryRetained`, while a
command/exec route must carry its exact `FailedBeforeExecution` receipt. A
deferred interaction consumes the admission permit, creates its durable repair
record, and transfers that record to the runtime recovery owner before the
router may construct `Tombstoned(TransportRetired)`. Missing or mismatched owner
settlement leaves the entry live and forces supervisor recovery; a bare
transport reason can never tombstone it. If a response commit wins, the
entry becomes a committed tombstone or `CommittedPending`; every later transport
retirement preserves its receipt and can only run the existing repair. It never
compensates the semantic response or creates a `TransportRetired` settlement for
that id. The router never probes both owners, converts command/exec into a thread
operation, or stores an unbound attachment id.

The response id is derived from connection, inbound RPC, opaque request id, and
normalized body; `keyed_response_digest` is private and content-hiding. An exact
same-id/same-digest tombstone replay returns `AlreadyCommitted` with the
byte-identical safe receipt, from which the adapter emits the same released
`permission_resolved(decision,scope,strictAutoReview)` shape. A different RPC or
body for a consumed id returns `AlreadyConsumed` and the released
`unknown permission request: {request_id}` error; it cannot reveal or overwrite
the winning body. A transport-retired or absent id returns `UnknownRequest`.

`DeferredCommitted` uses the exact correlated gap behavior and message in the
preceding JSONL gap section: it emits no permission success, writes/flushes the
fixed error when the RPC is open, retires that RPC, and otherwise seals the lane.
It is never reclassified as `RetryableUncommitted`, and the committed-pending
route prevents another response from winning while the router executes repair.

`user_input/respond` and `mcp_elicitation/respond` use the direct interaction
ledger, never the permission router or the current pending-manager remove-first
path. The runtime registers the exact attachment-bound responder and interaction
kind before encoding the corresponding request frame. The writer performs the
same Registered -> Writing -> Published physical-ack CAS. A response is eligible
only in Writing or Published, its method must equal the stored kind, and the
bound responder constructs `RespondInteraction` with the runtime-injected token,
route epoch, grant, fence, answer policy, and response identity. UserInput maps
only its bounded answer; McpElicitation maps only the released accept/decline and
bounded opaque-content form allowed by
`LegacyJsonlV0250McpOpaqueContent`. Neither method can select another
interaction or authority fingerprint.

Direct response lookup does not remove the entry before runtime returns. A
semantic commit moves it to CommittedPending when projection/owner ack is still
missing, and only its private repair may finish it. Exact same-body replay uses
the content-hiding digest and returns the byte-identical safe receipt; changed
body or RPC returns AlreadyConsumed; retired/absent returns UnknownRequest and
the released `unknown user input request: {request_id}` or
`unknown MCP elicitation request: {request_id}` message. DeferredCommitted uses
the same correlated gap rule and emits no false `user_input_resolved` or
`mcp_elicitation_resolved`. Close/transport retirement CASes against response
commit while the entry is `Routed`, revokes the response grant, and applies the
interaction's persisted unavailable disposition. If commit already won, it
preserves the committed receipt and drains or retains the existing repair; it
never leaves a waiter in a removed manager entry.

Permission and direct ledgers share one per-connection tombstone budget but
retain disjoint live maps and response methods. `expires_at` is exactly
`retired_at + JSONL_REQUEST_TOMBSTONE_TTL_MS` on the same injected monotonic
clock. At the start of every response lookup and every tombstone insertion, all
tombstones with `expires_at <= now` are removed in the deterministic order
`(expires_at.tick, retirement_sequence, owner_rank, opaque_request_id UTF-8
bytes)`. Lookup occurs only after that cleanup, so an expired id returns
`UnknownRequest` even when no later insertion occurred.

Every tombstone consumes the `retirement_sequence` reserved by its live
admission; insertion never allocates and therefore cannot overflow a sequence
during close. The shared allocator is connection-scoped across both ledgers and
checked-increments once per successful registration. `owner_rank` is closed as
ThreadPermission=0, CommandExecPermission=1, DirectUserInput=2, and
DirectMcpElicitation=3. After expiry cleanup, if insertion would exceed
`JSONL_REQUEST_TOMBSTONE_LIMIT`, the oldest remaining tombstones are evicted by
that same tuple until the limit holds. `Routed` and `CommittedPending` entries
count toward the separate live limit and are never evicted. Opaque ids are never
reused within the connection even after tombstone expiry or eviction; a later
response returns `UnknownRequest` and can never route to a new owner. Expiry or
eviction does not revoke, alter, or compensate a committed runtime receipt.
Tests drive lookup and insertion with the same injected clock and exact ordering
tuple without wall-clock sleeps.

### JSONL legacy turn control

`ControlJsonlTurn` resolves the legacy turn id in the host registry and performs
lookup plus actor control as one runtime-owned command. An unknown id returns
the read-only `JsonlTurnControlResult::Idle` branch with no mutation or
acknowledgement. Resolved uncommitted codes have exact released projections:

```text
OperationAlreadyTerminal | StaleFence -> error "turn is not active: {turn_id}"
OperationNotInterrupted -> error "turn is not interrupted or no longer accepts resume: {turn_id}"
OperationNotSteerable -> error "turn no longer accepts steer input: {turn_id}"
WrongThread -> error "turn {turn_id} does not belong to thread {expected_thread_id}"
```

The bound JSONL host handle proves attachment and `LegacyJsonlControl`
capability before constructing this command, and malformed steer input fails in
the compatibility decoder. A close race is normalized inside the registry:
known terminal/closing ownership returns `OperationAlreadyTerminal`, while an
unknown retired id returns `Idle`. Therefore `WrongAttachment`, `InvalidInput`,
`CapabilityDenied`, and `ThreadClosed` are structurally unreachable command
results and are intentionally absent from both the command row and this released
projection table.

A resolved committed output emits `turn_controlled` from its cursor and status;
Idle emits the same released event directly from its typed read result. For
Steer, the typed user Item is emitted immediately after that control frame.
The adapter keeps only RPC correlation; it owns no active/completed turn map.
`Resolved { mutation: Deferred(ProjectionDegraded) }` follows the same exact
correlated gap branch and emits no `turn_controlled`; runtime retains the
committed control fact and repair token. It is not an uncommitted control error.

### JSONL event ordering

The `v0.2.50` ordering corpus remains exact. In particular:

- first assistant delta: item_started, item channel delta, legacy channel delta;
- tool request: tool item, optional file-change item, tool_requested;
- tool completion: tool_completed, item_completed(tool),
  item_completed(file-change when present);
- workflow start: item before legacy event;
- workflow result/failure: legacy event before completed item;
- turn_controlled precedes any steer-created user item;
- turn_completed is final for its RPC id.

Surface facts with no released event use `NoLegacyProjection`; adapters may not
invent names for cursor, recovery, Goal, settings, session health, or capability
state.
