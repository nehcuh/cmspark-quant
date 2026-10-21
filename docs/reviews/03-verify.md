# Phase 3 对抗审查：验证（基于真实 e2e 运行与截图）

证据：`scripts/e2e.sh` 真实 Docker 运行输出（见 README）、`docs/screenshots/01..09`、`cargo test --workspace`（30 项）与
`cargo clippy --workspace --all-targets --all-features -D warnings` 均通过。

## Quant Domain Critic
* 6 个样例的结局符合预期且各有其因：MA 交叉 / 动量 / 布林带被成本后 OOS 与稳定性拒绝；“明日涨跌”被前视探针拦截；
  联网策略在断网沙箱失败；短期反转在含植入效应的合成数据上通过 → **门禁会拒绝大多数社区想法，这是正确的默认**。
* 尝试次数按策略累计，每次重跑都会抬高 Sharpe 门槛（第 2 次 0.40，第 20 次 0.75）。📌 跨策略的全局多重检验
  （同一想法换个帖子再提交）尚未去重到“想法”层面，只按内容 hash 去重。
* 📌 真实数据端到端未能在本机完成（TDX 服务器对本机返回空数据），只验证了 rstdx 编译、连接、握手与失败审计。

## Security Critic
* 运行时证据：`phone_home` 在容器内得到 `Network is unreachable`；隔离探针测试全部通过。
* 📌 `docker` 组授权是为本机演示而做的宿主配置变更（需用户批准）；生产建议 rootless。
* ✅（验证阶段修复）Runner 基础镜像改为按 digest 固定（`runner/Dockerfile`），Python 依赖锁版本（`requirements.lock`）。

## UX/Product Critic
* 完整操作路径已在 GUI 中实际点击验证：摄入 → 抽取 → 选中 → 提交回测（Docker）→ 门禁明细 → 确认台 → 晋升 → 信号页 → 审计页。
* 溯源：原帖 URL（`open_url`）、归档原文（`xdg-open` 本地文件）、每个 job 的 stdout/stderr 路径与 stderr 尾部直接显示。
* 📌 无自动刷新（有“刷新”按钮）；无参数编辑 UI（CLI `--params` 支持“调整后重跑”）。
