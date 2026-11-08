//! Battery readings from Samsung's sec_battery power_supply ("battery").
//!
//! Units on this kernel (kernel/samsung/sm7325/drivers/battery/common/
//! sec_battery.c, sec_bat_get_property): current_now / current_avg are **mA**
//! (the fuel gauge is asked for SEC_BATTERY_CURRENT_MA, not the standard µA),
//! voltage_now is µV, capacity is %, status is the standard string.
//!
//! Reading `capacity` makes the sm5714 fuel gauge do a register dump, so this
//! is read only every 30 s while awake (the event loop schedules it), never in
//! a loop.
//!
//! The hours-left figure in v1-lite is the simple estimate
//! remaining_mAh = capacity% × 4500 mAh × 0.97 (3 % reserve), divided by the
//! average draw. The v1.0 energy-model estimator replaces it.

use std::fs;

const DIR: &str = "/sys/class/power_supply/battery";
const DESIGN_MAH: f64 = 4500.0;
const RESERVE: f64 = 0.97;

#[derive(Clone, Debug, Default)]
pub struct Battery {
    pub capacity: Option<u32>,
    pub status: String,
    pub voltage_mv: Option<u32>,
    pub current_avg_ma: Option<i32>,
    pub current_now_ma: Option<i32>,
    /// The learned draw while music plays with the screen off ([`OffDraw`], set by main.rs).
    pub screen_off_ma: Option<f64>,
}

fn read_str(name: &str) -> Option<String> {
    fs::read_to_string(format!("{DIR}/{name}")).ok().map(|s| s.trim().to_string())
}

fn read_i64(name: &str) -> Option<i64> {
    read_str(name)?.parse().ok()
}

impl Battery {
    pub fn read() -> Battery {
        Battery {
            capacity: read_i64("capacity").map(|v| v.clamp(0, 100) as u32),
            status: read_str("status").unwrap_or_else(|| "Unknown".into()),
            voltage_mv: read_i64("voltage_now").map(|uv| (uv / 1000) as u32),
            current_avg_ma: read_i64("current_avg").map(|v| v as i32),
            current_now_ma: read_i64("current_now").map(|v| v as i32),
            screen_off_ma: None,
        }
    }

    pub fn charging(&self) -> bool {
        matches!(self.status.as_str(), "Charging" | "Full")
    }

    /// Discharge draw in mA (positive), if known and plausible.
    pub fn draw_ma(&self) -> Option<f64> {
        let ma = self.current_avg_ma.or(self.current_now_ma)?;
        // Samsung reports discharge as negative current; accept either sign
        // while discharging.
        let ma = (ma as f64).abs();
        (ma >= 1.0 && !self.charging()).then_some(ma)
    }

    /// Hours left at a given draw in mA.
    pub fn hours_at(&self, ma: f64) -> Option<f64> {
        let cap = self.capacity? as f64;
        (ma >= 1.0).then(|| cap / 100.0 * DESIGN_MAH * RESERVE / ma)
    }

    /// Hours of playback left at the current average draw (screen on or off, whatever it is now).
    pub fn hours_left(&self) -> Option<f64> {
        self.hours_at(self.draw_ma()?)
    }

    /// Hours of music left the way Abyss is meant to run: screen off (the learned draw).
    pub fn hours_screen_off(&self) -> Option<f64> {
        self.hours_at(self.screen_off_ma?)
    }
}

/// Where the learned screen-off draw is kept between sessions (one number, mA).
const OFF_FILE: &str = "/data/misc/abyss/out/screen_off_draw";
/// The fuel gauge's current_avg lags; skip this long after the screen went off.
const SETTLE_MS: u64 = 90_000;
/// Write the learned value at most this often.
const SAVE_EVERY_MS: u64 = 300_000;

/// Learns the draw of music with the screen off: only samples taken while music plays, the screen
/// has been off for [`SETTLE_MS`] and the phone is not charging (an exponential average, so the
/// figure follows the output, codec and Wi-Fi use of the session). Kept across sessions.
#[derive(Debug, Default)]
pub struct OffDraw {
    ma: Option<f64>,
    saved_at: u64,
}

impl OffDraw {
    pub fn load() -> OffDraw {
        let ma = fs::read_to_string(OFF_FILE)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|v| (1.0..2000.0).contains(v));
        OffDraw { ma, saved_at: 0 }
    }

    /// One 30 s battery reading. `off_for_ms` is None while the screen is on.
    pub fn sample(&mut self, b: &Battery, off_for_ms: Option<u64>, playing: bool, now: u64) {
        if !playing || off_for_ms.is_none_or(|ms| ms < SETTLE_MS) {
            return;
        }
        let Some(d) = b.draw_ma() else { return };
        let ma = match self.ma {
            Some(m) => m * 0.8 + d * 0.2,
            None => d,
        };
        self.ma = Some(ma);
        if now.saturating_sub(self.saved_at) >= SAVE_EVERY_MS {
            if let Err(e) = fs::write(OFF_FILE, format!("{ma:.1}\n")) {
                log::warn!("screen-off draw: {e}");
            }
            self.saved_at = now;
        }
    }

    pub fn ma(&self) -> Option<f64> {
        self.ma
    }
}
