//! 依執行檔名稱找程序 PID（CreateToolhelp32Snapshot），以及檢查程序是否還在執行。

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};

use super::wide_to_string;

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
        let exe = wide_to_string(&entry.szExeFile);
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

/// 持有程序的 handle，用來便宜地檢查它是否還在執行（不用每次都列舉所有程序）。
/// 持有 handle 期間 PID 不會被別的程序重複使用
pub struct ProcessHandle {
    pid: u32,
    handle: HANDLE,
}

impl ProcessHandle {
    pub fn open(pid: u32) -> Option<Self> {
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }.ok()?;
        Some(Self { pid, handle })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn is_running(&self) -> bool {
        unsafe { WaitForSingleObject(self.handle, 0) == WAIT_TIMEOUT }
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}
