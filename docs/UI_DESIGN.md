# 面板设计规范 UI Design Guide

面板是 GPUI 原生应用（`crates/cmsq-dashboard`），组件统一来自 longbridge 的
[gpui-component](https://github.com/longbridge/gpui-component)（通过 `gpui-kit` 一个依赖引入，风格参考 shadcn/ui）。
本文件约束以后所有面板改动；改 UI 的 PR 要按 §5 自查并附截图。

## 1. 原则
- **先复用成熟组件**：gpui-component 有的（Button、Input、Tag、Alert、Table、AreaChart/LineChart、resizable 分栏、Icon）一律直接用，不再手写同类控件。
- **组件源码可控**：只依赖 `gpui-kit`（它固定 gpui-component / gpui-base / gpui-pre 一整套同版本）。不直接依赖 `gpui`、`gpui-component` 或别的 UI crate。
- **严格不重复依赖**：`cargo tree -d -p cmsq-dashboard | grep -i gpui` 里只能出现一套 `gpui-pre`（当前 0.3.9，由 gpui-kit 0.7.3 固定）。升级时整体升级 `gpui-kit`。
- **统一设计语言**：颜色、圆角、字号都从主题来，同类信息用同一种组件。

## 2. 主题与颜色 token
深色主题：`Theme::change(ThemeMode::Dark, ..)`（gpui-component 默认深色，shadcn 风格）。
视图代码只能用 `ui::tok()` 的语义 token，**禁止 `rgb(0x..)` 字面量**（单元测试 `no_raw_colour_literals` 会拦）。

| token | 来源 (Theme) | 用途 |
|---|---|---|
| `bg` | `background` | 窗口底色 |
| `panel` | `group_box` | 卡片、侧栏、工具栏、状态栏 |
| `border` | `border` | 所有分隔线、卡片边框 |
| `fg` / `muted` | `foreground` / `muted_foreground` | 正文 / 次要说明 |
| `accent` | `blue` | actor、链接型文字、Extracted 状态 |
| `ok` | `success` | 通过、盈利、Promoted |
| `err` | `danger` | 失败、亏损、审计异常、Failed |
| `warn` | `yellow` | 进行中、提示、DEMO/模拟说明 |
| `caution` | `warning` | Rejected（被拒但诚实） |
| `vault` | `magenta` | 一次性留存样本（vault）段、Evaluated |
| `cyan` | `cyan` | GatePassed |
| `hover` / `active` / `sunken` | `list_hover` / `list_active` / `muted` | 列表悬停 / 选中 / 日志区底色 |

浅色底的提示块用 `ui::tint(color)`（12% 透明度），不要另调颜色。

## 3. 组件规则
| 场景 | 组件 |
|---|---|
| 动作 | `Button`（主操作 `.primary()`，带 `IconName`），危险/不可逆操作必须先过输入确认语的确认台 |
| 状态、标签 | `ui::chip` → gpui-component `Tag`（outline + small） |
| 全局横幅（DEMO、无 Docker） | `Alert::warning/error(..).banner()`，固定在工具栏下方，**DEMO 横幅永远不能去掉或折叠** |
| 页内提示块（SIMULATED、确认台） | `ui::callout(tok().warn)` |
| 表格型数据（审计、成交） | `Table/TableHeader/TableRow/TableCell`，定宽列加 `.flex_shrink_0()`，最后一列 `.flex_1()` |
| 权益曲线 | `AreaChart`（`equity_chart`），vault 段用 `vault` 色单独一段、同一 y 轴 |
| 主区与任务日志 | `v_resizable` + `resizable_panel`，日志默认 132px，可拖 60–420px |
| 统计卡片、侧栏导航 | 用主题 token 拼的组合（`card`、`sidebar`），gpui-component 没有更贴切的无状态组件 |

## 4. 图标与动画
- 图标只用 gpui-component 自带的 Lucide 图标集（`IconName::*`，资源由 `gpui_kit::assets::Assets` 提供）。每个导航项一个图标，按钮图标放在文字左边，不混用 emoji 当图标（✓ ✗ 只用于检查结果文字）。
- 动画只用组件自带的（按钮/分栏手柄过渡、图表首次绘入），时长取主题 motion token，系统“减少动态效果”会自动跳过。不手写定时器动画，不做装饰性动效。

## 5. 截图自查流程（参考 taste-skill：先定方向、找参照、再截图复查）
1. **美术方向**：本面板是“审计台”——信息密度高、深色、克制，颜色只表达状态；任何“看起来像能下单”的元素都不允许出现（没有实盘路径）。
2. **参照**：改动前把要改的页面截图放进 `docs/screenshots/before/`，作为对比基准；视觉参照是 gpui-component 自带的 story 示例和 shadcn/ui 深色主题。
3. **重拍**：`sg docker -c "REAL_HOME=.. DEMO_HOME=.. [SHOTS_HOME=.. SCEN_DEMO_HOME=.. V7_HOME=..] scripts/ui_screenshots.sh"`。脚本构建 release 面板，重拍 `docs/screenshots/` 全部截图，并在 `/tmp/cmsq-ui-review/` 生成 before/after 对比图。
4. **逐张复查**（每条都要过）：
   - 没有 `rgb(` 字面量、没有混入其他风格的控件；
   - DEMO 工作区截图顶部有 DEMO 横幅，SIMULATED 提示还在；
   - 文字不重叠、不截断关键数字，表格列对齐；
   - 鼠标已移开（没有残留 tooltip/hover）；
   - 侧栏审计链为绿色（红色说明工作区本身坏了，换一个工作区，不要拍进文档）。
5. 不合格就改代码再拍，直到对比图全部通过，再提交截图。
