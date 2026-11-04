//! PC audio to the TV: WASAPI loopback of the default playback device -> 48 kHz s16 stereo -> 5 ms UDP
//! datagrams stamped with the sender clock (the same clock as video `t_us`). See PROTOCOL.md, "Audio".

pub mod driver;
pub mod pcm;
pub mod policy;
pub mod qos;
pub(crate) mod sha256;
pub mod sink;
pub mod stream;
pub mod tv_only;
pub mod wasapi;

use crate::num::f64_from_u64;
use anyhow::Result;
use std::time::{Duration, Instant};
use wasapi::{Com, Endpoint, Loopback, Mute, Tone};

/// RMS (both channels) of the loopback for `dur`, after `settle` (lets a mute/volume change take effect).
fn loopback_rms(cap: &mut Loopback, tone: Option<&mut Tone>, settle: Duration, dur: Duration) -> Result<f64> {
    let mut tone = tone;
    let t0 = Instant::now();
    let (mut sum, mut n) = (0.0f64, 0u64);
    let mut frames = Vec::new();
    while t0.elapsed() < settle + dur {
        if let Some(t) = tone.as_deref_mut() {
            t.fill()?;
        }
        let counting = t0.elapsed() >= settle;
        cap.read(5, |c| {
            if !counting {
                return;
            }
            frames.clear();
            if let Some(d) = c.data {
                cap_format_decode(d, &mut frames);
            } else {
                frames.resize(c.frames, [0.0, 0.0]);
            }
            for f in &frames {
                sum += f64::from(f[0]) * f64::from(f[0]) + f64::from(f[1]) * f64::from(f[1]);
                n += 2;
            }
        })?;
    }
    Ok(if n == 0 { 0.0 } else { (sum / f64_from_u64(n)).sqrt() })
}

/// The probe decodes with a fixed float-stereo view: the tone test only runs on float mix formats, and the
/// RMS is a ratio check, so the exact downmix does not matter.
fn cap_format_decode(d: &[u8], out: &mut Vec<pcm::Frame>) {
    for c in d.as_chunks::<8>().0 {
        out.push([f32::from_le_bytes([c[0], c[1], c[2], c[3]]), f32::from_le_bytes([c[4], c[5], c[6], c[7]])]);
    }
}

/// Puts the endpoint's mute and volume back when dropped.
struct Restore<'a> {
    ep: &'a Endpoint,
    mute: Mute,
    volume: f32,
}

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        let _ = self.ep.set_volume(self.volume);
        let _ = self.ep.set_mute(self.mute);
    }
}

/// `tunnelrez-send audio-probe`: does WASAPI loopback still capture when the PC endpoint is muted, or when
/// its volume is 0? Plays a quiet 440 Hz tone (-26 dBFS) while the endpoint is muted, then at volume 0, and
/// measures the loopback each time. The PC stays silent the whole time; mute and volume are restored after.
///
/// # Errors
///
/// No playback device, or WASAPI errors.
pub fn probe() -> Result<String> {
    const AMPLITUDE: f32 = 0.05;
    let _com = Com::init()?;
    let ep = Endpoint::default_render()?;
    let restore = Restore { ep: &ep, mute: ep.mute()?, volume: ep.volume()? };
    let mut lines = vec![format!("endpoint {}: mute {:?}, volume {:.2}", ep.id, restore.mute, restore.volume)];
    let mut cap = Loopback::open()?;
    lines.push(format!("loopback format: {:?}", cap.format));
    let (settle, dur) = (Duration::from_millis(300), Duration::from_millis(700));
    // Silence check first (other apps playing would show here).
    ep.set_mute(Mute::On)?;
    let background = loopback_rms(&mut cap, None, settle, dur)?;
    let mut tone = Tone::start(AMPLITUDE)?;
    let muted = loopback_rms(&mut cap, Some(&mut tone), settle, dur)?;
    ep.set_volume(0.0)?;
    ep.set_mute(Mute::Off)?;
    let vol0 = loopback_rms(&mut cap, Some(&mut tone), settle, dur)?;
    drop(tone);
    drop(restore);
    let expect = f64::from(AMPLITUDE) / std::f64::consts::SQRT_2;
    let pct = |x: f64| 100.0 * x / expect;
    lines.push(format!("expected tone RMS {expect:.4} (no attenuation); background (muted, no tone) {background:.4}"));
    lines.push(format!("muted:      loopback RMS {muted:.4} = {:.0}% of the tone", pct(muted)));
    lines.push(format!("volume 0:   loopback RMS {vol0:.4} = {:.0}% of the tone", pct(vol0)));
    lines.push(format!(
        "verdict: loopback while muted: {}; loopback at volume 0: {}",
        if pct(muted) > 50.0 { "captures" } else { "SILENT" },
        if pct(vol0) > 50.0 { "captures" } else { "SILENT" }
    ));
    Ok(lines.join("\n"))
}

/// `tunnelrez-send audio-restore`: undo what a killed sender left (the virtual-sink default switch and the
/// "Play on TV only" mute). The app runs it on quit and after it had to kill a sender.
pub fn restore_all() -> String {
    let lines = recover_stale();
    if lines.is_empty() {
        "nothing to restore".into()
    } else {
        lines.join("\n")
    }
}

/// Undo a mute and a default-device switch an earlier sender left behind; one log line for each.
pub fn recover_stale() -> Vec<String> {
    [tv_only::recover_stale(), sink::recover_stale(), driver::recover_stale()].into_iter().flatten().collect()
}

/// `tunnelrez-send audio-devices`: one JSON line with the playback devices and the virtual sink `auto` would
/// use (`{"devices":[{"id","name","adapter"}],"auto":id|null}`), for the app's sink setting.
///
/// # Errors
///
/// COM or the device list is unavailable.
pub fn devices_json() -> Result<String> {
    let _com = Com::init()?;
    let devices = wasapi::render_devices()?;
    let auto = sink::pick(&devices, &sink::SinkChoice::Auto).map(|d| d.id.clone());
    let list: Vec<serde_json::Value> =
        devices.iter().map(|d| serde_json::json!({ "id": d.id, "name": d.name, "adapter": d.adapter })).collect();
    Ok(serde_json::json!({ "devices": list, "auto": auto }).to_string())
}
