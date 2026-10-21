# Paper trading (simulated matching, no broker)

Crate `crates/cmsq-paper` is a trusted, host-side matcher. It fills simulated orders against **incoming exchange quotes**: CTP ticks via `Quote::from(&ctp::Tick)`, or A-share quotes. It does not run an order book among participants. It cannot send a real order: it has no CTP, network or broker dependency, and its sources never name a trading API. Both properties are enforced by `tests/no_order_path.rs`, and cmsq-core's `ctp` module test asserts the same for market data.

Only strategies in status `Promoted` may submit orders.

## Matching rules (defaults are conservative)
| Order | Fill rule |
|---|---|
| market | buy at ask1 / sell at bid1; last price if no L1 |
| limit, marketable | at ask1 / bid1 |
| limit, passive — `trade_through` (default) | only when last trades strictly through the limit, at the limit price |
| limit, passive — `touch_queue` | also on a touch, once traded volume at the limit has consumed the queue ahead (L1 size at placement) |

- Size per quote ≤ ⌊Δcumulative volume × participation⌋ (default 10%), and ≤ the displayed L1 size when known. The first quote of a day has Δ = 0, so it produces no fill.
- No buy fills at/above limit-up and no sell fills at/below limit-down while the market trades at the limit. Limit prices outside the band are rejected. Suspended instruments reject orders.
- A-shares:
  - buys in 100-share lots; odd lots can be sold only as the whole holding; partial fills keep whole lots
  - T+1 (shares bought today cannot be sold until the next trading day)
  - commission is max(¥5, rate × filled value) per order
  - dated stamp duty from `scorer::stamp_duty_sides`
  - no shorting
- Futures:
  - two-way positions, split into today and yesterday lots
  - per-lot or ratio fees, with a close-today tier and the exchange close rule (SHFE/INE: 平今/平昨, cheaper bucket first; DCE/CZCE/CFFEX/GFEX: old lots first, but the fee is charged at the close-today rate first)
  - margin frozen on order and charged on fill
  - daily mark-to-market at the exchange settlement price (`set_settlement`), or at the last price if none is set
- Orders are day orders and expire at the trading-day rollover, which comes from the quote's `trading_day` (night sessions resolved upstream by the calendars).
- Deterministic replay: the same tape gives the same event log, and `event_log_hash` stays stable. This is tested.
- `qifi()` exports the account in QIFI layout, marked `cmsq.paper=true` and `broker=null`.

## Persistence and audit
`PaperSession` writes to the cmsq store (PostgreSQL in production, migration `0003_paper.sql`; SQLite offline). It uses three tables:
- `paper_sessions`
- `paper_events`: append-only, with a contiguous seq, and each row carries the chained hash h_n = sha256(h_{n-1} ‖ event)
- `paper_snapshots`: append-only; a full engine state is written at each settlement and at stop

Every flush adds an audit entry (`paper_events`) carrying the chain head, and session start and stop are audited as well. `verify_chain` recomputes the chain. `resume` refuses to continue if:
- the chain was altered,
- the snapshot does not match the log, or
- events exist after the last snapshot.

The app role gets INSERT/SELECT only on these tables. Event JSON uses `float_roundtrip`, so the hashes survive storage.

## Signals → orders
`SignalDriver` applies a Promoted strategy's stored target fraction once per trading day. It takes the latest signal dated before the quote's trading day, so a signal at close t acts on t+1.

Sizing:
- futures: lots = trunc(target × equity / (price × multiplier))
- A-shares: whole 100-share lots

The driver sends market orders, closing before opening, and the matcher fills them on later quotes.

## CLI
    cmsq paper run --strategy <id> --symbol FUT_RB --instrument rb2601 (--mock | --from-db | live CTP md) [--participation 0.1] [--limit-fill trade_through|touch_queue]
    cmsq paper list | show <session> (QIFI) | verify <session>

`--from-db` replays recorded ticks, so recorded ticks give identical fills. The live mode uses the CTP md feed (`--features ctp`, market data only).

## Status
Done: engine, persistence, audit, CLI, dashboard tab "模拟交易 Paper Trading" (account, positions, settlement equity, recent fills). Live quote push comes with the optional Redis layer.

Design reference: matching ideas only. No third-party code is used.

`cmsq data limits SH600519` / `cmsq data limits FUT_RB --contract rb2601` 从 tushare `stk_limit` / `ft_limit` 拉取交易所公布的逐日涨跌停价，存为 `data/<代码或合约>.limits.csv`（快照 sha256 写入审计）。模拟盘对不带涨跌停的行情（存储的 tick、回放的分钟线）优先使用这些数据；行情自带的涨跌停优先级最高；都没有时才按规则推算。

`cmsq paper run ... --from-bars FILE`：把真实录制的 1 分钟 K 线（CSV：datetime,open,high,low,close,volume）按 开→高/低（按路径顺序）→收 拆成 4 个报价回放，每个报价分得四分之一成交量；文件 sha256 写入审计。夜盘（21:00 后、03:00 前）归到下一个工作日，此处不处理节假日。
