use crate::error::{BatteryTrayError, Result};
use std::fs;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use windows::core::{Interface, PCWSTR};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IPersistFile,
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, STGM_READ,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Startup, IShellLinkW, SHGetKnownFolderPath, ShellLink,
    KF_FLAG_DEFAULT, SLGP_RAWPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
};

/// Application display name used for this build's shortcuts.
///
/// The Rust build uses its own namespace so installing it never overwrites or
/// removes entries created by the Python version.
pub const APP_NAME: &str = "电量托盘 Rust";
/// Desktop launcher created by [`ensure_desktop_entries`].
pub const LAUNCHER_NAME: &str = "启动电量托盘 Rust.lnk";
/// Name owned by the original Python build; only ever inspected, never changed.
pub const LEGACY_APP_NAME: &str = "电量托盘";
/// Launcher owned by the original Python build.
const LEGACY_LAUNCHER_NAME: &str = "启动电量托盘.lnk";
/// Panel window title shared with the Python version.
pub const PANEL_TITLE: &str = "电量托盘 · 简洁面板";
/// Add-device / device-settings window title.
pub const EDITOR_TITLE: &str = "电量托盘 · 设备";
/// HID debugger window title.
pub const DEBUG_TITLE: &str = "电量托盘 · 调试版";

fn wide(text: impl AsRef<str>) -> Vec<u16> {
    text.as_ref()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> Result<Self> {
        // SAFETY: initializes COM on the calling GUI/CLI thread; no pointers are retained.
        let status = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if status.is_err() {
            return Err(BatteryTrayError::Shell(format!("COM 初始化失败：{status}")));
        }
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: balances the successful CoInitializeEx on this same thread.
        unsafe { CoUninitialize() };
    }
}

/// Restores and focuses a top-level window with an exact title.
pub fn focus_window(title: &str) -> bool {
    let title = wide(title);
    // SAFETY: title is NUL terminated and FindWindowW returns a borrowed HWND.
    let window = unsafe { FindWindowW(PCWSTR::null(), PCWSTR(title.as_ptr())) };
    let Ok(window) = window else { return false };
    if window.0.is_null() {
        return false;
    }
    // SAFETY: `window` is a live top-level HWND from FindWindowW.
    unsafe {
        let _previous = ShowWindow(window, SW_RESTORE);
        let _focused = SetForegroundWindow(window);
    }
    true
}

/// Resolves a Windows known folder with an environment fallback.
pub fn known_folder(name: &str) -> Result<PathBuf> {
    let (id, fallback) = match name.to_lowercase().replace([' ', '_', '-'], "").as_str() {
        "desktop" | "desktopdirectory" | "桌面" => (
            &FOLDERID_Desktop,
            std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join("Desktop")),
        ),
        "startup" | "startupfolder" | "启动" => (
            &FOLDERID_Startup,
            std::env::var_os("APPDATA").map(|base| {
                PathBuf::from(base).join("Microsoft\\Windows\\Start Menu\\Programs\\Startup")
            }),
        ),
        _ => {
            return Err(BatteryTrayError::Invalid(format!(
                "不支持的已知文件夹：{name}"
            )))
        }
    };
    // SAFETY: IDs are static Win32 constants and no user token is supplied.
    match unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) } {
        Ok(raw) => {
            // SAFETY: raw is a CoTaskMemAlloc string returned by the shell.
            let path = unsafe { raw.to_string() }.map(PathBuf::from);
            // SAFETY: frees the shell allocation exactly once after copying it.
            unsafe { CoTaskMemFree(Some(raw.0.cast())) };
            if let Ok(path) = path {
                return Ok(path);
            }
            fallback.ok_or_else(|| BatteryTrayError::Shell(format!("无法获取已知文件夹：{name}")))
        }
        Err(_) => {
            fallback.ok_or_else(|| BatteryTrayError::Shell(format!("无法获取已知文件夹：{name}")))
        }
    }
}

/// Creates or replaces a Windows `.lnk` shortcut.
pub fn create_shortcut(
    link_path: &Path,
    target: &Path,
    arguments: &str,
    working_dir: &Path,
    description: &str,
) -> Result<()> {
    if link_path.as_os_str().is_empty() || target.as_os_str().is_empty() {
        return Err(BatteryTrayError::Invalid(
            "快捷方式路径和目标不能为空".to_owned(),
        ));
    }
    if let Some(parent) = link_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _com = ComApartment::initialize()?;
    // SAFETY: COM is initialized on this thread and ShellLink is a documented coclass.
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }
        .map_err(|error| BatteryTrayError::Shell(format!("创建 ShellLink 失败：{error}")))?;
    let target_wide = wide(target.to_string_lossy());
    let arguments_wide = wide(arguments);
    let working_wide = wide(working_dir.to_string_lossy());
    let description_wide = wide(description);
    // SAFETY: all UTF-16 buffers are NUL terminated and remain alive for each call.
    unsafe {
        link.SetPath(PCWSTR(target_wide.as_ptr()))?;
        link.SetArguments(PCWSTR(arguments_wide.as_ptr()))?;
        link.SetWorkingDirectory(PCWSTR(working_wide.as_ptr()))?;
        link.SetDescription(PCWSTR(description_wide.as_ptr()))?;
    }
    let persist: IPersistFile = link.cast()?;
    let link_wide = wide(link_path.to_string_lossy());
    // SAFETY: the path buffer is live and `true` requests the documented save behavior.
    unsafe { persist.Save(PCWSTR(link_wide.as_ptr()), true) }?;
    Ok(())
}

/// Shortcut properties read back from a `.lnk` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutInfo {
    /// Target executable or file.
    pub target: PathBuf,
    /// Raw argument string.
    pub arguments: String,
    /// Working directory.
    pub working_dir: PathBuf,
    /// Description text.
    pub description: String,
}

/// Reads a shortcut, returning `None` for missing or invalid files.
pub fn read_shortcut(link_path: &Path) -> Option<ShortcutInfo> {
    if !link_path.is_file() {
        return None;
    }
    (|| -> Result<ShortcutInfo> {
        let _com = ComApartment::initialize()?;
        // SAFETY: COM is initialized and the object is created on this thread.
        let link: IShellLinkW =
            unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }?;
        let persist: IPersistFile = link.cast()?;
        let path_wide = wide(link_path.to_string_lossy());
        // SAFETY: path remains alive; STGM_READ requests read-only loading.
        unsafe { persist.Load(PCWSTR(path_wide.as_ptr()), STGM_READ) }?;
        let mut target = [0_u16; 1024];
        let mut arguments = [0_u16; 1024];
        let mut working = [0_u16; 1024];
        let mut description = [0_u16; 1024];
        // SAFETY: each mutable UTF-16 slice has the capacity supplied by Rust's API;
        // a null WIN32_FIND_DATAW pointer is explicitly supported by GetPath.
        unsafe {
            link.GetPath(&mut target, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32)?;
            link.GetArguments(&mut arguments)?;
            link.GetWorkingDirectory(&mut working)?;
            link.GetDescription(&mut description)?;
        }
        fn text(buffer: &[u16]) -> String {
            let length = buffer
                .iter()
                .position(|item| *item == 0)
                .unwrap_or(buffer.len());
            String::from_utf16_lossy(&buffer[..length])
        }
        Ok(ShortcutInfo {
            target: PathBuf::from(text(&target)),
            arguments: text(&arguments),
            working_dir: PathBuf::from(text(&working)),
            description: text(&description),
        })
    })()
    .ok()
}

/// Real startup-shortcut state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutostartState {
    /// Matching shortcut exists and is enabled.
    Enabled,
    /// Shortcut is absent or disabled in Task Manager.
    Disabled,
    /// Shortcut exists but its target or arguments differ.
    Mismatch,
}

fn startup_approved(name: &str) -> bool {
    let key = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER).open_subkey(
        "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\StartupFolder",
    );
    match key.and_then(|key| key.get_raw_value(format!("{name}.lnk"))) {
        Ok(data) => data.bytes.first().copied() != Some(0x02),
        Err(_) => true,
    }
}

fn clear_startup_approved(name: &str) {
    if let Ok(key) = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey_with_flags(
        "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\StartupFolder",
        winreg::enums::KEY_SET_VALUE,
    ) {
        if let Err(error) = key.delete_value(format!("{name}.lnk")) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!("清除启动项状态失败：{error}");
            }
        }
    }
}

/// Returns the startup shortcut path.
pub fn startup_shortcut_path() -> Result<PathBuf> {
    Ok(known_folder("startup")?.join(format!("{APP_NAME}.lnk")))
}

/// Returns the GUI executable, preferring the sibling `battery-tray.exe` when
/// installation is launched from the console CLI binary.
pub fn tray_executable() -> Result<PathBuf> {
    let current = std::env::current_exe()?;
    if let Some(parent) = current.parent() {
        let gui = parent.join("battery-tray.exe");
        if gui.is_file() {
            return Ok(gui);
        }
    }
    Ok(current)
}

/// Returns the GUI executable and empty tray arguments.
pub fn current_tray_target() -> Result<(PathBuf, String, PathBuf)> {
    let executable = tray_executable()?;
    let working = executable.parent().unwrap_or(Path::new(".")).to_path_buf();
    Ok((executable, String::new(), working))
}

/// Reads startup state without changing it.
pub fn autostart_state() -> AutostartState {
    let Ok(path) = startup_shortcut_path() else {
        return AutostartState::Disabled;
    };
    if !path.is_file() {
        return AutostartState::Disabled;
    }
    if !startup_approved(APP_NAME) {
        return AutostartState::Disabled;
    }
    let Ok((target, arguments, _)) = current_tray_target() else {
        return AutostartState::Mismatch;
    };
    match read_shortcut(&path) {
        Some(info)
            if paths_equal(&info.target, &target)
                && info.arguments.trim().eq_ignore_ascii_case(arguments.trim()) =>
        {
            AutostartState::Enabled
        }
        _ => AutostartState::Mismatch,
    }
}

/// Enables or disables autostart only after an explicit user action.
pub fn set_autostart(enabled: bool) -> Result<String> {
    let path = startup_shortcut_path()?;
    let (target, arguments, working) = current_tray_target()?;
    if enabled {
        create_shortcut(
            &path,
            &target,
            &arguments,
            &working,
            "电量托盘 Rust 开机自启",
        )?;
        clear_startup_approved(APP_NAME);
        if autostart_state() != AutostartState::Enabled {
            return Err(BatteryTrayError::Shell(
                "开机自启快捷方式已写入，但内容校验不一致".to_owned(),
            ));
        }
        Ok("已启用开机自启".to_owned())
    } else {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        clear_startup_approved(APP_NAME);
        Ok("已关闭开机自启".to_owned())
    }
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    if left.as_os_str().is_empty() || right.as_os_str().is_empty() {
        return false;
    }
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
        || match (fs::canonicalize(left), fs::canonicalize(right)) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
}

/// Returns whether a path is a symlink, junction or `.lnk` shortcut.
pub fn is_link(path: &Path) -> bool {
    if path
        .extension()
        .and_then(|item| item.to_str())
        .is_some_and(|item| item.eq_ignore_ascii_case("lnk"))
    {
        return path.is_file();
    }
    fs::symlink_metadata(path)
        .map(|metadata| {
            metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
                || metadata.file_type().is_symlink()
        })
        .unwrap_or(false)
}

/// Removes only a link, never its target.
pub fn remove_link(path: &Path) -> Result<bool> {
    if !is_link(path) {
        return Ok(false);
    }
    if path.is_dir() {
        fs::remove_dir(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(true)
}

/// Creates a directory symlink with junction fallback.
pub fn create_dir_link(link: &Path, target: &Path) -> Result<&'static str> {
    if !target.is_dir() {
        return Err(BatteryTrayError::Invalid(format!(
            "目标目录不存在：{}",
            target.display()
        )));
    }
    if link.exists() && !remove_link(link)? {
        return Err(BatteryTrayError::Invalid(format!(
            "路径已存在且不是链接，拒绝覆盖：{}",
            link.display()
        )));
    }
    if std::os::windows::fs::symlink_dir(target, link).is_ok() {
        return Ok("symlink");
    }
    let status = Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .status()?;
    if !status.success() {
        return Err(BatteryTrayError::Shell(format!(
            "创建目录 junction 失败：{status}"
        )));
    }
    Ok("junction")
}

/// Creates a file symlink, hard link or `.lnk` fallback.
pub fn create_file_link(link: &Path, target: &Path) -> Result<&'static str> {
    if link.exists() && !remove_link(link)? {
        return Err(BatteryTrayError::Invalid(format!(
            "路径已存在且不是链接，拒绝覆盖：{}",
            link.display()
        )));
    }
    if target.is_file() {
        if std::os::windows::fs::symlink_file(target, link).is_ok() {
            return Ok("symlink");
        }
        if fs::hard_link(target, link).is_ok() {
            return Ok("hardlink");
        }
    }
    let shortcut = if link
        .extension()
        .and_then(|item| item.to_str())
        .is_some_and(|item| item.eq_ignore_ascii_case("lnk"))
    {
        link.to_path_buf()
    } else {
        PathBuf::from(format!("{}.lnk", link.to_string_lossy()))
    };
    create_shortcut(
        &shortcut,
        target,
        "",
        target.parent().unwrap_or(Path::new(".")),
        "电量托盘启动入口",
    )?;
    Ok("shortcut")
}

/// Creates or replaces one desktop shortcut for the GUI executable without
/// touching startup entries or any other desktop items.
pub fn install_desktop_shortcut(name: &str) -> Result<PathBuf> {
    if name.trim().is_empty() {
        return Err(BatteryTrayError::Invalid("快捷方式名称不能为空".to_owned()));
    }
    let desktop = known_folder("desktop")?;
    let link = desktop.join(format!("{}.lnk", name.trim()));
    let executable = tray_executable()?;
    let working = executable.parent().unwrap_or(Path::new("."));
    create_shortcut(&link, &executable, "", working, "电量托盘（Rust 原生版）")?;
    let verified = read_shortcut(&link).is_some_and(|info| paths_equal(&info.target, &executable));
    if !verified {
        return Err(BatteryTrayError::Shell(
            "桌面快捷方式已写入，但回读校验失败".to_owned(),
        ));
    }
    Ok(link)
}

/// One installed desktop entry.
#[derive(Debug, Clone)]
pub struct DesktopEntry {
    /// Entry type (`dir` or `file`).
    pub kind: &'static str,
    /// Link path.
    pub path: PathBuf,
    /// Link target.
    pub target: PathBuf,
    /// Creation method.
    pub method: &'static str,
}

/// Returns startup and desktop entries created by the Python build.
///
/// Used only to warn the user during installation; this build never edits or
/// removes them.
pub fn legacy_entries() -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Ok(desktop) = known_folder("desktop") {
        for name in [LEGACY_APP_NAME, LEGACY_LAUNCHER_NAME] {
            let path = desktop.join(name);
            if path.exists() {
                found.push(path);
            }
        }
    }
    if let Ok(startup) = known_folder("startup") {
        let path = startup.join(format!("{LEGACY_APP_NAME}.lnk"));
        if path.exists() {
            found.push(path);
        }
    }
    found
}

/// Installs a project-directory link and native executable launcher on Desktop.
pub fn ensure_desktop_entries(project_dir: &Path, executable: &Path) -> Result<Vec<DesktopEntry>> {
    let desktop = known_folder("desktop")?;
    let directory_link = desktop.join(APP_NAME);
    let method = if directory_link.exists() && !is_link(&directory_link) {
        "existing"
    } else {
        create_dir_link(&directory_link, project_dir)?
    };
    let launcher = desktop.join(LAUNCHER_NAME);
    create_shortcut(
        &launcher,
        executable,
        "",
        executable.parent().unwrap_or(project_dir),
        "启动电量托盘（Rust 原生版）",
    )?;
    Ok(vec![
        DesktopEntry {
            kind: "dir",
            path: directory_link,
            target: project_dir.to_path_buf(),
            method,
        },
        DesktopEntry {
            kind: "file",
            path: launcher,
            target: executable.to_path_buf(),
            method: "shortcut",
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_names_do_not_collide_with_python_build() {
        assert_ne!(APP_NAME, LEGACY_APP_NAME);
        assert!(LAUNCHER_NAME.contains(APP_NAME));
        assert_ne!(LAUNCHER_NAME, LEGACY_LAUNCHER_NAME);
        if let Ok(path) = startup_shortcut_path() {
            assert_eq!(
                path.file_name().and_then(|name| name.to_str()),
                Some("电量托盘 Rust.lnk")
            );
        }
    }
}
