//! 系統管理員權限：偵測目前是否已提升權限，以及用系統管理員身分重新啟動自己。

use std::ffi::c_void;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, w};

/// 目前程序是否以系統管理員身分執行（UAC 已提升）
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut c_void),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// 用系統管理員身分、以 `args` 為參數重新執行自己（會跳出 UAC 視窗）。
/// 使用者在 UAC 按「否」時回傳 false，呼叫端應該繼續執行原本的程式。
pub fn restart_as_admin(args: &[String]) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let args: Vec<String> = args.iter().map(|a| quote(a)).collect();
    let exe = HSTRING::from(exe.as_os_str());
    let args = HSTRING::from(args.join(" "));
    // ShellExecute 成功時回傳值大於 32
    let result = unsafe { ShellExecuteW(None, w!("runas"), &exe, &args, None, SW_SHOWNORMAL) };
    result.0 as isize > 32
}

/// 依 Windows（MSVC）命令列規則加上引號：引號前和結尾的反斜線要加倍，引號寫成 \"
fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn quotes_like_msvc() {
        assert_eq!(quote("--target"), "--target");
        assert_eq!(quote(r"C:\dir\x"), r"C:\dir\x");
        assert_eq!(quote(r"C:\some dir\"), r#""C:\some dir\\""#);
        assert_eq!(quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote(r#"a\"b c"#), r#""a\\\"b c""#);
        assert_eq!(quote(""), r#""""#);
    }
}
