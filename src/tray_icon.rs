use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::UI::Shell::{
    ExtractIconExW, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_WARNING,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::native_interop::{Color, WM_APP_TRAY};

const TRAY_ICON_ID: u32 = 1;

/// Menu item ID for toggling widget visibility (used by window.rs context menu).
pub const IDM_TOGGLE_WIDGET: u16 = 70;

/// Actions the tray message handler can request from the main window.
pub enum TrayAction {
    None,
    ToggleWidget,
    ShowContextMenu,
}

/// Create the tray icon. Uses the embedded app icon; falls back to a plain
/// brand-coloured badge (no text/digits) if extraction fails.
fn create_icon() -> HICON {
    let app_icon = load_embedded_app_icon();
    if !app_icon.is_invalid() {
        return app_icon;
    }
    create_fallback_icon()
}

fn create_fallback_icon() -> HICON {
    let size = 64_i32;
    let radius = 2_i32;
    let fill = Color::from_hex("#D97757");

    unsafe {
        let screen_dc = GetDC(HWND::default());
        let mem_dc = CreateCompatibleDC(screen_dc);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let dib =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();

        if dib.is_invalid() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(HWND::default(), screen_dc);
            return HICON::default();
        }

        let old_bmp = SelectObject(mem_dc, dib);

        // Zero-fill (transparent background)
        let pixel_data = std::slice::from_raw_parts_mut(bits as *mut u32, (size * size) as usize);
        for px in pixel_data.iter_mut() {
            *px = 0;
        }

        // Draw solid rounded-rectangle badge, no outline, no text
        let null_pen = GetStockObject(NULL_PEN);
        let old_pen = SelectObject(mem_dc, null_pen);

        let br_fill = CreateSolidBrush(COLORREF(fill.to_colorref()));
        let old_brush = SelectObject(mem_dc, br_fill);
        let _ = RoundRect(mem_dc, 0, 0, size + 1, size + 1, radius * 2, radius * 2);

        SelectObject(mem_dc, old_brush);
        SelectObject(mem_dc, old_pen);
        let _ = DeleteObject(br_fill);

        // Set alpha: non-zero BGR pixel -> fully opaque; background stays transparent
        for px in pixel_data.iter_mut() {
            if *px != 0 {
                *px = (*px & 0x00FF_FFFF) | 0xFF00_0000;
            }
        }

        // Monochrome mask (per-pixel alpha from colour bitmap)
        let mask_bytes = vec![0u8; ((size * size + 7) / 8) as usize];
        let mask_bmp = CreateBitmap(
            size,
            size,
            1,
            1,
            Some(mask_bytes.as_ptr() as *const std::ffi::c_void),
        );

        let icon_info = ICONINFO {
            fIcon: TRUE,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask_bmp,
            hbmColor: dib,
        };
        let hicon = CreateIconIndirect(&icon_info).unwrap_or_default();

        let _ = DeleteObject(mask_bmp);
        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(HWND::default(), screen_dc);

        hicon
    }
}

fn load_embedded_app_icon() -> HICON {
    unsafe {
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        if len == 0 {
            return HICON::default();
        }

        let mut small_icon = HICON::default();
        let mut large_icon = HICON::default();
        let extracted = ExtractIconExW(
            PCWSTR::from_raw(exe_buf.as_ptr()),
            0,
            Some(&mut large_icon),
            Some(&mut small_icon),
            1,
        );

        if extracted == 0 {
            HICON::default()
        } else if !small_icon.is_invalid() {
            small_icon
        } else {
            large_icon
        }
    }
}

/// Show a Windows balloon notification from the tray icon.
/// Used to alert the user when re-authentication is required.
pub fn notify_balloon(hwnd: HWND, title: &str, message: &str) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_ICON_ID;
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = NIIF_WARNING;
        copy_wide(title, &mut nid.szInfoTitle);
        copy_wide_256(message, &mut nid.szInfo);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// Copy a string into a fixed-size wide buffer (truncates to fit).
fn copy_wide<const N: usize>(s: &str, buf: &mut [u16; N]) {
    let wide: Vec<u16> = s.encode_utf16().collect();
    let len = wide.len().min(N - 1);
    buf[..len].copy_from_slice(&wide[..len]);
    buf[len] = 0;
}

/// Copy a string into a 256-wide buffer.
fn copy_wide_256(s: &str, buf: &mut [u16; 256]) {
    copy_wide(s, buf)
}

/// Register the tray icon with the shell.
pub fn add(hwnd: HWND, tooltip: &str) {
    let hicon = create_icon();
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_ICON_ID;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_APP_TRAY;
        nid.hIcon = hicon;
        copy_to_tip(tooltip, &mut nid.szTip);
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        if !hicon.is_invalid() {
            let _ = DestroyIcon(hicon);
        }
    }
}

/// Update the tray icon tooltip to reflect current usage.
pub fn update(hwnd: HWND, tooltip: &str) {
    let hicon = create_icon();
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_ICON_ID;
        nid.uFlags = NIF_ICON | NIF_TIP;
        nid.hIcon = hicon;
        copy_to_tip(tooltip, &mut nid.szTip);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        if !hicon.is_invalid() {
            let _ = DestroyIcon(hicon);
        }
    }
}

/// Remove the tray icon from the shell.
pub fn remove(hwnd: HWND) {
    unsafe {
        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_ICON_ID;
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// Show/update the single tray icon with the given tooltip, or remove it
/// when no provider is enabled (`tooltip` is `None`).
pub fn sync(hwnd: HWND, tooltip: Option<&str>) {
    match tooltip {
        Some(tip) => {
            add(hwnd, tip);
            update(hwnd, tip);
        }
        None => remove(hwnd),
    }
}

/// Interpret a tray callback message and return the action to take.
pub fn handle_message(lparam: LPARAM) -> TrayAction {
    let mouse_msg = lparam.0 as u32;
    match mouse_msg {
        WM_LBUTTONUP => TrayAction::ToggleWidget,
        WM_RBUTTONUP => TrayAction::ShowContextMenu,
        _ => TrayAction::None,
    }
}

/// Copy a string into the fixed-size szTip field (max 127 chars + null).
fn copy_to_tip(s: &str, tip: &mut [u16; 128]) {
    let wide: Vec<u16> = s.encode_utf16().collect();
    let mut len = wide.len().min(127);
    // Don't leave a lone high surrogate at the truncation point
    if len > 0 && (0xD800..=0xDBFF).contains(&wide[len - 1]) {
        len -= 1;
    }
    tip[..len].copy_from_slice(&wide[..len]);
    tip[len] = 0;
}
