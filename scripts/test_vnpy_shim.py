#!/usr/bin/env python3
"""Runs the official vn.py example strategies (fixtures/vnpy_examples, unmodified)
through runner/vnpy_shim.py on synthetic bars; every one must import, trade, and
emit positions in [-1, 1]. Usage: python3 scripts/test_vnpy_shim.py"""
import glob, importlib.util, os, sys
root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(root, "runner"))
import numpy as np, pandas as pd, vnpy_shim as v
v.install()
rng = np.random.default_rng(1); n = 600; c = 3500 + np.cumsum(rng.normal(0, 30, n))
df = pd.DataFrame({"open": c + rng.normal(0, 5, n), "high": c + 20, "low": c - 20, "close": c, "volume": 1000.0},
                  index=pd.date_range("2000-01-03", periods=n, freq="B"))
bad = 0
for f in sorted(glob.glob(os.path.join(root, "fixtures/vnpy_examples/*_strategy.py"))):
    spec = importlib.util.spec_from_file_location("s", f); m = importlib.util.module_from_spec(spec)
    try:
        spec.loader.exec_module(m); o = v.run_cta(m, df, {}, -1.0)
        ch = sum(1 for a, b in zip(o, o[1:]) if a != b)
        ok = len(o) == n and all(-1 <= x <= 1 for x in o) and ch > 0
    except Exception as e:
        ok, ch = False, repr(e)
    print("PASS" if ok else "FAIL", os.path.basename(f), "position changes:", ch)
    bad += not ok
# price tick rounding (vn.py BacktestingEngine round_to) and pyramiding scale
assert v._round_to(3501.3, 1.0) == 3501.0 and v._round_to(10.004, 0.01) == 10.0 and v._round_to(5.0, 0) == 5.0
class T(v.CtaTemplate):
    parameters = ["fixed_size"]; fixed_size = 1
    def on_bar(self, bar):
        self.cancel_all()
        self.buy(bar.close_price + 0.37, 1)
        self.last = self._orders and list(self._orders.values())[-1][3]
mt = type(sys)("t"); mt.T = T
flat = pd.DataFrame({"open": 100.0, "high": 101.0, "low": 99.0, "close": 100.0, "volume": 1.0}, index=pd.date_range("2000-01-03", periods=5, freq="B"))
o = v.run_cta(mt, flat, {"cmsq_max_units": 4}, -1.0, pricetick=0.5)
assert o == [0.0, 0.25, 0.5, 0.75, 1.0], o   # 1 unit per bar, 4 units = full
o1 = v.run_cta(mt, flat, {}, -1.0, pricetick=0.5)
assert o1[1:] == [1.0] * 4, o1
s = T(None, "t", "x", {}); s._pricetick = 0.5; s.trading = True; s.buy(100.3, 1)
assert list(s._orders.values())[0][3] == 100.5 and s.get_pricetick() == 0.5
print("PASS pricetick rounding + cmsq_max_units pyramiding scale")
sys.exit(1 if bad else 0)
