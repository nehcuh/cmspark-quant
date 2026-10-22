# CMspark Quant

本地优先的量化策略**发现、沙箱验证与模拟盘**系统：从社区抓取策略，在断网沙箱里回测，用一组严格的防过拟合门槛筛选，
人工确认后只产生**观察信号**和**模拟盘**。**没有任何真实下单路径。**

```bash
git clone https://github.com/nehcuh/cmspark-quant && cd cmspark-quant
scripts/deploy.sh                     # 一键部署，详见 docs/QUICKSTART.md
set -a; . ~/.config/cmsq/cmsq.env; set +a
cmsq list
```

![总览](docs/screenshots/01-overview.png)

## 文档

**指挥者（使用者）按这个顺序读：**
1. [docs/QUICKSTART.md](docs/QUICKSTART.md)：从零到看板上的第一个结果，日常流程，常见问题
2. [docs/DESIGN.md](docs/DESIGN.md)：设计逻辑与理念：为什么要这么多门槛、为什么大多数策略被拒、为什么不实盘
3. [docs/STATUS.md](docs/STATUS.md)：现在做到哪了、真实数据上的结果、已知缺口
4. [docs/CONFIG.md](docs/CONFIG.md)：所有命令、环境变量、配置文件、数据源限流、门槛阈值的速查表

**开发者另外读：**
- [docs/USER_GUIDE.md](docs/USER_GUIDE.md)：部署组件与运维、门槛实现细节（regime、按折重选参、GA）
- 专题：[AUDIT_KEY](docs/AUDIT_KEY.md)（审计密钥与锚点）· [MARKET_DATA](docs/MARKET_DATA.md)（行情与复权）· [PAPER_TRADING](docs/PAPER_TRADING.md)（模拟撮合）·
  [SIGNALS](docs/SIGNALS.md)（信号刷新）· [REDIS](docs/REDIS.md) · [SCRAPING](docs/SCRAPING.md) · [DROP_FORMAT](docs/DROP_FORMAT.md) · [UI_DESIGN](docs/UI_DESIGN.md)（看板设计规范）
- 历史：[CHANGELOG](docs/CHANGELOG.md)（开发日志）· [PLAN](docs/PLAN.md) · `docs/reviews/`（对抗审查记录）

## 代码结构
| 模块 | 位置 |
|---|---|
| 核心（摄入、抽取、沙箱、计分、门槛、状态机、存储、审计、行情） | `crates/cmsq-core` |
| 命令行 `cmsq` | `crates/cmsq-cli` |
| 看板（GPUI + gpui-component） | `crates/cmsq-dashboard` |
| 模拟撮合 | `crates/cmsq-paper` |
| 沙箱内回测 runner（含 vn.py 兼容层） | `runner/` |
| 部署（compose、seccomp、systemd） | `deploy/` |
| 样例与攻击回归 | `fixtures/` |

开发验证：`cargo test --workspace`、`scripts/test_docker.sh`、`scripts/test_pg.sh`、`scripts/e2e.sh`。

## Docker setup
当前用户必须能访问 Docker daemon：`sudo usermod -aG docker $USER` 后重新登录，或在当前 shell 用 `sg docker -c "<命令>"`。
确认：`docker info`。详见 [QUICKSTART.md §0](docs/QUICKSTART.md#0-前置条件一次性) 与 §7 常见问题。
