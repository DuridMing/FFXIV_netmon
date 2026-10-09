//! 檔案總管相關：文件資料夾位置、用預設程式開啟檔案。

use std::path::{Path, PathBuf};

use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;
use windows::Win32::UI::Shell::{
    FOLDERID_Documents, KNOWN_FOLDER_FLAG, SHGetKnownFolderPath, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, w};

/// 使用者的「文件」資料夾（會跟著 OneDrive 等重新導向）
pub fn documents_dir() -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_Documents, KNOWN_FOLDER_FLAG(0), None).ok()?;
        let path = p.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(p.0 as *const _));
        path
    }
}

/// 用預設程式開啟檔案（HTML 會用瀏覽器開）。
/// 透過檔案總管開啟：程式以系統管理員身分執行時，瀏覽器才不會也跟著用系統管理員身分開。
/// 檔案總管要用完整路徑，不然 ShellExecute 會先在目前的工作目錄找 explorer.exe
pub fn open(path: &Path) {
    let Some(explorer) = windows_dir().map(|d| d.join("explorer.exe")) else {
        return;
    };
    let explorer = HSTRING::from(explorer.as_os_str());
    let file = HSTRING::from(format!("\"{}\"", path.display()));
    unsafe {
        ShellExecuteW(None, w!("open"), &explorer, &file, None, SW_SHOWNORMAL);
    }
}

/// Windows 資料夾（通常是 C:\Windows）
pub fn windows_dir() -> Option<PathBuf> {
    let mut buf = [0u16; 260];
    let len = unsafe { GetWindowsDirectoryW(Some(&mut buf)) } as usize;
    (len > 0 && len < buf.len()).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}
