#![allow(deprecated)]

use crate::battery::{BatteryReader, HidInterface, SharedMouseCache};
use crate::config::{ConfigStore, INTERVAL_CHOICES};
use crate::error::{BatteryTrayError, Result};
use crate::icon::{custom_icon_rgba, device_icon_rgba, icon_bytes};
use crate::platform::hid::WindowsHidBackend;
use crate::platform::shell::{self, AutostartState};
use crate::platform::sync::{NamedMutex, TRAY_MUTEX_NAME};
use crate::policy::FailurePolicy;
use crate::profiles::{
    enabled_profiles, load_profiles, tray_profiles, DeviceProfile, MAX_PROFILES,
};
use crate::protocol::{Reading, ReadingStatus};
use crate::refresh::compose_tooltip;
use crate::snapshot;
use native_windows_gui as nwg;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::process::Command;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Starts the native notification-area process.
pub fn run() -> Result<()> {
    let instance = NamedMutex::create(TRAY_MUTEX_NAME)
        .map_err(|error| BatteryTrayError::Shell(format!("创建托盘单实例锁失败：{error}")))?;
    if instance.is_none() {
        launch("--panel");
        return Ok(());
    }
    // Keep tray and windows on the same DPI awareness path.
    crate::platform::gui::enable_dpi_awareness();
    nwg::init().map_err(|error| BatteryTrayError::Gui(error.to_string()))?;
    if let Err(error) = nwg::Font::set_global_family("Microsoft YaHei UI") {
        eprintln!("设置默认字体失败：{error}");
    }
    let queue = Arc::new(Mutex::new(VecDeque::new()));
    let busy = Arc::new(AtomicBool::new(false));
    let interfaces = Arc::new(Mutex::new(Vec::new()));
    let dirty = Arc::new(AtomicBool::new(true));
    let mut root = nwg::Window::default();
    nwg::Window::builder()
        .title("电量托盘")
        .size((1, 1))
        .position((-32000, -32000))
        .flags(nwg::WindowFlags::POPUP)
        .build(&mut root)
        .map_err(gui_error)?;
    let mut notice = nwg::Notice::default();
    nwg::Notice::builder()
        .parent(&root)
        .build(&mut notice)
        .map_err(gui_error)?;
    let mut refresh_timer = nwg::Timer::default();
    nwg::Timer::builder()
        .parent(&root)
        .interval(60_000)
        .stopped(false)
        .build(&mut refresh_timer)
        .map_err(gui_error)?;
    let mut config_timer = nwg::Timer::default();
    nwg::Timer::builder()
        .parent(&root)
        .interval(3000)
        .stopped(false)
        .build(&mut config_timer)
        .map_err(gui_error)?;
    let mut debounce_timer = nwg::Timer::default();
    nwg::Timer::builder()
        .parent(&root)
        .interval(600)
        .stopped(true)
        .build(&mut debounce_timer)
        .map_err(gui_error)?;
    // The panel posts a small marker file when the user asks for a refresh.
    // Polling just one stat is far cheaper than opening HID from a second
    // process, and keeps the tray the single owner of device handles.
    let mut request_timer = nwg::Timer::default();
    nwg::Timer::builder()
        .parent(&root)
        .interval(500)
        .stopped(false)
        .build(&mut request_timer)
        .map_err(gui_error)?;
    let mut state = TrayState {
        store: ConfigStore::from_env(),
        root,
        notice,
        refresh_timer,
        config_timer,
        debounce_timer,
        request_timer,
        slots: Vec::new(),
        handlers: Vec::new(),
        raw_handler: None,
        policy: FailurePolicy::default(),
        readings: HashMap::new(),
        interval: 60,
        autostart: AutostartState::Disabled,
        queue,
        busy,
        interfaces,
        interfaces_dirty: dirty,
        mouse_cache: SharedMouseCache::default(),
        queued_manual: false,
        config_signature: Vec::new(),
        exit_requested: false,
        worker_count: 0,
        sequence: 0,
        last_request: None,
        weak: Weak::new(),
    };
    state.interval = state.load_interval();
    state.apply_interval();
    let state = Rc::new(RefCell::new(state));
    state.borrow_mut().weak = Rc::downgrade(&state);
    {
        let mut state_ref = state.borrow_mut();
        let weak_root = Rc::downgrade(&state);
        let root_handler =
            nwg::full_bind_event_handler(&state_ref.root.handle, move |event, data, handle| {
                if let Some(state) = weak_root.upgrade() {
                    if let Ok(mut state) = state.try_borrow_mut() {
                        state.on_event(event, data, handle);
                    }
                }
            });
        state_ref.handlers.push(root_handler);
        let weak_raw = Rc::downgrade(&state);
        // SAFETY: NWG owns the subclass lifetime and unbinds it through the
        // returned handle. The callback only touches Windows messages and the
        // single-threaded UI state behind RefCell.
        let raw = nwg::bind_raw_event_handler(
            &state_ref.root.handle,
            0x10001,
            move |_hwnd, msg, wparam, _lparam| {
                let Some(state) = weak_raw.upgrade() else {
                    return Some(0);
                };
                let Ok(mut state) = state.try_borrow_mut() else {
                    return None;
                };
                match msg {
                    0x0219 if matches!(wparam as u32, 0x0007 | 0x8000 | 0x8004) => {
                        state.policy.reset();
                        state.interfaces_dirty.store(true, Ordering::Release);
                        state.debounce_refresh(600);
                        Some(0)
                    }
                    0x0218 if matches!(wparam as u32, 0x0007 | 0x0012) => {
                        state.interfaces_dirty.store(true, Ordering::Release);
                        state.debounce_refresh(1200);
                        Some(0)
                    }
                    _ => None,
                }
            },
        )
        .map_err(gui_error)?;
        state_ref.raw_handler = Some(raw);
        state_ref.sync_configuration();
        state_ref.request_refresh(true);
    }
    nwg::dispatch_thread_events();
    if let Ok(mut state) = state.try_borrow_mut() {
        state.cleanup();
    }
    drop(state);
    if let Some(instance) = instance {
        drop(instance);
    }
    Ok(())
}

fn gui_error(error: nwg::NwgError) -> BatteryTrayError {
    BatteryTrayError::Gui(error.to_string())
}

fn launch(argument: &str) {
    match std::env::current_exe() {
        Ok(executable) => {
            if let Err(error) = Command::new(executable).arg(argument).spawn() {
                eprintln!("启动 {argument} 失败：{error}");
            }
        }
        Err(error) => eprintln!("获取程序路径失败：{error}"),
    }
}

struct TraySlot {
    window: nwg::Window,
    tray: nwg::TrayNotification,
    icon: nwg::Icon,
    menu: nwg::Menu,
    open: nwg::MenuItem,
    refresh: nwg::MenuItem,
    autostart: nwg::MenuItem,
    _interval_menu: nwg::Menu,
    intervals: Vec<(u32, nwg::MenuItem)>,
    exit: nwg::MenuItem,
    _separators: Vec<nwg::MenuSeparator>,
    assigned_key: Option<String>,
    profile: Option<DeviceProfile>,
    shown: bool,
    last_visual: Option<(Option<u8>, bool, String, String, bool)>,
}

impl TraySlot {
    fn create(index: usize) -> Result<Self> {
        let mut window = nwg::Window::default();
        nwg::Window::builder()
            .title("电量托盘 · 图标槽")
            .size((1, 1))
            .position((-32000, -32000 - index as i32))
            .flags(nwg::WindowFlags::POPUP)
            .build(&mut window)
            .map_err(gui_error)?;
        let image = device_icon_rgba("device", None, 32, false, taskbar_is_light());
        let bytes = icon_bytes(&image)?;
        let mut icon = nwg::Icon::default();
        nwg::Icon::builder()
            .source_bin(Some(&bytes))
            .strict(true)
            .size(Some((32, 32)))
            .build(&mut icon)
            .map_err(gui_error)?;
        let mut tray = nwg::TrayNotification::default();
        nwg::TrayNotification::builder()
            .parent(&window)
            .icon(Some(&icon))
            .tip(Some("电量托盘"))
            .visible(false)
            .build(&mut tray)
            .map_err(gui_error)?;
        let mut menu = nwg::Menu::default();
        nwg::Menu::builder()
            .popup(true)
            .parent(&window)
            .build(&mut menu)
            .map_err(gui_error)?;
        let mut open = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .text("打开面板")
            .parent(&menu)
            .build(&mut open)
            .map_err(gui_error)?;
        let mut separator1 = nwg::MenuSeparator::default();
        nwg::MenuSeparator::builder()
            .parent(&menu)
            .build(&mut separator1)
            .map_err(gui_error)?;
        let mut refresh = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .text("立即刷新")
            .parent(&menu)
            .build(&mut refresh)
            .map_err(gui_error)?;
        let mut autostart = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .text("开机自启")
            .parent(&menu)
            .check(false)
            .build(&mut autostart)
            .map_err(gui_error)?;
        let mut interval_menu = nwg::Menu::default();
        nwg::Menu::builder()
            .text("刷新间隔")
            .parent(&menu)
            .build(&mut interval_menu)
            .map_err(gui_error)?;
        let mut intervals = Vec::new();
        for (seconds, label) in INTERVAL_CHOICES {
            let mut item = nwg::MenuItem::default();
            nwg::MenuItem::builder()
                .text(label)
                .parent(&interval_menu)
                .build(&mut item)
                .map_err(gui_error)?;
            intervals.push((*seconds, item));
        }
        let mut separator2 = nwg::MenuSeparator::default();
        nwg::MenuSeparator::builder()
            .parent(&menu)
            .build(&mut separator2)
            .map_err(gui_error)?;
        let mut exit = nwg::MenuItem::default();
        nwg::MenuItem::builder()
            .text("退出")
            .parent(&menu)
            .build(&mut exit)
            .map_err(gui_error)?;
        Ok(Self {
            window,
            tray,
            icon,
            menu,
            open,
            refresh,
            autostart,
            _interval_menu: interval_menu,
            intervals,
            exit,
            _separators: vec![separator1, separator2],
            assigned_key: None,
            profile: None,
            shown: false,
            last_visual: None,
        })
    }

    fn hide(&mut self) {
        if self.shown {
            self.tray.set_visibility(false);
            self.shown = false;
        }
        self.assigned_key = None;
        self.profile = None;
        self.last_visual = None;
    }
}

#[derive(Debug, Clone)]
struct WorkerResult {
    readings: Vec<Reading>,
    manual: bool,
}

struct TrayState {
    store: ConfigStore,
    root: nwg::Window,
    notice: nwg::Notice,
    refresh_timer: nwg::Timer,
    config_timer: nwg::Timer,
    debounce_timer: nwg::Timer,
    request_timer: nwg::Timer,
    slots: Vec<TraySlot>,
    handlers: Vec<nwg::EventHandler>,
    raw_handler: Option<nwg::RawEventHandler>,
    policy: FailurePolicy,
    readings: HashMap<String, Reading>,
    interval: u32,
    autostart: AutostartState,
    queue: Arc<Mutex<VecDeque<WorkerResult>>>,
    busy: Arc<AtomicBool>,
    interfaces: Arc<Mutex<Vec<HidInterface>>>,
    interfaces_dirty: Arc<AtomicBool>,
    mouse_cache: SharedMouseCache,
    queued_manual: bool,
    config_signature: Vec<DeviceProfile>,
    exit_requested: bool,
    worker_count: usize,
    sequence: u64,
    last_request: Option<u128>,
    weak: Weak<RefCell<TrayState>>,
}

impl TrayState {
    fn load_interval(&self) -> u32 {
        self.store
            .load()
            .get("interval")
            .and_then(|value| value.as_u64())
            .map(|value| value.clamp(30, 1800) as u32)
            .unwrap_or(60)
    }

    fn apply_interval(&self) {
        self.refresh_timer
            .set_interval(self.interval.saturating_mul(1000));
        self.refresh_timer.start();
    }

    fn on_event(&mut self, event: nwg::Event, _data: nwg::EventData, handle: nwg::ControlHandle) {
        if handle == self.notice.handle && event == nwg::Event::OnNotice {
            self.drain_results();
            return;
        }
        if handle == self.refresh_timer.handle && event == nwg::Event::OnTimerTick {
            self.request_refresh(false);
            return;
        }
        if handle == self.config_timer.handle && event == nwg::Event::OnTimerTick {
            if self.sync_configuration() {
                self.request_refresh(true);
            }
            return;
        }
        if handle == self.debounce_timer.handle && event == nwg::Event::OnTimerTick {
            self.debounce_timer.stop();
            self.request_refresh(false);
            return;
        }
        if handle == self.request_timer.handle && event == nwg::Event::OnTimerTick {
            if let Some(token) = snapshot::take_request(self.store.directory()) {
                if self.last_request != Some(token) {
                    self.last_request = Some(token);
                    self.request_refresh(true);
                }
            }
            return;
        }
        let action = self.action_for(&handle);
        match (event, action) {
            (
                nwg::Event::OnMousePress(nwg::MousePressEvent::MousePressLeftUp),
                Some(Action::Tray(index)),
            ) => {
                self.request_refresh(true);
                let _ = index;
            }
            (nwg::Event::OnContextMenu, Some(Action::Tray(index))) => {
                if let Some(slot) = self.slots.get(index) {
                    let (x, y) = nwg::GlobalCursor::position();
                    slot.menu.popup(x, y);
                }
            }
            (nwg::Event::OnMenuItemSelected, Some(Action::Open)) => launch("--panel"),
            (nwg::Event::OnMenuItemSelected, Some(Action::Refresh)) => self.request_refresh(true),
            (nwg::Event::OnMenuItemSelected, Some(Action::Autostart)) => self.toggle_autostart(),
            (nwg::Event::OnMenuItemSelected, Some(Action::Interval(seconds))) => {
                self.set_interval(seconds)
            }
            (nwg::Event::OnMenuItemSelected, Some(Action::Exit)) => {
                self.exit_requested = true;
                nwg::stop_thread_dispatch();
            }
            _ => {}
        }
    }

    fn action_for(&self, handle: &nwg::ControlHandle) -> Option<Action> {
        for (index, slot) in self.slots.iter().enumerate() {
            if handle == &slot.tray.handle {
                return Some(Action::Tray(index));
            }
            if handle == &slot.open.handle {
                return Some(Action::Open);
            }
            if handle == &slot.refresh.handle {
                return Some(Action::Refresh);
            }
            if handle == &slot.autostart.handle {
                return Some(Action::Autostart);
            }
            if let Some((seconds, _)) = slot
                .intervals
                .iter()
                .find(|(_, item)| handle == &item.handle)
            {
                return Some(Action::Interval(*seconds));
            }
            if handle == &slot.exit.handle {
                return Some(Action::Exit);
            }
        }
        None
    }

    fn debounce_refresh(&mut self, interval_ms: u32) {
        self.debounce_timer.set_interval(interval_ms);
        self.debounce_timer.start();
    }

    fn request_refresh(&mut self, manual: bool) {
        if self.exit_requested {
            return;
        }
        if manual {
            self.policy.reset();
            self.interfaces_dirty.store(true, Ordering::Release);
            if self.busy.load(Ordering::Acquire) {
                self.queued_manual = true;
                return;
            }
        } else if self.busy.load(Ordering::Acquire) {
            return;
        }
        let profiles = enabled_profiles(&load_profiles(&self.store));
        let keys = if manual {
            profiles
                .iter()
                .map(|profile| profile.key.clone())
                .collect::<Vec<_>>()
        } else {
            self.policy
                .auto_keys(profiles.iter().map(|profile| profile.key.as_str()))
        };
        if keys.is_empty() {
            self.refresh_all_visuals();
            return;
        }
        // Keep the configuration snapshot used to choose the devices and reuse
        // it inside the worker instead of reading the file again per refresh.
        let wanted = profiles
            .into_iter()
            .filter(|profile| keys.iter().any(|key| key == &profile.key))
            .collect::<Vec<_>>();
        self.busy.store(true, Ordering::Release);
        self.worker_count = self.worker_count.wrapping_add(1);
        let queue = Arc::clone(&self.queue);
        let busy = Arc::clone(&self.busy);
        let interfaces = Arc::clone(&self.interfaces);
        let dirty = Arc::clone(&self.interfaces_dirty);
        let mouse_cache = Arc::clone(&self.mouse_cache);
        let sender = self.notice.sender();
        let store = self.store.clone();
        let started = crate::platform::gui::task::spawn(busy, sender, move || {
            let snapshot = if dirty.swap(false, Ordering::AcqRel)
                || interfaces
                    .lock()
                    .map(|items| items.is_empty())
                    .unwrap_or(true)
            {
                match crate::platform::hid::enumerate_hid() {
                    Ok(found) => {
                        if let Ok(mut cache) = interfaces.lock() {
                            *cache = found.clone();
                        }
                        Some(found)
                    }
                    Err(error) => {
                        eprintln!("HID 枚举失败：{error}");
                        None
                    }
                }
            } else {
                interfaces.lock().ok().map(|items| items.clone())
            };
            let mut reader =
                BatteryReader::with_cache(WindowsHidBackend::new(), store, mouse_cache);
            let readings = match snapshot {
                Some(found) => reader.read_profiles_with_interfaces(&wanted, &found),
                None => wanted
                    .iter()
                    .map(|profile| {
                        let mut reading =
                            Reading::new(profile, None, ReadingStatus::Error, "HID 接口枚举失败");
                        reading.source = "Windows HID".to_owned();
                        reading
                    })
                    .collect(),
            };
            if let Ok(mut queue) = queue.lock() {
                queue.push_back(WorkerResult { readings, manual });
            }
        });
        if !started {
            self.queued_manual = false;
            eprintln!("无法启动后台刷新任务");
        }
    }

    fn drain_results(&mut self) {
        let results = self
            .queue
            .lock()
            .map(|mut queue| queue.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();
        for result in results {
            self.apply_result(result);
        }
    }

    fn apply_result(&mut self, result: WorkerResult) {
        let record_failures = !result.manual && !self.queued_manual;
        for reading in &result.readings {
            self.readings.insert(reading.key.clone(), reading.clone());
        }
        if record_failures {
            for reading in &result.readings {
                match reading.status {
                    ReadingStatus::Ok | ReadingStatus::Charging => {
                        self.policy.record(&reading.key, true);
                    }
                    ReadingStatus::Sleeping => {}
                    ReadingStatus::Disabled => {}
                    _ => {
                        if self.policy.record(&reading.key, false) {
                            eprintln!("设备 {} 连续失败，已暂停自动刷新", reading.key);
                        }
                    }
                }
            }
        }
        if result.readings.iter().any(|reading| {
            matches!(
                reading.status,
                ReadingStatus::Error | ReadingStatus::NotFound
            )
        }) {
            self.interfaces_dirty.store(true, Ordering::Release);
        }
        self.sync_configuration();
        self.refresh_all_visuals();
        self.publish_snapshot();
        if self.queued_manual && !self.busy.load(Ordering::Acquire) {
            self.queued_manual = false;
            self.request_refresh(true);
        }
    }

    /// Publishes the current readings for panels and other windows.
    ///
    /// The list follows configuration order and always has one entry per
    /// enabled device, so a subscriber never has to guess which cards to show.
    fn publish_snapshot(&mut self) {
        let profiles = enabled_profiles(&load_profiles(&self.store));
        let readings = profiles
            .iter()
            .map(|profile| {
                self.readings.get(&profile.key).cloned().unwrap_or_else(|| {
                    Reading::new(profile, None, ReadingStatus::Unknown, "等待首次读取")
                })
            })
            .collect::<Vec<_>>();
        self.sequence = self.sequence.wrapping_add(1);
        let published = snapshot::Snapshot {
            sequence: self.sequence,
            published_ms: chrono::Local::now().timestamp_millis(),
            readings,
        };
        if let Err(error) = snapshot::publish(self.store.directory(), &published) {
            eprintln!("发布读数快照失败：{error}");
        }
    }

    fn sync_configuration(&mut self) -> bool {
        let profiles = load_profiles(&self.store);
        let visible = tray_profiles(&profiles);
        // Comparing the structures avoids serializing every profile just to
        // detect a change on the 3-second configuration timer.
        let changed = visible != self.config_signature;
        self.config_signature.clone_from(&visible);
        if changed {
            // New, removed or renamed devices must reach an open panel even
            // when no HID refresh is due.
            self.publish_snapshot();
        }
        if !self.ensure_capacity(visible.len()) {
            return changed;
        }
        let mut desired = visible;
        for slot in &mut self.slots {
            if let Some(key) = slot.assigned_key.as_ref() {
                if !desired.iter().any(|profile| &profile.key == key) {
                    slot.hide();
                }
            }
        }
        for profile in desired.drain(..) {
            let index = self
                .slots
                .iter()
                .position(|slot| slot.assigned_key.as_deref() == Some(profile.key.as_str()))
                .or_else(|| {
                    self.slots
                        .iter()
                        .position(|slot| slot.assigned_key.is_none())
                });
            let Some(index) = index else { break };
            let slot = &mut self.slots[index];
            let needs_icon = slot.last_visual.is_none() || slot.profile.as_ref() != Some(&profile);
            slot.assigned_key = Some(profile.key.clone());
            slot.profile = Some(profile);
            if needs_icon {
                slot.last_visual = None;
            }
            if !slot.shown {
                slot.tray.set_visibility(true);
                slot.shown = true;
            }
        }
        if changed {
            // Registry and COM reads are only needed when the configuration
            // changed; the menu state is reused otherwise.
            self.autostart = shell::autostart_state();
        }
        for slot in &self.slots {
            slot.autostart
                .set_checked(self.autostart == AutostartState::Enabled);
            for (seconds, item) in &slot.intervals {
                item.set_checked(*seconds == self.interval);
            }
        }
        changed
    }

    /// Creates tray slots for the configured devices on demand.
    ///
    /// Slots own a hidden window, a tray icon and a popup menu, so creating all
    /// 32 up front wastes handles when only one or two devices are configured.
    fn ensure_capacity(&mut self, needed: usize) -> bool {
        while self.slots.len() < needed.min(MAX_PROFILES) {
            let index = self.slots.len();
            let slot = match TraySlot::create(index) {
                Ok(slot) => slot,
                Err(error) => {
                    eprintln!("创建托盘图标槽失败：{error}");
                    return false;
                }
            };
            let weak = self.weak.clone();
            let handler =
                nwg::full_bind_event_handler(&slot.window.handle, move |event, data, handle| {
                    if let Some(state) = weak.upgrade() {
                        if let Ok(mut state) = state.try_borrow_mut() {
                            state.on_event(event, data, handle);
                        }
                    }
                });
            self.handlers.push(handler);
            self.slots.push(slot);
        }
        true
    }

    fn refresh_all_visuals(&mut self) {
        for slot in &mut self.slots {
            let Some(profile) = slot.profile.clone() else {
                continue;
            };
            let key = profile.key.clone();
            let reading = self.readings.get(&key);
            let percent = reading.and_then(|item| item.percent);
            let charging = reading.is_some_and(|item| item.status == ReadingStatus::Charging);
            let light = taskbar_is_light();
            let visual = (
                percent,
                charging,
                profile.icon.clone(),
                profile.icon_path.clone(),
                light,
            );
            if slot.last_visual.as_ref() != Some(&visual) {
                let icon_size = (32.0 * crate::platform::gui::ui_scale()).round() as u32;
                if let Some(icon) = make_icon(&profile, percent, charging, light, icon_size) {
                    slot.tray.set_icon(&icon);
                    slot.icon = icon;
                    slot.last_visual = Some(visual);
                }
            }
            let tooltip_reading = reading.cloned().unwrap_or_else(|| {
                Reading::new(&profile, None, ReadingStatus::Unknown, "等待首次读取")
            });
            let tip = compose_tooltip(&tooltip_reading, &self.policy);
            slot.tray.set_tip(&tip);
        }
    }

    fn toggle_autostart(&mut self) {
        let enable = self.autostart != AutostartState::Enabled;
        match shell::set_autostart(enable) {
            Ok(message) => {
                self.autostart = shell::autostart_state();
                eprintln!("{message}");
            }
            Err(error) => eprintln!("设置开机自启失败：{error}"),
        }
        for slot in &self.slots {
            slot.autostart
                .set_checked(self.autostart == AutostartState::Enabled);
        }
    }

    fn set_interval(&mut self, seconds: u32) {
        match self.store.set_interval(seconds) {
            Ok(value) => {
                self.interval = value;
                self.apply_interval();
                for slot in &self.slots {
                    for (choice, item) in &slot.intervals {
                        item.set_checked(*choice == value);
                    }
                }
            }
            Err(error) => eprintln!("保存刷新间隔失败：{error}"),
        }
    }

    fn cleanup(&mut self) {
        for slot in &self.slots {
            if slot.shown {
                slot.tray.set_visibility(false);
            }
        }
        for handler in self.handlers.drain(..) {
            nwg::unbind_event_handler(&handler);
        }
        if let Some(raw) = self.raw_handler.take() {
            if let Err(error) = nwg::unbind_raw_event_handler(&raw) {
                eprintln!("解除设备事件监听失败：{error}");
            }
        }
        self.refresh_timer.stop();
        self.config_timer.stop();
        self.debounce_timer.stop();
        self.request_timer.stop();
        // Do not leave a stale snapshot behind: an open panel falls back to
        // reading locally when the file disappears.
        snapshot::clear(self.store.directory());
    }
}

enum Action {
    Tray(usize),
    Open,
    Refresh,
    Autostart,
    Interval(u32),
    Exit,
}

fn make_icon(
    profile: &DeviceProfile,
    percent: Option<u8>,
    charging: bool,
    light: bool,
    size: u32,
) -> Option<nwg::Icon> {
    let image = if profile.icon == "custom" && !profile.icon_path.is_empty() {
        custom_icon_rgba(
            Path::new(&profile.icon_path),
            percent,
            size,
            charging,
            light,
        )
        .unwrap_or_else(|_| device_icon_rgba("device", percent, size, charging, light))
    } else {
        let kind = if profile.icon == "custom" {
            "device"
        } else {
            profile.icon.as_str()
        };
        device_icon_rgba(kind, percent, size, charging, light)
    };
    let bytes = icon_bytes(&image).ok()?;
    let mut icon = nwg::Icon::default();
    nwg::Icon::builder()
        .source_bin(Some(&bytes))
        .strict(true)
        .size(Some((size, size)))
        .build(&mut icon)
        .ok()?;
    Some(icon)
}

fn taskbar_is_light() -> bool {
    winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize")
        .and_then(|key| key.get_value::<u32, _>("SystemUsesLightTheme"))
        .map(|value| value != 0)
        .unwrap_or(false)
}
