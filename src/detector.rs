//! 找出遊戲程序與它連到的伺服器，並追蹤連線變化。

use std::net::SocketAddrV4;

use crate::win::process::{self, ProcessHandle};
use crate::win::tcp_table::{self, TcpConn};

pub const GAME_PROCESSES: &[&str] = &["ffxiv_dx11.exe"];
/// 遊戲沒在執行時，每幾輪列舉一次所有程序找遊戲（3 × 2 秒 = 6 秒）
const SEARCH_ROUNDS: u32 = 3;
/// 遊戲執行中時，每幾輪重新列舉一次，找出後來才開的其他遊戲視窗（30 × 2 秒 = 1 分鐘）
const RESCAN_ROUNDS: u32 = 30;

#[derive(Clone, Default)]
pub struct GameSnapshot {
    pub running: bool,
    /// 遊戲目前已建立（ESTABLISHED）的連線，依遠端位址排序
    pub conns: Vec<TcpConn>,
}

impl GameSnapshot {
    pub fn conn_to(&self, remote: SocketAddrV4) -> Option<TcpConn> {
        self.conns.iter().find(|c| c.remote == remote).copied()
    }
}

/// 找到的遊戲程序。打不開 handle 時只記 PID，那就每輪都要重新列舉程序才知道它還在不在
enum GameProcess {
    Handle(ProcessHandle),
    Pid(u32),
}

impl GameProcess {
    fn pid(&self) -> u32 {
        match self {
            GameProcess::Handle(h) => h.pid(),
            GameProcess::Pid(pid) => *pid,
        }
    }
}

/// 找遊戲程序與它的連線。列舉所有程序很花 CPU，找到遊戲後改用 handle 檢查它是否還在執行
#[derive(Default)]
pub struct GameScanner {
    games: Vec<GameProcess>,
    rescan_in: u32,
}

impl GameScanner {
    /// Windows API 暫時失敗時回傳 None（不能當成遊戲關閉或斷線）
    pub fn scan(&mut self) -> Option<GameSnapshot> {
        self.games.retain(|g| match g {
            GameProcess::Handle(h) => h.is_running(),
            GameProcess::Pid(_) => true,
        });
        if self.games.is_empty() {
            // 遊戲剛關閉：回到比較頻繁的搜尋
            self.rescan_in = self.rescan_in.min(SEARCH_ROUNDS);
        }
        let pid_only = self.games.iter().any(|g| matches!(g, GameProcess::Pid(_)));
        self.rescan_in = self.rescan_in.saturating_sub(1);
        if self.rescan_in == 0 || pid_only {
            let pids = process::find_pids(GAME_PROCESSES)?;
            self.games.retain(|g| pids.contains(&g.pid()));
            for pid in pids {
                if !self.games.iter().any(|g| g.pid() == pid) {
                    self.games.push(match ProcessHandle::open(pid) {
                        Some(h) => GameProcess::Handle(h),
                        None => GameProcess::Pid(pid),
                    });
                }
            }
            self.rescan_in = if self.games.is_empty() {
                SEARCH_ROUNDS
            } else {
                RESCAN_ROUNDS
            };
        }

        let pids: Vec<u32> = self.games.iter().map(GameProcess::pid).collect();
        let mut conns = Vec::new();
        if !pids.is_empty() {
            conns = tcp_table::tcp_connections()
                .ok()?
                .into_iter()
                .filter(|c| {
                    pids.contains(&c.pid)
                        && c.state == tcp_table::STATE_ESTABLISHED
                        && !c.remote.ip().is_loopback()
                })
                .collect();
        }
        conns.sort_by_key(|c| c.remote);
        Some(GameSnapshot {
            running: !pids.is_empty(),
            conns,
        })
    }
}

pub enum GameEvent {
    Started,
    Exited,
    Connected(SocketAddrV4),
    /// 原本監測的連線結束，改監測另一條（例如登入後大廳連線關閉）
    TargetChanged(SocketAddrV4),
    /// 遊戲還開著，但所有連線都斷了
    AllConnectionsLost,
}

#[derive(Default)]
pub struct GameTracker {
    running: bool,
    /// 依第一次出現的順序排列；最早出現且仍存在的那條當監測目標
    seen: Vec<SocketAddrV4>,
    target: Option<SocketAddrV4>,
}

impl GameTracker {
    pub fn running(&self) -> bool {
        self.running
    }

    pub fn target(&self) -> Option<SocketAddrV4> {
        self.target
    }

    pub fn update(&mut self, snap: &GameSnapshot) -> Vec<GameEvent> {
        let mut events = Vec::new();
        match (self.running, snap.running) {
            (false, true) => events.push(GameEvent::Started),
            (true, false) => events.push(GameEvent::Exited),
            _ => {}
        }
        self.running = snap.running;

        let had_conns = !self.seen.is_empty();
        self.seen.retain(|r| snap.conn_to(*r).is_some());
        for c in &snap.conns {
            if !self.seen.contains(&c.remote) {
                self.seen.push(c.remote);
            }
        }
        if snap.running && had_conns && self.seen.is_empty() {
            events.push(GameEvent::AllConnectionsLost);
        }

        let new_target = self.seen.first().copied();
        if new_target != self.target {
            if let Some(t) = new_target {
                events.push(match self.target {
                    None => GameEvent::Connected(t),
                    Some(_) => GameEvent::TargetChanged(t),
                });
            }
            self.target = new_target;
        }
        events
    }
}
