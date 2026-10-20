//! CMspark Quant Dashboard (GPUI Kit). Reads the real SQLite store; all mutating
//! actions go through `cmsq_core::Orchestrator` (same code path as the CLI), so
//! every action is state-machine checked and audited.
use cmsq_core::market;
use cmsq_core::orchestrator::IngestOutcome;
use cmsq_core::sandbox::{docker::DockerSandboxExecutor, process::ProcessSandboxExecutor, SandboxExecutor};
use cmsq_core::store::Store;
use cmsq_core::{extract, AuditEntry, Evaluation, Home, JobRecord, Orchestrator, Strategy, StrategyStatus};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::*;
use gpui_kit::base::Disableable;
use gpui_kit::prelude::FluentBuilder;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Overview,
    Library,
    Jobs,
    Signals,
    Audit,
}

impl Tab {
    const ALL: [Tab; 5] = [Tab::Overview, Tab::Library, Tab::Jobs, Tab::Signals, Tab::Audit];
    fn label(&self) -> &'static str {
        match self {
            Tab::Overview => "总览 Overview",
            Tab::Library => "策略库 Strategy Library",
            Tab::Jobs => "沙箱任务 Sandbox Jobs",
            Tab::Signals => "信号 Signals (observe-only)",
            Tab::Audit => "审计 Audit",
        }
    }
}

#[derive(Default)]
struct Snapshot {
    strategies: Vec<Strategy>,
    jobs: Vec<JobRecord>,
    signals: Vec<(cmsq_core::Signal, String, String)>,
    audit: Vec<AuditEntry>,
    audit_ok: Option<(bool, i64, String)>,
    total_attempts: i64,
    datasets: Vec<market::DataMeta>,
}

struct Dashboard {
    home: Home,
    tab: Tab,
    snap: Snapshot,
    selected: Option<String>,
    symbol_ix: usize,
    pending_promote: Option<String>,
    status: String,
    rt: tokio::runtime::Runtime,
}

fn c_bg() -> Hsla { rgb(0x0f1419).into() }
fn c_panel() -> Hsla { rgb(0x171d24).into() }
fn c_border() -> Hsla { rgb(0x2a323c).into() }
fn c_text() -> Hsla { rgb(0xe6e9ee).into() }
fn c_muted() -> Hsla { rgb(0x8a94a3).into() }
fn c_accent() -> Hsla { rgb(0x4ea1ff).into() }

fn status_color(s: StrategyStatus) -> Hsla {
    match s {
        StrategyStatus::Ingested => rgb(0x8a94a3),
        StrategyStatus::Extracted => rgb(0x4ea1ff),
        StrategyStatus::Backtesting => rgb(0xd29922),
        StrategyStatus::Evaluated => rgb(0xa371f7),
        StrategyStatus::Promoted => rgb(0x3fb950),
        StrategyStatus::Rejected => rgb(0xf0883e),
        StrategyStatus::Failed => rgb(0xf85149),
    }
    .into()
}

/// Audit timestamps are stored in UTC; show local time (Asia/Shanghai on this box).
fn local(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts).map(|d| d.with_timezone(&chrono::Local).format("%m-%d %H:%M:%S").to_string()).unwrap_or_else(|_| ts.chars().take(19).collect())
}

fn pct(v: &serde_json::Value, k: &str) -> String {
    v[k].as_f64().map(|x| format!("{:+.2}%", x * 100.0)).unwrap_or_else(|| "-".into())
}
fn num(v: &serde_json::Value, k: &str) -> String {
    v[k].as_f64().map(|x| format!("{x:.2}")).unwrap_or_else(|| "-".into())
}

fn chip(text: impl Into<SharedString>, color: Hsla) -> Div {
    div().px_2().py_0p5().rounded_md().border_1().border_color(color).text_color(color).text_xs().child(text.into())
}

fn card(title: &str, value: String, sub: String, color: Hsla) -> Div {
    div().flex_1().p_4().rounded_lg().bg(c_panel()).border_1().border_color(c_border()).flex().flex_col().gap_1()
        .child(div().text_xs().text_color(c_muted()).child(title.to_string()))
        .child(div().text_2xl().font_weight(FontWeight::BOLD).text_color(color).child(value))
        .child(div().text_xs().text_color(c_muted()).child(sub))
}

fn section(title: &str) -> Div {
    div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(c_text()).mt_2().child(title.to_string())
}

impl Dashboard {
    fn new(home: Home) -> Self {
        let mut d = Self {
            home, tab: Tab::Overview, snap: Snapshot::default(), selected: None, symbol_ix: 0, pending_promote: None,
            status: "ready".into(), rt: tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap(),
        };
        d.reload();
        d
    }

    fn reload(&mut self) {
        let r = (|| -> anyhow::Result<Snapshot> {
            let st = Store::open_readonly(&self.home.db())?;
            let v = st.verify_audit()?;
            let mut datasets = vec![];
            if let Ok(rd) = std::fs::read_dir(self.home.data_dir()) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.to_string_lossy().ends_with(".meta.json") {
                        if let Ok(m) = serde_json::from_str::<market::DataMeta>(&std::fs::read_to_string(&p).unwrap_or_default()) {
                            datasets.push(m);
                        }
                    }
                }
            }
            datasets.sort_by(|a, b| a.symbol.cmp(&b.symbol));
            Ok(Snapshot {
                strategies: st.list_strategies(None)?, jobs: st.list_jobs(None)?, signals: st.list_signals()?,
                audit: st.audit_entries(None, 200)?, audit_ok: Some((v.ok, v.entries, v.problem.unwrap_or_default())),
                total_attempts: st.total_attempts()?, datasets,
            })
        })();
        match r {
            Ok(s) => self.snap = s,
            Err(e) => self.status = format!("load error: {e:#}"),
        }
        if self.selected.is_none() {
            self.selected = self.snap.strategies.first().map(|s| s.id.clone());
        }
    }

    fn orch(&self) -> anyhow::Result<Orchestrator> {
        Orchestrator::open(self.home.clone())
    }

    fn symbol(&self) -> String {
        self.snap.datasets.get(self.symbol_ix % self.snap.datasets.len().max(1)).map(|d| d.symbol.clone()).unwrap_or_else(|| "SAMPLE_TREND".into())
    }

    fn act_ingest(&mut self) {
        self.status = match self.orch().and_then(|o| o.ingest_path(&o.home.drop_dir())) {
            Ok(v) => {
                let n = v.iter().filter(|x| matches!(x.1, IngestOutcome::New(_))).count();
                let bad = v.iter().filter(|x| matches!(x.1, IngestOutcome::Invalid(_))).count();
                format!("ingest: {} file(s) in drop/, {n} new, {} duplicate, {bad} invalid", v.len(), v.len() - n - bad)
            }
            Err(e) => format!("ingest failed: {e:#}"),
        };
        self.reload();
    }

    fn act_extract(&mut self) {
        let r: anyhow::Result<usize> = self.rt.block_on(async {
            let o = self.orch()?;
            let ex = extract::from_env()?;
            let ids: Vec<String> = o.store.list_strategies(Some(StrategyStatus::Ingested))?.into_iter().map(|s| s.id).collect();
            for i in &ids {
                o.extract(i, ex.as_ref()).await?;
            }
            Ok(ids.len())
        });
        self.status = match r { Ok(n) => format!("extracted {n} strategy(ies)"), Err(e) => format!("extract failed: {e:#}") };
        self.reload();
    }

    fn act_backtest(&mut self, id: String) {
        let sym = self.symbol();
        let r = self.rt.block_on(async {
            let o = self.orch()?;
            let exec: Box<dyn SandboxExecutor> = match DockerSandboxExecutor::connect().await {
                Ok(d) => Box::new(d),
                Err(_) => Box::new(ProcessSandboxExecutor),
            };
            o.backtest(&id, &sym, exec.as_ref(), None).await
        });
        self.status = match r {
            Ok(s) => format!("backtest {} on {sym}: {} [{}] {}", &s.strategy_id[..8], s.status, s.job_status, s.reasons.join("; ")),
            Err(e) => format!("backtest failed: {e:#}"),
        };
        self.reload();
    }

    fn act_promote(&mut self, id: String) {
        self.status = match self.orch().and_then(|o| o.promote(&id, "user:dashboard", &id[..8])) {
            Ok(s) => format!("PROMOTED {} (observe-only signal recorded)", &s.id[..8]),
            Err(e) => format!("promote refused: {e:#}"),
        };
        self.pending_promote = None;
        self.reload();
    }

    // ---------------- views ----------------
    fn sidebar(&self, cx: &mut Context<Self>) -> Div {
        let mut col = div().w(px(230.)).h_full().bg(c_panel()).border_r_1().border_color(c_border()).p_3().flex().flex_col().gap_1()
            .child(div().text_lg().font_weight(FontWeight::BOLD).text_color(c_text()).child("CMspark Quant"))
            .child(div().text_xs().text_color(c_muted()).mb_3().child("策略发现 · 沙箱验证 · 不接实盘"));
        for t in Tab::ALL {
            let active = t == self.tab;
            col = col.child(
                div().id(SharedString::from(t.label())).px_3().py_2().rounded_md().cursor_pointer().text_sm()
                    .text_color(if active { c_text() } else { c_muted() })
                    .when(active, |d| d.bg(rgb(0x22303f)))
                    .hover(|d| d.bg(rgb(0x1d2733)))
                    .child(t.label())
                    .on_click(cx.listener(move |this, _, _, cx| { this.tab = t; this.pending_promote = None; cx.notify(); })),
            );
        }
        let (ok, n, prob) = self.snap.audit_ok.clone().unwrap_or((false, 0, String::new()));
        col.child(div().flex_1())
            .child(div().text_xs().text_color(c_muted()).child(format!("workspace: {}", self.home.abs().display())))
            .child(div().text_xs().text_color(if ok { rgb(0x3fb950) } else { rgb(0xf85149) })
                .child(if ok { format!("审计链完整 ✓ {n} entries") } else { format!("审计链异常 ✗ {prob}") }))
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> Div {
        let sym = self.symbol();
        let ds = self.snap.datasets.iter().find(|d| d.symbol == sym);
        div().flex().items_center().gap_2().p_3().border_b_1().border_color(c_border()).bg(c_panel())
            .child(Button::new("ingest").primary().label("触发摄入 Ingest drop/").on_click(cx.listener(|this, _, _, cx| { this.act_ingest(); cx.notify(); })))
            .child(Button::new("extract").label("抽取 Extract").on_click(cx.listener(|this, _, _, cx| { this.act_extract(); cx.notify(); })))
            .child(Button::new("symbol").label(format!("数据集: {sym}")).on_click(cx.listener(|this, _, _, cx| { this.symbol_ix += 1; cx.notify(); })))
            .child(div().text_xs().text_color(c_muted()).child(ds.map(|d| format!("{} · {} bars · {}", d.source, d.bars, d.label)).unwrap_or_default()))
            .child(div().flex_1())
            .child(Button::new("refresh").label("刷新 Refresh").on_click(cx.listener(|this, _, _, cx| { this.reload(); this.status = "refreshed".into(); cx.notify(); })))
    }

    fn overview(&self) -> Div {
        let count = |s: StrategyStatus| self.snap.strategies.iter().filter(|x| x.status == s).count();
        let running = self.snap.jobs.iter().filter(|j| j.status == "running").count();
        let failed_jobs = self.snap.jobs.iter().filter(|j| j.status != "succeeded" && j.status != "running").count();
        let mut flow = div().flex().gap_2().items_center().flex_wrap();
        for (i, s) in [StrategyStatus::Ingested, StrategyStatus::Extracted, StrategyStatus::Backtesting, StrategyStatus::Evaluated, StrategyStatus::Promoted].iter().enumerate() {
            if i > 0 { flow = flow.child(div().text_color(c_muted()).child("→")); }
            flow = flow.child(chip(format!("{} {}", s, count(*s)), status_color(*s)));
        }
        flow = flow.child(div().text_color(c_muted()).ml_4().child("归档 archive:"))
            .child(chip(format!("Rejected {}", count(StrategyStatus::Rejected)), status_color(StrategyStatus::Rejected)))
            .child(chip(format!("Failed {}", count(StrategyStatus::Failed)), status_color(StrategyStatus::Failed)));
        let best = self.snap.jobs.iter().filter_map(|j| j.result.as_ref().map(|r| (j, r)))
            .max_by(|a, b| a.1["out_of_sample"]["sharpe"].as_f64().unwrap_or(-9.0).total_cmp(&b.1["out_of_sample"]["sharpe"].as_f64().unwrap_or(-9.0)));
        let mut recent = div().flex().flex_col().gap_1();
        for a in self.snap.audit.iter().take(12) {
            recent = recent.child(div().flex().gap_3().text_xs()
                .child(div().w(px(44.)).text_color(c_muted()).child(format!("#{}", a.seq)))
                .child(div().w(px(150.)).text_color(c_muted()).child(local(&a.ts)))
                .child(div().w(px(90.)).text_color(c_accent()).child(a.actor.clone()))
                .child(div().text_color(if a.success { c_text() } else { rgb(0xf85149).into() }).child(a.action.clone()))
                .child(div().text_color(c_muted()).child(a.target.clone().map(|t| t.chars().take(8).collect::<String>()).unwrap_or_default())));
        }
        div().flex().flex_col().gap_4().p_4()
            .child(div().flex().gap_3()
                .child(card("策略总数 Strategies", self.snap.strategies.len().to_string(), format!("{} sample-labelled", self.snap.strategies.iter().filter(|s| s.sample).count()), c_text()))
                .child(card("已晋升 Promoted", count(StrategyStatus::Promoted).to_string(), format!("{} awaiting confirmation", self.snap.strategies.iter().filter(|s| s.status == StrategyStatus::Evaluated).count()), rgb(0x3fb950).into()))
                .child(card("沙箱任务 Jobs", self.snap.jobs.len().to_string(), format!("{running} running · {failed_jobs} failed/blocked"), c_accent()))
                .child(card("搜索尝试 Attempts", self.snap.total_attempts.to_string(), "计入多重检验门槛 multiple-testing hurdle".into(), rgb(0xd29922).into())))
            .child(section("状态流转 State machine"))
            .child(div().p_3().rounded_lg().bg(c_panel()).border_1().border_color(c_border()).child(flow))
            .child(section("最佳样本外 Best OOS (after costs)"))
            .child(div().p_3().rounded_lg().bg(c_panel()).border_1().border_color(c_border()).text_sm().text_color(c_text()).child(match best {
                Some((j, r)) => {
                    let title = self.snap.strategies.iter().find(|s| s.id == j.strategy_id).map(|s| s.title.clone()).unwrap_or_default();
                    format!("{title} · {} · OOS return {} · Sharpe {} · MaxDD {} · data: {}", j.symbol, pct(&r["out_of_sample"], "total_return"), num(&r["out_of_sample"], "sharpe"), pct(&r["out_of_sample"], "max_drawdown"), j.data_source)
                }
                None => "no completed backtests yet".into(),
            }))
            .child(section("最近审计 Recent audit"))
            .child(div().p_3().rounded_lg().bg(c_panel()).border_1().border_color(c_border()).child(recent))
    }

    fn library(&self, cx: &mut Context<Self>) -> Div {
        let mut list = div().id("lib-list").w(px(430.)).h_full().overflow_y_scroll().border_r_1().border_color(c_border()).flex().flex_col();
        for s in &self.snap.strategies {
            let sel = self.selected.as_deref() == Some(&s.id);
            let id = s.id.clone();
            list = list.child(
                div().id(SharedString::from(format!("row-{}", s.id))).p_3().border_b_1().border_color(c_border()).cursor_pointer()
                    .when(sel, |d| d.bg(rgb(0x1d2733))).hover(|d| d.bg(rgb(0x1a222c)))
                    .on_click(cx.listener(move |this, _, _, cx| { this.selected = Some(id.clone()); this.pending_promote = None; cx.notify(); }))
                    .child(div().flex().gap_2().items_center()
                        .child(chip(s.status.as_str(), status_color(s.status)))
                        .child(div().text_xs().text_color(c_muted()).child(format!("{} · try {}", &s.id[..8], s.attempts)))
                        .when(s.sample, |d| d.child(chip("SAMPLE", c_muted()))))
                    .child(div().mt_1().text_sm().text_color(c_text()).child(s.title.clone()))
                    .child(div().text_xs().text_color(c_muted()).child(format!("{} · {}", s.source_site, s.author.clone().unwrap_or_default()))),
            );
        }
        div().flex().size_full().min_w_0().overflow_hidden().child(list).child(self.detail(cx))
    }

    fn detail(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let base = div().id("detail").flex_1().min_w_0().overflow_x_hidden().h_full().overflow_y_scroll().p_4().flex().flex_col().gap_2();
        let Some(s) = self.selected.as_ref().and_then(|id| self.snap.strategies.iter().find(|s| &s.id == id)) else {
            return base.child(div().text_color(c_muted()).child("select a strategy"));
        };
        let jobs: Vec<&JobRecord> = self.snap.jobs.iter().filter(|j| j.strategy_id == s.id).collect();
        let ev: Option<Evaluation> = Store::open_readonly(&self.home.db()).ok().and_then(|st| st.latest_evaluation(&s.id).ok().flatten());
        let id = s.id.clone();
        let url = s.source_url.clone();
        let raw = s.raw_path.clone();
        let mut d = base
            .child(div().flex().gap_2().items_center().child(chip(s.status.as_str(), status_color(s.status)))
                .child(div().text_lg().font_weight(FontWeight::BOLD).text_color(c_text()).child(s.title.clone())))
            .child(div().text_xs().text_color(c_muted()).child(format!("id {} · ingested {} · scraped {} · attempts {}", s.id, s.ingested_at.format("%Y-%m-%d %H:%M"), s.scraped_at.clone().unwrap_or_default(), s.attempts)));
        if let Some(r) = &s.last_reason {
            let good = matches!(s.status, StrategyStatus::Evaluated | StrategyStatus::Promoted);
            d = d.child(div().p_2().rounded_md().bg(if good { rgb(0x16261b) } else { rgb(0x2a1d1d) }).text_sm()
                .text_color(if good { rgb(0x7ee2a8) } else { rgb(0xffb4a8) }).child(format!("{} {r}", if good { "状态 note:" } else { "原因 reason:" })));
        }
        d = d.child(section("溯源 Provenance"))
            .child(div().flex().gap_2().items_center()
                .child(Button::new("open-src").label("打开原帖 Open source").on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(u) = &url { cx.open_url(u); this.status = format!("opened {u}"); }
                    cx.notify();
                })))
                .child(Button::new("open-raw").label("归档原文 Raw archive").on_click(cx.listener(move |this, _, _, cx| {
                    let _ = std::process::Command::new("xdg-open").arg(&raw).spawn();
                    this.status = format!("raw archive: {raw}");
                    cx.notify();
                })))
                .child(div().text_xs().text_color(c_muted()).child(format!("{} · {}", s.source_site, s.source_url.clone().unwrap_or_default()))));
        if let Some(e) = &s.extracted {
            d = d.child(section(&format!("抽取 Extraction ({})", e.extractor)))
                .child(div().text_sm().text_color(c_text()).child(e.summary.clone()))
                .child(div().text_xs().text_color(c_muted()).child(format!("假设 assumptions: {}", e.assumptions.join(" | "))))
                .child(div().text_xs().text_color(c_accent()).child(format!("参数 params: {}", serde_json::Value::Object(e.parameters.clone()))));
            let mut risks = div().flex().flex_wrap().gap_1();
            for r in &e.risks {
                let col: Hsla = match r.severity { cmsq_core::Severity::High => rgb(0xf85149).into(), cmsq_core::Severity::Warn => rgb(0xd29922).into(), _ => c_muted() };
                risks = risks.child(chip(format!("{}: {}", r.code, r.message), col));
            }
            d = d.child(risks);
            if let Some(code) = &e.code {
                d = d.child(div().p_2().rounded_md().bg(rgb(0x0b0f14)).border_1().border_color(c_border()).text_xs().font_family("monospace").text_color(rgb(0xc9d1d9)).child(code.clone()));
            }
        }
        // actions
        let can_bt = matches!(s.status, StrategyStatus::Extracted | StrategyStatus::Rejected | StrategyStatus::Failed | StrategyStatus::Evaluated) && s.extracted.as_ref().is_some_and(|e| e.code.is_some());
        let can_promote = s.status == StrategyStatus::Evaluated && ev.as_ref().is_some_and(|e| e.passed);
        let id2 = id.clone();
        let mut actions = div().flex().gap_2().items_center().mt_2()
            .child(Button::new("bt").primary().disabled(!can_bt).label(if s.attempts > 0 { format!("重跑回测 Re-run on {}", self.symbol()) } else { format!("提交回测 Backtest on {}", self.symbol()) })
                .on_click(cx.listener(move |this, _, _, cx| { this.act_backtest(id2.clone()); cx.notify(); })));
        let id3 = id.clone();
        actions = actions.child(Button::new("promote").disabled(!can_promote).label("晋升… Promote").on_click(cx.listener(move |this, _, _, cx| { this.pending_promote = Some(id3.clone()); cx.notify(); })));
        d = d.child(actions);
        if self.pending_promote.as_deref() == Some(&s.id) {
            let id4 = id.clone();
            d = d.child(div().p_3().rounded_lg().border_1().border_color(rgb(0xd29922)).bg(rgb(0x2a2412)).flex().flex_col().gap_2()
                .child(div().text_sm().text_color(rgb(0xf2cc60)).child(format!("确认台 Confirmation: promote {} ? Gate passed. Signals stay OBSERVE-ONLY (no broker). This is audited as user:dashboard.", &s.id[..8])))
                .child(div().flex().gap_2()
                    .child(Button::new("confirm").primary().label(format!("确认晋升 Confirm {}", &s.id[..8])).on_click(cx.listener(move |this, _, _, cx| { this.act_promote(id4.clone()); cx.notify(); })))
                    .child(Button::new("cancel").label("取消 Cancel").on_click(cx.listener(|this, _, _, cx| { this.pending_promote = None; this.status = "promotion cancelled".into(); cx.notify(); })))));
        }
        if let Some(ev) = &ev {
            d = d.child(section(&format!("门禁 Gate: {}", if ev.passed { "PASSED ✓" } else { "FAILED ✗" })));
            if let Some(cs) = ev.checks["checks"].as_array() {
                for c in cs {
                    let ok = c["ok"].as_bool().unwrap_or(false);
                    d = d.child(div().text_xs().text_color(if ok { rgb(0x3fb950) } else { rgb(0xf85149) })
                        .child(format!("{} {} — {}", if ok { "✓" } else { "✗" }, c["check"].as_str().unwrap_or(""), c["detail"].as_str().unwrap_or(""))));
                }
            }
        }
        d = d.child(section("回测与日志 Backtests & logs"));
        for j in jobs {
            let m = j.result.as_ref().map(|r| r["out_of_sample"].clone()).unwrap_or_default();
            let split = j.result.as_ref().map(|r| format!("train {}..{} | test {}..{}", r["split"]["train"]["start"].as_str().unwrap_or(""), r["split"]["train"]["end"].as_str().unwrap_or(""), r["split"]["test"]["start"].as_str().unwrap_or(""), r["split"]["test"]["end"].as_str().unwrap_or(""))).unwrap_or_default();
            let stderr = std::fs::read_to_string(format!("{}/stderr.log", j.job_dir)).unwrap_or_default();
            let tail: String = stderr.lines().rev().take(4).map(|l| if l.chars().count() > 160 { format!("{}…", l.chars().take(160).collect::<String>()) } else { l.to_string() }).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
            d = d.child(div().p_2().rounded_md().bg(c_panel()).border_1().border_color(c_border()).flex().flex_col().gap_1()
                .child(div().text_xs().text_color(c_text()).child(format!("job {} · {} · {} · {} · {} ms · {}", &j.id[..8], j.backend, j.status, j.symbol, j.duration_ms.unwrap_or(0), j.data_source)))
                .when(j.status == "succeeded" && !split.is_empty(), |x| x.child(div().text_xs().text_color(c_muted()).child(split)))
                .when(j.status == "succeeded", |x| x.child(div().text_xs().text_color(c_accent()).child(format!("OOS net return {} · Sharpe {} · MaxDD {} · turnover {}x/yr", pct(&m, "total_return"), num(&m, "sharpe"), pct(&m, "max_drawdown"), num(&m, "turnover_annual")))))
                .when(j.status == "succeeded" && j.result.is_some(), |x| {
                    let r = j.result.as_ref().unwrap();
                    x.child(div().text_xs().text_color(c_muted()).child(format!("2x cost stress {} · buy&hold OOS {} · in-sample {} · costs: comm {} / stamp(sell) {} / slip {} · look-ahead probe: {}",
                        pct(&r["out_of_sample_cost_2x"], "total_return"), pct(&r["buy_and_hold_oos"], "total_return"), pct(&r["in_sample"], "total_return"),
                        r["cost_model"]["commission"], r["cost_model"]["stamp_duty_sell"], r["cost_model"]["slippage"],
                        if r["lookahead_probe"]["detected"].as_bool() == Some(false) { "clean" } else { "FLAGGED" })))
                     .when(r["data_caveats"].as_array().is_some_and(|a| !a.is_empty()), |x| x.child(div().text_xs().text_color(rgb(0xd29922)).child(format!("⚠ {}", r["data_caveats"].as_array().unwrap().iter().filter_map(|c| c.as_str()).collect::<Vec<_>>().join(" · ")))))
                })
                .when(!tail.is_empty(), |x| x.child(div().text_xs().font_family("monospace").text_color(rgb(0xffb4a8)).child(tail)))
                .child(div().text_xs().text_color(c_muted()).child(format!("logs: {}/{{stdout,stderr}}.log", j.job_dir))));
        }
        d
    }

    fn jobs(&self) -> Stateful<Div> {
        let mut t = div().id("jobs").p_4().flex().flex_col().gap_1().overflow_y_scroll().size_full()
            .child(div().flex().gap_2().text_xs().text_color(c_muted()).pb_2().border_b_1().border_color(c_border())
                .child(div().w(px(80.)).flex_none().child("JOB")).child(div().w(px(150.)).flex_none().child("BACKEND")).child(div().w(px(150.)).flex_none().child("STATUS"))
                .child(div().w(px(120.)).flex_none().child("SYMBOL")).child(div().w(px(80.)).flex_none().child("MS")).child(div().w(px(110.)).flex_none().child("OOS RET")).child(div().flex_1().child("STRATEGY / ERROR")));
        for j in &self.snap.jobs {
            let title = self.snap.strategies.iter().find(|s| s.id == j.strategy_id).map(|s| s.title.clone()).unwrap_or_default();
            let ok = j.status == "succeeded";
            t = t.child(div().flex().gap_2().text_xs().py_1().border_b_1().border_color(c_border())
                .child(div().w(px(80.)).flex_none().text_color(c_text()).child(j.id[..8].to_string()))
                .child(div().w(px(150.)).flex_none().text_color(if j.backend == "docker" { c_accent() } else { rgb(0xd29922).into() }).child(j.backend.clone()))
                .child(div().w(px(150.)).flex_none().text_color(if ok { rgb(0x3fb950) } else { rgb(0xf85149) }).child(j.status.clone()))
                .child(div().w(px(120.)).flex_none().text_color(c_text()).child(j.symbol.clone()))
                .child(div().w(px(80.)).flex_none().text_color(c_muted()).child(j.duration_ms.unwrap_or(0).to_string()))
                .child(div().w(px(110.)).flex_none().text_color(c_text()).child(j.result.as_ref().map(|r| pct(&r["out_of_sample"], "total_return")).unwrap_or("-".into())))
                .child(div().flex_1().min_w_0().overflow_hidden().text_color(c_muted()).child(format!("{title}{}", j.error.as_ref().map(|e| format!("  ✗ {e}")).unwrap_or_default()))));
        }
        t
    }

    fn signals(&self) -> Div {
        let mut d = div().p_4().flex().flex_col().gap_2()
            .child(div().p_3().rounded_md().border_1().border_color(rgb(0xd29922)).text_sm().text_color(rgb(0xf2cc60))
                .child("仅观察 OBSERVE-ONLY：未连接任何券商/实盘接口；信号仅来自人工确认晋升的策略。No broker is connected."));
        if self.snap.signals.is_empty() {
            d = d.child(div().text_color(c_muted()).child("no signals yet — promote a strategy that passed the gate"));
        }
        for (s, title, st) in &self.snap.signals {
            d = d.child(div().p_3().rounded_md().bg(c_panel()).border_1().border_color(c_border()).flex().gap_4().text_sm()
                .child(div().text_color(c_text()).child(s.date.clone()))
                .child(div().text_color(c_accent()).child(s.symbol.clone()))
                .child(div().text_color(c_text()).child(format!("target position {:.2}", s.target_position)))
                .child(div().text_color(c_muted()).child(format!("{title} [{st}] · job {}", &s.job_id[..8]))));
        }
        d
    }

    fn audit(&self) -> Stateful<Div> {
        let mut d = div().id("audit").p_4().flex().flex_col().gap_1().overflow_y_scroll().size_full();
        for a in &self.snap.audit {
            d = d.child(div().flex().gap_3().text_xs().py_0p5()
                .child(div().w(px(44.)).text_color(c_muted()).child(format!("#{}", a.seq)))
                .child(div().w(px(150.)).text_color(c_muted()).child(local(&a.ts)))
                .child(div().w(px(110.)).text_color(c_accent()).child(a.actor.clone()))
                .child(div().w(px(260.)).text_color(if a.success { c_text() } else { rgb(0xf85149).into() }).child(a.action.clone()))
                .child(div().w(px(70.)).text_color(c_muted()).child(a.target.clone().map(|t| t.chars().take(8).collect::<String>()).unwrap_or_default()))
                .child(div().text_color(rgb(0x5c6773)).font_family("monospace").child(a.hash[..16].to_string())));
        }
        d
    }
}

impl Render for Dashboard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = match self.tab {
            Tab::Overview => div().id("ov").size_full().overflow_y_scroll().child(self.overview()).into_any_element(),
            Tab::Library => self.library(cx).into_any_element(),
            Tab::Jobs => self.jobs().into_any_element(),
            Tab::Signals => self.signals().into_any_element(),
            Tab::Audit => self.audit().into_any_element(),
        };
        div().size_full().flex().bg(c_bg()).text_color(c_text()).font_family("Noto Sans CJK SC")
            .child(self.sidebar(cx))
            .child(div().flex_1().min_w_0().h_full().flex().flex_col().overflow_hidden()
                .child(self.toolbar(cx))
                .child(div().flex_1().min_h_0().child(body))
                .child(div().px_3().py_1().border_t_1().border_color(c_border()).bg(c_panel()).text_xs().text_color(c_muted()).child(self.status.clone())))
    }
}

fn main() {
    let home = Home::from_env();
    if let Err(e) = home.init().and_then(|_| Store::open(&home.db()).map(|_| ())) {
        eprintln!("cannot open workspace {}: {e:#}", home.root.display());
        std::process::exit(1);
    }
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        let opts = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1440.), px(900.)), cx))),
            titlebar: Some(TitlebarOptions { title: Some("CMspark Quant Dashboard".into()), ..Default::default() }),
            ..Default::default()
        };
        gpui_kit::open_window(opts, cx, |_, cx| cx.new(|_| Dashboard::new(home.clone()))).expect("open window");
    });
}
