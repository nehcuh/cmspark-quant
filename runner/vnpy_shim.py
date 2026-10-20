"""Minimal vn.py CtaTemplate compatibility layer (UNTRUSTED side, sandbox only).

It lets a vn.py-style strategy (class Foo(CtaTemplate) with on_bar) run inside
the positions-only runner. Nothing here places orders: buy/sell/short/cover
only move the strategy's virtual `pos`, and after every bar the runner records
`pos` as the target position for the next bar. The trusted host then applies
its own fills, costs, contract rules and look-ahead probe.

Approximations (documented in docs/DROP_FORMAT.md):
  * every order (limit or stop) is assumed filled at the bar it was sent on;
    the host executes the resulting target at the NEXT bar's open, so there is
    no same-bar fill advantage;
  * target = clip(pos / fixed_size, position_min, 1) where fixed_size is the
    strategy's `fixed_size` attribute (default 1);
  * only daily bars are fed; BarGenerator passes bars straight through
    (window bars are aggregated by count).
"""
from dataclasses import dataclass, field
from datetime import datetime
from enum import Enum
import sys
import types

import numpy as np


class Direction(Enum):
    LONG = "多"
    SHORT = "空"
    NET = "净"


class Offset(Enum):
    NONE = ""
    OPEN = "开"
    CLOSE = "平"
    CLOSETODAY = "平今"
    CLOSEYESTERDAY = "平昨"


class Interval(Enum):
    MINUTE = "1m"
    HOUR = "1h"
    DAILY = "d"
    WEEKLY = "w"


class Exchange(Enum):
    SHFE = "SHFE"
    DCE = "DCE"
    CZCE = "CZCE"
    CFFEX = "CFFEX"
    INE = "INE"
    GFEX = "GFEX"
    SSE = "SSE"
    SZSE = "SZSE"
    LOCAL = "LOCAL"


@dataclass
class BarData:
    symbol: str = "SIM"
    exchange: Exchange = Exchange.LOCAL
    datetime: datetime = None
    interval: Interval = Interval.DAILY
    volume: float = 0.0
    turnover: float = 0.0
    open_interest: float = 0.0
    open_price: float = 0.0
    high_price: float = 0.0
    low_price: float = 0.0
    close_price: float = 0.0
    gateway_name: str = "SIM"

    @property
    def vt_symbol(self):
        return "%s.%s" % (self.symbol, self.exchange.value)


@dataclass
class TickData:
    symbol: str = "SIM"
    exchange: Exchange = Exchange.LOCAL
    datetime: datetime = None
    last_price: float = 0.0
    volume: float = 0.0
    gateway_name: str = "SIM"


@dataclass
class OrderData:
    symbol: str = "SIM"
    exchange: Exchange = Exchange.LOCAL
    orderid: str = ""
    direction: Direction = None
    offset: Offset = Offset.NONE
    price: float = 0.0
    volume: float = 0.0
    gateway_name: str = "SIM"


@dataclass
class TradeData(OrderData):
    tradeid: str = ""


@dataclass
class StopOrder:
    vt_symbol: str = ""
    direction: Direction = None
    offset: Offset = None
    price: float = 0.0
    volume: float = 0.0
    stop_orderid: str = ""
    strategy_name: str = ""
    vt_orderids: list = field(default_factory=list)


class CtaTemplate:
    author = ""
    parameters = []
    variables = []

    def __init__(self, cta_engine, strategy_name, vt_symbol, setting):
        self.cta_engine = cta_engine
        self.strategy_name = strategy_name
        self.vt_symbol = vt_symbol
        self.inited = False
        self.trading = False
        self.pos = 0
        self._oid = 0
        self.variables = ["inited", "trading", "pos"] + [v for v in type(self).variables if v not in ("inited", "trading", "pos")]
        for k in type(self).parameters:
            if k in setting:
                setattr(self, k, setting[k])

    # lifecycle hooks (overridden by strategies)
    def on_init(self):
        pass

    def on_start(self):
        pass

    def on_stop(self):
        pass

    def on_tick(self, tick):
        pass

    def on_bar(self, bar):
        pass

    def on_trade(self, trade):
        pass

    def on_order(self, order):
        pass

    def on_stop_order(self, stop_order):
        pass

    # order API: virtual fills only
    def _fill(self, direction, offset, price, volume):
        if not self.trading:
            return []
        volume = abs(float(volume))
        if direction == Direction.LONG:
            self.pos += volume
        else:
            self.pos -= volume
        self._oid += 1
        oid = "SIM.%d" % self._oid
        t = TradeData(orderid=oid, tradeid=oid, direction=direction, offset=offset, price=float(price), volume=volume)
        self.on_trade(t)
        return [oid]

    def buy(self, price, volume, stop=False, lock=False, net=False):
        return self._fill(Direction.LONG, Offset.OPEN, price, volume)

    def sell(self, price, volume, stop=False, lock=False, net=False):
        return self._fill(Direction.SHORT, Offset.CLOSE, price, min(abs(volume), max(self.pos, 0)))

    def short(self, price, volume, stop=False, lock=False, net=False):
        return self._fill(Direction.SHORT, Offset.OPEN, price, volume)

    def cover(self, price, volume, stop=False, lock=False, net=False):
        return self._fill(Direction.LONG, Offset.CLOSE, price, min(abs(volume), max(-self.pos, 0)))

    def send_order(self, direction, offset, price, volume, stop=False, lock=False, net=False):
        return self._fill(direction, offset, price, volume)

    def cancel_order(self, vt_orderid):
        pass

    def cancel_all(self):
        pass

    def load_bar(self, days, interval=Interval.MINUTE, callback=None, use_database=False):
        pass

    def load_tick(self, days):
        pass

    def write_log(self, msg):
        pass

    def put_event(self):
        pass

    def send_email(self, msg):
        pass

    def sync_data(self):
        pass

    def get_engine_type(self):
        return "BACKTESTING"

    def get_pricetick(self):
        return 1.0

    def get_size(self):
        return 1


class CtaSignal:
    def __init__(self):
        self.signal_pos = 0

    def on_tick(self, tick):
        pass

    def on_bar(self, bar):
        pass

    def set_signal_pos(self, pos):
        self.signal_pos = pos

    def get_signal_pos(self):
        return self.signal_pos


class TargetPosTemplate(CtaTemplate):
    def set_target_pos(self, target_pos):
        if self.trading:
            self.pos = target_pos


class BarGenerator:
    def __init__(self, on_bar, window=0, on_window_bar=None, interval=Interval.MINUTE, daily_end=None):
        self.on_bar = on_bar
        self.window = window or 1
        self.on_window_bar = on_window_bar
        self._buf = []

    def update_tick(self, tick):
        pass

    def update_bar(self, bar):
        if not self.on_window_bar:
            return
        self._buf.append(bar)
        if len(self._buf) >= self.window:
            b = self._buf
            self.on_window_bar(BarData(symbol=b[-1].symbol, exchange=b[-1].exchange, datetime=b[-1].datetime,
                                       open_price=b[0].open_price, high_price=max(x.high_price for x in b),
                                       low_price=min(x.low_price for x in b), close_price=b[-1].close_price,
                                       volume=sum(x.volume for x in b), open_interest=b[-1].open_interest))
            self._buf = []

    def generate(self):
        return None


def _sma(x, n):
    return float(np.mean(x[-n:]))


def _ema(x, n):
    a = 2.0 / (n + 1)
    e = x[0]
    out = np.empty_like(x)
    for i, v in enumerate(x):
        e = v if i == 0 else a * v + (1 - a) * e
        out[i] = e
    return out


class ArrayManager:
    def __init__(self, size=100):
        self.count = 0
        self.size = size
        self.inited = False
        self.open_array = np.zeros(size)
        self.high_array = np.zeros(size)
        self.low_array = np.zeros(size)
        self.close_array = np.zeros(size)
        self.volume_array = np.zeros(size)
        self.turnover_array = np.zeros(size)
        self.open_interest_array = np.zeros(size)

    def update_bar(self, bar):
        self.count += 1
        if not self.inited and self.count >= self.size:
            self.inited = True
        for name, v in (("open_array", bar.open_price), ("high_array", bar.high_price), ("low_array", bar.low_price),
                        ("close_array", bar.close_price), ("volume_array", bar.volume), ("turnover_array", bar.turnover),
                        ("open_interest_array", bar.open_interest)):
            a = getattr(self, name)
            a[:-1] = a[1:]
            a[-1] = v

    open = property(lambda self: self.open_array)
    high = property(lambda self: self.high_array)
    low = property(lambda self: self.low_array)
    close = property(lambda self: self.close_array)
    volume = property(lambda self: self.volume_array)

    def _ret(self, arr, array):
        return arr if array else float(arr[-1])

    def sma(self, n, array=False):
        c = self.close_array
        out = np.array([np.mean(c[max(0, i - n + 1):i + 1]) for i in range(len(c))])
        return self._ret(out, array)

    def ema(self, n, array=False):
        return self._ret(_ema(self.close_array, n), array)

    def std(self, n, nbdev=1, array=False):
        c = self.close_array
        out = np.array([np.std(c[max(0, i - n + 1):i + 1]) * nbdev for i in range(len(c))])
        return self._ret(out, array)

    def atr(self, n, array=False):
        h, l, c = self.high_array, self.low_array, self.close_array
        pc = np.concatenate([[c[0]], c[:-1]])
        tr = np.maximum(h - l, np.maximum(abs(h - pc), abs(l - pc)))
        out = np.array([np.mean(tr[max(0, i - n + 1):i + 1]) for i in range(len(tr))])
        return self._ret(out, array)

    def rsi(self, n, array=False):
        c = self.close_array
        d = np.diff(c, prepend=c[0])
        up, dn = np.where(d > 0, d, 0.0), np.where(d < 0, -d, 0.0)
        au = np.array([np.mean(up[max(0, i - n + 1):i + 1]) for i in range(len(c))])
        ad = np.array([np.mean(dn[max(0, i - n + 1):i + 1]) for i in range(len(c))])
        out = np.where(ad == 0, 100.0, 100 - 100 / (1 + au / np.where(ad == 0, 1, ad)))
        return self._ret(out, array)

    def macd(self, fast, slow, signal, array=False):
        m = _ema(self.close_array, fast) - _ema(self.close_array, slow)
        s = _ema(m, signal)
        h = m - s
        return (m, s, h) if array else (float(m[-1]), float(s[-1]), float(h[-1]))

    def cci(self, n, array=False):
        tp = (self.high_array + self.low_array + self.close_array) / 3
        out = np.zeros_like(tp)
        for i in range(len(tp)):
            w = tp[max(0, i - n + 1):i + 1]
            md = np.mean(abs(w - w.mean()))
            out[i] = 0.0 if md == 0 else (tp[i] - w.mean()) / (0.015 * md)
        return self._ret(out, array)

    def momentum(self, n, array=False):
        c = self.close_array
        out = c - np.concatenate([np.full(n, c[0]), c[:-n]])
        return self._ret(out, array)

    def roc(self, n, array=False):
        c = self.close_array
        prev = np.concatenate([np.full(n, c[0]), c[:-n]])
        out = np.where(prev == 0, 0.0, (c - prev) / np.where(prev == 0, 1, prev) * 100)
        return self._ret(out, array)

    def boll(self, n, dev, array=False):
        mid, sd = self.sma(n, True), self.std(n, 1, True)
        up, dn = mid + sd * dev, mid - sd * dev
        return (up, dn) if array else (float(up[-1]), float(dn[-1]))

    def keltner(self, n, dev, array=False):
        mid, a = self.sma(n, True), self.atr(n, True)
        up, dn = mid + a * dev, mid - a * dev
        return (up, dn) if array else (float(up[-1]), float(dn[-1]))

    def donchian(self, n, array=False):
        h, l = self.high_array, self.low_array
        up = np.array([np.max(h[max(0, i - n + 1):i + 1]) for i in range(len(h))])
        dn = np.array([np.min(l[max(0, i - n + 1):i + 1]) for i in range(len(l))])
        return (up, dn) if array else (float(up[-1]), float(dn[-1]))


def install():
    """Register this module under the import paths vn.py strategies use."""
    me = sys.modules[__name__]
    for name in ("vnpy_ctastrategy", "vnpy.app.cta_strategy", "vnpy_ctastrategy.template"):
        sys.modules[name] = me
    for name in ("vnpy.trader.constant", "vnpy.trader.object", "vnpy.trader.utility"):
        sys.modules[name] = me
    for pkg in ("vnpy", "vnpy.app", "vnpy.trader"):
        sys.modules.setdefault(pkg, types.ModuleType(pkg))


def run_cta(mod, df, params, lo):
    cls = None
    for v in vars(mod).values():
        if isinstance(v, type) and issubclass(v, CtaTemplate) and v not in (CtaTemplate, TargetPosTemplate) and "on_bar" in vars(v):
            cls = v
    if cls is None:
        raise RuntimeError("no CtaTemplate subclass with on_bar found")
    s = cls(None, cls.__name__, "SIM.LOCAL", dict(params))
    s.on_init()
    s.inited = True
    s.trading = True
    s.on_start()
    size = float(getattr(s, "fixed_size", 1) or 1)
    out = []
    for d, r in df.iterrows():
        bar = BarData(datetime=d.to_pydatetime(), open_price=r["open"], high_price=r["high"], low_price=r["low"],
                      close_price=r["close"], volume=r["volume"], open_interest=float(r.get("open_interest", 0.0) or 0.0))
        s.on_bar(bar)
        out.append(max(lo, min(1.0, float(s.pos) / size)))
    s.on_stop()
    return out
