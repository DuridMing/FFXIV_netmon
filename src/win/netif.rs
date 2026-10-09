//! 網路卡狀態（GetIfEntry2）：目前用哪張網卡上網、有沒有連線、速度多少。

use windows::Win32::Foundation::NO_ERROR;
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfEntry2, GetIfTable2, IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211,
    MIB_IF_ROW2, MIB_IF_TABLE2,
};
use windows::Win32::NetworkManagement::Ndis::{
    IfOperStatusUp, MediaConnectStateDisconnected, NET_IF_ADMIN_STATUS_UP,
};

/// MIB_IF_ROW2.InterfaceAndOperStatusFlags 的位元：有實體接頭（實體網卡才有）
const CONNECTOR_PRESENT: u8 = 1 << 2;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum IfKind {
    Ethernet,
    Wifi,
    Other,
}

#[derive(Clone, PartialEq, Debug)]
pub struct NetIf {
    pub index: u32,
    /// 網卡 GUID，用來對應 WLAN API 的無線網卡
    pub guid: u128,
    /// 使用者看得到的名稱，例如「乙太網路」、「Wi-Fi」
    pub alias: String,
    /// 網卡型號
    pub description: String,
    pub kind: IfKind,
    /// 網卡被停用（裝置管理員或網路設定裡關掉）
    pub admin_down: bool,
    /// 實體連線：網路線有插上、Wi-Fi 有連上。
    /// PPPoE 撥號、VPN 這類虛擬網卡回報「未知」，也當作有連線，只有明確回報沒連線才算斷線
    pub media_connected: bool,
    pub oper_up: bool,
    /// 連線速度（bps），取收送較小的那個
    pub speed_bps: u64,
}

impl NetIf {
    pub fn connected(&self) -> bool {
        !self.admin_down && self.media_connected && self.oper_up
    }

    /// 斷線原因，給事件和診斷用
    pub fn down_reason(&self) -> &'static str {
        if self.admin_down {
            "網路卡被停用"
        } else if !self.media_connected {
            match self.kind {
                IfKind::Ethernet => "網路線沒有接上或鬆脫",
                IfKind::Wifi => "Wi-Fi 沒有連線",
                IfKind::Other => "網路卡沒有連線",
            }
        } else {
            "網路卡沒有正常運作"
        }
    }

    /// 例如「乙太網路，1 Gbps」
    pub fn summary(&self) -> String {
        let speed = match self.speed_bps {
            0 => String::new(),
            s if s >= 1_000_000_000 => format!("，{} Gbps", s / 1_000_000_000),
            s => format!("，{} Mbps", s / 1_000_000),
        };
        format!("{}{speed}", self.alias)
    }
}

pub fn interface(index: u32) -> Option<NetIf> {
    let mut row = MIB_IF_ROW2 {
        InterfaceIndex: index,
        ..Default::default()
    };
    if unsafe { GetIfEntry2(&mut row) } != NO_ERROR {
        return None;
    }
    Some(from_row(&row))
}

/// 電腦上的實體有線／無線網卡（不含 VPN、虛擬網卡）
pub fn physical_interfaces() -> Vec<NetIf> {
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    if unsafe { GetIfTable2(&mut table) } != NO_ERROR || table.is_null() {
        return Vec::new();
    }
    let rows = unsafe {
        std::slice::from_raw_parts(
            (&raw const (*table).Table).cast::<MIB_IF_ROW2>(),
            (*table).NumEntries as usize,
        )
    };
    let result = rows
        .iter()
        .filter(|r| r.InterfaceAndOperStatusFlags._bitfield & CONNECTOR_PRESENT != 0)
        .map(from_row)
        .filter(|n| n.kind != IfKind::Other)
        .collect();
    unsafe { FreeMibTable(table as *const _) };
    result
}

fn from_row(row: &MIB_IF_ROW2) -> NetIf {
    let text = |s: &[u16]| {
        let len = s.iter().position(|&c| c == 0).unwrap_or(s.len());
        String::from_utf16_lossy(&s[..len])
    };
    NetIf {
        index: row.InterfaceIndex,
        guid: row.InterfaceGuid.to_u128(),
        alias: text(&row.Alias),
        description: text(&row.Description),
        kind: match row.Type {
            IF_TYPE_ETHERNET_CSMACD => IfKind::Ethernet,
            IF_TYPE_IEEE80211 => IfKind::Wifi,
            _ => IfKind::Other,
        },
        admin_down: row.AdminStatus != NET_IF_ADMIN_STATUS_UP,
        media_connected: row.MediaConnectState != MediaConnectStateDisconnected,
        oper_up: row.OperStatus == IfOperStatusUp,
        speed_bps: row.TransmitLinkSpeed.min(row.ReceiveLinkSpeed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手動執行：cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn print_current_interface() {
        let index = crate::win::route::best_route(std::net::Ipv4Addr::new(1, 1, 1, 1))
            .expect("沒有對外路由")
            .if_index;
        let nif = interface(index).expect("讀不到網卡");
        println!("{nif:?}");
        println!("summary: {}, connected: {}", nif.summary(), nif.connected());
        assert!(nif.connected());
    }
}
