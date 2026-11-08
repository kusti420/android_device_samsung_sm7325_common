//! EXPERIMENTAL suspend (`persist.abyss.suspend=1`, default off).
//!
//! The v1.0 design re-enables autosuspend through system_suspend
//! (ISuspendControlService.enableAutosuspend). That call is blocked for any
//! domain but system_server by platform neverallow rules, so abyssd cannot
//! use it; this module stays experimental and OFF by default.
//! This module instead uses the classic kernel wakeup_count protocol directly:
//!
//! 1. read /sys/power/wakeup_count (blocks while wakeup events are in flight),
//! 2. write the same value back (fails with EBUSY if a wakeup event arrived
//!    meanwhile, so we must not suspend),
//! 3. write "mem" to /sys/power/state, which returns after resume.
//!
//! Only used while the screen is off, nothing plays, and no key is held.
//! system_suspend keeps running alongside and may hold its own wakelocks; a
//! failed attempt is harmless and simply logged.

use std::fs;
use std::io::Write;

use log::{debug, info};

/// Tries one suspend cycle. Returns true if the kernel actually suspended
/// (i.e. the write to /sys/power/state succeeded and returned after resume).
pub fn try_suspend() -> bool {
    let count = match fs::read_to_string("/sys/power/wakeup_count") {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            debug!("wakeup_count read: {e}");
            return false;
        }
    };
    let ok = fs::OpenOptions::new()
        .write(true)
        .open("/sys/power/wakeup_count")
        .and_then(|mut f| f.write_all(count.as_bytes()));
    if let Err(e) = ok {
        debug!("wakeup_count write ({count}): {e} - wakeup in progress, not suspending");
        return false;
    }
    let r = fs::OpenOptions::new()
        .write(true)
        .open("/sys/power/state")
        .and_then(|mut f| f.write_all(b"mem"));
    match r {
        Ok(()) => {
            info!("resumed from suspend");
            true
        }
        Err(e) => {
            debug!("suspend aborted: {e}");
            false
        }
    }
}
