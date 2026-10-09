//! 只允許同時執行一個：用具名 mutex 偵測是否已經有一個在執行。

use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_ABANDONED,
    WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::core::w;

/// 打不開 mutex（存取被拒）時，等待期間每隔多久再試一次
const RETRY_INTERVAL: Duration = Duration::from_millis(200);

/// 持有期間代表「這個程式是唯一在執行的那一個」，結束時自動釋放
pub struct InstanceLock(HANDLE);

pub enum Acquire {
    Acquired(InstanceLock),
    AlreadyRunning,
}

enum Attempt {
    Acquired(InstanceLock),
    /// 已經有另一個在執行
    Busy,
}

/// 取得執行權。`wait_ms` > 0 時，如果已經有另一個在執行，會等它結束再取得
/// （用在「以系統管理員身分重新啟動」或「改用其他繪圖方式重新啟動」：舊的程式正要結束）。
pub fn acquire(wait_ms: u32) -> Acquire {
    let deadline = Instant::now() + Duration::from_millis(wait_ms as u64);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match attempt(remaining.as_millis() as u32) {
            Attempt::Acquired(lock) => return Acquire::Acquired(lock),
            Attempt::Busy if remaining.is_zero() => return Acquire::AlreadyRunning,
            Attempt::Busy => thread::sleep(RETRY_INTERVAL.min(remaining)),
        }
    }
}

fn attempt(wait_ms: u32) -> Attempt {
    // Local\ 是同一個登入工作階段共用，系統管理員和一般權限的程式看得到同一個
    let handle = match unsafe { CreateMutexW(None, true, w!("Local\\ff14-netmon-single-instance")) }
    {
        Ok(h) => h,
        // 已經有一個以系統管理員身分在執行：mutex 存在，但一般權限打不開。這也是「已經在執行」
        Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => return Attempt::Busy,
        // 其他原因建不起來就不限制，至少程式能用
        Err(_) => return Attempt::Acquired(InstanceLock(HANDLE::default())),
    };
    if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
        return Attempt::Acquired(InstanceLock(handle));
    }
    if wait_ms > 0 {
        let result = unsafe { WaitForSingleObject(handle, wait_ms) };
        // 舊的程式結束時沒有釋放（被強制關閉）會得到 WAIT_ABANDONED，一樣算取得
        if result == WAIT_OBJECT_0 || result == WAIT_ABANDONED {
            return Attempt::Acquired(InstanceLock(handle));
        }
    }
    unsafe {
        let _ = CloseHandle(handle);
    }
    Attempt::Busy
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        if self.0.is_invalid() {
            return;
        }
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}
