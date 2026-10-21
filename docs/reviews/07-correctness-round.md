# 07 Adversarial review: close-today rules, vn.py ticks/units, synthetic/capacity gate, e2e happy path (2026-10-21)

| Item | Attack / doubt | Resolution |
|---|---|---|
| Close-today order | The earlier defaults (CFFEX today-first, DCE/CZCE FIFO with ordinary fee) were guesses | Checked against broker FAQs (国金期货, 国盛期货) and 信易科技's notes. Only SHFE/INE have a 平今 instruction (`TraderChooses`). On DCE/CZCE/CFFEX/GFEX the position is reduced old-lots-first, but the close is **charged at the close-today rate first** up to the lots opened that day (`FifoFeeTodayFirst`). Unit test reproduces the CFFEX example (1 old + 1 today, close 1 ⇒ old lot removed, close-today fee charged) |
| Close-today counters across flip/roll | A long→short flip on the same day could reuse the long side's counters | The ledger (and its counters) resets when the side goes flat; a roll reopens as same-day lots |
| SHFE cheapest bucket | Favourable to the strategy | Labelled in code and docs; overridable per product |
| pricetick | Rounding could leak future info | Rounding is a pure function of the order price and the host-supplied tick, so it is identical in every probe variant |
| `cmsq_max_units` | A free knob to make results look better | It is part of `params`, so it changes `params_hash` and is a separate trial in the DSR ledger |
| Synthetic data | `SAMPLE_PERSIST` has a built-in trend edge; a strong trend follower could pass the default gate (N=20) and be "promoted" on fake data | New gate check `real_market_data`: synthetic samples fail unless `CMSQ_ALLOW_SYNTHETIC=1`, which only the e2e demo workspaces set (recorded in the gate config of every evaluation) |
| Capacity | The spec requires a capacity estimate; daily-volume units differ (shares vs lots) | The estimate is the 10th percentile, over fills, of the account size at which the fill would be 5% of that day's traded value. A-shares: adjusted-scale qty × adjusted price; futures: lots × price × multiplier. Gate minimum is ¥1m (`CMSQ_GATE_MIN_CAPACITY`). Portfolio uses the minimum across legs |
| IPO-day ±44% | Listed as an unmodelled gap | Not reachable: fills start at bar 1 (decision at close t, fill at open t+1), so no order ever executes on the listing day |
| e2e happy path | The vault demo was rejected, so the QIFI export never ran end to end | New section 5b on `SAMPLE_PERSIST` (fixed bull/bear schedule, labelled mechanics-only): GatePassed → vault → Promoted → `export-qifi` → JSON validated → vault trade stats |
