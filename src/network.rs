//! 電腦本身的網路狀態：目前用哪張網卡、路由器（預設閘道）、ISP 節點，
//! 以及斷線、恢復、換網卡的偵測。網路斷線／恢復的狀態只在這裡記錄。

use std::net::Ipv4Addr;
use std::time::Instant;

use crate::monitor::INTERNET_TARGET;
use crate::trace;
use crate::win::netif::{self, IfKind, NetIf, fmt_speed};
use crate::win::{route, time};

/// 找 ISP 節點只需要看前幾跳
const ISP_TRACE_HOPS: u8 = 8;
/// 有網路但找不到 ISP 節點時，每幾輪重找一次（15 × 2 秒 = 30 秒）
const ISP_RETRY_ROUNDS: u32 = 15;
/// 剛恢復連線或剛換網路時 traceroute 可能還不通，比較快再找一次（3 × 2 秒 = 6 秒）
const ISP_QUICK_RETRY_ROUNDS: u32 = 3;

/// 本機流量取最近幾輪的最大值（3 × 2 秒 = 6 秒）：大量傳輸常在延遲突增前一點開始
const TRAFFIC_ROUNDS: usize = 3;
/// 本機流量超過這個值就算「大量傳輸」。一般家用網路上傳頻寬比下載小很多，上傳門檻較低
const HEAVY_RX_BPS: u64 = 20_000_000;
const HEAVY_TX_BPS: u64 = 5_000_000;
/// 有線網卡曾經有 1 Gbps 以上、後來掉到 100 Mbps 以下，就是降速了
const FAST_LINK_BPS: u64 = 1_000_000_000;
const SLOW_LINK_BPS: u64 = 100_000_000;
/// 降速要連續這麼多輪（3 × 2 秒 = 6 秒）才回報：睡眠恢復或重新接上時，網卡可能短暫協商在較低速度
const SLOW_LINK_ROUNDS: u32 = 3;
/// 有線網卡這麼久沒連上（包含睡眠時間，5 分鐘）就重新記錄最高速度：
/// 筆電可能已經帶到別的地方，接上只支援 100 Mbps 的交換器不是網路線的問題
const LINK_FORGET_MS: u64 = 5 * 60_000;

const NO_ROUTE: &str = "網路卡有連線，但沒有取得對外的路由（可能是沒有拿到閘道，或 VPN 正在切換）";
const NO_ADAPTER: &str = "沒有任何網路卡連上網路（網路線沒有接上、Wi-Fi 沒有連線，或網路卡被停用）";

/// 給異常偵測用的網路狀態
#[derive(Clone, Copy, Default)]
pub struct NetworkSignal {
    /// 目前斷線中
    pub down: bool,
    /// 這一輪剛斷線
    pub went_down: bool,
}

pub struct NetworkRound {
    /// 目前上網用的網卡；沒有路由時是最後一張用過的網卡
    pub net_if: Option<NetIf>,
    /// 斷線原因；None 代表網路正常
    pub down_reason: Option<String>,
    pub signal: NetworkSignal,
    /// 要顯示在事件列表的訊息：(文字, 是否嚴重)
    pub events: Vec<(String, bool)>,
    /// 路由器或 ISP 節點換了，呼叫端要更新對應層的量測目標
    pub targets_changed: bool,
    /// 換了網卡、路由器，或斷線後恢復：到遊戲伺服器的路徑本來就會變，不算 ISP 改路由
    pub path_reset: bool,
    /// 本機網卡最近幾輪的流量；還沒有兩次讀數時為 None
    pub traffic: Option<Throughput>,
}

/// 網卡流量（bps）
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Throughput {
    pub rx_bps: u64,
    pub tx_bps: u64,
}

impl Throughput {
    /// 這台電腦正在大量上傳或下載（Steam、Windows Update、直播等）
    pub fn heavy(self) -> bool {
        self.rx_bps >= HEAVY_RX_BPS || self.tx_bps >= HEAVY_TX_BPS
    }

    /// 例如「下載 35.2 Mbps、上傳 0.4 Mbps」
    pub fn text(self) -> String {
        format!(
            "下載 {:.1} Mbps、上傳 {:.1} Mbps",
            self.rx_bps as f64 / 1e6,
            self.tx_bps as f64 / 1e6
        )
    }
}

/// 用網卡的收送位元組計數算出流量，保留最近幾輪的結果
#[derive(Default)]
struct TrafficMeter {
    /// 上一次的讀數：(網卡, 收, 送, 時間)
    last: Option<(u32, u64, u64, Instant)>,
    /// 上一次讀數時計數器倒退了：網卡斷線、重新連線時計數器可能暫時變小再跳回累計值，
    /// 跳回來的那一輪差值不是真的流量，所以倒退後的下一輪也不算
    after_reset: bool,
    recent: [Option<Throughput>; TRAFFIC_ROUNDS],
    next: usize,
}

impl TrafficMeter {
    fn update(&mut self, net_if: Option<&NetIf>, now: Instant) -> Option<Throughput> {
        // 沒有連線的網卡計數器不可靠，不當作比較基準
        let net_if = net_if.filter(|n| n.connected());
        let current = net_if.map(|n| (n.index, n.in_octets, n.out_octets, now));
        let mut reset = false;
        let rate = match (self.last, current) {
            // 換網卡時這一輪沒有流量資料
            (Some((i0, rx0, tx0, t0)), Some((i1, rx1, tx1, t1))) if i0 == i1 => {
                if rx1 < rx0 || tx1 < tx0 {
                    // 計數器歸零（網卡重新啟用）或暫時變小
                    reset = true;
                    None
                } else if self.after_reset {
                    None
                } else {
                    let secs = t1.duration_since(t0).as_secs_f64();
                    (secs > 0.0)
                        .then(|| Throughput {
                            rx_bps: ((rx1 - rx0) as f64 * 8.0 / secs) as u64,
                            tx_bps: ((tx1 - tx0) as f64 * 8.0 / secs) as u64,
                        })
                        .filter(|r| net_if.is_some_and(|n| plausible(r, n.speed_bps)))
                }
            }
            _ => None,
        };
        self.after_reset = reset;
        self.last = current;
        self.recent[self.next] = rate;
        self.next = (self.next + 1) % TRAFFIC_ROUNDS;
        self.recent
            .iter()
            .flatten()
            .copied()
            .reduce(|a, b| Throughput {
                rx_bps: a.rx_bps.max(b.rx_bps),
                tx_bps: a.tx_bps.max(b.tx_bps),
            })
    }
}

/// 流量不可能超過網卡連線速度太多（連線速度是收送兩個方向中較低的那個，所以留兩倍的空間）；
/// 超過就是計數器不連續，這一輪不算。VPN 等虛擬網卡沒有連線速度，不檢查
fn plausible(rate: &Throughput, speed_bps: u64) -> bool {
    let limit = speed_bps.saturating_mul(2);
    speed_bps == 0 || (rate.rx_bps <= limit && rate.tx_bps <= limit)
}

/// 有線網卡連線速度下降（多半是網路線或接頭接觸不良）
#[derive(Default)]
struct LinkSpeedWatch {
    /// (網卡, 看過的最高速度)
    best: Option<(u32, u64)>,
    /// 最後一次看到有線網卡連上的時間（開機後毫秒數，包含睡眠）
    last_seen_ms: Option<u64>,
    /// 連續幾輪是降速的狀態
    slow_rounds: u32,
    degraded: bool,
}

impl LinkSpeedWatch {
    /// 換了網路（例如換路由器）：之前的最高速度不能拿來比
    fn reset(&mut self) {
        *self = Self::default();
    }

    /// 降速或恢復時回傳要顯示的訊息。`now_ms` 是開機後的毫秒數（包含睡眠）
    fn update(&mut self, net_if: Option<&NetIf>, now_ms: u64) -> Option<(String, bool)> {
        let n =
            net_if.filter(|n| n.kind == IfKind::Ethernet && n.connected() && n.speed_bps > 0)?;
        if self
            .last_seen_ms
            .is_some_and(|t| now_ms.saturating_sub(t) >= LINK_FORGET_MS)
        {
            self.reset();
        }
        self.last_seen_ms = Some(now_ms);
        let best = match self.best {
            Some((index, best)) if index == n.index => best.max(n.speed_bps),
            _ => {
                self.degraded = false;
                self.slow_rounds = 0;
                n.speed_bps
            }
        };
        self.best = Some((n.index, best));
        let slow = best >= FAST_LINK_BPS && n.speed_bps <= SLOW_LINK_BPS;
        self.slow_rounds = if slow { self.slow_rounds + 1 } else { 0 };
        let degraded = slow && (self.degraded || self.slow_rounds >= SLOW_LINK_ROUNDS);
        let was = std::mem::replace(&mut self.degraded, degraded);
        match (was, degraded) {
            (false, true) => Some((
                format!(
                    "有線網路速度從 {} 掉到 {}：可能是網路線或接頭接觸不良，請重插兩端或換一條網路線",
                    fmt_speed(best),
                    fmt_speed(n.speed_bps)
                ),
                true,
            )),
            (true, false) => Some((
                format!("有線網路速度恢復為 {}", fmt_speed(n.speed_bps)),
                false,
            )),
            _ => None,
        }
    }
}

/// 有線網卡目前只有 100 Mbps 以下（網卡或路由器本身也可能只支援這個速度，所以只當提示）
pub fn slow_wired_link(n: &NetIf) -> bool {
    n.kind == IfKind::Ethernet && n.speed_bps > 0 && n.speed_bps <= SLOW_LINK_BPS
}

pub struct NetworkWatcher {
    pub gateway: Option<Ipv4Addr>,
    pub isp: Option<Ipv4Addr>,
    /// 最後一張有路由的網卡；斷線時用它查斷線原因
    last_if: Option<NetIf>,
    /// 上一輪是否斷線中
    was_down: bool,
    isp_retry_in: u32,
    traffic: TrafficMeter,
    link_speed: LinkSpeedWatch,
}

impl NetworkWatcher {
    /// 啟動時找路由器和 ISP 節點（traceroute 約 1 秒）
    pub fn detect() -> Self {
        let route = route::best_route(INTERNET_TARGET);
        let gateway = route.as_ref().and_then(|r| r.next_hop);
        Self {
            gateway,
            isp: find_isp_hop(gateway),
            last_if: route.and_then(|r| netif::interface(r.if_index)),
            was_down: false,
            isp_retry_in: ISP_RETRY_ROUNDS,
            traffic: TrafficMeter::default(),
            link_speed: LinkSpeedWatch::default(),
        }
    }

    pub fn update(&mut self) -> NetworkRound {
        let route = route::best_route(INTERNET_TARGET);
        // 有路由時查路由用的那張網卡（讀不到就當作未知，不要沿用舊網卡的資料）；
        // 沒有路由時查最後一張用過的網卡，才知道是網路線拔掉還是只是沒有路由。
        // 一開始就沒有網路（還沒用過任何網卡）時，從實體網卡裡找出原因
        let net_if = match &route {
            Some(r) => netif::interface(r.if_index),
            None => match &self.last_if {
                Some(n) => netif::interface(n.index),
                None => offline_interface(),
            },
        };
        let down_reason = match (&route, &net_if) {
            (_, Some(n)) if !n.connected() => Some(n.down_reason().to_string()),
            (Some(_), _) => None,
            (None, Some(_)) => Some(NO_ROUTE.to_string()),
            (None, None) => Some(NO_ADAPTER.to_string()),
        };

        let mut events = Vec::new();
        let was_down = self.was_down;
        let is_down = down_reason.is_some();
        if was_down && !is_down {
            let card = net_if
                .as_ref()
                .map(|n| format!("：{}", n.summary()))
                .unwrap_or_default();
            events.push((format!("網路連線恢復{card}"), false));
        }
        if let (false, Some(old), Some(new)) = (is_down, &self.last_if, &net_if)
            && route.is_some()
            && old.index != new.index
        {
            events.push((format!("改用 {} 上網", new.summary()), false));
        }

        // 換了路由器就是換了網路，不是同一條網路線降速
        if !is_down && route.as_ref().is_some_and(|r| r.next_hop != self.gateway) {
            self.link_speed.reset();
        }
        if !is_down && let Some(e) = self.link_speed.update(net_if.as_ref(), time::uptime_ms().0) {
            events.push(e);
        }

        let mut targets_changed = false;
        let mut path_reset = false;
        if !is_down && let Some(r) = &route {
            let if_changed = self.last_if.as_ref().map(|n| n.index) != Some(r.if_index);
            // 換網卡、換路由器，或斷線後恢復（可能已經換到另一個網路，只是路由器 IP 剛好一樣）：
            // 重新找 ISP 節點，不然會一直量舊的目標。
            // PPPoE 撥號、VPN 這類直接連線的路由沒有路由器（next_hop 是 None），路由器那層就不量
            if r.next_hop != self.gateway || if_changed || was_down {
                path_reset = true;
                let (old_gateway, old_isp) = (self.gateway, self.isp);
                self.gateway = r.next_hop;
                self.isp = find_isp_hop(self.gateway);
                self.isp_retry_in = if self.isp.is_some() {
                    ISP_RETRY_ROUNDS
                } else {
                    ISP_QUICK_RETRY_ROUNDS
                };
                let isp = self.isp.map_or("找不到".into(), |h| h.to_string());
                if self.gateway != old_gateway {
                    targets_changed = true;
                    events.push((
                        format!(
                            "路由器變更為 {}，ISP 節點：{isp}",
                            fmt_gateway(self.gateway)
                        ),
                        false,
                    ));
                } else if self.isp != old_isp {
                    targets_changed = true;
                    events.push((format!("ISP 節點變更為 {isp}"), false));
                }
            } else if self.isp.is_none() {
                // 啟動時沒網路或剛換路由器時 traceroute 還不通，過一陣子再找一次
                self.isp_retry_in = self.isp_retry_in.saturating_sub(1);
                if self.isp_retry_in == 0 {
                    self.isp_retry_in = ISP_RETRY_ROUNDS;
                    self.isp = find_isp_hop(self.gateway);
                    if let Some(h) = self.isp {
                        targets_changed = true;
                        events.push((format!("找到 ISP 節點：{h}"), false));
                    }
                }
            }
        }

        if route.is_some()
            && let Some(n) = &net_if
        {
            self.last_if = Some(n.clone());
        }
        self.was_down = is_down;
        let traffic = self
            .traffic
            .update(net_if.as_ref().filter(|_| route.is_some()), Instant::now());
        NetworkRound {
            net_if,
            down_reason,
            signal: NetworkSignal {
                down: is_down,
                went_down: is_down && !was_down,
            },
            events,
            targets_changed,
            path_reset,
            traffic,
        }
    }
}

/// 路由器位址；沒有路由器時說明可能的原因
fn fmt_gateway(a: Option<Ipv4Addr>) -> String {
    a.map_or("無（電腦直接撥號或使用 VPN）".into(), |a| {
        a.to_string()
    })
}

/// 一開始就沒有網路時，找出最可能是哪張網卡的問題：
/// 有連線的那張（只是沒有路由）優先；只有一張實體網卡時就是它；有好幾張都沒連線就無法判斷
fn offline_interface() -> Option<NetIf> {
    let cards: Vec<NetIf> = netif::physical_interfaces()
        .into_iter()
        .filter(|n| !n.admin_down)
        .collect();
    if let Some(n) = cards.iter().find(|n| n.connected()) {
        return Some(n.clone());
    }
    match <[NetIf; 1]>::try_from(cards) {
        Ok([n]) => Some(n),
        Err(_) => None,
    }
}

/// traceroute，回傳路由器之後第一個公開 IP 的節點；找不到就退而求其次用第一個私有 IP 節點
fn find_isp_hop(gateway: Option<Ipv4Addr>) -> Option<Ipv4Addr> {
    let hops = trace::traceroute(INTERNET_TARGET, ISP_TRACE_HOPS).hops;
    let candidates: Vec<Ipv4Addr> = hops
        .iter()
        .filter_map(|h| h.addr)
        .filter(|&a| Some(a) != gateway && a != INTERNET_TARGET)
        .collect();
    candidates
        .iter()
        .find(|a| !a.is_private())
        .or(candidates.first())
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn card(index: u32, speed_bps: u64, rx: u64, tx: u64) -> NetIf {
        NetIf {
            index,
            guid: 0,
            alias: "乙太網路".into(),
            description: "x".into(),
            kind: IfKind::Ethernet,
            admin_down: false,
            media_connected: true,
            oper_up: true,
            speed_bps,
            in_octets: rx,
            out_octets: tx,
        }
    }

    #[test]
    fn traffic_rate_and_recent_peak() {
        let mut m = TrafficMeter::default();
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let idle = card(1, 0, 10_000_000, 250_000);
        assert_eq!(m.update(Some(&card(1, 0, 0, 0)), at(0)), None);
        // 2 秒收 10 MB = 40 Mbps
        let r = m.update(Some(&idle), at(2)).unwrap();
        assert_eq!((r.rx_bps, r.tx_bps), (40_000_000, 1_000_000));
        assert!(r.heavy());
        // 之後沒有流量：最近 3 輪內還記得剛才的高峰，超過就忘掉
        m.update(Some(&idle), at(4));
        assert_eq!(m.update(Some(&idle), at(6)).unwrap().rx_bps, 40_000_000);
        assert!(!m.update(Some(&idle), at(8)).unwrap().heavy());
    }

    #[test]
    fn traffic_ignores_adapter_change_and_counter_reset() {
        let t0 = Instant::now();
        let mut m = TrafficMeter::default();
        m.update(Some(&card(1, 0, 1_000_000_000, 0)), t0);
        assert_eq!(
            m.update(Some(&card(2, 0, 0, 0)), t0 + Duration::from_secs(2)),
            None
        );
        let mut m = TrafficMeter::default();
        m.update(Some(&card(1, 0, 5_000, 0)), t0);
        assert_eq!(
            m.update(Some(&card(1, 0, 10, 0)), t0 + Duration::from_secs(2)),
            None
        );
    }

    #[test]
    fn traffic_ignores_counter_jumping_back() {
        // 重新連線時計數器暫時變小，下一輪又跳回累計值：跳回來的差值不是流量
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let mut m = TrafficMeter::default();
        m.update(Some(&card(1, 0, 1_000_000_000, 0)), at(0));
        assert_eq!(m.update(Some(&card(1, 0, 0, 0)), at(2)), None);
        assert_eq!(m.update(Some(&card(1, 0, 1_000_000_000, 0)), at(4)), None);
        // 之後恢復正常計算
        let r = m
            .update(Some(&card(1, 0, 1_010_000_000, 0)), at(6))
            .unwrap();
        assert_eq!(r.rx_bps, 40_000_000);
    }

    #[test]
    fn traffic_rejects_rate_above_link_speed() {
        let t0 = Instant::now();
        let mut m = TrafficMeter::default();
        let gbps = 1_000_000_000;
        m.update(Some(&card(1, gbps, 0, 0)), t0);
        // 2 秒收 1 GB = 4 Gbps，網卡只有 1 Gbps
        assert_eq!(
            m.update(
                Some(&card(1, gbps, 1_000_000_000, 0)),
                t0 + Duration::from_secs(2)
            ),
            None
        );
        // 接近連線速度的真實流量照算
        let r = m
            .update(
                Some(&card(1, gbps, 1_200_000_000, 0)),
                t0 + Duration::from_secs(4),
            )
            .unwrap();
        assert_eq!(r.rx_bps, 800_000_000);
    }

    #[test]
    fn traffic_skips_disconnected_adapter() {
        let t0 = Instant::now();
        let mut m = TrafficMeter::default();
        let mut down = card(1, 0, 0, 0);
        down.media_connected = false;
        m.update(Some(&down), t0);
        // 斷線時的讀數不當基準：重新連上的第一輪只記下讀數
        assert_eq!(
            m.update(
                Some(&card(1, 0, 1_000_000_000, 0)),
                t0 + Duration::from_secs(2)
            ),
            None
        );
    }

    #[test]
    fn link_speed_drop_reported_once_then_recovery() {
        let mut w = LinkSpeedWatch::default();
        assert!(w.update(Some(&card(1, 1_000_000_000, 0, 0)), 0).is_none());
        let slow = card(1, 100_000_000, 0, 0);
        // 要連續 SLOW_LINK_ROUNDS 輪才回報
        for i in 1..SLOW_LINK_ROUNDS {
            assert!(w.update(Some(&slow), i as u64 * 2_000).is_none());
        }
        let (text, severe) = w.update(Some(&slow), 10_000).unwrap();
        assert!(severe && text.contains("1 Gbps") && text.contains("100 Mbps"));
        assert!(w.update(Some(&slow), 12_000).is_none());
        assert!(
            w.update(Some(&card(1, 1_000_000_000, 0, 0)), 14_000)
                .is_some_and(|(_, s)| !s)
        );
    }

    #[test]
    fn link_speed_ignores_brief_renegotiation() {
        let mut w = LinkSpeedWatch::default();
        w.update(Some(&card(1, 1_000_000_000, 0, 0)), 0);
        // 睡眠恢復時短暫協商在 100 Mbps，幾秒後回到 1 Gbps
        assert!(w.update(Some(&card(1, 100_000_000, 0, 0)), 2_000).is_none());
        assert!(w.update(Some(&card(1, 100_000_000, 0, 0)), 4_000).is_none());
        assert!(
            w.update(Some(&card(1, 1_000_000_000, 0, 0)), 6_000)
                .is_none()
        );
    }

    #[test]
    fn link_speed_forgets_after_long_disconnect() {
        let mut w = LinkSpeedWatch::default();
        w.update(Some(&card(1, 1_000_000_000, 0, 0)), 0);
        // 拔掉網路線（或睡眠）10 分鐘後接到 100 Mbps 的交換器：不是降速
        let slow = card(1, 100_000_000, 0, 0);
        for i in 0..SLOW_LINK_ROUNDS * 2 {
            assert!(w.update(Some(&slow), 600_000 + i as u64 * 2_000).is_none());
        }
        // 短暫斷線（網路線接觸不良）後掉到 100 Mbps：還是要回報
        let mut w = LinkSpeedWatch::default();
        w.update(Some(&card(1, 1_000_000_000, 0, 0)), 0);
        let reported = (0..SLOW_LINK_ROUNDS)
            .any(|i| w.update(Some(&slow), 20_000 + i as u64 * 2_000).is_some());
        assert!(reported);
    }

    #[test]
    fn link_speed_ignores_other_adapter_and_wifi() {
        let mut w = LinkSpeedWatch::default();
        w.update(Some(&card(1, 1_000_000_000, 0, 0)), 0);
        // 換到另一張只有 100 Mbps 的網卡：不是降速
        for i in 1..=SLOW_LINK_ROUNDS * 2 {
            assert!(
                w.update(Some(&card(2, 100_000_000, 0, 0)), i as u64 * 2_000)
                    .is_none()
            );
        }
        let mut wifi = card(3, 1_000_000_000, 0, 0);
        wifi.kind = IfKind::Wifi;
        let mut w = LinkSpeedWatch::default();
        w.update(Some(&wifi), 0);
        wifi.speed_bps = 50_000_000;
        assert!(w.update(Some(&wifi), 2_000).is_none());
    }

    #[test]
    fn speed_text_keeps_decimal() {
        assert_eq!(fmt_speed(1_000_000_000), "1 Gbps");
        assert_eq!(fmt_speed(2_500_000_000), "2.5 Gbps");
        assert_eq!(fmt_speed(10_000_000_000), "10 Gbps");
        assert_eq!(fmt_speed(100_000_000), "100 Mbps");
        assert_eq!(fmt_speed(866_700_000), "866 Mbps");
    }
}
