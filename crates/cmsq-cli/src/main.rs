//! `cmsq` — headless CLI for CMspark Quant (P0).
use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use cmsq_core::extract;
use cmsq_core::market::SampleSource;
use cmsq_core::orchestrator::IngestOutcome;
use cmsq_core::sandbox::{
    docker::DockerSandboxExecutor, process::ProcessSandboxExecutor, SandboxExecutor,
};
use cmsq_core::{Home, Orchestrator, StrategyStatus};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "cmsq",
    version,
    about = "CMspark Quant: scrape drop → extract → sandbox backtest → gate → manual promote. No live trading."
)]
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
    /// Scrape community sources (<home>/sources.json, or built-in GitHub sources) ->
    /// ingest -> extract -> sandboxed backtest; `--every 6h` repeats on a schedule
    Scrape {
        /// repeat interval, e.g. 30m, 6h, 1d (default: run once)
        #[arg(long)]
        every: Option<String>,
        /// dataset the new strategies are backtested on
        #[arg(long, default_value = "FUT_RB")]
        symbol: String,
        /// max sandbox backtests per cycle (0 = scrape/extract only)
        #[arg(long, default_value_t = 3)]
        backtests: usize,
        #[arg(long, value_enum, default_value = "docker")]
        backend: Backend,
        /// stop after N cycles (with --every)
        #[arg(long)]
        cycles: Option<usize>,
    },
    /// Source quality scores (what each source's items achieved downstream)
    SourceScores,
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
    Runs {
        #[arg(long)]
        kind: Option<String>,
    },
    /// Database (PostgreSQL primary / SQLite offline)
    Db {
        #[command(subcommand)]
        cmd: DbCmd,
    },
    /// P3: CTP market data (observe-only; no trading API is compiled in)
    Md {
        #[command(subcommand)]
        cmd: MdCmd,
    },
    /// Paper trading: simulated fills against exchange quotes for a PROMOTED strategy (no broker, no real orders)
    Paper {
        #[command(subcommand)]
        cmd: PaperCmd,
    },
    /// Manually promote a GatePassed strategy: type `PROMOTE <id8>` (prompted if --confirm
    /// is omitted), then the one-time vault holdout runs in Docker; only a vault pass promotes.
    Promote {
        id: String,
        #[arg(long)]
        confirm: Option<String>,
    },
    /// Manually reject
    Reject {
        id: String,
        #[arg(long)]
        reason: String,
    },
    /// List strategies
    List {
        #[arg(long)]
        status: Option<String>,
    },
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
    ExportQifi {
        id: String,
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
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
    Data {
        #[command(subcommand)]
        cmd: DataCmd,
    },
    /// Audit signer: holds the audit key in a separate process (see docs/AUDIT_KEY.md)
    Signer {
        #[command(subcommand)]
        cmd: SignerCmd,
    },
    /// Audit log
    Audit {
        #[command(subcommand)]
        cmd: AuditCmd,
    },
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
    /// Exchange-published daily price limits (tushare stk_limit / ft_limit) into
    /// data/<SYMBOL>.limits.csv; paper trading uses them when quotes carry no limits
    Limits {
        /// SH600519, or a futures product FUT_RB together with --contract
        symbol: String,
        /// futures contract (CTP instrument id, e.g. rb2601); limits are stored under it
        #[arg(long)]
        contract: Option<String>,
        #[arg(long, default_value = "2015-01-01")]
        start: String,
        #[arg(long)]
        end: Option<String>,
    },
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
    Probe {
        #[arg(long)]
        front: Option<String>,
    },
}

#[derive(Subcommand)]
enum PaperCmd {
    /// Run a session: quotes from recorded ticks (--from-db), the replay feed (--mock) or the live CTP md feed
    Run {
        #[arg(long)]
        strategy: String,
        /// dataset symbol the signals refer to, e.g. FUT_RB or SH600000
        #[arg(long)]
        symbol: String,
        /// quote instrument, e.g. rb2601 (defaults to --symbol)
        #[arg(long)]
        instrument: Option<String>,
        #[arg(long)]
        mock: bool,
        #[arg(long)]
        from_db: bool,
        #[arg(long, default_value_t = 60)]
        seconds: u64,
        #[arg(long)]
        front: Option<String>,
        #[arg(long, default_value_t = 0.1)]
        participation: f64,
        /// trade_through (default) or touch_queue
        #[arg(long, default_value = "trade_through")]
        limit_fill: String,
        #[arg(long)]
        session: Option<String>,
    },
    /// List sessions
    List,
    /// QIFI-layout account export of a session (simulated)
    Show { session: String },
    /// Re-verify a session's stored event chain
    Verify { session: String },
}

#[derive(Subcommand)]
enum JobsCmd {
    /// Crash recovery: mark jobs of dead processes abandoned, un-stick Backtesting strategies
    Reap,
}

#[derive(Subcommand)]
enum SignerCmd {
    /// Run the audit signer (as its own OS user). The key directory must be mode 0700.
    Serve {
        #[arg(long)]
        dir: std::path::PathBuf,
        #[arg(long)]
        socket: std::path::PathBuf,
        /// only these peer uids may connect (SO_PEERCRED); default: socket permissions only
        #[arg(long, value_delimiter = ',')]
        allow_uid: Vec<u32>,
    },
    /// Add a new key (rotation); new entries use it, old entries keep verifying
    Rotate {
        #[arg(long)]
        dir: std::path::PathBuf,
    },
    /// Health check: the signer answers on its socket
    Ping {
        #[arg(long)]
        socket: std::path::PathBuf,
    },
    /// Low-level enrollment (run inside the signer's own account, e.g. `docker compose exec signer`).
    /// Normally use `cmsq audit signer-enroll`, which computes chain/seq/mac.
    Enroll {
        #[arg(long)]
        dir: std::path::PathBuf,
        #[arg(long)]
        chain: String,
        #[arg(long)]
        seq: i64,
        #[arg(long)]
        mac: String,
    },
    /// Import an existing in-process audit.key as key 0 (migration), then delete the original
    Import {
        #[arg(long)]
        dir: std::path::PathBuf,
        #[arg(long)]
        key_file: std::path::PathBuf,
    },
}

#[derive(Subcommand)]
enum AuditCmd {
    Verify,
    /// Publish the audit chain head to a git repository (signed commit with --sign), optionally push / loop
    AnchorGit {
        #[arg(long)]
        repo: std::path::PathBuf,
        #[arg(long)]
        sign: bool,
        #[arg(long)]
        push: bool,
        /// repeat, e.g. 1h
        #[arg(long)]
        every: Option<String>,
    },
    /// Check every anchor ever committed to the git repository against the database
    VerifyGit {
        #[arg(long)]
        repo: std::path::PathBuf,
    },
    Tail {
        #[arg(long, default_value_t = 20)]
        n: i64,
    },
    /// Pin this workspace's audit chain (at its current head) in the signer directory.
    /// Run as the operator who can write the signer dir; replaces trust-on-first-use.
    SignerEnroll {
        #[arg(long, required_unless_present = "print")]
        signer_dir: Option<std::path::PathBuf>,
        /// only print `<chain> <seq> <mac>` (for `cmsq signer enroll` in another account)
        #[arg(long)]
        print: bool,
    },
    /// Move an existing workspace from the app-side audit.key to the signer: import the key,
    /// enroll the chain at its verified head, re-verify the whole chain THROUGH the running
    /// signer socket, then overwrite and delete the app-side key.
    MigrateToSigner {
        #[arg(long)]
        signer_dir: std::path::PathBuf,
        #[arg(long)]
        socket: std::path::PathBuf,
        /// keep the app-side key (no deletion); for dry runs
        #[arg(long)]
        keep_key: bool,
    },
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

/// (chain id, head seq, head mac) of the workspace's audit chain; the chain must verify.
fn audit_head(home: &Home, cfg: cmsq_core::audit::AuditConfig) -> Result<(String, i64, String)> {
    let chain = cfg.chain.clone();
    // Only a workspace that does not exist yet may be enrolled at the empty head. Enrolling an
    // existing chain at seq 0 would let the app re-sign history from GENESIS, so for anything
    // else the store must open and the head comes from the verified chain.
    if !home.abs().exists() {
        return Ok((chain, 0, cmsq_core::audit::GENESIS.to_string()));
    }
    let st = cmsq_core::Store::open_configured(&home.db(), &home.db_config(), cfg)?;
    let r = st.verify_audit()?;
    anyhow::ensure!(
        r.ok || r.entries == 0,
        "refusing to enroll a broken chain: {}",
        r.problem.unwrap_or_default()
    );
    if r.entries == 0 {
        return Ok((chain, 0, cmsq_core::audit::GENESIS.to_string()));
    }
    Ok((chain, r.entries, r.head_hash))
}

/// Overwrite a key file with zeros (fsync) then unlink it.
fn shred(p: &std::path::Path) -> Result<()> {
    use std::io::Write;
    let n = std::fs::metadata(p)?.len() as usize;
    let mut f = std::fs::OpenOptions::new().write(true).open(p)?;
    f.write_all(&vec![0u8; n.max(64)])?;
    f.sync_all()?;
    drop(f);
    std::fs::remove_file(p)?;
    Ok(())
}

/// "30m" / "6h" / "1d" / "90s"
fn parse_every(s: &str) -> Result<std::time::Duration> {
    let (n, u) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = n.parse().with_context(|| format!("bad interval {s}"))?;
    let secs = match u {
        "s" => n,
        "m" | "" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        _ => anyhow::bail!("bad interval unit in {s} (use s/m/h/d)"),
    };
    anyhow::ensure!(secs >= 60, "interval must be at least 1 minute");
    Ok(std::time::Duration::from_secs(secs))
}

fn pct(v: &serde_json::Value, k: &str) -> String {
    v[k].as_f64()
        .map(|x| format!("{:+.2}%", x * 100.0))
        .unwrap_or("-".into())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let home = cli
        .home
        .clone()
        .map(Home::new)
        .unwrap_or_else(Home::from_env);
    if let Cmd::Signer { cmd } = &cli.cmd {
        match cmd {
            SignerCmd::Serve {
                dir,
                socket,
                allow_uid,
            } => cmsq_core::signer::serve(dir, socket, allow_uid)?,
            SignerCmd::Rotate { dir } => {
                let id = cmsq_core::signer::SignerCore::open(dir)?.rotate()?;
                println!("new audit key {id}; restart the signer; new entries start epoch {id}");
            }
            SignerCmd::Ping { socket } => {
                use cmsq_core::audit::KeyProvider;
                cmsq_core::signer::SocketSigner::new(socket).head("ping")?;
                println!("signer ok");
            }
            SignerCmd::Enroll {
                dir,
                chain,
                seq,
                mac,
            } => {
                cmsq_core::signer::SignerCore::enroll(
                    dir,
                    &cmsq_core::signer::Enrollment {
                        chain: chain.clone(),
                        seq: *seq,
                        mac: mac.clone(),
                    },
                )?;
                println!("enrolled chain {chain} at head seq {seq}");
            }
            SignerCmd::Import { dir, key_file } => {
                cmsq_core::signer::SignerCore::import(dir, &std::fs::read_to_string(key_file)?)?;
                println!(
                    "imported as key 0 into {}; now delete {} and set CMSQ_AUDIT_SIGNER",
                    dir.display(),
                    key_file.display()
                );
            }
        }
        return Ok(());
    }
    if let Cmd::Audit {
        cmd: AuditCmd::SignerEnroll { signer_dir, print },
    } = &cli.cmd
    {
        // the chain id derives from the canonical workspace path: make it exist first
        std::fs::create_dir_all(home.abs())?;
        if *print {
            // read-only: works with the app-side key too (used by the migration in deploy.sh)
            let (chain, seq, mac) = audit_head(&home, home.audit_config()?)?;
            println!("{chain} {seq} {mac}");
            return Ok(());
        }
        let signer_dir = signer_dir.as_ref().unwrap();
        anyhow::ensure!(
            std::env::var("CMSQ_AUDIT_SIGNER").is_ok(),
            "set CMSQ_AUDIT_SIGNER (signer mode); for a workspace with an app-side audit.key use `audit migrate-to-signer`"
        );
        let (chain, seq, mac) = audit_head(&home, home.audit_config()?)?;
        cmsq_core::signer::SignerCore::enroll(
            signer_dir,
            &cmsq_core::signer::Enrollment {
                chain: chain.clone(),
                seq,
                mac: mac.clone(),
            },
        )?;
        println!(
            "enrolled chain {chain} at head seq {seq} ({})",
            &mac[..mac.len().min(16)]
        );
        return Ok(());
    }
    if let Cmd::Audit {
        cmd:
            AuditCmd::MigrateToSigner {
                signer_dir,
                socket,
                keep_key,
            },
    } = &cli.cmd
    {
        anyhow::ensure!(
            std::env::var("CMSQ_AUDIT_SIGNER").is_err() && std::env::var("CMSQ_AUDIT_KEY").is_err(),
            "unset CMSQ_AUDIT_SIGNER / CMSQ_AUDIT_KEY: migration reads the app-side key file"
        );
        let kf = home.key_file();
        anyhow::ensure!(kf.exists(), "no app-side key at {}", kf.display());
        let local = home.audit_config()?;
        let (chain, seq, mac) = audit_head(&home, local.clone())?;
        let key_hex = std::fs::read_to_string(&kf)?;
        cmsq_core::signer::SignerCore::import(signer_dir, &key_hex)?;
        cmsq_core::signer::SignerCore::enroll(
            signer_dir,
            &cmsq_core::signer::Enrollment {
                chain: chain.clone(),
                seq,
                mac,
            },
        )?;
        let mut via = local;
        via.key = Vec::new();
        via.signer = Some(std::sync::Arc::new(cmsq_core::signer::SocketSigner::new(
            socket,
        )));
        let st = cmsq_core::Store::open_configured(&home.db(), &home.db_config(), via)?;
        let r = st.verify_audit()?;
        anyhow::ensure!(
            r.ok && r.entries == seq,
            "chain does NOT verify through the signer ({}); app-side key left in place. Restart the signer so it loads key 0, then rerun verification.",
            r.problem.unwrap_or_default()
        );
        if !keep_key {
            shred(&kf)?;
        }
        println!(
            "migrated chain {chain} ({seq} entries) to the signer; verified through {}; app-side key {}. Now set CMSQ_AUDIT_SIGNER={}",
            socket.display(),
            if *keep_key { "KEPT" } else { "overwritten and deleted" },
            socket.display()
        );
        return Ok(());
    }
    let mut o = Orchestrator::open(home)?;
    if !cli.json {
        o.progress = Some(std::sync::Arc::new(|m: &str| eprintln!("  · {m}")));
    }
    let o = o;
    let out = |v: serde_json::Value, human: String| {
        if cli.json {
            println!("{}", serde_json::to_string_pretty(&v).unwrap())
        } else {
            println!("{human}")
        }
    };
    match cli.cmd {
        Cmd::Scrape {
            every,
            symbol,
            backtests,
            backend,
            cycles,
        } => {
            let sources = cmsq_core::scrape::load_sources(&o.home.abs().join("sources.json"))?;
            let token = std::fs::read_to_string(o.home.secrets.join("github_token"))
                .ok()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty());
            let fetcher = cmsq_core::scrape::HttpFetcher::new(token)?;
            let period = every.as_deref().map(parse_every).transpose()?;
            let exec = if backtests > 0 {
                Some(executor(backend).await?)
            } else {
                None
            };
            let mut n = 0;
            loop {
                n += 1;
                let rep = o
                    .scrape_cycle(&fetcher, &sources, exec.as_deref(), &symbol, backtests)
                    .await?;
                let mut lines = vec![format!("scrape cycle {n} (run {})", &rep.run_id[..8])];
                for s in &rep.sources {
                    lines.push(format!(
                        "  {:<34} quota {:>2}  new {:>2}  dup {:>2}  invalid {}  {}",
                        s["source"].as_str().unwrap_or(""),
                        s["quota"],
                        s["new"],
                        s["duplicates"],
                        s["invalid"],
                        s["error"]
                            .as_str()
                            .map(|e| format!("ERROR {e}"))
                            .unwrap_or_default()
                    ));
                }
                lines.push(format!(
                    "  extracted {}  backtested {}",
                    rep.extracted.len(),
                    rep.backtests.len()
                ));
                for b in &rep.backtests {
                    lines.push(format!(
                        "    {} {}",
                        &b["strategy"].as_str().unwrap_or("")[..8],
                        b["status"].as_str().or(b["error"].as_str()).unwrap_or("")
                    ));
                }
                for sc in &rep.scores {
                    lines.push(format!(
                        "  score {:.3}  {:<34} items {} extracted {} backtested {} passed {}",
                        sc.score, sc.source, sc.items, sc.extracted, sc.backtested, sc.gate_passed
                    ));
                }
                out(serde_json::to_value(&rep)?, lines.join("\n"));
                match period {
                    Some(p) if cycles.is_none_or(|c| n < c) => tokio::time::sleep(p).await,
                    _ => break,
                }
            }
        }
        Cmd::SourceScores => {
            let v = o.source_scores()?;
            let lines: Vec<String> = v.iter().map(|sc| format!("{:.3}  {:<34} items {:>3}  extracted {:>3}  backtested {:>3}  gate-passed {:>2}  failed {:>2}", sc.score, sc.source, sc.items, sc.extracted, sc.backtested, sc.gate_passed, sc.failed)).collect();
            out(serde_json::to_value(&v)?, lines.join("\n"));
        }
        Cmd::Init => {
            o.ensure_samples().await?;
            out(
                serde_json::json!({"home": o.home.abs()}),
                format!("workspace ready at {}", o.home.abs().display()),
            );
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
            out(
                serde_json::to_value(res.iter().map(|(f, r)| (f, r)).collect::<Vec<_>>())?,
                lines.join("\n"),
            );
        }
        Cmd::Extract { id } => {
            let ex = extract::from_env()?;
            let ids: Vec<String> = match id {
                Some(i) => vec![i],
                None => o
                    .store
                    .list_strategies(Some(StrategyStatus::Ingested))?
                    .into_iter()
                    .map(|s| s.id)
                    .collect(),
            };
            let mut lines = vec![];
            let mut js = vec![];
            for i in ids {
                let s = o.extract(&i, ex.as_ref()).await?;
                let e = s.extracted.as_ref();
                lines.push(format!(
                    "{:<9} {}  {}  params={}  risks=[{}]{}",
                    s.status.as_str(),
                    &s.id[..8],
                    s.title,
                    e.map(|e| serde_json::Value::Object(e.parameters.clone()).to_string())
                        .unwrap_or_default(),
                    e.map(|e| e
                        .risks
                        .iter()
                        .map(|r| r.code.as_str())
                        .collect::<Vec<_>>()
                        .join(","))
                        .unwrap_or_default(),
                    s.last_reason
                        .as_ref()
                        .map(|r| format!("\n          reason: {r}"))
                        .unwrap_or_default()
                ));
                js.push(serde_json::to_value(&s)?);
            }
            out(serde_json::Value::Array(js), lines.join("\n"));
        }
        Cmd::Backtest {
            id,
            all,
            symbol,
            backend,
            params,
            symbols,
            workers,
        } => {
            let exec = executor(backend).await?;
            let ov: Option<serde_json::Value> = params
                .map(|p| serde_json::from_str(&p))
                .transpose()
                .context("--params must be JSON")?;
            let ids: Vec<String> = if all {
                o.store
                    .list_strategies(Some(StrategyStatus::Extracted))?
                    .into_iter()
                    .map(|s| s.id)
                    .collect()
            } else {
                vec![id.context("give an id or --all")?]
            };
            let syms: Vec<String> = symbols
                .map(|s| {
                    s.split(',')
                        .map(|x| x.trim().to_uppercase())
                        .filter(|x| !x.is_empty())
                        .collect()
                })
                .unwrap_or_else(|| vec![symbol.clone()]);
            let mut lines = vec![format!(
                "backend: {} (hardened={})  symbols: {}  workers: {workers}  container slots: {}",
                exec.name(),
                exec.hardened(),
                syms.join(","),
                o.slots.available_permits()
            )];
            let mut js = vec![];
            let results: Vec<(
                String,
                String,
                Result<cmsq_core::orchestrator::BacktestSummary>,
            )> = if syms.len() == 1 && ids.len() == 1 && ov.is_some() {
                vec![(
                    ids[0].clone(),
                    syms[0].clone(),
                    o.backtest(&ids[0], &syms[0], exec.as_ref(), ov.clone())
                        .await,
                )]
            } else if ov.is_some() {
                anyhow::bail!("--params applies to a single id and symbol");
            } else {
                let pairs: Vec<(String, String)> = ids
                    .iter()
                    .flat_map(|i| syms.iter().map(move |s| (i.clone(), s.clone())))
                    .collect();
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
                let job = o
                    .store
                    .list_jobs(Some(&r.strategy_id))?
                    .into_iter()
                    .find(|j| j.id == r.job_id);
                let dsr = job
                    .as_ref()
                    .and_then(|j| j.result.as_ref())
                    .map(|v| v["dsr"].clone())
                    .unwrap_or_default();
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
        Cmd::Sweep {
            id,
            symbol,
            grid,
            workers,
            backend,
        } => {
            let exec = executor(backend).await?;
            let g: serde_json::Value =
                serde_json::from_str(&grid).context("--grid must be a JSON object")?;
            let g = g.as_object().context("--grid must be a JSON object")?;
            let r = o
                .sweep(&id, &symbol.to_uppercase(), g, exec.as_ref(), workers)
                .await?;
            let mut lines = vec![format!(
                "sweep {} on {}: {} points, workers {workers}",
                &r.run_id[..8],
                symbol.to_uppercase(),
                r.points.len()
            )];
            for p in &r.points {
                lines.push(format!(
                    "  #{:<3} {:<34} {:<15} sharpe {:>6}  ret {:>9}  DSR@record {}{}",
                    p["point"],
                    p["params"].to_string(),
                    p["status"].as_str().unwrap_or(""),
                    p["sharpe"]
                        .as_f64()
                        .map(|x| format!("{x:.2}"))
                        .unwrap_or("-".into()),
                    pct(p, "total_return"),
                    p["dsr_at_record"]
                        .as_f64()
                        .map(|x| format!("{x:.3}"))
                        .unwrap_or("-".into()),
                    if p["lookahead"].as_bool() == Some(true) {
                        "  [LOOK-AHEAD]"
                    } else {
                        ""
                    }
                ));
            }
            lines.push(format!(
                "best: {}  params {}  → {}  DSR(final ledger) {} N={}",
                r.best_job.as_deref().map(|j| &j[..8]).unwrap_or("-"),
                r.best_params.clone().unwrap_or_default(),
                r.status,
                r.dsr
                    .as_ref()
                    .map(|d| format!("{:.3}", d.dsr))
                    .unwrap_or("-".into()),
                r.dsr.as_ref().map(|d| d.trials).unwrap_or(0)
            ));
            for why in &r.reasons {
                lines.push(format!("  ✗ {why}"));
            }
            out(serde_json::to_value(&r)?, lines.join("\n"));
        }
        Cmd::Portfolio {
            id,
            symbols,
            alloc,
            params,
            workers,
            backend,
        } => {
            let exec = executor(backend).await?;
            let syms: Vec<String> = symbols
                .split(',')
                .map(|x| x.trim().to_uppercase())
                .filter(|x| !x.is_empty())
                .collect();
            let alloc = parse_alloc(&alloc)?;
            let ov = params
                .map(|p| serde_json::from_str(&p))
                .transpose()
                .context("--params must be JSON")?;
            let r = o
                .portfolio(&id, &syms, alloc, ov, exec.as_ref(), workers)
                .await?;
            let p = &r.result["portfolio"];
            let mut lines = vec![format!(
                "portfolio {} [{}] {}",
                &r.run_id[..8],
                syms.join("+"),
                r.result["alloc"]
            )];
            if p.is_null() {
                lines.extend(r.reasons.iter().map(|x| format!("  ✗ {x}")));
            } else {
                lines.push(format!(
                    "  window {} .. {} ({} bars, development only)",
                    r.result["window"]["start"].as_str().unwrap_or(""),
                    r.result["window"]["end"].as_str().unwrap_or(""),
                    r.result["window"]["bars"]
                ));
                lines.push(format!(
                    "  return {}  sharpe {:.2}  maxDD {}  exposure {}  trades {}  DSR {:.3} (N={})",
                    pct(p, "total_return"),
                    p["sharpe"].as_f64().unwrap_or(0.0),
                    pct(p, "max_drawdown"),
                    pct(p, "exposure"),
                    r.result["trades"],
                    r.result["dsr"]["dsr"].as_f64().unwrap_or(0.0),
                    r.result["dsr"]["trials"]
                ));
                for l in r.result["legs"].as_array().into_iter().flatten() {
                    lines.push(format!(
                        "    leg {:<10} sharpe {:.2}  ret {}",
                        l["symbol"].as_str().unwrap_or(""),
                        l["development"]["sharpe"].as_f64().unwrap_or(0.0),
                        pct(&l["development"], "total_return")
                    ));
                }
                lines.push(format!(
                    "  gate (informational): {}",
                    if r.passed {
                        "PASS".to_string()
                    } else {
                        format!("FAIL — {}", r.reasons.join("; "))
                    }
                ));
            }
            out(serde_json::to_value(&r)?, lines.join("\n"));
        }
        Cmd::Runs { kind } => {
            let v = o.store.list_runs(kind.as_deref())?;
            let lines: Vec<String> = v
                .iter()
                .map(|r| {
                    format!(
                        "{}  {:<9} {:<9} {}  {}",
                        &r.id[..8],
                        r.kind,
                        r.status,
                        &r.created_at[..19],
                        r.spec
                    )
                })
                .collect();
            out(serde_json::to_value(&v)?, lines.join("\n"));
        }
        Cmd::Db {
            cmd: DbCmd::Migrate,
        } => {
            let c = o.home.db_config();
            let mu = c
                .migrate_url
                .clone()
                .context("no CMSQ_DATABASE_MIGRATE_URL (run scripts/db_up.sh)")?;
            cmsq_core::store::Store::migrate_pg(&mu, c.schema.as_deref())?;
            out(
                serde_json::json!({"migrated": true, "schema": c.schema}),
                format!(
                    "migrations applied (schema {})",
                    c.schema.unwrap_or_default()
                ),
            );
        }
        Cmd::Db { cmd: DbCmd::Status } => {
            let c = o.home.db_config();
            let v = serde_json::json!({"backend": o.store.backend(), "schema": c.schema, "url": c.url.as_deref().map(cmsq_core::store::redact_url),
                "strategies": o.store.list_strategies(None)?.len(), "trials": o.store.trial_count()?, "audit": o.store.audit_entries(None, 1)?.first().map(|a| a.seq),
                "runs": o.store.list_runs(None)?.len(), "ticks": o.store.tick_count(None)?});
            out(v.clone(), format!("backend {}  schema {}  url {}\n  strategies {}  trials {}  audit head {}  research runs {}  ticks {}", v["backend"], v["schema"], v["url"], v["strategies"], v["trials"], v["audit"], v["runs"], v["ticks"]));
        }
        Cmd::Md { cmd } => md_cmd(&o, cmd, &out).await?,
        Cmd::Paper { cmd } => paper_cmd(&o, cmd, &out)?,
        Cmd::Promote { id, confirm } => {
            let s0 = o.get(&id)?;
            let confirm = match confirm {
                Some(c) => c,
                None => {
                    use std::io::{BufRead, IsTerminal, Write};
                    anyhow::ensure!(
                        std::io::stdin().is_terminal(),
                        "no --confirm given and stdin is not a terminal"
                    );
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
                format!(
                    "{} {}  {}  — {}",
                    s.status.as_str().to_uppercase(),
                    &s.id[..8],
                    s.title,
                    s.last_reason.clone().unwrap_or_default()
                )
            };
            out(serde_json::to_value(&s)?, msg);
            if s.status != StrategyStatus::Promoted {
                std::process::exit(3);
            }
        }
        Cmd::Reject { id, reason } => {
            let s = o.reject(&id, "user:cli", &reason)?;
            out(
                serde_json::to_value(&s)?,
                format!("REJECTED {}  {}", &s.id[..8], s.title),
            );
        }
        Cmd::List { status } => {
            let st = status
                .map(|s| StrategyStatus::parse(&s).context("unknown status"))
                .transpose()?;
            let v = o.store.list_strategies(st)?;
            let mut lines = vec![format!(
                "{:<8}  {:<11} {:>3}  {:<14} {}",
                "ID", "STATUS", "TRY", "SOURCE", "TITLE"
            )];
            for s in &v {
                lines.push(format!(
                    "{}  {:<11} {:>3}  {:<14} {}{}",
                    &s.id[..8],
                    s.status.as_str(),
                    s.attempts,
                    s.source_site,
                    s.title,
                    if s.sample { "  [sample]" } else { "" }
                ));
            }
            out(serde_json::to_value(&v)?, lines.join("\n"));
        }
        Cmd::Show { id } => {
            let s = o.get(&id)?;
            let ev = o.store.latest_evaluation(&s.id)?;
            let jobs = o.store.list_jobs(Some(&s.id))?;
            let audit = o.store.audit_entries(Some(&s.id), 50)?;
            let v =
                serde_json::json!({"strategy": s, "evaluation": ev, "jobs": jobs, "audit": audit});
            let mut h = format!(
                "{}  {}\nstatus: {}  attempts: {}\nsource: {} {}\nraw: {}\n",
                s.id,
                s.title,
                s.status,
                s.attempts,
                s.source_site,
                s.source_url.clone().unwrap_or_default(),
                s.raw_path
            );
            if let Some(r) = &s.last_reason {
                h += &format!("reason: {r}\n");
            }
            if let Some(e) = &s.extracted {
                h += &format!(
                    "summary: {}\nrisks: {}\n",
                    e.summary,
                    e.risks
                        .iter()
                        .map(|r| r.message.clone())
                        .collect::<Vec<_>>()
                        .join(" | ")
                );
            }
            for j in &jobs {
                h += &format!(
                    "job {} {} {} {}ms  logs: {}/stderr.log\n",
                    &j.id[..8],
                    j.backend,
                    j.status,
                    j.duration_ms.unwrap_or(0),
                    j.job_dir
                );
            }
            h += "audit:\n";
            for a in audit.iter().rev() {
                h += &format!(
                    "  #{:<4} {} {:<12} {}\n",
                    a.seq,
                    &a.ts[..19],
                    a.actor,
                    a.action
                );
            }
            out(v, h);
        }
        Cmd::Jobs {
            cmd: Some(JobsCmd::Reap),
        } => {
            let v = o.reap_stale_jobs()?;
            out(
                serde_json::json!(v),
                format!("reaped {} stuck job(s)/strategy(ies)", v.len()),
            );
        }
        Cmd::Jobs { cmd: None } => {
            let v = o.store.list_jobs(None)?;
            let mut lines = vec![];
            for j in &v {
                lines.push(format!(
                    "{}  {:<5} {:<20} {:<15} {:<12} {:>6}ms  strategy {}  runner {}",
                    &j.id[..8],
                    j.kind,
                    j.backend,
                    j.status,
                    j.symbol,
                    j.duration_ms.unwrap_or(0),
                    &j.strategy_id[..8],
                    j.runner_identity
                        .as_deref()
                        .map(|r| r.chars().take(19).collect::<String>()
                            + "…"
                            + r.rsplit('|').next().unwrap_or(""))
                        .unwrap_or_default()
                ));
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
            let mut lines = vec![format!(
                "library-wide trials: {}  (distinct ideas: {})",
                v.len(),
                v.iter()
                    .map(|t| &t.idea_key)
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
            )];
            for t in &v {
                lines.push(format!(
                    "{}  idea {}  params {}  {:<12} SR/day {:+.4}",
                    &t.strategy_id[..8],
                    &t.idea_key[..10],
                    t.params_hash,
                    t.symbol,
                    t.sharpe_daily
                ));
            }
            out(serde_json::json!(v.iter().map(|t| serde_json::json!({"strategy": t.strategy_id, "idea_key": t.idea_key, "params_hash": t.params_hash, "symbol": t.symbol, "sharpe_daily": t.sharpe_daily})).collect::<Vec<_>>()), lines.join("\n"));
        }
        Cmd::Signals => {
            let v = o.store.list_signals()?;
            let lines: Vec<String> = v
                .iter()
                .map(|(s, t, _)| {
                    format!(
                        "{}  {}  target={:.2}  {}  [OBSERVE ONLY]",
                        s.date, s.symbol, s.target_position, t
                    )
                })
                .collect();
            out(
                serde_json::to_value(v.iter().map(|x| &x.0).collect::<Vec<_>>())?,
                if lines.is_empty() {
                    "no signals (only promoted strategies emit observe-only signals)".into()
                } else {
                    lines.join("\n")
                },
            );
        }
        Cmd::Data {
            cmd:
                DataCmd::Fetch {
                    symbol,
                    concurrency,
                    count,
                    sources,
                    no_validate,
                },
        } => {
            use futures_util::stream::{self, StreamExt};
            let syms: Vec<String> = symbol
                .split(',')
                .map(|x| x.trim().to_uppercase())
                .filter(|x| !x.is_empty())
                .collect();
            let (router, notes) = o.data_router(sources.as_deref(), !no_validate);
            for n in notes {
                eprintln!("  · {n}");
            }
            let (o_ref, r_ref) = (&o, &router);
            // one router shared by all tasks: tushare's token bucket (400/min) and akshare's
            // serial ≥1 s spacing are per source, so concurrency cannot exceed them
            let res: Vec<(String, Result<cmsq_core::market::DataMeta>)> =
                stream::iter(syms.clone())
                    .map(|sym| async move {
                        let r = if sym.starts_with("SAMPLE") {
                            o_ref.fetch_data(&SampleSource, &sym, count).await
                        } else {
                            if r_ref.sources.is_empty() {
                                Err(anyhow::anyhow!("no data source available"))
                            } else {
                                o_ref.fetch_routed(r_ref, &sym, count).await
                            }
                        };
                        (sym, r)
                    })
                    .buffer_unordered(concurrency.max(1))
                    .collect()
                    .await;
            let mut lines = vec![];
            let mut js = vec![];
            let mut fail = false;
            for (sym, r) in res {
                match r {
                    Ok(meta) => {
                        let v = &meta.extra["validation"];
                        let val = if v.is_null() {
                            String::new()
                        } else {
                            format!(
                                "\n  cross-validation: {} vs {} — {}",
                                v["status"].as_str().unwrap_or("?"),
                                v["against"].as_str().unwrap_or("-"),
                                v["note"].as_str().unwrap_or("")
                            )
                        };
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
            out(
                if js.len() == 1 {
                    js[0].clone()
                } else {
                    serde_json::Value::Array(js)
                },
                lines.join("\n"),
            );
            if fail {
                std::process::exit(1);
            }
        }
        Cmd::Data {
            cmd:
                DataCmd::Limits {
                    symbol,
                    contract,
                    start,
                    end,
                },
        } => {
            let t = cmsq_core::data::tushare::Tushare::from_secrets(&o.home.secrets)?;
            let end = end.unwrap_or_else(chrono_today);
            let (code, fut, symbol) = match cmsq_core::futures::spec_for(&symbol) {
                Some(spec) => {
                    let c = contract.context("futures need --contract (e.g. rb2601)")?;
                    anyhow::ensure!(
                        c.to_uppercase().starts_with(&spec.product)
                            && c.len() <= 12
                            && c.chars().all(|x| x.is_ascii_alphanumeric()),
                        "contract {c} is not a {} contract",
                        spec.product
                    );
                    (format!("{}.{}", c.to_uppercase(), spec.ts_suffix), true, c)
                }
                None => (cmsq_core::data::tushare::ts_code(&symbol)?, false, symbol),
            };
            let rows = t.price_limits(&code, fut, &start, &end).await?;
            anyhow::ensure!(
                !rows.is_empty(),
                "tushare returned no price limits for {code}"
            );
            std::fs::create_dir_all(o.home.data_dir())?;
            let p = limits_path(&o.home.data_dir(), &symbol);
            let mut csv = String::from("date,up_limit,down_limit\n");
            for (d, u, l) in &rows {
                csv.push_str(&format!("{d},{u},{l}\n"));
            }
            std::fs::write(&p, &csv)?;
            let sha = cmsq_core::calendars::sha256_hex(csv.as_bytes());
            o.store.audit("data", "price_limits_fetch", Some(&symbol), serde_json::json!({"source": if fut {"tushare ft_limit"} else {"tushare stk_limit"}, "ts_code": code, "days": rows.len(), "first": rows[0].0, "last": rows[rows.len()-1].0, "snapshot_sha256": sha}), true)?;
            out(
                serde_json::json!({"symbol": symbol, "days": rows.len(), "path": p}),
                format!(
                    "{} days of exchange price limits for {code} -> {}",
                    rows.len(),
                    p.display()
                ),
            );
        }
        Cmd::Data {
            cmd: DataCmd::Calendar { start, exchange },
        } => {
            let t = cmsq_core::data::tushare::Tushare::from_secrets(&o.home.secrets)?;
            // through the end of next year: exchanges publish the holiday schedule in advance
            let end = format!(
                "{}-12-31",
                chrono_today()[..4].parse::<i32>().unwrap_or(2026) + 1
            );
            let ex = exchange.to_uppercase();
            let list: Vec<String> = if ex == "FUTURES" {
                cmsq_core::calendars::FUTURES_EXCHANGES
                    .iter()
                    .map(|x| x.to_string())
                    .collect()
            } else {
                vec![ex]
            };
            std::fs::create_dir_all(o.home.data_dir())?;
            let (mut js, mut lines, mut fail) = (vec![], vec![], false);
            for ex in list {
                match t.trade_cal_for(&ex, &start, &end).await {
                    Ok(days) if days.iter().any(|d| d.1) => {
                        let cal = cmsq_core::replay::Calendar::from_days(days);
                        let p =
                            cmsq_core::calendars::ExchangeCalendar::path(&o.home.data_dir(), &ex);
                        cal.save_as(&p, &ex)?;
                        let sha = cmsq_core::calendars::sha256_hex(&std::fs::read(&p)?);
                        o.store.audit("data", "trade_calendar_fetch", None, serde_json::json!({"source": "tushare trade_cal", "exchange": ex, "first": cal.first, "last": cal.last, "open_days": cal.open.len(), "snapshot_sha256": sha}), true)?;
                        lines.push(format!(
                            "{ex} calendar {}..{}: {} open days → {} (sha256 {})",
                            cal.first,
                            cal.last,
                            cal.open.len(),
                            p.display(),
                            &sha[..12]
                        ));
                        js.push(serde_json::json!({"exchange": ex, "path": p, "open_days": cal.open.len(), "sha256": sha}));
                    }
                    Ok(_) => {
                        fail = true;
                        lines.push(format!(
                            "{ex}: tushare returned no open days; snapshot left unchanged"
                        ));
                    }
                    Err(e) => {
                        fail = true;
                        lines.push(format!("{ex}: FAILED {e:#}; snapshot left unchanged"));
                    }
                }
            }
            out(serde_json::Value::Array(js), lines.join("\n"));
            if fail {
                std::process::exit(1);
            }
        }
        Cmd::Data {
            cmd: DataCmd::Sources,
        } => {
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
        Cmd::Replay {
            symbol,
            strategy,
            params,
        } => {
            use cmsq_core::{market, replay, scorer};
            let sym = symbol.to_uppercase();
            let bars = market::read_bars(&o.home.data_dir(), &sym)?;
            let view = cmsq_core::probe::container_view(&bars);
            let mut s = replay::by_name(&strategy, &serde_json::from_str(&params)?)?;
            let pos = replay::BarPlayer::new(&view).run(s.as_mut());
            let sim = scorer::simulate(&sym, &bars, &pos, &scorer::CostModel::default());
            let m = scorer::metrics(&bars, &sim, 0, bars.len());
            let cal_note =
                match replay::Calendar::load(&o.home.data_dir().join("_calendar_SSE.txt")) {
                    Ok(c) => {
                        let (miss, extra) = c.check(&bars);
                        format!(
                            "calendar check: {} open days missing, {} bars on closed days",
                            miss.len(),
                            extra.len()
                        )
                    }
                    Err(_) => "calendar check skipped (run `cmsq data calendar`)".into(),
                };
            out(serde_json::json!({"strategy": s.name(), "metrics": m, "trades": sim.trades, "note": "native host replay over the FULL series; not gated, not a promotion path"}),
                format!("{} on {sym} ({} .. {}, {} bars) — NATIVE REPLAY, full series, NOT gated\n  net return {:+.2}%  sharpe {:.2}  maxDD {:.1}%  exposure {:.0}%  trades {}\n  {cal_note}",
                    s.name(), m.start, m.end, m.bars, m.total_return * 100.0, m.sharpe, m.max_drawdown * 100.0, m.exposure * 100.0, sim.trades));
        }
        Cmd::Data {
            cmd:
                DataCmd::Import {
                    symbol,
                    file,
                    adjustment,
                },
        } => {
            let meta = o
                .fetch_data(
                    &cmsq_core::market::CsvSource {
                        path: file,
                        adjustment,
                    },
                    &symbol.to_uppercase(),
                    usize::MAX,
                )
                .await?;
            out(
                serde_json::to_value(&meta)?,
                format!(
                    "imported {} bars {} .. {} as {} (adjustment: {})",
                    meta.bars, meta.start, meta.end, meta.symbol, meta.adjustment
                ),
            );
        }
        Cmd::Data { cmd: DataCmd::List } => {
            let mut lines = vec![];
            for e in std::fs::read_dir(o.home.data_dir())? {
                let p = e?.path();
                if p.to_string_lossy().ends_with(".meta.json") {
                    let m: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p)?)?;
                    lines.push(format!(
                        "{:<14} {:>5} bars  {} .. {}  {}  [{}]",
                        m["symbol"].as_str().unwrap_or(""),
                        m["bars"],
                        m["start"].as_str().unwrap_or(""),
                        m["end"].as_str().unwrap_or(""),
                        m["source"].as_str().unwrap_or(""),
                        m["label"].as_str().unwrap_or("")
                    ));
                }
            }
            lines.sort();
            out(serde_json::json!(lines), lines.join("\n"));
        }
        Cmd::Audit {
            cmd: AuditCmd::Verify,
        } => {
            let r = o.store.verify_audit()?;
            let h = if r.ok {
                format!(
                    "audit chain OK: {} entries, HMAC-keyed, head {} anchored at seq {:?} in {}",
                    r.entries,
                    &r.head_hash[..16],
                    r.anchored_seq,
                    r.anchor.clone().unwrap_or_default()
                )
            } else {
                format!(
                    "AUDIT CHAIN BROKEN at seq {:?}: {}",
                    r.first_bad_seq,
                    r.problem.clone().unwrap_or_default()
                )
            };
            let ok = r.ok;
            out(serde_json::to_value(&r)?, h);
            if !ok {
                std::process::exit(1);
            }
        }
        Cmd::Audit {
            cmd:
                AuditCmd::AnchorGit {
                    repo,
                    sign,
                    push,
                    every,
                },
        } => loop {
            let r = o.store.verify_audit()?;
            anyhow::ensure!(
                r.ok,
                "refusing to anchor a broken chain: {}",
                r.problem.unwrap_or_default()
            );
            let c = cmsq_core::gitanchor::publish(
                &repo,
                o.store.audit_chain_id(),
                r.entries,
                &r.head_hash,
                sign,
                push,
            )?;
            out(
                serde_json::json!({"chain": o.store.audit_chain_id(), "seq": r.entries, "head": r.head_hash, "commit": c}),
                match &c {
                    Some(c) => format!(
                        "anchored seq {} head {} in commit {}",
                        r.entries,
                        &r.head_hash[..16],
                        &c[..12]
                    ),
                    None => "head already anchored".into(),
                },
            );
            match &every {
                Some(e) => tokio::time::sleep(parse_every(e)?).await,
                None => break,
            }
        },
        Cmd::Audit {
            cmd: AuditCmd::VerifyGit { repo },
        } => {
            let (n, bad) = o.store.verify_git_anchors(&repo)?;
            out(
                serde_json::json!({"anchors": n, "problems": bad}),
                if bad.is_empty() {
                    format!("{n} git anchors match the database")
                } else {
                    format!("GIT ANCHOR MISMATCH:\n{}", bad.join("\n"))
                },
            );
            if !bad.is_empty() {
                std::process::exit(1);
            }
        }
        Cmd::Signer { .. }
        | Cmd::Audit {
            cmd: AuditCmd::SignerEnroll { .. } | AuditCmd::MigrateToSigner { .. },
        } => unreachable!(),
        Cmd::Audit {
            cmd: AuditCmd::Tail { n },
        } => {
            let v = o.store.audit_entries(None, n)?;
            let lines: Vec<String> = v
                .iter()
                .rev()
                .map(|a| {
                    format!(
                        "#{:<4} {} {:<14} {:<34} {} {}",
                        a.seq,
                        &a.ts[..19],
                        a.actor,
                        a.action,
                        a.target
                            .as_deref()
                            .map(|t| &t[..t.len().min(8)])
                            .unwrap_or("-"),
                        if a.success { "" } else { "FAIL" }
                    )
                })
                .collect();
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
        "inverse_vol" | "invvol" => Alloc::InverseVol {
            lookback: if v.is_empty() {
                60
            } else {
                v.parse().context("lookback")?
            },
        },
        "fixed" => Alloc::Fixed {
            weights: v
                .split(',')
                .map(|x| x.trim().parse::<f64>())
                .collect::<std::result::Result<_, _>>()
                .context("fixed:W1,W2,...")?,
        },
        _ => anyhow::bail!("--alloc must be equal | inverse_vol[:N] | fixed:W1,W2,..."),
    })
}

fn paper_cmd(
    o: &Orchestrator,
    cmd: PaperCmd,
    out: &dyn Fn(serde_json::Value, String),
) -> Result<()> {
    use cmsq_core::ctp::{self, Aggregator, FeedEvent, MdConfig};
    use cmsq_paper::*;
    match cmd {
        PaperCmd::List => {
            let v = o.store.list_paper_sessions()?;
            let lines: Vec<String> = v
                .iter()
                .map(|r| {
                    format!(
                        "{}  strategy {}  {}  {}",
                        r.0,
                        &r.1[..8.min(r.1.len())],
                        r.3,
                        r.4
                    )
                })
                .collect();
            out(serde_json::to_value(&v)?, lines.join("\n"));
        }
        PaperCmd::Verify { session } => {
            let (n, h) = verify_chain(&o.store, &session)?;
            out(
                serde_json::json!({"session": session, "events": n, "chain": h, "ok": true}),
                format!("{session}: {n} events, chain {h} verified"),
            );
        }
        PaperCmd::Show { session } => {
            let r = PaperSession::resume(&o.store, &session)?;
            let q = r.engine.qifi(&session);
            out(q.clone(), serde_json::to_string_pretty(&q)?);
        }
        PaperCmd::Run {
            strategy,
            symbol,
            instrument,
            mock,
            from_db,
            seconds,
            front,
            participation,
            limit_fill,
            session,
        } => {
            let s = o.get(&strategy)?;
            anyhow::ensure!(
                s.status == StrategyStatus::Promoted,
                "strategy {} is {}: only Promoted strategies may paper-trade",
                s.id,
                s.status
            );
            let inst = instrument.unwrap_or_else(|| symbol.clone());
            let market = match cmsq_core::futures::spec_for(&symbol) {
                Some(spec) => Market::Futures { spec },
                None => Market::AShare {
                    rules: Default::default(),
                },
            };
            let prev_close = cmsq_core::market::read_bars(&o.home.data_dir(), &symbol)
                .ok()
                .and_then(|b| b.last().map(|x| x.close))
                .unwrap_or(0.0);
            let cfg = PaperConfig {
                participation,
                limit_fill: match limit_fill.as_str() {
                    "touch_queue" => LimitFill::TouchQueue,
                    "trade_through" => LimitFill::TradeThrough,
                    x => anyhow::bail!("unknown --limit-fill {x}"),
                },
                ..Default::default()
            };
            let mut e = PaperEngine::new(cfg);
            e.add_instrument(InstrumentSetup {
                symbol: inst.clone(),
                market,
                prev_close,
                suspended: false,
                exchange_limits: read_limits(&o.home.data_dir(), &inst),
            });
            let id = session.unwrap_or_else(|| {
                format!(
                    "paper_{}_{}",
                    &s.id[..8.min(s.id.len())],
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis()
                )
            });
            let mut ps = PaperSession::start(&o.store, &id, &s, e)?;
            let sig: Vec<(String, f64)> = o
                .store
                .list_signals()?
                .into_iter()
                .filter(|(g, ..)| g.strategy_id == s.id && g.symbol == symbol)
                .map(|(g, ..)| (g.date, g.target_position))
                .collect();
            let mut drv = SignalDriver::new(sig, &inst);
            let hot = cmsq_core::hot::global();
            let mut n = 0usize;
            if from_db {
                for t in o.store.list_ticks(&inst, 5_000_000)? {
                    for f in drv.on_quote(&mut ps, &s, &Quote::from(&t))? {
                        hot.publish(
                            cmsq_core::hot::CH_PAPER,
                            &serde_json::json!({"session": &id, "fill": f}),
                        );
                    }
                    n += 1;
                }
            } else {
                let mut feed: Box<dyn ctp::MdFeed> = if mock {
                    Box::new(ctp::ReplayFeed::demo(std::slice::from_ref(&inst)))
                } else {
                    ctp::live_feed()?
                };
                let mcfg = MdConfig {
                    front: front.unwrap_or_else(|| ctp::OPENCTP_7X24_MD.into()),
                    instruments: vec![inst.clone()],
                    broker_id: String::new(),
                    duration: std::time::Duration::from_secs(seconds),
                };
                feed.start(&mcfg)?;
                let mut agg = Aggregator::new(&mcfg.instruments);
                agg.calendars = cmsq_core::calendars::FuturesCalendars::load(&o.home.data_dir());
                let end = std::time::Instant::now() + mcfg.duration;
                while std::time::Instant::now() < end && !feed.finished() {
                    if let Some(FeedEvent::Tick(r)) =
                        feed.next(std::time::Duration::from_millis(500))
                    {
                        if let Some(t) = agg.accept(&r, &format!("ctp:{}", feed.name())) {
                            let mut q = Quote::from(&t);
                            if r.upper_limit.is_finite()
                                && r.upper_limit > 0.0
                                && r.upper_limit < 1e12
                            {
                                q.upper_limit = Some(r.upper_limit);
                            }
                            if r.lower_limit.is_finite()
                                && r.lower_limit > 0.0
                                && r.lower_limit < 1e12
                            {
                                q.lower_limit = Some(r.lower_limit);
                            }
                            hot.put_quote(&q.symbol, &serde_json::to_value(&q)?);
                            for f in drv.on_quote(&mut ps, &s, &q)? {
                                hot.publish(
                                    cmsq_core::hot::CH_PAPER,
                                    &serde_json::json!({"session": &id, "fill": f}),
                                );
                            }
                            n += 1;
                        }
                    }
                }
            }
            let fills = ps.engine.fills.len();
            let eq = ps.engine.equity();
            let chain = ps.engine.event_log_hash();
            let reb = drv.rebalances.clone();
            ps.stop()?;
            out(serde_json::json!({"session": id, "quotes": n, "fills": fills, "equity": eq, "chain": chain, "rebalances": reb, "simulated": true}),
                format!("paper session {id}: {n} quotes, {} rebalances, {fills} simulated fills, equity {eq:.2}\nchain {chain}", reb.len()));
        }
    }
    Ok(())
}

async fn md_cmd(
    o: &Orchestrator,
    cmd: MdCmd,
    out: &dyn Fn(serde_json::Value, String),
) -> Result<()> {
    use cmsq_core::ctp::{self, MdConfig};
    match cmd {
        MdCmd::Probe { front } => {
            let f = front.unwrap_or_else(|| ctp::OPENCTP_7X24_MD.into());
            let r = ctp::probe_front(&f, std::time::Duration::from_secs(6)).await;
            out(
                serde_json::json!({"front": f, "reachable": r.is_ok(), "error": r.as_ref().err().map(|e| e.to_string())}),
                match r {
                    Ok(ms) => format!("{f}: TCP reachable in {ms} ms"),
                    Err(e) => format!("{f}: NOT reachable from this machine: {e:#}"),
                },
            );
        }
        MdCmd::Run {
            instruments,
            front,
            seconds,
            mock,
        } => {
            let cfg = MdConfig {
                front: front.unwrap_or_else(|| ctp::OPENCTP_7X24_MD.into()),
                instruments: instruments
                    .split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect(),
                broker_id: "9999".into(),
                duration: std::time::Duration::from_secs(seconds),
            };
            let feed: Box<dyn ctp::MdFeed> = if mock {
                Box::new(ctp::ReplayFeed::demo(&cfg.instruments))
            } else {
                ctp::live_feed()?
            };
            let rep = ctp::run_md(o, feed, &cfg).await?;
            out(serde_json::to_value(&rep)?, format!("md [{}] {}: {} ticks stored, {} one-minute bars, {} rejected; instruments {:?}", rep.feed, cfg.front, rep.ticks, rep.bars, rep.rejected, cfg.instruments));
        }
    }
    Ok(())
}

fn limits_path(data: &std::path::Path, symbol: &str) -> std::path::PathBuf {
    data.join(format!("{symbol}.limits.csv"))
}

/// data/<SYMBOL>.limits.csv -> trading day -> (up, down); empty when absent/unparseable rows
fn read_limits(
    data: &std::path::Path,
    symbol: &str,
) -> std::collections::BTreeMap<String, (f64, f64)> {
    let mut m = std::collections::BTreeMap::new();
    if let Ok(s) = std::fs::read_to_string(limits_path(data, symbol)) {
        for l in s.lines().skip(1) {
            let f: Vec<&str> = l.split(',').collect();
            if let [d, u, dn] = f[..] {
                if let (Ok(u), Ok(dn)) = (u.parse::<f64>(), dn.parse::<f64>()) {
                    if u.is_finite() && dn.is_finite() && u > dn && dn > 0.0 {
                        m.insert(d.to_string(), (u, dn));
                    }
                }
            }
        }
    }
    m
}

fn chrono_today() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 8 * 3600;
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
