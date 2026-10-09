//! 診斷規則：找出最早出問題的層級，推斷問題在哪一段。

use std::net::Ipv4Addr;

use crate::events::IncidentKind;
use crate::monitor::LAYER_NAMES;
use crate::stats::Summary;
use crate::trace::Trace;
use crate::win::netif::NetIf;

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

/// 診斷時參考的額外資訊
#[derive(Default)]
pub struct Context<'a> {
    /// 事件發生時跑的 traceroute
    pub trace: Option<&'a Trace>,
    /// 目前 Wi-Fi 訊號 0–100；用有線網路時為 None
    pub wifi_quality: Option<u32>,
    /// 中間節點追蹤找到的持續掉包起點：(跳數, 位址)
    pub hop_loss_origin: Option<(u8, Option<Ipv4Addr>)>,
    /// 遊戲連線近 20 秒的重傳封包數；沒有系統管理員權限時為 None
    pub retrans_recent: Option<u32>,
    /// 上網用的網卡；網路中斷時是最後一張用過的網卡
    pub net_if: Option<&'a NetIf>,
    /// 電腦網路斷線的原因；None 代表網路正常
    pub network_down: Option<&'a str>,
}

pub fn diagnose(kind: IncidentKind, layers: &[LayerState; 4], ctx: &Context) -> String {
    let mut text = diagnose_network(kind, layers, ctx);
    let wifi_quality = ctx.wifi_quality;
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

fn diagnose_network(kind: IncidentKind, layers: &[LayerState; 4], ctx: &Context) -> String {
    if let Some(reason) = ctx.network_down {
        let card = ctx
            .net_if
            .map(|n| format!("{}：", n.alias))
            .unwrap_or_default();
        return match kind {
            IncidentKind::NetworkDown => format!(
                "電腦本身的網路斷了（{card}{reason}）。請檢查網路線是否鬆脫、Wi-Fi 是否斷線，或網路卡是否被停用。"
            ),
            IncidentKind::GameDisconnected => {
                format!("遊戲在電腦網路中斷期間斷線，是網路中斷造成的（{card}{reason}）。")
            }
            _ => format!("電腦網路目前中斷中（{card}{reason}）。"),
        };
    }
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
            text.push_str(&path_note(ctx));
            text
        }
        None if kind == IncidentKind::GameRetransmits => {
            let n = ctx.retrans_recent.unwrap_or(0);
            format!(
                "遊戲連線本身在掉包：ping 各層都正常，但遊戲連線近 20 秒重傳了 {n} 個封包。{}",
                path_note(ctx)
            )
        }
        None if kind == IncidentKind::GameRtoTimeout => format!(
            "遊戲連線發生重傳逾時（RTO）：封包送出後一直等不到伺服器回應。連續發生會導致 90002 斷線。{}",
            path_note(ctx)
        ),
        None if kind == IncidentKind::GameDisconnected => {
            let mut text =
                "本機網路正常：可能是伺服器主動斷線、伺服器維護，或是正常登出／回到標題畫面。"
                    .to_string();
            if let Some(n) = ctx.retrans_recent.filter(|&n| n > 0) {
                text.push_str(&format!(
                    "不過斷線前 20 秒遊戲連線重傳了 {n} 個封包，網路路徑可能有問題。"
                ));
            }
            text
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

/// 路徑上哪一段有問題：優先用中間節點追蹤的掉包起點，沒有的話用事件當下的 traceroute
fn path_note(ctx: &Context) -> String {
    if let Some((ttl, addr)) = ctx.hop_loss_origin {
        let addr = addr.map(|a| format!("（{a}）")).unwrap_or_default();
        return format!("路由追蹤顯示從第 {ttl} 跳{addr}開始持續掉包，問題很可能出在這一段。");
    }
    match ctx.trace {
        Some(t) => trace_note(t),
        None => String::new(),
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

    fn wifi(q: u32) -> Context<'static> {
        Context {
            wifi_quality: Some(q),
            ..Default::default()
        }
    }

    #[test]
    fn retransmits_with_healthy_ping_points_to_hop() {
        let layers = [good(), good(), good(), good()];
        let ctx = Context {
            retrans_recent: Some(12),
            hop_loss_origin: Some((7, Some("203.0.113.9".parse().unwrap()))),
            ..Default::default()
        };
        let text = diagnose(IncidentKind::GameRetransmits, &layers, &ctx);
        assert!(text.starts_with("遊戲連線本身在掉包"));
        assert!(text.contains("12 個封包"));
        assert!(text.contains("第 7 跳（203.0.113.9）"));
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
        assert!(
            diagnose(IncidentKind::HighLoss(0), &layers, &Context::default())
                .starts_with("家中網路")
        );
    }

    #[test]
    fn isp_loss_with_healthy_gateway() {
        let layers = [good(), state(25.0, Some(5)), good(), good()];
        assert!(
            diagnose(IncidentKind::HighLoss(1), &layers, &Context::default()).starts_with("ISP 端")
        );
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
        let text = diagnose(
            IncidentKind::GameUnreachable,
            &layers,
            &Context {
                trace: Some(&trace),
                ..Default::default()
            },
        );
        assert!(text.starts_with("遊戲伺服器或國際路由"));
        assert!(text.contains("第 5 跳"));
    }

    #[test]
    fn weak_wifi_adds_advice_only_for_home_issue() {
        let home = [state(30.0, None), good(), good(), good()];
        assert!(
            diagnose(IncidentKind::HighLoss(0), &home, &wifi(35)).contains("Wi-Fi 訊號只有 35%")
        );
        assert!(!diagnose(IncidentKind::HighLoss(0), &home, &wifi(80)).contains("Wi-Fi 訊號只有"));
        let isp = [good(), state(30.0, None), good(), good()];
        assert!(!diagnose(IncidentKind::HighLoss(1), &isp, &wifi(35)).contains("Wi-Fi 訊號只有"));
    }

    #[test]
    fn disconnect_with_healthy_network() {
        let layers = [good(), good(), good(), good()];
        assert!(
            diagnose(IncidentKind::GameDisconnected, &layers, &Context::default())
                .starts_with("本機網路正常")
        );
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    fn layers() -> [LayerState; 4] {
        std::array::from_fn(|_| LayerState {
            has_probe: true,
            last: Some(None),
            recent: None,
        })
    }

    #[test]
    fn network_down_and_game_disconnect_mention_reason() {
        let ctx = Context {
            network_down: Some("網路線沒有接上或鬆脫"),
            ..Default::default()
        };
        let down = diagnose(IncidentKind::NetworkDown, &layers(), &ctx);
        assert!(down.starts_with("電腦本身的網路斷了"));
        assert!(down.contains("網路線沒有接上或鬆脫"));
        let game = diagnose(IncidentKind::GameDisconnected, &layers(), &ctx);
        assert!(game.starts_with("遊戲在電腦網路中斷期間斷線"));
    }
}
