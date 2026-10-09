//! 網路卡狀態（GetIfEntry2）：目前用哪張網卡上網、有沒有連線、速度多少。

use windows::Win32::Foundation::NO_ERROR;
use windows::Win32::NetworkManagement::IpHelper::{
    GetIfEntry2, IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211, MIB_IF_ROW2,
};
use windows::Win32::NetworkManagement::Ndis::{
    IfOperStatusUp, MediaConnectStateConnected, NET_IF_ADMIN_STATUS_UP,
};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum IfKind {
    Ethernet,
    Wifi,
    Other,
}

#[derive(Clone, PartialEq, Debug)]
pub struct NetIf {
    pub index: u32,
    /// 使用者看得到的名稱，例如「乙太網路」、「Wi-Fi」
    pub alias: String,
    /// 網卡型號
    pub description: String,
    pub kind: IfKind,
    /// 網卡被停用（裝置管理員或網路設定裡關掉）
    pub admin_down: bool,
    /// 實體連線：網路線有插上、Wi-Fi 有連上
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
    let text = |s: &[u16]| {
        let len = s.iter().position(|&c| c == 0).unwrap_or(s.len());
        String::from_utf16_lossy(&s[..len])
    };
    Some(NetIf {
        index,
        alias: text(&row.Alias),
        description: text(&row.Description),
        kind: match row.Type {
            IF_TYPE_ETHERNET_CSMACD => IfKind::Ethernet,
            IF_TYPE_IEEE80211 => IfKind::Wifi,
            _ => IfKind::Other,
        },
        admin_down: row.AdminStatus != NET_IF_ADMIN_STATUS_UP,
        media_connected: row.MediaConnectState == MediaConnectStateConnected,
        oper_up: row.OperStatus == IfOperStatusUp,
        speed_bps: row.TransmitLinkSpeed.min(row.ReceiveLinkSpeed),
    })
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
