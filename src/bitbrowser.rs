//! BitBrowser profile discovery and conservative orphan cleanup.
//!
//! BitBrowser stores one Chromium profile per 32-character window id.  The
//! local API is the source of truth for currently configured windows; directory
//! names alone are not sufficient because deleted windows can leave data behind.

use crate::{paths, platform::{self, FileId}, Result};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::str;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

const DEFAULT_PORT: u16 = 54345;
const MAX_RESPONSE: usize = 8 * 1024 * 1024;
const MAX_ENTRIES_PER_PROFILE: usize = 1_000_000;

#[derive(Clone, Debug)]
pub struct Candidate {
    pub id: String,
    pub path: PathBuf,
    pub size: u64,
    pub files: usize,
    identity: FileId,
}

#[derive(Clone, Debug)]
pub struct Scan {
    pub root: PathBuf,
    pub current_windows: usize,
    pub current_ids: HashSet<String>,
    pub profiles: Vec<Candidate>,
    pub orphaned: usize,
    pub orphan_bytes: u64,
    pub limited: bool,
    pub cancelled: bool,
}

#[derive(Default, Debug)]
pub struct Outcome {
    pub deleted: usize,
    pub bytes: u64,
    pub skipped: usize,
    pub cancelled: bool,
}

pub fn default_root() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("BitBrowser")
        .join("BrowserCache")
}

fn is_window_id(name: &str) -> bool {
    name.len() == 32 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

fn read_port() -> u16 {
    let path = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default().join("BitBrowser").join("config.json");
    let Ok(bytes) = fs::read(path) else { return DEFAULT_PORT; };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else { return DEFAULT_PORT; };
    value.get("localServerAddress")
        .and_then(Value::as_str)
        .and_then(|address| address.rsplit(':').next())
        .and_then(|port| port.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT)
}

fn read_chunked<R: BufRead>(reader: &mut R) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).map_err(|e| format!("读取比特浏览器接口响应失败：{e}"))?;
        let line = str::from_utf8(&line).map_err(|_| "接口响应分块长度无效")?.trim();
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| "接口响应分块长度无效")?;
        if size == 0 { break; }
        if out.len().saturating_add(size) > MAX_RESPONSE { return Err("比特浏览器接口响应过大".into()); }
        let start = out.len();
        out.resize(start + size, 0);
        reader.read_exact(&mut out[start..]).map_err(|e| format!("读取比特浏览器接口响应失败：{e}"))?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf).map_err(|e| format!("读取比特浏览器接口响应失败：{e}"))?;
        if crlf != *b"\r\n" { return Err("比特浏览器接口返回了无效的分块响应".into()); }
    }
    Ok(out)
}

fn request_once(page: u32) -> Result<Value> {
    let address = format!("127.0.0.1:{}", read_port());
    let socket = address.to_socket_addrs().map_err(|e| format!("无法连接比特浏览器本地接口：{e}"))?.next().ok_or("无法解析比特浏览器本地接口地址")?;
    let mut stream = TcpStream::connect_timeout(&socket, Duration::from_secs(2)).map_err(|e| format!("无法连接比特浏览器，请先启动并登录客户端：{e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
    // concise avoids transferring passwords, cookies and other per-window data;
    // BitBrowser caps this endpoint at 100 rows per page.
    let body = format!(r#"{{"page":{},"pageSize":100}}"#, page);
    let request = format!("POST /browser/list/concise HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
    stream.write_all(request.as_bytes()).map_err(|e| format!("请求比特浏览器接口失败：{e}"))?;
    let mut reader = BufReader::new(stream);
    let mut header_bytes = Vec::new();
    loop {
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).map_err(|e| format!("读取比特浏览器接口失败：{e}"))?;
        if line.is_empty() { return Err("比特浏览器接口返回了不完整响应".into()); }
        if header_bytes.len().saturating_add(line.len()) > 64 * 1024 { return Err("比特浏览器接口响应头过大".into()); }
        header_bytes.extend_from_slice(&line);
        if header_bytes.ends_with(b"\r\n\r\n") { break; }
    }
    let header = String::from_utf8_lossy(&header_bytes).to_ascii_lowercase();
    if !header.starts_with("http/1.1 200") && !header.starts_with("http/1.0 200") { return Err(format!("比特浏览器接口返回异常状态：{}", header.lines().next().unwrap_or("未知"))); }
    let body = if header.contains("transfer-encoding: chunked") {
        read_chunked(&mut reader)?
    } else if let Some(length) = header.lines().find_map(|line| line.strip_prefix("content-length:").and_then(|value| value.trim().parse::<usize>().ok())) {
        if length > MAX_RESPONSE { return Err("比特浏览器接口响应过大".into()); }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).map_err(|e| format!("读取比特浏览器接口响应失败：{e}"))?;
        body
    } else {
        // HTTP/1.0 servers may legitimately delimit the body by closing the
        // connection. Modern BitBrowser responses include Content-Length;
        // retain this fallback for older compatible builds.
        let mut body = Vec::new();
        reader.read_to_end(&mut body).map_err(|e| format!("读取比特浏览器接口失败：{e}"))?;
        if body.len() > MAX_RESPONSE { return Err("比特浏览器接口响应过大".into()); }
        body
    };
    serde_json::from_slice(&body).map_err(|e| format!("解析比特浏览器窗口列表失败：{e}"))
}

fn request(page: u32) -> Result<Value> {
    let mut last_error = None;
    // The local API rate-limits bursts. A short bounded retry keeps the
    // parallel scan fast without turning a transient 429 into a failure.
    for delay in [0, 150, 350, 700, 1200] {
        if delay != 0 { thread::sleep(Duration::from_millis(delay)); }
        match request_once(page) {
            Ok(value) => return Ok(value),
            Err(error) if error.contains("429") => last_error = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| "比特浏览器接口请求失败".into()))
}

fn collect_rows(value: &Value, ids: &mut HashSet<String>) {
    match value {
        Value::Array(rows) => {
            for row in rows {
                if let Value::Object(map) = row {
                    for key in ["id", "browserId"] {
                        if let Some(Value::String(s)) = map.get(key) {
                            if is_window_id(s) { ids.insert(s.to_ascii_lowercase()); break; }
                        }
                    }
                }
            }
        }
        Value::Object(map) => {
            if let Some(data) = map.get("data") { collect_rows(data, ids); }
            else if let Some(list) = map.get("list") { collect_rows(list, ids); }
            else if let Some(rows) = map.get("rows") { collect_rows(rows, ids); }
        }
        _ => {}
    }
}

fn is_window_list_response(value: &Value) -> bool {
    match value {
        Value::Array(_) => true,
        Value::Object(map) => {
            if map.get("success").and_then(Value::as_bool) == Some(false) { return false; }
            if map.get("success").and_then(Value::as_bool) == Some(true) {
                return match map.get("data") {
                    Some(Value::Array(_)) => true,
                    Some(Value::Object(data)) => data.contains_key("list") || data.contains_key("rows"),
                    _ => map.contains_key("list") || map.contains_key("rows"),
                };
            }
            map.contains_key("data") || map.contains_key("list") || map.contains_key("rows")
        }
        _ => false,
    }
}

fn page_info(value: &Value) -> (usize, usize, usize) {
    let data = value.get("data").unwrap_or(value);
    let object = data.as_object().or_else(|| value.as_object());
    let total = object.and_then(|map| map.get("totalNum").or_else(|| map.get("total"))).and_then(Value::as_u64).unwrap_or(0) as usize;
    let page_size = object.and_then(|map| map.get("pageSize")).and_then(Value::as_u64).unwrap_or(100) as usize;
    let rows = match data {
        Value::Array(rows) => rows.len(),
        Value::Object(map) => map.get("list").or_else(|| map.get("rows")).and_then(Value::as_array).map(Vec::len).unwrap_or(0),
        _ => 0,
    };
    (total, page_size.max(1), rows)
}

fn current_ids(cancel: &AtomicBool) -> Result<HashSet<String>> {
    if cancel.load(Ordering::Relaxed) { return Err("操作已取消".into()); }
    let first = request(0)?;
    if !is_window_list_response(&first) { return Err("比特浏览器接口没有返回有效窗口列表".into()); }
    let (total, page_size, first_rows) = page_info(&first);
    let mut ids = HashSet::new();
    collect_rows(&first, &mut ids);
    if first_rows < page_size { return Ok(ids); }

    // The local API is limited to 100 rows per page. Fetch a few pages at a
    // time, which stays below its normal local concurrency limit. Most
    // versions include `totalNum`; older builds may omit it, so in that case
    // stop after the first short page and keep a hard safety bound.
    let known_page_count = (total > 0).then(|| (total + page_size - 1) / page_size);
    let mut next_page = 1usize;
    let mut fetched_pages = 1usize;
    while known_page_count.map_or(true, |count| next_page < count) {
        if cancel.load(Ordering::Relaxed) { return Err("操作已取消".into()); }
        if fetched_pages >= 10_000 { return Err("比特浏览器窗口列表分页数量异常，已停止读取".into()); }
        let end_page = known_page_count.map_or(next_page + 4, |count| (next_page + 4).min(count));
        let handles = (next_page..end_page).map(|page| thread::spawn(move || request(page as u32))).collect::<Vec<_>>();
        for handle in handles {
            let value = handle.join().map_err(|_| "读取比特浏览器窗口列表的后台任务异常".to_string())??;
            if !is_window_list_response(&value) { return Err("比特浏览器接口返回了无效窗口列表".into()); }
            let (_, _, rows) = page_info(&value);
            collect_rows(&value, &mut ids);
            fetched_pages += 1;
            if known_page_count.is_none() && rows < page_size { return Ok(ids); }
        }
        next_page = end_page;
    }
    Ok(ids)
}

fn measure_tree(root: &Path, cancel: &AtomicBool) -> (u64, usize, bool) {
    let mut bytes: u64 = 0; let mut files: usize = 0; let mut limited = false;
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        if cancel.load(Ordering::Relaxed) { break; }
        let Ok(entries) = fs::read_dir(&directory) else { continue; };
        for entry in entries.flatten() {
            if cancel.load(Ordering::Relaxed) { break; }
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else { continue; };
            if paths::is_reparse(&meta) { continue; }
            if meta.is_dir() { stack.push(path); }
            else if meta.is_file() { bytes = bytes.saturating_add(meta.len()); files += 1; }
            if files >= MAX_ENTRIES_PER_PROFILE { limited = true; break; }
        }
        if limited { break; }
    }
    (bytes, files, limited)
}

pub fn scan(root: &Path, cancel: &AtomicBool) -> Result<Scan> {
    let root = paths::existing_directory(root)?;
    let ids = current_ids(cancel)?;
    let mut profiles = Vec::new();
    let mut limited = false;
    for entry in fs::read_dir(&root).map_err(|e| format!("读取比特浏览器缓存目录失败：{e}"))? {
        if cancel.load(Ordering::Relaxed) { break; }
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_window_id(&name) { continue; }
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).map_err(|e| format!("读取窗口目录失败：{e}"))?;
        if !meta.is_dir() || paths::is_reparse(&meta) { continue; }
        let identity = platform::path_identity(&path)?;
        // Existing windows are protected and never selectable. Avoid walking
        // their entire Chromium trees; only orphan candidates need a size
        // estimate for the confirmation dialog.
        let (size, files, capped) = if ids.contains(&name.to_ascii_lowercase()) { (0, 0, false) } else { measure_tree(&path, cancel) };
        limited |= capped;
        profiles.push(Candidate { id: name.to_ascii_lowercase(), path, size, files, identity });
    }
    profiles.sort_by(|a, b| a.id.cmp(&b.id));
    let orphan_bytes = profiles.iter().filter(|p| !ids.contains(&p.id)).map(|p| p.size).sum();
    let orphaned = profiles.iter().filter(|p| !ids.contains(&p.id)).count();
    Ok(Scan { root, current_windows: ids.len(), current_ids: ids, profiles, orphaned, orphan_bytes, limited, cancelled: cancel.load(Ordering::Relaxed) })
}

fn remove_tree(path: &Path, cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) { return Err("操作已取消".into()); }
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if paths::is_reparse(&meta) { return Err("窗口目录包含重解析点，已跳过".into()); }
    if meta.is_dir() {
        for entry in fs::read_dir(path).map_err(|e| e.to_string())? { remove_tree(&entry.map_err(|e| e.to_string())?.path(), cancel)?; }
        fs::remove_dir(path).map_err(|e| e.to_string())
    } else {
        fs::remove_file(path).map_err(|e| e.to_string())
    }
}

fn clean_with_ids(scan: &Scan, selected: &[Candidate], current: &HashSet<String>, cancel: &AtomicBool, progress: &dyn Fn(String)) -> Result<Outcome> {
    if selected.is_empty() { return Err("没有选择要清理的残留窗口目录".into()); }
    let mut out = Outcome::default();
    for (index, candidate) in selected.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) { out.cancelled = true; break; }
        if current.contains(&candidate.id) { out.skipped += 1; continue; }
        if candidate.path.parent().map(paths::key) != Some(paths::key(&scan.root)) || candidate.path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase() != candidate.id).unwrap_or(true) { out.skipped += 1; continue; }
        if platform::path_identity(&candidate.path).ok() != Some(candidate.identity) { out.skipped += 1; continue; }
        let quarantine = scan.root.join(format!(".systools-bitbrowser-delete-{}-{}", candidate.id, platform::unique_id()));
        if fs::rename(&candidate.path, &quarantine).is_err() { out.skipped += 1; }
        else if remove_tree(&quarantine, cancel).is_ok() { out.deleted += 1; out.bytes = out.bytes.saturating_add(candidate.size); }
        else {
            // A cancellation, lock, or permission error must not strand a
            // profile under the private quarantine name. Restore it when the
            // original path is still free; if another process recreated that
            // path, leave the quarantine untouched rather than overwriting it.
            if fs::symlink_metadata(&candidate.path).is_err() { let _ = fs::rename(&quarantine, &candidate.path); }
            out.skipped += 1;
        }
        progress(format!("正在清理比特浏览器残留：{} / {}", index + 1, selected.len()));
    }
    Ok(out)
}

pub fn clean(scan: &Scan, selected: &[Candidate], cancel: &AtomicBool, progress: &dyn Fn(String)) -> Result<Outcome> {
    let current = current_ids(cancel)?;
    clean_with_ids(scan, selected, &current, cancel, progress)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    #[test]
    fn only_32_hex_names_are_window_ids() {
        assert!(is_window_id("0123456789abcdef0123456789abcdef"));
        assert!(!is_window_id("0123456789abcdef"));
        assert!(!is_window_id("0123456789abcdef0123456789abcdeg"));
    }

    #[test]
    fn collect_rows_only_reads_window_row_ids() {
        let id = "0123456789abcdef0123456789abcdef";
        let value: Value = serde_json::json!({"data":{"list":[{"id":id,"randomKey":"fedcba9876543210fedcba9876543210"}]}});
        let mut ids = HashSet::new(); collect_rows(&value, &mut ids);
        assert!(ids.contains(id));
        assert!(!ids.contains("fedcba9876543210fedcba9876543210"));
    }

    #[test]
    fn valid_empty_window_list_is_safe_to_use() {
        assert!(is_window_list_response(&serde_json::json!({"success":true,"data":{"list":[]}})));
        assert!(is_window_list_response(&serde_json::json!({"success":true,"data":[]})));
        assert!(!is_window_list_response(&serde_json::json!({"success":true,"data":{}})));
        assert!(!is_window_list_response(&serde_json::json!({"success":false,"message":"offline"})));
    }

    #[test]
    fn response_body_reading_handles_keep_alive_framing() {
        let mut reader = BufReader::new(Cursor::new(b"6\r\n{\"ok\"}\r\n0\r\n\r\n".to_vec()));
        assert_eq!(read_chunked(&mut reader).unwrap(), br#"{"ok"}"#);
        assert_eq!(page_info(&serde_json::json!([{"id":"0123456789abcdef0123456789abcdef"}])).2, 1);
    }

    #[test]
    fn cleanup_rechecks_current_ids_and_directory_identity() {
        let root = std::env::temp_dir().join(format!("systools-bitbrowser-test-{}", platform::unique_id()));
        fs::create_dir_all(&root).unwrap();
        let current_id = "0123456789abcdef0123456789abcdef".to_string();
        let orphan_id = "fedcba9876543210fedcba9876543210".to_string();
        let current_path = root.join(&current_id); let orphan_path = root.join(&orphan_id);
        fs::create_dir_all(&current_path).unwrap(); fs::create_dir_all(&orphan_path).unwrap();
        fs::write(current_path.join("keep"), b"keep").unwrap(); fs::write(orphan_path.join("remove"), b"remove").unwrap();
        let current_candidate = Candidate { id: current_id.clone(), path: current_path.clone(), size: 4, files: 1, identity: platform::path_identity(&current_path).unwrap() };
        let orphan_candidate = Candidate { id: orphan_id.clone(), path: orphan_path.clone(), size: 6, files: 1, identity: platform::path_identity(&orphan_path).unwrap() };
        let mut current_ids = HashSet::new(); current_ids.insert(current_id);
        let scan = Scan { root: root.clone(), current_windows: 1, current_ids: current_ids.clone(), profiles: vec![current_candidate.clone(), orphan_candidate.clone()], orphaned: 1, orphan_bytes: 6, limited: false, cancelled: false };
        let out = clean_with_ids(&scan, &[current_candidate, orphan_candidate], &current_ids, &AtomicBool::new(false), &|_| {}).unwrap();
        assert_eq!(out.deleted, 1); assert_eq!(out.skipped, 1); assert!(current_path.exists()); assert!(!orphan_path.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
