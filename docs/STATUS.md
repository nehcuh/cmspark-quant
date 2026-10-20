# CMspark Quant — 项目状态（P0，2026-10-20）

## 1. 产品做什么
从量化论坛/博客抓取的策略帖 → 抽取代码与参数 → 在**加固 Docker 沙箱**里只输出目标仓位 →
**宿主机 Rust** 按 A 股规则撮合、计费、算指标、做前视探针与多重检验（Deflated Sharpe）→ 门禁 →
人工键入确认后运行**一次性** vault 留出段 → 只产生**观察信号**（不下单）。全过程写入 HMAC 审计链，
数据来自多源（tushare 主源、akshare 交叉校验）并以内容哈希快照固定。核心原则：容器内的一切都不可信。

## 2. 快速开始
```bash
cargo --version                      # 必须是 1.99.1（~/.cargo/bin 放在 PATH 前面）
sudo usermod -aG docker $USER        # 或在当前 shell 里用 sg docker -c "<命令>"
scripts/build_runner.sh              # 构建 cmsq-runner:2.1.0，并记录镜像 digest
cargo test --workspace
scripts/test_docker.sh               # Docker/bwrap 测试与全部攻击回归
scripts/e2e.sh                       # 合成数据完整闭环
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
| 假日期 + libfaketime 冻结时钟 + 屏蔽 /proc | r2_clock_dev、r2_clock_dev_bh、r2_clock_vault、r2_clock_probe |
| 静态检查（shift(-n)、i+k、负 np.roll、iloc 未来访问） | 明日涨跌，以及上面各类前视样例 |
| 只读挂载，O_NOFOLLOW 读取输出，输出大小上限 | data_peek、symlink_out、evil_path、evil_deep、bigout |
| 断网、CapDrop=ALL、pids/内存/超时限制 | escape、forkbomb（`pids_exhausted`）、网格策略联网 |
| 数据证明：快照哈希必须与 HMAC 审计中的 fetch 记录一致，快照只读 | e2e 第 6 节：改快照、删 sha，均被拒 |
| vault 一次性，按 idea 簇 / 代码哈希 / 仓位相关度去重 | e2e 第 5 节：换标题重发同一想法被拒 |
| HMAC 审计链 + 外部锚点 | 改写、截断、无密钥重链，均被检出 |
| DSR 稳健方差池（剔除作弊 trial，用 MAD），N 下限 20 | 防止作弊结果撑大方差、压低门槛 |

截图见 `docs/screenshots/01-overview.png` … `14-market-data.png`。

## 5. 已知缺口
* fork bomb 的 `pids_exhausted` 标签是启发式判断；稀疏偷看只能概率性抓到。
* 真实数据的价格形态本身仍可能泄露年代；直接系统调用可以绕过 libfaketime（有假日期兜底）。
* HMAC 密钥与程序属于同一 uid，同时拿到密钥并改写锚点的人可以伪造历史。
* 交叉校验目前只有 tushare↔akshare 一对（本机连不上 baostock 和 TDX），只有 tushare 提供复权因子。
* ST 状态根据 namechange 推断；老股票新股上市首日的 44% 涨跌幅规则未建模；vault 的消耗不计入 DSR 的 N。
* e2e 的 vault 演示放在独立工作区，并把 DSR 下限设为 1，否则没有策略能通过门禁。
* 截图 06 和 07 是同一时刻的画面（vault 运行很快，几乎抓不到"进行中"的状态）。

## 6. 后续阶段（依据 research-cta-data.md）
* **P1：组合与参数扫描。** 多标的组合回测、参数网格扫描。每个参数点都计入全库 trial 账本和 DSR 的 N，
  并配合 walk-forward 与 vault 纪律，避免把扫描变成过拟合。同时补上 BaoStock 第二校验源，在国内节点验证 rstdx。
* **P2：vn.py 兼容的策略层与期货规则。** 在沙箱里提供 `CtaTemplate` / `ArrayManager` 兼容 shim（只复刻接口，MIT），
  让社区里的 vn.py 策略不改代码就能跑，但仍然只输出目标仓位。宿主机实现期货撮合：合约乘数、保证金、手续费表、
  平今与平昨、涨跌停、夜盘交易日归属。期货日线以 Tushare `fut_daily` 为主源入库。
* **P3：ctp2rs 行情接入 OpenCTP。** 只接 MdApi（编译期关掉 TraderApi），连接 OpenCTP 7x24 环境
  （行情前置 `:30011`，BrokerID 9999，需使用 TTS 版动态库），生成实时**观察**信号，并自行落库积累期货 tick。
  可选：用 Nautilus 作为独立进程交叉验证入围策略，不进入可信计算基。
* P4（按需）：购买分钟级数据权限，扩展到分钟级 CTA。
