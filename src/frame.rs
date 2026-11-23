// SPDX-License-Identifier: AGPL-3.0-or-later
//! Wire framing (port of Gadgetbridge `protocol/Message.java`).
//!
//! ```text
//! FE DC BA | type | opcode | len (u16 BE) | [00 if !request] | seq | payload | EF
//! ```
//! `len` = payload length + 1 (request) or + 2 (non-request: extra status byte,
//! `00` = OK; the earbuds answer a rejected request with a non-zero status, e.g. `09`).
//! The whole frame is therefore always `len + 8` bytes long.

pub const HEADER: [u8; 3] = [0xFE, 0xDC, 0xBA];
pub const TRAILER: u8 = 0xEF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    PhoneRequest,
    Response,
    EarbudsRequest,
    EarbudsNotify,
    Unknown(u8),
}

impl MessageType {
    pub fn code(self) -> u8 {
        match self {
            Self::PhoneRequest => 0xC4,
            Self::Response => 0x04,
            Self::EarbudsRequest => 0xC0,
            Self::EarbudsNotify => 0xC7,
            Self::Unknown(c) => c,
        }
    }

    pub fn from_code(c: u8) -> Self {
        match c {
            0xC4 => Self::PhoneRequest,
            0x04 => Self::Response,
            0xC0 => Self::EarbudsRequest,
            0xC7 => Self::EarbudsNotify,
            c => Self::Unknown(c),
        }
    }

    /// Requests have bit 0x40 set and carry no extra `00` byte before `seq`.
    pub fn is_request(self) -> bool {
        self.code() & 0x40 != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    GetDeviceInfo,
    Anc,
    GetDeviceRunInfo,
    ReportStatus,
    AuthChallenge,
    AuthConfirm,
    SetConfig,
    GetConfig,
    NotifyConfig,
    Unknown(u8),
}

impl Opcode {
    pub fn code(self) -> u8 {
        match self {
            Self::GetDeviceInfo => 0x02,
            Self::Anc => 0x08,
            Self::GetDeviceRunInfo => 0x09,
            Self::ReportStatus => 0x0E,
            Self::AuthChallenge => 0x50,
            Self::AuthConfirm => 0x51,
            Self::SetConfig => 0xF2,
            Self::GetConfig => 0xF3,
            Self::NotifyConfig => 0xF4,
            Self::Unknown(c) => c,
        }
    }

    pub fn from_code(c: u8) -> Self {
        match c {
            0x02 => Self::GetDeviceInfo,
            0x08 => Self::Anc,
            0x09 => Self::GetDeviceRunInfo,
            0x0E => Self::ReportStatus,
            0x50 => Self::AuthChallenge,
            0x51 => Self::AuthConfirm,
            0xF2 => Self::SetConfig,
            0xF3 => Self::GetConfig,
            0xF4 => Self::NotifyConfig,
            c => Self::Unknown(c),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub kind: MessageType,
    pub opcode: Opcode,
    pub seq: u8,
    /// Status byte of non-request frames (0 = OK). Always 0 for requests.
    pub status: u8,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    BadHeader,
    BadTrailer,
    BadLength,
}

impl Message {
    pub fn new(kind: MessageType, opcode: Opcode, seq: u8, payload: impl Into<Vec<u8>>) -> Self {
        Self { kind, opcode, seq, status: 0, payload: payload.into() }
    }

    pub fn encode(&self) -> Vec<u8> {
        let extra = if self.kind.is_request() { 1 } else { 2 };
        let len = (self.payload.len() + extra) as u16;
        let mut out = Vec::with_capacity(self.payload.len() + 8 + extra);
        out.extend_from_slice(&HEADER);
        out.push(self.kind.code());
        out.push(self.opcode.code());
        out.extend_from_slice(&len.to_be_bytes());
        if !self.kind.is_request() {
            out.push(self.status);
        }
        out.push(self.seq);
        out.extend_from_slice(&self.payload);
        out.push(TRAILER);
        out
    }

    /// Decodes exactly one complete frame.
    pub fn decode(frame: &[u8]) -> Result<Self, FrameError> {
        if frame.len() < 9 || frame[..3] != HEADER {
            return Err(FrameError::BadHeader);
        }
        let len = u16::from_be_bytes([frame[5], frame[6]]) as usize;
        if frame.len() != len + 8 {
            return Err(FrameError::BadLength);
        }
        if frame[frame.len() - 1] != TRAILER {
            return Err(FrameError::BadTrailer);
        }
        let kind = MessageType::from_code(frame[3]);
        let opcode = Opcode::from_code(frame[4]);
        let payload_off = if kind.is_request() { 8 } else { 9 };
        if payload_off > frame.len() - 1 {
            return Err(FrameError::BadLength);
        }
        Ok(Self {
            kind,
            opcode,
            seq: frame[payload_off - 1],
            status: if kind.is_request() { 0 } else { frame[7] },
            payload: frame[payload_off..frame.len() - 1].to_vec(),
        })
    }
}

/// Reassembles frames from a byte stream. Several frames may arrive in one read
/// and a frame may be split over reads; junk before a header is skipped.
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Returns the next complete raw frame, if any.
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        loop {
            // Resync on header.
            match self.buf.windows(3).position(|w| w == HEADER) {
                Some(0) => {}
                Some(p) => {
                    self.buf.drain(..p);
                }
                None => {
                    // Keep a possible partial header at the end.
                    let keep = self.buf.len().min(2);
                    let cut = self.buf.len() - keep;
                    self.buf.drain(..cut);
                    return None;
                }
            }
            if self.buf.len() < 7 {
                return None;
            }
            let total = u16::from_be_bytes([self.buf[5], self.buf[6]]) as usize + 8;
            if self.buf.len() < total {
                return None;
            }
            if self.buf[total - 1] != TRAILER {
                // Not a real frame start; skip this header and resync.
                self.buf.drain(..1);
                continue;
            }
            return Some(self.buf.drain(..total).collect());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_request() {
        // GET_DEVICE_INFO request as sent by Gadgetbridge after auth.
        let m = Message::new(MessageType::PhoneRequest, Opcode::GetDeviceInfo, 3, [0xFF; 4]);
        assert_eq!(
            m.encode(),
            [0xFE, 0xDC, 0xBA, 0xC4, 0x02, 0x00, 0x05, 0x03, 0xFF, 0xFF, 0xFF, 0xFF, 0xEF]
        );
    }

    #[test]
    fn encode_response_has_extra_zero() {
        let m = Message::new(MessageType::Response, Opcode::AuthConfirm, 7, [0x01]);
        assert_eq!(m.encode(), [0xFE, 0xDC, 0xBA, 0x04, 0x51, 0x00, 0x03, 0x00, 0x07, 0x01, 0xEF]);
        // Empty ack (REPORT_STATUS response).
        let m = Message::new(MessageType::Response, Opcode::ReportStatus, 0x12, []);
        assert_eq!(m.encode(), [0xFE, 0xDC, 0xBA, 0x04, 0x0E, 0x00, 0x02, 0x00, 0x12, 0xEF]);
    }

    #[test]
    fn roundtrip_all_types() {
        for kind in [
            MessageType::PhoneRequest,
            MessageType::Response,
            MessageType::EarbudsRequest,
            MessageType::EarbudsNotify,
        ] {
            let m = Message::new(kind, Opcode::NotifyConfig, 0xAB, vec![1, 2, 3, 0xFE, 0xDC, 0xBA]);
            assert_eq!(Message::decode(&m.encode()).unwrap(), m);
        }
    }

    #[test]
    fn decode_response_status() {
        // Real reply from Redmi Buds 3 Pro rejecting an ANC SET while the phone was playing.
        let m = Message::decode(&[0xFE, 0xDC, 0xBA, 0x04, 0x08, 0x00, 0x02, 0x09, 0x05, 0xEF]).unwrap();
        assert_eq!((m.kind, m.opcode, m.status, m.seq), (MessageType::Response, Opcode::Anc, 0x09, 0x05));
        assert!(m.payload.is_empty());
        // Accepted one.
        let m = Message::decode(&[0xFE, 0xDC, 0xBA, 0x04, 0x08, 0x00, 0x02, 0x00, 0x09, 0xEF]).unwrap();
        assert_eq!((m.status, m.seq), (0x00, 0x09));
    }

    #[test]
    fn decode_rejects_garbage() {
        assert_eq!(Message::decode(&[0; 10]), Err(FrameError::BadHeader));
        let mut f = Message::new(MessageType::Response, Opcode::Anc, 0, [1]).encode();
        *f.last_mut().unwrap() = 0;
        assert_eq!(Message::decode(&f), Err(FrameError::BadTrailer));
    }

    #[test]
    fn reader_splits_piggybacked_and_partial() {
        let a = Message::new(MessageType::EarbudsNotify, Opcode::ReportStatus, 1, [4, 0, 0x64, 0x5A, 0xFF]).encode();
        let b = Message::new(MessageType::Response, Opcode::GetConfig, 2, [0x00, 0x0B]).encode();
        let mut stream = vec![0x00, 0x11];
        stream.extend(&a);
        stream.extend(&b);
        let mut r = FrameReader::new();
        // Feed in awkward chunks.
        let (x, y) = stream.split_at(a.len() + 4);
        r.push(x);
        assert_eq!(r.next_frame().unwrap(), a);
        assert!(r.next_frame().is_none());
        r.push(y);
        assert_eq!(r.next_frame().unwrap(), b);
        assert!(r.next_frame().is_none());
    }
}
