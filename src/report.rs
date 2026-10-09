//! 匯出報告：從 SQLite 讀出最近一段時間的資料，產生 HTML 報告和 CSV 原始樣本。

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use rusqlite::{Connection, OpenFlags};

use crate::monitor::LAYER_NAMES;
use crate::storage;
use crate::win::{shell, time};

pub struct Exported {
    pub html: PathBuf,
    pub csv: PathBuf,
}

struct LayerStat {
    layer: usize,
    target: String,
    count: i64,
    lost: i64,
    avg_ms: Option<f64>,
    max_ms: Option<i64>,
}

struct HourCell {
    count: i64,
    lost: i64,
    avg_ms: Option<f64>,
}

struct IncidentRow {
    time: String,
    title: String,
    diagnosis: String,
    details: String,
}

/// 匯出最近 `hours` 小時的報告到「文件\ff14-netmon」，完成後用預設程式開啟 HTML
pub fn export(hours: u32) -> Result<Exported, String> {
    let db = storage::db_path().ok_or("找不到記錄檔位置")?;
    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("無法開啟記錄檔：{e}"))?;
    let since = storage::now_ms() - hours as i64 * 60 * 60 * 1000;

    let dir = shell::documents_dir()
        .ok_or("找不到「文件」資料夾")?
        .join("ff14-netmon");
    fs::create_dir_all(&dir).map_err(|e| format!("無法建立資料夾：{e}"))?;
    let stamp = time::now_stamp();
    let html_path = dir.join(format!("report-{stamp}.html"));
    let csv_path = dir.join(format!("samples-{stamp}.csv"));

    let html = build_html(&conn, since, hours).map_err(|e| format!("讀取記錄失敗：{e}"))?;
    fs::write(&html_path, html).map_err(|e| format!("寫入報告失敗：{e}"))?;
    let csv = build_csv(&conn, since).map_err(|e| format!("讀取記錄失敗：{e}"))?;
    fs::write(&csv_path, csv).map_err(|e| format!("寫入 CSV 失敗：{e}"))?;

    shell::open(&html_path);
    Ok(Exported {
        html: html_path,
        csv: csv_path,
    })
}

fn layer_name(layer: i64) -> &'static str {
    LAYER_NAMES.get(layer as usize).copied().unwrap_or("?")
}

fn build_csv(conn: &Connection, since: i64) -> rusqlite::Result<String> {
    // 加 BOM，Excel 才會用 UTF-8 讀中文
    let mut out = String::from("\u{feff}時間,層級,目標,延遲(ms)\r\n");
    let mut stmt = conn.prepare(
        "SELECT datetime(ts_ms / 1000, 'unixepoch', 'localtime'), layer, target, rtt_ms
         FROM samples WHERE ts_ms >= ?1 ORDER BY ts_ms, layer",
    )?;
    let rows = stmt.query_map([since], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<i64>>(3)?,
        ))
    })?;
    for row in rows {
        let (time, layer, target, rtt) = row?;
        let rtt = rtt.map_or("逾時".to_string(), |v| v.to_string());
        let _ = write!(out, "{time},{},{target},{rtt}\r\n", layer_name(layer));
    }
    Ok(out)
}

fn build_html(conn: &Connection, since: i64, hours: u32) -> rusqlite::Result<String> {
    let layers = query_layer_stats(conn, since)?;
    let hourly = query_hourly(conn, since)?;
    let wifi = query_wifi_hourly(conn, since).unwrap_or_default();
    let incidents = query_incidents(conn, since)?;
    let generated: String =
        conn.query_row("SELECT datetime('now', 'localtime')", [], |r| r.get(0))?;

    let mut h = String::new();
    let _ = write!(
        h,
        r#"<!doctype html>
<html lang="zh-Hant"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>FF14 連線報告</title>
<style>{CSS}</style></head><body><main>
<h1>FF14 繁中服連線報告</h1>
<p class="meta">範圍：最近 {hours} 小時　產生時間：{generated}</p>
"#
    );

    // 摘要
    let _ = write!(h, "<h2>摘要</h2>");
    if incidents.is_empty() {
        let _ = write!(h, "<p>這段期間沒有偵測到異常。</p>");
    } else {
        let _ = write!(
            h,
            "<p>這段期間共偵測到 <strong>{}</strong> 次異常，詳見下方事件列表。</p>",
            incidents.len()
        );
    }

    // 各層統計
    let _ = write!(
        h,
        "<h2>各層統計</h2><table><thead><tr><th>層級</th><th>目標</th><th>量測次數</th>\
         <th>平均延遲</th><th>最高延遲</th><th>掉包率</th></tr></thead><tbody>"
    );
    for s in &layers {
        let loss = pct(s.lost, s.count);
        let _ = write!(
            h,
            "<tr><td>{}</td><td>{}</td><td class=num>{}</td><td class=num>{}</td>\
             <td class=num>{}</td><td class='num {}'>{:.1}%</td></tr>",
            LAYER_NAMES[s.layer],
            esc(&s.target),
            s.count,
            fmt_ms(s.avg_ms),
            s.max_ms.map_or("-".into(), |v| format!("{v} ms")),
            loss_class(loss),
            loss
        );
    }
    let _ = write!(h, "</tbody></table>");

    // 每小時
    let _ = write!(
        h,
        "<h2>每小時狀況</h2><p class=meta>每格是平均延遲／掉包率。黃色代表有掉包，紅色代表掉包率 5% 以上。</p>\
         <div class=scroll><table><thead><tr><th>時段</th>"
    );
    for name in LAYER_NAMES {
        let _ = write!(h, "<th>{name}</th>");
    }
    if !wifi.is_empty() {
        let _ = write!(h, "<th>Wi-Fi 訊號</th>");
    }
    let _ = write!(h, "</tr></thead><tbody>");
    for (hour, cells) in &hourly {
        let _ = write!(h, "<tr><td>{hour}</td>");
        for layer in 0..LAYER_NAMES.len() {
            match cells.get(&layer) {
                Some(c) => {
                    let loss = pct(c.lost, c.count);
                    let _ = write!(
                        h,
                        "<td class='num {}'>{}／{:.1}%</td>",
                        loss_class(loss),
                        fmt_ms(c.avg_ms),
                        loss
                    );
                }
                None => {
                    let _ = write!(h, "<td class=num>-</td>");
                }
            }
        }
        if !wifi.is_empty() {
            match wifi.get(hour) {
                Some((avg, min)) => {
                    let class = if *min < 50 { "warn" } else { "" };
                    let _ = write!(
                        h,
                        "<td class='num {class}'>平均 {avg:.0}%（最低 {min}%）</td>"
                    );
                }
                None => {
                    let _ = write!(h, "<td class=num>-</td>");
                }
            }
        }
        let _ = write!(h, "</tr>");
    }
    let _ = write!(h, "</tbody></table></div>");

    // 事件
    let _ = write!(h, "<h2>異常事件</h2>");
    if incidents.is_empty() {
        let _ = write!(h, "<p>沒有異常事件。</p>");
    }
    for i in &incidents {
        let _ = write!(
            h,
            "<details><summary><span class=time>{}</span> {}<br><span class=diag>{}</span></summary>\
             <pre>{}</pre></details>",
            esc(&i.time),
            esc(&i.title),
            esc(&i.diagnosis),
            esc(&i.details)
        );
    }

    let _ = write!(
        h,
        "<p class=meta>由 ff14-netmon 產生。延遲：路由器、ISP、外部網路為 ICMP ping；\
         遊戲伺服器為 TCP 連線建立時間。</p></main></body></html>"
    );
    Ok(h)
}

fn query_layer_stats(conn: &Connection, since: i64) -> rusqlite::Result<Vec<LayerStat>> {
    let mut stmt = conn.prepare(
        "SELECT layer, target, COUNT(*), SUM(rtt_ms IS NULL), AVG(rtt_ms), MAX(rtt_ms)
         FROM samples WHERE ts_ms >= ?1 GROUP BY layer, target ORDER BY layer, MIN(ts_ms)",
    )?;
    let rows = stmt.query_map([since], |r| {
        Ok(LayerStat {
            layer: r.get::<_, i64>(0)? as usize,
            target: r.get(1)?,
            count: r.get(2)?,
            lost: r.get(3)?,
            avg_ms: r.get(4)?,
            max_ms: r.get(5)?,
        })
    })?;
    Ok(rows
        .filter_map(Result::ok)
        .filter(|s| s.layer < LAYER_NAMES.len())
        .collect())
}

fn query_hourly(
    conn: &Connection,
    since: i64,
) -> rusqlite::Result<BTreeMap<String, BTreeMap<usize, HourCell>>> {
    let mut stmt = conn.prepare(
        "SELECT strftime('%m/%d %H:00', ts_ms / 1000, 'unixepoch', 'localtime') AS hour,
                layer, COUNT(*), SUM(rtt_ms IS NULL), AVG(rtt_ms)
         FROM samples WHERE ts_ms >= ?1 GROUP BY hour, layer",
    )?;
    let rows = stmt.query_map([since], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)? as usize,
            HourCell {
                count: r.get(2)?,
                lost: r.get(3)?,
                avg_ms: r.get(4)?,
            },
        ))
    })?;
    let mut map: BTreeMap<String, BTreeMap<usize, HourCell>> = BTreeMap::new();
    for (hour, layer, cell) in rows.flatten() {
        map.entry(hour).or_default().insert(layer, cell);
    }
    Ok(map)
}

/// 每小時的 Wi-Fi 訊號（平均、最低）；沒用 Wi-Fi 就是空的
fn query_wifi_hourly(
    conn: &Connection,
    since: i64,
) -> rusqlite::Result<BTreeMap<String, (f64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT strftime('%m/%d %H:00', ts_ms / 1000, 'unixepoch', 'localtime') AS hour,
                AVG(quality), MIN(quality)
         FROM wifi_samples WHERE ts_ms >= ?1 GROUP BY hour",
    )?;
    let rows = stmt.query_map([since], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
    Ok(rows.flatten().collect())
}

fn query_incidents(conn: &Connection, since: i64) -> rusqlite::Result<Vec<IncidentRow>> {
    let mut stmt = conn.prepare(
        "SELECT datetime(ts_ms / 1000, 'unixepoch', 'localtime'), title, diagnosis, details
         FROM incidents WHERE ts_ms >= ?1 ORDER BY ts_ms DESC",
    )?;
    let rows = stmt.query_map([since], |r| {
        Ok(IncidentRow {
            time: r.get(0)?,
            title: r.get(1)?,
            diagnosis: r.get(2)?,
            details: r.get(3)?,
        })
    })?;
    Ok(rows.flatten().collect())
}

fn pct(part: i64, total: i64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 * 100.0 / total as f64
    }
}

fn loss_class(loss: f64) -> &'static str {
    if loss >= 5.0 {
        "bad"
    } else if loss > 0.0 {
        "warn"
    } else {
        ""
    }
}

fn fmt_ms(v: Option<f64>) -> String {
    v.map_or("-".into(), |v| format!("{v:.1} ms"))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

const CSS: &str = r#"
:root { --bg:#ffffff; --fg:#1f2328; --muted:#656d76; --line:#d0d7de; --head:#f6f8fa;
        --warn-bg:#fff3cd; --bad-bg:#ffd7d5; --accent:#0969da; }
@media (prefers-color-scheme: dark) {
  :root { --bg:#0d1117; --fg:#e6edf3; --muted:#8d96a0; --line:#30363d; --head:#161b22;
          --warn-bg:#4d3800; --bad-bg:#5c1a1a; --accent:#58a6ff; }
}
* { box-sizing: border-box; }
body { margin:0; background:var(--bg); color:var(--fg);
       font-family:"Microsoft JhengHei","Noto Sans TC",system-ui,sans-serif; line-height:1.6; }
main { max-width:1000px; margin:0 auto; padding:24px 16px 48px; }
h1 { font-size:1.6rem; margin:0 0 4px; }
h2 { font-size:1.2rem; margin:32px 0 8px; border-bottom:1px solid var(--line); padding-bottom:4px; }
.meta { color:var(--muted); font-size:.9rem; }
.scroll { overflow-x:auto; }
table { border-collapse:collapse; width:100%; font-size:.92rem; }
th, td { border:1px solid var(--line); padding:6px 10px; text-align:left; white-space:nowrap; }
th { background:var(--head); }
td.num { text-align:right; font-variant-numeric:tabular-nums; }
td.warn { background:var(--warn-bg); }
td.bad { background:var(--bad-bg); }
details { border:1px solid var(--line); border-radius:6px; padding:8px 12px; margin:8px 0; }
summary { cursor:pointer; }
.time { font-variant-numeric:tabular-nums; color:var(--accent); }
.diag { color:var(--muted); }
pre { white-space:pre-wrap; font-size:.85rem; margin:8px 0 0; }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// 用真實記錄檔產生報告到暫存資料夾（不開啟瀏覽器）。手動執行：cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn build_report_from_real_db() {
        let db = storage::db_path().unwrap();
        let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let since = storage::now_ms() - 24 * 60 * 60 * 1000;
        let html = build_html(&conn, since, 24).unwrap();
        let csv = build_csv(&conn, since).unwrap();

        let dir = std::env::temp_dir().join("ff14-netmon-test");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("report.html"), &html).unwrap();
        fs::write(dir.join("samples.csv"), &csv).unwrap();
        println!("{}", dir.display());
        println!("csv lines: {}", csv.lines().count());
        assert!(html.contains("各層統計"));
        assert!(csv.starts_with('\u{feff}'));
    }
}
