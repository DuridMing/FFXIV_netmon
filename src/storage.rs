//! SQLite 記錄：每輪量測樣本與異常事件。

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};

use crate::game_tcp::GameTcpReport;
use crate::hops::HopAggregate;
use crate::win::wlan::WifiInfo;

/// 樣本保留天數
const RETENTION_DAYS: i64 = 30;
/// 另一個連線（例如清除紀錄）正在寫入時，最多等這麼久，不要直接回報失敗
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Storage {
    conn: Connection,
}

pub struct Sample<'a> {
    pub layer: usize,
    pub target: &'a str,
    /// None 代表逾時
    pub rtt_ms: Option<u32>,
}

/// 程式資料夾：%LOCALAPPDATA%\ff14-netmon
pub fn data_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("ff14-netmon"))
}

/// 資料庫位置：%LOCALAPPDATA%\ff14-netmon\netmon.db
pub fn db_path() -> Option<PathBuf> {
    Some(data_dir()?.join("netmon.db"))
}

/// 清除所有量測樣本、Wi-Fi 樣本和事件，並壓縮資料庫檔案。
/// 背景量測用的是另一個連線，可以同時進行。
pub fn clear_all() -> Result<(), String> {
    clear_db(&db_path().ok_or("找不到 LOCALAPPDATA 資料夾")?)
}

fn clear_db(path: &std::path::Path) -> Result<(), String> {
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    conn.busy_timeout(BUSY_TIMEOUT).map_err(|e| e.to_string())?;
    conn.execute_batch(
        "BEGIN;
         DELETE FROM samples;
         DELETE FROM tcp_stats;
         DELETE FROM hop_stats;
         DELETE FROM wifi_samples;
         DELETE FROM incidents;
         COMMIT;",
    )
    .map_err(|e| e.to_string())?;
    // 壓縮失敗不影響結果（資料已經刪掉了），只是檔案暫時不會變小
    let _ = conn.execute_batch("VACUUM;");
    Ok(())
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
        conn.busy_timeout(BUSY_TIMEOUT).map_err(|e| e.to_string())?;
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
             CREATE TABLE IF NOT EXISTS tcp_stats (
                 ts_ms           INTEGER NOT NULL,
                 remote          TEXT    NOT NULL,
                 smoothed_rtt_ms INTEGER NOT NULL,
                 rtt_var_ms      INTEGER NOT NULL,
                 retrans         INTEGER NOT NULL,  -- 這一輪新增的重傳封包數
                 timeouts        INTEGER NOT NULL   -- 這一輪新增的 RTO 逾時次數
             );
             CREATE INDEX IF NOT EXISTS tcp_stats_ts ON tcp_stats (ts_ms);
             CREATE TABLE IF NOT EXISTS hop_stats (   -- 中間節點每分鐘彙總
                 ts_ms  INTEGER NOT NULL,
                 target TEXT    NOT NULL,
                 ttl    INTEGER NOT NULL,
                 addr   TEXT,                       -- NULL 代表這一跳從沒回應
                 count  INTEGER NOT NULL,
                 lost   INTEGER NOT NULL,
                 avg_ms REAL
             );
             CREATE INDEX IF NOT EXISTS hop_stats_ts ON hop_stats (ts_ms);
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
        for table in ["samples", "wifi_samples", "tcp_stats", "hop_stats"] {
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

    pub fn record_tcp(&self, ts_ms: i64, remote: &str, r: &GameTcpReport) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO tcp_stats (ts_ms, remote, smoothed_rtt_ms, rtt_var_ms, retrans, timeouts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![ts_ms, remote, r.smoothed_rtt_ms, r.rtt_var_ms, r.retrans, r.timeouts])?;
        Ok(())
    }

    pub fn record_hops(
        &mut self,
        ts_ms: i64,
        target: &str,
        hops: &[HopAggregate],
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO hop_stats (ts_ms, target, ttl, addr, count, lost, avg_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for h in hops {
                stmt.execute(params![
                    ts_ms,
                    target,
                    h.ttl,
                    h.addr.map(|a| a.to_string()),
                    h.count,
                    h.lost,
                    h.avg_ms
                ])?;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 複製真實記錄檔到暫存資料夾再清除，不會動到真正的紀錄。
    /// 手動執行：cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn clear_copy_of_real_db() {
        let src = db_path().unwrap();
        let dir = std::env::temp_dir().join("ff14-netmon-test");
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("netmon-copy.db");
        // WAL 模式下要用 SQLite 備份，直接複製檔案可能漏掉還沒寫回的資料
        Connection::open(&src)
            .unwrap()
            .execute("VACUUM INTO ?1", [copy.to_str().unwrap()])
            .unwrap();
        let count = |c: &Connection| -> i64 {
            c.query_row(
                "SELECT (SELECT count(*) FROM samples) + (SELECT count(*) FROM incidents)",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        let before = count(&Connection::open(&copy).unwrap());
        let size_before = std::fs::metadata(&copy).unwrap().len();

        clear_db(&copy).unwrap();

        let after = count(&Connection::open(&copy).unwrap());
        let size_after = std::fs::metadata(&copy).unwrap().len();
        println!("rows {before} -> {after}, size {size_before} -> {size_after} bytes");
        assert_eq!(after, 0);
        std::fs::remove_file(&copy).unwrap();
    }
}
