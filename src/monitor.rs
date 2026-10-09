//! 背景量測執行緒：每 2 秒量測各層延遲，透過 channel 把結果送給介面。

use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

use crate::detector::{self, GameEvent, GameTracker};
use crate::diagnosis::{self, LayerState};
use crate::events::{EventDetector, IncidentKind, RECENT};
use crate::game_tcp::{GameTcpReport, GameTcpTracker, TcpStatus};
use crate::hops::{HopReport, HopTracker};
use crate::stats::{Summary, Window};
use crate::storage::{self, Sample, Storage};
use crate::trace::{self, Trace};
use crate::win::elevation;
use crate::win::icmp::{EchoResult, Icmp};
use crate::win::wlan::{WifiInfo, Wlan};
use crate::win::{route, time};

pub const INTERVAL: Duration = Duration::from_secs(2);
const TIMEOUT: Duration = Duration::from_millis(1000);
const INTERNET_TARGET: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);
/// 統計視窗：30 次 × 2 秒 = 60 秒
pub const WINDOW_SIZE: usize = 30;
/// 找 ISP 節點只需要看前幾跳
const ISP_TRACE_HOPS: u8 = 8;
const GAME_TRACE_HOPS: u8 = 20;

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
        /// None 代表沒有連 Wi-Fi（有線網路或沒有無線網卡）
        wifi: Option<WifiInfo>,
        game: GameStatus,
        layers: Vec<LayerReport>,
        /// 遊戲連線的 TCP 統計
        tcp: TcpStatus,
        /// 中間節點追蹤的目標與各跳狀態；沒有遊戲目標時是空的
        hop_target: Option<Ipv4Addr>,
        hops: Vec<HopReport>,
        /// 持續掉包的起點：(跳數, 位址)
        hop_loss_origin: Option<(u8, Option<Ipv4Addr>)>,
    },
    Event {
        time: String,
        text: String,
        severe: bool,
    },
    Incident(Incident),
}

pub struct Incident {
    pub time: String,
    pub title: String,
    pub diagnosis: String,
    /// 各層數據與 traceroute，每行一筆
    pub details: Vec<String>,
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

    fn state(&self) -> LayerState {
        LayerState {
            has_probe: self.probe.is_some(),
            last: self.window.last(),
            recent: self.window.summary_last(RECENT),
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
    let mut storage = match Storage::open() {
        Ok(s) => {
            if let Some(path) = storage::db_path() {
                send(event(format!("記錄檔：{}", path.display()), false));
            }
            Some(s)
        }
        Err(e) => {
            send(event(format!("無法開啟記錄檔，這次不會存檔：{e}"), true));
            None
        }
    };

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
    let mut detector = EventDetector::default();
    // 斷線後連線表裡就沒有伺服器了，traceroute 要用最後一次看到的位址
    let mut last_game_target = manual_target;
    let wlan = Wlan::open();
    let mut last_wifi: Option<WifiInfo> = None;
    let elevated = elevation::is_elevated();
    let mut game_tcp = GameTcpTracker::default();
    let mut hop_tracker = HopTracker::default();
    loop {
        let round_start = Instant::now();

        let mut game_disconnected = false;
        // 斷線時遊戲層的視窗會被清掉，先留下斷線前的狀態給診斷用
        let mut game_before = None;
        let mut game_conn = None;
        if manual_target.is_none() {
            let snapshot = detector::scan();
            for ev in tracker.update(&snapshot) {
                let (text, severe) = match ev {
                    GameEvent::Started => ("偵測到遊戲程序".to_string(), false),
                    GameEvent::Exited => ("遊戲已關閉".to_string(), false),
                    GameEvent::Connected(t) => (format!("遊戲伺服器：{t}"), false),
                    GameEvent::TargetChanged(t) => (format!("原連線已結束，改監測 {t}"), false),
                    GameEvent::AllConnectionsLost => {
                        game_disconnected = true;
                        game_before = Some(layers[GAME].state());
                        continue;
                    }
                };
                if !send(event(text, severe)) {
                    return;
                }
            }
            layers[GAME].set_probe(tracker.target().map(Probe::Tcp));
            if let Some(t) = tracker.target() {
                last_game_target = Some(t);
                game_conn = snapshot.conn_to(t);
            }
        }
        // 中間節點追蹤跟著目前的遊戲伺服器（手動目標也算）
        hop_tracker.set_target(manual_target.or(tracker.target()).map(|t| *t.ip()));

        // 各層和中間節點同時量測，一輪最多花 TIMEOUT 的時間
        let hop_probe = hop_tracker.target().map(|t| (t, hop_tracker.probe_hops()));
        let (results, hop_trace): (Vec<Option<Option<u32>>>, Option<Trace>) = thread::scope(|s| {
            let hop_handle = hop_probe.map(|(t, n)| s.spawn(move || trace::traceroute(t, n)));
            let handles: Vec<_> = layers
                .iter()
                .map(|l| l.probe.map(|p| s.spawn(move || p.run())))
                .collect();
            let results = handles
                .into_iter()
                .map(|h| h.map(|h| h.join().unwrap_or(None)))
                .collect();
            (results, hop_handle.and_then(|h| h.join().ok()))
        });
        let hop_minute = hop_trace.as_ref().and_then(|t| hop_tracker.record(t));
        let tcp = game_tcp.update(game_conn, elevated);
        let tcp_report = match tcp {
            TcpStatus::Stats(r) => Some(r),
            _ => None,
        };

        let wifi = wlan.as_ref().and_then(Wlan::current);
        let wifi_event = match (&last_wifi, &wifi) {
            (None, Some(w)) => Some((format!("Wi-Fi：{}，訊號 {}%", w.ssid, w.quality), false)),
            (Some(old), None) => Some((format!("Wi-Fi 連線中斷（{}）", old.ssid), true)),
            (Some(old), Some(w)) if old.ssid != w.ssid => {
                Some((format!("Wi-Fi 換成 {}，訊號 {}%", w.ssid, w.quality), false))
            }
            _ => None,
        };
        if let Some((text, severe)) = wifi_event
            && !send(event(text, severe))
        {
            return;
        }
        last_wifi = wifi.clone();

        let ts_ms = storage::now_ms();
        let targets: Vec<Option<String>> =
            layers.iter().map(|l| l.probe.map(Probe::target)).collect();
        for (layer, last) in layers.iter_mut().zip(&results) {
            if let Some(r) = *last {
                layer.window.push(r);
            }
        }

        if let Some(db) = &mut storage {
            let samples: Vec<Sample> = targets
                .iter()
                .zip(&results)
                .enumerate()
                .filter_map(|(i, (target, last))| {
                    Some(Sample {
                        layer: i,
                        target: target.as_deref()?,
                        rtt_ms: (*last)?,
                    })
                })
                .collect();
            let result = db
                .record_round(ts_ms, &samples, wifi.as_ref())
                .and_then(|()| match (tcp_report, last_game_target) {
                    (Some(r), Some(t)) => db.record_tcp(ts_ms, &t.to_string(), &r),
                    _ => Ok(()),
                })
                .and_then(|()| match (&hop_minute, hop_tracker.target()) {
                    (Some(agg), Some(t)) => db.record_hops(ts_ms, &t.to_string(), agg),
                    _ => Ok(()),
                });
            if let Err(e) = result {
                send(event(format!("寫入記錄檔失敗，停止存檔：{e}"), true));
                storage = None;
            }
        }

        let windows = [0, 1, 2, 3].map(|i| &layers[i].window);
        let hop_reports = hop_tracker.reports();
        let hop_loss_origin = hop_tracker.loss_origin();
        if let Some(kind) = detector.check(windows, game_disconnected, tcp_report.as_ref()) {
            let mut states = [0, 1, 2, 3].map(|i| layers[i].state());
            if let Some(before) = game_before {
                states[GAME] = before;
            }
            let extra = IncidentExtra {
                wifi: wifi.as_ref(),
                tcp: tcp_report,
                hops: &hop_reports,
                hop_loss_origin,
            };
            let incident = build_incident(kind, &states, &targets, last_game_target, &extra);
            if let Some(db) = &storage {
                // 事件寫入失敗不影響監測；資料庫真的壞了，下一輪寫樣本時會回報
                let _ = db.record_incident(
                    ts_ms,
                    &kind.code(),
                    &incident.title,
                    &incident.diagnosis,
                    &incident.details.join("\n"),
                );
            }
            if !send(MonitorMsg::Incident(incident)) {
                return;
            }
        }

        let reports = layers
            .iter()
            .zip(targets)
            .zip(results)
            .map(|((layer, target), last)| LayerReport {
                target,
                last,
                summary: layer.window.summary(),
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
            wifi,
            game,
            layers: reports,
            tcp,
            hop_target: hop_tracker.target(),
            hops: hop_reports,
            hop_loss_origin,
        }) {
            return;
        }

        thread::sleep(INTERVAL.saturating_sub(round_start.elapsed()));
    }
}

/// 事件發生當下的其他資訊，寫進事件詳情、也給診斷參考
struct IncidentExtra<'a> {
    wifi: Option<&'a WifiInfo>,
    tcp: Option<GameTcpReport>,
    hops: &'a [HopReport],
    hop_loss_origin: Option<(u8, Option<Ipv4Addr>)>,
}

fn build_incident(
    kind: IncidentKind,
    states: &[LayerState; 4],
    targets: &[Option<String>],
    game_target: Option<SocketAddrV4>,
    extra: &IncidentExtra,
) -> Incident {
    let wifi = extra.wifi;
    let involves_game = matches!(
        kind,
        IncidentKind::GameDisconnected
            | IncidentKind::GameUnreachable
            | IncidentKind::HighLoss(GAME)
            | IncidentKind::LatencySpike(GAME)
            | IncidentKind::GameRetransmits
            | IncidentKind::GameRtoTimeout
    );
    let trace = game_target
        .filter(|_| involves_game)
        .map(|t| trace::traceroute(*t.ip(), GAME_TRACE_HOPS));

    let mut details: Vec<String> = states
        .iter()
        .enumerate()
        .map(|(i, s)| layer_detail(i, s, targets.get(i).cloned().flatten()))
        .collect();
    details.push(match wifi {
        Some(w) => format!("Wi-Fi：{}，訊號 {}%", w.ssid, w.quality),
        None => "Wi-Fi：沒有使用（有線網路）".into(),
    });
    if let Some(t) = &extra.tcp {
        details.push(format!(
            "遊戲連線 TCP：實際延遲 {} ms（變動 {} ms），近 20 秒重傳 {} 個封包、逾時 {} 次，近 60 秒重傳 {} 個、逾時 {} 次",
            t.smoothed_rtt_ms,
            t.rtt_var_ms,
            t.retrans_recent,
            t.timeouts_recent,
            t.retrans_60s,
            t.timeouts_60s
        ));
    }
    if !extra.hops.is_empty() {
        details.push("中間節點（近 60 秒）：".into());
        details.extend(hop_lines(extra.hops));
    }
    if let (Some(t), Some(target)) = (&trace, game_target) {
        details.push(format!("traceroute 到 {}：", target.ip()));
        details.extend(trace_lines(t));
    }

    let ctx = diagnosis::Context {
        trace: trace.as_ref(),
        wifi_quality: wifi.map(|w| w.quality),
        hop_loss_origin: extra.hop_loss_origin,
        retrans_recent: extra.tcp.map(|t| t.retrans_recent),
    };
    Incident {
        time: time::now_hms(),
        title: kind.title(),
        diagnosis: diagnosis::diagnose(kind, states, &ctx),
        details,
    }
}

fn layer_detail(i: usize, s: &LayerState, target: Option<String>) -> String {
    let name = LAYER_NAMES[i];
    if !s.has_probe && s.recent.is_none() {
        return format!("{name}：沒有目標");
    }
    let target = target.map(|t| format!("（{t}）")).unwrap_or_default();
    let last = match s.last {
        Some(Some(ms)) => format!("{ms} ms"),
        Some(None) => "逾時".into(),
        None => "-".into(),
    };
    match s.recent {
        Some(r) => format!(
            "{name}{target}：最近一次 {last}，近 20 秒平均 {}、掉包 {:.0}%",
            r.avg_ms.map_or("-".into(), |v| format!("{v:.1} ms")),
            r.loss_pct
        ),
        None => format!("{name}{target}：最近一次 {last}"),
    }
}

fn hop_lines(hops: &[HopReport]) -> Vec<String> {
    hops.iter()
        .map(|h| {
            let addr = h.addr.map_or("*".to_string(), |a| a.to_string());
            match h.summary {
                Some(s) => format!(
                    "  {:>2}  {addr}  平均 {}、掉包 {:.0}%",
                    h.ttl,
                    s.avg_ms.map_or("-".into(), |v| format!("{v:.1} ms")),
                    s.loss_pct
                ),
                None => format!("  {:>2}  {addr}", h.ttl),
            }
        })
        .collect()
}

fn trace_lines(t: &Trace) -> Vec<String> {
    t.hops
        .iter()
        .map(|h| match (h.addr, h.rtt_ms) {
            (Some(a), Some(ms)) => format!("  {:>2}  {a}  {ms} ms", h.ttl),
            _ => format!("  {:>2}  *", h.ttl),
        })
        .collect()
}

/// traceroute，回傳路由器之後第一個公開 IP 的節點；找不到就退而求其次用第一個私有 IP 節點
fn find_isp_hop(gateway: Option<Ipv4Addr>) -> Option<Ipv4Addr> {
    let hops = trace::traceroute(INTERNET_TARGET, ISP_TRACE_HOPS).hops;
    let candidates: Vec<Ipv4Addr> = hops
        .iter()
        .filter_map(|h| h.addr)
        .filter(|&a| Some(a) != gateway && a != INTERNET_TARGET)
        .collect();
    candidates
        .iter()
        .find(|a| !a.is_private())
        .or(candidates.first())
        .copied()
}
