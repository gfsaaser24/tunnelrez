//! User settings, stored as JSON in `%APPDATA%\TunnelRez\settings.json`.
//!
//! Unknown or missing fields fall back to defaults, so old files keep loading after the app grows new settings.
//! Before the rename the folder was `%APPDATA%\SlipStream`: the first start copies it once (settings, logs) and
//! leaves the old folder in place.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::tvcaps::{KnownTv, TvCaps};

/// What the TV shows.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    /// A virtual monitor: the TV is a second screen.
    #[default]
    Extend,
    /// A copy of one physical monitor.
    Mirror,
}

/// Output resolution and frame rate.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum Picture {
    #[default]
    // 60 Hz only: an old `1080p120` setting loads as 1080p60.
    #[serde(rename = "1080p60", alias = "1080p120")]
    P1080x60,
    #[serde(rename = "4k60")]
    P2160x60,
}

impl Picture {
    pub(crate) fn size(self) -> (u32, u32) {
        match self {
            Self::P1080x60 => (1920, 1080),
            Self::P2160x60 => (3840, 2160),
        }
    }

    /// Frames per second: 60 for every picture.
    pub(crate) const FPS: u32 = 60;

    /// Short label for the tray tooltip ("4K60").
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::P1080x60 => "1080p60",
            Self::P2160x60 => "4K60",
        }
    }

    /// The bitrate for this picture until the user moves the slider: see [`BITRATE_TABLE`].
    pub(crate) fn default_mbps(self) -> u32 {
        self.rates().default_mbps
    }

    fn rates(self) -> &'static PictureRates {
        BITRATE_TABLE.iter().find(|r| r.picture == self).expect("BITRATE_TABLE has a row for every picture")
    }
}

/// The bitrate numbers of one picture.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PictureRates {
    pub(crate) picture: Picture,
    /// Used until the user moves the slider, Mb/s.
    pub(crate) default_mbps: u32,
    /// Above this the panel says the rate adds delay on Wi-Fi for little gain, Mb/s; `None` = never.
    pub(crate) high_mbps: Option<u32>,
}

/// Bitrate per picture. Change the numbers here; the panel and the sender command line follow.
///
/// Where they come from (real TV over Wi-Fi, issue #14 and the run book): a fixed rate pads every frame up to
/// bitrate / fps, so on Wi-Fi a higher rate means bigger frames and more delay.
/// - 1080p60: native HEVC was stable at 75 Mb/s and bounced into yellow at 150; `FFmpeg` NVENC HEVC was under
///   20 ms at 25 Mb/s, under 30 ms at 75 and yellow at 150. 40 Mb/s keeps the picture clean with headroom; above
///   80 Mb/s the delay grows with little visible gain.
/// - 4K60: native HEVC at 150 Mb/s ran clean at 59.9 fps (run book 2026-10-26-01 and -02). 100 Mb/s is the
///   default; the slider's maximum (150) is still fine there, so no hint.
pub(crate) const BITRATE_TABLE: [PictureRates; 2] = [
    PictureRates { picture: Picture::P1080x60, default_mbps: 40, high_mbps: Some(80) },
    PictureRates { picture: Picture::P2160x60, default_mbps: 100, high_mbps: None },
];

/// The panel's hint when `mbps` is far above what `picture` needs, else `None`.
pub(crate) fn high_bitrate_hint(picture: Picture, mbps: u32) -> Option<String> {
    let high = picture.rates().high_mbps?;
    let name = match picture {
        Picture::P1080x60 => "1080p",
        Picture::P2160x60 => "4K",
    };
    (mbps > high).then(|| format!("High bitrate at {name} adds delay on Wi-Fi with little visible gain"))
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Codec {
    /// The best codec for the picture on this TV: HEVC at 1080p and 4K (measured, see `tvcaps`). Only among the
    /// codecs the encoder check says can run.
    #[default]
    Auto,
    Hevc,
    H264,
    Av1,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Encoder {
    /// Let the sender pick (it probes and falls back on its own when it can).
    #[default]
    Auto,
    Nvenc,
    /// In-process capture straight into the NVENC SDK (`--backend native`).
    NvencNative,
    Qsv,
    Amf,
    /// Windows Media Foundation through `FFmpeg` (`--encoder mf`): the GPU's encoder through Windows' own API.
    Mf,
    Cpu,
}

impl Encoder {
    /// The panel's name for the choice; it says which paths go through `FFmpeg`. Settings values do not change.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Encoder::Auto => "Auto",
            Encoder::NvencNative => "NVIDIA NVENC native",
            Encoder::Nvenc => "NVIDIA NVENC (FFmpeg)",
            Encoder::Qsv => "Intel Quick Sync (FFmpeg)",
            Encoder::Amf => "AMD AMF (FFmpeg)",
            Encoder::Mf => "Windows Media Foundation (FFmpeg)",
            Encoder::Cpu => "CPU x264 / x265 / SVT-AV1 (FFmpeg)",
        }
    }
}

/// The sender's `codec_out` (`hevc/native`, `h264/x264 copy`, `hevc/ffmpeg nvenc`) in the panel's words:
/// `HEVC · NVIDIA NVENC native`, `H.264 · CPU x264 (FFmpeg)`.
pub(crate) fn codec_out_words(codec_out: &str) -> String {
    let (codec, rest) = codec_out.split_once('/').unwrap_or((codec_out, ""));
    let c = match codec {
        "hevc" => "HEVC".to_string(),
        "h264" => "H.264".to_string(),
        "av1" => "AV1".to_string(),
        other => other.to_ascii_uppercase(),
    };
    let e = rest.split_whitespace().find_map(|w| match w {
        "native" => Some("NVIDIA NVENC native"),
        "nvenc" => Some("NVIDIA NVENC (FFmpeg)"),
        "qsv" => Some("Intel Quick Sync (FFmpeg)"),
        "amf" => Some("AMD AMF (FFmpeg)"),
        "mf" => Some("Windows Media Foundation (FFmpeg)"),
        "x264" | "libx264" => Some("CPU x264 (FFmpeg)"),
        "x265" | "libx265" => Some("CPU x265 (FFmpeg)"),
        "svtav1" | "libsvtav1" => Some("CPU SVT-AV1 (FFmpeg)"),
        _ => None,
    });
    e.map_or(c.clone(), |e| format!("{c} · {e}"))
}

/// What happens to the PC's sound while streaming.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AudioMode {
    /// The PC's sound plays on the TV (and on the PC too). The sender's own default, so no flag is passed.
    #[default]
    On,
    /// No sound is sent (`--audio off`).
    Off,
    /// "Play on TV only": the sender mutes this PC's speakers while it streams (`--audio tv-only`).
    TvOnly,
}

impl AudioMode {
    /// The sender's `--audio` value for this choice, when it is not the sender's default.
    pub(crate) fn flag_value(self) -> Option<&'static str> {
        match self {
            Self::On => None,
            Self::Off => Some("off"),
            Self::TvOnly => Some("tv-only"),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct Settings {
    /// TV address (host or IP). Empty until the first run asks for it.
    pub(crate) tv_host: String,
    pub(crate) mode: Mode,
    /// GDI device name of the monitor to mirror; `None` = the primary monitor.
    pub(crate) mirror_monitor: Option<String>,
    pub(crate) picture: Picture,
    pub(crate) codec: Codec,
    pub(crate) encoder: Encoder,
    /// Target bitrate. Stored as a number (Mb/s) or `"auto"`; `null` (the old "not set") loads as the default.
    pub(crate) bitrate_mbps: Bitrate,
    /// Sound on the TV: on (default), off, or on with this PC's speakers muted. A file without it means on.
    pub(crate) audio: AudioMode,
    /// The virtual playback device the sender streams from (`--audio-sink`): `""` = auto (the sender picks one if
    /// present), `"off"` = capture the PC speakers, else a device name. A file without it means auto.
    pub(crate) audio_sink: String,
    /// Explicit sender executable; `None` = look next to the app, then in a source checkout.
    pub(crate) sender_path: Option<PathBuf>,
    /// The first-run screen was completed.
    pub(crate) setup_done: bool,
    /// The panel stays open when it loses focus (a normal window the user can drag by its header), instead of the
    /// Wi-Fi-flyout behaviour (hide on blur, anchored above the tray).
    pub(crate) keep_panel_open: bool,
    /// Where the user last left the kept-open panel: the window's top-left corner, physical px.
    pub(crate) panel_pos: Option<(i32, i32)>,
    /// File format of the logger (the panel's Log switch).
    pub(crate) log_format: LogFormat,
    /// What the TV said it decodes (its CAPS) the last time it was connected, with its address. Not settable from
    /// the panel; Auto codec reads it.
    pub(crate) tv_caps: Option<KnownTv>,
    /// Layout version of the file; a file without it is version 1 (see [`Settings::migrate`]).
    #[serde(default = "legacy_schema")]
    pub(crate) schema: u32,
}

/// The current settings file layout. 2: `bitrate_mbps` can be `"default"` (the picture's own rate).
const SCHEMA: u32 = 2;

fn legacy_schema() -> u32 {
    1
}

/// The logger's file format.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LogFormat {
    /// Readable: tables and lists.
    #[default]
    Markdown,
    /// For graphs and the run book.
    Json,
}

impl LogFormat {
    pub(crate) fn extension(self) -> &'static str {
        match self {
            Self::Markdown => "md",
            Self::Json => "json",
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            tv_host: String::new(),
            mode: Mode::default(),
            mirror_monitor: None,
            picture: Picture::default(),
            codec: Codec::default(),
            encoder: Encoder::default(),
            bitrate_mbps: Bitrate::default(),
            audio: AudioMode::default(),
            audio_sink: String::new(),
            sender_path: None,
            setup_done: false,
            keep_panel_open: true,
            panel_pos: None,
            log_format: LogFormat::default(),
            tv_caps: None,
            schema: SCHEMA,
        }
    }
}

/// Bitrate range the panel offers, Mb/s.
pub(crate) const BITRATE_MIN: u32 = 10;
pub(crate) const BITRATE_MAX: u32 = 150;
/// What files before [`SCHEMA`] 2 saved when the user had not picked a rate (the old one-size default).
const LEGACY_DEFAULT_MBPS: u32 = 100;
/// The longest `audio_sink` the panel may set, in characters (a device name or endpoint id is far shorter).
const AUDIO_SINK_MAX: usize = 256;

/// The stream's target bitrate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Bitrate {
    /// Not picked by the user: the picture's own rate from [`BITRATE_TABLE`], as a fixed rate. It follows the
    /// picture when the user switches 1080p / 4K.
    #[default]
    Default,
    /// The sender's default for the codec and size, as a ceiling (simple content sends less).
    Auto,
    /// The user moved the slider: this many Mb/s, really sent (the sender pads simple content up to it). Kept when
    /// the picture changes.
    Mbps(u32),
}

impl Bitrate {
    /// A fixed rate clamped to the panel's range.
    pub(crate) fn mbps(v: u32) -> Self {
        Self::Mbps(v.clamp(BITRATE_MIN, BITRATE_MAX))
    }

    /// The fixed rate to send at `picture`, Mb/s; `None` for Auto (the sender decides).
    pub(crate) fn effective(self, picture: Picture) -> Option<u32> {
        match self {
            Self::Default => Some(picture.default_mbps()),
            Self::Auto => None,
            Self::Mbps(v) => Some(v),
        }
    }

    /// From the stored or panel form: a number, `"auto"`, `"default"`, or `null` (the default). `None` for
    /// anything else.
    fn from_json(v: &serde_json::Value) -> Option<Self> {
        match v {
            serde_json::Value::Null => Some(Self::Default),
            serde_json::Value::String(s) if s.eq_ignore_ascii_case("auto") => Some(Self::Auto),
            serde_json::Value::String(s) if s.eq_ignore_ascii_case("default") => Some(Self::Default),
            serde_json::Value::Number(n) => n.as_u64().map(|v| Self::mbps(u32::try_from(v).unwrap_or(u32::MAX))),
            _ => None,
        }
    }
}

impl Serialize for Bitrate {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Default => s.serialize_str("default"),
            Self::Auto => s.serialize_str("auto"),
            Self::Mbps(v) => s.serialize_u32(*v),
        }
    }
}

impl<'de> Deserialize<'de> for Bitrate {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        // An unreadable value falls back to the default rather than discarding the whole settings file.
        Ok(Self::from_json(&v).unwrap_or_default())
    }
}

/// The settings folder's name under `%APPDATA%`.
const DIR: &str = "TunnelRez";
/// Its name before the rename.
const LEGACY_DIR: &str = "SlipStream";
/// The folder this run uses and what the one-time copy did (see [`Settings::dir`]).
static MIGRATION: std::sync::OnceLock<(PathBuf, Option<String>)> = std::sync::OnceLock::new();

/// Copy `old` to `new` once: only when `new` does not exist yet, through a temporary folder so a crash never leaves
/// half a copy that would block the next try. Returns whether it copied.
///
/// # Errors
/// When the copy fails; `new` is then still absent.
fn migrate_dir(old: &Path, new: &Path) -> std::io::Result<bool> {
    if !old.is_dir() || new.exists() {
        return Ok(false);
    }
    let tmp = new.with_file_name(format!("{DIR}.migrating-{}", std::process::id()));
    let done = copy_tree(old, &tmp).and_then(|()| std::fs::rename(&tmp, new));
    let _ = std::fs::remove_dir_all(&tmp);
    match done {
        Ok(()) => Ok(true),
        // Another copy of the app finished first: its copy is as good as ours.
        Err(_) if new.is_dir() => Ok(false),
        Err(e) => Err(e),
    }
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Copy `old` to `new` if needed and say which folder to use now, with a note for the log.
fn choose_dir(old: &Path, new: &Path) -> (PathBuf, Option<String>) {
    match migrate_dir(old, new) {
        Ok(true) => (new.to_path_buf(), Some(format!("copied {} to {}", old.display(), new.display()))),
        Ok(false) => (new.to_path_buf(), None),
        Err(e) => (
            old.to_path_buf(),
            Some(format!("could not copy {} to {}: {e}; using the old folder for now", old.display(), new.display())),
        ),
    }
}

impl Settings {
    /// `%APPDATA%\TunnelRez`, after the one-time copy from `%APPDATA%\SlipStream`. When that copy fails, this run
    /// keeps using the old folder: writing the log or the settings into a new folder would make the next start take
    /// it as a finished copy and leave the saved settings behind.
    pub(crate) fn dir() -> PathBuf {
        // Not logged here: `core::log` opens its file in this folder, so it would come back into this call.
        MIGRATION
            .get_or_init(|| {
                let base = std::env::var_os("APPDATA").map_or_else(std::env::temp_dir, PathBuf::from);
                choose_dir(&base.join(LEGACY_DIR), &base.join(DIR))
            })
            .0
            .clone()
    }

    /// What the one-time copy from `%APPDATA%\SlipStream` did, for the app log (`None`: nothing to copy).
    pub(crate) fn migration_note() -> Option<&'static str> {
        let _ = Self::dir();
        MIGRATION.get().and_then(|m| m.1.as_deref())
    }

    fn file() -> PathBuf {
        Self::dir().join("settings.json")
    }

    /// Load the settings file, or defaults when it is missing or unreadable.
    pub(crate) fn load() -> Self {
        Self::load_from(&Self::file())
    }

    fn load_from(path: &Path) -> Self {
        let s: Self = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        s.migrate()
    }

    /// Bring a file of an older layout up to [`SCHEMA`]. Version 1 always saved a number, so its default (100)
    /// cannot be told apart from a pick; it becomes [`Bitrate::Default`]. Any other number was the user's pick.
    #[must_use]
    fn migrate(mut self) -> Self {
        if self.schema < 2 && self.bitrate_mbps == Bitrate::Mbps(LEGACY_DEFAULT_MBPS) {
            self.bitrate_mbps = Bitrate::Default;
        }
        self.schema = SCHEMA;
        self
    }

    /// Save atomically (write a temp file, then rename over the old one).
    ///
    /// # Errors
    /// When the folder or file cannot be written.
    pub(crate) fn save(&self) -> Result<()> {
        self.save_to(&Self::file())
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("settings path has no folder")?;
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    }

    /// The CAPS of the TV at [`Settings::tv_host`], when it has been connected before.
    pub(crate) fn tv(&self) -> Option<&TvCaps> {
        self.tv_caps.as_ref().filter(|k| !self.tv_host.is_empty() && k.host == self.tv_host).map(|k| &k.caps)
    }

    /// Remember `caps` for the current TV. Returns whether that changed anything.
    pub(crate) fn remember_tv(&mut self, caps: TvCaps) -> bool {
        let known = KnownTv { host: self.tv_host.clone(), caps };
        if self.tv_caps.as_ref() == Some(&known) {
            return false;
        }
        self.tv_caps = Some(known);
        true
    }

    /// The sender's `--audio-sink` value, or `None` for auto (the sender's default: no flag).
    pub(crate) fn audio_sink_flag(&self) -> Option<&str> {
        Some(self.audio_sink.as_str()).filter(|v| !v.is_empty())
    }

    /// Apply one change from the panel. Returns false when the key or value is not valid.
    pub(crate) fn set(&mut self, key: &str, value: serde_json::Value) -> bool {
        fn get<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Option<T> {
            serde_json::from_value(v).ok()
        }
        match key {
            "tv_host" => get::<String>(value).map(|h| self.tv_host = h.trim().to_string()).is_some(),
            "mode" => get(value).map(|m| self.mode = m).is_some(),
            "mirror_monitor" => get(value).map(|m| self.mirror_monitor = m).is_some(),
            "picture" => get(value).map(|p| self.picture = p).is_some(),
            "codec" => get(value).map(|c| self.codec = c).is_some(),
            "encoder" => get(value).map(|e| self.encoder = e).is_some(),
            "audio" => get(value).map(|a| self.audio = a).is_some(),
            "audio_sink" => {
                get::<String>(value).and_then(|v| audio_sink_value(&v)).map(|v| self.audio_sink = v).is_some()
            }
            "keep_panel_open" => get(value).map(|k| self.keep_panel_open = k).is_some(),
            "log_format" => get(value).map(|f| self.log_format = f).is_some(),
            "bitrate_mbps" => Bitrate::from_json(&value).map(|b| self.bitrate_mbps = b).is_some(),
            _ => false,
        }
    }
}

/// A panel value for `audio_sink`, trimmed; `"auto"` is stored as `""`. `None` when too long or not one line.
fn audio_sink_value(v: &str) -> Option<String> {
    let v = v.trim();
    if v.chars().count() > AUDIO_SINK_MAX || v.chars().any(char::is_control) {
        return None;
    }
    Some(if v.eq_ignore_ascii_case("auto") { String::new() } else { v.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_have_no_tv_host() {
        let s = Settings::default();
        assert!(s.tv_host.is_empty());
        assert_eq!(s.picture, Picture::P1080x60);
        assert!(!s.setup_done);
        assert!(s.keep_panel_open, "keep open is on by default");
        assert_eq!(s.panel_pos, None);
        assert_eq!(s.bitrate_mbps, Bitrate::Default, "the picture's own rate, not Auto, by default");
        assert_eq!(s.log_format, LogFormat::Markdown);
    }

    #[test]
    fn default_bitrate_follows_the_picture() {
        assert_eq!(Bitrate::Default.effective(Picture::P1080x60), Some(40));
        assert_eq!(Bitrate::Default.effective(Picture::P2160x60), Some(100));
        assert_eq!(Bitrate::Auto.effective(Picture::P2160x60), None, "Auto: the sender decides");
        // A rate the user picked stays when the picture changes.
        assert_eq!(Bitrate::Mbps(60).effective(Picture::P1080x60), Some(60));
        assert_eq!(Bitrate::Mbps(60).effective(Picture::P2160x60), Some(60));
        let mut s = Settings::default();
        assert!(s.set("picture", json!("4k60")));
        assert_eq!(s.bitrate_mbps.effective(s.picture), Some(100), "not picked: 4K's own rate");
        assert!(s.set("bitrate_mbps", json!(120)));
        assert!(s.set("picture", json!("1080p60")));
        assert_eq!(s.bitrate_mbps.effective(s.picture), Some(120), "picked on purpose: kept");
        assert!(s.set("bitrate_mbps", json!("default")));
        assert_eq!(s.bitrate_mbps.effective(s.picture), Some(40), "back to the picture's own rate");
        for row in BITRATE_TABLE {
            assert!((BITRATE_MIN..=BITRATE_MAX).contains(&row.default_mbps), "{row:?}");
            assert!(row.high_mbps.is_none_or(|h| h > row.default_mbps), "{row:?}");
        }
    }

    #[test]
    fn high_bitrate_hint_only_far_above_the_need() {
        assert_eq!(high_bitrate_hint(Picture::P1080x60, 80), None);
        assert_eq!(
            high_bitrate_hint(Picture::P1080x60, 85).as_deref(),
            Some("High bitrate at 1080p adds delay on Wi-Fi with little visible gain")
        );
        assert_eq!(high_bitrate_hint(Picture::P2160x60, BITRATE_MAX), None, "4K at 150 ran clean");
    }

    #[test]
    fn the_old_folder_is_copied_once() {
        let root = std::env::temp_dir().join(format!("tunnelrez-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (old, new) = (root.join(LEGACY_DIR), root.join(DIR));
        std::fs::create_dir_all(old.join("logs")).unwrap();
        let saved = serde_json::json!({"schema": 2, "tv_host": "192.0.2.20", "bitrate_mbps": 75});
        std::fs::write(old.join("settings.json"), saved.to_string()).unwrap();
        std::fs::write(old.join("logs").join("a.md"), "log").unwrap();

        assert!(migrate_dir(&old, &new).unwrap());
        let s = Settings::load_from(&new.join("settings.json"));
        assert_eq!(s.tv_host, "192.0.2.20", "values are kept");
        assert_eq!(s.bitrate_mbps, Bitrate::Mbps(75));
        assert_eq!(std::fs::read_to_string(new.join("logs").join("a.md")).unwrap(), "log");
        assert!(old.join("settings.json").is_file(), "the old folder stays");

        // Once only: a later start keeps what the new folder has.
        std::fs::write(new.join("settings.json"), "{}").unwrap();
        assert!(!migrate_dir(&old, &new).unwrap());
        assert_eq!(std::fs::read_to_string(new.join("settings.json")).unwrap(), "{}");
        assert!(!migrate_dir(&root.join("missing"), &root.join("other")).unwrap());

        // A copy that fails keeps this run in the old folder (here: the new path is a file, so no folder can go there).
        let blocked = root.join("blocked");
        std::fs::write(&blocked, "x").unwrap();
        let (used, note) = choose_dir(&old, &root.join("blocked").join("TunnelRez"));
        assert_eq!(used, old, "{note:?}");
        assert!(note.is_some_and(|n| n.contains("using the old folder")));
        assert_eq!(choose_dir(&old, &new).0, new, "an existing new folder is used");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn old_files_migrate() {
        let dir = std::env::temp_dir().join(format!("tunnelrez-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let load = |v: serde_json::Value| {
            std::fs::write(&path, v.to_string()).unwrap();
            Settings::load_from(&path)
        };
        // Version 1 always saved a number; 100 was its default, so it is read as "not picked".
        assert_eq!(load(json!({"bitrate_mbps": 100})).bitrate_mbps, Bitrate::Default);
        assert_eq!(load(json!({"bitrate_mbps": 150})).bitrate_mbps, Bitrate::Mbps(150), "a real pick is kept");
        assert_eq!(load(json!({"bitrate_mbps": "auto"})).bitrate_mbps, Bitrate::Auto);
        let s = load(json!({"bitrate_mbps": 100, "schema": 2}));
        assert_eq!(s.bitrate_mbps, Bitrate::Mbps(100), "a version 2 file's 100 is a pick");
        assert_eq!(load(json!({})).schema, SCHEMA);
        let _ = std::fs::remove_dir_all(dir);
        // Older apps read "default" as their own default, so a new file still loads there.
        assert_eq!(serde_json::to_value(Bitrate::Default).unwrap(), json!("default"));
    }

    #[test]
    fn encoder_labels_say_which_path_uses_ffmpeg() {
        assert_eq!(codec_out_words("hevc/native"), "HEVC · NVIDIA NVENC native");
        assert_eq!(codec_out_words("h264/nvenc zero-copy"), "H.264 · NVIDIA NVENC (FFmpeg)");
        assert_eq!(codec_out_words("hevc/ffmpeg x265"), "HEVC · CPU x265 (FFmpeg)");
        assert_eq!(codec_out_words("av1/svtav1 copy"), "AV1 · CPU SVT-AV1 (FFmpeg)");
        assert_eq!(codec_out_words("hevc/mf copy"), "HEVC · Windows Media Foundation (FFmpeg)");
        assert_eq!(Encoder::Nvenc.label(), "NVIDIA NVENC (FFmpeg)");
        assert_eq!(Encoder::NvencNative.label(), "NVIDIA NVENC native");
        // The stored values stay the same, so old settings files load.
        assert_eq!(serde_json::to_value(Encoder::NvencNative).unwrap(), json!("nvenc_native"));
        assert_eq!(serde_json::from_value::<Encoder>(json!("cpu")).unwrap(), Encoder::Cpu);
        // Media Foundation (issue #23): its own value and label.
        assert_eq!(serde_json::to_value(Encoder::Mf).unwrap(), json!("mf"));
        assert_eq!(Encoder::Mf.label(), "Windows Media Foundation (FFmpeg)");
        let mut s = Settings::default();
        assert!(s.set("encoder", json!("mf")));
        assert_eq!(s.encoder, Encoder::Mf);
    }

    #[test]
    fn tv_caps_are_kept_per_tv_and_old_files_load() {
        let s: Settings = serde_json::from_value(json!({"tv_host": "tv.lan", "encoder": "qsv"})).unwrap();
        assert_eq!(s.tv_caps, None, "a file from before this field");
        assert_eq!(s.encoder, Encoder::Qsv);
        let mut s = Settings { tv_host: "tv.lan".into(), ..Default::default() };
        let caps =
            TvCaps::parse(r#"{"decoders":[{"name":"OMX.A.AV1","mime":"video/av01","max_w":4096,"max_h":2304}]}"#)
                .unwrap();
        assert!(s.remember_tv(caps.clone()));
        assert!(!s.remember_tv(caps.clone()), "the same again: no change");
        assert_eq!(s.tv(), Some(&caps));
        let back: Settings = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(back.tv(), Some(&caps), "saved and loaded");
        s.tv_host = "other.lan".into();
        assert_eq!(s.tv(), None, "another TV: not known");
        assert!(!s.set("tv_caps", json!(null)), "not settable from the panel");
    }

    #[test]
    fn bitrate_file_forms() {
        let load = |v: serde_json::Value| {
            serde_json::from_value::<Settings>(json!({ "bitrate_mbps": v })).unwrap().bitrate_mbps
        };
        assert_eq!(load(json!(null)), Bitrate::Default, "old files saved null for 'not set'");
        assert_eq!(load(json!(40)), Bitrate::Mbps(40));
        assert_eq!(load(json!(5)), Bitrate::Mbps(10), "old 5 Mb/s floor: clamped to the new range");
        assert_eq!(load(json!("auto")), Bitrate::Auto);
        assert_eq!(load(json!("default")), Bitrate::Default);
        assert_eq!(load(json!({"x": 1})), Bitrate::Default, "unreadable: default, not a lost file");
        let missing: Settings = serde_json::from_value(json!({"tv_host": "tv"})).unwrap();
        assert_eq!(missing.bitrate_mbps, Bitrate::Default);
        assert_eq!(serde_json::to_value(Bitrate::Auto).unwrap(), json!("auto"));
        assert_eq!(serde_json::to_value(Bitrate::Mbps(150)).unwrap(), json!(150));
    }

    #[test]
    fn audio_defaults_to_on_and_old_files_load() {
        assert_eq!(Settings::default().audio, AudioMode::On);
        let old: Settings = serde_json::from_value(json!({"tv_host": "tv.lan", "setup_done": true})).unwrap();
        assert_eq!(old.audio, AudioMode::On, "a file from before audio means on");
        let off: Settings = serde_json::from_value(json!({"audio": "off"})).unwrap();
        assert_eq!(off.audio, AudioMode::Off);
        let tv_only: Settings = serde_json::from_value(json!({"audio": "tv_only"})).unwrap();
        assert_eq!(tv_only.audio, AudioMode::TvOnly);
        assert_eq!(serde_json::to_value(AudioMode::TvOnly).unwrap(), json!("tv_only"));
        assert_eq!(serde_json::to_value(Settings::default()).unwrap()["audio"], json!("on"));
        assert_eq!(AudioMode::On.flag_value(), None);
        assert_eq!(AudioMode::Off.flag_value(), Some("off"));
        assert_eq!(AudioMode::TvOnly.flag_value(), Some("tv-only"));
    }

    #[test]
    fn audio_old_file_in_a_temp_dir_loads() {
        let dir = std::env::temp_dir().join(format!("tunnelrez-audio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, json!({"tv_host": "tv.lan", "picture": "4k60"}).to_string()).unwrap();
        assert_eq!(Settings::load_from(&path).audio, AudioMode::On);
        let s = Settings { audio: AudioMode::TvOnly, ..Default::default() };
        s.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path).audio, AudioMode::TvOnly, "survives a save");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn audio_is_settable_from_the_panel() {
        let mut s = Settings::default();
        assert!(s.set("audio", json!("tv_only")));
        assert_eq!(s.audio, AudioMode::TvOnly);
        assert!(s.set("audio", json!("off")));
        assert_eq!(s.audio, AudioMode::Off);
        assert!(!s.set("audio", json!("loud")));
        assert!(!s.set("audio", json!(true)));
        assert_eq!(s.audio, AudioMode::Off, "a bad value changes nothing");
    }

    #[test]
    fn audio_sink_defaults_to_auto_and_old_files_load() {
        assert_eq!(Settings::default().audio_sink, "");
        assert_eq!(Settings::default().audio_sink_flag(), None, "auto: no flag");
        let dir = std::env::temp_dir().join(format!("tunnelrez-sink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, json!({"tv_host": "tv.lan", "audio": "tv_only"}).to_string()).unwrap();
        let old = Settings::load_from(&path);
        assert_eq!((old.audio_sink.as_str(), old.audio), ("", AudioMode::TvOnly), "a file from before the sink");
        let s = Settings { audio_sink: "Speakers (Steam Streaming Speakers)".into(), ..Default::default() };
        s.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path).audio_sink, s.audio_sink, "survives a save");
        let _ = std::fs::remove_dir_all(dir);
        assert_eq!(serde_json::to_value(Settings::default()).unwrap()["audio_sink"], json!(""));
    }

    #[test]
    fn audio_sink_is_settable_from_the_panel() {
        let mut s = Settings::default();
        assert!(s.set("audio_sink", json!("off")));
        assert_eq!(s.audio_sink_flag(), Some("off"));
        assert!(s.set("audio_sink", json!("  Steam Streaming Speakers ")));
        assert_eq!(s.audio_sink, "Steam Streaming Speakers", "trimmed");
        assert!(s.set("audio_sink", json!("auto")));
        assert_eq!(s.audio_sink, "", "auto is stored as empty");
        assert!(s.set("audio_sink", json!("Speakers")));
        assert!(!s.set("audio_sink", json!(3)), "not a string");
        assert!(
            !s.set(
                "audio_sink",
                json!(
                    "a
b"
                )
            ),
            "not one line"
        );
        assert!(!s.set("audio_sink", json!("x".repeat(AUDIO_SINK_MAX + 1))), "too long");
        assert_eq!(s.audio_sink, "Speakers", "a bad value changes nothing");
        assert!(s.set("audio_sink", json!("")));
        assert_eq!(s.audio_sink_flag(), None);
    }

    #[test]
    fn old_file_keeps_panel_open() {
        let s: Settings = serde_json::from_value(json!({"tv_host": "tv.lan", "setup_done": true})).unwrap();
        assert!(s.keep_panel_open);
        let s: Settings = serde_json::from_value(json!({"keep_panel_open": false, "panel_pos": [-1200, 40]})).unwrap();
        assert!(!s.keep_panel_open);
        assert_eq!(s.panel_pos, Some((-1200, 40)));
    }

    #[test]
    fn partial_and_unknown_fields_load() {
        let s: Settings = serde_json::from_value(json!({"tv_host": "tv.lan", "picture": "4k60", "future": 1})).unwrap();
        assert_eq!(s.tv_host, "tv.lan");
        assert_eq!(s.picture, Picture::P2160x60);
        assert_eq!(s.mode, Mode::Extend);
    }

    #[test]
    fn set_validates() {
        let mut s = Settings::default();
        assert!(s.set("codec", json!("hevc")));
        assert_eq!(s.codec, Codec::Hevc);
        assert!(!s.set("codec", json!("mpeg2")));
        assert!(s.set("bitrate_mbps", json!(9999)));
        assert_eq!(s.bitrate_mbps, Bitrate::Mbps(BITRATE_MAX));
        assert!(s.set("bitrate_mbps", json!(1)));
        assert_eq!(s.bitrate_mbps, Bitrate::Mbps(BITRATE_MIN));
        assert!(s.set("bitrate_mbps", json!("auto")));
        assert_eq!(s.bitrate_mbps, Bitrate::Auto);
        assert!(!s.set("bitrate_mbps", json!("fast")));
        assert!(!s.set("bitrate_mbps", json!(-5)));
        assert_eq!(s.bitrate_mbps, Bitrate::Auto, "a bad value changes nothing");
        assert!(s.set("tv_host", json!("  tv.lan ")));
        assert_eq!(s.tv_host, "tv.lan");
        assert!(!s.set("setup_done", json!(true)), "not settable from the panel");
        assert!(s.set("keep_panel_open", json!(false)));
        assert!(!s.keep_panel_open);
        assert!(!s.set("keep_panel_open", json!("yes")));
        assert!(!s.set("panel_pos", json!([0, 0])), "saved by the window, not the panel");
        assert!(s.set("log_format", json!("json")));
        assert_eq!(s.log_format, LogFormat::Json);
        assert!(!s.set("log_format", json!("csv")));
        assert!(!s.set("schema", json!(9)), "not settable");
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("tunnelrez-test-{}", std::process::id()));
        let path = dir.join("settings.json");
        let s = Settings {
            tv_host: "tv.lan".into(),
            encoder: Encoder::Qsv,
            bitrate_mbps: Bitrate::Mbps(40),
            ..Default::default()
        };
        s.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), s);
        let auto = Settings { bitrate_mbps: Bitrate::Auto, ..s };
        auto.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), auto, "Auto survives a save");
        let _ = std::fs::remove_dir_all(dir);
    }
}
