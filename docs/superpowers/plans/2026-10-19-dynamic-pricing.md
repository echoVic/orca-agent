# 动态价格（#136）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 成本估算改用 DeepSeek 当前官方价，分高峰和空闲两档计价；价格每天从 models.dev 后台刷新，刷新失败不影响使用；用户可以覆盖单个模型的价格，也可以关闭刷新。

**Architecture:**
- orca-runtime 新增 `pricing` 模块，分三个文件：
  - `rates`：内置价格表、北京时间的高峰规则、节假日表、单次计价，全部是纯函数；
  - `book`：进程级价格表、缓存文件的读写，以及缓存和内置表之间的新旧选择；
  - `refresh`：从 models.dev 拉取、解压、解析和校验，判断是否需要刷新，并起后台线程。
- `CostTracker` 不再持有固定价格。每次 `add_usage` 时按当前价格表、会话的价格覆盖和注入的时钟计价。
- 一个进程第一次创建 `InteractiveSession` 时检查是否需要刷新；会话的价格覆盖也在这里传给 `CostTracker`。

**Tech Stack:** Rust（orca-core、orca-runtime、根 crate 的集成测试）。用到 chrono、serde/serde_json、reqwest blocking（必须经 `orca_mcp::http::blocking_client()` 构建）、`flate2`（依赖树里已有，这次给 orca-runtime 加成直接依赖）、`orca_platform::fs::atomic_write`。

**Spec:** `docs/superpowers/specs/2026-10-19-dynamic-pricing-design.md`

**范围说明（写计划时对 spec 的补充）：**
1. 项目级 `.orca/config.toml` 不能设置 `[pricing]` 和 `[models.*.pricing]`。价格决定成本预算什么时候让运行停下，所以和 `budget` 一样只认用户配置。任务 2 把这条补进 spec 第 2.5 节。
2. "进程启动后后台刷新"这一条，没有用跑整个二进制的集成测试来验证：`orca exec` 可能在后台请求完成之前就退出了，这样的测试会时好时坏。改为两层验证：
   - 库测试同步调用 `refresh_and_store`，用本地 HTTP fixture 覆盖 200（gzip）、304、错误、超时和过大响应；
   - 二进制集成测试只验证"缓存在、就按缓存计价"，以及 doctor 的输出。

## Global Constraints

- 价格单位是美元 / 百万 tokens。内置表存的是空闲价：
  - flash：`input_cache_miss = 0.15`、`input_cache_hit = 0.003`、`output = 0.6`
  - pro：`0.66`、`0.022`、`1.98`
  - `builtin_as_of = 2026-10-19`
- 高峰时段：北京时间（UTC+8）周一至周五的 [09:00, 12:00) 和 [14:00, 18:00)，当天不在节假日表里。高峰价 = 空闲价 × `PEAK_MULTIPLIER = 2.0`。周末全天空闲，包括调休上班的周末。
- 节假日表只收落在周一至周五的放假日，2026 年共 19 天：
  - 1 月 1、2 日
  - 2 月 16–20、23 日
  - 4 月 6 日
  - 5 月 1、4、5 日
  - 6 月 19 日
  - 9 月 25 日
  - 10 月 1、2、5、6、7 日
- 刷新的各项参数：

  | 项目 | 值 |
  |---|---|
  | 地址 | `https://models.dev/api.json`，可用环境变量 `ORCA_PRICING_URL` 替换 |
  | 刷新间隔 | 24 小时 |
  | 请求超时 | 15 秒 |
  | 响应体上限 | 解压后最多 32 MiB |
  | 请求头 | `Accept-Encoding: gzip`、`User-Agent: orca-pricing`，有缓存时带 `If-None-Match` |
  | 字段映射 | `cost.input` → 未命中价，`cost.cache_read` → 命中价，`cost.output` → 输出价 |

- 价格校验：数值有限，且 0 ≤ 值 ≤ 100。内置表里有的模型，三个价格都必须落在 [内置值 / 10, 内置值 × 10] 之内，否则丢掉这个模型。
- 缓存：路径 `$ORCA_HOME/pricing-cache.json`，`version = 1`，用 `atomic_write(..., AtomicWritePolicy::NoFollow)` 写入。缓存 `fetched_at` 的日期 ≥ `builtin_as_of` 时，缓存里的价格优先。
- 模型匹配顺序：配置覆盖 → 价格表精确匹配（先用 `canonical_model_name` 归一）→ 名字含 `v4-pro` 的按 pro → 其余按 flash。配置覆盖是固定价，不乘高峰系数。
- 开发构建（`crate::update_check::is_dev_build_run()` 为真）不刷新，除非设了 `ORCA_PRICING_URL`。测试一律不访问外网，不写 `~/.orca`，结果不能因为运行的时刻（高峰或空闲）而变。
- 金额仍是美元：`estimated_cost_usd`、`max_cost_usd`、事件名和字段名都不改。
- HTTP 客户端只能用 `orca_mcp::http::blocking_client()`（CI 的 `validate-http-client-boundary.mjs` 会检查）。请求放在 `std::thread` 里发，不能在 tokio 上下文里发。
- 所有 crate 都有 `#![deny(deprecated)]`，而 CI 用最新稳定版 Rust，所以不能用已弃用的接口。例如要用 `FixedOffset::east_opt`、`NaiveDate::from_ymd_opt`，不能用不带 `_opt` 的旧版本。Windows 只能在 CI 上编译；插入新条目时，检查上下的 `#[cfg]` 有没有被抢走。
- 公开文档（`site/src/docs/md/**`）不能出现 `tests/public_docs_contract.rs` 禁止的字符串，例如 `orca config `、`--max-cost `（注意都带尾随空格）。
- 每个任务提交前都要通过：
  - `cargo fmt --all -- --check`
  - `env -u NODE_OPTIONS cargo clippy -p <改动的 crate> --all-targets --locked`，不能新增警告
  - 改动 crate 的测试
  
  所有 cargo test 和 node 命令都要带 `env -u NODE_OPTIONS`。
- 提交信息用 conventional 风格，与 `git log` 一致，正文带 `Refs #136`。不要 push。

## Review Focus

1. **缓存文件被另一个版本写过，或者写到一半崩溃留下截断的 JSON**：估算照常进行（退回内置表），doctor 照常输出。由任务 3 的 `read_cache_ignores_corrupt_foreign_and_implausible_files` 钉住。
2. **UTC 日期和北京日期不同时碰上节假日**：比如 UTC 10 月 1 日 01:30 = 北京 10 月 1 日 09:30，是节假日，应算空闲；UTC 周日 23:00 = 北京周一 07:00，也算空闲。由任务 1 的 `holidays_and_weekdays_use_the_beijing_calendar` 钉住。
3. **`$ORCA_HOME` 不可写**（只读 home、沙箱、CI）：刷新照样成功，结果只在本进程生效，不报错，也不崩。由任务 4 的 `refresh_and_store_succeeds_when_the_cache_cannot_be_written` 钉住。
4. **用户给本地或免费模型配了零价**：成本为 0，高峰时段也不翻倍。由任务 5 的 `a_zero_price_override_costs_nothing_at_peak` 钉住。
5. **项目级 `.orca/config.toml` 想把价格设成 0、或者关掉刷新**：两项都被忽略，`model`、`supports_images` 这些项目允许的设置照常生效。由任务 2 的 `project_layer_drops_pricing_settings` 钉住。

---

### Task 1: 内置价格和分时规则（`pricing::rates`）

**Files:**
- Create: `crates/orca-runtime/src/pricing/mod.rs`
- Create: `crates/orca-runtime/src/pricing/rates.rs`
- Modify: `crates/orca-runtime/src/lib.rs`（在 `pub mod cost;` 下一行加 `pub mod pricing;`；这一行和上下文都没有 `#[cfg]`）

**Interfaces:**
- Produces：
  - `pricing::rates::Rates { input_cache_miss: f64, input_cache_hit: f64, output: f64 }`，derive `Clone, Copy, Debug, Deserialize, PartialEq, Serialize`
  - `FLASH_RATES: Rates`、`PRO_RATES: Rates`、`PEAK_MULTIPLIER: f64`
  - `fn builtin_as_of() -> NaiveDate`
  - `fn builtin_rates() -> [(&'static str, Rates); 2]`
  - `fn is_peak(DateTime<Utc>) -> bool`
  - `fn time_of_use_multiplier(DateTime<Utc>) -> f64`
  - `fn cost_usd(Rates, f64, Usage) -> f64`
  - `#[doc(hidden)] fn off_peak_instant() -> DateTime<Utc>`、`#[doc(hidden)] fn peak_instant() -> DateTime<Utc>`（测试用的时钟）

- [ ] **Step 1: 写模块骨架和失败的测试**

`crates/orca-runtime/src/pricing/mod.rs`：

```rust
//! Cost estimates at DeepSeek's prices: the built-in table and its
//! time-of-use rule (`rates`).

pub mod rates;
```

`crates/orca-runtime/src/pricing/rates.rs` 先只写测试和最小签名（`todo!()` 让测试编译后失败）：

```rust
//! DeepSeek's published prices as of a fixed date, and its time-of-use rule:
//! peak hours on Beijing weekdays cost twice the off-peak price.

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, Timelike, Utc, Weekday};
use orca_core::model::{FLASH_MODEL, PRO_MODEL};
use orca_core::provider_types::Usage;
use serde::{Deserialize, Serialize};

/// Prices in US dollars per million tokens.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct Rates {
    pub input_cache_miss: f64,
    pub input_cache_hit: f64,
    pub output: f64,
}

pub fn is_peak(_at: DateTime<Utc>) -> bool {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(text: &str) -> DateTime<Utc> {
        text.parse().expect("valid UTC instant")
    }

    /// `text` is a Beijing wall-clock time such as `2026-10-19T09:00:00`.
    fn beijing(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("{text}+08:00"))
            .expect("valid Beijing time")
            .with_timezone(&Utc)
    }

    #[test]
    fn peak_windows_follow_beijing_hours_on_weekdays() {
        // Monday 2026-10-19 is an ordinary working day.
        for (time, peak) in [
            ("08:59:59", false),
            ("09:00:00", true),
            ("11:59:59", true),
            ("12:00:00", false),
            ("13:59:59", false),
            ("14:00:00", true),
            ("17:59:59", true),
            ("18:00:00", false),
        ] {
            assert_eq!(is_peak(beijing(&format!("2026-10-19T{time}"))), peak, "{time}");
        }
    }

    #[test]
    fn weekends_are_off_peak_all_day_even_as_make_up_workdays() {
        assert!(!is_peak(beijing("2026-10-17T10:00:00"))); // Saturday
        assert!(!is_peak(beijing("2026-10-18T15:00:00"))); // Sunday
        assert!(!is_peak(beijing("2026-10-10T10:00:00"))); // Saturday make-up workday
    }

    #[test]
    fn weekday_holidays_are_off_peak_all_day() {
        assert!(!is_peak(beijing("2026-10-05T10:00:00"))); // Monday, National Day holiday
        assert!(!is_peak(beijing("2026-02-16T15:00:00"))); // Monday, Spring Festival
        assert!(is_peak(beijing("2026-10-09T10:00:00"))); // Friday after the holiday
    }

    #[test]
    fn holidays_and_weekdays_use_the_beijing_calendar() {
        // 01:30 UTC on 1 October is 09:30 on the National Day holiday in Beijing.
        assert!(!is_peak(utc("2026-10-01T01:30:00Z")));
        // 01:30 UTC on 8 October is 09:30 on an ordinary Thursday in Beijing.
        assert!(is_peak(utc("2026-10-08T01:30:00Z")));
        // 23:00 UTC on Sunday is 07:00 on Monday in Beijing: before the window.
        assert!(!is_peak(utc("2026-10-18T23:00:00Z")));
        // 06:30 UTC on Monday is 14:30 in Beijing: the afternoon window.
        assert!(is_peak(utc("2026-10-19T06:30:00Z")));
    }

    #[test]
    fn the_peak_multiplier_doubles_the_cost() {
        assert_eq!(time_of_use_multiplier(peak_instant()), PEAK_MULTIPLIER);
        assert_eq!(time_of_use_multiplier(off_peak_instant()), 1.0);
        assert_eq!(PEAK_MULTIPLIER, 2.0);
    }

    #[test]
    fn cost_matches_the_published_prices_at_both_tiers() {
        // Issue #137's bug-fix run: 12,698 cache-miss and 60,672 cache-hit
        // input tokens, 2,052 output tokens.
        let usage = Usage {
            input_tokens: 12_698 + 60_672,
            output_tokens: 2_052,
            cache_tokens: 60_672,
        };
        let expected = (12_698.0 * 0.15 + 60_672.0 * 0.003 + 2_052.0 * 0.6) / 1_000_000.0;
        assert!((cost_usd(FLASH_RATES, 1.0, usage) - expected).abs() < 1e-12);
        assert!((cost_usd(FLASH_RATES, PEAK_MULTIPLIER, usage) - 2.0 * expected).abs() < 1e-12);
        let pro = (12_698.0 * 0.66 + 60_672.0 * 0.022 + 2_052.0 * 1.98) / 1_000_000.0;
        assert!((cost_usd(PRO_RATES, 1.0, usage) - pro).abs() < 1e-12);
    }

    #[test]
    fn builtin_table_matches_the_published_off_peak_prices() {
        assert_eq!(builtin_as_of(), NaiveDate::from_ymd_opt(2026, 10, 19).unwrap());
        let table = builtin_rates();
        assert_eq!(table[0], (FLASH_MODEL, Rates { input_cache_miss: 0.15, input_cache_hit: 0.003, output: 0.6 }));
        assert_eq!(table[1], (PRO_MODEL, Rates { input_cache_miss: 0.66, input_cache_hit: 0.022, output: 1.98 }));
    }

    #[test]
    fn the_holiday_table_only_lists_valid_weekdays() {
        for &(year, month, day) in WEEKDAY_HOLIDAYS {
            let date = NaiveDate::from_ymd_opt(year, month, day).expect("valid holiday date");
            assert!(!matches!(date.weekday(), Weekday::Sat | Weekday::Sun), "{date}");
        }
        assert_eq!(WEEKDAY_HOLIDAYS.len(), 19);
    }

    #[test]
    fn test_clocks_land_in_their_tiers() {
        assert!(!is_peak(off_peak_instant()));
        assert!(is_peak(peak_instant()));
    }
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `env -u NODE_OPTIONS cargo test -p orca-runtime --lib pricing::rates -- --test-threads=1`
Expected: 编译失败（`time_of_use_multiplier`、`cost_usd` 等未定义）。

- [ ] **Step 3: 实现**

把 `rates.rs` 测试模块以上的部分替换为：

```rust
//! DeepSeek's published prices as of a fixed date, and its time-of-use rule:
//! peak hours on Beijing weekdays cost twice the off-peak price.

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, Timelike, Utc, Weekday};
use orca_core::model::{FLASH_MODEL, PRO_MODEL};
use orca_core::provider_types::Usage;
use serde::{Deserialize, Serialize};

/// Prices in US dollars per million tokens.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct Rates {
    pub input_cache_miss: f64,
    pub input_cache_hit: f64,
    pub output: f64,
}

/// Off-peak prices from https://api-docs.deepseek.com/quick_start/pricing as of
/// [`builtin_as_of`]. Peak hours cost [`PEAK_MULTIPLIER`] times as much.
pub const FLASH_RATES: Rates = Rates {
    input_cache_miss: 0.15,
    input_cache_hit: 0.003,
    output: 0.6,
};
pub const PRO_RATES: Rates = Rates {
    input_cache_miss: 0.66,
    input_cache_hit: 0.022,
    output: 1.98,
};

pub const PEAK_MULTIPLIER: f64 = 2.0;

/// DeepSeek states its peak hours and holidays in Beijing time (UTC+8, no DST).
const BEIJING_UTC_OFFSET_SECS: i32 = 8 * 60 * 60;

/// Peak hours as `[start, end)` Beijing hours, Monday to Friday.
const PEAK_HOURS: [(u32, u32); 2] = [(9, 12), (14, 18)];

/// Weekdays that are days off in mainland China, so off-peak all day. From the
/// State Council's yearly notice; 2026:
/// https://www.gov.cn/zhengce/zhengceku/202511/content_7047091.htm
const WEEKDAY_HOLIDAYS: &[(i32, u32, u32)] = &[
    (2026, 1, 1),
    (2026, 1, 2),
    (2026, 2, 16),
    (2026, 2, 17),
    (2026, 2, 18),
    (2026, 2, 19),
    (2026, 2, 20),
    (2026, 2, 23),
    (2026, 4, 6),
    (2026, 5, 1),
    (2026, 5, 4),
    (2026, 5, 5),
    (2026, 6, 19),
    (2026, 9, 25),
    (2026, 10, 1),
    (2026, 10, 2),
    (2026, 10, 5),
    (2026, 10, 6),
    (2026, 10, 7),
];

/// The day the built-in prices were checked against DeepSeek's pricing page.
pub fn builtin_as_of() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, 19).expect("valid built-in date")
}

/// The built-in prices, by canonical model name.
pub fn builtin_rates() -> [(&'static str, Rates); 2] {
    [(FLASH_MODEL, FLASH_RATES), (PRO_MODEL, PRO_RATES)]
}

/// Whether DeepSeek bills a request made at `at` at its peak price.
pub fn is_peak(at: DateTime<Utc>) -> bool {
    let offset = FixedOffset::east_opt(BEIJING_UTC_OFFSET_SECS).expect("valid UTC offset");
    let beijing = at.with_timezone(&offset);
    if matches!(beijing.weekday(), Weekday::Sat | Weekday::Sun) {
        return false;
    }
    let date = beijing.date_naive();
    if WEEKDAY_HOLIDAYS
        .iter()
        .any(|&(year, month, day)| NaiveDate::from_ymd_opt(year, month, day) == Some(date))
    {
        return false;
    }
    let hour = beijing.hour();
    PEAK_HOURS
        .iter()
        .any(|&(start, end)| (start..end).contains(&hour))
}

pub fn time_of_use_multiplier(at: DateTime<Utc>) -> f64 {
    if is_peak(at) { PEAK_MULTIPLIER } else { 1.0 }
}

/// The cost of `usage` at `rates` times `multiplier`. `cache_tokens` are the
/// part of `input_tokens` that hit the cache.
pub fn cost_usd(rates: Rates, multiplier: f64, usage: Usage) -> f64 {
    let cache_miss = usage.input_tokens.saturating_sub(usage.cache_tokens);
    (cache_miss as f64 * rates.input_cache_miss
        + usage.cache_tokens as f64 * rates.input_cache_hit
        + usage.output_tokens as f64 * rates.output)
        * multiplier
        / 1_000_000.0
}

/// Saturday 2026-10-17, 10:00 in Beijing: off-peak. A clock for tests.
#[doc(hidden)]
pub fn off_peak_instant() -> DateTime<Utc> {
    "2026-10-17T02:00:00Z".parse().expect("valid instant")
}

/// Monday 2026-10-19, 10:00 in Beijing, a working day: peak. A clock for tests.
#[doc(hidden)]
pub fn peak_instant() -> DateTime<Utc> {
    "2026-10-19T02:00:00Z".parse().expect("valid instant")
}
```

- [ ] **Step 4: 运行测试，确认通过**

Run: `env -u NODE_OPTIONS cargo test -p orca-runtime --lib pricing::rates -- --test-threads=1`
Expected: 9 个测试全部通过。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add crates/orca-runtime/src/pricing crates/orca-runtime/src/lib.rs
git commit -m "feat(pricing): add DeepSeek built-in prices and peak hours" -m "Refs #136"
```

---

### Task 2: 配置——价格覆盖和刷新开关

**Files:**
- Modify: `crates/orca-core/src/model.rs`（`UsdPerMillion`、`ModelPriceOverride`、`ModelDefinition.pricing`、`ModelSelection::price_overrides`、`builtin_model_definition`）
- Modify: `crates/orca-core/src/config/file.rs`（`PricingFileConfig`；`FileConfig`、`RawFileConfig` 的 `pricing` 字段；`Default`、`From`；`remove_project_denied_fields`）
- Modify: `crates/orca-core/src/config/mod.rs`（`RunConfig.pricing_refresh`、`Default`、`format_config_show`）
- Modify: `crates/orca-runtime/src/command/config.rs`（`assemble_run_config`）
- Modify: 编译器报出的所有 `ModelDefinition { supports_images: .. }` 和完整的 `RunConfig { .. }` 字面量。已知的有：
  - `crates/orca-runtime/src/child_agent_tests.rs`（约 428 行）
  - `crates/orca-runtime/src/image_routing.rs`（约 516 行）
  - `crates/orca-runtime/src/computer_use/execute.rs`（约 914 行）
  - `crates/orca-runtime/src/command/config.rs`（约 153、194 行）
  - `crates/orca-core/src/config/mod.rs` 的测试（约 1130、1219、1303 行）
  - `crates/orca-runtime/src/lifecycle.rs` 测试里的 `fn config()`
- Modify: `docs/superpowers/specs/2026-10-19-dynamic-pricing-design.md` 第 2.5 节

**Interfaces:**
- Produces：
  - `orca_core::model::UsdPerMillion`：`new(f64) -> Option<Self>`、`get(self) -> f64`；derive `Clone, Copy, Debug, PartialEq, Serialize`，手写 `Eq` 和 `Deserialize`
  - `orca_core::model::ModelPriceOverride { input_cache_miss, input_cache_hit, output: UsdPerMillion }`
  - `ModelDefinition.pricing: Option<ModelPriceOverride>`
  - `ModelSelection::price_overrides(&self) -> BTreeMap<String, ModelPriceOverride>`，键是 canonical 模型名
  - `orca_core::config::file::PricingFileConfig { refresh: bool }`，默认 true；`FileConfig.pricing`
  - `RunConfig.pricing_refresh: bool`：`RunConfig::default()` 里为 false，`assemble_run_config` 取 `file.pricing.refresh`

- [ ] **Step 1: 写失败的测试**

`crates/orca-core/src/model.rs` 的 `mod tests` 追加：

```rust
    #[test]
    fn usd_per_million_accepts_only_finite_non_negative_prices() {
        assert_eq!(UsdPerMillion::new(0.0).map(UsdPerMillion::get), Some(0.0));
        assert_eq!(UsdPerMillion::new(1.5).map(UsdPerMillion::get), Some(1.5));
        for bad in [-0.01, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(UsdPerMillion::new(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn price_overrides_are_keyed_by_canonical_model_name() {
        let price = |value| UsdPerMillion::new(value).unwrap();
        let pricing = ModelPriceOverride {
            input_cache_miss: price(1.0),
            input_cache_hit: price(0.1),
            output: price(2.0),
        };
        let models = BTreeMap::from([(
            LEGACY_FLASH_MODEL.to_string(),
            ModelDefinition {
                pricing: Some(pricing),
                ..ModelDefinition::default()
            },
        )]);
        let selection = ModelSelection::parse_with_models(None, models).unwrap();
        assert_eq!(selection.price_overrides().get(FLASH_MODEL), Some(&pricing));
        assert_eq!(selection.price_overrides().len(), 1);
    }
```

`crates/orca-core/src/config/file.rs` 的 `mod tests` 追加：

```rust
    #[test]
    fn model_pricing_overrides_parse_and_reject_invalid_prices() {
        let config: FileConfig = toml::from_str(
            "[models.\"my-model\".pricing]\ninput_cache_miss = 0.5\ninput_cache_hit = 0.05\noutput = 2\n",
        )
        .unwrap();
        let pricing = config.models["my-model"].pricing.expect("pricing");
        assert_eq!(pricing.input_cache_miss.get(), 0.5);
        assert_eq!(pricing.input_cache_hit.get(), 0.05);
        assert_eq!(pricing.output.get(), 2.0);

        for bad in [
            "input_cache_miss = -1.0",
            "input_cache_miss = nan",
            "input_cache_miss = inf",
        ] {
            let source = format!(
                "[models.\"m\".pricing]\n{bad}\ninput_cache_hit = 0.1\noutput = 0.2\n"
            );
            assert!(toml::from_str::<FileConfig>(&source).is_err(), "{bad}");
        }
        let missing = "[models.\"m\".pricing]\ninput_cache_miss = 0.1\noutput = 0.2\n";
        assert!(toml::from_str::<FileConfig>(missing).is_err());
        let unknown =
            "[models.\"m\".pricing]\ninput_cache_miss = 0.1\ninput_cache_hit = 0.1\noutput = 0.2\ninput = 0.1\n";
        assert!(toml::from_str::<FileConfig>(unknown).is_err());
    }

    #[test]
    fn pricing_refresh_defaults_on_and_can_be_disabled() {
        assert!(FileConfig::default().pricing.refresh);
        let config: FileConfig = toml::from_str("[pricing]\nrefresh = false\n").unwrap();
        assert!(!config.pricing.refresh);
    }

    #[test]
    fn project_layer_drops_pricing_settings() {
        let mut project: Value = toml::from_str(
            r#"
model = "deepseek-v4-pro"

[pricing]
refresh = false

[models."deepseek-flash"]
supports_images = true

[models."deepseek-flash".pricing]
input_cache_miss = 0.0
input_cache_hit = 0.0
output = 0.0
"#,
        )
        .unwrap();

        remove_project_denied_fields(&mut project);

        assert!(project.get("pricing").is_none());
        assert!(project["models"]["deepseek-flash"].get("pricing").is_none());
        assert_eq!(
            project["models"]["deepseek-flash"]["supports_images"].as_bool(),
            Some(true)
        );
        assert_eq!(project["model"].as_str(), Some("deepseek-v4-pro"));
    }
```

`crates/orca-runtime/src/command/config.rs` 的 `mod tests` 追加：

```rust
    #[test]
    fn run_config_carries_the_pricing_refresh_switch() {
        let request = || RunConfigRequest::new("0.0.0", PathBuf::from("/workspace"));
        assert!(
            assemble_run_config(request(), FileConfig::default())
                .unwrap()
                .pricing_refresh
        );
        let mut file = FileConfig::default();
        file.pricing.refresh = false;
        assert!(!assemble_run_config(request(), file).unwrap().pricing_refresh);
    }
```

在 `crates/orca-core/src/config/mod.rs` 的 `format_config_show_redacts_api_key_and_includes_effective_values` 里，测试的 `RunConfig` 字面量加 `pricing_refresh: false,`，并追加：

```rust
        assert!(shown.contains("pricing_refresh = false\n"), "{shown}");
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `env -u NODE_OPTIONS cargo test -p orca-core model:: -- --test-threads=1`
Expected: 编译失败（`UsdPerMillion` 未定义）。

- [ ] **Step 3: 实现 orca-core 的类型**

在 `crates/orca-core/src/model.rs` 中 `ModelDefinition` 之前加入：

```rust
/// A price in US dollars per million tokens: finite and not negative, so two
/// prices compare like integers do.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct UsdPerMillion(f64);

impl Eq for UsdPerMillion {}

impl UsdPerMillion {
    pub fn new(value: f64) -> Option<Self> {
        (value.is_finite() && value >= 0.0).then_some(Self(value))
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for UsdPerMillion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = f64::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| {
            serde::de::Error::custom(
                "a price must be a finite, non-negative number of US dollars per million tokens",
            )
        })
    }
}

/// `[models."<name>".pricing]`: fixed prices that replace DeepSeek's published
/// ones for this model, at every hour.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPriceOverride {
    pub input_cache_miss: UsdPerMillion,
    pub input_cache_hit: UsdPerMillion,
    pub output: UsdPerMillion,
}
```

`ModelDefinition` 改为：

```rust
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelDefinition {
    #[serde(default)]
    pub supports_images: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<ModelPriceOverride>,
}
```

`builtin_model_definition` 的返回值改为 `ModelDefinition { supports_images, pricing: None }`。

`impl ModelSelection` 里 `supports_images` 之后加入：

```rust
    /// Fixed prices from `[models."<name>".pricing]`, by canonical model name.
    pub fn price_overrides(&self) -> BTreeMap<String, ModelPriceOverride> {
        self.models
            .iter()
            .filter_map(|(model, definition)| {
                definition.pricing.map(|pricing| (model.clone(), pricing))
            })
            .collect()
    }
```

- [ ] **Step 4: 实现文件配置和 RunConfig**

`crates/orca-core/src/config/file.rs`：

```rust
/// `[pricing]`: whether Orca refreshes DeepSeek's prices from models.dev.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct PricingFileConfig {
    #[serde(default = "default_true")]
    pub refresh: bool,
}

impl Default for PricingFileConfig {
    fn default() -> Self {
        Self { refresh: true }
    }
}
```

在 `FileConfig` 和 `RawFileConfig` 的 `update_check` 字段之后各加：

```rust
    #[serde(default)]
    pub pricing: PricingFileConfig,
```

`impl Default for FileConfig` 加 `pricing: PricingFileConfig::default(),`；`impl From<RawFileConfig> for FileConfig` 加 `pricing: raw.pricing,`。

`remove_project_denied_fields`：
- 在键列表里 `"budget",` 之后加 `"pricing",`；
- 在 `for key in [...] { table.remove(key); }` 之后、`if let Some(table)` 块之内加：

```rust
        // Prices decide when a cost budget stops a run, so like `budget` they
        // stay user-owned.
        if let Some(models) = table.get_mut("models").and_then(Value::as_table_mut) {
            for definition in models.values_mut() {
                if let Some(definition) = definition.as_table_mut() {
                    definition.remove("pricing");
                }
            }
        }
```

`crates/orca-core/src/config/mod.rs`：
- `RunConfig` 在 `pub update_check: bool,` 后加 `pub pricing_refresh: bool,`；
- `impl Default for RunConfig` 在 `update_check: false,` 后加 `pricing_refresh: false,`；
- `format_config_show` 的 `concat!` 在 `"update_check = {}\n",` 后加 `"pricing_refresh = {}\n",`，参数列表在 `config.update_check,` 后加 `config.pricing_refresh,`。

`crates/orca-runtime/src/command/config.rs` 的 `assemble_run_config`，在 `update_check: file.update_check,` 后加 `pricing_refresh: file.pricing.refresh,`。

用 `env -u NODE_OPTIONS cargo build --workspace --all-targets --locked 2>&1 | grep -E '^error' -A6` 找出所有缺字段的字面量，按以下方式补齐。不要改动别的地方：
- `ModelDefinition { supports_images: X }` 改为 `ModelDefinition { supports_images: X, ..ModelDefinition::default() }`；
- 完整的 `RunConfig { .. }` 字面量加 `pricing_refresh: false,`。

- [ ] **Step 5: 补充 spec**

在 `docs/superpowers/specs/2026-10-19-dynamic-pricing-design.md` 第 2.5 节的最后一条之后，加一条：

```markdown
- 项目级 `.orca/config.toml` 不能设置 `[pricing]`，也不能设置任何 `[models.*.pricing]`（`remove_project_denied_fields` 会删掉它们）。价格决定成本预算什么时候让运行停下，所以和 `budget` 一样只认用户配置。
```

- [ ] **Step 6: 运行测试，确认通过**

Run:
- `env -u NODE_OPTIONS cargo test -p orca-core -- --test-threads=1`
- `env -u NODE_OPTIONS cargo test -p orca-runtime --lib command::config -- --test-threads=1`
- `env -u NODE_OPTIONS cargo build --workspace --all-targets --locked`

Expected: 全部通过，新测试都在其中。

- [ ] **Step 7: 提交**

```bash
cargo fmt --all
git add -A crates docs/superpowers/specs/2026-10-19-dynamic-pricing-design.md
git commit -m "feat(config): add model price overrides and the pricing refresh switch" -m "Refs #136"
```

---

### Task 3: 价格表和缓存（`pricing::book`）

**Files:**
- Create: `crates/orca-runtime/src/pricing/book.rs`
- Modify: `crates/orca-runtime/src/pricing/mod.rs`（加 `pub mod book;`）

**Interfaces:**
- Consumes：Task 1 的 `Rates`、`FLASH_RATES`、`builtin_rates()`、`builtin_as_of()`
- Produces：
  - `CACHE_FILE: &str = "pricing-cache.json"`、`CACHE_VERSION: u32 = 1`
  - `PricingCache { version: u32, source: String, fetched_at: DateTime<Utc>, etag: Option<String>, models: BTreeMap<String, Rates> }`，derive `Clone, Debug, Deserialize, PartialEq, Serialize`
  - `PriceSource::{Builtin { as_of: NaiveDate }, Fetched { source: String, fetched_at: DateTime<Utc> }}`
  - `PriceBook { models: BTreeMap<String, Rates>, source: PriceSource }`，方法 `PriceBook::rates_for(&self, model: &str) -> Rates`
  - `fn rates_are_plausible(model: &str, rates: Rates) -> bool`
  - `fn merge(cache: Option<&PricingCache>) -> PriceBook`
  - `fn cache_path() -> Option<PathBuf>`
  - `fn read_cache(path: &Path) -> Option<PricingCache>`
  - `fn write_cache(path: &Path, cache: &PricingCache) -> io::Result<()>`
  - `fn current() -> Arc<PriceBook>`、`pub(crate) fn install(cache: &PricingCache)`

- [ ] **Step 1: 写失败的测试**

`crates/orca-runtime/src/pricing/book.rs`：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::rates::{FLASH_RATES, PRO_RATES};
    use orca_core::model::{FLASH_MODEL, PRO_MODEL};

    const FETCHED_FLASH: Rates = Rates {
        input_cache_miss: 0.2,
        input_cache_hit: 0.004,
        output: 0.8,
    };
    const NEW_MODEL: Rates = Rates {
        input_cache_miss: 0.5,
        input_cache_hit: 0.01,
        output: 2.0,
    };

    fn cache(fetched_at: &str) -> PricingCache {
        PricingCache {
            version: CACHE_VERSION,
            source: "https://models.dev/api.json".to_string(),
            fetched_at: fetched_at.parse().unwrap(),
            etag: Some("W/\"v1\"".to_string()),
            models: BTreeMap::from([
                (FLASH_MODEL.to_string(), FETCHED_FLASH),
                ("deepseek-v5-flash".to_string(), NEW_MODEL),
            ]),
        }
    }

    #[test]
    fn without_a_cache_the_book_is_the_builtin_table() {
        let book = merge(None);
        assert_eq!(book.rates_for(FLASH_MODEL), FLASH_RATES);
        assert_eq!(book.rates_for(PRO_MODEL), PRO_RATES);
        assert_eq!(
            book.source,
            PriceSource::Builtin {
                as_of: crate::pricing::rates::builtin_as_of()
            }
        );
    }

    #[test]
    fn a_current_cache_overrides_builtin_prices_and_adds_models() {
        let book = merge(Some(&cache("2026-10-20T08:00:00Z")));
        assert_eq!(book.rates_for(FLASH_MODEL), FETCHED_FLASH);
        assert_eq!(book.rates_for(PRO_MODEL), PRO_RATES);
        assert_eq!(book.rates_for("deepseek-v5-flash"), NEW_MODEL);
        assert!(matches!(book.source, PriceSource::Fetched { .. }));
    }

    #[test]
    fn a_cache_older_than_the_builtin_table_only_adds_models() {
        let book = merge(Some(&cache("2026-09-01T08:00:00Z")));
        assert_eq!(book.rates_for(FLASH_MODEL), FLASH_RATES);
        assert_eq!(book.rates_for("deepseek-v5-flash"), NEW_MODEL);
        assert!(matches!(book.source, PriceSource::Builtin { .. }));
    }

    #[test]
    fn unknown_models_fall_back_to_pro_for_v4_pro_names_and_flash_otherwise() {
        let book = merge(None);
        assert_eq!(book.rates_for("deepseek-v4-pro-exp"), PRO_RATES);
        assert_eq!(book.rates_for("qwen-max"), FLASH_RATES);
        assert_eq!(book.rates_for(""), FLASH_RATES);
    }

    #[test]
    fn plausibility_rejects_unit_errors_and_impossible_numbers() {
        let flash = |input_cache_miss| Rates {
            input_cache_miss,
            ..FLASH_RATES
        };
        assert!(rates_are_plausible(FLASH_MODEL, FLASH_RATES));
        assert!(rates_are_plausible(FLASH_MODEL, flash(1.4))); // under 10x
        assert!(!rates_are_plausible(FLASH_MODEL, flash(1.6))); // over 10x
        assert!(!rates_are_plausible(FLASH_MODEL, flash(0.15e-6))); // per token
        assert!(!rates_are_plausible(FLASH_MODEL, flash(0.15e-3))); // per thousand
        assert!(!rates_are_plausible(FLASH_MODEL, flash(-0.15)));
        assert!(!rates_are_plausible(FLASH_MODEL, flash(f64::NAN)));
        assert!(!rates_are_plausible("unknown", Rates { output: 101.0, ..FLASH_RATES }));
        assert!(rates_are_plausible("unknown", NEW_MODEL));
    }

    #[test]
    fn the_cache_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(CACHE_FILE);
        let written = cache("2026-10-20T08:00:00Z");
        write_cache(&path, &written).unwrap();
        assert_eq!(read_cache(&path), Some(written));
    }

    #[test]
    fn read_cache_ignores_corrupt_foreign_and_implausible_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        assert_eq!(read_cache(&path), None); // missing

        let mut text = serde_json::to_string(&cache("2026-10-20T08:00:00Z")).unwrap();
        text.truncate(text.len() / 2);
        std::fs::write(&path, text).unwrap();
        assert_eq!(read_cache(&path), None); // truncated by a crash

        let mut foreign = cache("2026-10-20T08:00:00Z");
        foreign.version = CACHE_VERSION + 1;
        std::fs::write(&path, serde_json::to_vec(&foreign).unwrap()).unwrap();
        assert_eq!(read_cache(&path), None); // written by a newer Orca

        let mut implausible = cache("2026-10-20T08:00:00Z");
        implausible.models = BTreeMap::from([(
            FLASH_MODEL.to_string(),
            Rates { output: 0.6e-6, ..FLASH_RATES },
        )]);
        std::fs::write(&path, serde_json::to_vec(&implausible).unwrap()).unwrap();
        assert_eq!(read_cache(&path), None); // nothing plausible left

        let mut partly = cache("2026-10-20T08:00:00Z");
        partly.models.insert(
            PRO_MODEL.to_string(),
            Rates { input_cache_miss: 660.0, ..PRO_RATES },
        );
        std::fs::write(&path, serde_json::to_vec(&partly).unwrap()).unwrap();
        let read = read_cache(&path).expect("plausible entries survive");
        assert!(!read.models.contains_key(PRO_MODEL));
        assert_eq!(read.models.get(FLASH_MODEL), Some(&FETCHED_FLASH));
    }
}
```

`crates/orca-runtime/src/pricing/mod.rs` 改为：

```rust
//! Cost estimates at DeepSeek's prices: the built-in table and its
//! time-of-use rule (`rates`), and the process-wide price book with its cache
//! (`book`).

pub mod book;
pub mod rates;
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `env -u NODE_OPTIONS cargo test -p orca-runtime --lib pricing::book -- --test-threads=1`
Expected: 编译失败（`merge`、`PricingCache` 等未定义）。

- [ ] **Step 3: 实现**

在 `book.rs` 的测试模块之前写：

```rust
//! The prices estimates use: the built-in table, overlaid by the prices last
//! fetched from models.dev and cached in `$ORCA_HOME/pricing-cache.json`.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use chrono::{DateTime, NaiveDate, Utc};
use orca_core::model::{FLASH_MODEL, PRO_MODEL};
use orca_platform::fs::{AtomicWritePolicy, atomic_write};
use serde::{Deserialize, Serialize};

use super::rates::{self, Rates};

pub const CACHE_FILE: &str = "pricing-cache.json";
pub const CACHE_VERSION: u32 = 1;
/// The highest price accepted from any source, in US dollars per million tokens.
const MAX_PRICE: f64 = 100.0;
/// How far a fetched price may move from the built-in one before it is taken
/// for a unit error (per token, per thousand tokens).
const MAX_DRIFT: f64 = 10.0;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PricingCache {
    pub version: u32,
    pub source: String,
    pub fetched_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    pub models: BTreeMap<String, Rates>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PriceSource {
    Builtin {
        as_of: NaiveDate,
    },
    Fetched {
        source: String,
        fetched_at: DateTime<Utc>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct PriceBook {
    pub models: BTreeMap<String, Rates>,
    pub source: PriceSource,
}

impl PriceBook {
    /// Off-peak rates for a canonical model name: its own, else Pro's for a
    /// `v4-pro` variant, else Flash's (the default model).
    pub fn rates_for(&self, model: &str) -> Rates {
        if let Some(rates) = self.models.get(model) {
            return *rates;
        }
        if model.contains("v4-pro")
            && let Some(rates) = self.models.get(PRO_MODEL)
        {
            return *rates;
        }
        self.models
            .get(FLASH_MODEL)
            .copied()
            .unwrap_or(rates::FLASH_RATES)
    }
}

/// Whether `rates` can be trusted for `model`: every price is a plausible
/// amount, and none is ten times off the built-in price for the same model.
pub fn rates_are_plausible(model: &str, rates: Rates) -> bool {
    let prices = [rates.input_cache_miss, rates.input_cache_hit, rates.output];
    if prices
        .iter()
        .any(|price| !price.is_finite() || *price < 0.0 || *price > MAX_PRICE)
    {
        return false;
    }
    let Some((_, builtin)) = rates::builtin_rates()
        .into_iter()
        .find(|(name, _)| *name == model)
    else {
        return true;
    };
    within_drift(builtin.input_cache_miss, rates.input_cache_miss)
        && within_drift(builtin.input_cache_hit, rates.input_cache_hit)
        && within_drift(builtin.output, rates.output)
}

fn within_drift(reference: f64, price: f64) -> bool {
    price >= reference / MAX_DRIFT && price <= reference * MAX_DRIFT
}

/// The built-in table overlaid by `cache`: a cache fetched on or after the
/// built-in date wins for every model it lists; an older one only adds models
/// the built-in table lacks.
pub fn merge(cache: Option<&PricingCache>) -> PriceBook {
    let mut models: BTreeMap<String, Rates> = rates::builtin_rates()
        .into_iter()
        .map(|(model, rates)| (model.to_string(), rates))
        .collect();
    let as_of = rates::builtin_as_of();
    let Some(cache) = cache else {
        return PriceBook {
            models,
            source: PriceSource::Builtin { as_of },
        };
    };
    let cache_is_current = cache.fetched_at.date_naive() >= as_of;
    for (model, rates) in &cache.models {
        if cache_is_current || !models.contains_key(model) {
            models.insert(model.clone(), *rates);
        }
    }
    let source = if cache_is_current {
        PriceSource::Fetched {
            source: cache.source.clone(),
            fetched_at: cache.fetched_at,
        }
    } else {
        PriceSource::Builtin { as_of }
    };
    PriceBook { models, source }
}

pub fn cache_path() -> Option<PathBuf> {
    orca_core::home::orca_home().map(|home| home.join(CACHE_FILE))
}

/// The cache at `path`, without its implausible entries; `None` when it is
/// missing, unreadable, written by another cache version, or left with nothing.
pub fn read_cache(path: &Path) -> Option<PricingCache> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut cache: PricingCache = serde_json::from_str(&text).ok()?;
    if cache.version != CACHE_VERSION {
        return None;
    }
    cache
        .models
        .retain(|model, rates| rates_are_plausible(model, *rates));
    (!cache.models.is_empty()).then_some(cache)
}

pub fn write_cache(path: &Path, cache: &PricingCache) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec_pretty(cache).map_err(io::Error::other)?;
    bytes.push(b'\n');
    atomic_write(path, &bytes, AtomicWritePolicy::NoFollow).map_err(io::Error::other)
}

fn global() -> &'static RwLock<Arc<PriceBook>> {
    static BOOK: OnceLock<RwLock<Arc<PriceBook>>> = OnceLock::new();
    BOOK.get_or_init(|| {
        let cache = cache_path().and_then(|path| read_cache(&path));
        RwLock::new(Arc::new(merge(cache.as_ref())))
    })
}

/// The process-wide book: loaded from the cache on first use, replaced when a
/// refresh lands.
pub fn current() -> Arc<PriceBook> {
    global()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

pub(crate) fn install(cache: &PricingCache) {
    let book = Arc::new(merge(Some(cache)));
    *global()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = book;
}
```

- [ ] **Step 4: 运行测试，确认通过**

Run: `env -u NODE_OPTIONS cargo test -p orca-runtime --lib pricing:: -- --test-threads=1`
Expected: rates 和 book 的测试全部通过。

`install` 要到任务 4、5 才有调用方，clippy 会报 `dead_code`。先在 `install` 上加 `#[allow(dead_code)]`，任务 5 删掉。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add crates/orca-runtime/src/pricing
git commit -m "feat(pricing): add the price book and its cache" -m "Refs #136"
```

---

### Task 4: 从 models.dev 刷新（`pricing::refresh`）

**Files:**
- Modify: `Cargo.toml`：`[workspace.dependencies]` 里按字母序加 `flate2 = "1.1"`
- Modify: `crates/orca-runtime/Cargo.toml`：`[dependencies]` 里加 `flate2 = { workspace = true }`
- Modify: `Cargo.lock`：由 `cargo build` 更新，只应多出 orca-runtime 对 flate2 的依赖边
- Create: `crates/orca-runtime/src/pricing/refresh.rs`
- Modify: `crates/orca-runtime/src/pricing/mod.rs`（加 `pub mod refresh;`，并更新模块说明）

**Interfaces:**
- Consumes：Task 3 的 `PricingCache`、`CACHE_VERSION`、`rates_are_plausible`、`read_cache`、`write_cache`、`cache_path`、`install`；Task 1 的 `Rates`
- Produces：
  - `DEFAULT_URL: &str`、`URL_ENV: &str = "ORCA_PRICING_URL"`
  - `enum RefreshOutcome { Updated(PricingCache), NotModified(PricingCache), Failed(String) }`，derive `Debug, PartialEq`
  - `fn parse_models_dev(body: &[u8]) -> Result<BTreeMap<String, Rates>, String>`
  - `fn fetch(url, previous: Option<&PricingCache>, now) -> RefreshOutcome`
  - `fn fetch_with_limits(url, previous, now, timeout: Duration, max_body_bytes: u64) -> RefreshOutcome`
  - `fn refresh_url(enabled: bool, url_override: Option<String>, dev_build: bool, cached: Option<&PricingCache>, now) -> Option<String>`
  - `fn refresh_and_store(url, path: &Path, previous, now) -> RefreshOutcome`
  - `pub(crate) fn ensure_background_refresh(enabled: bool)`

- [ ] **Step 1: 加依赖**

在 `Cargo.toml` 的 `[workspace.dependencies]` 里加 `flate2 = "1.1"`，在 `crates/orca-runtime/Cargo.toml` 的 `[dependencies]` 里加 `flate2 = { workspace = true }`。然后跑 `env -u NODE_OPTIONS cargo build -p orca-runtime`，确认 `git diff Cargo.lock` 只多出 orca-runtime 对 flate2 的依赖。

- [ ] **Step 2: 写失败的测试**

`crates/orca-runtime/src/pricing/refresh.rs` 的测试模块（实现部分先只写签名，函数体用 `todo!()`，让测试能编译但失败）：

```rust
#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::time::Instant;

    use super::*;
    use crate::pricing::book::read_cache;
    use crate::pricing::rates::{FLASH_RATES, PRO_RATES};
    use orca_core::model::{FLASH_MODEL, PRO_MODEL};

    /// The deepseek provider from models.dev's api.json on 2026-10-19, with one
    /// unrelated provider to skip.
    const MODELS_DEV: &str = r#"{
      "openai": {"id": "openai", "models": {"gpt-x": {"cost": {"input": 1.0, "output": 2.0}}}},
      "deepseek": {"id": "deepseek", "api": "https://api.deepseek.com", "models": {
        "deepseek-flash": {"id": "deepseek-flash", "cost": {"input": 0.15, "output": 0.6, "reasoning": 0.6, "cache_read": 0.003}},
        "deepseek-v4-flash": {"id": "deepseek-v4-flash", "cost": {"input": 0.15, "output": 0.6, "reasoning": 0.6, "cache_read": 0.003}},
        "deepseek-v4-pro": {"id": "deepseek-v4-pro", "cost": {"input": 0.66, "output": 1.98, "reasoning": 1.98, "cache_read": 0.022}},
        "deepseek-v4-flash-vision-exp": {"id": "deepseek-v4-flash-vision-exp", "cost": {"input": 0.15, "output": 0.6, "reasoning": 0.6, "cache_read": 0.003}}
      }}
    }"#;

    fn now() -> DateTime<Utc> {
        "2026-10-20T08:00:00Z".parse().unwrap()
    }

    enum Reply {
        Send(Vec<u8>),
        Stall,
    }

    /// A blocking local HTTP server answering successive connections with
    /// `replies`, one each; joins to the request heads it read (lowercased).
    fn serve(replies: Vec<Reply>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind pricing fixture");
        let url = format!("http://{}/api.json", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut heads = Vec::new();
            for reply in replies {
                let (mut stream, _) = listener.accept().expect("accept pricing request");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }
                heads.push(head.to_ascii_lowercase());
                match reply {
                    Reply::Send(bytes) => {
                        let _ = stream.write_all(&bytes);
                        let _ = stream.flush();
                    }
                    Reply::Stall => std::thread::sleep(Duration::from_secs(10)),
                }
            }
            heads
        });
        (url, server)
    }

    fn response(status: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
        let mut head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(body);
        Reply::Send(bytes)
    }

    fn gzip(body: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(body).unwrap();
        encoder.finish().unwrap()
    }

    fn cached(fetched_at: &str, etag: Option<&str>) -> PricingCache {
        PricingCache {
            version: CACHE_VERSION,
            source: DEFAULT_URL.to_string(),
            fetched_at: fetched_at.parse().unwrap(),
            etag: etag.map(str::to_string),
            models: BTreeMap::from([(FLASH_MODEL.to_string(), FLASH_RATES)]),
        }
    }

    #[test]
    fn parse_keeps_canonical_deepseek_prices() {
        let prices = parse_models_dev(MODELS_DEV.as_bytes()).unwrap();
        assert_eq!(prices.get(FLASH_MODEL), Some(&FLASH_RATES));
        assert_eq!(prices.get(PRO_MODEL), Some(&PRO_RATES));
        assert_eq!(prices.len(), 2, "aliases fold into canonical names: {prices:?}");
    }

    #[test]
    fn parse_prefers_the_canonical_entry_over_an_alias() {
        let body = r#"{"deepseek": {"models": {
          "deepseek-v4-flash": {"cost": {"input": 0.2, "output": 0.8, "cache_read": 0.004}},
          "deepseek-flash": {"cost": {"input": 0.15, "output": 0.6, "cache_read": 0.003}}
        }}}"#;
        let prices = parse_models_dev(body.as_bytes()).unwrap();
        assert_eq!(prices.get(FLASH_MODEL), Some(&FLASH_RATES));
    }

    #[test]
    fn parse_skips_incomplete_or_implausible_entries_and_rejects_empty_results() {
        let body = r#"{"deepseek": {"models": {
          "deepseek-flash": {"cost": {"input": 0.15e-6, "output": 0.6e-6, "cache_read": 0.003e-6}},
          "deepseek-v4-pro": {"cost": {"input": 0.66, "output": 1.98}},
          "deepseek-v5-flash": {"cost": {"input": 0.3, "output": 1.2, "cache_read": 0.006}}
        }}}"#;
        let prices = parse_models_dev(body.as_bytes()).unwrap();
        assert_eq!(prices.keys().collect::<Vec<_>>(), ["deepseek-v5-flash"]);

        assert!(parse_models_dev(br#"{"openai": {"models": {}}}"#).is_err());
        assert!(parse_models_dev(br#"{"deepseek": {"models": {}}}"#).is_err());
        assert!(parse_models_dev(b"not json").is_err());
    }

    #[test]
    fn fetch_reads_gzip_json_and_records_the_etag() {
        let (url, server) = serve(vec![response(
            "200 OK",
            &[("Content-Encoding", "gzip"), ("ETag", "W/\"v1\"")],
            &gzip(MODELS_DEV.as_bytes()),
        )]);
        let outcome = fetch(&url, None, now());
        let RefreshOutcome::Updated(cache) = outcome else {
            panic!("expected an update, got {outcome:?}");
        };
        assert_eq!(cache.version, CACHE_VERSION);
        assert_eq!(cache.source, url);
        assert_eq!(cache.fetched_at, now());
        assert_eq!(cache.etag.as_deref(), Some("W/\"v1\""));
        assert_eq!(cache.models.get(FLASH_MODEL), Some(&FLASH_RATES));
        let heads = server.join().unwrap();
        assert!(heads[0].contains("accept-encoding: gzip"), "{heads:?}");
        assert!(heads[0].contains("user-agent: orca-pricing"), "{heads:?}");
        assert!(!heads[0].contains("if-none-match"), "{heads:?}");
    }

    #[test]
    fn fetch_accepts_an_uncompressed_body() {
        let (url, server) = serve(vec![response("200 OK", &[], MODELS_DEV.as_bytes())]);
        assert!(matches!(fetch(&url, None, now()), RefreshOutcome::Updated(_)));
        server.join().unwrap();
    }

    #[test]
    fn fetch_sends_the_etag_and_keeps_the_cache_on_304() {
        let (url, server) = serve(vec![response("304 Not Modified", &[], b"")]);
        let previous = cached("2026-10-18T08:00:00Z", Some("\"v1\""));
        let outcome = fetch(&url, Some(&previous), now());
        let RefreshOutcome::NotModified(cache) = outcome else {
            panic!("expected not-modified, got {outcome:?}");
        };
        assert_eq!(cache.models, previous.models);
        assert_eq!(cache.etag, previous.etag);
        assert_eq!(cache.fetched_at, now());
        let heads = server.join().unwrap();
        assert!(heads[0].contains("if-none-match: \"v1\""), "{heads:?}");
    }

    #[test]
    fn fetch_fails_cleanly_on_bad_responses() {
        let (url, server) = serve(vec![
            response("500 Internal Server Error", &[], b"oops"),
            response("200 OK", &[("Content-Encoding", "gzip")], b"not gzip"),
            response("304 Not Modified", &[], b""),
            response("200 OK", &[], &[b' '; 4096]),
        ]);
        assert!(matches!(fetch(&url, None, now()), RefreshOutcome::Failed(_)));
        assert!(matches!(fetch(&url, None, now()), RefreshOutcome::Failed(_)));
        // 304 without a cached copy to keep.
        assert!(matches!(fetch(&url, None, now()), RefreshOutcome::Failed(_)));
        let too_large = fetch_with_limits(&url, None, now(), Duration::from_secs(5), 1024);
        assert!(matches!(too_large, RefreshOutcome::Failed(ref error) if error.contains("too large")), "{too_large:?}");
        server.join().unwrap();
    }

    #[test]
    fn fetch_gives_up_at_its_timeout() {
        let (url, _server) = serve(vec![Reply::Stall]);
        let started = Instant::now();
        let outcome = fetch_with_limits(&url, None, now(), Duration::from_millis(500), 1024);
        assert!(matches!(outcome, RefreshOutcome::Failed(_)), "{outcome:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn refresh_url_follows_the_switch_the_build_and_the_cache_age() {
        let fresh = cached("2026-10-20T02:00:00Z", None);
        let stale = cached("2026-10-19T07:00:00Z", None);
        let custom = || Some("http://127.0.0.1:9/api.json".to_string());
        assert_eq!(refresh_url(false, None, false, None, now()), None);
        assert_eq!(refresh_url(true, None, true, None, now()), None);
        assert_eq!(refresh_url(true, custom(), true, None, now()), custom());
        assert_eq!(refresh_url(true, Some("  ".to_string()), true, None, now()), None);
        assert_eq!(refresh_url(true, None, false, Some(&fresh), now()), None);
        assert_eq!(
            refresh_url(true, None, false, Some(&stale), now()),
            Some(DEFAULT_URL.to_string())
        );
        assert_eq!(
            refresh_url(true, None, false, None, now()),
            Some(DEFAULT_URL.to_string())
        );
    }

    #[test]
    fn refresh_and_store_writes_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pricing-cache.json");
        let (url, server) = serve(vec![
            response("200 OK", &[("ETag", "\"v2\"")], MODELS_DEV.as_bytes()),
            response("304 Not Modified", &[], b""),
        ]);
        let first = refresh_and_store(&url, &path, None, now());
        assert!(matches!(first, RefreshOutcome::Updated(_)));
        let stored = read_cache(&path).expect("stored cache");
        assert_eq!(stored.etag.as_deref(), Some("\"v2\""));

        let later: DateTime<Utc> = "2026-10-21T09:00:00Z".parse().unwrap();
        let second = refresh_and_store(&url, &path, Some(&stored), later);
        assert!(matches!(second, RefreshOutcome::NotModified(_)));
        assert_eq!(read_cache(&path).unwrap().fetched_at, later);
        server.join().unwrap();
    }

    #[test]
    fn refresh_and_store_succeeds_when_the_cache_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"").unwrap();
        let path = blocker.join("pricing-cache.json");
        let (url, server) = serve(vec![response("200 OK", &[], MODELS_DEV.as_bytes())]);
        assert!(matches!(
            refresh_and_store(&url, &path, None, now()),
            RefreshOutcome::Updated(_)
        ));
        assert!(!path.exists());
        server.join().unwrap();
    }
}
```

- [ ] **Step 3: 运行测试，确认失败**

Run: `env -u NODE_OPTIONS cargo test -p orca-runtime --lib pricing::refresh -- --test-threads=1`
Expected: 失败（`todo!()` panic，或者函数未定义）。

- [ ] **Step 4: 实现**

`refresh.rs` 测试模块以上：

```rust
//! Refreshes DeepSeek's prices from models.dev, which records the off-peak
//! price: at most once a day, in the background, never blocking a turn.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use flate2::read::GzDecoder;
use orca_core::model::canonical_model_name;
use serde::Deserialize;

use super::book::{self, CACHE_VERSION, PricingCache};
use super::rates::Rates;

pub const DEFAULT_URL: &str = "https://models.dev/api.json";
/// Replaces [`DEFAULT_URL`]; also lets a development build refresh, which is
/// how tests point it at a local fixture.
pub const URL_ENV: &str = "ORCA_PRICING_URL";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BODY_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, PartialEq)]
pub enum RefreshOutcome {
    Updated(PricingCache),
    NotModified(PricingCache),
    Failed(String),
}

#[derive(Deserialize)]
struct ModelsDevApi {
    deepseek: Option<ModelsDevProvider>,
}

#[derive(Deserialize)]
struct ModelsDevProvider {
    #[serde(default)]
    models: BTreeMap<String, ModelsDevModel>,
}

#[derive(Deserialize)]
struct ModelsDevModel {
    cost: Option<ModelsDevCost>,
}

#[derive(Deserialize)]
struct ModelsDevCost {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
}

/// DeepSeek's off-peak prices in a models.dev `api.json` body, by canonical
/// model name. Incomplete and implausible entries are skipped; a canonical
/// entry wins over an alias of it.
pub fn parse_models_dev(body: &[u8]) -> Result<BTreeMap<String, Rates>, String> {
    let api: ModelsDevApi = serde_json::from_slice(body)
        .map_err(|error| format!("invalid models.dev JSON: {error}"))?;
    let provider = api
        .deepseek
        .ok_or_else(|| "models.dev lists no deepseek provider".to_string())?;
    let mut prices = BTreeMap::new();
    for (id, model) in provider.models {
        let Some(ModelsDevCost {
            input: Some(input),
            output: Some(output),
            cache_read: Some(cache_read),
        }) = model.cost
        else {
            continue;
        };
        let rates = Rates {
            input_cache_miss: input,
            input_cache_hit: cache_read,
            output,
        };
        let canonical = canonical_model_name(&id);
        if !book::rates_are_plausible(canonical, rates) {
            continue;
        }
        if id == canonical || !prices.contains_key(canonical) {
            prices.insert(canonical.to_string(), rates);
        }
    }
    if prices.is_empty() {
        return Err("models.dev lists no plausible deepseek prices".to_string());
    }
    Ok(prices)
}

pub fn fetch(url: &str, previous: Option<&PricingCache>, now: DateTime<Utc>) -> RefreshOutcome {
    fetch_with_limits(url, previous, now, REQUEST_TIMEOUT, MAX_BODY_BYTES)
}

/// One conditional GET of `url`, sending `previous`'s ETag.
pub fn fetch_with_limits(
    url: &str,
    previous: Option<&PricingCache>,
    now: DateTime<Utc>,
    timeout: Duration,
    max_body_bytes: u64,
) -> RefreshOutcome {
    match try_fetch(url, previous, now, timeout, max_body_bytes) {
        Ok(outcome) => outcome,
        Err(error) => RefreshOutcome::Failed(error),
    }
}

fn try_fetch(
    url: &str,
    previous: Option<&PricingCache>,
    now: DateTime<Utc>,
    timeout: Duration,
    max_body_bytes: u64,
) -> Result<RefreshOutcome, String> {
    use reqwest::header::{ACCEPT_ENCODING, CONTENT_ENCODING, ETAG, IF_NONE_MATCH, USER_AGENT};

    let client = orca_mcp::http::blocking_client()?;
    let mut request = client
        .get(url)
        .timeout(timeout)
        .header(ACCEPT_ENCODING, "gzip")
        .header(USER_AGENT, "orca-pricing");
    if let Some(etag) = previous.and_then(|cache| cache.etag.as_deref()) {
        request = request.header(IF_NONE_MATCH, etag);
    }
    let response = request
        .send()
        .map_err(|error| format!("pricing request failed: {error}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        let mut cache = previous
            .cloned()
            .ok_or_else(|| "pricing source answered 304 with no cached copy".to_string())?;
        cache.fetched_at = now;
        return Ok(RefreshOutcome::NotModified(cache));
    }
    if !status.is_success() {
        return Err(format!("pricing request returned HTTP {status}"));
    }
    let etag = response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let gzip = response
        .headers()
        .get(CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("gzip"));
    let raw = read_limited(response, max_body_bytes)?;
    let body = if gzip {
        read_limited(GzDecoder::new(raw.as_slice()), max_body_bytes)?
    } else {
        raw
    };
    let models = parse_models_dev(&body)?;
    Ok(RefreshOutcome::Updated(PricingCache {
        version: CACHE_VERSION,
        source: url.to_string(),
        fetched_at: now,
        etag,
        models,
    }))
}

fn read_limited(reader: impl Read, max_bytes: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("reading the pricing response failed: {error}"))?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!(
            "pricing response is too large (over {max_bytes} bytes)"
        ));
    }
    Ok(bytes)
}

/// How long a cached copy counts as fresh.
fn refresh_after() -> TimeDelta {
    TimeDelta::hours(24)
}

/// The URL to refresh from now, if any: refresh is on, the build is not a
/// development one (unless a URL is named), and the cache is missing or a day old.
pub fn refresh_url(
    enabled: bool,
    url_override: Option<String>,
    dev_build: bool,
    cached: Option<&PricingCache>,
    now: DateTime<Utc>,
) -> Option<String> {
    if !enabled {
        return None;
    }
    let url_override = url_override.filter(|url| !url.trim().is_empty());
    if url_override.is_none() && dev_build {
        return None;
    }
    if cached.is_some_and(|cache| now.signed_duration_since(cache.fetched_at) < refresh_after()) {
        return None;
    }
    Some(url_override.unwrap_or_else(|| DEFAULT_URL.to_string()))
}

/// Fetches and caches; a cache that cannot be written still counts, so the
/// caller can use the prices for this process.
pub fn refresh_and_store(
    url: &str,
    path: &Path,
    previous: Option<&PricingCache>,
    now: DateTime<Utc>,
) -> RefreshOutcome {
    let outcome = fetch(url, previous, now);
    if let RefreshOutcome::Updated(cache) | RefreshOutcome::NotModified(cache) = &outcome {
        let _ = book::write_cache(path, cache);
    }
    outcome
}

/// Starts at most one background refresh per process, when it is due.
pub(crate) fn ensure_background_refresh(enabled: bool) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if !enabled || STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some(path) = book::cache_path() else {
        return;
    };
    let cached = book::read_cache(&path);
    let Some(url) = refresh_url(
        true,
        std::env::var(URL_ENV).ok(),
        crate::update_check::is_dev_build_run(),
        cached.as_ref(),
        Utc::now(),
    ) else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("orca-pricing-refresh".to_string())
        .spawn(move || {
            if let RefreshOutcome::Updated(cache) | RefreshOutcome::NotModified(cache) =
                refresh_and_store(&url, &path, cached.as_ref(), Utc::now())
            {
                book::install(&cache);
            }
        });
}
```

`pricing/mod.rs` 改为：

```rust
//! Cost estimates at DeepSeek's prices: the built-in table and its
//! time-of-use rule (`rates`), the process-wide price book with its cache
//! (`book`), and the daily refresh from models.dev (`refresh`).

pub mod book;
pub mod rates;
pub mod refresh;
```

- [ ] **Step 5: 运行测试和校验，确认通过**

Run:
- `env -u NODE_OPTIONS cargo test -p orca-runtime --lib pricing:: -- --test-threads=1`
- `env -u NODE_OPTIONS node scripts/test-validate-http-client-boundary.mjs && env -u NODE_OPTIONS node scripts/validate-http-client-boundary.mjs`
- `env -u NODE_OPTIONS cargo clippy -p orca-runtime --all-targets --locked`

Expected: 全部通过。clippy 不能新增警告。若有 `dead_code` 警告（`ensure_background_refresh` 要到任务 5 才有调用方），可以先在这个函数上加 `#[allow(dead_code)]`，任务 5 删掉。

- [ ] **Step 6: 提交**

```bash
cargo fmt --all
git add Cargo.toml Cargo.lock crates/orca-runtime/Cargo.toml crates/orca-runtime/src/pricing
git commit -m "feat(pricing): refresh DeepSeek prices from models.dev" -m "Refs #136"
```

---

### Task 5: `CostTracker` 接入价格表，会话触发刷新

**Files:**
- Modify: `crates/orca-runtime/src/cost.rs`（整体改写，`usd_to_micros` 保持不变）
- Modify: `crates/orca-runtime/src/session.rs`（`new_with_prepared_history_and_runtime_id`：触发刷新；`cost_tracker:` 一行）
- Modify: `crates/orca-runtime/src/pricing/refresh.rs`、`crates/orca-runtime/src/pricing/book.rs`（删掉任务 3、4 临时加在 `install` 和 `ensure_background_refresh` 上的 `#[allow(dead_code)]`）
- Modify: 写死旧价格的测试：
  - `crates/orca-runtime/src/child_agent_tests.rs`（约 320–332 行）
  - `crates/orca-runtime/src/subagent_execution.rs`（`sync_subagent_receives_only_remaining_aggregate_budget`）
  - `tests/runtime_lifecycle_contract.rs`（`task_actor_routes_model_turn_and_updates_cost_model`）

**Interfaces:**
- Consumes：Task 1 的 `rates::{cost_usd, time_of_use_multiplier, Rates, off_peak_instant, peak_instant}`；Task 3 的 `book::current`；Task 4 的 `refresh::ensure_background_refresh`；Task 2 的 `ModelPriceOverride`、`ModelSelection::price_overrides`、`RunConfig.pricing_refresh`
- Produces：
  - `pub type PricingClock = fn() -> DateTime<Utc>`
  - `CostTracker::new(Option<&str>)`：签名不变
  - `CostTracker::with_overrides(Option<&str>, &BTreeMap<String, ModelPriceOverride>) -> CostTracker`
  - `CostTracker::with_clock(self, PricingClock) -> CostTracker`
  - `add_usage`、`set_model`、`merge`、`merge_totals`、`totals` 的签名不变

- [ ] **Step 1: 写失败的测试**

`crates/orca-runtime/src/cost.rs` 的测试模块整体替换为：

```rust
#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use orca_core::model::{
        FLASH_MODEL, LEGACY_FLASH_MODEL, LEGACY_VISION_MODEL, ModelPriceOverride, PRO_MODEL,
        UsdPerMillion,
    };

    use super::*;
    use crate::pricing::rates::{off_peak_instant, peak_instant};

    fn usage(input_tokens: u64, output_tokens: u64, cache_tokens: u64) -> Usage {
        Usage {
            input_tokens,
            output_tokens,
            cache_tokens,
        }
    }

    fn cost(tracker: CostTracker, usage: Usage) -> f64 {
        let mut tracker = tracker;
        tracker.add_usage(usage).estimated_cost_usd
    }

    #[test]
    fn accumulates_tokens_and_off_peak_flash_cost() {
        let mut tracker = CostTracker::new(Some(FLASH_MODEL)).with_clock(off_peak_instant);
        let totals = tracker.add_usage(usage(120, 30, 10));
        assert_eq!(totals.input_tokens, 120);
        assert_eq!(totals.output_tokens, 30);
        assert_eq!(totals.cache_tokens, 10);
        assert_eq!(totals.total_tokens(), 150);
        let expected = (110.0 * 0.15 + 10.0 * 0.003 + 30.0 * 0.6) / 1_000_000.0;
        assert!((totals.estimated_cost_usd - expected).abs() < 1e-12);
    }

    #[test]
    fn peak_hours_double_the_estimate() {
        let used = usage(1_000_000, 1_000_000, 100_000);
        let off_peak = cost(CostTracker::new(Some(FLASH_MODEL)).with_clock(off_peak_instant), used);
        let peak = cost(CostTracker::new(Some(FLASH_MODEL)).with_clock(peak_instant), used);
        assert!((peak - 2.0 * off_peak).abs() < 1e-12);
    }

    #[test]
    fn merge_accumulates_from_child_tracker() {
        let mut parent = CostTracker::new(Some(FLASH_MODEL)).with_clock(off_peak_instant);
        parent.add_usage(usage(100, 50, 20));
        let mut child = CostTracker::new(Some(LEGACY_VISION_MODEL)).with_clock(off_peak_instant);
        child.add_usage(usage(200, 80, 30));
        let parent_cost_before = parent.totals().estimated_cost_usd;
        let child_cost = child.totals().estimated_cost_usd;

        parent.merge(&child);

        assert_eq!(parent.totals().input_tokens, 300);
        assert_eq!(parent.totals().output_tokens, 130);
        assert_eq!(parent.totals().cache_tokens, 50);
        assert!(
            (parent.totals().estimated_cost_usd - (parent_cost_before + child_cost)).abs() < 1e-12
        );
    }

    #[test]
    fn retired_flash_aliases_use_canonical_flash_pricing() {
        let used = usage(1_000_000, 1_000_000, 100_000);
        let canonical = cost(CostTracker::new(Some(FLASH_MODEL)).with_clock(off_peak_instant), used);
        for alias in [LEGACY_FLASH_MODEL, LEGACY_VISION_MODEL] {
            assert_eq!(
                cost(CostTracker::new(Some(alias)).with_clock(off_peak_instant), used),
                canonical
            );
        }
    }

    #[test]
    fn experimental_pro_names_keep_pro_pricing_and_unknown_models_use_flash() {
        let used = usage(900_000, 100_000, 200_000);
        let at = |model| cost(CostTracker::new(Some(model)).with_clock(off_peak_instant), used);
        assert_eq!(at("deepseek-v4-pro-exp"), at(PRO_MODEL));
        assert_eq!(at("qwen-max"), at(FLASH_MODEL));
        assert_eq!(
            cost(CostTracker::new(None).with_clock(off_peak_instant), used),
            at(FLASH_MODEL)
        );
        assert!(at(PRO_MODEL) > at(FLASH_MODEL));
    }

    #[test]
    fn set_model_switches_the_prices_for_later_usage() {
        let used = usage(1_000, 1_000, 0);
        let mut tracker = CostTracker::new(Some(PRO_MODEL)).with_clock(off_peak_instant);
        tracker.set_model(Some(FLASH_MODEL));
        let expected = (1_000.0 * 0.15 + 1_000.0 * 0.6) / 1_000_000.0;
        assert!((tracker.add_usage(used).estimated_cost_usd - expected).abs() < 1e-12);
    }

    fn override_prices(miss: f64, hit: f64, output: f64) -> BTreeMap<String, ModelPriceOverride> {
        let price = |value| UsdPerMillion::new(value).unwrap();
        BTreeMap::from([(
            "my-model".to_string(),
            ModelPriceOverride {
                input_cache_miss: price(miss),
                input_cache_hit: price(hit),
                output: price(output),
            },
        )])
    }

    #[test]
    fn overrides_use_fixed_prices_at_every_hour() {
        let overrides = override_prices(1.0, 0.5, 2.0);
        let used = usage(1_000_000, 1_000_000, 500_000);
        for clock in [off_peak_instant as PricingClock, peak_instant] {
            let tracker = CostTracker::with_overrides(Some("my-model"), &overrides).with_clock(clock);
            assert!((cost(tracker, used) - (0.5 + 0.25 + 2.0)).abs() < 1e-12);
        }
        // Models without an override keep DeepSeek's prices.
        let flash = CostTracker::with_overrides(Some(FLASH_MODEL), &overrides).with_clock(off_peak_instant);
        let plain = CostTracker::new(Some(FLASH_MODEL)).with_clock(off_peak_instant);
        assert_eq!(cost(flash, used), cost(plain, used));
    }

    #[test]
    fn a_zero_price_override_costs_nothing_at_peak() {
        let overrides = override_prices(0.0, 0.0, 0.0);
        let tracker = CostTracker::with_overrides(Some("my-model"), &overrides).with_clock(peak_instant);
        assert_eq!(cost(tracker, usage(5_000_000, 2_000_000, 1_000_000)), 0.0);
    }

    #[test]
    fn usd_to_micros_rounds_and_rejects_invalid_values() {
        assert_eq!(usd_to_micros(0.000_001_5), 2);
        assert_eq!(usd_to_micros(0.0), 0);
        assert_eq!(usd_to_micros(-0.1), 0);
        assert_eq!(usd_to_micros(f64::NAN), 0);
    }
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `env -u NODE_OPTIONS cargo test -p orca-runtime --lib cost::tests -- --test-threads=1`
Expected: 编译失败（`with_clock`、`with_overrides`、`PricingClock` 未定义）。

- [ ] **Step 3: 实现 `CostTracker`**

`cost.rs` 测试模块以上替换为（`usd_to_micros` 原样保留）：

```rust
use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use orca_core::cost_types::UsageTotals;
use orca_core::model::{ModelPriceOverride, canonical_model_name};
use orca_core::provider_types::Usage;

use crate::pricing::book;
use crate::pricing::rates::{self, Rates};

pub(crate) fn usd_to_micros(usd: f64) -> u64 {
    if usd.is_finite() && usd > 0.0 {
        (usd * 1_000_000.0).round().min(u64::MAX as f64) as u64
    } else {
        0
    }
}

/// When usage is priced: DeepSeek's peak hours cost twice as much. Tests pin it.
pub type PricingClock = fn() -> DateTime<Utc>;

#[derive(Clone, Debug)]
pub struct CostTracker {
    totals: UsageTotals,
    /// Canonical name of the model the next usage is billed to.
    model: Option<String>,
    /// Fixed prices from `[models."<name>".pricing]`, by canonical name.
    overrides: Arc<BTreeMap<String, Rates>>,
    clock: PricingClock,
}

impl CostTracker {
    pub fn new(model: Option<&str>) -> Self {
        Self {
            totals: UsageTotals::default(),
            model: canonical(model),
            overrides: Arc::default(),
            clock: Utc::now,
        }
    }

    /// A tracker that bills the models in `overrides` at their fixed prices.
    pub fn with_overrides(
        model: Option<&str>,
        overrides: &BTreeMap<String, ModelPriceOverride>,
    ) -> Self {
        let overrides = overrides
            .iter()
            .map(|(model, price)| {
                (
                    model.clone(),
                    Rates {
                        input_cache_miss: price.input_cache_miss.get(),
                        input_cache_hit: price.input_cache_hit.get(),
                        output: price.output.get(),
                    },
                )
            })
            .collect();
        Self {
            overrides: Arc::new(overrides),
            ..Self::new(model)
        }
    }

    pub fn with_clock(mut self, clock: PricingClock) -> Self {
        self.clock = clock;
        self
    }

    pub fn add_usage(&mut self, usage: Usage) -> UsageTotals {
        self.totals.input_tokens += usage.input_tokens;
        self.totals.output_tokens += usage.output_tokens;
        self.totals.cache_tokens += usage.cache_tokens;
        self.totals.estimated_cost_usd += self.estimate(usage);
        self.totals
    }

    /// DeepSeek bills cache-miss input, cache-hit input, and output tokens
    /// separately; peak hours double its prices, never an override's.
    fn estimate(&self, usage: Usage) -> f64 {
        let model = self.model.as_deref().unwrap_or("");
        if let Some(rates) = self.overrides.get(model) {
            return rates::cost_usd(*rates, 1.0, usage);
        }
        let rates = book::current().rates_for(model);
        rates::cost_usd(rates, rates::time_of_use_multiplier((self.clock)()), usage)
    }

    pub fn set_model(&mut self, model: Option<&str>) {
        self.model = canonical(model);
    }
```

后面的 `merge`、`merge_totals`、`totals` 方法体照搬原文件，最后加：

```rust
fn canonical(model: Option<&str>) -> Option<String> {
    model.map(|model| canonical_model_name(model).to_string())
}
```

删掉旧的 `ModelPricing` 结构体及其 `impl`，以及对 `PRO_MODEL` 的引用。

- [ ] **Step 4: 会话接入**

`crates/orca-runtime/src/session.rs` 的 `new_with_prepared_history_and_runtime_id`，在函数体第一行（`let cwd = ...` 之前）加：

```rust
        crate::pricing::refresh::ensure_background_refresh(config.pricing_refresh);
```

同一个函数构造 `Self` 时，把 `cost_tracker: CostTracker::new(None),` 换成：

```rust
            cost_tracker: CostTracker::with_overrides(None, &config.model.price_overrides()),
```

删掉任务 3、4 临时加在 `book::install` 和 `refresh::ensure_background_refresh` 上的 `#[allow(dead_code)]`。

- [ ] **Step 5: 修正写死旧价格的测试**

处理原则：固定时钟，按新价格重算期望值。不能放宽断言，也不能删掉断言。

`crates/orca-runtime/src/child_agent_tests.rs`（约 320 行）：

```rust
    let mut tracker = CostTracker::new(Some(orca_core::model::PRO_MODEL))
        .with_clock(crate::pricing::rates::off_peak_instant);
```

期望值改为：

```rust
    let expected_flash_cost = (1_000.0 * 0.15 + 1_000.0 * 0.6) / 1_000_000.0;
```

`crates/orca-runtime/src/subagent_execution.rs` 的 `sync_subagent_receives_only_remaining_aggregate_budget`：

```rust
        let mut cost_tracker =
            CostTracker::new(None).with_clock(crate::pricing::rates::off_peak_instant);
```

注释和断言改为：

```rust
        // 1M flash input tokens cost $0.15 off-peak, leaving $0.35 of the $0.50 budget.
        let remaining = child_config
            .budget
            .max_cost_usd_micros
            .expect("remaining budget");
        assert!((remaining as f64 - 350_000.0).abs() < 2_000.0);
```

`tests/runtime_lifecycle_contract.rs` 的 `task_actor_routes_model_turn_and_updates_cost_model`：

```rust
    let mut cost_tracker =
        CostTracker::new(Some(PRO_MODEL)).with_clock(orca_runtime::pricing::rates::off_peak_instant);
```

期望值改为：

```rust
    let expected_flash_cost = (100.0 * 0.15 + 50.0 * 0.6) / 1_000_000.0;
```

然后跑下面 Step 6 的全部命令。再有测试因为旧价格失败，用同样的方法修，并在最终报告里逐个列出。

- [ ] **Step 6: 运行测试，确认通过**

Run:
- `env -u NODE_OPTIONS cargo test -p orca-runtime --lib cost:: -- --test-threads=1`
- `env -u NODE_OPTIONS cargo test -p orca-runtime --lib -- --test-threads=1`（memory 里记录过 `--lib` 有已知的 ORCA_HOME 竞争，所以要串行跑）
- `env -u NODE_OPTIONS cargo test --test runtime_lifecycle_contract --test exec_jsonl --test execution_budget_contract --test history_contract`
- `env -u NODE_OPTIONS cargo test -p orca-runtime --test budget_lease_contract -- --test-threads=1`
- `env -u NODE_OPTIONS cargo clippy -p orca-runtime --all-targets --locked`

Expected: 全部通过，clippy 没有新增警告。

- [ ] **Step 7: 提交**

```bash
cargo fmt --all
git add -A crates tests
git commit -m "feat(cost): price usage from the refreshed DeepSeek price book" -m "Refs #136"
```

---

### Task 6: `orca doctor` 显示价格来源，补二进制级集成测试

**Files:**
- Modify: `crates/orca-runtime/src/diagnostics.rs`（新增 `check_pricing`，并在 `collect_doctor_with_version` 里调用）
- Modify: `tests/doctor_cli_contract.rs`（新增测试）
- Create: `tests/pricing_contract.rs`

**Interfaces:**
- Consumes：Task 3 的 `book::{cache_path, read_cache, merge, PriceSource}`；Task 1 的 `rates::is_peak`；Task 2 的 `FileConfig.pricing.refresh`
- Produces：doctor 增加一项 check：`id = "pricing"`，`status = "pass"`，`detail` 的格式为：
  - `built-in prices as of 2026-10-19; daily refresh on; DeepSeek off-peak hours now`
  - 或 `<source> prices fetched <RFC3339 秒级 UTC>; refresh off; DeepSeek peak hours now`

- [ ] **Step 1: 写失败的测试**

`tests/doctor_cli_contract.rs` 追加：

```rust
#[test]
fn doctor_reports_which_prices_estimates_use() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("workspace");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&cwd).unwrap();
    let pricing = |output: std::process::Output| -> Value {
        let report: Value = serde_json::from_slice(&output.stdout).expect("doctor JSON");
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["id"] == "pricing")
            .cloned()
            .expect("pricing check")
    };

    let builtin = pricing(run_doctor(&home, &cwd, &["--format", "json"]));
    assert_eq!(builtin["status"], "pass");
    let detail = builtin["detail"].as_str().unwrap();
    assert!(detail.starts_with("built-in prices as of 2026-10-19; daily refresh on; DeepSeek "), "{detail}");

    fs::write(
        home.join("pricing-cache.json"),
        r#"{"version":1,"source":"https://models.dev/api.json","fetched_at":"2026-10-20T00:00:00Z","models":{"deepseek-flash":{"input_cache_miss":0.2,"input_cache_hit":0.004,"output":0.8}}}"#,
    )
    .unwrap();
    fs::write(home.join("config.toml"), "[pricing]\nrefresh = false\n").unwrap();
    let fetched = pricing(run_doctor(&home, &cwd, &["--format", "json"]));
    let detail = fetched["detail"].as_str().unwrap();
    assert!(
        detail.starts_with("https://models.dev/api.json prices fetched 2026-10-20T00:00:00Z; refresh off; DeepSeek "),
        "{detail}"
    );
}
```

`tests/pricing_contract.rs`：

```rust
//! An `orca exec` run prices usage from the cache in its ORCA_HOME, falling
//! back to the built-in table. The clock is real, so each expectation allows
//! DeepSeek's off-peak price or its doubled peak price.

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

/// `mock_usage` reports 120 input tokens (10 cached) and 30 output tokens on
/// the default model, `deepseek-flash`.
fn mock_usage_cost(home: &Path) -> f64 {
    let output = Command::new(env!("CARGO_BIN_EXE_orca"))
        .env("ORCA_HOME", home)
        .env_remove("ORCA_PRICING_URL")
        .args(["exec", "--output-format", "jsonl", "--provider", "mock", "mock_usage"])
        .stdin(Stdio::null())
        .output()
        .expect("run orca exec");
    assert_eq!(output.status.code(), Some(0), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["type"] == "usage.updated")
        .last()
        .expect("usage event")["payload"]["estimated_cost_usd"]
        .as_f64()
        .unwrap()
}

fn at_either_tier(cost: f64, off_peak: f64) {
    assert!(
        (cost - off_peak).abs() < 1e-12 || (cost - 2.0 * off_peak).abs() < 1e-12,
        "cost {cost} is neither {off_peak} nor its peak double"
    );
}

#[test]
fn exec_without_a_cache_uses_the_builtin_prices() {
    let home = tempfile::tempdir().unwrap();
    let off_peak = (110.0 * 0.15 + 10.0 * 0.003 + 30.0 * 0.6) / 1_000_000.0;
    at_either_tier(mock_usage_cost(home.path()), off_peak);
    assert!(!home.path().join("pricing-cache.json").exists(), "a development build must not refresh");
}

#[test]
fn exec_prices_usage_from_a_current_cache() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("pricing-cache.json"),
        r#"{"version":1,"source":"https://models.dev/api.json","fetched_at":"2026-10-20T00:00:00Z","models":{"deepseek-flash":{"input_cache_miss":0.2,"input_cache_hit":0.004,"output":0.8}}}"#,
    )
    .unwrap();
    let off_peak = (110.0 * 0.2 + 10.0 * 0.004 + 30.0 * 0.8) / 1_000_000.0;
    at_either_tier(mock_usage_cost(home.path()), off_peak);
}

#[test]
fn exec_ignores_a_corrupt_cache() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("pricing-cache.json"), b"{\"version\":1,\"source\":").unwrap();
    let off_peak = (110.0 * 0.15 + 10.0 * 0.003 + 30.0 * 0.6) / 1_000_000.0;
    at_either_tier(mock_usage_cost(home.path()), off_peak);
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `env -u NODE_OPTIONS cargo test --test doctor_cli_contract --test pricing_contract`
Expected：
- `doctor_reports_which_prices_estimates_use` 失败（没有 pricing 这一项）；
- 如果任务 5 已经完成，`pricing_contract` 的三个测试应当已经通过，它们是任务 5 的回归保护，这一点要在报告里写明；
- 如果任务 5 还没做，`exec_prices_usage_from_a_current_cache` 失败。

- [ ] **Step 3: 实现 doctor 检查**

`crates/orca-runtime/src/diagnostics.rs`：`collect_doctor_with_version` 在 `checks.push(check_model(effective_cwd, config_is_usable));` 之后加：

```rust
    checks.push(check_pricing(effective_cwd, config_is_usable));
```

在 `check_model` 之后加：

```rust
/// Which prices cost estimates use and whether they refresh. Read-only: it
/// never fetches or writes the cache.
fn check_pricing(cwd: &Path, config_is_usable: bool) -> DiagnosticCheck {
    use crate::pricing::book::{self, PriceSource};

    let cache = book::cache_path().and_then(|path| book::read_cache(&path));
    let source = match book::merge(cache.as_ref()).source {
        PriceSource::Builtin { as_of } => format!("built-in prices as of {as_of}"),
        PriceSource::Fetched { source, fetched_at } => format!(
            "{source} prices fetched {}",
            fetched_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ),
    };
    let refresh = if !config_is_usable {
        "refresh not evaluated"
    } else if file::load_effective_config(cwd, ConfigOverrides::default())
        .map(|config| config.pricing.refresh)
        .unwrap_or(true)
    {
        "daily refresh on"
    } else {
        "refresh off"
    };
    let tier = if crate::pricing::rates::is_peak(chrono::Utc::now()) {
        "peak"
    } else {
        "off-peak"
    };
    pass_check(
        "pricing",
        format!("{source}; {refresh}; DeepSeek {tier} hours now"),
    )
}
```

- [ ] **Step 4: 运行测试，确认通过**

Run:
- `env -u NODE_OPTIONS cargo test --test doctor_cli_contract --test pricing_contract`
- `env -u NODE_OPTIONS cargo test -p orca-runtime --lib diagnostics -- --test-threads=1`

Expected: 全部通过。`doctor_is_a_real_read_only_command_with_stable_json` 仍然通过，说明 doctor 依旧只读，不写任何文件。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add crates/orca-runtime/src/diagnostics.rs tests/doctor_cli_contract.rs tests/pricing_contract.rs
git commit -m "feat(doctor): report the prices cost estimates use" -m "Refs #136"
```

---

### Task 7: 文档和发版清单

**Files:**
- Modify: `README.md`（模型段落之后，约 227 行，即 "or use `ORCA_REASONING_EFFORT`." 那段之后）
- Modify: `README.zh-CN.md`（模型段落之后，约 140 行）
- Modify: `site/src/docs/md/en/models.mdx`、`site/src/docs/md/zh/models.mdx`（新增一节）
- Modify: `site/src/docs/md/en/configuration.mdx`、`site/src/docs/md/zh/configuration.mdx`（在 "Minimal user configuration" / 对应中文节之后新增一节）
- Modify: `docs/release-process.md`（"### 4. Run pre-release checks" 一节的末尾）

- [ ] **Step 1: README**

`README.md` 加一段：

```markdown
Cost estimates follow DeepSeek's published prices, including the peak rate
that applies on weekdays from 01:00 to 04:00 and from 06:00 to 10:00 UTC
(Chinese public holidays are off-peak all day). Orca ships with the prices
current at release and refreshes them from [models.dev](https://models.dev) at
most once a day in the background; set `refresh = false` under `[pricing]` in
`~/.orca/config.toml` to keep it offline. To price a model yourself, for
example one served by another endpoint, set `input_cache_miss`,
`input_cache_hit`, and `output` (US dollars per million tokens) under
`[models."<name>".pricing]`; those prices apply at every hour. Both settings
are read only from the user configuration.
```

`README.zh-CN.md` 加一段：

```markdown
成本估算按 DeepSeek 公布的价格计算，包括工作日北京时间 9:00–12:00 和 14:00–18:00 的高峰价（中国法定节假日全天按空闲价）。Orca 自带发布时的价格，并在后台每天最多一次从 [models.dev](https://models.dev) 刷新；在 `~/.orca/config.toml` 的 `[pricing]` 里设置 `refresh = false` 就不会联网刷新。要自己给某个模型定价（比如通过其他 endpoint 调用的模型），在 `[models."<名称>".pricing]` 里设置 `input_cache_miss`、`input_cache_hit` 和 `output`（美元 / 百万 tokens），这个价格不分时段。这两项都只在用户配置里生效。
```

- [ ] **Step 2: site 文档**

在 `site/src/docs/md/en/models.mdx` 的 "## Default model" 之前插入：

````markdown
## Cost estimates

The cost in the status bar and in `usage.updated` events, which
`--max-cost-usd` also uses, is an estimate in US dollars. It follows
DeepSeek's published prices: cache-miss input, cache-hit input, and output
tokens are priced separately, and the peak rate (twice the off-peak rate)
applies on weekdays from 09:00 to 12:00 and from 14:00 to 18:00 Beijing time,
except on Chinese public holidays.

Orca ships with the prices current at release and refreshes them from
models.dev at most once a day in the background. Run `orca doctor` to see which
prices are in use. To keep Orca offline, or to price a model yourself:

~~~toml
[pricing]
refresh = false

[models."my-model".pricing]
input_cache_miss = 0.5   # US dollars per million tokens
input_cache_hit = 0.05
output = 1.5
~~~

A model priced this way costs the same at every hour. Both settings are read
only from the user configuration; a project's `.orca/config.toml` cannot
change them.
````

`site/src/docs/md/zh/models.mdx` 在 "## 默认模型" 之前插入：

````markdown
## 成本估算

状态栏和 `usage.updated` 事件里的成本是按美元估算的，`--max-cost-usd` 也用这个值。估算按 DeepSeek 公布的价格：输入（缓存未命中）、输入（缓存命中）和输出分别计价；工作日北京时间 9:00–12:00 和 14:00–18:00 为高峰时段，价格是空闲时段的两倍，中国法定节假日除外。

Orca 自带发布时的价格，并在后台每天最多一次从 models.dev 刷新。运行 `orca doctor` 可以查看当前使用的价格。不想联网刷新，或者想自己给模型定价：

~~~toml
[pricing]
refresh = false

[models."my-model".pricing]
input_cache_miss = 0.5   # 美元 / 百万 tokens
input_cache_hit = 0.05
output = 1.5
~~~

这样定价的模型不分时段。这两项都只在用户配置里生效，项目的 `.orca/config.toml` 不能修改。
````

`site/src/docs/md/en/configuration.mdx` 在 "## Computer use (macOS)" 之前插入：

````markdown
## Cost estimates

Cost estimates follow DeepSeek's published peak and off-peak prices and refresh
from models.dev at most once a day. Both settings belong in the user
configuration; project `.orca/config.toml` cannot set them:

```toml
[pricing]
refresh = false            # do not contact models.dev

[models."my-model".pricing]
input_cache_miss = 0.5     # US dollars per million tokens
input_cache_hit = 0.05
output = 1.5
```

A `[models."<name>".pricing]` table needs all three prices and applies at every
hour. See [Models](models.md#cost-estimates) for how estimates are made.
````

`site/src/docs/md/zh/configuration.mdx` 在 "## Computer use（macOS）" 之前插入：

````markdown
## 成本估算

成本估算按 DeepSeek 公布的高峰和空闲价格计算，每天最多从 models.dev 刷新一次。这两项只能写在用户配置里，项目的 `.orca/config.toml` 不能设置：

```toml
[pricing]
refresh = false            # 不访问 models.dev

[models."my-model".pricing]
input_cache_miss = 0.5     # 美元 / 百万 tokens
input_cache_hit = 0.05
output = 1.5
```

`[models."<名称>".pricing]` 必须写全三个价格，且不分时段。估算方法见[模型](models.md#成本估算)。
````

（中文页面的站内链接直接用中文锚点，例如 `session-commands.md#存储与清理`，所以这里写 `models.md#成本估算`。）

- [ ] **Step 3: 发版清单**

在 `docs/release-process.md` 的 "### 4. Run pre-release checks" 一节末尾（"A manual `workflow_dispatch` ..." 那段之后）加：

```markdown
Check the built-in prices before each release: compare `FLASH_RATES`,
`PRO_RATES`, and the peak hours in `crates/orca-runtime/src/pricing/rates.rs`
with https://api-docs.deepseek.com/quick_start/pricing, and move
`builtin_as_of` to the day you checked. After the State Council publishes the
next year's holiday schedule (usually in November), add that year's weekday
days off to `WEEKDAY_HOLIDAYS`.
```

- [ ] **Step 4: 验证**

Run:
- `env -u NODE_OPTIONS cargo test --test public_docs_contract`
- `env -u NODE_OPTIONS npm --prefix site run build`
- `git diff --check`

Expected: 全部通过。网站能正常构建，新章节没有破坏 MDX。

- [ ] **Step 5: 提交**

```bash
git add README.md README.zh-CN.md site/src/docs/md docs/release-process.md
git commit -m "docs: explain peak-hour cost estimates and the pricing settings" -m "Refs #136"
```

- [ ] **Step 6: 给下一版 release notes 准备的要点（写进最终报告，不提交）**

- 成本估算改用 DeepSeek 当前官方价，区分高峰（工作日北京时间 9–12、14–18 点，×2）和空闲。输出价比原来的估算高一倍以上，高峰时再翻倍，所以同样的用量估算更高，`max_cost_usd` 预算会更早触发。
- Orca 每天最多访问一次 `https://models.dev/api.json` 来刷新价格；在 `[pricing]` 里设 `refresh = false` 可以关闭。
- 新增 `[models."<name>".pricing]`，可以给模型设固定价格。
- `orca doctor` 会显示当前使用的价格来源。

---

## 全部任务完成后

- `cargo fmt --all -- --check`
- `env -u NODE_OPTIONS cargo clippy --workspace --all-targets --locked`：不能新增警告
- 9 个 CI node 校验脚本，见 `.github/workflows/runtime-contract.yml` 的 "Validate reviewed artifacts" 和 "Validate repository adapters"
- `env -u NODE_OPTIONS cargo nextest run --workspace --all-targets --locked --profile ci --no-fail-fast`：已知的并行偶发失败和 main 上已有的失败要单独复核（见 #137–#140 的验证记录）
- `env -u NODE_OPTIONS cargo nextest run -p orca-tui --lib --locked --profile ci-serial`
- 手工验证：
  - 用 `ORCA_PRICING_URL` 指向本地 fixture，`ORCA_HOME` 用临时目录，跑一次 `orca exec`，确认 `pricing-cache.json` 被写出；
  - 再用 release 构建，在真实网络下跑一次，确认能从 models.dev 拿到价格，`orca doctor` 显示 `https://models.dev/api.json prices fetched ...`；这一步会真实访问 models.dev。
- 做一次只读的最终审查（Codex `-s read-only`）

## 执行中的调整

- Batch B：项目配置拒绝规则剥离 `pricing` 后，同时移除变空的模型别名定义，避免影响用户配置的别名合并。
- `ceb82cf4`：models.dev 改为宽松解析，跳过字段缺失或格式错误的模型条目，其余有效价格仍可刷新。
- 用户价格覆盖改为进程级保存，每个会话启动时写入，之后创建的所有 tracker（含子代理和挂起 / 后台结算）均使用其快照。
- 库测试的空闲时钟统一由 `CostTracker::new` 的 `default_clock` 提供，移除会话和后台结算中的分散注入；集成测试继续显式固定时钟或接受两档价格。

- Batch E1：项目层丢弃所有非 table 的模型定义，阻止数组形式绕过价格拒绝规则，并保留 table 中允许的字段。
- Batch E2：价格覆盖改在每个进程的 `build_run_config` 安装，覆盖 detached worker，移除会话构造时对全局覆盖的写入；长进程使用最近加载的用户配置。
- Batch E3：用户覆盖价格上限设为每百万 tokens 10,000 美元；正无穷成本转换为 `u64::MAX`，避免预算溢出后按零收费。
- Batch E4：缓存升为 v2，按每模型 `last_updated` 内容日期合并而非抓取时间；宽松回退日期，忽略 v1，来源仅在缓存条目实际生效时标为 Fetched。
- Batch E5：缓存新鲜度和 ETag 仅用于相同 source URL，切换来源时无条件抓取且替换缓存。
- Batch E6：未知模型的抓取与缓存价格按 Flash/Pro 每字段范围做 ±10 倍校验，拒绝零价和单位缩放错误，用户覆盖不受此校验影响。
- Batch E7：读取缓存时传入当前时间并忽略超前一天以上的抓取时间，新鲜度限定为非负且不足 24 小时；规格记录 release 自定义 target-dir 的联网保护限制。

- Batch F1：可信项目层导致合并配置解析失败时重试用户层，成功则告警并忽略项目配置，保留用户预算和价格覆盖；用户层也无效时仍告警并使用默认配置。
