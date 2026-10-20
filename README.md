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

## 快速开始（Linux）

```bash
# 依赖：Rust stable、Docker（当前用户可访问 daemon）、bubblewrap（DEV 后端可选）
scripts/build_runner.sh          # 构建 cmsq-runner:1.0.0（仅构建时联网）
scripts/e2e.sh                   # 全新工作区跑完整闭环并打印结果
scripts/dashboard_demo.sh        # 准备演示工作区并打开 Dashboard
```

CLI（`CMSQ_HOME` 默认 `./var`）：

```bash
cmsq init                                   # 工作区 + 固定资产 + 样例数据
cmsq data fetch SH600519 --count 1500       # 宿主机经 rstdx 拉 A 股日线（沙箱外）
cmsq ingest [path]                          # 默认摄入 <home>/drop
cmsq extract                                # 离线启发式抽取；CMSQ_EXTRACTOR=llm 用 LLM（需 --features llm）
cmsq backtest <id>|--all --symbol SAMPLE_RANGE [--backend docker|process|auto] [--params '{"fast":5}']
cmsq promote <id> --confirm <id 前 8 位>    # 门禁通过 + Docker 结果 + 手动确认
cmsq list | show <id> | jobs | signals | audit verify | audit tail
```

## 关键设计
* **沙箱（默认 Docker / bollard）**：`NetworkMode=none`、只读 rootfs、`CapDrop=ALL`、`no-new-privileges`、非 root、
  pids/内存（无 swap）/CPU 限制、超时强杀+删除、`/tmp` tmpfs noexec；`/data`、`/job`、`/vendor` 只读，仅 `/out` 可写；
  从不挂载 docker.sock。DEV 后端为 bubblewrap（全部 namespace 隔离含网络 + prlimit），其结果**不能通过门禁**。
* **Runner**：信号统一延后一根 bar 成交；A 股成本（双边佣金 2.5bp、卖出印花税 5bp、滑点 5bp）；时间序 70/30 切分；
  输出 IS/OOS/2 倍成本/OOS 两半/买入持有基准、换手、回撤、数据出处、huquant 交易日历校验、**前视探针**。
* **门禁**：OOS 样本量、成本后 OOS 收益 > 0、OOS Sharpe ≥ 0.3 + 0.15·ln(尝试次数)、OOS 两半均为正、最大回撤 ≥ −35%、
  最低持仓暴露、2 倍成本仍为正、必须来自加固沙箱、前视探针干净。失败一律归档并写明原因。
* **状态机**：Ingested→Extracted→Backtesting→Evaluated→Promoted/Rejected，Failed 归档；Rejected/Failed/Evaluated 可重跑
  （尝试次数 +1）。非法转换被拒绝并审计。
* **审计**：追加写（触发器禁止 UPDATE/DELETE）+ sha256 哈希链，`cmsq audit verify` 检出修改/删除。

## 验证结果（本机，2026-10-20）
* `cargo test --workspace`：30 通过（含真实 Docker 隔离/超时/内存与全链路测试，`CMSQ_REQUIRE_DOCKER=1`）。
* `cargo clippy --workspace --all-targets --all-features -- -D warnings`：通过。
* `scripts/e2e.sh`（Docker 后端）：6 个样例 → 1 晋升、3 门禁拒绝、2 失败（前视、联网），篡改检测通过。
* 截图：`docs/screenshots/01-overview.png` … `09-audit-chain.png`。

## 明确不做（P0）
实盘 / paper trading / IB；进化搜索；分布式；Redis。
