//! Dark, flat visual theme for the native UI.
//!
//! No new dependencies: NWG's raw event hook handles `WM_CTLCOLOR*` (background
//! and text colours) and `WM_DRAWITEM` (borderless buttons). All colours live
//! here so the three windows stay in sync and the theme is easy to tweak.

use native_windows_gui as nwg;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, CreatePen, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawTextW,
    FillRect, FillRgn, GetWindowDC, InvalidateRect, LineTo, MoveToEx, ReleaseDC, SelectObject,
    SetBkColor, SetBkMode, SetTextColor, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, DRAW_TEXT_FORMAT, HBRUSH, HDC, HFONT, HGDIOBJ, OPAQUE, PS_SOLID, SRCCOPY,
    TRANSPARENT,
};
use windows::Win32::UI::Controls::DRAWITEMSTRUCT;
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, GetClassNameW, GetClientRect, GetWindowRect, GetWindowTextW, SendMessageW,
    SetWindowLongPtrW, SetWindowPos, GWLP_WNDPROC, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, WM_CTLCOLORBTN, WM_CTLCOLORDLG, WM_CTLCOLOREDIT, WM_CTLCOLORLISTBOX,
    WM_CTLCOLORSTATIC, WM_GETFONT, WM_NCPAINT, WM_PAINT, WNDPROC,
};

const WM_DRAWITEM: u32 = 0x002B;
const WM_ERASEBKGND: u32 = 0x0014;
const WM_NOTIFY: u32 = 0x004E;
const NM_CUSTOMDRAW: u32 = (-12i32) as u32;
const LVM_SETBKCOLOR: u32 = 0x1001;
const LVM_SETTEXTCOLOR: u32 = 0x1024;
const LVM_SETTEXTBKCOLOR: u32 = 0x1026;
const BS_OWNERDRAW: u32 = 0x000B;
const BS_FLAT: u32 = 0x8000;
const ODT_COMBOBOX: u32 = 3;
const CB_GETLBTEXT: u32 = 0x0148;
const CB_GETCURSEL: u32 = 0x0147;
const CBS_OWNERDRAWFIXED: u32 = 0x0010;
const CBS_HASSTRINGS: u32 = 0x0200;

/// Deep slate window surface.
pub const WINDOW_BG: [u8; 3] = [22, 26, 34];
/// Slightly lighter card surface for lists and editors.
pub const CARD_BG: [u8; 3] = [33, 39, 51];
/// Main text / list text.
pub const TEXT: [u8; 3] = [226, 232, 240];
/// Muted text for disabled controls.
pub const MUTED: [u8; 3] = [148, 163, 184];
/// Subtle blue-grey window border.
pub const BORDER: [u8; 3] = [64, 76, 100];
/// Button surface: deliberately lighter than the window, no outline needed.
pub const BUTTON_BG: [u8; 3] = [54, 70, 100];
/// Button surface while pressed.
pub const BUTTON_ACTIVE_BG: [u8; 3] = [78, 100, 140];
/// Button surface when disabled.
pub const BUTTON_DISABLED_BG: [u8; 3] = [38, 44, 56];
/// Button label colour.
pub const BUTTON_TEXT: [u8; 3] = [240, 245, 252];
/// Device card surface: lighter than the window, no outline needed.
pub const CARD_SURFACE: [u8; 3] = [42, 50, 66];
/// Toggle box while the switch is off.
pub const CHECK_BOX_BG: [u8; 3] = [64, 80, 112];
/// Toggle box while the switch is on.
pub const CHECK_BOX_ON: [u8; 3] = [86, 128, 214];
/// First character of a toggle button's text; it marks the button as an on/off
/// switch so the custom paint knows to draw a box instead of a label only.
pub(crate) const TOGGLE_MARK: char = '\u{1}';

/// Encodes a toggle button's label and state into its window text.
pub(crate) fn toggle_text(on: bool, label: &str) -> String {
    format!("{TOGGLE_MARK}{}\n{label}", u8::from(on))
}

/// First character of an icon tile's text.
pub(crate) const ICON_MARK: char = '\u{2}';

/// Encodes an icon tile: which built-in shape it shows, whether it is the
/// selected one, and the caption underneath.
pub(crate) fn icon_text(kind: &str, selected: bool, label: &str) -> String {
    format!("{ICON_MARK}{kind}\n{}\n{label}", u8::from(selected))
}

/// Corner radius shared by buttons and fields, in logical pixels.
/// The window regions are built from the same value so fills and clips match.
const CORNER_RADIUS: f64 = 12.0;
/// Slightly tighter radius for the shorter combo box, so it stays a rounded
/// rectangle instead of turning into a pill.
const FIELD_RADIUS: f64 = 8.0;
fn colorref([r, g, b]: [u8; 3]) -> COLORREF {
    COLORREF((u32::from(b) << 16) | (u32::from(g) << 8) | u32::from(r))
}

fn solid_brush(color: [u8; 3]) -> isize {
    // SAFETY: CreateSolidBrush only allocates a GDI object; it is leaked on
    // purpose so the handle stays valid for the process lifetime.
    unsafe { CreateSolidBrush(colorref(color)) }.0 as isize
}

fn brush(slot: &'static OnceLock<isize>, color: [u8; 3]) -> isize {
    *slot.get_or_init(|| solid_brush(color))
}

static WINDOW_BRUSH: OnceLock<isize> = OnceLock::new();
static CARD_BRUSH: OnceLock<isize> = OnceLock::new();
static BUTTON_BRUSH: OnceLock<isize> = OnceLock::new();
static ACTIVE_BRUSH: OnceLock<isize> = OnceLock::new();
static DISABLED_BRUSH: OnceLock<isize> = OnceLock::new();
static SURFACE_BRUSH: OnceLock<isize> = OnceLock::new();
static NAME_FONT: OnceLock<isize> = OnceLock::new();
static PERCENT_FONT: OnceLock<isize> = OnceLock::new();
static STATUS_FONT: OnceLock<isize> = OnceLock::new();

fn window_brush() -> isize {
    brush(&WINDOW_BRUSH, WINDOW_BG)
}

fn card_brush() -> isize {
    brush(&CARD_BRUSH, CARD_BG)
}

fn button_brush_for(color: [u8; 3]) -> isize {
    if color == BUTTON_ACTIVE_BG {
        brush(&ACTIVE_BRUSH, color)
    } else if color == BUTTON_DISABLED_BG {
        brush(&DISABLED_BRUSH, color)
    } else {
        brush(&BUTTON_BRUSH, BUTTON_BG)
    }
}

fn surface_brush_for(color: [u8; 3]) -> isize {
    if color == BUTTON_ACTIVE_BG {
        brush(&ACTIVE_BRUSH, color)
    } else {
        brush(&SURFACE_BRUSH, color)
    }
}

/// Sets the colours of a control's device context and returns its background
/// brush, as required by every `WM_CTLCOLOR*` handler.
///
/// Native edit/static/list controls redraw text incrementally: with a
/// transparent background mode the previous glyphs are never erased and the
/// text piles up into unreadable "garbled" lines. OPAQUE + an explicit
/// background colour makes each repaint clean the glyph area first.
fn paint_control(hdc: HDC, background: [u8; 3], foreground: [u8; 3], surface: isize) -> isize {
    // SAFETY: hdc is the live DC passed in wParam of a WM_CTLCOLOR* message.
    unsafe {
        _ = SetBkMode(hdc, OPAQUE);
        _ = SetTextColor(hdc, colorref(foreground));
        _ = SetBkColor(hdc, colorref(background));
    }
    surface
}

/// Converts a logical coordinate into physical pixels for the current DPI.
fn px(logical: f64) -> i32 {
    (logical * crate::platform::gui::ui_scale()).round() as i32
}

fn handle_brush(value: isize) -> HBRUSH {
    HBRUSH(value as *mut core::ffi::c_void)
}

/// Lazily creates (and caches) a Microsoft YaHei UI font at the given logical
/// size. Uses the same CreateFontW parameters as NWG's global font.
fn card_font(slot: &'static OnceLock<isize>, logical_size: f64, weight: i32) -> HFONT {
    let handle = *slot.get_or_init(|| {
        // SAFETY: CreateFontW copies the face name during the call.
        unsafe {
            let face: Vec<u16> = "Microsoft YaHei UI"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            CreateFontW(
                px(logical_size),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                1, // DEFAULT_CHARSET
                0, // OUT_DEFAULT_PRECIS
                0, // CLIP_DEFAULT_PRECIS
                5, // CLEARTYPE_QUALITY
                2, // VARIABLE_PITCH
                PCWSTR(face.as_ptr()),
            )
            .0 as isize
        }
    });
    HFONT(handle as *mut core::ffi::c_void)
}

fn draw_text_line(hdc: HDC, text: &str, rect: &mut RECT, font: HFONT, color: [u8; 3]) {
    let mut buffer: Vec<u16> = text.encode_utf16().collect();
    if buffer.is_empty() {
        return;
    }
    // SAFETY: hdc/font are live GDI objects; rect is a valid output rectangle.
    unsafe {
        if !font.0.is_null() {
            _ = SelectObject(hdc, HGDIOBJ(font.0));
        }
        _ = SetTextColor(hdc, colorref(color));
        _ = DrawTextW(hdc, &mut buffer, rect, DRAW_TEXT_FORMAT(4 | 32));
    }
}

/// Draws the exact device silhouette used by the tray icon, without its
/// battery bar. RGBA pixels are composited onto the card colour in software so
/// a plain StretchDIBits (SRCCOPY) is enough - no alpha-capable API needed.
fn draw_glyph(hdc: HDC, kind: &str, x: i32, y: i32, size: i32, background: [u8; 3]) {
    let size = size.clamp(8, 256);
    let image = crate::icon::device_glyph_rgba(kind, size as u32, TEXT);
    let width = image.width() as i32;
    let height = image.height() as i32;
    let mut bits: Vec<u8> = Vec::with_capacity(image.as_raw().len());
    for pixel in image.as_raw().chunks_exact(4) {
        let alpha = f64::from(pixel[3]) / 255.0;
        let mix = |channel: u8, background: u8| {
            (f64::from(channel) * alpha + f64::from(background) * (1.0 - alpha)).round() as u8
        };
        bits.push(mix(pixel[2], background[2])); // B
        bits.push(mix(pixel[1], background[1])); // G
        bits.push(mix(pixel[0], background[0])); // R
        bits.push(0);
    }
    // SAFETY: BITMAPINFO is a POD; zeroing it is a valid initial state.
    let mut info: BITMAPINFO = unsafe { std::mem::zeroed() };
    info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = width;
    info.bmiHeader.biHeight = -height; // top-down DIB
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB.0;
    // SAFETY: hdc is live; bits/info describe a valid 32bpp top-down DIB.
    unsafe {
        if StretchDIBits(
            hdc,
            x,
            y,
            width,
            height,
            0,
            0,
            width,
            height,
            Some(bits.as_ptr() as *const core::ffi::c_void),
            &info,
            DIB_RGB_COLORS,
            SRCCOPY,
        ) < 0
        {
            // GDI_ERROR: the glyph is cosmetic, keep the text visible.
        }
    }
}

/// Draws one device card: a rounded, borderless colour block containing the
/// tray-style device silhouette, the name, the percentage and a status line.
///
/// Card buttons encode their data as four newline-separated fields so the
/// drawing stays inside this module and no extra child controls are needed.
fn draw_card(item: &DRAWITEMSTRUCT, text: &str) {
    let hdc = item.hDC;
    let r = item.rcItem;
    let mut parts = text.split('\n');
    let name = parts.next().unwrap_or("");
    let percent = parts
        .next()
        .unwrap_or("")
        .trim()
        .parse::<i32>()
        .unwrap_or(-1);
    let status = parts.next().unwrap_or("");
    let kind = parts.next().unwrap_or("device");

    // SAFETY: hdc belongs to the control being drawn; all rectangles are
    // derived from its own rcItem and every brush/font is process-lifetime.
    unsafe {
        let previous = SelectObject(hdc, HGDIOBJ(card_font(&NAME_FONT, 14.0, 400).0));
        _ = SetBkMode(hdc, OPAQUE);
        _ = SetBkColor(hdc, colorref(CARD_SURFACE));

        // Fill through a rounded region so the corners are unambiguously
        // rounded, with no pen stroke or border anywhere.
        let diameter = px(12.0) * 2;
        let region =
            CreateRoundRectRgn(r.left, r.top, r.right + 1, r.bottom + 1, diameter, diameter);
        if !FillRgn(
            hdc,
            region,
            handle_brush(brush(&SURFACE_BRUSH, CARD_SURFACE)),
        )
        .as_bool()
        {
            // Cosmetic only.
        }
        if !DeleteObject(HGDIOBJ(region.0)).as_bool() {
            // Cosmetic only.
        }

        let glyph_size = px(44.0);
        let glyph_x = r.left + px(18.0);
        let glyph_y = r.top + ((r.bottom - r.top) - glyph_size) / 2;
        draw_glyph(hdc, kind, glyph_x, glyph_y, glyph_size, CARD_SURFACE);

        let text_x = r.left + px(74.0);
        let mut name_rect = RECT {
            left: text_x,
            top: r.top + px(14.0),
            right: r.right - px(14.0),
            bottom: r.top + px(38.0),
        };
        draw_text_line(
            hdc,
            name,
            &mut name_rect,
            card_font(&NAME_FONT, 14.0, 400),
            TEXT,
        );

        let shown = if percent < 0 {
            "--".to_owned()
        } else {
            format!("{percent}%")
        };
        let mut percent_rect = RECT {
            left: text_x,
            top: r.top + px(36.0),
            right: r.right - px(14.0),
            bottom: r.top + px(74.0),
        };
        draw_text_line(
            hdc,
            &shown,
            &mut percent_rect,
            card_font(&PERCENT_FONT, 27.0, 700),
            BUTTON_TEXT,
        );

        let mut status_rect = RECT {
            left: text_x,
            top: r.bottom - px(30.0),
            right: r.right - px(14.0),
            bottom: r.bottom - px(8.0),
        };
        draw_text_line(
            hdc,
            status,
            &mut status_rect,
            card_font(&STATUS_FONT, 11.0, 400),
            MUTED,
        );
        _ = SelectObject(hdc, previous);
    }
}

/// Draws a toggle button: a rounded on/off box followed by its label.
///
/// The state lives in the button text (see `toggle_text`), so no native check
/// box is involved - the system would always paint that one white.
fn draw_toggle(item: &DRAWITEMSTRUCT, text: &str) {
    let r = item.rcItem;
    let hdc = item.hDC;
    let pressed = item.itemState.0 & 0x0001 != 0; // ODS_SELECTED
    let disabled = item.itemState.0 & 0x0004 != 0; // ODS_DISABLED
    let body = text.trim_start_matches(TOGGLE_MARK);
    let (state, label) = body.split_once('\n').unwrap_or((body, ""));
    let on = state.starts_with('1');

    // SAFETY: hdc belongs to the control being drawn and every rectangle comes
    // from its own rcItem; brushes and fonts live for the process lifetime.
    unsafe {
        _ = FillRect(hdc, &r, HBRUSH(window_brush() as *mut core::ffi::c_void));
        if pressed {
            let diameter = px(CORNER_RADIUS) * 2;
            let region =
                CreateRoundRectRgn(r.left, r.top, r.right + 1, r.bottom + 1, diameter, diameter);
            _ = FillRgn(hdc, region, handle_brush(brush(&BUTTON_BRUSH, BUTTON_BG)));
            _ = DeleteObject(HGDIOBJ(region.0));
        }

        let size = px(16.0).max(12);
        let left = r.left + px(2.0);
        let top = r.top + ((r.bottom - r.top) - size) / 2;
        let diameter = px(4.0) * 2;
        let region = CreateRoundRectRgn(
            left,
            top,
            left + size + 1,
            top + size + 1,
            diameter,
            diameter,
        );
        let fill = if disabled {
            brush(&DISABLED_BRUSH, BUTTON_DISABLED_BG)
        } else if on {
            brush(&CHECK_ON_BRUSH, CHECK_BOX_ON)
        } else {
            brush(&CHECK_OFF_BRUSH, CHECK_BOX_BG)
        };
        _ = FillRgn(hdc, region, handle_brush(fill));
        _ = DeleteObject(HGDIOBJ(region.0));

        if on && !disabled {
            let pen = CreatePen(PS_SOLID, px(1.6).max(2), colorref(BUTTON_TEXT));
            let previous = SelectObject(hdc, HGDIOBJ(pen.0));
            let edge = f64::from(size);
            let point = |fx: f64, fy: f64| (left + (edge * fx) as i32, top + (edge * fy) as i32);
            let (start_x, start_y) = point(0.24, 0.50);
            let (mid_x, mid_y) = point(0.43, 0.70);
            let (end_x, end_y) = point(0.77, 0.29);
            _ = MoveToEx(hdc, start_x, start_y, None);
            _ = LineTo(hdc, mid_x, mid_y);
            _ = LineTo(hdc, end_x, end_y);
            _ = SelectObject(hdc, previous);
            _ = DeleteObject(HGDIOBJ(pen.0));
        }

        let font = SendMessageW(item.hwndItem, WM_GETFONT, WPARAM(0), LPARAM(0));
        let font = HFONT(font.0 as *mut core::ffi::c_void);
        let previous = if font.0.is_null() {
            None
        } else {
            Some(SelectObject(hdc, HGDIOBJ(font.0)))
        };
        let mut buffer: Vec<u16> = label.encode_utf16().collect();
        if !buffer.is_empty() {
            _ = SetBkMode(hdc, TRANSPARENT);
            _ = SetTextColor(hdc, colorref(if disabled { MUTED } else { TEXT }));
            let mut text_rect = RECT {
                left: left + size + px(8.0),
                top: r.top,
                right: r.right,
                bottom: r.bottom,
            };
            // DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS
            _ = DrawTextW(
                hdc,
                &mut buffer,
                &mut text_rect,
                DRAW_TEXT_FORMAT(4 | 32 | 0x8000),
            );
        }
        if let Some(previous) = previous {
            _ = SelectObject(hdc, previous);
        }
    }
}

/// Draws one icon tile: a rounded surface, the device silhouette and a caption.
///
/// The selected tile uses the accent surface; the chosen shape is what the
/// panel card and the tray icon will use later.
fn draw_icon_tile(item: &DRAWITEMSTRUCT, text: &str) {
    let r = item.rcItem;
    let hdc = item.hDC;
    let pressed = item.itemState.0 & 0x0001 != 0; // ODS_SELECTED
    let body = text.trim_start_matches(ICON_MARK);
    let mut parts = body.split('\n');
    let kind = parts.next().unwrap_or("device");
    let selected = parts.next().unwrap_or("0").starts_with('1');
    let label = parts.next().unwrap_or("");

    // SAFETY: hdc belongs to the control being drawn; every rectangle comes from
    // its own rcItem and all brushes/fonts live for the process lifetime.
    unsafe {
        _ = FillRect(hdc, &r, HBRUSH(window_brush() as *mut core::ffi::c_void));
        let surface = if selected {
            BUTTON_ACTIVE_BG
        } else if pressed {
            BUTTON_BG
        } else {
            CARD_SURFACE
        };
        let diameter = px(CORNER_RADIUS) * 2;
        let region =
            CreateRoundRectRgn(r.left, r.top, r.right + 1, r.bottom + 1, diameter, diameter);
        _ = FillRgn(hdc, region, handle_brush(surface_brush_for(surface)));
        _ = DeleteObject(HGDIOBJ(region.0));

        let glyph = px(34.0);
        let glyph_x = r.left + ((r.right - r.left) - glyph) / 2;
        let glyph_y = r.top + px(12.0);
        draw_glyph(hdc, kind, glyph_x, glyph_y, glyph, surface);

        let font = card_font(&STATUS_FONT, 11.0, 400);
        let previous = SelectObject(hdc, HGDIOBJ(font.0));
        _ = SetBkMode(hdc, TRANSPARENT);
        _ = SetTextColor(hdc, colorref(BUTTON_TEXT));
        let mut buffer: Vec<u16> = label.encode_utf16().collect();
        if !buffer.is_empty() {
            let mut rect = RECT {
                left: r.left,
                top: r.bottom - px(28.0),
                right: r.right,
                bottom: r.bottom - px(6.0),
            };
            // DT_CENTER | DT_VCENTER | DT_SINGLELINE
            _ = DrawTextW(hdc, &mut buffer, &mut rect, DRAW_TEXT_FORMAT(1 | 4 | 32));
        }
        _ = SelectObject(hdc, previous);
    }
}

/// Paints an owner-draw push button as a solid colour block with no border.
///
/// Colour alone separates it from the window: normal, pressed and disabled each
/// get their own shade. The block is filled through a rounded region so the
/// corners are round even where the window region is not applied (owner-draw
/// controls receive a plain rectangular clip from the system).
fn draw_button(lparam: isize) {
    if lparam == 0 {
        return;
    }
    // SAFETY: WM_DRAWITEM always supplies a valid, read-only DRAWITEMSTRUCT
    // with a live DC; the GDI objects used here belong to that DC or the control.
    unsafe {
        let item = &*(lparam as *const DRAWITEMSTRUCT);
        if item.CtlType.0 != 4 {
            // ODT_BUTTON == 4; leave other owner-draw controls to Windows.
            return;
        }
        // Device cards are push buttons whose text holds three newline-separated
        // fields; anything else is an ordinary toolbar button.
        let mut card_label = [0u16; 256];
        let card_length = GetWindowTextW(item.hwndItem, &mut card_label);
        if card_length > 0 {
            let length = (card_length as usize).min(card_label.len());
            let text = String::from_utf16_lossy(&card_label[..length]);
            if text.starts_with(TOGGLE_MARK) {
                draw_toggle(item, &text);
                return;
            }
            if text.starts_with(ICON_MARK) {
                draw_icon_tile(item, &text);
                return;
            }
            if text.contains('\n') {
                draw_card(item, &text);
                return;
            }
        }
        let disabled = item.itemState.0 & 0x0004 != 0; // ODS_DISABLED
        let pressed = item.itemState.0 & 0x0001 != 0; // ODS_SELECTED
        let background = if disabled {
            BUTTON_DISABLED_BG
        } else if pressed {
            BUTTON_ACTIVE_BG
        } else {
            BUTTON_BG
        };
        let foreground = if disabled { MUTED } else { BUTTON_TEXT };

        // Erase the full rectangle with the window tone so nothing stale stays
        // in the corners that the rounded fill leaves untouched.
        _ = FillRect(
            item.hDC,
            &item.rcItem,
            HBRUSH(window_brush() as *mut core::ffi::c_void),
        );
        let diameter = px(CORNER_RADIUS) * 2;
        let region = CreateRoundRectRgn(
            item.rcItem.left,
            item.rcItem.top,
            item.rcItem.right + 1,
            item.rcItem.bottom + 1,
            diameter,
            diameter,
        );
        _ = FillRgn(
            item.hDC,
            region,
            HBRUSH(button_brush_for(background) as *mut core::ffi::c_void),
        );
        _ = DeleteObject(HGDIOBJ(region.0));
        _ = SetBkMode(item.hDC, TRANSPARENT);
        _ = SetTextColor(item.hDC, colorref(foreground));

        // Use the control's own font (Microsoft YaHei UI) instead of the DC
        // default, otherwise Chinese labels render in a different, smaller face.
        let font = SendMessageW(item.hwndItem, WM_GETFONT, WPARAM(0), LPARAM(0));
        let font = HFONT(font.0 as *mut core::ffi::c_void);
        let previous = if font.0.is_null() {
            None
        } else {
            Some(SelectObject(item.hDC, HGDIOBJ(font.0)))
        };

        let mut label = [0u16; 256];
        let length = GetWindowTextW(item.hwndItem, &mut label);
        if length > 0 {
            let length = (length as usize).min(label.len());
            let mut rect = item.rcItem;
            // DT_CENTER | DT_VCENTER | DT_SINGLELINE
            _ = DrawTextW(
                item.hDC,
                &mut label[..length],
                &mut rect,
                DRAW_TEXT_FORMAT(1 | 4 | 32),
            );
        }
        if let Some(previous) = previous {
            _ = SelectObject(item.hDC, previous);
        }
    }
}

/// Minimal Win32 notification header (the first member of every NM_* struct).
#[repr(C)]
struct NmHdr {
    hwnd_from: *mut core::ffi::c_void,
    id_from: usize,
    code: u32,
}

/// Prefix of NMLVCUSTOMDRAW. Later members are only read for list views,
/// never for headers (whose NMCUSTOMDRAW is shorter).
#[repr(C)]
struct NmCustomDraw {
    hdr: NmHdr,
    draw_stage: u32,
    hdc: *mut core::ffi::c_void,
    rc: RECT,
    item_spec: usize,
    item_state: u32,
    item_lparam: isize,
    clr_text: u32,
    clr_text_bk: u32,
    sub_item: i32,
}

fn class_name(hwnd: HWND) -> String {
    let mut buffer = [0u16; 64];
    // SAFETY: hwnd is a live control handle; the buffer is large enough.
    unsafe { GetClassNameW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer)
        .trim_end_matches('\0')
        .to_owned()
}

/// Custom-draws list views and their headers so the whole control is dark,
/// including the empty area below the last row (which LVM_SETBKCOLOR cannot
/// reach while visual styles are active).
fn custom_draw(lparam: isize) -> Option<isize> {
    if lparam == 0 {
        return None;
    }
    // SAFETY: WM_NOTIFY carries at least an NMHDR; custom draw carries the
    // larger NMCUSTOMDRAW / NMLVCUSTOMDRAW structure.
    unsafe {
        let draw = &*(lparam as *const NmCustomDraw);
        if draw.hdr.code != NM_CUSTOMDRAW {
            return None;
        }
        let hdc = HDC(draw.hdc);
        let is_header = class_name(HWND(draw.hdr.hwnd_from)) == "SysHeader32";
        match draw.draw_stage {
            1 => {
                // CDDS_PREPAINT: fill the whole client area with the card colour.
                _ = FillRect(
                    hdc,
                    &draw.rc,
                    HBRUSH(card_brush() as *mut core::ffi::c_void),
                );
                _ = SetBkMode(hdc, TRANSPARENT);
                _ = SetTextColor(hdc, colorref(TEXT));
                Some(0x20) // CDRF_NOTIFYITEMDRAW
            }
            0x0001_0001 => {
                // CDDS_ITEMPREPAINT
                if is_header {
                    _ = SetBkMode(hdc, TRANSPARENT);
                    _ = SetTextColor(hdc, colorref(TEXT));
                    Some(0) // CDRF_DODEFAULT
                } else {
                    let draw = &mut *(lparam as *mut NmCustomDraw);
                    draw.clr_text = colorref(TEXT).0;
                    draw.clr_text_bk = colorref(CARD_BG).0;
                    Some(2) // CDRF_NEWFONT
                }
            }
            _ => None,
        }
    }
}

/// Draws an owner-draw combo box item: dark surface, light text, no border.
fn draw_combo(lparam: isize) {
    if lparam == 0 {
        return;
    }
    // SAFETY: WM_DRAWITEM supplies a valid DRAWITEMSTRUCT with a live DC.
    unsafe {
        let item = &*(lparam as *const DRAWITEMSTRUCT);
        if item.CtlType.0 != ODT_COMBOBOX {
            return;
        }
        let selected = item.itemState.0 & 0x0001 != 0; // ODS_SELECTED
        let surface = if selected {
            button_brush_for(BUTTON_BG)
        } else {
            card_brush()
        };
        _ = FillRect(
            item.hDC,
            &item.rcItem,
            HBRUSH(surface as *mut core::ffi::c_void),
        );
        _ = SetBkMode(item.hDC, TRANSPARENT);
        _ = SetTextColor(item.hDC, colorref(TEXT));

        let font = SendMessageW(item.hwndItem, WM_GETFONT, WPARAM(0), LPARAM(0));
        let font = HFONT(font.0 as *mut core::ffi::c_void);
        let previous = if font.0.is_null() {
            None
        } else {
            Some(SelectObject(item.hDC, HGDIOBJ(font.0)))
        };

        let mut label = [0u16; 256];
        let length = SendMessageW(
            item.hwndItem,
            CB_GETLBTEXT,
            WPARAM(item.itemID as usize),
            LPARAM(label.as_mut_ptr() as isize),
        );
        if length.0 > 0 {
            let length = (length.0 as usize).min(label.len());
            let mut rect = item.rcItem;
            rect.left += 8;
            // DT_LEFT | DT_VCENTER | DT_SINGLELINE
            _ = DrawTextW(
                item.hDC,
                &mut label[..length],
                &mut rect,
                DRAW_TEXT_FORMAT(4 | 32),
            );
        }
        if let Some(previous) = previous {
            _ = SelectObject(item.hDC, previous);
        }
    }
}

// ---------------------------------------------------------------------------
// Controls Windows insists on painting itself.
//
// Check boxes and combo boxes draw a white system glyph/frame that no colour
// message can reach and no visual-style override can recolour. Both are
// subclassed here: after the control paints normally, the offending pixels are
// painted over with flat dark shapes from this palette.
// ---------------------------------------------------------------------------

/// Original window procedure of every subclassed control, keyed by HWND.
static SUBCLASSED: OnceLock<Mutex<HashMap<isize, isize>>> = OnceLock::new();
static CHECK_OFF_BRUSH: OnceLock<isize> = OnceLock::new();
static CHECK_ON_BRUSH: OnceLock<isize> = OnceLock::new();

fn subclassed() -> &'static Mutex<HashMap<isize, isize>> {
    SUBCLASSED.get_or_init(|| Mutex::new(HashMap::new()))
}

fn original_proc(hwnd: HWND) -> WNDPROC {
    let key = hwnd.0 as isize;
    let raw = subclassed()
        .lock()
        .ok()
        .and_then(|map| map.get(&key).copied())
        .unwrap_or(0);
    // SAFETY: the stored value is exactly what SetWindowLongPtrW returned for
    // this HWND, i.e. the control's previous window procedure pointer.
    unsafe { std::mem::transmute::<isize, WNDPROC>(raw) }
}

/// Installs `proc` as the control's window procedure, remembering the original.
fn subclass(hwnd: HWND, proc: WNDPROC) {
    let key = hwnd.0 as isize;
    if subclassed()
        .lock()
        .map(|map| map.contains_key(&key))
        .unwrap_or(true)
    {
        return;
    }
    let Some(entry) = proc else {
        return;
    };
    // SAFETY: hwnd is a live child control created by this process; the value
    // returned by SetWindowLongPtrW is only ever passed back to CallWindowProcW.
    let previous = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, entry as usize as isize) };
    if let Ok(mut map) = subclassed().lock() {
        map.insert(key, previous);
    }
}

/// Removes the light 3D window edge the system draws around a control.
///
/// The edge is not affected by colour messages and is not clipped away by the
/// window region, so a rounded corner would always keep a light L-shaped seam.
fn strip_borders(hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, GWL_STYLE, SWP_FRAMECHANGED, SWP_NOMOVE,
        SWP_NOSIZE, SWP_NOZORDER, WS_BORDER, WS_EX_CLIENTEDGE, WS_EX_WINDOWEDGE,
    };
    // SAFETY: hwnd is a live control created by this process; both calls only
    // touch its own style bits and force a non-client recalculation.
    unsafe {
        let extended = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let trimmed = extended & !(WS_EX_CLIENTEDGE.0 | WS_EX_WINDOWEDGE.0);
        if trimmed != extended {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, trimmed as isize);
        }
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let borderless = style & !WS_BORDER.0;
        if borderless != style {
            SetWindowLongPtrW(hwnd, GWL_STYLE, borderless as isize);
        }
        if SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
        )
        .is_err()
        {
            eprintln!("移除控件边框失败");
        }
    }
}

unsafe extern "system" fn combo_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // A combo box draws its light 3D frame in the non-client area, which is
    // (re)painted by its own WM_NCPAINT - after our WM_PAINT overpaint. Taking
    // that message over with the same flat fill removes the frame for good.
    if msg == WM_NCPAINT {
        paint_combo(hwnd);
        return LRESULT(0);
    }
    // SAFETY: forwarding the exact message received to the window's previous
    // procedure is what a subclass does; the handle is the same window.
    let result = unsafe { CallWindowProcW(original_proc(hwnd), hwnd, msg, w, l) };
    if msg == WM_PAINT {
        paint_combo(hwnd);
    }
    result
}

/// Repaints the closed combo box: flat dark field with rounded corners, the
/// current selection and a muted chevron instead of the system arrow button.
fn paint_combo(hwnd: HWND) {
    // SAFETY: hwnd is the live control inside its own WM_PAINT handler; the
    // window DC is released before returning and all GDI objects are local.
    unsafe {
        let mut frame = RECT::default();
        if GetWindowRect(hwnd, &mut frame).is_err() {
            return;
        }
        let width = frame.right - frame.left;
        let height = frame.bottom - frame.top;
        if width <= 0 || height <= 0 {
            return;
        }
        let hdc = GetWindowDC(hwnd);
        if hdc.0.is_null() {
            return;
        }
        // Combo boxes erase themselves with the white COLOR_WINDOW class brush;
        // the corners outside the rounded fill would keep that white seam
        // unless the whole rectangle is primed with the window tone first.
        _ = FillRect(
            hdc,
            &RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            },
            HBRUSH(window_brush() as *mut core::ffi::c_void),
        );
        let diameter = px(FIELD_RADIUS) * 2;
        let region = CreateRoundRectRgn(0, 0, width + 1, height + 1, diameter, diameter);
        _ = FillRgn(
            hdc,
            region,
            handle_brush(brush(&SURFACE_BRUSH, CARD_SURFACE)),
        );
        _ = DeleteObject(HGDIOBJ(region.0));

        let index = SendMessageW(hwnd, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
        let mut label = [0u16; 256];
        if index >= 0 {
            let length = SendMessageW(
                hwnd,
                CB_GETLBTEXT,
                WPARAM(index as usize),
                LPARAM(label.as_mut_ptr() as isize),
            )
            .0;
            if length > 0 {
                let length = (length as usize).min(label.len());
                let font = SendMessageW(hwnd, WM_GETFONT, WPARAM(0), LPARAM(0));
                let font = HFONT(font.0 as *mut core::ffi::c_void);
                let previous = if font.0.is_null() {
                    None
                } else {
                    Some(SelectObject(hdc, HGDIOBJ(font.0)))
                };
                _ = SetBkMode(hdc, TRANSPARENT);
                _ = SetTextColor(hdc, colorref(TEXT));
                let mut text_rect = RECT {
                    left: px(12.0),
                    top: 0,
                    right: width - px(26.0),
                    bottom: height,
                };
                // DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS
                _ = DrawTextW(
                    hdc,
                    &mut label[..length],
                    &mut text_rect,
                    DRAW_TEXT_FORMAT(4 | 32 | 0x8000),
                );
                if let Some(previous) = previous {
                    _ = SelectObject(hdc, previous);
                }
            }
        }

        // Chevron: two strokes instead of the system's white arrow button.
        let pen = CreatePen(PS_SOLID, px(1.6).max(2), colorref(MUTED));
        let previous = SelectObject(hdc, HGDIOBJ(pen.0));
        let center_x = width - px(17.0);
        let center_y = height / 2;
        let half = px(4.0).max(3);
        _ = MoveToEx(hdc, center_x - half, center_y - half / 2, None);
        _ = LineTo(hdc, center_x, center_y + half / 2);
        _ = LineTo(hdc, center_x + half, center_y - half / 2);
        _ = SelectObject(hdc, previous);
        _ = DeleteObject(HGDIOBJ(pen.0));
        ReleaseDC(hwnd, hdc);
    }
}

/// Applies the theme to a top-level window.
///
/// One hook covers all `WM_CTLCOLOR*` messages (sent to the parent) plus
/// `WM_DRAWITEM` for the borderless buttons.
pub fn apply(window: &nwg::Window) {
    let hook = nwg::bind_raw_event_handler(&window.handle, 0x20000, |hwnd, msg, w, l| {
        let hdc = HDC(w as *mut core::ffi::c_void);
        match msg {
            WM_ERASEBKGND => {
                // Top-level windows never receive WM_CTLCOLORDLG: their client
                // area is painted with the WNDCLASS brush. Fill it ourselves so
                // the dark theme covers the gaps between controls.
                let mut rect = RECT::default();
                let hwnd = HWND(hwnd as *mut core::ffi::c_void);
                // SAFETY: hwnd/hdc belong to the window being repainted.
                unsafe {
                    if GetClientRect(hwnd, &mut rect).is_ok() {
                        let _ =
                            FillRect(hdc, &rect, HBRUSH(window_brush() as *mut core::ffi::c_void));
                    }
                }
                Some(1)
            }
            WM_NOTIFY => custom_draw(l),
            WM_CTLCOLORSTATIC | WM_CTLCOLORDLG | WM_CTLCOLORBTN => {
                // SAFETY: wParam is the control's HDC for every CTLCOLOR message.
                Some(paint_control(hdc, WINDOW_BG, TEXT, window_brush()))
            }
            WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
                // SAFETY: same as above; editors/lists use the lighter card tone.
                Some(paint_control(hdc, CARD_BG, TEXT, card_brush()))
            }
            WM_DRAWITEM => {
                draw_button(l);
                draw_combo(l);
                Some(1)
            }
            _ => None,
        }
    });
    // NWG keeps the subclass and its closure registered on the HWND; the
    // returned handle is only a thin, non-Drop marker, so dropping it here is
    // enough and does not unbind the hook.
    drop(hook);
    apply_dwm(window);
}

/// Windows 11: dark immersive caption bar, matching border, rounded corners.
/// Older Windows versions silently ignore these attributes.
fn apply_dwm(window: &nwg::Window) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
    };

    let Some(raw) = window.handle.hwnd() else {
        return;
    };
    let hwnd = HWND(raw as *mut core::ffi::c_void);
    let caption = colorref(WINDOW_BG).0;
    let border = colorref(BORDER).0;
    let dark: u32 = 1;
    let rounded: u32 = 2; // DWMWCP_ROUND

    // SAFETY: hwnd belongs to a live window; each attribute is a DWORD-sized
    // value that DWM copies during the call. Failures are cosmetic only.
    unsafe {
        for (attribute, value) in [
            (DWMWA_USE_IMMERSIVE_DARK_MODE, dark),
            (DWMWA_CAPTION_COLOR, caption),
            (DWMWA_BORDER_COLOR, border),
        ] {
            if DwmSetWindowAttribute(
                hwnd,
                attribute,
                &value as *const u32 as *const core::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            )
            .is_err()
            {
                // Older Windows versions reject some attributes; cosmetic only.
            }
        }
        if DwmSetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWA_WINDOW_CORNER_PREFERENCE,
            &rounded as *const u32 as *const core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
        .is_err()
        {
            // Older Windows versions keep the default corner style.
        }
    }
}

/// Makes a label paint its whole rectangle.
///
/// NWG centres label text by shrinking the client area in `WM_NCCALCSIZE` and
/// filling the leftover band with `COLOR_WINDOW`, which on a dark window shows
/// as white hairlines (and leaves ghosts when the text changes). Answering both
/// messages here keeps the client area equal to the control and leaves the
/// painting to the theme's `WM_CTLCOLORSTATIC` brush.
pub fn style_label(label: &nwg::Label) {
    /// WM_NCCALCSIZE.
    const WM_NCCALCSIZE: u32 = 0x0083;
    // The handler only suppresses NWG's centring hack; it is leaked on purpose
    // so it stays bound for the lifetime of the window.
    let hook = nwg::bind_raw_event_handler(&label.handle, 0x20001, |_raw, msg, _w, _l| match msg {
        WM_NCCALCSIZE | WM_NCPAINT => Some(0),
        _ => None,
    });
    std::mem::forget(hook);
}

/// Re-runs the non-client calculation and repaints a label.
///
/// Changing the text changes the client height NWG computes, so the glyphs move.
/// Without this the previous position stays on screen as a faint remnant.
pub fn refresh_label(label: &nwg::Label) {
    let Some(raw) = label.handle.hwnd() else {
        return;
    };
    let hwnd = HWND(raw as *mut core::ffi::c_void);
    // SAFETY: hwnd is a live label of this process; the call only recalculates
    // its frame and requests a repaint.
    unsafe {
        _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
        );
        _ = InvalidateRect(hwnd, None, BOOL(1));
    }
}

/// Forces a control to repaint immediately.
///
/// Owner-draw buttons only repaint when their text changes, which is not the
/// case when only the selected state of an icon tile flips.
pub fn repaint(control: &nwg::Button) {
    let Some(raw) = control.handle.hwnd() else {
        return;
    };
    // SAFETY: the handle belongs to a live control created by this process; the
    // rectangle is optional and the flag only requests a background erase.
    unsafe {
        _ = InvalidateRect(HWND(raw as *mut core::ffi::c_void), None, BOOL(1));
    }
}

/// Applies the card surface to a ListView control.
pub fn style_list(list: &nwg::ListView) {
    list.set_background_color(CARD_BG[0], CARD_BG[1], CARD_BG[2]);
    list.set_text_color(TEXT[0], TEXT[1], TEXT[2]);
}

/// Turns push buttons into borderless owner-draw blocks and gives native
/// controls rounded corners.
///
/// Called from each window's `apply_layout` so the regions are rebuilt whenever
/// the window is resized (and DPI-scaled radii stay correct).
pub fn polish(window: &nwg::Window) {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::EnumChildWindows;

    let Some(raw) = window.handle.hwnd() else {
        return;
    };
    let hwnd = HWND(raw as *mut core::ffi::c_void);

    /// One child-window visitor: make push buttons owner-drawn, then apply a
    /// rounded region to buttons, editors, combo boxes and list views.
    unsafe extern "system" fn visit(child: HWND, data: LPARAM) -> BOOL {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Gdi::InvalidateRect;
        use windows::Win32::Graphics::Gdi::{CreateRoundRectRgn, SetWindowRgn};
        use windows::Win32::UI::Controls::SetWindowTheme;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetClassNameW, GetWindowLongW, GetWindowRect, SendMessageW, SetWindowLongW, GWL_STYLE,
        };

        // SAFETY: child is a live HWND supplied by EnumChildWindows; all GDI
        // regions created here are handed to (and owned by) that window.
        unsafe {
            let radius = data.0 as i32;
            let mut class = [0u16; 64];
            GetClassNameW(child, &mut class);
            let name = String::from_utf16_lossy(&class);
            let name = name.trim_end_matches('\0');
            let mut rect = RECT::default();
            if GetWindowRect(child, &mut rect).is_err() {
                return BOOL(1);
            }
            let width = rect.right - rect.left;
            let height = rect.bottom - rect.top;
            if width <= 0 || height <= 0 {
                return BOOL(1);
            }
            let style = GetWindowLongW(child, GWL_STYLE) as u32;
            if name == "ComboBox" {
                // CBS_DROPDOWNLIST paints its own white display area and
                // ignores every WM_CTLCOLOR*. Owner-draw fixes that without
                // changing CB_GETCURSEL / CB_SETCURSEL behaviour.
                if style & 0x0003 == 0x0003 {
                    let updated = (style & !0x0033) | CBS_OWNERDRAWFIXED | CBS_HASSTRINGS;
                    SetWindowLongW(child, GWL_STYLE, updated as i32);
                }
                // The system edge is a light 3D frame that survives every
                // colour message and is not clipped by the window region;
                // dropping the edge styles hands the whole rectangle to us.
                strip_borders(child);
                // Its white frame and arrow button are painted by the control
                // itself, so combo_proc overpaints them after every repaint.
                subclass(child, Some(combo_proc));
            }
            if name == "Edit" {
                // Multiline editors draw system scrollbars; Windows 11 honours
                // this theme class and paints them dark.
                let class: Vec<u16> = "DarkMode_Explorer"
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                if SetWindowTheme(child, PCWSTR(class.as_ptr()), PCWSTR::null()).is_err() {
                    // Older systems keep the light scrollbars.
                }
            }
            if matches!(name, "ComboBox" | "SysListView32" | "SysHeader32") {
                // Visual styles ignore LVM_SET*COLOR and paint white rows.
                // Turning them off makes the dark colour messages effective;
                // custom_draw() then covers the empty area and the header.
                let empty = [0u16; 1];
                let empty = PCWSTR(empty.as_ptr());
                if SetWindowTheme(child, empty, empty).is_err() {
                    // Older systems keep the light controls; still usable.
                }
                if name == "SysListView32" {
                    let background = colorref(CARD_BG).0 as isize;
                    let foreground = colorref(TEXT).0 as isize;
                    for (message, value) in [
                        (LVM_SETBKCOLOR, background),
                        (LVM_SETTEXTBKCOLOR, background),
                        (LVM_SETTEXTCOLOR, foreground),
                    ] {
                        _ = SendMessageW(child, message, WPARAM(0), LPARAM(value));
                    }
                }
            }
            let rounded = match name {
                // A combo box is painted entirely by us, corner pixels included:
                // a window region would clip those pixels away but the light
                // frame the system painted before the region existed can never be
                // repainted by the parent (its DC is clipped by the child rect),
                // leaving an L-shaped seam at every corner.
                "SysListView32" | "Edit" => true,
                // Only plain/def push buttons (low nibble 0 or 1) become
                // owner-drawn and rounded; radio buttons and group boxes keep
                // their native rendering.
                "Button" => matches!(style & 0x0F, 0 | 1),
                _ => false,
            };
            if name == "Button" {
                match style & 0x0F {
                    0 | 1 => {
                        let updated = (style & !(0x0F | BS_FLAT)) | BS_OWNERDRAW;
                        SetWindowLongW(child, GWL_STYLE, updated as i32);
                        if !InvalidateRect(child, None, BOOL(1)).as_bool() {
                            eprintln!("按钮重绘请求失败");
                        }
                    }
                    _ => {}
                }
            }
            if rounded {
                // SetWindowRgn takes ownership of the region, so it must not be
                // deleted here. Rebuilt on every resize by the caller.
                let region =
                    CreateRoundRectRgn(0, 0, width + 1, height + 1, radius * 2, radius * 2);
                if SetWindowRgn(child, region, BOOL(1)) == 0 {
                    eprintln!("设置圆角区域失败：{name}");
                }
            }
        }
        BOOL(1)
    }

    let radius = (CORNER_RADIUS * crate::platform::gui::ui_scale()).round() as isize;
    // SAFETY: hwnd belongs to a live window; the visitor only touches its
    // children and GDI regions owned by those children.
    unsafe {
        if !EnumChildWindows(hwnd, Some(visit), LPARAM(radius)).as_bool() {
            eprintln!("控件圆角应用失败");
        }
        // Reshaping a child clips its *future* painting only: the light system
        // frame it drew before the region existed would stay on screen at the
        // clipped corners forever. Forcing a full repaint of the window lets
        // the (dark) background paint over those leftover pixels.
        _ = windows::Win32::Graphics::Gdi::InvalidateRect(hwnd, None, BOOL(1));
    }
}

#[cfg(test)]
mod card_tests {
    use super::*;

    #[test]
    fn card_fonts_are_created() {
        assert!(!card_font(&NAME_FONT, 14.0, 400).0.is_null());
        assert!(!card_font(&PERCENT_FONT, 27.0, 700).0.is_null());
        assert!(!card_font(&STATUS_FONT, 11.0, 400).0.is_null());
    }

    #[test]
    fn device_glyph_is_not_empty() {
        let image = crate::icon::device_glyph_rgba("mouse", 48, TEXT);
        assert_eq!(image.width(), 48);
        assert!(image.as_raw().chunks_exact(4).any(|p| p[3] > 0));
    }
}
