//! FF14 繁中服連線監測工具。
//!
//! 用法：
//!   ff14-netmon                     自動偵測遊戲伺服器
//!   ff14-netmon --target IP:PORT    手動指定要 TCP ping 的目標（沒開遊戲時測試用）
//!
//! 程式自己會用到的參數（重新啟動時帶上）：
//!   --renderer dx12|gl|warp         繪圖方式，預設 dx12；初始化失敗時會依序改用下一種
//!   --restarted                     由舊的程式重新啟動，要等舊的結束再取得執行權
//!   --crash-restarts N              執行中繪圖出錯而重新啟動的次數，避免無限重啟
//!   --data-dir PATH                 記錄檔和設定的資料夾（以系統管理員身分重新啟動時沿用原本使用者的）
//!   --docs-dir PATH                 匯出報告的資料夾（同上）
//!   --test-gpu-failure              測試用：讓 DirectX 12 初始化失敗，檢查備援流程

#![windows_subsystem = "windows"]

mod app;
mod detector;
mod diagnosis;
mod events;
mod game_tcp;
mod hops;
mod icon;
mod monitor;
mod network;
mod report;
mod settings;
mod stats;
mod storage;
mod trace;
mod wifi;
mod win;

use std::net::SocketAddrV4;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use eframe::egui;

use icon::Health;
use win::instance::{self, Acquire};
use win::{dialog, tray};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const APP_NAME: &str = "FF14 連線監測";
const APP_ICON_SIZE: u32 = 64;
/// 重新啟動時，等舊的程式結束最多等多久
const RESTART_WAIT_MS: u32 = 15_000;
/// 執行中繪圖出錯時，連續重新啟動最多幾次（每次都很快又出錯就放棄）
const MAX_CRASH_RESTARTS: u32 = 3;
/// 執行超過這麼久才出錯，就不算連續出錯，重新計算次數
const CRASH_RESET_AFTER: Duration = Duration::from_secs(60);

/// 畫面已經初始化成功（eframe 建好繪圖裝置後才會呼叫 app creator）。
/// 之後 run_native 回傳錯誤就不是「這種繪圖方式不能用」，而是執行中發生的錯誤
static GRAPHICS_READY: AtomicBool = AtomicBool::new(false);

/// 繪圖方式。DirectX 12 用省電顯示卡最不影響遊戲；失敗時依序改用 OpenGL、軟體繪圖
#[derive(Clone, Copy, PartialEq)]
enum Renderer {
    Dx12,
    OpenGl,
    Warp,
}

impl Renderer {
    fn arg(self) -> &'static str {
        match self {
            Renderer::Dx12 => "dx12",
            Renderer::OpenGl => "gl",
            Renderer::Warp => "warp",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        [Renderer::Dx12, Renderer::OpenGl, Renderer::Warp]
            .into_iter()
            .find(|r| r.arg() == s)
    }

    fn next(self) -> Option<Self> {
        match self {
            Renderer::Dx12 => Some(Renderer::OpenGl),
            Renderer::OpenGl => Some(Renderer::Warp),
            Renderer::Warp => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Renderer::Dx12 => "DirectX 12",
            Renderer::OpenGl => "OpenGL",
            Renderer::Warp => "軟體繪圖（WARP）",
        }
    }
}

struct Options {
    target: Result<Option<SocketAddrV4>, String>,
    renderer: Renderer,
    restarted: bool,
    crash_restarts: u32,
    test_gpu_failure: bool,
    /// 重新啟動時要原樣帶過去的參數（不含 --renderer、--restarted、--crash-restarts）
    passthrough: Vec<String>,
}

impl Options {
    fn parse() -> Self {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let value_of = |flag: &str| {
            args.iter()
                .position(|a| a == flag)
                .map(|i| args.get(i + 1).cloned())
        };

        let target = match value_of("--target") {
            None => Ok(None),
            Some(v) => v.and_then(|s| s.parse().ok()).map(Some).ok_or_else(|| {
                "--target 格式錯誤，應為 IP:PORT，例如 --target 203.0.113.10:55006".to_string()
            }),
        };
        let renderer = value_of("--renderer")
            .flatten()
            .and_then(|s| Renderer::parse(&s))
            .unwrap_or(Renderer::Dx12);
        let crash_restarts = value_of("--crash-restarts")
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if let Some(dir) = value_of("--data-dir").flatten() {
            storage::set_data_dir(PathBuf::from(dir));
        }
        if let Some(dir) = value_of("--docs-dir").flatten() {
            report::set_export_dir(PathBuf::from(dir));
        }

        let mut passthrough = Vec::new();
        let mut iter = args.iter();
        while let Some(a) = iter.next() {
            match a.as_str() {
                "--renderer" | "--crash-restarts" => {
                    iter.next();
                }
                "--restarted" => {}
                _ => passthrough.push(a.clone()),
            }
        }

        Options {
            target,
            renderer,
            restarted: args.iter().any(|a| a == "--restarted"),
            crash_restarts,
            test_gpu_failure: args.iter().any(|a| a == "--test-gpu-failure"),
            passthrough,
        }
    }
}

fn main() {
    let opts = Options::parse();

    let lock = match instance::acquire(if opts.restarted { RESTART_WAIT_MS } else { 0 }) {
        Acquire::Acquired(lock) => lock,
        Acquire::AlreadyRunning => {
            // 已經有一個在執行：把它的視窗叫出來就好
            if !tray::show_existing() {
                dialog::info(
                    APP_NAME,
                    "程式已經在執行中，請從工作列右下角的系統匣圖示打開視窗。",
                );
            }
            return;
        }
    };

    let started = Instant::now();
    let Err(err) = run(&opts) else { return };
    // winit 一個程式只能建立一次事件迴圈，不管哪種情況都要重新啟動自己
    if GRAPHICS_READY.load(Ordering::SeqCst) {
        // 畫面原本好好的，執行中才出錯（例如顯示卡驅動重置）：用同一種繪圖方式重開，繼續監測。
        // 每次一開就出錯的話不要無限重啟
        let count = if started.elapsed() > CRASH_RESET_AFTER {
            0
        } else {
            opts.crash_restarts
        };
        if count >= MAX_CRASH_RESTARTS {
            show_runtime_error(&err.to_string());
            return;
        }
        drop(lock);
        if let Err(e) = relaunch(&opts, opts.renderer, count + 1) {
            show_relaunch_failed(&err.to_string(), &e.to_string());
        }
        return;
    }
    // 繪圖初始化失敗：改用下一種繪圖方式
    match opts.renderer.next() {
        Some(next) => {
            drop(lock);
            if let Err(e) = relaunch(&opts, next, 0) {
                show_relaunch_failed(&err.to_string(), &e.to_string());
            }
        }
        None => show_fatal(&err.to_string()),
    }
}

fn run(opts: &Options) -> eframe::Result {
    let mut startup_events = Vec::new();
    let target = match &opts.target {
        Ok(t) => *t,
        Err(e) => {
            startup_events.push((e.clone(), true));
            None
        }
    };
    if opts.renderer != Renderer::Dx12 {
        startup_events.push((
            format!(
                "這台電腦無法使用 DirectX 12 繪製畫面，已改用 {}。功能不受影響。",
                opts.renderer.label()
            ),
            false,
        ));
    }

    let icon = egui::IconData {
        rgba: icon::circle_rgba(APP_ICON_SIZE, Health::Good),
        width: APP_ICON_SIZE,
        height: APP_ICON_SIZE,
    };
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([780.0, 720.0])
            .with_min_inner_size([560.0, 480.0])
            .with_icon(Arc::new(icon))
            // 啟動時不搶焦點，避免正在玩的遊戲失去焦點
            .with_active(false),
        ..Default::default()
    };
    match opts.renderer {
        Renderer::Dx12 => {
            options.renderer = eframe::Renderer::Wgpu;
            options.wgpu_options = dx12_config(false, opts.test_gpu_failure);
        }
        Renderer::OpenGl => options.renderer = eframe::Renderer::Glow,
        Renderer::Warp => {
            options.renderer = eframe::Renderer::Wgpu;
            options.wgpu_options = dx12_config(true, false);
        }
    }

    eframe::run_native(
        &format!("{APP_NAME} v{VERSION}"),
        options,
        Box::new(move |cc| {
            GRAPHICS_READY.store(true, Ordering::SeqCst);
            let ctx = cc.egui_ctx.clone();
            let monitor = monitor::spawn(target, move || ctx.request_repaint());

            let (tray_tx, tray_rx) = crossbeam_channel::unbounded();
            let ctx = cc.egui_ctx.clone();
            let tray = tray::spawn(APP_NAME, move |cmd| {
                let _ = tray_tx.send(cmd);
                ctx.request_repaint();
            });

            Ok(Box::new(app::App::new(
                cc,
                monitor,
                startup_events,
                tray,
                tray_rx,
            )))
        }),
    )
}

/// DirectX 12 的設定。
/// 一般情況優先選省電的顯示卡（有內顯就用內顯）：用 OpenGL 或獨立顯示卡會跟遊戲搶資源，開程式的瞬間遊戲會卡一下。
/// `software` 為 true 時改用 Windows 內建的軟體繪圖（WARP），完全不需要顯示卡驅動。
fn dx12_config(software: bool, test_failure: bool) -> eframe::WgpuConfiguration {
    use eframe::egui_wgpu::WgpuSetup;
    use eframe::wgpu;

    let mut config = eframe::WgpuConfiguration::default();
    if let WgpuSetup::CreateNew(setup) = &mut config.wgpu_setup {
        setup.instance_descriptor.backends = wgpu::Backends::DX12;
        setup.power_preference = wgpu::PowerPreference::LowPower;
        if software || test_failure {
            setup.native_adapter_selector = Some(Arc::new(move |adapters, _surface| {
                if test_failure {
                    return Err("測試用：模擬 DirectX 12 初始化失敗".into());
                }
                adapters
                    .iter()
                    .find(|a| a.get_info().device_type == wgpu::DeviceType::Cpu)
                    .cloned()
                    .ok_or_else(|| "找不到 Windows 軟體繪圖裝置（WARP）".to_string())
            }));
        }
    }
    config
}

fn relaunch(opts: &Options, renderer: Renderer, crash_restarts: u32) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.args(&opts.passthrough)
        .args(["--renderer", renderer.arg(), "--restarted"]);
    if crash_restarts > 0 {
        cmd.args(["--crash-restarts", &crash_restarts.to_string()]);
    }
    cmd.spawn().map(|_| ())
}

/// 畫面出錯後要重新啟動自己，但啟動失敗
fn show_relaunch_failed(err: &str, relaunch_err: &str) {
    dialog::error(
        APP_NAME,
        &format!(
            "{APP_NAME} 的畫面發生錯誤，需要重新啟動，但無法重新啟動程式。\n\n\
             畫面錯誤：{err}\n\
             重新啟動失敗：{relaunch_err}\n\n\
             請自己再開一次程式。如果程式檔案被移動、改名，或被防毒軟體擋住，也會發生這個問題。"
        ),
    );
}

/// 執行中畫面一直出錯，重新啟動好幾次都一樣
fn show_runtime_error(err: &str) {
    dialog::error(
        APP_NAME,
        &format!(
            "{APP_NAME} 的畫面一直發生錯誤，重新啟動 {MAX_CRASH_RESTARTS} 次都沒有改善，程式先停止。\n\n\
             錯誤訊息：{err}\n\n\
             可以試試：\n\
             ・更新顯示卡驅動程式\n\
             ・重新開機後再試一次"
        ),
    );
}

/// 所有繪圖方式都無法初始化
fn show_fatal(err: &str) {
    dialog::error(
        APP_NAME,
        &format!(
            "{APP_NAME} 無法顯示畫面。\n\n\
             已經依序嘗試 DirectX 12、OpenGL 和軟體繪圖，都無法使用。\n\n\
             錯誤訊息：{err}\n\n\
             可以試試：\n\
             ・更新顯示卡驅動程式\n\
             ・不要透過遠端桌面執行\n\
             ・重新開機後再試一次"
        ),
    );
}
