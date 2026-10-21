# Review 10: paper matcher (adversarial)

| Attack / mistake | Guard / test |
|---|---|
| Non-promoted strategy trades | `only_promoted_strategies_trade` |
| Fill on first tick using day-cumulative volume | Δvolume = 0 on first quote |
| Unlimited size at the touch | participation cap plus L1 depth cap |
| Optimistic passive limit fills | default trade-through; touch needs the queue consumed (`limit_trade_through_vs_touch_queue`) |
| Buying a sealed limit-up | `no_buy_at_limit_up_no_sell_at_limit_down` |
| Same-day sell of today's buy | T+1 test |
| Min commission charged per partial fill | per-order cumulative commission |
| DCE 平今 instruction / fee-tier dodge | rejected; fee today-first test |
| Stale quote from a settled day | ignored |
| Frozen cash leaking after expiry | `day_orders_expire_and_unfreeze` |
| Real order path | `no_order_path.rs` |
| Non-determinism | BTreeMap ordering; `deterministic_replay` |

Known limitations:
- No transfer fee.
- Market orders freeze at the limit-up price.
- Futures available funds exclude floating profit, which is conservative.
- If the ask sits at limit-up but trades happen below it, a buy can still fill at the ask.

## Persistence round
| Attack / mistake | Guard / test |
|---|---|
| Edit a stored event (DROP TRIGGER first) | chain recompute → "altered"; resume refuses |
| Float JSON round-trip breaking the hash | found by test (20749.6 vs …0002); fixed with `float_roundtrip` |
| Resume from a stale snapshot | refuses if events exist after the snapshot |
| Sequence gap or duplicate | seq check in a transaction plus PK(session, seq) |
| PG app role UPDATE/DELETE | trigger plus grants (`pg_paper_lifecycle`) |
| Signal look-ahead | a signal acts only on a later trading day (driver test) |
