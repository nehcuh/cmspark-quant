# CMspark scrape-result drop format — `cmspark.scrape.v1`

CMspark (browser agent) writes one JSON file per scraped post into `<CMSQ_HOME>/drop/`.
`cmsq ingest` (or the dashboard **Ingest** button) hands each file over to Core: the original
bytes are archived immutably at `raw/<uuid>/post.json`, the file is moved to `drop/processed/`
(or `drop/rejected/` + `.reason.txt`), and an audit entry is written. Once dropped, CMspark never
touches the sandbox (architecture boundary from the Trade-off Judge decision).

```json
{
  "spec": "cmspark.scrape.v1",            // required, exact
  "sample": false,                        // true = fixture / demo content
  "source": {
    "site": "quant-forum",                // required, short source id
    "url": "https://…/thread/123",        // optional, must be http(s); used for 1-click provenance
    "author": "someone",                  // optional
    "community": "…"                      // optional free text
  },
  "scraped_at": "2026-10-19T09:12:00+08:00",   // required, RFC3339 with offset
  "scraper": { "agent": "cmspark", "version": "x.y", "task_id": "…" },
  "content": {
    "title": "双均线金叉死叉",              // required, non-empty
    "format": "markdown",
    "body": "…prose… ```python\n…code…\n```"   // required; python code fences are extracted
  },
  "attachments": [ { "name": "strategy.py", "language": "python", "content": "…" } ],
  "labels": ["strategy-idea"]
}
```

Rules enforced by `ingest.rs`:
* max 1 MiB per file; invalid JSON / wrong spec / missing fields / non-http URLs → rejected (audited).
* De-duplication by sha256 over (url, body, attachments) → `DUPLICATE`, original id returned.
* Nothing from the drop is used as a filesystem path; Core assigns a UUID.
* Content is untrusted data. The LLM extractor wraps it in `<post>` and is told to ignore instructions in it; the code that
  is executed is always copied verbatim from the post (never LLM-written) and only ever runs inside the sandbox.

## Runner contract for strategy code
The extracted code must define `generate_signals(df, **params) -> pd.Series` returning a target position in `[0, 1]`
per bar (long-only, A-share). `df` has `open/high/low/close/volume`, indexed by date. Default values in the
signature become the extracted parameters.

Fixtures in `fixtures/posts/*.json` are **SAMPLE** posts (labelled `"sample": true`, URLs on `example.invalid`).

## vn.py CtaTemplate strategies (runner 2.2.0)

Besides `generate_signals(df, **params)`, a code block may define a vn.py-style
strategy: `class Foo(CtaTemplate)` with `on_bar`. Imports from `vnpy_ctastrategy`,
`vnpy.app.cta_strategy`, `vnpy.trader.constant|object|utility` resolve to the
in-sandbox shim `runner/vnpy_shim.py` (CtaTemplate, TargetPosTemplate, CtaSignal,
BarGenerator, ArrayManager with sma/ema/std/atr/rsi/macd/cci/boll/keltner/donchian/
momentum/roc, BarData/TickData/OrderData/TradeData/StopOrder, Direction/Offset/
Interval/Exchange). Parameters come from the class's `parameters = [...]` list and
the class-level defaults; sweeps override them by name.

Semantics — the shim never places an order:
* `buy/sell/short/cover/send_order` create resting limit or stop orders (`on_order` /
  `on_stop_order` called back); `cancel_order` / `cancel_all` remove them. On the next
  bar they are crossed the way vn.py's BacktestingEngine does (limit before stop; limit
  buy fills if low ≤ px at min(px, open); stop buy triggers if high ≥ px at max(px, open);
  mirrored for sells), which moves the virtual `pos` and calls `on_trade`.
* Order prices are rounded to the contract tick (`job.json` `pricetick`: futures spec tick,
  0.01 for A-shares) like vn.py's BacktestingEngine; `get_pricetick()` returns it.
* After every bar the runner records `clip(pos / (fixed_size × max_units), position_min, 1)` as the
  target; the trusted host executes it at the next bar's open with its own costs and
  contract rules — one bar later than vn.py fills (conservative, never look-ahead).
* ArrayManager ATR/RSI use TA-Lib-style Wilder smoothing; other indicators are numpy
  re-implementations (no TA-Lib in the image).
* `load_bar`, `put_event`, `write_log`, `send_email`, `sync_data` are no-ops; `on_tick`
  is never called (daily bars only). Intraday `BarGenerator` windows (minute/hour) pass
  daily bars through; DAILY/WEEKLY windows aggregate N bars.
* Verified: the official examples DoubleMa, AtrRsi, BollChannel, KingKeltner, TurtleSignal
  and DualThrust run unmodified (`scripts/test_vnpy_shim.py`, and in Docker on FUT_RB).
  `max_units` = the `cmsq_max_units` parameter, else a `max_units` strategy attribute, else 1;
  pyramiding strategies (the turtle adds up to 4 units) set it so each unit is a fraction of
  the full position. Known differences: time-of-day logic (DualThrust's exit time) is
  meaningless on daily bars; no TA-Lib, so values can differ slightly from vn.py.

`job.json` now carries `position_min`: 0 for A-shares, -1 for futures (`FUT_*`), so
short targets survive only on futures jobs; the host re-validates the range.
