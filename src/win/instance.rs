//! 只允許同時執行一個：用具名 mutex 偵測是否已經有一個在執行。

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows::core::w;

/// 持有期間代表「這個程式是唯一在執行的那一個」，結束時自動釋放
pub struct InstanceLock(HANDLE);

pub enum Acquire {
    Acquired(InstanceLock),
    AlreadyRunning,
}

/// 取得執行權。`wait_ms` > 0 時，如果已經有另一個在執行，會等它結束再取得
/// （用在「以系統管理員身分重新啟動」或「改用其他繪圖方式重新啟動」：舊的程式正要結束）。
pub fn acquire(wait_ms: u32) -> Acquire {
    // Local\ 是同一個登入工作階段共用，系統管理員和一般權限的程式看得到同一個
    let Ok(handle) =
        (unsafe { CreateMutexW(None, true, w!("Local\\ff14-netmon-single-instance")) })
    else {
        // 建不起來就不限制，至少程式能用
        return Acquire::Acquired(InstanceLock(HANDLE::default()));
    };
    if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
        return Acquire::Acquired(InstanceLock(handle));
    }
    if wait_ms > 0 {
        let result = unsafe { WaitForSingleObject(handle, wait_ms) };
        // 舊的程式結束時沒有釋放（被強制關閉）會得到 WAIT_ABANDONED，一樣算取得
        if result == WAIT_OBJECT_0 || result == WAIT_ABANDONED {
            return Acquire::Acquired(InstanceLock(handle));
        }
    }
    unsafe {
        let _ = CloseHandle(handle);
    }
    Acquire::AlreadyRunning
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
