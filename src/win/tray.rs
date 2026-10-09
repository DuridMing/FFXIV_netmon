//! 系統匣圖示（Shell_NotifyIcon）：狀態燈號、滑鼠提示、右鍵選單、通知氣泡。
//!
//! 圖示需要一個視窗接收滑鼠訊息，所以另開一條執行緒跑隱藏視窗和訊息迴圈。

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::thread;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIcon, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DispatchMessageW, GetCursorPos, GetMessageW, HICON, MF_STRING, MSG, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW,
};
use windows::core::w;

use crate::icon::{self, Health};

pub enum TrayCommand {
    Show,
    Exit,
}

const WM_TRAY: u32 = WM_APP + 1;
const ICON_ID: u32 = 1;
const MENU_SHOW: usize = 1;
const MENU_EXIT: usize = 2;
const ICON_SIZE: u32 = 32;

/// HWND、HICON 都不是 Send，存成整數才能在執行緒間共用
#[derive(Clone, Copy)]
pub struct Tray {
    hwnd: isize,
    icons: [isize; 4],
}

struct State {
    health: Health,
    tip: String,
}

static HANDLER: OnceLock<Box<dyn Fn(TrayCommand) + Send + Sync>> = OnceLock::new();
static TRAY: OnceLock<Tray> = OnceLock::new();
static STATE: Mutex<State> = Mutex::new(State {
    health: Health::Unknown,
    tip: String::new(),
});
/// 檔案總管重啟後會廣播這個訊息，要重新加入圖示
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

/// 建立系統匣圖示。使用者點圖示或選單時會呼叫 `handler`（在系統匣執行緒上）。
pub fn spawn(tip: &str, handler: impl Fn(TrayCommand) + Send + Sync + 'static) -> Option<Tray> {
    HANDLER.set(Box::new(handler)).ok()?;
    STATE.lock().ok()?.tip = tip.to_string();

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || unsafe {
        let hinstance = GetModuleHandleW(None).ok().map(|h| HINSTANCE(h.0));
        let class = w!("ff14-netmon-tray");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.unwrap_or_default(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        // 一般的隱藏視窗（不是 message-only），右鍵選單才能正確取得焦點
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("FF14 連線監測"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            hinstance,
            None,
        ) else {
            let _ = tx.send(None);
            return;
        };
        TASKBAR_CREATED.store(
            RegisterWindowMessageW(w!("TaskbarCreated")),
            Ordering::Relaxed,
        );

        let tray = Tray {
            hwnd: hwnd.0 as isize,
            icons: Health::ALL.map(|h| make_icon(hinstance, h)),
        };
        let _ = TRAY.set(tray);
        tray.add();
        let _ = tx.send(Some(tray));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    });
    rx.recv().ok().flatten()
}

impl Tray {
    fn data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: HWND(self.hwnd as *mut c_void),
            uID: ICON_ID,
            ..Default::default()
        }
    }

    fn icon(&self, health: Health) -> HICON {
        let i = Health::ALL.iter().position(|&h| h == health).unwrap_or(0);
        HICON(self.icons[i] as *mut c_void)
    }

    fn add(&self) {
        let Ok(state) = STATE.lock() else { return };
        let mut d = self.data();
        d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        d.uCallbackMessage = WM_TRAY;
        d.hIcon = self.icon(state.health);
        copy_wide(&mut d.szTip, &state.tip);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &d);
        }
    }

    /// 更新燈號顏色和滑鼠提示文字；沒有變化就不呼叫系統
    pub fn set_status(&self, health: Health, tip: &str) {
        let Ok(mut state) = STATE.lock() else { return };
        if state.health == health && state.tip == tip {
            return;
        }
        state.health = health;
        state.tip = tip.to_string();

        let mut d = self.data();
        d.uFlags = NIF_ICON | NIF_TIP;
        d.hIcon = self.icon(health);
        copy_wide(&mut d.szTip, tip);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
        }
    }

    /// 跳出通知；Windows 10/11 會顯示成右下角的通知
    pub fn notify(&self, title: &str, text: &str) {
        let mut d = self.data();
        d.uFlags = NIF_INFO;
        d.dwInfoFlags = NIIF_WARNING;
        copy_wide(&mut d.szInfoTitle, title);
        copy_wide(&mut d.szInfo, text);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &d);
        }
    }

    /// 結束前移除圖示，不然系統匣會留下殘影直到滑鼠移過去
    pub fn remove(&self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.data());
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_TRAY {
        match lparam.0 as u32 & 0xffff {
            WM_LBUTTONUP => emit(TrayCommand::Show),
            WM_RBUTTONUP => unsafe { show_menu(hwnd) },
            _ => {}
        }
        return LRESULT(0);
    }
    let taskbar_created = TASKBAR_CREATED.load(Ordering::Relaxed);
    if taskbar_created != 0
        && msg == taskbar_created
        && let Some(tray) = TRAY.get()
    {
        tray.add();
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

unsafe fn show_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, MENU_SHOW, w!("顯示視窗"));
        let _ = AppendMenuW(menu, MF_STRING, MENU_EXIT, w!("結束"));

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // 沒有先取得前景，點選單外面時選單不會關閉
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);

        match cmd.0 as usize {
            MENU_SHOW => emit(TrayCommand::Show),
            MENU_EXIT => emit(TrayCommand::Exit),
            _ => {}
        }
    }
}

fn emit(cmd: TrayCommand) {
    if let Some(handler) = HANDLER.get() {
        handler(cmd);
    }
}

fn make_icon(hinstance: Option<HINSTANCE>, health: Health) -> isize {
    let rgba = icon::circle_rgba(ICON_SIZE, health);
    // CreateIcon 的 32 位元圖要 BGRA
    let bgra: Vec<u8> = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[2], p[1], p[0], p[3]])
        .collect();
    // 有 alpha 通道時 AND 遮罩全為 0 即可
    let and_mask = vec![0u8; (ICON_SIZE * ICON_SIZE / 8) as usize];
    unsafe {
        CreateIcon(
            hinstance,
            ICON_SIZE as i32,
            ICON_SIZE as i32,
            1,
            32,
            and_mask.as_ptr(),
            bgra.as_ptr(),
        )
        .map_or(0, |h| h.0 as isize)
    }
}

/// 複製字串到固定長度的 UTF-16 陣列，太長就截斷，保留結尾的 0
fn copy_wide(dst: &mut [u16], s: &str) {
    let max = dst.len() - 1;
    let mut n = 0;
    for c in s.encode_utf16().take(max) {
        dst[n] = c;
        n += 1;
    }
    dst[n] = 0;
}
