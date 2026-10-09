# FF14 繁中服連線監測工具（ff14-netmon）

FF14 繁中服常常斷線。這個 Windows 小程式會持續監測連線，在斷線當下判斷問題出在哪一段：自己的網路、ISP，還是遊戲伺服器。

```text
[你的電腦] → [Wi-Fi/網卡] → [路由器] → [ISP] → [國際路由] → [FF14 繁中服伺服器]
```

> **目前狀態：M4 完成**。可以即時監測各層延遲，發生異常時自動診斷、跳出通知並記錄；可以縮到系統匣常駐，也能匯出報告。

## 功能

- **自動找出遊戲伺服器**：讀取 `ffxiv_dx11.exe` 的 TCP 連線，不用手動填 IP。
- **分層監測**：每 1–2 秒量測各層的延遲、抖動和掉包率。

  | 層級 | 目標 | 方法 |
  | --- | --- | --- |
  | L1 | 本機網卡、Wi-Fi 訊號 | WLAN API |
  | L2 | 路由器（預設閘道） | ICMP ping |
  | L3 | ISP（traceroute 第 2–3 跳） | ICMP ping |
  | L4 | 外部網路（1.1.1.1 / 8.8.8.8） | ICMP ping |
  | L5 | 遊戲伺服器 | TCP ping（伺服器常擋 ICMP） |

- **斷線偵測**：以下任一情況算一次斷線，並保存前後 60 秒的數據、自動跑 traceroute：
  - 遊戲的 TCP 連線消失
  - L5 連續逾時
  - 掉包率超過門檻
- **自動診斷**：

  | 現象 | 判斷 |
  | --- | --- |
  | 路由器也掉包 | 家中網路問題 |
  | 路由器正常、ISP 掉包 | ISP 問題 |
  | 只有遊戲伺服器掉包 | 伺服器或國際路由問題 |
  | 全部正常但遊戲斷線 | 伺服器主動斷線或維護 |

- **記錄檔**：每 2 秒的量測結果和異常事件都存在 `%LOCALAPPDATA%\ff14-netmon\netmon.db`（SQLite），量測樣本保留 30 天。
- **報告匯出**：按「匯出報告」會把最近 24 小時的 HTML 報告（各層統計、每小時狀況、異常事件）和 CSV 原始資料存到「文件\ff14-netmon」，並自動開啟報告。可以交給 ISP 或遊戲客服當證據。
- **系統匣常駐**：按視窗的 X 會縮到系統匣繼續監測。圖示顏色代表連線狀態（綠／黃／紅），滑鼠移上去可以看到目前延遲。右鍵選「結束」才會關閉程式。
- **異常通知**：發生異常時跳出 Windows 通知，內容包含診斷結論。可以在視窗右上角關閉。
- **Wi-Fi 訊號**：顯示目前連線的 Wi-Fi 和訊號強度。家中網路出問題、而且訊號偏弱時，會建議改用有線網路。

### 安全性

本工具只讀取 Windows 的連線資訊，不讀取、注入或修改遊戲封包，也不碰遊戲程序本身。

## 技術

- 語言：Rust
- 介面：eframe / egui
- 圖表：egui_plot
- 呼叫 Windows API：`windows` crate
  - `IcmpSendEcho`：ICMP ping，不需要系統管理員權限
  - `GetExtendedTcpTable`：找出遊戲連線
- 資料儲存：SQLite（`rusqlite`）
- 目標大小：單一 exe，約 3–8 MB，不需要另外安裝任何東西

## 開發環境設定

1. 安裝 [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)，勾選「使用 C++ 的桌面開發」。Rust 在 Windows 上要用到它的連結器。
2. 安裝 Rust：

   ```powershell
   winget install Rustlang.Rustup
   ```

   也可以從 <https://rustup.rs> 下載 `rustup-init.exe` 安裝。
3. 重新開啟終端機，確認有裝好：

   ```powershell
   rustc --version
   cargo --version
   ```

## 建置與執行

```powershell
cargo run                                  # 開啟監測視窗，自動偵測遊戲伺服器
cargo run -- --target 203.0.113.10:55006   # 手動指定目標（沒開遊戲時測試用）
cargo build --release                      # 正式版，輸出在 target\release\
```

## 開發階段

- [x] **M1 命令列原型**：抓出遊戲的伺服器 IP 和連接埠，對各層做 ping，結果印在終端機
- [x] **M2 基本介面**：狀態表、即時延遲折線圖
- [x] **M3 事件與診斷**：斷線偵測、自動 traceroute、診斷結論、SQLite 記錄
- [x] **M4 完善**：通知、報告匯出、系統匣、Wi-Fi 訊號
- [ ] **M5 進階**：TCP 重傳統計（需要系統管理員權限）、長期統計
