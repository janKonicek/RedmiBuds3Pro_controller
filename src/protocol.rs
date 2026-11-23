// SPDX-License-Identifier: AGPL-3.0-or-later
//! Session logic (port of Gadgetbridge `RedmiBuds5ProProtocol.java`):
//! authentication handshake, init requests, battery / ANC decoding and acks.
//!
//! Transport-agnostic: feed received bytes into [`Session::on_bytes`], write
//! every returned buffer to the socket.

use crate::auth;
use crate::frame::{FrameReader, Message, MessageType, Opcode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    /// 0..=100
    pub level: u8,
    pub charging: bool,
}

impl Battery {
    /// `0xFF` = not available (e.g. bud not connected / case closed).
    pub fn parse(b: u8) -> Option<Self> {
        (b != 0xFF).then_some(Self { level: b & 0x7F, charging: b & 0x80 != 0 })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Batteries {
    pub left: Option<Battery>,
    pub right: Option<Battery>,
    pub case: Option<Battery>,
}

impl Batteries {
    /// Payload order is left, right, case.
    fn from3(b: &[u8]) -> Self {
        Self { left: Battery::parse(b[0]), right: Battery::parse(b[1]), case: Battery::parse(b[2]) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AncMode {
    Off,
    NoiseCancelling,
    Transparency,
    Unknown(u8),
}

impl AncMode {
    pub fn from_code(c: u8) -> Self {
        match c {
            0 => Self::Off,
            1 => Self::NoiseCancelling,
            2 => Self::Transparency,
            c => Self::Unknown(c),
        }
    }
    pub fn code(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::NoiseCancelling => 1,
            Self::Transparency => 2,
            Self::Unknown(c) => c,
        }
    }
}

/// Tap gesture kinds (Gadgetbridge `RedmiBudsTapType`). The 3 Pro reports
/// double, triple and long.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TapType {
    Single,
    Double,
    Triple,
    Long,
}

impl TapType {
    pub fn code(self) -> u8 {
        match self {
            Self::Single => 0x04,
            Self::Double => 0x01,
            Self::Triple => 0x02,
            Self::Long => 0x03,
        }
    }
    pub fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            0x04 => Self::Single,
            0x01 => Self::Double,
            0x02 => Self::Triple,
            0x03 => Self::Long,
            _ => return None,
        })
    }
}

/// Gesture action codes (Gadgetbridge `RedmiBudsGestureAction` / `RedmiBudsLongGestureAction`).
pub mod gesture {
    /// Short taps (double / triple / single).
    pub const TAP_ACTIONS: &[(u8, &str)] = &[
        (0x01, "Play/Pause"),
        (0x02, "Previous track"),
        (0x03, "Next track"),
        (0x04, "Volume up"),
        (0x05, "Volume down"),
        (0x08, "None"),
    ];
    /// Long press.
    pub const LONG_ACTIONS: &[(u8, &str)] = &[(0x00, "Voice assistant"), (0x06, "Noise control"), (0x08, "None")];
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Raw frame received (for debugging).
    Frame(Message),
    Authenticated,
    DeviceInfo { fw: String, fw2: String, vid: u16, pid: u16 },
    Battery(Batteries),
    Anc(AncMode),
    /// Ear-detection state from run info (true = enabled).
    WearDetection(bool),
    /// Gesture assignment for one tap type; actions are raw codes, see [`gesture`].
    Gesture { tap: TapType, left: u8, right: u8 },
    /// In-ear / in-case state (NOTIFY_CONFIG 0x0C), raw bits. Observed:
    /// 0x0C with both earbuds worn, 0x08 with one. Bits 3/2 are taken to be
    /// "worn" and bits 1/0 "in case" (Gadgetbridge leaves this undecoded).
    InEar(u8),
    /// The earbuds rejected our request (response status != 0).
    Nack { opcode: Opcode, seq: u8, status: u8 },
    /// Frame we did not decode.
    Unhandled(Message),
}

#[derive(Debug, Default)]
pub struct Session {
    seq: u8,
    reader: FrameReader,
    authenticated: bool,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    fn next_seq(&mut self) -> u8 {
        let s = self.seq;
        self.seq = self.seq.wrapping_add(1);
        s
    }

    fn request(&mut self, op: Opcode, payload: impl Into<Vec<u8>>) -> Vec<u8> {
        let seq = self.next_seq();
        Message::new(MessageType::PhoneRequest, op, seq, payload).encode()
    }

    /// First frame to send after the RFCOMM socket is open.
    pub fn start(&mut self) -> Vec<u8> {
        self.start_with_challenge(&auth::random_challenge())
    }

    pub fn start_with_challenge(&mut self, challenge: &[u8; 16]) -> Vec<u8> {
        let mut p = Vec::with_capacity(17);
        p.push(0x01);
        p.extend_from_slice(challenge);
        self.request(Opcode::AuthChallenge, p)
    }

    /// Sets the ANC / ambient sound mode.
    pub fn encode_set_anc(&mut self, mode: AncMode) -> Vec<u8> {
        self.request(Opcode::Anc, [0x02, 0x04, mode.code()])
    }

    pub fn encode_get_device_info(&mut self) -> Vec<u8> {
        self.request(Opcode::GetDeviceInfo, [0xFF; 4])
    }

    pub fn encode_get_run_info(&mut self) -> Vec<u8> {
        self.request(Opcode::GetDeviceRunInfo, [0xFF; 4])
    }

    /// Enables / disables wear (in-ear) detection.
    pub fn encode_set_wear_detection(&mut self, enabled: bool) -> Vec<u8> {
        self.request(Opcode::Anc, [0x02, 0x06, if enabled { 0x00 } else { 0x01 }])
    }

    /// Sets a gesture action; `None` leaves that side unchanged.
    pub fn encode_set_gesture(&mut self, tap: TapType, left: Option<u8>, right: Option<u8>) -> Vec<u8> {
        self.request(
            Opcode::SetConfig,
            [0x05, 0x00, config::GESTURES, tap.code(), left.unwrap_or(0xFF), right.unwrap_or(0xFF)],
        )
    }

    /// GET_CONFIG for one config id (see [`config`]).
    pub fn encode_get_config(&mut self, id: u8) -> Vec<u8> {
        self.request(Opcode::GetConfig, [0x00, id])
    }

    /// Feeds received bytes. Returns decoded events and frames to write back.
    pub fn on_bytes(&mut self, data: &[u8]) -> (Vec<Event>, Vec<Vec<u8>>) {
        self.reader.push(data);
        let mut events = Vec::new();
        let mut out = Vec::new();
        while let Some(raw) = self.reader.next_frame() {
            match Message::decode(&raw) {
                Ok(m) => {
                    events.push(Event::Frame(m.clone()));
                    self.handle(m, &mut events, &mut out);
                }
                Err(_) => continue,
            }
        }
        (events, out)
    }

    fn handle(&mut self, m: Message, ev: &mut Vec<Event>, out: &mut Vec<Vec<u8>>) {
        if m.kind == MessageType::Response && m.status != 0 {
            ev.push(Event::Nack { opcode: m.opcode, seq: m.seq, status: m.status });
            return;
        }
        match m.opcode {
            Opcode::AuthChallenge => {
                if m.kind == MessageType::Response {
                    // Earbuds answered our challenge (not verified, as in Gadgetbridge).
                    out.push(self.request(Opcode::AuthConfirm, [0x01, 0x00]));
                } else if m.payload.len() >= 17 {
                    let mut ch = [0u8; 16];
                    ch.copy_from_slice(&m.payload[1..17]);
                    let r = auth::challenge_response(&ch);
                    let mut p = Vec::with_capacity(17);
                    p.push(0x01);
                    p.extend_from_slice(&r);
                    out.push(Message::new(MessageType::Response, Opcode::AuthChallenge, m.seq, p).encode());
                }
            }
            Opcode::AuthConfirm => {
                if m.kind != MessageType::Response {
                    out.push(Message::new(MessageType::Response, Opcode::AuthConfirm, m.seq, [0x01]).encode());
                    self.authenticated = true;
                    ev.push(Event::Authenticated);
                    out.push(self.encode_get_device_info());
                    out.push(self.encode_get_run_info());
                    out.push(self.encode_get_config(config::GESTURES));
                }
            }
            Opcode::GetDeviceInfo => decode_device_info(&m.payload, ev),
            Opcode::GetDeviceRunInfo => decode_run_info(&m.payload, ev),
            Opcode::ReportStatus => {
                decode_report_status(&m.payload, ev);
                out.push(Message::new(MessageType::Response, Opcode::ReportStatus, m.seq, []).encode());
            }
            Opcode::GetConfig => {
                if !decode_get_config(&m.payload, ev) {
                    ev.push(Event::Unhandled(m));
                }
            }
            Opcode::NotifyConfig => {
                decode_notify_config(&m.payload, ev);
                out.push(Message::new(MessageType::Response, Opcode::NotifyConfig, m.seq, []).encode());
            }
            _ => ev.push(Event::Unhandled(m)),
        }
    }
}

/// Iterates TLV entries `len, index, data[len-1]` (len covers index + data).
fn tlv(payload: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut i = 0;
    std::iter::from_fn(move || {
        if i + 1 >= payload.len() {
            return None;
        }
        let len = payload[i] as usize;
        let idx = payload[i + 1];
        let end = (i + 1 + len).min(payload.len());
        let data = &payload[(i + 2).min(end)..end];
        i += len + 1;
        Some((idx, data))
    })
}

fn nibbles(a: u8, b: u8) -> String {
    format!("{}.{}.{}.{}", a >> 4, a & 0xF, b >> 4, b & 0xF)
}

fn decode_device_info(p: &[u8], ev: &mut Vec<Event>) {
    let (mut fw, mut vidpid) = ([0u8; 4], [0u8; 4]);
    for (idx, d) in tlv(p) {
        match idx {
            0x01 if d.len() >= 4 => fw.copy_from_slice(&d[..4]),
            0x03 if d.len() >= 4 => vidpid.copy_from_slice(&d[..4]),
            0x07 if d.len() >= 3 => ev.push(Event::Battery(Batteries::from3(d))),
            _ => {}
        }
    }
    ev.push(Event::DeviceInfo {
        fw: nibbles(fw[0], fw[1]),
        fw2: nibbles(fw[2], fw[3]),
        vid: u16::from_be_bytes([vidpid[0], vidpid[1]]),
        pid: u16::from_be_bytes([vidpid[2], vidpid[3]]),
    });
}

fn decode_run_info(p: &[u8], ev: &mut Vec<Event>) {
    for (idx, d) in tlv(p) {
        match (idx, d.first()) {
            (0x09, Some(&m)) => ev.push(Event::Anc(AncMode::from_code(m))),
            (0x0A, Some(&v)) => ev.push(Event::WearDetection(v == 0x00)),
            _ => {}
        }
    }
}

fn decode_report_status(p: &[u8], ev: &mut Vec<Event>) {
    for (idx, d) in tlv(p) {
        match idx {
            0x00 if d.len() >= 3 => ev.push(Event::Battery(Batteries::from3(d))),
            0x04 if !d.is_empty() => ev.push(Event::Anc(AncMode::from_code(d[0]))),
            _ => {}
        }
    }
}

/// Config ids (Gadgetbridge `prefs/Configuration.java`).
pub mod config {
    pub const GESTURES: u8 = 0x02;
    pub const AUTO_ANSWER: u8 = 0x03;
    pub const DOUBLE_CONNECTION: u8 = 0x04;
    pub const EAR_DETECTION: u8 = 0x06;
    pub const EQ_PRESET: u8 = 0x07;
    pub const LONG_GESTURES: u8 = 0x0A;
    pub const EFFECT_STRENGTH: u8 = 0x0B;
}

/// GET_CONFIG response; payload[2] is the config id. Returns false if not decoded.
fn decode_get_config(p: &[u8], ev: &mut Vec<Event>) -> bool {
    match p.get(2) {
        // Entries of (tap type, left action, right action) from [3]. On the 3 Pro:
        // double = [3..6], triple = [6..9], long = [9..12].
        Some(&config::GESTURES) => {
            let mut i = 3;
            while i + 2 < p.len() {
                if let Some(tap) = TapType::from_code(p[i]) {
                    ev.push(Event::Gesture { tap, left: p[i + 1], right: p[i + 2] });
                }
                i += 3;
            }
            true
        }
        _ => false,
    }
}

fn decode_notify_config(p: &[u8], ev: &mut Vec<Event>) {
    // Entries: len, 0x00, config id, data...
    let mut i = 0;
    while i + 2 < p.len() {
        let len = p[i] as usize;
        if p[i + 2] == 0x0B && i + 3 < p.len() {
            ev.push(Event::Anc(AncMode::from_code(p[i + 3])));
        }
        if p[i + 2] == 0x0C && i + 3 < p.len() {
            ev.push(Event::InEar(p[i + 3]));
        }
        i += len + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(kind: MessageType, op: Opcode, seq: u8, p: &[u8]) -> Vec<u8> {
        Message::new(kind, op, seq, p.to_vec()).encode()
    }

    #[test]
    fn start_frame_layout() {
        let mut s = Session::new();
        let f = s.start_with_challenge(&[0xAA; 16]);
        let m = Message::decode(&f).unwrap();
        assert_eq!(m.kind, MessageType::PhoneRequest);
        assert_eq!(m.opcode, Opcode::AuthChallenge);
        assert_eq!(m.seq, 0);
        assert_eq!(m.payload.len(), 17);
        assert_eq!(m.payload[0], 1);
        assert_eq!(&f[..8], &[0xFE, 0xDC, 0xBA, 0xC4, 0x50, 0x00, 0x12, 0x00]);
    }

    #[test]
    fn full_handshake() {
        let mut s = Session::new();
        s.start_with_challenge(&[0; 16]); // seq 0

        // Earbuds answer our challenge -> we send AUTH_CONFIRM request (seq 1).
        let (_, out) = s.on_bytes(&frame(MessageType::Response, Opcode::AuthChallenge, 0, &[1; 17]));
        let m = Message::decode(&out[0]).unwrap();
        assert_eq!((m.kind, m.opcode, m.seq, m.payload.as_slice()), (MessageType::PhoneRequest, Opcode::AuthConfirm, 1, &[1u8, 0][..]));

        // Earbuds challenge us -> we answer with the cipher output, echoing seq.
        let mut p = vec![0x01];
        p.extend([7u8; 16]);
        let (_, out) = s.on_bytes(&frame(MessageType::EarbudsRequest, Opcode::AuthChallenge, 0x42, &p));
        let m = Message::decode(&out[0]).unwrap();
        assert_eq!((m.kind, m.opcode, m.seq), (MessageType::Response, Opcode::AuthChallenge, 0x42));
        assert_eq!(&m.payload[1..], &auth::challenge_response(&[7; 16]));

        // Earbuds confirm -> ack + device info + run info requests.
        let (ev, out) = s.on_bytes(&frame(MessageType::EarbudsRequest, Opcode::AuthConfirm, 0x43, &[0x01]));
        assert!(ev.contains(&Event::Authenticated));
        assert!(s.is_authenticated());
        let ops: Vec<_> = out.iter().map(|f| Message::decode(f).unwrap().opcode).collect();
        assert_eq!(ops, [Opcode::AuthConfirm, Opcode::GetDeviceInfo, Opcode::GetDeviceRunInfo, Opcode::GetConfig]);
    }

    #[test]
    fn device_info_battery_and_version() {
        let mut s = Session::new();
        // TLVs: fw (01) 1.2.3.4 / 5.6.7.8, vid/pid (03) 2717:5035, battery (07) L=80 charging, R=75, case n/a
        let p = [5, 0x01, 0x12, 0x34, 0x56, 0x78, 5, 0x03, 0x27, 0x17, 0x50, 0x35, 4, 0x07, 0x80 | 80, 75, 0xFF];
        let (ev, _) = s.on_bytes(&frame(MessageType::Response, Opcode::GetDeviceInfo, 2, &p));
        assert!(ev.contains(&Event::Battery(Batteries {
            left: Some(Battery { level: 80, charging: true }),
            right: Some(Battery { level: 75, charging: false }),
            case: None,
        })));
        assert!(ev.contains(&Event::DeviceInfo { fw: "1.2.3.4".into(), fw2: "5.6.7.8".into(), vid: 0x2717, pid: 0x5035 }));
    }

    #[test]
    fn run_info_anc() {
        let mut s = Session::new();
        let p = [2, 0x09, 0x01, 2, 0x0A, 0x00];
        let (ev, _) = s.on_bytes(&frame(MessageType::Response, Opcode::GetDeviceRunInfo, 3, &p));
        assert!(ev.contains(&Event::Anc(AncMode::NoiseCancelling)));
        assert!(ev.contains(&Event::WearDetection(true)));
    }

    #[test]
    fn report_status_is_decoded_and_acked() {
        let mut s = Session::new();
        let p = [4, 0x00, 0x64, 0x5A, 0x80 | 40, 2, 0x04, 0x02];
        let (ev, out) = s.on_bytes(&frame(MessageType::EarbudsNotify, Opcode::ReportStatus, 9, &p));
        assert!(ev.contains(&Event::Battery(Batteries {
            left: Some(Battery { level: 100, charging: false }),
            right: Some(Battery { level: 90, charging: false }),
            case: Some(Battery { level: 40, charging: true }),
        })));
        assert!(ev.contains(&Event::Anc(AncMode::Transparency)));
        assert_eq!(out, vec![frame(MessageType::Response, Opcode::ReportStatus, 9, &[])]);
    }

    #[test]
    fn get_config_gestures_buds3pro_layout() {
        let mut s = Session::new();
        assert_eq!(
            s.encode_get_config(config::GESTURES),
            [0xFE, 0xDC, 0xBA, 0xC4, 0xF3, 0x00, 0x03, 0x00, 0x00, 0x02, 0xEF]
        );
        // Real reply from a Redmi Buds 3 Pro (fw 1.0.9.9):
        //        len   00    id    dbl   dL    dR    tri   tL    tR    long  lL    lR
        let p = [0x0B, 0x00, 0x02, 0x01, 0x01, 0x01, 0x02, 0x03, 0x03, 0x03, 0x06, 0x06];
        let (ev, out) = s.on_bytes(&frame(MessageType::Response, Opcode::GetConfig, 1, &p));
        assert!(ev.contains(&Event::Gesture { tap: TapType::Double, left: 1, right: 1 }));
        assert!(ev.contains(&Event::Gesture { tap: TapType::Triple, left: 3, right: 3 }));
        assert!(ev.contains(&Event::Gesture { tap: TapType::Long, left: 6, right: 6 }));
        assert!(out.is_empty());
    }

    #[test]
    fn set_gesture_and_wear_detection_frames() {
        let mut s = Session::new();
        assert_eq!(
            s.encode_set_gesture(TapType::Triple, None, Some(0x02)),
            [0xFE, 0xDC, 0xBA, 0xC4, 0xF2, 0x00, 0x07, 0x00, 0x05, 0x00, 0x02, 0x02, 0xFF, 0x02, 0xEF]
        );
        assert_eq!(
            s.encode_set_wear_detection(false),
            [0xFE, 0xDC, 0xBA, 0xC4, 0x08, 0x00, 0x04, 0x01, 0x02, 0x06, 0x01, 0xEF]
        );
    }

    #[test]
    fn in_ear_notify() {
        let mut s = Session::new();
        // Real frame: one earbud worn.
        let (ev, out) = s.on_bytes(&[0xFE, 0xDC, 0xBA, 0xC7, 0xF4, 0x00, 0x05, 0x02, 0x03, 0x00, 0x0C, 0x08, 0xEF]);
        assert!(ev.contains(&Event::InEar(0x08)));
        assert_eq!(out, vec![frame(MessageType::Response, Opcode::NotifyConfig, 2, &[])]);
    }

    #[test]
    fn rejected_set_is_reported_as_nack() {
        let mut s = Session::new();
        let (ev, out) = s.on_bytes(&[0xFE, 0xDC, 0xBA, 0x04, 0x08, 0x00, 0x02, 0x09, 0x05, 0xEF]);
        assert!(ev.contains(&Event::Nack { opcode: Opcode::Anc, seq: 5, status: 9 }));
        assert!(out.is_empty());
    }

    #[test]
    fn set_anc_frame() {
        let mut s = Session::new();
        assert_eq!(
            s.encode_set_anc(AncMode::Off),
            [0xFE, 0xDC, 0xBA, 0xC4, 0x08, 0x00, 0x04, 0x00, 0x02, 0x04, 0x00, 0xEF]
        );
    }
}
