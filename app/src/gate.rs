//! Which encoder x codec combinations can really run, from the sender's `caps` probe (a short real encode per
//! encoder and codec at the picked size, cached by the sender). The panel greys out what cannot work, Start never
//! launches it, and Auto never picks it.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;

use crate::platform;
use crate::settings::{Codec, Encoder, Picture, Settings};
use crate::stream::Source;
use crate::tvcaps::{self, TvCaps};

/// The Microsoft Store page of the free "HEVC Video Extensions from Device Manufacturer" (installs on some PCs only).
pub(crate) const HEVC_STORE_FREE: &str = "ms-windows-store://pdp/?ProductId=9N4WGH0Z6VHQ";
/// The Microsoft Store page of the paid "HEVC Video Extensions".
pub(crate) const HEVC_STORE_PAID: &str = "ms-windows-store://pdp/?ProductId=9NMZLZ57R3T7";
/// Web pages of the same two items, when the Store app cannot be opened.
pub(crate) const HEVC_WEB_FREE: &str = "https://apps.microsoft.com/detail/9n4wgh0z6vhq";
pub(crate) const HEVC_WEB_PAID: &str = "https://apps.microsoft.com/detail/9nmzlz57r3t7";

/// One `caps` result.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    encoder: String,
    codec: String,
    ok: bool,
    reason: String,
}

/// Every `caps` result for one picture.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Matrix {
    entries: Vec<Entry>,
}

/// A combination the panel must not start, and why.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Blocked {
    /// One line for the inline hint and tooltip.
    pub(crate) message: String,
    /// The failure is the missing Windows HEVC codec: show the Microsoft Store buttons.
    pub(crate) hevc_store: bool,
}

/// The sender's `caps` name for an encoder choice (None = Auto).
fn encoder_name(e: Encoder) -> Option<&'static str> {
    match e {
        Encoder::Auto => None,
        Encoder::Nvenc => Some("nvenc"),
        Encoder::NvencNative => Some("native"),
        Encoder::Qsv => Some("qsv"),
        Encoder::Amf => Some("amf"),
        Encoder::Mf => Some("mf"),
        Encoder::Cpu => Some("cpu"),
    }
}

/// The panel's name for an encoder choice (it says which paths use `FFmpeg`).
fn encoder_words(e: Encoder) -> &'static str {
    e.label()
}

/// The sender's name for a concrete codec (None = Auto).
fn codec_name(c: Codec) -> Option<&'static str> {
    match c {
        Codec::Auto => None,
        Codec::Hevc => Some("hevc"),
        Codec::H264 => Some("h264"),
        Codec::Av1 => Some("av1"),
    }
}

fn codec_words(name: &str) -> &'static str {
    match name {
        "hevc" => "HEVC",
        "h264" => "H.264",
        _ => "AV1",
    }
}

/// The encoder choices the panel can pick for a concrete combination (Auto excluded).
const PICKABLE: [Encoder; 6] =
    [Encoder::Nvenc, Encoder::NvencNative, Encoder::Qsv, Encoder::Amf, Encoder::Mf, Encoder::Cpu];

/// The failure says the Windows HEVC codec (HEVC Video Extensions) is missing: Media Foundation found no HEVC
/// encoder (`MF_E_TOPO_CODEC_NOT_FOUND`, or ffmpeg's "could not find any MFT").
pub(crate) fn needs_hevc_extension(reason: &str) -> bool {
    reason.to_ascii_lowercase().contains("hevc") && mft_missing(reason)
}

/// Media Foundation found no encoder transform: "could not find any MFT", `MF_E_TOPO_CODEC_NOT_FOUND`.
pub(crate) fn mft_missing(reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    (r.contains("mft") && (r.contains("could not find") || r.contains("no ") || r.contains("failed")))
        || r.contains("0xc00d5212")
}

/// A probe failure in plain words: "too slow: 40 fps, needs 60" -> "too slow, 40 fps"; "too slow: falling
/// behind, latency 35 -> 120 ms" -> "too slow, falling behind".
fn reason_words(reason: &str) -> String {
    // An older sender's reason, kept in its results.
    if reason.contains("not rendered by an NVIDIA GPU") {
        return "the monitor renders on another GPU".into();
    }
    if let Some(rest) = reason.strip_prefix("too slow: ") {
        let fps = rest.split(',').next().unwrap_or(rest).trim();
        return format!("too slow, {fps}");
    }
    if reason.contains("amfrt64.dll") || reason.to_ascii_lowercase().contains("no amd gpu") {
        return "no AMD GPU/driver".into();
    }
    if reason.starts_with("no ") && reason.ends_with(" encoder") {
        return "it has no encoder for this codec".into();
    }
    reason.to_string()
}

impl Matrix {
    /// Parse the sender's `caps` output (the last JSON line on stdout).
    pub(crate) fn parse(stdout: &str) -> Option<Matrix> {
        let line = stdout.lines().rev().find(|l| l.trim_start().starts_with('{'))?;
        let v: Value = serde_json::from_str(line).ok()?;
        let entries = v
            .get("results")?
            .as_array()?
            .iter()
            .filter_map(|r| {
                Some(Entry {
                    encoder: r.get("encoder")?.as_str()?.to_string(),
                    codec: r.get("codec")?.as_str()?.to_string(),
                    ok: r.get("ok")?.as_bool()?,
                    reason: r.get("reason").and_then(Value::as_str).unwrap_or("").to_string(),
                })
            })
            .collect();
        Some(Matrix { entries })
    }

    fn get(&self, encoder: &str, codec: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.encoder == encoder && e.codec == codec)
    }

    /// `encoder` can run `codec`: true when the probe said so or did not test it (unknown is not blocked).
    fn works(&self, encoder: &str, codec: &str) -> bool {
        self.get(encoder, codec).is_none_or(|e| e.ok)
    }

    /// Any encoder runs `codec` (what `--encoder auto` can find).
    fn any_works(&self, codec: &str) -> bool {
        let tested: Vec<&Entry> = self.entries.iter().filter(|e| e.codec == codec).collect();
        tested.is_empty() || tested.iter().any(|e| e.ok)
    }

    /// The codec Auto picks for `encoder`: the first of `order` (see [`tvcaps::auto_order`]) that works. None when
    /// none works.
    pub(crate) fn auto_codec(&self, encoder: Encoder, order: [Codec; 3]) -> Option<Codec> {
        order.into_iter().find(|&c| {
            let name = codec_name(c).expect("concrete codec");
            encoder_name(encoder).map_or_else(|| self.any_works(name), |e| self.works(e, name))
        })
    }

    /// Why `encoder` x `codec` cannot run at `picture`, or None when it can (or is not known). `other` is the probe
    /// of the other picture, to suggest it when it would work; `tv` is what the TV decodes (for Auto codec).
    pub(crate) fn blocked(
        &self,
        encoder: Encoder,
        codec: Codec,
        picture: Picture,
        other: Option<&Matrix>,
        tv: Option<&TvCaps>,
    ) -> Option<Blocked> {
        let Some(codec) = (match codec {
            Codec::Auto => self.auto_codec(encoder, tvcaps::auto_order(picture, tv)),
            c => Some(c),
        }) else {
            return Some(self.blocked_on_every_codec(encoder, picture));
        };
        let name = codec_name(codec).expect("concrete codec");
        // The entry itself says MF and HEVC; the reason (ffmpeg's prefix stripped) only has to show the MF cause.
        let hevc_store = codec == Codec::Hevc && self.get("mf", "hevc").is_some_and(|e| mft_missing(&e.reason));
        let Some(enc) = encoder_name(encoder) else {
            return (!self.any_works(name)).then(|| Blocked {
                message: format!(
                    "No encoder on this PC can send {} at {}. Pick another codec.",
                    codec_words(name),
                    picture.label()
                ),
                hevc_store,
            });
        };
        let entry = self.get(enc, name).filter(|e| !e.ok)?;
        let mut alts: Vec<String> = PICKABLE
            .iter()
            .filter(|&&e| e != encoder && encoder_name(e).is_some_and(|n| self.works_tested(n, name)))
            .map(|&e| encoder_words(e).to_string())
            .collect();
        let other_pic = match picture {
            Picture::P2160x60 => Picture::P1080x60,
            Picture::P1080x60 => Picture::P2160x60,
        };
        if other.is_some_and(|m| m.get(enc, name).is_some_and(|e| e.ok)) {
            alts.push(other_pic.label().to_string());
        }
        let pick =
            if alts.is_empty() { "Pick another codec.".to_string() } else { format!("Pick {}.", alts.join(" or ")) };
        Some(Blocked {
            message: format!(
                "{} can't encode {} at {} on this PC ({}). {pick}",
                encoder_words(encoder),
                codec_words(name),
                picture.label(),
                reason_words(&entry.reason)
            ),
            hevc_store: hevc_store && enc == "mf",
        })
    }

    /// Why no codec runs on `encoder`: the real reasons the check gave, and what can run instead.
    fn blocked_on_every_codec(&self, encoder: Encoder, picture: Picture) -> Blocked {
        let enc = encoder_name(encoder);
        let failed: Vec<&Entry> = self.entries.iter().filter(|e| !e.ok && enc.is_none_or(|n| e.encoder == n)).collect();
        let same_cause = failed.windows(2).all(|p| reason_words(&p[0].reason) == reason_words(&p[1].reason));
        let why = if same_cause {
            failed.first().map(|e| reason_words(&e.reason)).unwrap_or_default()
        } else {
            let mut lines: Vec<String> = Vec::new();
            for e in &failed {
                let line = format!("{}: {}", codec_words(&e.codec), reason_words(&e.reason));
                if !lines.contains(&line) {
                    lines.push(line);
                }
            }
            lines.join("; ")
        };
        let alts: Vec<&str> = PICKABLE
            .iter()
            .filter(|&&e| {
                e != encoder && encoder_name(e).is_some_and(|n| self.entries.iter().any(|x| x.encoder == n && x.ok))
            })
            .map(|&e| encoder_words(e))
            .collect();
        let pick = if alts.is_empty() {
            "Pick 1080p60 or check the graphics driver.".to_string()
        } else {
            format!("Pick {}.", alts.join(" or "))
        };
        let who = enc.map_or("No encoder", |_| encoder_words(encoder));
        let verb = if enc.is_some() { "can't run" } else { "can run" };
        Blocked {
            message: format!("{who} {verb} at {} on this PC ({why}). {pick}", picture.label()),
            hevc_store: matches!(encoder, Encoder::Auto | Encoder::Mf)
                && self.get("mf", "hevc").is_some_and(|e| mft_missing(&e.reason)),
        }
    }

    /// `encoder` was tested with `codec` and works (alternatives are only suggested when known to work).
    fn works_tested(&self, encoder: &str, codec: &str) -> bool {
        self.get(encoder, codec).is_some_and(|e| e.ok)
    }

    /// Every (encoder, codec) choice the panel offers that cannot run, keyed `"<encoder>|<codec>"` with the panel's
    /// setting values (`nvenc_native|hevc`), for greying out.
    pub(crate) fn blocked_map(
        &self,
        picture: Picture,
        other: Option<&Matrix>,
        tv: Option<&TvCaps>,
    ) -> Vec<(String, Blocked)> {
        let mut out = Vec::new();
        for e in std::iter::once(Encoder::Auto).chain(PICKABLE) {
            for c in [Codec::Auto, Codec::Hevc, Codec::H264, Codec::Av1] {
                if let Some(b) = self.blocked(e, c, picture, other, tv) {
                    let key = format!(
                        "{}|{}",
                        serde_json::to_value(e).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default(),
                        serde_json::to_value(c).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
                    );
                    out.push((key, b));
                }
            }
        }
        out
    }
}

/// `settings` with Auto codec resolved: the best codec for the picture on this TV ([`tvcaps::auto_order`]) that
/// the encoder check says works for the chosen encoder; the best one outright when nothing is measured.
pub(crate) fn resolve_codec(settings: &Settings, m: Option<&Matrix>) -> Settings {
    let mut s = settings.clone();
    if s.codec == Codec::Auto {
        let order = tvcaps::auto_order(s.picture, s.tv());
        s.codec = match m {
            Some(m) => m.auto_codec(s.encoder, order).unwrap_or(order[0]),
            None => order[0],
        };
    }
    s
}

/// The `caps` command line for `picture` on `source`.
pub(crate) fn caps_args(picture: Picture, source: &Source) -> Vec<String> {
    let (w, h) = picture.size();
    let mut a = vec!["caps".to_string()];
    match source {
        Source::Extend => a.push("--extend".into()),
        Source::Monitor(i) => a.extend(["--monitor".into(), i.to_string()]),
    }
    a.extend(["--size".into(), format!("{w}x{h}"), "--fps".into(), Picture::FPS.to_string()]);
    a
}

/// Run `sender caps` (a few seconds when cached, ~20 s the first time) and parse it.
///
/// # Errors
/// When the sender cannot be run or prints no `caps` result (an older sender).
pub(crate) fn probe(sender: &Path, picture: Picture, source: &Source) -> Result<Matrix> {
    let out = platform::hide_console(&mut Command::new(sender))
        .args(caps_args(picture, source))
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run {} caps", sender.display()))?;
    Matrix::parse(&String::from_utf8_lossy(&out.stdout)).context("the sender printed no caps result (older sender?)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tvcaps::KnownTv;

    const HEVC_FIRST: [Codec; 3] = [Codec::Hevc, Codec::H264, Codec::Av1];

    /// This laptop at 4K60 (measured): x265 and SVT-AV1 too slow, no AMD driver, QSV too slow.
    const CAPS_4K: &str = r#"{"results":[
 {"encoder":"native","codec":"h264","ok":true,"reason":""},{"encoder":"nvenc","codec":"h264","ok":true,"reason":""},
 {"encoder":"qsv","codec":"h264","ok":false,"reason":"too slow: 50 fps, 311.1 ms per frame"},
 {"encoder":"amf","codec":"h264","ok":false,"reason":"DLL amfrt64.dll failed to open"},
 {"encoder":"mf","codec":"h264","ok":true,"reason":""},{"encoder":"cpu","codec":"h264","ok":true,"reason":""},
 {"encoder":"native","codec":"hevc","ok":true,"reason":""},{"encoder":"nvenc","codec":"hevc","ok":true,"reason":""},
 {"encoder":"qsv","codec":"hevc","ok":false,"reason":"too slow: 75 fps, 195.3 ms per frame"},
 {"encoder":"amf","codec":"hevc","ok":false,"reason":"DLL amfrt64.dll failed to open"},
 {"encoder":"mf","codec":"hevc","ok":true,"reason":""},
 {"encoder":"cpu","codec":"hevc","ok":false,"reason":"too slow: 40 fps, 400.4 ms per frame"},
 {"encoder":"native","codec":"av1","ok":true,"reason":""},{"encoder":"nvenc","codec":"av1","ok":true,"reason":""},
 {"encoder":"qsv","codec":"av1","ok":false,"reason":"no av1 encoder"},
 {"encoder":"amf","codec":"av1","ok":false,"reason":"Error setting option header_insertion_mode to value idr."},
 {"encoder":"mf","codec":"av1","ok":false,"reason":"no av1 encoder"},
 {"encoder":"cpu","codec":"av1","ok":false,"reason":"too slow: 48 fps, 255.4 ms per frame"}],"size":"3840x2160@60"}"#;

    /// Parse a fixture as the sender prints it: stderr-style lines, then the JSON on one line.
    fn caps(json: &str) -> Matrix {
        Matrix::parse(&format!("[caps] native h264: ok\n{}", json.replace('\n', ""))).expect("parses")
    }

    fn m4k() -> Matrix {
        caps(CAPS_4K)
    }

    fn cpu_ok_1080() -> Matrix {
        caps(r#"{"results":[{"encoder":"cpu","codec":"hevc","ok":true,"reason":""}]}"#)
    }

    #[test]
    fn cpu_hevc_at_4k_is_blocked_with_a_reason_and_alternatives() {
        let b =
            m4k().blocked(Encoder::Cpu, Codec::Hevc, Picture::P2160x60, Some(&cpu_ok_1080()), None).expect("blocked");
        assert_eq!(
            b.message,
            "CPU x264 / x265 / SVT-AV1 (FFmpeg) can't encode HEVC at 4K60 on this PC (too slow, 40 fps). \
             Pick NVIDIA NVENC (FFmpeg) or NVIDIA NVENC native or Windows Media Foundation (FFmpeg) or 1080p60."
        );
        assert!(!b.hevc_store);
        // Without a 1080p probe, 1080p is not suggested.
        let b = m4k().blocked(Encoder::Cpu, Codec::Hevc, Picture::P2160x60, None, None).unwrap();
        assert!(!b.message.contains("1080p60"), "{}", b.message);
    }

    #[test]
    fn working_and_unknown_combinations_are_not_blocked() {
        let m = m4k();
        for (e, c) in [(Encoder::Nvenc, Codec::Hevc), (Encoder::NvencNative, Codec::Av1), (Encoder::Cpu, Codec::H264)] {
            assert_eq!(m.blocked(e, c, Picture::P2160x60, None, None), None, "{e:?} {c:?}");
        }
        // AV1 works on NVENC here, so Auto + AV1 is allowed (the TV has an AV1 decoder).
        assert_eq!(m.blocked(Encoder::Auto, Codec::Av1, Picture::P2160x60, None, None), None);
        // Nothing probed: nothing blocked (an older sender without `caps`).
        assert_eq!(Matrix::default().blocked(Encoder::Cpu, Codec::Hevc, Picture::P2160x60, None, None), None);
    }

    #[test]
    fn auto_codec_never_picks_one_that_cannot_run() {
        let m = m4k();
        assert_eq!(
            m.auto_codec(Encoder::Cpu, HEVC_FIRST),
            Some(Codec::H264),
            "CPU at 4K60: HEVC too slow, H.264 works"
        );
        assert_eq!(m.auto_codec(Encoder::Nvenc, HEVC_FIRST), Some(Codec::Hevc));
        assert_eq!(m.auto_codec(Encoder::Qsv, HEVC_FIRST), None);
        assert_eq!(m.blocked(Encoder::Cpu, Codec::Auto, Picture::P2160x60, None, None), None);
        let b = m.blocked(Encoder::Qsv, Codec::Auto, Picture::P2160x60, None, None).expect("QSV: nothing keeps up");
        assert!(!b.message.contains("any codec"), "the real causes, not a guess: {}", b.message);
        assert!(
            b.message.contains("H.264: too slow, 50 fps") && b.message.contains("AV1: it has no encoder"),
            "{}",
            b.message
        );
        assert!(b.message.contains("Pick NVIDIA NVENC (FFmpeg) or NVIDIA NVENC native"), "{}", b.message);
        let s = Settings { encoder: Encoder::Cpu, codec: Codec::Auto, ..Default::default() };
        assert_eq!(resolve_codec(&s, Some(&m)).codec, Codec::H264);
        assert_eq!(resolve_codec(&s, None).codec, Codec::Hevc, "nothing measured: the best for 1080p");
    }

    #[test]
    fn one_cause_on_every_codec_is_said_once() {
        let m = caps(
            r#"{"results":[{"encoder":"native","codec":"h264","ok":false,"reason":"NVENC session limit reached"},
                {"encoder":"native","codec":"hevc","ok":false,"reason":"NVENC session limit reached"},
                {"encoder":"native","codec":"av1","ok":false,"reason":"NVENC session limit reached"},
                {"encoder":"nvenc","codec":"hevc","ok":true,"reason":""}]}"#,
        );
        let b = m.blocked(Encoder::NvencNative, Codec::Auto, Picture::P2160x60, None, None).expect("blocked");
        assert_eq!(
            b.message,
            "NVIDIA NVENC native can't run at 4K60 on this PC (NVENC session limit reached). Pick NVIDIA NVENC (FFmpeg)."
        );
        // Nothing runs at all: no encoder to name, and no codec-picking advice that cannot help.
        let none = caps(
            r#"{"results":[{"encoder":"cpu","codec":"hevc","ok":false,"reason":"too slow: 20 fps, 90 ms per frame"}]}"#,
        );
        let b = none.blocked(Encoder::Auto, Codec::Auto, Picture::P2160x60, None, None);
        assert_eq!(b, None, "H.264 and AV1 were not tested: not ruled out");
        // A result an older sender left: the words name the cause.
        let old = caps(
            r#"{"results":[{"encoder":"native","codec":"hevc","ok":false,"reason":"the monitor is not rendered by an NVIDIA GPU"}]}"#,
        );
        let b = old.blocked(Encoder::NvencNative, Codec::Hevc, Picture::P2160x60, None, None).expect("blocked");
        assert!(b.message.contains("the monitor renders on another GPU"), "{}", b.message);
    }

    #[test]
    fn auto_encoder_is_blocked_only_when_nothing_runs_the_codec() {
        let none = caps(
            r#"{"results":[{"encoder":"nvenc","codec":"av1","ok":false,"reason":"no av1 encoder"},
                {"encoder":"cpu","codec":"av1","ok":false,"reason":"too slow: 30 fps, 100 ms per frame"}]}"#,
        );
        let b = none.blocked(Encoder::Auto, Codec::Av1, Picture::P1080x60, None, None).expect("blocked");
        assert_eq!(b.message, "No encoder on this PC can send AV1 at 1080p60. Pick another codec.");
    }

    #[test]
    fn missing_hevc_extension_shows_the_store() {
        let m = caps(
            r#"{"results":[{"encoder":"mf","codec":"hevc","ok":false,"reason":"could not find any MFT for the given media type"},
                {"encoder":"nvenc","codec":"hevc","ok":false,"reason":"no NVIDIA GPU"},
                {"encoder":"cpu","codec":"hevc","ok":false,"reason":"too slow: 20 fps, 90 ms per frame"}]}"#,
        );
        let b = m.blocked(Encoder::Auto, Codec::Hevc, Picture::P1080x60, None, None).expect("blocked");
        assert!(b.hevc_store, "{b:?}");
        // The other form the sender stores (codec prefix stripped).
        let m = caps(
            r#"{"results":[{"encoder":"mf","codec":"hevc","ok":false,"reason":"MFTEnumEx failed: 0xC00D5212"},
            {"encoder":"cpu","codec":"hevc","ok":false,"reason":"too slow: 20 fps, 90 ms per frame"}]}"#,
        );
        assert!(m.blocked(Encoder::Auto, Codec::Hevc, Picture::P1080x60, None, None).expect("blocked").hevc_store);
        // MF H.264 failing the same way is not the HEVC codec.
        let m = caps(r#"{"results":[{"encoder":"mf","codec":"h264","ok":false,"reason":"could not find any MFT"}]}"#);
        assert_eq!(
            m.blocked(Encoder::Auto, Codec::H264, Picture::P1080x60, None, None).map(|b| b.hevc_store),
            Some(false)
        );
        assert!(needs_hevc_extension("[encoder] probe failed: mf hevc_mf igpu copy: could not find any MFT"));
        assert!(needs_hevc_extension("hevc_mf: MFTEnumEx failed 0xC00D5212"));
        assert!(!needs_hevc_extension("x265 too slow"));
        assert!(!needs_hevc_extension("h264_mf: could not find any MFT"), "H.264 is built into Windows");
    }

    #[test]
    fn blocked_map_keys_use_setting_values() {
        let map = m4k().blocked_map(Picture::P2160x60, None, None);
        assert!(map.iter().any(|(k, _)| k == "cpu|hevc"));
        assert!(map.iter().any(|(k, _)| k == "qsv|auto"));
        assert!(!map.iter().any(|(k, _)| k == "nvenc_native|hevc"));
        assert!(!map.iter().any(|(k, _)| k == "auto|av1"), "AV1 is never greyed out when an encoder runs it");
    }

    /// A TV with a hardware AV1 decoder for 4K60 (the Hisense's CAPS, cut down).
    fn av1_tv() -> TvCaps {
        TvCaps::parse(r#"{"decoders":[{"name":"OMX.MTK.VIDEO.DECODER.AV1","mime":"video/av01","max_w":4096,"max_h":2304,"max_fps":60,"hw":true,"perf":["3840x2160@60"]}]}"#)
            .expect("caps")
    }

    #[test]
    fn auto_codec_follows_the_picture_and_the_tv() {
        let m = m4k();
        let tv = av1_tv();
        let mut s = Settings {
            tv_host: "tv.lan".into(),
            picture: Picture::P2160x60,
            tv_caps: Some(KnownTv { host: "tv.lan".into(), caps: tv.clone() }),
            ..Default::default()
        };
        assert_eq!(
            resolve_codec(&s, Some(&m)).codec,
            Codec::Hevc,
            "4K: HEVC, even on a TV with AV1 (run 2026-10-26-08)"
        );
        s.encoder = Encoder::NvencNative;
        assert_eq!(resolve_codec(&s, Some(&m)).codec, Codec::Hevc);
        s.encoder = Encoder::Cpu;
        assert_eq!(resolve_codec(&s, Some(&m)).codec, Codec::H264, "CPU can't do AV1 or HEVC at 4K: H.264");
        s.encoder = Encoder::Auto;
        s.picture = Picture::P1080x60;
        assert_eq!(resolve_codec(&s, Some(&m)).codec, Codec::Hevc, "1080p: HEVC (run 2026-10-26-09)");
        s.picture = Picture::P2160x60;
        s.tv_host = "other.lan".into();
        assert_eq!(resolve_codec(&s, Some(&m)).codec, Codec::Hevc, "another TV, not known yet: HEVC");
        s.codec = Codec::H264;
        assert_eq!(resolve_codec(&s, Some(&m)).codec, Codec::H264, "a manual pick stays");
        // HEVC cannot run: at 4K, AV1 comes next on a TV that decodes it.
        let no_hevc = caps(
            r#"{"results":[{"encoder":"native","codec":"hevc","ok":false,"reason":"no hevc encoder"},
            {"encoder":"native","codec":"av1","ok":true,"reason":""},{"encoder":"native","codec":"h264","ok":true,"reason":""}]}"#,
        );
        let s = Settings { encoder: Encoder::NvencNative, codec: Codec::Auto, tv_host: "tv.lan".into(), ..s };
        let s = Settings { tv_caps: Some(KnownTv { host: "tv.lan".into(), caps: tv.clone() }), ..s };
        assert_eq!(resolve_codec(&s, Some(&no_hevc)).codec, Codec::Av1);
        // Auto + Auto is blocked only by what Auto would really pick.
        assert_eq!(m.blocked(Encoder::Auto, Codec::Auto, Picture::P2160x60, None, Some(&tv)), None);
        assert!(m.blocked(Encoder::Qsv, Codec::Auto, Picture::P2160x60, None, Some(&tv)).is_some());
    }

    #[test]
    fn media_foundation_is_gated_like_the_others() {
        let m = m4k();
        assert_eq!(m.blocked(Encoder::Mf, Codec::Hevc, Picture::P2160x60, None, None), None, "mf hevc: ok");
        let b = m.blocked(Encoder::Mf, Codec::Av1, Picture::P2160x60, None, None).expect("no av1 encoder");
        assert!(b.message.starts_with("Windows Media Foundation (FFmpeg) can't encode AV1 at 4K60"), "{}", b.message);
        assert!(m.blocked_map(Picture::P2160x60, None, None).iter().any(|(k, _)| k == "mf|av1"));
        // QSV's suggestion list now names MF too (it works for HEVC here).
        let q = m.blocked(Encoder::Qsv, Codec::Hevc, Picture::P2160x60, None, None).expect("blocked");
        assert!(q.message.contains("Windows Media Foundation (FFmpeg)"), "{}", q.message);
    }

    #[test]
    fn reasons_in_plain_words() {
        assert_eq!(reason_words("too slow: 40 fps, needs 60"), "too slow, 40 fps");
        assert_eq!(reason_words("too slow: falling behind, latency 35 -> 120 ms"), "too slow, falling behind");
        assert_eq!(reason_words("no AMD GPU/driver (amfrt64.dll not found)"), "no AMD GPU/driver");
        assert_eq!(reason_words("DLL amfrt64.dll failed to open"), "no AMD GPU/driver");
        assert_eq!(reason_words("no av1 encoder"), "it has no encoder for this codec");
    }

    #[test]
    fn caps_command_line() {
        assert_eq!(
            caps_args(Picture::P2160x60, &Source::Monitor(2)).join(" "),
            "caps --monitor 2 --size 3840x2160 --fps 60"
        );
    }
}
