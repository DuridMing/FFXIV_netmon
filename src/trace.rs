//! traceroute：各 TTL 同時送出，約 1 秒就能完成。

use std::net::Ipv4Addr;
use std::thread;
use std::time::Duration;

use crate::win::icmp::{EchoResult, Icmp};

const TIMEOUT: Duration = Duration::from_millis(1000);

pub struct Hop {
    pub ttl: u8,
    /// None 代表這一跳沒有回應
    pub addr: Option<Ipv4Addr>,
    pub rtt_ms: Option<u32>,
}

pub struct Trace {
    pub hops: Vec<Hop>,
    /// 是否到達目的地（目的地可能擋 ICMP，所以沒到達不代表斷線）
    pub reached: bool,
}

impl Trace {
    /// 最後一個有回應的節點
    pub fn last_responding(&self) -> Option<&Hop> {
        self.hops.iter().rev().find(|h| h.addr.is_some())
    }
}

pub fn traceroute(dest: Ipv4Addr, max_hops: u8) -> Trace {
    let results: Vec<EchoResult> = thread::scope(|s| {
        let handles: Vec<_> = (1..=max_hops)
            .map(|ttl| {
                s.spawn(move || match Icmp::new() {
                    Ok(icmp) => icmp.echo(dest, Some(ttl), TIMEOUT),
                    Err(_) => EchoResult::Timeout,
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or(EchoResult::Timeout))
            .collect()
    });

    let mut hops = Vec::new();
    let mut reached = false;
    for (ttl, result) in (1..=max_hops).zip(results) {
        let (addr, rtt_ms) = match result {
            EchoResult::TtlExpired { from, rtt_ms } => (Some(from), Some(rtt_ms)),
            EchoResult::Reply { rtt_ms } => (Some(dest), Some(rtt_ms)),
            EchoResult::Timeout => (None, None),
        };
        hops.push(Hop { ttl, addr, rtt_ms });
        // TTL 夠大的封包都會到達目的地，後面的結果都一樣
        if addr == Some(dest) {
            reached = true;
            break;
        }
    }
    // 去掉結尾連續沒有回應的節點
    if !reached {
        while hops.last().is_some_and(|h| h.addr.is_none()) {
            hops.pop();
        }
    }
    Trace { hops, reached }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 需要網路，手動執行：cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn trace_to_public_dns() {
        let t = traceroute(Ipv4Addr::new(1, 1, 1, 1), 20);
        for h in &t.hops {
            println!("{:>2}  {:?}  {:?}", h.ttl, h.addr, h.rtt_ms);
        }
        assert!(t.reached);
    }
}
