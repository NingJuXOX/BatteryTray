#![allow(deprecated)]
//! Add-device wizard and per-device settings window.
//!
//! Four pages share one window: how to import, which device to analyze, the
//! prompt/JSON page, and the settings page. The wizard never touches the
//! configuration until the device has actually reported a battery level, so a
//! half-finished AI answer cannot pollute the playlist.

use super::{init_gui, status_text, theme, Toggle};
use crate::battery::BatteryReader;
use crate::candidates::{candidates, DeviceCandidate};
use crate::config::ConfigStore;
use crate::error::Result;
use crate::platform::hid::WindowsHidBackend;
use crate::platform::shell;
use crate::platform::sync::{NamedMutex, EDITOR_MUTEX_NAME};
use crate::profiles::{
    load_profiles, make_ai_prompt_with, normalize_profile, save_profiles, validate_profile,
    DeviceProfile, MAX_PROFILES,
};
use crate::protocol::Reading;
use native_windows_gui as nwg;
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Icon tiles shown in the settings page, in display order.
const ICON_CHOICES: [(&str, &str); 4] = [
    ("mouse", "鼠标"),
    ("keyboard", "键盘"),
    ("headphone", "耳机"),
    ("gamepad", "手柄"),
];
/// Device list refresh cadence while the pick page is open.
const SCAN_INTERVAL_MS: u32 = 2000;

/// Result of one background HID enumeration.
type ScanOutcome = std::result::Result<Vec<DeviceCandidate>, String>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Method,
    Pick,
    Json,
    Settings,
}

/// Opens the wizard. `key` selects an existing device for editing.
pub fn run(key: Option<String>) -> Result<()> {
    let instance = NamedMutex::create(EDITOR_MUTEX_NAME)?;
    if instance.is_none() {
        shell::focus_window(shell::EDITOR_TITLE);
        return Ok(());
    }
    init_gui()?;
    let store = ConfigStore::from_env();
    let profiles = load_profiles(&store);
    let editing = key
        .as_deref()
        .and_then(|wanted| profiles.iter().position(|item| item.key == wanted))
        .map(|index| profiles[index].clone());
    let mode_edit = editing.is_some();

    let mut window = nwg::Window::default();
    nwg::Window::builder()
        .title(shell::EDITOR_TITLE)
        .size(crate::platform::gui::window_size(780, 640))
        .center(true)
        .flags(nwg::WindowFlags::WINDOW | nwg::WindowFlags::VISIBLE)
        .build(&mut window)?;
    theme::apply(&window);

    let title = label(&window, "")?;
    let hint = label(&window, "")?;
    let status = label(&window, "")?;

    let mut ai = nwg::Button::default();
    nwg::Button::builder()
        .text("AI 分析")
        .parent(&window)
        .build(&mut ai)?;
    let ai_hint = label(&window, "复制提示词交给 AI，拿回协议 JSON 后导入")?;
    let mut import = nwg::Button::default();
    nwg::Button::builder()
        .text("JSON 导入")
        .parent(&window)
        .build(&mut import)?;
    let import_hint = label(&window, "已有协议 JSON，直接粘贴并验证")?;

    let mut devices = nwg::ListBox::default();
    nwg::ListBox::builder()
        .parent(&window)
        .build(&mut devices)?;
    let list_status = label(&window, "")?;

    let mut copy = nwg::Button::default();
    nwg::Button::builder()
        .text("复制提示词")
        .parent(&window)
        .build(&mut copy)?;
    let mut paste = nwg::Button::default();
    nwg::Button::builder()
        .text("粘贴 JSON")
        .parent(&window)
        .build(&mut paste)?;
    let mut test = nwg::Button::default();
    nwg::Button::builder()
        .text("读取并导入")
        .parent(&window)
        .build(&mut test)?;
    let mut json_box = nwg::TextBox::default();
    nwg::TextBox::builder()
        .parent(&window)
        .flags(
            nwg::TextBoxFlags::VISIBLE
                | nwg::TextBoxFlags::VSCROLL
                | nwg::TextBoxFlags::AUTOVSCROLL
                | nwg::TextBoxFlags::HSCROLL
                | nwg::TextBoxFlags::AUTOHSCROLL,
        )
        .build(&mut json_box)?;

    let name_label = label(&window, "设备名")?;
    let mut name = nwg::TextInput::default();
    nwg::TextInput::builder().parent(&window).build(&mut name)?;
    let icon_label = label(&window, "图标")?;
    let mut icons = Vec::with_capacity(ICON_CHOICES.len());
    for (kind, label) in ICON_CHOICES {
        let mut button = nwg::Button::default();
        nwg::Button::builder()
            .text(&theme::icon_text(kind, false, label))
            .parent(&window)
            .build(&mut button)?;
        icons.push(button);
    }
    let mut upload = nwg::Button::default();
    nwg::Button::builder()
        .text("上传本地图标…")
        .parent(&window)
        .build(&mut upload)?;
    let icon_path_label = label(&window, "")?;
    let tray = Toggle::build(&window, "在托盘显示", true)?;
    let summary = label(&window, "")?;

    let mut back = nwg::Button::default();
    nwg::Button::builder()
        .text("上一步")
        .parent(&window)
        .build(&mut back)?;
    let mut next = nwg::Button::default();
    nwg::Button::builder()
        .text("下一步")
        .parent(&window)
        .build(&mut next)?;
    let mut save = nwg::Button::default();
    nwg::Button::builder()
        .text(if mode_edit {
            "保存"
        } else {
            "保存并添加"
        })
        .parent(&window)
        .build(&mut save)?;
    let mut delete = nwg::Button::default();
    nwg::Button::builder()
        .text("删除设备")
        .parent(&window)
        .build(&mut delete)?;
    let mut cancel = nwg::Button::default();
    nwg::Button::builder()
        .text("取消")
        .parent(&window)
        .build(&mut cancel)?;

    let mut notice = nwg::Notice::default();
    nwg::Notice::builder().parent(&window).build(&mut notice)?;
    let mut timer = nwg::Timer::default();
    nwg::Timer::builder()
        .parent(&window)
        .interval(SCAN_INTERVAL_MS)
        .stopped(true)
        .build(&mut timer)?;

    let mut state = EditorState {
        window,
        title,
        hint,
        status,
        ai,
        ai_hint,
        import,
        import_hint,
        devices,
        list_status,
        copy,
        paste,
        test,
        json_box,
        name_label,
        name,
        icon_label,
        icons,
        upload,
        icon_path_label,
        tray,
        summary,
        back,
        next,
        save,
        delete,
        cancel,
        notice,
        timer,
        page: if mode_edit {
            Page::Settings
        } else {
            Page::Method
        },
        editing: mode_edit,
        ai_flow: false,
        profiles,
        candidate: None,
        candidates: Vec::new(),
        labels: Vec::new(),
        known: HashSet::new(),
        fresh: HashSet::new(),
        selected: None,
        seed: None,
        pending: None,
        tested: None,
        key: editing
            .as_ref()
            .map(|item| item.key.clone())
            .unwrap_or_default(),
        uid: editing.as_ref().map(|item| item.uid).unwrap_or(0),
        icon_kind: "device".to_owned(),
        icon_path: String::new(),
        busy: Arc::new(AtomicBool::new(false)),
        scan_busy: Arc::new(AtomicBool::new(false)),
        queue: Arc::new(Mutex::new(VecDeque::new())),
        scan_queue: Arc::new(Mutex::new(VecDeque::new())),
        handlers: Vec::new(),
        closing: false,
    };

    if let Some(profile) = editing {
        state.key = profile.key.clone();
        state.uid = profile.uid;
        state.begin_settings(profile);
    }
    let state = Rc::new(std::cell::RefCell::new(state));
    {
        let weak: Weak<std::cell::RefCell<EditorState>> = Rc::downgrade(&state);
        let handler = nwg::full_bind_event_handler(
            &state.borrow().window.handle,
            move |event, _data, handle| {
                if let Some(state) = weak.upgrade() {
                    if let Ok(mut state) = state.try_borrow_mut() {
                        state.on_event(event, handle);
                    }
                }
            },
        );
        state.borrow_mut().handlers.push(handler);
    }
    state.borrow_mut().show_page();
    nwg::dispatch_thread_events();
    if let Ok(mut state) = state.try_borrow_mut() {
        state.cleanup();
    }
    drop(state);
    drop(instance);
    Ok(())
}

struct EditorState {
    window: nwg::Window,
    title: nwg::Label,
    hint: nwg::Label,
    status: nwg::Label,
    ai: nwg::Button,
    ai_hint: nwg::Label,
    import: nwg::Button,
    import_hint: nwg::Label,
    devices: nwg::ListBox<String>,
    list_status: nwg::Label,
    copy: nwg::Button,
    paste: nwg::Button,
    test: nwg::Button,
    json_box: nwg::TextBox,
    name_label: nwg::Label,
    name: nwg::TextInput,
    icon_label: nwg::Label,
    icons: Vec<nwg::Button>,
    upload: nwg::Button,
    icon_path_label: nwg::Label,
    tray: Toggle,
    summary: nwg::Label,
    back: nwg::Button,
    next: nwg::Button,
    save: nwg::Button,
    delete: nwg::Button,
    cancel: nwg::Button,
    notice: nwg::Notice,
    timer: nwg::Timer,
    page: Page,
    editing: bool,
    ai_flow: bool,
    profiles: Vec<DeviceProfile>,
    candidate: Option<DeviceCandidate>,
    candidates: Vec<DeviceCandidate>,
    labels: Vec<String>,
    known: HashSet<String>,
    fresh: HashSet<String>,
    selected: Option<String>,
    seed: Option<DeviceProfile>,
    pending: Option<DeviceProfile>,
    tested: Option<DeviceProfile>,
    key: String,
    uid: u32,
    icon_kind: String,
    icon_path: String,
    busy: Arc<AtomicBool>,
    scan_busy: Arc<AtomicBool>,
    queue: Arc<Mutex<VecDeque<std::result::Result<Reading, String>>>>,
    scan_queue: Arc<Mutex<VecDeque<ScanOutcome>>>,
    handlers: Vec<nwg::EventHandler>,
    closing: bool,
}

impl EditorState {
    fn on_event(&mut self, event: nwg::Event, handle: nwg::ControlHandle) {
        if handle == self.window.handle && event == nwg::Event::OnWindowClose {
            self.close();
        } else if handle == self.notice.handle && event == nwg::Event::OnNotice {
            self.drain_test();
            self.drain_scan();
        } else if handle == self.timer.handle && event == nwg::Event::OnTimerTick {
            self.request_scan();
        } else if handle == self.devices.handle && event == nwg::Event::OnListBoxSelect {
            self.remember_selection();
        } else if handle == self.cancel.handle && event == nwg::Event::OnButtonClick {
            self.close();
        } else if handle == self.ai.handle && event == nwg::Event::OnButtonClick {
            self.start_ai();
        } else if handle == self.import.handle && event == nwg::Event::OnButtonClick {
            self.start_json_import();
        } else if handle == self.next.handle && event == nwg::Event::OnButtonClick {
            self.go_json();
        } else if handle == self.back.handle && event == nwg::Event::OnButtonClick {
            self.go_back();
        } else if handle == self.copy.handle && event == nwg::Event::OnButtonClick {
            self.copy_prompt();
        } else if handle == self.paste.handle && event == nwg::Event::OnButtonClick {
            self.paste_json();
        } else if handle == self.test.handle && event == nwg::Event::OnButtonClick {
            self.start_test();
        } else if handle == self.upload.handle && event == nwg::Event::OnButtonClick {
            self.choose_icon();
        } else if handle == self.save.handle && event == nwg::Event::OnButtonClick {
            self.save_device();
        } else if handle == self.delete.handle && event == nwg::Event::OnButtonClick {
            self.delete_device();
        } else {
            for (index, (kind, _)) in ICON_CHOICES.iter().enumerate() {
                if self.icons.get(index).map(|item| item.handle) == Some(handle)
                    && event == nwg::Event::OnButtonClick
                {
                    self.select_icon(kind);
                    return;
                }
            }
        }
    }

    // -- pages ------------------------------------------------------------

    fn show_page(&mut self) {
        let page = self.page;
        let method = page == Page::Method;
        let pick = page == Page::Pick;
        let json = page == Page::Json;
        let settings = page == Page::Settings;

        self.ai.set_visible(method);
        self.ai_hint.set_visible(method);
        self.import.set_visible(method);
        self.import_hint.set_visible(method);

        self.devices.set_visible(pick);
        self.list_status.set_visible(pick);

        self.copy.set_visible(json && self.ai_flow);
        self.paste.set_visible(json);
        self.test.set_visible(json);
        self.json_box.set_visible(json);

        self.name_label.set_visible(settings);
        self.name.set_visible(settings);
        self.icon_label.set_visible(settings);
        for icon in &self.icons {
            icon.set_visible(settings);
        }
        self.upload.set_visible(settings);
        self.icon_path_label.set_visible(settings);
        self.tray.button.set_visible(settings);
        self.summary.set_visible(settings);

        self.next.set_visible(pick);
        self.back.set_visible(pick || json);
        self.save.set_visible(settings);
        self.delete.set_visible(settings && self.editing);
        self.cancel.set_visible(true);

        if pick {
            // Enumeration now runs on a worker; the list fills in as soon as
            // the Notice arrives, so the window stays responsive meanwhile.
            self.request_scan();
            self.timer.start();
        } else {
            self.timer.stop();
        }
        // Lay out first: setting the text before the controls have their final
        // geometry leaves a ghost of the glyphs where they were painted last.
        self.apply_layout();
        if pick && self.labels.is_empty() {
            self.set_label(&self.list_status, "正在扫描设备…");
        }

        let title = match page {
            Page::Method => "添加设备",
            Page::Pick => "选择要分析的设备",
            Page::Json if self.ai_flow => "提示词与协议 JSON",
            Page::Json => "粘贴协议 JSON",
            Page::Settings => "设备设置",
        };
        self.set_label(&self.title, title);
        let hint = match page {
            Page::Method => "先选择一种导入方式，两种方式都会验证设备是否真的能读出电量。",
            Page::Pick => "列表每 2 秒自动刷新：可通过拔插设备确认目标设备，刚接入的会标记出来。",
            Page::Json if self.ai_flow => {
                "把提示词交给 AI，拿回 JSON 粘贴到下面；只有成功读到电量才会进入设置页。"
            }
            Page::Json => "粘贴已有的协议 JSON，只有成功读到电量才会进入设置页。",
            Page::Settings => "设置只影响显示方式，协议已在导入时验证通过。",
        };
        self.set_label(&self.hint, hint);
    }

    fn apply_layout(&self) {
        let (width, height) = self.window.size();
        let scale = crate::platform::gui::ui_scale();
        let width = (width as f64 / scale).round() as u32;
        let height = (height as f64 / scale).round() as u32;
        let width = width.max(680);
        let height = height.max(560);
        let pad = 22_u32;
        let content_w = width.saturating_sub(pad * 2);

        crate::place!(self.title, pad as i32, 16, content_w, 30);
        crate::place!(self.hint, pad as i32, 50, content_w, 22);

        let action_y = height.saturating_sub(96) as i32;
        crate::place!(
            self.status,
            pad as i32,
            height.saturating_sub(44) as i32,
            content_w.saturating_sub(120) as i32,
            26
        );
        crate::place!(self.cancel, (width - pad - 88) as i32, action_y, 88, 34);

        match self.page {
            Page::Method => {
                let half = (content_w.saturating_sub(24)) / 2;
                crate::place!(self.ai, pad as i32, 110, half, 76);
                crate::place!(self.ai_hint, pad as i32, 194, half, 22);
                crate::place!(self.import, (pad + half + 24) as i32, 110, half, 76);
                crate::place!(self.import_hint, (pad + half + 24) as i32, 194, half, 22);
            }
            Page::Pick => {
                crate::place!(
                    self.devices,
                    pad as i32,
                    86,
                    content_w,
                    (action_y - 86 - 40) as u32
                );
                crate::place!(self.list_status, pad as i32, action_y - 32, content_w, 22);
                crate::place!(self.back, pad as i32, action_y, 88, 34);
                crate::place!(self.next, (pad + 96) as i32, action_y, 110, 34);
            }
            Page::Json => {
                crate::place!(self.copy, pad as i32, 86, 118, 34);
                crate::place!(self.paste, (pad + 126) as i32, 86, 110, 34);
                crate::place!(self.test, (pad + 244) as i32, 86, 128, 34);
                crate::place!(
                    self.json_box,
                    pad as i32,
                    132,
                    content_w,
                    (action_y - 132 - 12) as u32
                );
                crate::place!(self.back, pad as i32, action_y, 88, 34);
            }
            Page::Settings => {
                crate::place!(self.name_label, pad as i32, 92, 74, 26);
                crate::place!(
                    self.name,
                    (pad + 82) as i32,
                    90,
                    content_w.saturating_sub(82),
                    32
                );
                crate::place!(self.icon_label, pad as i32, 138, content_w, 22);
                let tile = 104_u32;
                for (index, icon) in self.icons.iter().enumerate() {
                    crate::place!(
                        icon,
                        (pad + index as u32 * (tile + 12)) as i32,
                        166,
                        tile,
                        88
                    );
                }
                crate::place!(self.upload, pad as i32, 266, 132, 32);
                crate::place!(
                    self.icon_path_label,
                    (pad + 144) as i32,
                    270,
                    content_w.saturating_sub(144),
                    24
                );
                crate::place!(self.tray.button, pad as i32, 310, 148, 28);
                crate::place!(self.summary, pad as i32, 348, content_w, 24);
                // Keep clear of the cancel button, which owns the right edge.
                crate::place!(
                    self.save,
                    (width - pad - 88 - 12 - 120) as i32,
                    action_y,
                    120,
                    34
                );
                crate::place!(self.delete, pad as i32, action_y, 104, 34);
            }
        }
        theme::polish(&self.window);
    }

    // -- navigation -------------------------------------------------------

    fn start_ai(&mut self) {
        self.ai_flow = true;
        self.page = Page::Pick;
        self.candidate = None;
        self.seed = None;
        self.selected = None;
        self.known.clear();
        self.fresh.clear();
        self.set_status("");
        self.show_page();
    }

    fn start_json_import(&mut self) {
        self.ai_flow = false;
        self.page = Page::Json;
        self.candidate = None;
        self.seed = None;
        self.set_status("把协议 JSON 粘贴到下面，然后点“读取并导入”。");
        self.show_page();
    }

    fn go_json(&mut self) {
        let Some(candidate) = self.candidate.clone() else {
            self.set_status("请先在列表中选择目标设备。");
            return;
        };
        self.ensure_identity(&candidate.name);
        self.seed = Some(candidate.seed_profile(&self.key, self.uid));
        self.page = Page::Json;
        self.set_status("点“复制提示词”，把它连同设备信息一起交给 AI。");
        self.show_page();
    }

    fn go_back(&mut self) {
        self.page = match self.page {
            Page::Json if self.editing => Page::Method,
            Page::Json if self.candidate.is_some() => Page::Pick,
            Page::Json => Page::Method,
            Page::Pick => Page::Method,
            _ => Page::Method,
        };
        self.set_status("");
        self.show_page();
    }

    fn begin_settings(&mut self, profile: DeviceProfile) {
        self.name.set_text(&profile.name);
        self.icon_kind = if profile.icon.is_empty() {
            "device".to_owned()
        } else {
            profile.icon.clone()
        };
        self.icon_path = profile.icon_path.clone();
        self.tray.set(profile.show_in_tray);
        let summary = format!(
            "协议 {} · {:04X}:{:04X} · MI {}",
            profile.protocol,
            profile.match_.vid,
            profile.match_.pid,
            profile.match_.interface_number
        );
        self.set_label(&self.summary, &summary);
        self.pending = Some(profile);
        self.page = Page::Settings;
        self.refresh_icons();
        self.show_page();
    }

    // -- device list ------------------------------------------------------

    /// Schedules a HID enumeration on a worker thread.
    ///
    /// `candidates()` opens every interface to read its strings, which used
    /// to block the UI thread for ~90 ms on this machine (longer with slow
    /// devices). Keeping one scan in flight also prevents overlapping scans
    /// when the 2-second timer fires while a worker is still busy.
    fn request_scan(&mut self) {
        if self.page != Page::Pick || self.closing || self.scan_busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let queue = Arc::clone(&self.scan_queue);
        let busy = Arc::clone(&self.scan_busy);
        let sender = self.notice.sender();
        let started = super::task::spawn(busy, sender, move || {
            let result = candidates().map_err(|error| error.to_string());
            if let Ok(mut queue) = queue.lock() {
                queue.push_back(result);
            }
        });
        if !started {
            self.set_label(&self.list_status, "无法启动后台设备扫描。");
        }
    }

    /// Applies the most recent scan result on the UI thread.
    fn drain_scan(&mut self) {
        let results = self
            .scan_queue
            .lock()
            .map(|mut queue| queue.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();
        let Some(latest) = results.into_iter().last() else {
            return;
        };
        if self.page != Page::Pick {
            return;
        }
        match latest {
            Ok(found) => self.apply_candidates(found),
            Err(error) => {
                self.set_label(&self.list_status, &format!("枚举 HID 设备失败：{error}"));
            }
        }
    }

    fn apply_candidates(&mut self, found: Vec<DeviceCandidate>) {
        let first = self.known.is_empty();
        let ids: HashSet<String> = found.iter().map(|item| item.id.clone()).collect();
        self.fresh = if first {
            HashSet::new()
        } else {
            ids.difference(&self.known).cloned().collect()
        };
        // Keep only the previous round's snapshot: a device that was unplugged
        // and plugged back in must be reported as newly attached again.
        self.known = ids;
        let labels: Vec<String> = found
            .iter()
            .map(|item| item.label(self.fresh.contains(&item.id)))
            .collect();
        self.candidates = found;
        if labels != self.labels {
            self.labels = labels.clone();
            self.devices.set_collection(labels);
            if let Some(selected) = self.selected.clone() {
                if let Some(index) = self.candidates.iter().position(|item| item.id == selected) {
                    self.devices.set_selection(Some(index));
                } else {
                    self.selected = None;
                }
            }
        }
        self.next.set_enabled(self.selected.is_some());
        let fresh = self.fresh.len();
        let mut text = format!("已发现 {} 台设备", self.candidates.len());
        if fresh > 0 {
            text.push_str(&format!("，其中 {fresh} 台是刚接入的"));
        }
        text.push_str(" · 插拔设备可刷新此列表");
        self.set_label(&self.list_status, &text);
    }

    fn remember_selection(&mut self) {
        let Some(index) = self.devices.selection() else {
            return;
        };
        self.selected = self.candidates.get(index).map(|item| item.id.clone());
        self.candidate = self.candidates.get(index).cloned();
        self.next.set_enabled(self.selected.is_some());
    }

    // -- prompt / json ----------------------------------------------------

    fn copy_prompt(&mut self) {
        let Some(candidate) = self.candidate.clone() else {
            self.set_status("请先选择目标设备。");
            return;
        };
        self.ensure_identity(&candidate.name);
        let seed = candidate.seed_profile(&self.key, self.uid);
        let prompt = make_ai_prompt_with(&seed, &candidate.evidence());
        nwg::Clipboard::set_data_text(&self.window, &prompt);
        self.set_status("提示词已复制：含设备名、VID/PID、序列号与全部接口信息。");
    }

    fn paste_json(&mut self) {
        let Some(text) = nwg::Clipboard::data_text(&self.window) else {
            self.set_status("剪贴板为空，请先复制 AI 返回的 JSON。");
            return;
        };
        let start = text.find('{');
        let end = text.rfind('}');
        let (Some(start), Some(end)) = (start, end) else {
            self.set_status("剪贴板里没有找到 JSON 对象。");
            return;
        };
        if end <= start {
            self.set_status("剪贴板里没有找到 JSON 对象。");
            return;
        }
        let candidate = &text[start..=end];
        if serde_json::from_str::<Value>(candidate).is_err() {
            self.set_status("剪贴板里的 JSON 无法解析，请检查是否被截断。");
            return;
        }
        self.json_box.set_text(candidate);
        self.set_status("已粘贴 JSON，点“读取并导入”开始验证。");
    }

    /// Merges the pasted protocol into the selected device and validates it.
    fn build_profile(&mut self) -> std::result::Result<DeviceProfile, String> {
        let text = self.json_box.text();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err("请先粘贴协议 JSON。".to_owned());
        }
        let name = json_name(trimmed).unwrap_or_default();
        self.ensure_identity(&name);
        let seed = self
            .seed
            .clone()
            .unwrap_or_else(|| crate::profiles::custom_profile(&self.key, "新设备", self.uid));
        let mut profile = merge_protocol(&seed, trimmed)?;
        if !name.is_empty() {
            profile.name = name;
        }
        profile.key = self.key.clone();
        profile.uid = self.uid;
        profile.enabled = true;
        profile.show_in_tray = self.tray.get();
        let errors = validate_profile(&profile);
        if !errors.is_empty() {
            return Err(errors.join("\n"));
        }
        Ok(profile)
    }

    fn start_test(&mut self) {
        let profile = match self.build_profile() {
            Ok(profile) => profile,
            Err(error) => {
                self.set_status(&error);
                return;
            }
        };
        if self.busy.swap(true, Ordering::AcqRel) {
            return;
        }
        self.tested = Some(profile.clone());
        self.test.set_enabled(false);
        self.set_status("正在读取设备电量…");
        let queue = Arc::clone(&self.queue);
        let busy = Arc::clone(&self.busy);
        let sender = self.notice.sender();
        let store = ConfigStore::from_env();
        let started = super::task::spawn(busy, sender, move || {
            let mut reader = BatteryReader::new(WindowsHidBackend::new(), store);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                reader.read_profile(&profile)
            }))
            .map_err(|_| "读取过程发生内部错误".to_owned());
            if let Ok(mut queue) = queue.lock() {
                queue.push_back(result);
            }
        });
        if !started {
            self.test.set_enabled(true);
            self.set_status("无法启动后台读取任务，请稍后重试。");
        }
    }

    fn drain_test(&mut self) {
        self.test.set_enabled(true);
        let results = self
            .queue
            .lock()
            .map(|mut queue| queue.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();
        for result in results {
            match result {
                Ok(reading) => match reading.percent {
                    Some(percent) => {
                        let Some(profile) = self.tested.take() else {
                            return;
                        };
                        self.set_status(&format!(
                            "读取成功：{} {}%，请设置显示方式后保存。",
                            reading.name, percent
                        ));
                        self.begin_settings(profile);
                    }
                    None => {
                        let message = if reading.message.is_empty() {
                            status_text(reading.status).to_owned()
                        } else {
                            reading.message.clone()
                        };
                        self.set_status(&format!("没有读到电量：{message}"));
                    }
                },
                Err(error) => self.set_status(&format!("读取失败：{error}")),
            }
        }
    }

    // -- settings ---------------------------------------------------------

    fn select_icon(&mut self, kind: &str) {
        self.icon_kind = kind.to_owned();
        if kind != "custom" {
            self.icon_path.clear();
        }
        self.refresh_icons();
    }

    fn refresh_icons(&mut self) {
        for (index, (kind, label)) in ICON_CHOICES.iter().enumerate() {
            if let Some(button) = self.icons.get(index) {
                button.set_text(&theme::icon_text(kind, self.icon_kind == *kind, label));
                theme::repaint(button);
            }
        }
        let hint = if self.icon_path.is_empty() {
            if self.icon_kind == "custom" {
                "尚未选择图片"
            } else {
                "使用内置图标；也可以上传 PNG/ICO/BMP"
            }
        } else {
            &self.icon_path
        };
        self.set_label(&self.icon_path_label, hint);
    }

    fn choose_icon(&mut self) {
        let mut dialog = nwg::FileDialog::default();
        if nwg::FileDialog::builder()
            .title("选择设备图标")
            .action(nwg::FileDialogAction::Open)
            .filters("支持的图片(*.png;*.ico;*.bmp)|PNG(*.png)|ICO(*.ico)|BMP(*.bmp)|所有文件(*.*)")
            .build(&mut dialog)
            .is_err()
        {
            self.set_status("无法打开文件选择器。");
            return;
        }
        if !dialog.run(Some(&self.window)) {
            return;
        }
        let Ok(path) = dialog.get_selected_item() else {
            self.set_status("读取所选文件失败。");
            return;
        };
        let path = std::path::PathBuf::from(path);
        match crate::icon::validate_image_file(&path) {
            Ok(()) => {
                self.icon_path = path.to_string_lossy().to_string();
                self.icon_kind = "custom".to_owned();
                self.refresh_icons();
                self.set_status("已选择自定义图标。");
            }
            Err(error) => {
                self.set_status(&format!("图标无法读取：{error}"));
                nwg::modal_error_message(&self.window, "图标无法读取", &error.to_string());
            }
        }
    }

    fn save_device(&mut self) {
        let Some(mut profile) = self.pending.clone() else {
            self.set_status("没有待保存的设备。");
            return;
        };
        let name = self.name.text();
        let name = name.trim();
        if name.is_empty() {
            self.set_status("请先填写设备名。");
            return;
        }
        profile.name = name.to_owned();
        profile.icon = self.icon_kind.clone();
        profile.icon_path = self.icon_path.clone();
        profile.show_in_tray = self.tray.get();
        profile.enabled = true;

        let store = ConfigStore::from_env();
        let mut profiles = load_profiles(&store);
        if profiles.len() >= MAX_PROFILES && !profiles.iter().any(|item| item.key == profile.key) {
            self.set_status(&format!("最多支持 {MAX_PROFILES} 台设备。"));
            return;
        }
        match profiles.iter().position(|item| item.key == profile.key) {
            Some(index) => profiles[index] = profile,
            None => profiles.push(profile),
        }
        match save_profiles(&store, &profiles) {
            Ok(saved) => {
                self.profiles = saved;
                self.close();
            }
            Err(error) => self.set_status(&format!("保存失败：{error}")),
        }
    }

    fn delete_device(&mut self) {
        let Some(profile) = self.pending.clone() else {
            return;
        };
        let params = nwg::MessageParams {
            title: "删除设备",
            content: &format!("确定删除“{}”吗？", profile.name),
            buttons: nwg::MessageButtons::YesNo,
            icons: nwg::MessageIcons::Question,
        };
        if nwg::modal_message(&self.window, &params) != nwg::MessageChoice::Yes {
            return;
        }
        let store = ConfigStore::from_env();
        let mut profiles = load_profiles(&store);
        if profiles.len() <= 1 {
            self.set_status("至少需要保留一台设备。");
            return;
        }
        profiles.retain(|item| item.key != profile.key);
        match save_profiles(&store, &profiles) {
            Ok(saved) => {
                self.profiles = saved;
                self.close();
            }
            Err(error) => self.set_status(&format!("删除失败：{error}")),
        }
    }

    // -- helpers ----------------------------------------------------------

    /// Assigns a unique key and tray UID once, when the device becomes known.
    fn ensure_identity(&mut self, preferred_name: &str) {
        if !self.key.is_empty() && self.uid > 0 {
            return;
        }
        let base = if preferred_name.trim().is_empty() {
            "device".to_owned()
        } else {
            slug(preferred_name)
        };
        let mut key = base.clone();
        let mut index = 2;
        while self.profiles.iter().any(|item| item.key == key) {
            key = format!("{base}_{index}");
            index += 1;
        }
        self.key = key;
        self.uid = self
            .profiles
            .iter()
            .map(|item| item.uid)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1);
    }

    fn set_status(&self, text: &str) {
        self.set_label(&self.status, text);
    }

    /// Updates a label and repaints it, so no old glyphs stay behind.
    fn set_label(&self, label: &nwg::Label, text: &str) {
        label.set_text(text);
        theme::refresh_label(label);
    }

    fn close(&mut self) {
        if self.closing {
            return;
        }
        self.closing = true;
        self.timer.stop();
        nwg::stop_thread_dispatch();
    }

    fn cleanup(&mut self) {
        self.timer.stop();
        for handler in self.handlers.drain(..) {
            nwg::unbind_event_handler(&handler);
        }
    }
}

impl Drop for EditorState {
    fn drop(&mut self) {
        if !self.closing {
            self.cleanup();
        }
    }
}

/// Derives a stable key from the device name, mirroring the CLI rule.
fn slug(name: &str) -> String {
    let mut key = String::new();
    let mut separator = false;
    for ch in name.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            key.push(ch);
            separator = false;
        } else if !key.is_empty() && !separator {
            key.push('_');
            separator = true;
        }
    }
    let key = key.trim_matches('_').to_owned();
    if key.is_empty() {
        "device".to_owned()
    } else {
        key
    }
}

/// Creates a dark label.
///
/// NWG's label centres its text by shrinking the client area and painting the
/// leftover band with `COLOR_WINDOW`; `theme::style_label` repaints that band in
/// the window tone so no white hairline shows through.
fn label(parent: &nwg::Window, text: &str) -> Result<nwg::Label> {
    let mut label = nwg::Label::default();
    nwg::Label::builder()
        .text(text)
        .parent(parent)
        .build(&mut label)?;
    theme::style_label(&label);
    Ok(label)
}

/// Device name declared by the pasted protocol, if any.
fn json_name(text: &str) -> Option<String> {
    let data: Value = serde_json::from_str(text).ok()?;
    let name = data.get("name")?.as_str()?.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// Merges the protocol part of `json` into the device we already know.
///
/// Everything the wizard discovered from the hardware (key, UID, matched VID/PID
/// and interface, icon, tray flag) is kept; only `match`, `request` and
/// `response` come from the AI answer, so a model that hallucinates a different
/// vendor cannot redirect the query to another device.
fn merge_protocol(seed: &DeviceProfile, json: &str) -> std::result::Result<DeviceProfile, String> {
    let data: Value = serde_json::from_str(json).map_err(|error| {
        format!(
            "JSON 格式错误：第 {} 行第 {} 列",
            error.line(),
            error.column()
        )
    })?;
    let object = data
        .as_object()
        .ok_or_else(|| "JSON 顶层必须是对象".to_owned())?;
    let mut base = seed.to_value().map_err(|error| error.to_string())?;
    let Some(base_object) = base.as_object_mut() else {
        return Err("设备结构序列化失败".to_owned());
    };
    for field in ["match", "request", "response"] {
        if let Some(value) = object.get(field) {
            if !value.is_object() {
                return Err(format!("{field} 必须是对象"));
            }
            base_object.insert(field.to_owned(), value.clone());
        }
    }
    Ok(normalize_profile(&base, Some(&seed.key), Some(seed.uid)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_json() -> String {
        serde_json::json!({
            "name": "瓦尔基里 VK M5",
            "match": {"vid": 14154, "pid": 41762, "usage_page": 65535, "usage": 2},
            "request": {"bytes_hex": "00 F7", "timeout_ms": 1000},
            "response": {"battery_offset": 3}
        })
        .to_string()
    }

    #[test]
    fn merge_keeps_device_identity_and_protocol() {
        let seed = crate::profiles::custom_profile("vk_m5", "待定设备", 7);
        let merged = merge_protocol(&seed, &sample_json());
        assert!(merged.is_ok(), "merge failed: {merged:?}");
        let Some(profile) = merged.ok() else { return };
        assert_eq!(profile.key, "vk_m5");
        assert_eq!(profile.uid, 7);
        assert_eq!(profile.match_.vid, 14154);
        assert_eq!(profile.match_.pid, 41762);
        assert!(crate::profiles::validate_profile(&profile).is_empty());
    }

    #[test]
    fn merge_rejects_malformed_json() {
        let seed = crate::profiles::custom_profile("a", "b", 1);
        assert!(merge_protocol(&seed, "{").is_err());
        assert!(merge_protocol(&seed, "[1,2]").is_err());
        assert!(merge_protocol(&seed, r#"{"match": 5}"#).is_err());
    }

    #[test]
    fn adding_a_device_lands_in_the_configuration() {
        let temporary = tempfile::tempdir();
        assert!(temporary.is_ok());
        let Some(store) = temporary
            .ok()
            .map(|dir| ConfigStore::new(dir.path().join("BatteryTray")))
        else {
            return;
        };
        let seed = crate::profiles::custom_profile("vk_m5", "瓦尔基里 VK M5", 9);
        let Ok(mut profile) = merge_protocol(&seed, &sample_json()) else {
            return;
        };
        profile.name = "瓦尔基里 VK M5".to_owned();
        profile.enabled = true;
        profile.show_in_tray = true;

        let mut profiles = load_profiles(&store);
        let before = profiles.len();
        profiles.push(profile);
        let saved = save_profiles(&store, &profiles);
        assert!(saved.is_ok(), "save failed: {saved:?}");
        assert_eq!(load_profiles(&store).len(), before + 1);
    }
}
