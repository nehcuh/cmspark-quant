# 04 — External adversarial review fixes

Sources: review-quant-security.md, review-product-code.md, attack posts in rq-adv (now fixtures/attacks/).

| Finding | Fix | Evidence |
|---|---|---|
| Container wrote its own metrics/result.json (forge) | runner 2.0.0 emits only positions.csv; host Rust scorer computes everything; gate reads only job status + image label | ADV_forge → Rejected (e2e); pipeline `docker_reviewer_attacks_never_pass` |
| Look-ahead probe run in the same process (memo/hidden/length peeks) | 10 variants, each a fresh container, shuffled order, block-bootstrap + truncation, compared on host | hidden_peek/memo_peek/length_tell caught by no_lookahead; screenshot 09 |
| Symlink /out escape, data peek | O_NOFOLLOW read, strict parse; ingest symlink-safe | ADV_symlink_out → Failed(bad_output); ADV_data_peek → Failed |
| Fork bomb / output flood | pids/mem limits, OUTPUT_CAP | forkbomb/bigout → Failed(oom); sandbox `docker_output_flood_is_capped` |
| Fill timing claimed vs actual | close t signal → open t+1 fill + slippage, label in result.json | scorer tests |
| A-share rules missing | lots 100, min ¥5 commission, dated stamp duty, board/date price limits, suspension, hfq adj_factor | scorer tests |
| Multiple testing | library-wide trial ledger, DSR ≥ 0.95, 4-fold walk-forward, one-shot vault | momentum Rejected by DSR 0.918; ma_crossover GatePassed then vault Rejected |
| Audit chain rewritable | HMAC key + anchor outside CMSQ_HOME | store attack tests; e2e edit/truncate/rechain detected |
| Fresh-clone build | rust-toolchain.toml, vendored rstdx, test_docker.sh fails loudly, e2e nonzero on mismatch | README |
| rstdx empty bars panic | parse fix + tests | rstdx security_bars 13 pass |
| No GatePassed/crash recovery/typed errors | GatePassed state, reap_stale_jobs, CoreError | pipeline crash test |
| Dashboard blocking, fake confirm | background jobs + log, typed PROMOTE input, timeline, equity, compare, no-Docker banner | screenshots 01–13 |

Remaining gaps: see README "已知局限".
