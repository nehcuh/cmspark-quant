"""Deterministic, look-ahead-free Kronos forecasts for strategies (UNTRUSTED side).

Weights are baked into the image (pinned by SHA256SUMS); there is no network.
Every forecast at bar t uses ONLY bars[: t+1] and reseeds torch with a seed
derived from t, so truncating or changing the future never changes the past
(the host's look-ahead probes verify this).
"""
import os

import numpy as np
import pandas as pd

ROOT = os.environ.get("CMSQ_KRONOS_ROOT", "/runner/kronos")
MODELS = {
    "mini": ("Kronos-mini", "Kronos-Tokenizer-2k", 2048),
    "small": ("Kronos-small", "Kronos-Tokenizer-base", 512),
}
_cache = {}


def _predictor(model):
    if model not in _cache:
        import torch
        from model import Kronos, KronosPredictor, KronosTokenizer

        torch.set_num_threads(1)
        torch.use_deterministic_algorithms(True)
        m, t, ctx = MODELS[model]
        tok = KronosTokenizer.from_pretrained(os.path.join(ROOT, "weights", t))
        net = Kronos.from_pretrained(os.path.join(ROOT, "weights", m))
        _cache[model] = KronosPredictor(net, tok, device="cpu", max_context=ctx)
    return _cache[model]


def forecast_paths(df, t, model="mini", lookback=64, pred_len=5, samples=4, seed=20251022):
    """`samples` forecast closes (shape samples × pred_len) using bars[: t+1] only."""
    import torch

    p = _predictor(model)
    lo = max(0, t + 1 - lookback)
    x = df.iloc[lo : t + 1]
    xs = pd.Series(x.index)
    step = xs.diff().median() if len(xs) > 1 else pd.Timedelta(days=1)
    ys = pd.Series([xs.iloc[-1] + step * (k + 1) for k in range(pred_len)])
    cols = [c for c in ("open", "high", "low", "close", "volume") if c in x]
    out = []
    for s in range(samples):
        torch.manual_seed(seed + 1000 * t + s)
        np.random.seed((seed + t + s) % (2**32))
        pred = p.predict(
            df=x[cols].reset_index(drop=True), x_timestamp=xs, y_timestamp=ys,
            pred_len=pred_len, T=1.0, top_p=0.9, sample_count=1, verbose=False,
        )
        out.append(pred["close"].to_numpy())
    return np.array(out)


def rolling_forecast(df, model="mini", lookback=64, pred_len=5, samples=4, stride=5, warmup=None):
    """Per-bar forecast frame (exp_ret, vol) refreshed every `stride` bars, forward-filled.

    exp_ret: mean predicted return to the end of the horizon
    vol:     cross-sample std of predicted log return (forecast dispersion)
    """
    n = len(df)
    warmup = lookback if warmup is None else warmup
    er = np.full(n, np.nan)
    vo = np.full(n, np.nan)
    close = df["close"].to_numpy(dtype=float)
    for t in range(warmup, n, stride):
        paths = forecast_paths(df, t, model, lookback, pred_len, samples)
        r = np.log(np.maximum(paths[:, -1], 1e-9) / close[t])
        er[t] = float(np.mean(r))
        vo[t] = float(np.std(r))
    return pd.DataFrame({"exp_ret": er, "vol": vo}, index=df.index).ffill()
