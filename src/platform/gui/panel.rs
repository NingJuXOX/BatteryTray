#![allow(deprecated)]

use super::{init_gui, status_text, Toggle};
use crate::battery::{BatteryReader, SharedMouseCache};
use crate::config::{ConfigStore, INTERVAL_CHOICES};
use crate::error::Result;
use crate::platform::hid::WindowsHidBackend;
use crate::platform::shell;
use crate::platform::sync::{NamedMutex, PANEL_MUTEX_NAME, TRAY_MUTEX_NAME};
use crate::profiles::enabled_profiles;
use crate::protocol::Reading;
use crate::snapshot;
use native_windows_gui as nwg;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Maximum number of device cards shown in the compact panel.
const MAX_CARDS: usize = 12;
/// How often an open panel checks the tray's published snapshot.
const SUBSCRIBE_INTERVAL_MS: u32 = 1500;
/// Older snapshots are ignored (for example left over after a tray crash).
const SNAPSHOT_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(2 * 3600);

/// Opens the compact battery panel, focusing an existing instance if present.
pub fn run() -> Result<()> {
    let instance = NamedMutex::create(PANEL_MUTEX_NAME)?;
    if instance.is_none() {
        shell::focus_window(shell::PANEL_TITLE);
        return Ok(());
    }
    init_gui()?;
    let mut window = nwg::Window::default();
    nwg::Window::builder()
        .title(shell::PANEL_TITLE)
        .size(crate::platform::gui::window_size(840, 530))
        .center(true)
        .flags(nwg::WindowFlags::WINDOW | nwg::WindowFlags::VISIBLE)
        .build(&mut window)?;
    crate::platform::gui::theme::apply(&window);
    let mut refresh = nwg::Button::default();
    nwg::Button::builder()
        .text("刷新")
        .parent(&window)
        .build(&mut refresh)?;
    let mut add = nwg::Button::default();
    nwg::Button::builder()
        .text("新增")
        .parent(&window)
        .build(&mut add)?;
    let mut interval_label = nwg::Label::default();
    nwg::Label::builder()
        .text("刷新间隔")
        .parent(&window)
        .build(&mut interval_label)?;
    let labels: Vec<String> = INTERVAL_CHOICES
        .iter()
        .map(|(_, label)| (*label).to_owned())
        .collect();
    let mut interval = nwg::ComboBox::default();
    nwg::ComboBox::builder()
        .parent(&window)
        .collection(labels)
        .build(&mut interval)?;
    let autostart_on = shell::autostart_state() == shell::AutostartState::Enabled;
    let autostart = Toggle::build(&window, "开机自启", autostart_on)?;
    let mut status = nwg::Label::default();
    nwg::Label::builder()
        .text("")
        .parent(&window)
        .build(&mut status)?;
    let mut cards = Vec::with_capacity(MAX_CARDS);
    for _ in 0..MAX_CARDS {
        let mut card = nwg::Button::default();
        nwg::Button::builder()
            .text("")
            .parent(&window)
            .build(&mut card)?;
        card.set_visible(false);
        cards.push(card);
    }
    let mut notice = nwg::Notice::default();
    nwg::Notice::builder().parent(&window).build(&mut notice)?;
    let mut timer = nwg::Timer::default();
    nwg::Timer::builder()
        .parent(&window)
        .interval(60_000)
        .stopped(true)
        .build(&mut timer)?;
    let tray_managed = NamedMutex::exists(TRAY_MUTEX_NAME);
    let store = ConfigStore::from_env();
    let interval_seconds = store
        .load()
        .get("interval")
        .and_then(|value| value.as_u64())
        .unwrap_or(60)
        .clamp(30, 1800) as u32;
    let initial_index = INTERVAL_CHOICES
        .iter()
        .position(|(seconds, _)| *seconds == interval_seconds)
        .unwrap_or(1);
    interval.set_selection(Some(initial_index));
    let state = Rc::new(RefCell::new(PanelState {
        window,
        refresh,
        add,
        interval_label,
        interval,
        autostart,
        status,
        cards,
        card_keys: vec![None; MAX_CARDS],
        notice,
        timer,
        store,
        interval_seconds,
        tray_managed,
        snapshot_sequence: None,
        busy: Arc::new(AtomicBool::new(false)),
        reload_pending: Arc::new(AtomicBool::new(false)),
        queue: Arc::new(Mutex::new(VecDeque::new())),
        mouse_cache: SharedMouseCache::default(),
        handlers: Vec::new(),
        closing: false,
    }));
    {
        let weak: Weak<RefCell<PanelState>> = Rc::downgrade(&state);
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
    {
        let state_ref = state.borrow();
        let interval = if tray_managed {
            SUBSCRIBE_INTERVAL_MS
        } else {
            interval_seconds.saturating_mul(1000)
        };
        state_ref.timer.set_interval(interval);
        state_ref.timer.start();
    }
    state.borrow_mut().apply_layout();
    state.borrow_mut().start();
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

struct PanelState {
    window: nwg::Window,
    refresh: nwg::Button,
    add: nwg::Button,
    interval_label: nwg::Label,
    interval: nwg::ComboBox<String>,
    autostart: Toggle,
    status: nwg::Label,
    cards: Vec<nwg::Button>,
    card_keys: Vec<Option<String>>,
    notice: nwg::Notice,
    timer: nwg::Timer,
    // Kept alive so the layouts stay bound to the window for its lifetime.
    store: ConfigStore,
    interval_seconds: u32,
    tray_managed: bool,
    snapshot_sequence: Option<u64>,
    busy: Arc<AtomicBool>,
    reload_pending: Arc<AtomicBool>,
    queue: Arc<Mutex<VecDeque<Vec<Reading>>>>,
    mouse_cache: SharedMouseCache,
    handlers: Vec<nwg::EventHandler>,
    closing: bool,
}

impl PanelState {
    /// Single source of geometry for the panel: toolbar, stretching list and
    /// status line. Re-run on every resize.
    /// Single source of geometry for the panel: toolbar, a responsive grid of
    /// device cards and a compact hint on the toolbar. Re-run on every resize.
    fn apply_layout(&self) {
        let (width, _) = self.window.size();
        let scale = crate::platform::gui::ui_scale();
        let width = (width as f64 / scale).round() as u32;
        let width = width.max(620);
        let pad = 18_u32;
        let content_w = width.saturating_sub(pad * 2);

        let mut x = pad as i32;
        let y = 12_i32;
        crate::place!(self.add, x, y, 76, 30);
        x += 76 + 8;
        crate::place!(self.refresh, x, y, 76, 30);
        x += 76 + 16;
        crate::place!(self.interval_label, x, y + 4, 64, 22);
        x += 64 + 8;
        // Combo box height reserves the dropdown area; a short value makes the
        // interval list impossible to open.
        crate::place!(self.interval, x, y, 108, 240);
        x += 108 + 16;
        crate::place!(self.autostart.button, x, y + 2, 104, 26);
        x += 104 + 12;
        let hint_w = (width as i32 - x - pad as i32).max(80) as u32;
        crate::place!(self.status, x, y + 4, hint_w, 22);

        let gap = 14_u32;
        let card_h = 100_u32;
        let min_card_w = 220_u32;
        let columns = ((content_w + gap) / (min_card_w + gap)).clamp(1, 4);
        let card_w = content_w.saturating_sub(gap * (columns - 1)) / columns;
        let top = (y + 30 + 14) as u32;
        for (index, card) in self.cards.iter().enumerate() {
            let column = (index % columns as usize) as u32;
            let row = (index / columns as usize) as u32;
            let card_x = pad + column * (card_w + gap);
            let card_y = top + row * (card_h + gap);
            crate::place!(card, card_x as i32, card_y as i32, card_w, card_h);
        }
        crate::platform::gui::theme::polish(&self.window);
    }

    fn on_event(&mut self, event: nwg::Event, handle: nwg::ControlHandle) {
        if handle == self.notice.handle && event == nwg::Event::OnNotice {
            self.drain_readings();
            if self.reload_pending.swap(false, Ordering::AcqRel) {
                self.request_refresh();
            }
        } else if handle == self.refresh.handle && event == nwg::Event::OnButtonClick {
            self.request_refresh();
        } else if handle == self.add.handle && event == nwg::Event::OnButtonClick {
            self.launch_editor(None);
        } else if handle == self.autostart.button.handle && event == nwg::Event::OnButtonClick {
            self.toggle_autostart();
        } else if handle == self.interval.handle && event == nwg::Event::OnComboxBoxSelection {
            self.change_interval();
        } else if handle == self.timer.handle && event == nwg::Event::OnTimerTick {
            if self.tray_managed {
                self.poll_snapshot();
            } else {
                self.request_refresh();
            }
        } else if handle == self.window.handle && event == nwg::Event::OnResize {
            self.apply_layout();
        } else if handle == self.window.handle && event == nwg::Event::OnWindowClose {
            self.close();
        } else if event == nwg::Event::OnButtonClick {
            for (index, card) in self.cards.iter().enumerate() {
                if card.handle == handle {
                    let key = self.card_keys.get(index).cloned().flatten();
                    if let Some(key) = key {
                        self.launch_editor(Some(&key));
                    }
                    return;
                }
            }
        }
    }

    /// Opens the add/settings window and refreshes the cards once it closes.
    fn launch_editor(&mut self, key: Option<&str>) {
        let Ok(executable) = std::env::current_exe() else {
            self.status.set_text("无法定位程序自身路径。");
            return;
        };
        let mut command = std::process::Command::new(executable);
        command.arg("--editor");
        if let Some(key) = key {
            command.arg("--key").arg(key);
        }
        match command.spawn() {
            Ok(mut child) => {
                let reload = Arc::clone(&self.reload_pending);
                let sender = self.notice.sender();
                std::thread::Builder::new()
                    .name("battery-tray-editor-wait".to_owned())
                    .spawn(move || {
                        _ = child.wait();
                        reload.store(true, Ordering::Release);
                        sender.notice();
                    })
                    .ok();
            }
            Err(error) => self
                .status
                .set_text(&format!("打开添加设备窗口失败：{error}")),
        }
    }

    /// Initial load: either ask the tray for its snapshot, or read HID locally.
    fn start(&mut self) {
        if self.tray_managed {
            self.poll_snapshot();
            if self.snapshot_sequence.is_none() {
                self.status.set_text("正在等待托盘首次刷新…");
            }
            if let Err(error) = snapshot::request(self.store.directory()) {
                self.status.set_text(&format!("请求托盘刷新失败：{error}"));
            }
        } else {
            self.request_refresh();
        }
    }

    /// Applies a newly published snapshot, if it changed since the last tick.
    fn poll_snapshot(&mut self) {
        let directory = self.store.directory();
        let Some(published) = snapshot::load(directory) else {
            // A tray that exits removes its snapshot. Panels that had already
            // received one fall back to reading locally instead of freezing.
            if self.tray_managed
                && self.snapshot_sequence.is_some()
                && !snapshot::path(directory).exists()
            {
                self.fall_back_to_local();
            }
            return;
        };
        if !snapshot::is_fresh(&published, SNAPSHOT_MAX_AGE)
            || self.snapshot_sequence == Some(published.sequence)
        {
            return;
        }
        self.snapshot_sequence = Some(published.sequence);
        self.populate(&published.readings);
    }

    fn fall_back_to_local(&mut self) {
        self.tray_managed = false;
        self.timer
            .set_interval(self.interval_seconds.saturating_mul(1000));
        self.timer.start();
        self.status.set_text("托盘已退出，面板改为本地读取。");
        self.request_refresh();
    }

    fn request_refresh(&mut self) {
        if self.tray_managed {
            // The tray owns the HID handles; a manual refresh is a request,
            // answered by the tray's 500 ms poller.
            match snapshot::request(self.store.directory()) {
                Ok(()) => self.status.set_text("已通知托盘刷新…"),
                Err(error) => self.status.set_text(&format!("请求托盘刷新失败：{error}")),
            }
            self.poll_snapshot();
            return;
        }
        if self.closing || self.busy.swap(true, Ordering::AcqRel) {
            return;
        }
        self.refresh.set_enabled(false);
        self.status.set_text("正在读取设备…");
        let queue = Arc::clone(&self.queue);
        let busy = Arc::clone(&self.busy);
        let mouse_cache = Arc::clone(&self.mouse_cache);
        let sender = self.notice.sender();
        let store = self.store.clone();
        let started = super::task::spawn(busy, sender, move || {
            let profiles = crate::profiles::load_profiles(&store);
            let keys = enabled_profiles(&profiles)
                .into_iter()
                .map(|profile| profile.key)
                .collect::<Vec<_>>();
            let mut reader =
                BatteryReader::with_cache(WindowsHidBackend::new(), store, mouse_cache);
            let readings = reader.read_all(Some(&keys));
            if let Ok(mut queue) = queue.lock() {
                queue.push_back(readings);
            }
        });
        if !started {
            self.refresh.set_enabled(true);
            self.status.set_text("无法启动后台读取任务，请稍后重试。");
        }
    }

    fn drain_readings(&mut self) {
        let results = self
            .queue
            .lock()
            .map(|mut queue| queue.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();
        if results.is_empty() {
            self.refresh.set_enabled(true);
            return;
        }
        let mut latest = Vec::new();
        for readings in results {
            latest = readings;
        }
        self.populate(&latest);
        self.refresh.set_enabled(true);
    }

    fn populate(&mut self, readings: &[Reading]) {
        let profiles = crate::profiles::load_profiles(&self.store);
        for (index, card) in self.cards.iter().enumerate() {
            if let Some(reading) = readings.get(index) {
                let percent = reading
                    .percent
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_owned());
                // Same glyph kind as the tray icon; custom images fall back to
                // the generic device silhouette for now.
                let kind = profiles
                    .iter()
                    .find(|profile| profile.key == reading.key)
                    .map(|profile| {
                        if profile.icon == "custom" {
                            "device"
                        } else {
                            profile.icon.as_str()
                        }
                    })
                    .unwrap_or("device");
                // Four newline-separated fields read by theme::draw_card.
                let text = format!(
                    "{}\n{}\n{}\n{}",
                    reading.name,
                    percent,
                    status_text(reading.status),
                    kind
                );
                card.set_text(&text);
                card.set_visible(true);
                if let Some(slot) = self.card_keys.get_mut(index) {
                    *slot = Some(reading.key.clone());
                }
            } else {
                card.set_visible(false);
                if let Some(slot) = self.card_keys.get_mut(index) {
                    *slot = None;
                }
            }
        }
        // The old "已读取 X/Y 台设备" bottom line is intentionally gone; this
        // toolbar hint only carries transient messages.
        self.status.set_text(if readings.is_empty() {
            "没有启用的设备，点“新增”添加设备。"
        } else {
            ""
        });
    }

    fn toggle_autostart(&mut self) {
        let enable = self.autostart.flip();
        match shell::set_autostart(enable) {
            Ok(message) => self.status.set_text(&message),
            Err(error) => {
                self.status.set_text(&format!("设置开机自启失败：{error}"));
                self.autostart.set(!enable);
            }
        }
    }

    fn change_interval(&mut self) {
        let Some(index) = self.interval.selection() else {
            return;
        };
        let Some((seconds, _)) = INTERVAL_CHOICES.get(index) else {
            return;
        };
        match self.store.set_interval(*seconds) {
            Ok(value) => {
                self.interval_seconds = value;
                if !self.tray_managed {
                    self.timer.set_interval(value.saturating_mul(1000));
                    self.timer.start();
                }
                self.status.set_text("刷新间隔已保存。");
            }
            Err(error) => self.status.set_text(&format!("保存刷新间隔失败：{error}")),
        }
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

impl Drop for PanelState {
    fn drop(&mut self) {
        if !self.closing {
            self.cleanup();
        }
    }
}
