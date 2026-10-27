use crate::{apps::App, paths, platform::{self, FileId}, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::UI::Shell::*;

pub const CATEGORIES: &[&str] = &["全部", "临时文件", "软件缓存", "软件文件", "软件数据", "系统文件", "用户文件 / 其他", "迁移 / 工具保护", "目录链接"];
pub const PAGE_SIZE: usize = 500;
const INDEX_LIMIT: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Item {
    pub path: PathBuf, pub size: u64, pub category: String, pub owner: String,
    pub deletable: bool, pub modified: u64, pub created: u64,
    #[serde(skip)] pub id: Option<FileId>,
}
#[derive(Clone, Default)]
pub struct Scan {
    pub index: PathBuf, pub root: PathBuf, pub categories: BTreeMap<String, (usize, u64)>,
    pub files: usize, pub skipped: usize, pub limited: bool, pub cancelled: bool, pub bytes: u64,
}
pub fn fixed_drives() -> Vec<String> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26).filter_map(|i| {
        if mask & (1 << i) == 0 { return None; }
        let root = format!("{}:\\", (b'A' + i as u8) as char);
        (unsafe { GetDriveTypeW(platform::wide(&root).as_ptr()) } == 3).then_some(root)
    }).collect()
}

struct Rules {
    apps: Vec<(PathBuf, String)>, temp: Vec<PathBuf>, system: Vec<PathBuf>,
    data_roots: Vec<PathBuf>, programs: Vec<PathBuf>, excluded: Vec<PathBuf>,
}
impl Rules {
    fn new(apps: &[App], data: &Path) -> Result<Self> {
        let history = crate::migration::history(data)?;
        if history.unreadable != 0 { return Err("有损坏的迁移记录，请先检查记录，再进行磁盘扫描".into()); }
        let mut excluded = vec![data.to_path_buf()];
        if let Ok(exe) = std::env::current_exe() { if let Some(root) = exe.parent() { excluded.push(root.to_path_buf()); } }
        for r in history.records { excluded.extend([r.source, r.target, r.backup, r.staging, r.restore_staging, r.held_link]); }
        let mut temp = vec![paths::windows_dir()?.join("Temp")];
        let data_roots = [FOLDERID_LocalAppData, FOLDERID_RoamingAppData, FOLDERID_ProgramData].iter().filter_map(|id| paths::known_folder(id).ok()).collect();
        if let Ok(local) = paths::known_folder(&FOLDERID_LocalAppData) { temp.push(local.join("Temp")); }
        let programs = [FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86, FOLDERID_ProgramFilesCommon, FOLDERID_ProgramFilesCommonX86].iter().filter_map(|id| paths::known_folder(id).ok()).collect();
        let mut system = vec![paths::windows_dir()?];
        if let Ok(pd) = paths::known_folder(&FOLDERID_ProgramData) { system.push(pd.join("Microsoft")); }
        let mut locations: Vec<_> = apps.iter().filter(|a| a.location.is_absolute() && a.location.is_dir()).map(|a| (a.location.clone(), a.name.clone())).collect();
        locations.sort_by_key(|(p, _)| std::cmp::Reverse(p.components().count()));
        Ok(Self { apps: locations, temp, system, data_roots, programs, excluded })
    }
    fn classify(&self, path: &Path, meta: &fs::Metadata) -> (String, String, bool) {
        // Avoid allocating a lowercase String for every path component while scanning
        // large fixed disks. Windows paths are case-insensitive, so ASCII comparison
        // is sufficient for the well-known directory names below.
        let has_component = |names: &[&str]| path.components().any(|c| {
            let value = c.as_os_str();
            names.iter().any(|name| value.to_string_lossy().eq_ignore_ascii_case(name))
        });
        if self.excluded.iter().any(|x| paths::within(path, x)) { return (CATEGORIES[7].into(), String::new(), false); }
        if paths::is_reparse(meta) { return (CATEGORIES[8].into(), String::new(), false); }
        let owner = self.apps.iter().find(|(p, _)| paths::within(path, p)).map(|(_, n)| n.clone()).unwrap_or_default();
        if self.temp.iter().any(|p| paths::within(path, p)) { return (CATEGORIES[1].into(), owner, meta.is_file()); }
        if self.system.iter().any(|p| paths::within(path, p))
            || ["pagefile.sys", "hiberfil.sys", "swapfile.sys", "ntuser.dat", "usrclass.dat"].iter().any(|n| path.file_name().is_some_and(|s| s.eq_ignore_ascii_case(n)))
            || has_component(&["$recycle.bin", "system volume information", "windowsapps", "recovery", "$windows.~bt", "windows.old"])
            || meta.file_attributes() & FILE_ATTRIBUTE_SYSTEM != 0 { return (CATEGORIES[5].into(), owner, false); }
        let in_data = self.data_roots.iter().any(|p| paths::within(path, p));
        let cache = has_component(&["cache", "cache2", "code cache", "gpucache", "gpu cache", "shadercache", "dawncache"]);
        if cache && (in_data || !owner.is_empty()) { return (CATEGORIES[2].into(), owner, meta.is_file()); }
        if !owner.is_empty() || self.programs.iter().any(|p| paths::within(path, p)) { return (CATEGORIES[3].into(), owner, false); }
        if in_data { return (CATEGORIES[4].into(), owner, meta.is_file()); }
        (CATEGORIES[6].into(), owner, meta.is_file())
    }
}

pub fn scan(root: &Path, apps: &[App], data: &Path, cancel: &AtomicBool, progress: &dyn Fn(String)) -> Result<Scan> {
    let root = paths::existing_directory(root)?;
    scan_from_roots(root.clone(), vec![root], apps, data, cancel, progress)
}

/// 只检查 Windows 与当前用户的临时目录，避免为了找可清理文件而遍历整块磁盘。
pub fn scan_temp(root: &Path, apps: &[App], data: &Path, cancel: &AtomicBool, progress: &dyn Fn(String)) -> Result<Scan> {
    let root = paths::existing_directory(root)?;
    let mut starts = vec![paths::windows_dir()?.join("Temp")];
    if let Ok(local) = paths::known_folder(&FOLDERID_LocalAppData) { starts.push(local.join("Temp")); }
    starts.retain(|path| path.is_dir()
        && fs::symlink_metadata(path).is_ok_and(|meta| !paths::is_reparse(&meta))
        && paths::within(path, &root));
    starts.sort(); starts.dedup();
    if starts.is_empty() { return Err("所选磁盘没有可读取的临时目录".into()); }
    scan_from_roots(root, starts, apps, data, cancel, progress)
}

fn scan_from_roots(root: PathBuf, starts: Vec<PathBuf>, apps: &[App], data: &Path, cancel: &AtomicBool, progress: &dyn Fn(String)) -> Result<Scan> {
    let rules = Rules::new(apps, data)?;
    let folder = data.join("disk-scans"); fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    paths::reject_reparse_ancestors(&folder)?;
    let index = folder.join(format!("{}.jsonl", platform::unique_id()));
    let mut guard = ScanIndexGuard { path: index.clone(), keep: false };
    let mut writer = BufWriter::with_capacity(256 * 1024, OpenOptions::new().create_new(true).write(true).open(&index).map_err(|e| e.to_string())?);
    let mut out = Scan { index, root: root.clone(), ..Scan::default() };
    let mut stack = starts.into_iter().map(|path| (path, 0usize)).collect::<Vec<_>>(); let mut written = 0u64; let mut last = Instant::now();
    'scan: while let Some((dir, depth)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) { out.cancelled = true; break; }
        if depth > 256 || stack.len() > 200_000 { out.limited = true; out.skipped += 1; continue; }
        let entries = match fs::read_dir(&dir) { Ok(v) => v, Err(_) => { out.skipped += 1; continue; } };
        for entry in entries {
            if cancel.load(Ordering::Relaxed) { out.cancelled = true; break 'scan; }
            let Ok(entry) = entry else { out.skipped += 1; continue; }; let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else { out.skipped += 1; continue; };
            let (category, owner, deletable) = rules.classify(&path, &meta);
            if meta.is_dir() && !paths::is_reparse(&meta) {
                if category == CATEGORIES[7] { out.skipped += 1; } else { stack.push((path, depth + 1)); }
                continue;
            }
            if !meta.is_file() && !paths::is_reparse(&meta) { continue; }
            let size = if paths::is_reparse(&meta) { 0 } else { meta.len() };
            let item = Item { path, size, category: category.clone(), owner, deletable, modified: meta.last_write_time(), created: meta.creation_time(), id: None };
            let line = serde_json::to_vec(&item).map_err(|e| e.to_string())?;
            writer.write_all(&line).and_then(|_| writer.write_all(b"\n")).map_err(|e| e.to_string())?;
            written += line.len() as u64 + 1; out.files += 1; out.bytes = out.bytes.saturating_add(size);
            let totals = out.categories.entry(category).or_default(); totals.0 += 1; totals.1 = totals.1.saturating_add(size);
            if last.elapsed().as_millis() >= 250 { progress(format!("已扫描 {} 项 / {}，跳过 {} 处", out.files, crate::format_size(out.bytes), out.skipped)); last = Instant::now(); }
            if written >= INDEX_LIMIT { out.limited = true; break 'scan; }
        }
    }
    writer.flush().map_err(|e| e.to_string())?;
    guard.keep = true;
    Ok(out)
}

struct ScanIndexGuard { path: PathBuf, keep: bool }
impl Drop for ScanIndexGuard {
    fn drop(&mut self) { if !self.keep { let _ = fs::remove_file(&self.path); } }
}

pub fn discard_stale(data: &Path) {
    let folder = data.join("disk-scans");
    if paths::reject_reparse_ancestors(&folder).is_err() { return; }
    let Ok(entries) = fs::read_dir(folder) else { return; };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "jsonl")
            && fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && !paths::is_reparse(&m)) {
            let _ = fs::remove_file(path);
        }
    }
}
pub fn page(scan: &Scan, category: &str, offset: usize) -> Result<(Vec<Item>, bool)> {
    let file = File::open(&scan.index).map_err(|e| e.to_string())?;
    let mut items = Vec::new(); let mut count = 0;
    for line in BufReader::new(file).lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.len() > 128 * 1024 { return Err("扫描索引异常，拒绝读取".into()); }
        let mut item: Item = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        if category != CATEGORIES[0] && item.category != category { continue; }
        if count < offset { count += 1; continue; }
        if items.len() == PAGE_SIZE { return Ok((items, true)); }
        if item.deletable { item.id = platform::path_identity(&item.path).ok(); }
        items.push(item);
    }
    Ok((items, false))
}

/// 删除本次扫描的落盘索引。索引只服务于当前窗口，避免每次扫描永久累积 JSONL 文件。
pub fn discard(scan: &Scan) {
    let _ = fs::remove_file(&scan.index);
}

pub fn clean(items: &[Item], scan: &Scan, data: &Path, cancel: &AtomicBool, progress: &dyn Fn(String)) -> (usize, u64, usize) {
    let mut removed = 0; let mut bytes = 0; let mut skipped = 0;
    let Ok(apps) = crate::apps::scan() else { return (0, 0, items.len()); };
    let Ok(rules) = Rules::new(&apps, data) else { return (0, 0, items.len()); };
    for (index, item) in items.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) { break; }
        if !item.deletable || !paths::within(&item.path, &scan.root) || paths::reject_reparse_ancestors(&item.path).is_err() || rules.excluded.iter().any(|p| paths::within(&item.path, p)) { skipped += 1; continue; }
        let Ok(meta) = fs::symlink_metadata(&item.path) else { skipped += 1; continue; };
        if !rules.classify(&item.path, &meta).2 || !meta.is_file() || meta.len() != item.size || meta.last_write_time() != item.modified || meta.creation_time() != item.created { skipped += 1; continue; }
        let Ok(file) = OpenOptions::new().access_mode(DELETE | FILE_READ_ATTRIBUTES).share_mode(FILE_SHARE_READ).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(&item.path) else { skipped += 1; continue; };
        let valid = file.metadata().is_ok_and(|m| !paths::is_reparse(&m) && m.len() == item.size && m.last_write_time() == item.modified && m.creation_time() == item.created);
        if !valid || item.id.is_none() || platform::identity(&file).ok() != item.id { skipped += 1; continue; }
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        if unsafe { SetFileInformationByHandle(file.as_raw_handle() as _, FileDispositionInfo, &disposition as *const _ as _, std::mem::size_of_val(&disposition) as u32) } != 0 { removed += 1; bytes += item.size; } else { skipped += 1; }
        if index % 100 == 0 { progress(format!("正在清理已确认项目：{} / {}", index + 1, items.len())); }
    }
    (removed, bytes, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn index_preserves_categories_and_skips_link_targets_and_changed_files() {
        let base = std::env::temp_dir().join(format!("systools-disk-test-{}", platform::unique_id()));
        let root = base.join("scan"); let data = base.join("records"); let outside = base.join("outside");
        fs::create_dir_all(&root).unwrap(); fs::create_dir_all(&data).unwrap(); fs::create_dir(&outside).unwrap();
        fs::write(root.join("personal.txt"), b"keep").unwrap(); fs::write(outside.join("outside.txt"), b"outside").unwrap();
        junction::create(&outside, root.join("link")).unwrap();
        let result = scan(&root, &[], &data, &AtomicBool::new(false), &|_| {}).unwrap();
        assert_eq!(result.files, 2);
        let (items, _) = page(&result, CATEGORIES[0], 0).unwrap();
        assert!(items.iter().find(|x| x.category == "目录链接").is_some_and(|x| !x.deletable));
        let personal = items.iter().find(|x| x.path.ends_with("personal.txt")).unwrap().clone();
        fs::write(&personal.path, b"changed").unwrap();
        assert_eq!(clean(&[personal], &result, &data, &AtomicBool::new(false), &|_| {}).0, 0);
        let next = scan(&root, &[], &data, &AtomicBool::new(false), &|_| {}).unwrap();
        assert!(result.index.exists(), "新扫描不能移除仍在使用的旧索引");
        discard(&result);
        assert!(!result.index.exists());
        discard_stale(&data);
        assert!(!next.index.exists());
        fs::remove_dir(root.join("link")).unwrap(); fs::remove_dir_all(base).unwrap();
    }
}
