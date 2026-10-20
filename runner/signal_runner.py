"""cmsq signal runner (UNTRUSTED side, runs inside the sandbox).

This process runs author-supplied strategy code, so NOTHING it writes is trusted.
It has exactly one job: turn the bars it was handed into target positions.

Contract
--------
/input/bars.csv      read-only  the ONLY market data in the container: one variant
                                 (full / truncated / resampled-future) prepared by
                                 the host. date,open,high,low,close,volume
/job/strategy.py     read-only  must define generate_signals(df, **params)
/job/job.json        read-only  {"params": {...}}  (no symbol, no costs, no split)
/out/positions.csv   writable   date,position   one row per input bar, in [0, 1]

Metrics, costs, fills, look-ahead comparison and gating are all computed by the
trusted host (Rust, crates/cmsq-core/src/scorer.rs). The host validates
positions.csv (exact dates, finite, 0..1, size cap) and ignores everything else.
"""
import importlib.util
import json
import os
import sys

import pandas as pd

IN = os.environ.get("CMSQ_INPUT", "/input")
JOB = os.environ.get("CMSQ_JOB", "/job")
OUT = os.environ.get("CMSQ_OUT", "/out")


def main():
    params = json.load(open(os.path.join(JOB, "job.json"))).get("params", {})
    df = pd.read_csv(os.path.join(IN, "bars.csv"), parse_dates=["date"]).set_index("date")
    for c in ("open", "high", "low", "close", "volume"):
        df[c] = df[c].astype(float)
    spec = importlib.util.spec_from_file_location("strategy", os.path.join(JOB, "strategy.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    if not hasattr(mod, "generate_signals"):
        raise RuntimeError("strategy.py does not define generate_signals(df, **params)")
    out = mod.generate_signals(df.copy(), **params)
    pos = pd.Series(out, index=df.index).astype(float).fillna(0.0).clip(0.0, 1.0)
    with open(os.path.join(OUT, "positions.csv"), "w") as f:
        f.write("date,position\n")
        for d, p in zip(df.index, pos.values):
            f.write("%s,%.10g\n" % (d.strftime("%Y-%m-%d"), p))


if __name__ == "__main__":
    try:
        main()
    except Exception as e:  # message only; the host decides what it means
        print("strategy error: %r" % (e,), file=sys.stderr)
        sys.exit(2)
