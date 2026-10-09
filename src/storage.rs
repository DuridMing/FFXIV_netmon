//! SQLite 記錄：每輪量測樣本與異常事件。

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};

use crate::game_tcp::GameTcpReport;
use crate::hops::HopAggregate;
use crate::wifi::WifiInfo;

/// 樣本保留天數
const RETENTION_DAYS: i64 = 30;
/// 另一個連線（例如清除紀錄）正在寫入時，最多等這麼久，不要直接回報失敗
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// 命令列指定的資料夾（以另一個帳號提升權限時，沿用原本使用者的資料夾）
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
/// 最後一次「清除所有紀錄」的時間；在這之前量到、還沒寫入的樣本不要再寫進去
static CLEARED_AT_MS: AtomicI64 = AtomicI64::new(0);

pub struct Storage {
    conn: Connection,
}

pub struct Sample {
    pub layer: usize,
    pub target: String,
    /// None 代表逾時
    pub rtt_ms: Option<u32>,
}

/// 一輪量測要寫進記錄檔的資料
pub struct RoundRecord {
    pub ts_ms: i64,
    pub samples: Vec<Sample>,
    pub wifi: Option<WifiInfo>,
    /// (遊戲連線的遠端位址, TCP 統計)
    pub tcp: Option<(String, GameTcpReport)>,
    /// (目標, 中間節點彙總)；換目標時會同時有舊目標不滿一分鐘的部分
    pub hops: Vec<(String, Vec<HopAggregate>)>,
    pub incident: Option<IncidentRecord>,
}

pub struct IncidentRecord {
    /// IncidentKind::code()
    pub kind: String,
    pub title: String,
    pub diagnosis: String,
    pub details: String,
}

pub fn set_data_dir(dir: PathBuf) {
    let _ = DATA_DIR.set(dir);
}

/// 程式資料夾：%LOCALAPPDATA%\ff14-netmon
pub fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = DATA_DIR.get() {
        return Some(dir.clone());
    }
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
    CLEARED_AT_MS.store(now_ms(), Ordering::SeqCst);
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
        Self::open_at(&db_path().ok_or("找不到 LOCALAPPDATA 資料夾")?)
    }

    fn open_at(path: &std::path::Path) -> Result<Self, String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
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

        let storage = Self { conn };
        storage.prune().map_err(|e| e.to_string())?;
        Ok(storage)
    }

    /// 刪掉超過保留天數的樣本（事件不刪）
    pub fn prune(&self) -> rusqlite::Result<()> {
        let cutoff = now_ms() - RETENTION_DAYS * 24 * 60 * 60 * 1000;
        for table in ["samples", "wifi_samples", "tcp_stats", "hop_stats"] {
            self.conn
                .execute(&format!("DELETE FROM {table} WHERE ts_ms < ?1"), [cutoff])?;
        }
        Ok(())
    }

    /// 把好幾輪的資料用一個交易寫進去，減少寫入硬碟的次數。
    /// 「清除所有紀錄」之前量到的資料會略過
    pub fn write_rounds(&mut self, rounds: &[RoundRecord]) -> rusqlite::Result<()> {
        let cleared_at = CLEARED_AT_MS.load(Ordering::SeqCst);
        let tx = self.conn.transaction()?;
        {
            let mut sample = tx.prepare_cached(
                "INSERT INTO samples (ts_ms, layer, target, rtt_ms) VALUES (?1, ?2, ?3, ?4)",
            )?;
            let mut wifi = tx.prepare_cached(
                "INSERT INTO wifi_samples (ts_ms, ssid, quality) VALUES (?1, ?2, ?3)",
            )?;
            let mut tcp = tx.prepare_cached(
                "INSERT INTO tcp_stats (ts_ms, remote, smoothed_rtt_ms, rtt_var_ms, retrans, timeouts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            let mut hop = tx.prepare_cached(
                "INSERT INTO hop_stats (ts_ms, target, ttl, addr, count, lost, avg_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            let mut incident = tx.prepare_cached(
                "INSERT INTO incidents (ts_ms, kind, title, diagnosis, details)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for r in rounds.iter().filter(|r| r.ts_ms > cleared_at) {
                let ts = r.ts_ms;
                for s in &r.samples {
                    sample.execute(params![ts, s.layer as i64, s.target, s.rtt_ms])?;
                }
                if let Some(w) = &r.wifi {
                    // 沒有位置權限時讀不到網路名稱，存空字串
                    wifi.execute(params![ts, w.ssid.as_deref().unwrap_or(""), w.quality])?;
                }
                if let Some((remote, t)) = &r.tcp {
                    tcp.execute(params![
                        ts,
                        remote,
                        t.smoothed_rtt_ms,
                        t.rtt_var_ms,
                        t.retrans,
                        t.timeouts
                    ])?;
                }
                for (target, hops) in &r.hops {
                    for h in hops {
                        hop.execute(params![
                            ts,
                            target,
                            h.ttl,
                            h.addr.map(|a| a.to_string()),
                            h.count,
                            h.lost,
                            h.avg_ms
                        ])?;
                    }
                }
                if let Some(i) = &r.incident {
                    incident.execute(params![ts, i.kind, i.title, i.diagnosis, i.details])?;
                }
            }
        }
        tx.commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(ts_ms: i64) -> RoundRecord {
        RoundRecord {
            ts_ms,
            samples: vec![Sample {
                layer: 0,
                target: "192.0.2.1".into(),
                rtt_ms: Some(3),
            }],
            wifi: Some(WifiInfo {
                ssid: None,
                quality: 80,
            }),
            tcp: None,
            hops: vec![(
                "203.0.113.1".into(),
                vec![HopAggregate {
                    ttl: 1,
                    addr: None,
                    count: 30,
                    lost: 30,
                    avg_ms: None,
                }],
            )],
            incident: None,
        }
    }

    #[test]
    fn writes_rounds_in_one_batch_and_prunes_old_samples() {
        let dir = std::env::temp_dir().join(format!("ff14-netmon-test-{}", std::process::id()));
        let path = dir.join("batch.db");
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Storage::open_at(&path).unwrap();
        let old = now_ms() - (RETENTION_DAYS + 1) * 24 * 60 * 60 * 1000;
        s.write_rounds(&[round(old), round(now_ms()), round(now_ms() + 2000)])
            .unwrap();
        let count = |table: &str| -> i64 {
            s.conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count("samples"), 3);
        assert_eq!(count("hop_stats"), 3);
        s.prune().unwrap();
        assert_eq!(count("samples"), 2);
        assert_eq!(count("wifi_samples"), 2);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 複製真實記錄檔到暫存資料夾再清除，不會動到真正的紀錄。
    /// 手動執行：cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn clear_copy_of_real_db() {
        /// 測試結束（不論成功或失敗）都刪掉副本，下次執行時 VACUUM INTO 才不會因為檔案已存在而失敗
        struct RemoveOnDrop(PathBuf);
        impl Drop for RemoveOnDrop {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }

        let src = db_path().unwrap();
        if !src.exists() {
            // 不要開啟不存在的檔案，不然會在真正的資料夾建立一個空的記錄檔
            println!("沒有記錄檔，略過：{}", src.display());
            return;
        }
        let dir = std::env::temp_dir().join("ff14-netmon-test");
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("netmon-copy.db");
        // 上次失敗留下的副本
        let _ = std::fs::remove_file(&copy);
        let _guard = RemoveOnDrop(copy.clone());
        // WAL 模式下要用 SQLite 備份，直接複製檔案可能漏掉還沒寫回的資料
        Connection::open_with_flags(&src, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
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
    }
}
