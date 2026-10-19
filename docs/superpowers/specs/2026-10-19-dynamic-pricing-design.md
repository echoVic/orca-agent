# Orca 成本估算：动态价格设计（#136）

> 状态：2026-10-19 设计确认（方案 M：拉 models.dev，分时规则内置），待写实施计划。
> 来源：issue #136。取代 `crates/orca-runtime/src/cost.rs` 里写死的价格表。

## 0. 背景和范围

`cost.rs` 里的价格已经过时，而且不区分高峰和空闲时段。官方价格页
（https://api-docs.deepseek.com/quick_start/pricing ，2026-10-19 抓取）的价格如下，单位为美元 / 百万 tokens：

| 模型 | 档位 | 输入（未命中） | 输入（命中） | 输出 |
| --- | --- | --- | --- | --- |
| `deepseek-flash`（V4.1-Flash-1017） | 空闲 | 0.15 | 0.003 | 0.6 |
| | 高峰 | 0.30 | 0.006 | 1.2 |
| `deepseek-v4-pro`（V4-Pro-0813） | 空闲 | 0.66 | 0.022 | 1.98 |
| | 高峰 | 1.32 | 0.044 | 3.96 |
| 代码里现在的 flash | — | 0.14 | 0.014 | 0.28 |
| 代码里现在的 pro | — | 0.435 | 0.044 | 0.87 |

官方脚注写明：空闲价是高峰价的一半；高峰时段是 UTC 周一至周五 01:00–04:00、06:00–10:00（即北京时间 9:00–12:00、14:00–18:00），中国法定节假日除外；周末和节假日全天都按空闲价。

**数据源的选择。** DeepSeek 没有价格接口：`/models` 只返回上下文、模态、effort 等字段，文档站也没有 `llms.txt` 或 markdown 版本。第三方的 JSON 接口都只给一个价，而且档位不统一：

- **models.dev** `https://models.dev/api.json`：记录的是空闲价。源文件注释写明"models.dev records the off-peak rate"，并说明高峰是空闲的 2 倍。V4.1-Flash-1017 发布后一天就跟进了更新（2026-10-18）。响应 gzip 后 587KB，支持 ETag。从国内直连实测：完整下载 1.8 秒，没有变化时返回 304，耗时 0.7 秒。
- LiteLLM 和 OpenRouter 记录的是高峰价。

决定：用 models.dev 提供价格；高峰系数、时段和节假日内置在 Orca 里。

**范围内：**

- 内置价格表更新为上表的空闲价，并支持高峰 / 空闲计价。
- 后台每天从 models.dev 刷新一次；刷新失败不影响使用。
- 两个新配置：`[models."<name>".pricing]` 覆盖单个模型的价格；`[pricing] refresh = false` 关闭联网刷新。
- `orca doctor` 显示当前使用的价格来源。

**不做：**

- 人民币显示。金额仍是美元，`estimated_cost_usd`、`max_cost_usd` 的语义和单位都不变。
- 按次计费的 Search/Fetch。Orca 的 `web_search` 用的是 Brave，不经过 DeepSeek。
- 重新计算历史会话的成本。
- 改动界面。

## 1. 价格来源

### 1.1 内置表

内置表就是第 0 节表里的空闲价（flash 和 pro 两行），并带一个日期 `BUILTIN_AS_OF = 2026-10-19`。它的用途有三个：首次运行时的价格、刷新失败时的回退、刷新结果的范围校验基准（见 1.2）。每次发版时都要核对它是否与官方页一致（见第 6 节）。

### 1.2 从 models.dev 刷新

**请求：**

- `GET https://models.dev/api.json`。环境变量 `ORCA_PRICING_URL` 可以替换这个地址。
- 请求头：`Accept-Encoding: gzip`、`User-Agent: orca/<版本>`。缓存的 `source` 与请求 URL 相同时才带上 `If-None-Match: <etag>`。
- 客户端必须用 `orca_mcp::http::blocking_client()`，这是 CI 的 http-client 边界校验所要求的。请求在普通 `std::thread` 里发，不在 tokio 上下文里。
- 总超时 15 秒。解压后的内容超过 32 MiB 就放弃。
- 工作区的 reqwest 没开 `gzip` feature，也不为这一个请求打开（打开会影响所有客户端）。这里自己用 `flate2` 解压；`flate2` 已经在依赖树里。

**解析：** 读取 `deepseek.models.<id>.cost` 中的三个字段，以及模型的 `last_updated`：

- `input` → 输入未命中价
- `cache_read` → 输入命中价
- `output` → 输出价

`last_updated` 按 `YYYY-MM-DD` 解析；缺失、格式错误或晚于抓取日期时，使用抓取日期（UTC）。三个价格都当作空闲价使用。模型 id 先经过 `canonical_model_name` 归一；归一后出现重复时，以 canonical id 那条为准。

**校验：** 按模型逐个检查，任何一项不通过就丢弃这个模型。

- 三个字段都必须存在，都是有限数，满足 `0 ≤ 值 ≤ 100`。
- 内置表里有这个模型时，每个值都必须落在 `[内置值 / 10, 内置值 × 10]` 之内。这条用来挡住按每 token、每千 token 这类单位错误。
- 内置表没有这个模型时，每个字段必须落在 `[min(Flash, Pro) / 10, max(Flash, Pro) × 10]` 内。抓取和缓存都应用这些校验，用户覆盖不受此范围限制。
- 没有一个模型通过、`deepseek` 段缺失、或 JSON 无效，都算这次刷新失败。

**结果：**

- **200 且校验通过**：原子写入缓存（1.3），替换进程内的价格表。正在运行的会话从下一次记录 usage 开始用新价。
- **304**：只更新缓存里的 `fetched_at`。
- **其他情况**：什么都不改。不重试，下次进程启动时再试。

### 1.3 缓存

缓存是 `$ORCA_HOME/pricing-cache.json`，用 orca-platform 现有的原子写入 helper 写：

```json
{
  "version": 2,
  "source": "https://models.dev/api.json",
  "fetched_at": "2026-10-19T08:00:00Z",
  "etag": "W/\"657a51e5a18b7553c44cdb8fd04fc5e4\"",
  "models": {
    "deepseek-flash": { "input_cache_miss": 0.15, "input_cache_hit": 0.003, "output": 0.6, "updated": "2026-10-18" },
    "deepseek-v4-pro": { "input_cache_miss": 0.66, "input_cache_hit": 0.022, "output": 1.98, "updated": "2026-08-13" }
  }
}
```

- 只存解析出来的价格和每个模型的内容日期 `updated`，不存 models.dev 的原始文件。v1 缓存没有内容日期，忽略并重新抓取。
- 读缓存时重新做一遍 1.2 的校验。文件损坏、版本不认识、`fetched_at` 比当前时间晚超过一天、或校验不通过，都当作没有缓存。
- 多个 Orca 进程可能同时刷新。写入是原子的，最后写的那个生效，没有问题。

### 1.4 生效的价格表

进程内有一张全局价格表，第一次计价时懒加载，规则如下：

1. 以内置表为底。
2. 缓存有效时逐个比较模型的 `updated` 和 `BUILTIN_AS_OF`：内容日期不早于内置日期的模型覆盖同名内置模型；更旧的内容保留内置价。缓存里有、内置表里没有的模型始终加入。
3. 至少一个缓存模型生效（覆盖内置模型或补充新模型）时，来源是 `Fetched`；否则是 `Builtin`。`fetched_at` 只用于刷新时效和 doctor 的抓取时间，不决定价格优先级；304 或重复下载旧内容都不会让旧价格重新覆盖内置价。
4. 刷新成功后，按同样的规则重建这张表，然后整张替换（`RwLock<Arc<…>>`）。

### 1.5 什么时候刷新

- 一个进程第一次创建会话时（`InteractiveSession` 的内部构造函数，所有入口都经过这里），会检查一次是否需要刷新，用进程级 `Once` 保证只检查一次。
- 只有同时满足以下条件才刷新：配置 `pricing.refresh` 为 true（默认就是 true）；当前 URL 没有缓存，或年龄不在 `[0, 24 小时)` 内（未来时间戳也必须刷新）。不同来源的缓存不影响新 URL 的刷新时效，也不向新来源发送旧 ETag。
- 开发构建不刷新，判断方式和 update check 的 `is_dev_build_run()` 相同（debug 构建，或可执行文件在 `target/` 下）。例外是设置了 `ORCA_PRICING_URL`：测试用它把请求指到本地 fixture。因此测试永远不会访问外网，也不会写 `~/.orca`。
- 刷新在后台线程里进行，不阻塞启动，也不阻塞任何 turn。进程在刷新完成前退出也没关系，下次再试。
- 子代理 worker 也会走到这里。缓存通常已经被父进程刷新过，所以 worker 一般不会去拉。

## 2. 计价规则

### 2.1 时段

把记录 usage 那一刻的 UTC 时间换算成北京时间（UTC+8，没有夏令时）：

- **高峰**：北京时间周一至周五 [09:00, 12:00) 和 [14:00, 18:00)，并且当天不在节假日表里。这与官方英文页写的 UTC 01–04、06–10 是同一个时段。
- **空闲**：其余所有时间，包括周末全天、节假日全天。
- **调休上班的周末**：按官方的字面规则（只有周一至周五有高峰）仍算空闲。

高峰时价格乘 `PEAK_MULTIPLIER = 2.0`。

### 2.2 节假日表

- 内置表只收周一至周五落在放假期间的日期（周末本来就是空闲）。数据来自国务院每年的放假安排通知。
- 现在只收 2026 年的 19 天。2026 年通知是 https://www.gov.cn/zhengce/zhengceku/202511/content_7047091.htm ，日期为：
  - 1 月 1、2 日；
  - 2 月 16–20、23 日；
  - 4 月 6 日；
  - 5 月 1、4、5 日；
  - 6 月 19 日；
  - 9 月 25 日；
  - 10 月 1、2、5、6、7 日。
- 这些日期都已经过去，只用于测试。
- 2027 年的安排一般在 11 月公布。公布后由下一个版本补进来，这一项写进发版清单（第 6 节）。
- **假设**：官方说的"中国法定节假日"是指放假通知里整段的休息日，不只是严格意义上的法定假日。

### 2.3 公式

公式与现在相同，最后乘上时段系数：

```
cost = ((input − cache) × 未命中价 + cache × 命中价 + output × 输出价) × 系数 / 1_000_000
```

`cache_tokens` 是 `input_tokens` 的子集。输出 token 里包含 reasoning，按输出价计费。

### 2.4 模型匹配

按以下顺序查找，命中即停：

1. 会话配置里的价格覆盖（2.5）。
2. 生效价格表（1.4）中的精确匹配，匹配前先做 `canonical_model_name` 归一。
3. 名字里含 `v4-pro` 的，按 `deepseek-v4-pro` 计价（保持现有行为，例如 `deepseek-v4-pro-exp`）。
4. 其余按 `deepseek-flash` 计价（保持现有行为：未知模型和省略模型时都按 flash）。

### 2.5 配置覆盖

```toml
[models."my-model".pricing]
input_cache_miss = 0.5   # 美元 / 百万 tokens
input_cache_hit = 0.05
output = 1.5
```

- 三个字段都必须写，都是有限数且在 `[0, 10_000]` 美元 / 百万 tokens 内，不合法时配置加载报错。
- 覆盖的价格是固定价，不乘高峰系数。覆盖多半用于第三方 endpoint，那里的规则与 DeepSeek 官方不同。
- `ModelDefinition` 目前 derive 了 `Copy`/`Eq`，并参与别名冲突检查。价格字段要用一个在解析阶段就拒绝 NaN 的新类型（例如 `UsdPerMillion`）来保存，这样 `Eq` 依然成立；别名冲突检查会自然覆盖到价格字段。
- 价格覆盖只来自用户配置，因此保存在进程级状态里；每个进程构建运行配置时写入，所有入口（包括 detached worker）都经过这里。长时间运行的进程使用最近一次加载的用户配置，之后创建的所有 `CostTracker` 都应用其快照，包括子代理和挂起 / 后台任务的成本结算。
- 项目级 `.orca/config.toml` 不能设置 `[pricing]`，也不能设置任何 `[models.*.pricing]`（`remove_project_denied_fields` 会删掉它们）。价格决定成本预算什么时候让运行停下，所以和 `budget` 一样只认用户配置。

```toml
[pricing]
refresh = false   # 关闭每天一次的 models.dev 刷新；默认为 true
```

## 3. 代码结构

- 新模块 `crates/orca-runtime/src/pricing/`：
  - `rates.rs`：内置表、`BUILTIN_AS_OF`、时段规则、节假日表、单次计价函数。这部分是纯函数，不碰 IO。
  - `book.rs`：进程内价格表的加载、合并（1.4）和替换，以及缓存文件的读写。
  - `refresh.rs`：HTTP 请求、gzip 解压、models.dev 解析和校验，以及触发条件（1.5）。
- `cost.rs` 的 `CostTracker`：
  - 不再保存 `ModelPricing`，改为保存模型名、价格覆盖和时钟。
  - `add_usage` 时按"当前生效表 + 当前时间"计价。
  - `set_model`、`merge`、`merge_totals` 的语义不变。
  - `CostTracker::new(model)` 使用进程级用户覆盖的快照和系统时钟；库测试统一默认使用空闲时刻。保留显式覆盖的构造（供测试使用）和可注入时钟的构造（`fn() -> DateTime<Utc>`，公开，供测试和需要确定结果的场景使用）。
- 配置：
  - orca-core 中 `ModelDefinition` 增加可选的 `pricing`。
  - `FileConfig`/`RawFileConfig` 增加 `pricing: { refresh }`，并接入配置合并和 `RunConfig`。
  - orca-core 的 `format_config_show` 增加一行 `pricing_refresh = <bool>`，位置挨着 `update_check`。
- `build_run_config` 在配置组装后将用户价格覆盖写入进程级状态；`InteractiveSession` 的内部构造函数触发 1.5 的刷新检查，再通过 `CostTracker::new` 构造会话的 tracker。
- `orca doctor`：文本和 JSON 输出都增加 pricing 一项，包含：
  - 来源：内置（附日期），或 models.dev（附抓取时间）；
  - 刷新是否开启；
  - 现在处于高峰还是空闲。

## 4. 失败和边界情况

| 情况 | 行为 |
| --- | --- |
| 无网、DNS 被污染、超时、非 200/304 | 保持当前价格表，不写缓存，下次进程启动时再试 |
| models.dev 字段改名、`deepseek` 段消失 | 刷新失败，同上 |
| 某个模型的数值异常或偏离内置值 10 倍以上 | 只丢弃这个模型，其余照常 |
| 缓存损坏或版本不认识 | 忽略缓存，用内置表；下次刷新时覆盖 |
| models.dev 改为记录高峰价 | 校验发现不了，估算会翻倍。属于已知风险（第 7 节） |
| 计价时间正好落在时段边界 | 区间是半开区间：09:00 属于高峰，12:00 属于空闲 |
| 会话进行中价格表被替换 | 已记录的 usage 不重算，之后的 usage 按新价 |
| `$ORCA_HOME` 不可写 | 刷新结果只在当前进程生效，不报错 |

所有失败都不弹提示、不打断用户。`orca doctor` 能看到当前用的是哪份价格。

## 5. 测试

所有测试都不访问外网，不写 `~/.orca`，结果不受运行时刻影响（时钟固定）。

- **rates**：
  - 时段边界：北京时间 08:59:59、09:00、11:59:59、12:00、14:00、17:59:59、18:00。
  - 周六、周日；节假日表中的工作日；调休上班的周六。
  - UTC 跨日换算：UTC 周日 23:00 对应北京时间周一 07:00，属于空闲。
- **公式**：
  - 用 issue #136 的 token 数，分别按空闲价、高峰价复算。
  - flash、pro、别名、`v4-pro-exp`、未知模型各一例。
- **models.dev 解析**：fixture 从 2026-10-19 的 `api.json` 中裁出 `deepseek` 段，另附一个无关 provider。需要被拒绝的情况：字段缺失、NaN、负数、超过 100、偏离 10 倍（例如按每 token 计价的值）、`deepseek` 段缺失。
- **book**：
  - 缓存比内置表新、比内置表旧两种情况下的合并结果。
  - 新型号从缓存加入；损坏的缓存被忽略。
  - 替换价格表后，新的 usage 用新价。
- **refresh**：用本地 HTTP fixture 测 200（gzip）、304、500、超时、超大响应，并检查缓存文件内容和原子写入。
  - fixture 注意两点：macOS 上 accept 之后要调用 `set_nonblocking(false)`；端口被占用时重试 bind。
  - 用 `ORCA_PRICING_URL` 把请求指向 fixture，`ORCA_HOME` 用临时目录。
- **配置**：
  - `[models.x.pricing]` 解析、校验报错、别名冲突。
  - 覆盖优先级高于价格表，且不乘高峰系数。
  - `[pricing] refresh = false` 时不发请求。
- **集成**：
  - `orca exec` 加 `ORCA_PRICING_URL` 指向 fixture 跑一次，`ORCA_HOME` 下应生成缓存。
  - `orca doctor` 输出 pricing 一项。
- **改旧测试**：现有测试中写死旧价的地方（`cost.rs` 的测试、`tests/runtime_lifecycle_contract.rs`、`child_agent_tests.rs` 等），改成"固定时钟 + 新价"。只断言成本大于 0 的测试保持不变。

## 6. 文档和维护

- **文档**：README（英文，以及其他语言版本中对应的段落）、`site/src/docs/md/{en,zh}/models.mdx` 和 `configuration.mdx`。需要写清楚：
  - 成本按官方价并区分高峰 / 空闲估算；
  - 每天从 models.dev 刷新一次（会访问 models.dev）；
  - 如何关闭刷新、如何覆盖价格。
  
  改完后要跑 `tests/public_docs_contract.rs`。
- **release notes**：输出价翻倍，高峰时段再翻倍，所以同样的用量估算会比以前高，`max_cost_usd` 预算会更早触发。另外说明新增了一个访问 models.dev 的后台请求，以及如何关闭。
- **发版清单**（`docs/release-process.md`）增加两项：
  - 核对内置价格表与官方价格页是否一致（同时更新 `BUILTIN_AS_OF`）；
  - 每年 11 月国务院公布次年放假安排后，补充节假日表。

## 7. 已知限制和假设

- 用户配置无效时，加载器仍按现有行为发出警告并回退到默认配置，已配置的预算也会丢失。

- 依赖 models.dev "记录空闲价"这个约定。如果它改成记录高峰价，估算会翻倍，范围校验发现不了。
- 时段规则和节假日只能随发版更新。官方调整时段规则后，到下一个版本发布之前，估算会有偏差。
- 计价用的是 Orca 记录 usage 的时间，DeepSeek 计费用的是它那边的时间；只有跨越时段边界的那一次请求可能出现差异。
- 人民币计费账户实际按人民币价格表扣费，美元价格表按汇率换算后大约贵 5%。本次只估算美元。
- 自定义 endpoint 在没有配置覆盖时，仍按 DeepSeek 官方价估算，与现在的行为相同。

- release 模式测试若使用不在 `target/` 下的自定义 `--target-dir`，开发构建保护可能无法阻止访问 models.dev；CI 使用 `target/` 下的 debug 构建。
