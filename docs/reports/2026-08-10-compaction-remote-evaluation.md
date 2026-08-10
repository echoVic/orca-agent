# Compaction Completion and Remote Evaluation

## Gate

The runtime policy/step boundary is complete on `ff2edc0b3`: soft and hard
pressure, prompt-too-long retry, cancellation, remote-summary fallback,
summary-state persistence, and started/completed event ordering are covered by
focused behavior tests. This slice adds only real-provider evidence.

## Verification

```text
cargo test -p orca-runtime compaction --lib --locked -- --test-threads=1
27 passed; 0 failed

cargo test -p orca-provider context --lib --locked -- --test-threads=1
68 passed; 0 failed

cargo run -p orca-provider --example summary_render_realapi --locked
ALL TARGETS MET

cargo run -p orca-provider --example compaction_realapi --locked
remote compaction verified: messages 34->9; wire_tokens 4992->1216;
summary_chars=477
```

The summary renderer real-API evaluation measured a 96.1% second-turn cache
hit, reduced the medium summary prompt from 2,192 to 399 tokens, and confirmed
the local summary cache avoids the second lookup request. The compaction smoke
crossed the 2,500-token soft line, returned `RemoteSummary`, retained the
current request, installed a summary baseline, and reduced wire pressure by
3,776 tokens. The harness exits successfully with a skip when credentials are
absent.

## Waiting and ownership boundary

Remote summary work is still synchronously awaited by the runtime compaction
step. `CancelToken` owns interruption of the provider wait, cancellation falls
back to local truncation, and history is appended before `context.compacted` is
emitted. No detached compaction worker or second agent loop is introduced.
