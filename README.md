# CMspark Quant

本地优先的量化策略**发现、沙箱验证与模拟盘**系统（只观察，无实盘下单路径）。

## 一键部署

```bash
git clone https://github.com/nehcuh/cmspark-quant && cd cmspark-quant
scripts/deploy.sh                    # PostgreSQL/TimescaleDB + 审计签名进程 + 工作区 ~/cmsq-home
scripts/deploy.sh --hot              # 另外启用 Redis 热层（可选）
scripts/deploy.sh --anchor-repo https://github.com/<you>/<anchors>   # 另外每小时把审计链头推送到私有锚点仓库
set -a; . ~/.config/cmsq/cmsq.env; set +a
cmsq list
```

可重复执行。完整说明（数据源、日常流程、模拟盘、看板、审计与运维）见 **[docs/USER_GUIDE.md](docs/USER_GUIDE.md)**。

---

## 开发者说明（原 P0 文档）

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
# 确认 PATH 中 ~/.cargo/bin 在前：cargo --version 应显示 1.99.1（系统自带旧 cargo 会编译失败）
# 真实数据需要 tushare token（写入 ~/.config/cmsq/secrets.env：TUSHARE_TOKEN=...，切勿提交）；
#   无 token 时只能用 SAMPLE 合成数据或 akshare 备份数据，二者都无法通过真实数据门禁（不可晋升）。
# rstdx 已 vendored 在 third_party/rstdx（含空 bars 修复），无需同级目录。
#
# Docker 组配置（一次性）：当前用户必须能访问 daemon
sudo usermod -aG docker $USER   # 然后重新登录；或在当前 shell 用：sg docker -c "<命令>"
docker info >/dev/null && echo ok
scripts/build_runner.sh          # 构建 cmsq-runner:2.2.2（仅构建时联网），镜像 digest 记录到 ~/.config/cmsq/runner_image_id
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
  门禁只读取容器的运行状态与 runner 镜像身份（标签 `cmsq.runner.version=2.2.2` **且** 镜像 digest 与 build_runner 记录一致），从不读取策略可控字段。
* **前视探针**：每个作业按种子随机抽取 5 个截断点（35%–98%）+ 固定尾部截断（n−1、n−2、n−4）+ 块自助重采样，约 16 个变体，
  每个在**全新容器**中、随机顺序运行；种子与截断点记入审计。容器看到的是从 2000-01-03 起重编号的**假日期**、
  libfaketime 冻结的时钟（2000-01-01）、被屏蔽的 /proc/uptime 等；静态检查覆盖 `i+k` 索引、负 `np.roll`、`iloc` 未来访问。
* **数据证明**：回测只读取 `snapshots/<sha>.csv`（只读、哈希匹配），且 sha 必须等于审计链中经 HMAC 复核的最新
  `market_data_fetch` 记录；复权/交叉校验结论取自审计记录而非 meta.json；source=sample/csv 不能通过真实数据门禁。
* **交叉校验策略**：差异 >0.1% 的日子被标记；标记日 ≤ max(3, 重叠/500) 且无 >1% → ok_with_flags（可晋升）；否则 divergent（阻断）；重叠 <20 天 → unvalidated。
* **成交**：t 日收盘信号 → t+1 开盘成交（含滑点），按收盘盯市；起始 ¥1,000,000。
* **A 股规则**：100 股一手；佣金 max(¥5, 2.5bp)；印花税完整历史（2008-09-19 前双边征收，之后仅卖出；0.3%→0.1%→0.05%@2023-08-28）；手数与费用按未复权价；
  涨跌停（主板 10%，ST 5%（2025-07-07 起 10%，依 namechange），注册制新股前 5 日无涨跌停，北交所含 92xxxx，科创 20%@2019-07-22，创业板 20%@2020-08-24，北交所 30%；按未复权价判断）涨停不买、跌停不卖；
  成交量 0 或交易日历缺口视为停牌；adj_factor 后复权供策略使用。
* **多重检验**：全库 trial 账本（相近重复识别），Deflated Sharpe ≥ 0.95（方差池剔除前视被抓/不合理 trial，≥5 个时用 MAD 稳健估计；N = max(相近重复簇数, 20)，`CMSQ_DSR_MIN_TRIALS` 可调）；另需 Sharpe 高于同期买入持有；开发段 80%（20% 预热 + 4 折 walk-forward，≥3 折为正），
  最后 20% 为 vault，仅在人工确认晋升时运行**一次**（先记录消耗再运行）；vault 按 idea 簇 + 标的、代码哈希、仓位相关 >0.9 去重，换标题重发同一想法不会获得新的 vault。
* **状态机**：Ingested→Extracted→Backtesting→GatePassed→(promote+vault)→Promoted/Rejected；Failed 归档；
  启动时回收死进程遗留的 running 作业（→ abandoned，策略 → Failed "interrupted"）。错误类型为 `CoreError`。
* **审计**：追加写 + HMAC-SHA256 链（密钥 `~/.config/cmsq/audit.key`，0600，位于 CMSQ_HOME 之外）+ 外部锚点文件
  （`<secrets>/anchors/*.anchor`）记录链头；`audit verify` 检出修改、删除、截断、回滚、无密钥/有密钥重链。

## 验证结果（本机，2026-10-20，第二轮审查修复后）
* `cargo test --workspace`：core 50 + pipeline 4 + dashboard 1 通过（3 个 Docker 测试 #[ignore]）。
* `scripts/test_docker.sh`：pipeline 3 + sandbox 6 + rstdx security_bars 13 通过（含全部攻击贴回归，包括第二轮的 7 个）。
* `cargo clippy --workspace --all-targets -- -D warnings`：通过。
* `scripts/e2e.sh`：全部符合预期，输出 `docs/e2e-output.txt`；单想法 vault 演示（独立工作区，DSR 下限设为 1）门禁通过、vault 亏损被拒，重发被拒；篡改快照 / 删除 sha 均被拒。
* `scripts/e2e_real.sh`：6 只股票 × 6 帖 = 36 次 Docker 回测，**0 通过门禁、0 晋升**；交叉校验 5 个 ok、SH600519 为 ok_with_flags（1 天 0.112%）。输出 `docs/e2e-real-output.txt`。
* 截图：`docs/screenshots/`（哪些已重拍、哪些是历史截图见其中的 README.md）；项目状态：`docs/STATUS.md`。审查修复映射：`docs/reviews/04-external-review-fixes.md`、`docs/reviews/05-round2-fixes.md`。
* 面板设计规范：`docs/UI_DESIGN.md`（gpui-component 组件与主题 token、图标/动画规则、截图自查流程；重拍脚本 `scripts/ui_screenshots.sh`）。

## 已知局限
fork bomb 的 `pids_exhausted` 标签是启发式；真实数据的价格形态仍可能泄露所处年代；直接系统调用可绕过 libfaketime（有假日期兜底）；
稀疏偷看只能概率性抓到；HMAC 密钥与程序同一 uid（持有密钥并改写锚点者可伪造）；交叉校验只有 tushare↔akshare 一对；
ST 依 namechange 推断，老新股 44% 规则未建模；vault 消耗未计入 DSR 的 N；只有 tushare 提供 adj_factor；DEV（bwrap）后端永不可晋升。

## 明确不做（P0）
实盘 / paper trading / IB；进化搜索；分布式；Redis。
