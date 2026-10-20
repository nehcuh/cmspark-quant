#!/usr/bin/env bash
# Runs the tests that need real isolation (Docker + bubblewrap). Fails loudly —
# never skips — when a prerequisite is missing.
set -euo pipefail
cd "$(dirname "$0")/.."
docker info >/dev/null 2>&1 || { echo "FAIL: docker daemon not reachable by $(id -un) (see README: Docker setup)"; exit 2; }
command -v bwrap >/dev/null || { echo "FAIL: bubblewrap (bwrap) not installed"; exit 2; }
docker image inspect cmsq-runner:2.0.0 >/dev/null 2>&1 || scripts/build_runner.sh
cargo test -p cmsq-core -- --ignored --test-threads=2
# vendored rstdx: the empty-bars regression test
(cd third_party/rstdx && CARGO_TARGET_DIR=../../target/rstdx cargo test --lib security_bars)
