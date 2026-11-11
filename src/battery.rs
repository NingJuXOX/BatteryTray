use crate::config::ConfigStore;
use crate::error::{BatteryTrayError, Result};
use crate::profiles::{enabled_profiles, DeviceProfile};
use crate::protocol::{
    build_darkrim_request, decode_generic_response, parse_darkrim_frame, parse_hex, Reading,
    ReadingStatus, DARKRIM_CMD_DONGLE_STATUS, DARKRIM_OP_READ,
};
use chrono::{DateTime, Local};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a second process may wait for a device already being queried.
///
/// The panel no longer reads HID while the tray runs, so contention is rare;
/// a short wait avoids freezing a window for eight seconds on a stale lock.
const DEVICE_LOCK_TIMEOUT_MS: u32 = 2500;

/// Description of one present HID interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidInterface {
    /// Windows device-interface path.
    pub path: String,
    /// USB vendor ID.
    pub vendor_id: u16,
    /// USB product ID.
    pub product_id: u16,
    /// Product string.
    pub product: String,
    /// Manufacturer string.
    pub manufacturer: String,
    /// Serial string kept only in memory.
    pub serial: String,
    /// HID usage page.
    pub usage_page: u16,
    /// HID usage.
    pub usage: u16,
    /// Input report byte length.
    pub input_len: u16,
    /// Output report byte length.
    pub output_len: u16,
    /// Feature report byte length.
    pub feature_len: u16,
    /// USB interface number, or -1 when unavailable.
    pub interface_number: i16,
}

impl HidInterface {
    /// Returns `VID:PID` in uppercase hexadecimal.
    pub fn vid_pid(&self) -> String {
        format!("{:04X}:{:04X}", self.vendor_id, self.product_id)
    }

    /// Returns a compact display label.
    pub fn label(&self) -> String {
        let name = if !self.product.is_empty() {
            &self.product
        } else if !self.manufacturer.is_empty() {
            &self.manufacturer
        } else {
            "HID 设备"
        };
        format!("{name} ({})", self.vid_pid())
    }
}

/// Access mode used when opening an interface.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenOptions {
    /// Open a command/feature handle with write access.
    pub write: bool,
    /// Open a separate overlapped input handle.
    pub read: bool,
}

/// Abstract HID report device used by protocol code and fake-device tests.
pub trait HidDevice: Send {
    /// Sends a feature report.
    fn set_feature(&mut self, data: &[u8]) -> Result<bool>;
    /// Reads a feature report, including its report ID.
    fn get_feature(&mut self, length: Option<usize>, report_id: u8) -> Result<Option<Vec<u8>>>;
    /// Writes an output report.
    fn write_output(&mut self, data: &[u8]) -> Result<bool>;
    /// Reads one input report with a timeout.
    fn read_input(&mut self, timeout_ms: u32) -> Result<Option<Vec<u8>>>;
    /// Returns the last Windows error code, when available.
    fn last_error(&self) -> u32 {
        0
    }
}

/// RAII marker for a held per-device cross-process lock.
///
/// Deliberately not `Send`: a Windows mutex must be released by the thread that
/// acquired it, so the lock cannot be moved across threads.
pub trait HidLock {}

/// Platform backend used by [`BatteryReader`].
pub trait HidBackend {
    /// Enumerates present interfaces.
    fn enumerate(&mut self) -> Result<Vec<HidInterface>>;
    /// Opens an interface.
    fn open(
        &mut self,
        interface: &HidInterface,
        options: OpenOptions,
    ) -> Result<Box<dyn HidDevice>>;
    /// Attempts to acquire a device lock before timeout.
    fn lock(&mut self, key: &str, timeout_ms: u32) -> Result<Option<Box<dyn HidLock>>>;
}

/// Last valid mouse reading retained while the mouse is sleeping.
#[derive(Debug, Clone, Default)]
pub struct MouseCache {
    percent: Option<u8>,
    updated: Option<DateTime<Local>>,
    acquired: Option<Instant>,
}

/// Shared process-local cache used to retain the last valid mouse reading.
pub type SharedMouseCache = Arc<Mutex<MouseCache>>;

/// Configuration-driven battery reader with a single enumeration per refresh.
pub struct BatteryReader<B> {
    backend: B,
    config: ConfigStore,
    mouse_cache: SharedMouseCache,
    debug: bool,
}

impl<B: HidBackend> BatteryReader<B> {
    /// Creates a reader over a backend and configuration store.
    pub fn new(backend: B, config: ConfigStore) -> Self {
        Self {
            backend,
            config,
            mouse_cache: Arc::new(Mutex::new(MouseCache::default())),
            debug: false,
        }
    }

    /// Creates a reader that shares the mouse sleep cache with other workers.
    pub fn with_cache(backend: B, config: ConfigStore, cache: SharedMouseCache) -> Self {
        Self {
            backend,
            config,
            mouse_cache: cache,
            debug: false,
        }
    }

    /// Enables or disables diagnostics on stderr.
    pub fn set_debug(&mut self, debug: bool) {
        self.debug = debug;
    }

    /// Reads selected enabled devices and enumerates at most once.
    pub fn read_all(&mut self, keys: Option<&[String]>) -> Vec<Reading> {
        let profiles = enabled_profiles(&crate::profiles::load_profiles(&self.config));
        let wanted = select_profiles(profiles, keys);
        self.read_profiles(&wanted)
    }

    /// Reads an already loaded profile list, enumerating HID at most once.
    ///
    /// Lets callers that just loaded the configuration reuse that snapshot
    /// instead of hitting the disk once per refresh and once per device.
    pub fn read_profiles(&mut self, profiles: &[DeviceProfile]) -> Vec<Reading> {
        if profiles.is_empty() {
            return Vec::new();
        }
        match self.backend.enumerate() {
            Ok(interfaces) => self.read_profiles_with_interfaces(profiles, &interfaces),
            Err(error) => profiles
                .iter()
                .map(|profile| self.enumeration_error(profile, &error))
                .collect(),
        }
    }

    /// Reads explicit profiles using an existing interface snapshot.
    pub fn read_profiles_with_interfaces(
        &mut self,
        profiles: &[DeviceProfile],
        interfaces: &[HidInterface],
    ) -> Vec<Reading> {
        profiles
            .iter()
            .map(|profile| self.read_with_retry(profile, interfaces))
            .collect()
    }

    fn enumeration_error(&self, profile: &DeviceProfile, error: &BatteryTrayError) -> Reading {
        let mut reading = Reading::new(
            profile,
            None,
            ReadingStatus::Error,
            format!("HID 接口枚举失败：{error}"),
        );
        reading.source = "Windows HID".to_owned();
        reading
    }

    /// Reads selected devices using an existing interface snapshot.
    pub fn read_all_with_interfaces(
        &mut self,
        keys: Option<&[String]>,
        interfaces: &[HidInterface],
    ) -> Vec<Reading> {
        let wanted = select_profiles(
            enabled_profiles(&crate::profiles::load_profiles(&self.config)),
            keys,
        );
        self.read_profiles_with_interfaces(&wanted, interfaces)
    }

    /// Reads one configured device, including its disabled state.
    pub fn read_device(&mut self, key: &str) -> Option<Reading> {
        let profile = crate::profiles::load_profiles(&self.config)
            .into_iter()
            .find(|item| item.key == key)?;
        if !profile.enabled {
            return Some(Reading::new(
                &profile,
                None,
                ReadingStatus::Disabled,
                "该设备已关闭查询",
            ));
        }
        match self.backend.enumerate() {
            Ok(interfaces) => Some(self.read_with_retry(&profile, &interfaces)),
            Err(error) => Some(Reading::new(
                &profile,
                None,
                ReadingStatus::Error,
                format!("HID 接口枚举失败：{error}"),
            )),
        }
    }

    /// Tests an unsaved profile in the device manager.
    pub fn read_profile(&mut self, profile: &DeviceProfile) -> Reading {
        match self.backend.enumerate() {
            Ok(interfaces) => self.read_with_retry(profile, &interfaces),
            Err(error) => Reading::new(
                profile,
                None,
                ReadingStatus::Error,
                format!("HID 接口枚举失败：{error}"),
            ),
        }
    }

    fn read_with_retry(&mut self, profile: &DeviceProfile, interfaces: &[HidInterface]) -> Reading {
        // Two profiles can point at the same receiver, and a device can be
        // renamed in the config. Keying the lock on the hardware identity
        // instead of the profile key makes the lock actually protect the HID
        // handle from concurrent opens.
        let lock_key = device_lock_key(profile, interfaces);
        let lock = match self.backend.lock(&lock_key, DEVICE_LOCK_TIMEOUT_MS) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                return Reading::new(
                    profile,
                    None,
                    ReadingStatus::Busy,
                    "设备正被另一个窗口读取，请稍后重试",
                )
            }
            Err(error) => {
                return Reading::new(
                    profile,
                    None,
                    ReadingStatus::Error,
                    format!("获取设备锁失败：{error}"),
                )
            }
        };
        let _lock = lock;
        // Calibration is shared by every protocol in this refresh; load it
        // once here instead of once per protocol handler (and once per retry).
        let calibration = self.config.calibration();
        let first = self.dispatch(profile, interfaces, &calibration);
        if first.status != ReadingStatus::Error {
            return first;
        }
        if self.debug {
            eprintln!(
                "[{}] 状态 error（{}），重试一次",
                profile.key, first.message
            );
        }
        self.dispatch(profile, interfaces, &calibration)
    }

    fn dispatch(
        &mut self,
        profile: &DeviceProfile,
        interfaces: &[HidInterface],
        calibration: &Map<String, Value>,
    ) -> Reading {
        match profile.protocol.as_str() {
            "vk_mouse" => {
                let mut reading = self.read_mouse(profile, interfaces, calibration);
                reading.name = profile.name.clone();
                reading
            }
            "darkrim" => {
                let mut reading = self.read_headphone(profile, interfaces, calibration);
                reading.name = profile.name.clone();
                reading
            }
            "hid_feature" | "hid_output_input" => self.read_generic(profile, interfaces),
            protocol => Reading::new(
                profile,
                None,
                ReadingStatus::Unsupported,
                format!("不支持的协议：{protocol}"),
            ),
        }
    }

    fn read_mouse(
        &mut self,
        profile: &DeviceProfile,
        interfaces: &[HidInterface],
        calibration: &Map<String, Value>,
    ) -> Reading {
        let matches = matching_interfaces(profile, interfaces);
        let Some(interface) = matches.first() else {
            return Reading::new(
                profile,
                None,
                ReadingStatus::NotFound,
                "未找到鼠标接收器，请插好 2.4G 接收器",
            );
        };
        let battery_index = calibration_index(calibration, "mouse_battery_index", 2, 63);
        let device_index = calibration_index(calibration, "mouse_device_index", 2, 255) as u8;
        let mut device = match self.backend.open(
            interface,
            OpenOptions {
                write: true,
                read: false,
            },
        ) {
            Ok(device) => device,
            Err(error) => {
                return Reading::new(
                    profile,
                    None,
                    ReadingStatus::Error,
                    format!("无法打开鼠标控制接口（{error}）"),
                )
            }
        };
        let mut frame = vec![0_u8; usize::from(interface.feature_len).max(2)];
        frame[1] = 0xF7;
        match device.set_feature(&frame) {
            Ok(true) => {}
            Ok(false) => {
                return Reading::new(
                    profile,
                    None,
                    ReadingStatus::Error,
                    format!("鼠标接口写入失败（错误 {}）", device.last_error()),
                )
            }
            Err(error) => {
                return Reading::new(
                    profile,
                    None,
                    ReadingStatus::Error,
                    format!("鼠标接口写入失败：{error}"),
                )
            }
        }
        std::thread::sleep(Duration::from_millis(u64::from(profile.request.settle_ms)));
        let response = match device.get_feature(profile.request.read_len.checked_into(), 0) {
            Ok(response) => response,
            Err(error) => {
                return Reading::new(
                    profile,
                    None,
                    ReadingStatus::Error,
                    format!("读取鼠标 Feature 失败：{error}"),
                )
            }
        };
        if self.debug {
            eprintln!("[mouse] F7 -> {response:?}");
        }
        let Some(response) = response.filter(|data| data.len() >= 10) else {
            return self.mouse_sleeping(profile);
        };
        let data = &response[1..];
        if data.len() < 9 || !data.iter().take(10).any(|byte| *byte != 0) {
            return self.mouse_sleeping(profile);
        }
        if !matches!(data[6], 0 | 3) && data[6] != device_index {
            return self.mouse_sleeping(profile);
        }
        let online = data[4] == 0;
        if !online {
            let mut reading = Reading::new(
                profile,
                None,
                ReadingStatus::Offline,
                "鼠标已关机或未连接到接收器",
            );
            reading.source = "HID Feature 0xF7".to_owned();
            reading
                .detail
                .insert("online".to_owned(), Value::from(false));
            reading.detail.insert(
                "raw".to_owned(),
                Value::from(crate::protocol::hex_dump(&response)),
            );
            return reading;
        }
        let percent = data
            .get(battery_index)
            .copied()
            .filter(|value| (1..=100).contains(value));
        let Some(percent) = percent else {
            return self.mouse_sleeping(profile);
        };
        let updated = Local::now();
        if let Ok(mut cache) = self.mouse_cache.lock() {
            *cache = MouseCache {
                percent: Some(percent),
                updated: Some(updated),
                acquired: Some(Instant::now()),
            };
        }
        let mut reading = Reading::new(profile, Some(percent), ReadingStatus::Ok, "");
        reading.source = "HID Feature 0xF7".to_owned();
        reading.updated = updated;
        reading
            .detail
            .insert("online".to_owned(), Value::from(true));
        reading
            .detail
            .insert("can_read".to_owned(), Value::from(data[0] == 1));
        reading.detail.insert(
            "raw".to_owned(),
            Value::from(crate::protocol::hex_dump(&response)),
        );
        reading
    }

    fn mouse_sleeping(&self, profile: &DeviceProfile) -> Reading {
        let cached =
            self.mouse_cache.lock().ok().and_then(|cache| {
                Some((cache.percent?, cache.updated?, cache.acquired?.elapsed()))
            });
        if let Some((percent, updated, age)) = cached {
            if age < Duration::from_secs(600) {
                let age_text = if age.as_secs() < 60 {
                    format!("{} 秒", age.as_secs().max(1))
                } else {
                    format!("{} 分钟", (age.as_secs() / 60).max(1))
                };
                let mut reading = Reading::new(
                    profile,
                    Some(percent),
                    ReadingStatus::Sleeping,
                    format!("鼠标休眠中，当前显示 {age_text}前的缓存"),
                );
                reading.source = "HID Feature 0xF7（缓存）".to_owned();
                reading.updated = updated;
                reading
                    .detail
                    .insert("cached".to_owned(), Value::from(true));
                reading
                    .detail
                    .insert("age".to_owned(), Value::from(age.as_secs_f64()));
                return reading;
            }
        }
        Reading::new(
            profile,
            None,
            ReadingStatus::Sleeping,
            "鼠标休眠中，动一下鼠标再试",
        )
    }

    fn read_headphone(
        &mut self,
        profile: &DeviceProfile,
        interfaces: &[HidInterface],
        calibration: &Map<String, Value>,
    ) -> Reading {
        let preferred = matching_interfaces(profile, interfaces);
        let interface = preferred.first().copied().or_else(|| {
            interfaces.iter().find(|item| {
                item.vendor_id == profile.match_.vid
                    && item.product_id == profile.match_.pid
                    && item.input_len == 64
                    && item.output_len == 64
            })
        });
        let Some(interface) = interface else {
            return Reading::new(
                profile,
                None,
                ReadingStatus::NotFound,
                "未找到耳机接收器，请插好 USB 接收器",
            );
        };
        let mut device = match self.backend.open(
            interface,
            OpenOptions {
                write: true,
                read: true,
            },
        ) {
            Ok(device) => device,
            Err(error) => {
                return Reading::new(
                    profile,
                    None,
                    ReadingStatus::Error,
                    format!("无法打开耳机控制接口（{error}）"),
                )
            }
        };
        let cmd = calibration_index(calibration, "headphone_cmd", 0x27, 0xFF) as u8;
        let valid_index = calibration_index(calibration, "headphone_valid_index", 4, 63);
        let level_index = calibration_index(calibration, "headphone_level_index", 5, 63);
        let frame = match darkrim_query(device.as_mut(), cmd, profile.request.settle_ms, 1000) {
            Ok(frame) => frame,
            Err(error) => {
                return Reading::new(
                    profile,
                    None,
                    ReadingStatus::Error,
                    format!("读取失败：{error}"),
                )
            }
        };
        let Some(frame) = frame else {
            let reachable = match darkrim_query(
                device.as_mut(),
                DARKRIM_CMD_DONGLE_STATUS,
                profile.request.settle_ms,
                800,
            ) {
                Ok(Some(frame)) => frame.payload.first().copied().unwrap_or_default() != 0,
                _ => false,
            };
            return if reachable {
                Reading::new(
                    profile,
                    None,
                    ReadingStatus::Error,
                    "接收器在线，但电量命令无响应",
                )
            } else {
                Reading::new(
                    profile,
                    None,
                    ReadingStatus::Offline,
                    "耳机没开机或未连接（dongle 无响应）",
                )
            };
        };
        if frame.payload.len() <= valid_index.max(level_index) {
            return Reading::new(profile, None, ReadingStatus::Error, "电量响应长度异常");
        }
        if frame.payload[valid_index] == 0 {
            return Reading::new(
                profile,
                None,
                ReadingStatus::Offline,
                "耳机未就绪（接收器无有效电量）",
            );
        }
        let percent = frame.payload[level_index];
        if percent > 100 {
            return Reading::new(profile, None, ReadingStatus::Error, "电量值超出范围");
        }
        let mut reading = Reading::new(profile, Some(percent), ReadingStatus::Ok, "");
        reading.source = "DARKRIM 0x27".to_owned();
        reading.detail.insert(
            "raw".to_owned(),
            Value::from(crate::protocol::hex_dump(&frame.payload)),
        );
        reading.detail.insert("firmware".to_owned(), Value::Null);
        reading
    }

    fn read_generic(&mut self, profile: &DeviceProfile, interfaces: &[HidInterface]) -> Reading {
        let matches = matching_interfaces(profile, interfaces);
        let Some(interface) = matches.first() else {
            return Reading::new(
                profile,
                None,
                ReadingStatus::NotFound,
                format!("未找到设备 {}，请检查 VID/PID 和连接状态", profile.name),
            );
        };
        let payload = match parse_hex(&profile.request.bytes_hex) {
            Ok(payload) if !payload.is_empty() => payload,
            Ok(_) => return Reading::new(profile, None, ReadingStatus::Error, "没有配置请求字节"),
            Err(error) => {
                return Reading::new(
                    profile,
                    None,
                    ReadingStatus::Error,
                    format!("请求字节无效：{error}"),
                )
            }
        };
        let output_input = profile.protocol == "hid_output_input";
        let mut device = match self.backend.open(
            interface,
            OpenOptions {
                write: true,
                read: output_input,
            },
        ) {
            Ok(device) => device,
            Err(error) => {
                return Reading::new(
                    profile,
                    None,
                    ReadingStatus::Error,
                    format!("无法打开控制接口（{error}）"),
                )
            }
        };
        if !output_input {
            match device.set_feature(&payload) {
                Ok(true) => {}
                Ok(false) => {
                    return Reading::new(profile, None, ReadingStatus::Error, "Feature 写入失败")
                }
                Err(error) => {
                    return Reading::new(
                        profile,
                        None,
                        ReadingStatus::Error,
                        format!("Feature 写入失败：{error}"),
                    )
                }
            }
            std::thread::sleep(Duration::from_millis(u64::from(profile.request.settle_ms)));
            let length =
                (profile.request.read_len > 0).then_some(usize::from(profile.request.read_len));
            return match device.get_feature(length, 0) {
                Ok(Some(response)) => decode_generic_response(profile, &response),
                Ok(None) => {
                    Reading::new(profile, None, ReadingStatus::Error, "没有返回 Feature 响应")
                }
                Err(error) => Reading::new(
                    profile,
                    None,
                    ReadingStatus::Error,
                    format!("Feature 读取失败：{error}"),
                ),
            };
        }
        match device.write_output(&payload) {
            Ok(true) => {}
            Ok(false) => {
                return Reading::new(profile, None, ReadingStatus::Error, "Output 写入失败")
            }
            Err(error) => {
                return Reading::new(
                    profile,
                    None,
                    ReadingStatus::Error,
                    format!("Output 写入失败：{error}"),
                )
            }
        }
        std::thread::sleep(Duration::from_millis(u64::from(profile.request.settle_ms)));
        let deadline =
            Instant::now() + Duration::from_millis(u64::from(profile.request.timeout_ms));
        let mut last_reason = "没有返回输入报告".to_owned();
        while Instant::now() < deadline {
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .clamp(10, u128::from(u32::MAX)) as u32;
            match device.read_input(remaining) {
                Ok(Some(data)) => {
                    if profile.response.report_id >= 0
                        && data.first().copied() != Some(profile.response.report_id as u8)
                    {
                        continue;
                    }
                    let reading = decode_generic_response(profile, &data);
                    if matches!(
                        reading.status,
                        ReadingStatus::Ok | ReadingStatus::Charging | ReadingStatus::Offline
                    ) {
                        return reading;
                    }
                    if !reading.message.is_empty() {
                        last_reason = reading.message;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    return Reading::new(
                        profile,
                        None,
                        ReadingStatus::Error,
                        format!("Input 读取失败：{error}"),
                    )
                }
            }
        }
        Reading::new(
            profile,
            None,
            ReadingStatus::Offline,
            format!("设备没有返回电量响应（{last_reason}）"),
        )
    }
}

trait CheckedInto {
    fn checked_into(self) -> Option<usize>;
}

impl CheckedInto for u16 {
    fn checked_into(self) -> Option<usize> {
        (self > 0).then_some(usize::from(self))
    }
}

fn calibration_index(
    calibration: &Map<String, Value>,
    key: &str,
    default: usize,
    maximum: usize,
) -> usize {
    let parsed = calibration.get(key).and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_str().and_then(|text| text.parse::<i64>().ok()))
    });
    parsed
        .filter(|value| *value >= 0 && *value <= maximum as i64)
        .map(|value| value as usize)
        .unwrap_or(default)
}

fn select_profiles(profiles: Vec<DeviceProfile>, keys: Option<&[String]>) -> Vec<DeviceProfile> {
    match keys {
        None => profiles,
        Some(keys) => {
            let mut selected = Vec::new();
            for key in keys {
                if !selected.contains(key) && profiles.iter().any(|item| &item.key == key) {
                    selected.push(key.clone());
                }
            }
            profiles
                .into_iter()
                .filter(|item| selected.contains(&item.key))
                .collect()
        }
    }
}

fn matching_interfaces<'a>(
    profile: &DeviceProfile,
    interfaces: &'a [HidInterface],
) -> Vec<&'a HidInterface> {
    interfaces
        .iter()
        .filter(|item| {
            let rules = &profile.match_;
            (rules.vid == 0 || item.vendor_id == rules.vid)
                && (rules.pid == 0 || item.product_id == rules.pid)
                && rules
                    .usage_page
                    .is_none_or(|value| item.usage_page == value)
                && rules.usage.is_none_or(|value| item.usage == value)
                && (rules.interface_number < 0 || item.interface_number == rules.interface_number)
                && (rules.product_contains.is_empty()
                    || item
                        .product
                        .to_lowercase()
                        .contains(&rules.product_contains.to_lowercase()))
                && (rules.input_len == 0 || item.input_len == rules.input_len)
                && (rules.output_len == 0 || item.output_len == rules.output_len)
                && (rules.feature_len == 0 || item.feature_len == rules.feature_len)
        })
        .collect()
}

/// Builds a stable, hardware-based key for the cross-process HID lock.
///
/// Prefer the concrete interface path (hashed, because Windows paths contain
/// characters that are illegal in a mutex name). When the device is not
/// present, fall back to the match rule so the lock still exists and still
/// agrees between processes with different profile keys.
fn device_lock_key(profile: &DeviceProfile, interfaces: &[HidInterface]) -> String {
    if let Some(interface) = matching_interfaces(profile, interfaces).first() {
        let digest = Sha256::digest(interface.path.to_lowercase().as_bytes());
        let short = digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        return format!("if-{short}");
    }
    let rules = &profile.match_;
    format!(
        "rule-{:04X}-{:04X}-{:04X}-{:04X}-{}",
        rules.vid,
        rules.pid,
        rules.usage_page.unwrap_or(0),
        rules.usage.unwrap_or(0),
        rules.interface_number
    )
}

fn darkrim_query(
    device: &mut dyn HidDevice,
    cmd: u8,
    settle_ms: u32,
    timeout_ms: u32,
) -> Result<Option<crate::protocol::DarkrimFrame>> {
    let request = build_darkrim_request(cmd, &[])?;
    if settle_ms > 0 {
        std::thread::sleep(Duration::from_millis(u64::from(settle_ms)));
    }
    if !device.write_output(&request)? {
        return Ok(None);
    }
    let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms));
    while Instant::now() < deadline {
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .clamp(10, u128::from(u32::MAX)) as u32;
        let Some(data) = device.read_input(remaining)? else {
            continue;
        };
        if let Some(frame) = parse_darkrim_frame(&data) {
            if frame.op == DARKRIM_OP_READ && frame.cmd == cmd {
                return Ok(Some(frame));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::sample_profiles;

    #[derive(Default)]
    struct EmptyBackend {
        enumerations: usize,
    }

    struct EmptyLock;
    impl HidLock for EmptyLock {}

    impl HidBackend for EmptyBackend {
        fn enumerate(&mut self) -> Result<Vec<HidInterface>> {
            self.enumerations += 1;
            Ok(Vec::new())
        }

        fn open(
            &mut self,
            _interface: &HidInterface,
            _options: OpenOptions,
        ) -> Result<Box<dyn HidDevice>> {
            Err(crate::error::BatteryTrayError::Hid("no device".to_owned()))
        }

        fn lock(&mut self, _key: &str, _timeout_ms: u32) -> Result<Option<Box<dyn HidLock>>> {
            Ok(Some(Box::new(EmptyLock)))
        }
    }

    struct ResponseDevice {
        response: Option<Vec<u8>>,
    }

    impl HidDevice for ResponseDevice {
        fn set_feature(&mut self, _data: &[u8]) -> Result<bool> {
            Ok(true)
        }

        fn get_feature(
            &mut self,
            _length: Option<usize>,
            _report_id: u8,
        ) -> Result<Option<Vec<u8>>> {
            Ok(self.response.clone())
        }

        fn write_output(&mut self, _data: &[u8]) -> Result<bool> {
            Ok(false)
        }

        fn read_input(&mut self, _timeout_ms: u32) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }
    }

    struct ResponseBackend {
        response: Option<Vec<u8>>,
    }

    impl HidBackend for ResponseBackend {
        fn enumerate(&mut self) -> Result<Vec<HidInterface>> {
            Ok(vec![HidInterface {
                path: "mouse".to_owned(),
                vendor_id: 0x1234,
                product_id: 0x5678,
                product: "Test Mouse".to_owned(),
                manufacturer: String::new(),
                serial: String::new(),
                usage_page: 0xFFFF,
                usage: 0x0002,
                input_len: 0,
                output_len: 0,
                feature_len: 65,
                interface_number: 2,
            }])
        }

        fn open(
            &mut self,
            _interface: &HidInterface,
            _options: OpenOptions,
        ) -> Result<Box<dyn HidDevice>> {
            Ok(Box::new(ResponseDevice {
                response: self.response.clone(),
            }))
        }

        fn lock(&mut self, _key: &str, _timeout_ms: u32) -> Result<Option<Box<dyn HidLock>>> {
            Ok(Some(Box::new(EmptyLock)))
        }
    }

    #[test]
    fn device_lock_key_identifies_hardware_not_profile_key() {
        let interface = HidInterface {
            path: "mouse".to_owned(),
            vendor_id: 0x1234,
            product_id: 0x5678,
            product: "Test Mouse".to_owned(),
            manufacturer: String::new(),
            serial: String::new(),
            usage_page: 0xFFFF,
            usage: 0x0002,
            input_len: 0,
            output_len: 0,
            feature_len: 65,
            interface_number: 2,
        };
        let profile = sample_profiles()
            .into_iter()
            .next()
            .unwrap_or_else(|| unreachable!());
        let mut renamed = profile.clone();
        renamed.key = "another-profile-key".to_owned();
        let with_device = device_lock_key(&profile, std::slice::from_ref(&interface));
        assert_eq!(
            with_device,
            device_lock_key(&renamed, std::slice::from_ref(&interface))
        );
        assert!(with_device.starts_with("if-"));

        let mut other_path = interface;
        other_path.path = "mouse-2".to_owned();
        assert_ne!(with_device, device_lock_key(&profile, &[other_path]));

        let fallback = device_lock_key(&profile, &[]);
        assert!(fallback.starts_with("rule-"));
        assert_eq!(fallback, device_lock_key(&renamed, &[]));
    }

    #[test]
    fn mouse_sleep_cache_persists_across_reader_workers() {
        let temporary = tempfile::tempdir();
        assert!(temporary.is_ok());
        let Some(directory) = temporary.ok() else {
            return;
        };
        let store = ConfigStore::new(directory.path().join("BatteryTray"));
        let Some(profile) = sample_profiles().into_iter().next() else {
            return;
        };
        let mut response = vec![0_u8; 65];
        response[0] = 0;
        response[1] = 1;
        response[3] = 81;
        response[5] = 0;
        response[7] = 2;
        let cache = SharedMouseCache::default();
        let mut fresh_reader = BatteryReader::with_cache(
            ResponseBackend {
                response: Some(response),
            },
            store.clone(),
            Arc::clone(&cache),
        );
        let fresh = fresh_reader.read_profile(&profile);
        assert_eq!(fresh.percent, Some(81));
        let acquired_at = fresh.updated;
        let mut sleeping_reader =
            BatteryReader::with_cache(ResponseBackend { response: None }, store, cache);
        let sleeping = sleeping_reader.read_profile(&profile);
        assert_eq!(sleeping.percent, Some(81));
        assert_eq!(sleeping.status, ReadingStatus::Sleeping);
        assert_eq!(sleeping.updated, acquired_at);
        assert!(sleeping.message.contains("缓存"));
    }

    #[test]
    fn shared_enumeration_and_key_filtering() {
        let temporary = tempfile::tempdir();
        assert!(temporary.is_ok());
        let Some(store) = temporary
            .ok()
            .map(|dir| ConfigStore::new(dir.path().join("BatteryTray")))
        else {
            return;
        };
        assert!(crate::profiles::save_profiles(&store, &sample_profiles()).is_ok());
        let mut reader = BatteryReader::new(EmptyBackend::default(), store);
        let readings = reader.read_all(None);
        assert_eq!(readings.len(), 2);
        assert!(readings
            .iter()
            .all(|item| item.status == ReadingStatus::NotFound));
        assert_eq!(reader.backend.enumerations, 1);
        let selected = reader.read_all(Some(&["mouse".to_owned(), "unknown".to_owned()]));
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].key, "mouse");
        assert_eq!(reader.backend.enumerations, 2);
        assert!(reader.read_all(Some(&[])).is_empty());
        assert_eq!(reader.backend.enumerations, 2);
    }
}
