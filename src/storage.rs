//! SQLite 記錄：每輪量測樣本與異常事件。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};

use crate::win::wlan::WifiInfo;

/// 樣本保留天數
const RETENTION_DAYS: i64 = 30;

pub struct Storage {
    conn: Connection,
}

pub struct Sample<'a> {
    pub layer: usize,
    pub target: &'a str,
    /// None 代表逾時
    pub rtt_ms: Option<u32>,
}

/// 資料庫位置：%LOCALAPPDATA%\ff14-netmon\netmon.db
pub fn db_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("ff14-netmon").join("netmon.db"))
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

impl Storage {
    pub fn open() -> Result<Self, String> {
        let path = db_path().ok_or("找不到 LOCALAPPDATA 資料夾")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let conn = Connection::open(&path).map_err(|e| e.to_string())?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS samples (
                 ts_ms  INTEGER NOT NULL,
                 layer  INTEGER NOT NULL,
                 target TEXT    NOT NULL,
                 rtt_ms INTEGER          -- NULL 代表逾時
             );
             CREATE INDEX IF NOT EXISTS samples_ts ON samples (ts_ms);
             CREATE TABLE IF NOT EXISTS wifi_samples (
                 ts_ms   INTEGER NOT NULL,
                 ssid    TEXT    NOT NULL,
                 quality INTEGER NOT NULL  -- 訊號品質 0–100
             );
             CREATE INDEX IF NOT EXISTS wifi_samples_ts ON wifi_samples (ts_ms);
             CREATE TABLE IF NOT EXISTS incidents (
                 id        INTEGER PRIMARY KEY,
                 ts_ms     INTEGER NOT NULL,
                 kind      TEXT    NOT NULL,
                 title     TEXT    NOT NULL,
                 diagnosis TEXT    NOT NULL,
                 details   TEXT    NOT NULL
             );",
        )
        .map_err(|e| e.to_string())?;

        let cutoff = now_ms() - RETENTION_DAYS * 24 * 60 * 60 * 1000;
        for table in ["samples", "wifi_samples"] {
            conn.execute(&format!("DELETE FROM {table} WHERE ts_ms < ?1"), [cutoff])
                .map_err(|e| e.to_string())?;
        }
        Ok(Self { conn })
    }

    pub fn record_round(
        &mut self,
        ts_ms: i64,
        samples: &[Sample],
        wifi: Option<&WifiInfo>,
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO samples (ts_ms, layer, target, rtt_ms) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for s in samples {
                stmt.execute(params![ts_ms, s.layer as i64, s.target, s.rtt_ms])?;
            }
        }
        if let Some(w) = wifi {
            tx.execute(
                "INSERT INTO wifi_samples (ts_ms, ssid, quality) VALUES (?1, ?2, ?3)",
                params![ts_ms, w.ssid, w.quality],
            )?;
        }
        tx.commit()
    }

    pub fn record_incident(
        &self,
        ts_ms: i64,
        kind: &str,
        title: &str,
        diagnosis: &str,
        details: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO incidents (ts_ms, kind, title, diagnosis, details)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![ts_ms, kind, title, diagnosis, details],
        )?;
        Ok(())
    }
}
