//! 背景量測執行緒：每 2 秒量測各層延遲，透過 channel 把結果送給介面。

use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use crate::detector::{GameEvent, GameScanner, GameSnapshot, GameTracker};
use crate::diagnosis::{self, LayerState};
use crate::events::{EventDetector, IncidentKind, RECENT, RECENT_SECS};
use crate::game_tcp::{GameTcpReport, GameTcpTracker, TcpStatus};
use crate::hops::{self, HopReport, HopTracker};
use crate::network::NetworkWatcher;
use crate::stats::{Summary, Window, fmt_ms, fmt_rtt};
use crate::storage::{self, IncidentRecord, RoundRecord, Sample, Storage};
use crate::trace::{self, Trace};
use crate::wifi::{WifiStatus, WifiWatcher};
use crate::win::elevation;
use crate::win::icmp::{EchoResult, Icmp};
use crate::win::netif::{IfKind, NetIf};
use crate::win::time;

pub const INTERVAL: Duration = Duration::from_secs(2);
/// 每次量測（ping、TCP 連線、traceroute 每一跳）最多等多久
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(1000);
pub const INTERNET_TARGET: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);
/// 統計視窗：30 次 × 2 秒 = 60 秒（各層、中間節點、遊戲連線 TCP 統計共用）
pub const WINDOW_SIZE: usize = 30;
/// 統計視窗的秒數，顯示「近 N 秒」用
pub const WINDOW_SECS: u64 = WINDOW_SIZE as u64 * INTERVAL.as_secs();
/// 讀不到遊戲連線表時，最多沿用上一輪的結果幾次（3 × 2 秒 = 6 秒），之後才當作遊戲沒有連線
const MAX_SCAN_FAILURES: u32 = 3;
/// 每幾輪寫一次記錄檔（30 × 2 秒 = 1 分鐘）；發生異常事件時立刻寫
const FLUSH_ROUNDS: usize = 30;
/// 寫入失敗時記憶體裡最多留幾輪（900 × 2 秒 = 30 分鐘），超過就丟掉最舊的
const MAX_BUFFERED_ROUNDS: usize = 900;
/// 每幾輪清一次超過保留天數的樣本（1800 × 2 秒 = 1 小時）
const PRUNE_ROUNDS: usize = 1800;
/// 結束程式時，最多等背景執行緒多久（寫入記錄檔、或正在做事件的 traceroute）
const STOP_WAIT: Duration = Duration::from_secs(3);

/// 各層的名稱與索引（索引也是記錄檔裡 samples.layer 的值，改順序要遷移資料庫）
pub const LAYER_NAMES: [&str; 4] = ["路由器", "ISP", "外部網路", "遊戲伺服器"];
pub const GATEWAY: usize = 0;
pub const ISP: usize = 1;
pub const INTERNET: usize = 2;
pub const GAME: usize = 3;

pub enum GameStatus {
    NotRunning,
    /// 遊戲開著但沒有連線（例如還在標題畫面）
    NoConnection,
    Connected(SocketAddrV4),
    Manual(SocketAddrV4),
}

pub struct LayerReport {
    pub target: Option<String>,
    /// 本輪結果：None = 沒有目標或無法量測，Some(None) = 逾時
    pub last: Option<Option<u32>>,
    /// 本機無法送出量測（例如被防火牆擋住），跟網路逾時不同，不算掉包
    pub failed: bool,
    pub summary: Option<Summary>,
}

/// 每一輪的量測結果
pub struct Round {
    /// 從監測開始經過的秒數，給圖表當 X 軸
    pub elapsed: f64,
    pub wifi: WifiStatus,
    pub game: GameStatus,
    pub layers: Vec<LayerReport>,
    /// 遊戲連線的 TCP 統計
    pub tcp: TcpStatus,
    /// 中間節點追蹤的目標與各跳狀態；沒有遊戲目標時是空的
    pub hop_target: Option<Ipv4Addr>,
    pub hops: Vec<HopReport>,
    /// 持續掉包的起點：(跳數, 位址)
    pub hop_loss_origin: Option<(u8, Option<Ipv4Addr>)>,
    /// 目前上網用的網卡；網路中斷時是最後一張用過的網卡
    pub net_if: Option<NetIf>,
    /// 電腦網路斷線的原因；None 代表網路正常
    pub network_down: Option<String>,
    /// 記錄檔寫入失敗的原因；None 代表正常
    pub storage_error: Option<String>,
}

pub enum MonitorMsg {
    /// 啟動進度
    Status(String),
    Round(Round),
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

/// 背景量測執行緒
pub struct Monitor {
    pub rx: Receiver<MonitorMsg>,
    stop: Sender<()>,
    /// 執行緒結束時會斷開
    done: Receiver<()>,
}

impl Drop for Monitor {
    /// 介面結束（包括畫面出錯要重新啟動）時，通知背景執行緒結束，並等它把還沒寫入的資料寫進記錄檔
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
        let _ = self.done.recv_timeout(STOP_WAIT);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Probe {
    Icmp(Ipv4Addr),
    /// 遊戲伺服器常擋 ICMP，改量 TCP handshake 時間
    Tcp(SocketAddrV4),
}

#[derive(Clone, Copy)]
enum Measured {
    Rtt(u32),
    Lost,
    /// 本機就失敗了（例如防火牆擋住這個程式連線），跟網路無關
    Failed,
}

impl Measured {
    /// 要放進統計和記錄檔的樣本：None = 不算（無法量測），Some(None) = 逾時
    fn sample(self) -> Option<Option<u32>> {
        match self {
            Measured::Rtt(ms) => Some(Some(ms)),
            Measured::Lost => Some(None),
            Measured::Failed => None,
        }
    }
}

impl Probe {
    fn run(self) -> Measured {
        match self {
            Probe::Icmp(ip) => match Icmp::new() {
                Ok(icmp) => match icmp.echo(ip, None, PROBE_TIMEOUT) {
                    EchoResult::Reply { rtt_ms } => Measured::Rtt(rtt_ms),
                    _ => Measured::Lost,
                },
                Err(_) => Measured::Failed,
            },
            Probe::Tcp(addr) => {
                let start = Instant::now();
                // 只做 handshake，連上後立刻關閉，不送任何資料
                match TcpStream::connect_timeout(&addr.into(), PROBE_TIMEOUT) {
                    Ok(_) => Measured::Rtt(start.elapsed().as_millis() as u32),
                    Err(e) if is_local_error(&e) => Measured::Failed,
                    Err(_) => Measured::Lost,
                }
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

/// 連線還沒送出去就失敗：權限被擋（防火牆）、本機資源不足等，不代表網路有問題
fn is_local_error(e: &std::io::Error) -> bool {
    // WSAENOBUFS、WSAEMFILE：本機 socket 資源用完
    matches!(
        e.kind(),
        ErrorKind::PermissionDenied | ErrorKind::AddrNotAvailable | ErrorKind::InvalidInput
    ) || matches!(e.raw_os_error(), Some(10055 | 10024))
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

/// 遊戲連線全部中斷前一刻的狀態。斷線後連線、統計、中間節點都會被清掉，
/// 先留下來給「遊戲連線中斷」事件用
struct Disconnect {
    game: LayerState,
    target: Option<String>,
    tcp: Option<GameTcpReport>,
    hops: Vec<HopReport>,
    hop_loss_origin: Option<(u8, Option<Ipv4Addr>)>,
}

/// 記錄檔寫入：每輪的資料先放在記憶體，每分鐘（或發生事件時）用一個交易寫入，減少硬碟寫入量。
/// 寫入失敗時資料留在記憶體，下次重新開啟記錄檔再試
struct Recorder {
    storage: Option<Storage>,
    buffer: Vec<RoundRecord>,
    /// 目前寫入失敗的原因
    error: Option<String>,
    rounds_since_prune: usize,
}

impl Recorder {
    fn open() -> (Self, (String, bool)) {
        let (storage, error, event) = match Storage::open() {
            Ok(s) => {
                let path = storage::db_path().map_or(String::new(), |p| p.display().to_string());
                (Some(s), None, (format!("記錄檔：{path}"), false))
            }
            Err(e) => (
                None,
                Some(e.clone()),
                (format!("無法開啟記錄檔，每分鐘會再試一次：{e}"), true),
            ),
        };
        let recorder = Self {
            storage,
            buffer: Vec::new(),
            error,
            rounds_since_prune: 0,
        };
        (recorder, event)
    }

    /// 加入一輪資料；累積夠多或 `now` 為 true 時寫入。回傳要顯示的訊息
    fn push(&mut self, record: RoundRecord, now: bool) -> Option<(String, bool)> {
        self.buffer.push(record);
        self.rounds_since_prune += 1;
        if now || self.buffer.len() >= FLUSH_ROUNDS {
            self.flush()
        } else {
            None
        }
    }

    /// 寫入記憶體裡的資料。寫入失敗或恢復時回傳要顯示的訊息
    fn flush(&mut self) -> Option<(String, bool)> {
        if self.buffer.is_empty() {
            return None;
        }
        match self.try_flush() {
            Ok(()) => {
                self.buffer.clear();
                self.error
                    .take()
                    .map(|_| ("記錄檔恢復寫入".to_string(), false))
            }
            Err(e) => {
                // 下次重新開啟記錄檔再試
                self.storage = None;
                if self.buffer.len() > MAX_BUFFERED_ROUNDS {
                    let excess = self.buffer.len() - MAX_BUFFERED_ROUNDS;
                    self.buffer.drain(..excess);
                }
                let first = self.error.is_none();
                self.error = Some(e.clone());
                first.then(|| {
                    (
                        format!("寫入記錄檔失敗，每分鐘會再試一次（資料先暫存在記憶體）：{e}"),
                        true,
                    )
                })
            }
        }
    }

    fn try_flush(&mut self) -> Result<(), String> {
        if self.storage.is_none() {
            self.storage = Some(Storage::open()?);
        }
        let Some(db) = self.storage.as_mut() else {
            return Ok(());
        };
        db.write_rounds(&self.buffer).map_err(|e| e.to_string())?;
        // 程式常駐好幾天時也要清掉超過保留天數的樣本；清除失敗不影響寫入
        if self.rounds_since_prune >= PRUNE_ROUNDS {
            self.rounds_since_prune = 0;
            let _ = db.prune();
        }
        Ok(())
    }
}

impl Drop for Recorder {
    /// 程式結束時把還沒寫入的資料寫進去
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// 啟動背景量測。每次送出訊息後會呼叫 `wake` 通知介面重繪。
pub fn spawn(manual_target: Option<SocketAddrV4>, wake: impl Fn() + Send + 'static) -> Monitor {
    let (tx, rx) = crossbeam_channel::unbounded();
    let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);
    let (done_tx, done_rx) = crossbeam_channel::bounded::<()>(0);
    thread::spawn(move || {
        let send = |msg| {
            // 介面關閉後 channel 會斷開，此時直接結束執行緒
            if tx.send(msg).is_err() {
                return false;
            }
            wake();
            true
        };
        run(manual_target, &send, &stop_rx);
        // 執行緒結束（記錄檔已經寫完）才斷開，讓 Monitor::stop 知道可以結束了
        drop(done_tx);
    });
    Monitor {
        rx,
        stop: stop_tx,
        done: done_rx,
    }
}

fn event(text: impl Into<String>, severe: bool) -> MonitorMsg {
    MonitorMsg::Event {
        time: time::now_hms(),
        text: text.into(),
        severe,
    }
}

fn run(
    manual_target: Option<SocketAddrV4>,
    send: &dyn Fn(MonitorMsg) -> bool,
    stop: &Receiver<()>,
) {
    // 函式結束（不論從哪裡 return）時，Recorder 會把剩下的資料寫進記錄檔
    let (mut recorder, (text, severe)) = Recorder::open();
    send(event(text, severe));

    send(MonitorMsg::Status(format!(
        "尋找路由器，並追蹤路由到 {INTERNET_TARGET} 尋找 ISP 節點..."
    )));
    let mut network = NetworkWatcher::detect();
    send(event(
        match network.gateway {
            Some(g) => format!("路由器：{g}"),
            None => "找不到路由器（電腦直接撥號、使用 VPN，或目前沒有網路）".into(),
        },
        false,
    ));
    send(event(
        match network.isp {
            Some(h) => format!("ISP 節點：{h}"),
            None => "找不到 ISP 節點，有網路時會每 30 秒重找一次".into(),
        },
        false,
    ));

    let mut layers = [
        Layer::new(network.gateway.map(Probe::Icmp)),
        Layer::new(network.isp.map(Probe::Icmp)),
        Layer::new(Some(Probe::Icmp(INTERNET_TARGET))),
        Layer::new(manual_target.map(Probe::Tcp)),
    ];

    send(MonitorMsg::Status(String::new()));
    if let Some(t) = manual_target {
        send(event(format!("遊戲伺服器：手動指定 {t}"), false));
    }

    let started = Instant::now();
    let mut scanner = GameScanner::default();
    let mut tracker = GameTracker::default();
    let mut detector = EventDetector::default();
    // 斷線後連線表裡就沒有伺服器了，traceroute 要用最後一次看到的位址
    let mut last_game_target = manual_target;
    let mut last_snapshot = GameSnapshot::default();
    let mut scan_failures = 0;
    let mut wifi_watcher = WifiWatcher::default();
    let mut last_wifi = WifiStatus::NotUsed;
    let elevated = elevation::is_elevated();
    let mut game_tcp = GameTcpTracker::default();
    // 上一輪的遊戲連線 TCP 統計：連線關閉後就讀不到了
    let mut last_tcp_report: Option<GameTcpReport> = None;
    let mut hop_tracker = HopTracker::default();
    let mut disconnect: Option<Disconnect> = None;
    loop {
        let round_start = Instant::now();

        let mut game_disconnected = false;
        let mut game_conn = None;
        if manual_target.is_none() {
            let snapshot = match scanner.scan() {
                Some(s) => {
                    scan_failures = 0;
                    last_snapshot = s.clone();
                    s
                }
                // Windows API 偶爾會暫時失敗，不能直接當成遊戲關閉或斷線
                None if scan_failures < MAX_SCAN_FAILURES => {
                    scan_failures += 1;
                    last_snapshot.clone()
                }
                None => GameSnapshot::default(),
            };
            for ev in tracker.update(&snapshot) {
                let (text, severe) = match ev {
                    GameEvent::Started => ("偵測到遊戲程序".to_string(), false),
                    GameEvent::Exited => ("遊戲已關閉".to_string(), false),
                    GameEvent::Connected(t) => (format!("遊戲伺服器：{t}"), false),
                    GameEvent::TargetChanged(t) => (format!("原連線已結束，改監測 {t}"), false),
                    GameEvent::AllConnectionsLost => {
                        game_disconnected = true;
                        disconnect = Some(Disconnect {
                            game: layers[GAME].state(),
                            target: layers[GAME].probe.map(Probe::target),
                            tcp: last_tcp_report,
                            hops: hop_tracker.reports(),
                            hop_loss_origin: hop_tracker.loss_origin(),
                        });
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
        // 中間節點追蹤跟著目前的遊戲伺服器（手動目標也算）；換目標時舊目標不滿一分鐘的資料也要寫入
        let mut hop_records = Vec::new();
        if let Some((old, agg)) =
            hop_tracker.set_target(manual_target.or(tracker.target()).map(|t| *t.ip()))
        {
            hop_records.push((old.to_string(), agg));
        }

        // 各層和中間節點同時量測，一輪最多花 PROBE_TIMEOUT 的時間
        let hop_probe = hop_tracker.target().map(|t| (t, hop_tracker.probe_hops()));
        let (results, hop_trace): (Vec<Option<Measured>>, Option<Trace>) = thread::scope(|s| {
            let hop_handle = hop_probe.map(|(t, n)| s.spawn(move || trace::traceroute(t, n)));
            let handles: Vec<_> = layers
                .iter()
                .map(|l| l.probe.map(|p| s.spawn(move || p.run())))
                .collect();
            let results = handles
                .into_iter()
                .map(|h| h.map(|h| h.join().unwrap_or(Measured::Failed)))
                .collect();
            (results, hop_handle.and_then(|h| h.join().ok()))
        });

        // 結果要記到量測當時的目標：先寫進統計，之後才更新路由器／ISP 節點
        let ts_ms = storage::now_ms();
        let targets: Vec<Option<String>> =
            layers.iter().map(|l| l.probe.map(Probe::target)).collect();
        for (layer, result) in layers.iter_mut().zip(&results) {
            if let Some(sample) = result.and_then(Measured::sample) {
                layer.window.push(sample);
            }
        }
        let samples: Vec<Sample> = targets
            .iter()
            .zip(&results)
            .enumerate()
            .filter_map(|(i, (target, result))| {
                Some(Sample {
                    layer: i,
                    target: target.clone()?,
                    rtt_ms: result.and_then(Measured::sample)?,
                })
            })
            .collect();

        let game_reachable = matches!(results[GAME], Some(Measured::Rtt(_)));
        if let Some(agg) = hop_trace
            .as_ref()
            .and_then(|t| hop_tracker.record(t, game_reachable))
            && let Some(t) = hop_tracker.target()
        {
            hop_records.push((t.to_string(), agg));
        }
        let tcp = game_tcp.update(game_conn, elevated);
        let tcp_report = match tcp {
            TcpStatus::Stats(r) => Some(r),
            _ => None,
        };
        last_tcp_report = tcp_report;

        let net = network.update();
        for (text, severe) in &net.events {
            if !send(event(text.clone(), *severe)) {
                return;
            }
        }
        if net.targets_changed {
            layers[GATEWAY].set_probe(network.gateway.map(Probe::Icmp));
            layers[ISP].set_probe(network.isp.map(Probe::Icmp));
        }

        let wifi = wifi_watcher.update(net.net_if.as_ref());
        if let Some((text, severe)) = wifi_event(&last_wifi, &wifi)
            && !send(event(text, severe))
        {
            return;
        }
        last_wifi = wifi.clone();

        let windows = layers.each_ref().map(|l| &l.window);
        let hop_reports = hop_tracker.reports();
        let hop_loss_origin = hop_tracker.loss_origin();
        let kind = detector.check(windows, game_disconnected, net.signal, tcp_report.as_ref());
        let mut incident_record = None;
        if let Some(kind) = kind {
            let mut states = layers.each_ref().map(Layer::state);
            let mut targets = targets.clone();
            let mut extra = IncidentExtra {
                net_if: net.net_if.as_ref(),
                network_down: net.down_reason.as_deref(),
                wifi: &wifi,
                tcp: tcp_report,
                hops: &hop_reports,
                hop_loss_origin,
                trace: hop_trace
                    .as_ref()
                    .filter(|_| hop_tracker.target() == last_game_target.map(|t| *t.ip())),
            };
            // 遊戲斷線（或同一輪的網路中斷）：遊戲層和中間節點用斷線前的狀態
            if let Some(d) = disconnect.as_ref().filter(|_| {
                matches!(
                    kind,
                    IncidentKind::GameDisconnected | IncidentKind::NetworkDown
                )
            }) {
                states[GAME] = d.game.clone();
                targets[GAME] = d.target.clone();
                if kind == IncidentKind::GameDisconnected {
                    extra.tcp = d.tcp;
                    extra.hops = &d.hops;
                    extra.hop_loss_origin = d.hop_loss_origin;
                }
            }
            let incident = build_incident(kind, &states, &targets, last_game_target, &extra);
            incident_record = Some(IncidentRecord {
                kind: kind.code(),
                title: incident.title.clone(),
                diagnosis: incident.diagnosis.clone(),
                details: incident.details.join("\n"),
            });
            if !send(MonitorMsg::Incident(incident)) {
                return;
            }
        }
        // 網路中斷那一輪的遊戲斷線會留到下一輪回報，其他情況用完就丟掉
        if kind != Some(IncidentKind::NetworkDown) {
            disconnect = None;
        }

        let flush_now = incident_record.is_some();
        let record = RoundRecord {
            ts_ms,
            samples,
            wifi: wifi.info().cloned(),
            tcp: tcp_report
                .zip(last_game_target)
                .map(|(r, t)| (t.to_string(), r)),
            hops: hop_records,
            incident: incident_record,
        };
        if let Some((text, severe)) = recorder.push(record, flush_now)
            && !send(event(text, severe))
        {
            return;
        }

        let reports = layers
            .iter()
            .zip(targets)
            .zip(results)
            .map(|((layer, target), result)| LayerReport {
                target,
                last: result.and_then(Measured::sample),
                failed: matches!(result, Some(Measured::Failed)),
                summary: layer.window.summary(),
            })
            .collect();

        let game = match (manual_target, tracker.target()) {
            (Some(t), _) => GameStatus::Manual(t),
            (None, Some(t)) => GameStatus::Connected(t),
            (None, None) if tracker.running() => GameStatus::NoConnection,
            (None, None) => GameStatus::NotRunning,
        };

        if !send(MonitorMsg::Round(Round {
            elapsed: started.elapsed().as_secs_f64(),
            wifi,
            game,
            layers: reports,
            tcp,
            hop_target: hop_tracker.target(),
            hops: hop_reports,
            hop_loss_origin,
            net_if: net.net_if,
            network_down: net.down_reason,
            storage_error: recorder.error.clone(),
        })) {
            return;
        }

        // 等到下一輪；程式要結束時會立刻醒來
        match stop.recv_timeout(INTERVAL.saturating_sub(round_start.elapsed())) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => return,
        }
    }
}

/// Wi-Fi 狀態變化時要顯示的訊息
fn wifi_event(old: &WifiStatus, new: &WifiStatus) -> Option<(String, bool)> {
    match (old, new) {
        (WifiStatus::Connected(o), WifiStatus::Connected(w)) => {
            (o.ssid.is_some() && w.ssid.is_some() && o.ssid != w.ssid).then(|| {
                (
                    format!("Wi-Fi 換成 {}，訊號 {}%", w.ssid_text(), w.quality),
                    false,
                )
            })
        }
        (_, WifiStatus::Connected(w)) => Some((
            format!("Wi-Fi：{}，訊號 {}%", w.ssid_text(), w.quality),
            false,
        )),
        (WifiStatus::Connected(o), WifiStatus::Disconnected) => {
            let name = o
                .ssid
                .as_ref()
                .map(|s| format!("（{s}）"))
                .unwrap_or_default();
            Some((format!("Wi-Fi 連線中斷{name}"), true))
        }
        (_, WifiStatus::Unreadable(reason)) if old != new => {
            Some((format!("無法讀取 Wi-Fi 資訊：{reason}"), false))
        }
        _ => None,
    }
}

/// 事件發生當下的其他資訊，寫進事件詳情、也給診斷參考
struct IncidentExtra<'a> {
    net_if: Option<&'a NetIf>,
    /// 電腦網路斷線的原因；None 代表網路正常
    network_down: Option<&'a str>,
    wifi: &'a WifiStatus,
    tcp: Option<GameTcpReport>,
    hops: &'a [HopReport],
    hop_loss_origin: Option<(u8, Option<Ipv4Addr>)>,
    /// 這一輪中間節點追蹤到遊戲伺服器的 traceroute；有的話事件直接用，不再另外追蹤一次
    trace: Option<&'a Trace>,
}

fn build_incident(
    kind: IncidentKind,
    states: &[LayerState; 4],
    targets: &[Option<String>],
    game_target: Option<SocketAddrV4>,
    extra: &IncidentExtra,
) -> Incident {
    let involves_game = matches!(
        kind,
        IncidentKind::GameDisconnected
            | IncidentKind::GameUnreachable
            | IncidentKind::HighLoss(GAME)
            | IncidentKind::LatencySpike(GAME)
            | IncidentKind::GameRetransmits
            | IncidentKind::GameRtoTimeout
    );
    // 斷線時中間節點追蹤已經停了，才需要另外追蹤一次
    let fresh_trace;
    let trace = match (involves_game, extra.trace) {
        (false, _) => None,
        (true, Some(t)) => Some(t),
        (true, None) => {
            fresh_trace = game_target.map(|t| trace::traceroute(*t.ip(), hops::MAX_HOPS));
            fresh_trace.as_ref()
        }
    };

    let mut details: Vec<String> = states
        .iter()
        .enumerate()
        .map(|(i, s)| layer_detail(i, s, targets.get(i).cloned().flatten()))
        .collect();
    if let Some(n) = extra.net_if {
        details.push(format!("網路卡：{}（{}）", n.summary(), n.description));
    }
    if let Some(reason) = extra.network_down {
        details.push(format!("電腦網路：中斷，{reason}"));
    }
    if let Some(line) = wifi_detail(extra.wifi, extra.net_if) {
        details.push(line);
    }
    if let Some(t) = &extra.tcp {
        details.push(format!(
            "遊戲連線 TCP：實際延遲 {} ms（變動 {} ms），近 {RECENT_SECS} 秒重傳 {} 個封包、逾時 {} 次，近 {WINDOW_SECS} 秒重傳 {} 個、逾時 {} 次",
            t.smoothed_rtt_ms,
            t.rtt_var_ms,
            t.retrans_recent,
            t.timeouts_recent,
            t.retrans_window,
            t.timeouts_window
        ));
    }
    if !extra.hops.is_empty() {
        details.push(format!("中間節點（近 {WINDOW_SECS} 秒）："));
        details.extend(hop_lines(extra.hops));
    }
    if let (Some(t), Some(target)) = (trace, game_target) {
        details.push(format!("traceroute 到 {}：", target.ip()));
        details.extend(trace_lines(t));
    }

    let ctx = diagnosis::Context {
        trace,
        wifi_quality: extra.wifi.info().map(|w| w.quality),
        hop_loss_origin: extra.hop_loss_origin,
        net_if: extra.net_if,
        network_down: extra.network_down,
        retrans_recent: extra.tcp.map(|t| t.retrans_recent),
    };
    Incident {
        time: time::now_hms(),
        title: kind.title(),
        diagnosis: diagnosis::diagnose(kind, states, &ctx),
        details,
    }
}

/// 事件詳情的 Wi-Fi 那一行，要跟網路卡的種類一致
fn wifi_detail(wifi: &WifiStatus, net_if: Option<&NetIf>) -> Option<String> {
    match wifi {
        WifiStatus::Connected(w) => Some(format!("Wi-Fi：{}，訊號 {}%", w.ssid_text(), w.quality)),
        WifiStatus::Disconnected => Some("Wi-Fi：沒有連線".into()),
        WifiStatus::Unreadable(reason) => Some(format!("Wi-Fi：無法讀取（{reason}）")),
        WifiStatus::NotUsed => match net_if.map(|n| n.kind) {
            Some(IfKind::Ethernet) => Some("Wi-Fi：沒有使用（有線網路）".into()),
            _ => None,
        },
    }
}

fn layer_detail(i: usize, s: &LayerState, target: Option<String>) -> String {
    let name = LAYER_NAMES[i];
    if !s.has_probe && s.recent.is_none() {
        return format!("{name}：沒有目標");
    }
    let target = target.map(|t| format!("（{t}）")).unwrap_or_default();
    let last = s.last.map_or("-".into(), fmt_rtt);
    match s.recent {
        Some(r) => format!(
            "{name}{target}：最近一次 {last}，近 {RECENT_SECS} 秒平均 {}、掉包 {:.0}%",
            fmt_ms(r.avg_ms),
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
                    fmt_ms(s.avg_ms),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wifi::WifiInfo;

    fn net_if(kind: IfKind) -> NetIf {
        NetIf {
            index: 1,
            guid: 0,
            alias: "x".into(),
            description: "x".into(),
            kind,
            admin_down: false,
            media_connected: kind != IfKind::Wifi,
            oper_up: true,
            speed_bps: 0,
        }
    }

    #[test]
    fn wifi_detail_matches_adapter_kind() {
        let wifi = net_if(IfKind::Wifi);
        let wired = net_if(IfKind::Ethernet);
        assert_eq!(
            wifi_detail(&WifiStatus::Disconnected, Some(&wifi)).as_deref(),
            Some("Wi-Fi：沒有連線")
        );
        assert_eq!(
            wifi_detail(&WifiStatus::NotUsed, Some(&wired)).as_deref(),
            Some("Wi-Fi：沒有使用（有線網路）")
        );
        assert_eq!(
            wifi_detail(&WifiStatus::NotUsed, Some(&net_if(IfKind::Other))),
            None
        );
    }

    #[test]
    fn wifi_events() {
        let a = WifiStatus::Connected(WifiInfo {
            ssid: Some("A".into()),
            quality: 80,
        });
        let hidden = WifiStatus::Connected(WifiInfo {
            ssid: None,
            quality: 80,
        });
        assert!(wifi_event(&WifiStatus::NotUsed, &a).is_some());
        assert!(wifi_event(&a, &a).is_none());
        assert!(wifi_event(&a, &hidden).is_none());
        assert!(wifi_event(&a, &WifiStatus::Disconnected).is_some_and(|(_, severe)| severe));
        // 權限被收回不是斷線
        let denied = WifiStatus::Unreadable(crate::wifi::NEED_LOCATION);
        assert!(wifi_event(&a, &denied).is_some_and(|(_, severe)| !severe));
        assert!(wifi_event(&denied, &denied).is_none());
    }
}
