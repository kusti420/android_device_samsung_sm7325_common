//! One clock for everything: CLOCK_BOOTTIME in milliseconds.
//!
//! BOOTTIME keeps counting while the SoC is suspended, so "screen off after
//! 5 s" or "battery every 30 s" stay correct across an (experimental) suspend.

use nix::time::{clock_gettime, ClockId};

/// Milliseconds since boot, including time spent in suspend.
pub fn now_ms() -> u64 {
    match clock_gettime(ClockId::CLOCK_BOOTTIME) {
        Ok(ts) => ts.tv_sec() as u64 * 1000 + ts.tv_nsec() as u64 / 1_000_000,
        Err(_) => 0,
    }
}
