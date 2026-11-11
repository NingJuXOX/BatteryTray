//! Agent-facing device onboarding API.
//!
//! The point of this module is to replace "guess a protocol from the device
//! name" with a short evidence loop: enumerate the candidate interfaces, collect
//! raw reports without writing anything, send one controlled read command, then
//! validate and persist the profile. Every function returns JSON so an agent can
//! call the CLI and keep the evidence in its context.

use crate::battery::{HidBackend, HidInterface, OpenOptions};
use crate::config::ConfigStore;
use crate::platform::hid::{enumerate_hid, WindowsHidBackend};
use crate::profiles::{load_profiles, normalize_profile, save_profiles, DeviceProfile};
use crate::protocol::{hex_dump, parse_hex};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
use std::time::{Duration, Instant};

/// Default passive listening window for one interface.
const DEFAULT_PASSIVE_MS: u32 = 1200;
/// Number of report IDs probed during a feature scan.
const FEATURE_REPORT_IDS: [u8; 4] = [0, 1, 2, 3];

/// Returns the configuration path, profiles and every present HID interface.
pub fn catalog() -> Value {
    let store = ConfigStore::from_env();
    let profiles = load_profiles(&store);
    let interfaces = enumerate_hid()
        .map(|items| items.iter().map(interface_json).collect::<Vec<_>>())
        .unwrap_or_default();
    json!({
        "ok": true,
        "config_path": store.path().display().to_string(),
        "profiles": profiles.iter().map(|item| item.to_value().unwrap_or(Value::Null)).collect::<Vec<_>>(),
        "interfaces": interfaces,
    })
}

/// Describes one interface for agents; numbers are hex strings to avoid unit
/// confusion when pasting VID/PID/usage into a prompt.
fn interface_json(item: &HidInterface) -> Value {
    json!({
        "vid": format!("0x{:04X}", item.vendor_id),
        "pid": format!("0x{:04X}", item.product_id),
        "product": item.product.clone(),
        "manufacturer": item.manufacturer.clone(),
        "mi": item.interface_number,
        "usage_page": format!("0x{:04X}", item.usage_page),
        "usage": format!("0x{:04X}", item.usage),
        "input_len": item.input_len,
        "output_len": item.output_len,
        "feature_len": item.feature_len,
        "path": item.path.clone(),
    })
}

/// Read-only evidence collection for one device.
///
/// Two passive techniques are used: `HidD_GetFeature` for the first few report
/// IDs (a read, not a write) and listening for unsolicited input reports. No
/// Output or SetFeature command is ever sent from this function.
pub fn scan(vid: u16, pid: u16, mi: Option<i16>, passive_ms: u32) -> Value {
    let started = Instant::now();
    let interfaces = match enumerate_hid() {
        Ok(items) => items
            .into_iter()
            .filter(|item| {
                item.vendor_id == vid
                    && item.product_id == pid
                    && mi.is_none_or(|wanted| item.interface_number == wanted)
            })
            .collect::<Vec<_>>(),
        Err(error) => {
            return json!({
                "ok": false,
                "error": format!("HID 枚举失败：{error}"),
                "hint": "设备可能未插入或已被其他程序独占，请插好设备后重试。",
            })
        }
    };
    if interfaces.is_empty() {
        return json!({
            "ok": false,
            "error": format!("没有找到 VID=0x{vid:04X} PID=0x{pid:04X} 的接口"),
            "hint": "确认设备已连接；接收器离线时先动一下设备再扫描。",
        });
    }
    let listen_ms = if passive_ms == 0 {
        DEFAULT_PASSIVE_MS
    } else {
        passive_ms
    };
    let mut results = Vec::new();
    for interface in interfaces {
        let mut backend = WindowsHidBackend::new();
        let mut feature = Vec::new();
        let mut passive = Vec::new();
        let mut errors = Vec::new();

        // Only interfaces that declare feature reports are worth probing;
        // keyboards and plain collections reject the open anyway.
        if interface.feature_len > 0 {
            match backend.open(
                &interface,
                OpenOptions {
                    write: true,
                    read: false,
                },
            ) {
                Ok(mut device) => {
                    for report_id in FEATURE_REPORT_IDS {
                        let attempt_started = Instant::now();
                        match device.get_feature(None, report_id) {
                            Ok(Some(bytes)) => feature.push(json!({
                                "report_id": report_id,
                                "status": "ok",
                                "hex": hex_dump(&bytes),
                                "elapsed_ms": attempt_started.elapsed().as_millis() as u64,
                            })),
                            Ok(None) => feature.push(json!({
                                "report_id": report_id,
                                "status": "no_data",
                            })),
                            Err(error) => feature.push(json!({
                                "report_id": report_id,
                                "status": "error",
                                "error": error.to_string(),
                            })),
                        }
                    }
                }
                Err(error) => errors.push(format!("无法打开接口读取 Feature：{error}")),
            }
        }

        if interface.input_len > 0 {
            match backend.open(
                &interface,
                OpenOptions {
                    write: false,
                    read: true,
                },
            ) {
                Ok(mut device) => {
                    let deadline = Instant::now() + Duration::from_millis(u64::from(listen_ms));
                    while Instant::now() < deadline {
                        let remaining = deadline
                            .saturating_duration_since(Instant::now())
                            .as_millis()
                            .clamp(10, u128::from(u32::MAX))
                            as u32;
                        match device.read_input(remaining) {
                            Ok(Some(bytes)) => {
                                passive.push(json!({
                                    "hex": hex_dump(&bytes),
                                    "report_id": bytes.first().copied(),
                                }));
                                if passive.len() >= 8 {
                                    break;
                                }
                            }
                            Ok(None) => break,
                            Err(error) => {
                                errors.push(format!("被动监听失败：{error}"));
                                break;
                            }
                        }
                    }
                }
                Err(error) => errors.push(format!("无法打开接口监听 Input：{error}")),
            }
        }

        results.push(json!({
            "mi": interface.interface_number,
            "usage_page": format!("0x{:04X}", interface.usage_page),
            "usage": format!("0x{:04X}", interface.usage),
            "input_len": interface.input_len,
            "output_len": interface.output_len,
            "feature_len": interface.feature_len,
            "product": interface.product.clone(),
            "feature_reads": feature,
            "passive_reports": passive,
            "errors": errors,
            "path": interface.path.clone(),
        }));
    }
    json!({
        "ok": true,
        "vid": format!("0x{vid:04X}"),
        "pid": format!("0x{pid:04X}"),
        "elapsed_ms": started.elapsed().as_millis() as u64,
        "interfaces": results,
        "notes": [
            "scan 只读：不会发送 Output 或 SetFeature。",
            "feature_reads 含 report ID 0..3；passive_reports 是设备主动推送的报告。",
            "没有数据时先动一下设备再扫描，休眠中的设备不会回应。",
        ],
    })
}

/// One controlled request/response round trip.
pub struct SendSpec {
    pub vid: u16,
    pub pid: u16,
    pub mi: Option<i16>,
    pub hex: String,
    pub mode: String,
    pub settle_ms: u32,
    pub timeout_ms: u32,
    pub read_len: u16,
}

/// Sends one read-only query and returns the raw reports.
///
/// This is the only function in the module that writes to a device, and it only
/// sends bytes supplied by the caller. Firmware upgrade, pairing and factory
/// reset commands must never be issued through this entry point.
pub fn send(spec: &SendSpec) -> Value {
    let payload = match parse_hex(&spec.hex) {
        Ok(payload) if !payload.is_empty() => payload,
        Ok(_) => return json!({"ok": false, "error": "请求字节为空"}),
        Err(error) => return json!({"ok": false, "error": error.to_string()}),
    };
    let interface = match pick_interface(spec.vid, spec.pid, spec.mi, &spec.mode) {
        Ok(interface) => interface,
        Err(error) => {
            return json!({"ok": false, "error": error});
        }
    };
    let mut backend = WindowsHidBackend::new();
    let output_mode = spec.mode == "output";
    let mut device = match backend.open(
        &interface,
        OpenOptions {
            write: true,
            read: output_mode,
        },
    ) {
        Ok(device) => device,
        Err(error) => {
            return json!({
                "ok": false,
                "error": format!("无法打开接口：{error}"),
                "hint": "设备可能被其他程序独占，或该接口没有写权限。",
            })
        }
    };

    let mut responses = Vec::new();
    let sent_at = Instant::now();
    let error = if output_mode {
        match device.write_output(&payload) {
            Ok(true) => {
                std::thread::sleep(Duration::from_millis(u64::from(spec.settle_ms)));
                let deadline =
                    Instant::now() + Duration::from_millis(u64::from(spec.timeout_ms.max(100)));
                let mut last_error = None;
                while Instant::now() < deadline {
                    let remaining = deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .clamp(10, u128::from(u32::MAX)) as u32;
                    match device.read_input(remaining) {
                        Ok(Some(bytes)) => {
                            responses.push(json!({
                                "kind": "input",
                                "hex": hex_dump(&bytes),
                                "elapsed_ms": sent_at.elapsed().as_millis() as u64,
                            }));
                            if responses.len() >= 16 {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Err(error) => {
                            last_error = Some(error.to_string());
                            break;
                        }
                    }
                }
                last_error
            }
            Ok(false) => Some("Output 写入失败".to_owned()),
            Err(error) => Some(error.to_string()),
        }
    } else {
        match device.set_feature(&payload) {
            Ok(true) => {
                std::thread::sleep(Duration::from_millis(u64::from(spec.settle_ms)));
                let read_len = (spec.read_len > 0).then_some(usize::from(spec.read_len));
                match device.get_feature(read_len, 0) {
                    Ok(Some(bytes)) => responses.push(json!({
                        "kind": "feature",
                        "hex": hex_dump(&bytes),
                        "elapsed_ms": sent_at.elapsed().as_millis() as u64,
                    })),
                    Ok(None) => {}
                    Err(error) => {
                        return json!({"ok": false, "error": format!("Feature 读取失败：{error}")})
                    }
                }
                None
            }
            Ok(false) => Some("Feature 写入失败".to_owned()),
            Err(error) => Some(error.to_string()),
        }
    };

    json!({
        "ok": error.is_none() && !responses.is_empty(),
        "mode": spec.mode,
        "sent": hex_dump(&payload),
        "mi": interface.interface_number,
        "input_len": interface.input_len,
        "output_len": interface.output_len,
        "feature_len": interface.feature_len,
        "responses": responses,
        "error": error.clone(),
        "hint": "响应为空时调整 settle_ms/timeout_ms，或换另一个 MI 接口重试。",
    })
}

fn pick_interface(
    vid: u16,
    pid: u16,
    mi: Option<i16>,
    mode: &str,
) -> std::result::Result<HidInterface, String> {
    let interfaces = enumerate_hid().map_err(|error| format!("HID 枚举失败：{error}"))?;
    let mut matches = interfaces
        .into_iter()
        .filter(|item| {
            item.vendor_id == vid
                && item.product_id == pid
                && mi.is_none_or(|wanted| item.interface_number == wanted)
        })
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return Err(format!("没有找到 VID=0x{vid:04X} PID=0x{pid:04X} 的接口"));
    }
    // Prefer the interface that actually supports the requested transfer.
    matches.sort_by_key(|item| {
        let suitable = if mode == "feature" {
            item.feature_len > 0
        } else {
            item.output_len > 0 && item.input_len > 0
        };
        !suitable
    });
    Ok(matches.remove(0))
}

/// Validates, merges and persists one or more device profiles.
///
/// Input is a JSON object or array. Existing devices are replaced by key; new
/// ones get the next free tray UID. Nothing is written when validation fails.
pub fn apply_profile_json(text: &str, test: bool, dry_run: bool) -> Value {
    let parsed: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => return json!({"ok": false, "error": format!("JSON 解析失败：{error}")}),
    };
    let entries = match &parsed {
        Value::Array(items) => items.clone(),
        Value::Object(_) => vec![parsed.clone()],
        _ => return json!({"ok": false, "error": "输入必须是 JSON 对象或数组"}),
    };
    if entries.is_empty() {
        return json!({"ok": false, "error": "没有设备配置"});
    }

    let store = ConfigStore::from_env();
    let mut profiles = load_profiles(&store);
    let mut applied = Vec::new();
    let mut errors = Vec::new();

    for (index, value) in entries.iter().enumerate() {
        let object = match value.as_object() {
            Some(object) => object.clone(),
            None => {
                errors.push(format!("第 {} 项不是对象", index + 1));
                continue;
            }
        };
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let existing_key = object.get("key").and_then(Value::as_str).map(str::to_owned);
        let key = existing_key.unwrap_or_else(|| slug(&name));
        let existing = profiles.iter().position(|item| item.key == key);
        let uid = existing
            .map(|index| profiles[index].uid)
            .unwrap_or_else(|| next_uid(&profiles));
        let profile = normalize_profile(value, Some(&key), Some(uid));
        let item_errors = crate::profiles::validate_profile(&profile);
        if !item_errors.is_empty() {
            errors.extend(item_errors);
            continue;
        }
        applied.push(json!({
            "key": profile.key,
            "name": profile.name,
            "uid": profile.uid,
            "protocol": profile.protocol,
        }));
        match existing {
            Some(index) => profiles[index] = profile,
            None => profiles.push(profile),
        }
    }
    if !errors.is_empty() {
        return json!({
            "ok": false,
            "errors": errors,
            "hint": "修正后再提交；未写入任何配置。",
        });
    }

    // A dry run validates and reads the device but leaves config.json alone,
    // so the agent can ask the user whether the value looks right first.
    let effective = if dry_run {
        profiles
    } else {
        match save_profiles(&store, &profiles) {
            Ok(saved) => saved,
            Err(error) => {
                return json!({
                    "ok": false,
                    "error": error.to_string(),
                    "hint": "若命令从构建目录/受限沙箱启动，请改用桌面上的主程序再试。",
                })
            }
        }
    };

    let readings = if test {
        let mut reader =
            crate::battery::BatteryReader::new(WindowsHidBackend::new(), store.clone());
        applied
            .iter()
            .filter_map(|item| item.get("key").and_then(Value::as_str))
            .filter_map(|key| effective.iter().find(|profile| profile.key == key))
            .map(|profile| {
                let reading = reader.read_profile(profile);
                json!({
                    "key": profile.key,
                    "name": reading.name,
                    "percent": reading.percent,
                    "status": format!("{:?}", reading.status),
                    "message": reading.message,
                    "source": reading.source,
                })
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    json!({
        "ok": true,
        "config_path": store.path().display().to_string(),
        "applied": applied,
        "device_count": effective.len(),
        "test_readings": readings,
        "dry_run": dry_run,
        "config_written": !dry_run,
        "next_step": if dry_run {
            "把 test_readings 里的电量告诉用户并询问是否合理；确认后去掉 --dry-run 正式写入。"
        } else {
            "配置已写入，托盘会在下次刷新时生效。"
        },
    })
}

fn next_uid(profiles: &[DeviceProfile]) -> u32 {
    profiles
        .iter()
        .map(|profile| profile.uid)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(1)
}

/// Derives a stable key from a device name.
fn slug(name: &str) -> String {
    let mut key = String::new();
    let mut separator = false;
    for ch in name.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            key.push(ch);
            separator = false;
        } else if !key.is_empty() && !separator {
            key.push('_');
            separator = true;
        }
    }
    let key = key.trim_matches('_').to_owned();
    if key.is_empty() {
        "device".to_owned()
    } else {
        key
    }
}

/// Handles all command-line work inside the main executable.
///
/// Returns `Some(exit_code)` when a command was handled, or `None` when the
/// caller should start the GUI. Keeping these in the main binary means there is
/// only one exe to distribute and share.
pub fn run_command(arguments: &[String]) -> Option<i32> {
    let has = |key: &str| arguments.iter().any(|item| item == key);
    if has("--help") || has("-h") {
        print_help();
        return Some(0);
    }
    // GUI modes are handled by platform::gui::run.
    if has("--panel") || has("--editor") || has("--debug-ui") {
        return None;
    }

    let options = parse_options(arguments);
    if has("--catalog") {
        return Some(emit_json(catalog()));
    }
    if has("--scan") {
        let vid = match parse_number(options.get("--vid"), "VID") {
            Ok(value) => value,
            Err(error) => return Some(fail(&error)),
        };
        let pid = match parse_number(options.get("--pid"), "PID") {
            Ok(value) => value,
            Err(error) => return Some(fail(&error)),
        };
        let mi = options
            .get("--mi")
            .and_then(|value| value.parse::<i16>().ok());
        let passive = number_option(&options, "--passive-ms", 0);
        return Some(emit_json(scan(vid, pid, mi, passive)));
    }
    if has("--send") {
        let vid = match parse_number(options.get("--vid"), "VID") {
            Ok(value) => value,
            Err(error) => return Some(fail(&error)),
        };
        let pid = match parse_number(options.get("--pid"), "PID") {
            Ok(value) => value,
            Err(error) => return Some(fail(&error)),
        };
        let hex = match options.get("--hex") {
            Some(value) => value.clone(),
            None => return Some(fail("缺少参数 --hex")),
        };
        let spec = SendSpec {
            vid,
            pid,
            mi: options
                .get("--mi")
                .and_then(|value| value.parse::<i16>().ok()),
            hex,
            mode: options
                .get("--mode")
                .map(String::as_str)
                .unwrap_or("feature")
                .to_owned(),
            settle_ms: number_option(&options, "--settle-ms", 50),
            timeout_ms: number_option(&options, "--timeout-ms", 1000),
            read_len: number_option(&options, "--read-len", 0) as u16,
        };
        return Some(emit_json(send(&spec)));
    }
    if has("--apply-profile") {
        let index = arguments
            .iter()
            .position(|item| item == "--apply-profile")
            .unwrap_or(0);
        let path = arguments
            .iter()
            .skip(index + 1)
            .find(|item| !item.starts_with("--"));
        let Some(path) = path else {
            return Some(fail("缺少配置路径；用 - 表示从标准输入读取"));
        };
        let text = if path == "-" {
            let mut buffer = String::new();
            if let Err(error) = std::io::stdin().read_to_string(&mut buffer) {
                return Some(fail(&format!("读取标准输入失败：{error}")));
            }
            buffer
        } else {
            match std::fs::read_to_string(path) {
                Ok(text) => text,
                Err(error) => return Some(fail(&format!("读取配置失败：{error}"))),
            }
        };
        let dry_run = has("--dry-run");
        let test = has("--test") || dry_run;
        return Some(emit_json(apply_profile_json(&text, test, dry_run)));
    }

    // Human-readable commands inherited from the former standalone CLI.
    if has("--list-hid") {
        return Some(match list_hid() {
            Ok(()) => 0,
            Err(error) => fail(&error.to_string()),
        });
    }
    if has("--install-desktop") {
        return Some(
            match crate::platform::shell::install_desktop_shortcut("电量托盘 Rust") {
                Ok(link) => {
                    println!("桌面快捷方式：{}", link.display());
                    println!("未修改开机自启。");
                    0
                }
                Err(error) => fail(&error.to_string()),
            },
        );
    }
    if has("--install") {
        return Some(match install(has("--no-startup")) {
            Ok(()) => 0,
            Err(error) => fail(&error.to_string()),
        });
    }
    if has("--probe") {
        probe_profile();
        return Some(0);
    }
    if has("--once") {
        print_once(has("--debug"));
        return Some(0);
    }
    None
}

fn print_help() {
    println!("电量托盘 —— 用法：不带参数启动托盘；--panel / --editor / --debug-ui 打开对应窗口");
    println!();
    println!("设备接入（输出 JSON，agent 可直接调用）：");
    println!("  --catalog                          列出配置、设备与全部 HID 接口");
    println!("  --scan --vid 0x1234 --pid 0x5678     只读收集原始响应");
    println!("  --send --hex \"00 20 01\" [--mode output] [--mi N] [--timeout-ms N]");
    println!("  --apply-profile device.json --dry-run  试读电量但不写配置（先问用户是否合理）");
    println!("  --apply-profile device.json --test      正式写入并实测");
    println!("  --apply-profile -                     从标准输入读取配置");
    println!();
    println!("其它：--once / --debug / --probe / --list-hid / --install / --install-desktop");
}

fn fail(message: &str) -> i32 {
    eprintln!("错误：{message}");
    1
}

fn emit_json(value: Value) -> i32 {
    match serde_json::to_string_pretty(&value) {
        Ok(text) => println!("{text}"),
        Err(error) => return fail(&format!("JSON 输出失败：{error}")),
    }
    if value.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        0
    } else {
        1
    }
}

fn parse_options(arguments: &[String]) -> HashMap<String, String> {
    let mut options = HashMap::new();
    for argument in arguments {
        if let Some((key, value)) = argument.split_once('=') {
            if key.starts_with("--") {
                options.insert(key.to_owned(), value.to_owned());
            }
        }
    }
    for (index, argument) in arguments.iter().enumerate() {
        if argument.starts_with("--") && !argument.contains('=') {
            if let Some(next) = arguments.get(index + 1) {
                if !next.starts_with("--") {
                    options
                        .entry(argument.clone())
                        .or_insert_with(|| next.clone());
                }
            }
        }
    }
    options
}

fn parse_number(value: Option<&String>, label: &str) -> std::result::Result<u16, String> {
    let text = value.ok_or_else(|| format!("缺少参数 {label}"))?.trim();
    let parsed = if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16)
    } else {
        text.parse::<u16>()
    };
    parsed.map_err(|_| format!("{label} 无效：{text}（应为 0x1234 或十进制）"))
}

fn number_option(options: &HashMap<String, String>, key: &str, default: u32) -> u32 {
    options
        .get(key)
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(default)
}

fn print_once(debug: bool) {
    let store = ConfigStore::from_env();
    let mut reader = crate::battery::BatteryReader::new(WindowsHidBackend::new(), store);
    reader.set_debug(debug);
    let readings = reader.read_all(None);
    if readings.is_empty() {
        println!("尚未添加设备：请打开面板点「新增」，或用 --apply-profile 导入协议 JSON。");
        return;
    }
    for reading in readings {
        println!("{}", reading.line());
    }
}

fn probe_profile() {
    let store = ConfigStore::from_env();
    let profiles = crate::profiles::load_profiles(&store);
    let mut reader = crate::battery::BatteryReader::new(WindowsHidBackend::new(), store);
    for profile in profiles.iter().filter(|item| item.enabled) {
        println!("{}", reader.read_profile(profile).line());
    }
}

fn list_hid() -> crate::error::Result<()> {
    for interface in enumerate_hid()? {
        println!(
            "{}  usage={:04X}:{:04X} in={} out={} feature={} MI={}",
            interface.label(),
            interface.usage_page,
            interface.usage,
            interface.input_len,
            interface.output_len,
            interface.feature_len,
            interface.interface_number
        );
        println!("  {}", interface.path);
    }
    Ok(())
}

fn install(no_startup: bool) -> crate::error::Result<()> {
    let executable = crate::platform::shell::tray_executable()?;
    let project = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    println!("项目目录：{}", project.display());
    println!("托盘程序：{}", executable.display());
    for path in crate::platform::shell::legacy_entries() {
        println!("检测到 Python 版入口（保持原样）：{}", path.display());
    }
    for entry in crate::platform::shell::ensure_desktop_entries(&project, &executable)? {
        println!(
            "{}：{} -> {}（{}）",
            entry.kind,
            entry.path.display(),
            entry.target.display(),
            entry.method
        );
    }
    if no_startup {
        crate::platform::shell::set_autostart(false)?;
        println!("开机自启：已关闭");
    } else {
        println!("开机自启：{}", crate::platform::shell::set_autostart(true)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_expected_rules() {
        assert_eq!(slug("Keychron K8 Pro"), "keychron_k8_pro");
        assert_eq!(slug("  测试鼠标 A5 "), "a5");
        assert_eq!(slug(""), "device");
    }
}
