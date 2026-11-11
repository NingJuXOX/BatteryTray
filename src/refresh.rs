use crate::policy::FailurePolicy;
use crate::protocol::{Reading, ReadingStatus};
use chrono::Local;

/// Formats a tray tooltip with acquisition time and optional cache note.
pub fn format_tooltip(reading: &Reading) -> String {
    let name = if reading.name.trim().is_empty() {
        "设备"
    } else {
        reading.name.trim()
    };
    match reading.percent {
        None => format!(
            "{}：--（{}）",
            name,
            if reading.message.trim().is_empty() {
                "读取失败"
            } else {
                reading.message.trim()
            }
        ),
        Some(percent) => {
            let stamp = reading.updated.format("%H:%M");
            if reading.message.trim().is_empty() {
                format!("{}：{percent}%（{stamp} 更新）", name)
            } else {
                format!(
                    "{}：{percent}%（{stamp} 更新，{}）",
                    name,
                    reading.message.trim()
                )
            }
        }
    }
}

/// Clips a tooltip to the 127 UTF-16 code units available in NOTIFYICONDATAW.
pub fn clip_tooltip(text: &str) -> String {
    let units = text.encode_utf16().count();
    if units <= 127 {
        return text.to_owned();
    }
    let mut output = String::new();
    let mut used = 0_usize;
    for ch in text.chars() {
        let width = ch.len_utf16();
        if used + width > 126 {
            break;
        }
        output.push(ch);
        used += width;
    }
    output.push('…');
    output
}

/// Adds suspension information without hiding the last known reading.
pub fn compose_tooltip(reading: &Reading, policy: &FailurePolicy) -> String {
    if !policy.is_suspended(&reading.key) {
        return clip_tooltip(&format_tooltip(reading));
    }
    let streak = policy.streak(&reading.key).max(1);
    let note = if reading.percent.is_some() {
        "自动刷新已暂停（左键点击手动刷新重试）".to_owned()
    } else {
        format!("连续失败 {streak} 次，已暂停自动刷新（左键点击手动刷新重试）")
    };
    let mut adjusted = reading.clone();
    adjusted.message = note;
    clip_tooltip(&format_tooltip(&adjusted))
}

/// Creates a fallback reading for a missing device response.
pub fn missing_reading(key: &str, name: &str) -> Reading {
    Reading {
        key: key.to_owned(),
        name: name.to_owned(),
        percent: None,
        status: ReadingStatus::NotFound,
        message: "未返回该设备数据".to_owned(),
        source: String::new(),
        updated: Local::now(),
        detail: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::sample_profiles;

    #[test]
    fn tooltip_matches_legacy_format_and_utf16_limit() {
        let profile = sample_profiles()
            .into_iter()
            .next()
            .unwrap_or_else(|| unreachable!());
        let mut reading = Reading::new(&profile, Some(94), ReadingStatus::Ok, "");
        assert!(format_tooltip(&reading).contains("：94%（"));
        reading.percent = None;
        reading.message = "缓存".to_owned();
        assert_eq!(format_tooltip(&reading), "测试鼠标：--（缓存）");
        let clipped = clip_tooltip(&"字".repeat(200));
        assert_eq!(clipped.encode_utf16().count(), 127);
        assert!(clipped.ends_with('…'));
    }
}
