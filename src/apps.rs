use crate::{paths, platform::{self, wide, Handle}, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const UNINSTALL_KEY: &str = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct App {
    pub id: String,
    pub name: String,
    pub publisher: String,
    pub version: String,
    pub location: PathBuf,
    pub command: String,
    pub category: String,
    pub protection: String,
    pub kind: String,
    pub machine: bool,
    pub view: u32,
    pub key: String,
}

impl App {
    pub fn can_uninstall(&self) -> bool { self.protection.is_empty() && !self.command.is_empty() }
    pub fn can_migrate(&self) -> bool {
        let name = self.name.to_lowercase();
        self.protection.is_empty() && self.location.is_absolute() && self.location.is_dir()
            && !name.contains("chrome") && !name.contains("edge") && !name.contains("bitbrowser") && !name.contains("比特")
    }
}

fn infer_path_from_command(command: &str) -> Option<PathBuf> {
    let text = expand(command).ok()?;
    let trimmed = text.trim();
    let value = if let Some(rest) = trimmed.strip_prefix('"') { rest.split_once('"')?.0.to_string() } else {
        let end = trimmed.to_ascii_lowercase().match_indices(".exe").map(|(i, _)| i + 4).find(|&i| Path::new(&trimmed[..i]).is_file())?;
        trimmed[..end].to_string()
    };
    let path = PathBuf::from(value);
    if path.is_file() { Some(path) } else { None }
}

fn infer_install_location(command: &str, display_icon: &str, display_name: &str) -> PathBuf {
    let mut candidates = Vec::new();
    if let Some(path) = infer_path_from_command(command) { candidates.push(path); }
    if let Some(path) = infer_path_from_command(display_icon.trim_end_matches(|c: char| c.is_ascii_digit() || c == ',' || c == '-' || c.is_whitespace())) { candidates.push(path); }
    let name = display_name.to_lowercase();
    for path in candidates {
        let mut directory = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let leaf = directory.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
        if ["uninstall", "uninstaller", "uninst"].contains(&leaf.as_str()) { directory = directory.parent().map(Path::to_path_buf).unwrap_or(directory); }
        if directory.is_dir() && (name.is_empty() || !directory.to_string_lossy().to_lowercase().contains("system32")) { return directory; }
    }
    PathBuf::new()
}

struct RegKey(HKEY);
impl Drop for RegKey { fn drop(&mut self) { unsafe { RegCloseKey(self.0); } } }

fn open(root: HKEY, name: &str, view: u32) -> Option<RegKey> {
    let mut out = null_mut();
    (unsafe { RegOpenKeyExW(root, wide(name).as_ptr(), 0, KEY_READ | view, &mut out) } == 0).then_some(RegKey(out))
}

fn string(key: HKEY, name: &str) -> String {
    let mut bytes = 0;
    let mut typ = 0;
    let name = wide(name);
    if unsafe { RegQueryValueExW(key, name.as_ptr(), null(), &mut typ, null_mut(), &mut bytes) } != 0 || bytes > 65536 || bytes < 2 || (typ != REG_SZ && typ != REG_EXPAND_SZ) { return String::new(); }
    let mut value = vec![0u16; (bytes as usize + 1) / 2 + 1];
    if unsafe { RegQueryValueExW(key, name.as_ptr(), null(), &mut typ, value.as_mut_ptr() as _, &mut bytes) } != 0 { return String::new(); }
    let end = value.iter().position(|x| *x == 0).unwrap_or(value.len());
    let text = String::from_utf16_lossy(&value[..end]);
    if typ == REG_EXPAND_SZ { expand(&text).unwrap_or(text) } else { text }
}

fn dword(key: HKEY, name: &str) -> u32 {
    let mut out: u32 = 0; let mut bytes = 4; let mut typ = 0;
    if unsafe { RegQueryValueExW(key, wide(name).as_ptr(), null(), &mut typ, &mut out as *mut _ as _, &mut bytes) } == 0 && typ == REG_DWORD && bytes == 4 { out } else { 0 }
}

pub fn expand(value: &str) -> Result<String> {
    let w = wide(value);
    let len = unsafe { ExpandEnvironmentStringsW(w.as_ptr(), null_mut(), 0) };
    if len == 0 || len > 32768 { return Err("卸载命令中的环境变量无法解析".into()); }
    let mut out = vec![0u16; len as usize];
    if unsafe { ExpandEnvironmentStringsW(w.as_ptr(), out.as_mut_ptr(), len) } != len { return Err("环境变量解析期间发生变化，请重试".into()); }
    Ok(String::from_utf16_lossy(&out[..out.len() - 1]))
}

pub fn scan() -> Result<Vec<App>> {
    let mut apps = Vec::new(); let mut seen = HashSet::new();
    for machine in [true, false] {
        for view in [KEY_WOW64_64KEY, KEY_WOW64_32KEY] {
            let root = if machine { HKEY_LOCAL_MACHINE } else { HKEY_CURRENT_USER };
            let Some(base) = open(root, UNINSTALL_KEY, view) else { continue; };
            for index in 0..20000 {
                let mut name = [0u16; 512]; let mut len = name.len() as u32;
                let code = unsafe { RegEnumKeyExW(base.0, index, name.as_mut_ptr(), &mut len, null(), null_mut(), null_mut(), null_mut()) };
                if code == ERROR_NO_MORE_ITEMS { break; }
                if code != 0 { continue; }
                let sub = String::from_utf16_lossy(&name[..len as usize]);
                let Some(key) = open(base.0, &sub, 0) else { continue; };
                let display = string(key.0, "DisplayName").trim().to_string();
                if display.is_empty() { continue; }
                let publisher = string(key.0, "Publisher");
                let version = string(key.0, "DisplayVersion");
                let mut command = string(key.0, "UninstallString");
                let display_icon = string(key.0, "DisplayIcon");
                // BCU 的 MSI 缺失命令补全策略按 Rust 重写；不猜测静默卸载参数。
                if command.is_empty() && dword(key.0, "WindowsInstaller") == 1 && valid_product_code(&sub) {
                    command = format!("msiexec.exe /x {sub}");
                }
                let (category, protection) = classify(&display, &publisher, dword(key.0, "SystemComponent") != 0, dword(key.0, "NoRemove") != 0, !string(key.0, "ParentKeyName").is_empty(), &string(key.0, "ReleaseType"));
                let mut location = PathBuf::from(string(key.0, "InstallLocation").trim_matches('"'));
                if !location.is_dir() { location = infer_install_location(&command, &display_icon, &display); }
                let dedup = format!("{}|{}|{}|{}", machine, display.to_lowercase(), version, command);
                if !seen.insert(dedup) { continue; }
                let kind = installer_kind(&command).to_string();
                apps.push(App { id: format!("{machine}-{view}-{sub}"), name: display, publisher, version, location, command, category, protection, kind, machine, view, key: sub });
            }
        }
    }
    apps.sort_by_cached_key(|a| a.name.to_lowercase());
    Ok(apps)
}

fn classify(name: &str, _publisher: &str, system: bool, no_remove: bool, parent: bool, release: &str) -> (String, String) {
    if system { return ("系统组件".into(), "安装登记标记为 SystemComponent，第一版禁止卸载和迁移".into()); }
    if no_remove { return ("受保护组件".into(), "安装登记禁止移除（NoRemove）".into()); }
    if parent || ["update", "hotfix", "security update"].contains(&release.to_lowercase().as_str()) { return ("更新组件".into(), "更新或从属安装项，应通过系统或所属软件管理".into()); }
    let n = name.to_lowercase();
    if ["microsoft visual c++", ".net runtime", ".net framework", "windows desktop runtime", "windows sdk", "webview2", "microsoft edge", "redistributable", "driver", "驱动"].iter().any(|p| n.contains(p)) {
        return ("共享组件 / 驱动".into(), "名称命中共享组件或驱动规则，默认保护；名称规则不代表完整系统文件鉴定".into());
    }
    ("普通软件".into(), String::new())
}

pub fn installer_kind(command: &str) -> &'static str {
    let c = command.to_lowercase();
    if c.contains("msiexec") { "Windows Installer" }
    else if c.contains("unins0") { "Inno Setup" }
    else if c.contains("installshield") { "InstallShield" }
    else { "原厂卸载程序" }
}

pub fn valid_product_code(s: &str) -> bool {
    s.is_ascii() && s.len() == 38 && s.starts_with('{') && s.ends_with('}') && s[1..37].bytes().enumerate().all(|(i, b)| if [8, 13, 18, 23].contains(&i) { b == b'-' } else { b.is_ascii_hexdigit() })
}

pub fn split_command(command: &str) -> Result<(PathBuf, String)> {
    let expanded = expand(command)?;
    let text = expanded.trim();
    if text.is_empty() { return Err("软件未登记卸载命令".into()); }
    let (program, arguments) = if let Some(tail) = text.strip_prefix('"') {
        let end = tail.find('"').ok_or("卸载命令引号不完整，拒绝猜测执行")?;
        (tail[..end].to_string(), tail[end + 1..].trim().to_string())
    } else {
        // 优先找到完整 EXE 路径，避免把未加引号的 Program Files 路径截成 C:\Program。
        let lower = text.to_ascii_lowercase();
        let end = lower.match_indices(".exe").map(|(i, _)| i + 4).find(|&i| (i == text.len() || text[i..].starts_with(char::is_whitespace)) && Path::new(&text[..i]).is_file());
        if let Some(end) = end { (text[..end].to_string(), text[end..].trim().to_string()) }
        else { let end = text.find(char::is_whitespace).unwrap_or(text.len()); (text[..end].to_string(), text[end..].trim().to_string()) }
    };
    let mut path = PathBuf::from(&program);
    if !path.is_absolute() {
        if program.contains(['\\', '/']) { return Err("卸载程序使用不明确的相对路径，拒绝执行".into()); }
        // 仅到系统目录查找裸文件名，避免当前目录搜索劫持。
        path = paths::windows_dir()?.join("System32").join(&program);
    }
    if !path.is_file() || path.extension().is_none_or(|e| !e.eq_ignore_ascii_case("exe")) { return Err("找不到有效的 EXE 卸载程序，保留软件及其安装记录".into()); }
    let args = if path.file_name().is_some_and(|n| n.eq_ignore_ascii_case("msiexec.exe")) { normalize_msi_args(&arguments) } else { arguments };
    Ok((path, args))
}

fn normalize_msi_args(args: &str) -> String {
    let trimmed = args.trim();
    if trimmed.as_bytes().get(..2).is_some_and(|s| s.eq_ignore_ascii_case(b"/i")) {
        let product = trimmed[2..].trim_start();
        if product.starts_with('{') && product.as_bytes().get(..38).is_some_and(|s| s.is_ascii()) && valid_product_code(&product[..38]) && (product.len() == 38 || product[38..].starts_with(char::is_whitespace)) {
            return format!("/x {product}");
        }
    }
    trimmed.to_string()
}

pub struct UninstallOutcome { pub message: String, pub completed: bool }

pub fn is_registered(app: &App) -> bool {
    let root = if app.machine { HKEY_LOCAL_MACHINE } else { HKEY_CURRENT_USER };
    open(root, &format!("{UNINSTALL_KEY}\\{}", app.key), app.view).is_some()
}

pub fn uninstall(app: &App, cancel: &AtomicBool) -> Result<UninstallOutcome> {
    let _com = platform::ComGuard::new();
    if !app.can_uninstall() { return Err("此项目受保护或未登记卸载程序".into()); }
    let (program, args) = split_command(&app.command)?;
    let executable = wide(&program); let parameters = wide(&args); let verb = wide(if app.machine { "runas" } else { "open" });
    let mut info = unsafe { std::mem::zeroed::<SHELLEXECUTEINFOW>() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI;
    info.lpVerb = verb.as_ptr(); info.lpFile = executable.as_ptr(); info.lpParameters = parameters.as_ptr(); info.nShow = SW_SHOWNORMAL;
    if unsafe { ShellExecuteExW(&mut info) } == 0 { return Err(platform::last_error("启动卸载程序失败或用户取消了权限请求")); }
    let process = Handle(info.hProcess);
    if process.0.is_null() { return Ok(UninstallOutcome { message: "已交给原卸载程序；请完成提示后点击残留复查".into(), completed: false }); }
    let started = std::time::Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) || started.elapsed().as_secs() > 1800 { return Ok(UninstallOutcome { message: "已停止等待，卸载程序仍可继续；完成后可点击残留复查".into(), completed: false }); }
        match unsafe { WaitForSingleObject(process.0, 250) } {
            WAIT_OBJECT_0 => break,
            WAIT_TIMEOUT => continue,
            _ => return Err(platform::last_error("等待卸载程序失败")),
        }
    }
    let mut exit_code = 0;
    if unsafe { GetExitCodeProcess(process.0, &mut exit_code) } == 0 { return Err(platform::last_error("读取卸载程序结果失败")); }
    let still_registered = is_registered(app);
    if [3010, 1641].contains(&exit_code) { return Ok(UninstallOutcome { message: format!("卸载程序返回需要重启（{exit_code}），重启后再复查残留"), completed: false }); }
    if exit_code != 0 { return Err(format!("卸载程序已退出，返回码 {exit_code}；未执行强制删除，请刷新确认")); }
    if still_registered { Ok(UninstallOutcome { message: "安装记录仍存在，可能交给了子进程；完成后可重新复查残留".into(), completed: false }) }
    else { Ok(UninstallOutcome { message: "原卸载程序已完成，正在复查残留".into(), completed: true }) }
}

pub fn product_aliases(app: &App) -> Vec<String> {
    let mut names = vec![app.name.clone()];
    if let Some(leaf) = app.location.file_name() { names.push(leaf.to_string_lossy().to_string()); }
    if let Some(path) = infer_path_from_command(&app.command) { if let Some(parent) = path.parent().and_then(Path::file_name) { names.push(parent.to_string_lossy().to_string()); } }
    let mut out = Vec::new();
    for name in names {
        let compact = normalize_name(&name);
        if compact.chars().count() >= 2 && !["microsoft", "tencent", "腾讯", "programfiles", "programfilesx86", "uninstall", "uninstaller", "app", "data", "cache", "commonfiles", "bin", "release", "software", "users"].contains(&compact.as_str()) && !compact.chars().all(|c| c.is_ascii_digit()) { out.push(compact); }
    }
    out.sort(); out.dedup(); out
}

pub fn normalize_name(value: &str) -> String {
    value.chars().take_while(|c| *c != '(' && *c != '（').filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn msi_only_converts_valid_product_install_switch() {
        let p = "{12345678-1234-ABCD-1234-123456789012}";
        assert_eq!(normalize_msi_args(&format!("/I{p} /passive")), format!("/x {p} /passive"));
        assert_eq!(normalize_msi_args("/i other.msi"), "/i other.msi");
        assert!(!valid_product_code("{not-a-product}"));
    }
    #[test]
    fn component_flags_take_precedence_over_name() {
        assert!(!classify("Example", "Vendor", true, false, false, "").1.is_empty());
        assert!(classify("Example", "Microsoft", false, false, false, "").1.is_empty());
    }
    #[test]
    fn quoted_command_keeps_arguments_and_system_resolution() {
        let win = paths::windows_dir().unwrap().join("System32\\msiexec.exe");
        let (p, args) = split_command(&format!("\"{}\" /i{{12345678-1234-ABCD-1234-123456789012}}", win.display())).unwrap();
        assert_eq!(p, win);
        assert!(args.starts_with("/x "));
        assert!(split_command("relative\\uninstall.exe /s").is_err());
    }
}
