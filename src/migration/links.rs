// Adapted from Viap migration/links.rs (MIT), Copyright (c) 2026 Evan Lau.
// Changes: junction-only admission, file-identity checks live in the migration journal.
use crate::{paths, platform, Result};
use std::fs;
use std::path::Path;

pub fn create(target: &Path, link: &Path) -> Result<()> {
    if fs::symlink_metadata(link).is_ok() { return Err("原位置已存在内容，拒绝覆盖".into()); }
    junction::create(target, link).map_err(|e| format!("创建 NTFS 目录链接失败，请检查权限：{e}"))?;
    verify(link, target)
}

pub fn verify(link: &Path, target: &Path) -> Result<()> {
    if !junction::exists(link).map_err(|e| e.to_string())? { return Err("原位置未成为 NTFS 目录链接".into()); }
    let actual = fs::canonicalize(junction::get_target(link).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let expected = fs::canonicalize(target).map_err(|e| e.to_string())?;
    if paths::key(&actual) != paths::key(&expected) { return Err("目录链接指向异常，保留所有副本".into()); }
    Ok(())
}

pub fn remove(link: &Path, target: &Path) -> Result<()> {
    verify(link, target)?;
    // 只移除链接本身，绝不对链接执行递归删除。
    fs::remove_dir(link).map_err(|e| format!("移除目录链接失败：{e}"))
}

pub fn preflight(target: &Path, source: &Path) -> Result<()> {
    let probe = source.parent().ok_or("目录没有父目录")?.join(format!(".systools-probe-{}", platform::unique_id()));
    create(target, &probe)?;
    remove(&probe, target)
}
