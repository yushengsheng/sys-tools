use crate::{platform::wide, Result};
use std::fs;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use windows_sys::core::GUID;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::*;

pub fn known_folder(id: &GUID) -> Result<PathBuf> {
    let mut p = null_mut();
    let code = unsafe { SHGetKnownFolderPath(id, 0, null_mut(), &mut p) };
    if code < 0 || p.is_null() { return Err(format!("读取 Windows 用户目录失败：{code:#x}")); }
    let mut len = 0;
    unsafe { while *p.add(len) != 0 { len += 1; } }
    let value = PathBuf::from(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) }));
    unsafe { CoTaskMemFree(p as _); }
    Ok(value)
}

pub fn windows_dir() -> Result<PathBuf> {
    let mut buffer = vec![0u16; 32768];
    let n = unsafe { windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if n == 0 || n as usize >= buffer.len() { return Err("无法取得 Windows 目录".into()); }
    Ok(PathBuf::from(String::from_utf16_lossy(&buffer[..n as usize])))
}

pub fn key(path: &Path) -> String {
    let s = path.to_string_lossy().replace('/', "\\");
    s.strip_prefix("\\\\?\\").unwrap_or(&s).trim_end_matches('\\').to_lowercase()
}

pub fn within(path: &Path, root: &Path) -> bool {
    let p = key(path);
    let r = key(root);
    p == r || p.strip_prefix(&r).is_some_and(|rest| rest.starts_with('\\'))
}

pub fn is_reparse(meta: &fs::Metadata) -> bool {
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

pub fn reject_reparse_ancestors(path: &Path) -> Result<()> {
    for part in path.ancestors() {
        if let Ok(m) = fs::symlink_metadata(part) {
            if is_reparse(&m) { return Err("路径含目录链接或重解析点，请直接选择真实目录".into()); }
        }
    }
    Ok(())
}

pub fn existing_directory(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() { return Err("请输入完整的本地目录路径".into()); }
    reject_reparse_ancestors(path)?;
    let raw = path.to_string_lossy();
    if raw.len() == 3 && raw.as_bytes()[1] == b':' && raw.as_bytes()[2] == b'\\' && path.is_dir() {
        return Ok(path.to_path_buf());
    }
    // Windows `canonicalize` may reject an extended-length drive root such as
    // `\\?\\D:\\` even though the root is usable. Preserve the validated
    // absolute path in that case; later operations still perform their own
    // identity and file-system checks.
    let p = fs::canonicalize(path).or_else(|_| if path.is_dir() { Ok(path.to_path_buf()) } else { Err(std::io::Error::last_os_error()) }).map_err(|e| format!("目录不存在或无法访问：{e}"))?;
    if !p.is_dir() { return Err("所选路径不是目录".into()); }
    Ok(p)
}

pub fn check_ntfs(path: &Path) -> Result<u64> {
    let mut volume = vec![0u16; 32768];
    if unsafe { GetVolumePathNameW(wide(path).as_ptr(), volume.as_mut_ptr(), volume.len() as u32) } == 0 { return Err(crate::platform::last_error("读取磁盘信息失败")); }
    if unsafe { GetDriveTypeW(volume.as_ptr()) } != 3 { return Err("第一版只支持本机固定磁盘上的 NTFS 目录".into()); }
    let mut fs_name = [0u16; 32];
    if unsafe { GetVolumeInformationW(volume.as_ptr(), null_mut(), 0, null_mut(), null_mut(), null_mut(), fs_name.as_mut_ptr(), fs_name.len() as u32) } == 0 { return Err(crate::platform::last_error("读取文件系统失败")); }
    let n = fs_name.iter().position(|x| *x == 0).unwrap_or(fs_name.len());
    if String::from_utf16_lossy(&fs_name[..n]) != "NTFS" { return Err("第一版只支持 NTFS，未对其他文件系统作兼容承诺".into()); }
    let mut available = 0;
    if unsafe { GetDiskFreeSpaceExW(wide(path).as_ptr(), &mut available, null_mut(), null_mut()) } == 0 { return Err(crate::platform::last_error("读取剩余空间失败")); }
    Ok(available)
}

pub fn check_migration_source(source: &Path, data: &Path) -> Result<()> {
    let win = fs::canonicalize(windows_dir()?).map_err(|e| e.to_string())?;
    if within(source, &win) || within(&win, source) { return Err("Windows 目录及其父目录禁止迁移".into()); }
    let critical = [FOLDERID_Profile, FOLDERID_LocalAppData, FOLDERID_RoamingAppData, FOLDERID_ProgramData, FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86, FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads];
    for id in &critical {
        if let Ok(p) = known_folder(id).and_then(|p| fs::canonicalize(p).map_err(|e| e.to_string())) {
            if within(&p, source) { return Err("系统目录、用户目录和标准存储根目录不能整体迁移，请选择某个软件的子目录".into()); }
        }
    }
    if let Ok(pd) = known_folder(&FOLDERID_ProgramData) {
        if within(source, &pd.join("Microsoft")) { return Err("Microsoft 系统共享数据禁止迁移".into()); }
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if within(&exe, source) || within(data, source) || within(source, data) { return Err("sys tools 自身及迁移记录目录不能迁移".into()); }
    let name = source.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
    if ["windowsapps", "winsxs", "system volume information", "$recycle.bin", "recovery"].contains(&name.as_str()) { return Err("所选目录由系统管理，禁止迁移".into()); }
    if ["chrome.exe", "msedge.exe", "BitBrowser.exe", "bitbrowser.exe"].iter().any(|exe| source.join(exe).is_file()) { return Err("该浏览器安装目录需要专项验证，第一版暂不迁移。可选择已确认独立的数据子目录".into()); }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn containment_uses_component_boundaries() {
        assert!(within(Path::new("C:\\Cache\\a"), Path::new("c:\\cache")));
        assert!(!within(Path::new("C:\\Cache2\\a"), Path::new("c:\\cache")));
        assert!(within(Path::new("\\\\?\\C:\\Cache\\a"), Path::new("C:\\Cache")));
    }
}
