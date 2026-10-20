# CMspark Quant Prototype

Rust + **GPUI Kit** (Zed GPUI + Longbridge components) dashboard prototype for the quant strategy discovery system.

## Features (current prototype)

- Native high-performance desktop dashboard
- Sidebar navigation: Overview / Strategy Library / Sandbox Jobs / Signals
- Mock data tables and status cards for strategies and sandbox jobs
- Structured for future integration with:
  - Periodic scraping (via CMspark Chrome agent)
  - Strategy extraction & combination (LLM + evolutionary/genetic)
  - Isolated sandbox backtesting (Docker / future AX)
  - Signal generation & IB paper/live (with hard gates)

## Run

```bash
cargo run
```

Requires Rust 1.92+ and platform libraries for GPUI (see [gpui-kit installation](https://gpui-kit.com/docs/installation)).

## Next steps

1. Wire real data sources and persistence.
2. Add Docker sandbox executor (MCP/tool for the agent).
3. Integrate backtest engine (Qlib / vectorbt / custom).
4. Connect scraping tasks from CMspark.
5. Add confirmation gates for any trading actions.

## Create the GitHub repo

```bash
# On your machine after cloning or copying this directory
gh auth login
gh repo create nehcuh/cmspark-quant --public --source=. --remote=origin --push
```

Or create the empty repo on GitHub web UI and `git push`.

---

Prototype generated for exploration of the quant agent + sandbox idea on top of CMspark.
