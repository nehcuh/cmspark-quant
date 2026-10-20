# Phase 2 对抗审查：实现（generator 产出后，三位 critic 独立审查 → 修复）

状态列：✅ 已修复并有测试/截图证据；📌 记录为已知差距。

## Quant Domain Critic
| # | 发现 | 处理 |
|---|---|---|
| Q1 | Runner 只做 `shift(1)`，但 `shift(-1)` 类前视在 shift 后仍然泄露（pos_t 依赖 close_t）。 | ✅ Runner 新增 **point-in-time probe**：6 个切点 × 3 种未来扰动（截断 / 未来价格×1.3 / ×0.7），切点前信号必须不变，否则 `lookahead_detected` → Failed。`future_peek` 样例被拦截（e2e + `tests/pipeline.rs`）。 |
| Q2 | 短期反转策略年换手 105x，成本假设一旦偏乐观结论就反转。 | ✅ 结果增加 `out_of_sample_cost_2x`；门禁新增 `cost_stress_2x`（2 倍成本下 OOS 仍须为正）。 |
| Q3 | 合成数据上通过 ≠ 有效。 | ✅ 结果 `data_caveats` 标注 SYNTHETIC，Dashboard 回测卡片黄色警示；文档明确 `SAMPLE_RANGE` 有人为植入的均值回归。 |
| Q4 | TDX 日线未复权，除权日会制造虚假收益。 | ✅ meta `adjustment` 字段 + `data_caveats` 警告；📌 复权处理列为 P1。 |
| Q5 | 只有一个 70/30 切分，没有 walk-forward。 | 📌 P0 用 OOS 两半稳定性 + 多重检验门槛补偿；walk-forward 列为 P1。 |
| Q6 | 门禁缺少基准对比。 | ✅ 结果含 `buy_and_hold_oos`，Dashboard 展示；📌 未作为门禁条件（避免对单边行情过度惩罚）。 |

## Security Critic
| # | 发现 | 处理 |
|---|---|---|
| S1 | DEV（bwrap）后端跑出的结果也能晋升，等于绕过 Docker 隔离。 | ✅ 门禁 `hardened_sandbox`（`require_hardened_backend=true`）；测试 `dev_backend_results_can_never_be_promoted`。 |
| S2 | 为让容器 uid 10001 写入，`out/` 被设为 0777。 | ✅ 去掉 0777；容器以 `/out` 属主（宿主非 root 用户）uid:gid 运行，root 被显式排除。 |
| S3 | 容器输出（stdout/stderr/result.json）不可信，可撑爆内存/磁盘。 | ✅ 日志 1 MiB 截断，result.json 4 MiB 上限，只按 JSON 解析。 |
| S4 | 沙箱策略需要可验证，而不是“声称”。 | ✅ `tests/sandbox_integration.rs` 在真实 Docker 中验证：外网不可达、/data 只读、rootfs 只读、非 root、无 docker.sock、超时强杀、内存超限失败；bwrap 后端同样验证。`CMSQ_REQUIRE_DOCKER=1` 防止“静默跳过=假通过”。 |
| S5 | 编排进程需要访问 docker.sock（docker 组≈root），与“Orchestrator 不持有特权”不完全一致。 | 📌 已知差距：建议 rootless Docker 或单独的沙箱 broker 进程（只暴露 `SandboxRequest` 接口）。Dashboard/CLI 本身不挂载 socket，容器内无 socket。 |
| S6 | 请求层校验：拒绝 network allowlist、docker.sock/`/proc`/`/sys`/`/dev`/`/` 挂载、相对路径、`..`、除 `/out` 外的可写挂载。 | ✅ `sandbox::validate` + 单测。 |
| S7 | 审计日志可被直接改库。 | ✅ 触发器禁止 UPDATE/DELETE；哈希链（长度前缀防字段拼接歧义）检测修改/删除/重排；e2e 演示篡改副本被检出。 |

## UX/Product Critic
| # | 发现 | 处理 |
|---|---|---|
| U1 | 组件按钮为浅色主题，与深色界面冲突。 | ✅ `Theme::change(Dark)`。 |
| U2 | 审计时间是 UTC，用户在 UTC+8。 | ✅ Dashboard 按本地时区显示（DB 仍存 UTC）。 |
| U3 | 详情面板横向溢出，长文本被截断。 | ✅ `min_w_0` / overflow 修正后自动换行（截图 04/05）。 |
| U4 | “gate passed” 被红色“原因”样式展示，误导。 | ✅ Evaluated/Promoted 显示绿色 note。 |
| U5 | 晋升需要确认台。 | ✅ 两步确认面板（截图 04），CLI 需输入 id 前 8 位；错误确认被拒并审计。 |
| U6 | 回测在 UI 线程同步执行，长任务会卡住界面。 | 📌 P0 回测 <1 s 可接受；P1 改为后台任务 + 日志流。 |
| U7 | 缺少多策略对比视图。 | 📌 Sandbox Jobs 表提供 OOS 收益横向对比；完整对比工作台为 P1（与 Synthesizer 优先级一致）。 |
