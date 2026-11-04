//! AV1 bitstream helpers for the decoder input: OBU scanning, temporal unit -> MediaCodec sample, and the
//! codec config record (`av1C`) built from the sequence header.
//!
//! The sender sends each FRAME as a low-overhead temporal unit (AV1 spec section 5): a temporal delimiter,
//! the sequence header on keyframes, then the frame OBU(s). Android feeds AV1 decoders the way MP4 and
//! Matroska store it (ISOBMFF AV1 binding, section 2.4): no temporal delimiter, padding or tile list OBUs in a
//! sample, and `csd-0` = the `AV1CodecConfigurationRecord`. The MediaTek decoder on the TV outputs an extra
//! picture for each temporal delimiter it is given and then stops taking input, so the receiver converts
//! every temporal unit to that sample form before queueing it.

/// OBU type of a sequence header.
const OBU_SEQUENCE_HEADER: u8 = 1;
/// OBU type of a temporal delimiter.
const OBU_TEMPORAL_DELIMITER: u8 = 2;
/// OBU type of a tile list (large-scale tile decoding only).
const OBU_TILE_LIST: u8 = 8;
/// OBU type of padding.
const OBU_PADDING: u8 = 15;

/// One OBU inside a buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Obu {
    typ: u8,
    /// Offset of the OBU header.
    start: usize,
    /// Offset of the payload (after the header, the extension byte and the size field).
    payload: usize,
    /// One past the last payload byte.
    end: usize,
}

/// Every complete OBU, in order. Stops at a truncated OBU (the rest is ignored). An OBU without
/// `obu_has_size_field` runs to the end of the buffer.
fn obus(data: &[u8]) -> Vec<Obu> {
    let mut v = Vec::new();
    let mut i = 0usize;
    while let Some(&h) = data.get(i) {
        let typ = (h >> 3) & 0x0f;
        let ext = usize::from((h >> 2) & 1);
        let has_size = (h >> 1) & 1 == 1;
        let mut p = i + 1 + ext;
        if p > data.len() {
            break;
        }
        if !has_size {
            v.push(Obu { typ, start: i, payload: p, end: data.len() });
            break;
        }
        let Some((size, n)) = leb128(&data[p..]) else { break };
        p += n;
        let Some(end) = usize::try_from(size).ok().and_then(|s| p.checked_add(s)) else {
            break;
        };
        if end > data.len() {
            break;
        }
        v.push(Obu { typ, start: i, payload: p, end });
        i = end;
    }
    v
}

/// A leb128 value and its length in bytes (at most 8 bytes, as the AV1 spec allows).
fn leb128(b: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, &byte) in b.iter().enumerate().take(8) {
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// The first sequence header OBU (header included) in `data`, e.g. the sender's CONFIG payload.
pub(crate) fn seq_header_obu(data: &[u8]) -> Option<&[u8]> {
    obus(data).into_iter().find(|o| o.typ == OBU_SEQUENCE_HEADER).map(|o| &data[o.start..o.end])
}

/// What [`sample_parts`] changed, for logs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SampleInfo {
    /// Temporal delimiter, padding and tile list OBUs left out.
    pub(crate) stripped: usize,
    /// A sequence header was put in front (keyframe without one).
    pub(crate) seq_added: bool,
}

/// The temporal unit `tu` as a MediaCodec sample, as slices to copy in order: without temporal delimiter,
/// padding and tile list OBUs, and with `seq` (a sequence header OBU) in front when it is given and `tu`
/// has none. Bytes the scan cannot parse (a truncated tail) are kept as they are.
pub(crate) fn sample_parts<'a>(tu: &'a [u8], seq: Option<&'a [u8]>) -> (Vec<&'a [u8]>, SampleInfo) {
    let list = obus(tu);
    let mut info = SampleInfo::default();
    let mut parts: Vec<&[u8]> = Vec::with_capacity(4);
    if let Some(s) = seq {
        if !list.iter().any(|o| o.typ == OBU_SEQUENCE_HEADER) {
            parts.push(s);
            info.seq_added = true;
        }
    }
    // Runs of kept OBUs become one slice each.
    let mut run: Option<(usize, usize)> = None;
    for o in &list {
        if matches!(o.typ, OBU_TEMPORAL_DELIMITER | OBU_PADDING | OBU_TILE_LIST) {
            info.stripped += 1;
            if let Some((a, b)) = run.take() {
                parts.push(&tu[a..b]);
            }
            continue;
        }
        run = Some(run.map_or((o.start, o.end), |(a, _)| (a, o.end)));
    }
    let parsed_end = list.last().map_or(0, |o| o.end);
    if let Some((a, b)) = run.take() {
        parts.push(&tu[a..b]);
    }
    if parsed_end < tu.len() {
        parts.push(&tu[parsed_end..]);
    }
    (parts, info)
}

/// Reads bits MSB first (AV1 `f(n)`).
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u8> {
        let byte = *self.data.get(self.pos / 8)?;
        let shift = 7 - u8::try_from(self.pos % 8).expect("bit index < 8");
        self.pos += 1;
        Some((byte >> shift) & 1)
    }

    /// `f(n)` for n <= 32.
    fn f(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | u32::from(self.bit()?);
        }
        Some(v)
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.bit()? == 1)
    }

    /// `uvlc()`.
    fn uvlc(&mut self) -> Option<u32> {
        let mut leading = 0u32;
        while !self.flag()? {
            leading += 1;
            if leading >= 32 {
                return Some(u32::MAX);
            }
        }
        Some(self.f(leading)?.saturating_add((1u32 << leading) - 1))
    }
}

/// The sequence header fields that go into `av1C`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SeqHeader {
    pub(crate) profile: u8,
    /// `seq_level_idx[0]` (e.g. 9 = level 4.1, 13 = level 5.1).
    pub(crate) level_idx: u8,
    pub(crate) tier: u8,
    pub(crate) max_width: u32,
    pub(crate) max_height: u32,
    pub(crate) color: ColorConfig,
}

/// `color_config()` fields that go into `av1C`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ColorConfig {
    /// 8, 10 or 12.
    pub(crate) bit_depth: u8,
    pub(crate) mono_chrome: bool,
    pub(crate) subsampling_x: bool,
    pub(crate) subsampling_y: bool,
    pub(crate) chroma_sample_position: u8,
}

/// Parse the payload of a sequence header OBU (AV1 spec 5.5) up to the color config.
pub(crate) fn parse_seq_header(payload: &[u8]) -> Option<SeqHeader> {
    let mut r = Bits { data: payload, pos: 0 };
    let profile = r.f(3)?;
    let _still_picture = r.flag()?;
    let reduced = r.flag()?;
    let (level_idx, tier) = if reduced { (r.f(5)?, 0) } else { operating_point0(&mut r)? };
    let width_bits = r.f(4)? + 1;
    let height_bits = r.f(4)? + 1;
    let max_width = r.f(width_bits)? + 1;
    let max_height = r.f(height_bits)? + 1;
    if !reduced && r.flag()? {
        // frame_id_numbers_present_flag
        r.f(4)?;
        r.f(3)?;
    }
    r.f(3)?; // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
    if !reduced {
        r.f(4)?; // interintra_compound, masked_compound, warped_motion, dual_filter
        let order_hint = r.flag()?;
        if order_hint {
            r.f(2)?; // jnt_comp, ref_frame_mvs
        }
        let force_screen_content_tools = if r.flag()? { 2 } else { r.f(1)? };
        if force_screen_content_tools > 0 && !r.flag()? {
            r.f(1)?; // seq_force_integer_mv
        }
        if order_hint {
            r.f(3)?;
        }
    }
    r.f(3)?; // enable_superres, enable_cdef, enable_restoration
    let color = color_config(&mut r, profile)?;
    Some(SeqHeader {
        profile: u8::try_from(profile).ok()?,
        level_idx: u8::try_from(level_idx).ok()?,
        tier: u8::try_from(tier).ok()?,
        max_width,
        max_height,
        color,
    })
}

/// Timing / decoder model info and the operating points; returns level and tier of operating point 0.
fn operating_point0(r: &mut Bits<'_>) -> Option<(u32, u32)> {
    let mut decoder_model_info = false;
    let mut buffer_delay_len = 0u32;
    if r.flag()? {
        // timing_info()
        r.f(32)?;
        r.f(32)?;
        if r.flag()? {
            r.uvlc()?;
        }
        decoder_model_info = r.flag()?;
        if decoder_model_info {
            buffer_delay_len = r.f(5)? + 1;
            r.f(32)?;
            r.f(5)?;
            r.f(5)?;
        }
    }
    let initial_display_delay = r.flag()?;
    let op_count = r.f(5)? + 1;
    let mut first = None;
    for _ in 0..op_count {
        r.f(12)?; // operating_point_idc
        let level = r.f(5)?;
        let tier = if level > 7 { r.f(1)? } else { 0 };
        first.get_or_insert((level, tier));
        if decoder_model_info && r.flag()? {
            r.f(buffer_delay_len)?;
            r.f(buffer_delay_len)?;
            r.f(1)?;
        }
        if initial_display_delay && r.flag()? {
            r.f(4)?;
        }
    }
    first
}

/// `color_config()` (AV1 spec 5.5.2).
fn color_config(r: &mut Bits<'_>, profile: u32) -> Option<ColorConfig> {
    let high_bitdepth = r.flag()?;
    let twelve_bit = profile == 2 && high_bitdepth && r.flag()?;
    let bit_depth = match (high_bitdepth, twelve_bit) {
        (_, true) => 12,
        (true, false) => 10,
        (false, false) => 8,
    };
    let mono_chrome = profile != 1 && r.flag()?;
    let (mut cp, mut tc, mut mc) = (2, 2, 2);
    if r.flag()? {
        (cp, tc, mc) = (r.f(8)?, r.f(8)?, r.f(8)?);
    }
    let c = |sx, sy, csp| ColorConfig {
        bit_depth,
        mono_chrome,
        subsampling_x: sx,
        subsampling_y: sy,
        chroma_sample_position: csp,
    };
    if mono_chrome {
        r.f(1)?; // color_range
        return Some(c(true, true, 0));
    }
    if cp == 1 && tc == 13 && mc == 0 {
        // sRGB: 4:4:4, full range, no color_range bit.
        return Some(c(false, false, 0));
    }
    r.f(1)?; // color_range
    let (sx, sy) = match profile {
        0 => (true, true),
        1 => (false, false),
        _ if twelve_bit => {
            let sx = r.flag()?;
            (sx, sx && r.flag()?)
        }
        _ => (true, false),
    };
    let csp = if sx && sy { u8::try_from(r.f(2)?).ok()? } else { 0 };
    Some(c(sx, sy, csp))
}

/// The `AV1CodecConfigurationRecord` (`av1C`, AV1-ISOBMFF 2.3) for `config` (bytes holding a sequence
/// header OBU): what MP4 demuxers put in `csd-0`. None if `config` has no parseable sequence header.
pub(crate) fn av1c_record(config: &[u8]) -> Option<(Vec<u8>, SeqHeader)> {
    let o = obus(config).into_iter().find(|o| o.typ == OBU_SEQUENCE_HEADER)?;
    let sh = parse_seq_header(&config[o.payload..o.end])?;
    let c = sh.color;
    let b = |x: bool| u8::from(x);
    let mut rec = vec![
        0x81, // marker 1, version 1
        (sh.profile << 5) | (sh.level_idx & 0x1f),
        (sh.tier << 7)
            | (b(c.bit_depth > 8) << 6)
            | (b(c.bit_depth == 12) << 5)
            | (b(c.mono_chrome) << 4)
            | (b(c.subsampling_x) << 3)
            | (b(c.subsampling_y) << 2)
            | (c.chroma_sample_position & 3),
        0, // no initial_presentation_delay
    ];
    rec.extend_from_slice(&config[o.start..o.end]);
    Some((rec, sh))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_seq_header(data: &[u8]) -> bool {
        seq_header_obu(data).is_some()
    }

    /// OBU header byte with `obu_has_size_field` set.
    fn hdr(typ: u8) -> u8 {
        (typ << 3) | 0b10
    }

    /// Sequence header OBU from `av1_nvenc` (`ffmpeg` 8.1, 1920x1080, Main, level 4.1 high tier).
    const SEQ_1080: [u8; 13] = [0x0a, 0x0b, 0x00, 0x00, 0x00, 0x4e, 0xab, 0xbf, 0xc3, 0x70, 0x08, 0x66, 0x01];
    /// The same encoder at 3840x2160 (level 5.1 high tier).
    const SEQ_2160: [u8; 14] = [0x0a, 0x0c, 0x00, 0x00, 0x00, 0x6e, 0xef, 0xbf, 0xe1, 0xbc, 0x02, 0x19, 0x80, 0x40];

    fn concat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn finds_seq_header() {
        // TD (size 0), sequence header (size 2), frame (size 3).
        let data = [hdr(2), 0, hdr(1), 2, 0xaa, 0xbb, hdr(6), 3, 1, 2, 3];
        assert!(has_seq_header(&data));
        assert_eq!(seq_header_obu(&data), Some(&data[2..6]));
        let no_seq = [hdr(2), 0, hdr(6), 1, 9];
        assert!(!has_seq_header(&no_seq));
        assert_eq!(seq_header_obu(&no_seq), None);
    }

    #[test]
    fn truncated_input_is_safe() {
        assert!(!has_seq_header(&[hdr(1), 0x80]));
        assert!(!has_seq_header(&[hdr(1), 5, 1]));
        assert!(!has_seq_header(&[]));
        // Extension flag set but no extension byte.
        assert!(!has_seq_header(&[hdr(1) | 0b100]));
        // Huge leb128 size must not overflow.
        assert!(!has_seq_header(&[hdr(1), 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]));
        let short = [hdr(6), 9, 1];
        let (parts, info) = sample_parts(&short, None);
        assert_eq!(concat(&parts), [hdr(6), 9, 1], "unparseable bytes are passed through");
        assert_eq!(info, SampleInfo::default());
    }

    #[test]
    fn sample_strips_td_and_padding() {
        // TD, sequence header, frame, padding, frame.
        let tu = [hdr(2), 0, hdr(1), 1, 7, hdr(6), 2, 1, 2, hdr(15), 2, 0, 0, hdr(6), 1, 5];
        let (parts, info) = sample_parts(&tu, None);
        assert_eq!(concat(&parts), [hdr(1), 1, 7, hdr(6), 2, 1, 2, hdr(6), 1, 5]);
        assert_eq!(info, SampleInfo { stripped: 2, seq_added: false });
        assert_eq!(parts.len(), 2, "kept runs are contiguous slices");
    }

    #[test]
    fn sample_adds_seq_header_to_a_keyframe_without_one() {
        let seq = [hdr(1), 1, 7];
        let tu = [hdr(2), 0, hdr(6), 2, 1, 2];
        let (parts, info) = sample_parts(&tu, Some(&seq));
        assert_eq!(concat(&parts), [hdr(1), 1, 7, hdr(6), 2, 1, 2]);
        assert!(info.seq_added);
        // A keyframe that has one keeps its own.
        let tu = [hdr(2), 0, hdr(1), 1, 8, hdr(6), 1, 1];
        let (parts, info) = sample_parts(&tu, Some(&seq));
        assert_eq!(concat(&parts), [hdr(1), 1, 8, hdr(6), 1, 1]);
        assert!(!info.seq_added);
    }

    #[test]
    fn sample_keeps_last_obu_without_size_field() {
        // TD with size, then a frame OBU without obu_has_size_field (runs to the end).
        let tu = [hdr(2), 0, 6 << 3, 1, 2, 3];
        let (parts, info) = sample_parts(&tu, None);
        assert_eq!(concat(&parts), [6 << 3, 1, 2, 3]);
        assert_eq!(info.stripped, 1);
    }

    #[test]
    fn extension_byte_is_skipped() {
        // Frame OBU with an extension byte (temporal/spatial id), then a TD.
        let tu = [hdr(6) | 0b100, 0x08, 2, 1, 2, hdr(2), 0];
        let (parts, _) = sample_parts(&tu, None);
        assert_eq!(concat(&parts), [hdr(6) | 0b100, 0x08, 2, 1, 2]);
    }

    #[test]
    fn parses_nvenc_sequence_headers() {
        let sh = parse_seq_header(&SEQ_1080[2..]).expect("valid sequence header");
        assert_eq!((sh.profile, sh.level_idx, sh.tier), (0, 9, 1));
        assert_eq!((sh.max_width, sh.max_height), (1920, 1080));
        let c = sh.color;
        assert!(c.bit_depth == 8 && !c.mono_chrome && c.subsampling_x && c.subsampling_y);
        let sh = parse_seq_header(&SEQ_2160[2..]).expect("valid sequence header");
        assert_eq!((sh.level_idx, sh.max_width, sh.max_height), (13, 3840, 2160));
    }

    /// `av1C` must match what the `ffmpeg` MP4 muxer writes for the same stream.
    #[test]
    fn av1c_matches_ffmpeg() {
        let (rec, _) = av1c_record(&SEQ_1080).expect("record");
        assert_eq!(rec[..4], [0x81, 0x09, 0x8c, 0x00]);
        assert_eq!(rec[4..], SEQ_1080);
        let (rec, _) = av1c_record(&SEQ_2160).expect("record");
        assert_eq!(rec[..4], [0x81, 0x0d, 0x8c, 0x00]);
        // CONFIG with other OBUs around the sequence header (TD first) still works.
        let mut cfg = vec![hdr(2), 0];
        cfg.extend_from_slice(&SEQ_1080);
        let (rec, _) = av1c_record(&cfg).expect("record");
        assert_eq!(rec[4..], SEQ_1080);
    }

    #[test]
    fn parses_color_description_and_timing_info() {
        // Hand-built: profile 0, timing info + decoder model, 1 operating point with a model, BT.709.
        let mut w = BitWriter::default();
        w.put(0, 3); // seq_profile
        w.put(0, 1); // still_picture
        w.put(0, 1); // reduced_still_picture_header
        w.put(1, 1); // timing_info_present_flag
        w.put(1, 32);
        w.put(60, 32);
        w.put(1, 1); // equal_picture_interval
        w.put(1, 1); // uvlc 0
        w.put(1, 1); // decoder_model_info_present_flag
        w.put(9, 5); // buffer_delay_length_minus_1 = 9 -> 10 bits
        w.put(1, 32);
        w.put(0, 5);
        w.put(0, 5);
        w.put(0, 1); // initial_display_delay_present_flag
        w.put(0, 5); // operating_points_cnt_minus_1
        w.put(0, 12);
        w.put(12, 5); // seq_level_idx 12 (5.0)
        w.put(0, 1); // tier
        w.put(1, 1); // decoder_model_present_for_this_op
        w.put(0, 10);
        w.put(0, 10);
        w.put(1, 1);
        w.put(11, 4); // frame_width_bits_minus_1
        w.put(11, 4);
        w.put(3839, 12);
        w.put(2159, 12);
        w.put(0, 1); // frame_id_numbers_present_flag
        w.put(0, 3);
        w.put(0, 4);
        w.put(1, 1); // enable_order_hint
        w.put(0, 2);
        w.put(0, 1); // seq_choose_screen_content_tools
        w.put(0, 1); // seq_force_screen_content_tools = 0
        w.put(6, 3); // order_hint_bits_minus_1
        w.put(0, 3);
        w.put(0, 1); // high_bitdepth
        w.put(0, 1); // mono_chrome
        w.put(1, 1); // color_description_present_flag
        w.put(1, 8);
        w.put(1, 8);
        w.put(1, 8);
        w.put(0, 1); // color_range
        w.put(2, 2); // chroma_sample_position
        w.put(0, 1); // separate_uv_delta_q
        let sh = parse_seq_header(&w.bytes()).expect("valid");
        assert_eq!((sh.level_idx, sh.tier, sh.color.chroma_sample_position), (12, 0, 2));
        assert_eq!((sh.max_width, sh.max_height), (3840, 2160));
        assert!(parse_seq_header(&w.bytes()[..6]).is_none(), "truncated header");
    }

    #[derive(Default)]
    struct BitWriter {
        bits: Vec<u8>,
    }

    impl BitWriter {
        fn put(&mut self, v: u32, n: u32) {
            for i in (0..n).rev() {
                self.bits.push(u8::from((v >> i) & 1 == 1));
            }
        }

        fn bytes(&self) -> Vec<u8> {
            self.bits.chunks(8).map(|c| c.iter().enumerate().fold(0u8, |a, (i, &b)| a | (b << (7 - i)))).collect()
        }
    }
}
