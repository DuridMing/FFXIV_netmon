//! Win32 API 包裝。所有 unsafe 呼叫集中在這裡，對外只提供安全的 API。

pub mod dialog;
pub mod elevation;
pub mod estats;
pub mod icmp;
pub mod instance;
pub mod netif;
pub mod process;
pub mod route;
pub mod shell;
pub mod tcp_table;
pub mod time;
pub mod tray;
pub mod wlan;

/// Win32 結構裡以 NUL 結尾的 UTF-16 字串
fn wide_to_string(s: &[u16]) -> String {
    let len = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..len])
}
