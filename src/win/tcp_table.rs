//! 讀取系統 TCP 連線表（GetExtendedTcpTable），附帶每條連線的擁有者 PID。

use std::ffi::c_void;
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};

use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_ALL,
};
use windows::Win32::Networking::WinSock::AF_INET;

pub const STATE_ESTABLISHED: u32 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpConn {
    pub local: SocketAddrV4,
    pub remote: SocketAddrV4,
    pub state: u32,
    pub pid: u32,
}

pub fn tcp_connections() -> io::Result<Vec<TcpConn>> {
    let mut size = 0u32;
    // 用 u32 確保表格對齊
    let mut buf: Vec<u32> = Vec::new();

    loop {
        let ptr = (!buf.is_empty()).then_some(buf.as_mut_ptr() as *mut c_void);
        let ret = unsafe {
            GetExtendedTcpTable(
                ptr,
                &mut size,
                false,
                AF_INET.0 as u32,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            )
        };
        if ret == NO_ERROR.0 {
            break;
        }
        if ret != ERROR_INSUFFICIENT_BUFFER.0 {
            return Err(io::Error::from_raw_os_error(ret as i32));
        }
        // 連線數可能在兩次呼叫之間增加，多留一點空間
        buf = vec![0u32; (size as usize).div_ceil(4) + 64];
        size = (buf.len() * 4) as u32;
    }

    let table = unsafe { &*(buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID) };
    let rows =
        unsafe { std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize) };

    Ok(rows
        .iter()
        .map(|r| TcpConn {
            local: sock_addr(r.dwLocalAddr, r.dwLocalPort),
            remote: sock_addr(r.dwRemoteAddr, r.dwRemotePort),
            state: r.dwState,
            pid: r.dwOwningPid,
        })
        .collect())
}

/// 連線表裡的位址和連接埠都是網路位元組順序
fn sock_addr(addr: u32, port: u32) -> SocketAddrV4 {
    SocketAddrV4::new(
        Ipv4Addr::from(addr.to_ne_bytes()),
        u16::from_be(port as u16),
    )
}
