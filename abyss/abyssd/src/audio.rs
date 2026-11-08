//! Audio output backends.
//!
//! Outputs, in the order abyssd picks them: Bluetooth headphones (while
//! connected through abyss-btd, see bt.rs) > a USB DAC (any snd-usb-audio card
//! in /proc/asound/cards, e.g. a FiiO BTR5 in USB DAC mode) > the 3.5 mm jack.
//! Never the speaker.
//!
//! Bluetooth, two modes (abyss-btd's `mode`, persist.abyss.bt.offload=1 opt-in):
//! - A2DP offload (opt-in): the ADSP encodes (a2dp_dsp.rs
//!   programs its SLIMBUS_7_RX encoder from the negotiated codec) and abyssd feeds it like
//!   the jack: `CompressOut` with the Bluetooth target (DSP decode, route
//!   "compress-offload-playback bt-a2dp" = SLIMBUS_7_RX Audio Mixer MultiMedia4) or, for
//!   CPU-decoded streams, `BtOut` -> `BtDspOut` (PCM on hw:0,0, "deep-buffer-playback
//!   bt-a2dp"). The AP may sleep between DSP fragments. If the DSP refuses the codec,
//!   abyssd asks abyss-btd for software encoding (`BtLink`, bt.rs request_software).
//! - Software (`BtOut`): s16le stereo PCM goes over the abyss_bt socket at the rate
//!   abyss-btd asks for (the codec rate, 44.1 or 48 kHz; the player resamples).
//!
//! Volume is the same software gain as USB (Q13 stream gain on the compress path); the
//! headset is kept at full scale (bt.rs "Volume policy").
//!
//! USB: plain tinyalsa `pcm_open(card, 0, PCM_OUT)`. The card's hw params are
//! queried (rate range, formats); abyssd tries the file's rate, then 48 kHz,
//! 44.1 kHz, 96 kHz, in S16_LE, else S32_LE / S24_3LE / S24_LE (samples are
//! widened). Volume is software gain (same 70 % ceiling as the jack).
//!
//! Jack: raw tinyalsa + libaudioroute:
//! - mixer paths from /vendor/etc/mixer_paths.xml (device/samsung/a52sxq/
//!   configs/audio/mixer_paths.xml, Samsung naming): front end
//!   "deep-buffer-playback headphones" (RX_CDC_DMA_RX_0 Audio Mixer
//!   MultiMedia1) + device path "headset" (includes "ear": WCD938x HPH,
//!   CLS_H_LOHIFI, RX_RX0/1 HPH Digital Volume 73).
//! - PCM: card 0 ("lahaina-yupikidp-snd-card"), device 0 (hw:0,0 = MultiMedia1,
//!   the deep-buffer front end; kernel techpack/audio/asoc/lahaina.c).
//!
//! The speaker is NEVER driven from here (it needs the vendor HAL's speaker
//! protection). Headphones out = the player pauses; this backend is closed.
//!
//! Jack, DSP decode (`CompressOut`, preferred for FLAC <= 48 kHz and MP3): the
//! ENCODED frames go to the Qualcomm ADSP through the ALSA compress-offload
//! device hw:0,7 ("Compress1" = MultiMedia4, kernel techpack/audio/asoc/
//! lahaina.c; USECASE_AUDIO_PLAYBACK_OFFLOAD id 7 in sm7325-common/audio/
//! configs/audio_platform_info.xml), path "compress-offload-playback
//! headphones" (RX_CDC_DMA_RX_0 Audio Mixer MultiMedia4) + "headset". The AP
//! only wakes when the DSP has used up a fragment (1 MB = ~9 s of FLAC 16/44.1).
//! Raw compress ioctls (include/uapi/sound/compress_offload.h) instead of
//! libtinycompress: that library is `vendor: true` and abyssd is a system_ext
//! binary, so soong cannot link it. Volume = the stream's own "Compress
//! Playback 7 Volume" control (Q13 linear gain, 0x2000 = 0 dB, applied by the
//! ASM session in the DSP; msm-compress-q6-v2.c msm_compr_add_volume_control).
//!
//! Older placeholder (`HalOffloadOut`): offload through the vendor audio HAL.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_uint, c_void};

use anyhow::{anyhow, Result};
use log::{info, warn};

use crate::{bt, clock, control};

/// A PCM sink for interleaved signed 16-bit stereo frames.
pub trait AudioOut: Send {
    /// Opens (or re-opens) the stream. Returns the rate actually used; the
    /// player resamples if it differs from `rate`.
    fn open(&mut self, rate: u32) -> Result<u32>;
    /// Blocking write of interleaved stereo i16 samples (len = frames × 2).
    fn write(&mut self, samples: &[i16]) -> Result<()>;
    /// Closes the stream and powers the path down (call on pause / end).
    fn standby(&mut self);
    fn is_open(&self) -> bool;
    fn name(&self) -> &'static str;
    /// The vendor audio HAL was restarted (init sets `sys.abyss.hal=restarted`),
    /// which resets the mixer: drop cached routing so the next open re-applies it.
    fn hal_restarted(&mut self) {}
}

/// Discards audio, pacing itself in real time. Used when tinyalsa cannot be
/// opened, so the rest of abyssd (UI, keys, resume state) still works.
pub struct NullOut {
    rate: u32,
    open: bool,
}

impl NullOut {
    pub fn new() -> Self {
        NullOut { rate: 48000, open: false }
    }
}

impl AudioOut for NullOut {
    fn open(&mut self, rate: u32) -> Result<u32> {
        self.rate = rate;
        self.open = true;
        Ok(rate)
    }
    fn write(&mut self, samples: &[i16]) -> Result<()> {
        let frames = samples.len() as u64 / 2;
        std::thread::sleep(std::time::Duration::from_micros(
            frames * 1_000_000 / self.rate.max(1) as u64,
        ));
        Ok(())
    }
    fn standby(&mut self) {
        self.open = false;
    }
    fn is_open(&self) -> bool {
        self.open
    }
    fn name(&self) -> &'static str {
        "null"
    }
}

/// Placeholder for the v1.0 backend: vendor audio HAL compress offload via a
/// cxx shim (ADSP decodes FLAC/MP3; AP wakes every ~9-26 s). Not implemented
/// in v1-lite; `open` always fails so callers fall back to tinyalsa / null.
#[allow(dead_code)] // v1.0 placeholder, not selected in v1-lite
pub struct HalOffloadOut;

impl AudioOut for HalOffloadOut {
    fn open(&mut self, _rate: u32) -> Result<u32> {
        Err(anyhow!("HAL offload backend not implemented in v1-lite"))
    }
    fn write(&mut self, _samples: &[i16]) -> Result<()> {
        Err(anyhow!("HAL offload backend not implemented in v1-lite"))
    }
    fn standby(&mut self) {}
    fn is_open(&self) -> bool {
        false
    }
    fn name(&self) -> &'static str {
        "hal-offload"
    }
}

// ---------------------------------------------------------------------------
// FFI: hand-written from external/tinyalsa/include/tinyalsa/asoundlib.h and
// system/media/audio_route/include/audio_route/audio_route.h.

#[repr(C)]
struct Pcm {
    _private: [u8; 0],
}
#[repr(C)]
struct AudioRoute {
    _private: [u8; 0],
}

/// struct pcm_config (asoundlib.h). `format` is enum pcm_format (int).
#[repr(C)]
struct PcmConfig {
    channels: c_uint,
    rate: c_uint,
    period_size: c_uint,
    period_count: c_uint,
    format: c_int,
    start_threshold: c_uint,
    stop_threshold: c_uint,
    silence_threshold: c_uint,
    silence_size: c_uint,
    avail_min: c_int,
}

const PCM_OUT: c_uint = 0x0000_0000;
const PCM_FORMAT_S16_LE: c_int = 0;
const PCM_FORMAT_S32_LE: c_int = 1;
const PCM_FORMAT_S24_LE: c_int = 3;
const PCM_FORMAT_S24_3LE: c_int = 4;
/// enum pcm_param (asoundlib.h): PCM_PARAM_RATE.
const PCM_PARAM_RATE: c_int = 6;

#[repr(C)]
struct PcmParams {
    _private: [u8; 0],
}

// Linked through soong `shared_libs: ["libtinyalsa", "libaudioroute"]`.
extern "C" {
    fn pcm_open(card: c_uint, device: c_uint, flags: c_uint, config: *mut PcmConfig) -> *mut Pcm;
    fn pcm_close(pcm: *mut Pcm) -> c_int;
    fn pcm_is_ready(pcm: *mut Pcm) -> c_int;
    fn pcm_get_error(pcm: *mut Pcm) -> *const c_char;
    fn pcm_write(pcm: *mut Pcm, data: *const c_void, count: c_uint) -> c_int;
    fn pcm_params_get(card: c_uint, device: c_uint, flags: c_uint) -> *mut PcmParams;
    fn pcm_params_free(params: *mut PcmParams);
    fn pcm_params_get_min(params: *const PcmParams, param: c_int) -> c_uint;
    fn pcm_params_get_max(params: *const PcmParams, param: c_int) -> c_uint;
    fn pcm_params_format_test(params: *mut PcmParams, format: c_int) -> c_int;
}

extern "C" {
    fn audio_route_init(card: c_uint, xml_path: *const c_char) -> *mut AudioRoute;
    fn audio_route_free(ar: *mut AudioRoute);
    fn audio_route_apply_path(ar: *mut AudioRoute, name: *const c_char) -> c_int;
    fn audio_route_reset_path(ar: *mut AudioRoute, name: *const c_char) -> c_int;
    fn audio_route_update_mixer(ar: *mut AudioRoute) -> c_int;
}

/// Owned audio_route handle.
struct Route {
    ar: *mut AudioRoute,
}

// SAFETY: the audio_route handle is only ever used from the player thread
// that owns the TinyAlsaOut; it is moved there once and never shared.
unsafe impl Send for Route {}

impl Route {
    fn new(card: u32, xml: &str) -> Option<Route> {
        let path = CString::new(xml).ok()?;
        // SAFETY: `path` is a valid NUL-terminated string that outlives the call.
        let ar = unsafe { audio_route_init(card, path.as_ptr()) };
        (!ar.is_null()).then_some(Route { ar })
    }

    fn apply(&mut self, paths: &[String]) {
        for p in paths {
            let Ok(c) = CString::new(p.as_str()) else { continue };
            // SAFETY: `self.ar` is a live handle from audio_route_init; `c` is valid.
            if unsafe { audio_route_apply_path(self.ar, c.as_ptr()) } != 0 {
                warn!("audio_route: no path '{p}'");
            }
        }
        // SAFETY: live handle.
        unsafe { audio_route_update_mixer(self.ar) };
    }

    fn reset(&mut self, paths: &[String]) {
        for p in paths.iter().rev() {
            let Ok(c) = CString::new(p.as_str()) else { continue };
            // SAFETY: live handle, valid string.
            unsafe { audio_route_reset_path(self.ar, c.as_ptr()) };
        }
        // SAFETY: live handle.
        unsafe { audio_route_update_mixer(self.ar) };
    }
}

impl Drop for Route {
    fn drop(&mut self) {
        // SAFETY: `ar` came from audio_route_init and is freed exactly once.
        unsafe { audio_route_free(self.ar) };
    }
}

/// Owned PCM handle.
struct PcmHandle {
    pcm: *mut Pcm,
}

// SAFETY: same single-owner argument as `Route`.
unsafe impl Send for PcmHandle {}

impl Drop for PcmHandle {
    fn drop(&mut self) {
        // SAFETY: `pcm` came from pcm_open and is closed exactly once.
        unsafe { pcm_close(self.pcm) };
    }
}

pub struct TinyAlsaOut {
    card: u32,
    device: u32,
    xml: String,
    paths: Vec<String>,
    route: Option<Route>,
    pcm: Option<PcmHandle>,
    routed: bool,
    created_at: u64,
    label: &'static str,
}

impl TinyAlsaOut {
    /// Defaults are the a52sxq jack route; each can be overridden by a
    /// property for bring-up without a rebuild.
    pub fn from_props() -> Self {
        let card =
            control::prop_str("persist.abyss.audio.card").and_then(|s| s.parse().ok()).unwrap_or(0);
        let device = control::prop_str("persist.abyss.audio.device")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let xml = control::prop_str("persist.abyss.audio.mixer_xml")
            .unwrap_or_else(|| "/vendor/etc/mixer_paths.xml".into());
        let paths = control::prop_str("persist.abyss.audio.paths")
            .unwrap_or_else(|| "deep-buffer-playback headphones,headset".into())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        TinyAlsaOut {
            card,
            device,
            xml,
            paths,
            route: None,
            pcm: None,
            routed: false,
            created_at: clock::now_ms(),
            label: "tinyalsa-jack",
        }
    }

    /// The deep-buffer front end routed to the Bluetooth backend (A2DP offload, CPU decode).
    /// No settle wait: Bluetooth is switched on long after the audio HAL restart at entry.
    pub fn bluetooth() -> Self {
        let mut t = TinyAlsaOut::from_props();
        t.paths = prop_paths("persist.abyss.audio.bt_paths", "deep-buffer-playback bt-a2dp,bt-a2dp");
        t.created_at = 0;
        t.label = "bluetooth-dsp";
        t
    }

    /// "Audio settle": stopping audioserver makes init restart the vendor
    /// audio HAL a few hundred ms after entry, and the restart resets the
    /// mixer. Wait until the HAL service runs again and at least 3 s have
    /// passed since start before touching the mixer (bounded to 6 s).
    fn wait_settle(&self) {
        wait_settle(self.created_at);
    }

    fn try_open(&self, rate: u32, period: u32, count: u32) -> Result<PcmHandle> {
        open_pcm(self.card, self.device, rate, PCM_FORMAT_S16_LE, period, count)
    }
}

/// Shared by the PCM and the compress jack backends (see `TinyAlsaOut::wait_settle`).
fn wait_settle(created_at: u64) {
    let deadline = created_at + 6000;
    loop {
        let now = clock::now_ms();
        let hal = control::prop_str("init.svc.vendor.audio-hal");
        let hal_ok = hal.as_deref().is_none_or(|s| s == "running");
        if (hal_ok && now >= created_at + 3000) || now >= deadline {
            if !hal_ok {
                warn!("vendor.audio-hal is {hal:?}; routing anyway");
            }
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn open_pcm(
    card: u32,
    device: u32,
    rate: u32,
    format: c_int,
    period: u32,
    count: u32,
) -> Result<PcmHandle> {
    let mut cfg = PcmConfig {
        channels: 2,
        rate,
        period_size: period,
        period_count: count,
        format,
        start_threshold: 0,
        stop_threshold: 0,
        silence_threshold: 0,
        silence_size: 0,
        avail_min: 0,
    };
    // SAFETY: `cfg` is a valid pcm_config for the duration of the call;
    // pcm_open copies what it needs and always returns a non-NULL handle
    // (a "bad pcm" on failure) that must be closed.
    let pcm = unsafe { pcm_open(card, device, PCM_OUT, &mut cfg) };
    if pcm.is_null() {
        return Err(anyhow!("pcm_open returned NULL"));
    }
    let h = PcmHandle { pcm };
    // SAFETY: `h.pcm` is the live handle from pcm_open.
    if unsafe { pcm_is_ready(h.pcm) } == 0 {
        // SAFETY: pcm_get_error returns a pointer into the pcm struct,
        // valid until pcm_close (which `h`'s Drop does after we copy it).
        let msg = unsafe { CStr::from_ptr(pcm_get_error(h.pcm)) }.to_string_lossy().into_owned();
        return Err(anyhow!("pcm_open hw:{card},{device} @{rate} fmt {format}: {msg}"));
    }
    Ok(h)
}

/// Blocking write of a plain-data slice (`i16` samples or packed bytes).
fn write_pcm<T: Copy>(h: &PcmHandle, data: &[T]) -> Result<()> {
    let len = std::mem::size_of_val(data) as c_uint;
    // SAFETY: `data` is a valid slice of `len` bytes for the call; `h.pcm` is
    // a live, ready handle.
    let r = unsafe { pcm_write(h.pcm, data.as_ptr() as *const c_void, len) };
    if r != 0 {
        // SAFETY: as in open_pcm.
        let msg = unsafe { CStr::from_ptr(pcm_get_error(h.pcm)) }.to_string_lossy().into_owned();
        return Err(anyhow!("pcm_write: {msg}"));
    }
    Ok(())
}

impl AudioOut for TinyAlsaOut {
    fn open(&mut self, rate: u32) -> Result<u32> {
        if self.route.is_none() {
            self.wait_settle();
            self.route = Route::new(self.card, &self.xml);
            if self.route.is_none() {
                return Err(anyhow!("audio_route_init({}, {}) failed", self.card, self.xml));
            }
        }
        if !self.routed {
            let paths = self.paths.clone();
            if let Some(r) = self.route.as_mut() {
                r.apply(&paths);
            }
            self.routed = true;
        }
        self.pcm = None;
        // The Q6 front end resamples to the backend rate itself, so try the
        // file's rate first; fall back to 48 kHz (the player then resamples).
        // ~85 ms periods × 4 keep AP wake-ups lower than the HAL's 20 ms.
        let mut last = None;
        for &(r, period, count) in &[(rate, 4096, 4), (48000, 4096, 4), (48000, 960, 5)] {
            match self.try_open(r, period, count) {
                Ok(h) => {
                    info!(
                        "tinyalsa: hw:{},{} {r} Hz period {period}x{count}",
                        self.card, self.device
                    );
                    self.pcm = Some(h);
                    return Ok(r);
                }
                Err(e) => {
                    warn!("{e}");
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("pcm_open failed")))
    }

    fn write(&mut self, samples: &[i16]) -> Result<()> {
        let h = self.pcm.as_ref().ok_or_else(|| anyhow!("pcm not open"))?;
        write_pcm(h, samples)
    }

    fn standby(&mut self) {
        self.pcm = None; // pcm_close
        if self.routed {
            let paths = self.paths.clone();
            if let Some(r) = self.route.as_mut() {
                r.reset(&paths);
            }
            self.routed = false;
        }
    }

    fn is_open(&self) -> bool {
        self.pcm.is_some()
    }

    fn name(&self) -> &'static str {
        self.label
    }

    fn hal_restarted(&mut self) {
        // audio_route caches mixer values and only writes differences, so a
        // reset mixer needs a fresh audio_route (re-reads the kernel state).
        self.pcm = None;
        self.route = None;
        self.routed = false;
    }
}

/// Picks the jack backend; NullOut if routing cannot even be initialised.
pub fn jack_backend() -> Box<dyn AudioOut> {
    let t = TinyAlsaOut::from_props();
    if std::path::Path::new(&t.xml).exists() {
        Box::new(t)
    } else {
        warn!("{} missing, audio goes to NullOut", t.xml);
        Box::new(NullOut::new())
    }
}

// ---------------------------------------------------------------------------
// Output selection and the USB DAC backend.

/// Where the music goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    Jack,
    Usb { card: u32, name: String },
    /// A2DP headphones connected through abyss-btd.
    Bluetooth { addr: String, name: String },
}

impl Output {
    /// What the screen says, e.g. "3.5 mm jack" or "USB · FiiO BTR5".
    pub fn label(&self) -> String {
        match self {
            Output::Jack => "3.5 mm jack".into(),
            Output::Usb { name, .. } => format!("USB · {name}"),
            Output::Bluetooth { name, .. } => format!("Bluetooth · {name}"),
        }
    }
    pub fn short(&self) -> &'static str {
        match self {
            Output::Jack => "3.5 mm",
            Output::Usb { .. } => "USB",
            Output::Bluetooth { .. } => "Bluetooth",
        }
    }
    /// The same output came back (for the 5 min replug resume). A USB DAC is
    /// matched by name: its card number may change on replug.
    pub fn same(&self, o: &Output) -> bool {
        match (self, o) {
            (Output::Jack, Output::Jack) => true,
            (Output::Usb { name: a, .. }, Output::Usb { name: b, .. }) => a == b,
            (Output::Bluetooth { addr: a, .. }, Output::Bluetooth { addr: b, .. }) => a == b,
            _ => false,
        }
    }
}

/// USB audio cards with a playback PCM, from /proc/asound/cards:
/// ` 1 [BTR5           ]: USB-Audio - FiiO BTR5`.
pub fn usb_cards() -> Vec<(u32, String)> {
    let Ok(text) = std::fs::read_to_string("/proc/asound/cards") else { return Vec::new() };
    parse_cards(&text)
        .into_iter()
        .filter(|(card, _)| std::path::Path::new(&format!("/proc/asound/card{card}/pcm0p")).exists())
        .collect()
}

fn parse_cards(text: &str) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        let Some(num) = t.split_whitespace().next().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let Some((_, rest)) = t.split_once("]: ") else { continue };
        let (driver, name) = rest.split_once(" - ").unwrap_or((rest, rest));
        if driver.trim() == "USB-Audio" {
            out.push((num, name.trim().to_string()));
        }
    }
    out
}

pub struct UsbOut {
    card: u32,
    pcm: Option<PcmHandle>,
    format: c_int,
    bytes: Vec<u8>,
}

impl UsbOut {
    pub fn new(card: u32) -> Self {
        UsbOut { card, pcm: None, format: PCM_FORMAT_S16_LE, bytes: Vec::new() }
    }

    /// (min rate, max rate, supported formats in preference order).
    fn params(&self) -> (u32, u32, Vec<c_int>) {
        let all = [PCM_FORMAT_S16_LE, PCM_FORMAT_S32_LE, PCM_FORMAT_S24_3LE, PCM_FORMAT_S24_LE];
        // SAFETY: plain query; the returned pointer is checked and freed below.
        let p = unsafe { pcm_params_get(self.card, 0, PCM_OUT) };
        if p.is_null() {
            return (8000, 384000, all.to_vec());
        }
        // SAFETY: `p` is a live pcm_params from pcm_params_get, freed once below.
        let (lo, hi) =
            unsafe { (pcm_params_get_min(p, PCM_PARAM_RATE), pcm_params_get_max(p, PCM_PARAM_RATE)) };
        // SAFETY: as above.
        let fmts: Vec<c_int> =
            all.iter().copied().filter(|&f| unsafe { pcm_params_format_test(p, f) } != 0).collect();
        // SAFETY: freed exactly once.
        unsafe { pcm_params_free(p) };
        let fmts = if fmts.is_empty() { all.to_vec() } else { fmts };
        (lo.max(1), hi.max(lo.max(1)), fmts)
    }
}

impl AudioOut for UsbOut {
    fn open(&mut self, rate: u32) -> Result<u32> {
        self.pcm = None;
        let (lo, hi, fmts) = self.params();
        info!("usb: card {} rates {lo}..{hi} formats {fmts:?}", self.card);
        let mut rates = vec![rate, 48000, 44100, 96000];
        rates.dedup();
        let mut last = None;
        for &r in rates.iter().filter(|&&r| (lo..=hi).contains(&r)) {
            for &f in &fmts {
                match open_pcm(self.card, 0, r, f, 1024, 4) {
                    Ok(h) => {
                        info!("usb: hw:{},0 {r} Hz format {f}", self.card);
                        self.pcm = Some(h);
                        self.format = f;
                        return Ok(r);
                    }
                    Err(e) => {
                        warn!("{e}");
                        last = Some(e);
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("USB DAC: no usable rate in {lo}..{hi}")))
    }

    fn write(&mut self, samples: &[i16]) -> Result<()> {
        let h = self.pcm.as_ref().ok_or_else(|| anyhow!("pcm not open"))?;
        self.bytes.clear();
        for &s in samples {
            match self.format {
                PCM_FORMAT_S32_LE => self.bytes.extend(((s as i32) << 16).to_le_bytes()),
                PCM_FORMAT_S24_LE => self.bytes.extend(((s as i32) << 8).to_le_bytes()),
                PCM_FORMAT_S24_3LE => self.bytes.extend(&((s as i32) << 8).to_le_bytes()[..3]),
                _ => self.bytes.extend(s.to_le_bytes()),
            }
        }
        write_pcm(h, &self.bytes)
    }

    fn standby(&mut self) {
        self.pcm = None;
    }

    fn is_open(&self) -> bool {
        self.pcm.is_some()
    }

    fn name(&self) -> &'static str {
        "tinyalsa-usb"
    }
}

// ---------------------------------------------------------------------------
// Bluetooth headphones through abyss-btd (protocol: bt.rs / abyss-bt-protocol.md).

/// PCM to abyss-btd. `open` sends `play` and waits for "audio started"; `write`
/// sends 20 ms packets (0x01 + s16le stereo) and blocks on the helper's
/// backpressure, which paces the decoder like pcm_write does; `standby` sends
/// `pause` (the helper suspends the A2DP stream and flushes its ring).
pub struct BtOut {
    bt: std::sync::Arc<bt::Shared>,
    fd: Option<std::sync::Arc<std::os::fd::OwnedFd>>,
    fmt_gen: u64,
    link_gen: u64,
    pkt: Vec<u8>,
    /// Offload mode: PCM to the DSP encoder instead of the socket.
    dsp: Option<BtDspOut>,
    use_dsp: bool,
}

impl BtOut {
    pub fn new(bt: std::sync::Arc<bt::Shared>) -> Self {
        BtOut {
            bt,
            fd: None,
            fmt_gen: 0,
            link_gen: 0,
            pkt: Vec::with_capacity(1 + bt::PCM_FRAMES_PER_PACKET * 4),
            dsp: None,
            use_dsp: false,
        }
    }
}

impl AudioOut for BtOut {
    fn open(&mut self, rate: u32) -> Result<u32> {
        self.fd = None;
        if self.bt.offload() {
            let bt = self.bt.clone();
            let d = self.dsp.get_or_insert_with(|| BtDspOut::new(bt));
            match d.open(rate) {
                Ok(r) => {
                    self.use_dsp = true;
                    return Ok(r);
                }
                Err(e) => {
                    self.use_dsp = false;
                    return Err(e);
                }
            }
        }
        self.use_dsp = false;
        // The A2DP codec fixes the rate (SBC/AAC: 44.1 or 48 kHz); the player
        // resamples from the file's rate.
        let (rate, fmt_gen, link_gen, fd) = self.bt.start_stream()?;
        info!("bluetooth: stream started, {rate} Hz s16 stereo");
        self.fd = Some(fd);
        self.fmt_gen = fmt_gen;
        self.link_gen = link_gen;
        Ok(rate)
    }

    fn write(&mut self, samples: &[i16]) -> Result<()> {
        if self.use_dsp {
            return match self.dsp.as_mut() {
                Some(d) => d.write(samples),
                None => Err(anyhow!("bluetooth dsp stream not open")),
            };
        }
        let Some(fd) = self.fd.clone() else { return Err(anyhow!("bluetooth stream not open")) };
        match self.bt.still_valid(self.fmt_gen, self.link_gen) {
            Ok(true) => {}
            Ok(false) => {
                // New codec format, or the headset/stack suspended the stream:
                // close, the player reopens at the next packet (new rate, `play`).
                info!("bluetooth: format changed or stream suspended; reopening");
                self.fd = None;
                return Ok(());
            }
            Err(e) => {
                // Closed first, so the player pauses instead of skipping the track.
                self.fd = None;
                return Err(e);
            }
        }
        for chunk in samples.chunks(bt::PCM_FRAMES_PER_PACKET * 2) {
            self.pkt.clear();
            self.pkt.push(bt::PKT_PCM);
            for &s in chunk {
                self.pkt.extend(s.to_le_bytes());
            }
            if let Err(e) = bt::send_pcm(&fd, &self.pkt) {
                self.fd = None;
                return Err(e);
            }
        }
        Ok(())
    }

    fn standby(&mut self) {
        if let Some(d) = self.dsp.as_mut() {
            d.standby();
        }
        self.use_dsp = false;
        if self.fd.take().is_some() {
            self.bt.suspend_stream();
        }
    }

    fn is_open(&self) -> bool {
        if self.use_dsp {
            return self.dsp.as_ref().is_some_and(|d| d.is_open());
        }
        self.fd.is_some()
    }

    fn name(&self) -> &'static str {
        if self.use_dsp {
            "bluetooth-dsp"
        } else {
            "bluetooth"
        }
    }

    fn hal_restarted(&mut self) {
        if let Some(d) = self.dsp.as_mut() {
            d.hal_restarted();
        }
    }
}

/// The A2DP offload stream with abyss-btd plus the DSP's Bluetooth encoder: `prepare`
/// before routing a front end to SLIMBUS_7_RX, `release` after the route is down
/// (the order of the vendor HAL: a2dp_start_playback in enable_snd_device, reset_codec_config
/// after the stream stopped).
pub struct BtLink {
    bt: std::sync::Arc<bt::Shared>,
    enc: crate::a2dp_dsp::BtEncoder,
    started: bool,
}

impl BtLink {
    pub fn new(bt: std::sync::Arc<bt::Shared>, card: u32) -> BtLink {
        BtLink { bt, enc: crate::a2dp_dsp::BtEncoder::new(card), started: false }
    }

    /// Starts the A2DP stream (`play`; the stack sends the offload start command) and
    /// programs the encoder. A refused encoder config switches the helper to software.
    pub fn prepare(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }
        let c = self.bt.start_offload()?;
        if let Err(e) = self.enc.program(&c) {
            self.bt.suspend_stream();
            self.bt.request_software();
            return Err(e.context(format!("DSP encoder for {}", c.label())));
        }
        info!("bluetooth: offload stream started, {}", c.label());
        self.started = true;
        Ok(())
    }

    /// The DSP would not take the stream (open/start failed after `prepare`).
    pub fn failed(&mut self) {
        self.release();
        self.bt.request_software();
    }

    pub fn release(&mut self) {
        if self.started {
            self.enc.reset();
            self.bt.suspend_stream();
            self.started = false;
        }
    }

    pub fn hal_restarted(&mut self) {
        // The mixer is back at defaults; the A2DP stream itself is still started (a
        // repeated `play` is answered at once), so only the encoder must be set again.
        self.enc.hal_restarted();
        self.started = false;
    }
}

/// PCM (CPU decode) to the DSP's Bluetooth encoder.
pub struct BtDspOut {
    pcm: TinyAlsaOut,
    link: BtLink,
}

impl BtDspOut {
    pub fn new(bt: std::sync::Arc<bt::Shared>) -> BtDspOut {
        let pcm = TinyAlsaOut::bluetooth();
        let card = pcm.card;
        BtDspOut { pcm, link: BtLink::new(bt, card) }
    }

    fn open(&mut self, rate: u32) -> Result<u32> {
        self.link.prepare()?;
        match self.pcm.open(rate) {
            Ok(r) => Ok(r),
            Err(e) => {
                self.pcm.standby();
                self.link.failed();
                Err(e)
            }
        }
    }

    fn write(&mut self, samples: &[i16]) -> Result<()> {
        self.pcm.write(samples)
    }

    fn standby(&mut self) {
        self.pcm.standby();
        self.link.release();
    }

    fn is_open(&self) -> bool {
        self.pcm.is_open()
    }

    fn hal_restarted(&mut self) {
        self.pcm.hal_restarted();
        self.link.hal_restarted();
    }
}

// ---------------------------------------------------------------------------
// Compress offload (ADSP decode) for the 3.5 mm jack.
//
// Layouts from the device kernel: include/uapi/sound/compress_offload.h,
// include/uapi/sound/compress_params.h (all packed + aligned(4); sizes checked
// on the host against those headers: snd_codec 120, snd_compr_params 132,
// snd_compr_tstamp 20, snd_compr_avail 28) and techpack/audio/include/uapi/
// audio/sound/audio_compressed_formats.h (FLAC is SND_AUDIOCODEC_BESPOKE with
// options.generic.reserved[0] = AUDIO_COMP_FORMAT_FLAC and a
// snd_generic_dec_flac from reserved[1]; msm-compress-q6-v2.c
// msm_compr_get_decoder_format + FORMAT_FLAC media format block).

/// Codecs the ADSP decodes here (Opus/Vorbis/hi-res stay on the CPU path).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OffloadCodec {
    Flac,
    Mp3,
}

impl OffloadCodec {
    pub fn name(self) -> &'static str {
        match self {
            OffloadCodec::Flac => "flac",
            OffloadCodec::Mp3 => "mp3",
        }
    }
}

/// FLAC STREAMINFO values the DSP decoder needs (asm_flac_cfg).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlacInfo {
    pub min_blk: u16,
    pub max_blk: u16,
    pub min_frame: u32,
    pub max_frame: u32,
    pub rate: u32,
    pub channels: u32,
    pub bits: u32,
}

impl FlacInfo {
    /// Parses the 34-byte STREAMINFO block (symphonia puts it in
    /// `CodecParams::extra_data` for FLAC).
    pub fn from_streaminfo(b: &[u8]) -> Option<FlacInfo> {
        if b.len() < 18 {
            return None;
        }
        let u24 = |i: usize| (b[i] as u32) << 16 | (b[i + 1] as u32) << 8 | b[i + 2] as u32;
        let packed = u64::from_be_bytes([b[10], b[11], b[12], b[13], b[14], b[15], b[16], b[17]]);
        Some(FlacInfo {
            min_blk: u16::from_be_bytes([b[0], b[1]]),
            max_blk: u16::from_be_bytes([b[2], b[3]]),
            min_frame: u24(4),
            max_frame: u24(7),
            rate: (packed >> 44) as u32 & 0xF_FFFF,
            channels: ((packed >> 41) as u32 & 0x7) + 1,
            bits: ((packed >> 36) as u32 & 0x1F) + 1,
        })
    }
}

/// What one compress stream is opened with.
#[derive(Clone, Copy, Debug)]
pub struct OffloadFormat {
    pub codec: OffloadCodec,
    pub rate: u32,
    pub channels: u32,
    /// Bits per decoded sample (16 or 24 for FLAC; 16 for MP3).
    pub bits: u32,
    pub flac: FlacInfo,
}

const SND_AUDIOCODEC_MP3: u32 = 0x2;
const SND_AUDIOCODEC_BESPOKE: u32 = 0xE;
const SND_AUDIOSTREAMFORMAT_RAW: u32 = 0x40;
const AUDIO_COMP_FORMAT_FLAC: u32 = 0x5;
/// msm-compress-q6-v2.c COMPRESSED_LR_VOL_MAX_STEPS (= unity gain).
pub const COMPRESS_VOL_UNITY: u32 = 0x2000;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SndCodec {
    id: u32,
    ch_in: u32,
    ch_out: u32,
    sample_rate: u32,
    bit_rate: u32,
    rate_control: u32,
    profile: u32,
    level: u32,
    ch_mode: u32,
    format: u32,
    align: u32,
    /// union snd_codec_options (64 bytes); generic = { bw, reserved[15] }.
    options: [u32; 16],
    /// reserved[0] = compr_passthr (0 = LEGACY_PCM, i.e. decode), [1] = flags.
    reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SndComprParams {
    fragment_size: u32,
    fragments: u32,
    codec: SndCodec,
    no_wake_mode: u8,
    _pad: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SndComprTstamp {
    byte_offset: u32,
    copied_total: u32,
    pcm_frames: u32,
    pcm_io_frames: u32,
    sampling_rate: u32,
}

// ioctl numbers (asm-generic encoding, same on arm64), computed from the
// device kernel headers above.
const SNDRV_COMPRESS_IOCTL_VERSION: u32 = 0x8004_4300;
const SNDRV_COMPRESS_SET_PARAMS: u32 = 0x4084_4312;
const SNDRV_COMPRESS_TSTAMP: u32 = 0x8014_4320;
const SNDRV_COMPRESS_PAUSE: u32 = 0x4330;
const SNDRV_COMPRESS_RESUME: u32 = 0x4331;
const SNDRV_COMPRESS_START: u32 = 0x4332;
const SNDRV_COMPRESS_STOP: u32 = 0x4333;
const SNDRV_COMPRESS_DRAIN: u32 = 0x4334;

fn codec_for(fmt: &OffloadFormat) -> SndCodec {
    let mut c = SndCodec {
        ch_in: fmt.channels,
        ch_out: fmt.channels,
        sample_rate: fmt.rate,
        format: SND_AUDIOSTREAMFORMAT_RAW,
        ..Default::default()
    };
    match fmt.codec {
        OffloadCodec::Mp3 => c.id = SND_AUDIOCODEC_MP3,
        OffloadCodec::Flac => {
            c.id = SND_AUDIOCODEC_BESPOKE;
            // options.generic.bw = options[0]; generic.reserved[0] = options[1].
            c.options[1] = AUDIO_COMP_FORMAT_FLAC;
            // struct snd_generic_dec_flac (5 x u16, little endian) from
            // generic.reserved[1] = options[2].
            let f = &fmt.flac;
            let min_frame = f.min_frame.min(0xFFFF);
            let max_frame = f.max_frame.min(0xFFFF);
            c.options[2] = fmt.bits | (f.min_blk as u32) << 16;
            c.options[3] = f.max_blk as u32 | min_frame << 16;
            c.options[4] = max_frame;
        }
    }
    c
}

/// One open compress stream (/dev/snd/comprCxDy). The ioctls used here are
/// safe to call from several threads (the drain thread and the UI's position
/// read run next to the player): with CONFIG_AUDIO_QGKI=y (lineage-a52sxq_
/// defconfig) DRAIN/TSTAMP/AVAIL skip the device mutex
/// (sound/core/compress_offload.c snd_compress_simple_ioctls).
pub struct CompressStream {
    fd: std::os::fd::OwnedFd,
}

/// What `CompressStream::wait` returned for.
#[derive(Debug, PartialEq, Eq)]
pub enum Wait {
    /// At least one fragment is free.
    Writable,
    /// A byte arrived on the wake fd (a command for the player).
    Woken,
    Timeout,
}

impl CompressStream {
    fn raw(&self) -> c_int {
        use std::os::fd::AsRawFd;
        self.fd.as_raw_fd()
    }

    fn ioctl_ptr<T>(&self, req: u32, arg: *mut T, what: &str) -> Result<()> {
        // SAFETY: `self.fd` is an open compress device; `arg` points to a live
        // value of the size encoded in `req` (or is NULL for _IO requests).
        let r = unsafe { libc::ioctl(self.raw(), req as _, arg) };
        if r < 0 {
            return Err(anyhow!("compress {what}: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }

    fn ioctl_none(&self, req: u32, what: &str) -> Result<()> {
        self.ioctl_ptr::<c_void>(req, std::ptr::null_mut(), what)
    }

    pub fn start(&self) -> Result<()> {
        self.ioctl_none(SNDRV_COMPRESS_START, "start")
    }
    pub fn pause(&self) -> Result<()> {
        self.ioctl_none(SNDRV_COMPRESS_PAUSE, "pause")
    }
    pub fn resume(&self) -> Result<()> {
        self.ioctl_none(SNDRV_COMPRESS_RESUME, "resume")
    }
    /// Also interrupts a DRAIN blocked in another thread (msm_compr_trigger
    /// STOP sets cmd_interrupt and wakes drain_wait).
    pub fn stop(&self) -> Result<()> {
        self.ioctl_none(SNDRV_COMPRESS_STOP, "stop")
    }

    /// Blocks until the DSP has played everything written (sends the last,
    /// partial fragment + EOS). Interrupted by the freezer on suspend
    /// (wait_event_interruptible in msm_compr_drain_buffer): retried unless
    /// `stop` is set.
    pub fn drain(&self, stop: &std::sync::atomic::AtomicBool) -> Result<()> {
        loop {
            // SAFETY: _IO request without argument on an open compress fd.
            let r = unsafe { libc::ioctl(self.raw(), SNDRV_COMPRESS_DRAIN as _, 0) };
            if r == 0 {
                return Ok(());
            }
            let e = std::io::Error::last_os_error();
            // 512 = ERESTARTSYS, can leak out of driver ioctls.
            let again = matches!(e.raw_os_error(), Some(libc::EINTR) | Some(512));
            if !again || stop.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(anyhow!("compress drain: {e}"));
            }
        }
    }

    /// Hands as much of `data` to the driver as fits; 0 = the buffer is full.
    /// (snd_compr_write never blocks: it copies min(avail, count).)
    pub fn write(&self, data: &[u8]) -> Result<usize> {
        // SAFETY: `data` is a valid slice for the duration of the call.
        let r = unsafe { libc::write(self.raw(), data.as_ptr() as *const c_void, data.len()) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::WouldBlock || e.raw_os_error() == Some(libc::EINTR) {
                return Ok(0);
            }
            return Err(anyhow!("compress write: {e}"));
        }
        Ok(r as usize)
    }

    /// (decoded frames played since the stream started, their rate).
    pub fn tstamp(&self) -> Option<(u32, u32)> {
        let mut t = SndComprTstamp::default();
        self.ioctl_ptr(SNDRV_COMPRESS_TSTAMP, &mut t, "tstamp").ok()?;
        Some((t.pcm_io_frames, t.sampling_rate))
    }

    /// Sleeps until a fragment is free (the DSP's write-done interrupt) or
    /// `wake` is readable. No wakelock is held: the AP may suspend meanwhile.
    pub fn wait(&self, wake: c_int, timeout_ms: i32) -> Result<Wait> {
        let mut fds = [
            libc::pollfd { fd: self.raw(), events: libc::POLLOUT, revents: 0 },
            libc::pollfd { fd: wake, events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: `fds` is a valid array of 2 pollfd for the call.
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout_ms) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                return Ok(Wait::Timeout);
            }
            return Err(anyhow!("compress poll: {e}"));
        }
        if fds[1].revents & libc::POLLIN != 0 {
            return Ok(Wait::Woken);
        }
        if fds[0].revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            return Err(anyhow!("compress poll: error state (xrun / DSP reset)"));
        }
        if fds[0].revents & libc::POLLOUT != 0 {
            return Ok(Wait::Writable);
        }
        Ok(Wait::Timeout)
    }
}

/// tinyalsa mixer (for the stream volume control).
#[repr(C)]
struct Mixer {
    _private: [u8; 0],
}
#[repr(C)]
struct MixerCtl {
    _private: [u8; 0],
}

extern "C" {
    fn mixer_open(card: c_uint) -> *mut Mixer;
    fn mixer_close(mixer: *mut Mixer);
    fn mixer_get_ctl_by_name(mixer: *mut Mixer, name: *const c_char) -> *mut MixerCtl;
    fn mixer_ctl_get_num_values(ctl: *mut MixerCtl) -> c_uint;
    fn mixer_ctl_set_value(ctl: *mut MixerCtl, id: c_uint, value: c_int) -> c_int;
}

struct MixerHandle {
    m: *mut Mixer,
}

// SAFETY: owned by the player thread only (same argument as `Route`).
unsafe impl Send for MixerHandle {}

impl Drop for MixerHandle {
    fn drop(&mut self) {
        // SAFETY: `m` came from mixer_open and is closed exactly once.
        unsafe { mixer_close(self.m) };
    }
}

/// Fragment plans, tried in order: 1 MB x 2 (one fragment in flight at the
/// DSP, the other being refilled: the AP wakes once per MB), then the HAL's
/// FLAC size, then the HAL default (vendor.audio.offload.buffer.size.kb=32).
const DEFAULT_FRAGMENTS: [(u32, u32); 3] = [(1 << 20, 2), (256 << 10, 4), (32 << 10, 4)];

/// The 3.5 mm jack through the ADSP decoder.
pub struct CompressOut {
    card: u32,
    device: u32,
    xml: String,
    paths: Vec<String>,
    jack_paths: Vec<String>,
    bt_paths: Vec<String>,
    /// Some = the Bluetooth target (A2DP offload); None = the jack.
    link: Option<BtLink>,
    vol_ctl: String,
    plans: Vec<(u32, u32)>,
    route: Option<Route>,
    routed: bool,
    mixer: Option<MixerHandle>,
    created_at: u64,
}

impl CompressOut {
    /// Defaults are the a52sxq offload route; properties override them for
    /// bring-up (persist.abyss.audio.offload_device / offload_paths /
    /// offload_frags = "1048576x2,262144x4").
    pub fn from_props() -> Self {
        let card =
            control::prop_str("persist.abyss.audio.card").and_then(|s| s.parse().ok()).unwrap_or(0);
        let device = control::prop_str("persist.abyss.audio.offload_device")
            .and_then(|s| s.parse().ok())
            .unwrap_or(7);
        let xml = control::prop_str("persist.abyss.audio.mixer_xml")
            .unwrap_or_else(|| "/vendor/etc/mixer_paths.xml".into());
        let paths = prop_paths("persist.abyss.audio.offload_paths", "compress-offload-playback headphones,headset");
        let bt_paths =
            prop_paths("persist.abyss.audio.bt_offload_paths", "compress-offload-playback bt-a2dp,bt-a2dp");
        let plans = control::prop_str("persist.abyss.audio.offload_frags")
            .map(|s| parse_plans(&s))
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| DEFAULT_FRAGMENTS.to_vec());
        CompressOut {
            card,
            device,
            xml,
            jack_paths: paths.clone(),
            paths,
            bt_paths,
            link: None,
            vol_ctl: format!("Compress Playback {device} Volume"),
            plans,
            route: None,
            routed: false,
            mixer: None,
            created_at: clock::now_ms(),
        }
    }

    /// Routes the jack to the offload front end, sets the stream volume and
    /// opens a stream for `fmt`. Refuses (caller falls back to CPU decode) if
    /// the volume control is missing: never play at an unknown level.
    pub fn open(&mut self, fmt: &OffloadFormat, vol_q13: u32) -> Result<std::sync::Arc<CompressStream>> {
        if !std::path::Path::new(&self.xml).exists() {
            return Err(anyhow!("{} missing", self.xml));
        }
        if self.route.is_none() {
            wait_settle(self.created_at);
            self.route = Route::new(self.card, &self.xml);
            if self.route.is_none() {
                return Err(anyhow!("audio_route_init({}, {}) failed", self.card, self.xml));
            }
        }
        if self.mixer.is_none() {
            // SAFETY: plain open; NULL is checked.
            let m = unsafe { mixer_open(self.card) };
            if m.is_null() {
                return Err(anyhow!("mixer_open({}) failed", self.card));
            }
            self.mixer = Some(MixerHandle { m });
        }
        // The volume is stored per front end and applied at START
        // (msm_compr_trigger), so set it before the stream exists.
        self.set_volume(vol_q13)?;
        // Bluetooth: A2DP stream started + encoder programmed before the backend is routed.
        if let Some(l) = self.link.as_mut() {
            l.prepare()?;
        }
        if !self.routed {
            let paths = self.paths.clone();
            if let Some(r) = self.route.as_mut() {
                r.apply(&paths);
            }
            self.routed = true;
        }
        let dev = format!("/dev/snd/comprC{}D{}", self.card, self.device);
        let mut last = None;
        for &(size, count) in &self.plans {
            match open_compress(&dev, fmt, size, count) {
                Ok(s) => {
                    info!(
                        "offload: {} {}/{} {}ch fragment {size}x{count} on {dev}",
                        fmt.codec.name(),
                        fmt.rate,
                        fmt.bits,
                        fmt.channels
                    );
                    return Ok(std::sync::Arc::new(s));
                }
                Err(e) => {
                    warn!("offload: fragment {size}x{count}: {e:#}");
                    last = Some(e);
                }
            }
        }
        if let Some(l) = self.link.as_mut() {
            // The DSP did not take the stream on the Bluetooth port (e.g. the encoder
            // block): software encoding from here.
            let paths = self.paths.clone();
            if let Some(r) = self.route.as_mut() {
                r.reset(&paths);
            }
            self.routed = false;
            l.failed();
        }
        self.standby();
        Err(last.unwrap_or_else(|| anyhow!("no fragment plan")))
    }

    /// Q13 linear gain on both channels (0x2000 = 0 dB).
    pub fn set_volume(&mut self, q13: u32) -> Result<()> {
        let m = self.mixer.as_ref().ok_or_else(|| anyhow!("mixer not open"))?;
        let name = CString::new(self.vol_ctl.as_str())?;
        // SAFETY: live mixer, valid name; the ctl pointer belongs to the mixer.
        let ctl = unsafe { mixer_get_ctl_by_name(m.m, name.as_ptr()) };
        if ctl.is_null() {
            return Err(anyhow!("no mixer control '{}'", self.vol_ctl));
        }
        let v = q13.min(COMPRESS_VOL_UNITY) as c_int;
        // SAFETY: `ctl` is a live control of the open mixer.
        let n = unsafe { mixer_ctl_get_num_values(ctl) }.clamp(1, 2);
        for i in 0..n {
            // SAFETY: as above; `i` < number of values.
            if unsafe { mixer_ctl_set_value(ctl, i, v) } != 0 {
                return Err(anyhow!("set '{}' = {v} failed", self.vol_ctl));
            }
        }
        Ok(())
    }

    /// Powers the offload route down (the stream must be closed already); Bluetooth:
    /// then suspends the A2DP stream and resets the encoder.
    pub fn standby(&mut self) {
        if self.routed {
            let paths = self.paths.clone();
            if let Some(r) = self.route.as_mut() {
                r.reset(&paths);
            }
            self.routed = false;
        }
        if let Some(l) = self.link.as_mut() {
            l.release();
        }
        self.mixer = None;
    }

    /// Points the DSP path at the jack (None) or at Bluetooth headphones. Powers down
    /// whatever the old target had first.
    pub fn set_bluetooth(&mut self, bt: Option<std::sync::Arc<bt::Shared>>) {
        if bt.is_some() == self.link.is_some() {
            return;
        }
        self.standby();
        match bt {
            Some(b) => {
                self.link = Some(BtLink::new(b, self.card));
                self.paths = self.bt_paths.clone();
            }
            None => {
                self.link = None;
                self.paths = self.jack_paths.clone();
            }
        }
    }

    pub fn is_routed(&self) -> bool {
        self.routed
    }

    pub fn hal_restarted(&mut self) {
        self.route = None;
        self.routed = false;
        self.mixer = None;
        if let Some(l) = self.link.as_mut() {
            l.hal_restarted();
        }
    }
}

/// A comma-separated mixer path list from a property, or the default.
fn prop_paths(prop: &str, default: &str) -> Vec<String> {
    control::prop_str(prop)
        .unwrap_or_else(|| default.into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_plans(s: &str) -> Vec<(u32, u32)> {
    s.split(',')
        .filter_map(|p| {
            let (a, b) = p.trim().split_once('x')?;
            let (a, b) = (a.parse::<u32>().ok()?, b.parse::<u32>().ok()?);
            (a >= 2048 && b >= 2 && a.checked_mul(b).is_some()).then_some((a, b))
        })
        .collect()
}

fn open_compress(dev: &str, fmt: &OffloadFormat, size: u32, count: u32) -> Result<CompressStream> {
    use std::os::fd::FromRawFd;
    let path = CString::new(dev)?;
    // SAFETY: valid path; the fd is owned below. O_WRONLY = playback direction.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(anyhow!("open {dev}: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: `fd` was just opened and is owned only by this OwnedFd.
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    let s = CompressStream { fd: owned };
    let mut ver: c_int = 0;
    s.ioctl_ptr(SNDRV_COMPRESS_IOCTL_VERSION, &mut ver, "version")?;
    let mut p = SndComprParams {
        fragment_size: size,
        fragments: count,
        codec: codec_for(fmt),
        ..Default::default()
    };
    s.ioctl_ptr(SNDRV_COMPRESS_SET_PARAMS, &mut p, "set_params")?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cards() {
        let t = " 0 [lahainayupikidp]: lahaina-yupikid - lahaina-yupikidp-snd-card\n                      lahaina-yupikidp-snd-card\n 1 [BTR5           ]: USB-Audio - FiiO BTR5\n                      FiiO FiiO BTR5 at usb-xhci-hcd.0.auto-1, high speed\n";
        assert_eq!(parse_cards(t), vec![(1, "FiiO BTR5".to_string())]);
    }

    #[test]
    fn compress_layouts() {
        assert_eq!(std::mem::size_of::<SndCodec>(), 120);
        assert_eq!(std::mem::size_of::<SndComprParams>(), 132);
        assert_eq!(std::mem::size_of::<SndComprTstamp>(), 20);
        // _IOC size field must match the struct size.
        assert_eq!((SNDRV_COMPRESS_SET_PARAMS >> 16) & 0x3FFF, 132);
        assert_eq!((SNDRV_COMPRESS_TSTAMP >> 16) & 0x3FFF, 20);
    }

    #[test]
    fn streaminfo_and_flac_options() {
        // 4096/4096 blocks, frames 14..12345, 44100 Hz, 2 ch, 16 bit, 1000 samples.
        let mut b = vec![0x10, 0x00, 0x10, 0x00, 0, 0, 14, 0x00, 0x30, 0x39];
        let packed: u64 = (44100u64 << 44) | (1u64 << 41) | (15u64 << 36) | 1000;
        b.extend(packed.to_be_bytes());
        b.extend([0u8; 16]);
        let f = FlacInfo::from_streaminfo(&b).unwrap();
        assert_eq!((f.min_blk, f.max_blk, f.min_frame, f.max_frame), (4096, 4096, 14, 12345));
        assert_eq!((f.rate, f.channels, f.bits), (44100, 2, 16));
        let fmt = OffloadFormat { codec: OffloadCodec::Flac, rate: 44100, channels: 2, bits: 16, flac: f };
        let c = codec_for(&fmt);
        assert_eq!(c.id, SND_AUDIOCODEC_BESPOKE);
        assert_eq!(c.options[1], AUDIO_COMP_FORMAT_FLAC);
        assert_eq!(c.options[2], 16 | 4096 << 16);
        assert_eq!(c.options[3], 4096 | 14 << 16);
        assert_eq!(c.options[4], 12345);
        assert_eq!(parse_plans("1048576x2, 262144x4,bad,1x9"), vec![(1 << 20, 2), (262144, 4)]);
    }
}
