//! The "about" window: what the app is, where it lives and how to close it.

use super::{init_gui, theme};
use crate::error::Result;
use crate::platform::shell;
use crate::platform::sync::{NamedMutex, ABOUT_MUTEX_NAME};
use native_windows_gui as nwg;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongW, SendMessageW, SetWindowLongW, GWL_STYLE,
};

/// Logical window size; `window_size` scales it for the current DPI.
const WINDOW_WIDTH: i32 = 460;
const WINDOW_HEIGHT: i32 = 372;
/// Pixels the application icon is drawn at inside the window.
const ICON_SIZE: u32 = 64;
const SS_ICON: u32 = 0x0003;
const SS_BITMAP: u32 = 0x000E;
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

    let mut icon_frame = nwg::ImageFrame::default();
    nwg::ImageFrame::builder()
        .parent(&window)
        .size((
            (ICON_SIZE as f64 * crate::platform::gui::ui_scale()).round() as i32,
            (ICON_SIZE as f64 * crate::platform::gui::ui_scale()).round() as i32,
        ))
        .background_color(Some(theme::WINDOW_BG))
        .build(&mut icon_frame)?;
    if let Some(icon) = crate::platform::gui::app_icon(ICON_SIZE) {
        show_icon(&icon_frame, icon);
    }

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

    crate::place!(icon_frame, 24, 24, ICON_SIZE, ICON_SIZE);
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

/// Replaces the frame's (absent) bitmap with the shared application icon.
///
/// `ImageFrame` picks `SS_BITMAP` when it is built without an image, and NWG
/// gives no way to hand it a handle the executable already loaded, so the style
/// is switched to `SS_ICON` here and the shared icon is pushed straight into the
/// static control.
fn show_icon(frame: &nwg::ImageFrame, icon: windows::Win32::UI::WindowsAndMessaging::HICON) {
    let Some(raw) = frame.handle.hwnd() else {
        return;
    };
    let hwnd = HWND(raw as *mut core::ffi::c_void);
    // SAFETY: `hwnd` belongs to the frame built above; the style update only
    // swaps SS_BITMAP for SS_ICON, and the icon handle is owned by the resource
    // section because it was loaded with LR_SHARED.
    unsafe {
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        SetWindowLongW(hwnd, GWL_STYLE, ((style & !SS_BITMAP) | SS_ICON) as i32);
        let _previous = SendMessageW(
            hwnd,
            STM_SETIMAGE,
            WPARAM(IMAGE_ICON as usize),
            LPARAM(icon.0 as isize),
        );
        let _painted = InvalidateRect(hwnd, None, BOOL(1));
    }
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
