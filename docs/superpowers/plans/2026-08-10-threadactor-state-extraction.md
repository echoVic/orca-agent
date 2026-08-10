# ThreadActor State Extraction Plan

- [x] Add a focused constructor-equivalence test for the usage ledger and preserve event creation.
- [x] Move the thread/event/usage/recovered-approval construction boundary to the private `runtime_host::thread_state` module.
- [x] Replace the direct `ThreadActor::new` setup while preserving the existing actor fields.
- [x] Run focused runtime-host/controller tests, workspace check, formatting, and diff checks.
- [ ] Review, document the seam, and commit the independent slice.
