# Cache-Aware Compaction

## Production Path

Remote summarization already existed, with real-provider evidence in
[the August evaluation](2026-08-10-compaction-remote-evaluation.md). This change
extends `RuntimeCompactionStep -> context::compact_with_summary* -> DeepSeek
/chat/completions`. There is no provider `/compact` endpoint or detached
compaction loop.

## Behavior

- Below the soft limit, valid messages are unchanged. Tool-boundary repair
  remains deterministic and is included in wire estimates.
- Under pressure, micro-compaction examines complete historical turns from
  the recent end, shortening large tool results only until the prompt reaches
  90% of the soft limit. It does not clear small results solely because they
  are old. Existing compaction markers are idempotent.
- Deep compaction reserves the tool schema, internal context, bounded summary,
  and complete current turn before retaining recent complete turns. Every
  pinned turn remains atomic, including its tool calls and results.
- Every leading system message stays byte-identical. Wire order is leading
  instructions, summary baseline/deltas, dynamic internal context, then
  retained conversation. A changed overlay therefore does not invalidate the
  unchanged summary prefix.
- Summary input uses original evidence, not a trial micro-compacted view.
  Natural-language messages remain intact across bounded chunks, including
  facts in the middle of a huge message. Tool output/arguments use the existing
  extractive renderer and omission markers.
- A summary request carries at most 4,096 estimated evidence tokens, plus
  fixed instruction/framing overhead. Each retained chunk summary is capped
  at 512 tokens; the API generation ceiling is four times its retained budget,
  clamped to 512..2,048 tokens, so a complete response can finish despite
  tokenizer differences. Retained state is bounded independently after a
  successful complete response. Thinking is disabled. All summary HTTP calls
  have one attempt: no transport, empty-response, strict-schema, or
  incomplete-stream retries.
- Chunk summaries are recursively merged. At most 32 summary operations are
  allowed per compaction, including cache lookups and baseline rebuilds.
  Failure, cancellation, or exhausted work budget installs no partial remote
  state and falls back to bounded local extractive context.
- Persistent summary text is capped at one quarter of the retention target,
  clamped to 32..2,048 estimated tokens. Baseline rebuilds also bound the number
  of deltas to five. Local fallback accounts for summary-message framing and
  merges deltas under hard pressure.
- Image admission budgets are estimates: low 384, high/auto 2,048, original
  4,096 tokens. No URL is fetched and no base64 is tokenized to make this
  estimate. Historical images participate in capacity decisions and have a
  retention allowance of at most 8,192 tokens, further capped at one quarter
  of the retention target. Recent historical images take priority over older
  images. Current-request and pinned images are retained.
- Automatic compaction writes the existing atomic context snapshot before
  replacing live state or emitting completion, even with event output
  disabled. Count-only replay cannot reproduce rewritten tool outputs,
  images, or pinned turns. The existing snapshot reader provides recovery.

## Limits

Semantic safety takes precedence over capacity and cache reuse. If immutable
instructions, pinned turns, tool schemas, or the current request alone exceed
the window, compaction does not silently discard them or claim they fit.
Local extraction and remote summaries are lossy; original history remains in
the session log. The image estimates are admission allowances, not verified
DeepSeek image billing formulas.

`measure_deepseek_prefix_reuse` compares exact lowered messages, provider scope,
and tools. It stops at the first changed message; identical later messages
cannot reuse their previous prefix cache. Its token/byte metrics are local
estimates and must not be presented as provider-reported KV cache hits.

## Verification

Focused tests cover soft/hard pressure, A/B eager versus cache-aware prefix
retention, repeated compaction, full current/pinned tool turns, pending-tool
repair, no-system inputs, huge messages, image detail/retention, bounded
hierarchy and work exhaustion, one-attempt failure, cancellation, cache reuse,
durable snapshot recovery, and persistence failure.

The parent serializes Cargo runs with `CARGO_INCREMENTAL=0` and the existing
target directory. No workspace-wide build or formatting is needed:

```sh
CARGO_INCREMENTAL=0 cargo test -p orca-provider --lib --locked -- --test-threads=1 --nocapture
CARGO_INCREMENTAL=0 cargo test -p orca-runtime compaction --lib --locked -- --test-threads=1
CARGO_INCREMENTAL=0 COMPACTION_REALAPI_RUNS=3 ORCA_SUMMARY_DEBUG=1 cargo run -p orca-provider --example compaction_realapi --locked
```

The real-API example uses `ORCA_API_KEY`, then `DEEPSEEK_API_KEY`, then the
`DEEPSEEK_API_KEY` entry in `ORCA_AUTH_FILE`, `$ORCA_HOME/auth.json`, or
`~/.orca/auth.json`. `ORCA_BASE_URL` overrides `DEEPSEEK_BASE_URL`. Set
`COMPACTION_REALAPI_RUNS=3` for three uninterrupted fail-fast runs (default
one, maximum ten). Each run uses a fresh RAII temporary cache,
prints no credentials or response bodies, and returns failure for unavailable
credentials or fallback rather than counting a skip as a pass. It exercises
remote hierarchy/cache replay plus an eager/cache-aware A/B comparison, with
estimated prefix metrics and actual API input/output/cache usage.

The initial parent provider run passed 177 tests and failed the existing
tiny-window summary-budget assertion. The summary-frame accounting fix keeps
that assertion intact; the next run passed 188/188. The subsequent provider
test for recent-image preference and replayed-reasoning accounting requires a
final 189-test rerun. The runtime test target compiled successfully after the
recovery test was changed to use the public store reader. Both new runtime
tests passed in the parent's subsequent runtime run. The parent-built runtime
binary was also executed directly with `compaction --test-threads=1`: 30 passed,
zero failed. Final provider-package verification after the generation-headroom
fix remains with the parent. Direct rustfmt checks and `git diff --check`
passed for the owned files. Coverage instrumentation was not requested and was
not run under the shared disk/build constraint.

## Real-API Evidence

The first run failed rather than reporting a credentials skip. A single
debug-enabled rerun identified `finish_reason=length`: the API returned 4,098
input tokens and exactly 375 output tokens, matching the small-window retained
summary budget. The existing parser rejected the incomplete response. Raising
the bounded generation ceiling independently from retained capacity fixed this
without accepting partial completions or introducing retries.

The first post-fix single run passed: summary requests reported
input/output tokens 4,098/435, 2,548/569, and 640/472; estimated context shrank
7,472 -> 1,248. The first A/B probe reported input/cache tokens 998/0 (eager)
and 7,508/1,536 (cache-aware).

Then the already-built example ran three uninterrupted times:

```sh
ORCA_SUMMARY_DEBUG=1 COMPACTION_REALAPI_RUNS=3 target/debug/examples/compaction_realapi
```

Exit status: 0. Model: `deepseek-v4-flash`. Framework retries: zero.
Each run used a fresh local summary-cache directory and completed two chunk
requests plus one merge. Repeating compaction within each run produced three
local cache hits with no summary HTTP request.

| Run | Messages | Estimated Wire Tokens | Chunk Input/Output | Merge Input/Output |
| --- | --- | --- | --- | --- |
| 1 | 50 -> 8 | 7,472 -> 1,098 | 4,098/142; 2,548/113 | 323/133 |
| 2 | 50 -> 8 | 7,472 -> 1,309 | 4,098/133; 2,548/362 | 563/358 |
| 3 | 50 -> 8 | 7,472 -> 1,254 | 4,098/200; 2,548/332 | 600/404 |

| A/B Metric | A: Eager Micro | B: Cache-Aware Micro |
| --- | --- | --- |
| Estimated prompt before | 8,646 | 8,646 |
| Estimated prompt after | 840 | 7,345 |
| Unchanged leading messages | 3 | 18 |
| Unchanged leading bytes | 307 | 57,497 |
| Unchanged leading tokens (estimate) | 53 | 7,233 |
| Actual API prompt tokens, all three runs | 998 | 7,508 |
| Actual API cache tokens, all three runs | 896 | 7,424 |

The server cache was warm from previous probes; these absolute hits are not an
isolated causal cache benchmark. The deterministic prefix comparison is the
reproducible reuse bound. B retains substantially more evidence and meets the
capacity target but sends more total input than A; cache reuse is not a claim
of universally lower cost. Rewriting the suffix invalidates its previous
prefix cache, even where later messages are byte-identical.
