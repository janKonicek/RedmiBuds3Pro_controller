// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tray icon, tooltip, context menu, shared UI state and the client.

use crate::popup;
use redmibuds::client::{Apply, Client, Link, Mode, Update};
use redmibuds::protocol::{AncMode, Batteries, Battery, Event, TapType};
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::*;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, SetProcessWorkingSetSize};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_SYSTEM_AWARE, GetDpiForSystem, SetProcessDpiAwarenessContext};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

/// false = connect on demand (popup open, setting change, periodic battery
/// refresh) and release the channel so the phone app keeps working.
/// true = keep the control channel open permanently.
const HOLD_CONNECTION: bool = false;
const REFRESH_EVERY: Duration = Duration::from_secs(5 * 60);
const LINGER: Duration = Duration::from_millis(1500);
const DEVICE_NAME: &str = "Redmi Buds 3 Pro";

const WM_TRAY: u32 = WM_APP + 1;
const WM_STATE: u32 = WM_APP + 2;
/// Posted by the popup to close itself outside its own window procedure.
pub const WM_CLOSE_POPUP: u32 = WM_APP + 3;
const TRAY_ID: u32 = 1;

const ID_EXIT: usize = 100;
const ID_AUTOSTART: usize = 101;
const ID_REFRESH: usize = 102;

/// Gesture rows shown in the popup.
pub const ROWS: [(TapType, &str); 3] = [(TapType::Double, "Double tap"), (TapType::Triple, "Triple tap"), (TapType::Long, "Long press")];

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("RedmiBudsTray");

pub struct UiState {
    pub link: Option<Link>,
    pub bat: Batteries,
    pub anc: Option<AncMode>,
    pub wear: Option<bool>,
    pub gestures: [Option<(u8, u8)>; 3],
    /// Local time (h, m) of the last successful read.
    pub updated: Option<(u16, u16)>,
    /// Result of the latest setting change.
    pub apply: Option<Apply>,
    /// Raw in-ear bits (NOTIFY_CONFIG 0x0C).
    pub in_ear: Option<u8>,
}

pub static STATE: Mutex<UiState> = Mutex::new(UiState {
    link: None,
    bat: Batteries { left: None, right: None, case: None },
    anc: None,
    wear: None,
    gestures: [None; 3],
    updated: None,
    apply: None,
    in_ear: None,
});
static MAIN: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static CLIENT: OnceLock<Client> = OnceLock::new();
static TRAY: Mutex<TrayIcon> = Mutex::new(TrayIcon { icon: 0, color: 0, popup_closed_at: None });

struct TrayIcon {
    icon: isize,
    color: u32,
    popup_closed_at: Option<Instant>,
}

pub fn main_hwnd() -> HWND {
    HWND(MAIN.load(Ordering::Relaxed) as _)
}

pub fn client() -> &'static Client {
    CLIENT.get().expect("client")
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn trim_memory() {
    unsafe {
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

pub fn popup_closed() {
    TRAY.lock().unwrap().popup_closed_at = Some(Instant::now());
    trim_memory();
}

// ---------------------------------------------------------------- state

fn on_update(u: Update) {
    match &u {
        Update::Raw(sent, b) => log_line(&format!("{} {}", if *sent { "TX" } else { "RX" }, redmibuds::hex(b))),
        Update::Event(Event::Frame(_)) => {}
        other => log_line(&format!("{other:?}")),
    }
    let mut st = STATE.lock().unwrap();
    let mut changed = true;
    match u {
        Update::Link(l) => {
            st.link = Some(l);
            if l == Link::Idle || l == Link::Connected {
                let t = unsafe { GetLocalTime() };
                st.updated = Some((t.wHour, t.wMinute));
            }
        }
        Update::Event(Event::Battery(b)) => st.bat = b,
        Update::Event(Event::Anc(m)) => st.anc = Some(m),
        Update::Event(Event::WearDetection(on)) => st.wear = Some(on),
        Update::Event(Event::InEar(b)) => st.in_ear = Some(b),
        Update::Apply(a) => st.apply = Some(a),
        Update::Event(Event::Gesture { tap, left, right }) => {
            if let Some(i) = ROWS.iter().position(|(t, _)| *t == tap) {
                st.gestures[i] = Some((left, right));
            }
        }
        _ => changed = false,
    }
    drop(st);
    if changed {
        unsafe {
            let _ = PostMessageW(main_hwnd(), WM_STATE, WPARAM(0), LPARAM(0));
        }
    }
}

fn fmt_bat(b: Option<Battery>) -> String {
    match b {
        Some(b) => format!("{}%{}", b.level, if b.charging { "+" } else { "" }),
        None => "–".into(),
    }
}

/// (kind, text): kind 0 ok, 1 busy, 2 disconnected, 3 connecting.
pub fn link_status(st: &UiState) -> (u8, String) {
    match st.link {
        None | Some(Link::Connecting) => (3, "Connecting…".into()),
        Some(Link::Connected) | Some(Link::Idle) => (0, "Connected".into()),
        Some(Link::Busy) => (1, "Busy (phone app?)".into()),
        Some(Link::Disconnected) => (2, "Disconnected".into()),
        Some(Link::NotPaired) => (2, "Not paired".into()),
    }
}

/// Message about the latest setting change: (text, is_warning). None = nothing to say.
pub fn apply_text(st: &UiState) -> Option<(String, bool)> {
    Some(match st.apply? {
        Apply::Applying => ("Applying…".into(), false),
        Apply::Done => return None,
        Apply::Retrying { attempt, of, busy: true } => (format!("Phone is using the controls – retrying ({attempt}/{of})"), true),
        Apply::Retrying { attempt, of, busy: false } => (format!("Couldn't apply – retrying ({attempt}/{of})"), true),
        Apply::Failed { busy: true } => ("Not applied – phone is using the controls".into(), true),
        Apply::Failed { busy: false } => ("Not applied – earbuds not reachable".into(), true),
        Apply::Rejected { .. } => {
            // Bits 3/2 = earbud worn (observed: 0x0C both, 0x08 one).
            let worn = st.in_ear.map(|b| ((b >> 2) & 1) + ((b >> 3) & 1));
            if worn.is_some_and(|w| w < 2) {
                ("Not applied – ANC needs both earbuds in".into(), true)
            } else {
                ("Not applied – the earbuds refused it".into(), true)
            }
        }
    })
}

// ---------------------------------------------------------------- log file

static LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);
const LOG_MAX: u64 = 256 * 1024;

fn log_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("redmibuds"))
}

/// Appends a line to %LOCALAPPDATA%\redmibuds\redmibuds.log (rotated to .1 at 256 KB).
pub fn log_line(s: &str) {
    use std::io::Write;
    let Some(dir) = log_dir() else { return };
    let path = dir.join("redmibuds.log");
    let mut g = LOG.lock().unwrap();
    if g.as_ref().and_then(|f| f.metadata().ok()).is_some_and(|m| m.len() > LOG_MAX) {
        *g = None;
        let _ = std::fs::rename(&path, dir.join("redmibuds.log.1"));
    }
    if g.is_none() {
        let _ = std::fs::create_dir_all(&dir);
        *g = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok();
    }
    if let Some(f) = g.as_mut() {
        let t = unsafe { GetLocalTime() };
        let _ = writeln!(f, "{:02}:{:02}:{:02}.{:03} {s}", t.wHour, t.wMinute, t.wSecond, t.wMilliseconds);
    }
}

pub fn anc_name(a: AncMode) -> &'static str {
    match a {
        AncMode::Off => "Off",
        AncMode::NoiseCancelling => "Noise cancelling",
        AncMode::Transparency => "Transparency",
        AncMode::Unknown(_) => "?",
    }
}

fn tooltip(st: &UiState) -> String {
    let mut s = format!(
        "Redmi Buds 3 Pro\nL {} · R {} · Case {}",
        fmt_bat(st.bat.left),
        fmt_bat(st.bat.right),
        fmt_bat(st.bat.case)
    );
    if let Some(a) = st.anc {
        s.push_str(&format!("\nANC: {}", anc_name(a)));
    }
    let (kind, text) = link_status(st);
    if kind == 1 || kind == 2 {
        s.push_str(&format!("\n{text}"));
    }
    s
}

/// Icon colour (0x00BBGGRR) from the lowest earbud battery.
fn icon_color(st: &UiState) -> u32 {
    let min = [st.bat.left, st.bat.right].iter().flatten().map(|b| b.level).min();
    match (st.link, min) {
        (Some(Link::Disconnected | Link::NotPaired), _) | (_, None) => 0x0090_9090,
        (_, Some(l)) if l <= 15 => 0x004D_48E5,
        (_, Some(l)) if l <= 40 => 0x0024_A5F5,
        _ => 0x0044_B32F,
    }
}

// ---------------------------------------------------------------- icon

/// Two round "earbuds" with stems in `color` on a transparent background.
unsafe fn make_icon(color: u32) -> HICON {
    let size = GetSystemMetrics(SM_CXSMICON).max(16);
    let mut px = vec![0u32; (size * size) as usize];
    let (r, g, b) = (color & 0xFF, (color >> 8) & 0xFF, (color >> 16) & 0xFF);
    let s = size as f32;
    let rad = s * 0.24;
    let centers = [(s * 0.28, s * 0.40), (s * 0.72, s * 0.40)];
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let mut a = 0f32;
            for (cx, cy) in centers {
                let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
                a = a.max((rad + 0.5 - d).clamp(0.0, 1.0));
                let sx = (fx - cx).abs();
                if fy > cy && fy < cy + rad * 2.0 {
                    a = a.max((s * 0.08 + 0.5 - sx).clamp(0.0, 1.0));
                }
            }
            let a8 = (a * 255.0) as u32;
            px[(y * size + x) as usize] = (a8 << 24) | ((r * a8 / 255) << 16) | ((g * a8 / 255) << 8) | (b * a8 / 255);
        }
    }
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let hdc = GetDC(None);
    let color_bmp = CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
    ReleaseDC(None, hdc);
    if !bits.is_null() {
        std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u32, px.len());
    }
    let mask = CreateBitmap(size, size, 1, 1, None);
    let info = ICONINFO { fIcon: TRUE, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color_bmp };
    let icon = CreateIconIndirect(&info).unwrap_or_default();
    let _ = DeleteObject(color_bmp);
    let _ = DeleteObject(mask);
    icon
}

// ---------------------------------------------------------------- tray

unsafe fn tray_data(main: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: main,
        uID: TRAY_ID,
        uCallbackMessage: WM_TRAY,
        ..Default::default()
    }
}

unsafe fn tray_update(add: bool) {
    let st = STATE.lock().unwrap();
    let tip = tooltip(&st);
    let color = icon_color(&st);
    drop(st);

    let mut tray = TRAY.lock().unwrap();
    if tray.icon == 0 || tray.color != color {
        let new = make_icon(color);
        if tray.icon != 0 {
            let _ = DestroyIcon(HICON(tray.icon as _));
        }
        tray.icon = new.0 as isize;
        tray.color = color;
    }
    let icon = HICON(tray.icon as _);
    drop(tray);

    let mut nid = tray_data(main_hwnd());
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.hIcon = icon;
    for (d, s) in nid.szTip.iter_mut().zip(tip.encode_utf16().take(127)) {
        *d = s;
    }
    let _ = Shell_NotifyIconW(if add { NIM_ADD } else { NIM_MODIFY }, &nid);
}

unsafe fn tray_remove() {
    let nid = tray_data(main_hwnd());
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

// ---------------------------------------------------------------- autostart

fn autostart_enabled() -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, None).is_ok() }
}

fn set_autostart(on: bool) {
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else { return };
            let v = wide(&format!("\"{}\"", exe.display()));
            let _ = RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, REG_SZ.0, Some(v.as_ptr() as *const _), (v.len() * 2) as u32);
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
        }
    }
}

unsafe fn show_menu(main: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let _ = AppendMenuW(menu, MF_STRING, ID_REFRESH, w!("Refresh now"));
    let flags = if autostart_enabled() { MF_STRING | MF_CHECKED } else { MF_STRING };
    let _ = AppendMenuW(menu, flags, ID_AUTOSTART, w!("Start with Windows"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, w!("Exit"));
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(main);
    let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, main, None);
    let _ = DestroyMenu(menu);
}

// ---------------------------------------------------------------- wndproc

unsafe extern "system" fn main_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            match (lp.0 as u32) & 0xFFFF {
                WM_LBUTTONUP => {
                    let just_closed = TRAY.lock().unwrap().popup_closed_at.is_some_and(|t| t.elapsed() < Duration::from_millis(300));
                    if popup::is_open() {
                        popup::close();
                    } else if !just_closed {
                        let mut pt = POINT::default();
                        let _ = GetCursorPos(&mut pt);
                        {
                            // Old results are not interesting once the popup is reopened.
                            let mut st = STATE.lock().unwrap();
                            if !matches!(st.apply, Some(Apply::Applying | Apply::Retrying { .. })) {
                                st.apply = None;
                            }
                        }
                        popup::open(pt);
                        client().send(redmibuds::client::Command::Refresh);
                    }
                }
                WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(h),
                _ => {}
            }
            LRESULT(0)
        }
        WM_STATE => {
            tray_update(false);
            popup::refresh();
            let link = STATE.lock().unwrap().link;
            if link == Some(Link::Idle) && !popup::is_open() {
                trim_memory();
            }
            LRESULT(0)
        }
        WM_CLOSE_POPUP => {
            popup::close();
            LRESULT(0)
        }
        WM_COMMAND => {
            match wp.0 & 0xFFFF {
                ID_EXIT => {
                    let _ = DestroyWindow(h);
                }
                ID_REFRESH => client().send(redmibuds::client::Command::Refresh),
                ID_AUTOSTART => set_autostart(!autostart_enabled()),
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            popup::close();
            tray_remove();
            client().send(redmibuds::client::Command::Shutdown);
            PostQuitMessage(0);
            LRESULT(0)
        }
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            tray_update(true);
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

pub fn run() {
    unsafe {
        let _mutex = CreateMutexW(None, true, w!("Local\\RedmiBudsTray.SingleInstance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE);
        popup::set_dpi(GetDpiForSystem());
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);

        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
        let main_class = WNDCLASSW { lpfnWndProc: Some(main_proc), hInstance: hinst, lpszClassName: w!("RedmiBudsTrayMain"), ..Default::default() };
        RegisterClassW(&main_class);
        popup::register_class(hinst);

        // Hidden top-level window (not message-only, so it receives TaskbarCreated).
        let main = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("RedmiBudsTrayMain"),
            w!("Redmi Buds Tray"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            hinst,
            None,
        )
        .expect("main window");
        MAIN.store(main.0 as isize, Ordering::Relaxed);

        tray_update(true);

        log_line(&format!("start, version {}", env!("CARGO_PKG_VERSION")));
        let mode = if HOLD_CONNECTION { Mode::Hold } else { Mode::OnDemand { refresh_every: REFRESH_EVERY, linger: LINGER } };
        let _ = CLIENT.set(Client::spawn(DEVICE_NAME.into(), mode, on_update));

        // Debug/screenshot aid: open the popup right away.
        if std::env::var_os("REDMIBUDS_OPEN_POPUP").is_some() {
            let _ = PostMessageW(main, WM_TRAY, WPARAM(TRAY_ID as usize), LPARAM(WM_LBUTTONUP as isize));
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
