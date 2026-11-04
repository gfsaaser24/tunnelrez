//! Audio sample conversion and packetizing: the PC's mix format -> 48 kHz 16-bit stereo -> 5 ms AUDIO
//! datagrams. Pure logic (no Windows calls), so it is unit tested on any host.

use crate::num::{f32_from_i32, i16_sat_from_f32, u64_sat_from_f64};
use crate::proto::{build, T_AUDIO};

/// The wire sample rate (PROTOCOL.md, `AUDIO_START`).
pub const RATE: u32 = 48_000;
/// Frames per AUDIO datagram: 5 ms at 48 kHz, 960 bytes of PCM, well under one Ethernet/Wi-Fi MTU.
pub const PACKET_FRAMES: usize = 240;

/// One stereo frame, left then right, nominal range -1.0..=1.0.
pub type Frame = [f32; 2];

/// How one sample is stored in the capture buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sample {
    F32,
    I16,
    /// 24-bit in 3 bytes.
    I24,
    /// 32-bit integer (also 24-bit samples in a 32-bit container: the low byte is 0).
    I32,
}

impl Sample {
    fn bytes(self) -> usize {
        match self {
            Sample::I16 => 2,
            Sample::I24 => 3,
            Sample::F32 | Sample::I32 => 4,
        }
    }

    fn read(self, b: &[u8]) -> f32 {
        match self {
            Sample::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            Sample::I16 => f32::from(i16::from_le_bytes([b[0], b[1]])) / 32_768.0,
            Sample::I24 => f32_from_i32(i32::from_le_bytes([0, b[0], b[1], b[2]])) / 2_147_483_648.0,
            Sample::I32 => f32_from_i32(i32::from_le_bytes([b[0], b[1], b[2], b[3]])) / 2_147_483_648.0,
        }
    }
}

/// `WAVEFORMATEXTENSIBLE` speaker bits used by the downmix.
const SPEAKER_FRONT_LEFT: u32 = 0x1;
const SPEAKER_FRONT_RIGHT: u32 = 0x2;
const SPEAKER_FRONT_CENTER: u32 = 0x4;
const SPEAKER_BACK_LEFT: u32 = 0x10;
const SPEAKER_BACK_RIGHT: u32 = 0x20;
const SPEAKER_SIDE_LEFT: u32 = 0x200;
const SPEAKER_SIDE_RIGHT: u32 = 0x400;
/// -3 dB: the usual weight of center and surround channels in a stereo downmix.
const MINUS_3DB: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// The capture stream's format, as the audio engine delivers it.
#[derive(Clone, Debug, PartialEq)]
pub struct InFormat {
    pub rate: u32,
    pub sample: Sample,
    /// Bytes per frame (all channels).
    pub block_align: usize,
    /// Per input channel: its weight in the left and right output.
    weights: Vec<Frame>,
}

impl InFormat {
    /// `channel_mask` is the `WAVEFORMATEXTENSIBLE` mask (0 = unknown: channels 0 and 1 are front L/R).
    ///
    /// # Errors
    ///
    /// No channels, or a block size too small for the channels.
    pub fn new(
        rate: u32,
        channels: u16,
        sample: Sample,
        block_align: usize,
        channel_mask: u32,
    ) -> Result<Self, String> {
        let n = usize::from(channels);
        if n == 0 || block_align < n * sample.bytes() || rate == 0 {
            return Err(format!("unusable capture format: {rate} Hz, {channels} ch, {block_align}-byte frames"));
        }
        Ok(InFormat { rate, sample, block_align, weights: downmix_weights(n, channel_mask) })
    }

    pub fn channels(&self) -> usize {
        self.weights.len()
    }

    /// Decode interleaved capture bytes to stereo frames, appended to `out`. A partial last frame is ignored.
    pub fn decode(&self, bytes: &[u8], out: &mut Vec<Frame>) {
        let step = self.sample.bytes();
        out.reserve(bytes.len() / self.block_align);
        for frame in bytes.chunks_exact(self.block_align) {
            let mut lr = [0.0f32; 2];
            for (ch, w) in self.weights.iter().enumerate() {
                let x = self.sample.read(&frame[ch * step..]);
                lr[0] += x * w[0];
                lr[1] += x * w[1];
            }
            out.push(lr);
        }
    }
}

/// Weights that fold `n` channels into stereo. Mono goes to both sides; front L/R pass through; center and
/// surrounds go in at -3 dB. LFE and unknown channels are left out.
fn downmix_weights(n: usize, mask: u32) -> Vec<Frame> {
    if n == 1 {
        return vec![[1.0, 1.0]];
    }
    // The speaker of each channel: channel i is the i-th set bit of the mask. No (or a short) mask: the
    // first two channels are front left and right, the rest unknown.
    let mut speakers: Vec<u32> = (0..32).map(|b| 1u32 << b).filter(|bit| mask & bit != 0).collect();
    if speakers.len() < n {
        speakers = vec![SPEAKER_FRONT_LEFT, SPEAKER_FRONT_RIGHT];
    }
    (0..n)
        .map(|i| match speakers.get(i).copied().unwrap_or(0) {
            SPEAKER_FRONT_LEFT => [1.0, 0.0],
            SPEAKER_FRONT_RIGHT => [0.0, 1.0],
            SPEAKER_FRONT_CENTER => [MINUS_3DB, MINUS_3DB],
            SPEAKER_BACK_LEFT | SPEAKER_SIDE_LEFT => [MINUS_3DB, 0.0],
            SPEAKER_BACK_RIGHT | SPEAKER_SIDE_RIGHT => [0.0, MINUS_3DB],
            _ => [0.0, 0.0],
        })
        .collect()
}

/// Linear-interpolation sample rate converter to [`RATE`]. Only used when the PC's mix format is not 48 kHz
/// (44.1 kHz endpoints); it delays the stream by one input frame.
#[derive(Debug)]
pub struct Resampler {
    /// Input frames per output frame.
    step: f64,
    /// Read position in the sequence `[prev] + input` of the next call.
    pos: f64,
    prev: Option<Frame>,
}

impl Resampler {
    pub fn new(in_rate: u32) -> Self {
        Resampler { step: f64::from(in_rate) / f64::from(RATE), pos: 0.0, prev: None }
    }

    /// Convert `input` (appending to `out`). Returns how many input frames before `input[0]` the first output
    /// frame lies (0.0 ..= 1.0), so the caller can time it.
    pub fn process(&mut self, input: &[Frame], out: &mut Vec<Frame>) -> f64 {
        let Some(&last) = input.last() else { return 0.0 };
        let prev = self.prev;
        let lead = usize::from(prev.is_some());
        let at = |k: usize| if k < lead { prev.unwrap_or_default() } else { input[k - lead] };
        let len = input.len() + lead;
        let first = f64_from_usize(lead) - self.pos;
        while self.pos + 1.0 <= f64_from_usize(len - 1) {
            let idx = usize_floor(self.pos);
            let weight = f32_from_unit(self.pos - f64_from_usize(idx));
            let (lo, hi) = (at(idx), at(idx + 1));
            out.push([lo[0] + (hi[0] - lo[0]) * weight, lo[1] + (hi[1] - lo[1]) * weight]);
            self.pos += self.step;
        }
        self.pos -= f64_from_usize(len - 1);
        self.prev = Some(last);
        first.max(0.0)
    }
}

#[expect(clippy::as_conversions, clippy::cast_precision_loss, reason = "frame counts far below 2^52")]
fn f64_from_usize(x: usize) -> f64 {
    x as f64
}

#[expect(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "floor of a non-negative read position"
)]
fn usize_floor(x: f64) -> usize {
    x.floor() as usize
}

#[expect(clippy::as_conversions, clippy::cast_possible_truncation, reason = "a 0..1 interpolation weight")]
fn f32_from_unit(x: f64) -> f32 {
    x as f32
}

/// Microseconds of `frames` frames at [`RATE`].
pub fn frames_to_us(frames: u64) -> u64 {
    frames * 1_000_000 / u64::from(RATE)
}

/// One AUDIO datagram's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    /// Index of the first frame since `AUDIO_START` (contiguous: a gap means a lost datagram).
    pub pos: u64,
    /// Sender clock (µs, the same clock as FRAME `t_us`) of the first frame's capture.
    pub t_us: u64,
    /// [`PACKET_FRAMES`] interleaved stereo frames.
    pub pcm: Vec<i16>,
}

impl Packet {
    /// The datagram: header (type AUDIO, `t_us`), then `sid` u32, `pos` u64, PCM s16le.
    pub fn encode(&self, sid: u32) -> Vec<u8> {
        let mut pcm = Vec::with_capacity(self.pcm.len() * 2);
        for s in &self.pcm {
            pcm.extend_from_slice(&s.to_le_bytes());
        }
        build(T_AUDIO, 0, self.t_us, &[&sid.to_le_bytes(), &self.pos.to_le_bytes(), &pcm])
    }
}

/// Cuts a stream of timed stereo frames into [`Packet`]s of [`PACKET_FRAMES`] frames.
#[derive(Debug, Default)]
pub struct Packetizer {
    pending: Vec<i16>,
    /// `t_us` of the first pending frame.
    pending_t: u64,
    next_pos: u64,
}

fn to_i16(x: f32) -> i16 {
    i16_sat_from_f32(x.clamp(-1.0, 1.0) * 32_767.0)
}

impl Packetizer {
    /// Frames packetized so far (including the pending ones).
    pub fn frames(&self) -> u64 {
        self.next_pos + u64::try_from(self.pending.len() / 2).unwrap_or(0)
    }

    /// Add `frames`, the first captured at `t_us`; full packets go to `out`.
    pub fn push(&mut self, frames: &[Frame], t_us: u64, out: &mut Vec<Packet>) {
        for (k, f) in frames.iter().enumerate() {
            self.push_one([to_i16(f[0]), to_i16(f[1])], t_us, k, out);
        }
    }

    /// Add `n` frames of silence, the first at `t_us`.
    pub fn push_silence(&mut self, n: usize, t_us: u64, out: &mut Vec<Packet>) {
        for k in 0..n {
            self.push_one([0, 0], t_us, k, out);
        }
    }

    fn push_one(&mut self, lr: [i16; 2], t_us: u64, k: usize, out: &mut Vec<Packet>) {
        if self.pending.is_empty() {
            self.pending_t = t_us + frames_to_us(u64::try_from(k).unwrap_or(0));
        }
        self.pending.extend_from_slice(&lr);
        if self.pending.len() == PACKET_FRAMES * 2 {
            let pcm = std::mem::replace(&mut self.pending, Vec::with_capacity(PACKET_FRAMES * 2));
            out.push(Packet { pos: self.next_pos, t_us: self.pending_t, pcm });
            self.next_pos += u64::try_from(PACKET_FRAMES).unwrap_or(0);
        }
    }
}

/// `t_us` of the first output frame of a resampled chunk: `t_us` of `input[0]` minus the lead the resampler
/// reported, in input frames at `in_rate`.
pub fn lead_t(t_us: u64, lead_frames: f64, in_rate: u32) -> u64 {
    t_us.saturating_sub(u64_sat_from_f64((lead_frames * 1e6 / f64::from(in_rate)).round()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{read_msg, HDR_LEN};

    #[test]
    fn decodes_float_stereo() {
        let fmt = InFormat::new(48_000, 2, Sample::F32, 8, 0x3).unwrap();
        let mut b = Vec::new();
        for x in [0.5f32, -0.25, 1.0, 0.0] {
            b.extend_from_slice(&x.to_le_bytes());
        }
        b.push(7); // partial frame: ignored
        let mut out = Vec::new();
        fmt.decode(&b, &mut out);
        assert_eq!(out, vec![[0.5, -0.25], [1.0, 0.0]]);
    }

    #[test]
    fn decodes_integer_formats() {
        let mut out = Vec::new();
        InFormat::new(48_000, 1, Sample::I16, 2, 0).unwrap().decode(&16_384i16.to_le_bytes(), &mut out);
        assert_eq!(out, vec![[0.5, 0.5]], "mono goes to both sides");
        out.clear();
        // 24-bit: -0.5 is 0xC00000.
        InFormat::new(48_000, 2, Sample::I24, 6, 0).unwrap().decode(&[0, 0, 0xC0, 0, 0, 0x40], &mut out);
        assert_eq!(out, vec![[-0.5, 0.5]]);
        out.clear();
        InFormat::new(48_000, 2, Sample::I32, 8, 0)
            .unwrap()
            .decode(&[i32::MIN.to_le_bytes(), (1i32 << 30).to_le_bytes()].concat(), &mut out);
        assert_eq!(out, vec![[-1.0, 0.5]]);
    }

    #[test]
    fn downmixes_5_1_with_center_and_surrounds() {
        // FL FR FC LFE BL BR
        let fmt = InFormat::new(48_000, 6, Sample::F32, 24, 0x3F).unwrap();
        assert_eq!(fmt.channels(), 6);
        let frame: Vec<u8> = [0.1f32, 0.2, 0.3, 0.9, 0.4, 0.5].iter().flat_map(|x| x.to_le_bytes()).collect();
        let mut out = Vec::new();
        fmt.decode(&frame, &mut out);
        let want_l = 0.1 + 0.3 * MINUS_3DB + 0.4 * MINUS_3DB;
        let want_r = 0.2 + 0.3 * MINUS_3DB + 0.5 * MINUS_3DB;
        assert!((out[0][0] - want_l).abs() < 1e-6 && (out[0][1] - want_r).abs() < 1e-6, "{out:?}, LFE left out");
    }

    #[test]
    fn rejects_unusable_formats() {
        assert!(InFormat::new(48_000, 0, Sample::F32, 8, 0).is_err());
        assert!(InFormat::new(48_000, 2, Sample::F32, 4, 0).is_err());
        assert!(InFormat::new(0, 2, Sample::F32, 8, 0).is_err());
    }

    #[test]
    fn resampler_at_48k_passes_frames_through_one_frame_late() {
        let mut r = Resampler::new(48_000);
        let a: Vec<Frame> = (0..10u8).map(|i| [f32::from(i), 0.0]).collect();
        let mut out = Vec::new();
        r.process(&a[..4], &mut out);
        r.process(&a[4..], &mut out);
        let got: Vec<f32> = out.iter().map(|f| f[0]).collect();
        assert_eq!(got, (0..9u8).map(f32::from).collect::<Vec<_>>());
    }

    #[test]
    fn resampler_44k1_keeps_the_rate() {
        let mut r = Resampler::new(44_100);
        let mut out = Vec::new();
        // One second of a ramp, in 441-frame chunks (10 ms).
        let ramp: Vec<Frame> = (0..44_100u16).map(|i| [f32::from(i) / 44_100.0, 0.0]).collect();
        for c in ramp.chunks(441) {
            r.process(c, &mut out);
        }
        assert!((47_990..=48_000).contains(&out.len()), "{} frames", out.len());
        // A ramp stays a ramp: monotonic and smooth.
        assert!(out.windows(2).all(|w| w[1][0] >= w[0][0] && w[1][0] - w[0][0] < 2.0 / 44_100.0));
    }

    #[test]
    fn packetizer_cuts_5ms_packets_with_times() {
        let mut p = Packetizer::default();
        let mut out = Vec::new();
        // 10 ms + 2.5 ms of frames: two full packets, half a packet pending.
        p.push(&vec![[0.5, -0.5]; 600], 1_000_000, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].pos, out[0].t_us), (0, 1_000_000));
        assert_eq!((out[1].pos, out[1].t_us), (240, 1_005_000));
        assert_eq!(out[0].pcm.len(), PACKET_FRAMES * 2);
        assert_eq!(&out[0].pcm[..2], &[16_384, -16_384]);
        assert_eq!(p.frames(), 600);
        // The pending half keeps its own start time (frame 480 of the first chunk).
        out.clear();
        p.push_silence(120, 2_000_000, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].pos, out[0].t_us), (480, 1_010_000));
        assert_eq!(out[0].pcm[2 * 119..2 * 121], [16_384, -16_384, 0, 0]);
    }

    #[test]
    fn packet_encodes_to_an_audio_datagram() {
        let pkt = Packet { pos: 480, t_us: 77, pcm: vec![1, -2] };
        let d = pkt.encode(0xABCD);
        let (h, p) = read_msg(&mut &d[..]).unwrap();
        assert_eq!((h.typ, h.t_us, d.len()), (T_AUDIO, 77, HDR_LEN + 4 + 8 + 4));
        assert_eq!(&p[..4], &0xABCDu32.to_le_bytes());
        assert_eq!(&p[4..12], &480u64.to_le_bytes());
        assert_eq!(&p[12..], &[1, 0, 0xFE, 0xFF]);
    }

    #[test]
    fn lead_time_moves_back_by_the_resampler_lead() {
        assert_eq!(lead_t(1_000_000, 1.0, 44_100), 1_000_000 - 23);
        assert_eq!(lead_t(10, 1.0, 44_100), 0);
    }
}
