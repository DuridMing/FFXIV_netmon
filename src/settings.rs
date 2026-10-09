//! 使用者設定，存在 %LOCALAPPDATA%\ff14-netmon\settings.txt（每行 key=value）。

use std::fs;
use std::path::PathBuf;

use crate::storage;

#[derive(Clone, Copy, PartialEq)]
pub enum CloseAction {
    /// 按 X 縮到系統匣，繼續在背景監測
    MinimizeToTray,
    /// 按 X 直接結束程式
    Exit,
}

#[derive(Clone, Copy, PartialEq)]
pub struct Settings {
    pub close_action: CloseAction,
    /// 發生異常時跳出通知
    pub notify: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            close_action: CloseAction::MinimizeToTray,
            notify: true,
        }
    }
}

fn path() -> Option<PathBuf> {
    Some(storage::data_dir()?.join("settings.txt"))
}

impl Settings {
    /// 讀取設定；檔案不存在或內容看不懂的項目用預設值
    pub fn load() -> Self {
        path()
            .and_then(|p| fs::read_to_string(p).ok())
            .map_or_else(Self::default, |text| Self::parse(&text))
    }

    fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match (key.trim(), value.trim()) {
                ("close_action", "exit") => s.close_action = CloseAction::Exit,
                ("close_action", "tray") => s.close_action = CloseAction::MinimizeToTray,
                ("notify", v) => s.notify = v != "false",
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) -> Result<(), String> {
        let path = path().ok_or("找不到 LOCALAPPDATA 資料夾")?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let close = match self.close_action {
            CloseAction::MinimizeToTray => "tray",
            CloseAction::Exit => "exit",
        };
        let text = format!("close_action={close}\nnotify={}\n", self.notify);
        fs::write(path, text).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_values_and_ignore_garbage() {
        let s = Settings::parse("close_action=exit\nnotify=false\nunknown=1\nbroken line\n");
        assert!(s.close_action == CloseAction::Exit);
        assert!(!s.notify);

        let s = Settings::parse("close_action=???\n");
        assert!(s == Settings::default());
    }
}
