//! 找出遊戲程序與它連到的伺服器，並追蹤連線變化。

use std::net::SocketAddrV4;

use crate::win::tcp_table::TcpConn;
use crate::win::{process, tcp_table};

pub const GAME_PROCESSES: &[&str] = &["ffxiv_dx11.exe"];

#[derive(Clone, Default)]
pub struct GameSnapshot {
    pub running: bool,
    /// 遊戲目前已建立（ESTABLISHED）的遠端連線
    pub remotes: Vec<SocketAddrV4>,
    /// 同上，但包含本機端位址；讀 TCP 統計時要用完整的連線
    pub conns: Vec<TcpConn>,
}

impl GameSnapshot {
    pub fn conn_to(&self, remote: SocketAddrV4) -> Option<TcpConn> {
        self.conns.iter().find(|c| c.remote == remote).copied()
    }
}

/// 讀取遊戲程序與它的連線。Windows API 暫時失敗時回傳 None（不能當成遊戲關閉或斷線）
pub fn scan() -> Option<GameSnapshot> {
    let pids = process::find_pids(GAME_PROCESSES)?;
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
    let mut remotes: Vec<SocketAddrV4> = conns.iter().map(|c| c.remote).collect();
    remotes.sort();
    remotes.dedup();
    Some(GameSnapshot {
        running: !pids.is_empty(),
        remotes,
        conns,
    })
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
        self.seen.retain(|r| snap.remotes.contains(r));
        for r in &snap.remotes {
            if !self.seen.contains(r) {
                self.seen.push(*r);
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
