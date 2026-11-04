//! What the session needs from an encoder backend (ffmpeg process or native NVENC): a stream of
//! codec config + access units with capture timestamps, and a way to ask for an IDR. Both
//! backends feed the same session code, so they send identical protocol streams.

/// One encoded picture.
#[derive(Debug)]
pub struct Au {
    /// Annex-B (H.264/HEVC) or AV1 OBUs (temporal unit, TD optional).
    pub data: Vec<u8>,
    /// Capture time, sender monotonic clock (`clock::now_us`).
    pub cap_us: u64,
    /// When the encoded picture was complete in this process.
    pub arrive_us: u64,
    pub key: bool,
    /// Per-stage times, when the backend knows them (native).
    pub stages: Option<Stages>,
}

/// Native backend stage times (sender clock, µs).
#[derive(Clone, Copy, Debug)]
pub struct Stages {
    /// The captured frame reached this process.
    pub arrive_us: u64,
    /// GPU colour conversion done.
    pub convert_done_us: u64,
    /// Time of the explicit copy between GPUs for this picture, in microseconds (0: none).
    pub xfer_us: u64,
    /// NVENC started on it.
    pub encode_start_us: u64,
    /// Re-encode of the previous picture (no new screen content).
    pub repeat: bool,
}

#[derive(Debug)]
pub enum Event {
    /// Codec config: Annex-B parameter sets (H.264/HEVC) or the AV1 sequence header OBU.
    Config(Vec<u8>),
    Frame(Au),
    /// Nothing yet (the backend polls); the caller checks stop/IDR requests and calls again.
    Idle,
}

/// How [`Source::stop`] ends the encoder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StopMode {
    /// Ask the encoder to finish cleanly (ffmpeg: `q` on stdin, short grace period).
    Graceful,
    /// Kill it right away.
    Kill,
}

pub trait Source {
    /// Next event; Ok(None) when the encoder ended.
    ///
    /// # Errors
    ///
    /// When the encoder backend fails (pipe, parse or device error); the session then
    /// calls [`Source::exit_reason`] and restarts or gives up.
    fn next(&mut self) -> anyhow::Result<Option<Event>>;
    /// Ask for an IDR as soon as possible. False if that is not possible (caller restarts).
    fn force_idr(&mut self) -> bool;
    /// CPU time used by the encoder process so far (µs), when known.
    fn cpu_time_us(&self) -> Option<u64> {
        None
    }
    /// After `next` returned None/Err: why it ended.
    fn exit_reason(&mut self) -> String {
        String::new()
    }
    fn stop(self: Box<Self>, mode: StopMode);
}
