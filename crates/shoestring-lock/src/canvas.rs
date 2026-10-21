//! Per-output pixels for the lock screen.
//!
//! A [`Canvas`] holds the screen in CPU memory and redraws only the regions
//! that change: a maze step touches a couple of cells, a keystroke the prompt
//! box. Drawing goes through a [`Target`] clipped to the region being redrawn.
//!
//! Frames are shown through a small pool of shm buffers that are reused once
//! the compositor releases them. Each buffer remembers which regions of the
//! canvas changed since it was last filled, so presenting copies only those,
//! and the surface is damaged only where the frame changed. Redrawing and
//! damaging the whole surface every frame made the compositor re-upload and
//! re-composite the full screen 30 times a second.

use std::fs::File;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use memmap2::MmapMut;
use wayland_client::{
    protocol::{
        wl_buffer::WlBuffer,
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
    Dispatch, QueueHandle,
};

/// Buffers per output. Two cover the usual case (the compositor still holds
/// the last frame while we fill the next); more only if it holds on longer.
const MAX_BUFFERS: usize = 4;
/// Past this many damage rects, damage (or copy) their bounding box instead.
const MAX_RECTS: usize = 16;

/// A rectangle in buffer pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    pub fn intersect(&self, o: &Rect) -> Rect {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = (self.x + self.w).min(o.x + o.w);
        let y1 = (self.y + self.h).min(o.y + o.h);
        Rect::new(x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
    }

    /// The smallest rectangle covering both.
    pub fn bounding(&self, o: &Rect) -> Rect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = (self.x + self.w).max(o.x + o.w);
        let y1 = (self.y + self.h).max(o.y + o.h);
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }
}

/// Collapse a long list of rects into their bounding box.
fn simplify(rects: &mut Vec<Rect>) {
    if rects.len() > MAX_RECTS {
        let bbox = rects
            .iter()
            .fold(Rect::new(0, 0, 0, 0), |acc, r| acc.bounding(r));
        rects.clear();
        rects.push(bbox);
    }
}

/// Somewhere to draw: ARGB8888 pixels, with every write clipped to `clip`.
pub struct Target<'a> {
    pub px: &'a mut [u8],
    pub w: u32,
    pub clip: Rect,
}

impl Target<'_> {
    pub fn fill_rect(&mut self, r: Rect, color: u32) {
        let r = r.intersect(&self.clip);
        if r.is_empty() {
            return;
        }
        let stride = self.w as usize * 4;
        let row: Vec<u8> = color.to_ne_bytes().repeat(r.w as usize);
        for y in r.y..r.y + r.h {
            let off = y as usize * stride + r.x as usize * 4;
            self.px[off..off + row.len()].copy_from_slice(&row);
        }
    }

    /// Blend an 8-bit coverage bitmap (`src_w`×`src_h`) in `color` at
    /// `(dst_x, dst_y)`.
    pub fn blit_alpha(
        &mut self,
        dst_x: i32,
        dst_y: i32,
        src_w: u32,
        src_h: u32,
        src: &[u8],
        color: u32,
    ) {
        let area = Rect::new(dst_x, dst_y, src_w as i32, src_h as i32).intersect(&self.clip);
        if area.is_empty() {
            return;
        }
        let [fb, fg, fr, _fa] = color.to_le_bytes();
        let stride = self.w as usize * 4;
        for dy in area.y..area.y + area.h {
            let sy = (dy - dst_y) as u32;
            for dx in area.x..area.x + area.w {
                let sx = (dx - dst_x) as u32;
                let coverage = src[(sy * src_w + sx) as usize];
                if coverage == 0 {
                    continue;
                }
                let off = dy as usize * stride + dx as usize * 4;
                let a = coverage as i32;
                for (i, fg_chan) in [fb, fg, fr].iter().enumerate() {
                    let bg = self.px[off + i] as i32;
                    let out = bg + (a * (*fg_chan as i32 - bg)) / 255;
                    self.px[off + i] = out.clamp(0, 255) as u8;
                }
                self.px[off + 3] = 0xFF;
            }
        }
    }
}

/// One shm buffer in the pool.
struct ShmBuf {
    _file: File,
    mmap: MmapMut,
    buffer: WlBuffer,
    /// Set while the compositor may still read it (cleared on `release`).
    busy: Arc<AtomicBool>,
    /// Canvas regions that changed since this buffer was last filled.
    stale: Vec<Rect>,
}

impl Drop for ShmBuf {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

/// One output's lock-screen pixels plus the buffers that show them.
pub struct Canvas {
    pub w: u32,
    pub h: u32,
    pixels: Vec<u8>,
    bufs: Vec<ShmBuf>,
    /// Damage drawn on the canvas but not yet committed to the surface
    /// (every buffer was busy at the time).
    unpresented: Vec<Rect>,
    /// Where the password prompt was last drawn, so hiding or resizing it
    /// knows what to repaint.
    pub prompt_box: Option<Rect>,
}

impl Canvas {
    pub fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            pixels: vec![0; w as usize * h as usize * 4],
            bufs: Vec::new(),
            unpresented: Vec::new(),
            prompt_box: None,
        }
    }

    pub fn full(&self) -> Rect {
        Rect::new(0, 0, self.w as i32, self.h as i32)
    }

    /// Draw into the canvas, clipped to `clip`.
    pub fn target(&mut self, clip: Rect) -> Target<'_> {
        let clip = clip.intersect(&Rect::new(0, 0, self.w as i32, self.h as i32));
        Target {
            px: &mut self.pixels,
            w: self.w,
            clip,
        }
    }

    pub fn has_unpresented(&self) -> bool {
        !self.unpresented.is_empty()
    }

    /// Show the canvas on `surface`, damaging `damage` (the regions drawn
    /// since the last call) plus anything left over from a frame that found
    /// no free buffer. Returns `false` if every buffer is still held by the
    /// compositor; the damage is kept for the next call.
    pub fn present<D>(
        &mut self,
        shm: &WlShm,
        qh: &QueueHandle<D>,
        surface: &WlSurface,
        damage: &[Rect],
    ) -> Result<bool>
    where
        D: Dispatch<WlBuffer, Arc<AtomicBool>> + Dispatch<WlShmPool, ()> + 'static,
    {
        self.unpresented.extend_from_slice(damage);
        simplify(&mut self.unpresented);
        for b in &mut self.bufs {
            b.stale.extend_from_slice(damage);
            simplify(&mut b.stale);
        }

        let idx = match self
            .bufs
            .iter()
            .position(|b| !b.busy.load(Ordering::Acquire))
        {
            Some(i) => i,
            None if self.bufs.len() < MAX_BUFFERS => {
                self.bufs.push(self.new_buffer(shm, qh)?);
                self.bufs.len() - 1
            }
            None => return Ok(false),
        };

        let stride = self.w as usize * 4;
        let buf = &mut self.bufs[idx];
        for r in buf.stale.drain(..) {
            let r = r.intersect(&Rect::new(0, 0, self.w as i32, self.h as i32));
            for y in r.y..r.y + r.h {
                let a = y as usize * stride + r.x as usize * 4;
                let b = a + r.w as usize * 4;
                buf.mmap[a..b].copy_from_slice(&self.pixels[a..b]);
            }
        }
        buf.busy.store(true, Ordering::Release);
        surface.attach(Some(&buf.buffer), 0, 0);
        for r in self.unpresented.drain(..) {
            surface.damage_buffer(r.x, r.y, r.w, r.h);
        }
        surface.commit();
        Ok(true)
    }

    fn new_buffer<D>(&self, shm: &WlShm, qh: &QueueHandle<D>) -> Result<ShmBuf>
    where
        D: Dispatch<WlBuffer, Arc<AtomicBool>> + Dispatch<WlShmPool, ()> + 'static,
    {
        let stride = self.w as i32 * 4;
        let size = stride as usize * self.h as usize;
        let file = tempfile::tempfile()?;
        file.set_len(size as u64)?;
        // SAFETY: the file is private to us and sized above.
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        let pool: WlShmPool = shm.create_pool(file.as_fd(), size as i32, qh, ());
        let busy = Arc::new(AtomicBool::new(false));
        let buffer = pool.create_buffer(
            0,
            self.w as i32,
            self.h as i32,
            stride,
            wl_shm::Format::Argb8888,
            qh,
            busy.clone(),
        );
        pool.destroy();
        Ok(ShmBuf {
            _file: file,
            mmap,
            buffer,
            busy,
            // A new buffer holds nothing yet.
            stale: vec![self.full()],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_intersect_and_bounding() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(&b), Rect::new(5, 5, 5, 5));
        assert_eq!(a.bounding(&b), Rect::new(0, 0, 15, 15));
        assert!(a.intersect(&Rect::new(20, 20, 1, 1)).is_empty());
        assert_eq!(Rect::new(0, 0, 0, 0).bounding(&b), b);
    }

    #[test]
    fn many_rects_collapse_to_their_bounding_box() {
        let mut rects: Vec<Rect> = (0..20).map(|i| Rect::new(i * 2, i, 1, 1)).collect();
        simplify(&mut rects);
        assert_eq!(rects, vec![Rect::new(0, 0, 39, 20)]);
    }

    #[test]
    fn fill_rect_stays_inside_the_clip() {
        let mut px = vec![0u8; 4 * 4 * 4];
        let mut t = Target {
            px: &mut px,
            w: 4,
            clip: Rect::new(1, 1, 2, 2),
        };
        t.fill_rect(Rect::new(0, 0, 4, 4), 0xFF11_2233);
        let painted: Vec<(usize, usize)> = (0..4)
            .flat_map(|y| (0..4).map(move |x| (x, y)))
            .filter(|&(x, y)| px[(y * 4 + x) * 4 + 3] == 0xFF)
            .collect();
        assert_eq!(painted, vec![(1, 1), (2, 1), (1, 2), (2, 2)]);
    }
}
