//! TunnelRez receiver: Android TV NativeActivity in Rust.
//!
//! TCP (`PROTOCOL.md`) -> network thread -> bounded frame queue -> decoder thread -> AMediaCodec (hardware)
//! -> window surface, newest-frame-only rendering. Audio: UDP -> jitter buffer / playout clock -> `AAudio`.
//!
//! The app builds for Android only. The pure modules (framing, frame queue + IDR gate, decoder key plans,
//! AV1 sample form + av1C, CAPS JSON, stats math) also build on the host so their unit tests run with plain `cargo test`.
#![cfg_attr(
    not(target_os = "android"),
    expect(dead_code, reason = "host builds compile only the pure modules, for their unit tests")
)]

mod av1;
mod caps;
mod format_keys;
mod frame_queue;
mod jitter;
mod metrics;
mod proto;
mod session_end;

#[cfg(target_os = "android")]
mod activity;
#[cfg(target_os = "android")]
mod audio;
#[cfg(target_os = "android")]
mod codec;
#[cfg(target_os = "android")]
mod decoders;
#[cfg(target_os = "android")]
mod idle;
#[cfg(target_os = "android")]
mod net;
#[cfg(target_os = "android")]
mod pipeline;
#[cfg(target_os = "android")]
mod util;
#[cfg(target_os = "android")]
mod wifi;
