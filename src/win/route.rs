//! 查詢路由表（GetBestRoute）。

use std::net::Ipv4Addr;

use windows::Win32::Foundation::NO_ERROR;
use windows::Win32::NetworkManagement::IpHelper::{GetBestRoute, MIB_IPFORWARDROW};

/// 連到 `dest` 時的下一跳，通常就是預設閘道（路由器）
pub fn next_hop(dest: Ipv4Addr) -> Option<Ipv4Addr> {
    let mut row = MIB_IPFORWARDROW::default();
    let ret = unsafe { GetBestRoute(u32::from_ne_bytes(dest.octets()), None, &mut row) };
    if ret != NO_ERROR.0 {
        return None;
    }
    let hop = Ipv4Addr::from(row.dwForwardNextHop.to_ne_bytes());
    (!hop.is_unspecified() && hop != dest).then_some(hop)
}
