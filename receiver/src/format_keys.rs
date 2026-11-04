//! Which `AMediaFormat` keys the decoder gets, per decoder vendor, each switchable for A/B tests.
//!
//! Defaults follow Moonlight's `MediaCodecHelper`: MediaTek decoders get `vdec-lowlatency=1` (MTK's ACodec
//! key for `OMX.MTK.index.param.video.LowLatencyDecode`) and no `max-width` / `max-height` (on OMX with a
//! Surface those request adaptive playback, which Moonlight blacklists for `omx.mtk`).
//!
//! Every key can be forced on or off with a system property (`adb shell setprop <prop> 1|0`; empty = the
//! default), read when the decoder is created:
//!
//! | key | property | default |
//! |---|---|---|
//! | `low-latency=1` | `debug.lrt.key.lowlat` | on |
//! | `priority=0` | `debug.lrt.key.priority` | on |
//! | `operating-rate=N` | `debug.lrt.oprate` (N, 0 = off) | 120 |
//! | `vdec-lowlatency=1` | `debug.lrt.key.vdec` | on for MTK, else off |
//! | `vendor.low-latency.enable=1` | `debug.lrt.key.amlogic` | on |
//! | `max-width`/`max-height` | `debug.lrt.key.maxsize` | off for MTK, else on |
//!
//! `debug.lrt.lowlat=0` (older switch) turns off every latency key at once.

use std::fmt;

/// Default `operating-rate`.
pub(crate) const DEFAULT_OPERATING_RATE: u16 = 120;

/// One optional format key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Key {
    /// `low-latency=1` (standard, API 30; backported in this TV's ACodec).
    LowLatency,
    /// `priority=0` (realtime).
    Priority,
    /// `operating-rate=N` (frames/s the decoder clock should sustain).
    OperatingRate(u16),
    /// `vdec-lowlatency=1` (MediaTek).
    VdecLowLatency,
    /// `vendor.low-latency.enable=1` (Amlogic vendor extension).
    AmlogicLowLatency,
    /// `max-width` / `max-height` (adaptive playback on OMX + Surface).
    MaxSize,
}

impl Key {
    /// Short tag for logs and STATS.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Key::LowLatency => "lowlat",
            Key::Priority => "prio",
            Key::OperatingRate(_) => "oprate",
            Key::VdecLowLatency => "vdec",
            Key::AmlogicLowLatency => "amlogic",
            Key::MaxSize => "maxsize",
        }
    }

    /// Vendor-specific keys: the first to go if configure fails.
    fn is_vendor(self) -> bool {
        matches!(self, Key::VdecLowLatency | Key::AmlogicLowLatency)
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Key::LowLatency => f.write_str("low-latency=1"),
            Key::Priority => f.write_str("priority=0"),
            Key::OperatingRate(n) => write!(f, "operating-rate={n}"),
            Key::VdecLowLatency => f.write_str("vdec-lowlatency=1"),
            Key::AmlogicLowLatency => f.write_str("vendor.low-latency.enable=1"),
            Key::MaxSize => f.write_str("max-width/max-height"),
        }
    }
}

/// A per-key override from a system property.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Switch {
    #[default]
    Default,
    On,
    Off,
}

impl Switch {
    /// "1" = on, "0" = off, anything else (unset) = default.
    pub(crate) fn parse(s: &str) -> Switch {
        match s.trim() {
            "1" => Switch::On,
            "0" => Switch::Off,
            _ => Switch::Default,
        }
    }

    fn resolve(self, default: Use) -> Use {
        match self {
            Switch::Default => default,
            Switch::On => Use::Yes,
            Switch::Off => Use::No,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Use {
    Yes,
    No,
}

impl From<bool> for Use {
    fn from(b: bool) -> Use {
        if b {
            Use::Yes
        } else {
            Use::No
        }
    }
}

/// The key switches, as read from system properties.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Switches {
    /// `debug.lrt.lowlat=0` turns every latency key off.
    pub(crate) all_latency: Switch,
    pub(crate) low_latency: Switch,
    pub(crate) priority: Switch,
    /// `operating-rate` value; 0 = do not set.
    pub(crate) operating_rate: u16,
    pub(crate) vdec: Switch,
    pub(crate) amlogic: Switch,
    pub(crate) max_size: Switch,
}

impl Default for Switches {
    fn default() -> Switches {
        Switches {
            all_latency: Switch::Default,
            low_latency: Switch::Default,
            priority: Switch::Default,
            operating_rate: DEFAULT_OPERATING_RATE,
            vdec: Switch::Default,
            amlogic: Switch::Default,
            max_size: Switch::Default,
        }
    }
}

/// True for MediaTek decoders (`OMX.MTK.*`, `c2.mtk.*`).
pub(crate) fn is_mtk(codec_name: &str) -> bool {
    let n = codec_name.to_ascii_lowercase();
    n.starts_with("omx.mtk") || n.starts_with("c2.mtk")
}

/// The keys to set for decoder `codec_name`, in the order they are applied.
pub(crate) fn plan(codec_name: &str, sw: Switches) -> Vec<Key> {
    let mtk = is_mtk(codec_name);
    let latency = sw.all_latency != Switch::Off;
    let mut keys = Vec::new();
    if latency {
        if sw.low_latency.resolve(Use::Yes) == Use::Yes {
            keys.push(Key::LowLatency);
        }
        if sw.priority.resolve(Use::Yes) == Use::Yes {
            keys.push(Key::Priority);
        }
        if sw.operating_rate > 0 {
            keys.push(Key::OperatingRate(sw.operating_rate));
        }
        if sw.vdec.resolve(mtk.into()) == Use::Yes {
            keys.push(Key::VdecLowLatency);
        }
        if sw.amlogic.resolve(Use::Yes) == Use::Yes {
            keys.push(Key::AmlogicLowLatency);
        }
    }
    if sw.max_size.resolve((!mtk).into()) == Use::Yes {
        keys.push(Key::MaxSize);
    }
    keys
}

/// Key sets to try in order when `configure` fails (Moonlight retries with fewer keys too): all keys;
/// then without vendor keys and `operating-rate`; then `low-latency` alone; then none. Duplicates removed.
pub(crate) fn ladder(keys: &[Key]) -> Vec<Vec<Key>> {
    let all = keys.to_vec();
    let no_vendor: Vec<Key> =
        keys.iter().copied().filter(|k| !k.is_vendor() && !matches!(k, Key::OperatingRate(_))).collect();
    let minimal: Vec<Key> = keys.iter().copied().filter(|k| *k == Key::LowLatency).collect();
    let mut out: Vec<Vec<Key>> = Vec::new();
    for set in [all, no_vendor, minimal, Vec::new()] {
        if !out.contains(&set) {
            out.push(set);
        }
    }
    out
}

/// The rung after `rung` in a ladder of `rungs` key sets, or None if `rung` is the last one.
pub(crate) fn next_rung(rung: usize, rungs: usize) -> Option<usize> {
    let next = rung + 1;
    (next < rungs).then_some(next)
}

/// "low-latency=1 priority=0 ..." for logs.
pub(crate) fn describe(keys: &[Key]) -> String {
    if keys.is_empty() {
        return "(none)".into();
    }
    keys.iter().map(ToString::to_string).collect::<Vec<_>>().join(" ")
}

/// "lowlat,prio,vdec" for STATS.
pub(crate) fn tags(keys: &[Key]) -> String {
    keys.iter().map(|k| k.tag()).collect::<Vec<_>>().join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MTK: &str = "OMX.MTK.VIDEO.DECODER.HEVC";

    #[test]
    fn next_rung_stops_at_the_last_key_set() {
        assert_eq!(next_rung(0, 4), Some(1));
        assert_eq!(next_rung(2, 4), Some(3));
        assert_eq!(next_rung(3, 4), None);
        assert_eq!(next_rung(0, 1), None);
    }

    #[test]
    fn mtk_detection() {
        assert!(is_mtk(MTK));
        assert!(is_mtk("c2.mtk.hevc.decoder"));
        assert!(!is_mtk("OMX.amlogic.hevc.decoder.awesome2"));
        assert!(!is_mtk("video/hevc (by type)"));
        // MIME fallback with the picked component known (API 28+): still MediaTek.
        assert!(is_mtk("c2.mtk.hevc.decoder (by type)"));
        assert_eq!(
            plan("c2.mtk.hevc.decoder (by type)", Switches::default()),
            plan("c2.mtk.hevc.decoder", Switches::default())
        );
    }

    #[test]
    fn mtk_defaults_match_moonlight() {
        let k = plan(MTK, Switches::default());
        assert_eq!(
            k,
            [Key::LowLatency, Key::Priority, Key::OperatingRate(120), Key::VdecLowLatency, Key::AmlogicLowLatency]
        );
        assert!(!k.contains(&Key::MaxSize));
    }

    #[test]
    fn other_vendor_defaults() {
        let k = plan("OMX.qcom.video.decoder.hevc", Switches::default());
        assert!(!k.contains(&Key::VdecLowLatency));
        assert!(k.contains(&Key::MaxSize));
    }

    #[test]
    fn each_key_switches() {
        let off = Switches {
            low_latency: Switch::Off,
            priority: Switch::Off,
            operating_rate: 0,
            vdec: Switch::Off,
            amlogic: Switch::Off,
            ..Switches::default()
        };
        assert_eq!(plan(MTK, off), Vec::<Key>::new());
        let on = Switches { max_size: Switch::On, ..Switches::default() };
        assert!(plan(MTK, on).contains(&Key::MaxSize));
        let vdec_off = Switches { vdec: Switch::Off, ..Switches::default() };
        assert!(!plan(MTK, vdec_off).contains(&Key::VdecLowLatency));
        let vdec_on = Switches { vdec: Switch::On, ..Switches::default() };
        assert!(plan("OMX.other", vdec_on).contains(&Key::VdecLowLatency));
    }

    #[test]
    fn lowlat_zero_turns_off_all_latency_keys_only() {
        let sw = Switches { all_latency: Switch::Off, max_size: Switch::On, ..Switches::default() };
        assert_eq!(plan(MTK, sw), [Key::MaxSize]);
    }

    #[test]
    fn switch_parse() {
        assert_eq!(Switch::parse("1"), Switch::On);
        assert_eq!(Switch::parse(" 0\n"), Switch::Off);
        assert_eq!(Switch::parse(""), Switch::Default);
        assert_eq!(Switch::parse("yes"), Switch::Default);
    }

    #[test]
    fn ladder_drops_vendor_keys_first() {
        let k = plan(MTK, Switches::default());
        let l = ladder(&k);
        assert_eq!(l[0], k);
        assert_eq!(l[1], [Key::LowLatency, Key::Priority]);
        assert_eq!(l[2], [Key::LowLatency]);
        assert_eq!(l[3], Vec::<Key>::new());
        assert_eq!(l.len(), 4);
        // No duplicates when a step changes nothing.
        assert_eq!(ladder(&[Key::LowLatency]), [vec![Key::LowLatency], vec![]]);
        assert_eq!(ladder(&[]), [Vec::<Key>::new()]);
    }

    #[test]
    fn text_forms() {
        let k = [Key::LowLatency, Key::VdecLowLatency, Key::OperatingRate(60)];
        assert_eq!(describe(&k), "low-latency=1 vdec-lowlatency=1 operating-rate=60");
        assert_eq!(tags(&k), "lowlat,vdec,oprate");
        assert_eq!(describe(&[]), "(none)");
    }
}
