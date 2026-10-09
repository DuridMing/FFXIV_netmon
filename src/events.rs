//! 異常事件偵測：根據各層的量測視窗判斷何時該記錄一次事件。

use crate::monitor::LAYER_NAMES;
use crate::stats::Window;

/// 判斷用的近期範圍：10 次 × 2 秒 = 20 秒
pub const RECENT: usize = 10;
/// 掉包率超過這個值就觸發，低於 LOSS_OFF 才算恢復（遲滯，避免來回觸發）
const LOSS_ON: f64 = 20.0;
const LOSS_OFF: f64 = 5.0;
/// 遊戲伺服器連續逾時幾次算連不上
const UNREACHABLE_AFTER: usize = 3;
/// 延遲突增：超過近期平均的 3 倍，而且至少 150 ms
const SPIKE_FACTOR: f64 = 3.0;
const SPIKE_MIN_MS: u32 = 150;
/// 冷卻時間（輪數）：15 × 2 秒 = 30 秒內的連鎖異常併成同一次事件
const COOLDOWN_ROUNDS: u32 = 15;

const GAME: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IncidentKind {
    /// 遊戲還開著，但所有連線都斷了
    GameDisconnected,
    /// 遊戲伺服器連續逾時
    GameUnreachable,
    HighLoss(usize),
    LatencySpike(usize),
}

impl IncidentKind {
    pub fn title(self) -> String {
        match self {
            IncidentKind::GameDisconnected => "遊戲連線中斷".into(),
            IncidentKind::GameUnreachable => "遊戲伺服器連不上".into(),
            IncidentKind::HighLoss(i) => format!("{} 大量掉包", LAYER_NAMES[i]),
            IncidentKind::LatencySpike(i) => format!("{} 延遲突增", LAYER_NAMES[i]),
        }
    }

    /// 存進資料庫用的代碼
    pub fn code(self) -> String {
        match self {
            IncidentKind::GameDisconnected => "game_disconnected".into(),
            IncidentKind::GameUnreachable => "game_unreachable".into(),
            IncidentKind::HighLoss(i) => format!("high_loss:{i}"),
            IncidentKind::LatencySpike(i) => format!("latency_spike:{i}"),
        }
    }
}

#[derive(Default)]
pub struct EventDetector {
    unreachable: bool,
    high_loss: [bool; 4],
    spike: [bool; 4],
    /// 距離上次事件還剩幾輪冷卻
    cooldown: u32,
}

impl EventDetector {
    /// 每輪量測後呼叫一次。`game_disconnected` 由連線表偵測，不受冷卻限制。
    pub fn check(
        &mut self,
        windows: [&Window; 4],
        game_disconnected: bool,
    ) -> Option<IncidentKind> {
        self.cooldown = self.cooldown.saturating_sub(1);

        // 每層狀態都要更新，即使在冷卻中，才不會冷卻結束後又補觸發
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
            if self.high_loss[i] && !was {
                lowest_loss.get_or_insert(i);
            }

            let spiking = match (w.last(), s.avg_ms) {
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
        triggered.extend(lowest_spike.map(IncidentKind::LatencySpike));

        let kind = if game_disconnected {
            Some(IncidentKind::GameDisconnected)
        } else if self.cooldown == 0 {
            triggered.first().copied()
        } else {
            None
        };
        if kind.is_some() {
            self.cooldown = COOLDOWN_ROUNDS;
        }
        kind
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
        d.check([&w[0], &w[1], &w[2], &w[3]], disconnected)
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
