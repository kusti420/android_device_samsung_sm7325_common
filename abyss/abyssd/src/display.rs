//! KMS output on /dev/dri/card0 with one XRGB8888 dumb buffer.
//!
//! The display composer HAL is stopped before abyssd starts, so abyssd can
//! become DRM master. Drawing happens into a RAM `Canvas`; `present()` copies
//! it into the mapped dumb buffer and pushes one frame. The panel is a
//! command-mode panel (no continuous scan-out from RAM), so a frame costs
//! power only when we actually push one: the UI only presents on change.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, BorrowedFd};

use anyhow::{anyhow, Context, Result};
use drm::buffer::DrmFourcc;
use drm::control::{
    connector, crtc, dumbbuffer::DumbBuffer, framebuffer, property, Device as ControlDevice, Mode,
    ModeTypeFlags, PageFlipFlags,
};
use drm::Device as DrmDevice;
use log::{info, warn};

/// DRM_MODE_DPMS_ON / DRM_MODE_DPMS_OFF from drm_mode.h.
const DPMS_ON: u64 = 0;
const DPMS_OFF: u64 = 3;

/// Thin wrapper so the drm crate's traits can be implemented for the card fd.
struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl DrmDevice for Card {}
impl ControlDevice for Card {}

/// Simple 32-bit XRGB pixel canvas in RAM.
pub struct Canvas {
    pub w: u32,
    pub h: u32,
    px: Vec<u32>,
}

impl Canvas {
    pub fn new(w: u32, h: u32) -> Self {
        Canvas { w, h, px: vec![0; (w * h) as usize] }
    }

    pub fn clear(&mut self, color: u32) {
        self.px.fill(color);
    }

    #[inline]
    pub fn put(&mut self, x: i32, y: i32, color: u32) {
        if x >= 0 && y >= 0 && (x as u32) < self.w && (y as u32) < self.h {
            self.px[(y as u32 * self.w + x as u32) as usize] = color;
        }
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.w as i32);
        let y1 = (y + h).min(self.h as i32);
        for yy in y0..y1 {
            let row = (yy as u32 * self.w) as usize;
            self.px[row + x0 as usize..row + x1.max(x0) as usize].fill(color);
        }
    }

    /// Rectangle outline with line thickness `t` (the design uses outlines, not fills).
    pub fn outline(&mut self, x: i32, y: i32, w: i32, h: i32, t: i32, color: u32) {
        self.fill_rect(x, y, w, t, color);
        self.fill_rect(x, y + h - t, w, t, color);
        self.fill_rect(x, y, t, h, color);
        self.fill_rect(x + w - t, y, t, h, color);
    }
}

pub struct Display {
    card: Card,
    conn: connector::Handle,
    crtc: crtc::Handle,
    mode: Mode,
    fb: framebuffer::Handle,
    db: DumbBuffer,
    dpms: Option<property::Handle>,
    /// True once the CRTC has been set with our framebuffer since the last unblank.
    modeset_done: bool,
    on: bool,
    pub canvas: Canvas,
}

impl Display {
    pub fn open(path: &str) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("open {path}"))?;
        let card = Card(file);
        // Without CAP_SYS_ADMIN (abyss.rc does not grant it) SET_MASTER is
        // refused on this 5.4 kernel even when we already are master: the
        // first opener after the composer exits becomes master on its own.
        // main.rs retries the whole open until a modeset succeeds.
        if let Err(e) = card.acquire_master_lock() {
            log::debug!("DRM SET_MASTER: {e} (expected without CAP_SYS_ADMIN)");
        }

        let res = card.resource_handles().context("DRM resources")?;
        // The phone's own panel is DSI. msm DRM also reports a DisplayPort
        // connector (USB-C video out) as "connected" with a 4096x2160 mode even
        // with nothing plugged in, so never just take the first connected one:
        // rank DSI first, then eDP/LVDS, and DisplayPort/HDMI only as a last resort.
        fn rank(i: connector::Interface) -> u8 {
            match i {
                connector::Interface::DSI => 0,
                connector::Interface::EmbeddedDisplayPort | connector::Interface::LVDS => 1,
                connector::Interface::DisplayPort | connector::Interface::HDMIA => 3,
                _ => 2,
            }
        }
        let mut chosen: Option<(connector::Info, Mode)> = None;
        for &h in res.connectors() {
            let info = match card.get_connector(h, false) {
                Ok(i) => i,
                Err(e) => {
                    warn!("connector {h:?}: {e}");
                    continue;
                }
            };
            log::info!(
                "display: candidate {:?} {:?} {:?}, {} modes",
                h,
                info.interface(),
                info.state(),
                info.modes().len()
            );
            if info.state() != connector::State::Connected || info.modes().is_empty() {
                continue;
            }
            // The panel has a 120 Hz and a 60 Hz mode at 1080x2400: take the
            // preferred mode's size and, of the modes with that size, the one
            // nearest 60 Hz (half the panel power of 120 Hz, and the UI
            // never animates).
            let pref = info
                .modes()
                .iter()
                .copied()
                .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
                .unwrap_or(info.modes()[0]);
            for m in info.modes() {
                let (mw, mh) = m.size();
                info!("display:   mode {mw}x{mh}@{}", m.vrefresh());
            }
            let mode = info
                .modes()
                .iter()
                .copied()
                .filter(|m| m.size() == pref.size())
                .min_by_key(|m| (m.vrefresh() as i64 - 60).abs())
                .unwrap_or(pref);
            let better = match &chosen {
                None => true,
                Some((c, _)) => rank(info.interface()) < rank(c.interface()),
            };
            if better {
                chosen = Some((info, mode));
            }
        }
        let (cinfo, mode) = chosen.ok_or_else(|| anyhow!("no connected DRM connector"))?;

        // CRTC: the one the current encoder drives, else the first possible one.
        let enc_handle = cinfo
            .current_encoder()
            .or_else(|| cinfo.encoders().first().copied())
            .ok_or_else(|| anyhow!("connector has no encoder"))?;
        let enc = card.get_encoder(enc_handle).context("get encoder")?;
        let crtc = match enc.crtc() {
            Some(c) => c,
            None => *res
                .filter_crtcs(enc.possible_crtcs())
                .first()
                .ok_or_else(|| anyhow!("no CRTC for encoder"))?,
        };

        let (w, h) = mode.size();
        let (w, h) = (w as u32, h as u32);
        info!(
            "display: connector {:?} mode {}x{}@{} crtc {:?}",
            cinfo.handle(),
            w,
            h,
            mode.vrefresh(),
            crtc
        );
        let db = card
            .create_dumb_buffer((w, h), DrmFourcc::Xrgb8888, 32)
            .context("create dumb buffer")?;
        let fb = card.add_framebuffer(&db, 24, 32).context("add framebuffer")?;

        let dpms = card
            .get_properties(cinfo.handle())
            .ok()
            .and_then(|props| props.as_hashmap(&card).ok())
            .and_then(|m| m.get("DPMS").map(|p| p.handle()));

        Ok(Display {
            card,
            conn: cinfo.handle(),
            crtc,
            mode,
            fb,
            db,
            dpms,
            modeset_done: false,
            on: false,
            canvas: Canvas::new(w, h),
        })
    }

    /// Copies the canvas into the dumb buffer and pushes one frame to the panel.
    pub fn present(&mut self) -> Result<()> {
        self.copy_canvas()?;
        if !self.modeset_done {
            self.set_crtc()?;
            self.modeset_done = true;
        } else {
            // Command-mode panel: a flip (even to the same buffer) kicks off one
            // transfer. A legacy SETCRTC is the fallback if the flip is refused
            // (e.g. EBUSY while a previous flip is still pending).
            let _ = self.card.dirty_framebuffer(self.fb, &[]);
            if let Err(e) = self.card.page_flip(self.crtc, self.fb, PageFlipFlags::empty(), None) {
                log::debug!("page_flip: {e}; falling back to set_crtc");
                self.set_crtc()?;
            }
        }
        self.on = true;
        Ok(())
    }

    fn set_crtc(&self) -> Result<()> {
        self.card
            .set_crtc(self.crtc, Some(self.fb), (0, 0), &[self.conn], Some(self.mode))
            .context("set_crtc")
    }

    fn copy_canvas(&mut self) -> io::Result<()> {
        let pitch = self.db_pitch();
        let w = self.canvas.w as usize;
        let h = self.canvas.h as usize;
        let mut map = self.card.map_dumb_buffer(&mut self.db)?;
        let dst: &mut [u8] = map.as_mut();
        for y in 0..h {
            let src = &self.canvas.px[y * w..(y + 1) * w];
            let row = &mut dst[y * pitch..y * pitch + w * 4];
            for (d, s) in row.chunks_exact_mut(4).zip(src) {
                d.copy_from_slice(&s.to_le_bytes());
            }
        }
        Ok(())
        // `map` is dropped here, which unmaps the buffer.
    }

    fn db_pitch(&self) -> usize {
        use drm::buffer::Buffer;
        self.db.pitch() as usize
    }

    /// Screen off: DPMS off on the connector, or disable the CRTC if DPMS is missing.
    pub fn blank(&mut self) -> Result<()> {
        if !self.on {
            return Ok(());
        }
        let r = match self.dpms {
            Some(p) => self.card.set_property(self.conn, p, DPMS_OFF),
            None => Err(io::Error::from(io::ErrorKind::Unsupported)),
        };
        if let Err(e) = r {
            warn!("DPMS off failed ({e}); disabling CRTC");
            self.card.set_crtc(self.crtc, None, (0, 0), &[], None).context("disable crtc")?;
        }
        self.on = false;
        self.modeset_done = false;
        Ok(())
    }

    /// Screen on: DPMS on, then the next `present()` does a full SETCRTC.
    pub fn unblank(&mut self) {
        if let Some(p) = self.dpms {
            if let Err(e) = self.card.set_property(self.conn, p, DPMS_ON) {
                warn!("DPMS on failed: {e}");
            }
        }
        self.modeset_done = false;
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        let _ = self.card.destroy_framebuffer(self.fb);
        let _ = self.card.release_master_lock();
        // The dumb buffer is freed with the fd.
    }
}
