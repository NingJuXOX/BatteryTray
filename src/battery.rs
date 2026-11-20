use crate::config::ConfigStore;
use crate::error::{BatteryTrayError, Result};
use crate::profiles::{enabled_profiles, DeviceProfile};
use crate::protocol::{decode_generic_response, parse_hex, Reading, ReadingStatus};
use chrono::{DateTime, Local};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a second process may wait for a device already being queried.
///
/// The panel no longer reads HID while the tray runs, so contention is rare;
/// a short wait avoids freezing a window for eight seconds on a stale lock.
const DEVICE_LOCK_TIMEOUT_MS: u32 = 2500;

/// Pause between polls when a read reported "nothing yet".
///
/// `read_input` returns `Ok(None)` immediately while the driver still owns the
/// previous request, so the polling loops below used to spin a whole core for
/// the length of the timeout.
const READ_POLL_BACKOFF_MS: u32 = 10;

/// Windows error codes that are worth a second attempt.
///
/// Anything else — a refused handle, a rejected report, a broken request — is
/// reproduced identically by the retry, which then only doubles the cost of the
/// refresh (another open, another settle delay, another timeout).
const TRANSIENT_ERRORS: [u32; 7] = [
    21,   // ERROR_NOT_READY
    31,   // ERROR_GEN_FAILURE
    170,  // ERROR_BUSY
    258,  // ERROR_NO_MORE_ITEMS, used here for "the driver still owns the read"
    995,  // ERROR_OPERATION_ABORTED
    1167, // ERROR_DEVICE_NOT_CONNECTED
    1460, // ERROR_TIMEOUT, reported by the overlapped IOCTL path
];

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
pub trait HidBackend: Send {
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
    /// Creates an independent backend for another thread.
    ///
    /// A device is queried, and its lock released, on the thread that opened it,
    /// so a parallel refresh needs one backend per worker instead of sharing
    /// one. Stateful backends should hand out a handle to their shared state.
    fn duplicate(&self) -> Self
    where
        Self: Sized;
}

/// One successful percentage, kept so an idle device still shows a number.
#[derive(Debug, Clone)]
struct CachedReading {
    percent: u8,
    updated: DateTime<Local>,
    acquired: Instant,
}

/// Process-local cache of the last successful percentage per device key.
///
/// Sleep, offline and error results carry no percentage of their own. Without
/// this cache the tray falls back to 「--」 a few seconds after an idle
/// mouse parks its radio, even though a percentage was read moments earlier.
#[derive(Debug, Clone, Default)]
pub struct ReadingCache {
    entries: HashMap<String, CachedReading>,
}

impl ReadingCache {
    /// Stores the newest percentage for one device key.
    fn remember(&mut self, key: &str, percent: u8, updated: DateTime<Local>) {
        self.entries.insert(
            key.to_owned(),
            CachedReading {
                percent,
                updated,
                acquired: Instant::now(),
            },
        );
    }

    /// Returns the cached percentage, when it was read, and how long ago.
    fn recall(&self, key: &str) -> Option<(u8, DateTime<Local>, Duration)> {
        self.entries
            .get(key)
            .map(|entry| (entry.percent, entry.updated, entry.acquired.elapsed()))
    }
}

/// Shared process-local cache of the last successful reading per device.
pub type SharedReadingCache = Arc<Mutex<ReadingCache>>;

/// Renders a cache age as 「8 秒」 or 「3 分钟」.
fn age_text(age: Duration) -> String {
    if age.as_secs() < 60 {
        format!("{} 秒", age.as_secs().max(1))
    } else {
        format!("{} 分钟", (age.as_secs() / 60).max(1))
    }
}

/// Last Windows error reported by any device opened by one reader.
///
/// A [`Reading`] only carries a human-readable message, so the retry decision is
/// made from the OS error code instead. The tracker below records it after every
/// HID call; the reader inspects it once an attempt has failed.
#[derive(Clone, Default)]
struct DeviceError(Arc<AtomicU32>);

impl DeviceError {
    fn clear(&self) {
        self.0.store(0, Ordering::Relaxed);
    }

    fn store(&self, code: u32) {
        self.0.store(code, Ordering::Relaxed);
    }

    fn load(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Forwards HID calls while recording the device's last error code.
struct TrackedDevice {
    inner: Box<dyn HidDevice>,
    error: DeviceError,
}

impl TrackedDevice {
    fn record<T>(&self, result: Result<T>) -> Result<T> {
        self.error.store(self.inner.last_error());
        result
    }
}

impl HidDevice for TrackedDevice {
    fn set_feature(&mut self, data: &[u8]) -> Result<bool> {
        let result = self.inner.set_feature(data);
        self.record(result)
    }

    fn get_feature(&mut self, length: Option<usize>, report_id: u8) -> Result<Option<Vec<u8>>> {
        let result = self.inner.get_feature(length, report_id);
        self.record(result)
    }

    fn write_output(&mut self, data: &[u8]) -> Result<bool> {
        let result = self.inner.write_output(data);
        self.record(result)
    }

    fn read_input(&mut self, timeout_ms: u32) -> Result<Option<Vec<u8>>> {
        let result = self.inner.read_input(timeout_ms);
        self.record(result)
    }

    fn last_error(&self) -> u32 {
        self.inner.last_error()
    }
}

/// Whether a failed attempt may succeed when repeated right away.
fn is_transient_error(code: u32) -> bool {
    TRANSIENT_ERRORS.contains(&code)
}

/// Configuration-driven battery reader with a single enumeration per refresh.
pub struct BatteryReader<B> {
    backend: B,
    config: ConfigStore,
    reading_cache: SharedReadingCache,
    debug: bool,
    device_error: DeviceError,
}

impl<B: HidBackend> BatteryReader<B> {
    /// Creates a reader over a backend and configuration store.
    pub fn new(backend: B, config: ConfigStore) -> Self {
        Self {
            backend,
            config,
            reading_cache: Arc::new(Mutex::new(ReadingCache::default())),
            debug: false,
            device_error: DeviceError::default(),
        }
    }

    /// Creates a reader that shares the reading cache with other workers.
    pub fn with_cache(backend: B, config: ConfigStore, cache: SharedReadingCache) -> Self {
        Self {
            backend,
            config,
            reading_cache: cache,
            debug: false,
            device_error: DeviceError::default(),
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
    ///
    /// Every device is queried on its own thread. A sleeping or absent device
    /// spends its whole timeout waiting, so reading 32 of them in sequence could
    /// take half a minute and made the tray look frozen. The result keeps the
    /// order of `profiles`.
    ///
    /// Locks are taken inside the worker threads on purpose: a Windows mutex
    /// must be released by the thread that acquired it, so neither the lock nor
    /// the device handle can be shared.
    pub fn read_profiles_with_interfaces(
        &mut self,
        profiles: &[DeviceProfile],
        interfaces: &[HidInterface],
    ) -> Vec<Reading> {
        if profiles.len() < 2 {
            return profiles
                .iter()
                .map(|profile| self.read_with_retry(profile, interfaces))
                .collect();
        }
        let workers = profiles.iter().map(|_| self.worker()).collect::<Vec<_>>();
        let mut results: Vec<Option<Reading>> = profiles.iter().map(|_| None).collect();
        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(profiles.len());
            for (index, (worker, profile)) in workers.into_iter().zip(profiles).enumerate() {
                let task = move || {
                    let mut worker = worker;
                    (index, worker.read_with_retry(profile, interfaces))
                };
                match std::thread::Builder::new().spawn_scoped(scope, task) {
                    Ok(handle) => handles.push(handle),
                    Err(error) => eprintln!("无法启动设备读取线程：{error}"),
                }
            }
            for handle in handles {
                match handle.join() {
                    Ok((index, reading)) => results[index] = Some(reading),
                    Err(_) => eprintln!("设备读取线程意外退出，稍后串行重试"),
                }
            }
        });
        let mut readings = Vec::with_capacity(profiles.len());
        for (index, profile) in profiles.iter().enumerate() {
            let reading = match results[index].take() {
                Some(reading) => reading,
                None => self.worker().read_with_retry(profile, interfaces),
            };
            readings.push(reading);
        }
        readings
    }

    /// Builds a reader that owns its own backend for one worker thread.
    fn worker(&self) -> Self {
        Self {
            backend: self.backend.duplicate(),
            config: self.config.clone(),
            reading_cache: Arc::clone(&self.reading_cache),
            debug: self.debug,
            device_error: DeviceError::default(),
        }
    }

    /// Opens an interface and records the device's error code on failure.
    fn open_interface(
        &mut self,
        interface: &HidInterface,
        options: OpenOptions,
    ) -> Result<Box<dyn HidDevice>> {
        match self.backend.open(interface, options) {
            Ok(device) => Ok(Box::new(TrackedDevice {
                inner: device,
                error: self.device_error.clone(),
            })),
            Err(error) => {
                // The backend reports an open failure as a message, not as a
                // Windows code: treat it as permanent rather than retry it.
                self.device_error.clear();
                Err(error)
            }
        }
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
                return self.with_cached_percent(Reading::new(
                    profile,
                    None,
                    ReadingStatus::Busy,
                    "设备正被另一个窗口读取，请稍后重试",
                ))
            }
            Err(error) => {
                return self.with_cached_percent(Reading::new(
                    profile,
                    None,
                    ReadingStatus::Error,
                    format!("获取设备锁失败：{error}"),
                ))
            }
        };
        let _lock = lock;
        self.device_error.clear();
        let first = self.dispatch(profile, interfaces);
        if first.status != ReadingStatus::Error {
            return self.with_cached_percent(first);
        }
        let code = self.device_error.load();
        if !is_transient_error(code) {
            if self.debug {
                eprintln!(
                    "[{}] 状态 error（{}，错误码 {code}），不重试",
                    profile.key, first.message
                );
            }
            return self.with_cached_percent(first);
        }
        if self.debug {
            eprintln!(
                "[{}] 状态 error（{}，错误码 {code}），重试一次",
                profile.key, first.message
            );
        }
        self.device_error.clear();
        let retried = self.dispatch(profile, interfaces);
        self.with_cached_percent(retried)
    }

    /// Remembers a fresh percentage, and fills in the previous one otherwise.
    ///
    /// The number no longer disappears while a 2.4G device parks its radio: the
    /// receiver keeps answering and reports the device as asleep, so the last
    /// reading is shown again with a note saying how old it is. That case is
    /// downgraded from 「离线」 to 「休眠」 as well — an idle device is not a failed
    /// read, and it must not count towards the failure streak that pauses
    /// automatic refreshing.
    fn with_cached_percent(&self, mut reading: Reading) -> Reading {
        let Ok(mut cache) = self.reading_cache.lock() else {
            return reading;
        };
        if matches!(
            reading.status,
            ReadingStatus::Ok | ReadingStatus::Charging | ReadingStatus::Sleeping
        ) {
            if let Some(percent) = reading.percent {
                cache.remember(&reading.key, percent, reading.updated);
            }
            return reading;
        }
        if reading.percent.is_some() {
            return reading;
        }
        let Some((percent, updated, age)) = cache.recall(&reading.key) else {
            return reading;
        };
        reading.percent = Some(percent);
        reading.updated = updated;
        if reading.status == ReadingStatus::Offline {
            reading.status = ReadingStatus::Sleeping;
        }
        let note = format!("显示 {}前的读数", age_text(age));
        reading.message = if reading.message.trim().is_empty() {
            note
        } else {
            format!("{}（{note}）", reading.message.trim())
        };
        reading
            .detail
            .insert("cached".to_owned(), Value::from(true));
        reading
            .detail
            .insert("age".to_owned(), Value::from(age.as_secs_f64()));
        reading
    }

    fn dispatch(&mut self, profile: &DeviceProfile, interfaces: &[HidInterface]) -> Reading {
        match profile.protocol.as_str() {
            "hid_feature" | "hid_output_input" => self.read_generic(profile, interfaces),
            protocol => Reading::new(
                profile,
                None,
                ReadingStatus::Unsupported,
                format!("不支持的协议：{protocol}"),
            ),
        }
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
        let mut device = match self.open_interface(
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
                Ok(Some(response)) => decode_with_sleep_fallback(profile, &response),
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
                    let reading = decode_with_sleep_fallback(profile, &data);
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
                Ok(None) => {
                    // Nothing arrived yet: back off instead of polling flat out.
                    std::thread::sleep(Duration::from_millis(u64::from(
                        remaining.min(READ_POLL_BACKOFF_MS),
                    )));
                }
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

/// Decodes a response, but keeps the level a sleeping device still reports.
///
/// A 2.4G receiver keeps answering after its device parks the radio, and the
/// frame it returns still holds the last level it saw — only the "online" (or
/// "valid") flag flips. Reporting "--" in that case throws away a number the
/// hardware just handed us, so the offsets are decoded a second time with those
/// flags ignored and the result is labelled 「休眠」: the device did not answer,
/// which must not count towards the failure streak either.
fn decode_with_sleep_fallback(profile: &DeviceProfile, response: &[u8]) -> Reading {
    let reading = decode_generic_response(profile, response);
    if reading.status != ReadingStatus::Offline {
        return reading;
    }
    let mut relaxed = profile.clone();
    relaxed.response.online_offset = -1;
    relaxed.response.online_equals = -1;
    relaxed.response.valid_offset = -1;
    relaxed.response.valid_equals = -1;
    let mut sleeping = decode_generic_response(&relaxed, response);
    if sleeping.percent.is_none() || sleeping.status != ReadingStatus::Ok {
        return reading;
    }
    sleeping.status = ReadingStatus::Sleeping;
    sleeping.message = "设备已休眠，显示接收器报告的最后电量".to_owned();
    sleeping
        .detail
        .insert("online".to_owned(), Value::from(false));
    sleeping
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

/// Reports whether the enumeration still contains an interface that matches
/// the profile.
///
/// A 2.4G receiver is a USB device of its own, so unplugging it removes every
/// matching interface even though the profile stays in the config. The tray
/// uses this to hide the icon instead of leaving a dead 「--」 behind.
pub fn has_matching_interface(profile: &DeviceProfile, interfaces: &[HidInterface]) -> bool {
    !matching_interfaces(profile, interfaces).is_empty()
}

/// Returns whether `interface` is a Bluetooth HID node.
///
/// USB devices are enumerated as `\\?\hid#vid_1234&pid_5678&mi_02#...`, while a
/// Bluetooth node carries a service GUID right after `hid#`
/// (`\\?\hid#{00001124-0000-1000-8000-00805f9b34fb}_vid&...`). Windows drops a
/// Bluetooth node from the device list as soon as the device sleeps, so its
/// presence says nothing about whether the hardware is still around.
pub fn is_bluetooth_interface(interface: &HidInterface) -> bool {
    match interface.path.to_ascii_lowercase().split_once("hid#") {
        Some((_, rest)) => rest.starts_with('{'),
        None => false,
    }
}

/// Returns whether any interface matched by `profile` is a Bluetooth node.
pub fn has_bluetooth_interface(profile: &DeviceProfile, interfaces: &[HidInterface]) -> bool {
    matching_interfaces(profile, interfaces)
        .iter()
        .any(|item| is_bluetooth_interface(item))
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

        fn duplicate(&self) -> Self {
            Self::default()
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
            Ok(vec![test_interface()])
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

        fn duplicate(&self) -> Self {
            Self {
                response: self.response.clone(),
            }
        }
    }

    /// Backend whose device always fails with a fixed Windows error code.
    struct FailingDevice {
        error: u32,
    }

    impl HidDevice for FailingDevice {
        fn set_feature(&mut self, _data: &[u8]) -> Result<bool> {
            Ok(false)
        }

        fn get_feature(
            &mut self,
            _length: Option<usize>,
            _report_id: u8,
        ) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }

        fn write_output(&mut self, _data: &[u8]) -> Result<bool> {
            Ok(false)
        }

        fn read_input(&mut self, _timeout_ms: u32) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }

        fn last_error(&self) -> u32 {
            self.error
        }
    }

    struct FailingBackend {
        error: u32,
        opens: Arc<AtomicU32>,
    }

    impl HidBackend for FailingBackend {
        fn enumerate(&mut self) -> Result<Vec<HidInterface>> {
            Ok(vec![test_interface()])
        }

        fn open(
            &mut self,
            _interface: &HidInterface,
            _options: OpenOptions,
        ) -> Result<Box<dyn HidDevice>> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(FailingDevice { error: self.error }))
        }

        fn lock(&mut self, _key: &str, _timeout_ms: u32) -> Result<Option<Box<dyn HidLock>>> {
            Ok(Some(Box::new(EmptyLock)))
        }

        fn duplicate(&self) -> Self {
            Self {
                error: self.error,
                opens: Arc::clone(&self.opens),
            }
        }
    }

    fn test_interface() -> HidInterface {
        HidInterface {
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
        }
    }

    #[test]
    fn retry_only_repeats_transient_errors() {
        let Some(profile) = sample_profiles().into_iter().next() else {
            return;
        };
        let temporary = tempfile::tempdir();
        assert!(temporary.is_ok());
        let Some(store) = temporary
            .ok()
            .map(|dir| ConfigStore::new(dir.path().join("BatteryTray")))
        else {
            return;
        };
        // 1 is ERROR_INVALID_FUNCTION: repeating it cannot help. 258 means the
        // driver still owned the read, so a second attempt is worthwhile.
        for (error, expected) in [(1_u32, 1_u32), (258, 2)] {
            let opens = Arc::new(AtomicU32::new(0));
            let backend = FailingBackend {
                error,
                opens: Arc::clone(&opens),
            };
            let mut reader = BatteryReader::new(backend, store.clone());
            let reading = reader.read_profile(&profile);
            assert_eq!(reading.status, ReadingStatus::Error);
            assert_eq!(
                opens.load(Ordering::Relaxed),
                expected,
                "error code {error} must open the device {expected} time(s)"
            );
        }
    }

    #[test]
    fn parallel_reads_keep_the_configured_order() {
        let temporary = tempfile::tempdir();
        assert!(temporary.is_ok());
        let Some(store) = temporary
            .ok()
            .map(|dir| ConfigStore::new(dir.path().join("BatteryTray")))
        else {
            return;
        };
        let profiles = sample_profiles();
        assert!(profiles.len() >= 2);
        let mut reader = BatteryReader::new(EmptyBackend::default(), store);
        let readings = reader.read_profiles_with_interfaces(&profiles, &[]);
        let keys = readings
            .iter()
            .map(|reading| reading.key.clone())
            .collect::<Vec<_>>();
        let expected = profiles
            .iter()
            .map(|profile| profile.key.clone())
            .collect::<Vec<_>>();
        assert_eq!(keys, expected);
        assert!(readings
            .iter()
            .all(|reading| reading.status == ReadingStatus::NotFound));
    }

    #[test]
    fn device_lock_key_identifies_hardware_not_profile_key() {
        let interface = test_interface();
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
        let cache = SharedReadingCache::default();
        let mut fresh_reader = BatteryReader::with_cache(
            ResponseBackend {
                response: Some(response),
            },
            store.clone(),
            Arc::clone(&cache),
        );
        let fresh = fresh_reader.read_profile(&profile);
        assert_eq!(fresh.percent, Some(81));
        let _acquired_at = fresh.updated;
        // The receiver still answers after the mouse parks its radio, and the
        // frame it returns holds the level it last saw with the online flag
        // flipped. That number must be shown, not thrown away.
        let mut idle = vec![0_u8; 65];
        idle[1] = 1;
        idle[3] = 66;
        idle[5] = 1;
        let mut sleeping_reader = BatteryReader::with_cache(
            ResponseBackend {
                response: Some(idle),
            },
            store.clone(),
            Arc::clone(&cache),
        );
        let sleeping = sleeping_reader.read_profile(&profile);
        assert_eq!(sleeping.percent, Some(66));
        assert_eq!(sleeping.status, ReadingStatus::Sleeping);
        assert!(sleeping.message.contains("休眠"));

        // Silence after that point falls back to the last level, annotated.
        let mut silent_reader =
            BatteryReader::with_cache(ResponseBackend { response: None }, store, cache);
        let silent = silent_reader.read_profile(&profile);
        assert_eq!(silent.percent, Some(66));
        assert_eq!(silent.status, ReadingStatus::Error);
        assert!(silent.detail.contains_key("cached"));
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
