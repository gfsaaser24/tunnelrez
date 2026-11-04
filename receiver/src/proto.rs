//! TunnelRez wire framing, magic `LRT0` (see `PROTOCOL.md`): 20-byte header + payload.

use std::fmt;

/// TCP port the TV listens on.
pub(crate) const PORT: u16 = 7878;
const MAGIC: &[u8; 4] = b"LRT0";
/// Largest payload accepted (PROTOCOL.md: max 8 MiB).
const MAX_LEN: u32 = 8 * 1024 * 1024;
/// Header size in bytes.
pub(crate) const HEADER_LEN: usize = 20;
/// FRAME flag bit 0: keyframe / IDR.
pub(crate) const FLAG_KEY: u8 = 1;

pub(crate) const T_HELLO: u8 = 0;
pub(crate) const T_CONFIG: u8 = 1;
pub(crate) const T_FRAME: u8 = 2;
pub(crate) const T_PING: u8 = 3;
pub(crate) const T_PONG: u8 = 4;
pub(crate) const T_STATS: u8 = 5;
pub(crate) const T_IDR_REQUEST: u8 = 6;
pub(crate) const T_BYE: u8 = 7;
/// TV -> PC, once per connection: the TV's video decoders (JSON, see `caps.rs`).
pub(crate) const T_CAPS: u8 = 8;
/// PC -> TV (TCP), JSON: audio starts (format + session id). See `jitter.rs`.
pub(crate) const T_AUDIO_START: u8 = 9;
/// PC -> TV (TCP), empty: audio stops.
pub(crate) const T_AUDIO_STOP: u8 = 10;
/// PC -> TV, UDP only: one 5 ms audio datagram.
pub(crate) const T_AUDIO: u8 = 11;

/// A parsed message header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub(crate) typ: u8,
    pub(crate) flags: u8,
    pub(crate) t_us: u64,
    pub(crate) len: u32,
}

impl Header {
    /// Payload length in bytes.
    pub(crate) fn payload_len(self) -> usize {
        usize::try_from(self.len).expect("len <= MAX_LEN (8 MiB) fits in usize")
    }

    pub(crate) fn is_key(self) -> bool {
        self.flags & FLAG_KEY != 0
    }
}

/// Why a header was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HeaderError {
    BadMagic([u8; 4]),
    TooLong(u32),
}

impl fmt::Display for HeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeaderError::BadMagic(m) => write!(f, "bad magic {m:02x?}"),
            HeaderError::TooLong(n) => write!(f, "len {n} > max {MAX_LEN}"),
        }
    }
}

impl std::error::Error for HeaderError {}

/// Parse a 20-byte header.
///
/// # Errors
///
/// Wrong magic, or a payload length above 8 MiB.
pub(crate) fn parse_header(b: &[u8; HEADER_LEN]) -> Result<Header, HeaderError> {
    let magic = [b[0], b[1], b[2], b[3]];
    if &magic != MAGIC {
        return Err(HeaderError::BadMagic(magic));
    }
    let t_us = u64::from_le_bytes([b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]]);
    let len = u32::from_le_bytes([b[16], b[17], b[18], b[19]]);
    let h = Header { typ: b[4], flags: b[5], t_us, len };
    if h.len > MAX_LEN {
        return Err(HeaderError::TooLong(h.len));
    }
    Ok(h)
}

/// Build one message (header + payload).
pub(crate) fn encode(typ: u8, flags: u8, t_us: u64, payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len()).expect("messages sent by the TV are small");
    let mut v = Vec::with_capacity(HEADER_LEN + payload.len());
    v.extend_from_slice(MAGIC);
    v.push(typ);
    v.push(flags);
    v.extend_from_slice(&[0, 0]);
    v.extend_from_slice(&t_us.to_le_bytes());
    v.extend_from_slice(&len.to_le_bytes());
    v.extend_from_slice(payload);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let msg = encode(T_FRAME, FLAG_KEY, 123_456_789, &[1, 2, 3]);
        let hdr: [u8; HEADER_LEN] = msg[..HEADER_LEN].try_into().unwrap();
        let h = parse_header(&hdr).unwrap();
        assert_eq!(h, Header { typ: T_FRAME, flags: FLAG_KEY, t_us: 123_456_789, len: 3 });
        assert!(h.is_key());
        assert_eq!(h.payload_len(), 3);
        assert_eq!(&msg[HEADER_LEN..], &[1, 2, 3]);
    }

    #[test]
    fn rejects_bad_magic_and_long_len() {
        let mut hdr: [u8; HEADER_LEN] = encode(T_PING, 0, 0, &[])[..].try_into().unwrap();
        hdr[0] = b'X';
        assert!(matches!(parse_header(&hdr), Err(HeaderError::BadMagic(_))));
        let mut hdr: [u8; HEADER_LEN] = encode(T_PING, 0, 0, &[])[..].try_into().unwrap();
        hdr[16..20].copy_from_slice(&(MAX_LEN + 1).to_le_bytes());
        assert_eq!(parse_header(&hdr), Err(HeaderError::TooLong(MAX_LEN + 1)));
    }
}
