//! 本地時間（GetLocalTime），省掉 chrono 的體積。

use windows::Win32::System::SystemInformation::GetLocalTime;

/// 目前的本地時間，格式 HH:MM:SS
pub fn now_hms() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{:02}:{:02}:{:02}", t.wHour, t.wMinute, t.wSecond)
}
