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

## Status
Done in this chunk: the engine and its tests.

Next chunks:
- Postgres persistence of orders, fills and snapshots (audited)
- a CLI driver that connects Promoted-strategy signals to the md pipeline
- the dashboard view

Design reference: matching ideas only. No third-party code is used.
