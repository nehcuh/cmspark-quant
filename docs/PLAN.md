# P0 实施计划（方案 A 为主干）

规格来源：产品讨论中的「角色 5 Synthesizer」修订需求 + 「Trade-off Judge」最终架构（方案 A 为主干，吸收方案 B 轻量化思想）。

## 模块

| 模块 | 位置 | 说明 |
|---|---|---|
| Drop 摄入适配器 | `cmsq-core::ingest` | CMspark 抓取结果落盘 JSON（`docs/DROP_FORMAT.md`），Core 接管后归档到 `raw/<id>/` |
| 抽取 | `cmsq-core::extract` | `Extractor` trait；`HeuristicExtractor`（离线、无 key）；`LlmExtractor`（`llm` feature + 环境变量） |
| 行情数据 | `cmsq-core::market` | `MarketDataSource` trait；`SampleSource`（离线合成样例）、`RstdxSource`（`tdx` feature，宿主机经 TDX 协议拉日线） |
| 沙箱 | `cmsq-core::sandbox` | `SandboxExecutor` trait；`DockerSandboxExecutor`（bollard，默认）+ `ProcessSandboxExecutor`（bwrap，dev 模式） |
| 回测 Runner | `runner/` | 容器内固定版本 Python（pandas/numpy 锁版本）+ 只读挂载的 huquant 交易日历 |
| 评估与门禁 | `cmsq-core::gate` | 样本外成本后为正 + 稳定性 + 多重检验调整 |
| 状态机/编排 | `cmsq-core::{state,orchestrator}` | 合法转换、审计、失败归档、尝试次数 |
| 存储 | `cmsq-core::store` | SQLite（元数据/状态/哈希链审计）+ 文件产物按策略 ID 分目录 |
| CLI | `cmsq-cli` | `cmsq init/ingest/extract/backtest/evaluate/promote/list/show/jobs/audit verify/data fetch` |
| Dashboard | `cmsq-dashboard` | gpui + gpui-component，读真实 SQLite |

## 阶段
1. Plan（本文件）→ `docs/reviews/01-plan.md` 对抗审查 → 修订
2. Implement（TDD：状态机、审计链、抽取、门禁先写测试）→ `docs/reviews/02-implement.md`
3. Verify（e2e、截图、clippy）→ `docs/reviews/03-verify.md`

## 明确不做（P0）
实盘 / IB / paper trading；进化搜索；分布式调度；Redis 热存储。
