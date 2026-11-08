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

    /// Hours of playback left at the current average draw.
    pub fn hours_left(&self) -> Option<f64> {
        let cap = self.capacity? as f64;
        let remaining_mah = cap / 100.0 * DESIGN_MAH * RESERVE;
        Some(remaining_mah / self.draw_ma()?)
    }
}
