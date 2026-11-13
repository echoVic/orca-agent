# DeepSeek Web Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `web_search` and `web_fetch` use DeepSeek's Search and Fetch APIs when Orca runs against the official DeepSeek endpoint, and `web_search` honours the `fresh_days` recency the system prompt already asks for.

**Architecture:** A new `orca_tools::deepseek_web` module owns the DeepSeek HTTP calls and an access value (`DeepSeekWebAccess`) that the runtime builds from `RunConfig` and hands to the tools through `ToolContext`. `web_search` picks Brave, DeepSeek or Exa and falls back from DeepSeek to Exa. `web_fetch` keeps fetching locally and retries through DeepSeek only for failures that are not Orca's own safety refusals.

**Tech Stack:** Rust (reqwest via `orca_mcp::http`, tokio current-thread runtimes, serde, chrono), MDX site docs.

**Spec:** None. On 2026-11-14 the user skipped the design questions ("直接开始吧"). The Decisions section records Claude's calls and the live API evidence they rest on.

## Decisions

- **No server tools in the main loop.** Live probes on 2026-11-14 showed `deepseek_web_search` next to function tools is accepted only in thinking mode, and inside one request the model sees either the client tools or the server search toolset. One search ended the round with `stop` and the function never ran. The standalone APIs work: `POST https://api.deepseek.com/v1/tools/search` answered in 0.4 s.
- **Search order:** `BRAVE_SEARCH_API_KEY` set → Brave, unchanged. Otherwise DeepSeek when available, otherwise Exa. When a DeepSeek search fails, Orca falls back to Exa and says so in the output. An empty DeepSeek result is not a failure.
- **Fetch order:** local fetch first, since it is free and reaches whatever the user's machine reaches. DeepSeek Fetch is only a retry.
- **Key safety:** the key goes only to the fixed origin `https://api.deepseek.com`, and only when the provider is DeepSeek and the configured base URL is that host over https on the default port. A custom or third-party `base_url` disables the DeepSeek web APIs.
- **Switch:** `[tools] deepseek_web = true` by default. `false` restores today's behaviour. `[tools]` is already user-config-only.
- **Recency contract:** `fresh_days` (integer days) is the schema parameter, matching `crates/orca-runtime/src/system_prompt.rs:137`. The old `freshness` codes are still accepted but no longer advertised.
- **Not in scope:** adding tool fees to Orca's cost estimate. DeepSeek bills $0.75 per 1,000 successful searches or fetches; the docs say this isn't counted.

## Global Constraints

- Build every HTTP client with `orca_mcp::http::client_builder()`; `scripts/validate-http-client-boundary.mjs` enforces it.
- Send the API key only to `https://api.deepseek.com`. Never put it in output, errors or logs; `Debug` for `DeepSeekWebAccess` prints `api_key: "<redacted>"`.
- No new crate dependencies.
- Tests make no live network calls. Local fixtures bind `127.0.0.1:0` and call `set_nonblocking(false)` on every accepted stream (on macOS the accepted stream inherits O_NONBLOCK).
- Without DeepSeek access, or with `deepseek_web = false`, `web_search` and `web_fetch` behave exactly as before.
- DeepSeek search query: at most 128 Unicode chars, operators included. Count clamped to 1–10. Summary capped at 800 chars, then `…`. Search timeout 25 s, fetch timeout 60 s.
- Exact strings:
  - Search fallback note: `[DeepSeek search failed: {reason}. These results come from Exa.]`
  - Fetch fallback note: `[The direct request failed ({reason}), so this page was fetched through the DeepSeek Fetch API.]`
  - DeepSeek HTTP errors: 401 → `DeepSeek rejected the API key (HTTP 401)`, 402 → `the DeepSeek account balance is too low (HTTP 402)`, 429 → `DeepSeek rate limit reached (HTTP 429)`, any other status → `DeepSeek returned HTTP {status}: {error.message}`, with the message cut to 200 chars.

## Review Focus

1. A third-party `base_url` (e.g. `https://api.siliconflow.cn/v1`) with its own key must never reach DeepSeek. Task 2 pins it: `for_provider` returns `None`.
2. A long Chinese query plus `after:` must stay within 128 chars, measured in chars, and never split a char. Task 3 pins it with `deepseek_query_with_operators_stays_within_128_chars`.
3. A model that sends `fresh_days: "7"`, `0` or `9999` must not fail the tool call. Task 1 pins it with `fresh_days_accepts_numeric_strings_and_clamps`.
4. A local fetch refused for a cross-origin redirect must not be retried through DeepSeek. Task 4 pins it with `retry_decision_skips_rejections_missing_pages_and_private_hosts`.
5. A LAN or intranet URL (IP literal in a private range, `.local`, `.internal`, …) that fails locally must not be sent to DeepSeek. Same Task 4 test.

---

### Task 1: `fresh_days` recency contract for `web_search`

**Files:**
- Modify: `crates/orca-tools/src/web_search.rs` (`SearchArgs`, `parse_args`, `infer_freshness`, `brave_query_params`, `exa_query`, tests)
- Modify: `crates/orca-tools/src/registry.rs` (web_search schema, ~line 742)

**Interfaces:**
- Produces: `pub(crate) enum Recency { LastDays(u32), Range { start: String, end: String } }` in `web_search.rs`. `SearchArgs.recency: Option<Recency>` replaces `freshness`. `fn infer_recency(query: &str) -> Option<Recency>`. `fn brave_freshness(recency: &Recency, today: chrono::NaiveDate) -> String`. `fn exa_query(args: &SearchArgs) -> String` keeps its name.

- [ ] **Step 1: Write the failing tests** in `web_search.rs` tests (today = 2026-11-14):
  - `fresh_days_maps_to_brave_codes_and_custom_ranges`: LastDays 1→`pd`, 7→`pw`, 30→`pm`, 31→`pm`, 365→`py`, 3→`2026-11-11to2026-11-14`. `Range{2026-01-01,2026-02-01}`→`2026-01-01to2026-02-01`.
  - `fresh_days_accepts_numeric_strings_and_clamps`: `{"query":"x","fresh_days":"7"}`→`LastDays(7)`. `{"query":"x","fresh_days":0}`→`None`. `9999`→`LastDays(365)`. `"abc"` is ignored rather than failing.
  - `legacy_freshness_codes_still_parse`: `pw`→`LastDays(7)`, `pm`→`LastDays(31)`, `2026-01-01to2026-02-01`→`Range`.
  - `fresh_days_wins_over_legacy_freshness`: `{"fresh_days":1,"freshness":"py"}`→`LastDays(1)`.
  - `inferred_recency_understands_today_and_chinese_keywords`:
    - `rust news today` → 1
    - `今天 天气` → 1
    - `本周 rust 发布` → 7
    - `deepseek 最新 模型` → 31
    - `latest rust release` → 31
    - `rust ownership` → `None`
  - `exa_query_describes_day_windows_and_ranges`:
    - `LastDays(1)` → `q from the last 24 hours`
    - `LastDays(3)` → `q from the last 3 days`
    - `Range` → `q published between 2026-01-01 and 2026-02-01`
  - In `registry.rs` tests, `web_search_schema_exposes_fresh_days`: the schema properties hold `fresh_days` of type `integer`, and no `freshness`.
  - Update the existing tests that read `args.freshness` to the new representation.

- [ ] **Step 2: Run them and watch them fail**
  Run: `env -u NODE_OPTIONS cargo test -p orca-tools fresh_days` (and once more with each other new test name)
  Expected: compile errors or FAIL on the missing `Recency`/`recency`.

- [ ] **Step 3: Implement.** `fresh_days` accepts an integer or a digit string. Values ≤0 or unparsable are dropped, and values above 365 are clamped to 365. Legacy `freshness` maps `pd/pw/pm/py` to 1/7/31/365 and the custom range to `Range`. Precedence: `fresh_days`, then `freshness`, then `infer_recency(query)`.
  - Inference uses English tokens as today: `today` → 1, the rest of the current list → 31. Chinese substrings: `今天`/`今日` → 1, `本周`/`这周` → 7, `最新`/`最近`/`近期`/`新闻` → 31.
  - Brave uses the code when days ∈ {1, 7, 30, 31, 365}, otherwise `{today-N}to{today}`. Take today from `chrono::Utc::now().date_naive()` at the call site and pass it in.
  - Schema: replace the `freshness` property with `fresh_days`: `{"type":"integer","description":"Optional recency window in days (1-365): only return pages published in the last N days. Use 1 for today, 7 for this week, 30 for recent news or releases."}`. No `minimum`/`maximum` keywords, because strict tool schemas may reject them.

- [ ] **Step 4: Run the tests and see them pass**
  Run: `env -u NODE_OPTIONS cargo test -p orca-tools` and `env -u NODE_OPTIONS cargo test -p orca-runtime --lib system_prompt`
  Expected: all pass. `prompt_requires_fresh_days_for_recent_web_searches` is unchanged and green.

- [ ] **Step 5: Commit** — `fix(tools): honour fresh_days in web_search`

### Task 2: `deepseek_web` switch and access plumbing

**Files:**
- Create: `crates/orca-tools/src/deepseek_web.rs` (access type only in this task; `pub mod deepseek_web;` in `crates/orca-tools/src/lib.rs`)
- Modify: `crates/orca-core/src/config/mod.rs` (`ToolConfig` ~286, `[tools]` block of the config-show text ~880)
- Modify: `crates/orca-core/src/config/file.rs` (tests near the `[tools]` sample ~1343 and its assertions ~1509)
- Modify: `crates/orca-tools/src/registry.rs` (`ToolContext` 67–145, dispatch ~2024–2031)
- Modify: `crates/orca-tools/src/lib.rs` (`execute_with_mcp_external_roots_policy_or_cancel_and_elicitation_with_profile`, 149–209)
- Modify: `crates/orca-runtime/src/runtime_normal_tool.rs:98`, `crates/orca-runtime/src/runtime_tool_call.rs:329`

**Interfaces:**
- Produces:
  - `ToolConfig.deepseek_web: bool` (serde default `true`).
  - `#[derive(Clone)] pub struct DeepSeekWebAccess` with `pub fn for_provider(provider: orca_core::config::ProviderKind, base_url: Option<&str>, api_key: Option<&str>, enabled: bool) -> Option<DeepSeekWebAccess>`, and `pub(crate) fn origin(&self) -> &str` / `pub(crate) fn api_key(&self) -> &str`. Plus `#[cfg(test)] pub(crate) fn for_test(origin: &str, api_key: &str) -> Self` for local fixtures. The production origin is the constant `https://api.deepseek.com`.
  - `ToolContext.deepseek_web: Option<DeepSeekWebAccess>` and `ToolContext::with_deepseek_web(self, Option<DeepSeekWebAccess>) -> Self`.
  - New last-but-one parameter `deepseek_web: Option<DeepSeekWebAccess>` on the `_with_profile` entry point. The older entry points pass `None`.
  - `web_search::execute_or_cancel(request, max_bytes, deepseek: Option<&DeepSeekWebAccess>, should_cancel)` and the same shape for `web_fetch::execute_or_cancel`. In this task they accept the value and ignore it. `execute(request, max_bytes)` keeps its signature.
  - Runtime helper `fn deepseek_web_access(config: &RunConfig) -> Option<DeepSeekWebAccess>` in `runtime_normal_tool.rs`.

- [ ] **Step 1: Write the failing tests**
  - `deepseek_web.rs` `access_requires_official_https_endpoint_and_a_key` (`sk-x` key):
    - `(DeepSeek, None, Some("sk-x"), true)` → `Some`
    - `https://api.deepseek.com/beta` → `Some`
    - `https://API.DeepSeek.com` → `Some`
    - `http://api.deepseek.com` → `None`
    - `https://api.deepseek.com.evil.example` → `None`
    - `https://api.deepseek.com:8443` → `None`
    - `https://api.siliconflow.cn/v1` → `None`
    - key `None`, or `"  "` → `None`
    - `enabled=false` → `None`
    - `ProviderKind::Mock` → `None`
  - `access_debug_redacts_the_key`: `format!("{:?}", access)` doesn't contain `sk-x` and contains `<redacted>`.
  - orca-core: `tools_deepseek_web_defaults_on_and_parses_off`. Default config → `true`; `[tools]\ndeepseek_web = false` → `false`. The config-show text contains `deepseek_web = true`. A project config's `[tools] deepseek_web` is ignored, which the existing deny list already does; assert it.
  - orca-runtime `deepseek_web_access_follows_provider_and_switch`: a `RunConfig` with `provider: DeepSeek`, `api_key: Some(..)` → `Some`. The same with `tools.deepseek_web = false` → `None`.

- [ ] **Step 2: Run and watch them fail**
  Run: `env -u NODE_OPTIONS cargo test -p orca-tools access_` · `env -u NODE_OPTIONS cargo test -p orca-core deepseek_web` · `env -u NODE_OPTIONS cargo test -p orca-runtime --lib deepseek_web_access`
  Expected: compile errors or FAIL.

- [ ] **Step 3: Implement the switch, the access type and the plumbing.**
  - Host check: parse with `url::Url`, require scheme `https`, ASCII-case-insensitive host equal to `api.deepseek.com`, and `port()` equal to `None`.
  - The read-only executor in `runtime_tool_call.rs` passes `None`. Comment it: web tools are not concurrent-safe, so they never reach this path.

- [ ] **Step 4: Run the tests and see them pass**
  Run: `env -u NODE_OPTIONS cargo test -p orca-tools` · `env -u NODE_OPTIONS cargo test -p orca-core` · `env -u NODE_OPTIONS cargo test -p orca-runtime --lib runtime_tool_call` · `env -u NODE_OPTIONS cargo build`
  Expected: all pass.

- [ ] **Step 5: Commit** — `feat(tools): pass DeepSeek web access to the web tools`

### Task 3: DeepSeek search backend

**Files:**
- Modify: `crates/orca-tools/src/deepseek_web.rs` (search call)
- Modify: `crates/orca-tools/src/web_search.rs` (backend choice, fallback, output with date)
- Modify: `crates/orca-tools/src/registry.rs` (web_search description)

**Interfaces:**
- Consumes: `DeepSeekWebAccess` (Task 2), `Recency` (Task 1).
- Produces:
  - `pub(crate) struct DeepSeekSearchHit { title: String, url: String, summary: String, date: Option<String> }`
  - `pub(crate) async fn search(access: &DeepSeekWebAccess, query: &str, count: usize, should_cancel: &dyn Fn() -> bool) -> Result<Vec<DeepSeekSearchHit>, DeepSeekWebError>`
  - `pub(crate) enum DeepSeekWebError { Cancelled, Failed(String) }`, where `Failed` holds the exact strings from Global Constraints
  - `fn deepseek_query(query: &str, recency: Option<&Recency>, today: NaiveDate) -> String`
  - `pub(crate) enum SearchBackend { Brave, DeepSeek, Exa }`
  - `fn choose_search_backend(brave_key: Option<&str>, deepseek: Option<&DeepSeekWebAccess>) -> SearchBackend`

- [ ] **Step 1: Write the failing tests** (local fixtures; route the Exa endpoint through a parameter the way `search_exa_at_or_cancel_async` already does):
  - `deepseek_search_posts_query_with_recency_operator_and_bearer_key`: the fixture records `POST /v1/tools/search`, `authorization: Bearer sk-test`, and the JSON body `{"query":"rust release after:2026-11-07","count":3}` for `LastDays(7)` with today = 2026-11-14.
  - `deepseek_search_formats_date_and_caps_summary_at_800_chars`: a 1,000-char Chinese summary yields exactly 800 chars plus `…`. `date` `2026-11-12T16:00:00+00:00` renders as `1. Title (2026-11-12)`.
  - `deepseek_query_with_operators_stays_within_128_chars`: a 150-char Chinese query plus `LastDays(7)` gives ≤128 chars that end with ` after:2026-11-07`.
  - `deepseek_http_errors_map_to_readable_messages`: fixture statuses 401/402/429/500 (with body `{"error":{"message":"boom"}}`) give the four exact strings.
  - `auto_backend_prefers_brave_then_deepseek_then_exa`: (`Some("k")`, any) → Brave. (`None`, `Some`) → DeepSeek. (`None`, `None`) → Exa. (`Some("  ")`, `None`) → Exa.
  - `deepseek_failure_falls_back_to_exa_with_note`: DeepSeek fixture 500 and Exa fixture with one result. The output starts with `[DeepSeek search failed: DeepSeek returned HTTP 500: boom. These results come from Exa.]` and contains the Exa result.
  - `empty_deepseek_results_do_not_fall_back`: `{"results":[]}` → `(no web search results)`, and the Exa fixture is never hit.

- [ ] **Step 2: Run and watch them fail**
  Run: `env -u NODE_OPTIONS cargo test -p orca-tools deepseek_` · `env -u NODE_OPTIONS cargo test -p orca-tools auto_backend`
  Expected: compile errors or FAIL.

- [ ] **Step 3: Implement.** Request body `{"query", "count"}`; `need_content` is left out.
  - The summary falls back to `snippet` when empty.
  - `date` keeps its first 10 chars only when they parse as `%Y-%m-%d`.
  - Truncate the query part by chars to fit `query + " " + operators` within 128.
  - `SearchResult` gains `published: Option<String>`, which only DeepSeek fills.
  - The output format stays `{n}. {title}[ ({date})]\n{description}\n{url}`.
  - Description: `Search the web for current information. Returns the top results with title, URL, publication date when known, and a summary.`

- [ ] **Step 4: Run the tests and see them pass**
  Run: `env -u NODE_OPTIONS cargo test -p orca-tools`
  Expected: all pass, including `tests/web_search_no_system_certs.rs`.

- [ ] **Step 5: Commit** — `feat(tools): search through DeepSeek when it is the provider`

### Task 4: DeepSeek fetch retry and docs

**Files:**
- Modify: `crates/orca-tools/src/deepseek_web.rs` (fetch call)
- Modify: `crates/orca-tools/src/web_fetch.rs` (error kinds, retry decision, composition)
- Modify: `site/src/docs/md/en/configuration.mdx`, `site/src/docs/md/zh/configuration.mdx` (new section before "Network and certificates" / "网络与证书")

**Interfaces:**
- Consumes: `DeepSeekWebAccess`, `DeepSeekWebError` (Tasks 2–3).
- Produces:
  - `pub(crate) struct DeepSeekPage { title: String, content: String }`
  - `pub(crate) async fn fetch(access: &DeepSeekWebAccess, url: &str, should_cancel: &dyn Fn() -> bool) -> Result<DeepSeekPage, DeepSeekWebError>`
  - In `web_fetch.rs`: `enum FetchError { Cancelled, Rejected(String), Failed { message: String, status: Option<u16> } }` and `fn should_retry_with_deepseek(error: &FetchError, url: &str) -> bool`

- [ ] **Step 1: Write the failing tests**
  - `deepseek_fetch_posts_url_and_returns_markdown`: the fixture records `POST /v1/tools/fetch`, the bearer key and the body `{"url":"https://example.com/a"}`, and returns `{"title":"A","content":"# A\n\nbody"}`.
  - `retry_decision_skips_rejections_missing_pages_and_private_hosts`. `false` for:
    - `Rejected`, e.g. a cross-origin redirect or too many redirects
    - `Failed` with status 404 or 410
    - hosts `10.0.0.5`, `192.168.1.10`, `172.16.0.1`, `127.0.0.1`, `169.254.1.1`, `[fd00::1]`, `[::1]`, `printer.local`, `svc.internal`, `nas.lan`, `router.home.arpa`

    `true` for `Failed` with status 403, 429 or 503, or with no status (timeout), on `https://example.com/a`.
  - `empty_extraction_is_retried`: a local body whose extracted text is blank counts as `Failed { status: None }`.
  - `fallback_output_starts_with_note_and_applies_prompt_filter`: local `Failed` 403 plus DeepSeek page content. The output starts with `[The direct request failed (HTTP 403), so this page was fetched through the DeepSeek Fetch API.]`, and the `prompt` filter applies to the DeepSeek content.
  - `both_failures_are_reported_together`: local 403 and DeepSeek 500. The error has the local reason, then `; DeepSeek Fetch also failed: ` and the DeepSeek reason.
  - `without_deepseek_access_errors_are_unchanged`: access `None` returns the local error text unchanged.

- [ ] **Step 2: Run and watch them fail**
  Run: `env -u NODE_OPTIONS cargo test -p orca-tools retry_decision` (and the other new names)
  Expected: compile errors or FAIL.

- [ ] **Step 3: Implement the retry and write the docs.**
  - `Rejected` covers `normalise_url` refusals, cross-origin redirects and too many redirects.
  - Keep `{reason}` short: `HTTP 403`, `timed out`, `no readable text`, …
  - An empty DeepSeek `content` is a failure: `DeepSeek Fetch returned no content`.
  - Docs, one section per language, same facts:
    - the search order
    - the fetch retry rules
    - only the official endpoint, with the key sent nowhere else
    - `[tools] deepseek_web = false` to turn it off (user config only)
    - billing: DeepSeek charges $0.75 per 1,000 successful searches or fetches (as of 2026-11), apart from token charges, and Orca's cost estimate doesn't include it
    - the `BRAVE_SEARCH_API_KEY` variable

    The MDX must have no bare `{`, `}` or `<` outside code spans or blocks.

- [ ] **Step 4: Run the whole verification**
  Run each command and check its exit code:
  - `env -u NODE_OPTIONS cargo test -p orca-tools`
  - `env -u NODE_OPTIONS cargo test -p orca-core`
  - `env -u NODE_OPTIONS cargo test -p orca-runtime --lib system_prompt`
  - `env -u NODE_OPTIONS cargo test -p orca-runtime --lib runtime_tool_call`
  - `env -u NODE_OPTIONS cargo clippy -p orca-tools -p orca-core -p orca-runtime --all-targets --locked`, as CI runs it: no errors, and no new warnings in touched files
  - `cargo fmt --all -- --check`
  - each of the four validator pairs `node scripts/test-validate-<name>.mjs && node scripts/validate-<name>.mjs` (runtime-surface-contract, windows-platform-boundaries, execution-broker-boundary, http-client-boundary), plus `node scripts/test-repository-hygiene.mjs`

  Expected: every exit code 0.

- [ ] **Step 5: Commit** — `feat(tools): retry failed fetches through DeepSeek and document the web backends`
