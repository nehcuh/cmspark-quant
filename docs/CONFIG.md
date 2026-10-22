# 配置与命令参考（CONFIG）

所有条目都按代码核对过（`crates/cmsq-cli/src/main.rs`、`crates/cmsq-core/src/**`、`scripts/*.sh`）。`cmsq <命令> --help` 给出同样的信息（英文）。

## 1. 文件与目录

| 路径 | 内容 | 谁写 | 可否进 git |
|---|---|---|---|
| `~/.config/cmsq/`（`CMSQ_SECRETS_DIR`） | 密钥目录，0700，必须在工作区之外 | 部署脚本 | **否** |
| `~/.config/cmsq/secrets.env` | `TUSHARE_TOKEN=...` | 你 | **绝不** |
| `~/.config/cmsq/cmsq.env` | `CMSQ_HOME`、`CMSQ_SECRETS_DIR`、`CMSQ_AUDIT_SIGNER`、`PATH`、（可选）`CMSQ_PYTHON` | `deploy.sh`（每次覆盖） | 否 |
| `~/.config/cmsq/db.env` | `CMSQ_DATABASE_URL`（cmsq_app）、`CMSQ_DATABASE_MIGRATE_URL`（cmsq_owner） | `db_up.sh` | 否 |
| `~/.config/cmsq/pg_app_pw`、`pg_owner_pw` | 数据库口令 | `db_up.sh` | 否 |
| `~/.config/cmsq/redis.env`、`redis_pw` | `CMSQ_REDIS_URL` 与口令 | `redis_up.sh` | 否 |
| `~/.config/cmsq/runner_image_id` | 沙箱镜像 digest，门槛 `runner_pinned` 用 | `build_runner.sh` | 否 |
| `~/.config/cmsq/audit.key` | 旧的应用侧审计密钥（仅未启用签名进程时） | cmsq | **绝不** |
| `~/.config/cmsq/anchors/*.anchor` | 本地审计锚点 | cmsq | 否 |
| `~/.config/cmsq/anchor_signing_ed25519` | 锚点提交的 SSH 签名私钥 | `deploy.sh --anchor-repo` | **绝不**（公钥可加到 GitHub） |
| `~/.config/cmsq/github_token` | 可选，抓取用 GitHub token | 你 | 否 |
| `~/.local/state/cmsq/run/sock`（`CMSQ_RUN_DIR`） | 签名进程 socket | 签名容器 | — |
| `~/.local/share/cmsq/anchors` | 锚点 git 仓库的本地克隆 | `deploy.sh --anchor-repo` | 是（它本身就是锚点仓库） |
| `$CMSQ_HOME`（默认 `deploy.sh` 用 `~/cmsq-home`；直接运行 cmsq 时为 `./var`） | 工作区：`drop/`、`data/`、`strategies/`、`snapshots/` 等 | cmsq | 否 |
| `$CMSQ_HOME/drop/` | 待摄入的策略帖 JSON（格式见 [DROP_FORMAT.md](DROP_FORMAT.md)） | 你 / scrape | — |
| `$CMSQ_HOME/sources.json` | 可选，自定义抓取源（缺省用内置 GitHub 源，见 [SCRAPING.md](SCRAPING.md)） | 你 | — |
| `$CMSQ_HOME/DEMO` | 存在即为 DEMO 工作区（横幅） | 演示脚本 | — |
| `deploy/docker-compose.yml` | db、signer、redis（profile `hot`）服务 | 仓库 | 是 |

## 2. 环境变量

### 常用
| 变量 | 默认 | 作用 |
|---|---|---|
| `CMSQ_HOME` | `./var`（`--home` 优先） | 工作区目录 |
| `CMSQ_SECRETS_DIR` | `$XDG_CONFIG_HOME/cmsq` 或 `~/.config/cmsq` | 密钥目录 |
| `CMSQ_AUDIT_SIGNER` | 未设（部署后为 socket 路径） | 审计签名进程 socket；设了就不用本地密钥 |
| `CMSQ_PYTHON` | `python3` | 运行 akshare/baostock 辅助脚本的 python |
| `CMSQ_DATA_SOURCES` | `tushare,akshare,baostock,tdx` | 行情源优先级（`data fetch --sources` 优先） |
| `CMSQ_TUSHARE_ENV_FILE` | 未设 → `<secrets>/secrets.env` | 另指 token 文件 |
| `CMSQ_CONTAINER_SLOTS` | CPU 核数 | 同时运行的沙箱容器上限 |

### 门槛阈值（改了会记录在每次门槛结果的 `checks.config` 里）
| 变量 | 默认 | 作用 |
|---|---|---|
| `CMSQ_DSR_MIN_TRIALS` | 20 | DSR 试验数 N 的下限 |
| `CMSQ_GATE_MIN_CAPACITY` | 1000000（元） | 容量估计下限 |
| `CMSQ_GATE_REGIME_MIN_BARS` | 60 | 市场状态至少多少根 K 线才计入 |
| `CMSQ_GATE_REGIME_MAX_LOSING` | 1 | 每组最多几个状态允许亏钱 |
| `CMSQ_GATE_REGIME_MAX_LOSS` | -0.10 | 任一状态亏损下限 |
| `CMSQ_ALLOW_SYNTHETIC` | 未设 | `=1` 时合成数据可过 `real_market_data`，**仅 DEMO/测试** |

### 数据库与 Redis
| 变量 | 默认 | 作用 |
|---|---|---|
| `CMSQ_DATABASE_URL` | 读 `<secrets>/db.env` | 运行时连接（cmsq_app） |
| `CMSQ_DATABASE_MIGRATE_URL` | 读 `<secrets>/db.env` | 迁移连接（cmsq_owner），只给 `cmsq db migrate` |
| `CMSQ_DATABASE_SCHEMA` | 按工作区自动 | 指定 schema |
| `CMSQ_DATABASE_POOL` | 16 | 连接池大小 |
| `CMSQ_DB` | 未设 | `=sqlite` 时走离线 SQLite（测试用） |
| `CMSQ_PG_PORT` | 55432 | `db_up.sh` 映射端口 |
| `CMSQ_PG_IMAGE_DIGEST` | compose 内固定值 | PG 镜像 digest |
| `CMSQ_REDIS_URL` | 读 `<secrets>/redis.env` | Redis 连接 |
| `CMSQ_REDIS` | 未设 | `=off` 关闭 Redis |
| `CMSQ_REDIS_PORT` | 56379 | `redis_up.sh` 映射端口 |
| `CMSQ_REDIS_IMAGE_DIGEST` | compose 内固定值 | Redis 镜像 digest |

### 进阶 / 开发
| 变量 | 默认 | 作用 |
|---|---|---|
| `CMSQ_TUSHARE_RPM` | 400 | tushare 每接口每分钟调用上限 |
| `CMSQ_TDX_SERVER` | 内置 | TDX 服务器（需 `tdx` 特性） |
| `CMSQ_FUTURES_SPECS` | 内置合约表 | JSON 文件，覆盖期货乘数/手续费/`close_order` |
| `CMSQ_EXTRACTOR` | 启发式 | `=llm` 用 LLM 抽取（需 `llm` 特性 + `CMSQ_LLM_BASE_URL`/`CMSQ_LLM_API_KEY`/`CMSQ_LLM_MODEL`） |
| `CMSQ_CTP_MD_LIB` | 未设 | CTP 行情动态库路径（需 `--features ctp`） |
| `CMSQ_RUNNER_IMAGE_ID` | 读 `runner_image_id` | 期望的沙箱镜像 id |
| `CMSQ_AUDIT_KEY` / `CMSQ_AUDIT_KEY_FILE` | 未设 / `<secrets>/audit.key` | 不用签名进程时的密钥（hex）/ 密钥文件 |
| `CMSQ_RUN_DIR`、`CMSQ_HOST_UID`、`CMSQ_HOST_GID`、`CMSQ_SIGNER_CONTEXT` | 由 `deploy.sh` 设置 | 签名容器的 socket 目录、uid/gid、构建目录 |
| `CMSQ_SAMPLE_AS_OF` | 未设 | 演示用：合成数据只显示到某日（模拟时间推进） |
| `CMSQ_DASH_TAB` / `CMSQ_DASH_SELECT` / `CMSQ_DASH_RUN` / `CMSQ_DASH_CONFIRM` | 未设 | 看板启动页 / 选中策略 / 研究页类型 / 打开确认台（截图用，不提交） |
| `CMSQ_DEBUG_PAPER` | 未设 | 模拟盘调试输出 |

`XDG_CONFIG_HOME` 会改变默认密钥目录位置。

## 3. 脚本
| 脚本 | 作用 |
|---|---|
| `scripts/deploy.sh [--hot] [--anchor-repo URL] [--home DIR]` | 一键部署（可重复执行），见 QUICKSTART |
| `scripts/db_up.sh` | 只启动/修复 PostgreSQL（口令自愈） |
| `scripts/redis_up.sh` | 只启动 Redis 热层 |
| `scripts/build_runner.sh` | 构建沙箱镜像 `cmsq-runner:2.2.2` 并记录 digest |
| `scripts/dashboard_demo.sh` | 合成样本工作区 `/tmp/cmsq-demo` + 打开看板 |
| `scripts/paper_demo.sh` | DEMO 工作区完整流程（晋升 → 信号 → 模拟盘） |
| `scripts/e2e.sh` / `scripts/e2e_real.sh` | 合成数据 / 真实数据完整闭环验证 |
| `scripts/test_docker.sh`、`test_pg.sh`、`signer_e2e.sh`、`signer_migrate_e2e.sh` | 开发测试 |
| `scripts/screenshots.sh`、`screenshots_scenarios.sh`、`ui_screenshots.sh` | 重拍看板截图 |

## 4. `cmsq` 命令
全局参数：`--home DIR`（工作区）、`--json`（机器可读输出）。`--backend` 取 `docker`（默认，唯一可过门槛）或 `process`（DEV，永不可晋升）。

| 命令 | 作用 | 主要参数（默认） |
|---|---|---|
| `init` | 创建工作区、写入固定资源、缓存样例数据 | — |
| `scrape` | 抓取 → 摄入 → 抽取 → 沙箱回测 | `--every`（只跑一次）、`--symbol FUT_RB`、`--backtests 3`、`--cycles`、`--backend` |
| `source-scores` | 各抓取源的质量分 | — |
| `ingest [PATH]` | 摄入 drop 文件（默认 `<home>/drop`） | — |
| `extract [ID]` | 抽取全部 Ingested（或一个） | — |
| `backtest [ID]` | 沙箱回测 + 门槛 | `--all`、`--symbol SAMPLE_TREND`、`--symbols A,B`、`--params JSON`、`--workers 2`、`--backend` |
| `sweep ID` | 参数网格扫描（≤64 点，每点计入 DSR） | `--symbol`、`--grid JSON`、`--workers 3`、`--ga`、`--population 8`、`--generations 4`、`--seed 1` |
| `portfolio ID` | 多标的组合回测（只作参考，不可晋升） | `--symbols`、`--alloc equal`（或 `inverse_vol[:N]`、`fixed:W1,W2`）、`--params`、`--workers 3` |
| `runs` | 研究运行列表 | `--kind` |
| `promote ID` | 人工晋升：键入 `PROMOTE <id前8位>`，运行一次性 vault | `--confirm "PROMOTE xxxxxxxx"`；未晋升退出码 3 |
| `reject ID` | 人工拒绝 | `--reason` |
| `list` | 策略列表 | `--status` |
| `show ID` | 来源、评估、任务、审计 | — |
| `jobs` / `jobs reap` | 沙箱任务 / 回收中断任务 | — |
| `trials` | DSR 试验账本 | — |
| `signals` | 只观察信号历史；`--refresh` 重跑已晋升策略 | `--refresh`、`--fetch`、`--id`、`--count 1500`、`--every`、`--backend` |
| `export-qifi ID` | 导出已晋升策略的 QIFI 账户 | `--out` |
| `replay SYMBOL` | 宿主机原生回放参考策略（不过门槛） | `--strategy ma_crossover`、`--params {}` |
| `data fetch SYMBOL[,..]` | 拉日线（A 股 `SH600519`、期货 `FUT_RB`、样例 `SAMPLE_*`） | `--count 1500`、`--concurrency 3`、`--sources`、`--no-validate` |
| `data calendar` | 交易日历快照 | `--exchange SSE`（或 SHFE/DCE/CZCE/CFFEX/INE/GFEX/`futures`）、`--start 2005-01-01` |
| `data sources` | 各数据源健康/冷却 | — |
| `data limits SYMBOL` | 交易所涨跌停价 | `--contract`、`--start 2015-01-01`、`--end` |
| `data import SYMBOL FILE` | 导入 CSV（不能过真实数据门槛） | `--adjustment none`（`adj_factor`/`qfq`/`hfq`） |
| `data list` | 已缓存数据集 | — |
| `db migrate` / `db status` | 数据库迁移 / 状态 | — |
| `md run` / `md probe` | CTP 行情（只观察） | `--instruments`、`--front`（默认 openctp 7x24）、`--seconds 60`、`--mock` |
| `paper run` | 模拟盘会话（仅 Promoted 策略） | `--strategy`、`--symbol`、`--instrument`、`--mock` / `--from-db` / `--from-bars CSV`、`--seconds 60`、`--participation 0.1`、`--limit-fill trade_through`（或 `touch_queue`）、`--session`、`--front` |
| `paper list` / `paper show S` / `paper verify S` | 会话列表 / QIFI 账户 / 校验事件链 | — |
| `audit verify` | 全链校验 + 锚点 | — |
| `audit tail` | 最近审计条目 | `--n 20` |
| `audit anchor-git` | 链头推送到 git | `--repo`、`--sign`、`--push`、`--every` |
| `audit verify-git` | 校验 git 中所有历史锚点 | `--repo` |
| `audit signer-enroll` | 在签名进程登记本工作区链头 | `--signer-dir` 或 `--print` |
| `audit migrate-to-signer` | 从 audit.key 迁移到签名进程 | `--signer-dir`、`--socket`、`--keep-key` |
| `signer serve/rotate/ping/enroll/import` | 签名进程本身（在 signer 容器里运行） | 见 [AUDIT_KEY.md](AUDIT_KEY.md) |

看板：`cargo run --release -p cmsq-dashboard`（读取同样的 `CMSQ_HOME`/`CMSQ_SECRETS_DIR`/数据库配置，只读）。

## 5. 数据源与限流
| 源 | 角色 | 限流 / 冷却 | 需要 |
|---|---|---|---|
| tushare | 主源，唯一提供复权因子；期货 `fut_daily`、日历、涨跌停 | 每接口 400 次/分钟（`CMSQ_TUSHARE_RPM`），`--hot` 时多进程共享 | `TUSHARE_TOKEN` |
| akshare | 备份 + 交叉校验（未复权） | 串行，间隔 ≥1.2 秒；失败冷却 60 秒 | `CMSQ_PYTHON` 里装 akshare |
| baostock | 备份 | 间隔 ≥0.5 秒；失败冷却 45 秒 | 装 baostock |
| tdx（rstdx） | 备份 | — | 编译 `tdx` 特性；海外网络通常不可达 |
| CTP（openctp） | 实时 tick（只观察） | — | `--features ctp` + `CMSQ_CTP_MD_LIB` |

交叉校验规则：差异 >0.1% 的日子被标记；标记日 ≤ max(3, 重叠/500) 且无 >1% → `ok_with_flags`（可通过）；否则 `divergent`（阻断）；重叠 <20 天 → `unvalidated`（阻断）。

## 6. 门槛检查一览
| 检查 | 通过条件 |
|---|---|
| `job_succeeded` | 沙箱任务成功 |
| `hardened_sandbox` | Docker 后端（DEV 后端永不通过） |
| `runner_pinned` | 镜像标签 `cmsq.runner.version=2.2.2` 且 digest 与记录一致 |
| `min_bars` | ≥ 250 根 |
| `no_lookahead` | 所有前视探针变体一致 |
| `static_risks` | 静态检查无前视 |
| `real_market_data` | 签名的 tushare/rstdx 来源（`CMSQ_ALLOW_SYNTHETIC=1` 例外） |
| `data_cross_validated` | 交叉校验 `ok` 或 `ok_with_flags` |
| `data_adjusted` | 有复权 |
| `net_return_positive` | 开发段净收益 > 0 |
| `sharpe` | ≥ 0.5 |
| `deflated_sharpe` | DSR ≥ 0.95，N ≥ `CMSQ_DSR_MIN_TRIALS` |
| `walk_forward` | 4 折中 ≥ 3 折为正 |
| `regime_stability` | 见上节阈值 |
| `cost_stress_2x` | 2 倍成本下净收益仍为正 |
| `max_drawdown` | 不差于 −35% |
| `exposure` | 平均总敞口 ≥ 5% |
| `beats_buy_and_hold` | Sharpe 高于同期买入持有 |
| `capacity` | ≥ `CMSQ_GATE_MIN_CAPACITY` |
| `futures_roll_ledger` | 期货换月腿完整可成交 |

全部通过 → `GatePassed`；随后 `promote` 运行最后 20% 的 vault，通过才 `Promoted`。
