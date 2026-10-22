# 项目状态（2026-10-22）

开发日志已移到 [CHANGELOG.md](CHANGELOG.md)。

## 已完成
- **流水线**：社区抓取（GitHub vn.py CTA）→ 摄入 → 抽取 → Docker 沙箱回测（只输出仓位）→ 宿主机计分 → 门槛 → 人工晋升 + 一次性 vault → 只观察信号 → 模拟盘。
- **门槛**：前视探针、静态检查、DSR + 全库试验账本、walk-forward（扫描按折重选参）、市场状态稳定性、2 倍成本、回撤、敞口、买入持有对比、真实数据来源、交叉校验、复权、容量。见 [DESIGN.md](DESIGN.md)。
- **市场**：A 股日线（tushare 主源 + akshare 校验）、期货主力连续（换月、平今、交易所日历、涨跌停）、CTP 行情（只观察，回放测试）、vn.py 策略兼容层。
- **研究**：参数扫描、GA 搜索、多标的组合。
- **存储与审计**：PostgreSQL 16 + TimescaleDB（最小权限角色）、可选 Redis 热层、HMAC 审计链 + 签名进程 + git 锚点（私有仓库 `cmspark-quant-anchors`）。
- **模拟盘**：按真实报价模拟撮合，事件哈希链，QIFI 导出。
- **看板**：10 个页面，已迁到 gpui-component 设计语言（[UI_DESIGN.md](UI_DESIGN.md)）。
- **文档**：QUICKSTART / DESIGN / CONFIG（本轮新增），QUICKSTART 已在全新 clone + 全新工作区上实测跑通。

## 真实数据上的结果
**0 个策略通过门槛，0 个晋升。** 这是诚实结果，不是故障（原因见 [DESIGN.md §4](DESIGN.md#4-为什么大多数策略都被拒)）。
- A 股：6 只股票 × 6 个样例帖共 36 次回测全部被拒；最好的是双均线在 SZ300750 上 Sharpe 1.25，DSR 0.837 < 0.95。
- 期货 FUT_RB：唐奇安、vn.py 双均线及其参数扫描全部被拒（最近一次：Sharpe 0.07、DSR 0.039）。
- "明日涨跌"这类偷看未来的策略每次都被前视探针抓到。
- 所有"通过/晋升/信号/模拟成交"的截图都来自 DEMO 工作区（合成数据，黄色横幅），不是证据。

## 已知缺口
- 时钟：vDSO 直接读时间、文件 ctime 在 Linux 上无法伪造（假日期兜底）。
- DEV（bwrap）后端 /out 上限靠 50 ms 轮询，可能小幅超出（Docker 后端由内核强制）；DEV 结果永不可晋升。
- 交叉校验只有 tushare↔akshare 一对；只有 tushare 提供复权因子。
- 一台机器一个共享数据库；签名进程与应用同机。
- CTP 实时行情未在真实网络验证（本机连不上 openctp）。
- 稀疏偷看只能概率性抓到；价格形态本身可能泄露年代。

## 待你决定 / 可选
- 把锚点签名公钥加到 GitHub 作为 Signing key（显示 Verified）。
- issue #11 等待审阅者复核。
