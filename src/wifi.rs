//! Wi-Fi 狀態：只看目前上網用的那張網卡。SSID 偶爾讀一次，訊號強度每輪讀；
//! WLAN 服務重新啟動或一開始打不開時，會定期重新開啟。

use crate::win::netif::{IfKind, NetIf};
use crate::win::wlan::{self, ERROR_ACCESS_DENIED, Wlan};

/// 每幾輪重讀一次 SSID（30 × 2 秒 = 1 分鐘）
const SSID_REFRESH_ROUNDS: u32 = 30;
/// WLAN 服務打不開時，每幾輪重試一次（15 × 2 秒 = 30 秒）
const REOPEN_ROUNDS: u32 = 15;

#[derive(Clone, PartialEq)]
pub struct WifiInfo {
    /// None 代表 Windows 沒有給位置權限，讀不到網路名稱
    pub ssid: Option<String>,
    /// 訊號品質 0–100
    pub quality: u32,
}

impl WifiInfo {
    pub fn ssid_text(&self) -> &str {
        self.ssid.as_deref().unwrap_or("（名稱需要位置權限）")
    }
}

#[derive(Clone, PartialEq)]
pub enum WifiStatus {
    /// 上網用的不是 Wi-Fi（有線網路、VPN 等）或沒有網路卡資訊
    NotUsed,
    /// 上網用的是無線網卡，但沒有連上 Wi-Fi
    Disconnected,
    Connected(WifiInfo),
    /// 是無線網卡，但讀不到資訊（原因）
    Unreadable(&'static str),
}

impl WifiStatus {
    pub fn info(&self) -> Option<&WifiInfo> {
        match self {
            WifiStatus::Connected(w) => Some(w),
            _ => None,
        }
    }
}

pub const NEED_LOCATION: &str = "Windows 沒有允許這個程式存取位置";
const SERVICE_DOWN: &str = "Windows 的無線網路服務沒有回應";

#[derive(Default)]
pub struct WifiWatcher {
    wlan: Option<Wlan>,
    reopen_in: u32,
    /// 上次讀到的 (網卡 GUID, SSID)
    ssid: Option<(u128, Option<String>)>,
    ssid_refresh_in: u32,
}

impl WifiWatcher {
    pub fn update(&mut self, net_if: Option<&NetIf>) -> WifiStatus {
        let Some(n) = net_if.filter(|n| n.kind == IfKind::Wifi) else {
            self.ssid = None;
            return WifiStatus::NotUsed;
        };
        self.open();
        let Some(wlan) = &self.wlan else {
            return WifiStatus::Unreadable(SERVICE_DOWN);
        };
        match wlan.is_connected(n.guid) {
            Ok(true) => {}
            Ok(false) => {
                self.ssid = None;
                return WifiStatus::Disconnected;
            }
            Err(_) => {
                // handle 可能已經失效（WLAN 服務重新啟動過），下一輪重新開啟
                self.wlan = None;
                return WifiStatus::Unreadable(SERVICE_DOWN);
            }
        }

        let mut quality = None;
        let cached = self.ssid.as_ref().filter(|(guid, _)| *guid == n.guid);
        self.ssid_refresh_in = self.ssid_refresh_in.saturating_sub(1);
        if cached.is_none() || self.ssid_refresh_in == 0 {
            self.ssid_refresh_in = SSID_REFRESH_ROUNDS;
            let ssid = match wlan.connection(n.guid) {
                Ok((ssid, q)) => {
                    quality = Some(q);
                    Some(ssid)
                }
                Err(_) => None,
            };
            self.ssid = Some((n.guid, ssid));
        }
        let ssid = self.ssid.as_ref().and_then(|(_, s)| s.clone());

        match wlan.rssi(n.guid) {
            Ok(rssi) => quality = Some(wlan::rssi_to_quality(rssi)),
            Err(ERROR_ACCESS_DENIED) if quality.is_none() => {
                return WifiStatus::Unreadable(NEED_LOCATION);
            }
            Err(_) => {}
        }
        match quality {
            Some(quality) => WifiStatus::Connected(WifiInfo { ssid, quality }),
            // 讀不到 RSSI 的網卡：退回每輪讀連線資訊
            None => match wlan.connection(n.guid) {
                Ok((ssid, quality)) => WifiStatus::Connected(WifiInfo {
                    ssid: Some(ssid),
                    quality,
                }),
                Err(ERROR_ACCESS_DENIED) => WifiStatus::Unreadable(NEED_LOCATION),
                Err(_) => WifiStatus::Unreadable(SERVICE_DOWN),
            },
        }
    }

    /// 還沒開啟（或之前失效）時開啟 WLAN handle；打不開就隔一陣子再試
    fn open(&mut self) {
        if self.wlan.is_some() {
            return;
        }
        if self.reopen_in > 0 {
            self.reopen_in -= 1;
            return;
        }
        self.wlan = Wlan::open();
        if self.wlan.is_none() {
            self.reopen_in = REOPEN_ROUNDS;
        }
    }
}
