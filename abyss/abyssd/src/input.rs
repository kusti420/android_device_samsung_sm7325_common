//! evdev input: the three side keys and the headphone-jack switch.
//!
//! On a52sxq (kernel DT, see README): "gpio-keys" has KEY_VOLUMEUP,
//! "qpnp_pon" has KEY_POWER + KEY_VOLUMEDOWN (resin), and
//! "lahaina-yupikidp-snd-card Headset Jack" reports SW_HEADPHONE_INSERT.
//! We do not hard-code names: every /dev/input/event* that exposes one of
//! those codes is opened. Devices are NOT grabbed (abyss-guard also watches
//! the power key).
//!
//! Event timestamps are switched to CLOCK_BOOTTIME so that hold durations stay
//! correct when a press wakes the SoC from suspend and press + release are
//! read together afterwards.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::time::UNIX_EPOCH;

use anyhow::{bail, Result};
use evdev::raw_stream::RawDevice;
use evdev::{EventSummary, KeyCode, SwitchCode};
use log::{info, warn};
use nix::fcntl::{fcntl, FcntlArg, OFlag};

use crate::clock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Power,
}

impl Key {
    pub fn idx(self) -> usize {
        match self {
            Key::Up => 0,
            Key::Down => 1,
            Key::Power => 2,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Input {
    /// A side key went down (`down = true`) or up. `t` = CLOCK_BOOTTIME ms.
    Key { key: Key, down: bool, t: u64 },
    /// Headphone jack state changed.
    Headphones(bool),
    /// Headset button (KEY_MEDIA / KEY_PLAYPAUSE) pressed.
    /// main counts presses: 1× play/pause, 2× next, 3× previous.
    HeadsetButton,
    /// A headset remote's own volume key (true = up).
    HeadsetVolume(bool),
}

// EVIOCSCLOCKID = _IOW('E', 0xa0, int)
nix::ioctl_write_ptr!(eviocsclockid, b'E', 0xa0, libc::c_int);

struct Dev {
    dev: RawDevice,
    /// True when EVIOCSCLOCKID(CLOCK_BOOTTIME) succeeded: event times are usable.
    boottime: bool,
    has_jack: bool,
    /// The headset remote ("... Button Jack"): its volume keys are not side keys.
    headset: bool,
}

pub struct Inputs {
    devs: Vec<Dev>,
}

fn is_ours(dev: &RawDevice) -> (bool, bool) {
    let keys = dev.supported_keys().is_some_and(|k| {
        k.contains(KeyCode::KEY_VOLUMEUP)
            || k.contains(KeyCode::KEY_VOLUMEDOWN)
            || k.contains(KeyCode::KEY_POWER)
            || k.contains(KeyCode::KEY_MEDIA)
    });
    let jack =
        dev.supported_switches().is_some_and(|s| s.contains(SwitchCode::SW_HEADPHONE_INSERT));
    (keys, jack)
}

impl Inputs {
    pub fn open() -> Result<Self> {
        let mut devs = Vec::new();
        let mut entries: Vec<_> = std::fs::read_dir("/dev/input")?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("event"))
            })
            .collect();
        entries.sort();
        for path in entries {
            let dev = match RawDevice::open(&path) {
                Ok(d) => d,
                Err(e) => {
                    warn!("{}: {e}", path.display());
                    continue;
                }
            };
            let (keys, jack) = is_ours(&dev);
            if !keys && !jack {
                continue;
            }
            let fd = dev.as_raw_fd();
            fcntl(fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
            let clk: libc::c_int = libc::CLOCK_BOOTTIME;
            // SAFETY: `fd` is a valid open evdev fd owned by `dev`, and the
            // pointer refers to a live c_int for the duration of the call.
            let boottime = unsafe { eviocsclockid(fd, &clk) }.is_ok();
            info!(
                "input: {} '{}' keys={keys} jack={jack} boottime={boottime}",
                path.display(),
                dev.name().unwrap_or("?")
            );
            let headset = dev.name().is_some_and(|n| n.contains("Jack"));
            devs.push(Dev { dev, boottime, has_jack: jack, headset });
        }
        if devs.is_empty() {
            bail!("no key input devices found");
        }
        Ok(Inputs { devs })
    }

    /// Fds to poll, in the same order as `read(idx)` expects.
    pub fn fds(&self) -> Vec<BorrowedFd<'_>> {
        self.devs.iter().map(|d| d.dev.as_fd()).collect()
    }

    pub fn len(&self) -> usize {
        self.devs.len()
    }

    /// Current jack state (read once at start; later changes come as events).
    pub fn headphones_present(&self) -> bool {
        self.devs.iter().filter(|d| d.has_jack).any(|d| {
            d.dev
                .get_switch_state()
                .map(|s| s.contains(SwitchCode::SW_HEADPHONE_INSERT))
                .unwrap_or(false)
        })
    }

    /// Drains all pending events of device `idx` into `out`.
    pub fn read(&mut self, idx: usize, out: &mut Vec<Input>) {
        let d = &mut self.devs[idx];
        let boottime = d.boottime;
        let headset = d.headset;
        let events = match d.dev.fetch_events() {
            Ok(ev) => ev,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(e) => {
                warn!("input read: {e}");
                return;
            }
        };
        for ev in events {
            let t = if boottime {
                // With CLOCK_BOOTTIME selected the "SystemTime" evdev builds is
                // really boot-relative; take the raw duration.
                ev.timestamp()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or_else(|_| clock::now_ms())
            } else {
                clock::now_ms()
            };
            match ev.destructure() {
                EventSummary::Key(_, code, value) => {
                    // value: 1 = down, 0 = up, 2 = autorepeat (ignored).
                    if value == 2 {
                        continue;
                    }
                    if headset && matches!(code, KeyCode::KEY_VOLUMEUP | KeyCode::KEY_VOLUMEDOWN) {
                        if value == 1 {
                            out.push(Input::HeadsetVolume(code == KeyCode::KEY_VOLUMEUP));
                        }
                        continue;
                    }
                    let key = match code {
                        KeyCode::KEY_VOLUMEUP => Key::Up,
                        KeyCode::KEY_VOLUMEDOWN => Key::Down,
                        KeyCode::KEY_POWER => Key::Power,
                        KeyCode::KEY_MEDIA | KeyCode::KEY_PLAYPAUSE => {
                            if value == 1 {
                                out.push(Input::HeadsetButton);
                            }
                            continue;
                        }
                        _ => continue,
                    };
                    out.push(Input::Key { key, down: value == 1, t });
                }
                EventSummary::Switch(_, SwitchCode::SW_HEADPHONE_INSERT, value) => {
                    out.push(Input::Headphones(value != 0));
                }
                _ => {}
            }
        }
    }
}
