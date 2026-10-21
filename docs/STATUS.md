# CMspark Quant — 项目状态（P0–P3，2026-10-20）

## 1. 产品做什么
从量化论坛/博客抓取的策略帖 → 抽取代码与参数 → 在**加固 Docker 沙箱**里只输出目标仓位 →
**宿主机 Rust** 按 A 股规则撮合、计费、算指标、做前视探针与多重检验（Deflated Sharpe）→ 门禁 →
人工键入确认后运行**一次性** vault 留出段 → 只产生**观察信号**（不下单）。全过程写入 HMAC 审计链，
数据来自多源（tushare 主源、akshare 交叉校验）并以内容哈希快照固定。核心原则：容器内的一切都不可信。

## 2. 快速开始
```bash
cargo --version                      # 必须是 1.99.1（~/.cargo/bin 放在 PATH 前面）
sudo usermod -aG docker $USER        # 或在当前 shell 里用 sg docker -c "<命令>"
scripts/db_up.sh                     # 启动独立部署的 PostgreSQL+TimescaleDB（docker compose），口令写入 ~/.config/cmsq/db.env
scripts/build_runner.sh              # 构建 cmsq-runner:2.2.2（含 vn.py shim），并记录镜像 digest
scripts/test_pg.sh                   # PostgreSQL 并发/权限/篡改测试
cargo test --workspace
scripts/test_docker.sh               # Docker/bwrap 测试与全部攻击回归
scripts/e2e.sh                       # 合成数据完整闭环（默认 PostgreSQL；CMSQ_DB=sqlite 走离线 SQLite）
# 真实数据：把 TUSHARE_TOKEN=... 写进 ~/.config/cmsq/secrets.env（切勿提交）
CMSQ_PYTHON=<含 akshare 的 python> scripts/e2e_real.sh
scripts/dashboard_demo.sh            # 演示工作区 + Dashboard
```
没有 tushare token 时只能用 SAMPLE 合成数据，或 akshare 的未复权备份数据，两者都无法通过真实数据门禁。

## 3. 真实数据结果（e2e_real，2026-10-20 17:20 UTC+8）
6 只股票 × 6 个样例帖，共 36 次 Docker 回测，数据区间为 2016-07 至 2026-10（SZ300750 从 2018-06 开始）。
**结果：0 个通过门禁，0 个晋升。** 表格中每格为开发期净收益 / Sharpe。DSR 的 N 取下限 20。

| 策略 | SH600519 | SZ000001 | SH601318 | SZ000858 | SH600036 | SZ300750 |
|---|---|---|---|---|---|---|
| 明日涨跌(前视) | +454945.61% / 6.59 | +390509.02% / 6.05 | +88726.88% / 5.87 | +6218940.73% / 6.92 | +163245.66% / 6.06 | +8327581.26% / 6.61 |
| 双均线 | +77.48% / 0.54 | -37.46% / -0.21 | -37.60% / -0.27 | +73.49% / 0.46 | -24.41% / -0.12 | +697.45% / 1.25 |
| 布林带 | +40.14% / 0.39 | -17.54% / -0.07 | +8.52% / 0.16 | -31.45% / -0.14 | +11.23% / 0.18 | +22.54% / 0.28 |
| 20日动量 | +58.11% / 0.45 | +24.17% / 0.27 | -19.66% / -0.08 | +84.64% / 0.52 | -15.62% / -0.03 | +154.81% / 0.82 |
| 网格(联网) | Failed（断网） | Failed（断网） | Failed（断网） | Failed（断网） | Failed（断网） | Failed（断网） |
| 短期反转 | -10.89% / -0.04 | -46.64% / -0.47 | -34.72% / -0.41 | -30.79% / -0.19 | -41.15% / -0.47 | +44.29% / 0.39 |

* 明日涨跌每次都被前视探针（no_lookahead）和静态检查同时抓到。它的收益越夸张，越说明问题出在前视，而不是策略本身好。
* 最好的诚实结果是双均线在 SZ300750 上：Sharpe 1.25，但 DSR 只有 0.837（门槛 0.95），被拒。
* 网格策略试图联网，被 `NetworkMode=none` 阻断，以 strategy_error 失败。
* 交叉校验（tushare 对 akshare）：5 只为 ok；SH600519 为 ok_with_flags，2017-02-27 一天差 0.112%，在容忍范围内并已记录。
* 完整输出见 `docs/e2e-real-output.txt`。

## 4. 防御与攻击样例
攻击样例都放在 `fixtures/attacks/`，每个都是"必须失败"的回归测试，由 `scripts/test_docker.sh` 和 `scripts/e2e.sh` 运行。

| 防御 | 被拦下的攻击样例 |
|---|---|
| 宿主机计算指标，不读取容器自报结果 | forge.json（伪造 result.json） |
| 前视探针：随机种子截断点 + 尾部截断 + 块自助重采样；每个变体用全新容器、随机顺序 | hidden_peek、memo_peek、length_tell、r2_cut_aware、r2_rng_alias、r2_sparse_peek（概率性） |
| 假日期 + libfaketime 冻结时钟 + 屏蔽 /proc + seccomp 禁止直接读墙钟的系统调用（`deploy/seccomp/cmsq-runner.json`） | r2_clock_dev、r2_clock_dev_bh、r2_clock_vault、r2_clock_probe；`docker_denies_raw_wall_clock_syscalls`（ctypes 直接 syscall 得到 EPERM） |
| 静态检查（shift(-n)、i+k、负 np.roll、iloc 未来访问） | 明日涨跌，以及上面各类前视样例 |
| 只读挂载，O_NOFOLLOW 读取输出，输出大小上限 | data_peek、symlink_out、evil_path、evil_deep、bigout |
| 断网、CapDrop=ALL、pids/内存/超时限制 | escape、forkbomb（`pids_exhausted`）、网格策略联网 |
| 数据证明：快照哈希必须与 HMAC 审计中的 fetch 记录一致，快照只读 | e2e 第 6 节：改快照、删 sha，均被拒 |
| vault 一次性，按 idea 簇 / 代码哈希 / 仓位相关度去重 | e2e 第 5 节：换标题重发同一想法被拒 |
| HMAC 审计链 + 外部锚点 | 改写、截断、无密钥重链，均被检出 |
| DSR 稳健方差池（剔除作弊 trial，用 MAD），N 下限 20 | 防止作弊结果撑大方差、压低门槛 |

截图见 `docs/screenshots/01-overview.png` … `18-futures-sweep-vnpy.png`。

## 5. 已知缺口
* fork bomb 的 `pids_exhausted` 标签是启发式判断；稀疏偷看只能概率性抓到。
* DEV（bwrap）后端的 /out 是宿主机目录，字节和文件数预算靠 50 ms 轮询的看门狗执行：超限即杀进程（`QuotaExceeded`），但两次轮询之间仍可能多写入一些文件。Docker 后端由内核（限容 tmpfs）强制执行。
* 真实数据的价格形态本身仍可能泄露年代。直接读墙钟的系统调用（time、gettimeofday、clock_gettime 的 REALTIME/TAI/BOOTTIME 等）已被 seccomp 拒绝；但直接调用 vDSO、或用原始 stat 系统调用读自己刚写的文件的时间戳，仍能拿到真实时间（有假日期兜底）。
* HMAC 密钥与程序属于同一 uid，同时拿到密钥并改写锚点的人可以伪造历史。
* 交叉校验目前只有 tushare↔akshare 一对（本机连不上 baostock 和 TDX），只有 tushare 提供复权因子。
* ST 状态根据 namechange 推断；老规则新股首日 +44%/-36% 涨跌幅不需要建模：信号在收盘决定、次日开盘成交，上市首日（第 0 根）永远不会成交；vault 的消耗不计入 DSR 的 N。
* e2e 的 vault 演示放在独立工作区，并把 DSR 下限设为 1，否则没有策略能通过门禁。
* 截图 06 和 07 是同一时刻的画面（vault 运行很快，几乎抓不到"进行中"的状态）。

## 6. 本轮完成（分支 p1-onward）
### P0 存储与并发
* 主存储改为**独立部署的 PostgreSQL 16 + TimescaleDB**（`deploy/docker-compose.yml`，镜像按 digest 固定，仅 127.0.0.1）。
  sqlx 连接池 + 内嵌迁移；SQLite 只保留给测试与离线（`CMSQ_DB=sqlite`），两种后端共用同一套 SQL 与语义。
* 最小权限：迁移用 `cmsq_owner`，运行时用 `cmsq_app`（证据表只能 INSERT/SELECT，不能删改、TRUNCATE、停用或删除触发器）。
  每个工作区一个 schema；`bars`/`ticks` 为 hypertable。
* 并发正确性：审计追加持 advisory lock + 锚点文件锁，序号与锚点严格有序；trial 账本插入与快照原子化，每个 trial 看到不同的 N；
  vault 去重与占用在同一把锁内完成，判定只能写一次，之后触发器冻结；状态迁移用 CAS。
  竞争测试 `tests/store_concurrency.rs`：SQLite 4 项 + PostgreSQL 7 项，全部通过。
* 并发能力：回测/扫描/组合的各个变体并行跑，用容器槽位信号量限流（`CMSQ_CONTAINER_SLOTS`，默认 CPU 数）；
  `data fetch A,B,C --concurrency N` 并发拉取，tushare 限速 400 次/分钟，akshare 串行且间隔 ≥1 秒。
### P1 组合与参数扫描
* `cmsq sweep`（最多 64 个点，只允许已知参数）：**每个点都计入 DSR 试验账本**；最优点按最终账本重新计算 DSR。
* `cmsq portfolio`：单账户共享现金、先卖后买；分配方式为等权 / 逆波动率（无前视）/ 固定权重；只用共同开发期。
  组合门禁只作参考，组合不可晋升。`cmsq backtest --symbols A,B --workers N` 并发跑多标的。
### P2 vn.py 兼容层与期货规则
* 沙箱内提供 `CtaTemplate`/`ArrayManager`/`BarGenerator` shim（runner 2.2.2），vn.py 风格策略不改代码即可运行，仍然只输出仓位。
* 期货：`FUT_<品种>` 主力连续（按持仓量换月，连续 3 日确认，次日生效）、合约乘数、保证金上限、最小变动价位、涨跌停、
  夜盘交易日归属、逐合约手续费、换月两腿成本；空头仓位 [-1,1]。akshare 逐合约交叉校验，覆盖不足 90% 时记为 partial。
### P3 CTP 行情（只观察）
* `cmsq md run/probe`；`--features ctp` 启用 ctp2rs MdApi（只引用行情 API，有单元测试守护）；tick 校验、1 分钟 K 线、落库并写审计。
* **本机连不上 openctp 7x24（TCP 超时）**，所以用内置回放源（带坏数据）做测试，实时源未经实网验证。
### Dashboard
新增"参数扫描/组合"和"期货/CTP"两页；侧栏显示数据库后端。新截图为 `15-sweep`、`16-portfolio`、`17-futures-ctp`、`18-futures-sweep-vnpy`，并重拍了 `01-overview`。

## 7. 本轮真实数据结果（2026-10-20 21:58 UTC+8，PostgreSQL）
* 参数扫描：双均线在 SH600519 上 6 个点，最优 fast=10/slow=60，Sharpe 0.69，DSR 0.432（N=20），被拒。
* 组合：SH600519+SZ000858+SH600036，逆波动率(60)，收益 +39.30%，Sharpe 0.39，DSR 0.179；跑输等权持有（+72.8%）。
* 期货 FUT_RB（1100 根，2022-04 至 2026-10，13 次换月，akshare 逐合约校验 1100/1100 一致）：
  唐奇安突破 -9.77% / Sharpe -0.11；vn.py 双均线 -12.64% / -0.18。唐奇安扫描 n=10 最好，+11.78%，Sharpe 0.32；
  vn.py 扫描最好 +0.32%。全部被拒，0 个晋升。
* A 股交叉校验三只全部为 ok（tushare 对 akshare）。完整输出见 `docs/e2e-real-output.txt`。

## 8. 本轮审查发现并已修复
* 期货空头的平均敞口为负，被"最低敞口"规则误杀 → 改为总敞口 |x|。
* 新浪只保留约 5 年合约，导致整段期货校验失败 → 改为逐段容错，并计算覆盖率（不足 90% 记 partial，门禁不通过）。
* 期货 vault 结果写的是 A 股的执行标签 → 已修正。
* DEV(bwrap) 后端缺少 vnpy_shim.py → 已随 runner 一起写入。
* Dashboard 原来只读 SQLite → 改为按配置只读打开（PG 用运行时角色，不跑迁移）。
* e2e 中 `| head` 触发 SIGPIPE 导致误报失败 → 改为先写文件再看。
* 新增缺口：CLI 进程从 db.env 读取了 owner URL（只用于迁移）；回放源不能代替实网验证。（节假日表已在第 10 节补上。）

## 10. 本轮（2026-10-21）：平今、交易所日历、QIFI 导出、交易统计
* **A 平今手续费**：每笔开仓按交易日打标签（逐日 lot 账本）；平仓按交易所顺序消耗 lot——大商所、郑商所、广期所先开先平，
  中金所先平今，上期所和能源中心由交易者指定（按"较便宜的一档先平"建模）。当日开当日平的 lot 收平今档手续费。
  可在 `CMSQ_FUTURES_SPECS` 里按品种设 `close_order` 覆盖。日线下开平总在不同交易日，所以平今档不会出现（有测试锁定）；
  新的 `futures::simulate_session` 支持分钟 K 线和交易日标签，夜盘开仓、次日上午平仓也算平今。
  设计只参考了 qa-rs 的思路（该仓库没有许可证），没有拷贝代码或数据。
* **B 交易所日历**：`cmsq data calendar --exchange futures` 拉取六家期货交易所的 tushare trade_cal 快照
  （本机实测 SHFE/DCE/CZCE/CFFEX 各 3160 个交易日，INE 2580，GFEX 1220，覆盖到 2027-12-31，sha256 写入审计）。
  CTP 行情的交易日归属改用对应交易所的日历；节前没有夜盘的 tick 会计入 `no_night_session_per_calendar`
  （只标记不丢弃，因为 7x24 仿真前置本来就会推这类 tick）。期货的日历缺口填补也改用交易所日历。
  没有快照时退回"工作日都开市"，并在报告里写明。
* **C QIFI 导出（可选）**：`cmsq export-qifi <id> [--out]` 只对已晋升的策略开放，按 QIFI 布局（accounts / positions / trades / orders）
  导出 vault 窗口内的模拟账户和观察信号。`orders` 恒为空，并标注 `cmsq.observe_only=true`；导出写审计并记录 sha256。
  vault 结果里新增 `account` 快照；2.2.1 之前的 vault 结果没有这份快照，会被拒绝导出。
* **D 交易级统计（只展示）**：根据宿主机记录的成交逐笔 FIFO 配对，在复权价尺度上计算胜率、盈亏比、盈利因子、平均/最长持仓 bar 数，
  换月腿成本单独列出。开发期和 vault 的 `result.json` 都带 `trade_stats`（`display_only: true`）。
  门禁不读取它，有单元测试检查 gate.rs 不引用该模块。
* Dashboard：每个 run 下面显示交易统计和平今成本；通过 vault 的 run 再显示 QIFI 账户摘要。
* 对抗审查记录见 `docs/reviews/06-pingjin-calendar-qifi-tradestats.md`。
* 验证：`cargo test` 全部通过（core 单元测试 81 个，另有集成测试），clippy 0 告警，`test_docker.sh` 通过，`e2e.sh` 通过（E2E OK）。

## 12. 独立评审整改（#5–#11，见 reviews/08）
* #5：门禁的“真实数据”只认签名来源（tushare、rstdx），meta.json、CSV、未知来源一律不放行。
* #6：前视探针覆盖整个评分区间：首个评分 bar 截断，分层 mirror 变体（未来收益取反）覆盖开发段和 vault。
* #7：DSR 试验数按“策略 + 参数 + 标的 + 快照 + 窗口 + 运行类型 + 组合”计数，完全相同的重跑才去重。
* #8：运行租约（主机|PID|进程启动时间）与状态 CAS 写在同一条语句里；恢复只回收本机已确认死亡的 owner。
* #9：Docker 的 /out 改为限定大小和 inode 数的 tmpfs；日志按预算实时监控，超出即终止（QuotaExceeded），daemon 日志也有上限。
* #10：期货换月按实际持有的合约逐腿计价和收费；换月腿缺失、不可成交或是旧数据时不能晋升。
* OHLC 不合理的 bar 在签名前就被拒绝。

## 11. 正确性一轮（2026-10-21，见 reviews/07）
* 平今规则按公开资料核对后改正：上期所和能源中心由交易者指定（按较便宜的一档建模）；大商所、郑商所、中金所、广期所
  持仓按先开先平减扣，但只要当日有今仓，手续费先按平今收取，直到按平今收费的手数达到当日开仓手数。
* vn.py shim（runner 2.2.2）：委托价按合约最小变动价位取整（与 BacktestingEngine 的 round_to 一致）；
  用 `cmsq_max_units` 参数或策略的 `max_units` 属性支持加仓（海龟策略为 4 个单位）。
* 门禁新增 `real_market_data`（合成数据不能通过，只有演示时设置 `CMSQ_ALLOW_SYNTHETIC=1` 才放行）和 `capacity`
  （容量估计：90% 的成交不超过当日成交额的 5%，最低 ¥100 万）。
* e2e 新增 5b 节，完整跑通 GatePassed → vault → Promoted → QIFI 导出。所用的 SAMPLE_PERSIST 是固定牛熊节奏的
  合成数据，只用来演示流程。

## 9. 后续阶段
* **P1：组合与参数扫描。** 多标的组合回测、参数网格扫描。每个参数点都计入全库 trial 账本和 DSR 的 N，
  并配合 walk-forward 与 vault 纪律，避免把扫描变成过拟合。同时补上 BaoStock 第二校验源，在国内节点验证 rstdx。
* **P2：vn.py 兼容的策略层与期货规则。** 在沙箱里提供 `CtaTemplate` / `ArrayManager` 兼容 shim（只复刻接口，MIT），
  让社区里的 vn.py 策略不改代码就能跑，但仍然只输出目标仓位。宿主机实现期货撮合：合约乘数、保证金、手续费表、
  平今与平昨、涨跌停、夜盘交易日归属。期货日线以 Tushare `fut_daily` 为主源入库。
* **P3：ctp2rs 行情接入 OpenCTP。** 只接 MdApi（编译期关掉 TraderApi），连接 OpenCTP 7x24 环境
  （行情前置 `:30011`，BrokerID 9999，需使用 TTS 版动态库），生成实时**观察**信号，并自行落库积累期货 tick。
  可选：用 Nautilus 作为独立进程交叉验证入围策略，不进入可信计算基。
* P4（按需）：购买分钟级数据权限，扩展到分钟级 CTA。
