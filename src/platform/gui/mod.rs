//! Native Windows panels: compact battery list, add-device wizard and HID debugger.

mod about;
mod debug;
mod editor;
mod panel;
pub(crate) mod task;
pub(crate) mod theme;

use crate::error::Result;
use native_windows_gui as nwg;
use windows::core::PCWSTR;
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

/// Dispatches the GUI mode selected by command-line arguments.
pub fn run() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = crate::device_api::run_command(&arguments) {
        std::process::exit(code);
    }
    if arguments.iter().any(|item| item == "--about") {
        about::run()?;
        return Ok(());
    }
    if arguments.iter().any(|item| item == "--panel") {
        panel::run()
    } else if arguments.iter().any(|item| item == "--editor") {
        let key = arguments
            .iter()
            .position(|item| item == "--key")
            .and_then(|index| arguments.get(index + 1))
            .cloned();
        editor::run(key)
    } else if arguments.iter().any(|item| item == "--debug-ui") {
        debug::run()
    } else {
        crate::platform::tray::run()
    }
}

/// Shows a startup error even when GUI initialization failed.
pub fn show_error(title: &str, message: &str) {
    let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let message: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: both UTF-16 buffers are NUL terminated and remain alive during
    // the modal call; no owner window is needed during startup failure.
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// A flat on/off control for the native panels.
///
/// A native check box always paints its box white, and the colour cannot be
/// overridden, so a plain push button carries the state instead: the label is
/// encoded into the button text and `theme` draws a rounded box plus the label.
pub(crate) struct Toggle {
    pub(crate) button: nwg::Button,
    label: String,
    on: bool,
}

impl Toggle {
    pub(crate) fn build(parent: &nwg::Window, label: &str, on: bool) -> Result<Self> {
        let mut button = nwg::Button::default();
        nwg::Button::builder().parent(parent).build(&mut button)?;
        let mut toggle = Self {
            button,
            label: label.to_owned(),
            on,
        };
        toggle.refresh();
        Ok(toggle)
    }

    pub(crate) fn get(&self) -> bool {
        self.on
    }

    pub(crate) fn set(&mut self, on: bool) {
        self.on = on;
        self.refresh();
    }

    /// Flips the state, repaints, and returns the new value.
    pub(crate) fn flip(&mut self) -> bool {
        self.on = !self.on;
        self.refresh();
        self.on
    }

    fn refresh(&mut self) {
        self.button
            .set_text(&theme::toggle_text(self.on, &self.label));
    }
}

pub(crate) fn status_text(status: crate::protocol::ReadingStatus) -> &'static str {
    use crate::protocol::ReadingStatus as S;
    match status {
        S::Ok => "正常",
        S::Charging => "充电中",
        S::Offline => "离线",
        S::NotFound => "未找到",
        S::Sleeping => "休眠",
        S::Unsupported => "不支持",
        S::Busy => "占用中",
        S::Error => "错误",
        S::Disabled => "已关闭",
        S::Unknown => "等待读取",
    }
}

/// Resource id `build.rs` assigns to `assets/app.ico` when it embeds the icon.
const APP_ICON_RESOURCE_ID: u16 = 1;

/// Installs the embedded application icon on a window's title bar and its
/// task-switcher entry.
///
/// NWG registers its window class with a null icon, and `Window::set_icon`
/// only ever sends `ICON_SMALL`, so a window keeps the generic Windows icon
/// until both slots are filled. The bitmap is loaded straight from this
/// executable's resource section (`LR_SHARED`, so it is never copied or
/// freed), which is why the icon is not bundled a second time.
pub(crate) fn apply_window_icon(window: &nwg::Window) {
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
    use windows::Win32::UI::WindowsAndMessaging::{
        LoadImageW, SendMessageW, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR, LR_SHARED,
        SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON, WM_SETICON,
    };

    let Some(raw) = window.handle.hwnd() else {
        return;
    };
    let hwnd = HWND(raw as *mut core::ffi::c_void);
    // SAFETY: `LR_SHARED` hands back a handle owned by the resource section, so
    // nothing here allocates or frees a GDI object; `WM_SETICON` takes over the
    // handle, and both sizes are re-read from the screen metrics of this
    // window's DPI so 125% and 150% scaling pick the matching icon rung.
    unsafe {
        let Ok(module) = GetModuleHandleW(None) else {
            return;
        };
        let instance = HINSTANCE(module.0);
        let name = PCWSTR(APP_ICON_RESOURCE_ID as usize as *const u16);
        let dpi = GetDpiForWindow(hwnd);
        for (slot, width, height) in [
            (ICON_SMALL, SM_CXSMICON, SM_CYSMICON),
            (ICON_BIG, SM_CXICON, SM_CYICON),
        ] {
            let width = GetSystemMetricsForDpi(width, dpi).max(16);
            let height = GetSystemMetricsForDpi(height, dpi).max(16);
            let Ok(handle) = LoadImageW(
                instance,
                name,
                IMAGE_ICON,
                width,
                height,
                LR_DEFAULTCOLOR | LR_SHARED,
            ) else {
                continue;
            };
            SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(slot as usize),
                LPARAM(handle.0 as isize),
            );
        }
    }
}

/// Loads the embedded application icon at the given pixel size.
///
/// The handle is owned by the resource section (`LR_SHARED`), so callers must
/// not destroy it. Returns `None` when the build has no icon resource or the
/// requested rung is missing.
pub(crate) fn app_icon(size: u32) -> Option<windows::Win32::UI::WindowsAndMessaging::HICON> {
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        LoadImageW, HICON, IMAGE_ICON, LR_DEFAULTCOLOR, LR_SHARED,
    };

    // SAFETY: the module handle names this executable, and `LR_SHARED` returns
    // a borrowed handle that stays valid for the lifetime of the process.
    unsafe {
        let module = GetModuleHandleW(None).ok()?;
        let handle = LoadImageW(
            HINSTANCE(module.0),
            PCWSTR(APP_ICON_RESOURCE_ID as usize as *const u16),
            IMAGE_ICON,
            size as i32,
            size as i32,
            LR_DEFAULTCOLOR | LR_SHARED,
        )
        .ok()?;
        if handle.0.is_null() {
            return None;
        }
        Some(HICON(handle.0))
    }
}

static DPI_SCALE: std::sync::OnceLock<f64> = std::sync::OnceLock::new();

/// Enables Per-Monitor V2 DPI awareness once, before any window is created.
///
/// Windows then renders our text/borders at native resolution instead of
/// stretching the whole window as a bitmap, which is what made the UI look
/// blurry at 125%. `ui_scale()` tells our layout code how much to enlarge
/// every logical coordinate so the window keeps the same physical size.
pub fn enable_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    // Applied once before creating any window; a process-wide setting.
    if let Err(error) =
        // SAFETY: SetProcessDpiAwarenessContext only changes process state and
        // is called exactly once here, before any window or DC exists.
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
    {
        eprintln!("DPI 感知设置失败：{error}");
    }
}

/// Effective scale factor of the primary monitor (1.0 at 100%, 1.25 at 125%).
///
/// `GetDpiForSystem()` cannot be used here: on machines where the scaling is
/// configured per monitor it still reports 96. `GetDpiForMonitor` returns the
/// real effective DPI regardless of the process's awareness mode.
pub fn ui_scale() -> f64 {
    *DPI_SCALE.get_or_init(|| {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
        use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
        // SAFETY: both APIs only read monitor state; pointers are valid locals.
        unsafe {
            let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            let mut dpi_x = 0_u32;
            let mut dpi_y = 0_u32;
            if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_ok()
                && dpi_x >= 96
            {
                (f64::from(dpi_x) / 96.0).clamp(1.0, 3.0)
            } else {
                1.0
            }
        }
    })
}

/// Scales a logical window size into physical pixels for `Window::builder`.
pub fn window_size(width: i32, height: i32) -> (i32, i32) {
    let scale = ui_scale();
    (
        (f64::from(width) * scale).round() as i32,
        (f64::from(height) * scale).round() as i32,
    )
}

/// Scales a logical size for list-view columns and similar measurements.
pub fn column_width(width: i32) -> i32 {
    (f64::from(width) * ui_scale()).round() as i32
}

/// Places a control using logical coordinates; the macro multiplies every
/// value by the current monitor scale before calling NWG.
#[macro_export]
macro_rules! place {
    ($control:expr, $x:expr, $y:expr, $w:expr, $h:expr) => {{
        let scale = $crate::platform::gui::ui_scale();
        $control.set_position(
            ($x as f64 * scale).round() as i32,
            ($y as f64 * scale).round() as i32,
        );
        $control.set_size(
            ($w as f64 * scale).round() as u32,
            ($h as f64 * scale).round() as u32,
        );
    }};
}

pub(crate) fn init_gui() -> Result<()> {
    enable_dpi_awareness();
    native_windows_gui::init()
        .map_err(|error| crate::error::BatteryTrayError::Gui(error.to_string()))?;
    let mut font = native_windows_gui::Font::default();
    let point_size = (16.0 * ui_scale()).round() as u32;
    match native_windows_gui::Font::builder()
        .family("Microsoft YaHei UI")
        .size_absolute(point_size)
        .build(&mut font)
    {
        Ok(()) => {
            native_windows_gui::Font::set_global_default(Some(font));
        }
        Err(error) => eprintln!("设置默认字体失败：{error}"),
    }
    Ok(())
}
