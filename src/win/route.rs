//! 查詢路由表（GetBestRoute）。

use std::net::Ipv4Addr;

use windows::Win32::Foundation::NO_ERROR;
use windows::Win32::NetworkManagement::IpHelper::{GetBestRoute, MIB_IPFORWARDROW};

pub struct BestRoute {
    /// 下一跳，通常就是預設閘道（路由器）；目的地在同一個網段時為 None
    pub next_hop: Option<Ipv4Addr>,
    /// 走哪張網卡
    pub if_index: u32,
}

/// 連到 `dest` 會走的路由。沒有路由（網路線拔掉、沒有取得閘道）時回傳 None。
/// 一次查詢同時得到閘道和網卡，兩者才不會因為分開查而短暫不一致
pub fn best_route(dest: Ipv4Addr) -> Option<BestRoute> {
    let mut row = MIB_IPFORWARDROW::default();
    let ret = unsafe { GetBestRoute(u32::from_ne_bytes(dest.octets()), None, &mut row) };
    if ret != NO_ERROR.0 {
        return None;
    }
    let hop = Ipv4Addr::from(row.dwForwardNextHop.to_ne_bytes());
    Some(BestRoute {
        next_hop: (!hop.is_unspecified() && hop != dest).then_some(hop),
        if_index: row.dwForwardIfIndex,
    })
}
