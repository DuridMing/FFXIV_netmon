//! FF14 繁中服連線監測工具。
//!
//! 用法：
//!   ff14-netmon                     自動偵測遊戲伺服器
//!   ff14-netmon --target IP:PORT    手動指定要 TCP ping 的目標（沒開遊戲時測試用）

#![windows_subsystem = "windows"]

mod app;
mod detector;
mod diagnosis;
mod events;
mod game_tcp;
mod hops;
mod icon;
mod monitor;
mod report;
mod settings;
mod stats;
mod storage;
mod trace;
mod win;

use std::net::SocketAddrV4;
use std::sync::Arc;

use eframe::egui;

use icon::Health;
use win::tray;

const APP_ICON_SIZE: u32 = 64;

fn main() -> eframe::Result {
    let (manual_target, startup_error) = match parse_target_arg() {
        Ok(t) => (t, None),
        Err(e) => (None, Some(e)),
    };

    let icon = egui::IconData {
        rgba: icon::circle_rgba(APP_ICON_SIZE, Health::Good),
        width: APP_ICON_SIZE,
        height: APP_ICON_SIZE,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([780.0, 680.0])
            .with_min_inner_size([560.0, 460.0])
            .with_icon(Arc::new(icon))
            // 啟動時不搶焦點，避免正在玩的遊戲失去焦點
            .with_active(false),
        renderer: eframe::Renderer::Wgpu,
        wgpu_options: low_power_wgpu(),
        ..Default::default()
    };
    eframe::run_native(
        "FF14 連線監測",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let rx = monitor::spawn(manual_target, move || ctx.request_repaint());

            let (tray_tx, tray_rx) = crossbeam_channel::unbounded();
            let ctx = cc.egui_ctx.clone();
            let tray = tray::spawn("FF14 連線監測", move |cmd| {
                let _ = tray_tx.send(cmd);
                ctx.request_repaint();
            });

            Ok(Box::new(app::App::new(
                cc,
                rx,
                startup_error,
                tray,
                tray_rx,
            )))
        }),
    )
}

/// 只用 DirectX 12，並優先選省電的顯示卡（有內顯就用內顯）。
/// 用 OpenGL 會跟遊戲搶同一張獨立顯示卡，開程式的瞬間遊戲會卡一下。
fn low_power_wgpu() -> eframe::WgpuConfiguration {
    use eframe::egui_wgpu::WgpuSetup;
    use eframe::wgpu;

    let mut config = eframe::WgpuConfiguration::default();
    if let WgpuSetup::CreateNew(setup) = &mut config.wgpu_setup {
        setup.instance_descriptor.backends = wgpu::Backends::DX12;
        setup.power_preference = wgpu::PowerPreference::LowPower;
    }
    config
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
