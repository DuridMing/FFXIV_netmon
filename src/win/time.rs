//! 本地時間（GetLocalTime），省掉 chrono 的體積。

use windows::Win32::System::SystemInformation::GetLocalTime;

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
