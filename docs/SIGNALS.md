# 信号刷新（只观察）

`cmsq signals --refresh [--fetch] [--id ID] [--every 1d] [--backend docker]`

- 只针对已晋升（Promoted）的策略，只用它晋升时的同一标的。代码和参数与门槛、留存样本验证时完全一致。
- 加 `--fetch` 时先经多源路由拉取最新行情。快照必须是带签名的真实行情抓取；合成数据只在 `CMSQ_ALLOW_SYNTHETIC=1`（演示）下允许。
- 在 Docker 沙箱里重跑。前视/一致性探针集中在上次信号之后新增的 K 线（至少最后 20 根），任一探针不一致就不产生信号，并写入审计（`signal_refresh_rejected`）。
- 每根新 K 线追加一条信号，历史全部保留。模拟盘按信号历史逐日调仓，所以会随策略持续跟踪。
- 每次刷新都写审计：`signal_refresh_start`、`signal_refresh`，或 `signal_refresh_noop`、`signal_refresh_failed`。

## 实盘衰减（只标记，不改状态）
每次刷新都会在宿主机上计算晋升之后的样本外表现。出现下面任一情况就标记 ⚠ LIVE DECAY：
- 最大回撤差于 -20%；
- 至少 60 根样本外 K 线，且年化 Sharpe 低于 -0.5。

我们选择**只标记、不自动降级**：样本外区间很短时噪声很大，自动拒绝会把正常回撤误判为失效；而且状态变更是不可逆的人工决定（`cmsq reject <id>`）。标记会写进刷新结果和审计，CLI 会显示出来。

## 演示
`scripts/paper_demo.sh` 在单独的 DEMO 工作区（`/tmp/cmsq-paper-demo`，根目录有 `DEMO` 文件；CLI 和看板都会显示横幅）里演示：策略在合成数据 SAMPLE_DEMO 上验证（SAMPLE_PERSIST 用同一种子延长到 2020 年，DSR 试验下限为 1），通过 `CMSQ_SAMPLE_AS_OF` 逐月模拟时间推进，每月刷新信号，再用这些合成日线做模拟盘。**这只是机制演示，不构成任何有效性证据。**
