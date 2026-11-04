//! Platform layer. Only Windows exists today; a macOS/Linux port adds a sibling module with the same API.

#[cfg(windows)]
mod pcstats;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use self::pcstats::*;
#[cfg(windows)]
pub(crate) use self::windows::*;
