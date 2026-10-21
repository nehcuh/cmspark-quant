# rstdx

[![中文文档](README.zh.md)](README.zh.md)

Pure Rust implementation of TongDaXin (TDX) stock data protocol with Python bindings.

## Features

- High-performance TDX protocol implementation in Rust
- Synchronous and asynchronous Python APIs
- **K-line data**: Daily, weekly, monthly, and minute-level OHLCV data
- **Real-time quotes**: Stock snapshot with bid/ask levels
- **Financial data**: Comprehensive financial information including balance sheet and income statement
- **Security list**: Query all securities in a market
- Connection pool with automatic server discovery and latency-based selection
- Type-safe Python bindings with type stubs

## Installation

```bash
pip install rstdx
```

## Quick Start

### Synchronous API

```python
from rstdx import TdxClient, MARKET_SH, KLINE_TYPE_DAILY

client = TdxClient("114.80.63.12:7709")
bars = client.get_bars(KLINE_TYPE_DAILY, MARKET_SH, "600000", 0, 100)
for bar in bars:
    print(f"{bar['datetime']}: O={bar['open']} H={bar['high']} L={bar['low']} C={bar['close']}")
client.close()
```

### Asynchronous API

```python
import asyncio
from rstdx import AsyncTdxClient, MARKET_SH, KLINE_TYPE_DAILY

async def main():
    client = await AsyncTdxClient.connect("114.80.63.12:7709")

    # Get K-line data
    bars = await client.get_bars(KLINE_TYPE_DAILY, MARKET_SH, "600000", 0, 100)
    for bar in bars:
        print(f"{bar['datetime']}: O={bar['open']} C={bar['close']}")

    # Get real-time quotes
    quotes = await client.get_security_quotes([(0, "000001"), (1, "600000")])
    for quote in quotes:
        print(f"{quote['code']}: {quote['price']}")

    # Get financial info
    info = await client.get_finance_info(1, "600000")
    print(f"Total assets: {info['total_assets']}, Net profit: {info['net_profit']}")

    client.close()

asyncio.run(main())
```

## Available Constants

### Market Codes

- `MARKET_SH` (1): Shanghai Stock Exchange
- `MARKET_SZ` (0): Shenzhen Stock Exchange
- `MARKET_BJ` (2): Beijing Stock Exchange (北交所/新三板精选层)

### K-line Types

- `KLINE_TYPE_1MIN` (8): 1-minute K-line
- `KLINE_TYPE_5MIN` (0): 5-minute K-line
- `KLINE_TYPE_15MIN` (1): 15-minute K-line
- `KLINE_TYPE_30MIN` (2): 30-minute K-line
- `KLINE_TYPE_1HOUR` (3): 1-hour K-line
- `KLINE_TYPE_DAILY` (4): Daily K-line
- `KLINE_TYPE_WEEKLY` (5): Weekly K-line
- `KLINE_TYPE_MONTHLY` (6): Monthly K-line

## API Reference

### Getting Real-time Quotes

Get stock snapshot data with current price, bid/ask levels:

```python
from rstdx import TdxClient

client = TdxClient("114.80.63.12:7709")

# Get quotes for multiple stocks
stocks = [(0, "000001"), (1, "600000")]  # (market, code)
quotes = client.get_security_quotes(stocks)

for quote in quotes:
    print(f"{quote['code']}: {quote['price']}")
    print(f"  Bid: {quote['bid1']} x {quote['bid_vol1']}")
    print(f"  Ask: {quote['ask1']} x {quote['ask_vol1']}")

client.close()
```

### Getting Financial Information

Get comprehensive financial data for a stock:

```python
from rstdx import TdxClient

client = TdxClient("114.80.63.12:7709")

# Get financial info for a stock
info = client.get_finance_info(1, "600000")  # market=1 (Shanghai), code="600000"

print(f"Code: {info['code']}")
print(f"IPO Date: {info['ipo_date']}")
print(f"Total Shares: {info['total_shares']}")
print(f"Total Assets: {info['total_assets']}")
print(f"Net Assets: {info['net_assets']}")
print(f"Net Profit: {info['net_profit']}")
print(f"Net Assets per Share: {info['net_assets_per_share']}")

client.close()
```

### Getting Available Server Pool

```python
from rstdx import ConnectionPool

pool = ConnectionPool()

# Refresh server list and probe all servers
count = pool.refresh_servers()
print(f"Refreshed {count} servers")

# Get list of available servers with latency info
servers = pool.get_servers()
for server in servers:
    print(f"{server['host']}:{server['port']} - latency: {server['avg_latency']:.2f}ms")

# Get the fastest server
best = pool.get_best_server()
if best:
    print(f"Best server: {best['host']}:{best['port']}")

# Connect to the best server
client = pool.connect()
```

### Querying Data by Date

The TDX API returns data from newest to oldest. Use helper functions to query specific dates:

```python
from rstdx import TdxClient, MARKET_SH, KLINE_TYPE_DAILY
from rstdx import helpers

client = TdxClient("114.80.63.12:7709")

# Get data for a specific date
klines = helpers.get_bars_for_date(
    client, MARKET_SH, "600000", "2024-01-15",
    category=KLINE_TYPE_DAILY, count=800
)

# Get data for a date range
klines = helpers.get_historical_bars(
    client, MARKET_SH, "600000",
    "2024-01-01", "2024-01-31",
    category=KLINE_TYPE_DAILY, count=800
)

# Get latest bars as Kline models (with Pydantic validation)
klines = helpers.get_latest_bars(
    client, MARKET_SH, "600000",
    category=KLINE_TYPE_DAILY, count=100
)

for kline in klines:
    print(f"{kline.datetime}: O={kline.open} C={kline.close}")

client.close()
```

### Helper Functions

- `get_bars_for_date(client, market, code, target_date, category, count)` - Get bars for a specific date
- `get_historical_bars(client, market, code, start_date, end_date, category, count)` - Get bars within a date range
- `get_latest_bars(client, market, code, category, count)` - Get latest bars as Kline models
- `find_bar_by_date(bars, target_date)` - Find a bar in a list by date
- `find_bars_in_range(bars, start_date, end_date)` - Find bars within a date range
- `bars_to_klines(bars)` - Convert bar dicts to Kline models
- `parse_kline_datetime(dt_str)` - Parse datetime string to datetime object

## Data Reference

### Security Quote Fields

| Field | Description |
|-------|-------------|
| `price` | Current price |
| `last_close` | Previous close price |
| `open` | Opening price |
| `high` | Highest price |
| `low` | Lowest price |
| `volume` | Trading volume (in lots) |
| `amount` | Trading amount (yuan) |
| `bid1-5` | Bid prices at 5 levels |
| `ask1-5` | Ask prices at 5 levels |
| `bid_vol1-5` | Bid volumes at 5 levels |
| `ask_vol1-5` | Ask volumes at 5 levels |

### Financial Info Fields

| Category | Fields |
|----------|--------|
| **Basic Info** | `code`, `market`, `market_name`, `ipo_date`, `updated_date`, `province`, `industry` |
| **Equity Structure** | `total_shares`, `floating_shares`, `state_shares`, `legal_shares`, `b_shares`, `h_shares` |
| **Assets** | `total_assets`, `current_assets`, `fixed_assets`, `intangible_assets` |
| **Liabilities** | `current_liabilities`, `long_term_liabilities`, `capital_reserve`, `net_assets` |
| **Income Statement** | `main_revenue`, `main_profit`, `operating_profit`, `investment_income`, `total_profit`, `net_profit` |
| **Cash Flow** | `operating_cash_flow`, `total_cash_flow` |
| **Per Share Data** | `net_assets_per_share` |

## High-Level Query API

The `QueryApi` provides an intuitive interface for querying stock data by date ranges:

```python
from rstdx import QueryApi

api = QueryApi()

# Query daily bars by date range
bars = api.get_bars("600000", "2024-01-01", "2024-01-31", "day")
for bar in bars:
    print(f"{bar['date']}: {bar['close']}")

# Query minute bars
bars = api.get_bars("600000", "2024-01-15", "2024-01-15", "5min")

# Get real-time quotes
quotes = api.get_quotes(["600000", "000001"])

# Trading calendar utilities
is_trading = api.is_trading_day("2024-01-15")
prev_day = api.previous_trading_day("2024-01-15")
next_day = api.next_trading_day("2024-01-15")

api.close()
```

### Supported Frequencies

- `"1min"`, `"5min"`, `"15min"`, `"30min"`, `"60min"` - Minute bars
- `"day"` - Daily bars
- `"week"` - Weekly bars
- `"month"` - Monthly bars

## CLI Usage

```bash
# Get K-line data
rstdx bars --code 600000 --category 4 --count 100

# Get quotes
rstdx quotes --codes 600000,000001

# Get financial info
rstdx finance --code 600000

# Query by date range
rstdx query --code 600000 --start 2024-01-01 --end 2024-01-31 --frequency day

# Trading calendar operations
rstdx trade-date check 2024-01-15
rstdx trade-date gap 2024-01-01 2024-01-31
rstdx trade-date adjacent 2024-01-15 --next
rstdx trade-date update

# List servers
rstdx servers --detailed
```

## License

MIT

---
