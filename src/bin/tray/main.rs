// SPDX-License-Identifier: AGPL-3.0-or-later
//! Redmi Buds 3 Pro tray app: battery tooltip + a Material 3 style popup
//! (battery, ANC mode, wear detection, gestures). Plain Win32 for the tray,
//! Direct2D/DirectWrite for the popup, which exists only while it is open.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(unsafe_op_in_unsafe_fn)]

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod popup;

#[cfg(not(windows))]
fn main() {
    eprintln!("redmibuds-tray: Windows only for now");
}

#[cfg(windows)]
fn main() {
    #[cfg(feature = "render-docs")]
    if let Some(dir) = std::env::args_os().skip_while(|a| a != "--render-docs").nth(1) {
        if let Err(e) = popup::render_docs(std::path::Path::new(&dir)) {
            eprintln!("render-docs: {e}");
            std::process::exit(1);
        }
        return;
    }
    app::run();
}
