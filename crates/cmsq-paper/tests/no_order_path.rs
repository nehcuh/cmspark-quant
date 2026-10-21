//! Hard guarantee: paper trading has no code path to a real order.
//! The crate may not depend on CTP, a network client, or any broker SDK, and
//! its sources may not name a trading API.
#[test]
fn no_broker_or_network_dependency() {
    let toml = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    let deps = toml
        .split("[dependencies]")
        .nth(1)
        .unwrap()
        .split("\n[")
        .next()
        .unwrap();
    for bad in [
        "ctp",
        "reqwest",
        "hyper",
        "tokio",
        "tungstenite",
        "redis",
        "sqlx",
        "openssl",
        "socket",
    ] {
        assert!(!deps.contains(bad), "cmsq-paper must not depend on {bad}");
    }
    // cmsq-core must be used WITHOUT its ctp feature (cmsq-core's own test asserts its
    // ctp module, market data only, never names the trading API)
    let core = deps.lines().find(|l| l.starts_with("cmsq-core")).unwrap();
    assert!(
        !core.contains("features") && !core.contains("ctp"),
        "{core}"
    );
}

#[test]
fn no_trading_api_symbols_in_sources() {
    let needles = [
        ["Trader", "Api"].concat(),
        ["ReqOrder", "Insert"].concat(),
        ["OrderInsert"].concat(),
        ["Std", "Tcp"].concat(),
        ["Tcp", "Stream"].concat(),
        ["Udp", "Socket"].concat(),
        ["std::", "net"].concat(),
    ];
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut n = 0;
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let s = std::fs::read_to_string(&p).unwrap();
        n += 1;
        for k in &needles {
            assert!(!s.contains(k.as_str()), "{} mentions {k}", p.display());
        }
    }
    assert!(n > 0);
}
