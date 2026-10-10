//! ICMP ping（IcmpSendEcho／IcmpSendEcho2），不需要系統管理員權限。

use std::ffi::c_void;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, GetLastError, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::NetworkManagement::IpHelper::{
    ICMP_ECHO_REPLY, IP_OPTION_INFORMATION, IP_SUCCESS, IP_TTL_EXPIRED_TRANSIT, IcmpCloseHandle,
    IcmpCreateFile, IcmpParseReplies, IcmpSendEcho, IcmpSendEcho2,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

const PAYLOAD: [u8; 32] = [0x61; 32];
/// 非同步完成時驅動程式要等這麼久才會放棄；逾時後再多等一點讓它回報
const ASYNC_GRACE: Duration = Duration::from_millis(500);

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

/// 回覆緩衝區：用 u64 確保 ICMP_ECHO_REPLY 對齊；多留空間給 ICMP 錯誤訊息和非同步用的 IO_STATUS_BLOCK
fn reply_buffer() -> Vec<u64> {
    vec![0u64; (size_of::<ICMP_ECHO_REPLY>() + PAYLOAD.len() + 128).div_ceil(8)]
}

fn parse_reply(buf: &[u64], count: u32) -> EchoResult {
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

/// 一個已送出、還在等回覆的非同步 echo
struct Pending {
    event: HANDLE,
    buf: Vec<u64>,
    options: IP_OPTION_INFORMATION,
    sent: bool,
}

pub struct Icmp(HANDLE);

impl Icmp {
    pub fn new() -> windows::core::Result<Self> {
        unsafe { IcmpCreateFile().map(Self) }
    }

    pub fn echo(&self, dest: Ipv4Addr, ttl: Option<u8>, timeout: Duration) -> EchoResult {
        let mut buf = reply_buffer();
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
        parse_reply(&buf, count)
    }

    /// traceroute 用：用同一個 handle 一次送出各 TTL 的 echo（不用每個 TTL 開一條執行緒），
    /// 全部回覆或逾時後依 TTL 順序回傳。
    pub fn echo_ttls(&self, dest: Ipv4Addr, ttls: &[u8], timeout: Duration) -> Vec<EchoResult> {
        // 先全部建好再送出，送出後 Vec 不再變動，緩衝區和選項的位址才會固定
        let mut pending: Vec<Pending> = ttls
            .iter()
            .map(|&ttl| Pending {
                event: unsafe { CreateEventW(None, true, false, None) }.unwrap_or_default(),
                buf: reply_buffer(),
                options: IP_OPTION_INFORMATION {
                    Ttl: ttl,
                    ..Default::default()
                },
                sent: false,
            })
            .collect();

        for p in &mut pending {
            if p.event.is_invalid() {
                continue;
            }
            let ret = unsafe {
                IcmpSendEcho2(
                    self.0,
                    Some(p.event),
                    None,
                    None,
                    u32::from_ne_bytes(dest.octets()),
                    PAYLOAD.as_ptr() as *const c_void,
                    PAYLOAD.len() as u16,
                    Some(&p.options),
                    p.buf.as_mut_ptr() as *mut c_void,
                    (p.buf.len() * 8) as u32,
                    timeout.as_millis() as u32,
                )
            };
            // 有指定 event 時正常會回 0 + ERROR_IO_PENDING；其他錯誤代表沒送出去
            p.sent = ret != 0 || unsafe { GetLastError() } == ERROR_IO_PENDING;
        }

        let deadline = Instant::now() + timeout + ASYNC_GRACE;
        let results = pending
            .iter_mut()
            .map(|p| {
                if !p.sent {
                    return EchoResult::Timeout;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if unsafe { WaitForSingleObject(p.event, left.as_millis() as u32) } != WAIT_OBJECT_0
                {
                    // 驅動程式還可能寫入緩衝區，不能釋放，只好留著
                    std::mem::forget(std::mem::take(&mut p.buf));
                    return EchoResult::Timeout;
                }
                let count = unsafe {
                    IcmpParseReplies(p.buf.as_mut_ptr() as *mut c_void, (p.buf.len() * 8) as u32)
                };
                parse_reply(&p.buf, count)
            })
            .collect();

        for p in &pending {
            if !p.event.is_invalid() {
                unsafe {
                    let _ = CloseHandle(p.event);
                }
            }
        }
        results
    }
}

impl Drop for Icmp {
    fn drop(&mut self) {
        unsafe {
            let _ = IcmpCloseHandle(self.0);
        }
    }
}
