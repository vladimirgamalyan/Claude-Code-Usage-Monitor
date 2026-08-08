use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Shell::ExtractIconExW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::diagnose;
use crate::models::{AppUsageData, CodexResetCredits};
use crate::native_interop::{
    self, Color, TIMER_COUNTDOWN, TIMER_POLL, TIMER_RESET_CREDITS, TIMER_RESET_POLL,
    WM_APP_RESET_CREDITS_UPDATED, WM_APP_USAGE_UPDATED,
};
use crate::poller;
use crate::strings;
use crate::theme;

/// Wrapper to make HWND sendable across threads (safe for PostMessage usage)
#[derive(Clone, Copy)]
struct SendHwnd(isize);

unsafe impl Send for SendHwnd {}

impl SendHwnd {
    fn from_hwnd(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }
    fn to_hwnd(self) -> HWND {
        HWND(self.0 as *mut _)
    }
}

/// Shared application state
struct AppState {
    hwnd: SendHwnd,
    is_dark: bool,

    session_percent: f64,
    session_text: String,
    weekly_percent: f64,
    weekly_text: String,
    codex_session_percent: f64,
    codex_session_text: String,
    codex_weekly_percent: f64,
    codex_weekly_text: String,
    antigravity_session_percent: f64,
    antigravity_session_text: String,
    antigravity_weekly_percent: f64,
    antigravity_weekly_text: String,
    /// One rendered line per Codex reset in hand, soonest to lapse first. Empty
    /// when there is none, and the rows are left out of the window entirely.
    codex_reset_lines: Vec<String>,
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,

    data: Option<AppUsageData>,
    codex_reset_credits: Option<CodexResetCredits>,
    /// Credit count from the last usage poll, kept to spot when it moves.
    last_codex_reset_credit_count: Option<usize>,

    poll_interval_ms: u32,
    reset_credit_interval_ms: u32,
    retry_count: u32,
    force_notify_auth_error: bool,
    auth_error_paused_polling: bool,
    auth_watch_mode: poller::CredentialWatchMode,
    auth_watch_snapshot: poller::CredentialWatchSnapshot,
    last_poll_ok: bool,

    /// Last known top-left screen position of the window, persisted so the app
    /// reopens where the user left it.
    window_x: Option<i32>,
    window_y: Option<i32>,
}

const RETRY_BASE_MS: u32 = 30_000; // 30 seconds

const POLL_1_MIN: u32 = 60_000;
const POLL_5_MIN: u32 = 300_000;
const POLL_15_MIN: u32 = 900_000;
const POLL_1_HOUR: u32 = 3_600_000;

const RESET_CREDITS_1_HOUR: u32 = 3_600_000;
const RESET_CREDITS_6_HOURS: u32 = 21_600_000;
const RESET_CREDITS_12_HOURS: u32 = 43_200_000;
const RESET_CREDITS_1_DAY: u32 = 86_400_000;

// Menu item IDs for update frequency
const IDM_FREQ_1MIN: u16 = 10;
const IDM_FREQ_5MIN: u16 = 11;
const IDM_FREQ_15MIN: u16 = 12;
const IDM_FREQ_1HOUR: u16 = 13;
const IDM_RESET_FREQ_1HOUR: u16 = 20;
const IDM_RESET_FREQ_6HOURS: u16 = 21;
const IDM_RESET_FREQ_12HOURS: u16 = 22;
const IDM_RESET_FREQ_1DAY: u16 = 23;
const IDM_MODEL_CLAUDE_CODE: u16 = 60;
const IDM_MODEL_CODEX: u16 = 61;
const IDM_MODEL_ANTIGRAVITY: u16 = 62;

const WM_DPICHANGED_MSG: u32 = 0x02E0;

/// Current system DPI (96 = 100% scaling, 144 = 150%, 192 = 200%, etc.)
static CURRENT_DPI: AtomicU32 = AtomicU32::new(96);

/// Scale a base pixel value (designed at 96 DPI) to the current DPI.
fn sc(px: i32) -> i32 {
    let dpi = CURRENT_DPI.load(Ordering::Relaxed);
    (px as f64 * dpi as f64 / 96.0).round() as i32
}

/// Re-query the monitor DPI for our window and update the cached value.
/// Uses GetDpiForWindow which returns the live DPI (unlike GetDpiForSystem
/// which is cached at process startup and never changes).
fn refresh_dpi() {
    let hwnd = {
        let state = lock_state();
        state.as_ref().map(|s| s.hwnd.to_hwnd())
    };
    if let Some(hwnd) = hwnd {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if dpi > 0 {
            CURRENT_DPI.store(dpi, Ordering::Relaxed);
        }
    }
}

fn load_embedded_app_icons() -> (HICON, HICON) {
    unsafe {
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        if len == 0 {
            return (HICON::default(), HICON::default());
        }

        let mut large_icon = HICON::default();
        let mut small_icon = HICON::default();
        let extracted = ExtractIconExW(
            PCWSTR::from_raw(exe_buf.as_ptr()),
            0,
            Some(&mut large_icon),
            Some(&mut small_icon),
            1,
        );

        if extracted == 0 {
            (HICON::default(), HICON::default())
        } else {
            (large_icon, small_icon)
        }
    }
}

unsafe impl Send for AppState {}

static STATE: Mutex<Option<AppState>> = Mutex::new(None);

/// Lock STATE safely, recovering from poisoned mutex
fn lock_state() -> MutexGuard<'static, Option<AppState>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn settings_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata)
        .join("ClaudeCodeUsageMonitor")
        .join("settings.json")
}

#[derive(Debug, Serialize, Deserialize)]
struct SettingsFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    window_x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    window_y: Option<i32>,
    #[serde(default = "default_poll_interval")]
    poll_interval_ms: u32,
    #[serde(default = "default_reset_credit_interval")]
    reset_credit_interval_ms: u32,
    #[serde(default = "default_show_claude_code")]
    show_claude_code: bool,
    #[serde(default = "default_show_codex")]
    show_codex: bool,
    #[serde(default = "default_show_antigravity")]
    show_antigravity: bool,
}

impl Default for SettingsFile {
    fn default() -> Self {
        Self {
            window_x: None,
            window_y: None,
            poll_interval_ms: default_poll_interval(),
            reset_credit_interval_ms: default_reset_credit_interval(),
            show_claude_code: true,
            show_codex: false,
            show_antigravity: false,
        }
    }
}

fn default_poll_interval() -> u32 {
    POLL_15_MIN
}

/// Reset credits are granted and spent by hand, so they are checked rarely.
fn default_reset_credit_interval() -> u32 {
    RESET_CREDITS_6_HOURS
}

fn default_show_claude_code() -> bool {
    true
}

fn default_show_codex() -> bool {
    false
}

fn default_show_antigravity() -> bool {
    false
}

fn load_settings() -> SettingsFile {
    let content = match std::fs::read_to_string(settings_path()) {
        Ok(c) => c,
        Err(_) => return SettingsFile::default(),
    };
    let mut settings: SettingsFile = serde_json::from_str(&content).unwrap_or_default();
    if !settings.show_claude_code && !settings.show_codex && !settings.show_antigravity {
        settings.show_claude_code = true;
    }
    settings
}

fn save_settings(settings: &SettingsFile) {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(path, json);
    }
}

fn save_state_settings() {
    let state = lock_state();
    if let Some(s) = state.as_ref() {
        save_settings(&SettingsFile {
            window_x: s.window_x,
            window_y: s.window_y,
            poll_interval_ms: s.poll_interval_ms,
            reset_credit_interval_ms: s.reset_credit_interval_ms,
            show_claude_code: s.show_claude_code,
            show_codex: s.show_codex,
            show_antigravity: s.show_antigravity,
        });
    }
}

/// Style of the main window: an ordinary caption bar with minimise and close,
/// but no resize grip - the content has a fixed size.
const MAIN_WINDOW_STYLE: WINDOW_STYLE = WINDOW_STYLE(
    WS_OVERLAPPED.0 | WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0,
);

/// Outer window size needed to host a client area of the given size.
fn window_size_for_client(client_width: i32, client_height: i32) -> (i32, i32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: client_width,
        bottom: client_height,
    };
    unsafe {
        let dpi = CURRENT_DPI.load(Ordering::Relaxed);
        // `true` accounts for the single-row menu bar.
        if AdjustWindowRectExForDpi(&mut rect, MAIN_WINDOW_STYLE, true, WINDOW_EX_STYLE(0), dpi)
            .is_err()
        {
            let _ = AdjustWindowRectEx(&mut rect, MAIN_WINDOW_STYLE, true, WINDOW_EX_STYLE(0));
        }
    }
    (rect.right - rect.left, rect.bottom - rect.top)
}

/// Resize the window so the content fits exactly, keeping its position - except
/// where growing would push the content off the bottom or right of the screen,
/// which is easy to hit now that reset credits add rows.
fn resize_window_to_content(hwnd: HWND) {
    let (width, height) = window_size_for_client(client_width(), client_height());
    unsafe {
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            let _ = SetWindowPos(
                hwnd,
                HWND::default(),
                0,
                0,
                width,
                height,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            return;
        }

        let (x, y) = nudge_onto_work_area(hwnd, rect.left, rect.top, width, height);
        let _ = SetWindowPos(
            hwnd,
            HWND::default(),
            x,
            y,
            width,
            height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Pull a window back just far enough to fit on its monitor's work area. Only
/// the far edges are corrected: a window the user has deliberately parked a
/// little off the left or top stays where they put it.
fn nudge_onto_work_area(hwnd: HWND, x: i32, y: i32, width: i32, height: i32) -> (i32, i32) {
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return (x, y);
        }

        let work = info.rcWork;
        let x = if x + width > work.right {
            (work.right - width).max(work.left)
        } else {
            x
        };
        let y = if y + height > work.bottom {
            (work.bottom - height).max(work.top)
        } else {
            y
        };
        (x, y)
    }
}

/// Move the window to the centre of the monitor it currently sits on.
fn center_window(hwnd: HWND) {
    unsafe {
        let mut window_rect = RECT::default();
        if GetWindowRect(hwnd, &mut window_rect).is_err() {
            return;
        }
        let width = window_rect.right - window_rect.left;
        let height = window_rect.bottom - window_rect.top;

        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let work_area = if GetMonitorInfoW(monitor, &mut info).as_bool() {
            info.rcWork
        } else {
            RECT {
                left: 0,
                top: 0,
                right: GetSystemMetrics(SM_CXSCREEN),
                bottom: GetSystemMetrics(SM_CYSCREEN),
            }
        };

        let x = work_area.left + ((work_area.right - work_area.left) - width) / 2;
        let y = work_area.top + ((work_area.bottom - work_area.top) - height) / 2;
        let _ = SetWindowPos(
            hwnd,
            HWND::default(),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Clamp a stored position back onto a visible monitor, so a window saved on a
/// screen that is no longer attached does not open off-screen.
fn position_is_visible(x: i32, y: i32, width: i32, height: i32) -> bool {
    let rect = RECT {
        left: x,
        top: y,
        right: x + width,
        bottom: y + height,
    };
    unsafe {
        let monitor = MonitorFromRect(&rect, MONITOR_DEFAULTTONULL);
        !monitor.is_invalid()
    }
}

/// Match the caption bar to the current Windows light/dark theme.
fn apply_title_bar_theme(hwnd: HWND, is_dark: bool) {
    unsafe {
        let value = BOOL::from(is_dark);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &value as *const BOOL as *const std::ffi::c_void,
            std::mem::size_of::<BOOL>() as u32,
        );
    }
}

/// Record the current window position so it can be restored next launch.
fn remember_window_position(hwnd: HWND) {
    unsafe {
        // Ignore minimised/maximised placements - they report bogus coordinates.
        if IsIconic(hwnd).as_bool() {
            return;
        }
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return;
        }
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.window_x = Some(rect.left);
            s.window_y = Some(rect.top);
        }
    }
}

/// Usage text for a window a provider may not report at all - a missing window
/// reads as "--" rather than a misleading "0%".
fn format_section(section: &crate::models::UsageSection) -> String {
    if poller::section_is_unavailable(section) {
        "--".to_string()
    } else {
        poller::format_line(section)
    }
}

fn refresh_usage_texts(state: &mut AppState) {
    if !state.last_poll_ok {
        return;
    }

    let Some(data) = state.data.as_ref() else {
        return;
    };

    if let Some(claude_code) = data.claude_code.as_ref() {
        state.session_text = poller::format_line(&claude_code.session);
        state.weekly_text = poller::format_line(&claude_code.weekly);
    } else if state.show_claude_code {
        state.session_text = "!".to_string();
        state.weekly_text = "!".to_string();
    }

    if let Some(codex) = data.codex.as_ref() {
        state.codex_session_text = format_section(&codex.session);
        state.codex_weekly_text = format_section(&codex.weekly);
    } else if state.show_codex {
        state.codex_session_text = "!".to_string();
        state.codex_weekly_text = "!".to_string();
    }

    if let Some(antigravity) = data.antigravity.as_ref() {
        state.antigravity_session_text = poller::format_line(&antigravity.session);
        state.antigravity_weekly_text = format_section(&antigravity.weekly);
    } else if state.show_antigravity {
        state.antigravity_session_text = "!".to_string();
        state.antigravity_weekly_text = "!".to_string();
    }
}

/// Re-render the Codex reset credit lines. Kept out of `refresh_usage_texts`
/// because they survive a failed usage poll: the credits come from their own
/// endpoint on their own schedule.
fn refresh_reset_credit_lines(state: &mut AppState) {
    state.codex_reset_lines = match state.codex_reset_credits.as_ref() {
        Some(credits) if state.show_codex => poller::reset_credit_lines(credits),
        _ => Vec::new(),
    };
}

/// Caption text: name the single provider being shown, if there is only one.
fn window_title_for(
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
) -> &'static str {
    match (show_claude_code, show_codex, show_antigravity) {
        (false, true, false) => strings::CODEX_WINDOW_TITLE,
        (false, false, true) => strings::ANTIGRAVITY_WINDOW_TITLE,
        _ => strings::WINDOW_TITLE,
    }
}

fn set_window_title(state: &AppState) {
    let text = window_title_for(
        state.show_claude_code,
        state.show_codex,
        state.show_antigravity,
    );
    unsafe {
        let title = native_interop::wide_str(text);
        let _ = SetWindowTextW(state.hwnd.to_hwnd(), PCWSTR::from_raw(title.as_ptr()));
    }
}

/// Warn the user that a provider needs to be signed in again.
fn show_warning_message(hwnd: HWND, title: &str, message: &str) {
    unsafe {
        let title_wide = native_interop::wide_str(title);
        let message_wide = native_interop::wide_str(message);
        let _ = MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_OK | MB_ICONWARNING,
        );
    }
}

// Dimensions matching the C# version
const SEGMENT_W: i32 = 10;
const SEGMENT_H: i32 = 13;
const SEGMENT_GAP: i32 = 1;
const SEGMENT_COUNT: i32 = 10;
const CORNER_RADIUS: i32 = 2;

const LABEL_WIDTH: i32 = 18;
const LABEL_RIGHT_MARGIN: i32 = 10;
const BAR_RIGHT_MARGIN: i32 = 4;
/// Wide enough for the longest countdown line, "100% · 23h59m".
const TEXT_WIDTH: i32 = 86;

/// Padding between the window edges and the content.
const CONTENT_PADDING_X: i32 = 16;
const CONTENT_PADDING_Y: i32 = 14;
/// Vertical gap between two rows of the same model.
const ROW_GAP: i32 = 8;
/// Vertical gap between a model's heading and its first row.
const HEADING_GAP: i32 = 5;
/// Vertical gap between one model's block and the next.
const BLOCK_GAP: i32 = 14;
/// How far a model's rows sit in from its heading.
const ROW_INDENT: i32 = 10;
/// Keep the window wide enough for the menu bar. The caption text is left to
/// fend for itself - Windows ellipsises it rather than padding the content out.
const MIN_CLIENT_WIDTH: i32 = 200;

/// A provider's block in the stacked layout.
#[derive(Clone, Copy, PartialEq)]
enum Model {
    ClaudeCode,
    Codex,
    Antigravity,
}

/// The blocks on show, top to bottom. Never empty: the menu refuses to turn the
/// last model off, and a settings file that says otherwise falls back to Claude.
fn visible_models(show_claude_code: bool, show_codex: bool, show_antigravity: bool) -> Vec<Model> {
    let mut models = Vec::new();
    if show_claude_code {
        models.push(Model::ClaudeCode);
    }
    if show_codex {
        models.push(Model::Codex);
    }
    if show_antigravity {
        models.push(Model::Antigravity);
    }
    if models.is_empty() {
        models.push(Model::ClaudeCode);
    }
    models
}

/// The models on show and how many Codex reset rows ride along with them.
fn visible_layout() -> (Vec<Model>, i32) {
    let state = lock_state();
    match state.as_ref() {
        Some(s) => (
            visible_models(s.show_claude_code, s.show_codex, s.show_antigravity),
            if s.show_codex {
                s.codex_reset_lines.len() as i32
            } else {
                0
            },
        ),
        None => (visible_models(true, false, false), 0),
    }
}

/// Headings only earn their vertical space when there is more than one block to
/// tell apart - a lone model already names itself in the caption.
fn headings_shown(models: &[Model]) -> bool {
    models.len() > 1
}

fn row_indent(headings: bool) -> i32 {
    if headings {
        sc(ROW_INDENT)
    } else {
        0
    }
}

/// Rows in a model's block: the two usage windows, plus one per Codex reset in
/// hand.
fn block_row_count(model: Model, reset_rows: i32) -> i32 {
    match model {
        Model::Codex => 2 + reset_rows,
        _ => 2,
    }
}

/// Height of the drawn blocks themselves, without the outer padding.
fn content_height_for(models: &[Model], reset_rows: i32) -> i32 {
    let seg_h = sc(SEGMENT_H);
    let headings = headings_shown(models);
    let mut height = 0;
    for (index, model) in models.iter().enumerate() {
        if index > 0 {
            height += sc(BLOCK_GAP);
        }
        if headings {
            height += seg_h + sc(HEADING_GAP);
        }
        let rows = block_row_count(*model, reset_rows);
        height += seg_h * rows + sc(ROW_GAP) * (rows - 1);
    }
    height
}

/// Client height needed for every visible block plus padding, at the current DPI.
fn client_height() -> i32 {
    let (models, reset_rows) = visible_layout();
    sc(CONTENT_PADDING_Y) * 2 + content_height_for(&models, reset_rows)
}

/// Client width at the current DPI: the content, or the minimum if wider.
fn client_width() -> i32 {
    let (models, _) = visible_layout();
    content_width_for(headings_shown(&models)).max(sc(MIN_CLIENT_WIDTH))
}

/// Width of the drawn content itself, without the outer padding.
fn content_inner_width(headings: bool) -> i32 {
    row_indent(headings) + sc(LABEL_WIDTH) + sc(LABEL_RIGHT_MARGIN) + model_usage_width()
}

fn content_width_for(headings: bool) -> i32 {
    sc(CONTENT_PADDING_X) * 2 + content_inner_width(headings)
}

fn claude_accent_color() -> Color {
    Color::from_hex("#D97757")
}

fn codex_accent_color(is_dark: bool) -> Color {
    if is_dark {
        Color::from_hex("#F5F5F5")
    } else {
        Color::from_hex("#1F1F1F")
    }
}

fn antigravity_accent_color() -> Color {
    Color::from_hex("#4285F4")
}

fn claude_usage_text_color(is_dark: bool) -> Color {
    if is_dark {
        Color::from_hex("#F09A7A")
    } else {
        Color::from_hex("#A94F32")
    }
}

fn codex_usage_text_color(is_dark: bool) -> Color {
    if is_dark {
        Color::from_hex("#F5F5F5")
    } else {
        Color::from_hex("#1F1F1F")
    }
}

fn antigravity_usage_text_color(is_dark: bool) -> Color {
    if is_dark {
        Color::from_hex("#8AB4F8")
    } else {
        Color::from_hex("#1967D2")
    }
}

fn model_accent_color(model: Model, is_dark: bool) -> Color {
    match model {
        Model::ClaudeCode => claude_accent_color(),
        Model::Codex => codex_accent_color(is_dark),
        Model::Antigravity => antigravity_accent_color(),
    }
}

fn model_usage_text_color(model: Model, is_dark: bool) -> Color {
    match model {
        Model::ClaudeCode => claude_usage_text_color(is_dark),
        Model::Codex => codex_usage_text_color(is_dark),
        Model::Antigravity => antigravity_usage_text_color(is_dark),
    }
}

fn model_heading(model: Model) -> &'static str {
    match model {
        Model::ClaudeCode => strings::CLAUDE_CODE_MODEL,
        Model::Codex => strings::CODEX_MODEL,
        Model::Antigravity => strings::ANTIGRAVITY_MODEL,
    }
}

pub fn run() {
    // Enable Per-Monitor DPI Awareness V2 for crisp rendering at any scale factor
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CURRENT_DPI.store(GetDpiForSystem(), Ordering::Relaxed);
    }
    diagnose::log("window::run started");

    let class_name = native_interop::wide_str("ClaudeCodeUsageMonitor");

    // Single-instance guard: bring the running instance to the front and exit.
    let mutex_name = native_interop::wide_str("Global\\ClaudeCodeUsageMonitor");
    let _mutex = unsafe {
        let handle = CreateMutexW(None, true, PCWSTR::from_raw(mutex_name.as_ptr()));
        match handle {
            Ok(h) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    diagnose::log("startup aborted: another instance is already running");
                    activate_running_instance(&class_name);
                    return;
                }
                h
            }
            Err(error) => {
                diagnose::log_error(
                    "startup aborted: unable to create single-instance mutex",
                    error,
                );
                return;
            }
        }
    };

    unsafe {
        let hinstance = GetModuleHandleW(PCWSTR::null()).unwrap();
        let (large_icon, small_icon) = load_embedded_app_icons();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hIcon: large_icon,
            hIconSm: small_icon,
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };

        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            diagnose::log("RegisterClassExW returned 0");
        }

        let settings = load_settings();

        let title = native_interop::wide_str(window_title_for(
            settings.show_claude_code,
            settings.show_codex,
            settings.show_antigravity,
        ));
        // The shared state does not exist yet, so size the window off the
        // settings and leave the reset rows to the first resize.
        let initial_models = visible_models(
            settings.show_claude_code,
            settings.show_codex,
            settings.show_antigravity,
        );
        let (window_width, window_height) = window_size_for_client(
            content_width_for(headings_shown(&initial_models)).max(sc(MIN_CLIENT_WIDTH)),
            sc(CONTENT_PADDING_Y) * 2 + content_height_for(&initial_models, 0),
        );
        let restored_position = match (settings.window_x, settings.window_y) {
            (Some(x), Some(y)) if position_is_visible(x, y, window_width, window_height) => {
                Some((x, y))
            }
            _ => None,
        };
        // Never pass CW_USEDEFAULT: the first ShowWindow of such a window lets
        // the shell's STARTUPINFO override wherever we placed it afterwards.
        let (start_x, start_y) = restored_position.unwrap_or((0, 0));

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            MAIN_WINDOW_STYLE,
            start_x,
            start_y,
            window_width,
            window_height,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        )
        .unwrap();

        if !large_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_BIG as usize),
                LPARAM(large_icon.0 as isize),
            );
        }
        if !small_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_SMALL as usize),
                LPARAM(small_icon.0 as isize),
            );
        }

        diagnose::log(format!("main window created hwnd={:?}", hwnd));

        let is_dark = theme::is_dark_mode();

        {
            let mut state = lock_state();
            *state = Some(AppState {
                hwnd: SendHwnd::from_hwnd(hwnd),
                is_dark,
                session_percent: 0.0,
                session_text: "--".to_string(),
                weekly_percent: 0.0,
                weekly_text: "--".to_string(),
                codex_session_percent: 0.0,
                codex_session_text: "--".to_string(),
                codex_weekly_percent: 0.0,
                codex_weekly_text: "--".to_string(),
                antigravity_session_percent: 0.0,
                antigravity_session_text: "--".to_string(),
                antigravity_weekly_percent: 0.0,
                antigravity_weekly_text: "--".to_string(),
                codex_reset_lines: Vec::new(),
                show_claude_code: settings.show_claude_code,
                show_codex: settings.show_codex,
                show_antigravity: settings.show_antigravity,
                data: None,
                codex_reset_credits: None,
                last_codex_reset_credit_count: None,
                poll_interval_ms: settings.poll_interval_ms,
                reset_credit_interval_ms: settings.reset_credit_interval_ms,
                retry_count: 0,
                force_notify_auth_error: false,
                auth_error_paused_polling: false,
                auth_watch_mode: poller::CredentialWatchMode::ActiveSource,
                auth_watch_snapshot: Vec::new(),
                last_poll_ok: false,
                window_x: settings.window_x,
                window_y: settings.window_y,
            });
        }

        refresh_menu_bar(hwnd);

        // The window may have landed on a monitor with a different scale factor
        // than the one we sized it for, so re-measure before showing it.
        refresh_dpi();
        resize_window_to_content(hwnd);
        apply_title_bar_theme(hwnd, is_dark);
        if restored_position.is_none() {
            center_window(hwnd);
        }
        remember_window_position(hwnd);

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        diagnose::log("window shown");

        repaint();

        // Poll timer: 15 minutes
        let initial_poll_ms = {
            let state = lock_state();
            state
                .as_ref()
                .map(|s| s.poll_interval_ms)
                .unwrap_or(POLL_15_MIN)
        };
        SetTimer(hwnd, TIMER_POLL, initial_poll_ms, None);

        // Initial poll
        let send_hwnd = SendHwnd::from_hwnd(hwnd);
        std::thread::spawn(move || {
            diagnose::log("initial poll thread started");
            do_poll(send_hwnd);
        });

        sync_reset_credit_timer(hwnd, settings.show_codex);

        // Initial theme check
        check_theme_change();

        // Message loop
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Invalidate the window so the content is redrawn from the current state.
fn repaint() {
    refresh_dpi();
    let hwnd = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => s.hwnd.to_hwnd(),
            None => return,
        }
    };
    unsafe {
        let _ = InvalidateRect(hwnd, None, false);
    }
}

/// Bring the window of an already-running instance to the foreground.
fn activate_running_instance(class_name: &[u16]) {
    unsafe {
        let Ok(existing) = FindWindowW(PCWSTR::from_raw(class_name.as_ptr()), PCWSTR::null())
        else {
            return;
        };
        if existing.is_invalid() {
            return;
        }
        if IsIconic(existing).as_bool() {
            let _ = ShowWindow(existing, SW_RESTORE);
        }
        let _ = SetForegroundWindow(existing);
    }
}

/// Paint the stacked model blocks onto a DC with a given background color.
fn paint_content(
    hdc: HDC,
    width: i32,
    height: i32,
    is_dark: bool,
    bg: &Color,
    text_color: &Color,
    track: &Color,
    session_pct: f64,
    session_text: &str,
    weekly_pct: f64,
    weekly_text: &str,
    codex_session_pct: f64,
    codex_session_text: &str,
    codex_weekly_pct: f64,
    codex_weekly_text: &str,
    antigravity_session_pct: f64,
    antigravity_session_text: &str,
    antigravity_weekly_pct: f64,
    antigravity_weekly_text: &str,
    codex_reset_lines: &[String],
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
) {
    unsafe {
        let client_rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };

        let bg_brush = CreateSolidBrush(COLORREF(bg.to_colorref()));
        FillRect(hdc, &client_rect, bg_brush);
        let _ = DeleteObject(bg_brush);

        let models = visible_models(show_claude_code, show_codex, show_antigravity);
        let headings = headings_shown(&models);
        let reset_rows = if show_codex {
            codex_reset_lines.len() as i32
        } else {
            0
        };

        // Centre the content so it stays balanced when the window is wider than
        // the content needs, or a pixel or two taller than we asked for.
        let content_h = content_height_for(&models, reset_rows);
        let content_x = ((width - content_inner_width(headings)) / 2).max(sc(CONTENT_PADDING_X));
        let mut y = ((height - content_h) / 2).max(sc(CONTENT_PADDING_Y));
        let seg_h = sc(SEGMENT_H);
        let row_x = content_x + row_indent(headings);

        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));

        let font_name = native_interop::wide_str("Segoe UI");
        let make_font = |weight: FONT_WEIGHT| {
            CreateFontW(
                sc(-12),
                0,
                0,
                0,
                weight.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_TT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                CLEARTYPE_QUALITY.0 as u32,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                PCWSTR::from_raw(font_name.as_ptr()),
            )
        };
        let font = make_font(FW_MEDIUM);
        let heading_font = make_font(FW_SEMIBOLD);
        let old_font = SelectObject(hdc, font);

        for (index, model) in models.iter().enumerate() {
            let model = *model;
            if index > 0 {
                y += sc(BLOCK_GAP);
            }

            // A lone model keeps the neutral text colour it has always had;
            // stacked ones are told apart by their own.
            let value_color = if headings {
                model_usage_text_color(model, is_dark)
            } else {
                *text_color
            };
            let accent = model_accent_color(model, is_dark);

            if headings {
                SelectObject(hdc, heading_font);
                draw_heading(hdc, content_x, y, model_heading(model), &value_color);
                SelectObject(hdc, font);
                y += seg_h + sc(HEADING_GAP);
            }

            let (session_pct, session_text, weekly_pct, weekly_text) = match model {
                Model::ClaudeCode => (session_pct, session_text, weekly_pct, weekly_text),
                Model::Codex => (
                    codex_session_pct,
                    codex_session_text,
                    codex_weekly_pct,
                    codex_weekly_text,
                ),
                Model::Antigravity => (
                    antigravity_session_pct,
                    antigravity_session_text,
                    antigravity_weekly_pct,
                    antigravity_weekly_text,
                ),
            };

            draw_usage_row(
                hdc,
                row_x,
                y,
                text_color,
                strings::SESSION_WINDOW,
                session_pct,
                session_text,
                &accent,
                track,
                &value_color,
            );
            y += seg_h + sc(ROW_GAP);
            draw_usage_row(
                hdc,
                row_x,
                y,
                text_color,
                strings::WEEKLY_WINDOW,
                weekly_pct,
                weekly_text,
                &accent,
                track,
                &value_color,
            );
            y += seg_h;

            if model == Model::Codex {
                for reset_text in codex_reset_lines {
                    y += sc(ROW_GAP);
                    draw_reset_row(hdc, row_x, y, text_color, reset_text, &value_color);
                    y += seg_h;
                }
            }
        }

        SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
        let _ = DeleteObject(heading_font);
    }
}

fn do_poll(send_hwnd: SendHwnd) {
    let hwnd = send_hwnd.to_hwnd();
    let (show_claude_code, show_codex, show_antigravity) = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| (s.show_claude_code, s.show_codex, s.show_antigravity))
            .unwrap_or((true, false, false))
    };

    match poller::poll(show_claude_code, show_codex, show_antigravity) {
        Ok(data) => {
            let mut refetch_reset_credits = false;
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                refetch_reset_credits = reset_credits_look_stale(s, &data);
                if let Some(count) = data.codex_reset_credits_available {
                    s.last_codex_reset_credit_count = Some(count);
                }
                if let Some(claude_code) = data.claude_code.as_ref() {
                    s.session_percent = claude_code.session.percentage;
                    s.weekly_percent = claude_code.weekly.percentage;
                } else if s.show_claude_code {
                    s.session_percent = 0.0;
                    s.weekly_percent = 0.0;
                }
                if let Some(codex) = data.codex.as_ref() {
                    s.codex_session_percent = codex.session.percentage;
                    s.codex_weekly_percent = codex.weekly.percentage;
                } else if s.show_codex {
                    s.codex_session_percent = 0.0;
                    s.codex_weekly_percent = 0.0;
                }
                if let Some(antigravity) = data.antigravity.as_ref() {
                    s.antigravity_session_percent = antigravity.session.percentage;
                    s.antigravity_weekly_percent = antigravity.weekly.percentage;
                } else if s.show_antigravity {
                    s.antigravity_session_percent = 0.0;
                    s.antigravity_weekly_percent = 0.0;
                }
                // Stop fast-poll if reset data is now fresh
                if !poller::app_is_past_reset(&data) {
                    unsafe {
                        let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                    }
                }

                s.data = Some(data);
                s.last_poll_ok = true;
                refresh_usage_texts(s);

                // Recovered from errors — restore normal poll interval
                if s.retry_count > 0 {
                    s.retry_count = 0;
                    let interval = s.poll_interval_ms;
                    unsafe {
                        SetTimer(hwnd, TIMER_POLL, interval, None);
                    }
                }
                s.force_notify_auth_error = false;
                s.auth_error_paused_polling = false;
                s.auth_watch_mode = poller::CredentialWatchMode::ActiveSource;
                s.auth_watch_snapshot.clear();
            }
            drop(state);

            // The count moved, so the expiry dates behind it are worth re-reading
            // now rather than at the next scheduled check.
            if refetch_reset_credits {
                let sh = SendHwnd::from_hwnd(hwnd);
                std::thread::spawn(move || {
                    do_reset_credit_poll(sh);
                });
            }

            unsafe {
                let _ = PostMessageW(hwnd, WM_APP_USAGE_UPDATED, WPARAM(0), LPARAM(0));
            }
        }
        Err(e) => {
            let auth_watch = match e {
                poller::PollError::AuthRequired | poller::PollError::TokenExpired
                    if show_antigravity && !show_claude_code && !show_codex =>
                {
                    Some((
                        poller::CredentialWatchMode::Antigravity,
                        poller::credential_watch_snapshot(poller::CredentialWatchMode::Antigravity),
                    ))
                }
                poller::PollError::AuthRequired | poller::PollError::TokenExpired => Some((
                    poller::CredentialWatchMode::ActiveSource,
                    poller::credential_watch_snapshot(poller::CredentialWatchMode::ActiveSource),
                )),
                poller::PollError::NoCredentials => Some((
                    poller::CredentialWatchMode::AllSources,
                    poller::credential_watch_snapshot(poller::CredentialWatchMode::AllSources),
                )),
                poller::PollError::RequestFailed => None,
            };
            // Distinguish auth-required errors from transient errors.
            let notify_auth_error = {
                let mut state = lock_state();
                let mut should_notify = false;
                if let Some(s) = state.as_mut() {
                    s.last_poll_ok = false;
                    match auth_watch {
                        Some((watch_mode, watch_snapshot)) => {
                            // Only show the balloon on the first failure so it doesn't spam.
                            if s.retry_count == 0 || s.force_notify_auth_error {
                                should_notify = true;
                            }
                            s.force_notify_auth_error = false;
                            s.auth_error_paused_polling = true;
                            s.auth_watch_mode = watch_mode;
                            s.auth_watch_snapshot = watch_snapshot;
                            s.session_text = "!".to_string();
                            s.weekly_text = "!".to_string();
                            s.codex_session_text = "!".to_string();
                            s.codex_weekly_text = "!".to_string();
                            s.antigravity_session_text = "!".to_string();
                            s.antigravity_weekly_text = "!".to_string();
                            s.retry_count = s.retry_count.saturating_add(1);
                            unsafe {
                                let _ = KillTimer(hwnd, TIMER_POLL);
                                let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                                let _ = KillTimer(hwnd, TIMER_COUNTDOWN);
                                SetTimer(hwnd, TIMER_POLL, s.poll_interval_ms, None);
                            }
                        }
                        _ => {
                            // Transient network / credential-missing errors: exponential backoff.
                            s.force_notify_auth_error = false;
                            s.auth_error_paused_polling = false;
                            s.auth_watch_mode = poller::CredentialWatchMode::ActiveSource;
                            s.auth_watch_snapshot.clear();
                            s.session_text = "...".to_string();
                            s.weekly_text = "...".to_string();
                            s.codex_session_text = "...".to_string();
                            s.codex_weekly_text = "...".to_string();
                            s.antigravity_session_text = "...".to_string();
                            s.antigravity_weekly_text = "...".to_string();
                            s.retry_count = s.retry_count.saturating_add(1);
                            let backoff = RETRY_BASE_MS.saturating_mul(
                                1u32.checked_shl(s.retry_count - 1).unwrap_or(u32::MAX),
                            );
                            let retry_ms = backoff.min(s.poll_interval_ms);
                            unsafe {
                                let _ = KillTimer(hwnd, TIMER_RESET_POLL);
                                SetTimer(hwnd, TIMER_POLL, retry_ms, None);
                            }
                        }
                    }
                }
                should_notify
            };

            if notify_auth_error {
                let alert = {
                    let state = lock_state();
                    state.as_ref().map(|s| {
                        if s.show_claude_code {
                            (strings::TOKEN_EXPIRED_TITLE, strings::TOKEN_EXPIRED_BODY)
                        } else if s.show_codex {
                            (
                                strings::CODEX_TOKEN_EXPIRED_TITLE,
                                strings::CODEX_TOKEN_EXPIRED_BODY,
                            )
                        } else {
                            (
                                strings::ANTIGRAVITY_TOKEN_EXPIRED_TITLE,
                                strings::ANTIGRAVITY_TOKEN_EXPIRED_BODY,
                            )
                        }
                    })
                };
                if let Some((title, body)) = alert {
                    show_warning_message(hwnd, title, body);
                }
            }

            unsafe {
                let _ = PostMessageW(hwnd, WM_APP_USAGE_UPDATED, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// Every usage poll carries the number of reset credits in hand, so a change
/// there is a free signal that the credit list - and the expiry dates only that
/// list carries - needs re-reading. Also fires the first time a count arrives
/// while no list has been fetched yet, which covers a failed startup fetch.
fn reset_credits_look_stale(state: &AppState, data: &AppUsageData) -> bool {
    poller::reset_credits_need_refresh(
        state.last_codex_reset_credit_count,
        data.codex_reset_credits_available,
        state.codex_reset_credits.is_some(),
    )
}

/// Start or stop the reset credit timer to match whether Codex is on show,
/// kicking off an immediate poll whenever it is (re)enabled.
fn sync_reset_credit_timer(hwnd: HWND, show_codex: bool) {
    unsafe {
        let _ = KillTimer(hwnd, TIMER_RESET_CREDITS);
    }

    if !show_codex {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.codex_reset_credits = None;
            s.codex_reset_lines = Vec::new();
            s.last_codex_reset_credit_count = None;
        }
        return;
    }

    let interval = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| s.reset_credit_interval_ms)
            .unwrap_or(RESET_CREDITS_6_HOURS)
    };
    unsafe {
        SetTimer(hwnd, TIMER_RESET_CREDITS, interval, None);
    }

    let sh = SendHwnd::from_hwnd(hwnd);
    std::thread::spawn(move || {
        do_reset_credit_poll(sh);
    });
}

/// Fetch the reset credits Codex has granted. A failure keeps whatever was last
/// known rather than blanking the row - these change far too rarely for a
/// transient error to be worth surfacing.
fn do_reset_credit_poll(send_hwnd: SendHwnd) {
    let hwnd = send_hwnd.to_hwnd();
    let show_codex = {
        let state = lock_state();
        state.as_ref().map(|s| s.show_codex).unwrap_or(false)
    };
    if !show_codex {
        return;
    }

    match poller::poll_codex_reset_credits() {
        Ok(credits) => {
            {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    s.codex_reset_credits = Some(credits);
                    refresh_reset_credit_lines(s);
                }
            }
            unsafe {
                let _ = PostMessageW(hwnd, WM_APP_RESET_CREDITS_UPDATED, WPARAM(0), LPARAM(0));
            }
        }
        Err(error) => diagnose::log(format!("Codex reset credit poll failed: {error:?}")),
    }
}

fn schedule_countdown_timer() {
    let state = lock_state();
    let s = match state.as_ref() {
        Some(s) => s,
        None => return,
    };

    let hwnd = s.hwnd.to_hwnd();
    // The reset credit row keeps counting down even when usage polling is
    // broken - its data comes from elsewhere and is still valid.
    let credit_delay = s
        .codex_reset_credits
        .as_ref()
        .and_then(poller::reset_credits_time_until_display_change);

    if !s.last_poll_ok {
        unsafe {
            let _ = KillTimer(hwnd, TIMER_RESET_POLL);
            match credit_delay {
                Some(delay) => {
                    SetTimer(hwnd, TIMER_COUNTDOWN, countdown_timer_ms(Some(delay)), None);
                }
                None => {
                    let _ = KillTimer(hwnd, TIMER_COUNTDOWN);
                }
            }
        }
        return;
    }

    let data = match &s.data {
        Some(d) => d,
        None => return,
    };

    // If a reset time has passed, poll every 5s to pick up fresh data
    if poller::app_is_past_reset(data) {
        unsafe {
            SetTimer(hwnd, TIMER_RESET_POLL, 5_000, None);
        }
    }

    let delays = [
        data.claude_code
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.session.resets_at)),
        data.claude_code
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.weekly.resets_at)),
        data.codex
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.session.resets_at)),
        data.codex
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.weekly.resets_at)),
        data.antigravity
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.session.resets_at)),
        data.antigravity
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.weekly.resets_at)),
        credit_delay,
    ];
    let min_delay = delays.into_iter().flatten().min();

    unsafe {
        SetTimer(hwnd, TIMER_COUNTDOWN, countdown_timer_ms(min_delay), None);
    }
}

/// A missing delay falls back to a minute; anything shorter than a second is
/// rounded up so the timer cannot spin.
fn countdown_timer_ms(delay: Option<Duration>) -> u32 {
    delay
        .unwrap_or(Duration::from_secs(60))
        .as_millis()
        .max(1000) as u32
}

fn check_theme_change() {
    let new_dark = theme::is_dark_mode();
    let hwnd = {
        let mut state = lock_state();
        match state.as_mut() {
            Some(s) if s.is_dark != new_dark => {
                s.is_dark = new_dark;
                Some(s.hwnd.to_hwnd())
            }
            _ => None,
        }
    };
    if let Some(hwnd) = hwnd {
        apply_title_bar_theme(hwnd, new_dark);
        repaint();
    }
}

fn update_display() {
    let mut state = lock_state();
    let s = match state.as_mut() {
        Some(s) => s,
        None => return,
    };

    refresh_reset_credit_lines(s);

    // Don't overwrite error text with stale cached data
    if !s.last_poll_ok {
        return;
    }

    refresh_usage_texts(s);
}

/// Main window procedure
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint(hdc, hwnd);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MOVE => {
            remember_window_position(hwnd);
            LRESULT(0)
        }
        WM_DPICHANGED_MSG => {
            let new_dpi = (wparam.0 & 0xFFFF) as u32;
            CURRENT_DPI.store(new_dpi, Ordering::Relaxed);
            // lparam carries the position Windows suggests for the new scale;
            // honour it, then re-fit the width to the rescaled content.
            let suggested = &*(lparam.0 as *const RECT);
            let _ = SetWindowPos(
                hwnd,
                HWND::default(),
                suggested.left,
                suggested.top,
                suggested.right - suggested.left,
                suggested.bottom - suggested.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            resize_window_to_content(hwnd);
            repaint();
            LRESULT(0)
        }
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            if msg == WM_SETTINGCHANGE {
                check_theme_change();
            }
            refresh_dpi();
            resize_window_to_content(hwnd);
            repaint();
            LRESULT(0)
        }
        WM_TIMER => {
            let timer_id = wparam.0;
            match timer_id {
                TIMER_POLL => {
                    let auth_watch = {
                        let state = lock_state();
                        state.as_ref().map(|s| {
                            (
                                s.auth_error_paused_polling,
                                s.auth_watch_mode,
                                s.auth_watch_snapshot.clone(),
                            )
                        })
                    };
                    match auth_watch {
                        Some((true, watch_mode, previous_snapshot)) => {
                            let current_snapshot = poller::credential_watch_snapshot(watch_mode);
                            if current_snapshot != previous_snapshot {
                                let mut state = lock_state();
                                if let Some(s) = state.as_mut() {
                                    if s.auth_error_paused_polling
                                        && s.auth_watch_mode == watch_mode
                                    {
                                        s.auth_watch_snapshot = current_snapshot;
                                    }
                                }
                                drop(state);
                                let sh = SendHwnd::from_hwnd(hwnd);
                                std::thread::spawn(move || {
                                    do_poll(sh);
                                });
                            }
                        }
                        Some((false, _, _)) => {
                            let sh = SendHwnd::from_hwnd(hwnd);
                            std::thread::spawn(move || {
                                do_poll(sh);
                            });
                        }
                        None => {}
                    }
                }
                TIMER_COUNTDOWN => {
                    update_display();
                    // An expiring credit takes its row with it.
                    resize_window_to_content(hwnd);
                    repaint();
                    schedule_countdown_timer();
                }
                TIMER_RESET_CREDITS => {
                    let sh = SendHwnd::from_hwnd(hwnd);
                    std::thread::spawn(move || {
                        do_reset_credit_poll(sh);
                    });
                }
                TIMER_RESET_POLL => {
                    let should_poll = {
                        let state = lock_state();
                        state
                            .as_ref()
                            .map(|s| !s.auth_error_paused_polling)
                            .unwrap_or(false)
                    };
                    if should_poll {
                        let sh = SendHwnd::from_hwnd(hwnd);
                        std::thread::spawn(move || {
                            do_poll(sh);
                        });
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_USAGE_UPDATED => {
            check_theme_change();
            repaint();
            schedule_countdown_timer();
            LRESULT(0)
        }
        WM_APP_RESET_CREDITS_UPDATED => {
            // The row appears and disappears with the credits, so the window has
            // to be re-fitted around it.
            resize_window_to_content(hwnd);
            repaint();
            schedule_countdown_timer();
            LRESULT(0)
        }
        WM_RBUTTONUP | WM_CONTEXTMENU => {
            show_context_menu(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = wparam.0 as u16;
            match id {
                1 => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.session_text = "...".to_string();
                            s.weekly_text = "...".to_string();
                            s.codex_session_text = "...".to_string();
                            s.codex_weekly_text = "...".to_string();
                            s.force_notify_auth_error = true;
                        }
                    }
                    repaint();
                    let sh = SendHwnd::from_hwnd(hwnd);
                    std::thread::spawn(move || {
                        do_poll(sh);
                    });
                    let show_codex = {
                        let state = lock_state();
                        state.as_ref().map(|s| s.show_codex).unwrap_or(false)
                    };
                    if show_codex {
                        let sh = SendHwnd::from_hwnd(hwnd);
                        std::thread::spawn(move || {
                            do_reset_credit_poll(sh);
                        });
                    }
                }
                2 => {
                    let _ = DestroyWindow(hwnd);
                }
                IDM_FREQ_1MIN | IDM_FREQ_5MIN | IDM_FREQ_15MIN | IDM_FREQ_1HOUR => {
                    let new_interval = match id {
                        IDM_FREQ_1MIN => POLL_1_MIN,
                        IDM_FREQ_5MIN => POLL_5_MIN,
                        IDM_FREQ_15MIN => POLL_15_MIN,
                        IDM_FREQ_1HOUR => POLL_1_HOUR,
                        _ => POLL_15_MIN,
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.poll_interval_ms = new_interval;
                        }
                    }
                    save_state_settings();
                    // Reset the poll timer with the new interval
                    SetTimer(hwnd, TIMER_POLL, new_interval, None);
                    refresh_menu_bar(hwnd);
                }
                IDM_RESET_FREQ_1HOUR
                | IDM_RESET_FREQ_6HOURS
                | IDM_RESET_FREQ_12HOURS
                | IDM_RESET_FREQ_1DAY => {
                    let new_interval = match id {
                        IDM_RESET_FREQ_1HOUR => RESET_CREDITS_1_HOUR,
                        IDM_RESET_FREQ_12HOURS => RESET_CREDITS_12_HOURS,
                        IDM_RESET_FREQ_1DAY => RESET_CREDITS_1_DAY,
                        _ => RESET_CREDITS_6_HOURS,
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.reset_credit_interval_ms = new_interval;
                        }
                    }
                    save_state_settings();
                    SetTimer(hwnd, TIMER_RESET_CREDITS, new_interval, None);
                    refresh_menu_bar(hwnd);
                }
                IDM_MODEL_CLAUDE_CODE | IDM_MODEL_CODEX | IDM_MODEL_ANTIGRAVITY => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            match id {
                                IDM_MODEL_CLAUDE_CODE => {
                                    if s.show_codex || s.show_antigravity || !s.show_claude_code {
                                        s.show_claude_code = !s.show_claude_code;
                                    }
                                }
                                IDM_MODEL_CODEX => {
                                    if s.show_claude_code || s.show_antigravity || !s.show_codex {
                                        s.show_codex = !s.show_codex;
                                    }
                                }
                                IDM_MODEL_ANTIGRAVITY => {
                                    if s.show_claude_code || s.show_codex || !s.show_antigravity {
                                        s.show_antigravity = !s.show_antigravity;
                                    }
                                }
                                _ => {}
                            }
                            s.session_text = "...".to_string();
                            s.weekly_text = "...".to_string();
                            s.codex_session_text = "...".to_string();
                            s.codex_weekly_text = "...".to_string();
                            s.antigravity_session_text = "...".to_string();
                            s.antigravity_weekly_text = "...".to_string();
                            set_window_title(s);
                        }
                    }
                    save_state_settings();
                    let show_codex = {
                        let state = lock_state();
                        state.as_ref().map(|s| s.show_codex).unwrap_or(false)
                    };
                    sync_reset_credit_timer(hwnd, show_codex);
                    refresh_menu_bar(hwnd);
                    resize_window_to_content(hwnd);
                    repaint();
                    let sh = SendHwnd::from_hwnd(hwnd);
                    std::thread::spawn(move || {
                        do_poll(sh);
                    });
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            remember_window_position(hwnd);
            save_state_settings();
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Append every application command to `menu`. Shared by the window's menu bar
/// and by the right-click context menu so the two can never drift apart.
unsafe fn append_app_menu_items(menu: HMENU) {
    let (current_interval, current_reset_interval, show_claude_code, show_codex, show_antigravity) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.poll_interval_ms,
                s.reset_credit_interval_ms,
                s.show_claude_code,
                s.show_codex,
                s.show_antigravity,
            ),
            None => (
                POLL_15_MIN,
                default_reset_credit_interval(),
                true,
                false,
                false,
            ),
        }
    };
    let refresh_str = native_interop::wide_str(strings::REFRESH);
    let _ = AppendMenuW(
        menu,
        MENU_ITEM_FLAGS(0),
        1,
        PCWSTR::from_raw(refresh_str.as_ptr()),
    );

    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

    // Update Frequency submenu
    let freq_menu = CreatePopupMenu().unwrap();
    let freq_items: [(u16, u32, &str); 4] = [
        (IDM_FREQ_1MIN, POLL_1_MIN, strings::ONE_MINUTE),
        (IDM_FREQ_5MIN, POLL_5_MIN, strings::FIVE_MINUTES),
        (IDM_FREQ_15MIN, POLL_15_MIN, strings::FIFTEEN_MINUTES),
        (IDM_FREQ_1HOUR, POLL_1_HOUR, strings::ONE_HOUR),
    ];
    for (id, interval, label) in freq_items {
        let label_str = native_interop::wide_str(label);
        let flags = if interval == current_interval {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            freq_menu,
            flags,
            id as usize,
            PCWSTR::from_raw(label_str.as_ptr()),
        );
    }

    let freq_label = native_interop::wide_str(strings::UPDATE_FREQUENCY);
    let _ = AppendMenuW(
        menu,
        MF_POPUP,
        freq_menu.0 as usize,
        PCWSTR::from_raw(freq_label.as_ptr()),
    );

    // Reset credits belong to Codex, so the setting only shows with Codex on.
    if show_codex {
        let reset_freq_menu = CreatePopupMenu().unwrap();
        let reset_freq_items: [(u16, u32, &str); 4] = [
            (
                IDM_RESET_FREQ_1HOUR,
                RESET_CREDITS_1_HOUR,
                strings::ONE_HOUR,
            ),
            (
                IDM_RESET_FREQ_6HOURS,
                RESET_CREDITS_6_HOURS,
                strings::SIX_HOURS,
            ),
            (
                IDM_RESET_FREQ_12HOURS,
                RESET_CREDITS_12_HOURS,
                strings::TWELVE_HOURS,
            ),
            (IDM_RESET_FREQ_1DAY, RESET_CREDITS_1_DAY, strings::ONE_DAY),
        ];
        for (id, interval, label) in reset_freq_items {
            let label_str = native_interop::wide_str(label);
            let flags = if interval == current_reset_interval {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                reset_freq_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        let reset_freq_label = native_interop::wide_str(strings::RESET_CREDIT_FREQUENCY);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            reset_freq_menu.0 as usize,
            PCWSTR::from_raw(reset_freq_label.as_ptr()),
        );
    }

    // Models submenu
    let models_menu = CreatePopupMenu().unwrap();
    let model_items: [(u16, bool, &str); 3] = [
        (
            IDM_MODEL_CLAUDE_CODE,
            show_claude_code,
            strings::CLAUDE_CODE_MODEL,
        ),
        (IDM_MODEL_CODEX, show_codex, strings::CODEX_MODEL),
        (
            IDM_MODEL_ANTIGRAVITY,
            show_antigravity,
            strings::ANTIGRAVITY_MODEL,
        ),
    ];
    for (id, enabled, label) in model_items {
        let label_str = native_interop::wide_str(label);
        let flags = if enabled {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            models_menu,
            flags,
            id as usize,
            PCWSTR::from_raw(label_str.as_ptr()),
        );
    }

    let models_label = native_interop::wide_str(strings::MODELS);
    let _ = AppendMenuW(
        menu,
        MF_POPUP,
        models_menu.0 as usize,
        PCWSTR::from_raw(models_label.as_ptr()),
    );

    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

    let exit_str = native_interop::wide_str(strings::EXIT);
    let _ = AppendMenuW(
        menu,
        MENU_ITEM_FLAGS(0),
        2,
        PCWSTR::from_raw(exit_str.as_ptr()),
    );
}

/// (Re)build the window's menu bar. Called whenever a checked state changes,
/// since menu items are built with their state baked in.
fn refresh_menu_bar(hwnd: HWND) {
    let label = strings::SETTINGS;

    unsafe {
        let Ok(bar) = CreateMenu() else {
            return;
        };
        let Ok(popup) = CreatePopupMenu() else {
            let _ = DestroyMenu(bar);
            return;
        };
        append_app_menu_items(popup);

        let label_str = native_interop::wide_str(label);
        let _ = AppendMenuW(
            bar,
            MF_POPUP,
            popup.0 as usize,
            PCWSTR::from_raw(label_str.as_ptr()),
        );

        let previous = GetMenu(hwnd);
        if SetMenu(hwnd, bar).is_ok() {
            if !previous.is_invalid() {
                let _ = DestroyMenu(previous);
            }
            let _ = DrawMenuBar(hwnd);
        } else {
            let _ = DestroyMenu(bar);
        }
    }
}

/// The same commands as the menu bar, shown where the user right-clicked.
fn show_context_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        append_app_menu_items(menu);

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
        let _ = DestroyMenu(menu);
    }
}

/// Draw the window content into the device context supplied by WM_PAINT.
fn paint(hdc: HDC, hwnd: HWND) {
    let (
        is_dark,
        session_pct,
        session_text,
        weekly_pct,
        weekly_text,
        codex_session_pct,
        codex_session_text,
        codex_weekly_pct,
        codex_weekly_text,
        antigravity_session_pct,
        antigravity_session_text,
        antigravity_weekly_pct,
        antigravity_weekly_text,
        codex_reset_lines,
        show_claude_code,
        show_codex,
        show_antigravity,
    ) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.is_dark,
                s.session_percent,
                s.session_text.clone(),
                s.weekly_percent,
                s.weekly_text.clone(),
                s.codex_session_percent,
                s.codex_session_text.clone(),
                s.codex_weekly_percent,
                s.codex_weekly_text.clone(),
                s.antigravity_session_percent,
                s.antigravity_session_text.clone(),
                s.antigravity_weekly_percent,
                s.antigravity_weekly_text.clone(),
                s.codex_reset_lines.clone(),
                s.show_claude_code,
                s.show_codex,
                s.show_antigravity,
            ),
            None => return,
        }
    };
    let track = if is_dark {
        Color::from_hex("#444444")
    } else {
        Color::from_hex("#AAAAAA")
    };
    let text_color = if is_dark {
        Color::from_hex("#888888")
    } else {
        Color::from_hex("#404040")
    };
    let bg_color = if is_dark {
        Color::from_hex("#1C1C1C")
    } else {
        Color::from_hex("#F3F3F3")
    };

    unsafe {
        let mut client_rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut client_rect);
        let width = client_rect.right - client_rect.left;
        let height = client_rect.bottom - client_rect.top;

        if width <= 0 || height <= 0 {
            return;
        }

        let mem_dc = CreateCompatibleDC(hdc);
        let mem_bmp = CreateCompatibleBitmap(hdc, width, height);
        let old_bmp = SelectObject(mem_dc, mem_bmp);

        paint_content(
            mem_dc,
            width,
            height,
            is_dark,
            &bg_color,
            &text_color,
            &track,
            session_pct,
            &session_text,
            weekly_pct,
            &weekly_text,
            codex_session_pct,
            &codex_session_text,
            codex_weekly_pct,
            &codex_weekly_text,
            antigravity_session_pct,
            &antigravity_session_text,
            antigravity_weekly_pct,
            &antigravity_weekly_text,
            &codex_reset_lines,
            show_claude_code,
            show_codex,
            show_antigravity,
        );

        let _ = BitBlt(hdc, 0, 0, width, height, mem_dc, 0, 0, SRCCOPY);

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(mem_bmp);
        let _ = DeleteDC(mem_dc);
    }
}

/// The name a block of rows belongs to, drawn above them in the model's colour.
fn draw_heading(hdc: HDC, x: i32, y: i32, text: &str, color: &Color) {
    unsafe {
        let _ = SetTextColor(hdc, COLORREF(color.to_colorref()));
        let mut text_wide: Vec<u16> = text.encode_utf16().collect();
        let mut rect = RECT {
            left: x,
            top: y,
            right: x + content_inner_width(true),
            bottom: y + sc(SEGMENT_H),
        };
        let _ = DrawTextW(
            hdc,
            &mut text_wide,
            &mut rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

/// One usage window of one model: its label, the segmented bar and the text.
fn draw_usage_row(
    hdc: HDC,
    x: i32,
    y: i32,
    label_color: &Color,
    label: &str,
    percent: f64,
    text: &str,
    accent: &Color,
    track: &Color,
    value_color: &Color,
) {
    draw_row_label(hdc, x, y, label, label_color);
    draw_usage_bar(
        hdc,
        x + sc(LABEL_WIDTH) + sc(LABEL_RIGHT_MARGIN),
        y,
        percent,
        text,
        accent,
        track,
        value_color,
    );
}

/// The resets-in-hand row: no bar to fill, just the count and how long the
/// soonest credit has left, aligned under the usage text above it.
fn draw_reset_row(hdc: HDC, x: i32, y: i32, label_color: &Color, text: &str, value_color: &Color) {
    unsafe {
        draw_row_label(hdc, x, y, strings::RESET_CREDIT_WINDOW, label_color);

        let text_x =
            x + sc(LABEL_WIDTH) + sc(LABEL_RIGHT_MARGIN) + bar_width() + sc(BAR_RIGHT_MARGIN);
        let mut text_wide: Vec<u16> = text.encode_utf16().collect();
        let mut text_rect = RECT {
            left: text_x,
            top: y,
            right: text_x + sc(TEXT_WIDTH),
            bottom: y + sc(SEGMENT_H),
        };
        let _ = SetTextColor(hdc, COLORREF(value_color.to_colorref()));
        let _ = DrawTextW(
            hdc,
            &mut text_wide,
            &mut text_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

fn draw_row_label(hdc: HDC, x: i32, y: i32, label: &str, color: &Color) {
    unsafe {
        let _ = SetTextColor(hdc, COLORREF(color.to_colorref()));
        let mut label_wide: Vec<u16> = label.encode_utf16().collect();
        let mut label_rect = RECT {
            left: x,
            top: y,
            right: x + sc(LABEL_WIDTH),
            bottom: y + sc(SEGMENT_H),
        };
        let _ = DrawTextW(
            hdc,
            &mut label_wide,
            &mut label_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

/// Width of the bar alone, without the usage text beside it.
fn bar_width() -> i32 {
    (sc(SEGMENT_W) + sc(SEGMENT_GAP)) * SEGMENT_COUNT - sc(SEGMENT_GAP)
}

/// Width of a model's bar and the usage text that follows it.
fn model_usage_width() -> i32 {
    bar_width() + sc(BAR_RIGHT_MARGIN) + sc(TEXT_WIDTH)
}

fn draw_usage_bar(
    hdc: HDC,
    bar_x: i32,
    y: i32,
    percent: f64,
    text: &str,
    accent: &Color,
    track: &Color,
    text_color: &Color,
) {
    let seg_w = sc(SEGMENT_W);
    let seg_h = sc(SEGMENT_H);
    let seg_gap = sc(SEGMENT_GAP);
    let corner_r = sc(CORNER_RADIUS);

    unsafe {
        let percent_clamped = percent.clamp(0.0, 100.0);
        let segment_percent = 100.0 / SEGMENT_COUNT as f64;

        for i in 0..SEGMENT_COUNT {
            let seg_x = bar_x + i * (seg_w + seg_gap);
            let seg_start = (i as f64) * segment_percent;
            let seg_end = seg_start + segment_percent;

            let seg_rect = RECT {
                left: seg_x,
                top: y,
                right: seg_x + seg_w,
                bottom: y + seg_h,
            };

            if percent_clamped >= seg_end {
                draw_rounded_rect(hdc, &seg_rect, accent, corner_r);
            } else if percent_clamped <= seg_start {
                draw_rounded_rect(hdc, &seg_rect, track, corner_r);
            } else {
                draw_rounded_rect(hdc, &seg_rect, track, corner_r);
                let fraction = (percent_clamped - seg_start) / segment_percent;
                let fill_width = (seg_w as f64 * fraction) as i32;
                if fill_width > 0 {
                    let fill_rect = RECT {
                        left: seg_x,
                        top: y,
                        right: seg_x + fill_width,
                        bottom: y + seg_h,
                    };
                    let rgn = CreateRoundRectRgn(
                        seg_rect.left,
                        seg_rect.top,
                        seg_rect.right + 1,
                        seg_rect.bottom + 1,
                        corner_r * 2,
                        corner_r * 2,
                    );
                    let _ = SelectClipRgn(hdc, rgn);
                    let brush = CreateSolidBrush(COLORREF(accent.to_colorref()));
                    FillRect(hdc, &fill_rect, brush);
                    let _ = DeleteObject(brush);
                    let _ = SelectClipRgn(hdc, HRGN::default());
                    let _ = DeleteObject(rgn);
                }
            }
        }

        let text_x = bar_x + bar_width() + sc(BAR_RIGHT_MARGIN);
        let mut text_wide: Vec<u16> = text.encode_utf16().collect();
        let mut text_rect = RECT {
            left: text_x,
            top: y,
            right: text_x + sc(TEXT_WIDTH),
            bottom: y + seg_h,
        };
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));
        let _ = DrawTextW(
            hdc,
            &mut text_wide,
            &mut text_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

fn draw_rounded_rect(hdc: HDC, rect: &RECT, color: &Color, radius: i32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
        let rgn = CreateRoundRectRgn(
            rect.left,
            rect.top,
            rect.right + 1,
            rect.bottom + 1,
            radius * 2,
            radius * 2,
        );
        let _ = FillRgn(hdc, rgn, brush);
        let _ = DeleteObject(rgn);
        let _ = DeleteObject(brush);
    }
}
