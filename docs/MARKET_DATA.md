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
* **Native replay** (replaces huquant BarPlayer/Strategy): `cmsq_core::replay` provides `Calendar`, `Strategy` trait, `BarPlayer` (the strategy only ever gets `bars[..=i]`), and `MaCrossover`/`Momentum`/`BuyAndHold`. `cmsq replay <SYM> --strategy …` is a reference run, **not gated**. The Docker test asserts that BarPlayer reproduces the sandboxed runner's ma_crossover positions bar for bar.
* **Real-data e2e**: `scripts/e2e_real.sh` (output in `docs/e2e-real-output.txt`).
