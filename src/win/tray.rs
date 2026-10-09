//! 系統匣圖示（Shell_NotifyIcon）：狀態燈號、滑鼠提示、右鍵選單、通知氣泡。
//!
//! 圖示需要一個視窗接收滑鼠訊息，所以另開一條執行緒跑隱藏視窗和訊息迴圈。

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};
use std::thread;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, AppendMenuW, ChangeWindowMessageFilterEx, CreateIcon,
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW, FindWindowW,
    GetCursorPos, GetMessageW, GetWindowThreadProcessId, HICON, KillTimer, MF_STRING, MSG,
    MSGFLT_ALLOW, PostMessageW, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
    SetTimer, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_APP, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
};
use windows::core::{PCWSTR, w};

use crate::icon::{self, Health};

pub enum TrayCommand {
    Show,
    Exit,
}

const WM_TRAY: u32 = WM_APP + 1;
/// 介面執行緒更新了燈號或提示文字，請系統匣執行緒送給檔案總管
const WM_TRAY_UPDATE: u32 = WM_APP + 2;
const TRAY_CLASS: PCWSTR = w!("ff14-netmon-tray");
const TRAY_TITLE: PCWSTR = w!("FF14 連線監測");
const ICON_ID: u32 = 1;
const MENU_SHOW: usize = 1;
const MENU_EXIT: usize = 2;
const ICON_SIZE: u32 = 32;
/// 圖示加入或更新失敗（例如工作列忙碌）時，每隔多久再試一次
const RETRY_TIMER_ID: usize = 1;
const RETRY_MS: u32 = 2000;

/// HWND、HICON 都不是 Send，存成整數才能在執行緒間共用
#[derive(Clone, Copy)]
pub struct Tray {
    hwnd: isize,
    icons: [isize; 4],
}

struct State {
    /// 要顯示的燈號和提示文字
    health: Health,
    tip: String,
    /// 檔案總管目前顯示的燈號和提示文字；None 代表還沒成功送過
    shown: Option<(Health, String)>,
}

static HANDLER: OnceLock<Box<dyn Fn(TrayCommand) + Send + Sync>> = OnceLock::new();
static TRAY: OnceLock<Tray> = OnceLock::new();
static STATE: Mutex<State> = Mutex::new(State {
    health: Health::Unknown,
    tip: String::new(),
    shown: None,
});
/// 檔案總管重啟後會廣播這個訊息，要重新加入圖示
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
/// 圖示目前有沒有成功加到系統匣
static ADDED: AtomicBool = AtomicBool::new(false);
/// 已經送出 WM_TRAY_UPDATE、系統匣執行緒還沒處理
static UPDATE_PENDING: AtomicBool = AtomicBool::new(false);

/// 建立系統匣圖示。使用者點圖示或選單時會呼叫 `handler`（在系統匣執行緒上）。
pub fn spawn(tip: &str, handler: impl Fn(TrayCommand) + Send + Sync + 'static) -> Option<Tray> {
    HANDLER.set(Box::new(handler)).ok()?;
    STATE.lock().ok()?.tip = tip.to_string();

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || unsafe {
        let hinstance = GetModuleHandleW(None).ok().map(|h| HINSTANCE(h.0));
        let class = TRAY_CLASS;
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
            TRAY_TITLE,
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
        // 這個程式以系統管理員身分執行時，一般權限的程式送來的訊息預設會被 UIPI 擋掉；
        // 開放「點圖示」這個訊息，重複開啟時才叫得出這個視窗
        let _ = ChangeWindowMessageFilterEx(hwnd, WM_TRAY, MSGFLT_ALLOW, None);

        let tray = Tray {
            hwnd: hwnd.0 as isize,
            icons: Health::ALL.map(|h| make_icon(hinstance, h)),
        };
        let _ = TRAY.set(tray);
        tray.sync();
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

    fn hwnd(&self) -> HWND {
        HWND(self.hwnd as *mut c_void)
    }

    /// 目前要顯示的狀態。呼叫檔案總管時不能拿著鎖，不然它忙碌時介面執行緒也會被卡住
    fn desired() -> Option<(Health, String)> {
        let state = STATE.lock().ok()?;
        Some((state.health, state.tip.clone()))
    }

    fn mark_shown(shown: Option<(Health, String)>) {
        if let Ok(mut state) = STATE.lock() {
            state.shown = shown;
        }
    }

    fn add(&self) -> bool {
        let Some((health, tip)) = Self::desired() else {
            return false;
        };
        let mut d = self.data();
        d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        d.uCallbackMessage = WM_TRAY;
        d.hIcon = self.icon(health);
        copy_wide(&mut d.szTip, &tip);
        let ok = unsafe { Shell_NotifyIconW(NIM_ADD, &d) }.as_bool();
        ADDED.store(ok, Ordering::SeqCst);
        Self::mark_shown(ok.then_some((health, tip)));
        ok
    }

    /// 把要顯示的燈號和提示文字送給檔案總管；燈號沒變就不重送圖示。成功或不需要更新時回傳 true
    fn apply(&self) -> bool {
        let Some((health, tip)) = Self::desired() else {
            return true;
        };
        let shown = STATE.lock().ok().and_then(|s| s.shown.clone());
        let mut d = self.data();
        if shown.as_ref().is_none_or(|(h, _)| *h != health) {
            d.uFlags |= NIF_ICON;
            d.hIcon = self.icon(health);
        }
        if shown.as_ref().is_none_or(|(_, t)| *t != tip) {
            d.uFlags |= NIF_TIP;
            copy_wide(&mut d.szTip, &tip);
        }
        if d.uFlags.0 == 0 {
            return true;
        }
        let ok = unsafe { Shell_NotifyIconW(NIM_MODIFY, &d) }.as_bool();
        if ok {
            Self::mark_shown(Some((health, tip)));
        }
        ok
    }

    /// 在系統匣執行緒上呼叫：圖示還沒加入就加入，已加入就更新狀態；失敗時開計時器定期重試
    fn sync(&self) {
        let done = if self.is_shown() {
            self.apply()
        } else {
            self.add()
        };
        unsafe {
            if done {
                let _ = KillTimer(Some(self.hwnd()), RETRY_TIMER_ID);
            } else {
                SetTimer(Some(self.hwnd()), RETRY_TIMER_ID, RETRY_MS, None);
            }
        }
    }

    /// 圖示目前是否顯示在系統匣（加入失敗、還在重試時為 false）
    pub fn is_shown(&self) -> bool {
        ADDED.load(Ordering::SeqCst)
    }

    /// 更新燈號顏色和滑鼠提示文字。實際送給檔案總管的工作交給系統匣執行緒，
    /// 檔案總管忙碌時介面才不會卡住；送失敗時系統匣執行緒會自己重試
    pub fn set_status(&self, health: Health, tip: &str) {
        {
            let Ok(mut state) = STATE.lock() else { return };
            if state.health == health && state.tip == tip {
                return;
            }
            state.health = health;
            state.tip = tip.to_string();
        }
        if !UPDATE_PENDING.swap(true, Ordering::SeqCst) {
            unsafe {
                let _ = PostMessageW(Some(self.hwnd()), WM_TRAY_UPDATE, WPARAM(0), LPARAM(0));
            }
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

/// 通知已經在執行的那個程式把視窗叫出來（模擬點一下它的系統匣圖示）。找不到時回傳 false
pub fn show_existing() -> bool {
    unsafe {
        let Ok(hwnd) = FindWindowW(TRAY_CLASS, TRAY_TITLE) else {
            return false;
        };
        // 使用者剛點了 exe，前景權限在這個程式手上；不轉給原本的程式的話，
        // 它的視窗會被 Windows 的前景鎖擋住，只在工作列閃爍，不會跑到最前面
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != 0 {
            let _ = AllowSetForegroundWindow(pid);
        }
        PostMessageW(
            Some(hwnd),
            WM_TRAY,
            WPARAM(0),
            LPARAM(WM_LBUTTONUP as isize),
        )
        .is_ok()
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
    if taskbar_created != 0 && msg == taskbar_created {
        // 檔案總管重新啟動，圖示已經不見了
        ADDED.store(false, Ordering::SeqCst);
    }
    if msg == WM_TRAY_UPDATE {
        UPDATE_PENDING.store(false, Ordering::SeqCst);
    }
    if ((taskbar_created != 0 && msg == taskbar_created)
        || msg == WM_TRAY_UPDATE
        || (msg == WM_TIMER && wparam.0 == RETRY_TIMER_ID))
        && let Some(tray) = TRAY.get()
    {
        tray.sync();
        return LRESULT(0);
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
        // TrackPopupMenu 文件要求：通知區域的選單關閉後送一個空訊息，下次右鍵選單才不會一閃就關掉
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
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
