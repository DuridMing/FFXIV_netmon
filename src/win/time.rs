//! 本地時間（GetLocalTime），省掉 chrono 的體積；開機後經過的時間（睡眠偵測用）。

use windows::Win32::System::SystemInformation::{GetLocalTime, GetTickCount64};
use windows::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTime;

/// 目前的本地時間，格式 HH:MM:SS
pub fn now_hms() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{:02}:{:02}:{:02}", t.wHour, t.wMinute, t.wSecond)
}

/// 檔名用的時間戳記，格式 YYYYMMDD-HHMMSS
pub fn now_stamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

/// 今天從午夜到現在經過的秒數（本地時間）
pub fn now_secs_of_day() -> i64 {
    let t = unsafe { GetLocalTime() };
    t.wHour as i64 * 3600 + t.wMinute as i64 * 60 + t.wSecond as i64
}

/// 開機後經過的毫秒數：（包含睡眠的時間, 不含睡眠的時間）。
/// 兩個都不受系統校時影響，相減就是期間睡了多久
pub fn uptime_ms() -> (u64, u64) {
    let total = unsafe { GetTickCount64() };
    let mut unbiased = 0u64;
    let _ = unsafe { QueryUnbiasedInterruptTime(&mut unbiased) };
    (total, unbiased / 10_000)
}
