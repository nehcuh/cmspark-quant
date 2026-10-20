use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::label::Label;
use gpui_kit::component::list::List;
use gpui_kit::component::sidebar::{Sidebar, SidebarItem};
use gpui_kit::component::table::{DataTable, TableColumn};
use gpui_kit::component::tabs::Tabs;
use gpui_kit::*;
use std::sync::Arc;

#[derive(Clone, Debug)]
struct StrategyItem {
    id: String,
    source: String,
    title: String,
    status: String,
    sharpe: f64,
}

#[derive(Clone, Debug)]
struct SandboxJob {
    id: String,
    strategy: String,
    phase: String,
    progress: u8,
}

struct Dashboard {
    active_tab: usize,
    strategies: Vec<StrategyItem>,
    jobs: Vec<SandboxJob>,
}

impl Dashboard {
    fn new() -> Self {
        Self {
            active_tab: 0,
            strategies: vec![
                StrategyItem {
                    id: "S-001".into(),
                    source: "QuantConnect Forum".into(),
                    title: "Mean Reversion on BTC 4h".into(),
                    status: "Backtested".into(),
                    sharpe: 1.42,
                },
                StrategyItem {
                    id: "S-002".into(),
                    source: "Reddit r/algotrading".into(),
                    title: "Momentum + Volatility Filter".into(),
                    status: "Sandbox Running".into(),
                    sharpe: 0.0,
                },
                StrategyItem {
                    id: "S-003".into(),
                    source: "GitHub quant-strategies".into(),
                    title: "Pairs Trading Equity Basket".into(),
                    status: "Extracted".into(),
                    sharpe: 0.0,
                },
            ],
            jobs: vec![
                SandboxJob {
                    id: "JOB-42".into(),
                    strategy: "S-002".into(),
                    phase: "Backtest".into(),
                    progress: 67,
                },
                SandboxJob {
                    id: "JOB-41".into(),
                    strategy: "S-001".into(),
                    phase: "Completed".into(),
                    progress: 100,
                },
            ],
        }
    }
}

impl Render for Dashboard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strategies = self.strategies.clone();
        let jobs = self.jobs.clone();

        div()
            .flex()
            .flex_row()
            .size_full()
            .bg(rgb(0x1e1e1e))
            .child(
                // Sidebar
                Sidebar::new()
                    .width(px(220.))
                    .child(
                        SidebarItem::new("overview")
                            .label("Overview")
                            .selected(self.active_tab == 0)
                            .on_click(cx.listener(|this, _, _, _| {
                                this.active_tab = 0;
                            })),
                    )
                    .child(
                        SidebarItem::new("strategies")
                            .label("Strategy Library")
                            .selected(self.active_tab == 1)
                            .on_click(cx.listener(|this, _, _, _| {
                                this.active_tab = 1;
                            })),
                    )
                    .child(
                        SidebarItem::new("sandboxes")
                            .label("Sandbox Jobs")
                            .selected(self.active_tab == 2)
                            .on_click(cx.listener(|this, _, _, _| {
                                this.active_tab = 2;
                            })),
                    )
                    .child(
                        SidebarItem::new("signals")
                            .label("Signals")
                            .selected(self.active_tab == 3)
                            .on_click(cx.listener(|this, _, _, _| {
                                this.active_tab = 3;
                            })),
                    ),
            )
            .child(
                // Main content
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .p_4()
                    .gap_4()
                    .child(
                        Label::new(match self.active_tab {
                            0 => "Overview",
                            1 => "Strategy Library",
                            2 => "Sandbox Jobs",
                            _ => "Signals",
                        })
                        .text_xl()
                        .font_weight(FontWeight::BOLD),
                    )
                    .child(match self.active_tab {
                        0 => self.render_overview(cx),
                        1 => self.render_strategies(&strategies, cx),
                        2 => self.render_jobs(&jobs, cx),
                        _ => self.render_signals(cx),
                    }),
            )
    }
}

impl Dashboard {
    fn render_overview(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child("CMspark Quant Prototype")
            .child("Periodic scrape → extract/combine → sandbox backtest → filter → signals")
            .child(
                div()
                    .flex()
                    .gap_4()
                    .child(self.metric_card("Active Jobs", &self.jobs.len().to_string()))
                    .child(self.metric_card("Strategies", &self.strategies.len().to_string()))
                    .child(self.metric_card("Avg Sharpe (done)", "1.42")),
            )
            .child(
                Button::new("refresh")
                    .label("Refresh (mock)")
                    .on_click(|_, _, _| println!("Refresh triggered")),
            )
    }

    fn metric_card(&self, title: &str, value: &str) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .p_4()
            .bg(rgb(0x2d2d2d))
            .rounded_md()
            .gap_1()
            .child(Label::new(title).text_sm().text_color(rgb(0xaaaaaa)))
            .child(Label::new(value).text_2xl().font_weight(FontWeight::BOLD))
    }

    fn render_strategies(
        &self,
        strategies: &[StrategyItem],
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut table = DataTable::new("strategies");
        for s in strategies {
            table = table.row(vec![
                s.id.clone().into(),
                s.source.clone().into(),
                s.title.clone().into(),
                s.status.clone().into(),
                format!("{:.2}", s.sharpe).into(),
            ]);
        }
        table
            .columns(vec![
                TableColumn::new("ID").width(px(80.)),
                TableColumn::new("Source").width(px(180.)),
                TableColumn::new("Title").flex_1(),
                TableColumn::new("Status").width(px(140.)),
                TableColumn::new("Sharpe").width(px(80.)),
            ])
            .into_any_element()
    }

    fn render_jobs(&self, jobs: &[SandboxJob], _cx: &mut Context<Self>) -> impl IntoElement {
        div().flex().flex_col().gap_2().children(jobs.iter().map(|j| {
            div()
                .flex()
                .flex_row()
                .gap_4()
                .p_2()
                .bg(rgb(0x2d2d2d))
                .rounded_md()
                .child(Label::new(&j.id))
                .child(Label::new(&j.strategy))
                .child(Label::new(&j.phase))
                .child(Label::new(format!("{}%", j.progress)))
        }))
    }

    fn render_signals(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child("No live signals yet (prototype).")
            .child("Future: integrate with IB paper trading via controlled gateway.")
    }
}

fn main() {
    application()
        .with_assets(assets::Assets)
        .run(|cx| {
            init(cx);

            open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(1200.), px(800.)),
                        cx,
                    ))),
                    ..Default::default()
                },
                cx,
                |_, cx| cx.new(|_| Dashboard::new()),
            )
            .expect("Failed to open window");
        });
}
