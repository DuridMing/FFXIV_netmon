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

/// 用系統管理員身分重新執行自己（會跳出 UAC 視窗），參數照原樣帶過去。
/// `extra` 是要另外加上的 (參數, 值)，原本就有同名參數時會取代掉。
/// 使用者在 UAC 按「否」時回傳 false，呼叫端應該繼續執行原本的程式。
pub fn restart_as_admin(extra: &[(&str, String)]) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let mut args = Vec::new();
    let mut iter = std::env::args().skip(1);
    while let Some(a) = iter.next() {
        if extra.iter().any(|(flag, _)| *flag == a) {
            iter.next();
        } else if a != "--restarted" {
            args.push(quote(&a));
        }
    }
    for (flag, value) in extra {
        args.push(flag.to_string());
        args.push(quote(value));
    }
    // 加上 --restarted：新的程式會等這個結束後才開始，不會被當成重複開啟
    args.push("--restarted".into());
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
