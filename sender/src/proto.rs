//! LRT mirror protocol v0 message layer (see `mirror/PROTOCOL.md`).
//!
//! 20-byte little-endian header: magic `LRT0`, type u8, flags u8, reserved u16 (0),
//! `t_us` u64, len u32 (max 8 MiB), then `len` payload bytes.
//! Independent of the socket type so a UDP transport can replace TCP later.

use std::io::{self, Read, Write};

pub const MAGIC: [u8; 4] = *b"LRT0";
pub const HDR_LEN: usize = 20;
pub const MAX_LEN: u32 = 8 * 1024 * 1024;

pub const T_HELLO: u8 = 0;
pub const T_CONFIG: u8 = 1;
pub const T_FRAME: u8 = 2;
pub const T_PING: u8 = 3;
pub const T_PONG: u8 = 4;
pub const T_STATS: u8 = 5;
pub const T_IDR_REQUEST: u8 = 6;
pub const T_BYE: u8 = 7;
/// T->S, UTF-8 JSON once per connection: the TV's video decoders.
pub const T_CAPS: u8 = 8;
/// S->T over TCP, UTF-8 JSON: audio starts (format, session id). Only sent to a TV whose CAPS lists `audio`.
pub const T_AUDIO_START: u8 = 9;
/// S->T over TCP, empty: audio stops (the TV stops playing and frees its audio output).
pub const T_AUDIO_STOP: u8 = 10;
/// S->T over UDP only: one audio datagram (`sid` u32, `pos` u64, PCM). `t_us` = capture time of its first frame.
pub const T_AUDIO: u8 = 11;

pub const FLAG_KEY: u8 = 1;

pub fn type_name(t: u8) -> &'static str {
    match t {
        T_HELLO => "HELLO",
        T_CONFIG => "CONFIG",
        T_FRAME => "FRAME",
        T_PING => "PING",
        T_PONG => "PONG",
        T_STATS => "STATS",
        T_IDR_REQUEST => "IDR_REQUEST",
        T_BYE => "BYE",
        T_CAPS => "CAPS",
        T_AUDIO_START => "AUDIO_START",
        T_AUDIO_STOP => "AUDIO_STOP",
        T_AUDIO => "AUDIO",
        _ => "UNKNOWN",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub typ: u8,
    pub flags: u8,
    pub reserved: u16,
    pub t_us: u64,
    pub len: u32,
}

impl Header {
    pub fn encode(&self) -> [u8; HDR_LEN] {
        let mut b = [0u8; HDR_LEN];
        b[0..4].copy_from_slice(&MAGIC);
        b[4] = self.typ;
        b[5] = self.flags;
        b[6..8].copy_from_slice(&self.reserved.to_le_bytes());
        b[8..16].copy_from_slice(&self.t_us.to_le_bytes());
        b[16..20].copy_from_slice(&self.len.to_le_bytes());
        b
    }

    /// Strict decode: bad magic or oversize length is an error.
    ///
    /// # Errors
    ///
    /// `InvalidData` when the magic is not `LRT0` or `len` exceeds [`MAX_LEN`].
    pub fn decode(b: &[u8; HDR_LEN]) -> io::Result<Header> {
        let [m0, m1, m2, m3, typ, flags, r0, r1, t0, t1, t2, t3, t4, t5, t6, t7, l0, l1, l2, l3] = *b;
        if [m0, m1, m2, m3] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad magic {:02x?}", &b[0..4])));
        }
        let h = Header {
            typ,
            flags,
            reserved: u16::from_le_bytes([r0, r1]),
            t_us: u64::from_le_bytes([t0, t1, t2, t3, t4, t5, t6, t7]),
            len: u32::from_le_bytes([l0, l1, l2, l3]),
        };
        if h.len > MAX_LEN {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("len {} > 8 MiB", h.len)));
        }
        Ok(h)
    }
}

/// Build one message (header + concatenated payload parts) into a single buffer,
/// so it can be written with one `write_all` (atomic w.r.t. other writers holding the lock).
///
/// # Panics
///
/// When the payload is larger than [`MAX_LEN`] (a caller bug: encoders never emit that).
pub fn build(typ: u8, flags: u8, t_us: u64, parts: &[&[u8]]) -> Vec<u8> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    let len32 = u32::try_from(len).ok().filter(|&l| l <= MAX_LEN).expect("message too large");
    let mut v = Vec::with_capacity(HDR_LEN + len);
    v.extend_from_slice(&Header { typ, flags, reserved: 0, t_us, len: len32 }.encode());
    for p in parts {
        v.extend_from_slice(p);
    }
    v
}

/// Build one message (see [`build`]) and write it.
///
/// # Errors
///
/// Any I/O error from `w`.
///
/// # Panics
///
/// When the payload is larger than [`MAX_LEN`] (see [`build`]).
pub fn write_msg<W: Write>(w: &mut W, typ: u8, flags: u8, t_us: u64, parts: &[&[u8]]) -> io::Result<()> {
    w.write_all(&build(typ, flags, t_us, parts))
}

/// Read one message. Returns the header and its payload.
///
/// # Errors
///
/// Any I/O error from `r` (including EOF), or `InvalidData` for a bad header.
pub fn read_msg<R: Read>(r: &mut R) -> io::Result<(Header, Vec<u8>)> {
    let mut hb = [0u8; HDR_LEN];
    r.read_exact(&mut hb)?;
    let h = Header::decode(&hb)?;
    let n = usize::try_from(h.len).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "len does not fit usize"))?;
    let mut p = vec![0u8; n];
    r.read_exact(&mut p)?;
    Ok((h, p))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        let m = build(T_FRAME, FLAG_KEY, 123_456_789, &[b"ab", b"cde"]);
        assert_eq!(m.len(), 25);
        assert_eq!(&m[0..4], b"LRT0");
        let (h, p) = read_msg(&mut &m[..]).unwrap();
        assert_eq!(h, Header { typ: T_FRAME, flags: 1, reserved: 0, t_us: 123_456_789, len: 5 });
        assert_eq!(p, b"abcde");
    }
    #[test]
    fn bad_magic() {
        let mut m = build(T_PING, 0, 1, &[&[0; 8]]);
        m[0] = b'X';
        assert!(read_msg(&mut &m[..]).is_err());
    }
}
