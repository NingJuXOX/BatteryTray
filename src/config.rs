use crate::error::{BatteryTrayError, Result};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Default refresh interval in seconds.
pub const CONFIG_INTERVAL_DEFAULT: u32 = 60;
/// Minimum refresh interval in seconds.
pub const CONFIG_INTERVAL_MIN: u32 = 30;
/// Maximum refresh interval in seconds.
pub const CONFIG_INTERVAL_MAX: u32 = 1800;
/// Shared refresh interval menu entries.
pub const INTERVAL_CHOICES: &[(u32, &str)] = &[
    (30, "30 秒"),
    (60, "60 秒"),
    (120, "2 分钟"),
    (300, "5 分钟"),
    (900, "15 分钟"),
    (1800, "30 分钟"),
];

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// One parsed configuration, keyed by the file's modification stamp and size.
///
/// A single refresh used to read and parse `config.json` four or more times
/// (choosing devices, per-device calibration, applying results, ...). Parsing
/// is only repeated when the file actually changes; unchanged reads just copy
/// the cached map. The cache is process-wide because several modules create
/// their own `ConfigStore` for the same path.
#[derive(Clone)]
struct CacheEntry {
    signature: (u128, u64),
    config: Map<String, Value>,
}

static CONFIG_CACHE: OnceLock<Mutex<HashMap<PathBuf, CacheEntry>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<PathBuf, CacheEntry>> {
    CONFIG_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_cache<R>(action: impl FnOnce(&mut HashMap<PathBuf, CacheEntry>) -> R) -> R {
    match cache().lock() {
        Ok(mut entries) => action(&mut entries),
        Err(poisoned) => action(&mut poisoned.into_inner()),
    }
}

/// Cheap identity of the configuration file's current contents.
fn file_signature(path: &Path) -> (u128, u64) {
    match fs::metadata(path) {
        Ok(metadata) => {
            let stamp = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            (stamp, metadata.len())
        }
        Err(_) => (0, 0),
    }
}

fn cached_config(path: &Path, signature: (u128, u64)) -> Option<Map<String, Value>> {
    with_cache(|entries| {
        entries
            .get(path)
            .filter(|entry| entry.signature == signature)
            .map(|entry| entry.config.clone())
    })
}

fn remember_config(path: &Path, signature: (u128, u64), config: Map<String, Value>) {
    with_cache(|entries| {
        entries.insert(path.to_path_buf(), CacheEntry { signature, config });
    });
}

/// Drops the cached value after a write so the next read sees the new file.
fn forget_config(path: &Path) {
    with_cache(|entries| {
        entries.remove(path);
    });
}

/// Clamps a refresh interval to the supported 30–1800 second range.
pub fn clamp_interval(value: Option<&Value>) -> u32 {
    let parsed = match value {
        Some(Value::Number(number)) => number.as_i64().unwrap_or(CONFIG_INTERVAL_DEFAULT as i64),
        Some(Value::String(text)) => text
            .trim()
            .parse::<i64>()
            .unwrap_or(CONFIG_INTERVAL_DEFAULT as i64),
        _ => CONFIG_INTERVAL_DEFAULT as i64,
    };
    parsed.clamp(CONFIG_INTERVAL_MIN as i64, CONFIG_INTERVAL_MAX as i64) as u32
}

/// JSON configuration store rooted at the BatteryTray data directory.
#[derive(Debug, Clone)]
pub struct ConfigStore {
    directory: PathBuf,
    corruption_reported: Arc<AtomicBool>,
}

impl ConfigStore {
    /// Creates a store at an explicit data directory.
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            corruption_reported: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Creates the standard `%APPDATA%\BatteryTray` store.
    pub fn from_env() -> Self {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .map(|home| PathBuf::from(home).join("AppData").join("Roaming"))
            })
            .unwrap_or_else(|| PathBuf::from("."));
        Self::new(base.join("BatteryTray"))
    }

    /// Returns the data directory.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Returns the configuration file path.
    pub fn path(&self) -> PathBuf {
        self.directory.join("config.json")
    }

    /// Injects the clamped refresh interval into a configuration map.
    fn normalize(mut config: Map<String, Value>) -> Map<String, Value> {
        let interval = clamp_interval(config.get("interval"));
        config.insert("interval".to_owned(), Value::from(interval));
        config
    }

    /// Loads configuration, tolerating missing or malformed files.
    ///
    /// A malformed file is moved to `config.json.corrupt-<timestamp>` before
    /// defaults are returned, so the original data is never lost.
    pub fn load(&self) -> Map<String, Value> {
        match self.load_strict() {
            Ok(config) => config,
            Err(error) => {
                if !self.corruption_reported.swap(true, Ordering::Relaxed) {
                    eprintln!("{error}");
                }
                Self::normalize(Map::new())
            }
        }
    }

    /// Loads configuration and reports malformed files instead of hiding them.
    ///
    /// The corrupt file is backed up and an explicit error is returned; callers
    /// that write must not silently replace it with defaults.
    pub fn load_strict(&self) -> Result<Map<String, Value>> {
        let path = self.path();
        let signature = file_signature(&path);
        if let Some(config) = cached_config(&path, signature) {
            return Ok(config);
        }
        let parsed = self.parse_file(&path);
        match &parsed {
            Ok(config) => remember_config(&path, signature, config.clone()),
            Err(_) => forget_config(&path),
        }
        parsed
    }

    fn parse_file(&self, path: &Path) -> Result<Map<String, Value>> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::normalize(Map::new()))
            }
            Err(error) => return Err(error.into()),
        };
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(config)) => Ok(Self::normalize(config)),
            Ok(_) => Err(self.reject_corrupt("配置文件顶层不是 JSON 对象")),
            Err(error) => Err(self.reject_corrupt(&format!("配置文件 JSON 无法解析：{error}"))),
        }
    }

    /// Moves a corrupt configuration aside and describes the outcome.
    fn reject_corrupt(&self, reason: &str) -> BatteryTrayError {
        match self.backup_corrupt() {
            Some(path) => BatteryTrayError::Invalid(format!(
                "{reason}；原文件已备份到 {}，请检查后重试",
                path.display()
            )),
            None => BatteryTrayError::Invalid(format!(
                "{reason}；备份失败，请手动检查 {}",
                self.path().display()
            )),
        }
    }

    /// Renames the current configuration to a timestamped backup.
    fn backup_corrupt(&self) -> Option<PathBuf> {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let base = self.directory.join(format!("config.json.corrupt-{stamp}"));
        let mut target = base.clone();
        let mut suffix = 1_u32;
        while target.exists() {
            target = PathBuf::from(format!("{}-{suffix}", base.to_string_lossy()));
            suffix = suffix.saturating_add(1);
        }
        // Copy the damaged bytes aside and replace the original with a valid
        // empty object. A rename is unreliable here when another process holds
        // the file, but the bytes are preserved either way.
        match fs::read(self.path()).and_then(|bytes| {
            fs::write(&target, &bytes)?;
            fs::write(self.path(), b"{}")
        }) {
            Ok(()) => {
                forget_config(&self.path());
                Some(target)
            }
            Err(error) => {
                eprintln!("备份损坏配置失败：{error}");
                None
            }
        }
    }

    /// Atomically replaces the complete configuration while holding a process lock.
    pub fn save(&self, mut config: Map<String, Value>) -> Result<()> {
        #[cfg(windows)]
        let _lock = crate::platform::sync::NamedMutex::acquire(
            crate::platform::sync::CONFIG_MUTEX_NAME,
            3000,
        )?
        .ok_or_else(|| {
            BatteryTrayError::Invalid("配置文件正被另一个窗口写入，请稍后重试".to_owned())
        })?;
        self.write_locked(&mut config)
    }

    /// Merges changes into the latest configuration under the same write lock.
    pub fn update(&self, changes: impl IntoIterator<Item = (String, Value)>) -> Result<()> {
        #[cfg(windows)]
        let _lock = crate::platform::sync::NamedMutex::acquire(
            crate::platform::sync::CONFIG_MUTEX_NAME,
            3000,
        )?
        .ok_or_else(|| {
            BatteryTrayError::Invalid("配置文件正被另一个窗口写入，请稍后重试".to_owned())
        })?;
        let mut config = self.load_strict()?;
        config.extend(changes);
        self.write_locked(&mut config)
    }

    fn write_locked(&self, config: &mut Map<String, Value>) -> Result<()> {
        let interval = clamp_interval(config.get("interval"));
        config.insert("interval".to_owned(), Value::from(interval));
        fs::create_dir_all(&self.directory)?;
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary =
            self.path()
                .with_extension(format!("json.{}.{}.tmp", std::process::id(), sequence));
        let mut bytes = serde_json::to_vec_pretty(config)?;
        bytes.push(b'\n');
        let write_result = (|| -> Result<()> {
            fs::write(&temporary, &bytes)?;
            let file = fs::OpenOptions::new().write(true).open(&temporary)?;
            file.sync_all()?;
            if Self::replace_with_retry(&temporary, &self.path()).is_err() {
                // Some environments refuse MoveFileExW over the live file. The
                // temp copy is already flushed, so an in-place write is safe.
                fs::write(self.path(), &bytes)?;
            }
            Ok(())
        })();
        if let Err(error) = write_result {
            if let Err(remove_error) = fs::remove_file(&temporary) {
                if remove_error.kind() != std::io::ErrorKind::NotFound {
                    return Err(BatteryTrayError::Invalid(format!(
                        "{error}; 临时文件清理失败：{remove_error}"
                    )));
                }
            }
            return Err(error);
        }
        forget_config(&self.path());
        Ok(())
    }

    /// Replaces the configuration file, retrying short sharing violations.
    ///
    /// The tray re-reads the file on a timer; on Windows a rename over a file
    /// that is open by a reader can fail with ERROR_ACCESS_DENIED for a few
    /// milliseconds. Retrying keeps writes from the manager or CLI reliable.
    fn replace_with_retry(from: &Path, to: &Path) -> Result<()> {
        let mut last_error = None;
        for _ in 0..5 {
            match fs::rename(from, to) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    last_error = Some(error);
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(BatteryTrayError::Io(last_error.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "配置文件正被其他进程占用",
            )
        })))
    }

    /// Changes only the refresh interval.
    pub fn set_interval(&self, seconds: u32) -> Result<u32> {
        let clamped = seconds.clamp(CONFIG_INTERVAL_MIN, CONFIG_INTERVAL_MAX);
        self.update([("interval".to_owned(), Value::from(clamped))])?;
        Ok(clamped)
    }

    /// Reads protocol calibration values.
    pub fn calibration(&self) -> Map<String, Value> {
        self.load()
            .get("calibration")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_handles_bounds_and_bad_values() {
        assert_eq!(clamp_interval(Some(&Value::from(29))), 30);
        assert_eq!(clamp_interval(Some(&Value::from(1801))), 1800);
        assert_eq!(clamp_interval(Some(&Value::from("120"))), 120);
        assert_eq!(clamp_interval(Some(&Value::from("bad"))), 60);
        assert_eq!(clamp_interval(None), 60);
    }

    #[test]
    fn missing_file_is_read_only_and_update_preserves_fields() {
        let temporary = tempfile::tempdir();
        assert!(temporary.is_ok(), "temporary directory must be created");
        let Some(root) = temporary.ok().map(|dir| dir.path().join("BatteryTray")) else {
            return;
        };
        let store = ConfigStore::new(root.clone());
        assert_eq!(store.load().get("interval"), Some(&Value::from(60)));
        assert!(!store.path().exists());
        let result = store.update([
            ("interval".to_owned(), Value::from(300)),
            (
                "calibration".to_owned(),
                serde_json::json!({"mouse_battery_index": 7}),
            ),
        ]);
        assert!(result.is_ok(), "configuration update failed: {result:?}");
        assert_eq!(store.load().get("interval"), Some(&Value::from(300)));
        assert!(root.join("config.json").is_file());
    }

    #[test]
    fn cache_reflects_external_writes_and_store_writes() {
        let temporary = tempfile::tempdir();
        assert!(temporary.is_ok(), "temporary directory must be created");
        let Some(root) = temporary.ok().map(|dir| dir.path().join("BatteryTray")) else {
            return;
        };
        assert!(std::fs::create_dir_all(&root).is_ok());
        let store = ConfigStore::new(root);
        assert_eq!(store.load().get("interval"), Some(&Value::from(60)));

        // Another process changing the file must be visible on the next load.
        assert!(std::fs::write(store.path(), r#"{"interval":300,"marker":1}"#).is_ok());
        assert_eq!(store.load().get("interval"), Some(&Value::from(300)));
        assert_eq!(store.load().get("marker"), Some(&Value::from(1)));

        // A write through the store must invalidate its own cached copy.
        assert!(store
            .update([("marker".to_owned(), Value::from(2))])
            .is_ok());
        assert_eq!(store.load().get("marker"), Some(&Value::from(2)));
    }

    #[test]
    fn corrupt_config_is_backed_up_instead_of_overwritten() {
        let temporary = tempfile::tempdir();
        assert!(temporary.is_ok(), "temporary directory must be created");
        let Some(root) = temporary.ok().map(|dir| dir.path().join("BatteryTray")) else {
            return;
        };
        assert!(std::fs::create_dir_all(&root).is_ok());
        let store = ConfigStore::new(root.clone());
        let original = b"{ not json";
        assert!(std::fs::write(store.path(), original).is_ok());

        // Writing must refuse rather than replace the damaged file.
        let update = store.update([("interval".to_owned(), Value::from(300))]);
        assert!(update.is_err(), "corrupt config must not be overwritten");
        assert!(
            store.path().exists(),
            "damaged file is preserved and reset to a valid empty object"
        );

        let backups = std::fs::read_dir(&root)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with("config.json.corrupt-"))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert_eq!(backups.len(), 1, "exactly one backup must be created");
        assert_eq!(
            std::fs::read(&backups[0]).ok().as_deref(),
            Some(original.as_slice())
        );

        // The original bytes are preserved and the live file is now valid.
        assert_eq!(store.load().get("interval"), Some(&Value::from(60)));
    }

    #[test]
    fn corrupt_backups_do_not_collide() {
        let temporary = tempfile::tempdir();
        assert!(temporary.is_ok(), "temporary directory must be created");
        let Some(root) = temporary.ok().map(|dir| dir.path().join("BatteryTray")) else {
            return;
        };
        let store = ConfigStore::new(root.clone());
        assert!(std::fs::create_dir_all(&root).is_ok());
        for _ in 0..2 {
            assert!(std::fs::write(store.path(), b"[]").is_ok());
            assert!(store.load_strict().is_err());
        }
        let count = std::fs::read_dir(&root)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .filter(|entry| {
                        entry
                            .file_name()
                            .to_str()
                            .is_some_and(|name| name.starts_with("config.json.corrupt-"))
                    })
                    .count()
            })
            .unwrap_or_default();
        assert_eq!(count, 2, "each damaged file needs its own backup");
    }
}
