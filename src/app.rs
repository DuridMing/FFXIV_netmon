//! egui 介面：遊戲狀態、各層狀態表、即時延遲圖、事件列表；系統匣常駐、通知、匯出報告。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::thread;

use crossbeam_channel::Receiver;
use eframe::egui::{self, Color32, RichText};
use egui_plot::{Legend, Line, Plot, PlotBounds, PlotPoints, Points, VLine};

use crate::game_tcp::TcpStatus;
use crate::icon::Health;
use crate::monitor::{
    GAME, GameStatus, Incident, LAYER_NAMES, LayerReport, Monitor, MonitorMsg, Round, WINDOW_SECS,
};
use crate::report::{self, Exported};
use crate::settings::{CloseAction, Settings};
use crate::stats::{fmt_ms, fmt_rtt};
use crate::storage;
use crate::wifi::{self, WEAK_WIFI, WifiStatus};
use crate::win::tray::{Tray, TrayCommand};
use crate::win::{elevation, file_map, shell, time};

/// 圖表保留的量測次數：300 × 2 秒 = 10 分鐘
const HISTORY: usize = 300;
/// 圖表 X 軸顯示的時間長度（秒）
const CHART_SPAN_SECS: f64 = 600.0;
/// 圖表 Y 軸至少顯示到這個值，延遲很低時線才不會貼滿整張圖
const CHART_MIN_Y_MS: f64 = 10.0;
const MAX_EVENTS: usize = 500;
/// 中文字型候選，依序嘗試（都在 Windows 字型資料夾）。
/// 繁中版 Windows 有微軟正黑體；其他語言版本不一定有，就退而求其次用其他含中文字的字型
/// 第三個欄位：這個字型是否涵蓋大部分繁體中文字（日文字型缺很多繁體字）
const FONT_CANDIDATES: [(&str, &str, bool); 6] = [
    ("msjh.ttc", "微軟正黑體", true),
    ("mingliu.ttc", "細明體", true),
    ("msyh.ttc", "微軟雅黑", true),
    // simsun.ttc 的第 0 個字型是「宋體」（新宋體是第 1 個）
    ("simsun.ttc", "宋體", true),
    ("YuGothM.ttc", "Yu Gothic", false),
    ("meiryo.ttc", "Meiryo", false),
];
const LOCATION_HINT: &str = "Windows 11 把讀取 Wi-Fi 資訊當成存取位置。\
    到 Windows 設定 →「隱私權與安全性」→「位置」，開啟「位置服務」和「允許桌面應用程式存取您的位置」就能顯示。";

/// 主畫面下半部的分頁
#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Chart,
    Hops,
}

const GRAY: Color32 = rgb(Health::Unknown);
const YELLOW: Color32 = rgb(Health::Warn);
const RED: Color32 = rgb(Health::Bad);
const LAYER_COLORS: [Color32; 4] = [
    Color32::from_rgb(0x5b, 0x9b, 0xd5),
    Color32::from_rgb(0x9b, 0x7b, 0xd9),
    Color32::from_rgb(0x4d, 0xb6, 0xac),
    Color32::from_rgb(0xf0, 0x8c, 0x3c),
];

const fn rgb(health: Health) -> Color32 {
    let [r, g, b] = health.rgb();
    Color32::from_rgb(r, g, b)
}

struct EventEntry {
    /// 給可展開區塊當唯一 ID，越新的越大
    id: u64,
    kind: EntryKind,
}

enum EntryKind {
    /// 一般訊息
    Plain {
        time: String,
        text: String,
        severe: bool,
    },
    /// 異常事件：有診斷結論與詳細數據，可以展開
    Incident(Incident),
}

pub struct App {
    monitor: Monitor,
    /// 啟動進度，空字串代表已就緒
    status: String,
    /// 最新一輪的量測結果；還沒收到任何一輪時為 None
    round: Option<Round>,
    tab: Tab,
    /// 每層的 (經過秒數, 延遲)；延遲 None 代表逾時
    history: [VecDeque<(f64, Option<u32>)>; 4],
    /// 每一輪的 (經過秒數, 量測時間)，給圖表的滑鼠提示用
    round_times: VecDeque<(f64, String)>,
    /// 監測開始（經過秒數 0）時是一天中的第幾秒，用來把 X 軸換成實際時間
    clock_base_secs: Option<i64>,
    /// 最新的在最前面
    events: VecDeque<EventEntry>,
    /// 事件列表每一筆實際畫出來的高度（依事件 id），只排版看得到的事件時用
    event_heights: HashMap<u64, f32>,
    next_event_id: u64,

    /// 建立系統匣失敗時為 None，此時關閉視窗就直接結束
    tray: Option<Tray>,
    tray_rx: Receiver<TrayCommand>,
    /// 從系統匣選「結束」後才真的關閉；設定成縮到系統匣時，按 X 只會隱藏視窗
    quitting: bool,
    /// 要結束時還在匯出報告或清除紀錄：等做完再結束
    exit_when_idle: bool,
    hide_hint_shown: bool,
    settings: Settings,
    export_rx: Option<Receiver<Result<Exported, String>>>,
    /// 正在顯示「確定要清除紀錄嗎」對話框
    confirm_clear: bool,
    clear_rx: Option<Receiver<Result<(), String>>>,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        monitor: Monitor,
        startup_events: Vec<(String, bool)>,
        tray: Option<Tray>,
        tray_rx: Receiver<TrayCommand>,
    ) -> Self {
        let mut app = Self {
            monitor,
            status: "啟動中...".into(),
            round: None,
            tab: Tab::Chart,
            history: Default::default(),
            round_times: VecDeque::new(),
            clock_base_secs: None,
            events: VecDeque::new(),
            event_heights: HashMap::new(),
            next_event_id: 0,
            tray,
            tray_rx,
            quitting: false,
            exit_when_idle: false,
            hide_hint_shown: false,
            settings: Settings::load(),
            export_rx: None,
            confirm_clear: false,
            clear_rx: None,
        };
        match load_system_font(&cc.egui_ctx) {
            Some(0) => {}
            Some(i) => app.push_event(
                String::new(),
                match FONT_CANDIDATES[i] {
                    (_, name, true) => format!("找不到微軟正黑體，改用{name}"),
                    (_, name, false) => {
                        format!("找不到繁體中文字型，改用{name}，部分繁體中文字可能會顯示成方框")
                    }
                },
                !FONT_CANDIDATES[i].2,
            ),
            None => app.push_event(
                String::new(),
                "找不到可以顯示中文的字型，中文可能會顯示成方框".into(),
                true,
            ),
        }
        if app.tray.is_none() {
            app.push_event(
                String::new(),
                "無法建立系統匣圖示，關閉視窗就會結束程式".into(),
                true,
            );
        }
        for (text, severe) in startup_events {
            app.push_event(String::new(), text, severe);
        }
        app
    }

    fn push_event(&mut self, time: String, text: String, severe: bool) {
        self.push_entry(EntryKind::Plain { time, text, severe });
    }

    fn push_entry(&mut self, kind: EntryKind) {
        self.next_event_id += 1;
        self.events.push_front(EventEntry {
            id: self.next_event_id,
            kind,
        });
        self.events.truncate(MAX_EVENTS);
        // 已經被擠掉的事件不用再記高度（id 越新越大，最後一筆是最舊的）
        if let Some(oldest) = self.events.back().map(|e| e.id) {
            self.event_heights.retain(|&id, _| id >= oldest);
        }
    }

    fn drain_messages(&mut self) {
        while let Ok(msg) = self.monitor.rx.try_recv() {
            match msg {
                MonitorMsg::Status(s) => self.status = s,
                MonitorMsg::Event { time, text, severe } => self.push_event(time, text, severe),
                MonitorMsg::Incident(incident) => {
                    if self.settings.notify
                        && let Some(tray) = &self.tray
                    {
                        tray.notify(&format!("⚠ {}", incident.title), &incident.diagnosis);
                    }
                    self.push_entry(EntryKind::Incident(incident));
                }
                MonitorMsg::Round(round) => {
                    let elapsed = round.elapsed;
                    for (history, report) in self.history.iter_mut().zip(&round.layers) {
                        match report.last {
                            Some(r) => history.push_back((elapsed, r)),
                            // 這一輪無法量測：圖上留空，不要清掉
                            None if report.failed => {}
                            // 目標消失（例如遊戲關閉）時清掉舊的線
                            None => history.clear(),
                        }
                        if history.len() > HISTORY {
                            history.pop_front();
                        }
                    }
                    // 每輪重新對時：執行中調整時間、時區或日光節約時間時，圖表的時間軸才會跟著變
                    self.clock_base_secs = Some(time::now_secs_of_day() - elapsed.round() as i64);
                    self.round_times.push_back((elapsed, time::now_hms()));
                    if self.round_times.len() > HISTORY {
                        self.round_times.pop_front();
                    }
                    self.round = Some(round);
                    self.update_tray();
                }
            }
        }
    }

    fn game(&self) -> Option<&GameStatus> {
        self.round.as_ref().map(|r| &r.game)
    }

    fn layer(&self, i: usize) -> Option<&LayerReport> {
        self.round.as_ref()?.layers.get(i)
    }

    /// 整體燈號：有遊戲目標時看遊戲伺服器那層，否則看遊戲狀態
    fn overall_health(&self) -> Health {
        match self.game() {
            None | Some(GameStatus::NotRunning) => Health::Unknown,
            Some(GameStatus::NoConnection) => Health::Warn,
            Some(GameStatus::Connected(_) | GameStatus::Manual(_)) => self
                .layer(GAME)
                .map_or(Health::Unknown, |r| layer_state(r).0),
        }
    }

    fn update_tray(&self) {
        let Some(tray) = &self.tray else { return };
        let game_layer = self.layer(GAME);
        let game = match (self.game(), game_layer.and_then(|r| r.last)) {
            (Some(GameStatus::Connected(_) | GameStatus::Manual(_)), Some(Some(ms))) => {
                format!("遊戲伺服器 {ms} ms")
            }
            (Some(GameStatus::Connected(_) | GameStatus::Manual(_)), None)
                if game_layer.is_some_and(|r| r.failed) =>
            {
                "遊戲伺服器 無法量測".into()
            }
            (Some(GameStatus::Connected(_) | GameStatus::Manual(_)), _) => "遊戲伺服器 逾時".into(),
            (Some(GameStatus::NoConnection), _) => "遊戲未連線".into(),
            _ => "遊戲未執行".into(),
        };
        let mut tip = format!("FF14 連線監測\n{game}");
        if let Some(w) = self.round.as_ref().and_then(|r| r.wifi.info()) {
            tip.push_str(&format!("\nWi-Fi 訊號 {}%", w.quality));
        }
        tray.set_status(self.overall_health(), &tip);
    }

    fn handle_tray_commands(&mut self, ctx: &egui::Context) {
        while let Ok(cmd) = self.tray_rx.try_recv() {
            match cmd {
                TrayCommand::Show => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                TrayCommand::Exit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    /// 正在匯出報告或清除紀錄（背景執行緒，程式結束的話會被中斷）
    fn busy(&self) -> bool {
        self.export_rx.is_some() || self.clear_rx.is_some()
    }

    /// 處理關閉視窗。要在 logic() 呼叫：視窗最小化時 eframe 不會呼叫 ui()，
    /// 這時從工作列關閉視窗也要照設定縮到系統匣，不能直接結束。
    /// 設定成縮到系統匣時，按 X 只隱藏視窗，繼續在背景監測；否則照常結束
    fn handle_close(&mut self, ctx: &egui::Context) {
        if self.exit_when_idle && !self.busy() {
            self.exit_when_idle = false;
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        let exiting =
            self.quitting || self.tray.is_none() || self.settings.close_action == CloseAction::Exit;
        if exiting {
            if self.busy() {
                // 等匯出或清除做完再結束，不然報告會不完整、清除會被復原
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                if !self.exit_when_idle {
                    self.exit_when_idle = true;
                    self.push_event(
                        time::now_hms(),
                        "正在匯出報告或清除紀錄，完成後會自動結束".into(),
                        false,
                    );
                }
            }
            return;
        }

        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        let Some(tray) = self.tray.filter(Tray::is_shown) else {
            // 系統匣圖示還沒出現（例如工作列忙碌，還在重試）：先縮到工作列，不然視窗會找不回來
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            return;
        };
        // 先最小化再隱藏：只隱藏的話 eframe 仍會每次重繪都畫一個看不見的畫面
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        if !self.hide_hint_shown {
            tray.notify(
                "FF14 連線監測仍在執行",
                "程式已縮到系統匣，會繼續監測。點圖示可以打開視窗，右鍵選「結束」可以關閉。也可以在「設定」改成按 X 直接結束。",
            );
            self.hide_hint_shown = true;
        }
    }

    fn start_export(&mut self, ctx: &egui::Context, hours: u32) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(report::export(hours));
            ctx.request_repaint();
        });
        self.export_rx = Some(rx);
    }

    fn poll_export(&mut self) {
        let Some(rx) = &self.export_rx else { return };
        let Ok(result) = rx.try_recv() else { return };
        self.export_rx = None;
        match result {
            Ok(e) => self.push_event(
                time::now_hms(),
                format!(
                    "已匯出報告：{}（CSV：{}）",
                    e.html.display(),
                    e.csv.display()
                ),
                false,
            ),
            Err(e) => self.push_event(time::now_hms(), format!("匯出失敗：{e}"), true),
        }
    }

    fn start_clear(&mut self, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(storage::clear_all());
            ctx.request_repaint();
        });
        self.clear_rx = Some(rx);
    }

    fn poll_clear(&mut self) {
        let Some(rx) = &self.clear_rx else { return };
        let Ok(result) = rx.try_recv() else { return };
        self.clear_rx = None;
        match result {
            Ok(()) => {
                self.events.clear();
                self.event_heights.clear();
                self.push_event(time::now_hms(), "已清除所有紀錄".into(), false);
            }
            Err(e) => self.push_event(time::now_hms(), format!("清除紀錄失敗：{e}"), true),
        }
    }

    fn save_settings(&mut self) {
        if let Err(e) = self.settings.save() {
            self.push_event(time::now_hms(), format!("無法儲存設定：{e}"), true);
        }
    }

    fn settings_menu(&mut self, ui: &mut egui::Ui) {
        let before = self.settings;
        ui.label(RichText::new(format!("{} v{}", crate::APP_NAME, crate::VERSION)).color(GRAY));
        ui.separator();

        ui.label(RichText::new("按視窗的 X 時").strong());
        ui.add_enabled_ui(self.tray.is_some(), |ui| {
            ui.radio_value(
                &mut self.settings.close_action,
                CloseAction::MinimizeToTray,
                "縮到系統匣，繼續監測",
            );
        });
        ui.radio_value(
            &mut self.settings.close_action,
            CloseAction::Exit,
            "直接結束程式",
        );
        ui.separator();

        ui.add_enabled_ui(self.tray.is_some(), |ui| {
            ui.checkbox(&mut self.settings.notify, "發生異常時跳出通知");
        });
        ui.separator();

        let clearing = self.clear_rx.is_some();
        let label = if clearing {
            "清除中..."
        } else {
            "清除所有紀錄..."
        };
        if ui
            .add_enabled(
                !clearing,
                egui::Button::new(RichText::new(label).color(RED)),
            )
            .clicked()
        {
            self.confirm_clear = true;
            ui.close();
        }

        if self.settings != before {
            self.save_settings();
        }
    }

    fn confirm_clear_ui(&mut self, ctx: &egui::Context) {
        if !self.confirm_clear {
            return;
        }
        let modal = egui::Modal::new(egui::Id::new("confirm_clear")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.heading("清除所有紀錄？");
            ui.add_space(6.0);
            ui.label(
                "會刪除記錄檔裡所有的量測數據、Wi-Fi 紀錄和異常事件，畫面上的事件列表也會清空。",
            );
            ui.label(
                RichText::new("刪除後無法復原。如果之後可能需要當作證據，請先匯出報告。")
                    .color(YELLOW),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("清除").color(RED)).clicked() {
                    self.start_clear(ui.ctx());
                    self.confirm_clear = false;
                }
                if ui.button("取消").clicked() {
                    self.confirm_clear = false;
                }
            });
        });
        if modal.should_close() {
            self.confirm_clear = false;
        }
    }

    fn header_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let text = match self.game() {
                None => "遊戲狀態：等待中".to_string(),
                Some(GameStatus::NotRunning) => "遊戲狀態：未執行".to_string(),
                Some(GameStatus::NoConnection) => "遊戲狀態：未連線".to_string(),
                Some(GameStatus::Connected(t)) => format!("遊戲狀態：已連線　伺服器：{t}"),
                Some(GameStatus::Manual(t)) => format!("手動目標：{t}"),
            };
            ui.label(
                RichText::new("●")
                    .color(rgb(self.overall_health()))
                    .size(18.0),
            );
            ui.label(RichText::new(text).size(16.0));
            if !self.status.is_empty() {
                // 不用轉圈動畫：動畫會讓視窗一直重繪，跟遊戲搶顯示卡
                ui.separator();
                ui.label(RichText::new(&self.status).color(GRAY));
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.export_rx.is_some() {
                    ui.add_enabled(false, egui::Button::new("匯出中..."));
                } else {
                    ui.menu_button("匯出報告", |ui| {
                        ui.label(
                            RichText::new("HTML 報告和 CSV 會存到「文件\\ff14-netmon」")
                                .small()
                                .color(GRAY),
                        );
                        for (hours, label) in report::RANGES {
                            if ui.button(label).clicked() {
                                self.start_export(ui.ctx(), hours);
                                ui.close();
                            }
                        }
                    });
                }
                ui.menu_button("設定", |ui| self.settings_menu(ui));
            });
        });

        if let Some(round) = &self.round {
            ui.horizontal_wrapped(|ui| {
                match (&round.net_if, &round.network_down) {
                    (Some(n), None) => {
                        ui.label(RichText::new(format!("網路：{}", n.summary())).color(GRAY))
                            .on_hover_text(&n.description);
                    }
                    (Some(n), Some(reason)) => {
                        ui.label(
                            RichText::new(format!("網路中斷：{}（{reason}）", n.alias)).color(RED),
                        );
                    }
                    (None, Some(reason)) => {
                        ui.label(RichText::new(format!("網路中斷：{reason}")).color(RED));
                    }
                    (None, None) => {}
                }
                match &round.wifi {
                    WifiStatus::Connected(w) => {
                        let color = if w.quality < WEAK_WIFI { YELLOW } else { GRAY };
                        ui.label(
                            RichText::new(format!("Wi-Fi：{}　訊號 {}%", w.ssid_text(), w.quality))
                                .color(color),
                        );
                        if w.quality < WEAK_WIFI {
                            ui.label(RichText::new("訊號偏弱，建議改用有線網路").color(YELLOW));
                        }
                    }
                    WifiStatus::Unreadable(reason) => {
                        let label = ui.label(
                            RichText::new(format!("Wi-Fi：無法讀取訊號（{reason}）")).color(GRAY),
                        );
                        if *reason == wifi::NEED_LOCATION {
                            label.on_hover_text(LOCATION_HINT);
                        }
                    }
                    WifiStatus::NotUsed | WifiStatus::Disconnected => {}
                }
            });

            if let Some(e) = &round.storage_error {
                ui.label(
                    RichText::new(format!(
                        "記錄檔寫入失敗，資料暫存在記憶體，每分鐘會再試一次：{e}"
                    ))
                    .color(RED),
                );
            }
        }

        if let Some(i) = self.latest_incident() {
            ui.label(
                RichText::new(format!("最近異常 {}：{}　{}", i.time, i.title, i.diagnosis))
                    .color(YELLOW),
            );
        }
    }

    /// 遊戲連線本身的 TCP 統計（需要系統管理員權限）
    fn tcp_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("遊戲連線（TCP）").strong().color(LAYER_COLORS[GAME]));
            match self.round.as_ref().map(|r| r.tcp) {
                None | Some(TcpStatus::NoConnection) => {
                    ui.label(RichText::new("沒有遊戲連線").color(GRAY));
                }
                Some(TcpStatus::NotElevated) => {
                    ui.label(
                        RichText::new("實際延遲和重傳統計需要系統管理員權限").color(GRAY),
                    );
                    let busy = self.busy();
                    if ui
                        .add_enabled(!busy, egui::Button::new("以系統管理員身分重新啟動"))
                        .on_hover_text("會跳出 Windows 的權限確認視窗，按「是」後程式會重新開啟")
                        .on_disabled_hover_text("正在匯出報告或清除紀錄，完成後才能重新啟動")
                        .clicked()
                    {
                        self.restart_as_admin(ui.ctx());
                    }
                }
                Some(TcpStatus::Error(code)) => {
                    ui.label(
                        RichText::new(format!("無法讀取遊戲連線統計（Windows 錯誤碼 {code}）"))
                            .color(YELLOW),
                    );
                }
                Some(TcpStatus::Stats(t)) => {
                    ui.label(format!("實際延遲 {} ms（變動 ±{} ms）", t.smoothed_rtt_ms, t.rtt_var_ms));
                    ui.separator();
                    let color = match Health::from_tcp(
                        t.retrans_window.into(),
                        t.timeouts_window.into(),
                    ) {
                        Health::Good => GRAY,
                        h => rgb(h),
                    };
                    ui.label(
                        RichText::new(format!(
                            "近 {WINDOW_SECS} 秒重傳 {} 個封包、逾時 {} 次",
                            t.retrans_window, t.timeouts_window
                        ))
                        .color(color),
                    )
                    .on_hover_text(
                        "重傳：封包送出後沒收到確認，系統重送。逾時：等太久都沒回應，連續發生會導致 90002 斷線。",
                    );
                }
            }
        });
    }

    fn restart_as_admin(&mut self, ctx: &egui::Context) {
        if elevation::restart_as_admin(&crate::admin_restart_args()) {
            // 新的程式已經用系統管理員身分啟動，這個直接結束
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else {
            self.push_event(
                time::now_hms(),
                "已取消以系統管理員身分重新啟動".into(),
                false,
            );
        }
    }

    /// 中間節點追蹤（類似 MTR）
    fn hops_ui(&self, ui: &mut egui::Ui) {
        let Some(Round {
            hop_target: Some(target),
            hops,
            hop_loss_origin,
            ..
        }) = &self.round
        else {
            ui.label(RichText::new("連上遊戲伺服器後，會開始追蹤路徑上的每一個節點。").color(GRAY));
            return;
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("到 {target} 的路徑（近 {WINDOW_SECS} 秒）"));
            match hop_loss_origin {
                Some((ttl, addr)) => {
                    let addr = addr.map(|a| format!("（{a}）")).unwrap_or_default();
                    ui.label(RichText::new(format!("從第 {ttl} 跳{addr}開始持續掉包")).color(RED));
                }
                None => {
                    ui.label(RichText::new("沒有持續掉包的節點").color(GRAY));
                }
            }
        });
        ui.label(
            RichText::new(
                "只有某一跳掉包、後面的節點都正常時，通常是那台路由器限制回應，不代表真的掉包。",
            )
            .small()
            .color(GRAY),
        );
        if hops.is_empty() {
            ui.label(RichText::new("追蹤中...").color(GRAY));
            return;
        }

        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                egui::Grid::new("hops")
                    .striped(true)
                    .num_columns(5)
                    .spacing([24.0, 4.0])
                    .show(ui, |ui| {
                        for h in ["跳數", "節點", "延遲", "平均", "掉包率"] {
                            ui.label(RichText::new(h).strong());
                        }
                        ui.end_row();

                        let origin = hop_loss_origin.map(|(ttl, _)| ttl);
                        for h in hops {
                            let in_lossy_part = origin.is_some_and(|o| h.ttl >= o);
                            let ttl = RichText::new(h.ttl.to_string());
                            ui.label(if in_lossy_part { ttl.color(RED) } else { ttl });
                            ui.label(
                                h.addr
                                    .map_or("*（沒有回應）".to_string(), |a| a.to_string()),
                            );
                            ui.label(fmt_rtt(h.last));
                            ui.label(fmt_ms(h.summary.and_then(|s| s.avg_ms)));
                            let loss = h.summary.map_or(0.0, |s| s.loss_pct);
                            let color = if in_lossy_part {
                                RED
                            } else if loss > 0.0 {
                                YELLOW
                            } else {
                                GRAY
                            };
                            ui.label(RichText::new(format!("{loss:.0}%")).color(color));
                            ui.end_row();
                        }
                    });
            });
    }

    fn table_ui(&self, ui: &mut egui::Ui) {
        ui.label(format!("統計範圍：最近 {WINDOW_SECS} 秒"));
        egui::Grid::new("layers")
            .striped(true)
            .num_columns(7)
            .spacing([24.0, 6.0])
            .show(ui, |ui| {
                for h in ["層級", "目標", "延遲", "平均", "抖動", "掉包率", "狀態"] {
                    ui.label(RichText::new(h).strong());
                }
                ui.end_row();

                for (i, name) in LAYER_NAMES.iter().enumerate() {
                    let report = self.layer(i);
                    ui.label(RichText::new(*name).color(LAYER_COLORS[i]).strong());
                    ui.label(report.and_then(|r| r.target.clone()).unwrap_or("-".into()));

                    let last = match report.and_then(|r| r.last) {
                        None if report.is_some_and(|r| r.failed) => "無法量測".to_string(),
                        None => "-".to_string(),
                        Some(rtt) => fmt_rtt(rtt),
                    };
                    let summary = report.and_then(|r| r.summary);
                    ui.label(last);
                    ui.label(fmt_ms(summary.and_then(|s| s.avg_ms)));
                    ui.label(fmt_ms(summary.and_then(|s| s.jitter_ms)));
                    ui.label(summary.map_or("-".to_string(), |s| format!("{:.1}%", s.loss_pct)));

                    let (health, state) = report.map_or((Health::Unknown, "-"), layer_state);
                    let state = ui.label(RichText::new(format!("● {state}")).color(rgb(health)));
                    if report.is_some_and(|r| r.failed) {
                        state.on_hover_text(
                            "這台電腦沒辦法送出量測（例如防火牆或防毒軟體擋住這個程式連線），不算掉包。遊戲本身的連線不受影響。",
                        );
                    }
                    ui.end_row();
                }
            });
    }

    fn chart_ui(&self, ui: &mut egui::Ui) {
        let lost: Vec<[f64; 2]> = self
            .history
            .iter()
            .flatten()
            .filter(|(_, ms)| ms.is_none())
            .map(|&(x, _)| [x, 0.0])
            .collect();

        // 顯示範圍每一幀都自己算：最近 10 分鐘、Y 軸從 0 到最大值再留一點空間。
        // 交給圖表自動縮放的話，遊戲重開清掉資料後範圍會卡住，線會跑出畫面外。
        let bounds = self.round_times.back().map(|&(x_max, _)| {
            let x_min = self
                .round_times
                .front()
                .map_or(x_max, |&(x, _)| x)
                .max(x_max - CHART_SPAN_SECS);
            let y_max = self
                .history
                .iter()
                .flatten()
                .filter(|(x, _)| *x >= x_min)
                .filter_map(|&(_, ms)| ms)
                .max()
                .map_or(CHART_MIN_Y_MS, |ms| (ms as f64 * 1.15).max(CHART_MIN_Y_MS));
            // 剛啟動只有一筆資料時，X 範圍不能是 0
            PlotBounds::from_min_max([x_min.min(x_max - 10.0), 0.0], [x_max, y_max])
        });
        let clock_base = self.clock_base_secs.unwrap_or(0);

        let response = Plot::new("latency")
            .legend(Legend::default())
            .allow_drag(false)
            .allow_zoom(false)
            .allow_scroll(false)
            .allow_boxed_zoom(false)
            .allow_double_click_reset(false)
            .allow_axis_zoom_drag(false)
            .y_axis_label("ms")
            .x_axis_formatter(move |mark, _| {
                let s = (clock_base + mark.value.round() as i64).rem_euclid(24 * 3600);
                format!("{:02}:{:02}", s / 3600, s % 3600 / 60)
            })
            // 預設的座標提示沒有意義（X 軸是經過秒數），改用下面的數值提示
            .show_x(false)
            .show_y(false)
            .show(ui, |plot| {
                if let Some(b) = bounds {
                    plot.set_plot_bounds(b);
                }
                for (i, history) in self.history.iter().enumerate() {
                    let points: PlotPoints = history
                        .iter()
                        .filter_map(|&(x, ms)| ms.map(|ms| [x, ms as f64]))
                        .collect();
                    plot.line(Line::new(LAYER_NAMES[i], points).color(LAYER_COLORS[i]));
                }
                if !lost.is_empty() {
                    plot.points(Points::new("掉包", lost).color(RED).radius(3.5));
                }

                // 滑鼠所在位置最近的一輪：畫一條垂直線，並標出各層在那一輪的數值
                let hovered = plot
                    .pointer_coordinate()
                    .and_then(|p| self.nearest_round(p.x));
                if let Some((x, _)) = hovered {
                    // 名稱留空，才不會出現在圖例裡
                    plot.vline(VLine::new("", x).color(GRAY));
                    for (i, history) in self.history.iter().enumerate() {
                        if let Some(Some(ms)) = value_at(history, x) {
                            plot.points(
                                Points::new("", vec![[x, ms as f64]])
                                    .color(LAYER_COLORS[i])
                                    .radius(4.5),
                            );
                        }
                    }
                }
                hovered
            });

        if let Some((x, time)) = response.inner {
            response.response.on_hover_ui_at_pointer(|ui| {
                ui.label(RichText::new(time).strong());
                egui::Grid::new("chart_hover")
                    .num_columns(2)
                    .show(ui, |ui| {
                        for (i, history) in self.history.iter().enumerate() {
                            let Some(value) = value_at(history, x).map(fmt_rtt) else {
                                continue;
                            };
                            ui.label(RichText::new(LAYER_NAMES[i]).color(LAYER_COLORS[i]));
                            let text = RichText::new(value);
                            ui.label(if value_at(history, x) == Some(None) {
                                text.color(RED)
                            } else {
                                text
                            });
                            ui.end_row();
                        }
                    });
            });
        }
    }

    /// 找出 X 座標最接近 `x` 的那一輪，回傳 (該輪的 X, 量測時間)
    fn nearest_round(&self, x: f64) -> Option<(f64, String)> {
        self.round_times
            .iter()
            .min_by(|a, b| (a.0 - x).abs().total_cmp(&(b.0 - x).abs()))
            .cloned()
    }

    /// 事件列表。最多 500 筆，只排版看得到的那幾筆：每一筆的高度記下來，
    /// 看不到的就直接跳過那段高度（異常事件可以展開，高度不固定，第一次畫之前先用估計值）
    fn events_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("事件紀錄").strong());
        let gap = ui.spacing().item_spacing.y;
        let line = ui.spacing().interact_size.y;
        let estimate = |e: &EventEntry| match e.kind {
            EntryKind::Plain { .. } => line,
            // 標題列加上一行診斷
            EntryKind::Incident(_) => line * 2.0 + gap,
        };
        let events = &self.events;
        let heights = &mut self.event_heights;
        let height_of = |e: &EventEntry, heights: &HashMap<u64, f32>| {
            heights.get(&e.id).copied().unwrap_or_else(|| estimate(e))
        };
        let total: f32 = events.iter().map(|e| height_of(e, heights) + gap).sum();

        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show_viewport(ui, |ui, viewport| {
                ui.set_height(total);
                let origin = ui.max_rect().min;
                let width = ui.available_width();
                let mut y = 0.0;
                let mut resized = false;
                for e in events {
                    let h = height_of(e, heights);
                    if y + h >= viewport.min.y && y <= viewport.max.y {
                        let rect = egui::Rect::from_min_size(
                            origin + egui::vec2(0.0, y),
                            egui::vec2(width, h),
                        );
                        let mut row = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(rect)
                                .layout(egui::Layout::top_down(egui::Align::Min)),
                        );
                        event_ui(&mut row, e);
                        let actual = row.min_rect().height();
                        if (actual - h).abs() > 0.5 {
                            heights.insert(e.id, actual);
                            resized = true;
                        }
                    }
                    y += h + gap;
                }
                // 高度跟估計的不一樣（例如剛展開事件）：再畫一次，位置才會對
                if resized {
                    ui.ctx().request_repaint();
                }
            });
    }

    fn latest_incident(&self) -> Option<&Incident> {
        self.events.iter().find_map(|e| match &e.kind {
            EntryKind::Incident(i) => Some(i),
            EntryKind::Plain { .. } => None,
        })
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 背景執行緒每送一筆訊息就會要求重繪，不需要持續重繪；視窗隱藏或最小化時也會呼叫這裡
        self.drain_messages();
        self.handle_tray_commands(ctx);
        self.poll_export();
        self.poll_clear();
        self.handle_close(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.confirm_clear_ui(&ctx);

        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(4.0);
            self.header_ui(ui);
            ui.add_space(4.0);
        });
        egui::Panel::bottom("events")
            .resizable(true)
            .default_size(150.0)
            .min_size(80.0)
            .show(ui, |ui| self.events_ui(ui));
        egui::CentralPanel::default_margins().show(ui, |ui| {
            self.table_ui(ui);
            ui.add_space(4.0);
            self.tcp_ui(ui);
            ui.separator();
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Chart, "延遲圖表");
                ui.selectable_value(&mut self.tab, Tab::Hops, "中間節點");
            });
            match self.tab {
                Tab::Chart => self.chart_ui(ui),
                Tab::Hops => self.hops_ui(ui),
            }
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(tray) = &self.tray {
            tray.remove();
        }
        // App 被釋放時，Monitor 會等背景執行緒把還沒寫入的量測資料寫進記錄檔
    }
}

/// 某一層在 X 座標 `x` 那一輪的結果；None = 那一輪沒有這層的資料，Some(None) = 逾時
fn value_at(history: &VecDeque<(f64, Option<u32>)>, x: f64) -> Option<Option<u32>> {
    history.iter().find(|(hx, _)| *hx == x).map(|&(_, ms)| ms)
}

fn event_ui(ui: &mut egui::Ui, e: &EventEntry) {
    match &e.kind {
        EntryKind::Plain { time, text, severe } => {
            ui.horizontal(|ui| {
                ui.label(RichText::new(time).monospace().color(GRAY));
                let text = RichText::new(text);
                ui.label(if *severe { text.color(RED) } else { text });
            });
        }
        EntryKind::Incident(incident) => incident_ui(ui, e.id, incident),
    }
}

/// 異常事件：標題列顯示時間、事件與診斷，展開後看各層數據和 traceroute
fn incident_ui(ui: &mut egui::Ui, id: u64, incident: &Incident) {
    let header = format!("{}  ⚠ {}", incident.time, incident.title);
    egui::CollapsingHeader::new(RichText::new(header).color(RED))
        .id_salt(("incident", id))
        .show(ui, |ui| {
            ui.label(RichText::new(&incident.diagnosis).strong());
            ui.add_space(4.0);
            for line in &incident.details {
                ui.label(RichText::new(line).monospace());
            }
        });
    ui.label(RichText::new(format!("　　{}", incident.diagnosis)).color(GRAY));
}

/// 狀態燈：最近一次逾時或掉包率 ≥ 5% 為紅，有掉包或抖動大為黃
fn layer_state(r: &LayerReport) -> (Health, &'static str) {
    match (r.last, r.summary) {
        (None, _) if r.failed => (Health::Warn, "無法量測"),
        (None, _) => (Health::Unknown, "無目標"),
        (Some(None), _) => (Health::Bad, "逾時"),
        (_, Some(s)) => match Health::from_loss(s.loss_pct) {
            Health::Bad => (Health::Bad, "掉包"),
            Health::Warn => (Health::Warn, "不穩"),
            _ if s.jitter_ms.unwrap_or(0.0) > 30.0 => (Health::Warn, "不穩"),
            _ => (Health::Good, "正常"),
        },
        _ => (Health::Good, "正常"),
    }
}

/// 載入系統的中文字型（不嵌入 exe，避免檔案變大）。回傳用了 FONT_CANDIDATES 的第幾個
fn load_system_font(ctx: &egui::Context) -> Option<usize> {
    let windir = shell::windows_dir().unwrap_or_else(|| "C:\\Windows".into());
    let fonts_dir = windir.join("Fonts");
    let (index, bytes) = FONT_CANDIDATES
        .iter()
        .enumerate()
        .find_map(|(i, (file, _, _))| {
            file_map::map_static(&fonts_dir.join(file)).map(|b| (i, b))
        })?;
    // 字型檔約 20 MB，用記憶體映射只會載入實際用到的字。
    // 用 from_owned 的話 egui 內部會再複製一份，'static 的資料就不會
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("msjh".into(), Arc::new(egui::FontData::from_static(bytes)));
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "msjh".into());
    // 等寬字型保留預設的英數字，中文再用中文字型
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push("msjh".into());
    ctx.set_fonts(fonts);
    Some(index)
}
