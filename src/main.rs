//! FF14 繁中服連線監測工具。
//!
//! 用法：
//!   ff14-netmon                     自動偵測遊戲伺服器
//!   ff14-netmon --target IP:PORT    手動指定要 TCP ping 的目標（沒開遊戲時測試用）

#![windows_subsystem = "windows"]

mod app;
mod detector;
mod monitor;
mod stats;
mod win;

use std::net::SocketAddrV4;

use eframe::egui;

fn main() -> eframe::Result {
    let (manual_target, startup_error) = match parse_target_arg() {
        Ok(t) => (t, None),
        Err(e) => (None, Some(e)),
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([780.0, 640.0])
            .with_min_inner_size([560.0, 440.0]),
        ..Default::default()
    };
    eframe::run_native(
        "FF14 連線監測",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let rx = monitor::spawn(manual_target, move || ctx.request_repaint());
            Ok(Box::new(app::App::new(cc, rx, startup_error)))
        }),
    )
}

fn parse_target_arg() -> Result<Option<SocketAddrV4>, String> {
    let args: Vec<String> = std::env::args().collect();
    let Some(i) = args.iter().position(|a| a == "--target") else {
        return Ok(None);
    };
    match args.get(i + 1).map(|s| s.parse()) {
        Some(Ok(addr)) => Ok(Some(addr)),
        _ => Err("--target 格式錯誤，應為 IP:PORT，例如 --target 203.0.113.10:55006".into()),
    }
}
