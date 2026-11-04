//! The TV's video decoders, from its CAPS message (PROTOCOL.md, type 8), and the codec Auto picks per picture.
//!
//! The receiver sends CAPS once per connection; the sender prints it as one stderr line (`[tv] caps {...}`). The
//! app keeps the last one per TV address in the settings, so Auto can pick before the next connection.
//!
//! Why Auto picks what it picks (real TV, Hisense MT5895, motion page full screen, run book
//! `docs/runbook/runs/`, capture-to-shown latency from the TV clock):
//! - **1080p60 → HEVC.** At 40 Mb/s native NVENC, HEVC had the lowest delay: 17.6 ms, decode 5.5 ms
//!   (2026-10-26-09); AV1 22.6 ms, decode 7.0 ms (2026-10-26-10).
//! - **4K60 → HEVC too.** At 100 Mb/s native NVENC, HEVC decoded in 10.9 ms and took 36.3 ms (2026-10-26-08). AV1's
//!   34.4 ms and decode 11.3 ms (2026-10-26-11) do not beat it: that picture stood still 45 of its 118 s, so most of
//!   its lead is the still screen, and its decode was slower all the same. H.264 was 43.3 ms, decode 14.7 ms
//!   (2026-10-26-12). When HEVC cannot run, AV1 comes next at 4K on a TV with a hardware decoder for it, else H.264.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::settings::{Codec, Picture};

/// The prefix of the sender's stderr line with the TV's CAPS JSON.
pub(crate) const LINE_PREFIX: &str = "[tv] caps ";

/// One video decoder of the TV.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct Decoder {
    /// `OMX.MTK.VIDEO.DECODER.AV1`.
    pub(crate) name: String,
    /// `video/av01`, `video/hevc`, `video/avc`.
    pub(crate) mime: String,
    pub(crate) max_w: u32,
    pub(crate) max_h: u32,
    /// 0 = not reported.
    pub(crate) max_fps: u32,
    /// `isHardwareAccelerated`, API 29+ only.
    pub(crate) hw: Option<bool>,
    /// Performance points `WxH@fps` (API 29+): the only joint size-and-rate support the TV reports. The `max_*`
    /// values are separate range ends and do not say that `max_w x max_h` runs at `max_fps`.
    pub(crate) perf: Vec<String>,
}

impl Decoder {
    /// A software decoder (Android's own `c2.android.*` / `OMX.google.*`, or one that says `hw: false`). They exist
    /// for most codecs and cannot keep up with a 4K60 stream, so they never count.
    fn software(&self) -> bool {
        let n = self.name.to_ascii_lowercase();
        self.hw == Some(false) || n.starts_with("c2.android.") || n.starts_with("omx.google.")
    }
}

/// What the TV can decode.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct TvCaps {
    pub(crate) decoders: Vec<Decoder>,
}

/// The MIME type of a concrete codec.
fn mime(c: Codec) -> Option<&'static str> {
    match c {
        Codec::Auto => None,
        Codec::Hevc => Some("video/hevc"),
        Codec::H264 => Some("video/avc"),
        Codec::Av1 => Some("video/av01"),
    }
}

impl TvCaps {
    /// Parse the CAPS JSON. Unknown keys are ignored; `None` when it is not a CAPS object.
    pub(crate) fn parse(json: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(json).ok()?;
        let list = v.get("decoders")?.as_array()?;
        let decoders = list.iter().filter_map(|d| serde_json::from_value::<Decoder>(d.clone()).ok()).collect();
        Some(Self { decoders })
    }

    /// The CAPS from a sender stderr line (`[tv] caps {...}`), else `None`.
    pub(crate) fn from_line(line: &str) -> Option<Self> {
        Self::parse(line.trim().strip_prefix(LINE_PREFIX)?.trim())
    }

    /// The TV has a hardware decoder for `codec` with a performance point that covers `picture` at its frame rate.
    /// Unknown (no performance points: Android before 10) is not support.
    pub(crate) fn decodes(&self, codec: Codec, picture: Picture) -> bool {
        let Some(mime) = mime(codec) else { return false };
        let (w, h) = picture.size();
        self.decoders.iter().any(|d| {
            d.mime.eq_ignore_ascii_case(mime)
                && !d.software()
                && d.perf
                    .iter()
                    .filter_map(|p| perf_point(p))
                    .any(|(pw, ph, fps)| pw.max(ph) >= w.max(h) && pw.min(ph) >= w.min(h) && fps >= Picture::FPS)
        })
    }
}

/// `3840x2160@60` -> (3840, 2160, 60).
fn perf_point(p: &str) -> Option<(u32, u32, u32)> {
    let (size, fps) = p.trim().split_once('@')?;
    let (w, h) = size.split_once('x')?;
    // A fractional rate (`59.94`) counts by its whole part.
    let fps = fps.split('.').next()?;
    Some((w.parse().ok()?, h.parse().ok()?, fps.parse().ok()?))
}

/// The CAPS of one TV, by its address.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct KnownTv {
    pub(crate) host: String,
    pub(crate) caps: TvCaps,
}

/// The codecs Auto tries for `picture`, best first (see the module docs for the measurements): HEVC leads everywhere.
/// `tv` is what the TV said it decodes, when known; AV1 goes before H.264 at 4K only when the TV said it has a hardware
/// AV1 decoder for it.
pub(crate) fn auto_order(picture: Picture, tv: Option<&TvCaps>) -> [Codec; 3] {
    match picture {
        Picture::P2160x60 if tv.is_some_and(|t| t.decodes(Codec::Av1, picture)) => {
            [Codec::Hevc, Codec::Av1, Codec::H264]
        }
        _ => [Codec::Hevc, Codec::H264, Codec::Av1],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Hisense MT5895's CAPS (PROTOCOL.md), with Android's software AV1 decoder too.
    const HISENSE: &str = r#"{"v":1,"caps":[],"decoders":[
        {"name":"OMX.MTK.VIDEO.DECODER.AV1","mime":"video/av01","max_w":4096,"max_h":2304,"max_fps":60,
         "max_bps":60000000,"hw":true,"perf":["3840x2160@60"]},
        {"name":"OMX.MTK.VIDEO.DECODER.HEVC","mime":"video/hevc","max_w":4096,"max_h":2304,"max_fps":60,"hw":true,
         "perf":["3840x2160@60","1920x1080@120"]},
        {"name":"c2.android.av1.decoder","mime":"video/av01","max_w":4096,"max_h":4096,"max_fps":60}]}"#;

    #[test]
    fn parses_caps_and_the_sender_line() {
        let c = TvCaps::parse(HISENSE).expect("caps");
        assert_eq!(c.decoders.len(), 3);
        assert_eq!(c.decoders[0].max_w, 4096);
        let line = format!("{LINE_PREFIX}{}", HISENSE.replace('\n', ""));
        assert_eq!(TvCaps::from_line(&line), Some(c));
        assert_eq!(TvCaps::from_line("[net] connected"), None);
        assert_eq!(TvCaps::from_line("[tv] caps unreadable: expected value"), None);
        assert_eq!(TvCaps::parse(r#"{"v":1}"#), None, "no decoder list: not CAPS");
    }

    #[test]
    fn decodes_needs_a_hardware_decoder_big_enough() {
        let c = TvCaps::parse(HISENSE).expect("caps");
        assert!(c.decodes(Codec::Av1, Picture::P2160x60));
        assert!(c.decodes(Codec::Hevc, Picture::P1080x60));
        assert!(!c.decodes(Codec::H264, Picture::P1080x60), "no AVC decoder listed");
        // Only Android's software AV1 decoder: it does not count.
        let sw = TvCaps::parse(
            r#"{"decoders":[{"name":"c2.android.av1.decoder","mime":"video/av01","max_w":4096,"max_h":4096}]}"#,
        )
        .expect("caps");
        assert!(!sw.decodes(Codec::Av1, Picture::P2160x60));
        // A 1080p-only AV1 decoder.
        let small = TvCaps::parse(
            r#"{"decoders":[{"name":"OMX.X.AV1","mime":"video/av01","max_w":1920,"max_h":1088,"max_fps":60,"hw":true,"perf":["1920x1080@60"]}]}"#,
        )
        .expect("caps");
        assert!(small.decodes(Codec::Av1, Picture::P1080x60));
        assert!(!small.decodes(Codec::Av1, Picture::P2160x60));
        // Separate range ends that look like 4K60 but are not one mode (review on #24): 4K only at 30.
        let split = TvCaps::parse(
            r#"{"decoders":[{"name":"OMX.X.AV1","mime":"video/av01","max_w":4096,"max_h":2304,"max_fps":120,"hw":true,
                "perf":["3840x2160@30","1920x1080@120"]}]}"#,
        )
        .expect("caps");
        assert!(!split.decodes(Codec::Av1, Picture::P2160x60));
        assert!(split.decodes(Codec::Av1, Picture::P1080x60));
        // No performance points (Android 9): unknown, so not support.
        let old = TvCaps::parse(
            r#"{"decoders":[{"name":"OMX.X.AV1","mime":"video/av01","max_w":4096,"max_h":2304,"max_fps":60}]}"#,
        )
        .expect("caps");
        assert!(!old.decodes(Codec::Av1, Picture::P2160x60));
        assert_eq!(perf_point("3840x2160@59.94"), Some((3840, 2160, 59)));
        assert_eq!(perf_point("bad"), None);
    }

    #[test]
    fn auto_picks_per_picture() {
        let tv = TvCaps::parse(HISENSE).expect("caps");
        assert_eq!(
            auto_order(Picture::P1080x60, Some(&tv))[0],
            Codec::Hevc,
            "1080p: HEVC even with AV1 (run 09 vs 10)"
        );
        assert_eq!(
            auto_order(Picture::P2160x60, Some(&tv)),
            [Codec::Hevc, Codec::Av1, Codec::H264],
            "4K: HEVC first (run 08)"
        );
        assert_eq!(auto_order(Picture::P2160x60, None)[0], Codec::Hevc, "4K, TV not known yet: HEVC");
        assert_eq!(auto_order(Picture::P2160x60, Some(&TvCaps::default()))[0], Codec::Hevc, "no AV1 decoder: HEVC");
    }
}
