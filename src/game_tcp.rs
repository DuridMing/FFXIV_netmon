//! 遊戲連線的 TCP 統計：實際 RTT、重傳、RTO 逾時（需要系統管理員權限）。

use std::collections::VecDeque;

use crate::events::RECENT;
use crate::win::estats::{self, ERROR_ACCESS_DENIED, PathStats};
use crate::win::tcp_table::TcpConn;

/// 統計視窗：30 次 × 2 秒 = 60 秒
const WINDOW_SIZE: usize = 30;

#[derive(Clone, Copy)]
pub struct GameTcpReport {
    /// 遊戲連線實際的平滑 RTT（OS 根據遊戲封包算的，最接近遊戲裡感受到的延遲）
    pub smoothed_rtt_ms: u32,
    pub rtt_var_ms: u32,
    /// 這一輪新增的重傳封包數與 RTO 逾時次數
    pub retrans: u32,
    pub timeouts: u32,
    /// 近 20 秒（RECENT 輪）的合計
    pub retrans_recent: u32,
    pub timeouts_recent: u32,
    /// 近 60 秒的合計
    pub retrans_60s: u32,
    pub timeouts_60s: u32,
}

#[derive(Clone, Copy)]
pub enum TcpStatus {
    /// 沒有系統管理員權限，無法啟用統計
    NotElevated,
    /// 沒有遊戲連線
    NoConnection,
    /// 啟用或讀取失敗（Windows 錯誤碼）
    Error(u32),
    Stats(GameTcpReport),
}

#[derive(Default)]
pub struct GameTcpTracker {
    conn: Option<TcpConn>,
    enabled: bool,
    prev: Option<PathStats>,
    /// 每輪新增的 (重傳, 逾時)
    window: VecDeque<(u32, u32)>,
}

impl GameTcpTracker {
    pub fn update(&mut self, conn: Option<TcpConn>, elevated: bool) -> TcpStatus {
        if !elevated {
            return TcpStatus::NotElevated;
        }
        let Some(conn) = conn else {
            *self = Self::default();
            return TcpStatus::NoConnection;
        };
        if self.conn != Some(conn) {
            *self = Self {
                conn: Some(conn),
                ..Self::default()
            };
        }
        if !self.enabled {
            if let Err(e) = estats::enable(&conn) {
                return TcpStatus::Error(e);
            }
            self.enabled = true;
        }

        let stats = match estats::read(&conn) {
            Ok(s) => s,
            Err(e) => {
                // 權限被收回之類的情況，下一輪重新啟用
                if e == ERROR_ACCESS_DENIED {
                    self.enabled = false;
                }
                return TcpStatus::Error(e);
            }
        };
        // 計數是累加的，第一次讀取只當作基準
        let (retrans, timeouts) = match self.prev {
            Some(p) => (
                stats.pkts_retrans.saturating_sub(p.pkts_retrans),
                stats.timeouts.saturating_sub(p.timeouts),
            ),
            None => (0, 0),
        };
        self.prev = Some(stats);
        if self.window.len() == WINDOW_SIZE {
            self.window.pop_front();
        }
        self.window.push_back((retrans, timeouts));

        let sum = |n: usize| {
            self.window
                .iter()
                .rev()
                .take(n)
                .fold((0, 0), |(r, t), &(dr, dt)| (r + dr, t + dt))
        };
        let (retrans_recent, timeouts_recent) = sum(RECENT);
        let (retrans_60s, timeouts_60s) = sum(WINDOW_SIZE);
        TcpStatus::Stats(GameTcpReport {
            smoothed_rtt_ms: stats.smoothed_rtt_ms,
            rtt_var_ms: stats.rtt_var_ms,
            retrans,
            timeouts,
            retrans_recent,
            timeouts_recent,
            retrans_60s,
            timeouts_60s,
        })
    }
}
