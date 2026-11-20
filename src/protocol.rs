use crate::error::{BatteryTrayError, Result};
use crate::profiles::DeviceProfile;
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Device reading state shared by tray, panel and CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadingStatus {
    /// A fresh battery value was read.
    Ok,
    /// A fresh battery value was read while charging.
    Charging,
    /// The receiver exists but the device is offline.
    Offline,
    /// No matching HID interface was found.
    NotFound,
    /// The mouse is sleeping and may have a cached value.
    Sleeping,
    /// The configured protocol is unsupported.
    Unsupported,
    /// Another process holds the device lock.
    Busy,
    /// Reading failed.
    Error,
    /// Querying is disabled for this device.
    Disabled,
    /// No state is available.
    Unknown,
}

/// A normalized battery reading.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reading {
    /// Stable device key.
    pub key: String,
    /// Display name.
    pub name: String,
    /// Battery percentage, absent when no current value is available.
    pub percent: Option<u8>,
    /// Reading state.
    pub status: ReadingStatus,
    /// User-facing diagnostic or cache note.
    pub message: String,
    /// Protocol/source label.
    pub source: String,
    /// Original acquisition time.
    pub updated: chrono::DateTime<Local>,
    /// Protocol-specific raw details.
    pub detail: Map<String, Value>,
}

impl Reading {
    /// Creates a reading with the current local time and empty details.
    pub fn new(
        profile: &DeviceProfile,
        percent: Option<u8>,
        status: ReadingStatus,
        message: impl Into<String>,
    ) -> Self {
        Self {
            key: profile.key.clone(),
            name: profile.name.clone(),
            percent,
            status,
            message: message.into(),
            source: String::new(),
            updated: Local::now(),
            detail: Map::new(),
        }
    }

    /// Formats the same one-line output as the Python CLI.
    pub fn line(&self) -> String {
        match self.percent {
            None => format!(
                "{}：--（{}）",
                self.name,
                if self.message.is_empty() {
                    format!("{:?}", self.status)
                } else {
                    self.message.clone()
                }
            ),
            Some(percent) => {
                let charging = if self.status == ReadingStatus::Charging {
                    "，充电中"
                } else {
                    ""
                };
                if self.source.is_empty() {
                    format!("{}：{percent}%{charging}", self.name)
                } else {
                    format!(
                        "{}：{percent}%{charging}（数据来源：{}）",
                        self.name, self.source
                    )
                }
            }
        }
    }
}

/// Parses flexible hexadecimal request text.
pub fn parse_hex(text: &str) -> Result<Vec<u8>> {
    let mut cleaned = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '0' && matches!(chars.peek(), Some('x' | 'X')) {
            chars.next();
            continue;
        }
        if !matches!(ch, ' ' | '\t' | '\r' | '\n' | ',' | ';' | ':' | '_' | '-') {
            cleaned.push(ch);
        }
    }
    if cleaned.is_empty() {
        return Ok(Vec::new());
    }
    if cleaned.len() % 2 != 0 {
        return Err(BatteryTrayError::Invalid("请求字节长度为奇数".to_owned()));
    }
    cleaned
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16).ok_or_else(|| {
                BatteryTrayError::Invalid("请求字节包含非十六进制字符".to_owned())
            })?;
            let low = (pair[1] as char).to_digit(16).ok_or_else(|| {
                BatteryTrayError::Invalid("请求字节包含非十六进制字符".to_owned())
            })?;
            Ok(((high << 4) | low) as u8)
        })
        .collect()
}

/// Converts bytes to uppercase diagnostic hex.
pub fn hex_dump(data: &[u8]) -> String {
    data.iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Decodes a generic custom-HID response according to configured offsets.
pub fn decode_generic_response(profile: &DeviceProfile, response: &[u8]) -> Reading {
    let cfg = &profile.response;
    let source = format!("HID {}", profile.protocol);
    let mut reading = Reading::new(profile, None, ReadingStatus::Error, "");
    reading.source = source;
    reading
        .detail
        .insert("raw".to_owned(), Value::from(hex_dump(response)));
    reading
        .detail
        .insert("protocol".to_owned(), Value::from(profile.protocol.clone()));
    let byte_count = i32::from(cfg.battery_bytes.max(1));
    let battery_last = cfg.battery_offset + byte_count - 1;
    let max_offset = battery_last
        .max(cfg.valid_offset)
        .max(cfg.online_offset)
        .max(cfg.charging_offset);
    if max_offset < 0 || response.len() <= max_offset as usize {
        reading.message = format!(
            "响应长度不足（需要至少 {} 字节，实际 {}）",
            max_offset + 1,
            response.len()
        );
        return reading;
    }
    if cfg.online_offset >= 0 && cfg.online_equals >= 0 {
        let online = response[cfg.online_offset as usize] == cfg.online_equals as u8;
        reading
            .detail
            .insert("online".to_owned(), Value::from(online));
        if !online {
            reading.status = ReadingStatus::Offline;
            reading.message = "设备未连接或不在线".to_owned();
            return reading;
        }
    }
    if cfg.valid_offset >= 0 && cfg.valid_equals >= 0 {
        let valid = response[cfg.valid_offset as usize] == cfg.valid_equals as u8;
        reading
            .detail
            .insert("valid".to_owned(), Value::from(valid));
        if !valid {
            reading.status = ReadingStatus::Offline;
            reading.message = "设备尚未返回有效电量".to_owned();
            return reading;
        }
    }
    let offset = cfg.battery_offset as usize;
    let raw = if byte_count == 2 {
        let first = u16::from(response[offset]);
        let second = u16::from(response[offset + 1]);
        if cfg.battery_endian == "be" {
            (first << 8) | second
        } else {
            first | (second << 8)
        }
    } else {
        u16::from(response[offset])
    };
    let masked = if cfg.battery_mask == 0 {
        raw
    } else {
        raw & cfg.battery_mask
    };
    let value = masked >> cfg.battery_shift.min(15);
    let scaled = f64::from(value) * cfg.battery_scale + f64::from(cfg.battery_adjust);
    if !scaled.is_finite() {
        reading.message = "电量字段解析失败".to_owned();
        return reading;
    }
    let percent = scaled.round() as i32;
    reading
        .detail
        .insert("battery_raw".to_owned(), Value::from(value));
    reading.detail.insert(
        "battery_bytes".to_owned(),
        Value::from(cfg.battery_bytes.max(1)),
    );
    reading
        .detail
        .insert("battery_offset".to_owned(), Value::from(cfg.battery_offset));
    if !(0..=100).contains(&percent) {
        reading.message = format!("电量值超出范围：{percent}");
        return reading;
    }
    reading.percent = Some(percent as u8);
    reading.status = ReadingStatus::Ok;
    if cfg.charging_offset >= 0 && cfg.charging_mask > 0 {
        let value = response[cfg.charging_offset as usize];
        let charging = (value & cfg.charging_mask) == cfg.charging_equals;
        reading
            .detail
            .insert("charging".to_owned(), Value::from(charging));
        if charging {
            reading.status = ReadingStatus::Charging;
        }
    }
    reading
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::custom_profile;

    #[test]
    fn sixteen_bit_and_bitfield_battery_values_are_decoded() {
        let mut profile = custom_profile("wide", "宽字节", 11);
        profile.response.battery_offset = 2;
        profile.response.battery_bytes = 2;

        let reading = decode_generic_response(&profile, &[0x00, 0x00, 0x64, 0x00]);
        assert_eq!(reading.percent, Some(100), "little-endian 16-bit value");

        profile.response.battery_endian = "be".to_owned();
        let reading = decode_generic_response(&profile, &[0x00, 0x00, 0x00, 0x64]);
        assert_eq!(reading.percent, Some(100), "big-endian 16-bit value");

        // Low 7 bits hold the level, bit 7 is the charging flag.
        let mut packed = custom_profile("packed", "位域", 12);
        packed.response.battery_offset = 1;
        packed.response.battery_mask = 0x7F;
        packed.response.charging_offset = 1;
        packed.response.charging_mask = 0x80;
        packed.response.charging_equals = 0x80;
        let reading = decode_generic_response(&packed, &[0x00, 0xE4]);
        assert_eq!(reading.percent, Some(100));
        assert_eq!(reading.status, ReadingStatus::Charging);

        // A shifted field: bits 4..8 hold 0..15 and are scaled to percent.
        let mut shifted = custom_profile("shifted", "移位", 13);
        shifted.response.battery_offset = 0;
        shifted.response.battery_mask = 0xF0;
        shifted.response.battery_shift = 4;
        shifted.response.battery_scale = 10.0;
        let reading = decode_generic_response(&shifted, &[0x50]);
        assert_eq!(reading.percent, Some(50));

        // Two-byte values must be rejected when the frame is too short.
        let reading = decode_generic_response(&profile, &[0x00, 0x00, 0x64]);
        assert_eq!(reading.percent, None);
        assert!(reading.message.contains("响应长度不足"));
    }

    #[test]
    fn generic_decoder_handles_charging_offline_and_short_frames() {
        let mut profile = custom_profile("custom", "测试", 3);
        profile.response.battery_offset = 3;
        profile.response.valid_offset = 1;
        profile.response.valid_equals = 1;
        profile.response.online_offset = 2;
        profile.response.online_equals = 0;
        profile.response.charging_offset = 4;
        profile.response.charging_mask = 1;
        profile.response.charging_equals = 1;
        let charging = decode_generic_response(&profile, &[0, 1, 0, 87, 1]);
        assert_eq!(charging.percent, Some(87));
        assert_eq!(charging.status, ReadingStatus::Charging);
        let offline = decode_generic_response(&profile, &[0, 1, 1, 87, 0]);
        assert_eq!(offline.status, ReadingStatus::Offline);
        let short = decode_generic_response(&profile, &[0, 1]);
        assert_eq!(short.status, ReadingStatus::Error);
        assert!(short.message.contains("长度不足"));
    }

    #[test]
    fn hex_parser_accepts_separators_and_rejects_bad_input() {
        assert_eq!(
            parse_hex("0x4B, 80;27_00").unwrap_or_default(),
            [0x4B, 0x80, 0x27, 0]
        );
        assert!(parse_hex("ABC").is_err());
        assert!(parse_hex("GG").is_err());
        assert_eq!(parse_hex(" ").unwrap_or_default(), Vec::<u8>::new());
    }
}
