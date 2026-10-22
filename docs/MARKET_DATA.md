# Market data & third-party code (rstdx, huquant)

## Pluggable source (`cmsq_core::market::MarketDataSource`)
| Source | Feature | Symbols | Notes |
|---|---|---|---|
| `SampleSource` | always | `SAMPLE_TREND`, `SAMPLE_RANGE` | **SYNTHETIC** series (`scripts/gen_sample_data.py`, fixed seeds) on real SSE trading days 2015-01-05..2019-12-31. `SAMPLE_RANGE` has a deliberately planted short-term mean-reversion effect, so a strategy passing the gate on it demonstrates the pipeline, **not alpha**. |
| `RstdxSource` | `tdx` (on by default in the CLI) | `SH600519`, `SZ000001`, `BJ…` | Real A-share daily K-lines via the user's `rstdx` crate (TDX protocol). |

Fetching happens **on the host** (`cmsq data fetch <SYMBOL>`), never inside the sandbox; the result is cached as
`data/<SYMBOL>.csv` + `data/<SYMBOL>.meta.json` (source, server, fetched_at, range, bar count, adjustment, rstdx rev)
and the whole `data/` directory is mounted **read-only** at `/data` in every backtest container, which keeps
`NetworkMode=none`. The metadata is copied into every result JSON (`data_provenance`).

### rstdx status on this box (2026-10-20)
* Dependency: path dep `../rstdx` (repo is private; switch to `git = "https://github.com/nehcuh/rstdx", rev = …"` once
  CI has credentials).
* rstdx HEAD `574cd53` **did not compile** (`src/query/api.rs` imports the private module `client::managed`).
  A one-line fix was committed **locally** on branch `fix/managed-import` (`77629df`) in `/workspace/rstdx`; not pushed.
* Reachability: TCP to several TDX servers (e.g. 180.153.18.170:7709, 115.238.56.198:7709) succeeds and the handshake
  completes, but daily-bar requests return an empty/2-byte body from this (overseas) box — `pytdx` shows the same
  empty result against all 35 default servers, so this is server-side, not an rstdx bug. rstdx then reports
  `protocol error: Buffer too short for datetime` (it should treat a short body as "0 bars"; worth fixing upstream).
  The failure is audited (`market_data_fetch`, success=false) and `scripts/e2e.sh` falls back to SAMPLE data.
* Caveat: TDX prices are **unadjusted** (rstdx has no xdxr fetch). Unadjusted data fails the `data_adjusted` gate check, so it can be backtested but never promoted. To use real data, import a CSV with an `adj_factor` column: `cmsq data import <csv> --adjustment hfq`. The container sees raw×adj_factor; price-limit checks use raw prices.

## huquant (vendored, read-only)
Evaluated `/workspace/huquant` @ `928aff4`:
* `BarPlayer.py` / `Strategy.py` — live-loop wrappers around `jqdatasdk` that **authenticate at import time**, poll the
  wall clock and keep account state in-process. Not suitable inside a network-denied, deterministic sandbox, so they
  are **not** used.
* `trade_dates.py` — a static SSE trading calendar (1990-12-19 .. 2019-12-31) + helpers. Fits. Vendored verbatim at
  `vendor/huquant/trade_dates.py` (sha256 `0b1d1bf5…acf5`, checked at `cmsq init`), copied read-only into
  `<home>/vendor/huquant` and mounted read-only at `/vendor/huquant`. The runner only uses `trade_date_sse`; it reports
  `calendar_check` (bars on non-trading days, bars beyond calendar end). Known upstream bugs not exercised by us:
  `util_get_next_trade_date`/`util_trade_date_gap` call undefined `Hu_*` names, `gat` typo, duplicate function name.
  The calendar ends in 2019, so recent rstdx data shows as "beyond calendar".

## Multi-source router (2026-10-20)

`cmsq data fetch <SH600519|SZ000001|BJ…>` goes through `cmsq_core::data::router::Router`:

| Priority | Source | How | Rate limiting | Notes |
|---|---|---|---|---|
| 1 | **tushare** | HTTPS `https://api.tushare.pro` (`daily`, `adj_factor`, `trade_cal`, `stock_basic`) | per-endpoint token bucket, default **400/min** (5000-pt tier = 500/min, 20% headroom; `CMSQ_TUSHARE_RPM`); on "每分钟最多访问该接口N次" the bucket drops to 0.8·N and backs off with jitter; "每天最多…" marks the quota exhausted | only source with `adj_factor` → the only one that passes `data_adjusted`. Suspended open days (from `trade_cal`) are filled as volume-0 bars at the previous close. Volume converted 手→shares. ST names flagged in meta (±5% not modelled). Plain `http://` timed out from this box; HTTPS works. |
| 2 | **akshare** | host python helper `scripts/data_helper.py` (embedded in the binary), Sina `stock_zh_a_daily` first, Eastmoney `stock_zh_a_hist` fallback | serial, ≥1.2 s spacing ±25% jitter, 60 s timeout, retries with exponential backoff | unadjusted; backup + validator. Eastmoney push2his disconnects from this box; Sina works. |
| 3 | **baostock** | same helper, `query_history_k_data_plus adjustflag=3` | serial, ≥0.5 s spacing | unadjusted; login fails from this box ("网络接收错误"). |
| 4 | **tdx** (rstdx) | vendored rstdx, feature `tdx` | — | unadjusted; TDX servers unreachable from this box. |

* **Token**: env `TUSHARE_TOKEN`, else `TUSHARE_TOKEN=` in `<secrets dir>/secrets.env`, `$CMSQ_TUSHARE_ENV_FILE`, or `~/.config/cmsq/secrets.env`. It is never logged, put in meta/audit/errors (error text is redacted, `Debug` prints `<redacted>`), and is not in the repo.
* **Fallback + health**: sources are tried in order; each success/failure is recorded in `data/_sources_health.json` (ok/fail counts, last error, latency). Consecutive failures put a source in cooldown (30 s·2^n, max 30 min). `cmsq data sources` shows the state. Order: `--sources` or `CMSQ_DATA_SOURCES`.
* **Cross-source validation**: after the primary succeeds, the next reachable source is fetched and **raw closes are compared day by day** (volume-0 filler days skipped). A day with a >0.1% difference is flagged. The result goes into `meta.extra.validation` (`ok` | `divergent` | `unvalidated`, plus a sample of the divergent days). Gate check `data_cross_validated` **fails** for `divergent` and for `unvalidated` real data (synthetic samples pass; a user CSV passes, labelled "user attests").
* **Provenance + snapshots**: `meta.json` records source, endpoint, fetch time, router attempts, validators and adjustment. The cache CSV is mode 0444, and an immutable content-addressed copy is written to `data/snapshots/<sha256>.csv`. The sha goes into meta and into the audit chain (`market_data_fetch`). A backtest refuses to run if the cache no longer matches its snapshot sha.
* **Trade calendar**: `cmsq data calendar` saves the SSE `trade_cal` to `data/_calendar_SSE.txt` (audited).
  `cmsq data calendar --exchange futures` (or one of SHFE DCE CZCE CFFEX INE GFEX) saves each futures exchange's
  `trade_cal` to `data/_calendar_<EX>.txt` (sha256 in the audit entry). Used for CTP trading-day labels
  (night session → next trading day across holidays), the holiday-eve "no night session" flag, and futures
  calendar-gap filling. Without a snapshot the only fallback is "weekdays open", reported as such
  (`md` audit entry `calendars`). No holiday is hard-coded.
* **Native replay** (replaces huquant BarPlayer/Strategy): `cmsq_core::replay` provides `Calendar`, `Strategy` trait, `BarPlayer` (the strategy only ever gets `bars[..=i]`), and `MaCrossover`/`Momentum`/`BuyAndHold`. `cmsq replay <SYM> --strategy …` is a reference run, **not gated**. The Docker test asserts that BarPlayer reproduces the sandboxed runner's ma_crossover positions bar for bar.
* **Real-data e2e**: `scripts/e2e_real.sh` (output in `docs/e2e-real-output.txt`).

## 第二轮补充（2026-10-20）
* 交叉校验容差策略：差异 >0.1% 的日子被标记；标记日 ≤ max(3, 重叠天数/500) 且无单日 >1% → `ok_with_flags`（可晋升）；
  否则 `divergent`（阻断）；重叠 <20 天 → `unvalidated`（阻断）。策略与标记日期记录在 fetch 的审计条目中。
* ST：由 tushare `namechange` 得出 ST 区间，主板 ST 涨跌幅 5%（2025-07-07 起 10%）。
* 新股：取 `list_date`；注册制板块前 5 个交易日无涨跌停，其他板块仅上市首日。老新股 44% 规则未建模。
* 北交所：含 92xxxx 新代码。停牌：成交量 0 或交易日历缺口。
* CSV 导入与 sample 数据永不通过 `data_cross_validated`。

## 第三轮（P0 存储 / P2 期货 / P3 CTP，2026-10-20）

### 存储：独立部署的 PostgreSQL + TimescaleDB
* `deploy/docker-compose.yml`（镜像按 digest 固定，只监听 127.0.0.1:55432），`scripts/db_up.sh`
  生成两套口令并写入 `~/.config/cmsq/db.env`（0600，不在仓库/工作区内）。
* `cmsq_owner` 只用于迁移；运行时用 `cmsq_app`：对 audit/trials/signals/evaluations/bars/ticks 只有
  INSERT/SELECT，无 UPDATE/DELETE/TRUNCATE，也不能删/停触发器（`tests/store_concurrency.rs` 验证）。
* 每个工作区一个 schema `ws_<hash(home + 工作区 id)>`（新建工作区时写入随机 id 到 `.cmsq_workspace_id`；删掉工作区在同一路径重建会得到新 schema，不会继承旧的审计链；没有 id 文件的老工作区仍用 `ws_<hash(home)>`）；`bars`（日线快照镜像）与 `ticks`（CTP 行情）是 TimescaleDB hypertable。
* `CMSQ_DB=sqlite` 走离线 SQLite（同一套 SQL、同一套审计/账本语义），仅用于测试和无数据库环境。

### 期货主力连续（`FUT_<品种>`，如 `FUT_RB`）
* tushare `fut_basic` + 逐合约 `fut_daily`；按持仓量领先且连续 3 个收盘确认换月，次日生效，不回切。
  `adj_factor` 记录换月比例；回测按持有合约的原始价格结算，换月两腿都付手续费和滑点。
* 合约规格（乘数、最小变动价位、保证金率、涨跌停幅度、逐合约手续费、夜盘时段）见 `futures.rs`，
  可用 `CMSQ_FUTURES_SPECS=<json>` 覆盖。
* 交叉校验：akshare（新浪 `futures_zh_daily_sina`）逐合约比对原始收盘价。新浪只保留约 5 年的到期合约，
  覆盖率不足 90% 时状态为 `partial`（门禁视为未校验）。期货建议 `--count 1100`，此时覆盖 100%。

### CTP 行情（P3，只观察）
* `cmsq md run --instruments rb2601,au2612 [--front tcp://...] [--seconds N] [--mock]`；`cmsq md probe`。
* 实时源：`cargo build -p cmsq-cli --features ctp`，并设置 `CMSQ_CTP_MD_LIB=<thostmduserapi_se.so>`（OpenCTP TTS 版）。
  只引用 ctp2rs 的 MdApi；单元测试检查 `src/ctp/` 中不出现交易 API。
* 每个 tick 都要校验：价格有限且 < 1e11（CTP 用 DBL_MAX 表示"无值"），在涨跌停内，时间与累计成交量单调，
  合约必须在订阅列表里。拒绝原因分项计数。交易日归属用 `trading_day_for`（夜盘归下一交易日）。
* **本机连不上 openctp 7x24（trading.openctp.cn:30011 TCP 超时）**，因此 e2e 使用内置回放源
  `ReplayFeed::demo`：含 DBL_MAX、NaN、超涨停、时间倒退、成交量倒退、非法时间、未订阅合约等坏数据。
