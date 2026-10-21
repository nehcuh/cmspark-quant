# 第二轮审查（review-r2-quant-security / review-r2-product-code）修复映射

| 问题 | 修复 | 证据 |
|---|---|---|
| C1 时钟/真实日期泄露、截断点固定 | runner 2.1.0 libfaketime 冻结 2000-01-01、假交易日期、屏蔽 /proc/uptime 等、文件 mtime 固定；种子随机截断点 + 尾部 n−1/n−2/n−4，种子记入审计；静态检查新增 i+k、负 np.roll、iloc 未来访问 | fixtures r2_clock_* / r2_cut_aware / r2_clock_probe 全部 Rejected；clock_probe 暴露 0 |
| H1 DSR 方差池可被污染 | 剔除前视被抓与 |SR/日|>0.2，≥5 个用 1.4826·MAD；N 下限 20 | scorer 单测；e2e_real N=20 |
| H2 vault 换标题重用 | vault 按 idea 簇+标的、代码哈希、仓位相关>0.9 记账，`VaultSpent` | pipeline 测试与 e2e 第 5 节：重发被拒 |
| H3 数据篡改 | 快照 sha 必须存在且等于 HMAC 复核的审计 fetch 记录；快照只读+哈希；sample/csv 不过真实门禁 | `data_tampering_is_refused`；e2e 第 6 节 |
| M 印花税历史 / ST 5% / 新股无涨跌停 / BJ 92 / 日历缺口停牌 / 未复权手数费用 | scorer `Rules`、`limit_for`、`stamp_duty_sides` | scorer 单测 |
| M 稀疏偷看 | 更多截断点 + 正则 | r2_sparse_peek Rejected（概率性） |
| M helper 环境变量 | `scrubbed_env` 去除 TOKEN/KEY/SECRET/CMSQ_/TUSHARE | pyhelper 单测；e2e_real 日志 grep token = 0 |
| M fork bomb 标签 | `pids_exhausted`（启发式） | ADV_forkbomb |
| M runner 身份 | 标签 + 镜像 digest | gate::runner_pinned 单测 |
| M 旧 HMAC 工作区提示 | seq 1 不匹配时说明原因 | store 单测 |
| M README | token 要求、cargo --version 检查 | README |
| M 交叉校验容差 | 见 MARKET_DATA.md，记录于审计 | router 单测；e2e_real |
| M 买入持有基准 | `beats_buy_and_hold` 门禁项 | gate 单测 |
| M Dashboard 数据健康 | 新"行情数据"页 | screenshots/14-market-data.png |
