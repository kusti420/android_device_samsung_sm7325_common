//! Talking to init: leave Abyss, power off, read our properties.
//!
//! Properties go through librustutils (bionic __system_property_* under the
//! hood), so this module needs no unsafe code of its own.
//!
//! - leave: `sys.abyss.request=leave`; init's abyss.rc stops abyssd and
//!   restarts the framework. abyssd also exits 0 on its own. abyssd never
//!   writes `sys.abyss.state` (init owns it).
//! - power off: `sys.powerctl=shutdown` (init performs a clean shutdown).

use log::{error, info};
use rustutils::android::system_properties;

pub const PROP_REQUEST: &str = "sys.abyss.request";
pub const PROP_SUSPEND: &str = "persist.abyss.suspend";

pub fn set(name: &str, value: &str) {
    match system_properties::write(name, value) {
        Ok(()) => info!("setprop {name} {value}"),
        Err(e) => error!("setprop {name} {value} failed: {e}"),
    }
}

/// Reads a boolean property ("1", "true", ...), `default` if unset/invalid.
pub fn prop_bool(name: &str, default: bool) -> bool {
    system_properties::read_bool(name, default).unwrap_or(default)
}

/// Reads a string property.
pub fn prop_str(name: &str) -> Option<String> {
    system_properties::read(name).ok().flatten().filter(|s| !s.is_empty())
}

/// Asks init to bring Android back. The caller stops audio, saves state and exits 0.
pub fn request_leave(reason: &str) {
    info!("leaving Abyss: {reason}");
    set(PROP_REQUEST, "leave");
}

/// Clean shutdown via init.
pub fn power_off() {
    info!("power off requested");
    set("sys.powerctl", "shutdown");
}
