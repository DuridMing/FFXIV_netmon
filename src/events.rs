//! 異常事件偵測：根據各層的量測視窗判斷何時該記錄一次事件。

use crate::game_tcp::GameTcpReport;
use crate::monitor::{GAME, INTERVAL, LAYER_NAMES};
use crate::network::NetworkSignal;
use crate::stats::Window;

/// 判斷用的近期範圍：10 次 × 2 秒 = 20 秒
pub const RECENT: usize = 10;
/// 近期範圍的秒數，顯示「近 N 秒」用
pub const RECENT_SECS: u64 = RECENT as u64 * INTERVAL.as_secs();
/// 掉包率超過這個值就觸發，低於 LOSS_OFF 才算恢復（遲滯，避免來回觸發）。
/// 診斷也用這個值判斷哪一層有問題，兩邊才一致
pub const LOSS_ON: f64 = 20.0;
const LOSS_OFF: f64 = 5.0;
/// 遊戲伺服器連續逾時幾次算連不上
const UNREACHABLE_AFTER: usize = 3;
/// 延遲突增：超過近期平均的 3 倍，而且至少 150 ms
const SPIKE_FACTOR: f64 = 3.0;
const SPIKE_MIN_MS: u32 = 150;
/// 冷卻時間（輪數）：15 × 2 秒 = 30 秒內的連鎖異常併成同一次事件
const COOLDOWN_ROUNDS: u32 = 15;
/// 遊戲連線近 20 秒重傳超過這個數就觸發，降到 RETRANS_OFF 以下才算恢復
const RETRANS_ON: u32 = 5;
const RETRANS_OFF: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IncidentKind {
    /// 遊戲還開著，但所有連線都斷了
    GameDisconnected,
    /// 遊戲伺服器連續逾時
    GameUnreachable,
    HighLoss(usize),
    LatencySpike(usize),
    /// 遊戲連線近期重傳變多（需要系統管理員權限才偵測得到）
    GameRetransmits,
    /// 遊戲連線發生重傳逾時（RTO），連續發生會導致 90002 斷線
    GameRtoTimeout,
    /// 電腦本身的網路斷了（網路線鬆脫、Wi-Fi 斷線、網卡被停用）
    NetworkDown,
}

impl IncidentKind {
    pub fn title(self) -> String {
        match self {
            IncidentKind::GameDisconnected => "遊戲連線中斷".into(),
            IncidentKind::GameUnreachable => "遊戲伺服器連不上".into(),
            IncidentKind::HighLoss(i) => format!("{} 大量掉包", LAYER_NAMES[i]),
            IncidentKind::LatencySpike(i) => format!("{} 延遲突增", LAYER_NAMES[i]),
            IncidentKind::GameRetransmits => "遊戲連線重傳增加".into(),
            IncidentKind::GameRtoTimeout => "遊戲連線重傳逾時".into(),
            IncidentKind::NetworkDown => "電腦網路中斷".into(),
        }
    }

    /// 存進資料庫用的代碼
    pub fn code(self) -> String {
        match self {
            IncidentKind::GameDisconnected => "game_disconnected".into(),
            IncidentKind::GameUnreachable => "game_unreachable".into(),
            IncidentKind::HighLoss(i) => format!("high_loss:{i}"),
            IncidentKind::LatencySpike(i) => format!("latency_spike:{i}"),
            IncidentKind::GameRetransmits => "game_retransmits".into(),
            IncidentKind::GameRtoTimeout => "game_rto_timeout".into(),
            IncidentKind::NetworkDown => "network_down".into(),
        }
    }
}

#[derive(Default)]
pub struct EventDetector {
    unreachable: bool,
    high_loss: [bool; 4],
    spike: [bool; 4],
    retransmitting: bool,
    /// 距離上次事件還剩幾輪冷卻
    cooldown: u32,
    /// 遊戲斷線跟電腦網路中斷發生在同一輪：這輪先回報網路中斷，遊戲斷線留到下一輪
    pending_disconnect: bool,
}

impl EventDetector {
    /// 每輪量測後呼叫一次。`game_disconnected` 由連線表偵測，不受冷卻限制。
    /// `network` 是電腦本身的網路狀態（由 NetworkWatcher 判斷）：剛斷線時回報一次「電腦網路中斷」，
    /// 斷線期間各層的掉包都是它造成的，不再另外回報；但遊戲斷線仍然要記錄，才不會漏掉斷線次數。
    /// 冷卻期間開始的問題先不回報，冷卻結束時還持續就會回報（很快就恢復的連鎖異常會被併掉）。
    /// `tcp` 是遊戲連線的 TCP 統計，沒有系統管理員權限時為 None。
    pub fn check(
        &mut self,
        windows: [&Window; 4],
        game_disconnected: bool,
        network: NetworkSignal,
        tcp: Option<&GameTcpReport>,
    ) -> Option<IncidentKind> {
        self.cooldown = self.cooldown.saturating_sub(1);

        let mut triggered = Vec::new();

        let game = windows[GAME];
        let unreachable = game.trailing_losses() >= UNREACHABLE_AFTER;
        if unreachable && !self.unreachable {
            triggered.push(IncidentKind::GameUnreachable);
        }
        self.unreachable = unreachable;

        // 同一輪多層一起出問題時，只回報最底層（上層通常是被連帶影響）
        let mut lowest_loss = None;
        let mut lowest_spike = None;
        for (i, w) in windows.iter().enumerate() {
            let Some(s) = w.summary_last(RECENT) else {
                self.high_loss[i] = false;
                self.spike[i] = false;
                continue;
            };

            let was = self.high_loss[i];
            self.high_loss[i] = if was {
                s.loss_pct > LOSS_OFF
            } else {
                // 樣本太少時掉包率起伏太大（例如 2 筆掉 1 筆就是 50%），累積滿 RECENT 筆才判斷
                w.len() >= RECENT && s.loss_pct >= LOSS_ON
            };
            // 遊戲伺服器已經在「連不上」狀態：掉包是同一件事，不再另外回報（狀態照樣更新，恢復時才不會補報）
            if self.high_loss[i] && !was && !(i == GAME && unreachable) {
                lowest_loss.get_or_insert(i);
            }

            // 跟最新一筆之前的平均比；把最新一筆算進平均的話，實際門檻會比 3 倍高很多
            let baseline = w.summary_before_last(RECENT).and_then(|b| b.avg_ms);
            let spiking = match (w.last(), baseline) {
                (Some(Some(ms)), Some(avg)) => {
                    ms >= SPIKE_MIN_MS && ms as f64 >= avg * SPIKE_FACTOR
                }
                _ => false,
            };
            if spiking && !self.spike[i] {
                lowest_spike.get_or_insert(i);
            }
            self.spike[i] = spiking;
        }
        triggered.extend(lowest_loss.map(IncidentKind::HighLoss));

        if let Some(t) = tcp {
            if t.timeouts > 0 {
                triggered.push(IncidentKind::GameRtoTimeout);
            }
            let was = self.retransmitting;
            self.retransmitting = if was {
                t.retrans_recent > RETRANS_OFF
            } else {
                t.retrans_recent >= RETRANS_ON
            };
            if self.retransmitting && !was {
                triggered.push(IncidentKind::GameRetransmits);
            }
        } else {
            self.retransmitting = false;
        }

        triggered.extend(lowest_spike.map(IncidentKind::LatencySpike));

        let game_disconnected = game_disconnected || std::mem::take(&mut self.pending_disconnect);
        let kind = if network.went_down {
            self.pending_disconnect = game_disconnected;
            Some(IncidentKind::NetworkDown)
        } else if game_disconnected {
            Some(IncidentKind::GameDisconnected)
        } else if network.down {
            None
        } else if self.cooldown == 0 {
            triggered.first().copied()
        } else {
            // 冷卻中：當作還沒觸發，問題持續到冷卻結束就會回報
            for &kind in &triggered {
                self.forget(kind);
            }
            None
        };
        if kind.is_some() {
            self.cooldown = COOLDOWN_ROUNDS;
        }
        kind
    }

    /// 把剛觸發的狀態改回未觸發，下一輪條件還成立時會再觸發一次
    fn forget(&mut self, kind: IncidentKind) {
        match kind {
            IncidentKind::GameUnreachable => self.unreachable = false,
            IncidentKind::HighLoss(i) => self.high_loss[i] = false,
            IncidentKind::LatencySpike(i) => self.spike[i] = false,
            IncidentKind::GameRetransmits => self.retransmitting = false,
            IncidentKind::GameRtoTimeout
            | IncidentKind::GameDisconnected
            | IncidentKind::NetworkDown => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows_from(samples: [&[Option<u32>]; 4]) -> [Window; 4] {
        samples.map(|s| {
            let mut w = Window::new(30);
            for &x in s {
                w.push(x);
            }
            w
        })
    }

    fn check(d: &mut EventDetector, w: &[Window; 4], disconnected: bool) -> Option<IncidentKind> {
        d.check(
            [&w[0], &w[1], &w[2], &w[3]],
            disconnected,
            NetworkSignal::default(),
            None,
        )
    }

    const OK: Option<u32> = Some(10);

    #[test]
    fn game_unreachable_after_three_timeouts_once() {
        let mut d = EventDetector::default();
        let ok = [OK; 10];
        let w = windows_from([&ok, &ok, &ok, &[OK, OK, None, None]]);
        assert_eq!(check(&mut d, &w, false), None);

        let w = windows_from([&ok, &ok, &ok, &[OK, OK, None, None, None]]);
        assert!(matches!(
            check(&mut d, &w, false),
            Some(IncidentKind::GameUnreachable)
        ));
        // 持續逾時不會重複觸發
        let w = windows_from([&ok, &ok, &ok, &[OK, None, None, None, None]]);
        assert_eq!(check(&mut d, &w, false), None);
    }

    #[test]
    fn reports_lowest_layer_when_all_lose_packets() {
        let mut d = EventDetector::default();
        let bad = [OK, OK, OK, None, None, OK, OK, None, OK, OK];
        let w = windows_from([&bad, &bad, &bad, &[OK; 10]]);
        assert!(matches!(
            check(&mut d, &w, false),
            Some(IncidentKind::HighLoss(0))
        ));
    }

    #[test]
    fn cooldown_suppresses_but_disconnect_always_reports() {
        let mut d = EventDetector::default();
        let ok = [OK; 10];
        let bad = [OK, OK, OK, None, None, OK, OK, None, OK, OK];
        let w = windows_from([&bad, &ok, &ok, &ok]);
        assert!(check(&mut d, &w, false).is_some());

        let w = windows_from([&bad, &bad, &ok, &ok]);
        assert_eq!(check(&mut d, &w, false), None);
        assert!(matches!(
            check(&mut d, &w, true),
            Some(IncidentKind::GameDisconnected)
        ));
    }

    #[test]
    fn problem_starting_in_cooldown_is_reported_after_cooldown() {
        let mut d = EventDetector::default();
        let ok = [OK; 10];
        let bad = [OK, OK, OK, None, None, OK, OK, None, OK, OK];
        let w = windows_from([&bad, &ok, &ok, &ok]);
        assert_eq!(check(&mut d, &w, false), Some(IncidentKind::HighLoss(0)));

        // ISP 在冷卻中開始持續掉包：冷卻期間不回報
        let w = windows_from([&bad, &bad, &ok, &ok]);
        for _ in 1..COOLDOWN_ROUNDS {
            assert_eq!(check(&mut d, &w, false), None);
        }
        // 冷卻結束時還在掉包，就要回報
        assert_eq!(check(&mut d, &w, false), Some(IncidentKind::HighLoss(1)));
        // 之後不重複回報
        d.cooldown = 0;
        assert_eq!(check(&mut d, &w, false), None);
    }

    #[test]
    fn unreachable_game_is_not_reported_again_as_high_loss() {
        let mut d = EventDetector::default();
        let ok = [OK; 10];
        let w = windows_from([&ok, &ok, &ok, &[OK, None, None, None]]);
        assert_eq!(
            check(&mut d, &w, false),
            Some(IncidentKind::GameUnreachable)
        );
        // 持續連不上，樣本累積到可以判斷掉包率：同一件事，冷卻結束後也不再回報
        let w = windows_from([&ok, &ok, &ok, &[None; 12]]);
        for _ in 0..COOLDOWN_ROUNDS * 2 {
            assert_eq!(check(&mut d, &w, false), None);
        }
    }

    #[test]
    fn problem_recovered_during_cooldown_is_not_reported() {
        let mut d = EventDetector::default();
        let ok = [OK; 10];
        let bad = [OK, OK, OK, None, None, OK, OK, None, OK, OK];
        let w = windows_from([&bad, &ok, &ok, &ok]);
        assert!(check(&mut d, &w, false).is_some());
        let w = windows_from([&bad, &bad, &ok, &ok]);
        assert_eq!(check(&mut d, &w, false), None);
        let w = windows_from([&ok, &ok, &ok, &ok]);
        for _ in 0..COOLDOWN_ROUNDS * 2 {
            assert_eq!(check(&mut d, &w, false), None);
        }
    }

    #[test]
    fn latency_spike_threshold_is_three_times_previous_average() {
        // 穩定 45 ms 跳到 165 ms：超過 150 ms，也超過之前平均的 3 倍（135 ms）
        let ok = [OK; 10];
        let mut spiky = [Some(45); 11];
        spiky[10] = Some(165);
        let w = windows_from([&ok, &ok, &ok, &spiky]);
        let mut d = EventDetector::default();
        assert_eq!(
            check(&mut d, &w, false),
            Some(IncidentKind::LatencySpike(3))
        );

        // 剛開始只有幾筆樣本時也抓得到
        let w = windows_from([&ok, &ok, &ok, &[Some(45), Some(45), Some(165)]]);
        let mut d = EventDetector::default();
        assert_eq!(
            check(&mut d, &w, false),
            Some(IncidentKind::LatencySpike(3))
        );
    }

    #[test]
    fn latency_spike() {
        let mut d = EventDetector::default();
        let ok = [OK; 10];
        let mut spiky = [Some(40); 10];
        spiky[9] = Some(400);
        let w = windows_from([&ok, &ok, &ok, &spiky]);
        assert!(matches!(
            check(&mut d, &w, false),
            Some(IncidentKind::LatencySpike(3))
        ));
    }

    #[test]
    fn no_incident_when_healthy() {
        let mut d = EventDetector::default();
        let ok = [OK; 10];
        let w = windows_from([&ok, &ok, &ok, &ok]);
        assert_eq!(check(&mut d, &w, false), None);
    }
}

#[cfg(test)]
mod tcp_tests {
    use super::*;

    fn healthy() -> [Window; 4] {
        std::array::from_fn(|_| {
            let mut w = Window::new(30);
            for _ in 0..10 {
                w.push(Some(10));
            }
            w
        })
    }

    fn report(retrans_recent: u32, timeouts: u32) -> GameTcpReport {
        GameTcpReport {
            smoothed_rtt_ms: 50,
            rtt_var_ms: 5,
            retrans: 0,
            timeouts,
            retrans_recent,
            timeouts_recent: timeouts,
            retrans_window: retrans_recent,
            timeouts_window: timeouts,
        }
    }

    #[test]
    fn retransmits_trigger_once_with_hysteresis() {
        let mut d = EventDetector::default();
        let w = healthy();
        let ws = [&w[0], &w[1], &w[2], &w[3]];
        assert_eq!(
            d.check(ws, false, NetworkSignal::default(), Some(&report(2, 0))),
            None
        );
        assert_eq!(
            d.check(ws, false, NetworkSignal::default(), Some(&report(6, 0))),
            Some(IncidentKind::GameRetransmits)
        );
        // 還在重傳就不會重複觸發；降到 1 以下才算恢復
        d.cooldown = 0;
        assert_eq!(
            d.check(ws, false, NetworkSignal::default(), Some(&report(3, 0))),
            None
        );
        d.check(ws, false, NetworkSignal::default(), Some(&report(1, 0)));
        d.cooldown = 0;
        assert_eq!(
            d.check(ws, false, NetworkSignal::default(), Some(&report(5, 0))),
            Some(IncidentKind::GameRetransmits)
        );
    }

    #[test]
    fn rto_timeout_triggers() {
        let mut d = EventDetector::default();
        let w = healthy();
        let ws = [&w[0], &w[1], &w[2], &w[3]];
        assert_eq!(
            d.check(ws, false, NetworkSignal::default(), Some(&report(1, 1))),
            Some(IncidentKind::GameRtoTimeout)
        );
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    #[test]
    fn network_down_reports_once_and_suppresses_layer_loss() {
        let mut d = EventDetector::default();
        let lossy: [Window; 4] = std::array::from_fn(|_| {
            let mut w = Window::new(30);
            for _ in 0..10 {
                w.push(None);
            }
            w
        });
        let ws = [&lossy[0], &lossy[1], &lossy[2], &lossy[3]];
        let went_down = NetworkSignal {
            down: true,
            went_down: true,
        };
        let still_down = NetworkSignal {
            down: true,
            went_down: false,
        };
        assert_eq!(
            d.check(ws, false, went_down, None),
            Some(IncidentKind::NetworkDown)
        );
        // 斷線期間各層都在掉包，不再另外回報
        d.cooldown = 0;
        assert_eq!(d.check(ws, false, still_down, None), None);
        // 但遊戲斷線一定要記錄，不然報告會少算斷線次數
        assert_eq!(
            d.check(ws, true, still_down, None),
            Some(IncidentKind::GameDisconnected)
        );
    }

    #[test]
    fn game_disconnect_in_same_round_as_network_down_is_reported_next_round() {
        let mut d = EventDetector::default();
        let w: [Window; 4] = std::array::from_fn(|_| Window::new(30));
        let ws = [&w[0], &w[1], &w[2], &w[3]];
        let went_down = NetworkSignal {
            down: true,
            went_down: true,
        };
        let still_down = NetworkSignal {
            down: true,
            went_down: false,
        };
        assert_eq!(
            d.check(ws, true, went_down, None),
            Some(IncidentKind::NetworkDown)
        );
        assert_eq!(
            d.check(ws, false, still_down, None),
            Some(IncidentKind::GameDisconnected)
        );
        assert_eq!(d.check(ws, false, still_down, None), None);
    }
}
