//! Wi-Fi 連線資訊（WLAN API）：網卡是否連線、SSID、訊號強度。
//!
//! Windows 11 24H2 以後，讀 SSID（wlan_intf_opcode_current_connection）會被當成存取位置：
//! 沒有位置權限時回傳 ERROR_ACCESS_DENIED，有權限時每次呼叫也要經過位置服務。
//! 所以 SSID 只偶爾讀一次，每輪的訊號強度改讀 RSSI（便宜，不經過位置服務）。

use std::ffi::c_void;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::WiFi::{
    WLAN_API_VERSION_2_0, WLAN_CONNECTION_ATTRIBUTES, WLAN_INTERFACE_INFO,
    WLAN_INTERFACE_INFO_LIST, WlanCloseHandle, WlanEnumInterfaces, WlanFreeMemory, WlanOpenHandle,
    WlanQueryInterface, wlan_interface_state_connected, wlan_intf_opcode_current_connection,
    wlan_intf_opcode_rssi,
};
use windows::core::GUID;

pub const ERROR_ACCESS_DENIED: u32 = windows::Win32::Foundation::ERROR_ACCESS_DENIED.0;

/// 讀取失敗時的 Windows 錯誤碼
pub type WlanError = u32;

pub struct Wlan(HANDLE);

impl Wlan {
    /// 沒有無線網卡或 WLAN 服務沒開時回傳 None
    pub fn open() -> Option<Self> {
        let mut version = 0;
        let mut handle = HANDLE::default();
        let ret = unsafe { WlanOpenHandle(WLAN_API_VERSION_2_0, None, &mut version, &mut handle) };
        (ret == 0).then_some(Self(handle))
    }

    /// 這張無線網卡（GUID）是否已連上 Wi-Fi。找不到這張網卡時回傳 Ok(false)。
    /// 失敗（例如 WLAN 服務重新啟動過，handle 已失效）時回傳錯誤碼
    pub fn is_connected(&self, guid: u128) -> Result<bool, WlanError> {
        unsafe {
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            let ret = WlanEnumInterfaces(self.0, None, &mut list);
            if ret != 0 || list.is_null() {
                return Err(ret);
            }
            let interfaces = std::slice::from_raw_parts(
                (&raw const (*list).InterfaceInfo).cast::<WLAN_INTERFACE_INFO>(),
                (*list).dwNumberOfItems as usize,
            );
            let connected = interfaces.iter().any(|i| {
                i.InterfaceGuid.to_u128() == guid && i.isState == wlan_interface_state_connected
            });
            WlanFreeMemory(list as *const c_void);
            Ok(connected)
        }
    }

    /// 目前連線的 SSID 和訊號品質（0–100）。24H2 以後沒有位置權限時回傳 Err(ERROR_ACCESS_DENIED)
    pub fn connection(&self, guid: u128) -> Result<(String, u32), WlanError> {
        unsafe {
            let data = self.query(guid, wlan_intf_opcode_current_connection)?;
            let attrs = &*(data as *const WLAN_CONNECTION_ATTRIBUTES);
            let assoc = &attrs.wlanAssociationAttributes;
            let len = (assoc.dot11Ssid.uSSIDLength as usize).min(assoc.dot11Ssid.ucSSID.len());
            let ssid = String::from_utf8_lossy(&assoc.dot11Ssid.ucSSID[..len]).into_owned();
            let quality = assoc.wlanSignalQuality;
            WlanFreeMemory(data);
            Ok((ssid, quality))
        }
    }

    /// 訊號強度（dBm）
    pub fn rssi(&self, guid: u128) -> Result<i32, WlanError> {
        unsafe {
            let data = self.query(guid, wlan_intf_opcode_rssi)?;
            let rssi = *(data as *const i32);
            WlanFreeMemory(data);
            Ok(rssi)
        }
    }

    /// 呼叫端要用 WlanFreeMemory 釋放回傳的資料
    unsafe fn query(
        &self,
        guid: u128,
        opcode: windows::Win32::NetworkManagement::WiFi::WLAN_INTF_OPCODE,
    ) -> Result<*mut c_void, WlanError> {
        let mut size = 0u32;
        let mut data: *mut c_void = std::ptr::null_mut();
        let ret = unsafe {
            WlanQueryInterface(
                self.0,
                &GUID::from_u128(guid),
                opcode,
                None,
                &mut size,
                &mut data,
                None,
            )
        };
        if ret != 0 {
            return Err(ret);
        }
        if data.is_null() {
            return Err(u32::MAX);
        }
        Ok(data)
    }
}

/// RSSI 換算成 0–100 的訊號品質，跟 Windows 的換算方式一樣：-100 dBm 以下為 0，-50 dBm 以上為 100
pub fn rssi_to_quality(rssi: i32) -> u32 {
    ((rssi + 100) * 2).clamp(0, 100) as u32
}

impl Drop for Wlan {
    fn drop(&mut self) {
        unsafe {
            WlanCloseHandle(self.0, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rssi_conversion() {
        assert_eq!(rssi_to_quality(-100), 0);
        assert_eq!(rssi_to_quality(-120), 0);
        assert_eq!(rssi_to_quality(-75), 50);
        assert_eq!(rssi_to_quality(-50), 100);
        assert_eq!(rssi_to_quality(-30), 100);
    }

    /// 手動執行：cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn print_current_wifi() {
        let Some(w) = Wlan::open() else {
            println!("WLAN 服務無法開啟（可能沒有無線網卡）");
            return;
        };
        for n in crate::win::netif::physical_interfaces() {
            if n.kind != crate::win::netif::IfKind::Wifi {
                continue;
            }
            println!(
                "{}: connected={:?} rssi={:?} connection={:?}",
                n.alias,
                w.is_connected(n.guid),
                w.rssi(n.guid),
                w.connection(n.guid).map(|(_, q)| q)
            );
        }
    }
}
