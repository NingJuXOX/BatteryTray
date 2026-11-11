use crate::config::ConfigStore;
use crate::error::{BatteryTrayError, Result};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Current device-list schema version.
pub const DEVICES_VERSION: u64 = 1;
/// Maximum number of configured devices.
pub const MAX_PROFILES: usize = 32;
/// Supported protocol identifiers.
pub const SUPPORTED_PROTOCOLS: &[&str] =
    &["vk_mouse", "darkrim", "hid_feature", "hid_output_input"];
/// Generic protocol identifiers.
pub const CUSTOM_PROTOCOLS: &[&str] = &["hid_feature", "hid_output_input"];
/// Supported icon identifiers.
pub const ICON_KINDS: &[&str] = &[
    "mouse",
    "keyboard",
    "headphone",
    "gamepad",
    "device",
    "custom",
];

/// HID interface matching rules.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MatchConfig {
    /// USB vendor ID; zero is invalid when saving.
    pub vid: u16,
    /// USB product ID; zero is invalid when saving.
    pub pid: u16,
    /// Optional HID usage page.
    pub usage_page: Option<u16>,
    /// Optional HID usage.
    pub usage: Option<u16>,
    /// USB interface number, or -1 for any.
    pub interface_number: i16,
    /// Optional case-insensitive product-name substring.
    pub product_contains: String,
    /// Required input report length; zero disables the check.
    pub input_len: u16,
    /// Required output report length; zero disables the check.
    pub output_len: u16,
    /// Required feature report length; zero disables the check.
    pub feature_len: u16,
}

impl Default for MatchConfig {
    fn default() -> Self {
        Self {
            vid: 0,
            pid: 0,
            usage_page: None,
            usage: None,
            interface_number: -1,
            product_contains: String::new(),
            input_len: 0,
            output_len: 0,
            feature_len: 0,
        }
    }
}

/// Request bytes and timing for a device protocol.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RequestConfig {
    /// Full request report in hexadecimal, including report ID.
    pub bytes_hex: String,
    /// Delay between write and read.
    pub settle_ms: u32,
    /// Input response timeout.
    pub timeout_ms: u32,
    /// Optional feature report length override.
    pub feature_len: u16,
    /// Optional feature read length override.
    pub read_len: u16,
    /// Optional built-in protocol command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmd: Option<u8>,
}

impl Default for RequestConfig {
    fn default() -> Self {
        Self {
            bytes_hex: String::new(),
            settle_ms: 20,
            timeout_ms: 1000,
            feature_len: 0,
            read_len: 0,
            cmd: None,
        }
    }
}

/// Response field offsets and transformations.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ResponseConfig {
    /// Battery byte offset in the complete response.
    pub battery_offset: i32,
    /// Number of bytes holding the raw battery value (1 or 2).
    pub battery_bytes: u8,
    /// Byte order for multi-byte values: `le` (default) or `be`.
    pub battery_endian: String,
    /// Optional bit mask applied to the raw value; 0 means no mask.
    pub battery_mask: u16,
    /// Optional right shift applied after masking.
    pub battery_shift: u8,
    /// Multiplier applied to the raw battery byte.
    pub battery_scale: f64,
    /// Additive adjustment after scaling.
    pub battery_adjust: i32,
    /// Optional valid-flag offset; -1 disables it.
    pub valid_offset: i32,
    /// Expected valid byte; -1 disables the check.
    pub valid_equals: i32,
    /// Optional online-flag offset; -1 disables it.
    pub online_offset: i32,
    /// Expected online byte; -1 disables the check.
    pub online_equals: i32,
    /// Optional charging-flags offset; -1 disables it.
    pub charging_offset: i32,
    /// Charging bit mask.
    pub charging_mask: u8,
    /// Expected masked charging value.
    pub charging_equals: u8,
    /// Expected input report ID; -1 accepts any.
    pub report_id: i32,
}

impl Default for ResponseConfig {
    fn default() -> Self {
        Self {
            battery_offset: 0,
            battery_bytes: 1,
            battery_endian: "le".to_owned(),
            battery_mask: 0,
            battery_shift: 0,
            battery_scale: 1.0,
            battery_adjust: 0,
            valid_offset: -1,
            valid_equals: -1,
            online_offset: -1,
            online_equals: -1,
            charging_offset: -1,
            charging_mask: 0,
            charging_equals: 0,
            report_id: -1,
        }
    }
}

/// A complete device definition compatible with the Python configuration.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DeviceProfile {
    /// Stable internal key.
    pub key: String,
    /// User-facing device name.
    pub name: String,
    /// Protocol identifier.
    pub protocol: String,
    /// Whether this device participates in queries.
    pub enabled: bool,
    /// Whether this device has a notification-area icon.
    pub show_in_tray: bool,
    /// Notification-area icon ID.
    pub uid: u32,
    /// Built-in shape or `custom`.
    pub icon: String,
    /// Absolute custom-image path.
    pub icon_path: String,
    /// HID matching rules.
    #[serde(rename = "match")]
    pub match_: MatchConfig,
    /// Request settings.
    pub request: RequestConfig,
    /// Response settings.
    pub response: ResponseConfig,
    /// Unknown top-level fields preserved for forward compatibility.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl DeviceProfile {
    /// Serializes using the legacy `match` field name.
    pub fn to_value(&self) -> Result<Value> {
        // `match_` carries `#[serde(rename = "match")]`, so every serializer
        // path writes the legacy name without a second remapping step.
        Ok(serde_json::to_value(self)?)
    }
}

fn parse_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|item| i64::try_from(item).ok())),
        Value::String(text) => {
            let text = text.trim();
            if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
                i64::from_str_radix(hex, 16).ok()
            } else {
                text.parse().ok()
            }
        }
        _ => None,
    }
}

fn integer(map: &Map<String, Value>, key: &str, default: i64, low: i64, high: i64) -> i64 {
    map.get(key)
        .and_then(parse_int)
        .unwrap_or(default)
        .clamp(low, high)
}

fn optional_integer(map: &Map<String, Value>, key: &str) -> Option<i64> {
    match map.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text.trim().is_empty() => None,
        value => value.and_then(parse_int),
    }
}

fn float(map: &Map<String, Value>, key: &str, default: f64) -> f64 {
    let value = match map.get(key) {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse().ok(),
        _ => None,
    };
    value.filter(|number| number.is_finite()).unwrap_or(default)
}

fn text(map: &Map<String, Value>, key: &str, default: &str) -> String {
    map.get(key)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .trim()
        .to_owned()
}

fn boolean(map: &Map<String, Value>, key: &str, default: bool) -> bool {
    map.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn absolute_path(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let expanded = if text == "~" || text.starts_with("~/") || text.starts_with("~\\") {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .map(|home| home.join(text.trim_start_matches('~').trim_start_matches(['/', '\\'])))
            .unwrap_or_else(|| PathBuf::from(text))
    } else {
        PathBuf::from(text)
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().unwrap_or_default().join(expanded)
    };
    absolute.to_string_lossy().chars().take(1024).collect()
}

/// Returns the two built-in device definitions.
pub fn default_profiles() -> Vec<DeviceProfile> {
    let mut mouse = custom_profile("mouse", "瓦尔基里 VK M5", 1);
    mouse.protocol = "vk_mouse".to_owned();
    mouse.icon = "mouse".to_owned();
    mouse.match_ = MatchConfig {
        vid: 0x374A,
        pid: 0xA322,
        usage_page: Some(0xFFFF),
        usage: Some(0x0002),
        ..MatchConfig::default()
    };
    mouse.request = RequestConfig {
        bytes_hex: "00 F7".to_owned(),
        settle_ms: 50,
        timeout_ms: 1000,
        ..RequestConfig::default()
    };
    mouse.response = ResponseConfig {
        battery_offset: 3,
        online_offset: 5,
        online_equals: 0,
        report_id: 0,
        ..ResponseConfig::default()
    };
    let mut headphone = custom_profile("headphone", "水月雨 GM01 Pro", 2);
    headphone.protocol = "darkrim".to_owned();
    headphone.icon = "headphone".to_owned();
    headphone.match_ = MatchConfig {
        vid: 0x35D8,
        pid: 0x0133,
        usage_page: Some(0x000C),
        usage: Some(0x0001),
        input_len: 64,
        output_len: 64,
        ..MatchConfig::default()
    };
    headphone.request = RequestConfig {
        cmd: Some(0x27),
        timeout_ms: 1200,
        ..RequestConfig::default()
    };
    headphone.response = ResponseConfig {
        battery_offset: 9,
        valid_offset: 8,
        valid_equals: 1,
        report_id: 0x4B,
        ..ResponseConfig::default()
    };
    vec![mouse, headphone]
}

/// Creates an empty generic device with a unique key and icon ID.
pub fn custom_profile(key: &str, name: &str, uid: u32) -> DeviceProfile {
    DeviceProfile {
        key: key.to_owned(),
        name: name.to_owned(),
        protocol: "hid_feature".to_owned(),
        enabled: true,
        show_in_tray: true,
        uid: uid.max(1),
        icon: "device".to_owned(),
        icon_path: String::new(),
        match_: MatchConfig::default(),
        request: RequestConfig::default(),
        response: ResponseConfig::default(),
        extra: BTreeMap::new(),
    }
}

fn normalize_match(raw: Option<&Map<String, Value>>, base: &MatchConfig) -> MatchConfig {
    let mut data = serde_json::to_value(base)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if let Some(raw) = raw {
        data.extend(raw.clone());
    }
    MatchConfig {
        vid: integer(&data, "vid", 0, 0, 0xFFFF) as u16,
        pid: integer(&data, "pid", 0, 0, 0xFFFF) as u16,
        usage_page: optional_integer(&data, "usage_page")
            .filter(|value| (0..=0xFFFF).contains(value))
            .map(|value| value as u16),
        usage: optional_integer(&data, "usage")
            .filter(|value| (0..=0xFFFF).contains(value))
            .map(|value| value as u16),
        interface_number: integer(&data, "interface_number", -1, -1, 255) as i16,
        product_contains: text(&data, "product_contains", "")
            .chars()
            .take(80)
            .collect(),
        input_len: integer(&data, "input_len", 0, 0, 4096) as u16,
        output_len: integer(&data, "output_len", 0, 0, 4096) as u16,
        feature_len: integer(&data, "feature_len", 0, 0, 4096) as u16,
    }
}

fn normalize_request(raw: Option<&Map<String, Value>>, base: &RequestConfig) -> RequestConfig {
    let mut data = serde_json::to_value(base)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if let Some(raw) = raw {
        data.extend(raw.clone());
    }
    RequestConfig {
        bytes_hex: text(&data, "bytes_hex", ""),
        settle_ms: integer(&data, "settle_ms", 20, 0, 5000) as u32,
        timeout_ms: integer(&data, "timeout_ms", 1000, 50, 10000) as u32,
        feature_len: integer(&data, "feature_len", 0, 0, 4096) as u16,
        read_len: integer(&data, "read_len", 0, 0, 4096) as u16,
        cmd: optional_integer(&data, "cmd").map(|value| value.clamp(0, 255) as u8),
    }
}

fn normalize_response(raw: Option<&Map<String, Value>>, base: &ResponseConfig) -> ResponseConfig {
    let mut data = serde_json::to_value(base)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if let Some(raw) = raw {
        data.extend(raw.clone());
    }
    ResponseConfig {
        battery_offset: integer(&data, "battery_offset", 0, 0, 1023) as i32,
        battery_bytes: integer(&data, "battery_bytes", 1, 1, 2) as u8,
        battery_endian: if text(&data, "battery_endian", "le").eq_ignore_ascii_case("be") {
            "be".to_owned()
        } else {
            "le".to_owned()
        },
        battery_mask: integer(&data, "battery_mask", 0, 0, 65535) as u16,
        battery_shift: integer(&data, "battery_shift", 0, 0, 15) as u8,
        battery_scale: float(&data, "battery_scale", 1.0),
        battery_adjust: integer(&data, "battery_adjust", 0, -10000, 10000) as i32,
        valid_offset: integer(&data, "valid_offset", -1, -1, 1023) as i32,
        valid_equals: integer(&data, "valid_equals", -1, -1, 255) as i32,
        online_offset: integer(&data, "online_offset", -1, -1, 1023) as i32,
        online_equals: integer(&data, "online_equals", -1, -1, 255) as i32,
        charging_offset: integer(&data, "charging_offset", -1, -1, 1023) as i32,
        charging_mask: integer(&data, "charging_mask", 0, 0, 255) as u8,
        charging_equals: integer(&data, "charging_equals", 0, 0, 255) as u8,
        report_id: integer(&data, "report_id", -1, -1, 255) as i32,
    }
}

/// Normalizes one raw JSON profile, optionally overriding key and icon ID.
pub fn normalize_profile(raw: &Value, key: Option<&str>, uid: Option<u32>) -> DeviceProfile {
    let source = raw.as_object().cloned().unwrap_or_default();
    let resolved_key = key
        .map(str::to_owned)
        .or_else(|| source.get("key").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default();
    let base = default_profiles()
        .into_iter()
        .find(|item| item.key == resolved_key)
        .unwrap_or_else(|| custom_profile(&resolved_key, "", uid.unwrap_or(3)));
    let mut data = base
        .to_value()
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    data.extend(source.clone());
    let enabled = boolean(&data, "enabled", true);
    let icon = text(&data, "icon", "device");
    let known = [
        "key",
        "name",
        "protocol",
        "enabled",
        "show_in_tray",
        "uid",
        "icon",
        "icon_path",
        "match",
        "request",
        "response",
    ];
    let extra = source
        .iter()
        .filter(|(field, _)| !known.contains(&field.as_str()))
        .map(|(field, value)| (field.clone(), value.clone()))
        .collect();
    DeviceProfile {
        key: resolved_key,
        name: text(&data, "name", "").chars().take(60).collect(),
        protocol: text(&data, "protocol", ""),
        enabled,
        show_in_tray: enabled && boolean(&data, "show_in_tray", true),
        uid: uid
            .unwrap_or_else(|| integer(&data, "uid", 3, 1, u32::MAX as i64) as u32)
            .max(1),
        icon: if ICON_KINDS.contains(&icon.as_str()) {
            icon
        } else {
            "device".to_owned()
        },
        icon_path: absolute_path(&text(&data, "icon_path", "")),
        match_: normalize_match(data.get("match").and_then(Value::as_object), &base.match_),
        request: normalize_request(
            data.get("request").and_then(Value::as_object),
            &base.request,
        ),
        response: normalize_response(
            data.get("response").and_then(Value::as_object),
            &base.response,
        ),
        extra,
    }
}

fn new_key(name: &str, used: &HashSet<String>) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for ch in name.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            separator = false;
        } else if !slug.is_empty() && !separator {
            slug.push('_');
            separator = true;
        }
    }
    let base = slug.trim_matches('_').chars().take(32).collect::<String>();
    let base = if base.is_empty() {
        "device".to_owned()
    } else {
        base
    };
    let mut candidate = base.clone();
    let mut index = 2_u32;
    while used.contains(&candidate) {
        candidate = format!("{base}_{index}");
        index = index.saturating_add(1);
    }
    candidate
}

/// Normalizes, deduplicates and bounds a raw device list.
pub fn normalize_profiles(raw: &[Value]) -> Vec<DeviceProfile> {
    let mut used_keys = HashSet::new();
    let mut used_uids: HashSet<u32> = HashSet::new();
    let mut result = Vec::new();
    for value in raw.iter().take(MAX_PROFILES) {
        let Some(object) = value.as_object() else {
            continue;
        };
        let raw_key = object
            .get("key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let name = object.get("name").and_then(Value::as_str).unwrap_or("");
        let key = if !raw_key.is_empty() && !used_keys.contains(raw_key) {
            raw_key.to_owned()
        } else {
            new_key(name, &used_keys)
        };
        let requested_uid = object
            .get("uid")
            .and_then(parse_int)
            .filter(|uid| *uid >= 1)
            .map(|uid| uid as u32);
        let mut next_uid = used_uids
            .iter()
            .max()
            .copied()
            .unwrap_or_default()
            .saturating_add(1)
            .max(1);
        let uid = requested_uid
            .filter(|uid| !used_uids.contains(uid))
            .unwrap_or_else(|| {
                while used_uids.contains(&next_uid) {
                    next_uid = next_uid.saturating_add(1);
                }
                next_uid
            });
        let profile = normalize_profile(value, Some(&key), Some(uid));
        used_keys.insert(profile.key.clone());
        used_uids.insert(profile.uid);
        result.push(profile);
    }
    result
}

/// Loads profiles, falling back to built-ins for legacy or malformed config.
pub fn load_profiles(store: &ConfigStore) -> Vec<DeviceProfile> {
    let config = store.load();
    let raw = config
        .get("devices")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if raw.is_empty() {
        return default_profiles();
    }
    let normalized = normalize_profiles(&raw);
    if normalized.is_empty() {
        default_profiles()
    } else {
        normalized
    }
}

/// Returns only devices enabled for querying.
pub fn enabled_profiles(profiles: &[DeviceProfile]) -> Vec<DeviceProfile> {
    profiles
        .iter()
        .filter(|item| item.enabled)
        .cloned()
        .collect()
}

/// Returns devices that are enabled and visible in the notification area.
pub fn tray_profiles(profiles: &[DeviceProfile]) -> Vec<DeviceProfile> {
    profiles
        .iter()
        .filter(|item| item.enabled && item.show_in_tray)
        .cloned()
        .collect()
}

/// Validates one device and returns user-facing errors.
pub fn validate_profile(profile: &DeviceProfile) -> Vec<String> {
    let mut errors = Vec::new();
    let name = if profile.name.trim().is_empty() {
        "设备"
    } else {
        profile.name.trim()
    };
    if profile.name.trim().is_empty() {
        errors.push("设备名称不能为空".to_owned());
    }
    if profile.key.trim().is_empty() {
        errors.push(format!("{name} 缺少内部 key"));
    }
    if profile.match_.vid == 0 {
        errors.push(format!("{name} 的 VID 必须是 1..65535"));
    }
    if profile.match_.pid == 0 {
        errors.push(format!("{name} 的 PID 必须是 1..65535"));
    }
    if !SUPPORTED_PROTOCOLS.contains(&profile.protocol.as_str()) {
        errors.push(format!("{name} 的协议类型不受支持：{}", profile.protocol));
    }
    if profile.icon == "custom" {
        if let Err(error) = crate::icon::validate_image_file(Path::new(&profile.icon_path)) {
            errors.push(format!("{name} 的图标无法读取：{error}"));
        }
    }
    if CUSTOM_PROTOCOLS.contains(&profile.protocol.as_str()) {
        match crate::protocol::parse_hex(&profile.request.bytes_hex) {
            Ok(payload) if payload.is_empty() => errors.push(format!("{name} 缺少请求字节")),
            Ok(payload) if payload.len() > 4096 => errors.push(format!("{name} 的请求字节过长")),
            Ok(_) => {}
            Err(error) => errors.push(format!("{name}：{error}")),
        }
        if profile.response.battery_scale == 0.0 || !profile.response.battery_scale.is_finite() {
            errors.push(format!("{name} 的 battery_scale 不能为 0"));
        }
        let max_offset = (profile.response.battery_offset
            + i32::from(profile.response.battery_bytes.max(1))
            - 1)
        .max(profile.response.valid_offset)
        .max(profile.response.online_offset)
        .max(profile.response.charging_offset);
        let match_len = profile
            .match_
            .input_len
            .max(profile.match_.output_len)
            .max(profile.match_.feature_len) as i32;
        if match_len > 0 && max_offset >= match_len {
            errors.push(format!(
                "{name} 的响应偏移 {max_offset} 超出接口长度 {match_len}"
            ));
        }
    }
    errors
}

/// Validates the complete list, including unique IDs and a usable tray entry.
pub fn validate_profiles(profiles: &[DeviceProfile]) -> Vec<String> {
    if profiles.is_empty() {
        return vec!["至少需要保留一个设备".to_owned()];
    }
    let mut errors = Vec::new();
    let mut keys = HashSet::new();
    let mut uids = HashSet::new();
    for profile in profiles {
        errors.extend(validate_profile(profile));
        if !keys.insert(&profile.key) {
            errors.push(format!("设备 key 重复：{}", profile.key));
        }
        if !uids.insert(profile.uid) {
            errors.push(format!("托盘图标 ID 重复：{}", profile.uid));
        }
    }
    if !profiles
        .iter()
        .any(|item| item.enabled && item.show_in_tray)
    {
        errors.push("至少需要一台“参与查询且在托盘显示”的设备，否则无法操作托盘".to_owned());
    }
    errors.sort();
    errors.dedup();
    errors
}

fn materialize_icons(store: &ConfigStore, profiles: &mut [DeviceProfile]) -> Result<()> {
    let directory = store.directory().join("icons");
    for profile in profiles.iter_mut().filter(|item| item.icon == "custom") {
        let source = PathBuf::from(&profile.icon_path);
        crate::icon::validate_image_file(&source)?;
        let bytes = fs::read(&source)?;
        let digest = Sha256::digest(&bytes);
        let digest_short = digest[..6]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let safe_key: String = profile
            .key
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        let extension = source
            .extension()
            .and_then(|item| item.to_str())
            .unwrap_or("png")
            .to_lowercase();
        let destination = directory.join(format!("{safe_key}_{digest_short}.{extension}"));
        if source != destination {
            fs::create_dir_all(&directory)?;
            fs::copy(&source, &destination)?;
        }
        profile.icon_path = destination.to_string_lossy().into_owned();
    }
    Ok(())
}

/// Validates and saves profiles without overwriting unrelated configuration.
pub fn save_profiles(
    store: &ConfigStore,
    profiles: &[DeviceProfile],
) -> Result<Vec<DeviceProfile>> {
    let raw = profiles
        .iter()
        .map(DeviceProfile::to_value)
        .collect::<Result<Vec<_>>>()?;
    let mut normalized = normalize_profiles(&raw);
    let errors = validate_profiles(&normalized);
    if !errors.is_empty() {
        return Err(BatteryTrayError::Invalid(errors.join("；")));
    }
    materialize_icons(store, &mut normalized)?;
    let errors = validate_profiles(&normalized);
    if !errors.is_empty() {
        return Err(BatteryTrayError::Invalid(errors.join("；")));
    }
    let devices = normalized
        .iter()
        .map(DeviceProfile::to_value)
        .collect::<Result<Vec<_>>>()?;
    store.update([
        ("devices".to_owned(), Value::Array(devices)),
        ("devices_version".to_owned(), Value::from(DEVICES_VERSION)),
    ])?;
    Ok(normalized)
}

/// Builds the device-onboarding prompt copied by the add-device wizard.
///
/// Evidence is gathered by the user (official driver first, then the web HID
/// probe) and pasted into the conversation; no local enumeration data is baked
/// into the prompt, so it stays valid when shared with other people.
const AI_PROMPT_TEMPLATE: &str = r##"你是 Windows USB HID 只读电量协议分析助手。任务：为目标设备确定可导入「电量托盘 BatteryTray」的只读电量查询协议，并在实测确认后生成配置 JSON。

本提示词采用分阶段执行：阶段 A 信息收集与取证、阶段 B 候选协议与 dry-run、阶段 C 用户确认后写入、阶段 D 最终交付。只有阶段 D 才输出纯 JSON；其他阶段允许提问、列证据、给候选、报告失败。不得为了满足“纯 JSON”而跳过取证、伪造实测或猜测字段。

【目标设备与程序】
- 目标设备：{name}
{device_block}
- 操作系统：Windows 10/11
- 主程序：{program_path}（运行时自动填入当前 exe 的完整路径；分发给他人时以其实际放置位置为准）
- 配置位置：%APPDATA%\BatteryTray\config.json
- 调试入口：托盘右键 → 调试版（「粘贴并测试」可导入 JSON 并立即实测）

【硬性约束】
1. 只做只读电量查询；严禁固件升级、恢复出厂、配对清除、重启设备、写存储类命令。
2. 不支持蓝牙 GATT、串口、私有 IOCTL。
3. 不得伪造描述符、请求响应、test_readings 或实测结果；无工具/无权限时输出待执行命令和缺失证据。
4. 无法确认的字段一律填 -1 或 0，绝不猜测。
5. 所有响应偏移一律相对「完整响应」，即包含 Report ID 字节。
6. 命令白名单：
   - hid_feature：SetFeature 仅允许发送查询类请求，GetFeature 读取响应；若无法确认请求不改变持久状态，停止。
   - hid_output_input：仅允许写 Output 查询报文，并在超时内读 Input。
   - 其他写命令一律禁止。

【阶段 A：确认设备与收集证据】
1. 如果用户没有明确说明设备型号、连接方式（2.4G 接收器 / 有线 / 蓝牙）、VID/PID、是否使用独立接收器、目标 HID 接口号(MI)，先问清楚再分析，不要凭名称猜测。
2. 同一型号常有多个 HID 接口，必须确认目标 MI；无法确认时列出候选接口及描述符差异。
3. 优先请用户提供官方驱动或配套软件：安装包、安装后驱动目录、官方调试/配置工具、配置文件或官方下载链接。
4. 优先从官方材料逆向协议：握手与查询命令字、Report ID、报文长度与补齐方式、请求结构、校验和（sum8/xor/累加和）、响应结构、电量字段偏移、在线/充电标志、数值换算。
5. 逆向结论必须标明依据：文件名/函数/字节序列；不要臆测。
6. 用 https://tool.pudiu.com/zh/hid-usb-test.html 验证逆向结论：
   - 对键盘、鼠标等被系统独占的设备使用 WebHID；不要对被内核独占的接口使用 WebUSB 声明。
   - 查看「描述符解析」，记录 VID/PID、接口号(MI)、Usage Page/Usage、Input/Output/Feature 报告长度。
   - 发送报文必须指定 Report ID；无 Report ID 的设备填 0x00；报文长度按接口报告长度用 0x00 补齐。
   - 只发查询类报文；记录每组「发出的完整 Hex」与「收到的完整 Hex（含 Report ID 字节）」。
   - 设备休眠/离线时不回应，先唤醒再重试，不要据此断定协议不支持。
7. 没有官方驱动时，才改用网页多组只读试探，并请用户补充：
   - 描述符解析结果（VID/PID、MI、Usage Page/Usage、三种报告长度）
   - 至少一组「请求 Hex → 响应 Hex」原文
   - 设备当前电量大致范围（用于定位电量字节）
   - 插线/充电变化前后的两组响应（用于定位充电位）
8. 证据不足时先列出缺少项向用户索要，绝不猜测。阶段 A 不输出最终 JSON。

【协议判定与字段语义】
- hid_feature：有 Feature Report，先 SetFeature 发查询请求，再 GetFeature 读响应。
- hid_output_input：无 Feature、有 Output+Input，写 Output 后在超时内读 Input。
- request.bytes_hex：完整发送报文，首字节为 Report ID；无 Report ID 填 00。
- request.feature_len / request.read_len：完整报文长度（含 Report ID）；未知填 0。
- response.report_id：只接受指定 Report ID；-1 表示不限制。
- battery_offset：相对完整响应（含 Report ID）的字节偏移。
- battery_bytes：1 或 2。
- 解码公式：先按 battery_endian 将 battery_bytes 组合为整数 raw；battery_mask=0 表示不掩码，否则 value=(raw & battery_mask)>>battery_shift；电量 = value * battery_scale + battery_adjust。电量合法范围 0–100，超范围判失败。
- valid_offset/valid_equals、online_offset/online_equals：offset=-1 或 equals=-1 表示不使用；使用时应校验原始字节或解码值与 equals 相等。
- charging_offset/charging_mask/charging_equals：charging_mask=0 表示不启用充电判断；启用时比较 ((raw & charging_mask) >> charging_shift?) 与 charging_equals；若无法确认 shift，charging_shift 填 0。
- battery_mask=0 表示不掩码；charging_mask=0 表示不启用。
- 匹配优先级：interface_number 精确 > usage_page/usage > input/output/feature_len > product_contains > vid/pid。多候选时列出候选，不自动写入。

【阶段 B：候选协议与 dry-run】
1. 基于证据给出一个或多个候选 JSON；每个候选必须附证据摘要：文件名/函数/字节序列，或「请求 Hex → 响应 Hex」原文。
2. 将首选候选保存为文件，执行试读，不写配置：
   "C:/Users/admin/Desktop/电量托盘 Rust.exe" --apply-profile <文件> --dry-run
3. 返回的 test_readings 给出每台设备的电量读数与状态。读到电量后立即告诉用户数值，并询问：
   「读到的电量是 XX%，与设备上看到/预期的数值是否一致？这个电量是否合理？」
4. 在用户确认合理之前，不要写入配置。试读失败（离线/超时/数值超范围）时，报告失败原因、缺失证据和下一步核对项，回到阶段 A，不要写入。

【阶段 C：用户确认后写入】
1. 用户确认读数合理后，执行正式写入并再次实测：
   "C:/Users/admin/Desktop/电量托盘 Rust.exe" --apply-profile <文件> --test
2. 报告写入与实测结果。若失败，说明原因并回到取证，不重复盲目写入。
3. 也可提示用户使用托盘右键 →「调试版」→「粘贴并测试」一步完成写入与实测。

【阶段 D：最终交付】
仅当已完成 dry-run、用户确认读数合理、并明确需要最终配置时，才输出最终 JSON。
最终输出要求：只输出一个可导入「电量托盘」的 JSON 对象；不要 Markdown、不要解释、JSON 前后不加任何文字；字段与下方模板一致，不添加额外字段。

【最终 JSON 模板】
{
  "name": "设备名称", "key": "device_key", "protocol": "hid_feature",
  "enabled": true, "show_in_tray": true, "icon": "device",
  "match": {"vid": 0, "pid": 0, "usage_page": null, "usage": null,
    "interface_number": -1, "product_contains": "", "input_len": 0,
    "output_len": 0, "feature_len": 0},
  "request": {"bytes_hex": "00 F7 00", "settle_ms": 50,
    "timeout_ms": 1000, "feature_len": 0, "read_len": 0},
  "response": {"battery_offset": 3, "battery_bytes": 1, "battery_endian": "le",
    "battery_mask": 0, "battery_shift": 0, "battery_scale": 1, "battery_adjust": 0,
    "valid_offset": -1, "valid_equals": -1, "online_offset": -1,
    "online_equals": -1, "charging_offset": -1, "charging_mask": 0,
    "charging_equals": 0, "report_id": -1}
}

【失败与无工具兜底】
- 证据不足：输出信息确认表、证据表、缺失项、下一步需要用户提供的材料或命令；不要输出最终 JSON。
- 无工具/无权限：不得伪造实测；输出候选协议、待执行命令、预期记录项和缺失证据。
- 用户坚持“直接给 JSON”但证据不足：输出缺失项报告，不猜测 JSON。
- 只有阶段 D 才允许纯 JSON 输出。"##;

/// Builds the prompt with the current executable path filled in.
///
/// `evidence` is an optional bullet list describing the device the user just
/// picked (VID/PID, serial, interfaces); it is inserted right below the target
/// device line so the AI never has to guess the hardware.
pub fn make_ai_prompt_with(profile: &DeviceProfile, evidence: &str) -> String {
    let vid_pid = if profile.match_.vid > 0 && profile.match_.pid > 0 {
        format!(
            "VID=0x{:04X}, PID=0x{:04X}",
            profile.match_.vid, profile.match_.pid
        )
    } else {
        "未知，请先确认设备型号与 VID/PID".to_owned()
    };
    let name = if profile.name.is_empty() {
        "未命名设备"
    } else {
        &profile.name
    };
    let program_path = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "电量托盘.exe".to_owned());
    AI_PROMPT_TEMPLATE
        .replace("{name}", name)
        .replace("{vid_pid}", &vid_pid)
        .replace("{program_path}", &program_path)
        .replace("{device_block}", evidence.trim())
}

/// Builds the prompt for a profile without extra device evidence.
pub fn make_ai_prompt(profile: &DeviceProfile) -> String {
    make_ai_prompt_with(profile, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_switches_are_independent() {
        let profiles = default_profiles();
        assert!(validate_profiles(&profiles).is_empty());
        let mut off = profiles[0].clone();
        off.enabled = false;
        off.show_in_tray = true;
        let normalized = normalize_profile(&off.to_value().unwrap_or(Value::Null), None, None);
        assert!(!normalized.enabled);
        assert!(!normalized.show_in_tray);
    }

    #[test]
    fn duplicate_keys_and_uids_are_reassigned() {
        let raw = vec![
            serde_json::json!({"key":"same","name":"A","uid":1,"match":{"vid":1,"pid":2}}),
            serde_json::json!({"key":"same","name":"B","uid":1,"match":{"vid":1,"pid":2}}),
        ];
        let profiles = normalize_profiles(&raw);
        assert_eq!(profiles.len(), 2);
        assert_ne!(profiles[0].key, profiles[1].key);
        assert_ne!(profiles[0].uid, profiles[1].uid);
    }

    #[test]
    fn save_preserves_unrelated_configuration() {
        let temporary = tempfile::tempdir();
        assert!(temporary.is_ok());
        let Some(store) = temporary
            .ok()
            .map(|dir| ConfigStore::new(dir.path().join("BatteryTray")))
        else {
            return;
        };
        assert!(store.set_interval(300).is_ok());
        let saved = save_profiles(&store, &default_profiles());
        assert!(saved.is_ok(), "save failed: {saved:?}");
        assert_eq!(store.load().get("interval"), Some(&Value::from(300)));
        assert_eq!(load_profiles(&store).len(), 2);
    }

    #[test]
    fn prompt_contains_protocol_contract() {
        let prompt = make_ai_prompt(&default_profiles()[0]);
        for token in [
            "阶段 A",
            "battery_offset",
            "bytes_hex",
            "只有阶段 D 才允许纯 JSON 输出",
            "https://tool.pudiu.com/zh/hid-usb-test.html",
            "官方驱动",
            "--dry-run",
            "是否合理",
            ".exe",
        ] {
            assert!(prompt.contains(token), "missing {token}");
        }
        assert!(
            !prompt.contains("{program_path}"),
            "path placeholder unresolved"
        );
        // Local enumeration evidence and the old CLI workflow must be gone.
        assert!(!prompt.contains("【接口证据】"));
        assert!(!prompt.contains("--catalog"));
        assert!(!prompt.contains("battery-tray-cli"));
    }
}
