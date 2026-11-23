// SPDX-License-Identifier: AGPL-3.0-or-later
//! Redmi Buds control protocol (Bluetooth Classic RFCOMM, service UUID
//! `0000fd2d-0000-1000-8000-00805f9b34fb`).
//!
//! Ported from Gadgetbridge (AGPL-3.0-or-later),
//! `service/devices/redmibuds5pro/*`, which implements the same Xiaomi
//! earbuds protocol family.
//!
//! The protocol layer is transport-agnostic and allocation-light so that it can
//! be reused by a low-resource tray app. The Windows transport lives in
//! [`windows_bt`].

pub mod auth;
pub mod frame;
pub mod protocol;

#[cfg(windows)]
pub mod windows_bt;

#[cfg(windows)]
pub mod client;

/// RFCOMM service UUID of the earbuds' control channel.
pub const SERVICE_UUID: u128 = 0x0000fd2d_0000_1000_8000_00805f9b34fb;

/// Formats bytes as space-separated upper-case hex.
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{b:02X}"));
    }
    s
}
