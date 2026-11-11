#![allow(clippy::expect_used, clippy::unwrap_used)]

use battery_tray::battery::{
    BatteryReader, HidBackend, HidDevice, HidInterface, HidLock, OpenOptions,
};
use battery_tray::config::ConfigStore;
use battery_tray::error::Result;
use battery_tray::profiles::{load_profiles, normalize_profile, save_profiles, DeviceProfile};
use battery_tray::protocol::ReadingStatus;
use serde_json::{json, Value};
use std::collections::VecDeque;

struct FakeLock;
impl HidLock for FakeLock {}

struct FakeDevice {
    response: Option<Vec<u8>>,
    inputs: VecDeque<Vec<u8>>,
}

impl HidDevice for FakeDevice {
    fn set_feature(&mut self, _data: &[u8]) -> Result<bool> {
        Ok(true)
    }

    fn get_feature(&mut self, _length: Option<usize>, _report_id: u8) -> Result<Option<Vec<u8>>> {
        Ok(self.response.clone())
    }

    fn write_output(&mut self, _data: &[u8]) -> Result<bool> {
        Ok(true)
    }

    fn read_input(&mut self, _timeout_ms: u32) -> Result<Option<Vec<u8>>> {
        Ok(self.inputs.pop_front())
    }
}

struct FakeBackend {
    response: Option<Vec<u8>>,
    inputs: Vec<Vec<u8>>,
}

impl HidBackend for FakeBackend {
    fn enumerate(&mut self) -> Result<Vec<HidInterface>> {
        Ok(vec![HidInterface {
            path: "fake".to_owned(),
            vendor_id: 0x1234,
            product_id: 0x5678,
            product: "Fake HID".to_owned(),
            manufacturer: String::new(),
            serial: String::new(),
            usage_page: 1,
            usage: 6,
            input_len: 64,
            output_len: 64,
            feature_len: 65,
            interface_number: 0,
        }])
    }

    fn open(
        &mut self,
        _interface: &HidInterface,
        _options: OpenOptions,
    ) -> Result<Box<dyn HidDevice>> {
        Ok(Box::new(FakeDevice {
            response: self.response.clone(),
            inputs: self.inputs.clone().into(),
        }))
    }

    fn lock(&mut self, _key: &str, _timeout_ms: u32) -> Result<Option<Box<dyn HidLock>>> {
        Ok(Some(Box::new(FakeLock)))
    }
}

fn store() -> (tempfile::TempDir, ConfigStore) {
    let directory = tempfile::tempdir().expect("temp directory");
    let store = ConfigStore::new(directory.path().join("BatteryTray"));
    (directory, store)
}

fn custom_profile() -> DeviceProfile {
    normalize_profile(
        &json!({
            "key": "custom",
            "name": "测试设备",
            "protocol": "hid_feature",
            "enabled": true,
            "show_in_tray": true,
            "uid": 3,
            "icon": "device",
            "match": {"vid": 0x1234, "pid": 0x5678, "input_len": 64, "output_len": 64},
            "request": {"bytes_hex": "00 F7 00", "settle_ms": 0, "timeout_ms": 100},
            "response": {
                "battery_offset": 3,
                "valid_offset": 1,
                "valid_equals": 1,
                "online_offset": 2,
                "online_equals": 0,
                "charging_offset": 4,
                "charging_mask": 1,
                "charging_equals": 1,
                "report_id": -1
            }
        }),
        None,
        None,
    )
}

fn two_profiles() -> Vec<DeviceProfile> {
    let mut second = custom_profile();
    second.key = "second".to_owned();
    second.name = "第二台设备".to_owned();
    second.uid = 4;
    vec![custom_profile(), second]
}

#[test]
fn legacy_config_round_trip_preserves_calibration_and_interval() {
    let (_directory, store) = store();
    std::fs::create_dir_all(store.directory()).expect("config directory");
    let legacy = json!({
        "interval": 300,
        "calibration": {"mouse_battery_index": 7, "headphone_cmd": 39},
        "devices": [
            {
                "key": "mouse", "name": "旧鼠标", "protocol": "vk_mouse", "enabled": true,
                "show_in_tray": true, "uid": 1, "icon": "mouse",
                "match": {"vid": 0x1234, "pid": 0x5678, "usage_page": 0xFFFF, "usage": 2},
                "request": {"bytes_hex": "00 F7"}, "response": {"battery_offset": 3}
            }
        ],
        "future_field": {"kept": true}
    });
    std::fs::write(
        store.path(),
        serde_json::to_vec_pretty(&legacy).expect("json"),
    )
    .expect("legacy config write");
    let profiles = load_profiles(&store);
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].name, "旧鼠标");
    assert_eq!(store.load().get("interval"), Some(&Value::from(300)));
    assert!(store.load().get("future_field").is_some());
    assert_eq!(
        store.calibration().get("mouse_battery_index"),
        Some(&Value::from(7))
    );
}

#[test]
fn saving_devices_does_not_overwrite_calibration() {
    let (_directory, store) = store();
    assert!(store
        .update([
            ("interval".to_owned(), Value::from(120)),
            (
                "calibration".to_owned(),
                json!({"mouse_battery_index": 5, "headphone_level_index": 9})
            ),
        ])
        .is_ok());
    let saved = save_profiles(&store, &two_profiles());
    assert!(saved.is_ok(), "save failed: {saved:?}");
    assert_eq!(store.load().get("interval"), Some(&Value::from(120)));
    assert_eq!(
        store.calibration().get("mouse_battery_index"),
        Some(&Value::from(5))
    );
    assert_eq!(load_profiles(&store).len(), 2);
}

#[test]
fn feature_reader_decodes_charging_percentage() {
    let (_directory, store) = store();
    let profile = custom_profile();
    let mut response = vec![0_u8; 65];
    response[1] = 1;
    response[2] = 0;
    response[3] = 87;
    response[4] = 1;
    let backend = FakeBackend {
        response: Some(response),
        inputs: Vec::new(),
    };
    let mut reader = BatteryReader::new(backend, store);
    let reading = reader.read_profile(&profile);
    assert_eq!(reading.percent, Some(87));
    assert_eq!(reading.status, ReadingStatus::Charging);
}

#[test]
fn output_input_reader_skips_unrelated_report_ids() {
    let (_directory, store) = store();
    let mut profile = custom_profile();
    profile.protocol = "hid_output_input".to_owned();
    profile.response.report_id = 0x4B;
    profile.response.valid_offset = -1;
    profile.response.online_offset = -1;
    profile.response.charging_offset = -1;
    let mut valid = vec![0_u8; 64];
    valid[0] = 0x4B;
    valid[3] = 66;
    let backend = FakeBackend {
        response: None,
        inputs: vec![vec![0_u8; 64], valid],
    };
    let mut reader = BatteryReader::new(backend, store);
    let reading = reader.read_profile(&profile);
    assert_eq!(reading.percent, Some(66));
    assert_eq!(reading.status, ReadingStatus::Ok);
}

#[test]
fn shortcut_creation_round_trips_in_writable_directory() {
    use battery_tray::platform::shell::{create_shortcut, read_shortcut};

    let directory = tempfile::tempdir().expect("temp directory");
    let target = std::env::current_exe().expect("current executable");
    let link = directory.path().join("BatteryTray Test.lnk");
    let result = create_shortcut(
        &link,
        &target,
        "--panel",
        directory.path(),
        "BatteryTray shortcut test",
    );
    assert!(result.is_ok(), "create shortcut failed: {result:?}");
    let read = read_shortcut(&link);
    assert!(read.is_some(), "shortcut must be readable");
    let read = read.unwrap_or_else(|| unreachable!());
    assert_eq!(read.arguments, "--panel");
    assert_eq!(read.target, target);
}

#[test]
fn invalid_profile_is_rejected_without_touching_config() {
    let (_directory, store) = store();
    let mut profile = custom_profile();
    profile.match_.vid = 0;
    profile.match_.pid = 0;
    let before = store.load();
    let result = save_profiles(&store, &[profile]);
    assert!(result.is_err());
    assert_eq!(store.load(), before);
}
