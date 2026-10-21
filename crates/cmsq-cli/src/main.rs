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
    /// hardened; the only backend whose results can pass the gate
    Docker,
    /// DEV ONLY bubblewrap backend; results can never be promoted
    Process,
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
        #[arg(long, value_enum, default_value = "docker")]
        backend: Backend,
        /// JSON object overriding extracted params, e.g. '{"fast":5}'
        #[arg(long)]
        params: Option<String>,
        /// comma list of symbols (overrides --symbol): every id × every symbol, run concurrently
        #[arg(long)]
        symbols: Option<String>,
        /// concurrent strategies (containers are additionally bounded by CMSQ_CONTAINER_SLOTS)
        #[arg(long, default_value_t = 2)]
        workers: usize,
    },
    /// P1: parameter grid sweep; every point is a probed sandbox run AND a trial in the DSR ledger
    Sweep {
        id: String,
        #[arg(long)]
        symbol: String,
        /// JSON object name -> array, e.g. '{"fast":[5,10,20],"slow":[30,60]}'
        #[arg(long)]
        grid: String,
        #[arg(long, default_value_t = 3)]
        workers: usize,
        #[arg(long, value_enum, default_value = "docker")]
        backend: Backend,
    },
    /// P1: multi-symbol portfolio backtest (one account, shared cash) on common development dates
    Portfolio {
        id: String,
        /// comma list, e.g. SH600519,SZ000858,SH600036
        #[arg(long)]
        symbols: String,
        /// equal | inverse_vol[:LOOKBACK] | fixed:W1,W2,...
        #[arg(long, default_value = "equal")]
        alloc: String,
        #[arg(long)]
        params: Option<String>,
        #[arg(long, default_value_t = 3)]
        workers: usize,
        #[arg(long, value_enum, default_value = "docker")]
        backend: Backend,
    },
    /// Research runs (sweeps, portfolios)
    Runs { #[arg(long)] kind: Option<String> },
    /// Database (PostgreSQL primary / SQLite offline)
    Db { #[command(subcommand)] cmd: DbCmd },
    /// P3: CTP market data (observe-only; no trading API is compiled in)
    Md { #[command(subcommand)] cmd: MdCmd },
    /// Manually promote a GatePassed strategy: type `PROMOTE <id8>` (prompted if --confirm
    /// is omitted), then the one-time vault holdout runs in Docker; only a vault pass promotes.
    Promote { id: String, #[arg(long)] confirm: Option<String> },
    /// Manually reject
    Reject { id: String, #[arg(long)] reason: String },
    /// List strategies
    List { #[arg(long)] status: Option<String> },
    /// Show one strategy with provenance, evaluation, jobs and audit trail
    Show { id: String },
    /// List sandbox jobs
    Jobs {
        #[command(subcommand)]
        cmd: Option<JobsCmd>,
    },
    /// Library-wide trial ledger used by the Deflated Sharpe Ratio
    Trials,
    /// Optional QIFI-style JSON export of a Promoted strategy's observe-only paper account + signals
    ExportQifi { id: String, #[arg(long)] out: Option<std::path::PathBuf> },
    /// Observe-only signals of promoted strategies
    Signals,
    /// Native host-side bar-by-bar replay of a reference strategy (not gated; for reference/cross-checks)
    Replay {
        symbol: String,
        #[arg(long, default_value = "ma_crossover")]
        strategy: String,
        #[arg(long, default_value = "{}")]
        params: String,
    },
    /// Market data (host-side; sandbox stays offline)
    Data { #[command(subcommand)] cmd: DataCmd },
    /// Audit log
    Audit { #[command(subcommand)] cmd: AuditCmd },
}

#[derive(Subcommand)]
enum DataCmd {
    /// Fetch daily bars: SAMPLE_* from bundled fixtures; SH/SZ/BJ codes through the
    /// multi-source router (default priority tushare,akshare,baostock,tdx), cross-validated
    Fetch {
        /// one symbol or a comma list (fetched concurrently; per-source rate limits still apply)
        symbol: String,
        #[arg(long, default_value_t = 3)]
        concurrency: usize,
        #[arg(long, default_value_t = 1500)]
        count: usize,
        /// comma list in priority order (env CMSQ_DATA_SOURCES)
        #[arg(long)]
        sources: Option<String>,
        /// skip the cross-source check (the dataset then fails data_cross_validated)
        #[arg(long)]
        no_validate: bool,
    },
    /// Fetch trade calendars from tushare into data/_calendar_<EX>.txt (snapshot, sha256 audited)
    Calendar {
        #[arg(long, default_value = "2005-01-01")]
        start: String,
        /// SSE (default), one futures exchange (SHFE DCE CZCE CFFEX INE GFEX), or `futures` for all six
        #[arg(long, default_value = "SSE")]
        exchange: String,
    },
    /// Per-source health / cooldown / rate-limit state
    Sources,
    /// Import a CSV (date,open,high,low,close,volume[,adj_factor]) as SYMBOL
    Import {
        symbol: String,
        file: PathBuf,
        /// how the prices are adjusted: adj_factor (column present) | qfq | hfq | none
        #[arg(long, default_value = "none")]
        adjustment: String,
    },
    /// List cached datasets
    List,
}

#[derive(Subcommand)]
enum DbCmd {
    /// Apply migrations with the owner URL (CMSQ_DATABASE_MIGRATE_URL / <secrets>/db.env)
    Migrate,
    /// Backend, schema, row counts
    Status,
}

#[derive(Subcommand)]
enum MdCmd {
    /// Subscribe to instruments on a CTP market-data front, store ticks, build 1-minute bars
    Run {
        /// comma list of instruments, e.g. rb2601,IF2512
        #[arg(long)]
        instruments: String,
        /// front address (default: OpenCTP 7x24 tcp://trading.openctp.cn:30011)
        #[arg(long)]
        front: Option<String>,
        /// stop after N seconds
        #[arg(long, default_value_t = 60)]
        seconds: u64,
        /// use the built-in replay feed instead of a live front (offline / CI)
        #[arg(long)]
        mock: bool,
    },
    /// Probe whether the CTP front is reachable from this machine
    Probe { #[arg(long)] front: Option<String> },
}

#[derive(Subcommand)]
enum JobsCmd {
    /// Crash recovery: mark jobs of dead processes abandoned, un-stick Backtesting strategies
    Reap,
}

#[derive(Subcommand)]
enum AuditCmd {
    Verify,
    Tail { #[arg(long, default_value_t = 20)] n: i64 },
}

async fn executor(b: Backend) -> Result<Box<dyn SandboxExecutor>> {
    match b {
        Backend::Docker => DockerSandboxExecutor::connect().await.map(|d| Box::new(d) as Box<dyn SandboxExecutor>)
            .map_err(|e| anyhow::anyhow!("docker backend unavailable: {e} — see README \"Docker setup\" (or pass --backend process for a DEV-only run)")),
        Backend::Process => {
            eprintln!("\n!!! WARNING: DEV process backend (bubblewrap). Weaker isolation than Docker; results are");
            eprintln!("!!! labelled and can NEVER pass the gate or be promoted. Use --backend docker for real runs.\n");
            Ok(Box::new(ProcessSandboxExecutor))
        }
    }
}

fn pct(v: &serde_json::Value, k: &str) -> String {
    v[k].as_f64().map(|x| format!("{:+.2}%", x * 100.0)).unwrap_or("-".into())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let home = cli.home.clone().map(Home::new).unwrap_or_else(Home::from_env);
    let mut o = Orchestrator::open(home)?;
    if !cli.json {
        o.progress = Some(std::sync::Arc::new(|m: &str| eprintln!("  · {m}")));
    }
    let o = o;
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
        Cmd::Backtest { id, all, symbol, backend, params, symbols, workers } => {
            let exec = executor(backend).await?;
            let ov: Option<serde_json::Value> = params.map(|p| serde_json::from_str(&p)).transpose().context("--params must be JSON")?;
            let ids: Vec<String> = if all {
                o.store.list_strategies(Some(StrategyStatus::Extracted))?.into_iter().map(|s| s.id).collect()
            } else {
                vec![id.context("give an id or --all")?]
            };
            let syms: Vec<String> = symbols.map(|s| s.split(',').map(|x| x.trim().to_uppercase()).filter(|x| !x.is_empty()).collect()).unwrap_or_else(|| vec![symbol.clone()]);
            let mut lines = vec![format!("backend: {} (hardened={})  symbols: {}  workers: {workers}  container slots: {}", exec.name(), exec.hardened(), syms.join(","), o.slots.available_permits())];
            let mut js = vec![];
            let results: Vec<(String, String, Result<cmsq_core::orchestrator::BacktestSummary>)> = if syms.len() == 1 && ids.len() == 1 && ov.is_some() {
                vec![(ids[0].clone(), syms[0].clone(), o.backtest(&ids[0], &syms[0], exec.as_ref(), ov.clone()).await)]
            } else if ov.is_some() {
                anyhow::bail!("--params applies to a single id and symbol");
            } else {
                let pairs: Vec<(String, String)> = ids.iter().flat_map(|i| syms.iter().map(move |s| (i.clone(), s.clone()))).collect();
                o.backtest_batch(&pairs, exec.as_ref(), workers).await
            };
            let mut failed = 0;
            for (i, sym, r) in results {
                let r = match r {
                    Ok(r) => r,
                    Err(e) => {
                        failed += 1;
                        lines.push(format!("ERROR      {}  {sym}: {e:#}", &i[..8.min(i.len())]));
                        continue;
                    }
                };
                let title = o.store.get_strategy(&r.strategy_id)?.title;
                let m = r.oos.clone().unwrap_or_default();
                let job = o.store.list_jobs(Some(&r.strategy_id))?.into_iter().find(|j| j.id == r.job_id);
                let dsr = job.as_ref().and_then(|j| j.result.as_ref()).map(|v| v["dsr"].clone()).unwrap_or_default();
                lines.push(format!("{:<10} {}  {sym:<10} job={} [{}]  dev ret {} sharpe {} maxDD {} DSR {} (N={})  — {}", r.status.as_str(), &r.strategy_id[..8], &r.job_id[..8], r.job_status,
                    pct(&m, "total_return"), m["sharpe"].as_f64().map(|x| format!("{x:.2}")).unwrap_or("-".into()), pct(&m, "max_drawdown"),
                    dsr["dsr"].as_f64().map(|x| format!("{x:.3}")).unwrap_or("-".into()), dsr["trials"], title));
                for why in &r.reasons {
                    lines.push(format!("           ✗ {why}"));
                }
                if r.status == StrategyStatus::GatePassed {
                    lines.push(format!("           ✓ gate passed on development data — to run the one-time vault holdout and promote: cmsq promote {}  (you will be asked to type the confirmation)", &r.strategy_id[..8]));
                }
                js.push(serde_json::to_value(&r)?);
            }
            out(serde_json::Value::Array(js), lines.join("\n"));
            if failed > 0 && syms.len() == 1 && ids.len() == 1 {
                std::process::exit(1);
            }
        }
        Cmd::Sweep { id, symbol, grid, workers, backend } => {
            let exec = executor(backend).await?;
            let g: serde_json::Value = serde_json::from_str(&grid).context("--grid must be a JSON object")?;
            let g = g.as_object().context("--grid must be a JSON object")?;
            let r = o.sweep(&id, &symbol.to_uppercase(), g, exec.as_ref(), workers).await?;
            let mut lines = vec![format!("sweep {} on {}: {} points, workers {workers}", &r.run_id[..8], symbol.to_uppercase(), r.points.len())];
            for p in &r.points {
                lines.push(format!("  #{:<3} {:<34} {:<15} sharpe {:>6}  ret {:>9}  DSR@record {}{}", p["point"], p["params"].to_string(), p["status"].as_str().unwrap_or(""),
                    p["sharpe"].as_f64().map(|x| format!("{x:.2}")).unwrap_or("-".into()), pct(p, "total_return"),
                    p["dsr_at_record"].as_f64().map(|x| format!("{x:.3}")).unwrap_or("-".into()), if p["lookahead"].as_bool() == Some(true) { "  [LOOK-AHEAD]" } else { "" }));
            }
            lines.push(format!("best: {}  params {}  → {}  DSR(final ledger) {} N={}", r.best_job.as_deref().map(|j| &j[..8]).unwrap_or("-"),
                r.best_params.clone().unwrap_or_default(), r.status, r.dsr.as_ref().map(|d| format!("{:.3}", d.dsr)).unwrap_or("-".into()), r.dsr.as_ref().map(|d| d.trials).unwrap_or(0)));
            for why in &r.reasons {
                lines.push(format!("  ✗ {why}"));
            }
            out(serde_json::to_value(&r)?, lines.join("\n"));
        }
        Cmd::Portfolio { id, symbols, alloc, params, workers, backend } => {
            let exec = executor(backend).await?;
            let syms: Vec<String> = symbols.split(',').map(|x| x.trim().to_uppercase()).filter(|x| !x.is_empty()).collect();
            let alloc = parse_alloc(&alloc)?;
            let ov = params.map(|p| serde_json::from_str(&p)).transpose().context("--params must be JSON")?;
            let r = o.portfolio(&id, &syms, alloc, ov, exec.as_ref(), workers).await?;
            let p = &r.result["portfolio"];
            let mut lines = vec![format!("portfolio {} [{}] {}", &r.run_id[..8], syms.join("+"), r.result["alloc"])];
            if p.is_null() {
                lines.extend(r.reasons.iter().map(|x| format!("  ✗ {x}")));
            } else {
                lines.push(format!("  window {} .. {} ({} bars, development only)", r.result["window"]["start"].as_str().unwrap_or(""), r.result["window"]["end"].as_str().unwrap_or(""), r.result["window"]["bars"]));
                lines.push(format!("  return {}  sharpe {:.2}  maxDD {}  exposure {}  trades {}  DSR {:.3} (N={})", pct(p, "total_return"), p["sharpe"].as_f64().unwrap_or(0.0), pct(p, "max_drawdown"),
                    pct(p, "exposure"), r.result["trades"], r.result["dsr"]["dsr"].as_f64().unwrap_or(0.0), r.result["dsr"]["trials"]));
                for l in r.result["legs"].as_array().into_iter().flatten() {
                    lines.push(format!("    leg {:<10} sharpe {:.2}  ret {}", l["symbol"].as_str().unwrap_or(""), l["development"]["sharpe"].as_f64().unwrap_or(0.0), pct(&l["development"], "total_return")));
                }
                lines.push(format!("  gate (informational): {}", if r.passed { "PASS".to_string() } else { format!("FAIL — {}", r.reasons.join("; ")) }));
            }
            out(serde_json::to_value(&r)?, lines.join("\n"));
        }
        Cmd::Runs { kind } => {
            let v = o.store.list_runs(kind.as_deref())?;
            let lines: Vec<String> = v.iter().map(|r| format!("{}  {:<9} {:<9} {}  {}", &r.id[..8], r.kind, r.status, &r.created_at[..19], r.spec)).collect();
            out(serde_json::to_value(&v)?, lines.join("\n"));
        }
        Cmd::Db { cmd: DbCmd::Migrate } => {
            let c = o.home.db_config();
            let mu = c.migrate_url.clone().context("no CMSQ_DATABASE_MIGRATE_URL (run scripts/db_up.sh)")?;
            cmsq_core::store::Store::migrate_pg(&mu, c.schema.as_deref())?;
            out(serde_json::json!({"migrated": true, "schema": c.schema}), format!("migrations applied (schema {})", c.schema.unwrap_or_default()));
        }
        Cmd::Db { cmd: DbCmd::Status } => {
            let c = o.home.db_config();
            let v = serde_json::json!({"backend": o.store.backend(), "schema": c.schema, "url": c.url.as_deref().map(cmsq_core::store::redact_url),
                "strategies": o.store.list_strategies(None)?.len(), "trials": o.store.trial_count()?, "audit": o.store.audit_entries(None, 1)?.first().map(|a| a.seq),
                "runs": o.store.list_runs(None)?.len(), "ticks": o.store.tick_count(None)?});
            out(v.clone(), format!("backend {}  schema {}  url {}\n  strategies {}  trials {}  audit head {}  research runs {}  ticks {}", v["backend"], v["schema"], v["url"], v["strategies"], v["trials"], v["audit"], v["runs"], v["ticks"]));
        }
        Cmd::Md { cmd } => md_cmd(&o, cmd, &out).await?,
        Cmd::Promote { id, confirm } => {
            let s0 = o.get(&id)?;
            let confirm = match confirm {
                Some(c) => c,
                None => {
                    use std::io::{BufRead, IsTerminal, Write};
                    anyhow::ensure!(std::io::stdin().is_terminal(), "no --confirm given and stdin is not a terminal");
                    eprint!("Promote \"{}\"? This runs the ONE-TIME vault holdout.\nType  {}  to confirm: ", s0.title, Orchestrator::confirmation_phrase(&s0));
                    std::io::stderr().flush()?;
                    let mut l = String::new();
                    std::io::stdin().lock().read_line(&mut l)?;
                    l.trim().to_string()
                }
            };
            let exec = executor(Backend::Docker).await?;
            let s = o.promote(&id, "user:cli", &confirm, exec.as_ref()).await?;
            let msg = if s.status == StrategyStatus::Promoted {
                format!("PROMOTED {}  {}  (vault holdout passed; signals are observe-only, no broker connected)", &s.id[..8], s.title)
            } else {
                format!("{} {}  {}  — {}", s.status.as_str().to_uppercase(), &s.id[..8], s.title, s.last_reason.clone().unwrap_or_default())
            };
            out(serde_json::to_value(&s)?, msg);
            if s.status != StrategyStatus::Promoted {
                std::process::exit(3);
            }
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
            let s = o.get(&id)?;
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
        Cmd::Jobs { cmd: Some(JobsCmd::Reap) } => {
            let v = o.reap_stale_jobs()?;
            out(serde_json::json!(v), format!("reaped {} stuck job(s)/strategy(ies)", v.len()));
        }
        Cmd::Jobs { cmd: None } => {
            let v = o.store.list_jobs(None)?;
            let mut lines = vec![];
            for j in &v {
                lines.push(format!("{}  {:<5} {:<20} {:<15} {:<12} {:>6}ms  strategy {}  runner {}", &j.id[..8], j.kind, j.backend, j.status, j.symbol, j.duration_ms.unwrap_or(0), &j.strategy_id[..8],
                    j.runner_identity.as_deref().map(|r| r.chars().take(19).collect::<String>() + "…" + r.rsplit('|').next().unwrap_or("")).unwrap_or_default()));
            }
            out(serde_json::to_value(&v)?, lines.join("\n"));
        }
        Cmd::ExportQifi { id, out: path } => {
            let (p, doc) = o.export_qifi(&id, path.as_deref())?;
            let a = &doc["accounts"];
            out(serde_json::json!({"path": p, "qifi": doc}), format!("QIFI (observe-only paper account) → {}\n  balance ¥{:.0} · available ¥{:.0} · margin ¥{:.0} · {} position(s) · {} trade(s) · {} signal(s)",
                p.display(), a["balance"].as_f64().unwrap_or(0.0), a["available"].as_f64().unwrap_or(0.0), a["margin"].as_f64().unwrap_or(0.0),
                doc["positions"].as_object().map(|x| x.len()).unwrap_or(0), doc["trades"].as_object().map(|x| x.len()).unwrap_or(0), doc["cmsq"]["signals"].as_array().map(|x| x.len()).unwrap_or(0)));
        }
        Cmd::Trials => {
            let v = o.store.trials()?;
            let mut lines = vec![format!("library-wide trials: {}  (distinct ideas: {})", v.len(), v.iter().map(|t| &t.idea_key).collect::<std::collections::BTreeSet<_>>().len())];
            for t in &v {
                lines.push(format!("{}  idea {}  params {}  {:<12} SR/day {:+.4}", &t.strategy_id[..8], &t.idea_key[..10], t.params_hash, t.symbol, t.sharpe_daily));
            }
            out(serde_json::json!(v.iter().map(|t| serde_json::json!({"strategy": t.strategy_id, "idea_key": t.idea_key, "params_hash": t.params_hash, "symbol": t.symbol, "sharpe_daily": t.sharpe_daily})).collect::<Vec<_>>()), lines.join("\n"));
        }
        Cmd::Signals => {
            let v = o.store.list_signals()?;
            let lines: Vec<String> = v.iter().map(|(s, t, _)| format!("{}  {}  target={:.2}  {}  [OBSERVE ONLY]", s.date, s.symbol, s.target_position, t)).collect();
            out(serde_json::to_value(v.iter().map(|x| &x.0).collect::<Vec<_>>())?, if lines.is_empty() { "no signals (only promoted strategies emit observe-only signals)".into() } else { lines.join("\n") });
        }
        Cmd::Data { cmd: DataCmd::Fetch { symbol, concurrency, count, sources, no_validate } } => {
            use futures_util::stream::{self, StreamExt};
            let syms: Vec<String> = symbol.split(',').map(|x| x.trim().to_uppercase()).filter(|x| !x.is_empty()).collect();
            let (router, notes) = o.data_router(sources.as_deref(), !no_validate);
            for n in notes {
                eprintln!("  · {n}");
            }
            let (o_ref, r_ref) = (&o, &router);
            // one router shared by all tasks: tushare's token bucket (400/min) and akshare's
            // serial ≥1 s spacing are per source, so concurrency cannot exceed them
            let res: Vec<(String, Result<cmsq_core::market::DataMeta>)> = stream::iter(syms.clone())
                .map(|sym| async move {
                    let r = if sym.starts_with("SAMPLE") { o_ref.fetch_data(&SampleSource, &sym, count).await } else {
                        if r_ref.sources.is_empty() { Err(anyhow::anyhow!("no data source available")) } else { o_ref.fetch_routed(r_ref, &sym, count).await }
                    };
                    (sym, r)
                })
                .buffer_unordered(concurrency.max(1))
                .collect().await;
            let mut lines = vec![];
            let mut js = vec![];
            let mut fail = false;
            for (sym, r) in res {
                match r {
                    Ok(meta) => {
                        let v = &meta.extra["validation"];
                        let val = if v.is_null() { String::new() } else { format!("\n  cross-validation: {} vs {} — {}", v["status"].as_str().unwrap_or("?"), v["against"].as_str().unwrap_or("-"), v["note"].as_str().unwrap_or("")) };
                        lines.push(format!("cached {} bars {} .. {} for {} from {} [{}]\n  adjustment: {}\n  snapshot sha256 {}{val}", meta.bars, meta.start, meta.end, meta.symbol, meta.source, meta.label,
                            meta.adjustment, meta.extra["snapshot_sha256"].as_str().unwrap_or("-")));
                        js.push(serde_json::to_value(&meta)?);
                    }
                    Err(e) => {
                        fail = true;
                        lines.push(format!("FAILED {sym}: {e:#}"));
                    }
                }
            }
            out(if js.len() == 1 { js[0].clone() } else { serde_json::Value::Array(js) }, lines.join("\n"));
            if fail {
                std::process::exit(1);
            }
        }
        Cmd::Data { cmd: DataCmd::Calendar { start, exchange } } => {
            let t = cmsq_core::data::tushare::Tushare::from_secrets(&o.home.secrets)?;
            // through the end of next year: exchanges publish the holiday schedule in advance
            let end = format!("{}-12-31", chrono_today()[..4].parse::<i32>().unwrap_or(2026) + 1);
            let ex = exchange.to_uppercase();
            let list: Vec<String> = if ex == "FUTURES" { cmsq_core::calendars::FUTURES_EXCHANGES.iter().map(|x| x.to_string()).collect() } else { vec![ex] };
            std::fs::create_dir_all(o.home.data_dir())?;
            let (mut js, mut lines, mut fail) = (vec![], vec![], false);
            for ex in list {
                match t.trade_cal_for(&ex, &start, &end).await {
                    Ok(days) if days.iter().any(|d| d.1) => {
                        let cal = cmsq_core::replay::Calendar::from_days(days);
                        let p = cmsq_core::calendars::ExchangeCalendar::path(&o.home.data_dir(), &ex);
                        cal.save_as(&p, &ex)?;
                        let sha = cmsq_core::calendars::sha256_hex(&std::fs::read(&p)?);
                        o.store.audit("data", "trade_calendar_fetch", None, serde_json::json!({"source": "tushare trade_cal", "exchange": ex, "first": cal.first, "last": cal.last, "open_days": cal.open.len(), "snapshot_sha256": sha}), true)?;
                        lines.push(format!("{ex} calendar {}..{}: {} open days → {} (sha256 {})", cal.first, cal.last, cal.open.len(), p.display(), &sha[..12]));
                        js.push(serde_json::json!({"exchange": ex, "path": p, "open_days": cal.open.len(), "sha256": sha}));
                    }
                    Ok(_) => { fail = true; lines.push(format!("{ex}: tushare returned no open days; snapshot left unchanged")); }
                    Err(e) => { fail = true; lines.push(format!("{ex}: FAILED {e:#}; snapshot left unchanged")); }
                }
            }
            out(serde_json::Value::Array(js), lines.join("\n"));
            if fail {
                std::process::exit(1);
            }
        }
        Cmd::Data { cmd: DataCmd::Sources } => {
            let (router, notes) = o.data_router(None, true);
            let h = router.load_health();
            let mut lines: Vec<String> = router.sources.iter().map(|s| {
                let x = h.get(s.name()).cloned().unwrap_or_default();
                format!("{:<9} ok {:>3}  fail {:>3}  consecutive_fail {}  cooldown {}  last_error {}", s.name(), x.ok, x.fail, x.consecutive_fail,
                    x.cooldown_until.unwrap_or("-".into()), x.last_error.unwrap_or("-".into()))
            }).collect();
            lines.extend(notes);
            out(serde_json::to_value(&h)?, lines.join("\n"));
        }
        Cmd::Replay { symbol, strategy, params } => {
            use cmsq_core::{market, replay, scorer};
            let sym = symbol.to_uppercase();
            let bars = market::read_bars(&o.home.data_dir(), &sym)?;
            let view = cmsq_core::probe::container_view(&bars);
            let mut s = replay::by_name(&strategy, &serde_json::from_str(&params)?)?;
            let pos = replay::BarPlayer::new(&view).run(s.as_mut());
            let sim = scorer::simulate(&sym, &bars, &pos, &scorer::CostModel::default());
            let m = scorer::metrics(&bars, &sim, 0, bars.len());
            let cal_note = match replay::Calendar::load(&o.home.data_dir().join("_calendar_SSE.txt")) {
                Ok(c) => { let (miss, extra) = c.check(&bars); format!("calendar check: {} open days missing, {} bars on closed days", miss.len(), extra.len()) }
                Err(_) => "calendar check skipped (run `cmsq data calendar`)".into(),
            };
            out(serde_json::json!({"strategy": s.name(), "metrics": m, "trades": sim.trades, "note": "native host replay over the FULL series; not gated, not a promotion path"}),
                format!("{} on {sym} ({} .. {}, {} bars) — NATIVE REPLAY, full series, NOT gated\n  net return {:+.2}%  sharpe {:.2}  maxDD {:.1}%  exposure {:.0}%  trades {}\n  {cal_note}",
                    s.name(), m.start, m.end, m.bars, m.total_return * 100.0, m.sharpe, m.max_drawdown * 100.0, m.exposure * 100.0, sim.trades));
        }
        Cmd::Data { cmd: DataCmd::Import { symbol, file, adjustment } } => {
            let meta = o.fetch_data(&cmsq_core::market::CsvSource { path: file, adjustment }, &symbol.to_uppercase(), usize::MAX).await?;
            out(serde_json::to_value(&meta)?, format!("imported {} bars {} .. {} as {} (adjustment: {})", meta.bars, meta.start, meta.end, meta.symbol, meta.adjustment));
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
            let h = if r.ok { format!("audit chain OK: {} entries, HMAC-keyed, head {} anchored at seq {:?} in {}", r.entries, &r.head_hash[..16], r.anchored_seq, r.anchor.clone().unwrap_or_default()) } else { format!("AUDIT CHAIN BROKEN at seq {:?}: {}", r.first_bad_seq, r.problem.clone().unwrap_or_default()) };
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

fn parse_alloc(s: &str) -> Result<cmsq_core::scorer::Alloc> {
    use cmsq_core::scorer::Alloc;
    let (k, v) = s.split_once(':').unwrap_or((s, ""));
    Ok(match k {
        "equal" => Alloc::Equal,
        "inverse_vol" | "invvol" => Alloc::InverseVol { lookback: if v.is_empty() { 60 } else { v.parse().context("lookback")? } },
        "fixed" => Alloc::Fixed { weights: v.split(',').map(|x| x.trim().parse::<f64>()).collect::<std::result::Result<_, _>>().context("fixed:W1,W2,...")? },
        _ => anyhow::bail!("--alloc must be equal | inverse_vol[:N] | fixed:W1,W2,..."),
    })
}

async fn md_cmd(o: &Orchestrator, cmd: MdCmd, out: &dyn Fn(serde_json::Value, String)) -> Result<()> {
    use cmsq_core::ctp::{self, MdConfig};
    match cmd {
        MdCmd::Probe { front } => {
            let f = front.unwrap_or_else(|| ctp::OPENCTP_7X24_MD.into());
            let r = ctp::probe_front(&f, std::time::Duration::from_secs(6)).await;
            out(serde_json::json!({"front": f, "reachable": r.is_ok(), "error": r.as_ref().err().map(|e| e.to_string())}),
                match r { Ok(ms) => format!("{f}: TCP reachable in {ms} ms"), Err(e) => format!("{f}: NOT reachable from this machine: {e:#}") });
        }
        MdCmd::Run { instruments, front, seconds, mock } => {
            let cfg = MdConfig { front: front.unwrap_or_else(|| ctp::OPENCTP_7X24_MD.into()), instruments: instruments.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
                broker_id: "9999".into(), duration: std::time::Duration::from_secs(seconds) };
            let feed: Box<dyn ctp::MdFeed> = if mock { Box::new(ctp::ReplayFeed::demo(&cfg.instruments)) } else { ctp::live_feed()? };
            let rep = ctp::run_md(o, feed, &cfg).await?;
            out(serde_json::to_value(&rep)?, format!("md [{}] {}: {} ticks stored, {} one-minute bars, {} rejected; instruments {:?}", rep.feed, cfg.front, rep.ticks, rep.bars, rep.rejected, cfg.instruments));
        }
    }
    Ok(())
}

fn chrono_today() -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 + 8 * 3600;
    let days = t.div_euclid(86_400);
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}
