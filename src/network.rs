//! 電腦本身的網路狀態：目前用哪張網卡、路由器（預設閘道）、ISP 節點，
//! 以及斷線、恢復、換網卡的偵測。網路斷線／恢復的狀態只在這裡記錄。

use std::net::Ipv4Addr;

use crate::monitor::INTERNET_TARGET;
use crate::trace;
use crate::win::netif::{self, NetIf};
use crate::win::route;

/// 找 ISP 節點只需要看前幾跳
const ISP_TRACE_HOPS: u8 = 8;
/// 有網路但找不到 ISP 節點時，每幾輪重找一次（15 × 2 秒 = 30 秒）
const ISP_RETRY_ROUNDS: u32 = 15;
/// 剛恢復連線或剛換網路時 traceroute 可能還不通，比較快再找一次（3 × 2 秒 = 6 秒）
const ISP_QUICK_RETRY_ROUNDS: u32 = 3;

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
}

pub struct NetworkWatcher {
    pub gateway: Option<Ipv4Addr>,
    pub isp: Option<Ipv4Addr>,
    /// 最後一張有路由的網卡；斷線時用它查斷線原因
    last_if: Option<NetIf>,
    down_reason: Option<String>,
    isp_retry_in: u32,
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
            down_reason: None,
            isp_retry_in: ISP_RETRY_ROUNDS,
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
        let was_down = self.down_reason.is_some();
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

        let mut targets_changed = false;
        if !is_down && let Some(r) = &route {
            let if_changed = self.last_if.as_ref().map(|n| n.index) != Some(r.if_index);
            // 換網卡、換路由器，或斷線後恢復（可能已經換到另一個網路，只是路由器 IP 剛好一樣）：
            // 重新找 ISP 節點，不然會一直量舊的目標。
            // PPPoE 撥號、VPN 這類直接連線的路由沒有路由器（next_hop 是 None），路由器那層就不量
            if r.next_hop != self.gateway || if_changed || was_down {
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
        self.down_reason = down_reason.clone();
        NetworkRound {
            net_if,
            down_reason,
            signal: NetworkSignal {
                down: is_down,
                went_down: is_down && !was_down,
            },
            events,
            targets_changed,
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
