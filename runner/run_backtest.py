"""cmsq fixed backtest runner (version pinned; see runner/requirements.lock).

Contract
--------
/job/job.json        read-only  {"symbol", "params", "costs", "split", "data_file"}
/job/strategy.py     read-only  must define generate_signals(df, **params) -> pd.Series
                                 target position in [0, 1] per bar (long-only, A-share)
/data/*.csv          read-only  OHLCV: date,open,high,low,close,volume
/vendor/huquant      read-only  vendored huquant trade_dates.py (SSE calendar)
/out/result.json     writable   standard result (schema_version 1)

Anti-lookahead: positions are ALWAYS shifted by one bar by the runner, so a
signal computed on bar t's close trades at bar t+1. Strategy code cannot opt out.
"""
import importlib.util
import json
import math
import os
import sys
import time
import traceback

import numpy as np
import pandas as pd

RUNNER_VERSION = "cmsq-runner/1.0.0"
OUT = os.environ.get("CMSQ_OUT", "/out")
JOB = os.environ.get("CMSQ_JOB", "/job")
DATA = os.environ.get("CMSQ_DATA", "/data")
VENDOR = os.environ.get("CMSQ_VENDOR", "/vendor/huquant")


def write(result):
    with open(os.path.join(OUT, "result.json"), "w") as f:
        json.dump(result, f, indent=2, ensure_ascii=False, allow_nan=False)


def clean(x):
    if x is None or (isinstance(x, float) and (math.isnan(x) or math.isinf(x))):
        return None
    return float(x)


def calendar_check(dates):
    """Validate bars against huquant's SSE trading calendar (covers 1990-12-19..2019-12-31)."""
    try:
        sys.path.insert(0, VENDOR)
        from trade_dates import trade_date_sse  # vendored, pinned

        cal = set(trade_date_sse)
        last = trade_date_sse[-1]
        in_range = [d for d in dates if d <= last]
        bad = [d for d in in_range if d not in cal]
        return {
            "source": "huquant/trade_dates.py (vendored)",
            "calendar_end": last,
            "bars_checked": len(in_range),
            "bars_beyond_calendar": len(dates) - len(in_range),
            "non_trading_day_bars": len(bad),
            "examples": bad[:5],
        }
    except Exception as e:  # calendar is advisory, never fatal
        return {"source": "huquant", "error": repr(e)}


class LookaheadError(Exception):
    pass


def lookahead_probe(df, full, signals, cuts=(0.3, 0.45, 0.6, 0.7, 0.8, 0.9)):
    """Point-in-time probe. For several cut points k, signals for bars < k must not
    change when the future (bars >= k) is (a) removed, (b) shocked up, (c) shocked
    down. Any difference means the strategy read the future (look-ahead bias)."""
    mismatches = []
    price_cols = ["open", "high", "low", "close"]
    for frac in cuts:
        k = int(len(df) * frac)
        variants = {"truncated": df.iloc[:k]}
        for name, f in (("future_up", 1.3), ("future_down", 0.7)):
            v = df.copy()
            v.iloc[k:, [v.columns.get_loc(c) for c in price_cols]] *= f
            variants[name] = v
        for name, frame in variants.items():
            a = signals(frame).iloc[:k].values
            b = full.iloc[:k].values
            diff = np.where(np.abs(a - b) > 1e-9)[0]
            if len(diff):
                mismatches.append({"cut": str(df.index[k - 1].date()), "variant": name,
                                   "bars_differ": int(len(diff)), "first": str(df.index[diff[0]].date())})
    return {"detected": bool(mismatches), "cuts_checked": len(cuts), "variants": 3, "mismatches": mismatches[:10]}


def caveats(prov):
    out = []
    src = str(prov.get("source", ""))
    if src == "sample":
        out.append("SYNTHETIC sample data: results demonstrate the pipeline, not real alpha")
    if "none" in str(prov.get("adjustment", "")).lower():
        out.append("unadjusted prices: ex-dividend/split gaps distort returns")
    return out


def metrics(ret, pos):
    ret = ret.dropna()
    n = len(ret)
    if n < 2:
        return {"bars": n}
    eq = (1 + ret).cumprod()
    total = float(eq.iloc[-1] - 1)
    ann = float((1 + total) ** (252 / n) - 1) if total > -1 else -1.0
    sd = float(ret.std(ddof=1))
    sharpe = float(ret.mean() / sd * math.sqrt(252)) if sd > 0 else 0.0
    dd = float((eq / eq.cummax() - 1).min())
    turnover = float(pos.diff().abs().sum() / n * 252)  # annualised one-way
    return {
        "bars": n,
        "total_return": clean(total),
        "annual_return": clean(ann),
        "sharpe": clean(sharpe),
        "max_drawdown": clean(dd),
        "turnover_annual": clean(turnover),
        "exposure": clean(float((pos > 0).mean())),
    }


def main():
    t0 = time.time()
    job = json.load(open(os.path.join(JOB, "job.json")))
    df = pd.read_csv(os.path.join(DATA, job["data_file"]), parse_dates=["date"])
    df = df.sort_values("date").drop_duplicates("date").set_index("date")
    for c in ("open", "high", "low", "close", "volume"):
        df[c] = df[c].astype(float)

    spec = importlib.util.spec_from_file_location("strategy", os.path.join(JOB, "strategy.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    if not hasattr(mod, "generate_signals"):
        raise RuntimeError("strategy.py does not define generate_signals(df, **params)")

    params = job.get("params", {})

    def signals(frame):
        out = mod.generate_signals(frame.copy(), **params)
        return pd.Series(out, index=frame.index).astype(float).fillna(0.0).clip(0.0, 1.0)

    raw = signals(df)
    leak = lookahead_probe(df, raw, signals)
    if leak["detected"]:
        raise LookaheadError(leak)
    pos = raw.shift(1).fillna(0.0)  # enforced: trade next bar

    c = job["costs"]
    buy_cost = c["commission"] + c["slippage"]
    sell_cost = c["commission"] + c["slippage"] + c["stamp_duty_sell"]
    dpos = pos.diff().fillna(pos.iloc[0])
    cost = dpos.clip(lower=0) * buy_cost + (-dpos.clip(upper=0)) * sell_cost
    asset_ret = df["close"].pct_change().fillna(0.0)
    gross = pos * asset_ret
    net = gross - cost
    net_2x = gross - 2 * cost  # cost-stress scenario

    split_at = int(len(df) * job["split"]["train_frac"])
    idx = df.index
    is_net, oos_net = net.iloc[:split_at], net.iloc[split_at:]
    is_pos, oos_pos = pos.iloc[:split_at], pos.iloc[split_at:]
    half = len(oos_net) // 2

    result = {
        "schema_version": 1,
        "runner": RUNNER_VERSION,
        "python": sys.version.split()[0],
        "pandas": pd.__version__,
        "numpy": np.__version__,
        "symbol": job["symbol"],
        "data_file": job["data_file"],
        "data_provenance": job.get("data_provenance"),
        "data_range": {"start": str(idx[0].date()), "end": str(idx[-1].date()), "bars": len(df)},
        "split": {
            "method": "chronological",
            "train_frac": job["split"]["train_frac"],
            "train": {"start": str(idx[0].date()), "end": str(idx[split_at - 1].date())},
            "test": {"start": str(idx[split_at].date()), "end": str(idx[-1].date())},
        },
        "point_in_time": True,
        "lookahead_probe": leak,
        "execution": "signal at close t, filled at close t+1 (runner-enforced shift)",
        "cost_model": dict(c, description="A-share: commission both sides, stamp duty sell only, fixed slippage"),
        "params": job.get("params", {}),
        "in_sample": metrics(is_net, is_pos),
        "out_of_sample": metrics(oos_net, oos_pos),
        "out_of_sample_gross": metrics(gross.iloc[split_at:], oos_pos),
        "out_of_sample_cost_2x": metrics(net_2x.iloc[split_at:], oos_pos),
        "data_caveats": caveats(job.get("data_provenance") or {}),
        "oos_halves": [metrics(oos_net.iloc[:half], oos_pos.iloc[:half]), metrics(oos_net.iloc[half:], oos_pos.iloc[half:])],
        "buy_and_hold_oos": metrics(asset_ret.iloc[split_at:], pd.Series(1.0, index=idx[split_at:])),
        "latest_signal": {"date": str(idx[-1].date()), "target_position": clean(float(raw.iloc[-1]))},
        "calendar_check": calendar_check([str(d.date()) for d in idx]),
        "elapsed_sec": round(time.time() - t0, 3),
        "status": "ok",
    }
    write(result)
    print(json.dumps({"status": "ok", "oos": result["out_of_sample"]}))


if __name__ == "__main__":
    try:
        main()
    except LookaheadError as e:
        print("LOOKAHEAD DETECTED: " + json.dumps(e.args[0]), file=sys.stderr)
        write({"schema_version": 1, "runner": RUNNER_VERSION, "status": "lookahead_detected",
               "error": "signals change when future bars are removed (look-ahead bias)", "lookahead_probe": e.args[0]})
        sys.exit(3)
    except Exception as e:
        tb = traceback.format_exc()
        print(tb, file=sys.stderr)
        try:
            write({"schema_version": 1, "runner": RUNNER_VERSION, "status": "error", "error": repr(e), "traceback": tb[-4000:]})
        except Exception:
            pass
        sys.exit(2)
