use super::{init_gui, status_text};
use crate::battery::{BatteryReader, HidBackend, OpenOptions, SharedMouseCache};
use crate::config::ConfigStore;
use crate::error::Result;
use crate::platform::hid::{enumerate_hid, WindowsHidBackend};
use crate::platform::shell;
use crate::platform::sync::{NamedMutex, DEBUG_MUTEX_NAME};
use crate::protocol::{hex_dump, parse_hex};
use native_windows_gui as nwg;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Opens the HID debugger and calibration window.
pub fn run() -> Result<()> {
    let instance = NamedMutex::create(DEBUG_MUTEX_NAME)?;
    if instance.is_none() {
        shell::focus_window(shell::DEBUG_TITLE);
        return Ok(());
    }
    init_gui()?;
    let mut window = nwg::Window::default();
    nwg::Window::builder()
        .title(shell::DEBUG_TITLE)
        .size(crate::platform::gui::window_size(1060, 740))
        .center(true)
        .flags(nwg::WindowFlags::WINDOW | nwg::WindowFlags::VISIBLE)
        .build(&mut window)?;
    crate::platform::gui::theme::apply(&window);
    let mut list = nwg::ListView::default();
    nwg::ListView::builder()
        .parent(&window)
        .list_style(nwg::ListViewStyle::Detailed)
        .ex_flags(nwg::ListViewExFlags::FULL_ROW_SELECT)
        .build(&mut list)?;
    crate::platform::gui::theme::style_list(&list);
    list.set_headers_enabled(true);
    for (index, (text, width)) in [
        ("产品", 190),
        ("VID:PID", 88),
        ("Usage", 84),
        ("In/Out/F", 78),
        ("MI", 34),
    ]
    .iter()
    .enumerate()
    {
        list.insert_column(nwg::InsertListViewColumn {
            index: Some(index as i32),
            fmt: None,
            width: Some(crate::platform::gui::column_width(*width)),
            text: Some((*text).to_owned()),
        });
    }
    let mut refresh = nwg::Button::default();
    nwg::Button::builder()
        .text("刷新 HID 列表")
        .parent(&window)
        .build(&mut refresh)?;
    let mut note = nwg::Label::default();
    nwg::Label::builder()
        .text("枚举只请求读取属性；发送命令前请确认设备协议。")
        .parent(&window)
        .build(&mut note)?;
    let mut raw = nwg::TextInput::default();
    nwg::TextInput::builder()
        .text("00 F7 00")
        .parent(&window)
        .build(&mut raw)?;
    let mut feature_read = nwg::Button::default();
    nwg::Button::builder()
        .text("读 Feature")
        .parent(&window)
        .build(&mut feature_read)?;
    let mut feature_send = nwg::Button::default();
    nwg::Button::builder()
        .text("发 Feature")
        .parent(&window)
        .build(&mut feature_send)?;
    let mut output_send = nwg::Button::default();
    nwg::Button::builder()
        .text("发 Output")
        .parent(&window)
        .build(&mut output_send)?;
    let mut input_read = nwg::Button::default();
    nwg::Button::builder()
        .text("读 Input")
        .parent(&window)
        .build(&mut input_read)?;
    let mut battery = nwg::Button::default();
    nwg::Button::builder()
        .text("读取全部电量")
        .parent(&window)
        .build(&mut battery)?;
    let mut import_test = nwg::Button::default();
    nwg::Button::builder()
        .text("粘贴并测试")
        .parent(&window)
        .build(&mut import_test)?;
    let mut log = nwg::TextBox::default();
    nwg::TextBox::builder()
        .parent(&window)
        .readonly(true)
        .flags(
            nwg::TextBoxFlags::VISIBLE
                | nwg::TextBoxFlags::VSCROLL
                | nwg::TextBoxFlags::AUTOVSCROLL
                | nwg::TextBoxFlags::HSCROLL
                | nwg::TextBoxFlags::AUTOHSCROLL,
        )
        .build(&mut log)?;
    let calibration = ConfigStore::from_env().calibration();
    let calibration_value = |key: &str, default: &str| {
        calibration
            .get(key)
            .and_then(Value::as_i64)
            .map(|value| value.to_string())
            .unwrap_or_else(|| default.to_owned())
    };
    let mut cal_label = nwg::Label::default();
    nwg::Label::builder()
        .text("手动标定（十进制或 0x 十六进制）")
        .parent(&window)
        .build(&mut cal_label)?;
    let mut mouse_battery = nwg::TextInput::default();
    nwg::TextInput::builder()
        .text(&calibration_value("mouse_battery_index", "2"))
        .parent(&window)
        .build(&mut mouse_battery)?;
    let mut mouse_device = nwg::TextInput::default();
    nwg::TextInput::builder()
        .text(&calibration_value("mouse_device_index", "2"))
        .parent(&window)
        .build(&mut mouse_device)?;
    let mut headphone_cmd = nwg::TextInput::default();
    nwg::TextInput::builder()
        .text(&calibration_value("headphone_cmd", "0x27"))
        .parent(&window)
        .build(&mut headphone_cmd)?;
    let mut headphone_level = nwg::TextInput::default();
    nwg::TextInput::builder()
        .text(&calibration_value("headphone_level_index", "5"))
        .parent(&window)
        .build(&mut headphone_level)?;
    let mut save_cal = nwg::Button::default();
    nwg::Button::builder()
        .text("保存标定")
        .parent(&window)
        .build(&mut save_cal)?;
    let mut status = nwg::Label::default();
    nwg::Label::builder()
        .text("选择左侧接口后可读取 Feature / Input。自定义协议偏移从完整响应第 0 字节开始。")
        .parent(&window)
        .build(&mut status)?;
    let mut notice = nwg::Notice::default();
    nwg::Notice::builder().parent(&window).build(&mut notice)?;
    let state = Rc::new(RefCell::new(DebugState {
        window,
        list,
        log,
        raw,
        refresh,
        note,
        feature_read,
        feature_send,
        output_send,
        input_read,
        battery,
        import_test,
        mouse_battery,
        mouse_device,
        headphone_cmd,
        headphone_level,
        cal_label,
        save_cal,
        status,
        notice,
        interfaces: Vec::new(),
        selected: None,
        queue: Arc::new(Mutex::new(VecDeque::new())),
        mouse_cache: SharedMouseCache::default(),
        battery_busy: Arc::new(AtomicBool::new(false)),
        handlers: Vec::new(),
        closing: false,
    }));
    {
        let weak: Weak<RefCell<DebugState>> = Rc::downgrade(&state);
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
    state.borrow_mut().apply_layout();
    state.borrow_mut().refresh_interfaces();
    nwg::dispatch_thread_events();
    if let Ok(mut state) = state.try_borrow_mut() {
        state.cleanup();
    }
    drop(state);
    Ok(())
}

type BatteryResults =
    Arc<Mutex<VecDeque<std::result::Result<Vec<crate::protocol::Reading>, String>>>>;

/// Renders test_readings into one short line for logs and the confirmation box.
fn summarize_readings(result: &serde_json::Value) -> String {
    let Some(items) = result
        .get("test_readings")
        .and_then(serde_json::Value::as_array)
    else {
        return "没有读数".to_owned();
    };
    if items.is_empty() {
        return "没有读数".to_owned();
    }
    items
        .iter()
        .map(|item| {
            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("设备");
            let percent = item
                .get("percent")
                .and_then(|v| v.as_u64())
                .map(|value| format!("{value}%"))
                .unwrap_or_else(|| "--".to_owned());
            let status = item.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let message = item.get("message").and_then(|v| v.as_str()).unwrap_or("");
            if message.is_empty() {
                format!("{name}：{percent}（{status}）")
            } else {
                format!("{name}：{percent}（{status}，{message}）")
            }
        })
        .collect::<Vec<_>>()
        .join("；")
}

struct DebugState {
    window: nwg::Window,
    list: nwg::ListView,
    log: nwg::TextBox,
    raw: nwg::TextInput,
    refresh: nwg::Button,
    note: nwg::Label,
    feature_read: nwg::Button,
    feature_send: nwg::Button,
    output_send: nwg::Button,
    input_read: nwg::Button,
    battery: nwg::Button,
    import_test: nwg::Button,
    mouse_battery: nwg::TextInput,
    mouse_device: nwg::TextInput,
    headphone_cmd: nwg::TextInput,
    headphone_level: nwg::TextInput,
    cal_label: nwg::Label,
    save_cal: nwg::Button,
    status: nwg::Label,
    notice: nwg::Notice,
    interfaces: Vec<crate::battery::HidInterface>,
    selected: Option<usize>,
    queue: BatteryResults,
    mouse_cache: SharedMouseCache,
    battery_busy: Arc<AtomicBool>,
    handlers: Vec<nwg::EventHandler>,
    closing: bool,
}

impl DebugState {
    /// Single source of geometry for the debugger: top bar, HID list, request
    /// rows, log and calibration block. Re-run on every resize.
    fn apply_layout(&self) {
        let (width, height) = self.window.size();
        let scale = crate::platform::gui::ui_scale();
        let width = (width as f64 / scale).round() as u32;
        let height = (height as f64 / scale).round() as u32;
        let width = width.max(900);
        let height = height.max(600);
        let pad = 16_u32;

        crate::place!(self.refresh, pad as i32, 12, 112, 28);
        crate::place!(
            self.note,
            (pad + 122) as i32,
            16,
            width.saturating_sub(pad + 122 + pad),
            22
        );

        let left_w = 480_u32;
        let right_x = pad + left_w + 14;
        let right_w = width.saturating_sub(right_x + pad);
        let rxi = right_x as i32;
        let rwi = right_w as i32;

        let status_h = 50_u32;
        let status_y = height.saturating_sub(pad + status_h);
        crate::place!(self.status, rxi, status_y as i32, right_w, status_h);

        let cal_y = status_y.saturating_sub(8 + 26);
        crate::place!(self.save_cal, rxi + rwi - 88, cal_y as i32, 88, 26);
        let mut cx = rxi;
        for (input, w) in [
            (&self.mouse_battery, 68_u32),
            (&self.mouse_device, 68),
            (&self.headphone_cmd, 74),
            (&self.headphone_level, 74),
        ] {
            crate::place!(input, cx, cal_y as i32, w, 26);
            cx += w as i32 + 6;
        }
        let cal_label_y = cal_y.saturating_sub(26);
        crate::place!(self.cal_label, rxi, cal_label_y as i32, right_w, 22);

        let list_top = 50_u32;
        let list_h = height.saturating_sub(pad + list_top);
        crate::place!(self.list, pad as i32, list_top as i32, left_w, list_h);

        let feature_w = 94_u32;
        let raw_w = right_w.saturating_sub(feature_w * 2 + 16).max(160);
        crate::place!(self.raw, rxi, list_top as i32, raw_w, 28);
        crate::place!(
            self.feature_read,
            rxi + raw_w as i32 + 8,
            list_top as i32,
            feature_w,
            28
        );
        crate::place!(
            self.feature_send,
            rxi + (raw_w + 8 + feature_w + 8) as i32,
            list_top as i32,
            feature_w,
            28
        );

        let row2 = list_top + 36;
        crate::place!(self.output_send, rxi, row2 as i32, 100, 28);
        crate::place!(self.input_read, rxi + 108, row2 as i32, 100, 28);
        crate::place!(self.battery, rxi + 216, row2 as i32, 112, 28);
        crate::place!(self.import_test, rxi + 336, row2 as i32, 120, 28);

        let log_top = row2 + 36;
        let log_bottom = cal_label_y.saturating_sub(12);
        crate::place!(
            self.log,
            rxi,
            log_top as i32,
            right_w,
            log_bottom.saturating_sub(log_top).max(120)
        );
        crate::platform::gui::theme::polish(&self.window);
    }

    fn on_event(&mut self, event: nwg::Event, handle: nwg::ControlHandle) {
        if handle == self.refresh.handle && event == nwg::Event::OnButtonClick {
            self.refresh_interfaces();
        } else if handle == self.list.handle && event == nwg::Event::OnListViewItemChanged {
            self.selected = self.list.selected_item();
            self.update_status();
        } else if handle == self.feature_read.handle && event == nwg::Event::OnButtonClick {
            self.read_feature();
        } else if handle == self.feature_send.handle && event == nwg::Event::OnButtonClick {
            self.send_feature();
        } else if handle == self.output_send.handle && event == nwg::Event::OnButtonClick {
            self.send_output();
        } else if handle == self.input_read.handle && event == nwg::Event::OnButtonClick {
            self.read_input();
        } else if handle == self.battery.handle && event == nwg::Event::OnButtonClick {
            self.read_batteries();
        } else if handle == self.import_test.handle && event == nwg::Event::OnButtonClick {
            self.import_and_test();
        } else if handle == self.save_cal.handle && event == nwg::Event::OnButtonClick {
            self.save_calibration();
        } else if handle == self.notice.handle && event == nwg::Event::OnNotice {
            self.drain_batteries();
        } else if handle == self.window.handle && event == nwg::Event::OnResize {
            self.apply_layout();
        } else if handle == self.window.handle && event == nwg::Event::OnWindowClose {
            self.close();
        }
    }

    /// Imports an AI-produced profile from the clipboard.
    ///
    /// Follows the same gate as the prompt: dry-read first, ask the user
    /// whether the battery value looks right, and only then persist anything.
    fn import_and_test(&self) {
        let Some(text) = nwg::Clipboard::data_text(&self.window) else {
            nwg::modal_error_message(
                &self.window,
                "剪贴板为空",
                "请先复制 AI 返回的设备 JSON，再点「粘贴并测试」。",
            );
            return;
        };
        let trial = crate::device_api::apply_profile_json(&text, true, true);
        if trial.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            self.append_log(&format!("试读失败：{}", trial));
            return;
        }
        let summary = summarize_readings(&trial);
        self.append_log(&format!("试读（未写入配置）：{summary}"));
        let question = format!(
            "试读结果：\r\n{summary}\r\n\r\n这个电量是否合理？\r\n点「是」写入配置，点「否」放弃。"
        );
        let choice = nwg::modal_message(
            &self.window,
            &nwg::MessageParams {
                title: "确认电量",
                content: &question,
                buttons: nwg::MessageButtons::YesNo,
                icons: nwg::MessageIcons::Question,
            },
        );
        if choice != nwg::MessageChoice::Yes {
            self.append_log("用户认为电量不合理，配置未写入。");
            return;
        }
        let saved = crate::device_api::apply_profile_json(&text, true, false);
        if saved.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
            self.append_log(&format!("配置已写入并实测：{}", summarize_readings(&saved)));
        } else {
            self.append_log(&format!("写入失败：{}", saved));
        }
    }

    fn append_log(&self, text: &str) {
        let mut content = self.log.text();
        if !content.is_empty() {
            content.push_str("\r\n");
        }
        content.push_str(text);
        if content.len() > 60_000 {
            content = content
                .chars()
                .rev()
                .take(50_000)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
        }
        self.log.set_text(&content);
        self.log.scroll_lastline();
    }

    fn update_status(&self) {
        let text = self
            .selected
            .and_then(|index| self.interfaces.get(index))
            .map(|interface| {
                format!(
                    "{}  usage={:04X}:{:04X}  In/Out/Feature={}/{}/{}  MI={}",
                    interface.label(),
                    interface.usage_page,
                    interface.usage,
                    interface.input_len,
                    interface.output_len,
                    interface.feature_len,
                    interface.interface_number
                )
            })
            .unwrap_or_else(|| "请选择一个 HID 接口。".to_owned());
        self.status.set_text(&text);
    }

    fn refresh_interfaces(&mut self) {
        match enumerate_hid() {
            Ok(interfaces) => {
                self.interfaces = interfaces;
                self.list.clear();
                for (row, interface) in self.interfaces.iter().enumerate() {
                    let values = [
                        if interface.product.is_empty() {
                            interface.manufacturer.clone()
                        } else {
                            interface.product.clone()
                        },
                        interface.vid_pid(),
                        format!("{:04X}:{:04X}", interface.usage_page, interface.usage),
                        format!(
                            "{}/{}/{}",
                            interface.input_len, interface.output_len, interface.feature_len
                        ),
                        interface.interface_number.to_string(),
                    ];
                    for (column, value) in values.iter().enumerate() {
                        if column == 0 {
                            self.list.insert_item(nwg::InsertListViewItem {
                                index: Some(row as i32),
                                column_index: 0,
                                text: Some(value.clone()),
                            });
                        } else {
                            self.list.update_item(
                                row,
                                nwg::InsertListViewItem {
                                    index: None,
                                    column_index: column as i32,
                                    text: Some(value.clone()),
                                },
                            );
                        }
                    }
                }
                self.selected = if self.interfaces.is_empty() {
                    None
                } else {
                    Some(0)
                };
                if !self.interfaces.is_empty() {
                    self.list.select_item(0, true);
                }
                self.append_log(&format!("枚举到 {} 个 HID 接口。", self.interfaces.len()));
                self.update_status();
            }
            Err(error) => {
                self.append_log(&format!("HID 枚举失败：{error}"));
                self.status.set_text(&format!("HID 枚举失败：{error}"));
            }
        }
    }

    fn selected_interface(&self) -> std::result::Result<crate::battery::HidInterface, String> {
        self.selected
            .and_then(|index| self.interfaces.get(index))
            .cloned()
            .ok_or_else(|| "请先选择一个 HID 接口".to_owned())
    }

    fn read_feature(&mut self) {
        self.with_device(
            OpenOptions {
                write: true,
                read: false,
            },
            "读取 Feature",
            |device| match device.get_feature(None, 0)? {
                Some(data) => Ok(format!("Feature[{}] {}", data.len(), hex_dump(&data))),
                None => Ok("Feature 读取无响应".to_owned()),
            },
        );
    }

    fn send_feature(&mut self) {
        let data = match parse_hex(&self.raw.text()) {
            Ok(data) if !data.is_empty() => data,
            Ok(_) => {
                self.append_log("请求字节为空");
                return;
            }
            Err(error) => {
                self.append_log(&format!("请求字节无效：{error}"));
                return;
            }
        };
        self.with_device(
            OpenOptions {
                write: true,
                read: false,
            },
            "发送 Feature",
            |device| {
                let ok = device.set_feature(&data)?;
                Ok(format!(
                    "Feature 写入 {}：{}",
                    if ok { "成功" } else { "失败" },
                    hex_dump(&data)
                ))
            },
        );
    }

    fn send_output(&mut self) {
        let data = match parse_hex(&self.raw.text()) {
            Ok(data) if !data.is_empty() => data,
            Ok(_) => {
                self.append_log("请求字节为空");
                return;
            }
            Err(error) => {
                self.append_log(&format!("请求字节无效：{error}"));
                return;
            }
        };
        self.with_device(
            OpenOptions {
                write: true,
                read: true,
            },
            "发送 Output",
            |device| {
                let ok = device.write_output(&data)?;
                let mut text = format!(
                    "Output 写入 {}：{}",
                    if ok { "成功" } else { "失败" },
                    hex_dump(&data)
                );
                if let Some(response) = device.read_input(1000)? {
                    text.push_str(&format!(
                        "\r\nInput[{}] {}",
                        response.len(),
                        hex_dump(&response)
                    ));
                }
                Ok(text)
            },
        );
    }

    fn read_input(&mut self) {
        self.with_device(
            OpenOptions {
                write: false,
                read: true,
            },
            "读取 Input",
            |device| match device.read_input(1000)? {
                Some(data) => Ok(format!("Input[{}] {}", data.len(), hex_dump(&data))),
                None => Ok("Input 读取超时".to_owned()),
            },
        );
    }

    fn with_device<F>(&mut self, options: OpenOptions, operation: &str, action: F)
    where
        F: FnOnce(&mut dyn crate::battery::HidDevice) -> Result<String>,
    {
        let interface = match self.selected_interface() {
            Ok(interface) => interface,
            Err(error) => {
                self.append_log(&error);
                self.status.set_text(&error);
                return;
            }
        };
        let mut backend = WindowsHidBackend::new();
        match backend.open(&interface, options) {
            Ok(mut device) => match action(device.as_mut()) {
                Ok(text) => {
                    self.append_log(&format!("[{operation}] {text}"));
                    self.status.set_text(&text.replace("\r\n", " | "));
                }
                Err(error) => {
                    self.append_log(&format!("[{operation}] 失败：{error}"));
                    self.status.set_text(&format!("{operation}失败：{error}"));
                }
            },
            Err(error) => {
                self.append_log(&format!("[{operation}] 打开接口失败：{error}"));
                self.status.set_text(&format!("打开接口失败：{error}"));
            }
        }
    }

    fn read_batteries(&mut self) {
        if self.battery_busy.swap(true, Ordering::AcqRel) {
            return;
        }
        self.battery.set_enabled(false);
        let queue = Arc::clone(&self.queue);
        let busy = Arc::clone(&self.battery_busy);
        let mouse_cache = Arc::clone(&self.mouse_cache);
        let sender = self.notice.sender();
        let started = super::task::spawn(busy, sender, move || {
            let mut reader = BatteryReader::with_cache(
                WindowsHidBackend::new(),
                ConfigStore::from_env(),
                mouse_cache,
            );
            let readings = reader.read_all(None);
            if let Ok(mut queue) = queue.lock() {
                queue.push_back(Ok(readings));
            }
        });
        if !started {
            self.battery.set_enabled(true);
            self.append_log("无法启动后台读取任务");
        }
    }

    fn drain_batteries(&mut self) {
        self.battery.set_enabled(true);
        let results = self
            .queue
            .lock()
            .map(|mut queue| queue.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();
        for result in results {
            match result {
                Ok(readings) => {
                    for reading in readings {
                        self.append_log(&format!(
                            "{}{}",
                            reading.line(),
                            if reading.message.is_empty() {
                                String::new()
                            } else {
                                format!(" [{}]", status_text(reading.status))
                            }
                        ));
                    }
                    self.update_status();
                }
                Err(error) => self.append_log(&format!("读取电量失败：{error}")),
            }
        }
    }

    fn save_calibration(&mut self) {
        let parse = |text: String, name: &str| -> std::result::Result<i64, String> {
            let text = text.trim().to_owned();
            if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
                i64::from_str_radix(hex, 16).map_err(|_| format!("{name} 不是有效整数"))
            } else {
                text.parse::<i64>()
                    .map_err(|_| format!("{name} 不是有效整数"))
            }
        };
        let values = (|| -> std::result::Result<Value, String> {
            let battery = parse(self.mouse_battery.text(), "鼠标电量偏移")?.clamp(0, 63);
            let device = parse(self.mouse_device.text(), "鼠标通道")?.clamp(0, 255);
            let cmd = parse(self.headphone_cmd.text(), "耳机命令")?.clamp(0, 255);
            let level = parse(self.headphone_level.text(), "耳机电量偏移")?.clamp(0, 63);
            Ok(serde_json::json!({
                "mouse_battery_index": battery,
                "mouse_device_index": device,
                "headphone_cmd": cmd,
                "headphone_level_index": level,
            }))
        })();
        match values {
            Ok(new_values) => {
                let store = ConfigStore::from_env();
                let mut calibration = serde_json::to_value(store.calibration())
                    .unwrap_or_else(|_| serde_json::json!({}));
                if let (Some(target), Some(source)) =
                    (calibration.as_object_mut(), new_values.as_object())
                {
                    for (key, value) in source {
                        target.insert(key.clone(), value.clone());
                    }
                }
                match store.update([("calibration".to_owned(), calibration)]) {
                    Ok(()) => {
                        self.append_log("标定已保存。");
                        self.status.set_text("标定已保存。");
                    }
                    Err(error) => self.status.set_text(&format!("保存标定失败：{error}")),
                }
            }
            Err(error) => self.status.set_text(&error),
        }
    }

    fn close(&mut self) {
        if self.closing {
            return;
        }
        self.closing = true;
        nwg::stop_thread_dispatch();
    }

    fn cleanup(&mut self) {
        for handler in self.handlers.drain(..) {
            nwg::unbind_event_handler(&handler);
        }
    }
}

impl Drop for DebugState {
    fn drop(&mut self) {
        if !self.closing {
            self.cleanup();
        }
    }
}
