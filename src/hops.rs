//! 中間節點追蹤（類似 MTR）：每輪對路徑上每一跳送 TTL 遞增的 ICMP，記錄各跳的延遲和掉包。
//!
//! 注意：很多路由器會限制 TTL 逾時回應的頻率，所以某一跳掉包、但後面的節點都正常時，
//! 通常不是真的掉包。只有「從某一跳開始、之後每一跳都掉包」才代表那一段真的有問題。

use std::net::Ipv4Addr;

use crate::monitor::WINDOW_SIZE;
use crate::stats::{Summary, Window};
use crate::trace::Trace;

/// 近期掉包率超過這個值才算這一跳在掉包
const LOSS_PCT: f64 = 20.0;
/// 判斷掉包起點時，至少要有這麼多筆樣本
const MIN_SAMPLES: usize = 10;
/// 每幾輪彙總一次寫進記錄檔（30 × 2 秒 = 1 分鐘）
pub const AGGREGATE_ROUNDS: u32 = 30;
/// 第一次追蹤時最多看幾跳（事件發生時的 traceroute 也用這個）
pub const MAX_HOPS: u8 = 20;
/// 目標本身連得上，但路徑最後幾跳連續這麼多輪沒有回應，就當作路徑變短、那幾跳已經不在路徑上
const STALE_ROUNDS: u32 = 5;

struct HopState {
    ttl: u8,
    addr: Option<Ipv4Addr>,
    window: Window,
    /// 這一分鐘的彙總：(次數, 掉包數, 延遲總和)
    minute: (u32, u32, u64),
    /// 連續幾輪沒有回應
    silent_rounds: u32,
}

#[derive(Clone)]
pub struct HopReport {
    pub ttl: u8,
    /// 最近一次有回應的位址；從來沒回應過時為 None
    pub addr: Option<Ipv4Addr>,
    /// 最近一次結果：None = 逾時
    pub last: Option<u32>,
    pub summary: Option<Summary>,
}

/// 一分鐘的彙總，寫進記錄檔用
pub struct HopAggregate {
    pub ttl: u8,
    pub addr: Option<Ipv4Addr>,
    pub count: u32,
    pub lost: u32,
    pub avg_ms: Option<f64>,
}

#[derive(Default)]
pub struct HopTracker {
    target: Option<Ipv4Addr>,
    hops: Vec<HopState>,
    rounds: u32,
}

impl HopTracker {
    pub fn target(&self) -> Option<Ipv4Addr> {
        self.target
    }

    /// 換目標（例如遊戲換伺服器、斷線）時清掉舊的節點資料。
    /// 舊目標還沒寫進記錄檔的部分（不滿一分鐘）會回傳出來：(舊目標, 彙總)
    pub fn set_target(
        &mut self,
        target: Option<Ipv4Addr>,
    ) -> Option<(Ipv4Addr, Vec<HopAggregate>)> {
        if target == self.target {
            return None;
        }
        let flushed = self
            .target
            .filter(|_| self.rounds > 0)
            .map(|old| (old, self.aggregate()));
        self.target = target;
        self.hops.clear();
        self.rounds = 0;
        flushed
    }

    /// 這一輪要送到第幾跳：已知路徑長度再多看 2 跳，還不知道時看 MAX_HOPS 跳
    pub fn probe_hops(&self) -> u8 {
        match self.hops.len() {
            0 => MAX_HOPS,
            n => (n as u8 + 2).min(MAX_HOPS),
        }
    }

    /// 記錄一輪 traceroute 的結果；每 AGGREGATE_ROUNDS 輪回傳一次彙總。
    /// `target_reachable`：這一輪用 TCP 連得上目標本身（代表路徑是通的）
    pub fn record(&mut self, trace: &Trace, target_reachable: bool) -> Option<Vec<HopAggregate>> {
        // 路徑長度取有史以來回應過的最遠一跳，短暫沒回應時不要把後面的節點丟掉
        let reached_len = trace
            .hops
            .iter()
            .rposition(|h| h.addr.is_some())
            .map_or(0, |i| i + 1);
        while self.hops.len() < reached_len {
            let ttl = self.hops.len() as u8 + 1;
            self.hops.push(HopState {
                ttl,
                addr: None,
                window: Window::new(WINDOW_SIZE),
                minute: (0, 0, 0),
                silent_rounds: 0,
            });
        }
        // 已經到達目的地時，後面多出來的跳數不會再有意義
        if trace.reached {
            self.hops.truncate(trace.hops.len());
        }

        for hop in &mut self.hops {
            let result = trace.hops.get(hop.ttl as usize - 1);
            let rtt = result.and_then(|h| h.rtt_ms);
            if let Some(addr) = result.and_then(|h| h.addr) {
                hop.addr = Some(addr);
            }
            hop.window.push(rtt);
            hop.minute.0 += 1;
            match rtt {
                Some(ms) => {
                    hop.minute.2 += ms as u64;
                    hop.silent_rounds = 0;
                }
                None => {
                    hop.minute.1 += 1;
                    hop.silent_rounds += 1;
                }
            }
        }
        // 遊戲伺服器通常不回 ICMP，traceroute 永遠「沒到達」，路徑變短時舊的跳數會一直逾時，
        // 被誤判成持續掉包。目標本身連得上就代表路徑是通的，最後幾跳一直沒回應就是已經不在路徑上了
        if target_reachable {
            while self
                .hops
                .last()
                .is_some_and(|h| h.silent_rounds >= STALE_ROUNDS)
            {
                self.hops.pop();
            }
        }

        self.rounds += 1;
        if self.rounds < AGGREGATE_ROUNDS {
            return None;
        }
        Some(self.aggregate())
    }

    /// 這一分鐘的彙總，並重新開始累計
    fn aggregate(&mut self) -> Vec<HopAggregate> {
        self.rounds = 0;
        self.hops
            .iter_mut()
            .map(|h| {
                let (count, lost, sum) = std::mem::take(&mut h.minute);
                let ok = count - lost;
                HopAggregate {
                    ttl: h.ttl,
                    addr: h.addr,
                    count,
                    lost,
                    avg_ms: (ok > 0).then(|| sum as f64 / ok as f64),
                }
            })
            .collect()
    }

    pub fn reports(&self) -> Vec<HopReport> {
        self.hops
            .iter()
            .map(|h| HopReport {
                ttl: h.ttl,
                addr: h.addr,
                last: h.window.last().flatten(),
                summary: h.window.summary(),
            })
            .collect()
    }

    /// 掉包起點：從這一跳開始，之後每一跳都在掉包。只有中間某一跳掉包不算（多半是路由器限制回應）
    ///
    /// 從來沒回應過的節點不列入判斷：路徑最後幾跳常常被防火牆擋住、完全不回應 ICMP，
    /// 算進去的話會被誤判成 100% 掉包。另外，掉包的那一段至少要有兩個有回應的節點，
    /// 才能排除單一路由器限制回應的情況。前段某一跳限制回應，不會影響後段真正的掉包判斷。
    pub fn loss_origin(&self) -> Option<(u8, Option<Ipv4Addr>)> {
        let lossy = |h: &&HopState| {
            h.window.len() >= MIN_SAMPLES
                && h.window.summary().is_some_and(|s| s.loss_pct >= LOSS_PCT)
        };
        let responding: Vec<&HopState> = self.hops.iter().filter(|h| h.addr.is_some()).collect();
        // 從最後一跳往前找，連續都在掉包的那一段
        let tail_len = responding.iter().rev().take_while(|h| lossy(h)).count();
        (tail_len >= 2).then(|| {
            let origin = responding[responding.len() - tail_len];
            (origin.ttl, origin.addr)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::Hop;

    fn trace(rtts: &[Option<u32>]) -> Trace {
        Trace {
            hops: rtts
                .iter()
                .enumerate()
                .map(|(i, &rtt)| Hop {
                    ttl: i as u8 + 1,
                    addr: rtt.map(|_| Ipv4Addr::new(10, 0, 0, i as u8 + 1)),
                    rtt_ms: rtt,
                })
                .collect(),
            reached: false,
        }
    }

    #[test]
    fn single_rate_limited_hop_is_not_loss_origin() {
        let mut t = HopTracker::default();
        for i in 0..20 {
            // 第 2 跳一半沒回應，但後面都正常
            let hop2 = if i % 2 == 0 { None } else { Some(5) };
            t.record(&trace(&[Some(1), hop2, Some(8), Some(10)]), true);
        }
        assert_eq!(t.loss_origin(), None);
    }

    #[test]
    fn loss_from_hop_onwards_is_reported() {
        let mut t = HopTracker::default();
        for i in 0..20 {
            let bad = |ms| if i % 3 == 0 { None } else { Some(ms) };
            t.record(&trace(&[Some(1), Some(3), bad(8), bad(10)]), false);
        }
        assert_eq!(t.loss_origin().map(|(ttl, _)| ttl), Some(3));
    }

    #[test]
    fn rate_limited_hop_does_not_hide_later_loss() {
        let mut t = HopTracker::default();
        for i in 0..20 {
            // 第 2 跳限制回應，第 3、4 跳正常，第 5 跳之後真的在掉包
            let hop2 = if i % 2 == 0 { None } else { Some(5) };
            let bad = |ms| if i % 3 == 0 { None } else { Some(ms) };
            t.record(
                &trace(&[Some(1), hop2, Some(8), Some(9), bad(10), bad(11), bad(12)]),
                false,
            );
        }
        assert_eq!(t.loss_origin().map(|(ttl, _)| ttl), Some(5));
    }

    #[test]
    fn hops_no_longer_on_path_are_dropped_when_target_is_reachable() {
        let mut t = HopTracker::default();
        let long = [Some(1), Some(3), Some(8), Some(9), Some(10)];
        for _ in 0..30 {
            t.record(&trace(&long), true);
        }
        // 路徑少了兩跳；遊戲伺服器不回 ICMP，traceroute 永遠沒到達
        for _ in 0..30 {
            t.record(&trace(&long[..3]), true);
        }
        assert_eq!(t.reports().len(), 3);
        assert_eq!(t.loss_origin(), None);
    }

    #[test]
    fn hops_are_kept_when_target_is_unreachable() {
        let mut t = HopTracker::default();
        let long = [Some(1), Some(3), Some(8), Some(9), Some(10)];
        for _ in 0..30 {
            t.record(&trace(&long), false);
        }
        // 第 4 跳之後完全斷掉：這是真的掉包，不能把後面的節點丟掉
        for _ in 0..20 {
            t.record(&trace(&long[..3]), false);
        }
        assert_eq!(t.reports().len(), 5);
        assert_eq!(t.loss_origin().map(|(ttl, _)| ttl), Some(4));
    }

    #[test]
    fn changing_target_returns_partial_minute() {
        let mut t = HopTracker::default();
        let a = Ipv4Addr::new(203, 0, 113, 1);
        t.set_target(Some(a));
        for _ in 0..5 {
            t.record(&trace(&[Some(1), Some(3)]), true);
        }
        let (old, agg) = t.set_target(None).expect("should flush partial minute");
        assert_eq!(old, a);
        assert_eq!(agg.len(), 2);
        assert_eq!(agg[0].count, 5);
        assert!(t.set_target(None).is_none());
    }

    #[test]
    fn silent_tail_is_not_loss_origin() {
        let mut t = HopTracker::default();
        for i in 0..20 {
            // 第 3 跳是最後一個會回應的節點而且偶爾掉包，後面兩跳被防火牆擋住、從來不回應。
            // 沒有後面的節點可以佐證，不能斷定是第 3 跳開始掉包
            let hop3 = if i % 3 == 0 { None } else { Some(8) };
            t.record(&trace(&[Some(1), Some(3), hop3, None, None]), false);
        }
        assert_eq!(t.reports().len(), 3);
        assert_eq!(t.loss_origin(), None);
    }

    #[test]
    fn keeps_hops_when_tail_stops_answering() {
        let mut t = HopTracker::default();
        t.record(&trace(&[Some(1), Some(3), Some(8)]), false);
        // traceroute 會把結尾沒回應的節點去掉，但路徑長度要維持
        t.record(&trace(&[Some(1)]), false);
        let reports = t.reports();
        assert_eq!(reports.len(), 3);
        assert_eq!(reports[2].last, None);
        assert!(reports[2].addr.is_some());
    }

    #[test]
    fn aggregates_every_minute() {
        let mut t = HopTracker::default();
        let mut agg = None;
        for _ in 0..AGGREGATE_ROUNDS {
            agg = t.record(&trace(&[Some(2), None]), false);
        }
        let agg = agg.expect("should aggregate after AGGREGATE_ROUNDS");
        assert_eq!(agg[0].count, AGGREGATE_ROUNDS);
        assert_eq!(agg[0].avg_ms, Some(2.0));
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// 需要網路，手動執行：cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn track_public_dns_for_a_few_rounds() {
        let mut t = HopTracker::default();
        t.set_target(Some(Ipv4Addr::new(1, 1, 1, 1)));
        for _ in 0..5 {
            let trace = crate::trace::traceroute(t.target().unwrap(), t.probe_hops());
            t.record(&trace, false);
        }
        for h in t.reports() {
            println!(
                "{:>2}  {:<16} last={:?} loss={:.0}%",
                h.ttl,
                h.addr.map_or("*".into(), |a| a.to_string()),
                h.last,
                h.summary.map_or(0.0, |s| s.loss_pct)
            );
        }
        println!("probe_hops after discovery: {}", t.probe_hops());
        assert!(!t.reports().is_empty());
    }
}
