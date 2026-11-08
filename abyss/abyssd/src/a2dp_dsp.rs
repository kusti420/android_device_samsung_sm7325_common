//! A2DP hardware offload, abyssd's half: the ADSP encodes Bluetooth audio.
//!
//! With offload, abyss-btd's stack only signals: it negotiates the codec with the headset,
//! starts the AVDTP stream and sends the controller the vendor "A2DP offload start" HCI
//! command (L2CAP channel, MTU, codec). The ADSP encodes on its SLIMBUS_7_RX AFE port; the
//! BT-FM slimbus link carries the encoded frames to the WCN6750, which sends them. abyssd
//! feeds the DSP like the jack (compress offload, DSP decode; or PCM for CPU-decoded
//! streams), so the AP can sleep between fragments.
//!
//! The vendor audio HAL would program the encoder from the codec config its Bluetooth IPC
//! library hands it (hardware/qcom-caf/sm8350/audio/hal/audio_extn/a2dp.c:
//! configure_a2dp_encoder_format, a2dp_set_backend_cfg, start_abr). On this phone that
//! library (btaudio_offload_if.so) does not exist and Android itself plays Bluetooth in
//! software, so abyssd does what a2dp.c does, from abyss-btd's `format` event:
//!
//! 1. "SLIM_7_RX Encoder Config" (bytes): u32 media format + the codec block. Layouts from
//!    techpack/audio/include/dsp/apr_audio-v2.h (asm_sbc_enc_cfg_t, asm_aac_enc_cfg_t,
//!    asm_custom_enc_cfg_t + asm_ldac_specific_enc_cfg_t + afe_abr_enc_cfg_t);
//!    msm-dai-q6-v2.c msm_dai_q6_afe_enc_cfg_put copies the block after the format.
//! 2. "AFE Input Bit Format" (S16_LE / S24_LE / S32_LE).
//! 3. "BT SampleRate RX" (falls back to "BT SampleRate"): the slimbus rate; LDAC runs the
//!    port at twice 44.1/48 kHz.
//! 4. "AFE Input Channels" (One / Two).
//! 5. LDAC adaptive bitrate only: "SLIM_7_TX Decoder Config" (feedback decoder),
//!    "BT SampleRate TX" KHZ_8, "A2DP_SLIM7_UL_HL Switch" 1, "BT set feedback channel" 1 and
//!    the hostless SLIMBUS_7 capture PCM (USECASE_AUDIO_A2DP_ABR_FEEDBACK, hw:0,36).
//!
//! Then the caller routes "bt-a2dp" + "<front end> bt-a2dp" (mixer_paths.xml:
//! SLIMBUS_7_RX Audio Mixer MultiMedia4 / MultiMedia1) and opens the stream. Stop is the
//! reverse (a2dp.c reset_codec_config). A wrong block makes the DSP refuse the port start
//! (the stream open/start fails): no audio, never damage; the caller then falls back to
//! software encoding.

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_uint, c_void};

use anyhow::{anyhow, Result};
use log::{info, warn};

use crate::control;

/// What abyss-btd's `format` event says when `offload` is true.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodecCfg {
    /// "SBC", "AAC" or "LDAC" (A2dpCodecConfig::CodecNameStr).
    pub codec: String,
    pub rate: u32,
    pub bits: u32,
    pub channels: u32,
    /// The stack's track bit rate (bit/s).
    pub bitrate: u32,
    pub mtu: u32,
    /// codec_specific_1 (LDAC: 1000 + quality index).
    pub specific1: i64,
    /// Over-the-air codec info, length-prefixed (byte 0 = length of the rest).
    pub ota: Vec<u8>,
}

impl CodecCfg {
    /// From the event's fields (hex `ota`).
    pub fn from_event(v: &serde_json::Value) -> Option<CodecCfg> {
        let num = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
        let ota_hex = v.get("ota").and_then(|x| x.as_str()).unwrap_or("");
        let ota = (0..ota_hex.len() / 2)
            .map(|i| u8::from_str_radix(&ota_hex[2 * i..2 * i + 2], 16))
            .collect::<Result<Vec<u8>, _>>()
            .ok()?;
        Some(CodecCfg {
            codec: v.get("codec")?.as_str()?.to_string(),
            rate: num("rate") as u32,
            bits: num("bits") as u32,
            channels: num("ch") as u32,
            bitrate: num("bitrate") as u32,
            mtu: num("mtu") as u32,
            specific1: num("specific1") as i64,
            ota,
        })
    }

    /// e.g. "LDAC 909 kbps 44.1 kHz".
    pub fn label(&self) -> String {
        format!("{} {} kbps {}", self.codec, self.bitrate / 1000, khz(self.rate))
    }
}

fn khz(rate: u32) -> String {
    if rate.is_multiple_of(1000) {
        format!("{} kHz", rate / 1000)
    } else {
        format!("{:.1} kHz", rate as f64 / 1000.0)
    }
}

// apr_audio-v2.h / a2dp.c constants.
const FMT_SBC: u32 = 0x0001_0BF2;
const FMT_AAC_V2: u32 = 0x0001_0DA6;
const FMT_LDAC: u32 = 0x0001_3224;
const SBC_MODE_MONO: u32 = 1;
const SBC_MODE_STEREO: u32 = 2;
const SBC_MODE_DUAL: u32 = 8;
const SBC_MODE_JOINT: u32 = 9;
const SBC_ALLOC_LOUDNESS: u32 = 0;
const SBC_ALLOC_SNR: u32 = 1;
const AAC_AOT_LC: u32 = 2;
/// ASM_MEDIA_FMT_AAC_FORMAT_FLAG_LATM: A2DP carries AAC as MPEG-4 LATM (A2DP spec 4.5.4).
/// Overridable (persist.abyss.bt.aac_fmt_flag) because the vendor IPC library that would
/// say it is missing on this phone.
const AAC_FMT_LATM: u32 = 4;
/// asm_aac_frame_size_control_t.ctl_type MTU_SIZE: the DSP keeps every AAC frame within
/// one AVDTP packet (offloaded media packets cannot be fragmented).
const AAC_FRAME_CTL_MTU: u32 = 0;
const PCM_CHANNEL_L: u8 = 1;
const PCM_CHANNEL_R: u8 = 2;
const PCM_CHANNEL_C: u8 = 3;
/// a2dp.c IMC_PURPOSE_ID_BT_INFO, CODEC_TYPE_LDAC (the ABR feedback decoder's format).
const IMC_PURPOSE_BT_INFO: u32 = 0x0001_32E2;
const CODEC_TYPE_LDAC: u32 = 0x2300_0000;
const IMC_TRANSMIT: u32 = 0;
const IMC_RECEIVE: u32 = 1;
const MAX_ABR_LEVELS: usize = 5;
/// sizeof(struct ldac_enc_cfg_t) in a2dp.c, format word included: custom 24 + ldac 8 + abr 61.
const LDAC_BLOCK: usize = 93;

/// The encoder block and the backend settings for one codec config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// "SLIM_7_RX Encoder Config" bytes (u32 format first).
    pub enc: Vec<u8>,
    pub bit_format: &'static str,
    pub backend_rate: &'static str,
    pub channels: &'static str,
    /// LDAC ABR: the feedback decoder block ("SLIM_7_TX Decoder Config").
    pub abr_dec: Option<Vec<u8>>,
}

fn put32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn bit_format(bits: u32) -> &'static str {
    match bits {
        32 => "S32_LE",
        24 => "S24_LE",
        _ => "S16_LE",
    }
}

fn rate_str(rate: u32) -> Result<&'static str> {
    Ok(match rate {
        44100 => "KHZ_44P1",
        48000 => "KHZ_48",
        88200 => "KHZ_88P2",
        96000 => "KHZ_96",
        r => return Err(anyhow!("no slimbus rate for {r} Hz")),
    })
}

/// Builds the encoder plan. `abr_map`: LDAC adaptive (level, bit/s) table, or None.
pub fn plan(c: &CodecCfg, aac_fmt_flag: u32, abr_map: Option<&[(u32, u32)]>) -> Result<Plan> {
    match c.codec.as_str() {
        "SBC" => plan_sbc(c),
        "AAC" => plan_aac(c, aac_fmt_flag),
        "LDAC" => plan_ldac(c, abr_map),
        other => Err(anyhow!("codec {other} is not offloaded here")),
    }
}

/// SBC OTA: [3] freq|mode, [4] block length|subbands|allocation, [5..6] bitpool.
fn plan_sbc(c: &CodecCfg) -> Result<Plan> {
    if c.ota.len() < 7 {
        return Err(anyhow!("SBC codec info too short ({} bytes)", c.ota.len()));
    }
    let mode = match c.ota[3] & 0x0F {
        0x08 => SBC_MODE_MONO,
        0x04 => SBC_MODE_DUAL,
        0x02 => SBC_MODE_STEREO,
        0x01 => SBC_MODE_JOINT,
        m => return Err(anyhow!("SBC channel mode 0x{m:x}")),
    };
    let blk = match c.ota[4] & 0xF0 {
        0x80 => 4,
        0x40 => 8,
        0x20 => 12,
        0x10 => 16,
        b => return Err(anyhow!("SBC block length 0x{b:x}")),
    };
    let subbands = match c.ota[4] & 0x0C {
        0x08 => 4,
        0x04 => 8,
        s => return Err(anyhow!("SBC subbands 0x{s:x}")),
    };
    let alloc = match c.ota[4] & 0x03 {
        0x02 => SBC_ALLOC_SNR,
        _ => SBC_ALLOC_LOUDNESS,
    };
    let mut e = Vec::with_capacity(28);
    put32(&mut e, FMT_SBC);
    put32(&mut e, subbands);
    put32(&mut e, blk);
    put32(&mut e, mode);
    put32(&mut e, alloc);
    put32(&mut e, c.bitrate);
    put32(&mut e, c.rate);
    Ok(Plan {
        enc: e,
        bit_format: bit_format(c.bits),
        backend_rate: rate_str(c.rate)?,
        channels: if mode == SBC_MODE_MONO { "One" } else { "Two" },
        abr_dec: None,
    })
}

/// AAC: asm_aac_enc_cfg_v2_t + frame_ctl (MTU) + frame_ctl_v2 (unused).
fn plan_aac(c: &CodecCfg, fmt_flag: u32) -> Result<Plan> {
    if c.channels != 1 && c.channels != 2 {
        return Err(anyhow!("AAC with {} channels", c.channels));
    }
    let mut e = Vec::with_capacity(36);
    put32(&mut e, FMT_AAC_V2);
    put32(&mut e, c.bitrate);
    put32(&mut e, AAC_AOT_LC);
    put16(&mut e, fmt_flag as u16);
    put16(&mut e, c.channels as u16);
    put32(&mut e, c.rate);
    put32(&mut e, AAC_FRAME_CTL_MTU);
    put32(&mut e, c.mtu);
    put32(&mut e, 0);
    put32(&mut e, 0);
    Ok(Plan {
        enc: e,
        bit_format: bit_format(c.bits),
        backend_rate: rate_str(c.rate)?,
        channels: if c.channels == 1 { "One" } else { "Two" },
        abr_dec: None,
    })
}

/// LDAC OTA: [3..6] vendor id, [7..8] codec id, [9] sample rate, [10] channel mode
/// (0x04 mono, 0x02 dual, 0x01 stereo; passed to the DSP as is, like a2dp.c).
fn plan_ldac(c: &CodecCfg, abr_map: Option<&[(u32, u32)]>) -> Result<Plan> {
    if c.ota.len() < 11 {
        return Err(anyhow!("LDAC codec info too short ({} bytes)", c.ota.len()));
    }
    let ch_mode = c.ota[10] & 0x07;
    let mono = ch_mode == 0x04;
    let mut e = Vec::with_capacity(LDAC_BLOCK);
    put32(&mut e, FMT_LDAC);
    // asm_custom_enc_cfg_t
    put32(&mut e, c.rate);
    put16(&mut e, if mono { 1 } else { 2 });
    put16(&mut e, 0);
    let mut map = [0u8; 8];
    if mono {
        map[0] = PCM_CHANNEL_C;
    } else {
        map[0] = PCM_CHANNEL_L;
        map[1] = PCM_CHANNEL_R;
    }
    e.extend_from_slice(&map);
    put32(&mut e, LDAC_BLOCK as u32);
    // asm_ldac_specific_enc_cfg_t
    put32(&mut e, c.bitrate);
    put16(&mut e, ch_mode as u16);
    put16(&mut e, c.mtu as u16);
    // afe_abr_enc_cfg_t: mapping_info, imc_info, is_abr_enabled (packed bool).
    let abr = abr_map.filter(|m| !m.is_empty());
    let levels = abr.map(|m| &m[..m.len().min(MAX_ABR_LEVELS)]).unwrap_or(&[]);
    put32(&mut e, levels.len() as u32);
    for i in 0..MAX_ABR_LEVELS {
        let (l, b) = levels.get(i).copied().unwrap_or((0, 0));
        put32(&mut e, l);
        put32(&mut e, b);
    }
    if abr.is_some() {
        put32(&mut e, IMC_RECEIVE);
        put32(&mut e, 1);
        put32(&mut e, IMC_PURPOSE_BT_INFO);
        put32(&mut e, ABR_INSTANCE);
        e.push(1);
    } else {
        e.extend_from_slice(&[0u8; 17]);
    }
    let abr_dec = abr.map(|_| {
        // abr_dec_cfg_t: dec_format + imc_info (a2dp.c configure_a2dp_source_decoder_format).
        let mut d = Vec::with_capacity(20);
        put32(&mut d, CODEC_TYPE_LDAC);
        put32(&mut d, IMC_TRANSMIT);
        put32(&mut d, 1);
        put32(&mut d, IMC_PURPOSE_BT_INFO);
        put32(&mut d, ABR_INSTANCE);
        d
    });
    // LDAC runs the slimbus port at 88.2/96 kHz for 44.1/48 kHz (a2dp_set_backend_cfg).
    let port_rate = if c.rate == 44100 || c.rate == 48000 { c.rate * 2 } else { c.rate };
    Ok(Plan {
        enc: e,
        bit_format: bit_format(c.bits),
        backend_rate: rate_str(port_rate)?,
        channels: if mono { "One" } else { "Two" },
        abr_dec,
    })
}

/// IMC instance shared by the ABR encoder and feedback decoder (a2dp.c counts down from
/// UINT32_MAX / 2; any value both sides agree on works).
const ABR_INSTANCE: u32 = u32::MAX / 2;

/// persist.abyss.bt.ldac_abr_map = "level:bitrate,...", e.g. "1:330000,2:660000,3:990000".
pub fn parse_abr_map(s: &str) -> Vec<(u32, u32)> {
    s.split(',')
        .filter_map(|p| {
            let (l, b) = p.trim().split_once(':')?;
            Some((l.trim().parse().ok()?, b.trim().parse().ok()?))
        })
        .take(MAX_ABR_LEVELS)
        .collect()
}

// ---------------------------------------------------------------------------
// tinyalsa (external/tinyalsa/include/tinyalsa/asoundlib.h)

#[repr(C)]
struct Mixer {
    _p: [u8; 0],
}
#[repr(C)]
struct MixerCtl {
    _p: [u8; 0],
}
#[repr(C)]
struct Pcm {
    _p: [u8; 0],
}
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
const PCM_IN: c_uint = 0x1000_0000;

extern "C" {
    fn mixer_open(card: c_uint) -> *mut Mixer;
    fn mixer_close(m: *mut Mixer);
    fn mixer_get_ctl_by_name(m: *mut Mixer, name: *const c_char) -> *mut MixerCtl;
    fn mixer_ctl_set_array(ctl: *mut MixerCtl, array: *const c_void, count: usize) -> c_int;
    fn mixer_ctl_get_array(ctl: *mut MixerCtl, array: *mut c_void, count: usize) -> c_int;
    fn mixer_ctl_get_num_values(ctl: *mut MixerCtl) -> c_uint;
    fn mixer_ctl_set_enum_by_string(ctl: *mut MixerCtl, s: *const c_char) -> c_int;
    fn mixer_ctl_set_value(ctl: *mut MixerCtl, id: c_uint, value: c_int) -> c_int;
    fn pcm_open(card: c_uint, device: c_uint, flags: c_uint, config: *mut PcmConfig) -> *mut Pcm;
    fn pcm_is_ready(pcm: *mut Pcm) -> c_int;
    fn pcm_start(pcm: *mut Pcm) -> c_int;
    fn pcm_close(pcm: *mut Pcm) -> c_int;
}

/// The DSP's Bluetooth encoder (one per CompressOut / PCM backend that targets Bluetooth).
pub struct BtEncoder {
    card: u32,
    mixer: *mut Mixer,
    abr_pcm: *mut Pcm,
    programmed: bool,
    abr: bool,
}

// SAFETY: the raw handles are owned by this struct and only used from the thread that
// owns it (the player thread); they are never shared.
unsafe impl Send for BtEncoder {}

impl Drop for BtEncoder {
    fn drop(&mut self) {
        self.reset();
        if !self.mixer.is_null() {
            // SAFETY: from mixer_open, closed once.
            unsafe { mixer_close(self.mixer) };
        }
    }
}

impl BtEncoder {
    pub fn new(card: u32) -> BtEncoder {
        BtEncoder { card, mixer: std::ptr::null_mut(), abr_pcm: std::ptr::null_mut(), programmed: false, abr: false }
    }

    fn ctl(&mut self, name: &str) -> Result<*mut MixerCtl> {
        if self.mixer.is_null() {
            // SAFETY: plain open; NULL checked.
            self.mixer = unsafe { mixer_open(self.card) };
            if self.mixer.is_null() {
                return Err(anyhow!("mixer_open({}) failed", self.card));
            }
        }
        let c = CString::new(name)?;
        // SAFETY: live mixer, valid name.
        let ctl = unsafe { mixer_get_ctl_by_name(self.mixer, c.as_ptr()) };
        if ctl.is_null() {
            return Err(anyhow!("no mixer control '{name}'"));
        }
        Ok(ctl)
    }

    fn set_enum(&mut self, name: &str, v: &str) -> Result<()> {
        let ctl = self.ctl(name)?;
        let s = CString::new(v)?;
        // SAFETY: live control, valid string.
        if unsafe { mixer_ctl_set_enum_by_string(ctl, s.as_ptr()) } != 0 {
            return Err(anyhow!("'{name}' = {v} refused"));
        }
        Ok(())
    }

    fn set_int(&mut self, name: &str, v: i32) -> Result<()> {
        let ctl = self.ctl(name)?;
        // SAFETY: live control; value 0 exists for every control.
        if unsafe { mixer_ctl_set_value(ctl, 0, v) } != 0 {
            return Err(anyhow!("'{name}' = {v} refused"));
        }
        Ok(())
    }

    fn set_bytes(&mut self, name: &str, b: &[u8]) -> Result<()> {
        let ctl = self.ctl(name)?;
        // SAFETY: live control; `b` is valid for b.len() bytes.
        let n = unsafe { mixer_ctl_get_num_values(ctl) } as usize;
        if b.len() > n {
            return Err(anyhow!("'{name}' takes {n} bytes, block is {}", b.len()));
        }
        // SAFETY: as above.
        if unsafe { mixer_ctl_set_array(ctl, b.as_ptr() as *const c_void, b.len()) } != 0 {
            return Err(anyhow!("'{name}' ({} bytes) refused", b.len()));
        }
        Ok(())
    }

    /// Reads a bytes control back (for the log: what the kernel kept).
    fn get_bytes(&mut self, name: &str, len: usize) -> Option<Vec<u8>> {
        let ctl = self.ctl(name).ok()?;
        let mut v = vec![0u8; len];
        // SAFETY: live control, buffer of `len` bytes.
        let r = unsafe { mixer_ctl_get_array(ctl, v.as_mut_ptr() as *mut c_void, len) };
        (r == 0).then_some(v)
    }

    /// Programs the encoder for `c`. On error everything set so far is reset.
    pub fn program(&mut self, c: &CodecCfg) -> Result<()> {
        if self.programmed {
            self.reset();
        }
        let fmt_flag = control::prop_str("persist.abyss.bt.aac_fmt_flag")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(AAC_FMT_LATM);
        let abr_map = if c.codec == "LDAC" && (c.specific1 == 0 || c.specific1 % 10 == 3) {
            let m = parse_abr_map(&control::prop_str("persist.abyss.bt.ldac_abr_map").unwrap_or_default());
            if m.is_empty() {
                warn!("a2dp-dsp: LDAC adaptive asked for but persist.abyss.bt.ldac_abr_map is empty; fixed rate");
                None
            } else {
                Some(m)
            }
        } else {
            None
        };
        let p = plan(c, fmt_flag, abr_map.as_deref())?;
        self.programmed = true;
        if let Err(e) = self.apply(&p) {
            self.reset();
            return Err(e);
        }
        let back = self.get_bytes("SLIM_7_RX Encoder Config", p.enc.len());
        info!(
            "a2dp-dsp: {} encoder set ({} bytes{}), {} {} {}{}",
            c.label(),
            p.enc.len(),
            match &back {
                Some(b) if b == &p.enc => ", read back equal".to_string(),
                Some(b) => format!(", read back DIFFERS: {}", hex(b)),
                None => String::new(),
            },
            p.backend_rate,
            p.bit_format,
            p.channels,
            if self.abr { ", adaptive" } else { "" }
        );
        Ok(())
    }

    fn apply(&mut self, p: &Plan) -> Result<()> {
        self.set_bytes("SLIM_7_RX Encoder Config", &p.enc)?;
        self.set_enum("AFE Input Bit Format", p.bit_format)?;
        if self.set_enum("BT SampleRate RX", p.backend_rate).is_err() {
            self.set_enum("BT SampleRate", p.backend_rate)?;
        }
        if let Some(d) = &p.abr_dec {
            self.set_bytes("SLIM_7_TX Decoder Config", d)?;
            self.set_enum("BT SampleRate TX", "KHZ_8")?;
            self.start_abr()?;
        }
        self.set_enum("AFE Input Channels", p.channels)?;
        Ok(())
    }

    /// a2dp.c start_abr: feedback path from the BT SoC + a hostless capture PCM.
    fn start_abr(&mut self) -> Result<()> {
        self.set_int("A2DP_SLIM7_UL_HL Switch", 1)?;
        self.set_int("BT set feedback channel", 1)?;
        let dev = control::prop_str("persist.abyss.bt.abr_pcm").and_then(|s| s.parse().ok()).unwrap_or(36);
        let mut cfg = PcmConfig {
            channels: 1,
            rate: 8000,
            period_size: 240,
            period_count: 2,
            format: 0,
            start_threshold: 0,
            stop_threshold: i32::MAX as c_uint,
            silence_threshold: 0,
            silence_size: 0,
            avail_min: 0,
        };
        // SAFETY: valid config for the call; the handle is closed in reset().
        let pcm = unsafe { pcm_open(self.card, dev, PCM_IN, &mut cfg) };
        self.abr_pcm = pcm;
        self.abr = true;
        // SAFETY: pcm is the handle from pcm_open (never NULL with tinyalsa).
        if pcm.is_null() || unsafe { pcm_is_ready(pcm) } == 0 || unsafe { pcm_start(pcm) } != 0 {
            return Err(anyhow!("ABR feedback PCM hw:{},{dev} did not start", self.card));
        }
        Ok(())
    }

    /// a2dp.c reset_codec_config. Errors are logged only (best effort on the way down).
    pub fn reset(&mut self) {
        if !self.programmed {
            return;
        }
        self.programmed = false;
        let mut errs = Vec::new();
        // A zero SBC-sized block clears the kernel's copy (msm_dai_q6_afe_enc_cfg_put memsets
        // first, then rejects format 0: the refusal is expected).
        let _ = self.set_bytes("SLIM_7_RX Encoder Config", &[0u8; 28]);
        if let Err(e) = self.set_enum("AFE Input Bit Format", "S16_LE") {
            errs.push(e);
        }
        if self.set_enum("BT SampleRate RX", "KHZ_8").is_err() {
            if let Err(e) = self.set_enum("BT SampleRate", "KHZ_8") {
                errs.push(e);
            }
        }
        if self.abr {
            if !self.abr_pcm.is_null() {
                // SAFETY: from pcm_open, closed once.
                unsafe { pcm_close(self.abr_pcm) };
                self.abr_pcm = std::ptr::null_mut();
            }
            let _ = self.set_bytes("SLIM_7_TX Decoder Config", &[0u8; 20]);
            let _ = self.set_enum("BT SampleRate TX", "KHZ_8");
            let _ = self.set_int("BT set feedback channel", 0);
            let _ = self.set_int("A2DP_SLIM7_UL_HL Switch", 0);
            self.abr = false;
        }
        if let Err(e) = self.set_enum("AFE Input Channels", "Zero") {
            errs.push(e);
        }
        for e in errs {
            warn!("a2dp-dsp: reset: {e:#}");
        }
    }

    /// The mixer was reset (vendor audio HAL restart): forget the handle.
    pub fn hal_restarted(&mut self) {
        if !self.abr_pcm.is_null() {
            // SAFETY: from pcm_open, closed once.
            unsafe { pcm_close(self.abr_pcm) };
            self.abr_pcm = std::ptr::null_mut();
        }
        if !self.mixer.is_null() {
            // SAFETY: from mixer_open, closed once.
            unsafe { mixer_close(self.mixer) };
            self.mixer = std::ptr::null_mut();
        }
        self.programmed = false;
        self.abr = false;
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le32(b: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
    }

    #[test]
    fn sbc_block() {
        // 44.1 kHz joint stereo, 16 blocks, 8 subbands, loudness, bitpool 2..53.
        let c = CodecCfg {
            codec: "SBC".into(),
            rate: 44100,
            bits: 16,
            channels: 2,
            bitrate: 328000,
            mtu: 895,
            ota: vec![6, 0, 0, 0x21, 0x15, 2, 53],
            ..Default::default()
        };
        let p = plan(&c, AAC_FMT_LATM, None).unwrap();
        // format + asm_sbc_enc_cfg_t (6 x u32)
        assert_eq!(p.enc.len(), 28);
        assert_eq!(le32(&p.enc, 0), FMT_SBC);
        assert_eq!(le32(&p.enc, 4), 8);
        assert_eq!(le32(&p.enc, 8), 16);
        assert_eq!(le32(&p.enc, 12), SBC_MODE_JOINT);
        assert_eq!(le32(&p.enc, 16), SBC_ALLOC_LOUDNESS);
        assert_eq!(le32(&p.enc, 20), 328000);
        assert_eq!(le32(&p.enc, 24), 44100);
        assert_eq!((p.backend_rate, p.bit_format, p.channels), ("KHZ_44P1", "S16_LE", "Two"));
    }

    #[test]
    fn aac_block() {
        let c = CodecCfg {
            codec: "AAC".into(),
            rate: 44100,
            bits: 16,
            channels: 2,
            bitrate: 320000,
            mtu: 1005,
            ota: vec![8, 0, 2, 0x80, 0x01, 0x04, 0x84, 0xe2, 0x00],
            ..Default::default()
        };
        let p = plan(&c, AAC_FMT_LATM, None).unwrap();
        // format + asm_aac_enc_cfg_v2_t (16) + 2 x frame size control (8 each)
        assert_eq!(p.enc.len(), 36);
        assert_eq!(le32(&p.enc, 0), FMT_AAC_V2);
        assert_eq!(le32(&p.enc, 4), 320000);
        assert_eq!(le32(&p.enc, 8), AAC_AOT_LC);
        assert_eq!(u16::from_le_bytes([p.enc[12], p.enc[13]]), 4);
        assert_eq!(u16::from_le_bytes([p.enc[14], p.enc[15]]), 2);
        assert_eq!(le32(&p.enc, 16), 44100);
        assert_eq!((le32(&p.enc, 20), le32(&p.enc, 24)), (AAC_FRAME_CTL_MTU, 1005));
    }

    #[test]
    fn ldac_block() {
        // Sony LDAC, 44.1 kHz stereo, HQ.
        let c = CodecCfg {
            codec: "LDAC".into(),
            rate: 44100,
            bits: 32,
            channels: 2,
            bitrate: 909000,
            mtu: 679,
            specific1: 1000,
            ota: vec![10, 0, 0xff, 0x2d, 0x01, 0, 0, 0xaa, 0x00, 0x20, 0x01],
        };
        let p = plan(&c, AAC_FMT_LATM, None).unwrap();
        assert_eq!(p.enc.len(), LDAC_BLOCK);
        assert_eq!(le32(&p.enc, 0), FMT_LDAC);
        assert_eq!(le32(&p.enc, 4), 44100);
        assert_eq!(u16::from_le_bytes([p.enc[8], p.enc[9]]), 2);
        assert_eq!(&p.enc[12..14], &[PCM_CHANNEL_L, PCM_CHANNEL_R]);
        assert_eq!(le32(&p.enc, 20), LDAC_BLOCK as u32);
        assert_eq!(le32(&p.enc, 24), 909000);
        assert_eq!(u16::from_le_bytes([p.enc[28], p.enc[29]]), 1);
        assert_eq!(u16::from_le_bytes([p.enc[30], p.enc[31]]), 679);
        assert_eq!(*p.enc.last().unwrap(), 0); // ABR off
        assert_eq!((p.backend_rate, p.bit_format), ("KHZ_88P2", "S32_LE"));
        assert!(p.abr_dec.is_none());

        let map = parse_abr_map("1:330000, 2:660000,3:990000,bad");
        let p = plan(&c, AAC_FMT_LATM, Some(&map)).unwrap();
        assert_eq!(p.enc.len(), LDAC_BLOCK);
        assert_eq!(le32(&p.enc, 32), 3);
        assert_eq!((le32(&p.enc, 36), le32(&p.enc, 40)), (1, 330000));
        assert_eq!(le32(&p.enc, 76), IMC_RECEIVE);
        assert_eq!(*p.enc.last().unwrap(), 1);
        assert_eq!(p.abr_dec.as_ref().map(|d| d.len()), Some(20));
    }

    #[test]
    fn event_roundtrip() {
        let v = serde_json::json!({"ev":"format","codec":"LDAC","rate":44100,"bits":32,"ch":2,
            "bitrate":909000,"offload":true,"mtu":679,"specific1":1000,"ota":"0a00ff2d010000aa002001"});
        let c = CodecCfg::from_event(&v).unwrap();
        assert_eq!(c.ota.len(), 11);
        assert_eq!(c.mtu, 679);
        assert_eq!(c.label(), "LDAC 909 kbps 44.1 kHz");
    }
}
