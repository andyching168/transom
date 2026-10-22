//! One proxy window: a native Wayland toplevel that stands in for a single Mac
//! window (keyed by its `WindowId`), with its own wgpu surface.
//!
//! The 1:1 guarantee lives in two rules this type enforces:
//!  * On every configure, the surface is reconfigured to the **exact physical
//!    client size** (never a logical size), so surface size == client size always.
//!  * The frame is point-fetched from the window's source sub-rect. When the
//!    client size equals the source size (the steady state after a snap) that is a
//!    pixel-exact blit; during a live drag the source is transiently stretched
//!    into the new size (the accepted resample, snapped away when the drag ends).

use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_wgpu::wgpu;
use egui_winit::winit::dpi::PhysicalSize;
use egui_winit::winit::window::Window;

use super::gpu::{Gpu, RenderMode, SourceTexture};
use super::input::ScrollAccumulator;
use super::resize::{Request, ResizeTracker};
use crate::pointer::PointerGate;
use crate::wire::{Rect, Size};

/// Right after a window is created some compositors reconfigure it to a size
/// that differs from the one requested (GNOME sizes the frame inside the first
/// requested size, so the client area comes back smaller by the decoration).
/// Within this window such a configure is answered by asking again, not treated
/// as the user resizing.
const STARTUP_GRACE: Duration = Duration::from_millis(2000);
const STARTUP_RETRIES: u8 = 3;

pub struct Proxy {
    pub window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    /// The window's sub-rect of the shared VDS texture (physical pixels).
    pub source: Rect,
    pub tracker: ResizeTracker,
    pub pointer: PointerGate,
    pub scroll: ScrollAccumulator,
    /// Last pointer position in client pixels, for wheel events (winit's scroll
    /// event carries none).
    pub cursor: (i32, i32),
    /// M0 diagnostic: draw a 1px checkerboard instead of sampling the stream.
    pub checkerboard: bool,
    /// The physical client size we want: the last size we asked for or that the
    /// compositor settled on. A scale-factor change must not alter it.
    pub desired: Size,
    created: Instant,
    startup_retries: u8,
    needs_configure: bool,
}

impl Proxy {
    pub fn new(
        gpu: &Gpu,
        window: Arc<Window>,
        source: Rect,
        checkerboard: bool,
    ) -> Result<Proxy, String> {
        let surface = gpu
            .state
            .instance
            .create_surface(window.clone())
            .map_err(|e| format!("create surface: {e}"))?;
        let inner = window.inner_size();
        let size = Size {
            w: inner.width.max(1),
            h: inner.height.max(1),
        };
        gpu.configure(&surface, size.w, size.h);
        Ok(Proxy {
            window,
            surface,
            source,
            tracker: ResizeTracker::new(size, Instant::now()),
            pointer: PointerGate::default(),
            scroll: ScrollAccumulator::default(),
            cursor: (0, 0),
            checkerboard,
            desired: size,
            created: Instant::now(),
            startup_retries: 0,
            needs_configure: false,
        })
    }

    /// The compositor configured a new client size. Reconfigures the surface to
    /// the exact physical size and returns the resize requests to send.
    pub fn resized(&mut self, gpu: &Gpu, w: u32, h: u32, now: Instant) -> Vec<Request> {
        if w == 0 || h == 0 {
            return vec![]; // minimized; nothing to size to
        }
        let mut size = Size { w, h };
        if now.duration_since(self.created) < STARTUP_GRACE
            && size != self.desired
            && self.startup_retries < STARTUP_RETRIES
        {
            self.startup_retries += 1;
            // Ask again; if the compositor takes the size it was only the
            // decoration adjustment. If it refuses (a tiling layout), the size it
            // chose stands and is handled as a normal resize below.
            let want = self.desired;
            self.tracker.expect(want);
            if let Some(applied) = self.window.request_inner_size(PhysicalSize::new(want.w, want.h))
            {
                if applied.width == want.w && applied.height == want.h {
                    size = want;
                }
            }
        } else {
            self.desired = size;
        }
        gpu.configure(&self.surface, size.w, size.h);
        self.needs_configure = false;
        let requests = self.tracker.on_resized(size, now);
        self.report_pixel_size();
        requests
    }

    fn report_pixel_size(&self) {
        if !self.checkerboard {
            return;
        }
        let inner = self.window.inner_size();
        let size = self.tracker.size;
        println!(
            "pixel-check: scale={:.3} physical={}x{} surface={}x{} {}",
            self.window.scale_factor(),
            inner.width,
            inner.height,
            size.w,
            size.h,
            if inner.width == size.w && inner.height == size.h {
                "PASS"
            } else {
                "FAIL"
            }
        );
    }

    /// The host reported this window's ACTUAL geometry (I-4). Updates the source
    /// sub-rect and, unless a drag is in flight, snaps the window's client size to
    /// it so the blit is exactly 1:1. Returns whether a snap was requested.
    pub fn set_source(&mut self, gpu: &Gpu, source: Rect) -> bool {
        self.source = source;
        self.snap_if_needed(gpu)
    }

    /// Set the client size to the source size. Wayland applies a client-chosen
    /// size synchronously (and sends no configure back for it), so the new size
    /// is adopted here; a tiled or maximized window refuses, and the blit then
    /// crops at native scale (never resamples) until it is resized.
    pub fn snap_if_needed(&mut self, gpu: &Gpu) -> bool {
        let size = self.tracker.size;
        if self.tracker.resizing || (size.w == self.source.w && size.h == self.source.h) {
            return false;
        }
        let target = Size {
            w: self.source.w.max(1),
            h: self.source.h.max(1),
        };
        self.desired = target;
        self.tracker.expect(target);
        if let Some(applied) = self
            .window
            .request_inner_size(PhysicalSize::new(target.w, target.h))
        {
            if applied.width > 0 && applied.height > 0 {
                let now = Instant::now();
                // Expected (or unchanged) sizes produce no requests.
                self.resized(gpu, applied.width, applied.height, now);
            }
        }
        true
    }

    /// Draw one frame and present.
    pub fn render(&mut self, gpu: &Gpu, source_tex: Option<&SourceTexture>) {
        let size = self.tracker.size;
        // While waiting for a resize acknowledgement, crop/letterbox at native
        // scale. Only an actual interactive resize may stretch the pixels.
        let stretch = self.tracker.resizing;
        let (draw_w, draw_h) = if stretch || self.checkerboard {
            (size.w, size.h)
        } else {
            (size.w.min(self.source.w).max(1), size.h.min(self.source.h).max(1))
        };
        let (crop_w, crop_h) = if stretch {
            (self.source.w, self.source.h)
        } else {
            (draw_w, draw_h)
        };
        let clear = (draw_w != size.w || draw_h != size.h).then_some([0.08, 0.10, 0.14, 1.0]);
        let mode = if self.checkerboard {
            RenderMode::Checkerboard
        } else if source_tex.is_some() {
            RenderMode::Source {
                crop: [self.source.x, self.source.y, crop_w, crop_h],
            }
        } else {
            RenderMode::Waiting
        };
        if self.needs_configure {
            gpu.configure(&self.surface, size.w, size.h);
            self.needs_configure = false;
        }
        let ok = gpu.draw(
            &self.surface,
            (size.w, size.h),
            (draw_w, draw_h),
            clear,
            mode,
            source_tex,
        );
        if !ok {
            // Outdated or lost: reconfigure and try again next tick.
            self.needs_configure = true;
        }
    }
}
