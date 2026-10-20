"""Generate SYNTHETIC sample A-share-like daily OHLCV (deterministic, seed fixed).
Dates = SSE trading days from vendored huquant calendar (2015-01-05..2019-12-31).
NOT real market data. For offline demo/tests only."""
import json, sys, os
import numpy as np, pandas as pd
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "vendor", "huquant"))
from trade_dates import trade_date_sse
dates = [d for d in trade_date_sse if "2015-01-05" <= d <= "2019-12-31"]
out = os.path.join(os.path.dirname(__file__), "..", "fixtures", "market")
specs = {
    # regime-switching drift: trending segments -> trend strategies have a chance
    "SAMPLE_TREND": dict(seed=7, vol=0.016, regimes=[0.0012, -0.0009, 0.0010, -0.0004, 0.0011]),
    # strongly mean-reverting around slow drift
    "SAMPLE_RANGE": dict(seed=11, vol=0.018, regimes=[0.0002], ar=-0.25),
}
for sym, s in specs.items():
    rng = np.random.default_rng(s["seed"])
    n = len(dates); seg = np.array_split(np.arange(n), len(s["regimes"]))
    mu = np.zeros(n)
    for r, ix in zip(s["regimes"], seg): mu[ix] = r
    eps = rng.normal(0, s["vol"], n); ret = np.zeros(n)
    for i in range(n):
        ret[i] = mu[i] + eps[i] + (s.get("ar", 0) * ret[i-1] if i else 0)
    close = 10 * np.exp(np.cumsum(ret))
    open_ = close * np.exp(rng.normal(0, 0.004, n))
    high = np.maximum(open_, close) * (1 + np.abs(rng.normal(0, 0.006, n)))
    low = np.minimum(open_, close) * (1 - np.abs(rng.normal(0, 0.006, n)))
    vol = rng.integers(2e6, 9e6, n)
    df = pd.DataFrame(dict(date=dates, open=open_.round(3), high=high.round(3), low=low.round(3), close=close.round(3), volume=vol))
    df.to_csv(os.path.join(out, f"{sym}.csv"), index=False)
    json.dump({"symbol": sym, "source": "synthetic", "label": "SAMPLE - SYNTHETIC DATA, NOT REAL MARKET PRICES",
               "generator": "scripts/gen_sample_data.py", "seed": s["seed"], "calendar": "huquant SSE trade_date_sse",
               "start": dates[0], "end": dates[-1], "bars": n, "adjustment": "n/a"},
              open(os.path.join(out, f"{sym}.meta.json"), "w"), indent=2, ensure_ascii=False)
    print(sym, n, close[-1].round(2))

# SAMPLE_REGIME: persistent Markov bull/bear regimes with a realistic open:
# open = prev close x overnight gap, close = open x intraday move, so next-open
# fills are not free. Trend-following has a STRUCTURAL edge here by construction
# (regime persistence), which is what lets the demo show a full
# GatePassed -> vault -> Promoted path. Seed fixed up front (not searched).
def regime_series(seed=2026, p_stay=0.995, bull=(0.0020, 0.010), bear=(-0.0025, 0.016)):
    rng = np.random.default_rng(seed)
    n = len(dates); state = 0; c = 10.0; rows = []
    for i in range(n):
        if rng.random() > p_stay: state = 1 - state
        mu, sd = bull if state == 0 else bear
        r = np.clip(mu + rng.normal(0, sd), -0.095, 0.095)
        gap = np.clip(r * 0.35 + rng.normal(0, 0.003), -0.095, 0.095)
        o = c * np.exp(gap) if i else c
        cl = c * np.exp(r)
        h = max(o, cl) * (1 + abs(rng.normal(0, 0.005))); l = min(o, cl) * (1 - abs(rng.normal(0, 0.005)))
        rows.append((dates[i], round(o, 3), round(h, 3), round(l, 3), round(cl, 3), int(rng.integers(2e6, 9e6))))
        c = cl
    df = pd.DataFrame(rows, columns=["date", "open", "high", "low", "close", "volume"])
    df.to_csv(os.path.join(out, "SAMPLE_REGIME.csv"), index=False)
    json.dump({"symbol": "SAMPLE_REGIME", "source": "synthetic", "label": "SAMPLE - SYNTHETIC DATA, NOT REAL MARKET PRICES",
               "generator": "scripts/gen_sample_data.py (regime_series)", "seed": seed, "calendar": "huquant SSE trade_date_sse",
               "start": dates[0], "end": dates[-1], "bars": n, "adjustment": "n/a"},
              open(os.path.join(out, "SAMPLE_REGIME.meta.json"), "w"), indent=2, ensure_ascii=False)
    print("SAMPLE_REGIME", n, round(c, 2))

regime_series()
