//! 單一 TCP 連線的擴充統計（Extended Statistics）：重傳、RTO 逾時、實際 RTT。
//!
//! 要先用 SetPerTcpConnectionEStats 對連線啟用收集，這一步需要系統管理員權限；
//! 啟用後計數從 0 開始累加。只讀取 OS 的統計，不碰遊戲的封包。

use windows::Win32::NetworkManagement::IpHelper::{
    GetPerTcpConnectionEStats, MIB_TCPROW_LH, MIB_TCPROW_LH_0, SetPerTcpConnectionEStats,
    TCP_ESTATS_PATH_ROD_v0, TCP_ESTATS_PATH_RW_v0, TcpConnectionEstatsPath,
};

use super::tcp_table::TcpConn;

pub const ERROR_ACCESS_DENIED: u32 = windows::Win32::Foundation::ERROR_ACCESS_DENIED.0;

/// 連線啟用收集以來的累計值；RTT 單位都是毫秒
#[derive(Clone, Copy, Debug, Default)]
pub struct PathStats {
    pub smoothed_rtt_ms: u32,
    pub rtt_var_ms: u32,
    /// 重傳的封包數
    pub pkts_retrans: u32,
    /// 重傳逾時（RTO）次數。連續逾時是 90002 斷線的前兆
    pub timeouts: u32,
}

fn row(conn: &TcpConn) -> MIB_TCPROW_LH {
    MIB_TCPROW_LH {
        Anonymous: MIB_TCPROW_LH_0 {
            dwState: conn.state,
        },
        dwLocalAddr: u32::from_ne_bytes(conn.local.ip().octets()),
        dwLocalPort: conn.local.port().to_be() as u32,
        dwRemoteAddr: u32::from_ne_bytes(conn.remote.ip().octets()),
        dwRemotePort: conn.remote.port().to_be() as u32,
    }
}

fn as_bytes<T>(v: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v as *const T as *const u8, size_of::<T>()) }
}

fn as_bytes_mut<T>(v: &mut T) -> &mut [u8] {
    unsafe { std::slice::from_raw_parts_mut(v as *mut T as *mut u8, size_of::<T>()) }
}

/// 對連線啟用路徑統計收集。沒有系統管理員權限時回傳 Err(ERROR_ACCESS_DENIED)
pub fn enable(conn: &TcpConn) -> Result<(), u32> {
    let rw = TCP_ESTATS_PATH_RW_v0 {
        EnableCollection: true,
    };
    let ret = unsafe {
        SetPerTcpConnectionEStats(&row(conn), TcpConnectionEstatsPath, as_bytes(&rw), 0, 0)
    };
    if ret == 0 { Ok(()) } else { Err(ret) }
}

pub fn read(conn: &TcpConn) -> Result<PathStats, u32> {
    let mut rod = TCP_ESTATS_PATH_ROD_v0::default();
    let ret = unsafe {
        GetPerTcpConnectionEStats(
            &row(conn),
            TcpConnectionEstatsPath,
            None,
            0,
            None,
            0,
            Some(as_bytes_mut(&mut rod)),
            0,
        )
    };
    if ret != 0 {
        return Err(ret);
    }
    Ok(PathStats {
        smoothed_rtt_ms: rod.SmoothedRtt,
        rtt_var_ms: rod.RttVar,
        pkts_retrans: rod.PktsRetrans,
        timeouts: rod.Timeouts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::tcp_table::{self, STATE_ESTABLISHED};

    /// 手動執行：cargo test -- --ignored --nocapture
    /// 一般權限下啟用應該回傳 ERROR_ACCESS_DENIED；系統管理員下應該讀得到數值
    #[test]
    #[ignore]
    fn enable_on_some_established_connection() {
        let conn = tcp_table::tcp_connections()
            .unwrap()
            .into_iter()
            .find(|c| c.state == STATE_ESTABLISHED && !c.remote.ip().is_loopback())
            .expect("需要至少一條對外的 TCP 連線");
        println!("conn: {} -> {}", conn.local, conn.remote);
        match enable(&conn) {
            Ok(()) => println!("enabled, read = {:?}", read(&conn)),
            Err(e) => {
                println!("enable error = {e}");
                assert_eq!(e, ERROR_ACCESS_DENIED);
            }
        }
    }
}
