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
//! `charge_counter` is µAh, but coarse: sm5714_fg_get_property returns
//! battery_data->Capacity (4500) × raw_capacity (0.1 % units), i.e. 4.5 mAh steps,
//! and sec_battery caches it from its monitor work (every 30 s awake, hourly when
//! suspended). Over long windows it is the honest number: it is the gauge's own
//! integral, so it needs no sampling while the AP sleeps.
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
    /// charge_counter in µAh (4.5 mAh steps on this gauge, see the module docs).
    pub charge_uah: Option<i64>,
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
            charge_uah: read_i64("charge_counter").filter(|v| *v > 0),
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

/// Where the learned screen-off draw is kept between sessions: "<mA> [cc]\n" ("cc" = measured
/// from charge_counter; without it the value is the current_avg fallback).
const OFF_FILE: &str = "/data/misc/abyss/out/screen_off_draw";
/// The fuel gauge's current_avg lags; skip this long after the screen went off.
const SETTLE_MS: u64 = 90_000;
/// Write the learned value at most this often.
const SAVE_EVERY_MS: u64 = 300_000;
/// A charge_counter window counts once it spans at least this long and this much charge
/// (2 steps of 4.5 mAh: the window runs from one step edge to a later one, so the only error
/// is when, within one battery poll, each edge was seen).
const WINDOW_MIN_MS: u64 = 10 * 60_000;
const WINDOW_MIN_UAH: i64 = 9_000;
/// A drop bigger than this between two readings is a gauge correction, not draw: restart.
const JUMP_UAH: i64 = 20_000;

/// An open charge_counter window: `edge` = (time, charge) of the first step edge seen while the
/// screen was off and music played; `last` = the latest reading.
#[derive(Debug, Clone, Copy)]
struct Window {
    edge: Option<(u64, i64)>,
    last: (u64, i64),
}

/// Learns the draw of music with the screen off: only while music plays, the screen is off and
/// the phone is not charging. Primary source: charge_counter windows (≥ 10 min, ≥ 2 steps,
/// edge to edge), folded in by window length. Until the first such window exists, current_avg
/// samples (taken once the screen has been off for [`SETTLE_MS`]) stand in. Kept across sessions.
#[derive(Debug, Default)]
pub struct OffDraw {
    ma: Option<f64>,
    /// `ma` comes from charge_counter windows (current samples no longer move it).
    measured: bool,
    win: Option<Window>,
    saved_at: u64,
}

impl OffDraw {
    pub fn load() -> OffDraw {
        let text = fs::read_to_string(OFF_FILE).unwrap_or_default();
        let mut it = text.split_whitespace();
        let ma = it.next().and_then(|s| s.parse::<f64>().ok()).filter(|v| (1.0..2000.0).contains(v));
        let measured = ma.is_some() && it.next() == Some("cc");
        OffDraw { ma, measured, win: None, saved_at: 0 }
    }

    /// One 30 s battery reading. `off_for_ms` is None while the screen is on.
    pub fn sample(&mut self, b: &Battery, off_for_ms: Option<u64>, playing: bool, now: u64) {
        let on = playing && off_for_ms.is_some() && !b.charging();
        match (on, b.charge_uah) {
            (true, Some(cc)) => self.track(cc, now),
            _ => self.win = None,
        }
        if !self.measured && on && off_for_ms.is_some_and(|ms| ms >= SETTLE_MS) {
            if let Some(d) = b.draw_ma() {
                self.ma = Some(match self.ma {
                    Some(m) => m * 0.8 + d * 0.2,
                    None => d,
                });
            }
        }
        if self.ma.is_some() && now.saturating_sub(self.saved_at) >= SAVE_EVERY_MS {
            self.save();
            self.saved_at = now;
        }
    }

    fn track(&mut self, cc: i64, now: u64) {
        let Some(mut w) = self.win else {
            self.win = Some(Window { edge: None, last: (now, cc) });
            return;
        };
        let drop = w.last.1 - cc;
        if !(0..=JUMP_UAH).contains(&drop) {
            // Rose (charging, recalibration) or jumped: start over from here.
            self.win = Some(Window { edge: None, last: (now, cc) });
            return;
        }
        if drop > 0 {
            match w.edge {
                None => w.edge = Some((now, cc)),
                Some((t0, c0)) => {
                    let (dt, dq) = (now.saturating_sub(t0), c0 - cc);
                    if dt >= WINDOW_MIN_MS && dq >= WINDOW_MIN_UAH {
                        self.fold(dq as f64 / 1000.0 / (dt as f64 / 3_600_000.0), dt);
                        // The next window starts at this edge.
                        w.edge = Some((now, cc));
                    }
                }
            }
        }
        w.last = (now, cc);
        self.win = Some(w);
    }

    fn fold(&mut self, ma: f64, dt_ms: u64) {
        if !(1.0..2000.0).contains(&ma) {
            return;
        }
        // Weight by window length: a 1 h window counts as much as everything before it.
        let w = (dt_ms as f64 / 3_600_000.0).clamp(0.15, 0.5);
        let new = match (self.ma, self.measured) {
            (Some(m), true) => m * (1.0 - w) + ma * w,
            _ => ma,
        };
        log::info!(
            "screen-off draw: window {:.0} min = {ma:.1} mA (charge_counter), learned {new:.1} mA",
            dt_ms as f64 / 60_000.0
        );
        self.ma = Some(new);
        self.measured = true;
        self.saved_at = 0; // save at the next sample
    }

    fn save(&self) {
        let Some(ma) = self.ma else { return };
        let tag = if self.measured { " cc" } else { "" };
        if let Err(e) = fs::write(OFF_FILE, format!("{ma:.1}{tag}\n")) {
            log::warn!("screen-off draw: {e}");
        }
    }

    pub fn ma(&self) -> Option<f64> {
        self.ma
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bat(cc: i64) -> Battery {
        Battery {
            capacity: Some(80),
            status: "Discharging".into(),
            current_avg_ma: Some(-60),
            charge_uah: Some(cc),
            ..Default::default()
        }
    }

    #[test]
    fn charge_counter_window_beats_current() {
        let mut d = OffDraw::default();
        let off = Some(SETTLE_MS);
        // 30 mA = 4.5 mAh every 9 min. Readings every 30 s, steps on the gauge's 4.5 mAh grid.
        let mut t = 0u64;
        let start = 3_600_000i64;
        for _ in 0..200 {
            let used = (t as f64 / 3_600_000.0 * 30_000.0) as i64; // µAh
            let cc = start - used / 4_500 * 4_500;
            d.sample(&bat(cc), off, true, t);
            t += 30_000;
        }
        let ma = d.ma().unwrap();
        assert!(d.measured);
        assert!((ma - 30.0).abs() < 2.5, "{ma}");
    }

    #[test]
    fn current_fallback_until_measured() {
        let mut d = OffDraw::default();
        d.sample(&bat(3_600_000), Some(SETTLE_MS), true, 0);
        assert_eq!(d.ma(), Some(60.0));
        assert!(!d.measured);
        // Screen on: nothing learned, window dropped.
        d.sample(&bat(3_595_500), None, true, 30_000);
        assert!(d.win.is_none());
    }

    #[test]
    fn rise_restarts_window() {
        let mut d = OffDraw::default();
        d.sample(&bat(3_600_000), Some(0), true, 0);
        d.sample(&bat(3_595_500), Some(0), true, 30_000);
        d.sample(&bat(3_604_500), Some(0), true, 60_000);
        assert!(d.win.unwrap().edge.is_none());
    }
}
