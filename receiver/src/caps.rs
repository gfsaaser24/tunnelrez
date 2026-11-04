//! The CAPS message (TV -> PC, once per connection): the TV's video decoders, so the PC can pick a codec,
//! size and rate the TV really decodes. See `PROTOCOL.md`, type 8.

use serde_json::{json, Value};

/// One video decoder and one MIME type it handles, from `MediaCodecList`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DecoderInfo {
    /// Component name, e.g. `OMX.MTK.VIDEO.DECODER.AV1`.
    pub(crate) name: String,
    /// e.g. `video/av01`.
    pub(crate) mime: String,
    /// Upper end of `getSupportedWidths()` / `getSupportedHeights()`.
    pub(crate) max_width: i32,
    pub(crate) max_height: i32,
    /// Upper end of `getSupportedFrameRates()`.
    pub(crate) max_fps: i32,
    /// Upper end of `getBitrateRange()`, bits/s. The declared limit: decoders often take more.
    pub(crate) max_bps: i32,
    /// `isHardwareAccelerated()` (API 29+); None before.
    pub(crate) hardware: Option<bool>,
    /// Performance points (API 29+), e.g. `3840x2160@60`.
    pub(crate) perf: Vec<String>,
}

/// Message version.
const VERSION: u32 = 1;

/// The CAPS JSON for `decoders`. `caps` (feature flags): `audio` = this TV plays AUDIO datagrams.
pub(crate) fn caps_json(decoders: &[DecoderInfo]) -> String {
    let list: Vec<Value> = decoders
        .iter()
        .map(|d| {
            let mut v = json!({
                "name": d.name,
                "mime": d.mime,
                "max_w": d.max_width,
                "max_h": d.max_height,
                "max_fps": d.max_fps,
                "max_bps": d.max_bps,
            });
            if let Some(hw) = d.hardware {
                v["hw"] = hw.into();
            }
            if !d.perf.is_empty() {
                v["perf"] = d.perf.clone().into();
            }
            v
        })
        .collect();
    json!({ "v": VERSION, "caps": ["audio"], "decoders": list }).to_string()
}

/// `WxH@F` from `PerformancePoint.toString()` (e.g. `PerformancePoint(3840x2160@60, ...)`), or None.
pub(crate) fn perf_point(s: &str) -> Option<String> {
    let start = s.find(|c: char| c.is_ascii_digit())?;
    let rest = &s[start..];
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == 'x' || c == '@')).unwrap_or(rest.len());
    let p = &rest[..end];
    let (size, fps) = p.split_once('@')?;
    let (w, h) = size.split_once('x')?;
    let all_digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    (all_digits(w) && all_digits(h) && all_digits(fps)).then(|| p.to_string())
}

/// Decoders worth reporting: video, not the `.secure` variants (DRM only).
pub(crate) fn wanted(name: &str, mime: &str) -> bool {
    mime.starts_with("video/") && !name.to_ascii_lowercase().ends_with(".secure")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_has_every_field() {
        let d = DecoderInfo {
            name: "OMX.MTK.VIDEO.DECODER.AV1".into(),
            mime: "video/av01".into(),
            max_width: 4096,
            max_height: 2304,
            max_fps: 60,
            max_bps: 60_000_000,
            hardware: Some(true),
            perf: vec!["3840x2160@60".into()],
        };
        let old = DecoderInfo { name: "OMX.google.h264.decoder".into(), mime: "video/avc".into(), ..d.clone() };
        let old = DecoderInfo { hardware: None, perf: Vec::new(), ..old };
        let v: Value = serde_json::from_str(&caps_json(&[d, old])).expect("valid JSON");
        assert_eq!(v["v"], 1);
        assert_eq!(v["caps"], json!(["audio"]));
        let first = &v["decoders"][0];
        assert_eq!(first["mime"], "video/av01");
        assert_eq!(first["max_w"], 4096);
        assert_eq!(first["max_h"], 2304);
        assert_eq!(first["max_fps"], 60);
        assert_eq!(first["max_bps"], 60_000_000);
        assert_eq!(first["hw"], true);
        assert_eq!(first["perf"], json!(["3840x2160@60"]));
        let second = &v["decoders"][1];
        assert!(second.get("hw").is_none() && second.get("perf").is_none(), "optional keys left out");
    }

    #[test]
    fn perf_point_parsing() {
        assert_eq!(perf_point("PerformancePoint(3840x2160@60)").as_deref(), Some("3840x2160@60"));
        assert_eq!(perf_point("PerformancePoint(1920x1080@120, 8160 blocks)").as_deref(), Some("1920x1080@120"));
        assert_eq!(perf_point("PerformancePoint()"), None);
        assert_eq!(perf_point("1920x@60"), None);
        assert_eq!(perf_point("garbage"), None);
    }

    #[test]
    fn filter() {
        assert!(wanted("OMX.MTK.VIDEO.DECODER.HEVC", "video/hevc"));
        assert!(!wanted("OMX.MTK.VIDEO.DECODER.AVC.secure", "video/avc"));
        assert!(!wanted("OMX.google.aac.decoder", "audio/mp4a-latm"));
    }
}
