use crate::{apps::{self, App}, paths, Result};
use std::fs;
use std::path::{Path, PathBuf};
use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, FOLDERID_LocalAppDataLow, FOLDERID_ProgramData, FOLDERID_RoamingAppData};

#[derive(Clone, Debug)]
pub struct Residue { pub path: PathBuf, pub category: String, pub size: u64, pub confidence: String }

fn normalized(value: &str) -> String { value.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase() }
fn size(path: &Path, total: &mut u64, count: &mut usize) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else { return 0; };
    if paths::is_reparse(&meta) || *count > 100_000 { return 0; }
    if meta.is_file() { *count += 1; *total = total.saturating_add(meta.len()); return meta.len(); }
    if !meta.is_dir() { return 0; }
    fs::read_dir(path).ok().into_iter().flat_map(|entries| entries.filter_map(|e| e.ok())).map(|e| size(&e.path(), total, count)).sum()
}
fn candidate_matches(path: &Path, tokens: &[String]) -> bool {
    let name = normalized(&path.file_name().unwrap_or_default().to_string_lossy());
    !name.is_empty() && tokens.iter().any(|token| token.len() >= 3 && (name == *token || name.contains(token) || token.contains(&name)))
}

pub fn scan(app: &App) -> Vec<Residue> {
    let tokens = apps::product_aliases(app);
    let mut roots = Vec::new();
    if app.location.is_dir() && app.location.parent().is_some() {
        let leaf = normalized(&app.location.file_name().unwrap_or_default().to_string_lossy());
        let generic = ["programfiles", "programfilesx86", "commonfiles", "microsoft", "shared", "public", "windows", "system32", "bin", "appdata", "programdata"];
        if !generic.contains(&leaf.as_str()) { roots.push((app.location.clone(), "安装目录残留".to_string(), "高".to_string())); }
    }
    for (id, category) in [(FOLDERID_LocalAppData, "用户数据 / 缓存"), (FOLDERID_RoamingAppData, "用户数据 / 缓存"), (FOLDERID_LocalAppDataLow, "用户数据 / 缓存"), (FOLDERID_ProgramData, "软件共享数据")] {
        if let Ok(root) = paths::known_folder(&id) { if let Ok(entries) = fs::read_dir(root) { for entry in entries.flatten() { let path = entry.path(); if candidate_matches(&path, &tokens) { roots.push((path, category.to_string(), "中".to_string())); } } } }
    }
    let mut seen = std::collections::HashSet::new(); let mut out = Vec::new();
    for (path, category, confidence) in roots {
        let Ok(meta) = fs::symlink_metadata(&path) else { continue; };
        if paths::is_reparse(&meta) || !path.is_absolute() || path.parent().is_none() || !seen.insert(paths::key(&path)) { continue; }
        let mut total = 0; let mut count = 0; let measured = size(&path, &mut total, &mut count);
        if measured == 0 && meta.is_dir() { continue; }
        out.push(Residue { path, category, size: measured, confidence });
    }
    out.sort_by(|a, b| b.size.cmp(&a.size)); out.truncate(100); out
}

fn remove_tree(path: &Path, count: &mut usize) -> Result<()> {
    if *count > 100_000 { return Err("残留项目超过清理上限，已停止".into()); }
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if paths::is_reparse(&meta) { return Err("残留路径含链接，已跳过以保护其他数据".into()); }
    if meta.is_file() { fs::remove_file(path).map_err(|e| e.to_string())?; *count += 1; return Ok(()); }
    for entry in fs::read_dir(path).map_err(|e| e.to_string())? { remove_tree(&entry.map_err(|e| e.to_string())?.path(), count)?; }
    fs::remove_dir(path).map_err(|e| e.to_string())?; Ok(())
}

pub fn clean(items: &[Residue]) -> (usize, Vec<String>) {
    let mut removed = 0; let mut errors = Vec::new();
    for item in items {
        if !item.path.is_absolute() || item.path.parent().is_none() { errors.push(format!("{}：路径不安全", item.path.display())); continue; }
        let mut count = 0;
        match remove_tree(&item.path, &mut count) { Ok(()) => removed += 1, Err(e) => errors.push(format!("{}：{}", item.path.display(), e)) }
    }
    (removed, errors)
}

pub fn format_summary(items: &[Residue]) -> String {
    let mut text = String::new();
    for item in items.iter().take(12) { text.push_str(&format!("[{}][{}] {} · {}\r\n", item.confidence, item.category, item.path.display(), crate::format_size(item.size))); }
    if items.len() > 12 { text.push_str(&format!("……还有 {} 项，完整列表已保留。\r\n", items.len() - 12)); }
    text
}
