//! egui 介面：遊戲狀態、各層狀態表、即時延遲圖、事件列表；系統匣常駐、通知、匯出報告。

use std::collections::VecDeque;
use std::sync::Arc;
use std::thread;

use crossbeam_channel::Receiver;
use eframe::egui::{self, Color32, RichText};
use egui_plot::{Legend, Line, Plot, PlotPoints, Points};

use crate::icon::Health;
use crate::monitor::{GameStatus, Incident, LAYER_NAMES, LayerReport, MonitorMsg, WINDOW_SIZE};
use crate::report::{self, Exported};
use crate::win::time;
use crate::win::tray::{Tray, TrayCommand};
use crate::win::wlan::WifiInfo;

/// 圖表保留的量測次數：300 × 2 秒 = 10 分鐘
const HISTORY: usize = 300;
const MAX_EVENTS: usize = 500;
const GAME_LAYER: usize = 3;
const FONT_PATH: &str = r"C:\Windows\Fonts\msjh.ttc";
const REPORT_HOURS: u32 = 24;
/// 低於這個 Wi-Fi 訊號就用黃色提醒
const WEAK_WIFI: u32 = 50;

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
    /// 給可展開區塊當唯一 ID
    id: u64,
    time: String,
    text: String,
    severe: bool,
    /// 異常事件才有：診斷結論與詳細數據
    incident: Option<Incident>,
}

pub struct App {
    rx: Receiver<MonitorMsg>,
    /// 啟動進度，空字串代表已就緒
    status: String,
    game: Option<GameStatus>,
    layers: Vec<LayerReport>,
    wifi: Option<WifiInfo>,
    /// 每層的 (經過秒數, 延遲)；延遲 None 代表逾時
    history: [VecDeque<(f64, Option<u32>)>; 4],
    /// 最新的在最前面
    events: VecDeque<EventEntry>,
    next_event_id: u64,

    /// 建立系統匣失敗時為 None，此時關閉視窗就直接結束
    tray: Option<Tray>,
    tray_rx: Receiver<TrayCommand>,
    /// 從系統匣選「結束」後才真的關閉，否則按 X 只是縮到系統匣
    quitting: bool,
    hide_hint_shown: bool,
    notify_enabled: bool,
    export_rx: Option<Receiver<Result<Exported, String>>>,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        rx: Receiver<MonitorMsg>,
        startup_error: Option<String>,
        tray: Option<Tray>,
        tray_rx: Receiver<TrayCommand>,
    ) -> Self {
        let mut app = Self {
            rx,
            status: "啟動中...".into(),
            game: None,
            layers: Vec::new(),
            wifi: None,
            history: Default::default(),
            events: VecDeque::new(),
            next_event_id: 0,
            tray,
            tray_rx,
            quitting: false,
            hide_hint_shown: false,
            notify_enabled: true,
            export_rx: None,
        };
        if !load_system_font(&cc.egui_ctx) {
            app.push_event(String::new(), format!("找不到中文字型 {FONT_PATH}"), true);
        }
        if app.tray.is_none() {
            app.push_event(
                String::new(),
                "無法建立系統匣圖示，關閉視窗就會結束程式".into(),
                true,
            );
        }
        if let Some(e) = startup_error {
            app.push_event(String::new(), e, true);
        }
        app
    }

    fn push_event(&mut self, time: String, text: String, severe: bool) {
        self.push_entry(time, text, severe, None);
    }

    fn push_entry(&mut self, time: String, text: String, severe: bool, incident: Option<Incident>) {
        self.next_event_id += 1;
        self.events.push_front(EventEntry {
            id: self.next_event_id,
            time,
            text,
            severe,
            incident,
        });
        self.events.truncate(MAX_EVENTS);
    }

    fn drain_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                MonitorMsg::Status(s) => self.status = s,
                MonitorMsg::Event { time, text, severe } => self.push_event(time, text, severe),
                MonitorMsg::Incident(incident) => {
                    if self.notify_enabled
                        && let Some(tray) = &self.tray
                    {
                        tray.notify(&format!("⚠ {}", incident.title), &incident.diagnosis);
                    }
                    let (time, text) = (incident.time.clone(), incident.title.clone());
                    self.push_entry(time, text, true, Some(incident));
                }
                MonitorMsg::Round {
                    elapsed,
                    wifi,
                    game,
                    layers,
                } => {
                    for (history, report) in self.history.iter_mut().zip(&layers) {
                        match report.last {
                            Some(r) => history.push_back((elapsed, r)),
                            // 目標消失（例如遊戲關閉）時清掉舊的線
                            None => history.clear(),
                        }
                        if history.len() > HISTORY {
                            history.pop_front();
                        }
                    }
                    self.game = Some(game);
                    self.layers = layers;
                    self.wifi = wifi;
                    self.update_tray();
                }
            }
        }
    }

    /// 整體燈號：有遊戲目標時看遊戲伺服器那層，否則看遊戲狀態
    fn overall_health(&self) -> Health {
        match &self.game {
            None | Some(GameStatus::NotRunning) => Health::Unknown,
            Some(GameStatus::NoConnection) => Health::Warn,
            Some(GameStatus::Connected(_) | GameStatus::Manual(_)) => self
                .layers
                .get(GAME_LAYER)
                .map_or(Health::Unknown, |r| layer_state(r).0),
        }
    }

    fn update_tray(&self) {
        let Some(tray) = &self.tray else { return };
        let game = match (&self.game, self.layers.get(GAME_LAYER).and_then(|r| r.last)) {
            (Some(GameStatus::Connected(_) | GameStatus::Manual(_)), Some(Some(ms))) => {
                format!("遊戲伺服器 {ms} ms")
            }
            (Some(GameStatus::Connected(_) | GameStatus::Manual(_)), _) => "遊戲伺服器 逾時".into(),
            (Some(GameStatus::NoConnection), _) => "遊戲未連線".into(),
            _ => "遊戲未執行".into(),
        };
        let mut tip = format!("FF14 連線監測\n{game}");
        if let Some(w) = &self.wifi {
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

    /// 按 X 時縮到系統匣，繼續在背景監測
    fn handle_close(&mut self, ctx: &egui::Context) {
        let Some(tray) = &self.tray else { return };
        if self.quitting || !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        if !self.hide_hint_shown {
            tray.notify(
                "FF14 連線監測仍在執行",
                "程式已縮到系統匣，會繼續監測。點圖示可以打開視窗，右鍵選「結束」可以關閉。",
            );
            self.hide_hint_shown = true;
        }
    }

    fn start_export(&mut self, ctx: &egui::Context) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(report::export(REPORT_HOURS));
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

    fn header_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let text = match &self.game {
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
                ui.separator();
                ui.spinner();
                ui.label(&self.status);
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let exporting = self.export_rx.is_some();
                let label = if exporting {
                    "匯出中..."
                } else {
                    "匯出報告"
                };
                if ui
                    .add_enabled(!exporting, egui::Button::new(label))
                    .on_hover_text(format!(
                        "匯出最近 {REPORT_HOURS} 小時的 HTML 報告和 CSV 到「文件\\ff14-netmon」"
                    ))
                    .clicked()
                {
                    self.start_export(ui.ctx());
                }
                if self.tray.is_some() {
                    ui.checkbox(&mut self.notify_enabled, "異常時通知");
                }
            });
        });

        ui.horizontal(|ui| match &self.wifi {
            Some(w) => {
                let color = if w.quality < WEAK_WIFI { YELLOW } else { GRAY };
                ui.label(
                    RichText::new(format!("Wi-Fi：{}　訊號 {}%", w.ssid, w.quality)).color(color),
                );
                if w.quality < WEAK_WIFI {
                    ui.label(RichText::new("訊號偏弱，建議改用有線網路").color(YELLOW));
                }
            }
            None => {
                ui.label(RichText::new("網路：有線（未使用 Wi-Fi）").color(GRAY));
            }
        });

        if let Some(i) = self.latest_incident() {
            ui.label(
                RichText::new(format!("最近異常 {}：{}　{}", i.time, i.title, i.diagnosis))
                    .color(YELLOW),
            );
        }
    }

    fn table_ui(&self, ui: &mut egui::Ui) {
        ui.label(format!(
            "統計範圍：最近 {} 秒",
            WINDOW_SIZE as u64 * crate::monitor::INTERVAL.as_secs()
        ));
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
                    let report = self.layers.get(i);
                    ui.label(RichText::new(*name).color(LAYER_COLORS[i]).strong());
                    ui.label(report.and_then(|r| r.target.clone()).unwrap_or("-".into()));

                    let last = match report.and_then(|r| r.last) {
                        None => "-".to_string(),
                        Some(None) => "逾時".to_string(),
                        Some(Some(ms)) => format!("{ms} ms"),
                    };
                    let summary = report.and_then(|r| r.summary);
                    let fmt_ms =
                        |v: Option<f64>| v.map_or("-".to_string(), |v| format!("{v:.1} ms"));
                    ui.label(last);
                    ui.label(fmt_ms(summary.and_then(|s| s.avg_ms)));
                    ui.label(fmt_ms(summary.and_then(|s| s.jitter_ms)));
                    ui.label(summary.map_or("-".to_string(), |s| format!("{:.1}%", s.loss_pct)));

                    let (health, state) = report.map_or((Health::Unknown, "-"), layer_state);
                    ui.label(RichText::new(format!("● {state}")).color(rgb(health)));
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

        Plot::new("latency")
            .legend(Legend::default())
            .include_y(0.0)
            .allow_drag(false)
            .allow_zoom(false)
            .allow_scroll(false)
            .allow_boxed_zoom(false)
            .y_axis_label("ms")
            .x_axis_formatter(|mark, _| {
                let s = mark.value.max(0.0) as u64;
                format!("{}:{:02}", s / 60, s % 60)
            })
            .show(ui, |plot| {
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
            });
    }

    fn events_ui(&self, ui: &mut egui::Ui) {
        ui.label(RichText::new("事件紀錄").strong());
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                for e in &self.events {
                    match &e.incident {
                        None => {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&e.time).monospace().color(GRAY));
                                let text = RichText::new(&e.text);
                                ui.label(if e.severe { text.color(RED) } else { text });
                            });
                        }
                        Some(incident) => incident_ui(ui, e.id, incident),
                    }
                }
            });
    }

    fn latest_incident(&self) -> Option<&Incident> {
        self.events.iter().find_map(|e| e.incident.as_ref())
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 背景執行緒每送一筆訊息就會要求重繪，不需要持續重繪；視窗隱藏時也會呼叫這裡
        self.drain_messages();
        self.handle_tray_commands(ctx);
        self.poll_export();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_close(&ctx);

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
            ui.separator();
            self.chart_ui(ui);
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(tray) = &self.tray {
            tray.remove();
        }
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
        (None, _) => (Health::Unknown, "無目標"),
        (Some(None), _) => (Health::Bad, "逾時"),
        (_, Some(s)) if s.loss_pct >= 5.0 => (Health::Bad, "掉包"),
        (_, Some(s)) if s.loss_pct > 0.0 || s.jitter_ms.unwrap_or(0.0) > 30.0 => {
            (Health::Warn, "不穩")
        }
        _ => (Health::Good, "正常"),
    }
}

/// 載入系統的微軟正黑體（不嵌入 exe，避免檔案變大）
fn load_system_font(ctx: &egui::Context) -> bool {
    let Ok(bytes) = std::fs::read(FONT_PATH) else {
        return false;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("msjh".into(), Arc::new(egui::FontData::from_owned(bytes)));
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "msjh".into());
    // 等寬字型保留預設的英數字，中文再用正黑體
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push("msjh".into());
    ctx.set_fonts(fonts);
    true
}
