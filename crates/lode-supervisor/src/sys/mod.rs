//! The platform layer under the supervise loop: spawning the app in a group it
//! can be stopped with, stopping and reaping it, and receiving signals.

use std::time::Duration;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::OwnedSignalSource;
#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::OwnedSignalSource;
#[cfg(windows)]
pub(crate) use windows::*;

/// Poll granularity while waiting for a child to exit during a graceful stop.
pub(crate) const STOP_POLL: Duration = Duration::from_millis(50);
