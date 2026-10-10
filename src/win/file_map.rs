//! 唯讀的記憶體映射檔：只有實際讀到的部分才會載入記憶體，系統記憶體不夠時也能直接丟掉再從檔案讀回來。

use std::fs::File;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, MapViewOfFile, PAGE_READONLY,
};

/// 把整個檔案映射成整個程式執行期間都有效的唯讀資料（不會解除映射，只適合字型這類一直要用的檔案）
pub fn map_static(path: &Path) -> Option<&'static [u8]> {
    let file = File::open(path).ok()?;
    let len = usize::try_from(file.metadata().ok()?.len()).ok()?;
    if len == 0 {
        // 空檔案沒辦法建立映射
        return None;
    }
    unsafe {
        let file_handle = HANDLE(file.as_raw_handle());
        let mapping = CreateFileMappingW(file_handle, None, PAGE_READONLY, 0, 0, None).ok()?;
        let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
        // 映射區會自己保留檔案和映射物件，handle 可以先關掉
        let _ = CloseHandle(mapping);
        if view.Value.is_null() {
            return None;
        }
        Some(std::slice::from_raw_parts(view.Value as *const u8, len))
    }
}
