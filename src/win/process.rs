//! 依執行檔名稱找程序 PID（CreateToolhelp32Snapshot）。

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

/// 找出名稱符合的程序；無法取得程序清單時回傳 None（不代表程序不存在）
pub fn find_pids(names: &[&str]) -> Option<Vec<u32>> {
    let mut pids = Vec::new();
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?;

    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut ok = unsafe { Process32FirstW(snap, &mut entry) }.is_ok();
    while ok {
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        let exe = String::from_utf16_lossy(&entry.szExeFile[..len]);
        if names.iter().any(|n| exe.eq_ignore_ascii_case(n)) {
            pids.push(entry.th32ProcessID);
        }
        ok = unsafe { Process32NextW(snap, &mut entry) }.is_ok();
    }

    unsafe {
        let _ = CloseHandle(snap);
    }
    Some(pids)
}
