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
* Caveat: TDX prices are **unadjusted**; the runner adds a `data_caveats` warning. Forward/back adjustment is a P1 item.

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
