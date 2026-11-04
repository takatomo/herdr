//! Framed-mode codec for the herdr-core socket.
//!
//! Every frame is `[channel u32 LE][kind u8][len u32 LE][payload]`, payload at
//! most 16 MiB (herdr-core `schema/README.md`, "Framed mode").

use bytes::{Buf, Bytes, BytesMut};

pub(crate) const HEADER_LEN: usize = 9;
pub(crate) const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameKind {
    Data,
    Credit,
    Close,
    Resize,
    Focus,
    Facts,
    AutomaticInput,
    Unknown(u8),
}

impl FrameKind {
    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Data,
            1 => Self::Credit,
            2 => Self::Close,
            3 => Self::Resize,
            4 => Self::Focus,
            5 => Self::Facts,
            6 => Self::AutomaticInput,
            other => Self::Unknown(other),
        }
    }

    pub(crate) fn as_u8(self) -> u8 {
        match self {
            Self::Data => 0,
            Self::Credit => 1,
            Self::Close => 2,
            Self::Resize => 3,
            Self::Focus => 4,
            Self::Facts => 5,
            Self::AutomaticInput => 6,
            Self::Unknown(value) => value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Frame {
    pub channel: u32,
    pub kind: FrameKind,
    pub payload: Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WireError {
    PayloadTooLarge(usize),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::PayloadTooLarge(len) => {
                write!(f, "core frame payload of {len} bytes exceeds 16 MiB")
            }
        }
    }
}

pub(crate) fn encode_frame(channel: u32, kind: FrameKind, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend_from_slice(&channel.to_le_bytes());
    frame.push(kind.as_u8());
    // Callers never build payloads near 4 GiB; the core caps them at 16 MiB.
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

pub(crate) fn credit_payload(consumed: u32) -> [u8; 4] {
    consumed.to_le_bytes()
}

pub(crate) fn resize_payload(cols: u16, rows: u16) -> [u8; 4] {
    let cols = cols.to_le_bytes();
    let rows = rows.to_le_bytes();
    [cols[0], cols[1], rows[0], rows[1]]
}

pub(crate) fn parse_resize(payload: &[u8]) -> Option<(u16, u16)> {
    let bytes: [u8; 4] = payload.try_into().ok()?;
    Some((
        u16::from_le_bytes([bytes[0], bytes[1]]),
        u16::from_le_bytes([bytes[2], bytes[3]]),
    ))
}

/// Incremental decoder: push bytes as they arrive, take whole frames out.
#[derive(Default)]
pub(crate) struct FrameDecoder {
    buf: BytesMut,
}

impl FrameDecoder {
    pub(crate) fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub(crate) fn next_frame(&mut self) -> Result<Option<Frame>, WireError> {
        if self.buf.len() < HEADER_LEN {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[5], self.buf[6], self.buf[7], self.buf[8]]) as usize;
        if len > MAX_PAYLOAD {
            return Err(WireError::PayloadTooLarge(len));
        }
        if self.buf.len() < HEADER_LEN + len {
            return Ok(None);
        }
        let channel = self.buf.get_u32_le();
        let kind = FrameKind::from_u8(self.buf.get_u8());
        let _ = self.buf.get_u32_le();
        let payload = self.buf.split_to(len).freeze();
        Ok(Some(Frame {
            channel,
            kind,
            payload,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips_through_the_decoder() {
        let encoded = encode_frame(7, FrameKind::Data, b"hello");
        assert_eq!(encoded.len(), HEADER_LEN + 5);
        assert_eq!(&encoded[..4], &7u32.to_le_bytes());
        assert_eq!(encoded[4], 0);
        assert_eq!(&encoded[5..9], &5u32.to_le_bytes());

        let mut decoder = FrameDecoder::default();
        decoder.push(&encoded);
        let frame = decoder.next_frame().unwrap().unwrap();
        assert_eq!(frame.channel, 7);
        assert_eq!(frame.kind, FrameKind::Data);
        assert_eq!(&frame.payload[..], b"hello");
        assert_eq!(decoder.next_frame().unwrap(), None);
    }

    #[test]
    fn decoder_waits_for_split_headers_and_payloads() {
        let mut bytes = encode_frame(1, FrameKind::Close, br#"{"reason":"exited"}"#);
        bytes.extend(encode_frame(2, FrameKind::Credit, &credit_payload(4096)));
        let mut decoder = FrameDecoder::default();
        let mut frames = Vec::new();
        for byte in bytes {
            decoder.push(&[byte]);
            while let Some(frame) = decoder.next_frame().unwrap() {
                frames.push(frame);
            }
        }
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].kind, FrameKind::Close);
        assert_eq!(&frames[0].payload[..], br#"{"reason":"exited"}"#);
        assert_eq!(frames[1].channel, 2);
        assert_eq!(&frames[1].payload[..], &4096u32.to_le_bytes());
    }

    #[test]
    fn decoder_rejects_payloads_over_16_mib_before_buffering_them() {
        let mut header = Vec::new();
        header.extend_from_slice(&3u32.to_le_bytes());
        header.push(0);
        header.extend_from_slice(&((MAX_PAYLOAD + 1) as u32).to_le_bytes());
        let mut decoder = FrameDecoder::default();
        decoder.push(&header);
        assert_eq!(
            decoder.next_frame(),
            Err(WireError::PayloadTooLarge(MAX_PAYLOAD + 1))
        );
    }

    #[test]
    fn kinds_map_to_the_published_numbers_and_keep_unknown_values() {
        let published = [
            (0, FrameKind::Data),
            (1, FrameKind::Credit),
            (2, FrameKind::Close),
            (3, FrameKind::Resize),
            (4, FrameKind::Focus),
            (5, FrameKind::Facts),
            (6, FrameKind::AutomaticInput),
        ];
        for (value, kind) in published {
            assert_eq!(FrameKind::from_u8(value), kind);
            assert_eq!(kind.as_u8(), value);
        }
        assert_eq!(FrameKind::from_u8(42), FrameKind::Unknown(42));
        assert_eq!(FrameKind::Unknown(42).as_u8(), 42);
    }

    #[test]
    fn resize_payload_is_cols_then_rows_little_endian() {
        assert_eq!(resize_payload(200, 120), [0xc8, 0x00, 0x78, 0x00]);
        assert_eq!(parse_resize(&[0xc8, 0x00, 0x78, 0x00]), Some((200, 120)));
        assert_eq!(parse_resize(&[1, 2, 3]), None);
    }
}
