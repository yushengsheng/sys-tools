use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc::{self, Receiver, Sender}};
use std::time::{Duration, Instant};
use systools::{apps::{self, App}, disk, format_size, migration::{self, History, Phase, Record}, platform::{self, wide, Handle}, residue::{self, Residue}, Result};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const TAB: u16 = 10;
const SEARCH: u16 = 100; const APPS: u16 = 101; const REFRESH: u16 = 102; const UNINSTALL: u16 = 103; const OPEN_APP: u16 = 104; const USE_APP: u16 = 105; const APP_DETAIL: u16 = 106;
const APP_CATEGORY_LABEL: u16 = 111; const APP_CATEGORY_FIRST: u16 = 112; const APP_CATEGORY_LAST: u16 = 115;
const CLEAN_INFO: u16 = 200; const SCAN: u16 = 201; const SELECT_ALL: u16 = 202; const CLEAN: u16 = 203; const FILES: u16 = 204; const DRIVE_PICK: u16 = 205; const CATEGORY_PICK: u16 = 206; const FAST_TEMP_SCAN: u16 = 207;
const SOURCE: u16 = 300; const DESTINATION: u16 = 301; const CHOOSE_SOURCE: u16 = 302; const CHOOSE_DEST: u16 = 303; const MIGRATE: u16 = 304; const MIGRATION_NOTE: u16 = 305; const HISTORY: u16 = 306; const RESTORE: u16 = 307; const RELEASE: u16 = 308; const REPAIR: u16 = 309; const OPEN_TARGET: u16 = 310;
const PURGE_HISTORY: u16 = 311;
const CTX_OPEN_SOURCE: u16 = 5001; const CTX_OPEN_TARGET: u16 = 5002; const CTX_RESTORE: u16 = 5003; const CTX_RELEASE: u16 = 5004; const CTX_REPAIR: u16 = 5005; const CTX_PURGE: u16 = 5006;
const STATUS: u16 = 900; const PROGRESS: u16 = 901; const CANCEL: u16 = 902; const ADMIN: u16 = 903;
const CLASS_NAME: &str = "QingzhouSysToolsNative";
const MUTATING: &[u16] = &[REFRESH, UNINSTALL, USE_APP, APP_CATEGORY_FIRST, APP_CATEGORY_FIRST + 1, APP_CATEGORY_FIRST + 2, APP_CATEGORY_FIRST + 3, SCAN, FAST_TEMP_SCAN, SELECT_ALL, CLEAN, MIGRATE, SOURCE, DESTINATION, CHOOSE_SOURCE, CHOOSE_DEST, RESTORE, RELEASE, REPAIR, PURGE_HISTORY, ADMIN];

enum Event {
    Apps(Result<Vec<App>>),
    Disk(Result<disk::Scan>),
    Progress(String),
    Action(Result<String>, bool),
    History(Result<History>),
    Residues(Vec<Residue>),
}

struct Ui {
    hwnd: HWND,
    controls: HashMap<u16, HWND>,
    font: HFONT,
    title_font: HFONT,
    scale: f32,
    page: i32,
    apps: Vec<App>,
    filtered: Vec<usize>,
    disk_scan: Option<disk::Scan>,
    disk_items: Vec<disk::Item>,
    records: Vec<Record>,
    data: PathBuf,
    busy: bool,
    rendering: bool,
    closing: bool,
    cancel: Arc<AtomicBool>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    smoke: Option<Instant>,
    pending_residues: Option<Vec<Residue>>,
}

impl Ui {
    fn control(&self, id: u16) -> HWND { self.controls.get(&id).copied().unwrap_or(null_mut()) }
    fn px(&self, n: i32) -> i32 { (n as f32 * self.scale).round() as i32 }
    fn text(&self, id: u16, text: &str) { unsafe { SetWindowTextW(self.control(id), wide(text).as_ptr()); } }
    fn get_text(&self, id: u16) -> String {
        let hwnd = self.control(id);
        let len = unsafe { GetWindowTextLengthW(hwnd) }.clamp(0, 32767) as usize;
        let mut buffer = vec![0u16; len + 1];
        let n = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
        String::from_utf16_lossy(&buffer[..n.max(0) as usize])
    }
    fn add(&mut self, id: u16, class: &str, text: &str, style: u32, ex: u32) -> HWND {
        let hwnd = unsafe { CreateWindowExW(ex, wide(class).as_ptr(), wide(text).as_ptr(), WS_CHILD | WS_VISIBLE | style, 0, 0, 0, 0, self.hwnd, id as usize as _, GetModuleHandleW(null()), null()) };
        self.controls.insert(id, hwnd);
        unsafe { SendMessageW(hwnd, WM_SETFONT, self.font as usize, 0); }
        hwnd
    }
    fn button(&mut self, id: u16, title: &str) { self.add(id, "BUTTON", title, WS_TABSTOP | BS_PUSHBUTTON as u32, 0); }
    fn label(&mut self, id: u16, text: &str) { self.add(id, "STATIC", text, 0, 0); }
    fn list(&mut self, id: u16, columns: &[(&str, i32)], checks: bool) {
        let hwnd = self.add(id, "SysListView32", "", WS_TABSTOP | LVS_REPORT | LVS_SHOWSELALWAYS | if checks { 0 } else { LVS_SINGLESEL }, WS_EX_CLIENTEDGE);
        unsafe {
            let flags = LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP | if checks { LVS_EX_CHECKBOXES } else { 0 };
            SendMessageW(hwnd, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, flags as isize);
            SendMessageW(hwnd, LVM_SETBKCOLOR, 0, 0x00ffffff);
            SendMessageW(hwnd, LVM_SETTEXTBKCOLOR, 0, 0x00ffffff);
            SendMessageW(hwnd, LVM_SETTEXTCOLOR, 0, 0x00372c24);
            for (index, (name, width)) in columns.iter().enumerate() {
                let mut value = wide(name);
                let column = LVCOLUMNW { mask: LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM, cx: self.px(*width), pszText: value.as_mut_ptr(), iSubItem: index as i32, ..std::mem::zeroed() };
                SendMessageW(hwnd, LVM_INSERTCOLUMNW, index, &column as *const _ as isize);
            }
            if id == APPS {
                SendMessageW(hwnd, LVM_ENABLEGROUPVIEW, 1, 0);
                for (group_id, header) in [(1, "普通软件"), (2, "更新组件"), (3, "共享组件 / 驱动"), (4, "系统组件")] {
                    let mut value = wide(header);
                    let group = LVGROUP { cbSize: std::mem::size_of::<LVGROUP>() as u32, mask: LVGF_HEADER | LVGF_GROUPID | LVGF_STATE, pszHeader: value.as_mut_ptr(), cchHeader: value.len() as i32 - 1, iGroupId: group_id, stateMask: LVGS_COLLAPSIBLE, state: LVGS_COLLAPSIBLE, ..std::mem::zeroed() };
                    SendMessageW(hwnd, LVM_INSERTGROUP, group_id as usize, &group as *const _ as isize);
                }
            }
        }
    }
    fn build(&mut self) {
        self.label(11, "sys tools");
        unsafe { SendMessageW(self.control(11), WM_SETFONT, self.title_font as usize, 0); }
        self.label(12, "软件、空间与迁移");
        self.label(13, "0.1 · 本机运行");
        let tab = self.add(TAB, "SysTabControl32", "", WS_TABSTOP, 0);
        for (i, title) in ["软件管理", "磁盘清理", "迁移与恢复"].iter().enumerate() {
            let mut name = wide(title);
            let item = TCITEMW { mask: TCIF_TEXT, pszText: name.as_mut_ptr(), ..unsafe { std::mem::zeroed() } };
            unsafe { SendMessageW(tab, TCM_INSERTITEMW, i, &item as *const _ as isize); }
        }
        self.add(SEARCH, "EDIT", "", WS_TABSTOP | ES_AUTOHSCROLL as u32, WS_EX_CLIENTEDGE);
        unsafe { SendMessageW(self.control(SEARCH), EM_SETCUEBANNER, 1, wide("搜索名称、发布者或安装位置").as_ptr() as isize); }
        self.button(REFRESH, "刷新列表");
        self.list(APPS, &[("软件名称", 270), ("版本", 120), ("发布者", 190), ("分类", 145), ("安装位置", 380)], false);
        self.label(APP_CATEGORY_LABEL, "分类（点击可定位展开）");
        for (id, title) in (APP_CATEGORY_FIRST..=APP_CATEGORY_LAST).zip(["普通软件", "更新组件", "共享组件 / 驱动", "系统组件"]) { self.button(id, title); }
        self.label(APP_DETAIL, "选择软件查看卸载方式与保护原因。系统组件、共享运行库和驱动默认保护。");
        self.button(UNINSTALL, "卸载所选软件…"); self.button(OPEN_APP, "打开所在目录"); self.button(USE_APP, "准备迁移…");
        self.label(CLEAN_INFO, "选择固定磁盘后扫描并分类。临时文件、软件缓存可清理；软件文件、系统文件和用户文件默认锁定。优先使用“快速扫描临时文件”处理临时目录；扫描只读取，不会自动删除。");
        self.add(DRIVE_PICK, "COMBOBOX", "", WS_TABSTOP | CBS_DROPDOWNLIST as u32, WS_EX_CLIENTEDGE);
        for drive in disk::fixed_drives() { unsafe { SendMessageW(self.control(DRIVE_PICK), CB_ADDSTRING, 0, wide(&drive).as_ptr() as isize); } }
        unsafe { SendMessageW(self.control(DRIVE_PICK), CB_SETCURSEL, 0, 0); }
        self.add(CATEGORY_PICK, "COMBOBOX", "", WS_TABSTOP | CBS_DROPDOWNLIST as u32, WS_EX_CLIENTEDGE);
        for category in disk::CATEGORIES { unsafe { SendMessageW(self.control(CATEGORY_PICK), CB_ADDSTRING, 0, wide(category).as_ptr() as isize); } }
        unsafe { SendMessageW(self.control(CATEGORY_PICK), CB_SETCURSEL, 0, 0); }
        self.button(SCAN, "扫描所选磁盘"); self.button(FAST_TEMP_SCAN, "快速扫描临时文件"); self.button(SELECT_ALL, "全选可清理项"); self.button(CLEAN, "清理勾选项…");
        self.list(FILES, &[("类别", 145), ("软件", 210), ("大小", 110), ("路径", 555)], true);
        self.label(320, "源目录");
        self.add(SOURCE, "EDIT", "", WS_TABSTOP | ES_AUTOHSCROLL as u32, WS_EX_CLIENTEDGE);
        self.button(CHOOSE_SOURCE, "选择…");
        self.label(321, "目标父目录");
        self.add(DESTINATION, "EDIT", "", WS_TABSTOP | ES_AUTOHSCROLL as u32, WS_EX_CLIENTEDGE);
        self.button(CHOOSE_DEST, "选择…"); self.button(MIGRATE, "校验并迁移…");
        self.label(MIGRATION_NOTE, "先退出软件及后台进程。目标将创建同名目录，原位置保留链接。\r\n迁移后先检查软件和数据，再释放原副本；恢复时使用目标盘最新数据。\r\n首版支持普通 NTFS 目录；浏览器整套迁移尚未专项验证。");
        self.label(322, "迁移记录");
        self.list(HISTORY, &[("目录", 175), ("状态", 235), ("原位置", 285), ("目标位置", 285)], false);
        self.button(RESTORE, "恢复原位置…"); self.button(RELEASE, "确认可用，释放副本…"); self.button(REPAIR, "修复中断记录"); self.button(OPEN_TARGET, "打开目标目录"); self.button(PURGE_HISTORY, "清理已完成记录");
        self.label(STATUS, "正在读取软件列表…");
        self.add(PROGRESS, "msctls_progress32", "", PBS_MARQUEE, 0);
        self.button(CANCEL, "取消 / 停止等待"); self.button(ADMIN, "管理员运行");
        self.show_page(0);
        self.set_busy(false);
        unsafe { SetTimer(self.hwnd, 1, 100, None); }
        self.refresh_apps();
    }
    fn place(&self, id: u16, x: i32, y: i32, width: i32, height: i32) {
        unsafe { MoveWindow(self.control(id), self.px(x), self.px(y), self.px(width.max(1)), self.px(height.max(1)), 1); }
    }
    fn layout(&self) {
        let mut r = RECT::default(); unsafe { GetClientRect(self.hwnd, &mut r); }
        let w = (r.right as f32 / self.scale) as i32; let h = (r.bottom as f32 / self.scale) as i32;
        self.place(11, 24, 15, 150, 36); self.place(12, 190, 25, 350, 26); self.place(13, w - 155, 26, 135, 22);
        self.place(TAB, 20, 67, w - 40, h - 137);
        self.place(SEARCH, 38, 112, w - 196, 30); self.place(REFRESH, w - 146, 112, 108, 30);
        self.place(APPS, 38, 154, w - 76, h - 385);
        self.place(APP_CATEGORY_LABEL, 40, h - 220, 145, 24);
        let category_width = ((w - 205) / 4).max(120);
        for (offset, id) in (APP_CATEGORY_FIRST..=APP_CATEGORY_LAST).enumerate() { self.place(id, 190 + offset as i32 * category_width, h - 225, category_width - 6, 31); }
        self.place(APP_DETAIL, 40, h - 177, w - 80, 52);
        self.place(UNINSTALL, 38, h - 115, 155, 31); self.place(OPEN_APP, 204, h - 115, 140, 31); self.place(USE_APP, 355, h - 115, 140, 31);
        self.place(CLEAN_INFO, 40, 113, w - 80, 48); self.place(DRIVE_PICK, 40, 168, 110, 31); self.place(CATEGORY_PICK, 160, 168, 155, 31);
        let clean_x = w - 200;
        self.place(SCAN, 326, 168, 145, 31); self.place(SELECT_ALL, clean_x - 240, 168, 85, 31); self.place(FAST_TEMP_SCAN, clean_x - 148, 168, 140, 31); self.place(CLEAN, clean_x, 168, 162, 31);
        self.place(FILES, 38, 212, w - 76, h - 302);
        self.place(320, 40, 119, 95, 22); self.place(SOURCE, 135, 112, w - 270, 30); self.place(CHOOSE_SOURCE, w - 124, 112, 86, 30);
        self.place(321, 40, 161, 95, 22); self.place(DESTINATION, 135, 154, w - 270, 30); self.place(CHOOSE_DEST, w - 124, 154, 86, 30);
        self.place(MIGRATION_NOTE, 40, 199, w - 255, 70); self.place(MIGRATE, w - 196, 209, 158, 34);
        self.place(322, 40, 277, 120, 23); self.place(HISTORY, 38, 306, w - 76, h - 442);
        self.place(RESTORE, 38, h - 115, 145, 31); self.place(RELEASE, 194, h - 115, 204, 31); self.place(REPAIR, 409, h - 115, 145, 31); self.place(OPEN_TARGET, 565, h - 115, 145, 31); self.place(PURGE_HISTORY, 720, h - 115, 160, 31);
        self.place(STATUS, 24, h - 53, w - 354, 36); self.place(PROGRESS, 24, h - 14, w - 48, 3);
        self.place(CANCEL, w - 318, h - 51, 165, 30); self.place(ADMIN, w - 143, h - 51, 119, 30);
    }
    fn show_page(&mut self, page: i32) {
        self.page = page;
        for (&id, &hwnd) in &self.controls {
            let visible = match id { 100..=199 => page == 0, 200..=299 => page == 1, 300..=399 => page == 2, PROGRESS => self.busy, _ => true };
            unsafe { ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE }); }
        }
        unsafe { SendMessageW(self.control(TAB), TCM_SETCURSEL, page as usize, 0); }
        self.update_selection();
    }
    fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
        for &id in MUTATING { unsafe { EnableWindow(self.control(id), (!busy) as i32); } }
        unsafe {
            EnableWindow(self.control(CANCEL), busy as i32);
            SendMessageW(self.control(PROGRESS), PBM_SETMARQUEE, busy as usize, 40);
            ShowWindow(self.control(PROGRESS), if busy { SW_SHOW } else { SW_HIDE });
        }
        self.update_selection();
    }
    fn start<F>(&mut self, status: &str, task: F)
    where F: FnOnce(Arc<AtomicBool>, Sender<Event>, PathBuf) -> Event + Send + 'static {
        if self.busy { return; }
        self.cancel.store(false, Ordering::Relaxed);
        self.text(STATUS, status); self.set_busy(true);
        let tx = self.tx.clone(); let cancel = self.cancel.clone(); let data = self.data.clone();
        std::thread::spawn(move || {
            let event = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| task(cancel, tx.clone(), data))).unwrap_or_else(|_| Event::Action(Err("后台操作发生异常，请检查迁移记录；未自动删除保留副本".into()), false));
            let _ = tx.send(event);
        });
    }
    fn review_residues(&mut self, items: Vec<Residue>) {
        if items.is_empty() { self.text(STATUS, "卸载完成，未发现高置信度残留"); return; }
        let summary = residue::format_summary(&items);
        let answer = unsafe { MessageBoxW(self.hwnd, wide(&format!("原卸载程序已完成。sys tools 重新检查到以下残留：\r\n\r\n{}\r\n是否删除这些高/中置信度残留？\r\n\r\n默认不会扫描或删除共享目录、未知目录和系统组件。", summary)).as_ptr(), wide("二次清理残留").as_ptr(), MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2) };
        if answer != IDYES { self.text(STATUS, &format!("卸载完成，保留 {} 项残留供你手动检查", items.len())); return; }
        self.start("正在清理已确认的卸载残留…", move |_, _, data| {
            let (removed, errors) = residue::clean(&items);
            platform::log_action(&data, "residue-clean", &format!("删除 {} 项，跳过 {} 项", removed, errors.len()));
            let text = if errors.is_empty() { format!("二次清理完成：删除 {} 项残留", removed) } else { format!("二次清理完成：删除 {} 项，跳过 {} 项；被占用或变化的项目已保留", removed, errors.len()) };
            Event::Action(Ok(text), false)
        });
    }
    fn refresh_apps(&mut self) {
        self.start("正在读取软件登记（不触发 Windows Installer 修复）…", |_, tx, data| {
            let _ = tx.send(Event::History(migration::history(&data)));
            Event::Apps(apps::scan())
        });
    }
    fn render_rows(&mut self, list: u16, rows: Vec<Vec<String>>) {
        self.rendering = true;
        let hwnd = self.control(list);
        unsafe {
            SendMessageW(hwnd, WM_SETREDRAW, 0, 0); SendMessageW(hwnd, LVM_DELETEALLITEMS, 0, 0);
            for (row, columns) in rows.iter().enumerate() {
                for (column, text) in columns.iter().enumerate() {
                    let mut value = wide(text);
                    let item = LVITEMW { mask: LVIF_TEXT, iItem: row as i32, iSubItem: column as i32, pszText: value.as_mut_ptr(), ..std::mem::zeroed() };
                    SendMessageW(hwnd, if column == 0 { LVM_INSERTITEMW } else { LVM_SETITEMW }, 0, &item as *const _ as isize);
                }
            }
            SendMessageW(hwnd, WM_SETREDRAW, 1, 0); InvalidateRect(hwnd, null(), 1);
        }
        self.rendering = false;
    }
    fn render_app_rows(&mut self) {
        self.rendering = true;
        let hwnd = self.control(APPS);
        unsafe { SendMessageW(hwnd, WM_SETREDRAW, 0, 0); SendMessageW(hwnd, LVM_DELETEALLITEMS, 0, 0); }
        for (row, &index) in self.filtered.iter().enumerate() {
            let app = &self.apps[index];
            let group_id = match app.category.as_str() { "普通软件" => 1, "更新组件" => 2, "共享组件 / 驱动" => 3, _ => 4 };
            for (column, text) in [app.name.clone(), app.version.clone(), app.publisher.clone(), app.category.clone(), app.location.display().to_string()].iter().enumerate() {
                let mut value = wide(text);
                let item = LVITEMW { mask: LVIF_TEXT | if column == 0 { LVIF_GROUPID } else { 0 }, iItem: row as i32, iSubItem: column as i32, iGroupId: group_id, pszText: value.as_mut_ptr(), ..unsafe { std::mem::zeroed() } };
                unsafe { SendMessageW(hwnd, if column == 0 { LVM_INSERTITEMW } else { LVM_SETITEMW }, 0, &item as *const _ as isize); }
            }
        }
        unsafe { SendMessageW(hwnd, WM_SETREDRAW, 1, 0); InvalidateRect(hwnd, null(), 1); }
        self.rendering = false;
    }
    fn filter_apps(&mut self) {
        let query = self.get_text(SEARCH).to_lowercase();
        self.filtered = self.apps.iter().enumerate().filter(|(_, a)| query.is_empty() || format!("{} {} {}", a.name, a.publisher, a.location.display()).to_lowercase().contains(&query)).map(|(i, _)| i).collect();
        self.render_app_rows(); self.update_category_buttons(); self.update_selection();
    }
    fn update_category_buttons(&self) {
        let groups = [("普通软件", 0usize), ("更新组件", 1), ("共享组件 / 驱动", 2), ("系统组件", 3)];
        for (offset, (name, _)) in groups.iter().enumerate() {
            let count = self.filtered.iter().filter(|&&i| self.apps.get(i).is_some_and(|a| a.category == *name)).count();
            self.text(APP_CATEGORY_FIRST + offset as u16, &format!("{} ({})", name, count));
        }
    }
    fn expand_app_group(&mut self, group_id: i32) {
        let mut group = LVGROUP { cbSize: std::mem::size_of::<LVGROUP>() as u32, mask: LVGF_STATE, stateMask: LVGS_COLLAPSED, state: 0, ..unsafe { std::mem::zeroed() } };
        unsafe {
            let hwnd = self.control(APPS);
            SendMessageW(hwnd, LVM_SETGROUPINFO, group_id as usize, &mut group as *mut _ as isize);
            let count = SendMessageW(hwnd, LVM_GETITEMCOUNT, 0, 0);
            for row in 0..count {
                let mut item = LVITEMW { mask: LVIF_GROUPID, iItem: row as i32, ..std::mem::zeroed() };
                if SendMessageW(hwnd, LVM_GETITEMW, 0, &mut item as *mut _ as isize) != 0 && item.iGroupId == group_id {
                    SendMessageW(hwnd, LVM_ENSUREVISIBLE, row as usize, 1);
                    break;
                }
            }
        }
    }
    fn selected(&self, id: u16) -> Option<usize> {
        let value = unsafe { SendMessageW(self.control(id), LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize) };
        if value >= 0 { Some(value as usize) } else { None }
    }
    fn selected_app(&self) -> Option<App> { self.selected(APPS).and_then(|i| self.filtered.get(i)).and_then(|i| self.apps.get(*i)).cloned() }
    fn selected_record(&self) -> Option<Record> { self.selected(HISTORY).and_then(|i| self.records.get(i)).cloned() }
    fn update_selection(&self) {
        if self.rendering { return; }
        let app = self.selected_app();
        unsafe {
            EnableWindow(self.control(UNINSTALL), (!self.busy && app.as_ref().is_some_and(App::can_uninstall)) as i32);
            EnableWindow(self.control(USE_APP), (!self.busy && app.as_ref().is_some_and(App::can_migrate)) as i32);
            EnableWindow(self.control(OPEN_APP), app.as_ref().is_some_and(|a| a.location.is_dir()) as i32);
            let record = self.selected_record();
            EnableWindow(self.control(RESTORE), (!self.busy && record.as_ref().is_some_and(|r| [Phase::Active, Phase::AwaitingReview].contains(&r.phase))) as i32);
            EnableWindow(self.control(RELEASE), (!self.busy && record.as_ref().is_some_and(|r| !r.copies_released && [Phase::Active, Phase::AwaitingReview, Phase::Restored, Phase::RolledBack].contains(&r.phase))) as i32);
            EnableWindow(self.control(REPAIR), (!self.busy && record.is_some()) as i32);
            EnableWindow(self.control(OPEN_TARGET), record.as_ref().is_some_and(|r| r.target.exists()) as i32);
            EnableWindow(self.control(PURGE_HISTORY), (!self.busy && self.records.iter().any(|r| r.copies_released && [Phase::Restored, Phase::RolledBack].contains(&r.phase))) as i32);
            EnableWindow(self.control(CLEAN), (!self.busy && !self.disk_items.is_empty()) as i32);
        }
        if let Some(a) = app {
            let reason = if a.protection.is_empty() { if a.command.is_empty() { "未登记卸载程序；不会直接删除软件目录" } else { "使用原卸载程序；不自动清理疑似残留" } } else { &a.protection };
            self.text(APP_DETAIL, &format!("{}  ·  {}\r\n{}", a.name, a.kind, reason));
        } else { self.text(APP_DETAIL, "选择软件查看卸载方式与保护原因。分类依据安装登记与保守规则，不能替代完整系统文件鉴定。"); }
    }
    fn confirm(&self, title: &str, message: &str) -> bool {
        unsafe { MessageBoxW(self.hwnd, wide(message).as_ptr(), wide(title).as_ptr(), MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2) == IDYES }
    }
    fn show_error(&self, error: &str) { unsafe { MessageBoxW(self.hwnd, wide(error).as_ptr(), wide("操作未完成").as_ptr(), MB_OK | MB_ICONWARNING); } }
    fn folder(&self) -> Option<PathBuf> {
        let info = BROWSEINFOW { hwndOwner: self.hwnd, ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE | BIF_EDITBOX, ..unsafe { std::mem::zeroed() } };
        // 对话框同步使用标题，保持 UTF-16 缓冲区存活。
        let title = wide("选择本地文件夹"); let info = BROWSEINFOW { lpszTitle: title.as_ptr(), ..info };
        let pidl = unsafe { SHBrowseForFolderW(&info) };
        if pidl.is_null() { return None; }
        let mut buffer = vec![0u16; 32768];
        let ok = unsafe { SHGetPathFromIDListEx(pidl, buffer.as_mut_ptr(), buffer.len() as u32, 0) };
        unsafe { windows_sys::Win32::System::Com::CoTaskMemFree(pidl as _); }
        if ok == 0 { return None; }
        let n = buffer.iter().position(|x| *x == 0).unwrap_or(buffer.len()); Some(PathBuf::from(String::from_utf16_lossy(&buffer[..n])))
    }
    fn open_folder(&self, folder: &Path) {
        let result = unsafe { ShellExecuteW(self.hwnd, wide("open").as_ptr(), wide(folder).as_ptr(), null(), null(), SW_SHOWNORMAL) };
        if result as usize <= 32 { self.show_error("目录无法打开或已经不存在"); }
    }
    fn history_context_menu(&mut self, activate: &NMITEMACTIVATE) {
        if activate.iItem < 0 || self.records.get(activate.iItem as usize).is_none() { return; }
        let list = self.control(HISTORY);
        let state = LVITEMW { stateMask: LVIS_SELECTED | LVIS_FOCUSED, state: LVIS_SELECTED | LVIS_FOCUSED, ..unsafe { std::mem::zeroed() } };
        unsafe { SendMessageW(list, LVM_SETITEMSTATE, activate.iItem as usize, &state as *const _ as isize); }
        let Some(record) = self.records.get(activate.iItem as usize) else { return; };
        let menu = unsafe { CreatePopupMenu() };
        if menu.is_null() { return; }
        let append = |id: u16, title: &str, enabled: bool| {
            let text = wide(title);
            unsafe { AppendMenuW(menu, MF_STRING | if enabled { 0 } else { MF_GRAYED }, id as usize, text.as_ptr()); }
        };
        append(CTX_OPEN_SOURCE, "打开原位置", record.source.exists());
        append(CTX_OPEN_TARGET, "打开目标目录", record.target.exists());
        unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, null()); }
        append(CTX_RESTORE, "恢复原位置", !self.busy && [Phase::Active, Phase::AwaitingReview].contains(&record.phase));
        append(CTX_RELEASE, "确认可用，释放副本", !self.busy && !record.copies_released && [Phase::Active, Phase::AwaitingReview, Phase::Restored, Phase::RolledBack].contains(&record.phase));
        append(CTX_REPAIR, "修复中断记录", !self.busy);
        append(CTX_PURGE, "清理此记录", !self.busy && record.copies_released && [Phase::Restored, Phase::RolledBack].contains(&record.phase));
        let mut point = activate.ptAction;
        unsafe {
            ClientToScreen(list, &mut point);
            let selected = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_RETURNCMD, point.x, point.y, 0, self.hwnd, null());
            DestroyMenu(menu);
            if selected != 0 { self.command(selected as u16, 0); }
        }
    }
    fn checked_disk(&self) -> Vec<disk::Item> {
        self.disk_items.iter().enumerate().filter(|(i, item)| item.deletable && unsafe { SendMessageW(self.control(FILES), LVM_GETITEMSTATE, *i, LVIS_STATEIMAGEMASK as isize) } >> 12 == 2).map(|(_, f)| f.clone()).collect()
    }
    fn selected_disk_category(&self) -> String {
        let len = unsafe { GetWindowTextLengthW(self.control(CATEGORY_PICK)) }.max(0) as usize;
        let mut buffer = vec![0u16; len + 1]; let n = unsafe { GetWindowTextW(self.control(CATEGORY_PICK), buffer.as_mut_ptr(), buffer.len() as i32) };
        String::from_utf16_lossy(&buffer[..n.max(0) as usize])
    }
    fn refresh_disk_page(&mut self) {
        let Some(scan) = self.disk_scan.clone() else { return; };
        let category = self.selected_disk_category();
        match disk::page(&scan, if category.is_empty() { disk::CATEGORIES[0] } else { &category }, 0) {
            Ok((items, more)) => { self.disk_items = items; let rows = self.disk_items.iter().map(|x| vec![x.category.clone(), x.owner.clone(), format_size(x.size), x.path.display().to_string()]).collect(); self.render_rows(FILES, rows); self.update_selection(); self.text(STATUS, &format!("分类“{}”显示 {} 项{}；扫描总计 {} 项", category, self.disk_items.len(), if more { "（仅显示前 500 项）" } else { "" }, scan.files)); }
            Err(e) => self.show_error(&e),
        }
    }
    fn command(&mut self, id: u16, notification: u16) {
        if id == SEARCH && notification as u32 == EN_CHANGE { self.filter_apps(); return; }
        if id == CANCEL { self.cancel.store(true, Ordering::Relaxed); self.text(STATUS, "正在安全停止；迁移需要完成回滚，请稍候…"); return; }
        if self.busy { return; }
        if id == CTX_OPEN_SOURCE {
            if let Some(r) = self.selected_record() { self.open_folder(&r.source); }
            return;
        }
        let id = match id { CTX_OPEN_TARGET => OPEN_TARGET, CTX_RESTORE => RESTORE, CTX_RELEASE => RELEASE, CTX_REPAIR => REPAIR, _ => id };
        match id {
            REFRESH => self.refresh_apps(),
            APP_CATEGORY_FIRST..=APP_CATEGORY_LAST => self.expand_app_group((id - APP_CATEGORY_FIRST + 1) as i32),
            OPEN_APP => if let Some(a) = self.selected_app() { self.open_folder(&a.location); },
            USE_APP => if let Some(a) = self.selected_app() { if a.can_migrate() { self.text(SOURCE, &a.location.display().to_string()); self.show_page(2); } },
            UNINSTALL => if let Some(a) = self.selected_app() {
                if !a.can_uninstall() { return; }
                if self.confirm("卸载软件", &format!("卸载“{}”？\r\n\r\n将调用原卸载程序，请按其提示操作。sys tools 不会自动删除疑似残留或强制结束进程。", a.name)) {
                    self.start("正在等待原卸载程序…", move |cancel, tx, data| {
                        let result = apps::uninstall(&a, &cancel);
                        if result.as_ref().is_ok_and(|r| r.completed) { let _ = tx.send(Event::Residues(residue::scan(&a))); }
                        let message = result.map(|outcome| outcome.message);
                        platform::log_action(&data, "uninstall", if message.is_ok() { "原卸载程序流程已返回；开始复查残留" } else { "未完成" }); Event::Action(message, true)
                    });
                }
            },
            SCAN => {
                let root = self.get_text(DRIVE_PICK); let apps = self.apps.clone(); let data = self.data.clone();
                self.start("正在扫描所选磁盘，尚未删除…", move |cancel, tx, _| Event::Disk(disk::scan(Path::new(root.trim()), &apps, &data, &cancel, &|m| { let _ = tx.send(Event::Progress(m)); })));
            }
            FAST_TEMP_SCAN => {
                let root = self.get_text(DRIVE_PICK); let apps = self.apps.clone(); let data = self.data.clone();
                self.start("正在快速扫描 Windows 与用户临时目录…", move |cancel, tx, _| Event::Disk(disk::scan_temp(Path::new(root.trim()), &apps, &data, &cancel, &|m| { let _ = tx.send(Event::Progress(m)); })));
            }
            CATEGORY_PICK if notification as u32 == CBN_SELCHANGE => self.refresh_disk_page(),
            SELECT_ALL => {
                let all_checked = self.checked_disk().len() == self.disk_items.iter().filter(|x| x.deletable).count();
                let item = LVITEMW { stateMask: LVIS_STATEIMAGEMASK, state: if all_checked { 0x1000 } else { 0x2000 }, ..unsafe { std::mem::zeroed() } };
                unsafe { SendMessageW(self.control(FILES), LVM_SETITEMSTATE, usize::MAX, &item as *const _ as isize); }
            }
            CLEAN => {
                let files = self.checked_disk(); if files.is_empty() { self.show_error("请先勾选可清理的临时文件或软件缓存"); return; }
                let bytes: u64 = files.iter().map(|f| f.size).sum(); let scan = self.disk_scan.clone(); let data = self.data.clone();
                if let Some(scan) = scan { if self.confirm("清理已确认分类", &format!("删除所选 {} 项，预计 {}？\r\n\r\n仅处理临时文件和软件缓存。软件数据、系统文件、用户文件和迁移保护项不会删除。变化或被占用的文件会跳过。", files.len(), format_size(bytes))) {
                    self.start("正在清理已确认的安全分类…", move |cancel, tx, _| {
                        let out = disk::clean(&files, &scan, &data, &cancel, &|m| { let _ = tx.send(Event::Progress(m)); });
                        let text = format!("已清理 {} 项、{}；跳过 {} 项", out.0, format_size(out.1), out.2);
                        platform::log_action(&data, "cleanup", &text); Event::Action(Ok(text), false)
                    });
                }}
            }
            CHOOSE_SOURCE | CHOOSE_DEST => if let Some(p) = self.folder() { self.text(if id == CHOOSE_SOURCE { SOURCE } else { DESTINATION }, &p.display().to_string()); },
            MIGRATE => {
                let source = PathBuf::from(self.get_text(SOURCE).trim().trim_matches('"')); let destination = PathBuf::from(self.get_text(DESTINATION).trim().trim_matches('"'));
                if source.as_os_str().is_empty() || destination.as_os_str().is_empty() { self.show_error("请选择源目录和目标父目录"); return; }
                if self.confirm("迁移目录", &format!("将目录迁移到所选目标父目录？\r\n\r\n源：{}\r\n目标父目录：{}\r\n\r\n请确认已退出软件及所有后台进程，操作完成前不要重新启动。迁移后保留原副本，检查可用后再释放。", source.display(), destination.display())) {
                    self.start("正在检查迁移条件…", move |cancel, tx, data| {
                        let result = migration::migrate(&source, &destination, &data, &cancel, &|m| { let _ = tx.send(Event::Progress(m)); }).map(|r| format!("迁移完成，{}。请先检查软件与数据，再释放原副本", format_size(r.bytes)));
                        platform::log_action(&data, "migrate", if result.is_ok() { "迁移完成，原副本保留" } else { "未完成，请检查记录" });
                        let _ = tx.send(Event::History(migration::history(&data))); Event::Action(result, false)
                    });
                }
            }
            RESTORE | RELEASE | REPAIR => if let Some(record) = self.selected_record() {
                let message = match id { RESTORE => "恢复到原位置？\r\n\r\n请先退出相关软件。将复制目标盘上的最新数据，原盘需有足够空间。恢复后仍保留目标副本，确认可用后再释放。", RELEASE => "已检查软件启动与数据完整，确认释放保留的副本？\r\n\r\n这是永久删除：迁移生效时删除原副本；恢复完成时删除目标及旧副本；回滚后删除本工具记录的半成品。", _ => "检查此条中断记录并尝试恢复可用路径？\r\n\r\n不会覆盖原位置新出现的内容，无法确认时将保留所有副本。" };
                if self.confirm("迁移记录操作", message) {
                    self.start("正在处理迁移记录…", move |cancel, tx, data| {
                        let progress = |m| { let _ = tx.send(Event::Progress(m)); };
                        let result = match id { RESTORE => migration::restore(&record, &data, &cancel, &progress), RELEASE => migration::release_copy(&record, &data, &cancel, &progress), _ => migration::recover(&record, &data) }.map(|r| r.label().to_string());
                        platform::log_action(&data, "migration-record", if result.is_ok() { "处理完成" } else { "未完成，副本保留" });
                        let _ = tx.send(Event::History(migration::history(&data))); Event::Action(result, false)
                    });
                }
            },
            OPEN_TARGET => if let Some(r) = self.selected_record() { self.open_folder(&r.target); },
            CTX_PURGE => if let Some(record) = self.selected_record() {
                if self.confirm("清理迁移记录", "仅删除已恢复或已回滚、且副本已释放的记录文件，不会删除软件目录或副本。是否继续？") {
                    self.start("正在清理迁移记录…", move |_, tx, data| {
                        let result = migration::purge_record(&record, &data).map(|_| "迁移记录已清理".to_string());
                        platform::log_action(&data, "migration-purge", if result.is_ok() { "清理单条已完成记录" } else { "记录仍保留" });
                        let _ = tx.send(Event::History(migration::history(&data)));
                        Event::Action(result, false)
                    });
                }
            },
            PURGE_HISTORY => if self.confirm("清理已完成记录", "只清理已恢复或已回滚、且副本已释放并通过路径核验的记录。正在生效的迁移和有残留副本的记录会保留。是否继续？") {
                self.start("正在检查并清理已完成记录…", move |_, tx, data| {
                    let result = migration::purge_completed(&data).map(|(removed, skipped)| format!("已清理 {} 条记录{}", removed, if skipped > 0 { format!("，{} 条因路径或副本状态异常而保留", skipped) } else { String::new() }));
                    platform::log_action(&data, "migration-purge", if result.is_ok() { "清理已完成记录" } else { "批量清理未完成" });
                    let _ = tx.send(Event::History(migration::history(&data)));
                    Event::Action(result, false)
                });
            },
            ADMIN => {
                if let Ok(exe) = std::env::current_exe() {
                    let mut info = unsafe { std::mem::zeroed::<SHELLEXECUTEINFOW>() };
                    let verb = wide("runas"); let file = wide(exe); info.cbSize = std::mem::size_of_val(&info) as u32; info.lpVerb = verb.as_ptr(); info.lpFile = file.as_ptr(); info.nShow = SW_SHOWNORMAL;
                    // 单实例互斥在本进程退出后释放，提权进程会短暂等待。
                    let params = wide("--elevated"); info.lpParameters = params.as_ptr();
                    if unsafe { ShellExecuteExW(&mut info) } != 0 { unsafe { DestroyWindow(self.hwnd); } } else { self.show_error("管理员权限请求未完成"); }
                }
            }
            _ => {}
        }
    }
    fn tick(&mut self) {
        let mut latest_progress = None;
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::Progress(text) => latest_progress = Some(text),
                Event::Apps(result) => {
                    self.set_busy(false);
                    match result { Ok(apps) => { self.apps = apps; self.filter_apps(); self.text(STATUS, &format!("已读取 {} 个安装项 · 保护项可查看，默认不可卸载", self.apps.len())); }, Err(e) => self.show_error(&e) }
                    latest_progress = None;
                }
                Event::Disk(result) => {
                    self.set_busy(false);
                    match result {
                        Ok(scan) => {
                            if scan.cancelled {
                                disk::discard(&scan);
                                self.text(STATUS, "扫描已取消，未删除任何文件");
                                latest_progress = None;
                                continue;
                            }
                            let categories = scan.categories.iter().map(|(name, (count, bytes))| format!("{} {}项 / {}", name, count, format_size(*bytes))).collect::<Vec<_>>().join("；");
                            match disk::page(&scan, disk::CATEGORIES[0], 0) {
                                Ok((items, _)) => {
                                    if let Some(previous) = self.disk_scan.take() { disk::discard(&previous); }
                                    self.disk_scan = Some(scan.clone()); self.disk_items = items;
                                    let rows = self.disk_items.iter().map(|x| vec![x.category.clone(), x.owner.clone(), format_size(x.size), x.path.display().to_string()]).collect();
                                    self.render_rows(FILES, rows); self.update_selection();
                                    self.text(STATUS, &format!("扫描完成：{} 个文件、{}；{}。可按类别查看并只勾选安全分类", scan.files, format_size(scan.bytes), categories));
                                }
                                Err(e) => { disk::discard(&scan); self.show_error(&e); },
                            }
                        }
                        Err(e) => { self.text(STATUS, "扫描未完成，未删除任何文件"); self.show_error(&e); }
                    }
                    latest_progress = None;
                }
                Event::History(result) => {
                    match result {
                        Ok(history) => {
                            self.records = history.records;
                            let rows = self.records.iter().map(|r| vec![r.source.file_name().unwrap_or_default().to_string_lossy().to_string(), format!("{}{}", r.label(), if r.last_error.is_empty() { "" } else { " · 有提示" }), r.source.display().to_string(), r.target.display().to_string()]).collect();
                            self.render_rows(HISTORY, rows);
                            if history.unreadable > 0 { self.show_error(&format!("{} 条迁移记录无法读取，清理和新迁移会暂停。请保留 data 目录并检查记录", history.unreadable)); }
                        }
                        Err(e) => self.show_error(&e),
                    }
                }
                Event::Residues(items) => { self.pending_residues = Some(items); }
                Event::Action(result, refresh) => {
                    self.set_busy(false); latest_progress = None;
                    match result { Ok(text) => { self.text(STATUS, &text); }, Err(e) => { self.text(STATUS, "操作未完成，详见提示与迁移记录"); self.show_error(&e); } }
                    // 清理完成后丢弃旧快照，避免重复执行和显示已经删除的条目。
        if self.page == 1 { if let Some(scan) = self.disk_scan.take() { disk::discard(&scan); } self.disk_items.clear(); self.render_rows(FILES, Vec::new()); self.update_selection(); }
                    if let Some(items) = self.pending_residues.take() {
                        let has_items = !items.is_empty();
                        self.review_residues(items);
                        if !has_items && refresh && !self.closing { self.refresh_apps(); }
                    } else if refresh && !self.closing { self.refresh_apps(); }
                }
            }
        }
        if let Some(text) = latest_progress { if self.busy { self.text(STATUS, &text); } }
        if (self.closing || self.smoke.is_some_and(|t| t.elapsed() >= Duration::from_secs(5))) && !self.busy { unsafe { DestroyWindow(self.hwnd); } }
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let create = &*(l as *const CREATESTRUCTW); SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        (*(create.lpCreateParams as *mut Ui)).hwnd = hwnd;
    }
    if msg == WM_CTLCOLORSTATIC || msg == WM_CTLCOLOREDIT { SetTextColor(w as _, 0x00372c24); SetBkColor(w as _, 0x00ffffff); return GetStockObject(WHITE_BRUSH) as isize; }
    let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ui;
    if pointer.is_null() { return DefWindowProcW(hwnd, msg, w, l); }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ui = &mut *pointer;
        match msg {
            WM_CREATE => { ui.build(); ui.layout(); 0 }
            WM_SIZE => { ui.layout(); 0 }
            WM_GETMINMAXINFO => { let info = &mut *(l as *mut MINMAXINFO); info.ptMinTrackSize = POINT { x: ui.px(920), y: ui.px(680) }; 0 }
            WM_DPICHANGED => {
                ui.scale = (w & 0xffff) as f32 / 96.0;
                let r = &*(l as *const RECT); SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE); ui.layout(); 0
            }
            WM_COMMAND => { ui.command((w & 0xffff) as u16, ((w >> 16) & 0xffff) as u16); 0 }
            WM_NOTIFY => {
                let notification = &*(l as *const NMHDR);
                if notification.idFrom == TAB as usize && notification.code == TCN_SELCHANGE { let page = SendMessageW(ui.control(TAB), TCM_GETCURSEL, 0, 0) as i32; ui.show_page(page); }
                else if notification.idFrom == HISTORY as usize && notification.code == NM_RCLICK { ui.history_context_menu(&*(l as *const NMITEMACTIVATE)); }
                else if notification.code == LVN_ITEMCHANGED && !ui.rendering {
                    // 分组收缩/展开会批量触发 ITEMCHANGED；只有选中状态变化才需要更新底部操作区，
                    // 避免每一行都重复读取文本并重绘详情，导致双击分组时像重新刷新。
                    let change = &*(l as *const NMLISTVIEW);
                    let selection_changed = change.uChanged & LVIF_STATE != 0
                        && ((change.uOldState ^ change.uNewState) & (LVIS_SELECTED | LVIS_FOCUSED)) != 0;
                    if selection_changed { ui.update_selection(); }
                }
                else if notification.code == NM_DBLCLK && notification.idFrom == HISTORY as usize { if let Some(r) = ui.selected_record() { if !r.last_error.is_empty() { ui.show_error(&r.last_error); } } }
                0
            }
            WM_TIMER => { ui.tick(); 0 }
            WM_CLOSE => {
                if ui.busy {
                    if ui.confirm("安全退出", "操作尚未完成。请求取消并等待安全回滚后退出？\r\n\r\n如果正在卸载，只停止等待，不会终止原卸载程序。") { ui.closing = true; ui.cancel.store(true, Ordering::Relaxed); ui.text(STATUS, "正在安全停止，请等待恢复完成…"); }
                } else { DestroyWindow(hwnd); }
                0
            }
            WM_DESTROY => { KillTimer(hwnd, 1); PostQuitMessage(0); 0 }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }));
    result.unwrap_or_else(|_| DefWindowProcW(hwnd, msg, w, l))
}

pub fn run() {
    let _com = platform::ComGuard::new();
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2); }
    let elevated = std::env::args().any(|a| a == "--elevated");
    // 以普通方式启动时自动请求一次管理员令牌，迁移和清理不再依赖用户
    // 事后点击按钮提权；--elevated 用于避免重启后的递归。
    if !elevated && unsafe { IsUserAnAdmin() } == 0 {
        if let Ok(exe) = std::env::current_exe() {
            let mut info = unsafe { std::mem::zeroed::<SHELLEXECUTEINFOW>() };
            let verb = wide("runas"); let file = wide(exe); let params = wide("--elevated");
            info.cbSize = std::mem::size_of_val(&info) as u32; info.lpVerb = verb.as_ptr(); info.lpFile = file.as_ptr(); info.lpParameters = params.as_ptr(); info.nShow = SW_SHOWNORMAL;
            if unsafe { ShellExecuteExW(&mut info) } == 0 {
                unsafe { MessageBoxW(null_mut(), wide("需要管理员权限才能执行迁移和清理操作").as_ptr(), wide("sys tools").as_ptr(), MB_OK | MB_ICONWARNING); }
            }
        }
        return;
    }
    let mutex_name = wide("Local\\QingzhouSysToolsNative-0.1");
    let mut mutex = Handle(unsafe { CreateMutexW(null(), 0, mutex_name.as_ptr()) });
    let mut exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if elevated && exists {
        for _ in 0..30 {
            drop(mutex); std::thread::sleep(Duration::from_millis(100));
            mutex = Handle(unsafe { CreateMutexW(null(), 0, mutex_name.as_ptr()) }); exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
            if !exists { break; }
        }
    }
    if exists {
        let hwnd = unsafe { FindWindowW(wide(CLASS_NAME).as_ptr(), null()) };
        if !hwnd.is_null() { unsafe { ShowWindow(hwnd, SW_RESTORE); SetForegroundWindow(hwnd); } }
        return;
    }
    let data = match platform::data_dir() { Ok(data) => data, Err(e) => { unsafe { MessageBoxW(null_mut(), wide(&e).as_ptr(), wide("sys tools").as_ptr(), MB_OK | MB_ICONWARNING); } return; } };
    disk::discard_stale(&data);
    let (tx, rx) = mpsc::channel();
    let scale = unsafe { GetDpiForSystem() }.max(96) as f32 / 96.0;
    let font = unsafe { CreateFontW(-(15.0 * scale) as i32, 0, 0, 0, FW_NORMAL as i32, 0, 0, 0, DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32, CLEARTYPE_QUALITY as u32, DEFAULT_PITCH as u32, wide("Microsoft YaHei UI").as_ptr()) };
    let title_font = unsafe { CreateFontW(-(28.0 * scale) as i32, 0, 0, 0, FW_SEMIBOLD as i32, 0, 0, 0, DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32, CLEARTYPE_QUALITY as u32, DEFAULT_PITCH as u32, wide("Microsoft YaHei UI").as_ptr()) };
    let ui = Box::new(Ui { hwnd: null_mut(), controls: HashMap::new(), font, title_font, scale, page: 0, apps: Vec::new(), filtered: Vec::new(), disk_scan: None, disk_items: Vec::new(), records: Vec::new(), data, busy: false, rendering: false, closing: false, cancel: Arc::new(AtomicBool::new(false)), tx, rx, smoke: std::env::args().any(|a| a == "--ui-smoke").then(Instant::now), pending_residues: None });
    let pointer = Box::into_raw(ui);
    unsafe {
        InitCommonControlsEx(&INITCOMMONCONTROLSEX { dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_LISTVIEW_CLASSES | ICC_TAB_CLASSES | ICC_PROGRESS_CLASS | ICC_STANDARD_CLASSES });
        let class_name = wide(CLASS_NAME);
        let class = WNDCLASSEXW { cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32, style: CS_HREDRAW | CS_VREDRAW, lpfnWndProc: Some(window_proc), hInstance: GetModuleHandleW(null()), hCursor: LoadCursorW(null_mut(), IDC_ARROW), hbrBackground: GetStockObject(WHITE_BRUSH) as _, lpszClassName: class_name.as_ptr(), ..std::mem::zeroed() };
        RegisterClassExW(&class);
        let hwnd = CreateWindowExW(0, class_name.as_ptr(), wide("sys tools").as_ptr(), WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN, CW_USEDEFAULT, CW_USEDEFAULT, (1060.0 * scale) as i32, (770.0 * scale) as i32, null_mut(), null_mut(), class.hInstance, pointer as _);
        if !hwnd.is_null() {
            ShowWindow(hwnd, SW_SHOW); UpdateWindow(hwnd);
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 { if IsDialogMessageW(hwnd, &msg) == 0 { TranslateMessage(&msg); DispatchMessageW(&msg); } }
        } else { MessageBoxW(null_mut(), wide(&platform::last_error("窗口创建失败")).as_ptr(), wide("sys tools").as_ptr(), MB_OK | MB_ICONERROR); }
        drop(Box::from_raw(pointer)); DeleteObject(font); DeleteObject(title_font);
    }
    drop(mutex);
}
