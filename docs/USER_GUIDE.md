# 使用指南

## 1. 系统做什么
从社区抓取策略 → 结构化抽取 → 在断网的 Docker 沙箱里回测（容器只输出目标仓位，成交、成本、指标、前视检测和门槛都由宿主机 Rust 计算）→ 门槛（DSR、walk-forward、一次性留存样本、买入持有对比、市场状态稳定性、真实行情、资金容量）→ 人工确认晋升 → 只观察信号与模拟盘。**没有任何真实下单接口。**

## 2. 部署
前置：Linux、Docker（当前用户能访问 daemon）、rustup（`rust-toolchain.toml` 固定版本，自动安装）。

```bash
scripts/deploy.sh [--hot] [--anchor-repo URL] [--home DIR]
```

| 组件 | 说明 |
|---|---|
| `db` | PostgreSQL 16 + TimescaleDB，仅监听 127.0.0.1:55432。应用账号不能改、删审计/试验/行情证据表 |
| `signer` | 审计签名进程（默认模式）。密钥只在 `cmsq-signer` 卷中，uid 10001，无网络；应用经 `~/.local/state/cmsq/run/sock` 请求签名，读不到密钥 |
| `redis`（`--hot`） | 可选热层：最新行情缓存、看板推送、多数据源共享限流。不存任何权威数据 |
| 锚点（`--anchor-repo`） | 每小时把审计链头提交（SSH 签名）并推送到 git 仓库，`cmsq audit verify-git` 校验全部历史锚点 |

部署写出 `~/.config/cmsq/cmsq.env`（`CMSQ_HOME`、`CMSQ_AUDIT_SIGNER` 等），使用前 `set -a; . ~/.config/cmsq/cmsq.env; set +a`。

已有工作区若仍用应用侧 `audit.key`，部署会自动迁移：导入签名进程 → 按已验证链头登记 → 经签名进程全链验证 → 覆写删除旧密钥。新工作区通过显式登记（`cmsq audit signer-enroll`）接入签名进程，签名进程不接受未登记的链。

## 3. 数据源
真实行情需要 Tushare token：写入 `~/.config/cmsq/secrets.env`（`TUSHARE_TOKEN=...`，权限 600，切勿提交）。路由顺序 tushare → akshare → baostock → TDX，各源独立限流（tushare ≤400 次/分钟，akshare 串行且间隔 ≥1 秒）；启用 `--hot` 时限流计数在 Redis 中共享。多源收盘价逐日交叉校验，差异 >0.1% 的日子会被标记。

```bash
cmsq data calendar
cmsq data fetch SH600519 --count 2500
cmsq data sources
```

## 4. 日常流程
```bash
cmsq scrape --every 6h                 # 抓取社区策略 → 摄入 → 抽取 → 沙箱回测（定时）
cmsq list                              # 策略状态
cmsq show <id>                         # 来源、评估、任务、审计
cmsq sweep / portfolio                 # 参数扫描（每个点计入 DSR 试验账本）/ 多标的组合
cmsq promote <id>                      # 需键入确认；运行一次性留存样本，通过才晋升
cmsq signals                           # 已晋升策略的只观察信号
cmsq paper ...                         # 模拟盘（见 docs/PAPER_TRADING.md）
```

## 5. 看板
`cargo run --release -p cmsq-dashboard`（GPUI）。总览、策略、试验、信号、模拟盘、审计等页面，截图见 `docs/screenshots/`。

## 6. 审计与运维
```bash
cmsq audit verify                      # 全链 HMAC 校验 + 锚点
cmsq audit anchor-git --repo DIR --sign --push
cmsq audit verify-git --repo DIR
docker compose -f deploy/docker-compose.yml exec signer cmsq signer rotate --dir /var/lib/cmsq-signer   # 轮换密钥，之后重启 signer
```
细节：`docs/AUDIT_KEY.md`、`docs/REDIS.md`、`docs/MARKET_DATA.md`、`docs/PAPER_TRADING.md`、`docs/STATUS.md`。

## 7. 边界
- 不实盘、不接券商；CTP 只接行情接口。
- 门槛很严格：真实数据上目前没有社区策略通过（见 `docs/STATUS.md`），这是预期内的诚实结果。

## 门槛：市场状态（regime）稳定性

`regime_stability` 检查策略是否只在某一种行情里赚钱。开发期（去掉热身段）的每根 K 线按"当时之前"的行情打两组标签，不看当根及以后的数据：

- 趋势：过去 60 根买入持有收益 > +5% 为 `trend_up`，< −5% 为 `trend_down`，其余为 `range`；
- 波动：过去 20 根收益率标准差高于窗口中位数为 `vol_high`，否则 `vol_low`。

只有 ≥60 根 K 线的状态才计入。通过条件：每组至少有 2 个计入的状态；每组最多 1 个状态亏钱；任何计入的状态亏损都不超过 −10%。组合（portfolio）用等权买入持有曲线来划分状态。每次回测的 `result.json` 里有 `regimes` 字段，记录每个状态的 K 线数、策略收益和同期市场收益。

说明：波动的高/低分界用的是整个窗口的中位数，这只影响标签，不进入策略。阈值目前写在代码默认值里（`GateConfig`：60 根、最多 1 个、−10%）。

## 参数扫描的 walk-forward：每折重新选参

单次回测的 `walk_forward` 是固定参数切 4 折。参数扫描（`cmsq sweep`）如果也这样做，最优参数本身就是看过全部开发期才选出来的，折内结果并不是样本外。

所以扫描的 `walk_forward` 改为"锚定式、每折重新选参"：第 k 折开始前，只用这之前的全部 K 线（含热身段）计算每个无前视的扫描点的夏普，选最高的那个，再只用它在第 k 折上的表现计分。门槛看这些折里有几折为正（至少 3 折）。每折选了哪个参数、训练用了多少根 K 线、折内结果，都记录在扫描结果的 `walk_forward_refit` 字段和 `gate` 审计里。DSR 仍按扫描的全部点数计入试验账本。
