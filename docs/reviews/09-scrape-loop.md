# Review 09 — scraping loop (adversarial)

| Attack / failure | Result |
|---|---|
| Redirect from allowlisted host to arbitrary host (SSRF) | Fixed: post-redirect URL host+scheme re-checked |
| Huge file / zip bomb | body limit + 64 KB strategy cap |
| Search result spam (non-strategy .py) | CtaTemplate + on_bar filter; `invalid` counted against source |
| Re-scrape same content inflating counts | content sha256 dedupe (verified real 2nd cycle: 0 new, 9 dup) |
| Low quota cap starves items forever | backlog queue (test `backlog_left_by_the_cap_is_backtested_next_cycle`) |
| One source down kills cycle | per-source error recorded (test) |
| Unbacktested items lower score | stage-wise smoothing (test `scores_follow_downstream_evidence`) |
| Token leakage | read from secrets dir, not in audit/spec/logs |

Remaining issues:
- A manual drop that claims `site = <source id>` counts toward that source's score. The effect is limited to quota steering.
- Backlog order follows the store listing, so a big source can delay a small source's backlog.
- The extractor is heuristic only.
