# Plan: Compaction Completion and Remote Evaluation

1. Run existing runtime and provider compaction focused gates.
2. Add a credential-gated harness that calls the production remote-summary
   compactor and checks retained context plus reduced wire pressure.
3. Run the harness with the configured DeepSeek credential, record output and
   any credential-gated skip in an evidence report, and update the roadmap.
4. Run formatting and diff checks, then create one semantic documentation and
   verification commit.
