# ThreadActor State Extraction

## Scope

Extract construction and ownership of the live `RuntimeThread`, its
`EventFactory`, aggregate usage ledger, and recovered background approval
registrations from `ThreadActor`. The actor keeps command dispatch, surface
commit sequencing, and generation ownership. The extracted module is private
to `runtime_host`; its fields are visible only to the parent actor module.

## Contract

- construction must preserve the thread's event-session identity and aggregate
  usage total;
- recovered background approval resolutions must be retained before the first
  command is processed;
- the actor-facing state API must not expose a second mutable thread owner;
- existing runtime-host lifecycle tests remain the behavioral acceptance oracle.

## Non-Goals

- No wire protocol, surface snapshot, or command behavior change.
- No source-line-count assertion or broad mechanical split of unrelated actor
  methods.
