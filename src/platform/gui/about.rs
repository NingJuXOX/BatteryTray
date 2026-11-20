//! The "about" window: what the app is, where it lives and how to close it.

use super::{init_gui, theme};
use crate::error::Result;
use crate::platform::shell;
use crate::platform::sync::{NamedMutex, ABOUT_MUTEX_NAME};
use native_windows_gui as nwg;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::SendMessageW;

/// Logical window size; `window_size` scales it for the current DPI.
const WINDOW_WIDTH: i32 = 460;
const WINDOW_HEIGHT: i32 = 372;
/// Pixels the application icon is drawn at inside the window.
const ICON_SIZE: u32 = 64;
/// Logical position of the icon inside the window.
const ICON_X: i32 = 24;
const ICON_Y: i32 = 24;
/// `SS_ICON`, `SS_CENTERIMAGE`, `STM_SETIMAGE` and `IMAGE_ICON` have no
/// counterpart in the `windows` crate, which only wraps the messages that need
/// typed wrappers.
const SS_ICON: u32 = 0x0003;
const SS_CENTERIMAGE: u32 = 0x0200;
const STM_SETIMAGE: u32 = 0x0172;
const IMAGE_ICON: u32 = 1;

pub fn run() -> Result<()> {
    let instance = NamedMutex::create(ABOUT_MUTEX_NAME)?;
    if instance.is_none() {
        shell::focus_window(shell::ABOUT_TITLE);
        return Ok(());
    }
    init_gui()?;

    let mut window = nwg::Window::default();
    nwg::Window::builder()
        .title(shell::ABOUT_TITLE)
        .size(crate::platform::gui::window_size(
            WINDOW_WIDTH,
            WINDOW_HEIGHT,
        ))
        .center(true)
        .flags(nwg::WindowFlags::WINDOW | nwg::WindowFlags::VISIBLE)
        .build(&mut window)?;
    theme::apply(&window);
    crate::platform::gui::apply_window_icon(&window);

    // Kept only so the handles stay reachable in this scope; the child windows
    // belong to `window` and are destroyed together with it.
    let _icon_frame = create_icon_frame(&window);

    let name_font = font(22.0, 700)?;
    let version_font = font(12.0, 400)?;
    let body_font = font(13.0, 400)?;

    let name = label(&window, "电量托盘 Rust", &name_font)?;
    let version = label(
        &window,
        &format!(
            "版本 {} · 单文件 exe，无需 Python 或 .NET",
            env!("CARGO_PKG_VERSION")
        ),
        &version_font,
    )?;
    let summary = label(&window, shell::ABOUT_SUMMARY, &body_font)?;
    let address_title = label(&window, "项目地址", &version_font)?;
    let address = label(&window, shell::PROJECT_URL, &body_font)?;

    let mut open = nwg::Button::default();
    nwg::Button::builder()
        .text("打开项目主页")
        .parent(&window)
        .build(&mut open)?;
    let mut close = nwg::Button::default();
    nwg::Button::builder()
        .text("关闭")
        .parent(&window)
        .build(&mut close)?;

    crate::place!(name, 100, 28, 336, 32);
    crate::place!(version, 100, 64, 336, 22);
    crate::place!(summary, 24, 108, 412, 76);
    crate::place!(address_title, 24, 196, 412, 20);
    crate::place!(address, 24, 218, 412, 22);
    crate::place!(open, 24, 254, 150, 32);
    crate::place!(close, 348, 306, 88, 34);
    theme::polish(&window);

    let mut handlers = Vec::new();
    let handle = nwg::full_bind_event_handler(&window.handle, move |event, _data, event_handle| {
        if event == nwg::Event::OnWindowClose && event_handle == window.handle {
            nwg::stop_thread_dispatch();
        } else if event == nwg::Event::OnButtonClick {
            if event_handle == open.handle {
                if let Err(error) = shell::open_url(shell::PROJECT_URL) {
                    super::show_error("打开项目主页失败", &error.to_string());
                }
            } else if event_handle == close.handle {
                nwg::stop_thread_dispatch();
            }
        }
    });
    handlers.push(handle);

    nwg::dispatch_thread_events();
    for handler in handlers.drain(..) {
        nwg::unbind_event_handler(&handler);
    }
    drop(instance);
    Ok(())
}

/// Builds the static control that shows the application icon.
///
/// `nwg::ImageFrame` is deliberately not used here: unless it is handed an
/// `nwg::Icon` while it is built it is created as `SS_BITMAP`, and NWG offers no
/// way to pass in a handle the executable already loaded. Switching the style
/// afterwards leaves the bitmap branch's cached state on screen, which shows up
/// as a full-size icon fragment in the frame's corner. A control created as
/// `SS_ICON` paints cleanly from the first frame, and `SS_CENTERIMAGE` keeps the
/// icon at its natural pixel size instead of stretching it to the frame.
fn create_icon_frame(window: &nwg::Window) -> Option<HWND> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, WINDOW_EX_STYLE, WINDOW_STYLE, WS_CHILD, WS_VISIBLE,
    };

    let raw = window.handle.hwnd()?;
    let parent = HWND(raw as *mut core::ffi::c_void);
    let scale = crate::platform::gui::ui_scale();
    let size = (f64::from(ICON_SIZE) * scale).round() as i32;
    let x = (f64::from(ICON_X) * scale).round() as i32;
    let y = (f64::from(ICON_Y) * scale).round() as i32;
    let class: Vec<u16> = "Static".encode_utf16().chain(std::iter::once(0)).collect();
    let style = WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | SS_ICON | SS_CENTERIMAGE);
    // SAFETY: the parent handle belongs to the window built above, the class
    // name buffer outlives the call, and the style only asks for a child static
    // control owned by this process.
    let hwnd = unsafe {
        let module = GetModuleHandleW(None).ok()?;
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR::null(),
            style,
            x,
            y,
            size,
            size,
            parent,
            None,
            HINSTANCE(module.0),
            None,
        )
        .ok()?
    };
    if let Some(icon) = crate::platform::gui::app_icon(size as u32) {
        // SAFETY: `hwnd` is the static control created above and the icon handle
        // is owned by this executable's resource section (`LR_SHARED`), so the
        // control must not destroy it.
        unsafe {
            let _previous = SendMessageW(
                hwnd,
                STM_SETIMAGE,
                WPARAM(IMAGE_ICON as usize),
                LPARAM(icon.0 as isize),
            );
        }
    }
    Some(hwnd)
}

fn font(logical_size: f64, weight: u32) -> Result<nwg::Font> {
    let pixels = (logical_size * crate::platform::gui::ui_scale())
        .round()
        .max(1.0) as u32;
    let mut font = nwg::Font::default();
    nwg::Font::builder()
        .family("Microsoft YaHei UI")
        .size_absolute(pixels)
        .weight(weight)
        .build(&mut font)?;
    Ok(font)
}

/// Creates a dark label; `theme::style_label` repaints the strip NWG leaves for
/// its own centring so no white hairline shows through.
fn label(parent: &nwg::Window, text: &str, font: &nwg::Font) -> Result<nwg::Label> {
    let mut label = nwg::Label::default();
    nwg::Label::builder()
        .text(text)
        .parent(parent)
        .build(&mut label)?;
    theme::style_label(&label);
    label.set_font(Some(font));
    Ok(label)
}
