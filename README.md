# CMspark Quant — P0

本地优先的量化策略**发现与沙箱验证**闭环（无实盘、无 IB）：

```
CMspark 抓取结果 drop (JSON) ─► 摄入/归档 ─► 结构化抽取 ─► Docker 沙箱回测（断网、只读数据）
      ─► 门禁评估（成本后样本外 + 稳定性 + 多重检验 + 前视探针）─► 人工确认晋升 ─► 仅观察信号
                          全程：SQLite 状态机 + 哈希链审计日志 + 按策略 ID 的产物目录
```

规格来源：产品讨论中的 Synthesizer 修订需求与 Trade-off Judge 架构（方案 A 为主干）。计划与三轮对抗审查见
`docs/PLAN.md`、`docs/reviews/01-plan.md`、`02-implement.md`、`03-verify.md`。

| 模块 | 位置 |
|---|---|
| Drop 格式规范 | `docs/DROP_FORMAT.md`（`cmspark.scrape.v1`） |
| 行情数据 / rstdx / huquant | `docs/MARKET_DATA.md` |
| Core（摄入、抽取、沙箱、门禁、状态机、存储、编排） | `crates/cmsq-core` |
| CLI `cmsq` | `crates/cmsq-cli` |
| Dashboard（GPUI Kit） | `crates/cmsq-dashboard` |
| 固定版本回测 Runner（容器内） | `runner/`（基础镜像按 digest 固定，pandas/numpy 锁版本） |
| 样例（均标注 SAMPLE） | `fixtures/posts/*.json`、`fixtures/market/*.csv`（合成数据） |

## 快速开始（Linux，全新 clone）

```bash
# 依赖：rustup（rust-toolchain.toml 固定 1.99.1，自动安装）、Docker、bubblewrap（仅 DEV 后端/测试）
# rstdx 已 vendored 在 third_party/rstdx（含空 bars 修复），无需同级目录。
#
# Docker 组配置（一次性）：当前用户必须能访问 daemon
sudo usermod -aG docker $USER   # 然后重新登录；或在当前 shell 用：sg docker -c "<命令>"
docker info >/dev/null && echo ok
scripts/build_runner.sh          # 构建 cmsq-runner:2.0.0（仅构建时联网）
cargo test --workspace           # 无需 Docker 的测试；需 Docker 的测试为 #[ignore]
scripts/test_docker.sh           # 运行全部 Docker/bwrap 测试；缺依赖时**失败**而不是跳过
scripts/e2e.sh                   # 全新工作区跑完整闭环；任一结果不符预期即非零退出（缺 Docker 即失败，除非 --allow-dev）
scripts/dashboard_demo.sh        # 准备演示工作区并打开 Dashboard
```

CLI（`CMSQ_HOME` 默认 `./var`；密钥目录 `CMSQ_SECRETS_DIR` 默认 `~/.config/cmsq`，必须在 CMSQ_HOME 之外）：

```bash
cmsq init
cmsq data calendar                               # tushare trade_cal → data/_calendar_SSE.txt
cmsq data fetch SH600519 --count 2500            # 多源路由：tushare(含 adj_factor) → akshare → baostock → tdx；交叉校验收盘价
cmsq data sources                                # 各数据源健康 / 冷却状态
cmsq replay SH600519 --strategy ma_crossover     # 宿主机原生逐 bar 回放（参考用，不过门禁）
cmsq data import <csv> --adjustment hfq          # 导入带 adj_factor 的复权数据
cmsq ingest [path] | extract
cmsq backtest <id>|--all --symbol SAMPLE_REGIME [--backend docker|process]   # 默认 docker；process 大声警告且永不可晋升
cmsq promote <id>                                # 需键入 "PROMOTE <id前8位>"；一次性运行 vault；未晋升退出码 3
cmsq list | show <id> | jobs | jobs reap | trials | signals | audit verify | audit tail
```

## 信任模型
* **容器内不可信**：策略代码 + `runner/signal_runner.py` 只读 `/input/bars.csv`、`/job/strategy.py`、`/job/job.json`，
  只写 `/out/positions.csv`（目标仓位）。容器的任何其他输出均被忽略；宿主机以 O_NOFOLLOW 读取并严格解析。
  容器以 `/out` 目录属主 uid 运行（非 root，镜像默认 `USER 10001`），`NetworkMode=none`、只读 rootfs、CapDrop=ALL、
  no-new-privileges、pids/内存/CPU 限制、超时强杀、输出上限；从不挂载 docker.sock。
* **宿主机可信（Rust，`cmsq-core::scorer`）**：成交、成本、指标、前视探针、DSR、walk-forward、门禁全部在宿主机计算。
  门禁只读取容器的运行状态与 runner 镜像身份（`docker image inspect` 标签 `cmsq.runner.version=2.0.0`），从不读取策略可控字段。
* **前视探针**：每个变体（截断/块自助重采样，共 10 个）在**全新容器**中、**随机顺序**运行，比较重叠区间仓位。
* **成交**：t 日收盘信号 → t+1 开盘成交（含滑点），按收盘盯市；起始 ¥1,000,000。
* **A 股规则**：100 股一手；佣金 max(¥5, 2.5bp)；卖出印花税按日期（0.3%→0.1%@2008-04-24→0.05%@2023-08-28）；
  涨跌停（主板 10%，科创 20%@2019-07-22，创业板 20%@2020-08-24，北交所 30%；按未复权价判断）涨停不买、跌停不卖；
  成交量 0 视为停牌；adj_factor 后复权供策略使用。
* **多重检验**：全库 trial 账本（相近重复识别），Deflated Sharpe ≥ 0.95；开发段 80%（20% 预热 + 4 折 walk-forward，≥3 折为正），
  最后 20% 为 vault，仅在人工确认晋升时运行**一次**（先记录消耗再运行）。
* **状态机**：Ingested→Extracted→Backtesting→GatePassed→(promote+vault)→Promoted/Rejected；Failed 归档；
  启动时回收死进程遗留的 running 作业（→ abandoned，策略 → Failed "interrupted"）。错误类型为 `CoreError`。
* **审计**：追加写 + HMAC-SHA256 链（密钥 `~/.config/cmsq/audit.key`，0600，位于 CMSQ_HOME 之外）+ 外部锚点文件
  （`<secrets>/anchors/*.anchor`）记录链头；`audit verify` 检出修改、删除、截断、回滚、无密钥/有密钥重链。

## 验证结果（本机，2026-10-20）
* `cargo test --workspace`：core 46 + pipeline 3 通过（Docker 测试 #[ignore]）。
* `scripts/test_docker.sh`：pipeline 3 + sandbox 6 + rstdx security_bars 13 通过（含全部攻击贴回归）。
* `cargo clippy --workspace --all-targets -- -D warnings`：通过。
* `scripts/e2e_real.sh`（真实 A 股，tushare + akshare 交叉校验，6 只股票 × 6 个样例帖 = 36 次 Docker 回测）：
  **0 个通过门禁、0 个晋升**。"明日涨跌" 每次都被前视探针抓到；SH600519 有 1 天 tushare/akshare 收盘价差 0.11%（2017-02-27），
  该数据集被 `data_cross_validated` 阻断；其余均因 Sharpe / DSR / 回撤 / 净收益不达标被拒。输出见 `docs/e2e-real-output.txt`。
* `scripts/e2e.sh`：输出见 `docs/e2e-output.txt`，全部符合预期；无策略被晋升（双均线门禁通过但 vault 段亏损被拒）。
* 截图：`docs/screenshots/01-overview.png` … `13-no-docker-banner.png`。
* 外部审查问题 → 修复映射：`docs/reviews/04-external-review-fixes.md`。

## 已知局限
ST ±5% 未建模（meta 中标记 is_st）；只有 tushare 提供 adj_factor，akshare/baostock/TDX 作为备份时数据为未复权、门禁拒绝；
本机 baostock 与 TDX 不可达、Eastmoney 断连，交叉校验实际只有 tushare↔akshare(Sina) 一对；同时持有密钥并改写锚点的攻击者仍可伪造历史；
DEV（bwrap）后端隔离较弱且永不可晋升；相近重复识别为启发式；SAMPLE_REGIME 为合成数据，参数为演示调过。

## 明确不做（P0）
实盘 / paper trading / IB；进化搜索；分布式；Redis。
