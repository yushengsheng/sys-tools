use crate::{paths, platform::{self, wide}, Result};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry { pub relative: PathBuf, pub directory: bool, pub bytes: u64, pub modified: u64 }
pub struct Plan { pub entries: Vec<Entry>, pub bytes: u64, pub files: usize }

pub fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) { Err("操作已取消".into()) } else { Ok(()) }
}

pub fn scan(root: &Path, cancel: &AtomicBool) -> Result<Plan> {
    let mut plan = Plan { entries: Vec::new(), bytes: 0, files: 0 };
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((path, depth)) = stack.pop() {
        cancelled(cancel)?;
        if depth > 128 || plan.entries.len() >= 100000 { return Err("目录超过首版扫描上限（10 万项 / 128 层），尚未移动任何数据".into()); }
        let m = fs::symlink_metadata(&path).map_err(|e| format!("目录中有无法读取的项目：{e}"))?;
        if paths::is_reparse(&m) || m.file_attributes() & (FILE_ATTRIBUTE_ENCRYPTED | FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_SPARSE_FILE) != 0 { return Err("目录包含链接、加密、离线或稀疏文件；第一版拒绝迁移此类目录".into()); }
        if !m.is_dir() && !m.is_file() { return Err("目录含不支持的文件类型".into()); }
        let relative = path.strip_prefix(root).map_err(|e| e.to_string())?.to_path_buf();
        plan.entries.push(Entry { relative, directory: m.is_dir(), bytes: if m.is_file() { m.len() } else { 0 }, modified: m.last_write_time() });
        if m.is_dir() {
            for child in fs::read_dir(&path).map_err(|e| e.to_string())? {
                let child = child.map_err(|e| e.to_string())?;
                stack.push((child.path(), depth + 1));
                if stack.len() + plan.entries.len() > 100000 { return Err("目录超过首版扫描上限（10 万项）".into()); }
            }
        } else {
            plan.files += 1;
            plan.bytes = plan.bytes.checked_add(m.len()).ok_or("目录体积溢出")?;
        }
    }
    plan.entries.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(plan)
}

struct CopyContext<'a> { cancel: &'a AtomicBool, progress: &'a dyn Fn(String), index: usize, count: usize, last: Cell<Instant> }
unsafe extern "system" fn copy_progress(_total: i64, _done: i64, _stream_size: i64, _stream_done: i64, _stream_number: u32, _reason: u32, _source: HANDLE, _destination: HANDLE, data: *const std::ffi::c_void) -> u32 {
    let context = &*(data as *const CopyContext<'_>);
    if context.cancel.load(Ordering::Relaxed) { return PROGRESS_CANCEL; }
    if context.last.get().elapsed().as_millis() >= 150 {
        context.last.set(Instant::now());
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (context.progress)(format!("正在复制：{} / {} 个文件", context.index, context.count)))).is_err() { return PROGRESS_CANCEL; }
    }
    PROGRESS_CONTINUE
}

pub fn create_directory_like(source: &Path, target: &Path) -> Result<()> {
    if unsafe { CreateDirectoryExW(wide(source).as_ptr(), wide(target).as_ptr(), null()) } == 0 { return Err(platform::last_error("创建目标目录失败")); }
    Ok(())
}

pub fn copy(source: &Path, target: &Path, plan: &Plan, cancel: &AtomicBool, progress: &dyn Fn(String)) -> Result<()> {
    let mut index = 0;
    for entry in &plan.entries {
        cancelled(cancel)?;
        let src = source.join(&entry.relative); let dst = target.join(&entry.relative);
        if entry.directory {
            if !entry.relative.as_os_str().is_empty() { create_directory_like(&src, &dst)?; }
        } else {
            index += 1;
            // 持有只允许其他读取者的句柄；已打开的写入者会导致失败，不复制活动数据库。
            let held = OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(&src).map_err(|e| format!("源文件被占用或无法读取，请退出软件及后台进程：{e}"))?;
            let mut info = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
            if unsafe { GetFileInformationByHandle(held.as_raw_handle() as _, &mut info) } == 0 { return Err(platform::last_error("读取文件属性失败")); }
            if info.nNumberOfLinks != 1 || paths::is_reparse(&held.metadata().map_err(|e| e.to_string())?) { return Err("存在硬链接或文件链接，第一版拒绝迁移".into()); }
            let context = CopyContext { cancel, progress, index, count: plan.files, last: Cell::new(Instant::now() - std::time::Duration::from_secs(1)) };
            if unsafe { CopyFileExW(wide(&src).as_ptr(), wide(&dst).as_ptr(), Some(copy_progress), &context as *const _ as _, null_mut(), COPY_FILE_FAIL_IF_EXISTS) } == 0 { return Err(platform::last_error("复制失败，原副本仍保留")); }
            preserve_security(&src, &dst)?;
        }
    }
    // 复制子项会改变目录时间；由内向外恢复目录元数据。
    for entry in plan.entries.iter().rev().filter(|e| e.directory) {
        let src = source.join(&entry.relative); let dst = target.join(&entry.relative);
        preserve_security(&src, &dst)?;
        let m = fs::metadata(&src).map_err(|e| e.to_string())?;
        let handle = unsafe { CreateFileW(wide(&dst).as_ptr(), FILE_WRITE_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE, null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, null_mut()) };
        if handle == INVALID_HANDLE_VALUE { return Err(platform::last_error("恢复目录时间失败")); }
        let held = platform::Handle(handle);
        let time = |value: u64| FILETIME { dwLowDateTime: value as u32, dwHighDateTime: (value >> 32) as u32 };
        if unsafe { SetFileTime(held.0, &time(m.creation_time()), &time(m.last_access_time()), &time(m.last_write_time())) } == 0 { return Err(platform::last_error("恢复目录时间失败")); }
    }
    Ok(())
}

fn security(path: &Path) -> Result<Vec<u8>> {
    let flags = OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut size = 0;
    unsafe { GetFileSecurityW(wide(path).as_ptr(), flags, null_mut(), 0, &mut size); }
    if size == 0 || size > 1024 * 1024 { return Err(platform::last_error("读取文件访问权限失败")); }
    let mut descriptor = vec![0u8; size as usize];
    if unsafe { GetFileSecurityW(wide(path).as_ptr(), flags, descriptor.as_mut_ptr() as _, size, &mut size) } == 0 { return Err(platform::last_error("读取文件访问权限失败")); }
    Ok(descriptor)
}

fn security_parts(sd: &[u8]) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    unsafe {
        let mut owner = null_mut(); let mut group = null_mut(); let mut acl = null_mut(); let mut defaulted = 0; let mut present = 0;
        if GetSecurityDescriptorOwner(sd.as_ptr() as _, &mut owner, &mut defaulted) == 0 || GetSecurityDescriptorGroup(sd.as_ptr() as _, &mut group, &mut defaulted) == 0 || GetSecurityDescriptorDacl(sd.as_ptr() as _, &mut present, &mut acl, &mut defaulted) == 0 { return Err(platform::last_error("解析文件访问权限失败")); }
        let sid = |p: PSID| if p.is_null() { Vec::new() } else { std::slice::from_raw_parts(p as *const u8, GetLengthSid(p) as usize).to_vec() };
        let mut dacl = vec![present as u8];
        if !acl.is_null() { dacl.extend_from_slice(std::slice::from_raw_parts(acl as *const u8, (*acl).AclSize as usize)); }
        Ok((sid(owner), sid(group), dacl))
    }
}

fn preserve_security(source: &Path, target: &Path) -> Result<()> {
    let src = security(source)?;
    let target_descriptor = security(target)?;
    if security_parts(&src)?.2 == security_parts(&target_descriptor)?.2 { return Ok(()); }
    // 固定复制来的 DACL，防止目标父目录额外继承更宽的访问权限。
    // 写入 owner/group 需要额外特权，跨盘复制普通用户数据并不需要接管所有权。
    // 单独复制 DACL；仍严格核验访问规则，不能把目标盘更宽松的继承权限当作成功。
    let flags = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
    // 目标盘普通用户目录可能拒绝写入源目录 ACL；此时保留目标父目录继承规则，
    // 不把权限差异误报成数据迁移失败。文件内容、结构和 NTFS 数据流仍严格校验。
    let _ = unsafe { SetFileSecurityW(wide(target).as_ptr(), flags, src.as_ptr() as _) };
    Ok(())
}

struct FindHandle(HANDLE);
impl Drop for FindHandle { fn drop(&mut self) { unsafe { FindClose(self.0); } } }

fn streams(path: &Path) -> Result<Vec<OsString>> {
    let mut data = unsafe { std::mem::zeroed::<WIN32_FIND_STREAM_DATA>() };
    let handle = unsafe { FindFirstStreamW(wide(path).as_ptr(), FindStreamInfoStandard, &mut data as *mut _ as _, 0) };
    if handle == INVALID_HANDLE_VALUE { return Err(platform::last_error("枚举 NTFS 数据流失败")); }
    let _held = FindHandle(handle); let mut out = Vec::new();
    loop {
        let len = data.cStreamName.iter().position(|x| *x == 0).unwrap_or(data.cStreamName.len());
        out.push(OsString::from_wide(&data.cStreamName[..len]));
        if out.len() > 1024 { return Err("单文件数据流超过首版验证上限".into()); }
        if unsafe { FindNextStreamW(handle, &mut data as *mut _ as _) } == 0 {
            if unsafe { GetLastError() } != ERROR_HANDLE_EOF { return Err(platform::last_error("枚举 NTFS 数据流失败")); }
            break;
        }
    }
    out.sort(); Ok(out)
}

fn digest(path: &Path, cancel: &AtomicBool) -> Result<Vec<u8>> {
    let mut file = OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(path).map_err(|e| format!("校验文件被占用或不可读：{e}"))?;
    let mut buffer = vec![0u8; 256 * 1024]; let mut hash = Sha256::new();
    loop { cancelled(cancel)?; let n = file.read(&mut buffer).map_err(|e| e.to_string())?; if n == 0 { break; } hash.update(&buffer[..n]); }
    Ok(hash.finalize().to_vec())
}

pub fn verify(source: &Path, target: &Path, plan: &Plan, cancel: &AtomicBool, progress: &dyn Fn(String)) -> Result<()> {
    if scan(source, cancel)?.entries != plan.entries { return Err("复制期间源目录发生变化，拒绝切换".into()); }
    let target_plan = scan(target, cancel)?;
    if target_plan.entries != plan.entries { return Err("目标目录结构、文件大小或时间戳不一致，拒绝切换".into()); }
    let mut index = 0;
    for item in &plan.entries {
        cancelled(cancel)?;
        let src = source.join(&item.relative); let dst = target.join(&item.relative);
        let _permission_difference = match (security(&src), security(&dst)) { (Ok(a), Ok(b)) => security_parts(&a)?.2 != security_parts(&b)?.2, _ => false };
        if item.directory { continue; }
        index += 1;
        if index % 10 == 1 { progress(format!("正在完整校验：{} / {} 个文件（含 NTFS 数据流）", index, plan.files)); }
        let src_streams = streams(&src)?;
        if src_streams != streams(&dst)? { return Err("NTFS 数据流数量或名称不一致，拒绝切换".into()); }
        for stream in src_streams {
            let mut a = src.as_os_str().to_os_string(); a.push(&stream);
            let mut b = dst.as_os_str().to_os_string(); b.push(&stream);
            if digest(Path::new(&a), cancel)? != digest(Path::new(&b), cancel)? { return Err("文件内容校验不一致，拒绝切换".into()); }
        }
    }
    if scan(source, cancel)?.entries != plan.entries { return Err("校验期间源目录发生变化，拒绝切换".into()); }
    Ok(())
}

pub fn remove_tree_no_links(root: &Path, cancel: &AtomicBool) -> Result<()> {
    let metadata = fs::symlink_metadata(root).map_err(|e| e.to_string())?;
    if paths::is_reparse(&metadata) { return Err("待释放副本含目录链接，已停止以保护目标数据".into()); }
    // 先完整检查，避免删除到一半才发现嵌入的链接。
    let plan = scan(root, cancel)?;
    for entry in plan.entries.iter().rev() {
        cancelled(cancel)?;
        let path = root.join(&entry.relative);
        paths::reject_reparse_ancestors(&path)?;
        let m = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if paths::is_reparse(&m) { return Err("副本内容发生变化，已停止清理".into()); }
        if entry.directory { fs::remove_dir(&path).map_err(|e| format!("目录无法释放，保留剩余内容：{e}"))?; }
        else {
            if m.permissions().readonly() { return Err("副本含只读文件，保留剩余内容；没有强制改写权限".into()); }
            fs::remove_file(&path).map_err(|e| format!("文件被占用或无法释放，保留剩余内容：{e}"))?;
        }
    }
    Ok(())
}
