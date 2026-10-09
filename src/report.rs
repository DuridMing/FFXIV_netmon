//! 匯出報告：從 SQLite 讀出一段時間的資料，產生 HTML 報告和 CSV。
//!
//! 24 小時以內的報告用每小時明細、CSV 是每 2 秒的原始樣本；
//! 更長的範圍改用每日趨勢加上「星期 × 小時」熱度表，CSV 改成每分鐘彙總，避免檔案太大。

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use rusqlite::{Connection, OpenFlags};

use crate::monitor::LAYER_NAMES;
use crate::storage;
use crate::win::{shell, time};

/// 可以選的報告範圍（小時）
pub const RANGES: [(u32, &str); 3] = [
    (24, "最近 24 小時"),
    (24 * 7, "最近 7 天"),
    (24 * 30, "最近 30 天"),
];
/// 超過這個範圍就改用每日彙總
const DETAIL_HOURS: u32 = 24;
const GAME_LAYER: i64 = 3;
/// 中間節點摘要最多列出幾個目標（遊戲換伺服器時會有多個）
const MAX_HOP_TARGETS: usize = 3;
const WEEKDAYS: [&str; 7] = ["日", "一", "二", "三", "四", "五", "六"];

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

struct Cell {
    count: i64,
    lost: i64,
    avg_ms: Option<f64>,
}

/// 一個時段（每小時或每天）的資料
#[derive(Default)]
struct Bucket {
    layers: BTreeMap<usize, Cell>,
    /// Wi-Fi 訊號 (平均, 最低)
    wifi: Option<(f64, i64)>,
    /// 遊戲連線 (重傳封包數, RTO 逾時次數)
    tcp: Option<(i64, i64)>,
    incidents: i64,
}

struct TcpSummary {
    rounds: i64,
    avg_rtt: f64,
    max_rtt: i64,
    retrans: i64,
    timeouts: i64,
}

struct HopRow {
    target: String,
    ttl: i64,
    addr: Option<String>,
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

fn range_label(hours: u32) -> String {
    RANGES.iter().find(|(h, _)| *h == hours).map_or_else(
        || format!("最近 {hours} 小時"),
        |(_, label)| label.to_string(),
    )
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
    let csv = if hours <= DETAIL_HOURS {
        build_csv(&conn, since)
    } else {
        build_minute_csv(&conn, since)
    }
    .map_err(|e| format!("讀取記錄失敗：{e}"))?;
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

/// 每 2 秒的原始樣本
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

/// 每分鐘彙總（長範圍用，原始樣本 30 天會有好幾百萬筆）
fn build_minute_csv(conn: &Connection, since: i64) -> rusqlite::Result<String> {
    let mut out =
        String::from("\u{feff}時間（每分鐘）,層級,目標,量測次數,平均延遲(ms),掉包率(%)\r\n");
    let mut stmt = conn.prepare(
        "SELECT strftime('%Y-%m-%d %H:%M', ts_ms / 1000, 'unixepoch', 'localtime') AS minute,
                layer, target, COUNT(*), AVG(rtt_ms), SUM(rtt_ms IS NULL)
         FROM samples WHERE ts_ms >= ?1
         GROUP BY minute, layer, target ORDER BY minute, layer",
    )?;
    let rows = stmt.query_map([since], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, Option<f64>>(4)?,
            r.get::<_, i64>(5)?,
        ))
    })?;
    for row in rows {
        let (minute, layer, target, count, avg, lost) = row?;
        let avg = avg.map_or(String::new(), |v| format!("{v:.1}"));
        let _ = write!(
            out,
            "{minute},{},{target},{count},{avg},{:.1}\r\n",
            layer_name(layer),
            pct(lost, count)
        );
    }
    Ok(out)
}

fn build_html(conn: &Connection, since: i64, hours: u32) -> rusqlite::Result<String> {
    let detailed = hours <= DETAIL_HOURS;
    // 24 小時內每小時一列，更長的範圍每天一列
    let bucket_fmt = if detailed { "%m/%d %H:00" } else { "%Y-%m-%d" };
    let layers = query_layer_stats(conn, since)?;
    let buckets = query_buckets(conn, since, bucket_fmt)?;
    let incidents = query_incidents(conn, since)?;
    // 舊版記錄檔沒有這些表，查不到就略過
    let tcp = query_tcp_summary(conn, since).ok().flatten();
    let hops = query_hops(conn, since).unwrap_or_default();
    let generated: String =
        conn.query_row("SELECT datetime('now', 'localtime')", [], |r| r.get(0))?;
    let label = range_label(hours);

    let mut h = String::new();
    let _ = write!(
        h,
        r#"<!doctype html>
<html lang="zh-Hant"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>FF14 連線報告</title>
<style>{CSS}</style></head><body><main>
<h1>FF14 繁中服連線報告</h1>
<p class="meta">範圍：{label}　產生時間：{generated}</p>
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

    write_layer_stats(&mut h, &layers);
    if let Some(t) = &tcp {
        write_tcp_summary(&mut h, t);
    }
    if !detailed {
        write_heatmap(&mut h, conn, since)?;
    }
    write_buckets(&mut h, &buckets, detailed);
    write_hops(&mut h, &hops);
    write_incidents(&mut h, &incidents);

    let _ = write!(
        h,
        "<p class=meta>由 ff14-netmon 產生。延遲：路由器、ISP、外部網路、中間節點為 ICMP ping；\
         遊戲伺服器為 TCP 連線建立時間；遊戲連線 TCP 統計為 Windows 對遊戲那條連線的實際量測\
         （需要以系統管理員身分執行）。</p></main></body></html>"
    );
    Ok(h)
}

fn write_layer_stats(h: &mut String, layers: &[LayerStat]) {
    let _ = write!(
        h,
        "<h2>各層統計</h2><table><thead><tr><th>層級</th><th>目標</th><th>量測次數</th>\
         <th>平均延遲</th><th>最高延遲</th><th>掉包率</th></tr></thead><tbody>"
    );
    for s in layers {
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
}

fn write_tcp_summary(h: &mut String, t: &TcpSummary) {
    let class = if t.timeouts > 0 {
        "bad"
    } else if t.retrans > 0 {
        "warn"
    } else {
        ""
    };
    let _ = write!(
        h,
        "<h2>遊戲連線 TCP 統計</h2>\
         <p class=meta>Windows 對遊戲那條連線的實際量測。重傳：封包送出後沒收到確認而重送；\
         逾時（RTO）：等太久都沒有回應，連續發生會導致 90002 斷線。</p>\
         <table><thead><tr><th>量測次數</th><th>平均實際延遲</th><th>最高實際延遲</th>\
         <th>重傳封包</th><th>重傳逾時</th></tr></thead><tbody>\
         <tr><td class=num>{}</td><td class=num>{:.1} ms</td><td class=num>{} ms</td>\
         <td class='num {class}'>{}</td><td class='num {}'>{}</td></tr></tbody></table>",
        t.rounds,
        t.avg_rtt,
        t.max_rtt,
        t.retrans,
        if t.timeouts > 0 { "bad" } else { "" },
        t.timeouts
    );
}

/// 星期 × 小時的異常次數，看出哪個時段最常出問題
fn write_heatmap(h: &mut String, conn: &Connection, since: i64) -> rusqlite::Result<()> {
    let mut counts = [[0i64; 24]; 7];
    let mut stmt = conn.prepare(
        "SELECT CAST(strftime('%w', ts_ms / 1000, 'unixepoch', 'localtime') AS INTEGER),
                CAST(strftime('%H', ts_ms / 1000, 'unixepoch', 'localtime') AS INTEGER),
                COUNT(*)
         FROM incidents WHERE ts_ms >= ?1 GROUP BY 1, 2",
    )?;
    let rows = stmt.query_map([since], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for (w, hour, n) in rows.flatten() {
        if let (Some(row), Ok(hour)) = (counts.get_mut(w as usize), usize::try_from(hour))
            && let Some(cell) = row.get_mut(hour)
        {
            *cell = n;
        }
    }
    // 遊戲伺服器各時段的掉包率
    let mut loss = [[None; 24]; 7];
    let mut stmt = conn.prepare(
        "SELECT CAST(strftime('%w', ts_ms / 1000, 'unixepoch', 'localtime') AS INTEGER),
                CAST(strftime('%H', ts_ms / 1000, 'unixepoch', 'localtime') AS INTEGER),
                COUNT(*), SUM(rtt_ms IS NULL)
         FROM samples WHERE ts_ms >= ?1 AND layer = ?2 GROUP BY 1, 2",
    )?;
    let rows = stmt.query_map([since, GAME_LAYER], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    for (w, hour, n, lost) in rows.flatten() {
        if let (Some(row), Ok(hour)) = (loss.get_mut(w as usize), usize::try_from(hour))
            && let Some(cell) = row.get_mut(hour)
        {
            *cell = Some(pct(lost, n));
        }
    }

    let _ = write!(
        h,
        "<h2>時段分析</h2><p class=meta>每格是該時段的異常次數，顏色越深代表越常出問題。\
         滑鼠移到格子上可以看到遊戲伺服器的掉包率。</p><div class=scroll><table class=heat><thead><tr><th></th>"
    );
    for hour in 0..24 {
        let _ = write!(h, "<th>{hour}</th>");
    }
    let _ = write!(h, "</tr></thead><tbody>");
    for (w, name) in WEEKDAYS.iter().enumerate() {
        let _ = write!(h, "<tr><th>週{name}</th>");
        for hour in 0..24 {
            let n = counts[w][hour];
            let level = match n {
                0 => 0,
                1 => 1,
                2..=3 => 2,
                _ => 3,
            };
            let title = loss[w][hour]
                .map_or("沒有遊戲伺服器的資料".to_string(), |l: f64| {
                    format!("遊戲伺服器掉包率 {l:.1}%")
                });
            let text = if n > 0 { n.to_string() } else { String::new() };
            let _ = write!(
                h,
                "<td class='num h{level}' title='週{name} {hour}:00，{title}'>{text}</td>"
            );
        }
        let _ = write!(h, "</tr>");
    }
    let _ = write!(h, "</tbody></table></div>");
    Ok(())
}

/// 每小時（24 小時內）或每天（更長範圍）的明細
fn write_buckets(h: &mut String, buckets: &BTreeMap<String, Bucket>, detailed: bool) {
    let has_wifi = buckets.values().any(|b| b.wifi.is_some());
    let has_tcp = buckets.values().any(|b| b.tcp.is_some());
    let (title, first_col) = if detailed {
        ("每小時狀況", "時段")
    } else {
        ("每日趨勢", "日期")
    };
    let _ = write!(
        h,
        "<h2>{title}</h2><p class=meta>延遲欄位是平均延遲／掉包率。黃色代表有掉包，紅色代表掉包率 5% 以上。</p>\
         <div class=scroll><table><thead><tr><th>{first_col}</th>"
    );
    for name in LAYER_NAMES {
        let _ = write!(h, "<th>{name}</th>");
    }
    if has_tcp {
        let _ = write!(h, "<th>遊戲連線重傳／逾時</th>");
    }
    if has_wifi {
        let _ = write!(h, "<th>Wi-Fi 訊號</th>");
    }
    let _ = write!(h, "<th>異常次數</th></tr></thead><tbody>");

    for (key, b) in buckets {
        let _ = write!(h, "<tr><td>{key}</td>");
        for layer in 0..LAYER_NAMES.len() {
            match b.layers.get(&layer) {
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
        if has_tcp {
            match b.tcp {
                Some((retrans, timeouts)) => {
                    let class = if timeouts > 0 {
                        "bad"
                    } else if retrans > 0 {
                        "warn"
                    } else {
                        ""
                    };
                    let _ = write!(h, "<td class='num {class}'>{retrans}／{timeouts}</td>");
                }
                None => {
                    let _ = write!(h, "<td class=num>-</td>");
                }
            }
        }
        if has_wifi {
            match b.wifi {
                Some((avg, min)) => {
                    let class = if min < 50 { "warn" } else { "" };
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
        let class = if b.incidents > 0 { "warn" } else { "" };
        let _ = write!(h, "<td class='num {class}'>{}</td></tr>", b.incidents);
    }
    let _ = write!(h, "</tbody></table></div>");
}

fn write_hops(h: &mut String, hops: &[HopRow]) {
    if hops.is_empty() {
        return;
    }
    let _ = write!(
        h,
        "<h2>中間節點</h2><p class=meta>到遊戲伺服器路徑上每一跳的統計。只有某一跳掉包、\
         後面的節點都正常時，通常是那台路由器限制回應，不代表真的掉包；\
         從某一跳開始之後每一跳都掉包，才代表那一段有問題。</p>"
    );
    let mut current = None;
    for row in hops {
        if current != Some(&row.target) {
            if current.is_some() {
                let _ = write!(h, "</tbody></table>");
            }
            let _ = write!(
                h,
                "<h3>到 {}</h3><table><thead><tr><th>跳數</th><th>節點</th><th>量測次數</th>\
                 <th>平均延遲</th><th>掉包率</th></tr></thead><tbody>",
                esc(&row.target)
            );
            current = Some(&row.target);
        }
        let loss = pct(row.lost, row.count);
        let _ = write!(
            h,
            "<tr><td class=num>{}</td><td>{}</td><td class=num>{}</td><td class=num>{}</td>\
             <td class='num {}'>{:.1}%</td></tr>",
            row.ttl,
            row.addr.as_deref().map_or("*（沒有回應）".to_string(), esc),
            row.count,
            fmt_ms(row.avg_ms),
            loss_class(loss),
            loss
        );
    }
    let _ = write!(h, "</tbody></table>");
}

fn write_incidents(h: &mut String, incidents: &[IncidentRow]) {
    let _ = write!(h, "<h2>異常事件</h2>");
    if incidents.is_empty() {
        let _ = write!(h, "<p>沒有異常事件。</p>");
    }
    for i in incidents {
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

/// 依 `fmt`（strftime 格式）分時段彙總各種資料
fn query_buckets(
    conn: &Connection,
    since: i64,
    fmt: &str,
) -> rusqlite::Result<BTreeMap<String, Bucket>> {
    let bucket = format!("strftime('{fmt}', ts_ms / 1000, 'unixepoch', 'localtime')");
    let mut map: BTreeMap<String, Bucket> = BTreeMap::new();

    let mut stmt = conn.prepare(&format!(
        "SELECT {bucket} AS b, layer, COUNT(*), SUM(rtt_ms IS NULL), AVG(rtt_ms)
         FROM samples WHERE ts_ms >= ?1 GROUP BY b, layer"
    ))?;
    let rows = stmt.query_map([since], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)? as usize,
            Cell {
                count: r.get(2)?,
                lost: r.get(3)?,
                avg_ms: r.get(4)?,
            },
        ))
    })?;
    for (key, layer, cell) in rows.flatten() {
        map.entry(key).or_default().layers.insert(layer, cell);
    }

    // 以下幾張表在舊版記錄檔可能不存在，失敗就略過
    let optional = |sql: String, apply: &mut dyn FnMut(&rusqlite::Row) -> rusqlite::Result<()>| {
        if let Ok(mut stmt) = conn.prepare(&sql)
            && let Ok(mut rows) = stmt.query([since])
        {
            while let Ok(Some(row)) = rows.next() {
                let _ = apply(row);
            }
        }
    };
    optional(
        format!(
            "SELECT {bucket} AS b, AVG(quality), MIN(quality) FROM wifi_samples WHERE ts_ms >= ?1 GROUP BY b"
        ),
        &mut |r| {
            map.entry(r.get(0)?).or_default().wifi = Some((r.get(1)?, r.get(2)?));
            Ok(())
        },
    );
    optional(
        format!(
            "SELECT {bucket} AS b, SUM(retrans), SUM(timeouts) FROM tcp_stats WHERE ts_ms >= ?1 GROUP BY b"
        ),
        &mut |r| {
            map.entry(r.get(0)?).or_default().tcp = Some((r.get(1)?, r.get(2)?));
            Ok(())
        },
    );
    optional(
        format!("SELECT {bucket} AS b, COUNT(*) FROM incidents WHERE ts_ms >= ?1 GROUP BY b"),
        &mut |r| {
            map.entry(r.get(0)?).or_default().incidents = r.get(1)?;
            Ok(())
        },
    );
    Ok(map)
}

fn query_tcp_summary(conn: &Connection, since: i64) -> rusqlite::Result<Option<TcpSummary>> {
    conn.query_row(
        "SELECT COUNT(*), AVG(smoothed_rtt_ms), MAX(smoothed_rtt_ms), SUM(retrans), SUM(timeouts)
         FROM tcp_stats WHERE ts_ms >= ?1",
        [since],
        |r| {
            let rounds: i64 = r.get(0)?;
            Ok((rounds > 0).then(|| -> rusqlite::Result<TcpSummary> {
                Ok(TcpSummary {
                    rounds,
                    avg_rtt: r.get(1)?,
                    max_rtt: r.get(2)?,
                    retrans: r.get(3)?,
                    timeouts: r.get(4)?,
                })
            }))
        },
    )?
    .transpose()
}

/// 中間節點彙總：最近用過的幾個目標，每個目標依跳數排列
fn query_hops(conn: &Connection, since: i64) -> rusqlite::Result<Vec<HopRow>> {
    let mut stmt = conn.prepare(
        "WITH recent AS (
             SELECT target FROM hop_stats WHERE ts_ms >= ?1
             GROUP BY target ORDER BY MAX(ts_ms) DESC LIMIT ?2
         )
         SELECT h.target, h.ttl, h.addr, SUM(h.count), SUM(h.lost),
                SUM(h.avg_ms * (h.count - h.lost)) / NULLIF(SUM(h.count - h.lost), 0)
         FROM hop_stats h JOIN recent USING (target)
         WHERE h.ts_ms >= ?1
         GROUP BY h.target, h.ttl, h.addr
         ORDER BY h.target, h.ttl, SUM(h.count) DESC",
    )?;
    let rows = stmt.query_map(rusqlite::params![since, MAX_HOP_TARGETS as i64], |r| {
        Ok(HopRow {
            target: r.get(0)?,
            ttl: r.get(1)?,
            addr: r.get(2)?,
            count: r.get(3)?,
            lost: r.get(4)?,
            avg_ms: r.get(5)?,
        })
    })?;
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
        .replace('\'', "&#39;")
}

const CSS: &str = r#"
:root { --bg:#ffffff; --fg:#1f2328; --muted:#656d76; --line:#d0d7de; --head:#f6f8fa;
        --warn-bg:#fff3cd; --bad-bg:#ffd7d5; --accent:#0969da;
        --h1:#ffe8b3; --h2:#ffbf80; --h3:#ff8a80; }
@media (prefers-color-scheme: dark) {
  :root { --bg:#0d1117; --fg:#e6edf3; --muted:#8d96a0; --line:#30363d; --head:#161b22;
          --warn-bg:#4d3800; --bad-bg:#5c1a1a; --accent:#58a6ff;
          --h1:#4d3800; --h2:#7a3d00; --h3:#8b1e1e; }
}
* { box-sizing: border-box; }
body { margin:0; background:var(--bg); color:var(--fg);
       font-family:"Microsoft JhengHei","Noto Sans TC",system-ui,sans-serif; line-height:1.6; }
main { max-width:1000px; margin:0 auto; padding:24px 16px 48px; }
h1 { font-size:1.6rem; margin:0 0 4px; }
h2 { font-size:1.2rem; margin:32px 0 8px; border-bottom:1px solid var(--line); padding-bottom:4px; }
h3 { font-size:1rem; margin:16px 0 6px; }
.meta { color:var(--muted); font-size:.9rem; }
.scroll { overflow-x:auto; }
table { border-collapse:collapse; width:100%; font-size:.92rem; }
th, td { border:1px solid var(--line); padding:6px 10px; text-align:left; white-space:nowrap; }
th { background:var(--head); }
td.num { text-align:right; font-variant-numeric:tabular-nums; }
td.warn { background:var(--warn-bg); }
td.bad { background:var(--bad-bg); }
table.heat th, table.heat td { padding:4px 6px; text-align:center; min-width:28px; }
td.h1 { background:var(--h1); } td.h2 { background:var(--h2); } td.h3 { background:var(--h3); }
details { border:1px solid var(--line); border-radius:6px; padding:8px 12px; margin:8px 0; }
summary { cursor:pointer; }
.time { font-variant-numeric:tabular-nums; color:var(--accent); }
.diag { color:var(--muted); }
pre { white-space:pre-wrap; font-size:.85rem; margin:8px 0 0; }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// 用真實記錄檔產生 24 小時和 30 天的報告到暫存資料夾（不開啟瀏覽器）。
    /// 手動執行：cargo test -- --ignored --nocapture
    #[test]
    #[ignore]
    fn build_report_from_real_db() {
        let db = storage::db_path().unwrap();
        let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let dir = std::env::temp_dir().join("ff14-netmon-test");
        fs::create_dir_all(&dir).unwrap();

        for (hours, _) in RANGES {
            let since = storage::now_ms() - hours as i64 * 60 * 60 * 1000;
            let html = build_html(&conn, since, hours).unwrap();
            let csv = if hours <= DETAIL_HOURS {
                build_csv(&conn, since)
            } else {
                build_minute_csv(&conn, since)
            }
            .unwrap();
            fs::write(dir.join(format!("report-{hours}h.html")), &html).unwrap();
            fs::write(dir.join(format!("samples-{hours}h.csv")), &csv).unwrap();
            println!(
                "{hours}h: html {} KB, csv {} lines, heatmap={}, hops={}, tcp={}",
                html.len() / 1024,
                csv.lines().count(),
                html.contains("<h2>時段分析"),
                html.contains("<h2>中間節點"),
                html.contains("<h2>遊戲連線 TCP 統計")
            );
            assert!(html.contains("各層統計"));
            assert!(csv.starts_with('\u{feff}'));
        }
        println!("{}", dir.display());
    }
}
