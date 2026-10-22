"""Kronos synthetic-path generator (UNTRUSTED side, cmsq-runner-kronos only).

/input/bars.csv  development-window bars (container view, fake dates)
/job/job.json    {"paths": N, "horizon": H, "lookback": L, "seed": S, "model": "mini"}
/out/positions.csv  (the sandbox's single framed output file) path,step,open,high,low,close,volume   (N × H rows)

The host validates every row and assigns dates itself; nothing else is read.
"""
import json
import os
import sys

import numpy as np
import pandas as pd

sys.path.insert(0, "/runner/kronos")
sys.path.insert(0, "/runner/kronos/kronos_code")


def main():
    job = json.load(open("/job/job.json"))
    n, h = int(job["paths"]), int(job["horizon"])
    lb, seed, model = int(job.get("lookback", 256)), int(job.get("seed", 0)), job.get("model", "mini")
    df = pd.read_csv("/input/bars.csv", parse_dates=["date"]).set_index("date")
    import torch
    from cmsq_kronos import _predictor

    torch.use_deterministic_algorithms(True)
    p = _predictor(model)
    x = df.iloc[-lb:]
    xs = pd.Series(x.index)
    step = xs.diff().median()
    ys = pd.Series([xs.iloc[-1] + step * (k + 1) for k in range(h)])
    cols = ["open", "high", "low", "close", "volume"]
    with open("/out/positions.csv", "w") as f:
        f.write("path,step,open,high,low,close,volume\n")
        for i in range(n):
            torch.manual_seed(seed + i)
            np.random.seed((seed + i) % (2**32))
            pr = p.predict(df=x[cols].reset_index(drop=True), x_timestamp=xs, y_timestamp=ys,
                           pred_len=h, T=1.0, top_p=0.9, sample_count=1, verbose=False)
            for s in range(h):
                o, c = float(pr["open"].iloc[s]), float(pr["close"].iloc[s])
                hi = max(float(pr["high"].iloc[s]), o, c)
                lo = min(float(pr["low"].iloc[s]), o, c)
                v = max(float(pr["volume"].iloc[s]), 0.0)
                f.write("%d,%d,%.6f,%.6f,%.6f,%.6f,%.2f\n" % (i, s, o, hi, lo, c, v))


if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        print("generator error: %r" % (e,), file=sys.stderr)
        sys.exit(2)
