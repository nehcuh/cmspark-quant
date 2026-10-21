# Independent acceptance of PR #12

Reviewed master `a0dfc2f0a2685aca73723a41c8631f403f1c4a99`, containing remediation merge `4857e998c674297dcccfc36e75d70b05fd5dff56` and scraping PR #13. The original `2de65f5` checkout and seven defect diagnostics were retained. `reviewer_acceptance.rs` adapts those diagnostics' expected outcomes and current API contracts; it does not substitute the author's new tests for independent evidence. PR #14's paper functionality is outside this review.

## Results on the merged code, before these follow-up fixes

| Issue | Independent outcome | Remaining boundary |
|---|---|---|
| #5 | PASS for original metadata relabeling and unsigned attestation mutation | Source classification is rooted in the trusted adapter's HMAC-attested metadata. The loader verifies the selected entry's HMAC and snapshot hash; this is not external proof of vendor correctness. Same-UID key/anchor compromise remains outside that guarantee. |
| #6 | PASS for original early-prefix cheat, all 50 seeds with production `plan_with_mirrors` | This is finite perturbation coverage, not proof of causality for arbitrary Python. Short/sparse peeking remains probabilistic under the documented contract. |
| #7 | PASS for 100-symbol selection, **FAIL for exact window identity** | Two distinct date windows with 781 bars both dedupe to `selections=1`. `TrialRow::selection_key` stores bar count, not start/end. |
| #8 | PASS for original observer race on SQLite and an isolated PostgreSQL instance | A macOS test incorrectly required Linux PID-start identity; the implementation's documented non-Linux policy conservatively delays recovery on reused PIDs. Cross-host recovery is not independently tested. |
| #9 | PASS for bytes/inodes/live logs; **FAIL for cancellation and host-copy errors** | A cancelled execution future or a failed `positions.csv` host copy left the container behind. Reviewer probes removed only their own leftovers. |
| #10 | PASS for priced old-leg cash flow, **FAIL for gate integration and new-leg executability** | `roll_unpriced=1` was ignored by `gate::evaluate`, despite being present in `GateInput`. Suspended/locked new legs generated roll-open fills without invalidating the model. The author's `gate_blocks_unpriced_rolls` only tested decoding `signed_rolls`. |
| OHLC | PASS for original impossible OHLC diagnostic | Cross-source coverage beyond close prices is still a separate quality contract. |

The original -10% RB old-leg fixture remains display-only with an invalid-roll flag. A fully executable fixture uses old leg -3% and new leg +2%, producing 970,000. The author's former +5% new-leg fixture sat exactly at RB's configured limit-up and is corrected here; it must not be used as an executable positive case.

## This isolated follow-up

- The actual development gate now checks `futures_roll_ledger`, rejecting any invalid roll; the vault's separate guard remains in place.
- Roll validation checks new-leg volume and order-direction price limits for long and short positions. Invalid cases may still be calculated for display, but are explicitly ineligible for development/vault qualification.
- Docker cleanup is armed immediately after a container ID is returned. Cancellation schedules forced removal independently of the cancelled future. Host-copy errors are returned only after awaited removal. Cleanup on abrupt process death, lost daemon access, or total runtime shutdown is not guaranteed by an async drop guard.
- The lease regression respects the declared Linux/non-Linux process identity contract.

The known equal-length-window failure (#7) is deliberately **not fixed here**: it requires persisting/defining the actual scoring interval, not another bar-count heuristic. Its explicit ignored reproduction is in `reviewer_acceptance.rs`; run `different_windows_with_same_bar_count_are_not_deduped -- --ignored --nocapture`. It must fail until that contract is implemented. Keep #7/#11 open; do not interpret the otherwise passing suite as full acceptance. This avoids mixing the trial schema change into active PR #14's storage work.

## Executed evidence and limits

Evidence logs in [evidence/2026-10-21-acceptance](evidence/2026-10-21-acceptance) include pre-fix gate/new-leg/window failures, Docker cleanup failures, and post-fix cleanup and PostgreSQL results. They are normalized only to remove local absolute checkout prefixes.

- Original seven reviewer diagnostics: 6 non-Docker tests passed; the original 9 MiB Docker probe separately passed by receiving ENOSPC instead of creating a host file.
- Additional reviewer tests: HMAC mutation refused; long/short suspended and locked new legs flagged; unpriced roll rejected by the actual gate. Bounded Docker tests enforce a 16-inode budget (15 files created), kill a stderr flood with `QuotaExceeded`, and remove containers on cancellation/host-copy failure.
- Core suite after follow-up: 89 unit, 15 author audit, 5 pipeline, 2 scrape, 4 SQLite concurrency, plus 9 non-Docker reviewer tests passed. Dependency-dependent cases remain ignored, including the explicitly known #7 reproduction. Counts are not evidence that ignored cases passed.
- Isolated PostgreSQL 16: reviewer-owned acceptance tests passed (including explicit `backend=postgres` observer verification); six audit/privilege/ledger/vault/CAS tests passed. The instance is disposable, loopback-only and uses no existing database credentials. TimescaleDB extension/hypertable testing was excluded, not passed.
- `cargo clippy -p cmsq-core --all-targets --locked --offline -- -D warnings` and formatting checked with local Homebrew Rust 1.96.0; repository pin 1.99.1 is not locally installed.
- Remote master CI [39557051861](https://github.com/nehcuh/cmspark-quant/actions/runs/39557051861), exact SHA `4857e998...`, was independently read: check, e2e-sqlite, e2e-postgres succeeded. This is remote evidence for that commit, not local rerunning or proof of this follow-up's CI.
- Local full pinned-runner e2e, bubblewrap, TimescaleDB extension, CTP network, exchange-history accuracy and external QIFI consumer compatibility remain unverified. Existing Python Docker image was used only to test the executor's bounded policies.

No merge, deployment, broker connection, data purchase or credential-scope change was performed. This branch is intended for a draft PR and independent review.
