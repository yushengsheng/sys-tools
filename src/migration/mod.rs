mod copy;
mod links;

use crate::{paths, platform::{self, FileId, wide}, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::AtomicBool;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::RestartManager::*;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Phase { Prepared, Copying, Verified, AwaitingReview, Active, Restoring, Restored, RolledBack, NeedsAttention }

impl Phase {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Prepared | Self::Copying | Self::Verified | Self::Restoring => "操作中断，需修复",
            Self::AwaitingReview => "已迁移，待确认释放原副本",
            Self::Active => "迁移生效",
            Self::Restored => "已恢复，目标副本保留",
            Self::RolledBack => "已回滚，检查目标半成品",
            Self::NeedsAttention => "需要检查，副本已保留",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub schema: u32,
    pub id: String,
    pub source: PathBuf,
    pub target: PathBuf,
    pub backup: PathBuf,
    pub staging: PathBuf,
    pub restore_staging: PathBuf,
    pub held_link: PathBuf,
    pub source_id: FileId,
    pub staging_id: Option<FileId>,
    pub target_id: Option<FileId>,
    pub restore_id: Option<FileId>,
    pub phase: Phase,
    pub bytes: u64,
    pub files: usize,
    pub created: u64,
    pub last_error: String,
    #[serde(default)]
    pub copies_released: bool,
}

impl Record {
    fn save(&self, data: &Path) -> Result<()> { platform::atomic_json(&data.join("migrations").join(format!("{}.json", self.id)), self) }
    pub fn label(&self) -> &'static str {
        if self.copies_released {
            match self.phase { Phase::Restored => return "已恢复，副本已释放", Phase::RolledBack => return "已回滚，半成品已释放", _ => {} }
        }
        self.phase.label()
    }
}

#[derive(Default)]
pub struct History { pub records: Vec<Record>, pub unreadable: usize }

pub fn history(data: &Path) -> Result<History> {
    let folder = data.join("migrations");
    if !folder.exists() { return Ok(History::default()); }
    paths::reject_reparse_ancestors(&folder)?;
    let mut out = History::default();
    for path in fs::read_dir(folder).map_err(|e| e.to_string())? {
        let path = path.map_err(|e| e.to_string())?.path();
        if path.extension().is_none_or(|e| e != "json") { continue; }
        if out.records.len() + out.unreadable >= 2000 { return Err("迁移记录超过读取上限，请先整理历史记录".into()); }
        let result: Result<Record> = (|| {
            let m = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            if paths::is_reparse(&m) || m.len() > 1024 * 1024 { return Err("不支持的记录文件".into()); }
            let record: Record = serde_json::from_reader(File::open(&path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            validate_layout(&record, data)?;
            if path.file_stem().is_none_or(|s| s != record.id.as_str()) { return Err("记录文件标识不匹配".into()); }
            Ok(record)
        })();
        match result { Ok(record) => out.records.push(record), Err(_e) => out.unreadable += 1 }
    }
    out.records.sort_by(|a, b| b.created.cmp(&a.created));
    Ok(out)
}

fn validate_layout(r: &Record, data: &Path) -> Result<()> {
    if r.schema != 1 || r.id.is_empty() || r.id.len() > 100 || !r.id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') { return Err("迁移记录格式不支持".into()); }
    let source_parent = r.source.parent().ok_or("原目录缺少父目录")?;
    let target_parent = r.target.parent().ok_or("目标缺少父目录")?;
    if !r.source.is_absolute() || !r.target.is_absolute() || r.source.file_name() != r.target.file_name() || paths::within(&r.target, &r.source) || paths::within(&r.source, &r.target)
        || r.backup != source_parent.join(format!(".systools-backup-{}", r.id))
        || r.staging != target_parent.join(format!(".systools-copy-{}", r.id))
        || r.restore_staging != source_parent.join(format!(".systools-restore-{}", r.id))
        || r.held_link != source_parent.join(format!(".systools-link-{}", r.id)) { return Err("迁移记录路径不符合本工具的布局，拒绝操作".into()); }
    paths::check_migration_source(&r.source, data)?;
    paths::existing_directory(source_parent)?;
    paths::existing_directory(target_parent)?;
    Ok(())
}

fn operation_lock(data: &Path) -> Result<File> {
    fs::create_dir_all(data).map_err(|e| e.to_string())?;
    paths::reject_reparse_ancestors(data)?;
    OpenOptions::new().create(true).truncate(false).read(true).write(true).share_mode(0).open(data.join("migration.lock")).map_err(|_| "另一个迁移操作正在进行，请等待结束".into())
}

fn check_identity(path: &Path, expected: Option<FileId>) -> Result<()> {
    paths::reject_reparse_ancestors(path)?;
    if expected.is_none() || platform::path_identity(path)? != expected.unwrap() { return Err("副本身份与记录不符，已停止操作并保留数据".into()); }
    Ok(())
}

struct RmSession(u32);
impl Drop for RmSession { fn drop(&mut self) { unsafe { RmEndSession(self.0); } } }

fn check_locks(root: &Path, plan: &copy::Plan, cancel: &AtomicBool) -> Result<()> {
    let mut session = 0; let mut key = [0u16; 33];
    let code = unsafe { RmStartSession(&mut session, 0, key.as_mut_ptr()) };
    if code != 0 { return Err(format!("无法完成系统占用检查（{code}），尚未迁移")); }
    let held = RmSession(session);
    let files: Vec<_> = plan.entries.iter().filter(|e| !e.directory).collect();
    for group in files.chunks(512) {
        copy::cancelled(cancel)?;
        let names: Vec<_> = group.iter().map(|e| wide(root.join(&e.relative))).collect();
        let pointers: Vec<_> = names.iter().map(|x| x.as_ptr()).collect();
        let code = unsafe { RmRegisterResources(held.0, pointers.len() as u32, pointers.as_ptr(), 0, null(), 0, null()) };
        if code != 0 { return Err(format!("无法检查部分文件的占用（{code}），请关闭软件后重试")); }
    }
    for _ in 0..3 {
        let mut needed = 0; let mut count = 0; let mut reasons = 0;
        let code = unsafe { RmGetList(held.0, &mut needed, &mut count, null_mut(), &mut reasons) };
        if code == 0 && needed == 0 { return Ok(()); }
        if code != ERROR_MORE_DATA || needed > 1024 { return Err(format!("系统占用检查未完成（{code}），已停止迁移")); }
        let mut processes = vec![RM_PROCESS_INFO::default(); needed as usize]; count = needed;
        let code = unsafe { RmGetList(held.0, &mut needed, &mut count, processes.as_mut_ptr(), &mut reasons) };
        if code == ERROR_MORE_DATA { continue; }
        if code != 0 { return Err(format!("系统占用检查失败（{code}）")); }
        if count == 0 { return Ok(()); }
        let names: Vec<_> = processes[..count as usize].iter().take(5).map(|p| {
            let n = p.strAppName.iter().position(|x| *x == 0).unwrap_or(p.strAppName.len());
            format!("{}（进程 {}）", String::from_utf16_lossy(&p.strAppName[..n]), p.Process.dwProcessId)
        }).collect();
        return Err(format!("文件仍被占用，请自行退出后重试：{}。sys tools 不会强行结束进程", names.join("、")));
    }
    Err("占用状态持续变化，请退出相关软件后重试".into())
}

pub fn migrate(source: &Path, destination_parent: &Path, data: &Path, cancel: &AtomicBool, progress: &dyn Fn(String)) -> Result<Record> {
    let _lock = operation_lock(data)?;
    let h = history(data)?;
    if h.unreadable != 0 { return Err("有无法读取的迁移记录，暂不开始新迁移；请先检查 data/migrations".into()); }
    let source = paths::existing_directory(source).map_err(|e| format!("源目录检查失败：{e}"))?;
    let parent = paths::existing_directory(destination_parent).map_err(|e| format!("目标父目录检查失败：{e}"))?;
    paths::check_migration_source(&source, data).map_err(|e| format!("源目录保护检查失败：{e}"))?;
    paths::check_ntfs(&source).map_err(|e| format!("源磁盘检查失败：{e}"))?;
    let available = paths::check_ntfs(&parent).map_err(|e| format!("目标磁盘检查失败：{e}"))?;
    let name = source.file_name().ok_or("不能迁移磁盘根目录")?;
    let target = parent.join(name);
    paths::check_migration_source(&target, data)?;
    if paths::within(&target, &source) || paths::within(&source, &target) || fs::symlink_metadata(&target).is_ok() { return Err("目标不能在源目录内、包含源目录，且目标同名目录必须不存在".into()); }
    for r in h.records {
        if r.phase != Phase::RolledBack && r.phase != Phase::Restored && [r.source, r.target, r.backup, r.staging].iter().any(|p| paths::within(&source, p) || paths::within(p, &source) || paths::within(&target, p) || paths::within(p, &target)) { return Err("所选路径与已有迁移重叠，请先处理原迁移记录".into()); }
    }
    progress("正在检查目录与文件占用…".into());
    let plan = copy::scan(&source, cancel)?;
    if available < plan.bytes.saturating_add(64 * 1024 * 1024) { return Err("目标盘空间不足，需要完整副本空间并保留 64 MiB 余量".into()); }
    check_locks(&source, &plan, cancel)?;
    copy::cancelled(cancel)?;
    let id = platform::unique_id();
    let source_parent = source.parent().ok_or("源目录无父目录")?;
    let mut r = Record { schema: 1, id: id.clone(), source: source.clone(), target, backup: source_parent.join(format!(".systools-backup-{id}")), staging: parent.join(format!(".systools-copy-{id}")), restore_staging: source_parent.join(format!(".systools-restore-{id}")), held_link: source_parent.join(format!(".systools-link-{id}")), source_id: platform::path_identity(&source)?, staging_id: None, target_id: None, restore_id: None, phase: Phase::Prepared, bytes: plan.bytes, files: plan.files, created: platform::now(), last_error: String::new(), copies_released: false };
    r.save(data)?;
    let work = (|| {
        copy::create_directory_like(&r.source, &r.staging)?;
        r.staging_id = Some(platform::path_identity(&r.staging)?); r.save(data)?;
        links::preflight(&r.staging, &r.source)?;
        r.phase = Phase::Copying; r.save(data)?;
        fs::rename(&r.source, &r.backup).map_err(|e| format!("无法保留原目录，可能仍被占用：{e}"))?;
        copy::copy(&r.backup, &r.staging, &plan, cancel, progress)?;
        copy::verify(&r.backup, &r.staging, &plan, cancel, progress)?;
        check_locks(&r.backup, &plan, cancel)?;
        copy::cancelled(cancel)?;
        r.phase = Phase::Verified; r.target_id = r.staging_id; r.save(data)?;
        fs::rename(&r.staging, &r.target).map_err(|e| format!("提交目标目录失败：{e}"))?;
        links::create(&r.target, &r.source)?;
        // 切换到最终目录后再次核对链接、身份和目录清单，尽早发现权限或文件系统导致的不可用状态。
        links::verify(&r.source, &r.target)?;
        check_identity(&r.target, r.target_id)?;
        let final_plan = copy::scan(&r.target, cancel)?;
        if final_plan.entries != plan.entries { return Err("迁移后目标目录校验不一致，软件可能无法正常使用".into()); }
        r.phase = Phase::AwaitingReview; r.last_error.clear(); r.save(data)?;
        Ok(())
    })();
    if let Err(error) = work {
        r.last_error = error;
        if let Err(recovery) = rollback(&mut r, data) { r.phase = Phase::NeedsAttention; r.last_error.push_str(&format!("；自动回滚未完成：{recovery}")); let _ = r.save(data); }
        return Err(format!("{}。原副本及目标半成品均已保留；请在迁移记录中检查", r.last_error));
    }
    Ok(r)
}

fn rollback(r: &mut Record, data: &Path) -> Result<()> {
    validate_layout(r, data)?;
    if fs::symlink_metadata(&r.backup).is_ok() {
        check_identity(&r.backup, Some(r.source_id))?;
        if fs::symlink_metadata(&r.source).is_ok() { links::remove(&r.source, &r.target)?; }
        fs::rename(&r.backup, &r.source).map_err(|e| format!("恢复原目录失败：{e}"))?;
    } else { check_identity(&r.source, Some(r.source_id))?; }
    r.phase = Phase::RolledBack; r.save(data)
}

pub fn recover(record: &Record, data: &Path) -> Result<Record> {
    let _lock = operation_lock(data)?;
    let mut r = load_current(record, data)?;
    validate_layout(&r, data)?;
    if r.phase == Phase::Restored && r.held_link.symlink_metadata().is_err() {
        check_identity(&r.source, r.restore_id)?;
        return Ok(r);
    }
    if r.phase == Phase::Restoring || fs::symlink_metadata(&r.held_link).is_ok() {
        if r.held_link.symlink_metadata().is_err() && links::verify(&r.source, &r.target).is_ok() {
            check_identity(&r.target, r.target_id)?;
            r.phase = if r.backup.exists() { Phase::AwaitingReview } else { Phase::Active };
            r.last_error.clear(); r.save(data)?; return Ok(r);
        }
        if fs::symlink_metadata(&r.source).is_ok() && !paths::is_reparse(&fs::symlink_metadata(&r.source).map_err(|e| e.to_string())?) && Some(platform::path_identity(&r.source)?) == r.restore_id {
            r.phase = Phase::Restored;
            if fs::symlink_metadata(&r.held_link).is_ok() { links::remove(&r.held_link, &r.target)?; }
        } else {
            check_identity(&r.target, r.target_id)?;
            if fs::symlink_metadata(&r.source).is_ok() { return Err("原位置出现新内容，拒绝覆盖；保留所有副本，请手工核对".into()); }
            links::verify(&r.held_link, &r.target)?;
            fs::rename(&r.held_link, &r.source).map_err(|e| e.to_string())?;
            r.phase = if r.backup.exists() { Phase::AwaitingReview } else { Phase::Active };
        }
    } else if links::verify(&r.source, &r.target).is_ok() {
        check_identity(&r.target, r.target_id)?;
        r.phase = if r.backup.exists() { Phase::AwaitingReview } else { Phase::Active };
    } else { rollback(&mut r, data)?; }
    r.last_error.clear(); r.save(data)?; Ok(r)
}

fn load_current(record: &Record, data: &Path) -> Result<Record> {
    history(data)?.records.into_iter().find(|r| r.id == record.id).ok_or("找不到有效迁移记录，拒绝操作".into())
}

pub fn restore(record: &Record, data: &Path, cancel: &AtomicBool, progress: &dyn Fn(String)) -> Result<Record> {
    let _lock = operation_lock(data)?;
    let mut r = load_current(record, data)?;
    if ![Phase::AwaitingReview, Phase::Active].contains(&r.phase) { return Err("此记录需要先修复，或已经恢复".into()); }
    validate_layout(&r, data)?; links::verify(&r.source, &r.target)?; check_identity(&r.target, r.target_id)?;
    if r.restore_staging.exists() || r.held_link.symlink_metadata().is_ok() { return Err("上次恢复的临时目录仍存在，请先修复中断记录".into()); }
    let plan = copy::scan(&r.target, cancel)?;
    if paths::check_ntfs(r.source.parent().ok_or("原目录缺少父目录")?)? < plan.bytes.saturating_add(64 * 1024 * 1024) { return Err("原盘空间不足，恢复需要目标盘最新数据的完整副本空间".into()); }
    check_locks(&r.target, &plan, cancel)?;
    r.phase = Phase::Restoring; r.copies_released = false; r.save(data)?;
    let work = (|| {
        fs::rename(&r.source, &r.held_link).map_err(|e| format!("暂存原目录链接失败：{e}"))?;
        copy::create_directory_like(&r.target, &r.restore_staging)?;
        r.restore_id = Some(platform::path_identity(&r.restore_staging)?); r.save(data)?;
        copy::copy(&r.target, &r.restore_staging, &plan, cancel, progress)?;
        copy::verify(&r.target, &r.restore_staging, &plan, cancel, progress)?;
        check_locks(&r.target, &plan, cancel)?;
        copy::cancelled(cancel)?;
        if fs::symlink_metadata(&r.source).is_ok() { return Err("恢复期间原位置出现新内容，拒绝覆盖".into()); }
        fs::rename(&r.restore_staging, &r.source).map_err(|e| e.to_string())?;
        // 先落盘提交状态；意外中断时以 restore_id 识别已恢复的最新数据。
        r.phase = Phase::Restored; r.last_error.clear(); r.save(data)?;
        links::remove(&r.held_link, &r.target)?;
        Ok(())
    })();
    if let Err(error) = work {
        r.last_error = error;
        if fs::symlink_metadata(&r.source).is_err() && links::verify(&r.held_link, &r.target).is_ok() {
            if fs::rename(&r.held_link, &r.source).is_ok() { r.phase = if r.backup.exists() { Phase::AwaitingReview } else { Phase::Active }; }
        } else if Some(platform::path_identity(&r.source).unwrap_or(r.source_id)) != r.restore_id { r.phase = Phase::NeedsAttention; }
        let _ = r.save(data);
        return Err(format!("{}；目标盘上的最新数据仍保留，恢复半成品也已保留，请检查迁移记录", r.last_error));
    }
    Ok(r)
}

pub fn release_copy(record: &Record, data: &Path, cancel: &AtomicBool, progress: &dyn Fn(String)) -> Result<Record> {
    let _lock = operation_lock(data)?;
    let mut r = load_current(record, data)?;
    validate_layout(&r, data)?;
    let mut copies = Vec::new();
    match r.phase {
        Phase::AwaitingReview | Phase::Active => {
            links::verify(&r.source, &r.target)?; check_identity(&r.target, r.target_id)?;
            copies.push((r.backup.clone(), Some(r.source_id)));
            // 若上次恢复失败，只允许释放本工具有身份记录的恢复半成品。
            copies.push((r.restore_staging.clone(), r.restore_id));
        }
        Phase::Restored => {
            check_identity(&r.source, r.restore_id)?;
            if r.held_link.symlink_metadata().is_ok() { links::remove(&r.held_link, &r.target)?; }
            copies.push((r.target.clone(), r.target_id)); copies.push((r.backup.clone(), Some(r.source_id)));
        }
        Phase::RolledBack => {
            check_identity(&r.source, Some(r.source_id))?;
            copies.push((r.staging.clone(), r.staging_id)); copies.push((r.target.clone(), r.target_id));
        }
        _ => return Err("请先修复中断记录，尚未删除任何副本".into()),
    }
    for (path, expected) in &copies { if path.symlink_metadata().is_ok() { check_identity(path, *expected)?; } }
    for (path, _) in copies {
        if path.symlink_metadata().is_ok() {
            progress("正在释放已确认的副本…".into());
            if let Err(error) = copy::remove_tree_no_links(&path, cancel) { r.last_error = error.clone(); r.save(data)?; return Err(error); }
        }
    }
    if r.phase == Phase::AwaitingReview { r.phase = Phase::Active; }
    r.copies_released = true; r.last_error.clear(); r.save(data)?; Ok(r)
}

fn validate_purgeable(r: &Record, data: &Path) -> Result<()> {
    if !r.copies_released || ![Phase::Restored, Phase::RolledBack].contains(&r.phase) {
        return Err("记录仍处于操作中，或还有未确认的副本，不能清理".into());
    }
    validate_layout(r, data)?;
    match r.phase {
        Phase::Restored => check_identity(&r.source, r.restore_id)?,
        Phase::RolledBack => check_identity(&r.source, Some(r.source_id))?,
        _ => unreachable!(),
    }
    for path in [&r.target, &r.backup, &r.staging, &r.restore_staging, &r.held_link] {
        if path.symlink_metadata().is_ok() { return Err("记录仍有保留副本或临时目录".into()); }
    }
    Ok(())
}

pub fn purge_record(record: &Record, data: &Path) -> Result<()> {
    let _lock = operation_lock(data)?;
    let r = load_current(record, data)?;
    validate_purgeable(&r, data)?;
    let path = data.join("migrations").join(format!("{}.json", r.id));
    paths::reject_reparse_ancestors(&path)?;
    fs::remove_file(path).map_err(|e| format!("删除迁移记录失败：{e}"))
}

pub fn purge_completed(data: &Path) -> Result<(usize, usize)> {
    let _lock = operation_lock(data)?;
    let history = history(data)?;
    if history.unreadable != 0 { return Err("存在无法读取的迁移记录，暂不清理；请先检查 data/migrations".into()); }
    let mut removed = 0; let mut skipped = 0;
    for record in history.records {
        if !record.copies_released || ![Phase::Restored, Phase::RolledBack].contains(&record.phase) { continue; }
        if validate_purgeable(&record, data).is_err() { skipped += 1; continue; }
        let path = data.join("migrations").join(format!("{}.json", record.id));
        if paths::reject_reparse_ancestors(&path).is_ok() && fs::remove_file(path).is_ok() { removed += 1; } else { skipped += 1; }
    }
    Ok((removed, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

    fn setup() -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("systools-migration-test-{}", platform::unique_id()));
        let source = root.join("source"); let target = root.join("dest"); let data = root.join("records");
        fs::create_dir_all(source.join("子目录")).unwrap(); fs::create_dir(&target).unwrap(); fs::create_dir(&data).unwrap();
        fs::write(source.join("子目录\\file.txt"), b"original").unwrap();
        fs::write(source.join("子目录\\file.txt:metadata"), b"alternate-stream").unwrap();
        (root, source, target, data)
    }

    #[test]
    fn migration_roundtrip_uses_latest_data_and_keeps_ads() {
        let (root, source, target, data) = setup();
        let cancel = AtomicBool::new(false);
        let r = migrate(&source, &target, &data, &cancel, &|_| {}).unwrap();
        assert_eq!(r.phase, Phase::AwaitingReview);
        assert!(junction::exists(&source).unwrap()); assert!(r.backup.exists());
        let active = release_copy(&r, &data, &cancel, &|_| {}).unwrap();
        assert!(purge_record(&active, &data).is_err());
        fs::write(source.join("子目录\\file.txt"), b"updated after migration").unwrap();
        fs::write(source.join("new.txt"), b"new-data").unwrap();
        let restored = restore(&active, &data, &cancel, &|_| {}).unwrap();
        assert_eq!(restored.phase, Phase::Restored);
        assert!(!paths::is_reparse(&fs::symlink_metadata(&source).unwrap()));
        assert_eq!(fs::read(source.join("子目录\\file.txt")).unwrap(), b"updated after migration");
        assert_eq!(fs::read(source.join("子目录\\file.txt:metadata")).unwrap(), b"alternate-stream");
        assert!(source.join("new.txt").exists()); assert!(restored.target.exists());
        release_copy(&restored, &data, &cancel, &|_| {}).unwrap();
        assert!(!restored.target.exists()); assert!(!restored.backup.exists());
        purge_record(&restored, &data).unwrap();
        assert!(history(&data).unwrap().records.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_copy_can_restore_original_without_touching_other_data() {
        let (root, source, target, data) = setup();
        let cancel = AtomicBool::new(false);
        let mut r = migrate(&source, &target, &data, &cancel, &|_| {}).unwrap();
        links::remove(&r.source, &r.target).unwrap(); r.phase = Phase::Copying; r.save(&data).unwrap();
        let repaired = recover(&r, &data).unwrap();
        assert_eq!(repaired.phase, Phase::RolledBack); assert!(source.join("子目录\\file.txt").exists());
        assert!(repaired.target.exists());
        release_copy(&repaired, &data, &cancel, &|_| {}).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancel_and_nested_links_preserve_source() {
        let (root, source, target, data) = setup();
        assert!(migrate(&source, &target, &data, &AtomicBool::new(true), &|_| {}).is_err());
        junction::create(&target, source.join("nested-link")).unwrap();
        assert!(migrate(&source, &target, &data, &AtomicBool::new(false), &|_| {}).is_err());
        assert!(source.join("子目录\\file.txt").exists());
        fs::remove_dir(source.join("nested-link")).unwrap(); fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn locked_file_is_not_migrated_or_deleted() {
        let (root, source, target, data) = setup();
        let mut held = OpenOptions::new().write(true).share_mode(FILE_SHARE_READ).open(source.join("子目录\\file.txt")).unwrap();
        held.write_all(b"locked").unwrap();
        assert!(migrate(&source, &target, &data, &AtomicBool::new(false), &|_| {}).is_err());
        assert!(source.join("子目录\\file.txt").exists());
        drop(held); fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_link_target_blocks_backup_release() {
        let (root, source, target, data) = setup();
        let r = migrate(&source, &target, &data, &AtomicBool::new(false), &|_| {}).unwrap();
        fs::remove_dir(&source).unwrap();
        let other = root.join("other"); fs::create_dir(&other).unwrap();
        junction::create(&other, &source).unwrap();
        assert!(release_copy(&r, &data, &AtomicBool::new(false), &|_| {}).is_err()); assert!(r.backup.exists());
        fs::remove_dir(&source).unwrap(); fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cross_volume_migration_allows_normal_user_acl_when_d_drive_exists() {
        let d = PathBuf::from("D:\\");
        if !d.is_dir() { return; }
        let root = std::env::temp_dir().join(format!("systools-cross-source-{}", platform::unique_id()));
        let destination_parent = d.join(format!("systools-cross-destination-{}", platform::unique_id()));
        let data = root.join("records"); let source = root.join("source");
        fs::create_dir_all(source.join("nested")).unwrap(); fs::create_dir_all(&data).unwrap(); fs::write(source.join("nested\\file.txt"), b"cross-volume").unwrap();
        let cancel = AtomicBool::new(false);
        fs::create_dir_all(&destination_parent).unwrap();
        let record = migrate(&source, &destination_parent, &data, &cancel, &|_| {}).unwrap();
        assert_eq!(record.phase, Phase::AwaitingReview); assert!(junction::exists(&source).unwrap()); assert!(record.target.join("nested\\file.txt").exists());
        release_copy(&record, &data, &cancel, &|_| {}).unwrap();
        fs::remove_dir_all(&root).unwrap(); if destination_parent.exists() { fs::remove_dir_all(destination_parent).unwrap(); }
    }
}
