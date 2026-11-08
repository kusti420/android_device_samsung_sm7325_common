//! Panel backlight, through init.
//!
//! abyssd cannot touch /sys/class/backlight/panel0-backlight (vendor sysfs
//! type, not nameable in product policy). Instead it sets the property
//! `sys.abyss.brightness=<n>`, and abyss.rc writes that value to the panel
//! while `sys.abyss.state=active`. init also saves Android's level on enter
//! and restores it on every leave path, so abyssd keeps no copy of its own.
//!
//! The composer leaves the panel at whatever it last set (often 1 =
//! unreadable), so abyssd asks for `persist.abyss.brightness` (default 160,
//! clamped to 1..486) after the first frame and again on every wake.

use crate::control;

const DEFAULT_LEVEL: u32 = 160;
/// max_brightness of panel0-backlight on a52sxq (unreadable for abyssd).
const MAX_LEVEL: u32 = 486;
pub const PROP_BRIGHTNESS: &str = "persist.abyss.brightness";
pub const PROP_SET: &str = "sys.abyss.brightness";

/// The level abyssd uses: the persist property, clamped to 1..=486.
pub fn level() -> u32 {
    control::prop_str(PROP_BRIGHTNESS)
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_LEVEL)
        .clamp(1, MAX_LEVEL)
}

/// Asks init to (re-)apply abyssd's level: after the first frame and on wake.
pub fn apply() {
    control::set(PROP_SET, &level().to_string());
}
