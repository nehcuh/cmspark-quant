//! `cmsq` — headless CLI for CMspark Quant (P0).
use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use cmsq_core::extract;
use cmsq_core::market::SampleSource;
use cmsq_core::orchestrator::IngestOutcome;
use cmsq_core::sandbox::{docker::DockerSandboxExecutor, process::ProcessSandboxExecutor, SandboxExecutor};
use cmsq_core::{Home, Orchestrator, StrategyStatus};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "cmsq", version, about = "CMspark Quant: scrape drop → extract → sandbox backtest → gate → manual promote. No live trading.")]
struct Cli {
    /// Workspace directory (or env CMSQ_HOME, default ./var)
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    /// Machine-readable JSON output
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum Backend {
    Docker,
    Process,
    Auto,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create workspace, write pinned runner/vendor assets, cache sample data
    Init,
    /// Ingest CMspark drop files (default: <home>/drop)
    Ingest { path: Option<PathBuf> },
    /// Extract all Ingested strategies (or one id)
    Extract { id: Option<String> },
    /// Run sandbox backtest for an id (or all Extracted with --all)
    Backtest {
        id: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long, default_value = "SAMPLE_TREND")]
        symbol: String,
        #[arg(long, value_enum, default_value = "auto")]
        backend: Backend,
        /// JSON object overriding extracted params, e.g. '{"fast":5}'
        #[arg(long)]
        params: Option<String>,
    },
    /// Manually promote (requires gate pass + typed confirmation = first 8 chars of id)
    Promote { id: String, #[arg(long)] confirm: String },
    /// Manually reject
    Reject { id: String, #[arg(long)] reason: String },
    /// List strategies
    List { #[arg(long)] status: Option<String> },
    /// Show one strategy with provenance, evaluation, jobs and audit trail
    Show { id: String },
    /// List sandbox jobs
    Jobs,
    /// Observe-only signals of promoted strategies
    Signals,
    /// Market data (host-side; sandbox stays offline)
    Data { #[command(subcommand)] cmd: DataCmd },
    /// Audit log
    Audit { #[command(subcommand)] cmd: AuditCmd },
}

#[derive(Subcommand)]
enum DataCmd {
    /// Fetch daily bars: SAMPLE_* from bundled fixtures, SH/SZ codes via rstdx (TDX)
    Fetch {
        symbol: String,
        #[arg(long, default_value_t = 1500)]
        count: usize,
        /// TDX server host:port (default: rstdx auto-discovery)
        #[arg(long)]
        server: Option<String>,
    },
    /// List cached datasets
    List,
}

#[derive(Subcommand)]
enum AuditCmd {
    Verify,
    Tail { #[arg(long, default_value_t = 20)] n: i64 },
}

async fn executor(b: Backend) -> Result<Box<dyn SandboxExecutor>> {
    match b {
        Backend::Docker => Ok(Box::new(DockerSandboxExecutor::connect().await?)),
        Backend::Process => Ok(Box::new(ProcessSandboxExecutor)),
        Backend::Auto => match DockerSandboxExecutor::connect().await {
            Ok(d) => Ok(Box::new(d)),
            Err(e) => {
                eprintln!("WARNING: docker unavailable ({e}); falling back to DEV process backend (weaker isolation)");
                Ok(Box::new(ProcessSandboxExecutor))
            }
        },
    }
}

fn pct(v: &serde_json::Value, k: &str) -> String {
    v[k].as_f64().map(|x| format!("{:+.2}%", x * 100.0)).unwrap_or("-".into())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let home = cli.home.clone().map(Home::new).unwrap_or_else(Home::from_env);
    let o = Orchestrator::open(home)?;
    let out = |v: serde_json::Value, human: String| {
        if cli.json { println!("{}", serde_json::to_string_pretty(&v).unwrap()) } else { println!("{human}") }
    };
    match cli.cmd {
        Cmd::Init => {
            o.ensure_samples().await?;
            out(serde_json::json!({"home": o.home.abs()}), format!("workspace ready at {}", o.home.abs().display()));
        }
        Cmd::Ingest { path } => {
            let p = path.unwrap_or_else(|| o.home.drop_dir());
            let res = o.ingest_path(&p)?;
            let mut lines = vec![];
            for (f, r) in &res {
                let name = f.file_name().unwrap_or_default().to_string_lossy();
                lines.push(match r {
                    IngestOutcome::New(id) => format!("NEW       {}  {name}", &id[..8]),
                    IngestOutcome::Duplicate(id) => format!("DUPLICATE {}  {name}", &id[..8]),
                    IngestOutcome::Invalid(e) => format!("INVALID   {name}: {e}"),
                });
            }
            out(serde_json::to_value(res.iter().map(|(f, r)| (f, r)).collect::<Vec<_>>())?, lines.join("\n"));
        }
        Cmd::Extract { id } => {
            let ex = extract::from_env()?;
            let ids: Vec<String> = match id {
                Some(i) => vec![i],
                None => o.store.list_strategies(Some(StrategyStatus::Ingested))?.into_iter().map(|s| s.id).collect(),
            };
            let mut lines = vec![];
            let mut js = vec![];
            for i in ids {
                let s = o.extract(&i, ex.as_ref()).await?;
                let e = s.extracted.as_ref();
                lines.push(format!("{:<9} {}  {}  params={}  risks=[{}]{}", s.status.as_str(), &s.id[..8], s.title,
                    e.map(|e| serde_json::Value::Object(e.parameters.clone()).to_string()).unwrap_or_default(),
                    e.map(|e| e.risks.iter().map(|r| r.code.as_str()).collect::<Vec<_>>().join(",")).unwrap_or_default(),
                    s.last_reason.as_ref().map(|r| format!("\n          reason: {r}")).unwrap_or_default()));
                js.push(serde_json::to_value(&s)?);
            }
            out(serde_json::Value::Array(js), lines.join("\n"));
        }
        Cmd::Backtest { id, all, symbol, backend, params } => {
            let exec = executor(backend).await?;
            let ov = params.map(|p| serde_json::from_str(&p)).transpose().context("--params must be JSON")?;
            let ids: Vec<String> = if all {
                o.store.list_strategies(Some(StrategyStatus::Extracted))?.into_iter().map(|s| s.id).collect()
            } else {
                vec![id.context("give an id or --all")?]
            };
            let mut lines = vec![format!("backend: {} (hardened={})  symbol: {symbol}", exec.name(), exec.hardened())];
            let mut js = vec![];
            for i in ids {
                let r = o.backtest(&i, &symbol, exec.as_ref(), ov.clone()).await?;
                let title = o.store.get_strategy(&r.strategy_id)?.title;
                let m = r.oos.clone().unwrap_or_default();
                lines.push(format!("{:<9} {}  job={} [{}]  OOS ret {} sharpe {} maxDD {} turnover {}  — {}", r.status.as_str(), &r.strategy_id[..8], &r.job_id[..8], r.job_status,
                    pct(&m, "total_return"), m["sharpe"].as_f64().map(|x| format!("{x:.2}")).unwrap_or("-".into()), pct(&m, "max_drawdown"),
                    m["turnover_annual"].as_f64().map(|x| format!("{x:.1}x")).unwrap_or("-".into()), title));
                for why in &r.reasons {
                    lines.push(format!("          ✗ {why}"));
                }
                if r.status == StrategyStatus::Evaluated {
                    lines.push(format!("          ✓ gate passed — awaiting manual confirmation: cmsq promote {} --confirm {}", &r.strategy_id[..8], &r.strategy_id[..8]));
                }
                js.push(serde_json::to_value(&r)?);
            }
            out(serde_json::Value::Array(js), lines.join("\n"));
        }
        Cmd::Promote { id, confirm } => {
            let s = o.promote(&id, "user:cli", &confirm)?;
            out(serde_json::to_value(&s)?, format!("PROMOTED {}  {}  (signals are observe-only; no broker connected)", &s.id[..8], s.title));
        }
        Cmd::Reject { id, reason } => {
            let s = o.reject(&id, "user:cli", &reason)?;
            out(serde_json::to_value(&s)?, format!("REJECTED {}  {}", &s.id[..8], s.title));
        }
        Cmd::List { status } => {
            let st = status.map(|s| StrategyStatus::parse(&s).context("unknown status")).transpose()?;
            let v = o.store.list_strategies(st)?;
            let mut lines = vec![format!("{:<8}  {:<11} {:>3}  {:<14} {}", "ID", "STATUS", "TRY", "SOURCE", "TITLE")];
            for s in &v {
                lines.push(format!("{}  {:<11} {:>3}  {:<14} {}{}", &s.id[..8], s.status.as_str(), s.attempts, s.source_site, s.title, if s.sample { "  [sample]" } else { "" }));
            }
            out(serde_json::to_value(&v)?, lines.join("\n"));
        }
        Cmd::Show { id } => {
            let s = o.store.get_strategy(&id)?;
            let ev = o.store.latest_evaluation(&s.id)?;
            let jobs = o.store.list_jobs(Some(&s.id))?;
            let audit = o.store.audit_entries(Some(&s.id), 50)?;
            let v = serde_json::json!({"strategy": s, "evaluation": ev, "jobs": jobs, "audit": audit});
            let mut h = format!("{}  {}\nstatus: {}  attempts: {}\nsource: {} {}\nraw: {}\n", s.id, s.title, s.status, s.attempts, s.source_site, s.source_url.clone().unwrap_or_default(), s.raw_path);
            if let Some(r) = &s.last_reason { h += &format!("reason: {r}\n"); }
            if let Some(e) = &s.extracted { h += &format!("summary: {}\nrisks: {}\n", e.summary, e.risks.iter().map(|r| r.message.clone()).collect::<Vec<_>>().join(" | ")); }
            for j in &jobs { h += &format!("job {} {} {} {}ms  logs: {}/stderr.log\n", &j.id[..8], j.backend, j.status, j.duration_ms.unwrap_or(0), j.job_dir); }
            h += "audit:\n";
            for a in audit.iter().rev() { h += &format!("  #{:<4} {} {:<12} {}\n", a.seq, &a.ts[..19], a.actor, a.action); }
            out(v, h);
        }
        Cmd::Jobs => {
            let v = o.store.list_jobs(None)?;
            let mut lines = vec![];
            for j in &v {
                lines.push(format!("{}  {:<20} {:<18} {:<12} {:>6}ms  strategy {}", &j.id[..8], j.backend, j.status, j.symbol, j.duration_ms.unwrap_or(0), &j.strategy_id[..8]));
            }
            out(serde_json::to_value(&v)?, lines.join("\n"));
        }
        Cmd::Signals => {
            let v = o.store.list_signals()?;
            let lines: Vec<String> = v.iter().map(|(s, t, _)| format!("{}  {}  target={:.2}  {}  [OBSERVE ONLY]", s.date, s.symbol, s.target_position, t)).collect();
            out(serde_json::to_value(v.iter().map(|x| &x.0).collect::<Vec<_>>())?, if lines.is_empty() { "no signals (only promoted strategies emit observe-only signals)".into() } else { lines.join("\n") });
        }
        Cmd::Data { cmd: DataCmd::Fetch { symbol, count, server } } => {
            let meta = if symbol.to_uppercase().starts_with("SAMPLE") {
                o.fetch_data(&SampleSource, &symbol.to_uppercase(), count).await?
            } else {
                #[cfg(feature = "tdx")]
                {
                    o.fetch_data(&cmsq_core::market::RstdxSource { server }, &symbol.to_uppercase(), count).await?
                }
                #[cfg(not(feature = "tdx"))]
                {
                    let _ = server;
                    anyhow::bail!("built without `tdx` feature")
                }
            };
            out(serde_json::to_value(&meta)?, format!("cached {} bars {} .. {} for {} from {} [{}]", meta.bars, meta.start, meta.end, meta.symbol, meta.source, meta.label));
        }
        Cmd::Data { cmd: DataCmd::List } => {
            let mut lines = vec![];
            for e in std::fs::read_dir(o.home.data_dir())? {
                let p = e?.path();
                if p.to_string_lossy().ends_with(".meta.json") {
                    let m: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p)?)?;
                    lines.push(format!("{:<14} {:>5} bars  {} .. {}  {}  [{}]", m["symbol"].as_str().unwrap_or(""), m["bars"], m["start"].as_str().unwrap_or(""), m["end"].as_str().unwrap_or(""), m["source"].as_str().unwrap_or(""), m["label"].as_str().unwrap_or("")));
                }
            }
            lines.sort();
            out(serde_json::json!(lines), lines.join("\n"));
        }
        Cmd::Audit { cmd: AuditCmd::Verify } => {
            let r = o.store.verify_audit()?;
            let h = if r.ok { format!("audit chain OK: {} entries, head {}", r.entries, &r.head_hash[..16]) } else { format!("AUDIT CHAIN BROKEN at seq {:?}: {}", r.first_bad_seq, r.problem.clone().unwrap_or_default()) };
            let ok = r.ok;
            out(serde_json::to_value(&r)?, h);
            if !ok { std::process::exit(1); }
        }
        Cmd::Audit { cmd: AuditCmd::Tail { n } } => {
            let v = o.store.audit_entries(None, n)?;
            let lines: Vec<String> = v.iter().rev().map(|a| format!("#{:<4} {} {:<14} {:<34} {} {}", a.seq, &a.ts[..19], a.actor, a.action, a.target.as_deref().map(|t| &t[..t.len().min(8)]).unwrap_or("-"), if a.success { "" } else { "FAIL" })).collect();
            out(serde_json::to_value(&v)?, lines.join("\n"));
        }
    }
    Ok(())
}
