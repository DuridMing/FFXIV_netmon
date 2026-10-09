//! Win32 API 包裝。所有 unsafe 呼叫集中在這裡，對外只提供安全的 API。

pub mod icmp;
pub mod process;
pub mod route;
pub mod tcp_table;
pub mod time;
