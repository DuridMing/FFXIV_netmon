//! 診斷規則：找出最早出問題的層級，推斷問題在哪一段。

use crate::events::IncidentKind;
use crate::monitor::LAYER_NAMES;
use crate::stats::Summary;
use crate::trace::Trace;

/// 近期掉包率超過這個值，或最近一次逾時，就算這層有問題
const BAD_LOSS: f64 = 20.0;

const GATEWAY: usize = 0;
const ISP: usize = 1;
const INTERNET: usize = 2;
const GAME: usize = 3;

pub struct LayerState {
    pub has_probe: bool,
    /// 最近一次結果：None = 逾時
    pub last: Option<Option<u32>>,
    /// 近期（約 20 秒）統計
    pub recent: Option<Summary>,
}

impl LayerState {
    fn is_bad(&self) -> bool {
        self.has_probe
            && (matches!(self.last, Some(None))
                || self.recent.is_some_and(|s| s.loss_pct >= BAD_LOSS))
    }
}

/// Wi-Fi 訊號低於這個值，家中網路的問題就很可能是 Wi-Fi 造成的
const WEAK_WIFI: u32 = 50;

/// `wifi_quality`：目前 Wi-Fi 訊號 0–100；用有線網路時為 None
pub fn diagnose(
    kind: IncidentKind,
    layers: &[LayerState; 4],
    trace: Option<&Trace>,
    wifi_quality: Option<u32>,
) -> String {
    let mut text = diagnose_network(kind, layers, trace);
    let home_issue = match kind {
        IncidentKind::LatencySpike(i) => i == GATEWAY,
        _ => layers[GATEWAY].is_bad(),
    };
    if home_issue && let Some(q) = wifi_quality.filter(|&q| q < WEAK_WIFI) {
        text.push_str(&format!(
            "目前 Wi-Fi 訊號只有 {q}%，建議改用有線網路或靠近路由器。"
        ));
    }
    text
}

fn diagnose_network(kind: IncidentKind, layers: &[LayerState; 4], trace: Option<&Trace>) -> String {
    if let IncidentKind::LatencySpike(i) = kind {
        return spike_diagnosis(i);
    }

    match (0..layers.len()).find(|&i| layers[i].is_bad()) {
        Some(GATEWAY) => {
            "家中網路問題：連路由器都掉包。請檢查 Wi-Fi 訊號、網路線，或重開路由器。".into()
        }
        Some(ISP) => {
            "ISP 端問題：路由器正常，但 ISP 節點掉包。若經常發生，可以把記錄拿給 ISP 客服。".into()
        }
        Some(INTERNET) => "ISP 對外網路問題：ISP 節點正常，但連到外部網路掉包。".into(),
        Some(_) => {
            let mut text = format!(
                "遊戲伺服器或國際路由問題：本機到外部網路都正常，只有{}出問題。",
                LAYER_NAMES[GAME]
            );
            if let Some(t) = trace {
                text.push_str(&trace_note(t));
            }
            text
        }
        None if kind == IncidentKind::GameDisconnected => {
            "本機網路正常：可能是伺服器主動斷線、伺服器維護，或是正常登出／回到標題畫面。".into()
        }
        None => "網路目前已恢復正常，問題只持續很短的時間，原因無法判斷。".into(),
    }
}

fn spike_diagnosis(layer: usize) -> String {
    match layer {
        GATEWAY => "家中網路延遲突增：可能是 Wi-Fi 干擾，或家裡有其他裝置大量下載。".into(),
        ISP | INTERNET => "ISP 端延遲突增：家中網路正常，可能是 ISP 網路壅塞。".into(),
        _ => "遊戲伺服器延遲突增：本機到外部網路都正常，可能是伺服器負載高或國際路由壅塞。".into(),
    }
}

fn trace_note(t: &Trace) -> String {
    if t.reached {
        return "traceroute 可以到達伺服器，路徑目前是通的。".into();
    }
    match t.last_responding() {
        Some(hop) => format!(
            "traceroute 最遠只到第 {} 跳（{}），之後沒有回應。",
            hop.ttl,
            hop.addr.map(|a| a.to_string()).unwrap_or_default()
        ),
        None => "traceroute 完全沒有回應。".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::Hop;

    fn state(loss_pct: f64, last: Option<u32>) -> LayerState {
        LayerState {
            has_probe: true,
            last: Some(last),
            recent: Some(Summary {
                avg_ms: Some(10.0),
                jitter_ms: Some(1.0),
                loss_pct,
            }),
        }
    }

    fn good() -> LayerState {
        state(0.0, Some(10))
    }

    #[test]
    fn gateway_loss_means_home_network() {
        let layers = [
            state(30.0, None),
            state(30.0, None),
            state(30.0, None),
            good(),
        ];
        assert!(diagnose(IncidentKind::HighLoss(0), &layers, None, None).starts_with("家中網路"));
    }

    #[test]
    fn isp_loss_with_healthy_gateway() {
        let layers = [good(), state(25.0, Some(5)), good(), good()];
        assert!(diagnose(IncidentKind::HighLoss(1), &layers, None, None).starts_with("ISP 端"));
    }

    #[test]
    fn only_game_bad_includes_trace_note() {
        let layers = [good(), good(), good(), state(50.0, None)];
        let trace = Trace {
            hops: vec![Hop {
                ttl: 5,
                addr: Some("10.0.0.5".parse().unwrap()),
                rtt_ms: Some(8),
            }],
            reached: false,
        };
        let text = diagnose(IncidentKind::GameUnreachable, &layers, Some(&trace), None);
        assert!(text.starts_with("遊戲伺服器或國際路由"));
        assert!(text.contains("第 5 跳"));
    }

    #[test]
    fn weak_wifi_adds_advice_only_for_home_issue() {
        let home = [state(30.0, None), good(), good(), good()];
        assert!(
            diagnose(IncidentKind::HighLoss(0), &home, None, Some(35))
                .contains("Wi-Fi 訊號只有 35%")
        );
        assert!(
            !diagnose(IncidentKind::HighLoss(0), &home, None, Some(80)).contains("Wi-Fi 訊號只有")
        );
        let isp = [good(), state(30.0, None), good(), good()];
        assert!(
            !diagnose(IncidentKind::HighLoss(1), &isp, None, Some(35)).contains("Wi-Fi 訊號只有")
        );
    }

    #[test]
    fn disconnect_with_healthy_network() {
        let layers = [good(), good(), good(), good()];
        assert!(
            diagnose(IncidentKind::GameDisconnected, &layers, None, None)
                .starts_with("本機網路正常")
        );
    }
}
