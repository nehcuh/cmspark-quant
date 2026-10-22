# 使用指南

> 第一次使用请先读 [QUICKSTART.md](QUICKSTART.md)（上手）和 [DESIGN.md](DESIGN.md)（设计理念）；命令与环境变量速查见 [CONFIG.md](CONFIG.md)。本文只补充部署组件、运维和门槛实现细节。

## 1. 部署
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

> **⚠️ `cmsq.env` 只有一份，每次部署都会覆盖 `CMSQ_HOME`。**
> - `~/.config/cmsq/cmsq.env` 全机只有一份，`scripts/deploy.sh` 每次运行都会整份重写它，`CMSQ_HOME` 被设为**这次运行**的工作区。
> - 不带 `--home` 时，工作区取当前 shell 里的 `CMSQ_HOME` 环境变量（若已 `source` 过 cmsq.env，就是上次的值），没有才用 `~/cmsq-home`。所以试跑后**不要**指望不带参数重跑就能恢复默认。
> - 做过试跑/测试部署（例如 `--home /tmp/qs-home`）之后，务必用你真正的工作区再跑一次，把它指回去：
>   ```bash
>   scripts/deploy.sh --home <你的真实工作区>     # 例如 --home ~/cmsq-home
>   ```
>   重跑不会丢数据（数据库、签名进程、已有工作区都是幂等处理）。
> - 查看当前指向哪个工作区：
>   ```bash
>   grep CMSQ_HOME ~/.config/cmsq/cmsq.env
>   ```
>   改完后已打开的终端需要重新 `set -a; . ~/.config/cmsq/cmsq.env; set +a`。


已有工作区若仍用应用侧 `audit.key`，部署会自动迁移：导入签名进程 → 按已验证链头登记 → 经签名进程全链验证 → 覆写删除旧密钥。新工作区通过显式登记（`cmsq audit signer-enroll`）接入签名进程，签名进程不接受未登记的链。

## 2. 数据源与日常流程
数据源配置、限流和日常命令见 [QUICKSTART.md §2/§6](QUICKSTART.md) 与 [CONFIG.md §4/§5](CONFIG.md)。

## 3. 看板
`cargo run --release -p cmsq-dashboard`（GPUI）。总览、策略、试验、信号、模拟盘、审计等页面，截图见 [QUICKSTART.md §4](QUICKSTART.md#4-打开看板) 与 `docs/screenshots/`。

## 4. 审计与运维
```bash
cmsq audit verify                      # 全链 HMAC 校验 + 锚点
cmsq audit anchor-git --repo DIR --sign --push
cmsq audit verify-git --repo DIR
docker compose -f deploy/docker-compose.yml exec signer cmsq signer rotate --dir /var/lib/cmsq-signer   # 轮换密钥，之后重启 signer
```
细节：`docs/AUDIT_KEY.md`、`docs/REDIS.md`、`docs/MARKET_DATA.md`、`docs/PAPER_TRADING.md`、`docs/STATUS.md`。

## 5. 边界
- 不实盘、不接券商；CTP 只接行情接口。
- 门槛很严格：真实数据上目前没有社区策略通过（见 [STATUS.md](STATUS.md)），这是预期内的诚实结果。

## 门槛：市场状态（regime）稳定性

`regime_stability` 检查策略是否只在某一种行情里赚钱。开发期（去掉热身段）的每根 K 线按"当时之前"的行情打两组标签，不看当根及以后的数据：

- 趋势：过去 60 根买入持有收益 > +5% 为 `trend_up`，< −5% 为 `trend_down`，其余为 `range`；
- 波动：过去 20 根收益率标准差高于窗口中位数为 `vol_high`，否则 `vol_low`。

只有 ≥60 根 K 线的状态才计入。通过条件：每组至少有 2 个计入的状态；每组最多 1 个状态亏钱；任何计入的状态亏损都不超过 −10%。组合（portfolio）用等权买入持有曲线来划分状态。每次回测的 `result.json` 里有 `regimes` 字段，记录每个状态的 K 线数、策略收益和同期市场收益。

说明：波动的高/低分界用的是整个窗口的中位数，这只影响标签，不进入策略。阈值可用环境变量调整：`CMSQ_GATE_REGIME_MIN_BARS`（默认 60）、`CMSQ_GATE_REGIME_MAX_LOSING`（默认 1）、`CMSQ_GATE_REGIME_MAX_LOSS`（默认 -0.10）；实际使用的值随每次门槛结果记录在 `checks.config` 里。

## 参数扫描的 walk-forward：每折重新选参

单次回测的 `walk_forward` 是固定参数切 4 折。参数扫描（`cmsq sweep`）如果也这样做，最优参数本身就是看过全部开发期才选出来的，折内结果并不是样本外。

所以扫描的 `walk_forward` 改为"锚定式、每折重新选参"：第 k 折开始前，只用这之前的全部 K 线（含热身段）计算每个无前视的扫描点的夏普，选最高的那个，再只用它在第 k 折上的表现计分。门槛看这些折里有几折为正（至少 3 折）。每折选了哪个参数、训练用了多少根 K 线、折内结果，都记录在扫描结果的 `walk_forward_refit` 字段和 `gate` 审计里。DSR 仍按扫描的全部点数计入试验账本。

## 遗传算法参数搜索（GA）

```bash
cmsq sweep <ID> --symbol FUT_RB --ga --population 8 --generations 4 --seed 1 \
  --grid '{"fast_window":[3,5,8,10,15,20],"slow_window":[30,40,60,90,120]}'
```

参数空间比网格上限（64 点）大时，用 `--ga` 只评估其中一部分：初始种群随机抽样，之后每代保留最优 2 个，用锦标赛选父代、均匀交叉、按基因变异，已经评估过的点不再重复评估。同一个 seed 结果可复现；总评估数 population×generations 不超过 256。

**每个实际评估的点都是一次完整的沙箱回测，也是试验账本里的一条 trial**：搜索结束后，最优点按包含全部评估点的账本重新计算 DSR，因此搜得越多、门槛越高。结果里的 `ga_generations` 记录每代新增评估数、累计评估数和当前最优夏普；walk-forward 同样按每折重新选参（在全部已评估点中选）。仪表盘"参数扫描"页同时显示 `sweep` 和 `ga` 两类运行。
