//! Minimal streaming FLV demuxer for ffmpeg's `-f flv` output on a pipe.
//!
//! Why FLV: every tag carries its exact size, so a frame can be forwarded the moment its last
//! byte arrives. Raw Annex-B would only reveal the end of a frame when the next one starts
//! (one frame = 16.7 ms of extra latency).
//!
//! Handles legacy AVC tags and Enhanced-FLV (`avc1`, `hvc1`, `av01`) tags. Converts
//! length-prefixed NAL units to Annex-B and decoder config records to Annex-B parameter sets
//! (H.264/HEVC) or to the AV1 sequence header OBU.

use std::io::{self, Read};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Av1 => "av1",
        }
    }
    pub fn parse(s: &str) -> Option<Codec> {
        match s.to_ascii_lowercase().as_str() {
            "h264" | "avc" => Some(Codec::H264),
            "hevc" | "h265" => Some(Codec::Hevc),
            "av1" => Some(Codec::Av1),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct Tag {
    pub kind: u8,
    pub ts_ms: u32,
    pub data: Vec<u8>,
}

#[derive(Debug)]
pub struct FlvReader<R: Read> {
    r: R,
    header_done: bool,
}

impl<R: Read> FlvReader<R> {
    pub fn new(r: R) -> Self {
        FlvReader { r, header_done: false }
    }

    pub fn into_inner(self) -> R {
        self.r
    }

    /// Next tag, or Ok(None) at clean EOF.
    ///
    /// # Errors
    ///
    /// Any read error, a truncated stream, or `InvalidData` when the stream is not FLV.
    pub fn next_tag(&mut self) -> io::Result<Option<Tag>> {
        if !self.header_done {
            let mut h = [0u8; 9];
            if !read_full_or_eof(&mut self.r, &mut h)? {
                return Ok(None);
            }
            if &h[0..3] != b"FLV" {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "not FLV"));
            }
            let off = to_usize(u32::from_be_bytes([h[5], h[6], h[7], h[8]]))?;
            if off > 9 {
                let mut skip = vec![0u8; off - 9];
                self.r.read_exact(&mut skip)?;
            }
            let mut pts0 = [0u8; 4];
            self.r.read_exact(&mut pts0)?;
            self.header_done = true;
        }
        let mut th = [0u8; 11];
        if !read_full_or_eof(&mut self.r, &mut th)? {
            return Ok(None);
        }
        let kind = th[0] & 0x1f;
        let size = to_usize(u32::from_be_bytes([0, th[1], th[2], th[3]]))?;
        let ts = u32::from_be_bytes([th[7], th[4], th[5], th[6]]);
        let mut data = vec![0u8; size];
        self.r.read_exact(&mut data)?;
        let mut prev = [0u8; 4];
        self.r.read_exact(&mut prev)?;
        Ok(Some(Tag { kind, ts_ms: ts, data }))
    }
}

/// A u32 length/offset from the stream as `usize` (always fits on the 64-bit targets we build).
fn to_usize(n: u32) -> io::Result<usize> {
    usize::try_from(n).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "FLV length does not fit usize"))
}

fn read_full_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => {
                if n == 0 {
                    return Ok(false);
                }
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated FLV"));
            }
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

#[derive(Debug)]
pub enum Video {
    /// Codec config: Annex-B SPS/PPS(/VPS) or AV1 sequence header OBU.
    Config(Vec<u8>),
    /// One access unit: Annex-B (H.264/HEVC) or AV1 OBUs (no temporal delimiter added).
    Frame {
        key: bool,
        data: Vec<u8>,
    },
    Other,
}

/// Parses video tags; remembers the NAL length size from the config record.
#[derive(Debug)]
pub struct VideoParser {
    pub codec: Codec,
    nal_len_size: usize,
    /// Frame payload format, known once a config record was seen: Some(false) = length-prefixed
    /// (avcC/hvcC record), Some(true) = Annex-B (no record, e.g. Media Foundation).
    annexb: Option<bool>,
}

const START: [u8; 4] = [0, 0, 0, 1];

impl VideoParser {
    pub fn new(codec: Codec) -> Self {
        VideoParser { codec, nal_len_size: 4, annexb: None }
    }

    /// Parse one FLV video tag body.
    ///
    /// # Errors
    ///
    /// `InvalidData` for a truncated tag or a malformed config record / NAL length.
    pub fn parse(&mut self, d: &[u8]) -> io::Result<Video> {
        if d.is_empty() {
            return Ok(Video::Other);
        }
        let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
        let b0 = d[0];
        let (frame_type, pkt_type, body): (u8, u8, &[u8]);
        if b0 & 0x80 != 0 {
            // Enhanced FLV: [IsEx|FrameType(3)|PacketType(4)] FourCC(4) ...
            if d.len() < 5 {
                return Err(bad("short ex video tag"));
            }
            frame_type = (b0 >> 4) & 7;
            let pt = b0 & 0x0f;
            let fourcc = &d[1..5];
            let mut rest = &d[5..];
            match fourcc {
                b"avc1" | b"hvc1" => {
                    if pt == 1 {
                        if rest.len() < 3 {
                            return Err(bad("short coded frames"));
                        }
                        rest = &rest[3..]; // composition time
                    }
                }
                b"av01" => {}
                _ => return Ok(Video::Other),
            }
            // Map to legacy-style packet types: 0 config, 1 frame, 2 end.
            pkt_type = match pt {
                0 => 0,
                1 | 3 => 1,
                2 => 2,
                _ => return Ok(Video::Other),
            };
            body = rest;
        } else {
            // Legacy: [FrameType(4)|CodecID(4)] AVCPacketType(1) CTS(3) ...
            frame_type = b0 >> 4;
            let codec_id = b0 & 0x0f;
            if codec_id != 7 && codec_id != 12 {
                return Ok(Video::Other);
            }
            if d.len() < 5 {
                return Err(bad("short avc tag"));
            }
            pkt_type = d[1];
            body = &d[5..];
        }
        match pkt_type {
            0 => Ok(Video::Config(match self.codec {
                Codec::H264 => self.avcc_to_annexb(body)?,
                Codec::Hevc => self.hvcc_to_annexb(body)?,
                Codec::Av1 => av1c_config_obus(body),
            })),
            1 => {
                let key = frame_type == 1;
                let data = match self.codec {
                    Codec::Av1 => body.to_vec(),
                    _ => self.lp_to_annexb(body)?,
                };
                Ok(Video::Frame { key, data })
            }
            _ => Ok(Video::Other),
        }
    }

    fn lp_to_annexb(&self, mut b: &[u8]) -> io::Result<Vec<u8>> {
        // Encoders without global headers (Media Foundation) give the muxer Annex-B packets and
        // no config record, and the FLV muxer passes them through. A length-prefixed NAL cannot
        // be 1 byte long, so before any config record a leading start code means Annex-B. Once a
        // record told us the format, trust it: a 256..511-byte NAL with a 4-byte length starts
        // with 00 00 01 too.
        let annexb = self.annexb.unwrap_or_else(|| b.starts_with(&[0, 0, 0, 1]) || b.starts_with(&[0, 0, 1]));
        if annexb {
            return Ok(b.to_vec());
        }
        let n = self.nal_len_size;
        let mut out = Vec::with_capacity(b.len() + 16);
        while !b.is_empty() {
            if b.len() < n {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "short NAL length"));
            }
            let mut len = 0usize;
            for &x in &b[..n] {
                len = (len << 8) | usize::from(x);
            }
            b = &b[n..];
            if len > b.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "NAL overruns tag"));
            }
            out.extend_from_slice(&START);
            out.extend_from_slice(&b[..len]);
            b = &b[len..];
        }
        Ok(out)
    }

    fn avcc_to_annexb(&mut self, c: &[u8]) -> io::Result<Vec<u8>> {
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "bad avcC");
        if c.len() < 7 {
            return Ok(Vec::new()); // no config record yet (parameter sets come in-band)
        }
        if c[0] != 1 {
            self.annexb = Some(true);
            return Ok(c.to_vec()); // already Annex-B
        }
        self.annexb = Some(false);
        self.nal_len_size = usize::from(c[4] & 3) + 1;
        let mut out = Vec::new();
        let mut p = 5;
        let sps_count = usize::from(c[p] & 0x1f);
        p += 1;
        for _ in 0..sps_count {
            let len = usize::from(u16::from_be_bytes([*c.get(p).ok_or_else(bad)?, *c.get(p + 1).ok_or_else(bad)?]));
            p += 2;
            out.extend_from_slice(&START);
            out.extend_from_slice(c.get(p..p + len).ok_or_else(bad)?);
            p += len;
        }
        let pps_count = usize::from(*c.get(p).ok_or_else(bad)?);
        p += 1;
        for _ in 0..pps_count {
            let len = usize::from(u16::from_be_bytes([*c.get(p).ok_or_else(bad)?, *c.get(p + 1).ok_or_else(bad)?]));
            p += 2;
            out.extend_from_slice(&START);
            out.extend_from_slice(c.get(p..p + len).ok_or_else(bad)?);
            p += len;
        }
        Ok(out)
    }

    fn hvcc_to_annexb(&mut self, c: &[u8]) -> io::Result<Vec<u8>> {
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "bad hvcC");
        if c.len() < 23 {
            return Ok(Vec::new()); // no config record yet (parameter sets come in-band)
        }
        if c[0] != 1 {
            self.annexb = Some(true);
            return Ok(c.to_vec()); // already Annex-B
        }
        self.annexb = Some(false);
        self.nal_len_size = usize::from(c[21] & 3) + 1;
        let narr = usize::from(c[22]);
        let mut p = 23;
        // Order VPS, SPS, PPS regardless of record order.
        let mut by_type: Vec<(u8, Vec<u8>)> = Vec::new();
        for _ in 0..narr {
            let t = *c.get(p).ok_or_else(bad)? & 0x3f;
            let count =
                usize::from(u16::from_be_bytes([*c.get(p + 1).ok_or_else(bad)?, *c.get(p + 2).ok_or_else(bad)?]));
            p += 3;
            for _ in 0..count {
                let len = usize::from(u16::from_be_bytes([*c.get(p).ok_or_else(bad)?, *c.get(p + 1).ok_or_else(bad)?]));
                p += 2;
                by_type.push((t, c.get(p..p + len).ok_or_else(bad)?.to_vec()));
                p += len;
            }
        }
        by_type.sort_by_key(|(t, _)| *t); // 32 VPS, 33 SPS, 34 PPS, 39/40 SEI
        let mut out = Vec::new();
        for (t, nal) in by_type {
            if (32..=34).contains(&t) {
                out.extend_from_slice(&START);
                out.extend_from_slice(&nal);
            }
        }
        Ok(out)
    }
}

/// av1C: 4 bytes of fields, then configOBUs (normally the sequence header OBU).
fn av1c_config_obus(c: &[u8]) -> Vec<u8> {
    if c.len() > 4 {
        c[4..].to_vec()
    } else {
        Vec::new()
    }
}

// ---------- helpers on parsed payloads ----------

/// Iterate Annex-B NAL units (without start codes).
pub fn annexb_nals(b: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= b.len() {
        if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut v = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let mut e = if k + 1 < starts.len() { starts[k + 1] - 3 } else { b.len() };
        while e > s && b[e - 1] == 0 && k + 1 < starts.len() {
            e -= 1;
        }
        v.push(&b[s..e]);
    }
    v
}

/// NAL unit type of one NAL (H.264: 5 bits; HEVC: 6 bits).
pub fn nal_type(codec: Codec, nal: &[u8]) -> u8 {
    match codec {
        Codec::H264 => nal.first().map_or(0, |b| b & 0x1f),
        _ => nal.first().map_or(0, |b| (b >> 1) & 0x3f),
    }
}

/// True if the Annex-B AU already contains SPS (H.264) / VPS+SPS (HEVC).
pub fn has_param_sets(codec: Codec, au: &[u8]) -> bool {
    annexb_nals(au).iter().any(|n| {
        let t = nal_type(codec, n);
        match codec {
            Codec::H264 => t == 7,
            _ => t == 33,
        }
    })
}

/// AV1 OBU (type, total length incl. header) list for a low-overhead OBU buffer.
pub fn av1_obus(mut b: &[u8]) -> Vec<(u8, &[u8])> {
    let mut v = Vec::new();
    while !b.is_empty() {
        let hdr = b[0];
        let t = (hdr >> 3) & 0x0f;
        let ext = hdr & 0x04 != 0;
        let has_size = hdr & 0x02 != 0;
        let mut p = 1 + usize::from(ext);
        if !has_size {
            v.push((t, b));
            break;
        }
        let mut sz = 0usize;
        let mut shift = 0;
        loop {
            let Some(&byte) = b.get(p) else { return v };
            p += 1;
            sz |= usize::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
        }
        let end = (p + sz).min(b.len());
        v.push((t, &b[..end]));
        b = &b[end..];
    }
    v
}

pub const AV1_OBU_SEQ_HDR: u8 = 1;
pub const AV1_OBU_TD: u8 = 2;
pub const AV1_TD: [u8; 2] = [0x12, 0x00];

#[cfg(test)]
mod parser_tests {
    use super::*;

    #[test]
    fn length_prefixed_nal_of_300_bytes_is_not_taken_for_annex_b() {
        let mut p = VideoParser::new(Codec::H264);
        // avcC: version 1, profile/compat/level, 4-byte lengths, one SPS, one PPS (1 byte each).
        let avcc = [1u8, 0x64, 0, 0x28, 0xff, 0xe1, 0, 1, 0x67, 1, 0, 1, 0x68];
        p.avcc_to_annexb(&avcc).unwrap();
        let mut nal = vec![0x41u8; 300];
        nal[0] = 0x41;
        let mut tag = (300u32).to_be_bytes().to_vec(); // 00 00 01 2c
        tag.extend_from_slice(&nal);
        let out = p.lp_to_annexb(&tag).unwrap();
        assert_eq!(&out[..4], &START);
        assert_eq!(out.len(), 304);
    }
}
