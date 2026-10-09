//! 依執行檔名稱找程序 PID（CreateToolhelp32Snapshot）。

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

pub fn find_pids(names: &[&str]) -> Vec<u32> {
    let mut pids = Vec::new();
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return pids;
    };

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
    pids
}
