#!/usr/bin/env python3
"""Host-side helper for python-only data providers (akshare / baostock).
Usage: data_helper.py <akshare|baostock> <SH600519> <YYYYMMDD start> <YYYYMMDD end>
Prints JSON {"bars": [[date,open,high,low,close,volume_shares],...], "meta": {...}} on stdout.
Unadjusted prices only (used as backup / cross-validation source). Never run inside the sandbox."""
import json, sys, io, contextlib

def akshare(mkt, code, start, end):
    """Sina daily endpoint first (shares), Eastmoney as fallback."""
    import akshare as ak
    errs = []
    try:
        df = ak.stock_zh_a_daily(symbol=f"{mkt.lower()}{code}", start_date=start, end_date=end, adjust="")
        rows = [[str(r["date"])[:10], float(r["open"]), float(r["high"]), float(r["low"]), float(r["close"]), float(r["volume"])]
                for _, r in df.iterrows()]
        return rows, {"endpoint": "sina stock_zh_a_daily", "akshare": ak.__version__, "volume_unit": "shares"}
    except Exception as e:  # noqa: BLE001
        errs.append(f"sina: {e}")
    try:
        df = ak.stock_zh_a_hist(symbol=code, period="daily", start_date=start, end_date=end, adjust="")
        rows = [[str(r["日期"])[:10], float(r["开盘"]), float(r["最高"]), float(r["最低"]), float(r["收盘"]), float(r["成交量"]) * 100]
                for _, r in df.iterrows()]
        return rows, {"endpoint": "eastmoney stock_zh_a_hist", "akshare": ak.__version__, "volume_unit": "shares (手×100)", "fallback_from": errs}
    except Exception as e:  # noqa: BLE001
        errs.append(f"eastmoney: {e}")
    raise SystemExit("akshare: all endpoints failed: " + " | ".join(x[:200] for x in errs))

def baostock(mkt, code, start, end):
    import baostock as bs
    with contextlib.redirect_stdout(io.StringIO()):
        lg = bs.login()
    if lg.error_code != "0":
        raise SystemExit(f"baostock login failed: {lg.error_msg}")
    try:
        f = lambda d: f"{d[:4]}-{d[4:6]}-{d[6:]}"
        rs = bs.query_history_k_data_plus(f"{mkt.lower()}.{code}", "date,open,high,low,close,volume,tradestatus",
                                          start_date=f(start), end_date=f(end), frequency="d", adjustflag="3")
        if rs.error_code != "0":
            raise SystemExit(f"baostock query failed: {rs.error_msg}")
        rows = []
        while rs.next():
            d, o, h, l, c, v, st = rs.get_row_data()
            if st != "1" or not o:  # suspended rows carry no prices
                continue
            rows.append([d, float(o), float(h), float(l), float(c), float(v or 0)])
        return rows, {"endpoint": "baostock query_history_k_data_plus adjustflag=3", "volume_unit": "shares"}
    finally:
        with contextlib.redirect_stdout(io.StringIO()):
            bs.logout()

def akshare_fut(contract, start, end):
    """Raw daily bars of ONE futures contract (sina), for cross-validation."""
    import akshare as ak
    df = ak.futures_zh_daily_sina(symbol=contract)
    rows = [[str(r["date"])[:10], float(r["open"]), float(r["high"]), float(r["low"]), float(r["close"]), float(r["volume"])]
            for _, r in df.iterrows() if start <= str(r["date"])[:10].replace("-", "") <= end]
    return rows, {"endpoint": "sina futures_zh_daily_sina", "akshare": ak.__version__, "contract": contract}

if __name__ == "__main__":
    src, sym, start, end = sys.argv[1:5]
    if sym.startswith("FUT:"):
        c = sym[4:]
        assert src == "akshare" and 3 <= len(c) <= 8 and c.isalnum(), "bad contract"
        with contextlib.redirect_stdout(sys.stderr):
            rows, meta = akshare_fut(c, start, end)
        print(json.dumps({"bars": rows, "meta": meta}))
        sys.exit(0)
    mkt, code = sym[:2].upper(), sym[2:]
    assert mkt in ("SH", "SZ", "BJ") and len(code) == 6 and code.isdigit(), "bad symbol"
    with contextlib.redirect_stdout(sys.stderr):
        rows, meta = {"akshare": akshare, "baostock": baostock}[src](mkt, code, start, end)
    print(json.dumps({"bars": rows, "meta": meta}))
