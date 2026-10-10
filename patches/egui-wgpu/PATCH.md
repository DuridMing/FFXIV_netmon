# egui-wgpu 0.36.2（ff14-netmon 修改版）

來源：<https://github.com/emilk/egui/tree/0.36.2/crates/egui-wgpu>（crates.io 的 egui-wgpu 0.36.2）。由根目錄 `Cargo.toml` 的 `[patch.crates-io]` 使用。

## 授權

egui-wgpu 以 MIT OR Apache-2.0 授權，原文在這個資料夾的 [LICENSE-MIT](LICENSE-MIT) 和 [LICENSE-APACHE](LICENSE-APACHE)（取自 egui 0.36.2 tag 的根目錄；crates.io 的套件沒有附）。版權屬於原作者，這個資料夾的修改沿用同樣的授權。修改過的檔案在修改處標有 `ff14-netmon patch`。

## 修改內容

只改了 `src/lib.rs` 的 `RenderState::create`（搜尋 `ff14-netmon patch`）：

- 原版把列舉到的所有顯示卡存在 `RenderState::available_adapters`，整個程式執行期間都不釋放。
- wgpu 的每個 adapter 都握著查詢能力時建立的 D3D12 裝置，所以雙顯示卡的電腦會一直在獨立顯示卡（遊戲用的那張）和 WARP 上各留一個 D3D12 裝置與它的驅動記憶體。
- 改成選好顯示卡、建立裝置後，清單只留實際使用的那一張。eframe 沒有用到這個欄位。

## 升級 egui／eframe 時

1. 看新版的 egui-wgpu 是否還會保留所有 adapter；已經不保留的話，刪掉這個資料夾和 `[patch.crates-io]`。
2. 還需要的話，用新版原始碼取代這個資料夾，再套用同樣的修改。
