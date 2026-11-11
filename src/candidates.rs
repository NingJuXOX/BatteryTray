//! Grouped view of the connected HID devices for the add-device flow.
//!
//! One physical receiver exposes several HID interfaces (keyboard, mouse,
//! vendor collection, ...). The wizard lets the user pick a *device*, so the
//! interfaces are grouped by their Windows instance id, and the group carries
//! everything the AI prompt needs to identify the hardware.

use crate::battery::HidInterface;
use crate::platform::hid::enumerate_hid;
use crate::profiles::{custom_profile, DeviceProfile};
use crate::Result;

/// Maximum number of interfaces quoted in the AI prompt.
const MAX_PROMPT_INTERFACES: usize = 8;

/// One physical device with all of its HID interfaces.
#[derive(Debug, Clone)]
pub struct DeviceCandidate {
    /// Identity used to keep the list selection: the serial number when the
    /// device exposes one (every collection of the same unit repeats it),
    /// otherwise the Windows instance id.
    pub id: String,
    /// Windows instance id of the first interface.
    pub instance: String,
    /// Product string, or a generic label when the device reports none.
    pub name: String,
    /// Manufacturer string.
    pub manufacturer: String,
    /// Serial number, empty when the device does not expose one.
    pub serial: String,
    /// USB vendor ID.
    pub vid: u16,
    /// USB product ID.
    pub pid: u16,
    /// Interfaces sorted by interface number.
    pub interfaces: Vec<HidInterface>,
}

impl DeviceCandidate {
    /// One line for the device list.
    ///
    /// `fresh` marks a device that appeared after the list was opened, which is
    /// how the "unplug and replug to confirm" hint pays off.
    pub fn label(&self, fresh: bool) -> String {
        let mut text = format!(
            "{}    {:04X}:{:04X}    {} 个接口",
            self.name,
            self.vid,
            self.pid,
            self.interfaces.len()
        );
        if !self.serial.is_empty() {
            text.push_str(&format!("    SN {}", self.serial));
        }
        if fresh {
            text.push_str("    · 刚接入");
        }
        text
    }

    /// Evidence block injected into the AI prompt.
    pub fn evidence(&self) -> String {
        let mut lines = vec![format!(
            "- VID/PID：0x{:04X} / 0x{:04X}",
            self.vid, self.pid
        )];
        if !self.manufacturer.is_empty() {
            lines.push(format!("- 制造商：{}", self.manufacturer));
        }
        if !self.serial.is_empty() {
            lines.push(format!("- 序列号：{}", self.serial));
        }
        lines.push(format!("- 系统设备实例：{}", self.instance));
        for (index, item) in self
            .interfaces
            .iter()
            .take(MAX_PROMPT_INTERFACES)
            .enumerate()
        {
            lines.push(format!(
                "- 接口 {}：MI={} · Usage 0x{:04X}:0x{:04X} · Input {}B / Output {}B / Feature {}B",
                index + 1,
                item.interface_number,
                item.usage_page,
                item.usage,
                item.input_len,
                item.output_len,
                item.feature_len
            ));
        }
        if self.interfaces.len() > MAX_PROMPT_INTERFACES {
            lines.push(format!(
                "- 另有 {} 个接口未列出",
                self.interfaces.len() - MAX_PROMPT_INTERFACES
            ));
        }
        lines.push(format!(
            "- 接口路径：{}",
            self.best_interface()
                .map(|item| item.path.clone())
                .unwrap_or_default()
        ));
        lines.join("\n")
    }

    /// The interface most likely to serve battery queries.
    pub fn best_interface(&self) -> Option<&HidInterface> {
        self.interfaces
            .iter()
            .find(|item| item.feature_len > 0)
            .or_else(|| {
                self.interfaces
                    .iter()
                    .find(|item| item.input_len > 0 && item.output_len > 0)
            })
            .or_else(|| self.interfaces.first())
    }

    /// Builds a profile pre-filled with everything already known about the
    /// device; the AI (or the user) only has to supply the protocol fields.
    pub fn seed_profile(&self, key: &str, uid: u32) -> DeviceProfile {
        let mut profile = custom_profile(key, &self.name, uid);
        profile.match_.vid = self.vid;
        profile.match_.pid = self.pid;
        if let Some(interface) = self.best_interface() {
            profile.match_.interface_number = interface.interface_number;
            profile.match_.usage_page = Some(interface.usage_page);
            profile.match_.usage = Some(interface.usage);
            profile.match_.input_len = interface.input_len;
            profile.match_.output_len = interface.output_len;
            profile.match_.feature_len = interface.feature_len;
            profile.protocol = if interface.feature_len > 0 {
                "hid_feature".to_owned()
            } else {
                "hid_output_input".to_owned()
            };
        }
        if !self.name.is_empty() {
            profile.match_.product_contains = self.name.clone();
        }
        profile
    }
}

/// Enumerates every present HID device, grouped by physical device.
pub fn candidates() -> Result<Vec<DeviceCandidate>> {
    let interfaces = enumerate_hid()?;
    let mut grouped: Vec<DeviceCandidate> = Vec::new();
    for interface in interfaces {
        let id = group_key(&interface);
        match grouped.iter_mut().find(|item| item.id == id) {
            Some(item) => item.interfaces.push(interface),
            None => grouped.push(DeviceCandidate {
                id,
                instance: instance_of(&interface.path),
                name: display_name(&interface),
                manufacturer: interface.manufacturer.clone(),
                serial: interface.serial.clone(),
                vid: interface.vendor_id,
                pid: interface.product_id,
                interfaces: vec![interface],
            }),
        }
    }
    for item in &mut grouped {
        item.interfaces
            .sort_by_key(|interface| interface.interface_number);
    }
    grouped.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then(left.vid.cmp(&right.vid))
            .then(left.pid.cmp(&right.pid))
            .then(left.id.cmp(&right.id))
    });
    Ok(grouped)
}

/// Identity of a physical device.
///
/// Every top-level collection of one unit reports the same serial, so the serial
/// is the best identity; devices without one fall back to the instance id of the
/// HID path, which is what Windows assigns per plugged-in unit.
fn group_key(interface: &HidInterface) -> String {
    let serial = interface.serial.trim();
    if serial.is_empty() {
        instance_of(&interface.path)
    } else {
        format!(
            "{:04X}:{:04X}:{serial}",
            interface.vendor_id, interface.product_id
        )
    }
}

/// Extracts the Windows instance id: the third `#`-separated path segment.
fn instance_of(path: &str) -> String {
    let mut parts = path.split('#');
    parts.next();
    parts.next();
    parts.next().unwrap_or(path).to_owned()
}

fn display_name(interface: &HidInterface) -> String {
    if !interface.product.trim().is_empty() {
        return interface.product.trim().to_owned();
    }
    if !interface.manufacturer.trim().is_empty() {
        return interface.manufacturer.trim().to_owned();
    }
    format!(
        "未命名设备 {:04X}:{:04X}",
        interface.vendor_id, interface.product_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_id_is_the_third_segment() {
        let path = r"\\?\hid#vid_374a&pid_a322&mi_01&col02#8&1f4b2c3d&0&0001#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        assert_eq!(instance_of(path), "8&1f4b2c3d&0&0001");
        assert_eq!(instance_of("no-separators"), "no-separators");
    }
}
