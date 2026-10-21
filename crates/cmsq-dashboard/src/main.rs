//! CMspark Quant Dashboard (GPUI Kit). Reads the configured store (PostgreSQL
//! primary, SQLite offline); all mutating
//! actions go through `cmsq_core::Orchestrator` (same code path as the CLI), so
//! every action is state-machine checked and audited.
//!
//! Long actions (ingest / extract / backtest / promote+vault) run on a background
//! thread with their own runtime and Orchestrator; the UI thread only polls a
//! shared progress log, so the window never blocks.
use cmsq_core::market;
use cmsq_core::orchestrator::IngestOutcome;
use cmsq_core::sandbox::{
    docker::DockerSandboxExecutor, process::ProcessSandboxExecutor, SandboxExecutor,
};
use cmsq_core::store::Store;
use cmsq_core::{
    extract, AuditEntry, Evaluation, Home, JobRecord, Orchestrator, Strategy, StrategyStatus,
};
use gpui_kit::base::Disableable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Overview,
    Library,
    Compare,
    Data,
    Jobs,
    Signals,
    Paper,
    Research,
    Futures,
    Audit,
}

impl Tab {
    const ALL: [Tab; 10] = [
        Tab::Overview,
        Tab::Library,
        Tab::Compare,
        Tab::Research,
        Tab::Futures,
        Tab::Data,
        Tab::Jobs,
        Tab::Signals,
        Tab::Paper,
        Tab::Audit,
    ];
    fn from_env() -> Tab {
        match std::env::var("CMSQ_DASH_TAB")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "library" => Tab::Library,
            "compare" => Tab::Compare,
            "research" | "sweep" | "portfolio" => Tab::Research,
            "futures" => Tab::Futures,
            "data" => Tab::Data,
            "jobs" => Tab::Jobs,
            "signals" => Tab::Signals,
            "paper" => Tab::Paper,
            "audit" => Tab::Audit,
            _ => Tab::Overview,
        }
    }
    fn label(&self) -> &'static str {
        match self {
            Tab::Overview => "总览 Overview",
            Tab::Library => "策略库 Strategy Library",
            Tab::Compare => "对比 Compare",
            Tab::Data => "行情数据 Market Data",
            Tab::Jobs => "沙箱任务 Sandbox Jobs",
            Tab::Signals => "信号 Signals (observe-only)",
            Tab::Paper => "模拟交易 Paper Trading",
            Tab::Research => "参数扫描/组合 Sweep & Portfolio",
            Tab::Futures => "期货/CTP Futures & Market Data",
            Tab::Audit => "审计 Audit",
        }
    }
}

struct PaperView {
    id: String,
    strategy: String,
    status: String,
    state: serde_json::Value,
    curve: Vec<(i64, Option<String>, f64)>,
    fills: Vec<serde_json::Value>,
}

#[derive(Default)]
struct Snapshot {
    strategies: Vec<Strategy>,
    jobs: Vec<JobRecord>,
    signals: Vec<(cmsq_core::Signal, String, String)>,
    /// (session id, strategy id, status, latest state, equity curve, recent fills)
    paper: Vec<PaperView>,
    audit: Vec<AuditEntry>,
    audit_ok: Option<(bool, i64, String)>,
    total_attempts: i64,
    trials: i64,
    datasets: Vec<market::DataMeta>,
    /// HMAC-verified fetch attestation per dataset (Err = refused)
    attest: Vec<Result<serde_json::Value, String>>,
    health: std::collections::BTreeMap<String, cmsq_core::data::router::SourceHealth>,
    runs: Vec<cmsq_core::store::ResearchRun>,
    ticks: i64,
    backend: String,
}

/// Shared between the UI thread and the background job thread.
#[derive(Default)]
struct Progress {
    running: Option<String>,
    log: Vec<String>,
    version: u64,
    finished: Option<String>,
    /// Some(true) = Docker reachable, Some(false) = not, None = still probing
    docker: Option<bool>,
    docker_detail: String,
}

struct Dashboard {
    home: Home,
    tab: Tab,
    snap: Snapshot,
    selected: Option<String>,
    symbol_ix: usize,
    pending_promote: Option<String>,
    confirm: Entity<InputState>,
    compare: Vec<String>,
    run_sel: Option<String>,
    status: String,
    progress: Arc<Mutex<Progress>>,
    seen_version: u64,
    _poll: Option<Task<()>>,
}

fn c_bg() -> Hsla {
    rgb(0x0f1419).into()
}
fn c_panel() -> Hsla {
    rgb(0x171d24).into()
}
fn c_border() -> Hsla {
    rgb(0x2a323c).into()
}
fn c_text() -> Hsla {
    rgb(0xe6e9ee).into()
}
fn c_muted() -> Hsla {
    rgb(0x8a94a3).into()
}
fn c_accent() -> Hsla {
    rgb(0x4ea1ff).into()
}

fn status_color(s: StrategyStatus) -> Hsla {
    match s {
        StrategyStatus::Ingested => rgb(0x8a94a3),
        StrategyStatus::Extracted => rgb(0x4ea1ff),
        StrategyStatus::Backtesting => rgb(0xd29922),
        StrategyStatus::Evaluated => rgb(0xa371f7),
        StrategyStatus::GatePassed => rgb(0x2ea8a0),
        StrategyStatus::Promoted => rgb(0x3fb950),
        StrategyStatus::Rejected => rgb(0xf0883e),
        StrategyStatus::Failed => rgb(0xf85149),
    }
    .into()
}

/// Audit timestamps are stored in UTC; show local time (Asia/Shanghai on this box).
fn local(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| ts.chars().take(19).collect())
}

fn pct(v: &serde_json::Value, k: &str) -> String {
    v[k].as_f64()
        .map(|x| format!("{:+.2}%", x * 100.0))
        .unwrap_or_else(|| "-".into())
}
fn num(v: &serde_json::Value, k: &str) -> String {
    v[k].as_f64()
        .map(|x| format!("{x:.2}"))
        .unwrap_or_else(|| "-".into())
}

fn chip(text: impl Into<SharedString>, color: Hsla) -> Div {
    div()
        .px_2()
        .py_0p5()
        .rounded_md()
        .border_1()
        .border_color(color)
        .text_color(color)
        .text_xs()
        .child(text.into())
}

fn card(title: &str, value: String, sub: String, color: Hsla) -> Div {
    div()
        .flex_1()
        .p_4()
        .rounded_lg()
        .bg(c_panel())
        .border_1()
        .border_color(c_border())
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(c_muted())
                .child(title.to_string()),
        )
        .child(
            div()
                .text_2xl()
                .font_weight(FontWeight::BOLD)
                .text_color(color)
                .child(value),
        )
        .child(div().text_xs().text_color(c_muted()).child(sub))
}

/// date,equity,... from a job's equity.csv (written by the trusted host scorer)
fn read_equity(job_dir: &str) -> Vec<(String, f64)> {
    std::fs::read_to_string(format!("{job_dir}/equity.csv"))
        .unwrap_or_default()
        .lines()
        .skip(1)
        .filter_map(|l| {
            let mut it = l.split(',');
            Some((it.next()?.to_string(), it.next()?.parse().ok()?))
        })
        .collect()
}

/// Column-style equity chart built from plain divs. `split` = index where the
/// vault (final holdout) begins; those columns are drawn in a different colour.
fn equity_chart(pts: &[(String, f64)], split: Option<usize>, height: f32) -> Div {
    if pts.len() < 2 {
        return div()
            .text_xs()
            .text_color(c_muted())
            .child("no equity curve");
    }
    const COLS: usize = 180;
    let step = (pts.len() as f64 / COLS as f64).max(1.0);
    let idx: Vec<usize> = (0..COLS.min(pts.len()))
        .map(|i| ((i as f64 * step) as usize).min(pts.len() - 1))
        .collect();
    let (lo, hi) = idx
        .iter()
        .map(|&i| pts[i].1)
        .fold((f64::MAX, f64::MIN), |(a, b), v| (a.min(v), b.max(v)));
    let span = (hi - lo).max(1e-9);
    let base = pts[0].1;
    let mut bars = div().flex().items_end().gap(px(1.)).h(px(height)).w_full();
    for &i in &idx {
        let v = pts[i].1;
        let hgt = 4.0 + (height - 4.0) * ((v - lo) / span) as f32;
        let vault = split.is_some_and(|s| i >= s);
        let col: Hsla = if vault {
            rgb(0xa371f7).into()
        } else if v >= base {
            rgb(0x3fb950).into()
        } else {
            rgb(0xf85149).into()
        };
        bars = bars.child(div().flex_1().h(px(hgt)).bg(col).opacity(0.8));
    }
    let last = pts.last().unwrap();
    div().flex().flex_col().gap_1().child(bars).child(
        div()
            .flex()
            .text_xs()
            .text_color(c_muted())
            .child(div().flex_1().child(pts[0].0.clone()))
            .child(div().child(format!(
                "¥{:.0} → ¥{:.0} ({:+.1}%)  min ¥{lo:.0} max ¥{hi:.0}{}",
                base,
                last.1,
                (last.1 / base - 1.0) * 100.0,
                if split.is_some() {
                    "  · purple = vault holdout"
                } else {
                    ""
                }
            )))
            .child(div().flex_1().flex().justify_end().child(last.0.clone())),
    )
}

/// Display-only trade stats line (never part of the gate).
fn trade_stats_line(r: &serde_json::Value) -> Option<String> {
    let t = &r["trade_stats"];
    if !t.is_object() {
        return None;
    }
    let p = |k: &str| {
        t[k].as_f64()
            .map(|x| format!("{:.0}%", x * 100.0))
            .unwrap_or_else(|| "-".into())
    };
    let f = |k: &str, d: usize| {
        t[k].as_f64()
            .map(|x| format!("{x:.d$}"))
            .unwrap_or_else(|| "-".into())
    };
    let ct = r["close_today"]["lots"]
        .as_f64()
        .filter(|x| *x > 0.0)
        .map(|x| {
            format!(
                " · close-today {x:.0} lots ¥{:.0}",
                r["close_today"]["fees"].as_f64().unwrap_or(0.0)
            )
        })
        .unwrap_or_default();
    Some(format!("交易统计 trades (display only, not gated): {} round trips · win rate {} · P/L ratio {} · profit factor {} · avg hold {} bars (max {}){ct}",
        t["round_trips"], p("win_rate"), f("pl_ratio", 2), f("profit_factor", 2), f("avg_bars_held", 1), t["max_bars_held"]))
}

/// QIFI-style account summary of a passed vault run.
fn qifi_line(r: &serde_json::Value) -> Option<String> {
    let a = &r["account"];
    if !a.is_object() {
        return None;
    }
    let f = |k: &str| a[k].as_f64().unwrap_or(0.0);
    let units = f("final_units");
    let pos = if units.abs() < 1e-9 {
        "flat".to_string()
    } else if a["exchange"].is_string() {
        format!(
            "{} {:.0} lots",
            if units > 0.0 { "long" } else { "short" },
            units.abs()
        )
    } else {
        format!("{:.0} shares", units)
    };
    Some(format!("QIFI 账户 (observe-only paper) {} · balance ¥{:.0} (from ¥{:.0}) · margin ¥{:.0} · {pos} @ {} · {} fills — export: cmsq export-qifi <id>",
        a["trading_day"].as_str().unwrap_or(""), f("balance"), f("start_equity"), f("margin"), f("final_price"), a["fills"].as_array().map(|x| x.len()).unwrap_or(0)))
}

fn section(title: &str) -> Div {
    div()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(c_text())
        .mt_2()
        .child(title.to_string())
}

impl Dashboard {
    fn new(home: Home, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let confirm =
            cx.new(|cx| InputState::new(window, cx).placeholder("type the confirmation phrase"));
        let progress = Arc::new(Mutex::new(Progress::default()));
        {
            // probe the docker daemon off the UI thread
            let p = progress.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let r = rt.block_on(DockerSandboxExecutor::connect());
                let mut g = p.lock().unwrap();
                g.docker = Some(r.is_ok());
                g.docker_detail = r.err().map(|e| e.to_string()).unwrap_or_default();
                g.version += 1;
            });
        }
        let poll = cx.spawn(
            async move |this: WeakEntity<Self>, cx: &mut AsyncApp| loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                    break;
                }
            },
        );
        let mut d = Self {
            home,
            tab: Tab::from_env(),
            snap: Snapshot::default(),
            selected: None,
            symbol_ix: 0,
            pending_promote: None,
            confirm,
            compare: vec![],
            run_sel: None,
            status: "ready".into(),
            progress,
            seen_version: 0,
            _poll: Some(poll),
        };
        d.reload();
        if let Ok(k) = std::env::var("CMSQ_DASH_RUN") {
            d.run_sel = d
                .snap
                .runs
                .iter()
                .find(|r| r.kind == k)
                .map(|r| r.id.clone());
        }
        if let Some(i) = d
            .snap
            .datasets
            .iter()
            .position(|m| m.symbol == "SAMPLE_REGIME")
        {
            d.symbol_ix = i;
        }
        d
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let (v, finished) = {
            let mut g = self.progress.lock().unwrap();
            (g.version, g.finished.take())
        };
        if v != self.seen_version {
            self.seen_version = v;
            if let Some(f) = finished {
                self.status = f;
                self.reload();
            }
            cx.notify();
        }
    }

    fn busy(&self) -> bool {
        self.progress.lock().unwrap().running.is_some()
    }

    fn docker(&self) -> Option<bool> {
        self.progress.lock().unwrap().docker
    }

    /// Run `f` on a background thread with its own runtime + Orchestrator; its
    /// progress lines stream into the UI log.
    fn run_bg<F>(&mut self, label: String, f: F)
    where
        F: FnOnce(&Orchestrator, &tokio::runtime::Runtime) -> anyhow::Result<String>
            + Send
            + 'static,
    {
        if self.busy() {
            self.status = "a job is already running".into();
            return;
        }
        {
            let mut g = self.progress.lock().unwrap();
            g.running = Some(label.clone());
            g.log.push(format!(
                "{} ▶ {label}",
                chrono::Local::now().format("%H:%M:%S")
            ));
            g.version += 1;
        }
        self.status = format!("running: {label}");
        let (home, p) = (self.home.clone(), self.progress.clone());
        std::thread::spawn(move || {
            let push = {
                let p = p.clone();
                move |m: &str| {
                    let mut g = p.lock().unwrap();
                    g.log
                        .push(format!("{}   {m}", chrono::Local::now().format("%H:%M:%S")));
                    let n = g.log.len();
                    if n > 400 {
                        g.log.drain(..n - 400);
                    }
                    g.version += 1;
                }
            };
            let r = (|| {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()?;
                let mut o = Orchestrator::open(home)?;
                o.progress = Some(Arc::new(push.clone()));
                f(&o, &rt)
            })();
            let msg = match r {
                Ok(m) => m,
                Err(e) => format!("{label} failed: {e:#}"),
            };
            push(&format!("■ {msg}"));
            let mut g = p.lock().unwrap();
            g.running = None;
            g.finished = Some(msg);
            g.version += 1;
        });
    }

    fn reload(&mut self) {
        let r = (|| -> anyhow::Result<Snapshot> {
            let st = Store::open_configured_readonly(
                &self.home.db(),
                &self.home.db_config(),
                self.home.audit_config()?,
            )?;
            let v = st.verify_audit()?;
            let mut datasets = vec![];
            if let Ok(rd) = std::fs::read_dir(self.home.data_dir()) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.to_string_lossy().ends_with(".meta.json") {
                        if let Ok(m) = serde_json::from_str::<market::DataMeta>(
                            &std::fs::read_to_string(&p).unwrap_or_default(),
                        ) {
                            datasets.push(m);
                        }
                    }
                }
            }
            datasets.sort_by(|a, b| a.symbol.cmp(&b.symbol));
            let attest = datasets
                .iter()
                .map(|m| match st.latest_fetch_attestation(&m.symbol) {
                    Ok(Some(a)) if a.details["snapshot_sha256"] == m.extra["snapshot_sha256"] => {
                        Ok(a.details)
                    }
                    Ok(Some(_)) => Err("meta.json does not match the audited snapshot".into()),
                    Ok(None) => Err("no audited fetch".into()),
                    Err(e) => Err(format!("{e:#}")),
                })
                .collect();
            let health = std::fs::read_to_string(self.home.data_dir().join("_sources_health.json"))
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            Ok(Snapshot {
                strategies: st.list_strategies(None)?,
                jobs: st.list_jobs(None)?,
                signals: st.list_signals()?,
                paper: st
                    .list_paper_sessions()?
                    .into_iter()
                    .take(10)
                    .map(|(id, strategy, _, status, _)| {
                        let state = st
                            .latest_paper_snapshot(&id)
                            .ok()
                            .flatten()
                            .map(|x| x.1)
                            .unwrap_or_default();
                        let curve = st.paper_equity_curve(&id).unwrap_or_default();
                        let fills = st
                            .list_paper_events(&id, 0, 100_000)
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|e| e.1 == "filled")
                            .map(|e| e.2["fill"].clone())
                            .rev()
                            .take(20)
                            .collect();
                        PaperView {
                            id,
                            strategy,
                            status,
                            state,
                            curve,
                            fills,
                        }
                    })
                    .collect(),
                audit: st.audit_entries(None, 200)?,
                audit_ok: Some((v.ok, v.entries, v.problem.unwrap_or_default())),
                total_attempts: st.total_attempts()?,
                trials: st.trial_count()?,
                datasets,
                attest,
                health,
                runs: st.list_runs(None)?,
                ticks: st.tick_count(None)?,
                backend: st.backend().to_string(),
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

    fn symbol(&self) -> String {
        self.snap
            .datasets
            .get(self.symbol_ix % self.snap.datasets.len().max(1))
            .map(|d| d.symbol.clone())
            .unwrap_or_else(|| "SAMPLE_TREND".into())
    }

    fn act_ingest(&mut self) {
        self.run_bg("ingest drop/".into(), |o, _| {
            let v = o.ingest_path(&o.home.drop_dir())?;
            let n = v
                .iter()
                .filter(|x| matches!(x.1, IngestOutcome::New(_)))
                .count();
            let bad = v
                .iter()
                .filter(|x| matches!(x.1, IngestOutcome::Invalid(_)))
                .count();
            Ok(format!(
                "ingest: {} file(s) in drop/, {n} new, {} duplicate, {bad} invalid",
                v.len(),
                v.len() - n - bad
            ))
        });
    }

    fn act_extract(&mut self) {
        self.run_bg("extract".into(), |o, rt| {
            let ex = extract::from_env()?;
            let ids: Vec<String> = o
                .store
                .list_strategies(Some(StrategyStatus::Ingested))?
                .into_iter()
                .map(|s| s.id)
                .collect();
            for i in &ids {
                rt.block_on(o.extract(i, ex.as_ref()))?;
            }
            Ok(format!("extracted {} strategy(ies)", ids.len()))
        });
    }

    fn act_backtest(&mut self, id: String, dev: bool) {
        let sym = self.symbol();
        self.run_bg(
            format!(
                "backtest {} on {sym}{}",
                &id[..8],
                if dev { " (DEV backend)" } else { "" }
            ),
            move |o, rt| {
                let exec: Box<dyn SandboxExecutor> = if dev {
                    Box::new(ProcessSandboxExecutor)
                } else {
                    Box::new(rt.block_on(DockerSandboxExecutor::connect())?)
                };
                let s = rt.block_on(o.backtest(&id, &sym, exec.as_ref(), None))?;
                Ok(format!(
                    "backtest {} on {sym}: {} [{}] {}",
                    &s.strategy_id[..8],
                    s.status,
                    s.job_status,
                    s.reasons.join("; ")
                ))
            },
        );
    }

    fn act_promote(&mut self, id: String, typed: String) {
        self.pending_promote = None;
        self.run_bg(
            format!("promote {} (one-time vault holdout)", &id[..8]),
            move |o, rt| {
                let exec = rt.block_on(DockerSandboxExecutor::connect())?;
                let s = rt.block_on(o.promote(&id, "user:dashboard", &typed, &exec))?;
                Ok(match s.status {
                    StrategyStatus::Promoted => format!(
                        "PROMOTED {} — vault passed; observe-only signal recorded",
                        &s.id[..8]
                    ),
                    st => format!(
                        "{st} {} — {}",
                        &s.id[..8],
                        s.last_reason.unwrap_or_default()
                    ),
                })
            },
        );
    }

    // ---------------- views ----------------
    fn sidebar(&self, cx: &mut Context<Self>) -> Div {
        let mut col = div()
            .w(px(230.))
            .h_full()
            .bg(c_panel())
            .border_r_1()
            .border_color(c_border())
            .p_3()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::BOLD)
                    .text_color(c_text())
                    .child("CMspark Quant"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(c_muted())
                    .mb_3()
                    .child("策略发现 · 沙箱验证 · 不接实盘"),
            );
        for t in Tab::ALL {
            let active = t == self.tab;
            col = col.child(
                div()
                    .id(SharedString::from(t.label()))
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .text_sm()
                    .text_color(if active { c_text() } else { c_muted() })
                    .when(active, |d| d.bg(rgb(0x22303f)))
                    .hover(|d| d.bg(rgb(0x1d2733)))
                    .child(t.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab = t;
                        this.pending_promote = None;
                        cx.notify();
                    })),
            );
        }
        let (ok, n, prob) = self
            .snap
            .audit_ok
            .clone()
            .unwrap_or((false, 0, String::new()));
        let (badge, bcol): (String, Hsla) = match self.docker() {
            Some(true) => ("sandbox: Docker (hardened) ✓".into(), rgb(0x3fb950).into()),
            Some(false) => ("sandbox: NO DOCKER ✗".into(), rgb(0xf85149).into()),
            None => ("sandbox: probing docker…".into(), c_muted()),
        };
        col.child(div().flex_1())
            .child(div().text_xs().text_color(bcol).child(badge))
            .child(
                div()
                    .text_xs()
                    .text_color(c_muted())
                    .child(format!("workspace: {}", self.home.abs().display())),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(c_muted())
                    .child(format!("database: {}", self.snap.backend)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(if ok { rgb(0x3fb950) } else { rgb(0xf85149) })
                    .child(if ok {
                        format!("审计链完整 ✓ {n} entries (HMAC + anchor)")
                    } else {
                        format!("审计链异常 ✗ {prob}")
                    }),
            )
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> Div {
        let sym = self.symbol();
        let ds = self.snap.datasets.iter().find(|d| d.symbol == sym);
        div()
            .flex()
            .items_center()
            .gap_2()
            .p_3()
            .border_b_1()
            .border_color(c_border())
            .bg(c_panel())
            .child(
                Button::new("ingest")
                    .primary()
                    .disabled(self.busy())
                    .label("触发摄入 Ingest drop/")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.act_ingest();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("extract")
                    .disabled(self.busy())
                    .label("抽取 Extract")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.act_extract();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("symbol")
                    .label(format!("数据集: {sym}"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.symbol_ix += 1;
                        cx.notify();
                    })),
            )
            .child(
                div().text_xs().text_color(c_muted()).child(
                    ds.map(|d| format!("{} · {} bars · {}", d.source, d.bars, d.label))
                        .unwrap_or_default(),
                ),
            )
            .child(div().flex_1())
            .child(
                Button::new("refresh")
                    .label("刷新 Refresh")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reload();
                        this.status = "refreshed".into();
                        cx.notify();
                    })),
            )
    }

    fn overview(&self) -> Div {
        let count = |s: StrategyStatus| {
            self.snap
                .strategies
                .iter()
                .filter(|x| x.status == s)
                .count()
        };
        let running = self
            .snap
            .jobs
            .iter()
            .filter(|j| j.status == "running")
            .count();
        let failed_jobs = self
            .snap
            .jobs
            .iter()
            .filter(|j| j.status != "succeeded" && j.status != "running")
            .count();
        let mut flow = div().flex().gap_2().items_center().flex_wrap();
        for (i, s) in [
            StrategyStatus::Ingested,
            StrategyStatus::Extracted,
            StrategyStatus::Backtesting,
            StrategyStatus::Evaluated,
            StrategyStatus::GatePassed,
            StrategyStatus::Promoted,
        ]
        .iter()
        .enumerate()
        {
            if i > 0 {
                flow = flow.child(div().text_color(c_muted()).child("→"));
            }
            flow = flow.child(chip(format!("{} {}", s, count(*s)), status_color(*s)));
        }
        flow = flow
            .child(div().text_color(c_muted()).ml_4().child("归档 archive:"))
            .child(chip(
                format!("Rejected {}", count(StrategyStatus::Rejected)),
                status_color(StrategyStatus::Rejected),
            ))
            .child(chip(
                format!("Failed {}", count(StrategyStatus::Failed)),
                status_color(StrategyStatus::Failed),
            ));
        let devjobs: Vec<(&JobRecord, &serde_json::Value)> = self
            .snap
            .jobs
            .iter()
            .filter(|j| j.kind == "dev")
            .filter_map(|j| j.result.as_ref().map(|r| (j, r)))
            .collect();
        let cands: Vec<(StrategyStatus, Option<String>, f64)> = devjobs
            .iter()
            .map(|(j, r)| {
                let s = self.snap.strategies.iter().find(|s| s.id == j.strategy_id);
                (
                    s.map(|s| s.status).unwrap_or(StrategyStatus::Failed),
                    s.and_then(|s| s.last_reason.clone()),
                    r["development"]["sharpe"].as_f64().unwrap_or(f64::NAN),
                )
            })
            .collect();
        let best = pick_best(&cands).map(|(i, label)| (devjobs[i].0, devjobs[i].1, label));
        let mut recent = div().flex().flex_col().gap_1();
        for a in self.snap.audit.iter().take(12) {
            recent = recent.child(
                div()
                    .flex()
                    .gap_3()
                    .text_xs()
                    .child(
                        div()
                            .w(px(44.))
                            .text_color(c_muted())
                            .child(format!("#{}", a.seq)),
                    )
                    .child(div().w(px(150.)).text_color(c_muted()).child(local(&a.ts)))
                    .child(
                        div()
                            .w(px(90.))
                            .text_color(c_accent())
                            .child(a.actor.clone()),
                    )
                    .child(
                        div()
                            .text_color(if a.success {
                                c_text()
                            } else {
                                rgb(0xf85149).into()
                            })
                            .child(a.action.clone()),
                    )
                    .child(
                        div().text_color(c_muted()).child(
                            a.target
                                .clone()
                                .map(|t| t.chars().take(8).collect::<String>())
                                .unwrap_or_default(),
                        ),
                    ),
            );
        }
        div().flex().flex_col().gap_4().p_4()
            .child(div().flex().gap_3()
                .child(card("策略总数 Strategies", self.snap.strategies.len().to_string(), format!("{} sample-labelled", self.snap.strategies.iter().filter(|s| s.sample).count()), c_text()))
                .child(card("已晋升 Promoted", count(StrategyStatus::Promoted).to_string(), format!("{} GatePassed, awaiting typed confirmation + vault", self.snap.strategies.iter().filter(|s| s.status == StrategyStatus::GatePassed).count()), rgb(0x3fb950).into()))
                .child(card("沙箱任务 Jobs", self.snap.jobs.len().to_string(), format!("{running} running · {failed_jobs} failed/blocked"), c_accent()))
                .child(card("全库试验 Library trials", self.snap.trials.to_string(), format!("N in the Deflated Sharpe Ratio · {} runs", self.snap.total_attempts), rgb(0xd29922).into())))
            .child(section("状态流转 State machine"))
            .child(div().p_3().rounded_lg().bg(c_panel()).border_1().border_color(c_border()).child(flow))
            .child(section("最佳开发期 Best development-window result (host-scored, after A-share costs; excludes Failed and lookahead/integrity rejections)"))
            .child(div().p_3().rounded_lg().bg(c_panel()).border_1().border_color(c_border()).text_sm().text_color(c_text()).child(match best {
                Some((j, r, label)) => {
                    let title = self.snap.strategies.iter().find(|s| s.id == j.strategy_id).map(|s| s.title.clone()).unwrap_or_default();
                    format!("[{label}] {title} · {} · net return {} · Sharpe {} · DSR {} (N={}) · MaxDD {} · data: {}", j.symbol, pct(&r["development"], "total_return"), num(&r["development"], "sharpe"),
                        r["dsr"]["dsr"].as_f64().map(|x| format!("{x:.3}")).unwrap_or_default(), r["dsr"]["trials"], pct(&r["development"], "max_drawdown"), j.data_source)
                }
                None => "暂无诚实的回测结果 no honest completed backtest (lookahead / integrity / failed runs are excluded)".into(),
            }))
            .child(section("最近审计 Recent audit"))
            .child(div().p_3().rounded_lg().bg(c_panel()).border_1().border_color(c_border()).child(recent))
    }

    fn library(&self, cx: &mut Context<Self>) -> Div {
        let mut list = div()
            .id("lib-list")
            .w(px(430.))
            .h_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(c_border())
            .flex()
            .flex_col();
        for s in &self.snap.strategies {
            let sel = self.selected.as_deref() == Some(&s.id);
            let id = s.id.clone();
            list =
                list.child(
                    div()
                        .id(SharedString::from(format!("row-{}", s.id)))
                        .p_3()
                        .border_b_1()
                        .border_color(c_border())
                        .cursor_pointer()
                        .when(sel, |d| d.bg(rgb(0x1d2733)))
                        .hover(|d| d.bg(rgb(0x1a222c)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.selected = Some(id.clone());
                            this.pending_promote = None;
                            cx.notify();
                        }))
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .items_center()
                                .child(chip(s.status.as_str(), status_color(s.status)))
                                .child(div().text_xs().text_color(c_muted()).child(format!(
                                    "{} · try {}",
                                    &s.id[..8],
                                    s.attempts
                                )))
                                .when(s.sample, |d| d.child(chip("SAMPLE", c_muted()))),
                        )
                        .child(
                            div()
                                .mt_1()
                                .text_sm()
                                .text_color(c_text())
                                .child(s.title.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().flex_1().text_xs().text_color(c_muted()).child(
                                    format!(
                                        "{} · {}",
                                        s.source_site,
                                        s.author.clone().unwrap_or_default()
                                    ),
                                ))
                                .child({
                                    let on = self.compare.contains(&s.id);
                                    let cid = s.id.clone();
                                    div()
                                        .id(SharedString::from(format!("cmp-{}", s.id)))
                                        .px_2()
                                        .rounded_md()
                                        .border_1()
                                        .text_xs()
                                        .cursor_pointer()
                                        .border_color(if on { c_accent() } else { c_border() })
                                        .text_color(if on { c_accent() } else { c_muted() })
                                        .child(if on { "✓ compare" } else { "+ compare" })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Some(p) =
                                                this.compare.iter().position(|x| x == &cid)
                                            {
                                                this.compare.remove(p);
                                            } else {
                                                this.compare.push(cid.clone());
                                            }
                                            cx.stop_propagation();
                                            cx.notify();
                                        }))
                                }),
                        ),
                );
        }
        div()
            .flex()
            .size_full()
            .min_w_0()
            .overflow_hidden()
            .child(list)
            .child(self.detail(cx))
    }

    fn detail(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let base = div()
            .id("detail")
            .flex_1()
            .min_w_0()
            .overflow_x_hidden()
            .h_full()
            .overflow_y_scroll()
            .p_4()
            .flex()
            .flex_col()
            .gap_2();
        let Some(s) = self
            .selected
            .as_ref()
            .and_then(|id| self.snap.strategies.iter().find(|s| &s.id == id))
        else {
            return base.child(div().text_color(c_muted()).child("select a strategy"));
        };
        let jobs: Vec<&JobRecord> = self
            .snap
            .jobs
            .iter()
            .filter(|j| j.strategy_id == s.id)
            .collect();
        let st = self
            .home
            .audit_config()
            .ok()
            .and_then(|c| Store::open_readonly(&self.home.db(), c).ok());
        let ev: Option<Evaluation> = st
            .as_ref()
            .and_then(|st| st.latest_evaluation(&s.id).ok().flatten());
        let timeline: Vec<AuditEntry> = st
            .as_ref()
            .and_then(|st| st.audit_entries(Some(&s.id), 200).ok())
            .unwrap_or_default();
        let id = s.id.clone();
        let url = s.source_url.clone();
        let raw = s.raw_path.clone();
        let mut d = base
            .child(
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(chip(s.status.as_str(), status_color(s.status)))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .text_color(c_text())
                            .child(s.title.clone()),
                    ),
            )
            .child(div().text_xs().text_color(c_muted()).child(format!(
                "id {} · ingested {} · scraped {} · attempts {}",
                s.id,
                s.ingested_at.format("%Y-%m-%d %H:%M"),
                s.scraped_at.clone().unwrap_or_default(),
                s.attempts
            )));
        if let Some(r) = &s.last_reason {
            let good = matches!(
                s.status,
                StrategyStatus::GatePassed | StrategyStatus::Promoted
            );
            d = d.child(
                div()
                    .p_2()
                    .rounded_md()
                    .bg(if good { rgb(0x16261b) } else { rgb(0x2a1d1d) })
                    .text_sm()
                    .text_color(if good { rgb(0x7ee2a8) } else { rgb(0xffb4a8) })
                    .child(format!(
                        "{} {r}",
                        if good {
                            "状态 note:"
                        } else {
                            "原因 reason:"
                        }
                    )),
            );
        }
        d = d.child(section("溯源 Provenance")).child(
            div()
                .flex()
                .gap_2()
                .items_center()
                .child(
                    Button::new("open-src")
                        .label("打开原帖 Open source")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(u) = &url {
                                cx.open_url(u);
                                this.status = format!("opened {u}");
                            }
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("open-raw")
                        .label("归档原文 Raw archive")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let _ = std::process::Command::new("xdg-open").arg(&raw).spawn();
                            this.status = format!("raw archive: {raw}");
                            cx.notify();
                        })),
                )
                .child(div().text_xs().text_color(c_muted()).child(format!(
                    "{} · {}",
                    s.source_site,
                    s.source_url.clone().unwrap_or_default()
                ))),
        );
        if let Some(e) = &s.extracted {
            d = d
                .child(section(&format!("抽取 Extraction ({})", e.extractor)))
                .child(
                    div()
                        .text_sm()
                        .text_color(c_text())
                        .child(e.summary.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(c_muted())
                        .child(format!("假设 assumptions: {}", e.assumptions.join(" | "))),
                )
                .child(div().text_xs().text_color(c_accent()).child(format!(
                    "参数 params: {}",
                    serde_json::Value::Object(e.parameters.clone())
                )));
            let mut risks = div().flex().flex_wrap().gap_1();
            for r in &e.risks {
                let col: Hsla = match r.severity {
                    cmsq_core::Severity::High => rgb(0xf85149).into(),
                    cmsq_core::Severity::Warn => rgb(0xd29922).into(),
                    _ => c_muted(),
                };
                risks = risks.child(chip(format!("{}: {}", r.code, r.message), col));
            }
            d = d.child(risks);
            if let Some(code) = &e.code {
                d = d.child(
                    div()
                        .p_2()
                        .rounded_md()
                        .bg(rgb(0x0b0f14))
                        .border_1()
                        .border_color(c_border())
                        .text_xs()
                        .font_family("monospace")
                        .text_color(rgb(0xc9d1d9))
                        .child(code.clone()),
                );
            }
        }
        // actions
        let busy = self.busy();
        let docker = self.docker() == Some(true);
        let has_code = s.extracted.as_ref().is_some_and(|e| e.code.is_some());
        let can_bt = !busy
            && has_code
            && matches!(
                s.status,
                StrategyStatus::Extracted
                    | StrategyStatus::Rejected
                    | StrategyStatus::Failed
                    | StrategyStatus::GatePassed
            );
        let can_promote = !busy
            && docker
            && s.status == StrategyStatus::GatePassed
            && ev.as_ref().is_some_and(|e| e.passed);
        let (id2, id2b) = (id.clone(), id.clone());
        let mut actions = div()
            .flex()
            .gap_2()
            .items_center()
            .mt_2()
            .child(
                Button::new("bt")
                    .primary()
                    .disabled(!can_bt || !docker)
                    .label(if s.attempts > 0 {
                        format!("重跑回测 Re-run on {} (Docker)", self.symbol())
                    } else {
                        format!("提交回测 Backtest on {} (Docker)", self.symbol())
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.act_backtest(id2.clone(), false);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("bt-dev")
                    .disabled(!can_bt)
                    .label("DEV run (bwrap, never promotable)")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.act_backtest(id2b.clone(), true);
                        cx.notify();
                    })),
            );
        let id3 = id.clone();
        actions = actions.child(
            Button::new("promote")
                .disabled(!can_promote)
                .label("晋升… Promote (vault)")
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.pending_promote = Some(id3.clone());
                    this.confirm
                        .update(cx, |st, cx| st.set_value("", window, cx));
                    cx.notify();
                })),
        );
        d = d.child(actions);
        if self.pending_promote.as_deref() == Some(&s.id) {
            let phrase = Orchestrator::confirmation_phrase(s);
            let typed = self.confirm.read(cx).value().to_string();
            let matches = typed.trim() == phrase;
            let id4 = id.clone();
            d = d.child(div().p_3().rounded_lg().border_1().border_color(rgb(0xd29922)).bg(rgb(0x2a2412)).flex().flex_col().gap_2()
                .child(div().text_sm().text_color(rgb(0xf2cc60)).child(format!("确认台 Confirmation desk — \"{}\" passed the gate on development data. Promoting runs the ONE-TIME vault holdout ({} bars never shown to the strategy); a vault failure rejects it permanently. Signals stay OBSERVE-ONLY (no broker). Audited as user:dashboard.", s.title, "last 20%")))
                .child(div().text_sm().text_color(c_text()).child(format!("Type  {phrase}  to enable the button:")))
                .child(div().flex().gap_2().items_center()
                    .child(div().w(px(260.)).child(Input::new(&self.confirm)))
                    .child(div().text_xs().text_color(if matches { rgb(0x3fb950) } else { rgb(0xf85149) }).child(if matches { "✓ matches" } else { "does not match yet" })))
                .child(div().flex().gap_2()
                    .child(Button::new("confirm").primary().disabled(!matches).label("确认晋升并运行 vault  Confirm & run vault").on_click(cx.listener(move |this, _, _, cx| {
                        let typed = this.confirm.read(cx).value().to_string();
                        this.act_promote(id4.clone(), typed);
                        cx.notify();
                    })))
                    .child(Button::new("cancel").label("取消 Cancel").on_click(cx.listener(|this, _, _, cx| { this.pending_promote = None; this.status = "promotion cancelled".into(); cx.notify(); })))));
        }
        // state-transition timeline (from the audit chain)
        d = d.child(section("状态时间线 State timeline (audit)"));
        let mut tl = div()
            .flex()
            .flex_col()
            .gap_0p5()
            .pl_2()
            .border_l_2()
            .border_color(c_border());
        for a in timeline.iter().rev().filter(|a| {
            a.action.starts_with("transition")
                || a.action.starts_with("vault")
                || a.action == "evaluate"
                || a.action.starts_with("promote")
                || a.action == "ingest"
                || a.action == "job_reaped"
        }) {
            let reason = a.details["reason"]
                .as_str()
                .map(|r| format!(" — {}", r.chars().take(140).collect::<String>()))
                .unwrap_or_default();
            let label = a.action.trim_start_matches("transition:").to_string();
            tl = tl.child(
                div()
                    .flex()
                    .gap_2()
                    .text_xs()
                    .child(
                        div()
                            .w(px(110.))
                            .flex_none()
                            .text_color(c_muted())
                            .child(local(&a.ts)),
                    )
                    .child(
                        div()
                            .w(px(90.))
                            .flex_none()
                            .text_color(c_accent())
                            .child(a.actor.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(if a.success {
                                c_text()
                            } else {
                                rgb(0xf85149).into()
                            })
                            .child(format!("{label}{reason}")),
                    ),
            );
        }
        d = d.child(tl);
        if let Some(ev) = &ev {
            d = d.child(section(&format!(
                "门禁 Gate: {} (inputs computed on the host; nothing read from the container)",
                if ev.passed {
                    "PASSED ✓"
                } else {
                    "FAILED ✗"
                }
            )));
            if let Some(cs) = ev.checks["checks"].as_array() {
                for c in cs {
                    let ok = c["ok"].as_bool().unwrap_or(false);
                    d = d.child(
                        div()
                            .text_xs()
                            .text_color(if ok { rgb(0x3fb950) } else { rgb(0xf85149) })
                            .child(format!(
                                "{} {} — {}",
                                if ok { "✓" } else { "✗" },
                                c["name"].as_str().unwrap_or(""),
                                c["detail"].as_str().unwrap_or("")
                            )),
                    );
                }
            }
        }
        // equity curve: vault run (full series) if any, else the latest dev run
        let vault = jobs
            .iter()
            .find(|j| j.kind == "vault" && j.status == "succeeded");
        let devj = jobs
            .iter()
            .find(|j| j.kind == "dev" && j.status == "succeeded");
        if let Some(j) = vault.or(devj) {
            let pts = read_equity(&j.job_dir);
            let split = if j.kind == "vault" {
                devj.and_then(|dj| dj.result.as_ref())
                    .and_then(|r| r["split"]["development"]["bars"].as_u64())
                    .map(|b| b as usize)
            } else {
                None
            };
            d = d
                .child(section(&format!(
                    "权益曲线 Equity curve — {} run, ¥1,000,000 start, next-open fills",
                    j.kind
                )))
                .child(
                    div()
                        .p_2()
                        .rounded_md()
                        .bg(c_panel())
                        .border_1()
                        .border_color(c_border())
                        .child(equity_chart(&pts, split, 120.)),
                );
        }
        d = d.child(section("回测与日志 Runs & logs"));
        for j in jobs {
            let r = j.result.clone().unwrap_or_default();
            let m = if j.kind == "vault" {
                r["vault"].clone()
            } else {
                r["development"].clone()
            };
            d = d.child(div().p_2().rounded_md().bg(c_panel()).border_1().border_color(c_border()).flex().flex_col().gap_1()
                .child(div().text_xs().text_color(c_text()).child(format!("{} job {} · {} · {} · {} · {} ms · {}", j.kind, &j.id[..8], j.backend, j.status, j.symbol, j.duration_ms.unwrap_or(0), j.data_source)))
                .child(div().text_xs().text_color(c_muted()).child(format!("runner (from backend): {}", j.runner_identity.clone().unwrap_or_else(|| "-".into()))))
                .when(j.status == "succeeded" && j.kind == "dev", |x| x
                    .child(div().text_xs().text_color(c_muted()).child(format!("dev window {}..{} ({} bars, {} warm-up) · vault {}..{} kept back",
                        r["split"]["development"]["start"].as_str().unwrap_or(""), r["split"]["development"]["end"].as_str().unwrap_or(""), r["split"]["development"]["bars"], r["split"]["development"]["warmup_bars"],
                        r["split"]["vault"]["start"].as_str().unwrap_or(""), r["split"]["vault"]["end"].as_str().unwrap_or(""))))
                    .child(div().text_xs().text_color(c_accent()).child(format!("net return {} · Sharpe {} · MaxDD {} · DSR {} (N={}) · folds +{}/{} · 2x costs {} · buy&hold {}",
                        pct(&m, "total_return"), num(&m, "sharpe"), pct(&m, "max_drawdown"), r["dsr"]["dsr"].as_f64().map(|x| format!("{x:.3}")).unwrap_or_default(), r["dsr"]["trials"],
                        r["walk_forward_folds"].as_array().map(|f| f.iter().filter(|x| x["total_return"].as_f64().unwrap_or(0.0) > 0.0).count()).unwrap_or(0), r["walk_forward_folds"].as_array().map(|f| f.len()).unwrap_or(0),
                        pct(&r["development_cost_2x"], "total_return"), pct(&r["buy_and_hold"], "total_return"))))
                    .child(div().text_xs().text_color(c_muted()).child(format!("{} · trades {} · blocked limit-up {} / limit-down {} / suspended {} · costs paid ¥{:.0} · probe: {} containers, {}",
                        r["execution"].as_str().unwrap_or(""), r["trades"], r["blocked"]["limit_up"], r["blocked"]["limit_down"], r["blocked"]["suspended"], r["costs_paid"].as_f64().unwrap_or(0.0),
                        r["lookahead_probe"]["containers"], if r["lookahead_probe"]["detected"].as_bool() == Some(false) { "clean" } else { "LOOK-AHEAD" })))
                    .when_some(trade_stats_line(&r), |x, l| x.child(div().text_xs().text_color(c_muted()).child(l))))
                .when(j.status == "succeeded" && j.kind == "vault", |x| x.child(div().text_xs().text_color(rgb(0xa371f7)).child(format!("vault {}..{} · net return {} · Sharpe {} · {}",
                    m["start"].as_str().unwrap_or(""), m["end"].as_str().unwrap_or(""), pct(&m, "total_return"), num(&m, "sharpe"), if r["passed"].as_bool() == Some(true) { "PASSED" } else { "FAILED" })))
                    .when_some(trade_stats_line(&r), |x, l| x.child(div().text_xs().text_color(c_muted()).child(l)))
                    .when_some(qifi_line(&r).filter(|_| r["passed"] == true), |x, l| x.child(div().text_xs().text_color(rgb(0x7ee2a8)).child(l))))
                .when(j.error.is_some(), |x| x.child(div().text_xs().font_family("monospace").text_color(rgb(0xffb4a8)).child(j.error.clone().unwrap_or_default())))
                .child(div().text_xs().text_color(c_muted()).child(format!("logs: {}/variants/NN/{{stdout,stderr}}.log", j.job_dir))));
        }
        d
    }

    fn compare_view(&self) -> Stateful<Div> {
        let mut d = div()
            .id("compare")
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .overflow_y_scroll()
            .size_full();
        if self.compare.is_empty() {
            return d.child(div().text_color(c_muted()).child("在策略库中点击 “+ compare” 选择 2–4 个策略进行对比 · pick strategies with “+ compare” in the library"));
        }
        let cols = [
            "STRATEGY", "STATUS", "SYMBOL", "NET RET", "SHARPE", "DSR (N)", "FOLDS+", "MAX DD",
            "2x COST", "VAULT",
        ];
        let w = [260., 90., 120., 80., 70., 90., 60., 80., 80., 90.];
        let mut hdr = div()
            .flex()
            .gap_2()
            .text_xs()
            .text_color(c_muted())
            .pb_1()
            .border_b_1()
            .border_color(c_border());
        for (c, w) in cols.iter().zip(w) {
            hdr = hdr.child(div().w(px(w)).flex_none().child(*c));
        }
        d = d.child(hdr);
        for id in &self.compare {
            let Some(s) = self.snap.strategies.iter().find(|s| &s.id == id) else {
                continue;
            };
            let dev = self
                .snap
                .jobs
                .iter()
                .find(|j| &j.strategy_id == id && j.kind == "dev" && j.status == "succeeded");
            let vault = self
                .snap
                .jobs
                .iter()
                .find(|j| &j.strategy_id == id && j.kind == "vault" && j.status == "succeeded");
            let r = dev.and_then(|j| j.result.clone()).unwrap_or_default();
            let m = &r["development"];
            let folds = r["walk_forward_folds"]
                .as_array()
                .map(|f| {
                    format!(
                        "{}/{}",
                        f.iter()
                            .filter(|x| x["total_return"].as_f64().unwrap_or(0.0) > 0.0)
                            .count(),
                        f.len()
                    )
                })
                .unwrap_or("-".into());
            let vret = vault
                .and_then(|j| j.result.as_ref())
                .map(|v| pct(&v["vault"], "total_return"))
                .unwrap_or("not run".into());
            let vals = [
                s.title.clone(),
                s.status.to_string(),
                dev.map(|j| j.symbol.clone()).unwrap_or("-".into()),
                pct(m, "total_return"),
                num(m, "sharpe"),
                r["dsr"]["dsr"]
                    .as_f64()
                    .map(|x| format!("{x:.3} ({})", r["dsr"]["trials"]))
                    .unwrap_or("-".into()),
                folds,
                pct(m, "max_drawdown"),
                pct(&r["development_cost_2x"], "total_return"),
                vret,
            ];
            let mut row = div().flex().gap_2().text_xs().py_1();
            for (i, (v, w)) in vals.iter().zip(w).enumerate() {
                row = row.child(
                    div()
                        .w(px(w))
                        .flex_none()
                        .overflow_hidden()
                        .text_color(if i == 1 {
                            status_color(s.status)
                        } else {
                            c_text()
                        })
                        .child(v.clone()),
                );
            }
            d = d.child(row);
            if let Some(j) = vault.or(dev) {
                d = d.child(
                    div()
                        .p_2()
                        .rounded_md()
                        .bg(c_panel())
                        .border_1()
                        .border_color(c_border())
                        .child(equity_chart(&read_equity(&j.job_dir), None, 60.)),
                );
            }
        }
        d
    }

    fn log_panel(&self) -> Div {
        let g = self.progress.lock().unwrap();
        let mut p = div()
            .h(px(132.))
            .flex_none()
            .border_t_1()
            .border_color(c_border())
            .bg(rgb(0x0b0f14))
            .p_2()
            .flex()
            .flex_col()
            .gap_0p5()
            .overflow_hidden()
            .child(
                div()
                    .text_xs()
                    .text_color(if g.running.is_some() {
                        rgb(0xd29922).into()
                    } else {
                        c_muted()
                    })
                    .child(match &g.running {
                        Some(l) => format!("● RUNNING (background thread, UI stays live): {l}"),
                        None => "job log · idle".into(),
                    }),
            );
        for l in g
            .log
            .iter()
            .rev()
            .take(7)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            p = p.child(
                div()
                    .text_xs()
                    .font_family("monospace")
                    .text_color(rgb(0xc9d1d9))
                    .child(l.clone()),
            );
        }
        p
    }

    fn jobs(&self) -> Stateful<Div> {
        let mut t = div()
            .id("jobs")
            .p_4()
            .flex()
            .flex_col()
            .gap_1()
            .overflow_y_scroll()
            .size_full()
            .child(
                div()
                    .flex()
                    .gap_2()
                    .text_xs()
                    .text_color(c_muted())
                    .pb_2()
                    .border_b_1()
                    .border_color(c_border())
                    .child(div().w(px(80.)).flex_none().child("JOB"))
                    .child(div().w(px(150.)).flex_none().child("BACKEND"))
                    .child(div().w(px(150.)).flex_none().child("STATUS"))
                    .child(div().w(px(120.)).flex_none().child("SYMBOL"))
                    .child(div().w(px(80.)).flex_none().child("MS"))
                    .child(div().w(px(110.)).flex_none().child("NET RET"))
                    .child(div().flex_1().child("STRATEGY / ERROR")),
            );
        for j in &self.snap.jobs {
            let title = self
                .snap
                .strategies
                .iter()
                .find(|s| s.id == j.strategy_id)
                .map(|s| s.title.clone())
                .unwrap_or_default();
            let ok = j.status == "succeeded";
            t = t.child(
                div()
                    .flex()
                    .gap_2()
                    .text_xs()
                    .py_1()
                    .border_b_1()
                    .border_color(c_border())
                    .child(
                        div()
                            .w(px(80.))
                            .flex_none()
                            .text_color(c_text())
                            .child(format!("{} {}", &j.id[..6], j.kind)),
                    )
                    .child(
                        div()
                            .w(px(150.))
                            .flex_none()
                            .text_color(if j.backend == "docker" {
                                c_accent()
                            } else {
                                rgb(0xd29922).into()
                            })
                            .child(j.backend.clone()),
                    )
                    .child(
                        div()
                            .w(px(150.))
                            .flex_none()
                            .text_color(if ok { rgb(0x3fb950) } else { rgb(0xf85149) })
                            .child(j.status.clone()),
                    )
                    .child(
                        div()
                            .w(px(120.))
                            .flex_none()
                            .text_color(c_text())
                            .child(j.symbol.clone()),
                    )
                    .child(
                        div()
                            .w(px(80.))
                            .flex_none()
                            .text_color(c_muted())
                            .child(j.duration_ms.unwrap_or(0).to_string()),
                    )
                    .child(
                        div().w(px(110.)).flex_none().text_color(c_text()).child(
                            j.result
                                .as_ref()
                                .map(|r| {
                                    pct(
                                        if j.kind == "vault" {
                                            &r["vault"]
                                        } else {
                                            &r["development"]
                                        },
                                        "total_return",
                                    )
                                })
                                .unwrap_or("-".into()),
                        ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_color(c_muted())
                            .child(format!(
                                "{title}{}",
                                j.error
                                    .as_ref()
                                    .map(|e| format!("  ✗ {e}"))
                                    .unwrap_or_default()
                            )),
                    ),
            );
        }
        t
    }

    fn data_view(&self) -> Stateful<Div> {
        let red: Hsla = rgb(0xf85149).into();
        let green: Hsla = rgb(0x3fb950).into();
        let amber: Hsla = rgb(0xd29922).into();
        let mut d = div().id("data").p_4().flex().flex_col().gap_2().overflow_y_scroll().size_full()
            .child(section("数据源健康 Source health (router priority order; persisted in data/_sources_health.json)"));
        if self.snap.health.is_empty() {
            d = d.child(
                div()
                    .text_xs()
                    .text_color(c_muted())
                    .child("no router fetches yet — `cmsq data fetch SH600519`"),
            );
        }
        for (name, h) in &self.snap.health {
            let cooling = h
                .cooldown_until
                .as_deref()
                .and_then(|u| chrono::DateTime::parse_from_rfc3339(u).ok())
                .is_some_and(|t| t > chrono::Utc::now());
            let (txt, col) = if cooling {
                ("COOLDOWN", red)
            } else if h.consecutive_fail > 0 {
                ("DEGRADED", amber)
            } else if h.ok > 0 {
                ("HEALTHY", green)
            } else {
                ("UNUSED", c_muted())
            };
            d = d.child(
                div()
                    .p_2()
                    .rounded_md()
                    .bg(c_panel())
                    .border_1()
                    .border_color(c_border())
                    .flex()
                    .gap_3()
                    .items_center()
                    .text_xs()
                    .child(
                        div()
                            .w(px(80.))
                            .text_color(c_text())
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(name.clone()),
                    )
                    .child(chip(txt, col))
                    .child(div().text_color(c_muted()).child(format!(
                        "ok {} · fail {} · last ok {} · {}",
                        h.ok,
                        h.fail,
                        h.last_ok.as_deref().map(local).unwrap_or("-".into()),
                        h.last_ms.map(|m| format!("{m} ms")).unwrap_or_default()
                    )))
                    .child(
                        div().flex_1().text_color(red).child(
                            h.last_error
                                .clone()
                                .unwrap_or_default()
                                .chars()
                                .take(120)
                                .collect::<String>(),
                        ),
                    ),
            );
        }
        d = d.child(section(
            "数据集 Datasets (verdicts from the HMAC-attested fetch record, not meta.json)",
        ));
        for (m, a) in self.snap.datasets.iter().zip(&self.snap.attest) {
            let mut row = div()
                .p_3()
                .rounded_md()
                .bg(c_panel())
                .border_1()
                .border_color(c_border())
                .flex()
                .flex_col()
                .gap_1()
                .text_xs()
                .child(
                    div()
                        .flex()
                        .gap_3()
                        .items_center()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(c_text())
                                .child(m.symbol.clone()),
                        )
                        .child(div().text_color(c_accent()).child(m.source.clone()))
                        .child(div().text_color(c_muted()).child(format!(
                            "{} bars · {} .. {} · fetched {}",
                            m.bars,
                            m.start,
                            m.end,
                            local(&m.fetched_at)
                        )))
                        .child(div().text_color(c_muted()).child(m.label.clone())),
                );
            match a {
                Ok(at) => {
                    let vok = at["validation_ok"].as_bool().unwrap_or(false);
                    let st =
                        at["validation"]["status"]
                            .as_str()
                            .unwrap_or(if m.source == "sample" {
                                "synthetic"
                            } else {
                                "none"
                            });
                    row = row
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .items_center()
                                .child(chip("attested ✓", green))
                                .child(chip(
                                    if at["adjusted"].as_bool().unwrap_or(false) {
                                        "adjusted"
                                    } else {
                                        "UNADJUSTED"
                                    },
                                    if at["adjusted"].as_bool().unwrap_or(false) {
                                        green
                                    } else {
                                        red
                                    },
                                ))
                                .child(chip(
                                    format!("cross-check: {st}"),
                                    if vok {
                                        if st == "ok_with_flags" {
                                            amber
                                        } else {
                                            green
                                        }
                                    } else {
                                        red
                                    },
                                ))
                                .child(div().text_color(c_muted()).font_family("monospace").child(
                                    format!(
                                            "snapshot {}",
                                            at["snapshot_sha256"]
                                                .as_str()
                                                .map(|x| &x[..16])
                                                .unwrap_or("-")
                                        ),
                                )),
                        )
                        .child(
                            div()
                                .text_color(if vok { c_muted() } else { red })
                                .child(at["validation_detail"].as_str().unwrap_or("").to_string()),
                        );
                    if let Some(sample) = at["validation"]["divergent_sample"]
                        .as_array()
                        .filter(|v| !v.is_empty())
                    {
                        let days: Vec<String> = sample
                            .iter()
                            .take(6)
                            .map(|x| {
                                format!(
                                    "{} {:.2} vs {:.2}",
                                    x["date"].as_str().unwrap_or(""),
                                    x["primary"].as_f64().unwrap_or(0.0),
                                    x["other"].as_f64().unwrap_or(0.0)
                                )
                            })
                            .collect();
                        row = row.child(
                            div()
                                .text_color(amber)
                                .child(format!("flagged days: {}", days.join(" · "))),
                        );
                    }
                }
                Err(e) => {
                    row = row.child(
                        div()
                            .flex()
                            .gap_2()
                            .child(chip("INTEGRITY REFUSED", red))
                            .child(div().text_color(red).child(e.clone())),
                    )
                }
            }
            d = d.child(row);
        }
        d
    }

    fn signals(&self) -> Div {
        let mut d = div().p_4().flex().flex_col().gap_2()
            .child(div().p_3().rounded_md().border_1().border_color(rgb(0xd29922)).text_sm().text_color(rgb(0xf2cc60))
                .child("仅观察 OBSERVE-ONLY：未连接任何券商/实盘接口；信号仅来自人工确认晋升的策略。No broker is connected."));
        if self.snap.signals.is_empty() {
            d = d.child(
                div()
                    .text_color(c_muted())
                    .child("no signals yet — promote a strategy that passed the gate"),
            );
        }
        for (s, title, st) in &self.snap.signals {
            d = d.child(
                div()
                    .p_3()
                    .rounded_md()
                    .bg(c_panel())
                    .border_1()
                    .border_color(c_border())
                    .flex()
                    .gap_4()
                    .text_sm()
                    .child(div().text_color(c_text()).child(s.date.clone()))
                    .child(div().text_color(c_accent()).child(s.symbol.clone()))
                    .child(
                        div()
                            .text_color(c_text())
                            .child(format!("target position {:.2}", s.target_position)),
                    )
                    .child(
                        div()
                            .text_color(c_muted())
                            .child(format!("{title} [{st}] · job {}", &s.job_id[..8])),
                    ),
            );
        }
        d
    }

    fn paper(&self) -> Stateful<Div> {
        let mut d = div().id("paper").p_4().flex().flex_col().gap_2().overflow_y_scroll()
            .child(div().p_3().rounded_md().border_1().border_color(rgb(0xd29922)).text_sm().text_color(rgb(0xf2cc60))
                .child("模拟撮合 SIMULATED：按交易所行情模拟成交，无券商接口、不会发出真实委托。Simulated fills against exchange quotes; no broker, no real orders."));
        if self.snap.paper.is_empty() {
            d =
                d.child(div().text_color(c_muted()).child(
                    "no paper sessions yet — `cmsq paper run --strategy <promoted id> ...`",
                ));
        }
        for p in &self.snap.paper {
            let a = &p.state["account"];
            let cash = a["cash"].as_f64().unwrap_or(0.0);
            let margin = a["margin"].as_f64().unwrap_or(0.0);
            let eq = p.curve.last().map(|x| x.2).unwrap_or(cash);
            let mut card = div()
                .p_3()
                .rounded_md()
                .bg(c_panel())
                .border_1()
                .border_color(c_border())
                .flex()
                .flex_col()
                .gap_1()
                .text_sm()
                .child(div().text_color(c_accent()).child(format!(
                    "{}  [{}]  strategy {}",
                    p.id,
                    p.status,
                    &p.strategy[..8.min(p.strategy.len())]
                )))
                .child(div().text_color(c_text()).child(format!(
                    "equity {eq:.2} · cash {cash:.2} · margin {margin:.2} · fees {:.2} · day {}",
                    a["fees"].as_f64().unwrap_or(0.0),
                    a["trading_day"].as_str().unwrap_or("-")
                )));
            if let Some(pos) = a["positions"].as_object() {
                for (sym, x) in pos {
                    card = card.child(div().text_color(c_text()).child(format!(
                        "  {sym}: long {} (today {}) @ {:.2} · short {} (today {}) @ {:.2}",
                        x["long_today"].as_i64().unwrap_or(0) + x["long_yd"].as_i64().unwrap_or(0),
                        x["long_today"],
                        x["long_cost"].as_f64().unwrap_or(0.0),
                        x["short_today"].as_i64().unwrap_or(0)
                            + x["short_yd"].as_i64().unwrap_or(0),
                        x["short_today"],
                        x["short_cost"].as_f64().unwrap_or(0.0)
                    )));
                }
            }
            if p.curve.len() > 1 {
                let pts: Vec<String> = p
                    .curve
                    .iter()
                    .map(|c| format!("{}:{:.0}", c.1.clone().unwrap_or_default(), c.2))
                    .collect();
                card = card.child(
                    div()
                        .text_xs()
                        .text_color(c_muted())
                        .child(format!("equity by settlement: {}", pts.join("  "))),
                );
            }
            for f in &p.fills {
                card = card.child(div().text_xs().text_color(c_muted()).child(format!(
                    "fill #{} {} {} {} {} @ {} fee {:.2} · {}",
                    f["order_id"],
                    f["symbol"].as_str().unwrap_or(""),
                    f["side"].as_str().unwrap_or(""),
                    f["offset"].as_str().unwrap_or(""),
                    f["qty"],
                    f["price"],
                    f["fee"].as_f64().unwrap_or(0.0),
                    f["ts"].as_str().unwrap_or("")
                )));
            }
            d = d.child(card);
        }
        d
    }

    fn research(&self, cx: &mut Context<Self>) -> Div {
        let mut list = div()
            .id("runs")
            .w(px(380.))
            .h_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(c_border())
            .flex()
            .flex_col();
        if self.snap.runs.is_empty() {
            list = list.child(
                div()
                    .p_3()
                    .text_xs()
                    .text_color(c_muted())
                    .child("no sweep/portfolio runs yet — `cmsq sweep` / `cmsq portfolio`"),
            );
        }
        let sel = self
            .run_sel
            .clone()
            .or_else(|| self.snap.runs.first().map(|r| r.id.clone()));
        for r in &self.snap.runs {
            let id = r.id.clone();
            let on = sel.as_deref() == Some(&r.id);
            let title = self
                .snap
                .strategies
                .iter()
                .find(|s| Some(&s.id) == r.strategy_id.as_ref())
                .map(|s| s.title.clone())
                .unwrap_or_default();
            let what = match r.kind.as_str() {
                "sweep" => format!(
                    "{} · {} points · grid {}",
                    r.spec["symbol"].as_str().unwrap_or(""),
                    r.spec["points"],
                    r.spec["grid"]
                ),
                _ => format!(
                    "{} · {}",
                    r.spec["symbols"]
                        .as_array()
                        .map(|v| v
                            .iter()
                            .filter_map(|x| x.as_str())
                            .collect::<Vec<_>>()
                            .join("+"))
                        .unwrap_or_default(),
                    r.spec["alloc"]
                ),
            };
            list = list.child(
                div()
                    .id(SharedString::from(format!("run-{}", r.id)))
                    .p_3()
                    .border_b_1()
                    .border_color(c_border())
                    .cursor_pointer()
                    .when(on, |d| d.bg(rgb(0x1d2733)))
                    .hover(|d| d.bg(rgb(0x1a222c)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.run_sel = Some(id.clone());
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .items_center()
                            .child(chip(
                                r.kind.clone(),
                                if r.kind == "sweep" {
                                    c_accent()
                                } else {
                                    rgb(0xa371f7).into()
                                },
                            ))
                            .child(chip(
                                r.status.clone(),
                                if r.status == "succeeded" {
                                    rgb(0x3fb950).into()
                                } else {
                                    rgb(0xf85149).into()
                                },
                            ))
                            .child(div().text_xs().text_color(c_muted()).child(format!(
                                "{} · {}",
                                &r.id[..8],
                                local(&r.created_at)
                            ))),
                    )
                    .child(div().mt_1().text_sm().text_color(c_text()).child(title))
                    .child(div().text_xs().text_color(c_muted()).child(what)),
            );
        }
        let detail = match self.snap.runs.iter().find(|r| Some(&r.id) == sel.as_ref()) {
            None => div(),
            Some(r) => self.run_detail(r),
        };
        div().size_full().flex().child(list).child(
            div()
                .id("run-detail")
                .flex_1()
                .h_full()
                .overflow_y_scroll()
                .p_4()
                .child(detail),
        )
    }

    fn run_detail(&self, r: &cmsq_core::store::ResearchRun) -> Div {
        let res = r.result.clone().unwrap_or_default();
        let mut d = div().flex().flex_col().gap_2();
        if r.kind == "sweep" {
            let best = &res["best"];
            d = d.child(section(&format!("参数扫描 Sweep {} · {}", &r.id[..8], r.spec["symbol"].as_str().unwrap_or(""))))
                .child(div().text_xs().text_color(c_muted()).child("每个扫描点都是一次独立试验，计入全库 DSR 试验账本；最优点按最终账本 N 重新计算 DSR。Every point is a trial in the library ledger; the best point is re-deflated against the final N."))
                .child(div().p_3().rounded_lg().bg(c_panel()).border_1().border_color(c_border()).text_sm().child(format!(
                    "best {}  params {}  →  {}   Sharpe {}  return {}   DSR(final ledger) {} over N={}",
                    best["job"].as_str().map(|j| &j[..8.min(j.len())]).unwrap_or("-"), best["params"], res["status"].as_str().unwrap_or("-"),
                    num(&best["development"], "sharpe"), pct(&best["development"], "total_return"),
                    best["dsr"]["dsr"].as_f64().map(|x| format!("{x:.3}")).unwrap_or("-".into()), best["dsr"]["trials"])));
            if let Some(rs) = res["reasons"].as_array() {
                for x in rs {
                    d = d.child(
                        div()
                            .text_xs()
                            .text_color(rgb(0xf0883e))
                            .child(format!("✗ {}", x.as_str().unwrap_or(""))),
                    );
                }
            }
            let mut t = div()
                .flex()
                .flex_col()
                .p_3()
                .rounded_lg()
                .bg(c_panel())
                .border_1()
                .border_color(c_border())
                .child(
                    div()
                        .flex()
                        .text_xs()
                        .text_color(c_muted())
                        .gap_2()
                        .child(div().w(px(40.)).child("#"))
                        .child(div().w(px(260.)).child("params"))
                        .child(div().w(px(90.)).child("status"))
                        .child(div().w(px(80.)).child("Sharpe"))
                        .child(div().w(px(100.)).child("return"))
                        .child(div().w(px(110.)).child("DSR@record"))
                        .child(div().child("note")),
                );
            let best_job = best["job"].as_str();
            for p in res["points"].as_array().cloned().unwrap_or_default() {
                let is_best = p["job"].as_str().is_some() && p["job"].as_str() == best_job;
                t = t.child(
                    div()
                        .flex()
                        .text_xs()
                        .gap_2()
                        .py_0p5()
                        .when(is_best, |x| x.bg(rgb(0x1d3324)))
                        .child(
                            div()
                                .w(px(40.))
                                .text_color(c_muted())
                                .child(p["point"].to_string()),
                        )
                        .child(
                            div()
                                .w(px(260.))
                                .font_family("monospace")
                                .child(p["params"].to_string()),
                        )
                        .child(
                            div()
                                .w(px(90.))
                                .text_color(if p["status"] == "succeeded" {
                                    c_text()
                                } else {
                                    rgb(0xf85149).into()
                                })
                                .child(p["status"].as_str().unwrap_or("").to_string()),
                        )
                        .child(div().w(px(80.)).child(num(&p, "sharpe")))
                        .child(div().w(px(100.)).child(pct(&p, "total_return")))
                        .child(
                            div().w(px(110.)).child(
                                p["dsr_at_record"]
                                    .as_f64()
                                    .map(|x| format!("{x:.3}"))
                                    .unwrap_or("-".into()),
                            ),
                        )
                        .child(div().text_color(rgb(0xf85149)).child(
                            if p["lookahead"].as_bool() == Some(true) {
                                "LOOK-AHEAD"
                            } else {
                                ""
                            },
                        ))
                        .when(is_best, |x| {
                            x.child(div().text_color(rgb(0x3fb950)).child("★ best"))
                        }),
                );
            }
            d = d.child(t);
        } else {
            let pm = &res["portfolio"];
            d = d.child(section(&format!("组合回测 Portfolio {} · {}", &r.id[..8], res["alloc"])))
                .child(div().text_xs().text_color(c_muted()).child(format!("一个账户共享现金，先卖后买；只用各标的共同的开发期日期 {} .. {}（{} bars），金库数据从不使用。组合不可晋升（P1 门禁仅供参考）。",
                    res["window"]["start"].as_str().unwrap_or(""), res["window"]["end"].as_str().unwrap_or(""), res["window"]["bars"])))
                .child(div().flex().gap_3()
                    .child(card("组合收益 Return", pct(pm, "total_return"), format!("Sharpe {} · MaxDD {}", num(pm, "sharpe"), pct(pm, "max_drawdown")), c_text()))
                    .child(card("2× 成本 Cost ×2", pct(&res["portfolio_cost_2x"], "total_return"), format!("Sharpe {}", num(&res["portfolio_cost_2x"], "sharpe")), rgb(0xd29922).into()))
                    .child(card("等权持有 EW buy&hold", pct(&res["equal_weight_buy_and_hold"], "total_return"), format!("Sharpe {}", num(&res["equal_weight_buy_and_hold"], "sharpe")), c_muted()))
                    .child(card("DSR", res["dsr"]["dsr"].as_f64().map(|x| format!("{x:.3}")).unwrap_or("-".into()), format!("N={} library trials · trades {}", res["dsr"]["trials"], res["trades"]),
                        if res["gate"]["passed"].as_bool() == Some(true) { rgb(0x3fb950).into() } else { rgb(0xf0883e).into() })));
            // metrics start after the warm-up bars (allocation look-back); chart the same window
            let warm = res["window"]["warmup_bars"].as_u64().unwrap_or(0) as usize;
            let pts: Vec<(String, f64)> = read_equity(&r_dir(&self.home, &r.id))
                .into_iter()
                .skip(warm.saturating_sub(1))
                .collect();
            d = d
                .child(section(
                    "组合净值 Portfolio equity (host-scored, A-share costs)",
                ))
                .child(
                    div()
                        .p_3()
                        .rounded_lg()
                        .bg(c_panel())
                        .border_1()
                        .border_color(c_border())
                        .child(equity_chart(&pts, None, 160.)),
                );
            let mut legs = div()
                .flex()
                .flex_col()
                .gap_1()
                .p_3()
                .rounded_lg()
                .bg(c_panel())
                .border_1()
                .border_color(c_border());
            for l in res["legs"].as_array().cloned().unwrap_or_default() {
                legs =
                    legs.child(
                        div()
                            .flex()
                            .gap_3()
                            .text_xs()
                            .child(
                                div()
                                    .w(px(130.))
                                    .text_color(c_accent())
                                    .child(l["symbol"].as_str().unwrap_or("").to_string()),
                            )
                            .child(
                                div()
                                    .w(px(160.))
                                    .child(format!("Sharpe {}", num(&l["development"], "sharpe"))),
                            )
                            .child(div().w(px(160.)).child(format!(
                                "return {}",
                                pct(&l["development"], "total_return")
                            )))
                            .child(div().text_color(c_muted()).child(format!(
                                    "job {}",
                                    l["job"]
                                        .as_str()
                                        .map(|j| &j[..8.min(j.len())])
                                        .unwrap_or("")
                                ))),
                    );
            }
            d = d
                .child(section(
                    "各腿 Legs (each leg is its own sandbox job with the full look-ahead probe)",
                ))
                .child(legs)
                .child(div().text_xs().text_color(c_muted()).child(format!(
                        "leg return correlation: {}",
                        res["leg_return_correlation"]
                            .as_array()
                            .map(|rows| rows
                                .iter()
                                .map(|r| r
                                    .as_array()
                                    .map(|v| v
                                        .iter()
                                        .map(|x| format!("{:+.2}", x.as_f64().unwrap_or(f64::NAN)))
                                        .collect::<Vec<_>>()
                                        .join(" "))
                                    .unwrap_or_default())
                                .collect::<Vec<_>>()
                                .join("  |  "))
                            .unwrap_or_default()
                    )));
            for x in res["gate"]["reasons"]
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                d = d.child(
                    div()
                        .text_xs()
                        .text_color(rgb(0xf0883e))
                        .child(format!("✗ {}", x.as_str().unwrap_or(""))),
                );
            }
        }
        d
    }

    fn futures_view(&self) -> Stateful<Div> {
        let mut d = div().id("fut").size_full().overflow_y_scroll().p_4().flex().flex_col().gap_2()
            .child(section("中国期货 China futures (P2) — 主力连续、合约乘数、保证金、最小变动价位、涨跌停、夜盘、逐合约手续费"));
        let futs: Vec<&market::DataMeta> = self
            .snap
            .datasets
            .iter()
            .filter(|m| m.symbol.starts_with("FUT_"))
            .collect();
        if futs.is_empty() {
            d = d.child(
                div()
                    .text_xs()
                    .text_color(c_muted())
                    .child("no futures dataset yet — `cmsq data fetch FUT_RB`"),
            );
        }
        for m in futs {
            let sp = &m.extra["spec"];
            let rolls = m.extra["rolls"].as_array().cloned().unwrap_or_default();
            let val = &m.extra["validation"];
            let mut box_ = div().p_3().rounded_lg().bg(c_panel()).border_1().border_color(c_border()).flex().flex_col().gap_1()
                .child(div().flex().gap_2().items_center()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(m.symbol.clone()))
                    .child(chip(format!("{} {}", sp["exchange"].as_str().unwrap_or(""), sp["product"].as_str().unwrap_or("")), c_accent()))
                    .child(chip(format!("validation: {}", val["status"].as_str().unwrap_or("none")), if val["status"] == "ok" { rgb(0x3fb950).into() } else { rgb(0xd29922).into() }))
                    .child(div().text_xs().text_color(c_muted()).child(format!("{} bars · {} · source {}", m.bars, m.label, m.source))))
                .child(div().text_xs().child(format!("multiplier {} · tick {} · margin {:.0}% · limit ±{:.0}% · fee {} · night {}",
                    sp["multiplier"], sp["tick"], sp["margin_rate"].as_f64().unwrap_or(0.0) * 100.0, sp["limit_pct"].as_f64().unwrap_or(0.0) * 100.0, sp["fee"],
                    sp["night"].as_array().map(|v| v.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("–")).unwrap_or("none".into()))))
                .child(div().text_xs().text_color(c_muted()).child(val["note"].as_str().unwrap_or("").to_string()))
                .child(div().text_xs().text_color(c_muted()).child(format!("main-contract rolls: {} (open-interest lead confirmed over 3 closes, effective next day, never rolls back; both legs pay fees + slippage)", rolls.len())));
            let mut rl = div().flex().flex_wrap().gap_2();
            for r in rolls.iter().rev().take(16) {
                rl = rl.child(chip(
                    format!(
                        "{} {}→{} ×{:.3}",
                        r["date"].as_str().unwrap_or(""),
                        r["from"].as_str().unwrap_or(""),
                        r["to"].as_str().unwrap_or(""),
                        r["factor"].as_f64().unwrap_or(1.0)
                    ),
                    c_muted(),
                ));
            }
            box_ = box_.child(rl);
            let jobs: Vec<&JobRecord> = self
                .snap
                .jobs
                .iter()
                .filter(|j| j.symbol == m.symbol && j.result.is_some())
                .take(8)
                .collect();
            for j in jobs {
                let r = j.result.as_ref().unwrap();
                let title = self
                    .snap
                    .strategies
                    .iter()
                    .find(|s| s.id == j.strategy_id)
                    .map(|s| s.title.clone())
                    .unwrap_or_default();
                box_ = box_.child(
                    div()
                        .flex()
                        .gap_3()
                        .text_xs()
                        .child(
                            div()
                                .w(px(70.))
                                .text_color(c_muted())
                                .child(j.id[..8].to_string()),
                        )
                        .child(div().w(px(300.)).child(title))
                        .child(
                            div()
                                .w(px(130.))
                                .child(format!("ret {}", pct(&r["development"], "total_return"))),
                        )
                        .child(
                            div()
                                .w(px(110.))
                                .child(format!("Sharpe {}", num(&r["development"], "sharpe"))),
                        )
                        .child(
                            div()
                                .w(px(120.))
                                .child(format!("MaxDD {}", pct(&r["development"], "max_drawdown"))),
                        )
                        .child(
                            div().text_color(c_muted()).child(
                                r["execution"]
                                    .as_str()
                                    .or(r["futures"]["execution"].as_str())
                                    .unwrap_or("")
                                    .chars()
                                    .take(90)
                                    .collect::<String>(),
                            ),
                        ),
                );
            }
            d = d.child(box_);
        }
        // P3 market data
        d = d.child(section(
            "CTP 行情 Market data (P3, observe-only — no trading API compiled in)",
        ));
        let md = latest_md_run(&self.home);
        let mut mdbox = div()
            .p_3()
            .rounded_lg()
            .bg(c_panel())
            .border_1()
            .border_color(c_border())
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_xs().child(format!(
                "ticks stored: {} (TimescaleDB hypertable `ticks` on PostgreSQL)",
                self.snap.ticks
            )));
        let sessions: Vec<&AuditEntry> = self
            .snap
            .audit
            .iter()
            .filter(|a| a.action == "md_session")
            .take(4)
            .collect();
        for a in sessions {
            mdbox = mdbox.child(div().text_xs().text_color(c_muted()).child(format!(
                "{} · feed {} · front {} · ticks {} · bars {} · rejected {}",
                local(&a.ts),
                a.details["feed"].as_str().unwrap_or(""),
                a.details["front"].as_str().unwrap_or(""),
                a.details["ticks"],
                a.details["bars"],
                a.details["rejected"]
            )));
        }
        if let Some((path, rows)) = md {
            mdbox = mdbox.child(
                div()
                    .text_xs()
                    .text_color(c_muted())
                    .mt_1()
                    .child(format!("latest 1-minute bars ({path})")),
            );
            for r in rows.iter().take(12) {
                mdbox = mdbox.child(div().text_xs().font_family("monospace").child(r.clone()));
            }
        }
        d.child(mdbox)
    }

    fn audit(&self) -> Stateful<Div> {
        let mut d = div()
            .id("audit")
            .p_4()
            .flex()
            .flex_col()
            .gap_1()
            .overflow_y_scroll()
            .size_full();
        for a in &self.snap.audit {
            d = d.child(
                div()
                    .flex()
                    .gap_3()
                    .text_xs()
                    .py_0p5()
                    .child(
                        div()
                            .w(px(44.))
                            .text_color(c_muted())
                            .child(format!("#{}", a.seq)),
                    )
                    .child(div().w(px(150.)).text_color(c_muted()).child(local(&a.ts)))
                    .child(
                        div()
                            .w(px(110.))
                            .text_color(c_accent())
                            .child(a.actor.clone()),
                    )
                    .child(
                        div()
                            .w(px(260.))
                            .text_color(if a.success {
                                c_text()
                            } else {
                                rgb(0xf85149).into()
                            })
                            .child(a.action.clone()),
                    )
                    .child(
                        div().w(px(70.)).text_color(c_muted()).child(
                            a.target
                                .clone()
                                .map(|t| t.chars().take(8).collect::<String>())
                                .unwrap_or_default(),
                        ),
                    )
                    .child(
                        div()
                            .text_color(rgb(0x5c6773))
                            .font_family("monospace")
                            .child(a.hash[..16].to_string()),
                    ),
            );
        }
        d
    }
}

impl Render for Dashboard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = match self.tab {
            Tab::Overview => div()
                .id("ov")
                .size_full()
                .overflow_y_scroll()
                .child(self.overview())
                .into_any_element(),
            Tab::Library => self.library(cx).into_any_element(),
            Tab::Compare => self.compare_view().into_any_element(),
            Tab::Data => self.data_view().into_any_element(),
            Tab::Jobs => self.jobs().into_any_element(),
            Tab::Signals => self.signals().into_any_element(),
            Tab::Paper => self.paper().into_any_element(),
            Tab::Research => self.research(cx).into_any_element(),
            Tab::Futures => self.futures_view().into_any_element(),
            Tab::Audit => self.audit().into_any_element(),
        };
        div().size_full().flex().bg(c_bg()).text_color(c_text()).font_family("Noto Sans CJK SC")
            .child(self.sidebar(cx))
            .child(div().flex_1().min_w_0().h_full().flex().flex_col().overflow_hidden()
                .child(self.toolbar(cx))
                .when(self.docker() == Some(false), |x| x.child(div().px_3().py_2().bg(rgb(0x5a1e1e)).border_b_1().border_color(rgb(0xf85149)).text_sm().text_color(rgb(0xffd7d0))
                    .child(format!("⚠ 未连接 Docker — NOT RUNNING ON DOCKER: {}. Only DEV (bwrap) runs are possible; their results can never pass the gate or be promoted. See README “Docker setup”.",
                        self.progress.lock().unwrap().docker_detail.chars().take(120).collect::<String>()))))
                .child(div().flex_1().min_h_0().child(body))
                .child(self.log_panel())
                .child(div().px_3().py_1().border_t_1().border_color(c_border()).bg(c_panel()).text_xs().text_color(c_muted()).child(self.status.clone())))
    }
}

fn main() {
    let home = Home::from_env();
    if let Err(e) = Orchestrator::open(home.clone()) {
        eprintln!("cannot open workspace {}: {e:#}", home.root.display());
        std::process::exit(1);
    }
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        let opts = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1440.), px(900.)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("CMspark Quant Dashboard".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        gpui_kit::open_window(opts, cx, |window, cx| {
            cx.new(|cx| Dashboard::new(home.clone(), window, cx))
        })
        .expect("open window");
    });
}

fn r_dir(home: &Home, id: &str) -> String {
    home.root.join("runs").join(id).display().to_string()
}

/// (path, csv lines) of the newest `runs/md_*/bars_1m.csv`
fn latest_md_run(home: &Home) -> Option<(String, Vec<String>)> {
    let mut v: Vec<std::path::PathBuf> = std::fs::read_dir(home.root.join("runs"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("md_"))
        })
        .collect();
    v.sort();
    let p = v.pop()?.join("bars_1m.csv");
    let text = std::fs::read_to_string(&p).ok()?;
    Some((
        p.display().to_string(),
        text.lines().map(String::from).collect(),
    ))
}

/// Reasons that mean a result is not an honest measurement and must never be shown as "best".
const DISHONEST: [&str; 6] = [
    "lookahead",
    "integrity",
    "static_risks",
    "pids_exhausted",
    "escape",
    "tamper",
];

/// Pick the Overview "best result": GatePassed/Promoted first, else the best honest
/// Evaluated/Rejected run (rejected only for performance). Failed and lookahead/integrity
/// rejections are never eligible. Returns (index, label).
fn pick_best(c: &[(StrategyStatus, Option<String>, f64)]) -> Option<(usize, &'static str)> {
    let honest = |i: &usize| {
        let (st, reason, sr) = &c[*i];
        sr.is_finite()
            && !matches!(
                st,
                StrategyStatus::Failed | StrategyStatus::Ingested | StrategyStatus::Extracted
            )
            && !reason
                .as_deref()
                .map(|r| DISHONEST.iter().any(|d| r.contains(d)))
                .unwrap_or(false)
    };
    let best_of = |f: &dyn Fn(StrategyStatus) -> bool| {
        (0..c.len())
            .filter(honest)
            .filter(|i| f(c[*i].0))
            .max_by(|a, b| c[*a].2.total_cmp(&c[*b].2))
    };
    best_of(&|s| matches!(s, StrategyStatus::GatePassed | StrategyStatus::Promoted))
        .map(|i| (i, "门禁通过 gate-passed"))
        .or_else(|| {
            best_of(&|_| true).map(|i| (i, "最佳诚实结果，未通过门禁 best honest, NOT gate-passed"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn c(st: StrategyStatus, r: Option<&str>, sr: f64) -> (StrategyStatus, Option<String>, f64) {
        (st, r.map(String::from), sr)
    }
    #[::core::prelude::v1::test]
    fn best_excludes_cheaters_and_prefers_gate_passed() {
        let v = vec![
            c(
                StrategyStatus::Rejected,
                Some("no_lookahead: positions changed; static_risks: lookahead"),
                6.6,
            ),
            c(StrategyStatus::Failed, Some("runner exit 2"), 3.0),
            c(
                StrategyStatus::Rejected,
                Some("data integrity refused"),
                5.0,
            ),
            c(
                StrategyStatus::Rejected,
                Some("deflated_sharpe: DSR 0.8"),
                1.25,
            ),
            c(StrategyStatus::Rejected, Some("sharpe: 0.3"), 0.3),
        ];
        assert_eq!(
            pick_best(&v),
            Some((3, "最佳诚实结果，未通过门禁 best honest, NOT gate-passed"))
        );
        let mut w = v.clone();
        w.push(c(StrategyStatus::GatePassed, None, 0.9));
        assert_eq!(pick_best(&w).unwrap().0, 5);
        assert_eq!(pick_best(&v[..3]), None);
    }

    #[::core::prelude::v1::test]
    fn trade_stats_and_qifi_lines() {
        let r: serde_json::Value = serde_json::from_str(r#"{"trade_stats": {"round_trips": 4, "win_rate": 0.5, "pl_ratio": 1.25, "profit_factor": 1.1, "avg_bars_held": 3.5, "max_bars_held": 7},
            "close_today": {"lots": 2.0, "fees": 12.0},
            "account": {"trading_day": "2024-01-05", "balance": 1050000, "start_equity": 1000000, "margin": 200000, "final_units": -3.0, "final_price": 4000.0, "exchange": "SHFE", "fills": [1, 2]}}"#).unwrap();
        let empty = serde_json::Value::Object(Default::default());
        let l = trade_stats_line(&r).unwrap();
        assert!(
            l.contains("not gated")
                && l.contains("win rate 50%")
                && l.contains("P/L ratio 1.25")
                && l.contains("close-today 2 lots"),
            "{l}"
        );
        let q = qifi_line(&r).unwrap();
        assert!(
            q.contains("short 3 lots") && q.contains("observe-only"),
            "{q}"
        );
        assert!(trade_stats_line(&empty).is_none() && qifi_line(&empty).is_none());
    }
}
