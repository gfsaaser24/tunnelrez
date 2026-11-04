//! H.264 levels: the lowest level that carries a stream (size, frame rate, bitrate), and the level an
//! encoder really wrote, read back from the SPS (H.264 and HEVC).

use crate::flv::{self, Codec};
use anyhow::{bail, Result};
use std::fmt;

/// One H.264 level (ITU-T H.264 Table A-1).
struct H264Limits {
    /// `level_idc` (10 x the level: 52 = 5.2).
    idc: u8,
    /// Max macroblocks per second.
    mbps: u64,
    /// Max frame size in macroblocks.
    fs: u64,
    /// Max video bitrate in kbit/s, at the Main-profile factor (1000 bit/s per unit). High allows 1.25x this;
    /// counting 1000 keeps a margin, so High 5.2 is used up to 240 Mb/s.
    br_kbps: u64,
}

const H264_LEVELS: &[H264Limits] = &[
    H264Limits { idc: 10, mbps: 1_485, fs: 99, br_kbps: 64 },
    H264Limits { idc: 11, mbps: 3_000, fs: 396, br_kbps: 192 },
    H264Limits { idc: 12, mbps: 6_000, fs: 396, br_kbps: 384 },
    H264Limits { idc: 13, mbps: 11_880, fs: 396, br_kbps: 768 },
    H264Limits { idc: 20, mbps: 11_880, fs: 396, br_kbps: 2_000 },
    H264Limits { idc: 21, mbps: 19_800, fs: 792, br_kbps: 4_000 },
    H264Limits { idc: 22, mbps: 20_250, fs: 1_620, br_kbps: 4_000 },
    H264Limits { idc: 30, mbps: 40_500, fs: 1_620, br_kbps: 10_000 },
    H264Limits { idc: 31, mbps: 108_000, fs: 3_600, br_kbps: 14_000 },
    H264Limits { idc: 32, mbps: 216_000, fs: 5_120, br_kbps: 20_000 },
    H264Limits { idc: 40, mbps: 245_760, fs: 8_192, br_kbps: 20_000 },
    H264Limits { idc: 41, mbps: 245_760, fs: 8_192, br_kbps: 50_000 },
    H264Limits { idc: 42, mbps: 522_240, fs: 8_704, br_kbps: 50_000 },
    H264Limits { idc: 50, mbps: 589_824, fs: 22_080, br_kbps: 135_000 },
    H264Limits { idc: 51, mbps: 983_040, fs: 36_864, br_kbps: 240_000 },
    H264Limits { idc: 52, mbps: 2_073_600, fs: 36_864, br_kbps: 240_000 },
    H264Limits { idc: 60, mbps: 4_177_920, fs: 139_264, br_kbps: 240_000 },
    H264Limits { idc: 61, mbps: 8_355_840, fs: 139_264, br_kbps: 480_000 },
    H264Limits { idc: 62, mbps: 16_711_680, fs: 139_264, br_kbps: 800_000 },
];

/// The lowest H.264 `level_idc` for `w`x`h` at `fps` and `bitrate` bits per second (52 = level 5.2).
///
/// # Errors
///
/// When no level carries the stream (above level 6.2: 800 Mb/s, or a frame size / rate beyond it).
pub fn h264_level_for(w: u32, h: u32, fps: u32, bitrate: u64) -> Result<u8> {
    let fs = u64::from(w.div_ceil(16)) * u64::from(h.div_ceil(16));
    let mbps = fs * u64::from(fps.max(1));
    let found = H264_LEVELS.iter().find(|l| fs <= l.fs && mbps <= l.mbps && bitrate <= l.br_kbps * 1000);
    match found {
        Some(l) => Ok(l.idc),
        None => bail!(
            "H.264 cannot carry {:.1} Mb/s at {w}x{h}@{fps}: the highest level (6.2) allows 800 Mb/s and \
             {} macroblocks/s; use a lower --bitrate or HEVC",
            crate::num::f64_from_u64(bitrate) / 1e6,
            H264_LEVELS.last().map_or(0, |l| l.mbps),
        ),
    }
}

/// `level_idc` as a level number: 52 -> "5.2", 9 -> "1b".
fn h264_level_name(idc: u8) -> String {
    if idc == 9 {
        "1b".into()
    } else {
        format!("{}.{}", idc / 10, idc % 10)
    }
}

/// The level written in a stream's SPS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpsLevel {
    /// H.264 `level_idc` (52 = 5.2).
    H264(u8),
    /// HEVC `general_level_idc` (30 x the level: 156 = 5.2) and the tier.
    Hevc { idc: u8, high_tier: bool },
}

impl fmt::Display for SpsLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            SpsLevel::H264(idc) => write!(f, "H.264 level {}", h264_level_name(idc)),
            SpsLevel::Hevc { idc, high_tier } => {
                write!(f, "HEVC level {}.{} {} tier", idc / 30, (idc % 30) / 3, if high_tier { "High" } else { "Main" })
            }
        }
    }
}

/// The SPS NAL with its emulation-prevention bytes (`00 00 03`) removed.
fn sps_rbsp(codec: Codec, annexb: &[u8]) -> Option<Vec<u8>> {
    let sps_type = match codec {
        Codec::H264 => 7,
        Codec::Hevc => 33,
        Codec::Av1 => return None,
    };
    let nal = flv::annexb_nals(annexb).into_iter().find(|n| flv::nal_type(codec, n) == sps_type)?;
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    Some(out)
}

/// The level in the SPS of `annexb` (codec config or a keyframe), or None when there is no SPS (AV1, or
/// parameter sets not seen yet).
pub fn sps_level(codec: Codec, annexb: &[u8]) -> Option<SpsLevel> {
    let r = sps_rbsp(codec, annexb)?;
    match codec {
        // NAL header, profile_idc, constraint flags, level_idc.
        Codec::H264 => r.get(3).map(|&idc| SpsLevel::H264(idc)),
        // 2-byte NAL header, vps id / sub-layers byte, then profile_tier_level: space/tier/profile byte,
        // 4 bytes compatibility flags, 6 bytes constraint flags, general_level_idc.
        Codec::Hevc => {
            let ptl = *r.get(3)?;
            let idc = *r.get(14)?;
            Some(SpsLevel::Hevc { idc, high_tier: ptl & 0x20 != 0 })
        }
        Codec::Av1 => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_level_follows_size_rate_and_bitrate() {
        assert_eq!(h264_level_for(1920, 1080, 60, 30_000_000).unwrap(), 42);
        assert_eq!(h264_level_for(3840, 2160, 60, 60_000_000).unwrap(), 52);
        assert_eq!(h264_level_for(3840, 2160, 60, 150_000_000).unwrap(), 52, "5.2 carries up to 240 Mb/s");
        assert_eq!(h264_level_for(3840, 2160, 60, 240_000_000).unwrap(), 52);
        assert_eq!(h264_level_for(3840, 2160, 60, 241_000_000).unwrap(), 61, "above 240 Mb/s: 6.x");
        assert_eq!(h264_level_for(1920, 1080, 60, 150_000_000).unwrap(), 51, "the bitrate raises the level");
        assert_eq!(h264_level_for(1920, 1080, 60, 800_000_000).unwrap(), 62);
    }

    #[test]
    fn h264_level_fails_beyond_6_2() {
        let e = h264_level_for(3840, 2160, 60, 900_000_000).unwrap_err().to_string();
        assert!(e.contains("900.0 Mb/s") && e.contains("6.2"), "{e}");
    }

    #[test]
    fn level_names() {
        assert_eq!(SpsLevel::H264(52).to_string(), "H.264 level 5.2");
        assert_eq!(SpsLevel::H264(9).to_string(), "H.264 level 1b");
        assert_eq!(SpsLevel::Hevc { idc: 153, high_tier: true }.to_string(), "HEVC level 5.1 High tier");
        assert_eq!(SpsLevel::Hevc { idc: 186, high_tier: false }.to_string(), "HEVC level 6.2 Main tier");
    }

    #[test]
    fn h264_level_from_sps() {
        // SPS: High (100), constraints 0, level 5.2, then the rest; a PPS before it.
        let cfg = [0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80, 0, 0, 0, 1, 0x67, 100, 0, 52, 0xac, 0x2b];
        assert_eq!(sps_level(Codec::H264, &cfg), Some(SpsLevel::H264(52)));
        assert_eq!(sps_level(Codec::H264, &cfg[..8]), None, "no SPS");
    }

    #[test]
    fn hevc_level_from_sps_with_emulation_prevention() {
        // SPS (type 33): header 42 01, vps/sub-layers 01, PTL High tier Main (0x21), compatibility
        // 60 00 00 00 (00 00 escaped as 00 00 03), constraint flags 90 00 00 00 00 00 (escaped), level 153.
        let cfg = [
            0, 0, 0, 1, 0x42, 0x01, 0x01, 0x21, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03,
            0x00, 153, 0xa0,
        ];
        assert_eq!(sps_level(Codec::Hevc, &cfg), Some(SpsLevel::Hevc { idc: 153, high_tier: true }));
        assert_eq!(sps_level(Codec::Av1, &cfg), None);
    }
}
