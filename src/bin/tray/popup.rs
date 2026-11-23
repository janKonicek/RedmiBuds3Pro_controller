// SPDX-License-Identifier: AGPL-3.0-or-later
//! Material 3 style popup drawn with Direct2D + DirectWrite.
//! The window, render target and factories exist only while the popup is open.

use crate::app::{self, ROWS, STATE, WM_CLOSE_POPUP, client, wide};
use redmibuds::client::Command;
use redmibuds::protocol::{AncMode, Battery, TapType};
use std::cell::RefCell;
use std::sync::atomic::{AtomicU32, Ordering};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT};
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PCWSTR, w};

const WM_MOUSELEAVE: u32 = 0x02A3;

/// Use the software rasterizer: avoids loading GPU driver DLLs (lower RAM).
const SOFTWARE_RENDERING: bool = true;

// Layout in DIPs.
const W: f32 = 360.0;
const H: f32 = 560.0;
const PAD: f32 = 16.0;
const CARD_X0: f32 = PAD;
const CARD_X1: f32 = W - PAD;
const IN_X0: f32 = CARD_X0 + 16.0;
const IN_X1: f32 = CARD_X1 - 16.0;
const BAT_Y: f32 = 72.0;
const BAT_H: f32 = 140.0;
const NC_Y: f32 = BAT_Y + BAT_H + 12.0;
const NC_H: f32 = 146.0;
const SEG_Y: f32 = NC_Y + 44.0;
const SEG_H: f32 = 40.0;
const SW_Y: f32 = NC_Y + 98.0;
const GE_Y: f32 = NC_Y + NC_H + 12.0;
const GE_H: f32 = 164.0;
const DROP_X: [f32; 2] = [IN_X0 + 88.0, IN_X0 + 88.0 + 108.0];
const DROP_W: f32 = 100.0;
const DROP_H: f32 = 32.0;

/// Short labels inside the dropdown boxes; menus show full names.
const TAP_ACTIONS: &[(u8, &str, &str)] = &[
    (0x01, "Play/Pause", "Play / Pause"),
    (0x02, "Previous", "Previous track"),
    (0x03, "Next", "Next track"),
    (0x04, "Volume +", "Volume up"),
    (0x05, "Volume −", "Volume down"),
    (0x08, "None", "None"),
];
const LONG_ACTIONS: &[(u8, &str, &str)] = &[(0x00, "Assistant", "Voice assistant"), (0x06, "Noise ctrl", "Noise control (cycle)"), (0x08, "None", "None")];

fn actions(row: usize) -> &'static [(u8, &'static str, &'static str)] {
    if ROWS[row].0 == TapType::Long { LONG_ACTIONS } else { TAP_ACTIONS }
}

static DPI: AtomicU32 = AtomicU32::new(96);

pub fn set_dpi(dpi: u32) {
    DPI.store(dpi.max(96), Ordering::Relaxed);
}

fn scale() -> f32 {
    DPI.load(Ordering::Relaxed) as f32 / 96.0
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    Seg(u8),
    Switch,
    Drop(usize, usize),
}

#[derive(Clone, Copy)]
struct Theme {
    bg: u32,
    card: u32,
    text: u32,
    subtle: u32,
    outline: u32,
    track: u32,
    accent: u32,
    on_accent: u32,
    hover_a: f32,
    sel_a: f32,
    dark: bool,
}

struct Fonts {
    title: IDWriteTextFormat,
    body: IDWriteTextFormat,
    body_b: IDWriteTextFormat,
    body_b_right: IDWriteTextFormat,
    body_b_center: IDWriteTextFormat,
    small: IDWriteTextFormat,
    chip: IDWriteTextFormat,
    drop: IDWriteTextFormat,
}

struct Popup {
    hwnd: HWND,
    _d2d: ID2D1Factory,
    dw: IDWriteFactory,
    rt: ID2D1RenderTarget,
    brush: ID2D1SolidColorBrush,
    fonts: Fonts,
    theme: Theme,
    hover: Option<Hit>,
    tracking: bool,
}

thread_local! {
    static POPUP: RefCell<Option<Popup>> = const { RefCell::new(None) };
}

pub fn is_open() -> bool {
    POPUP.with(|p| p.try_borrow().map(|p| p.is_some()).unwrap_or(true))
}

// ---------------------------------------------------------------- theme

fn reg_dword(key: PCWSTR, value: PCWSTR) -> Option<u32> {
    let mut v = 0u32;
    let mut size = 4u32;
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key, value, RRF_RT_REG_DWORD, None, Some(&mut v as *mut _ as *mut _), Some(&mut size))
            .ok()
            .ok()?;
    }
    Some(v)
}

fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |c: u32, s: u32| (((c >> s) & 0xFF) as f32 * (1.0 - t) + ((b >> s) & 0xFF) as f32 * t) as u32;
    (ch(a, 16) << 16) | (ch(a, 8) << 8) | ch(a, 0)
}

/// Windows' default accent colour (0xRRGGBB).
const DEFAULT_ACCENT: u32 = 0x0067C0;

fn theme() -> Theme {
    let dark = match std::env::var("REDMIBUDS_THEME").as_deref() {
        Ok("dark") => true,
        Ok("light") => false,
        _ => reg_dword(w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"), w!("AppsUseLightTheme")) == Some(0),
    };
    // AccentColor is 0xAABBGGRR -> convert to 0xRRGGBB.
    let accent = reg_dword(w!("Software\\Microsoft\\Windows\\DWM"), w!("AccentColor"))
        .map(|abgr| ((abgr & 0xFF) << 16) | (abgr & 0xFF00) | ((abgr >> 16) & 0xFF))
        .unwrap_or(DEFAULT_ACCENT);
    theme_for(dark, accent)
}

fn theme_for(dark: bool, accent: u32) -> Theme {
    if dark {
        Theme {
            bg: 0x1C1C21,
            card: 0x2A2A31,
            text: 0xE6E5EB,
            subtle: 0xA4A5AE,
            outline: 0x4B4C55,
            track: 0x3C3D46,
            accent: mix(accent, 0xFFFFFF, 0.35),
            on_accent: 0x1C1C21,
            hover_a: 0.08,
            sel_a: 0.30,
            dark,
        }
    } else {
        Theme {
            bg: 0xF3F3F8,
            card: 0xFFFFFF,
            text: 0x1B1B21,
            subtle: 0x5F6169,
            outline: 0xC6C7D0,
            track: 0xE3E4EB,
            accent,
            on_accent: 0xFFFFFF,
            hover_a: 0.06,
            sel_a: 0.20,
            dark,
        }
    }
}

fn col(rgb: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((rgb >> 16) & 0xFF) as f32 / 255.0,
        g: ((rgb >> 8) & 0xFF) as f32 / 255.0,
        b: (rgb & 0xFF) as f32 / 255.0,
        a,
    }
}

fn rect(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: l, top: t, right: r, bottom: b }
}

fn rr(l: f32, t: f32, r: f32, b: f32, rad: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT { rect: rect(l, t, r, b), radiusX: rad, radiusY: rad }
}

fn inside(r: &D2D_RECT_F, x: f32, y: f32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn seg_rect(i: usize) -> D2D_RECT_F {
    let w = (IN_X1 - IN_X0) / 3.0;
    rect(IN_X0 + w * i as f32, SEG_Y, IN_X0 + w * (i + 1) as f32, SEG_Y + SEG_H)
}

fn switch_rect() -> D2D_RECT_F {
    rect(IN_X1 - 52.0, SW_Y + 2.0, IN_X1, SW_Y + 34.0)
}

fn drop_rect(row: usize, side: usize) -> D2D_RECT_F {
    let y = GE_Y + 44.0 + row as f32 * 38.0;
    rect(DROP_X[side], y, DROP_X[side] + DROP_W, y + DROP_H)
}

fn hit_test(x: f32, y: f32) -> Option<Hit> {
    for i in 0..3 {
        if inside(&seg_rect(i), x, y) {
            return Some(Hit::Seg(i as u8));
        }
    }
    if inside(&switch_rect(), x, y) {
        return Some(Hit::Switch);
    }
    for row in 0..3 {
        for side in 0..2 {
            if inside(&drop_rect(row, side), x, y) {
                return Some(Hit::Drop(row, side));
            }
        }
    }
    None
}

// ---------------------------------------------------------------- resources

unsafe fn text_format(dw: &IDWriteFactory, family: PCWSTR, size: f32, weight: i32, align: DWRITE_TEXT_ALIGNMENT) -> windows::core::Result<IDWriteTextFormat> {
    let f = dw.CreateTextFormat(family, None, DWRITE_FONT_WEIGHT(weight), DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size, w!("en-us"))?;
    f.SetTextAlignment(align)?;
    f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
    f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
    let sign = dw.CreateEllipsisTrimmingSign(&f)?;
    let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
    f.SetTrimming(&trim, &sign)?;
    Ok(f)
}

unsafe fn font_exists(dw: &IDWriteFactory, name: PCWSTR) -> bool {
    let mut coll = None;
    if dw.GetSystemFontCollection(&mut coll, false).is_err() {
        return false;
    }
    let Some(coll) = coll else { return false };
    let (mut idx, mut exists) = (0u32, BOOL(0));
    coll.FindFamilyName(name, &mut idx, &mut exists).is_ok() && exists.as_bool()
}

fn rt_props(dpi: f32) -> D2D1_RENDER_TARGET_PROPERTIES {
    D2D1_RENDER_TARGET_PROPERTIES {
        r#type: if SOFTWARE_RENDERING { D2D1_RENDER_TARGET_TYPE_SOFTWARE } else { D2D1_RENDER_TARGET_TYPE_DEFAULT },
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
        dpiX: dpi,
        dpiY: dpi,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    }
}

unsafe fn create(hwnd: HWND) -> windows::core::Result<Popup> {
    let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
    let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
    let s = scale();
    let props = rt_props(DPI.load(Ordering::Relaxed) as f32);
    let hprops = D2D1_HWND_RENDER_TARGET_PROPERTIES {
        hwnd,
        pixelSize: D2D_SIZE_U { width: (W * s).round() as u32, height: (H * s).round() as u32 },
        presentOptions: D2D1_PRESENT_OPTIONS_NONE,
    };
    let rt: ID2D1RenderTarget = d2d.CreateHwndRenderTarget(&props, &hprops)?.cast()?;
    resources(hwnd, d2d, dw, rt, theme())
}

/// Brush, fonts and theme for a render target (window or offscreen).
unsafe fn resources(hwnd: HWND, d2d: ID2D1Factory, dw: IDWriteFactory, rt: ID2D1RenderTarget, theme: Theme) -> windows::core::Result<Popup> {
    rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
    let brush = rt.CreateSolidColorBrush(&col(0, 1.0), None)?;

    let (text_font, disp_font) = if font_exists(&dw, w!("Segoe UI Variable Text")) {
        (w!("Segoe UI Variable Text"), w!("Segoe UI Variable Display"))
    } else {
        (w!("Segoe UI"), w!("Segoe UI"))
    };
    let lead = DWRITE_TEXT_ALIGNMENT_LEADING;
    let fonts = Fonts {
        title: text_format(&dw, disp_font, 20.0, 600, lead)?,
        body: text_format(&dw, text_font, 13.0, 400, lead)?,
        body_b: text_format(&dw, text_font, 14.0, 600, lead)?,
        body_b_right: text_format(&dw, text_font, 13.0, 600, DWRITE_TEXT_ALIGNMENT_TRAILING)?,
        body_b_center: text_format(&dw, text_font, 13.0, 600, DWRITE_TEXT_ALIGNMENT_CENTER)?,
        small: text_format(&dw, text_font, 11.5, 400, lead)?,
        chip: text_format(&dw, text_font, 12.0, 600, lead)?,
        drop: text_format(&dw, text_font, 12.5, 400, lead)?,
    };
    Ok(Popup { hwnd, _d2d: d2d, dw, rt, brush, fonts, theme, hover: None, tracking: false })
}

// ---------------------------------------------------------------- drawing

impl Popup {
    unsafe fn fill(&self, r: D2D_RECT_F, c: D2D1_COLOR_F) {
        self.brush.SetColor(&c);
        self.rt.FillRectangle(&r, &self.brush);
    }
    unsafe fn fill_rr(&self, r: D2D1_ROUNDED_RECT, c: D2D1_COLOR_F) {
        self.brush.SetColor(&c);
        self.rt.FillRoundedRectangle(&r, &self.brush);
    }
    unsafe fn stroke_rr(&self, r: D2D1_ROUNDED_RECT, c: D2D1_COLOR_F, w: f32) {
        self.brush.SetColor(&c);
        self.rt.DrawRoundedRectangle(&r, &self.brush, w, None);
    }
    unsafe fn circle(&self, x: f32, y: f32, rad: f32, c: D2D1_COLOR_F) {
        self.brush.SetColor(&c);
        let e = D2D1_ELLIPSE { point: D2D_POINT_2F { x, y }, radiusX: rad, radiusY: rad };
        self.rt.FillEllipse(&e, &self.brush);
    }
    unsafe fn text(&self, s: &str, f: &IDWriteTextFormat, r: D2D_RECT_F, c: D2D1_COLOR_F) {
        self.brush.SetColor(&c);
        let t: Vec<u16> = s.encode_utf16().collect();
        self.rt.DrawText(
            &t,
            f,
            &r,
            &self.brush,
            D2D1_DRAW_TEXT_OPTIONS_CLIP | D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
            DWRITE_MEASURING_MODE_NATURAL,
        );
    }
    unsafe fn text_width(&self, s: &str, f: &IDWriteTextFormat) -> f32 {
        let t: Vec<u16> = s.encode_utf16().collect();
        let Ok(layout) = self.dw.CreateTextLayout(&t, f, 1000.0, 100.0) else { return 0.0 };
        let mut m = DWRITE_TEXT_METRICS::default();
        let _ = layout.GetMetrics(&mut m);
        m.widthIncludingTrailingWhitespace
    }
    /// Fill one segment of a segmented button (rounded only on the outer ends).
    unsafe fn fill_segment(&self, i: usize, c: D2D1_COLOR_F) {
        let r = seg_rect(i);
        let rad = SEG_H / 2.0;
        match i {
            0 => {
                self.fill_rr(rr(r.left, r.top, r.right, r.bottom, rad), c);
                self.fill(rect(r.left + rad, r.top, r.right, r.bottom), c);
            }
            2 => {
                self.fill_rr(rr(r.left, r.top, r.right, r.bottom, rad), c);
                self.fill(rect(r.left, r.top, r.right - rad, r.bottom), c);
            }
            _ => self.fill(r, c),
        }
    }

    unsafe fn draw(&self) {
        self.rt.BeginDraw();
        self.rt.Clear(Some(&col(self.theme.bg, 1.0)));
        self.paint();
        let _ = self.rt.EndDraw(None, None);
    }

    /// Draws the contents (everything except the background).
    unsafe fn paint(&self) {
        let t = self.theme;
        let text = col(t.text, 1.0);
        let subtle = col(t.subtle, 1.0);
        let st = STATE.lock().unwrap();

        // Header: title, last update, status chip.
        self.text("Redmi Buds 3 Pro", &self.fonts.title, rect(PAD + 4.0, 14.0, W - 150.0, 42.0), text);
        let updated = match st.updated {
            Some((h, m)) => format!("Updated {h:02}:{m:02}"),
            None => "Not read yet".into(),
        };
        match app::apply_text(&st) {
            Some((msg, warn)) => {
                let c = if !warn { subtle } else if t.dark { col(0xF5A524, 1.0) } else { col(0xB25E00, 1.0) };
                self.text(&msg, &self.fonts.small, rect(PAD + 4.0, 46.0, W - PAD, 64.0), c);
            }
            None => self.text(&updated, &self.fonts.small, rect(PAD + 4.0, 46.0, W - PAD, 64.0), subtle),
        }
        let (kind, status) = app::link_status(&st);
        let tw = self.text_width(&status, &self.fonts.chip);
        let (cx1, cy0) = (W - PAD, 14.0);
        let cx0 = cx1 - tw - 34.0;
        let dot = match kind {
            0 => col(0x2FB344, 1.0),
            1 => col(0xF5A524, 1.0),
            2 => col(0x8B8D98, 1.0),
            _ => col(t.accent, 1.0),
        };
        self.fill_rr(rr(cx0, cy0, cx1, cy0 + 30.0, 8.0), col(t.card, 1.0));
        self.stroke_rr(rr(cx0 + 0.5, cy0 + 0.5, cx1 - 0.5, cy0 + 29.5, 8.0), col(t.outline, 1.0), 1.0);
        self.circle(cx0 + 15.0, cy0 + 15.0, 4.0, dot);
        self.text(&status, &self.fonts.chip, rect(cx0 + 25.0, cy0, cx1 - 6.0, cy0 + 30.0), text);

        // Battery card.
        self.fill_rr(rr(CARD_X0, BAT_Y, CARD_X1, BAT_Y + BAT_H, 16.0), col(t.card, 1.0));
        self.text("Battery", &self.fonts.body_b, rect(IN_X0, BAT_Y + 12.0, IN_X1, BAT_Y + 36.0), text);
        let rows = [("Left", st.bat.left), ("Right", st.bat.right), ("Case", st.bat.case)];
        for (i, (name, b)) in rows.iter().enumerate() {
            let y = BAT_Y + 44.0 + i as f32 * 30.0;
            self.text(name, &self.fonts.body, rect(IN_X0, y, IN_X0 + 50.0, y + 24.0), text);
            let (bx0, bx1) = (IN_X0 + 52.0, IN_X1 - 70.0);
            let by = y + 7.0;
            self.fill_rr(rr(bx0, by, bx1, by + 10.0, 5.0), col(t.track, 1.0));
            if let Some(Battery { level, .. }) = b {
                let lvl = (*level).min(100) as f32;
                let c = if lvl <= 15.0 { 0xE5484D } else if lvl <= 40.0 { 0xF5A524 } else { t.accent };
                let fx = bx0 + (bx1 - bx0) * lvl / 100.0;
                if fx > bx0 + 1.0 {
                    self.fill_rr(rr(bx0, by, fx.max(bx0 + 10.0), by + 10.0, 5.0), col(c, 1.0));
                }
            }
            let label = match b {
                Some(b) if b.charging => format!("⚡ {} %", b.level),
                Some(b) => format!("{} %", b.level),
                None => "–".into(),
            };
            self.text(&label, &self.fonts.body_b_right, rect(IN_X1 - 64.0, y, IN_X1, y + 24.0), text);
        }

        // Noise control card.
        self.fill_rr(rr(CARD_X0, NC_Y, CARD_X1, NC_Y + NC_H, 16.0), col(t.card, 1.0));
        self.text("Noise control", &self.fonts.body_b, rect(IN_X0, NC_Y + 12.0, IN_X1, NC_Y + 36.0), text);
        let sel = match st.anc {
            Some(AncMode::Off) => Some(0),
            Some(AncMode::NoiseCancelling) => Some(1),
            Some(AncMode::Transparency) => Some(2),
            _ => None,
        };
        for (i, name) in ["Off", "ANC", "Transparency"].iter().enumerate() {
            if sel == Some(i) {
                self.fill_segment(i, col(mix(t.card, t.accent, t.sel_a), 1.0));
            } else if self.hover == Some(Hit::Seg(i as u8)) {
                self.fill_segment(i, col(mix(t.card, t.text, t.hover_a), 1.0));
            }
            let r = seg_rect(i);
            let label = if sel == Some(i) { format!("✓  {name}") } else { name.to_string() };
            self.text(&label, &self.fonts.body_b_center, r, text);
        }
        let outline = col(t.outline, 1.0);
        self.stroke_rr(rr(IN_X0 + 0.5, SEG_Y + 0.5, IN_X1 - 0.5, SEG_Y + SEG_H - 0.5, SEG_H / 2.0), outline, 1.0);
        self.brush.SetColor(&outline);
        for i in 1..3 {
            let x = seg_rect(i).left.round() + 0.5;
            self.rt.DrawLine(D2D_POINT_2F { x, y: SEG_Y }, D2D_POINT_2F { x, y: SEG_Y + SEG_H }, &self.brush, 1.0, None);
        }
        // Wear detection + switch.
        self.text("Wear detection", &self.fonts.body, rect(IN_X0, SW_Y, IN_X1 - 60.0, SW_Y + 20.0), text);
        self.text("Pause when an earbud is taken out", &self.fonts.small, rect(IN_X0, SW_Y + 18.0, IN_X1 - 60.0, SW_Y + 36.0), subtle);
        let sw = switch_rect();
        let on = st.wear == Some(true);
        let (tr, rad) = (rr(sw.left, sw.top, sw.right, sw.bottom, 16.0), 16.0);
        if on {
            self.fill_rr(tr, col(t.accent, 1.0));
            if self.hover == Some(Hit::Switch) {
                self.circle(sw.right - rad, sw.top + rad, 15.0, col(t.on_accent, 0.15));
            }
            self.circle(sw.right - rad, sw.top + rad, 12.0, col(t.on_accent, 1.0));
        } else {
            self.fill_rr(tr, col(t.track, 1.0));
            self.stroke_rr(rr(sw.left + 1.0, sw.top + 1.0, sw.right - 1.0, sw.bottom - 1.0, 15.0), col(t.subtle, 1.0), 2.0);
            if self.hover == Some(Hit::Switch) {
                self.circle(sw.left + rad, sw.top + rad, 14.0, col(t.text, t.hover_a));
            }
            self.circle(sw.left + rad, sw.top + rad, 8.0, col(t.subtle, 1.0));
        }

        // Gestures card.
        self.fill_rr(rr(CARD_X0, GE_Y, CARD_X1, GE_Y + GE_H, 16.0), col(t.card, 1.0));
        self.text("Gestures", &self.fonts.body_b, rect(IN_X0, GE_Y + 12.0, IN_X0 + 86.0, GE_Y + 36.0), text);
        self.text("Left", &self.fonts.small, rect(DROP_X[0] + 4.0, GE_Y + 14.0, DROP_X[0] + DROP_W, GE_Y + 36.0), subtle);
        self.text("Right", &self.fonts.small, rect(DROP_X[1] + 4.0, GE_Y + 14.0, DROP_X[1] + DROP_W, GE_Y + 36.0), subtle);
        for (row, (_, name)) in ROWS.iter().enumerate() {
            let y = drop_rect(row, 0).top;
            self.text(name, &self.fonts.body, rect(IN_X0, y, DROP_X[0] - 4.0, y + DROP_H), text);
            for side in 0..2 {
                let r = drop_rect(row, side);
                if self.hover == Some(Hit::Drop(row, side)) {
                    self.fill_rr(rr(r.left, r.top, r.right, r.bottom, 8.0), col(t.text, t.hover_a));
                }
                self.stroke_rr(rr(r.left + 0.5, r.top + 0.5, r.right - 0.5, r.bottom - 0.5, 8.0), outline, 1.0);
                let label = st.gestures[row]
                    .map(|(l, rr_)| if side == 0 { l } else { rr_ })
                    .map(|code| actions(row).iter().find(|a| a.0 == code).map(|a| a.1.to_string()).unwrap_or(format!("0x{code:02X}")))
                    .unwrap_or_else(|| "–".into());
                self.text(&label, &self.fonts.drop, rect(r.left + 10.0, r.top, r.right - 20.0, r.bottom), text);
                self.text("⌄", &self.fonts.body, rect(r.right - 18.0, r.top - 4.0, r.right - 4.0, r.bottom - 4.0), subtle);
            }
        }
        drop(st);
    }
}

// ---------------------------------------------------------------- window

pub fn register_class(hinst: HINSTANCE) {
    unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: Some(popup_proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: w!("RedmiBudsTrayPopup"),
            style: CS_DROPSHADOW,
            ..Default::default()
        };
        RegisterClassW(&class);
    }
}

pub fn open(at: POINT) {
    if is_open() {
        return;
    }
    unsafe {
        let s = scale();
        let (w, h) = ((W * s).round() as i32, (H * s).round() as i32);
        let mut work = RECT::default();
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut work as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
        let m = (12.0 * s) as i32;
        let x = (at.x - w / 2).clamp(work.left + m, (work.right - w - m).max(work.left + m));
        let y = if at.y >= work.bottom - m {
            work.bottom - h - m
        } else if at.y <= work.top + m {
            work.top + m
        } else {
            (at.y - h - m).max(work.top + m)
        };
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            w!("RedmiBudsTrayPopup"),
            w!("Redmi Buds 3 Pro"),
            WS_POPUP,
            x,
            y,
            w,
            h,
            app::main_hwnd(),
            None,
            None,
            None,
        ) else {
            return;
        };
        let popup = match create(hwnd) {
            Ok(p) => p,
            Err(_) => {
                let _ = DestroyWindow(hwnd);
                return;
            }
        };
        // Windows 11: rounded corners, dark border in dark mode.
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corner as *const _ as *const _, 4);
        let dark = BOOL::from(popup.theme.dark);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as *const _, 4);
        let o = popup.theme.outline;
        let border = ((o & 0xFF) << 16) | (o & 0xFF00) | ((o >> 16) & 0xFF); // COLORREF 0x00BBGGRR
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR, &border as *const _ as *const _, 4);

        POPUP.with(|p| *p.borrow_mut() = Some(popup));
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
}

pub fn close() {
    let Some(popup) = POPUP.with(|p| p.try_borrow_mut().ok().and_then(|mut p| p.take())) else { return };
    let hwnd = popup.hwnd;
    drop(popup); // releases render target and factories
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    app::popup_closed();
}

/// Repaint after a state change (no-op when closed).
pub fn refresh() {
    POPUP.with(|p| {
        if let Ok(p) = p.try_borrow() {
            if let Some(p) = p.as_ref() {
                unsafe {
                    let _ = InvalidateRect(p.hwnd, None, false);
                }
            }
        }
    });
}

fn with_popup<R>(f: impl FnOnce(&mut Popup) -> R) -> Option<R> {
    POPUP.with(|p| p.try_borrow_mut().ok().and_then(|mut p| p.as_mut().map(f)))
}

/// Runs a click outside the RefCell borrow (it may open a modal menu).
unsafe fn on_click(hwnd: HWND, hit: Hit) {
    let cmd = match hit {
        Hit::Seg(i) => {
            let m = [AncMode::Off, AncMode::NoiseCancelling, AncMode::Transparency][i as usize];
            STATE.lock().unwrap().anc = Some(m);
            Some(Command::SetAnc(m))
        }
        Hit::Switch => {
            let mut st = STATE.lock().unwrap();
            let on = st.wear != Some(true);
            st.wear = Some(on);
            Some(Command::SetWearDetection(on))
        }
        Hit::Drop(row, side) => {
            let current = STATE.lock().unwrap().gestures[row].map(|(l, r)| if side == 0 { l } else { r });
            let Ok(menu) = CreatePopupMenu() else { return };
            for (i, (code, _, full)) in actions(row).iter().enumerate() {
                let flags = if current == Some(*code) { MF_STRING | MF_CHECKED } else { MF_STRING };
                let name = wide(full);
                let _ = AppendMenuW(menu, flags, i + 1, PCWSTR(name.as_ptr()));
            }
            let r = drop_rect(row, side);
            let s = scale();
            let mut pt = POINT { x: (r.left * s) as i32, y: (r.bottom * s) as i32 + 2 };
            let _ = windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut pt);
            let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN | TPM_NONOTIFY, pt.x, pt.y, 0, hwnd, None).0;
            let _ = DestroyMenu(menu);
            if chosen <= 0 {
                None
            } else {
                let code = actions(row)[(chosen - 1) as usize].0;
                let mut st = STATE.lock().unwrap();
                let cur = st.gestures[row].unwrap_or((0xFF, 0xFF));
                st.gestures[row] = Some(if side == 0 { (code, cur.1) } else { (cur.0, code) });
                let tap = ROWS[row].0;
                Some(if side == 0 { Command::SetGesture(tap, Some(code), None) } else { Command::SetGesture(tap, None, Some(code)) })
            }
        }
    };
    if let Some(c) = cmd {
        client().send(c);
    }
    refresh();
}

unsafe extern "system" fn popup_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let s = scale();
    let pos = || ((lp.0 & 0xFFFF) as i16 as f32 / s, ((lp.0 >> 16) & 0xFFFF) as i16 as f32 / s);
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            BeginPaint(h, &mut ps);
            with_popup(|p| p.draw());
            let _ = EndPaint(h, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEMOVE => {
            let (x, y) = pos();
            let hit = hit_test(x, y);
            with_popup(|p| {
                if !p.tracking {
                    let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
                    let _ = TrackMouseEvent(&mut tme);
                    p.tracking = true;
                }
                if p.hover != hit {
                    p.hover = hit;
                    let _ = InvalidateRect(h, None, false);
                }
            });
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            with_popup(|p| {
                p.tracking = false;
                if p.hover.is_some() {
                    p.hover = None;
                    let _ = InvalidateRect(h, None, false);
                }
            });
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if with_popup(|p| p.hover.is_some()).unwrap_or(false) {
                SetCursor(LoadCursorW(None, IDC_HAND).unwrap_or_default());
                return LRESULT(1);
            }
            DefWindowProcW(h, msg, wp, lp)
        }
        WM_LBUTTONUP => {
            let (x, y) = pos();
            if let Some(hit) = hit_test(x, y) {
                on_click(h, hit);
            }
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE {
                let _ = PostMessageW(app::main_hwnd(), WM_CLOSE_POPUP, WPARAM(0), LPARAM(0));
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wp.0 == 0x1B => {
            let _ = PostMessageW(app::main_hwnd(), WM_CLOSE_POPUP, WPARAM(0), LPARAM(0));
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

// ---------------------------------------------------------------- docs screenshots

/// Renders the popup offscreen (no window, no screen capture) with fixed sample
/// data into transparent PNGs: `popup-light.png`, `popup-dark.png`, `popup-rejected.png`.
/// Only built with `--features render-docs`; run `redmibuds-tray --render-docs <dir>`.
#[cfg(feature = "render-docs")]
pub fn render_docs(dir: &std::path::Path) -> windows::core::Result<()> {
    use redmibuds::client::{Apply, Link};
    use redmibuds::protocol::Batteries;
    use windows::Win32::Graphics::Imaging::{CLSID_WICImagingFactory, IWICImagingFactory};
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};

    let bat = |level| Some(Battery { level, charging: false });
    let set = |anc, apply, in_ear| {
        let mut st = STATE.lock().unwrap();
        st.link = Some(Link::Idle);
        st.bat = Batteries { left: bat(71), right: bat(74), case: bat(76) };
        st.anc = Some(anc);
        st.wear = Some(true);
        // Factory defaults: Play/Pause, Next, Noise control.
        st.gestures = [Some((0x01, 0x01)), Some((0x03, 0x03)), Some((0x06, 0x06))];
        st.updated = Some((12, 0));
        st.apply = apply;
        st.in_ear = in_ear;
    };
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        set(AncMode::Transparency, None, Some(0x0C));
        render_png(&wic, false, &dir.join("popup-light.png"))?;
        render_png(&wic, true, &dir.join("popup-dark.png"))?;
        // ANC requested with one earbud worn -> refused, mode stays Off.
        set(AncMode::Off, Some(Apply::Rejected { status: 0x09 }), Some(0x08));
        render_png(&wic, true, &dir.join("popup-rejected.png"))?;
    }
    Ok(())
}

#[cfg(feature = "render-docs")]
unsafe fn render_png(wic: &windows::Win32::Graphics::Imaging::IWICImagingFactory, dark: bool, path: &std::path::Path) -> windows::core::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Graphics::Imaging::*;

    let (w, h) = (W as u32, H as u32);
    let bitmap = wic.CreateBitmap(w, h, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad)?;
    let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
    let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
    let mut props = rt_props(96.0);
    props.r#type = D2D1_RENDER_TARGET_TYPE_SOFTWARE;
    let rt = d2d.CreateWicBitmapRenderTarget(&bitmap, &props)?;
    let p = resources(HWND::default(), d2d, dw, rt, theme_for(dark, DEFAULT_ACCENT))?;

    // Transparent outside the window shape: Windows 11 rounds popups (8 px) and
    // draws a 1 px border in the outline colour (set via DWMWA_BORDER_COLOR).
    p.rt.BeginDraw();
    p.rt.Clear(Some(&col(0, 0.0)));
    p.fill_rr(rr(0.0, 0.0, W, H, 8.0), col(p.theme.bg, 1.0));
    p.paint();
    p.stroke_rr(rr(0.5, 0.5, W - 0.5, H - 0.5, 7.5), col(p.theme.outline, 1.0), 1.0);
    p.rt.EndDraw(None, None)?;

    let conv = wic.CreateFormatConverter()?;
    conv.Initialize(&bitmap, &GUID_WICPixelFormat32bppBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
    let stream = wic.CreateStream()?;
    let wpath: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    stream.InitializeFromFilename(PCWSTR(wpath.as_ptr()), GENERIC_WRITE.0)?;
    let enc = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
    enc.Initialize(&stream, WICBitmapEncoderNoCache)?;
    let mut frame = None;
    enc.CreateNewFrame(&mut frame, &mut None)?;
    let frame = frame.unwrap();
    frame.Initialize(None)?;
    frame.SetSize(w, h)?;
    let mut fmt = GUID_WICPixelFormat32bppBGRA;
    frame.SetPixelFormat(&mut fmt)?;
    frame.WriteSource(&conv, std::ptr::null())?;
    frame.Commit()?;
    enc.Commit()?;
    Ok(())
}
