//! Wi-Fi 連線資訊（WlanQueryInterface）：SSID 與訊號強度。

use std::ffi::c_void;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::WiFi::{
    WLAN_CONNECTION_ATTRIBUTES, WLAN_INTERFACE_INFO_LIST, WlanCloseHandle, WlanEnumInterfaces,
    WlanFreeMemory, WlanOpenHandle, WlanQueryInterface, wlan_interface_state_connected,
    wlan_intf_opcode_current_connection,
};

/// WLAN API 版本 2（Vista 以後）
const CLIENT_VERSION: u32 = 2;

#[derive(Clone, PartialEq)]
pub struct WifiInfo {
    pub ssid: String,
    /// 訊號品質 0–100
    pub quality: u32,
}

pub struct Wlan(HANDLE);

impl Wlan {
    /// 沒有無線網卡或 WLAN 服務沒開時回傳 None
    pub fn open() -> Option<Self> {
        let mut version = 0;
        let mut handle = HANDLE::default();
        let ret = unsafe { WlanOpenHandle(CLIENT_VERSION, None, &mut version, &mut handle) };
        (ret == 0).then_some(Self(handle))
    }

    /// 目前連線中的 Wi-Fi；沒有連 Wi-Fi（例如用有線）時回傳 None
    pub fn current(&self) -> Option<WifiInfo> {
        unsafe {
            let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
            if WlanEnumInterfaces(self.0, None, &mut list) != 0 || list.is_null() {
                return None;
            }
            let interfaces = std::slice::from_raw_parts(
                (*list).InterfaceInfo.as_ptr(),
                (*list).dwNumberOfItems as usize,
            );

            let mut result = None;
            for iface in interfaces {
                if iface.isState != wlan_interface_state_connected {
                    continue;
                }
                let mut size = 0u32;
                let mut data: *mut c_void = std::ptr::null_mut();
                let ret = WlanQueryInterface(
                    self.0,
                    &iface.InterfaceGuid,
                    wlan_intf_opcode_current_connection,
                    None,
                    &mut size,
                    &mut data,
                    None,
                );
                if ret != 0 || data.is_null() {
                    continue;
                }
                let attrs = &*(data as *const WLAN_CONNECTION_ATTRIBUTES);
                let assoc = &attrs.wlanAssociationAttributes;
                let len = (assoc.dot11Ssid.uSSIDLength as usize).min(assoc.dot11Ssid.ucSSID.len());
                result = Some(WifiInfo {
                    ssid: String::from_utf8_lossy(&assoc.dot11Ssid.ucSSID[..len]).into_owned(),
                    quality: assoc.wlanSignalQuality,
                });
                WlanFreeMemory(data);
                break;
            }
            WlanFreeMemory(list as *const c_void);
            result
        }
    }
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

    /// 手動執行：cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn print_current_wifi() {
        match Wlan::open() {
            None => println!("WLAN 服務無法開啟（可能沒有無線網卡）"),
            Some(w) => match w.current() {
                Some(info) => println!("SSID={} quality={}", info.ssid, info.quality),
                None => println!("目前沒有連線 Wi-Fi"),
            },
        }
    }
}
