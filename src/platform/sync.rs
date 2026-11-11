use crate::battery::HidLock;
use crate::error::{BatteryTrayError, Result};
use windows::core::{HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, HANDLE, WAIT_ABANDONED,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

/// Stable tray single-instance mutex name shared with the Python version.
pub const TRAY_MUTEX_NAME: &str = "Local\\BatteryTray_SingleInstance_v1";
/// Panel single-instance mutex name.
pub const PANEL_MUTEX_NAME: &str = "Local\\BatteryTray.Panel";
/// Add-device window single-instance mutex name.
pub const EDITOR_MUTEX_NAME: &str = "Local\\BatteryTray.Editor";
/// HID debugger single-instance mutex name.
pub const DEBUG_MUTEX_NAME: &str = "Local\\BatteryTray.Debug";
/// Cross-process configuration write mutex name.
pub const CONFIG_MUTEX_NAME: &str = "Local\\BatteryTray_Config_v1";
/// Prefix for per-device HID locks.
pub const HID_LOCK_PREFIX: &str = "Local\\BatteryTray_Hid_";

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Owned Windows named mutex.
///
/// Not `Send`: Windows mutex ownership is tied to the acquiring thread, so
/// releasing it from another thread would fail with `ERROR_NOT_OWNER`.
#[derive(Debug)]
pub struct NamedMutex {
    handle: HANDLE,
    release_on_drop: bool,
}

impl NamedMutex {
    /// Creates a mutex and reports whether this process is its first creator.
    pub fn create(name: &str) -> Result<Option<Self>> {
        let name = wide(name);
        // SAFETY: `name` is a live NUL-terminated UTF-16 buffer for the call;
        // no security attributes are retained by Windows.
        let handle = match unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) } {
            Ok(handle) => handle,
            // The object exists but this token cannot open it (for example a
            // second instance started with different rights). That still means
            // "another instance is running", not a fatal error.
            Err(error) if error.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0) => {
                return Ok(None)
            }
            Err(error) => {
                return Err(BatteryTrayError::Shell(format!(
                    "CreateMutexW 失败：{error}"
                )))
            }
        };
        if handle.is_invalid() {
            return Err(BatteryTrayError::Shell(format!(
                "CreateMutexW 返回无效句柄：{}",
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: GetLastError is called immediately after CreateMutexW on the
        // same thread, before any other Win32 operation can overwrite it.
        let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if already_exists {
            // SAFETY: `handle` is a valid newly opened mutex handle and is not
            // used after this close.
            if let Err(error) = unsafe { CloseHandle(handle) } {
                return Err(BatteryTrayError::Shell(format!(
                    "关闭重复互斥体失败：{error}"
                )));
            }
            return Ok(None);
        }
        Ok(Some(Self {
            handle,
            release_on_drop: false,
        }))
    }

    /// Creates and waits for a mutex; returns `None` on timeout or duplication.
    pub fn acquire(name: &str, timeout_ms: u32) -> Result<Option<Self>> {
        let name = wide(name);
        // SAFETY: see `create`; the UTF-16 buffer remains alive for the call.
        let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }?;
        if handle.is_invalid() {
            return Err(BatteryTrayError::Shell(format!(
                "CreateMutexW 返回无效句柄：{}",
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: `handle` is valid and `timeout_ms` is passed unchanged.
        let status = unsafe { WaitForSingleObject(handle, timeout_ms.max(1)) };
        if status == WAIT_OBJECT_0 || status == WAIT_ABANDONED {
            return Ok(Some(Self {
                handle,
                release_on_drop: true,
            }));
        }
        // SAFETY: the handle was not acquired and must be closed exactly once.
        if let Err(error) = unsafe { CloseHandle(handle) } {
            return Err(BatteryTrayError::Shell(format!("关闭互斥体失败：{error}")));
        }
        if status == WAIT_TIMEOUT {
            return Ok(None);
        }
        Err(BatteryTrayError::Shell(format!(
            "WaitForSingleObject 失败：{}",
            std::io::Error::last_os_error()
        )))
    }

    /// Checks for an existing named object without taking ownership.
    pub fn exists(name: &str) -> bool {
        matches!(Self::create(name), Ok(None))
    }
}

impl Drop for NamedMutex {
    fn drop(&mut self) {
        if self.release_on_drop {
            // SAFETY: this owner acquired the mutex and releases it once.
            if let Err(error) = unsafe { ReleaseMutex(self.handle) } {
                eprintln!("释放互斥体失败：{error}");
            }
        }
        // SAFETY: the handle is valid and this Drop is its sole final close.
        if let Err(error) = unsafe { CloseHandle(self.handle) } {
            eprintln!("关闭互斥体失败：{error}");
        }
    }
}

impl HidLock for NamedMutex {}

/// Builds the stable lock name for one configured device.
pub fn hid_lock_name(key: &str) -> String {
    let safe: String = key
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || matches!(ch, '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    format!("{HID_LOCK_PREFIX}{safe}_v1")
}
