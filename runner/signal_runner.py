"""cmsq signal runner (UNTRUSTED side, runs inside the sandbox).

This process runs author-supplied strategy code, so NOTHING it writes is trusted.
It has exactly one job: turn the bars it was handed into target positions.

Contract
--------
/input/bars.csv      read-only  the ONLY market data in the container: one variant
                                 (full / truncated / resampled-future) prepared by
                                 the host. date,open,high,low,close,volume
/job/strategy.py     read-only  defines generate_signals(df, **params), OR a vn.py
                                 CtaTemplate subclass with on_bar (see vnpy_shim.py)
/job/job.json        read-only  {"params": {...}, "position_min": 0|-1}
                                 (no symbol, no costs, no split)
/out/positions.csv   writable   date,position   one row per input bar, in
                                 [position_min, 1] (-1 only for futures jobs)

Metrics, costs, fills, look-ahead comparison and gating are all computed by the
trusted host (Rust, crates/cmsq-core/src/scorer.rs). The host validates
positions.csv (exact dates, finite, 0..1, size cap) and ignores everything else.
"""
import importlib.util
import json
import os
import sys

import pandas as pd

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import vnpy_shim  # noqa: E402

IN = os.environ.get("CMSQ_INPUT", "/input")
JOB = os.environ.get("CMSQ_JOB", "/job")
OUT = os.environ.get("CMSQ_OUT", "/out")


def main():
    job = json.load(open(os.path.join(JOB, "job.json")))
    params = job.get("params", {})
    lo = -1.0 if float(job.get("position_min", 0)) < 0 else 0.0
    df = pd.read_csv(os.path.join(IN, "bars.csv"), parse_dates=["date"]).set_index("date")
    for c in ("open", "high", "low", "close", "volume"):
        df[c] = df[c].astype(float)
    vnpy_shim.install()
    spec = importlib.util.spec_from_file_location("strategy", os.path.join(JOB, "strategy.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    if hasattr(mod, "generate_signals"):
        out = mod.generate_signals(df.copy(), **params)
    else:
        out = vnpy_shim.run_cta(mod, df.copy(), params, lo, job.get("pricetick"))
    pos = pd.Series(out, index=df.index).astype(float).fillna(0.0).clip(lo, 1.0)
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
