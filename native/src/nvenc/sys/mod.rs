//! Raw NVENC API bindings (bindgen output of `nvEncodeAPI.h`, API 12.1), plus the GUIDs and struct
//! version constants bindgen cannot generate. The DLL is loaded at runtime (see `super::Api`).
#![allow(
    warnings,
    missing_docs,
    unreachable_pub,
    clippy::all,
    clippy::pedantic,
    clippy::restriction,
    reason = "bindgen output and its hand-written companions: generated code, not linted"
)]

// Generated (bindgen, genver.py): kept as generated, not reformatted.
#[rustfmt::skip]
pub mod api;
#[rustfmt::skip]
pub mod guid;
#[rustfmt::skip]
pub mod version;

pub use api::*;
pub use guid::*;
pub use version::*;
