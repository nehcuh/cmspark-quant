//! Live CTP market data via ctp2rs `MdApi` (feature `ctp`). Market data only.
use super::md::{inv, FeedEvent, MdConfig, MdFeed, RawTick};
use anyhow::Result;
use ctp2rs::v1alpha1::{CThostFtdcDepthMarketDataField, CThostFtdcReqUserLoginField, CThostFtdcRspInfoField, CThostFtdcRspUserLoginField, CThostFtdcSpecificInstrumentField, MdApi, MdSpi};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

fn cstr(b: &[std::os::raw::c_char]) -> String {
    let bytes: Vec<u8> = b.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

enum Ev {
    Connected,
    Disconnected(i32),
    Login(i32, String),
    Sub(String, i32),
    Tick(RawTick),
}

struct Spi(Sender<Ev>);

impl MdSpi for Spi {
    fn on_front_connected(&mut self) {
        let _ = self.0.send(Ev::Connected);
    }
    fn on_front_disconnected(&mut self, n_reason: i32) {
        let _ = self.0.send(Ev::Disconnected(n_reason));
    }
    fn on_rsp_user_login(&mut self, _l: Option<&CThostFtdcRspUserLoginField>, info: Option<&CThostFtdcRspInfoField>, _id: i32, _last: bool) {
        let (c, m) = info.map(|i| (i.ErrorID, cstr(&i.ErrorMsg))).unwrap_or((0, String::new()));
        let _ = self.0.send(Ev::Login(c, m));
    }
    fn on_rsp_sub_market_data(&mut self, ins: Option<&CThostFtdcSpecificInstrumentField>, info: Option<&CThostFtdcRspInfoField>, _id: i32, _last: bool) {
        let _ = self.0.send(Ev::Sub(ins.map(|i| cstr(&i.InstrumentID)).unwrap_or_default(), info.map(|i| i.ErrorID).unwrap_or(0)));
    }
    fn on_rtn_depth_market_data(&mut self, d: Option<&CThostFtdcDepthMarketDataField>) {
        if let Some(d) = d {
            let _ = self.0.send(Ev::Tick(RawTick {
                instrument: cstr(&d.InstrumentID),
                trading_day: cstr(&d.TradingDay),
                action_day: cstr(&d.ActionDay),
                update_time: cstr(&d.UpdateTime),
                millis: d.UpdateMillisec,
                last: d.LastPrice,
                volume: d.Volume as f64,
                open_interest: d.OpenInterest,
                bid1: d.BidPrice1,
                ask1: d.AskPrice1,
                upper_limit: d.UpperLimitPrice,
                lower_limit: d.LowerLimitPrice,
            }));
        }
    }
}

pub struct CtpMdFeed {
    lib: String,
    api: Option<MdApi>,
    rx: Option<Receiver<Ev>>,
    cfg: Option<MdConfig>,
    req: i32,
}

// SAFETY: the CTP MdApi is documented thread-safe; we only call it from the
// thread that owns the feed, and callbacks only touch the channel sender.
unsafe impl Send for CtpMdFeed {}

impl CtpMdFeed {
    pub fn from_env() -> Result<Self> {
        let lib = std::env::var("CMSQ_CTP_MD_LIB").map_err(|_| inv("set CMSQ_CTP_MD_LIB to the CTP/openctp market-data library (thostmduserapi_se.so)"))?;
        if !std::path::Path::new(&lib).is_file() {
            return Err(inv(format!("CMSQ_CTP_MD_LIB={lib} is not a file")));
        }
        Ok(Self { lib, api: None, rx: None, cfg: None, req: 0 })
    }
}

impl MdFeed for CtpMdFeed {
    fn name(&self) -> &str {
        "ctp2rs"
    }
    fn start(&mut self, cfg: &MdConfig) -> Result<()> {
        let flow = std::env::temp_dir().join(format!("cmsq-ctp-flow-{}/", std::process::id()));
        std::fs::create_dir_all(&flow)?;
        let lib = self.lib.clone();
        let flow_s = flow.display().to_string();
        // ctp2rs panics if the library cannot be loaded; turn that into an error.
        let api = std::panic::catch_unwind(move || MdApi::create_api(&lib, &flow_s, false, false, false))
            .map_err(|_| inv(format!("could not load CTP market-data library {}", self.lib)))?;
        let (tx, rx) = channel();
        // leaked on purpose: must outlive the api's callback threads
        api.register_spi(Box::into_raw(Box::new(Spi(tx))));
        api.register_front(&cfg.front);
        api.init();
        self.api = Some(api);
        self.rx = Some(rx);
        self.cfg = Some(cfg.clone());
        Ok(())
    }
    fn next(&mut self, timeout: Duration) -> Option<FeedEvent> {
        let ev = self.rx.as_ref()?.recv_timeout(timeout).ok()?;
        let (api, cfg) = (self.api.as_ref()?, self.cfg.as_ref()?);
        Some(match ev {
            Ev::Tick(t) => FeedEvent::Tick(t),
            Ev::Connected => {
                let mut f: CThostFtdcReqUserLoginField = unsafe { std::mem::zeroed() };
                ctp2rs::ffi::copy_str_to_i8_array(&mut f.BrokerID, &cfg.broker_id);
                self.req += 1;
                let rc = api.req_user_login(&mut f, self.req);
                FeedEvent::Status(format!("front connected; login requested (rc={rc})"))
            }
            Ev::Login(0, _) => {
                let rc = api.subscribe_market_data(&cfg.instruments);
                FeedEvent::Status(format!("logged in; subscribed {:?} (rc={rc})", cfg.instruments))
            }
            Ev::Login(c, m) => FeedEvent::Status(format!("login failed: {c} {m}")),
            Ev::Sub(i, c) => FeedEvent::Status(format!("subscribe {i}: {}", if c == 0 { "ok".into() } else { format!("error {c}") })),
            Ev::Disconnected(r) => FeedEvent::Status(format!("front disconnected (reason {r:#x})")),
        })
    }
}

impl Drop for CtpMdFeed {
    fn drop(&mut self) {
        if let Some(a) = self.api.take() {
            a.release();
            std::mem::forget(a);
        }
    }
}
