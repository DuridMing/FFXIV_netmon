//! ICMP ping（IcmpSendEcho），不需要系統管理員權限。

use std::ffi::c_void;
use std::net::Ipv4Addr;
use std::time::Duration;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::IpHelper::{
    ICMP_ECHO_REPLY, IP_OPTION_INFORMATION, IP_SUCCESS, IP_TTL_EXPIRED_TRANSIT, IcmpCloseHandle,
    IcmpCreateFile, IcmpSendEcho,
};

const PAYLOAD: [u8; 32] = [0x61; 32];

pub enum EchoResult {
    Reply {
        rtt_ms: u32,
    },
    /// traceroute 用：封包在中途節點 TTL 歸零
    TtlExpired {
        from: Ipv4Addr,
        rtt_ms: u32,
    },
    Timeout,
}

pub struct Icmp(HANDLE);

impl Icmp {
    pub fn new() -> windows::core::Result<Self> {
        unsafe { IcmpCreateFile().map(Self) }
    }

    pub fn echo(&self, dest: Ipv4Addr, ttl: Option<u8>, timeout: Duration) -> EchoResult {
        // 用 u64 確保 ICMP_ECHO_REPLY 對齊；多留空間給 ICMP 錯誤訊息
        let mut buf = vec![0u64; (size_of::<ICMP_ECHO_REPLY>() + PAYLOAD.len() + 64).div_ceil(8)];
        let options = ttl.map(|ttl| IP_OPTION_INFORMATION {
            Ttl: ttl,
            ..Default::default()
        });

        let count = unsafe {
            IcmpSendEcho(
                self.0,
                u32::from_ne_bytes(dest.octets()),
                PAYLOAD.as_ptr() as *const c_void,
                PAYLOAD.len() as u16,
                options.as_ref().map(|o| o as *const _),
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 8) as u32,
                timeout.as_millis() as u32,
            )
        };

        let reply = unsafe { &*(buf.as_ptr() as *const ICMP_ECHO_REPLY) };
        match reply.Status {
            IP_SUCCESS if count > 0 => EchoResult::Reply {
                rtt_ms: reply.RoundTripTime,
            },
            IP_TTL_EXPIRED_TRANSIT if reply.Address != 0 => EchoResult::TtlExpired {
                from: Ipv4Addr::from(reply.Address.to_ne_bytes()),
                rtt_ms: reply.RoundTripTime,
            },
            _ => EchoResult::Timeout,
        }
    }
}

impl Drop for Icmp {
    fn drop(&mut self) {
        unsafe {
            let _ = IcmpCloseHandle(self.0);
        }
    }
}
