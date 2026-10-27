use crate::{paths, platform::{self, FileId}, Result};
use std::fs::{self, OpenOptions};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData;

const MIN_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
const MAX_FILES: usize = 10000;

#[derive(Clone, Debug)]
pub struct Candidate {
    pub path: PathBuf,
    pub size: u64,
    pub modified: u64,
    id: FileId,
    scope: PathBuf,
}

#[derive(Default)]
pub struct Scan {
    pub files: Vec<Candidate>,
    pub skipped: usize,
    pub limited: bool,
}

fn roots() -> Result<Vec<PathBuf>> {
    Ok(vec![paths::known_folder(&FOLDERID_LocalAppData)?.join("Temp"), paths::windows_dir()?.join("Temp")])
}

pub fn scan(data: &Path, cancel: &AtomicBool) -> Result<Scan> {
    let mut excluded = vec![data.to_path_buf()];
    if let Ok(exe) = std::env::current_exe() { if let Some(parent) = exe.parent() { excluded.push(parent.to_path_buf()); } }
    let history = crate::migration::history(data)?;
    if history.unreadable > 0 { return Err("有无法读取的迁移记录，暂不清理；请先检查 data/migrations".into()); }
    for r in history.records {
        excluded.extend([r.source, r.target, r.backup, r.staging, r.restore_staging]);
    }
    scan_roots(&roots()?, &excluded, cancel)
}

fn scan_roots(roots: &[PathBuf], excluded: &[PathBuf], cancel: &AtomicBool) -> Result<Scan> {
    let mut out = Scan::default();
    // 只计算一次时间，避免每个文件都调用系统时钟。
    let now = SystemTime::now();
    for root in roots {
        if cancel.load(Ordering::Relaxed) { break; }
        let Ok(root) = paths::existing_directory(root) else { out.skipped += 1; continue; };
        let mut stack = vec![(root.clone(), 0usize)];
        while let Some((directory, depth)) = stack.pop() {
            if cancel.load(Ordering::Relaxed) || out.limited { return Ok(out); }
            if excluded.iter().any(|x| paths::within(&directory, x)) { continue; }
            let entries = match fs::read_dir(&directory) { Ok(e) => e, Err(_) => { out.skipped += 1; continue; } };
            for e in entries {
                if cancel.load(Ordering::Relaxed) { return Ok(out); }
                let Ok(e) = e else { out.skipped += 1; continue; };
                let path = e.path();
                if excluded.iter().any(|x| paths::within(&path, x)) { continue; }
                let Ok(m) = fs::symlink_metadata(&path) else { out.skipped += 1; continue; };
                if paths::is_reparse(&m) { out.skipped += 1; continue; }
                if m.is_dir() {
                    if depth < 64 && !e.file_name().to_string_lossy().contains("systools-") { stack.push((path, depth + 1)); }
                    else { out.skipped += 1; }
                    continue;
                }
                if !m.is_file() || m.file_attributes() & (FILE_ATTRIBUTE_SYSTEM | FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_ENCRYPTED) != 0 || !m.modified().ok().and_then(|t| now.duration_since(t).ok()).is_some_and(|age| age >= MIN_AGE) { continue; }
                // 保留文件身份快照，删除时仍能阻止扫描后同名文件被替换。
                let Ok(file) = OpenOptions::new().access_mode(FILE_READ_ATTRIBUTES).share_mode(FILE_SHARE_READ).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(&path) else { out.skipped += 1; continue; };
                let Ok(id) = platform::identity(&file) else { out.skipped += 1; continue; };
                out.files.push(Candidate { path, size: m.len(), modified: m.last_write_time(), id, scope: root.clone() });
                if out.files.len() >= MAX_FILES { out.limited = true; break; }
            }
        }
    }
    out.files.sort_by(|a, b| b.size.cmp(&a.size));
    Ok(out)
}

#[derive(Default)]
pub struct Outcome { pub deleted: usize, pub bytes: u64, pub skipped: usize, pub cancelled: bool }

fn delete_one(candidate: &Candidate) -> Result<()> {
    paths::reject_reparse_ancestors(&candidate.path)?;
    let actual = fs::canonicalize(&candidate.path).map_err(|e| e.to_string())?;
    if !paths::within(&actual, &candidate.scope) || paths::key(&actual) == paths::key(&candidate.scope) { return Err("临时文件超出扫描范围".into()); }
    // 在同一文件句柄上核对身份并标记删除，避免核验后同名文件被替换。
    let file = OpenOptions::new().access_mode(DELETE | FILE_READ_ATTRIBUTES).share_mode(FILE_SHARE_READ).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(&candidate.path).map_err(|e| e.to_string())?;
    let m = file.metadata().map_err(|e| e.to_string())?;
    if !m.is_file() || paths::is_reparse(&m) || platform::identity(&file).ok() != Some(candidate.id) || m.len() != candidate.size || m.last_write_time() != candidate.modified { return Err("扫描后文件发生变化，已跳过".into()); }
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    if unsafe { SetFileInformationByHandle(file.as_raw_handle() as _, FileDispositionInfo, &disposition as *const _ as _, std::mem::size_of_val(&disposition) as u32) } == 0 { return Err(platform::last_error("临时文件被占用或不可删除")); }
    drop(file);
    Ok(())
}

pub fn clean(files: &[Candidate], cancel: &AtomicBool, progress: &dyn Fn(String)) -> Outcome {
    let mut out = Outcome::default();
    for (i, item) in files.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) { out.cancelled = true; break; }
        if delete_one(item).is_ok() { out.deleted += 1; out.bytes += item.size; } else { out.skipped += 1; }
        if i % 50 == 0 { progress(format!("正在清理临时文件：{} / {}", i + 1, files.len())); }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scan_and_delete_skip_junction_targets_and_changed_files() {
        let base = std::env::temp_dir().join(format!("systools-clean-test-{}", platform::unique_id()));
        let root = base.join("temp"); let outside = base.join("outside");
        fs::create_dir_all(&root).unwrap(); fs::create_dir(&outside).unwrap();
        for path in [root.join("old.tmp"), outside.join("keep.tmp")] {
            fs::write(&path, b"old").unwrap();
            OpenOptions::new().write(true).open(path).unwrap().set_modified(SystemTime::now() - MIN_AGE - Duration::from_secs(30)).unwrap();
        }
        junction::create(&outside, root.join("link")).unwrap();
        let scan = scan_roots(&[root.clone()], &[], &AtomicBool::new(false)).unwrap();
        assert_eq!(scan.files.len(), 1);
        fs::write(root.join("old.tmp"), b"changed after scan").unwrap();
        assert!(delete_one(&scan.files[0]).is_err());
        assert!(outside.join("keep.tmp").exists());
        junction::delete(root.join("link")).unwrap();
        fs::remove_dir_all(&base).unwrap();
    }
    #[test]
    fn deletion_requires_unchanged_snapshot() {
        let root = std::env::temp_dir().join(format!("systools-delete-test-{}", platform::unique_id()));
        fs::create_dir(&root).unwrap();
        let path = root.join("old.tmp"); fs::write(&path, b"content").unwrap();
        OpenOptions::new().write(true).open(&path).unwrap().set_modified(SystemTime::now() - MIN_AGE - Duration::from_secs(30)).unwrap();
        let scan = scan_roots(&[root.clone()], &[], &AtomicBool::new(false)).unwrap();
        let out = clean(&scan.files, &AtomicBool::new(false), &|_| {});
        assert_eq!(out.deleted, 1); assert!(!path.exists());
        fs::remove_dir(root).unwrap();
    }
}
