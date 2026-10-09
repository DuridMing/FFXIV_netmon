//! 背景量測執行緒：每 2 秒量測各層延遲，透過 channel 把結果送給介面。

use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

use crate::detector::{self, GameEvent, GameTracker};
use crate::stats::{Summary, Window};
use crate::win::icmp::{EchoResult, Icmp};
use crate::win::{route, time};

pub const INTERVAL: Duration = Duration::from_secs(2);
const TIMEOUT: Duration = Duration::from_millis(1000);
const INTERNET_TARGET: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);
/// 統計視窗：30 次 × 2 秒 = 60 秒
pub const WINDOW_SIZE: usize = 30;
const MAX_TRACE_HOPS: u8 = 8;

pub const LAYER_NAMES: [&str; 4] = ["路由器", "ISP", "外部網路", "遊戲伺服器"];
const GAME: usize = 3;

pub enum GameStatus {
    NotRunning,
    /// 遊戲開著但沒有連線（例如還在標題畫面）
    NoConnection,
    Connected(SocketAddrV4),
    Manual(SocketAddrV4),
}

pub struct LayerReport {
    pub target: Option<String>,
    /// 本輪結果：None = 沒有目標，Some(None) = 逾時
    pub last: Option<Option<u32>>,
    pub summary: Option<Summary>,
}

pub enum MonitorMsg {
    /// 啟動進度
    Status(String),
    Round {
        /// 從監測開始經過的秒數，給圖表當 X 軸
        elapsed: f64,
        game: GameStatus,
        layers: Vec<LayerReport>,
    },
    Event {
        time: String,
        text: String,
        severe: bool,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum Probe {
    Icmp(Ipv4Addr),
    /// 遊戲伺服器常擋 ICMP，改量 TCP handshake 時間
    Tcp(SocketAddrV4),
}

impl Probe {
    /// 回傳延遲毫秒數；逾時回傳 None
    fn run(self) -> Option<u32> {
        match self {
            Probe::Icmp(ip) => match Icmp::new().ok()?.echo(ip, None, TIMEOUT) {
                EchoResult::Reply { rtt_ms } => Some(rtt_ms),
                _ => None,
            },
            Probe::Tcp(addr) => {
                let start = Instant::now();
                // 只做 handshake，連上後立刻關閉，不送任何資料
                TcpStream::connect_timeout(&addr.into(), TIMEOUT)
                    .ok()
                    .map(|_| start.elapsed().as_millis() as u32)
            }
        }
    }

    fn target(self) -> String {
        match self {
            Probe::Icmp(ip) => ip.to_string(),
            Probe::Tcp(addr) => addr.to_string(),
        }
    }
}

struct Layer {
    probe: Option<Probe>,
    window: Window,
}

impl Layer {
    fn new(probe: Option<Probe>) -> Self {
        Self {
            probe,
            window: Window::new(WINDOW_SIZE),
        }
    }

    fn set_probe(&mut self, probe: Option<Probe>) {
        if probe != self.probe {
            self.probe = probe;
            self.window.clear();
        }
    }
}

/// 啟動背景量測。每次送出訊息後會呼叫 `wake` 通知介面重繪。
pub fn spawn(
    manual_target: Option<SocketAddrV4>,
    wake: impl Fn() + Send + 'static,
) -> Receiver<MonitorMsg> {
    let (tx, rx) = crossbeam_channel::unbounded();
    thread::spawn(move || {
        let send = |msg| {
            // 介面關閉後 channel 會斷開，此時直接結束執行緒
            if tx.send(msg).is_err() {
                return false;
            }
            wake();
            true
        };
        run(manual_target, &send);
    });
    rx
}

fn event(text: impl Into<String>, severe: bool) -> MonitorMsg {
    MonitorMsg::Event {
        time: time::now_hms(),
        text: text.into(),
        severe,
    }
}

fn run(manual_target: Option<SocketAddrV4>, send: &dyn Fn(MonitorMsg) -> bool) {
    send(MonitorMsg::Status("尋找路由器...".into()));
    let gateway = route::next_hop(INTERNET_TARGET);
    send(event(
        match gateway {
            Some(g) => format!("路由器：{g}"),
            None => "找不到預設閘道".into(),
        },
        gateway.is_none(),
    ));

    send(MonitorMsg::Status(format!(
        "追蹤路由到 {INTERNET_TARGET}，尋找 ISP 節點..."
    )));
    let isp = find_isp_hop(gateway);
    send(event(
        match isp {
            Some(h) => format!("ISP 節點：{h}"),
            None => "找不到 ISP 節點，略過這一層".into(),
        },
        false,
    ));

    let mut layers = [
        Layer::new(gateway.map(Probe::Icmp)),
        Layer::new(isp.map(Probe::Icmp)),
        Layer::new(Some(Probe::Icmp(INTERNET_TARGET))),
        Layer::new(manual_target.map(Probe::Tcp)),
    ];

    send(MonitorMsg::Status(String::new()));
    if let Some(t) = manual_target {
        send(event(format!("遊戲伺服器：手動指定 {t}"), false));
    }

    let started = Instant::now();
    let mut tracker = GameTracker::default();
    loop {
        let round_start = Instant::now();

        if manual_target.is_none() {
            for ev in tracker.update(&detector::scan()) {
                let (text, severe) = match ev {
                    GameEvent::Started => ("偵測到遊戲程序".to_string(), false),
                    GameEvent::Exited => ("遊戲已關閉".to_string(), false),
                    GameEvent::Connected(t) => (format!("遊戲伺服器：{t}"), false),
                    GameEvent::TargetChanged(t) => (format!("原連線已結束，改監測 {t}"), false),
                    GameEvent::AllConnectionsLost => {
                        (format!("遊戲連線全部中斷！{}", loss_brief(&layers)), true)
                    }
                };
                if !send(event(text, severe)) {
                    return;
                }
            }
            layers[GAME].set_probe(tracker.target().map(Probe::Tcp));
        }

        // 各層同時量測，一輪最多花 TIMEOUT 的時間
        let results: Vec<Option<Option<u32>>> = thread::scope(|s| {
            let handles: Vec<_> = layers
                .iter()
                .map(|l| l.probe.map(|p| s.spawn(move || p.run())))
                .collect();
            handles
                .into_iter()
                .map(|h| h.map(|h| h.join().unwrap_or(None)))
                .collect()
        });

        let reports = layers
            .iter_mut()
            .zip(results)
            .map(|(layer, last)| {
                if let Some(r) = last {
                    layer.window.push(r);
                }
                LayerReport {
                    target: layer.probe.map(Probe::target),
                    last,
                    summary: layer.window.summary(),
                }
            })
            .collect();

        let game = match (manual_target, tracker.target()) {
            (Some(t), _) => GameStatus::Manual(t),
            (None, Some(t)) => GameStatus::Connected(t),
            (None, None) if tracker.running() => GameStatus::NoConnection,
            (None, None) => GameStatus::NotRunning,
        };

        if !send(MonitorMsg::Round {
            elapsed: started.elapsed().as_secs_f64(),
            game,
            layers: reports,
        }) {
            return;
        }

        thread::sleep(INTERVAL.saturating_sub(round_start.elapsed()));
    }
}

/// 斷線事件附帶的簡短數據：各層最近 60 秒的掉包率
fn loss_brief(layers: &[Layer]) -> String {
    let parts: Vec<String> = layers
        .iter()
        .zip(LAYER_NAMES)
        .filter_map(|(l, name)| {
            l.window
                .summary()
                .map(|s| format!("{name} {:.0}%", s.loss_pct))
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("最近 60 秒掉包：{}", parts.join("、"))
    }
}

/// traceroute，回傳路由器之後第一個公開 IP 的節點；找不到就退而求其次用第一個私有 IP 節點
fn find_isp_hop(gateway: Option<Ipv4Addr>) -> Option<Ipv4Addr> {
    let icmp = Icmp::new().ok()?;
    let mut fallback = None;
    for ttl in 1..=MAX_TRACE_HOPS {
        let hop = match icmp.echo(INTERNET_TARGET, Some(ttl), TIMEOUT) {
            EchoResult::TtlExpired { from } => from,
            EchoResult::Reply { .. } => break,
            EchoResult::Timeout => continue,
        };
        if Some(hop) == gateway {
            continue;
        }
        if !hop.is_private() {
            return Some(hop);
        }
        fallback.get_or_insert(hop);
    }
    fallback
}
