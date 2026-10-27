use crate::Result;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;

pub fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

pub fn last_error(action: &str) -> String {
    format!("{action}：{}", std::io::Error::last_os_error())
}

pub struct Handle(pub HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.0); }
        }
    }
}

pub struct ComGuard(bool);
impl ComGuard {
    pub fn new() -> Self {
        Self(unsafe { windows_sys::Win32::System::Com::CoInitializeEx(null(), windows_sys::Win32::System::Com::COINIT_APARTMENTTHREADED as u32) } >= 0)
    }
}
impl Drop for ComGuard { fn drop(&mut self) { if self.0 { unsafe { windows_sys::Win32::System::Com::CoUninitialize(); } } } }

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

pub fn unique_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("{:x}-{:x}-{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos(), std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed))
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileId {
    pub volume: u32,
    pub index: u64,
}

pub fn identity(file: &File) -> Result<FileId> {
    let mut info = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) } == 0 {
        return Err(last_error("读取文件标识失败"));
    }
    Ok(FileId { volume: info.dwVolumeSerialNumber, index: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64 })
}

pub fn path_identity(path: &Path) -> Result<FileId> {
    let p = wide(path);
    let handle = unsafe { CreateFileW(p.as_ptr(), FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, null_mut()) };
    if handle == INVALID_HANDLE_VALUE { return Err(last_error("读取目录标识失败")); }
    let file = unsafe { File::from_raw_handle(handle as _) };
    identity(&file)
}

pub fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().ok_or("记录路径无父目录")?;
    fs::create_dir_all(parent).map_err(|e| format!("创建记录目录失败：{e}"))?;
    let tmp = parent.join(format!(".{}.tmp", unique_id()));
    let result = (|| {
        let mut file = OpenOptions::new().create_new(true).write(true).open(&tmp).map_err(|e| e.to_string())?;
        serde_json::to_writer_pretty(&mut file, value).map_err(|e| e.to_string())?;
        file.write_all(b"\n").map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        if unsafe { MoveFileExW(wide(&tmp).as_ptr(), wide(path).as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } == 0 {
            return Err(last_error("保存记录失败"));
        }
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(&tmp); }
    result
}

pub fn data_dir() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let directory = exe.parent().ok_or("无法确定程序目录")?.join("data");
    fs::create_dir_all(&directory).map_err(|e| format!("程序目录不可写，请把 sys tools 放到可写目录再运行：{e}"))?;
    Ok(directory)
}

pub fn log_action(data: &Path, action: &str, result: &str) {
    // 日志只保存在本机。限制单个日志大小，不记录卸载命令或连接参数。
    let path = data.join("operations.log");
    if fs::metadata(&path).is_ok_and(|m| m.len() > 512 * 1024) {
        let previous = data.join("operations.previous.log");
        // Windows 在目标文件已存在时不会覆盖重命名；先移除旧备份，
        // 避免轮转失败后 operations.log 无限增长。
        let _ = fs::remove_file(&previous);
        let _ = fs::rename(&path, previous);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}\t{}\t{}", now(), action, result.replace(['\r', '\n'], " "));
    }
}

