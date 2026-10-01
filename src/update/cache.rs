//! 临时安装包缓存：路径、加锁打开/创建、启动期过期清理。
//!
//! 不变量：缓存是否复用由哈希校验裁决（见 `installer`），本文件只管
//! “文件在哪、加什么锁打开、过期才删”；有效期内的文件不得无条件删除。

use std::os::windows::fs::OpenOptionsExt;

use crate::config::{INSTALLER_CACHE_MAX_AGE_SECS, UPDATE_HELPER_STALE_SECS};

const TEMP_FILE_NAME: &str = "traffic-monitor-setup-temp.exe";
/// 安装交接副本进程的文件名：每次交接由协调者把自身 exe 复制到此处再 re-exec。
const UPDATE_HELPER_FILE_NAME: &str = "traffic-monitor-update-helper.exe";

/// 安装器文件以只读共享模式打开，阻止其他进程改写已校验文件。
const FILE_SHARE_READ_ONLY: u32 = 0x0000_0001;

/// 安装包缓存与交接副本的共同落盘目录。
fn update_temp_dir() -> std::path::PathBuf {
    // var_os + PathBuf::from 无损：LOCALAPPDATA 含非 Unicode 可解码字符时，
    // var 会因非法 Unicode 返回 Err 而误走 temp 回退，有损中转则替换字符。
    std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Traffic Monitor")
}

pub(super) fn get_temp_installer_path() -> std::path::PathBuf {
    update_temp_dir().join(TEMP_FILE_NAME)
}

/// 安装交接副本进程的落盘路径：与安装包缓存同目录。
/// 副本不是缓存——它是每次交接临时落盘的自身副本；残留由 [`init_cleanup_temp`] 按
/// [`UPDATE_HELPER_STALE_SECS`] 年龄门控清理（门控为避开活跃交接，理由见该函数注释），
/// 与「有效期内必须保留」的安装包缓存是两条不同的清理规则。
pub(super) fn get_update_helper_path() -> std::path::PathBuf {
    update_temp_dir().join(UPDATE_HELPER_FILE_NAME)
}

pub(super) fn open_locked_installer(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ_ONLY)
        .open(path)
}

pub(super) fn create_locked_installer(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_READ_ONLY)
        .open(path)
}

/// 启动期清理长期残留的临时安装包：仅删除超过缓存有效期的文件。
///
/// 有效期内的文件是已通过哈希校验的可复用缓存（哈希不匹配的残留由
/// `do_update_check` 自行删除重下）；无条件删除会摧毁缓存，迫使每次
/// 检查都重新下载。时钟回拨导致 mtime 不可解析时保守保留。
pub fn init_cleanup_temp() {
    // 交接副本不是缓存：残留即历史交接的垃圾，但清理有年龄门控——门控理由与
    // 缓存的有效期不同，是避开活跃交接：刚落盘的副本可能正处在协调者「复制 →
    // spawn」的窗口内，此刻删除会让 spawn 撞上 FILE_NOT_FOUND（非瞬态、不重试）。
    // 副本进程仍在跑时文件被映像占用，删除失败静默忽略（下次启动再清）。
    // 时钟回拨导致 mtime 不可解析时保守保留，与下方缓存清理同一方向。
    let helper_path = get_update_helper_path();
    let helper_stale = std::fs::metadata(&helper_path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|mtime| mtime.elapsed().ok())
        .is_some_and(|age| age.as_secs() > UPDATE_HELPER_STALE_SECS);
    if helper_stale {
        let _ = std::fs::remove_file(&helper_path);
    }

    let path = get_temp_installer_path();
    let expired = std::fs::metadata(&path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|mtime| mtime.elapsed().ok())
        .is_some_and(|age| age.as_secs() > INSTALLER_CACHE_MAX_AGE_SECS);
    if expired {
        let _ = std::fs::remove_file(&path);
    }
}
