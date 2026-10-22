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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_wgpu::wgpu;
use egui_winit::winit::dpi::PhysicalSize;
use egui_winit::winit::window::{CursorIcon, Window};

use super::gpu::{Acquired, Gpu, Layer, RenderMode, SourceTexture};
use super::input::ScrollAccumulator;
use super::resize::{Request, ResizeTracker};
use super::strip::MenuStrip;
use crate::wire::CursorShape;
use crate::overlay::{self, Placed};
use crate::pointer::PointerGate;
use crate::wire::{InputEvent, MouseButton, Rect, Size};

/// What a pointer message on a proxy window turned into.
#[derive(Debug, PartialEq)]
pub enum Pointer {
    /// Send this to window `id` (the proxy itself or a menu/popover on it).
    Send(u64, InputEvent),
    /// A press on menu-bar item `index`: open its menu.
    Strip(u32),
    Nothing,
}

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
    /// Menus, popovers and input-method lists hanging off this window, parents
    /// first (`overlay::layout`).
    pub overlays: Vec<overlay::Item>,
    placed: Vec<Placed>,
    /// The Mac app's menu bar, once the host has sent it.
    pub strip: Option<MenuStrip>,
    /// Pointer state for each overlay, as `pointer` is for the window itself.
    gates: HashMap<u64, PointerGate>,
    /// The window a held button went down on; it gets the drag and the release.
    grab: Option<u64>,
    /// A press was spent on the menu strip; its release is too.
    strip_press: bool,
    /// What the Mac's pointer looks like under the client's pointer (`cursor`).
    mac_cursor: CursorShape,
    /// The icon last set on the window, so a motion event only calls into the
    /// compositor when it changes.
    applied_cursor: CursorIcon,
}

fn icon(shape: CursorShape) -> CursorIcon {
    match shape {
        CursorShape::Arrow => CursorIcon::Default,
        CursorShape::IBeam => CursorIcon::Text,
        CursorShape::PointingHand => CursorIcon::Pointer,
        CursorShape::Crosshair => CursorIcon::Crosshair,
        CursorShape::ResizeLeftRight => CursorIcon::EwResize,
        CursorShape::ResizeUpDown => CursorIcon::NsResize,
        CursorShape::OpenHand => CursorIcon::Grab,
        CursorShape::ClosedHand => CursorIcon::Grabbing,
        CursorShape::NotAllowed => CursorIcon::NotAllowed,
    }
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
            overlays: Vec::new(),
            placed: Vec::new(),
            strip: None,
            gates: HashMap::new(),
            grab: None,
            strip_press: false,
            mac_cursor: CursorShape::Arrow,
            applied_cursor: CursorIcon::Default,
        })
    }

    /// The compositor configured a new client size. Reconfigures the surface to
    /// the exact physical size and returns the resize requests to send.
    pub fn resized(&mut self, gpu: &Gpu, w: u32, h: u32, now: Instant) -> Vec<Request> {
        if w == 0 || h == 0 {
            return vec![]; // minimized; nothing to size to
        }
        // The tracker holds the content size; the menu strip is a band above it.
        let band = self.band();
        let mut size = Size {
            w,
            h: h.saturating_sub(band).max(1),
        };
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
            if let Some(applied) = self
                .window
                .request_inner_size(PhysicalSize::new(want.w, want.h + band))
            {
                if applied.width == want.w && applied.height == want.h + band {
                    size = want;
                }
            }
        } else {
            self.desired = size;
        }
        gpu.configure(&self.surface, size.w, size.h + band);
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
        let band = self.band();
        println!(
            "pixel-check: scale={:.3} physical={}x{} surface={}x{} {}",
            self.window.scale_factor(),
            inner.width,
            inner.height,
            size.w,
            size.h,
            if inner.width == size.w && inner.height == size.h + band {
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
            .request_inner_size(PhysicalSize::new(target.w, target.h + self.band()))
        {
            if applied.width > 0 && applied.height > 0 {
                let now = Instant::now();
                // Expected (or unchanged) sizes produce no requests.
                self.resized(gpu, applied.width, applied.height, now);
            }
        }
        true
    }

    /// Draw one frame and present. `ctx` paints the menu strip.
    pub fn render(&mut self, gpu: &Gpu, source_tex: Option<&SourceTexture>, ctx: &egui::Context) {
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
        let band = self.band();
        if self.needs_configure {
            gpu.configure(&self.surface, size.w, size.h + band);
            self.needs_configure = false;
        }
        // Menus and the menu strip are composed with the content. While a resize
        // gesture stretches the content the menus are left out (they are placed
        // from the settled size); the strip stays, since it is its own band.
        let composed = (!self.overlays.is_empty() || self.strip.is_some()) && !self.checkerboard;
        if let (true, Some(tex)) = (composed, source_tex) {
            self.render_composed(gpu, tex, ctx, (crop_w, crop_h), (draw_w, draw_h), stretch, clear);
            return;
        }
        let ok = gpu.draw(
            &self.surface,
            (size.w, size.h + band),
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

    // --- menus, popovers and the menu strip -----------------------------------

    fn scale(&self) -> f64 {
        self.window.scale_factor()
    }

    /// Height of the menu strip band above the content (physical pixels); `0`
    /// without a menu bar.
    pub fn band(&self) -> u32 {
        self.band_at(self.scale())
    }

    /// `band` at another scale factor, for a scale change not yet applied.
    pub fn band_at(&self, scale: f64) -> u32 {
        self.strip.as_ref().map_or(0, |_| MenuStrip::height(scale))
    }

    fn content(&self) -> Size {
        Size {
            w: self.source.w,
            h: self.source.h,
        }
    }

    /// Recompute where each overlay is drawn from the current size, source and
    /// menu strip.
    fn relayout(&mut self) {
        let scale = self.scale();
        let size = self.tracker.size;
        let strip = &self.strip;
        let band = self.band();
        let before = self.placed.clone();
        // Laid out in content coordinates, then moved down by the strip band: a
        // menu hanging off the strip goes at the top of the content, right under it.
        self.placed = overlay::layout(
            (size.w, size.h),
            self.source,
            &self.overlays,
            |item| {
                strip
                    .as_ref()
                    .and_then(|s| s.anchor_for_mac_x(item.source.x, scale))
                    .map(|(x, y)| (x, y.saturating_sub(band)))
            },
        );
        for p in &mut self.placed {
            p.y += band;
        }
        if std::env::var_os("TRANSOM_DEBUG").is_some() && before != self.placed {
            eprintln!(
                "debug: overlays on window at {:?} client {}x{}: {:?} -> {:?}",
                self.source, size.w, size.h, self.overlays, self.placed
            );
        }
        let live: Vec<u64> = self.overlays.iter().map(|o| o.id).collect();
        self.gates.retain(|id, _| live.contains(id));
        if self.grab.is_some_and(|g| g != 0 && !live.contains(&g)) {
            self.grab = None;
        }
    }

    fn render_composed(
        &mut self,
        gpu: &Gpu,
        tex: &SourceTexture,
        ctx: &egui::Context,
        crop: (u32, u32),
        draw: (u32, u32),
        stretch: bool,
        clear: Option<[f64; 4]>,
    ) {
        self.relayout();
        let size = self.tracker.size;
        let band = self.band();
        let surface = (size.w, size.h + band);
        let frame = match gpu.acquire(&self.surface) {
            Acquired::Frame(f) => f,
            Acquired::Reconfigure => {
                self.needs_configure = true;
                return;
            }
            Acquired::Skip => return,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut layers = vec![Layer {
            crop: [self.source.x, self.source.y, crop.0, crop.1],
            target: draw,
            dest: (0, band),
        }];
        for p in self.placed.iter().filter(|_| !stretch) {
            layers.push(Layer {
                crop: [p.crop.x, p.crop.y, p.w, p.h],
                target: (p.w, p.h),
                dest: (p.x, p.y),
            });
        }
        let mut encoder = gpu
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("transom proxy"),
            });
        gpu.encode_layers(
            &mut encoder,
            &view,
            surface,
            clear.unwrap_or([0.0, 0.0, 0.0, 1.0]),
            &layers,
            tex,
        );
        let scale = self.scale();
        let mut extra = Vec::new();
        if let Some(strip) = self.strip.as_mut() {
            extra = strip.paint(gpu, ctx, &mut encoder, &view, surface, scale);
        }
        gpu.queue()
            .submit(extra.into_iter().chain([encoder.finish()]));
        gpu.queue().present(frame);
    }

    /// The Mac's pointer shape changed under the client's pointer. The strip is the
    /// client's own, so over it the pointer stays the default arrow.
    pub fn set_mac_cursor(&mut self, shape: CursorShape) {
        self.mac_cursor = shape;
        self.apply_cursor(false);
    }

    fn apply_cursor(&mut self, over_strip: bool) {
        let want = if over_strip { CursorIcon::Default } else { icon(self.mac_cursor) };
        if want != self.applied_cursor {
            self.applied_cursor = want;
            self.window.set_cursor(want);
        }
    }

    /// Replace the transient windows hanging off this window.
    pub fn set_overlays(&mut self, overlays: Vec<overlay::Item>) {
        self.overlays = overlays;
    }

    /// Replace the menu bar. An empty one removes the strip. The strip is a band
    /// above the content, so the window grows or shrinks by its height to leave the
    /// content size (and the Mac window's) unchanged.
    pub fn set_menu_bar(&mut self, gpu: &Gpu, items: Vec<crate::wire::MenuItem>) {
        let before = self.band();
        self.replace_menu_bar(items);
        if self.band() != before {
            let content = self.tracker.size;
            self.tracker.expect(content);
            if let Some(applied) = self
                .window
                .request_inner_size(PhysicalSize::new(content.w, content.h + self.band()))
            {
                if applied.width > 0 && applied.height > 0 {
                    self.resized(gpu, applied.width, applied.height, Instant::now());
                }
            }
            self.needs_configure = true;
        }
    }

    fn replace_menu_bar(&mut self, items: Vec<crate::wire::MenuItem>) {
        if items.is_empty() {
            self.strip = None;
        } else if let Some(strip) = self.strip.as_mut() {
            strip.items = items;
        } else {
            self.strip = Some(MenuStrip {
                items,
                ..Default::default()
            });
        }
    }

    /// Where a pointer at the last known cursor position lands: the menu strip,
    /// a menu or popover, or the window itself. A held button keeps the window it
    /// went down on.
    fn target(&self) -> Target {
        if let Some(grab) = self.grab {
            return Target::Window(grab);
        }
        let (x, y) = self.cursor;
        if let Some(strip) = &self.strip {
            if let Some(index) = strip.item_at(x, y, self.scale()) {
                return Target::Strip(index);
            }
        }
        match overlay::hit(&self.placed, x, y) {
            Some((id, _, _)) => Target::Window(id),
            None => Target::Window(0),
        }
    }

    /// The point in `target`'s own coordinates and its content size. `0` is the
    /// window itself.
    fn local(&self, target: u64) -> Option<((i32, i32), Size)> {
        let (x, y) = self.cursor;
        if target == 0 {
            return Some(((x, y - self.band() as i32), self.content()));
        }
        let p = self.placed.iter().find(|p| p.id == target)?;
        // In the whole popup's coordinates, which is what the host maps.
        Some((
            (x - p.x as i32 + p.off.0 as i32, y - p.y as i32 + p.off.1 as i32),
            Size {
                w: p.full.0,
                h: p.full.1,
            },
        ))
    }

    /// Hover or drag. `own` is this proxy's window id.
    pub fn pointer_motion(&mut self, own: u64) -> (Pointer, bool) {
        let before = self.strip.as_ref().and_then(|s| s.hover);
        let target = self.target();
        let hover = match target {
            Target::Strip(i) => Some(i),
            _ => None,
        };
        if let Some(strip) = self.strip.as_mut() {
            strip.hover = hover;
        }
        let redraw = before != hover;
        self.apply_cursor(hover.is_some());
        let Target::Window(t) = target else {
            return (Pointer::Nothing, redraw);
        };
        let Some(((x, y), content)) = self.local(t) else {
            return (Pointer::Nothing, redraw);
        };
        let event = if t == 0 {
            self.pointer.motion(x, y, content)
        } else {
            self.gates.entry(t).or_default().motion(x, y, content)
        };
        let id = if t == 0 { own } else { t };
        (
            event.map_or(Pointer::Nothing, |e| Pointer::Send(id, e)),
            redraw,
        )
    }

    pub fn pointer_press(&mut self, own: u64, button: MouseButton) -> Pointer {
        let t = match self.target() {
            Target::Strip(index) => {
                self.strip_press = true;
                return Pointer::Strip(index);
            }
            Target::Window(t) => t,
        };
        let Some(((x, y), content)) = self.local(t) else {
            return Pointer::Nothing;
        };
        let event = if t == 0 {
            self.pointer.press(button, x, y, content)
        } else {
            self.gates.entry(t).or_default().press(button, x, y, content)
        };
        match event {
            Some(e) => {
                self.grab = Some(t);
                Pointer::Send(if t == 0 { own } else { t }, e)
            }
            None => Pointer::Nothing,
        }
    }

    pub fn pointer_release(&mut self, own: u64, button: MouseButton) -> Pointer {
        if std::mem::take(&mut self.strip_press) {
            return Pointer::Nothing;
        }
        let t = match self.target() {
            Target::Window(t) => t,
            Target::Strip(_) => return Pointer::Nothing,
        };
        let Some(((x, y), content)) = self.local(t) else {
            return Pointer::Nothing;
        };
        let event = if t == 0 {
            self.pointer.release(button, x, y, content)
        } else {
            self.gates.entry(t).or_default().release(button, x, y, content)
        };
        match event {
            Some(e) => {
                self.grab = None;
                Pointer::Send(if t == 0 { own } else { t }, e)
            }
            None => Pointer::Nothing,
        }
    }

    pub fn pointer_scroll(&mut self, own: u64, dx: i32, dy: i32) -> Pointer {
        let Target::Window(t) = self.target() else {
            return Pointer::Nothing;
        };
        let Some(((x, y), content)) = self.local(t) else {
            return Pointer::Nothing;
        };
        let event = if t == 0 {
            self.pointer.scroll(x, y, dx, dy, content)
        } else {
            self.gates.entry(t).or_default().scroll(x, y, dx, dy, content)
        };
        event.map_or(Pointer::Nothing, |e| {
            Pointer::Send(if t == 0 { own } else { t }, e)
        })
    }

    /// The pointer was taken away: release every button the host thinks is down,
    /// on the window and on every menu.
    pub fn release_all_pointers(&mut self, own: u64) -> Vec<(u64, InputEvent)> {
        self.grab = None;
        self.strip_press = false;
        let mut out: Vec<(u64, InputEvent)> = self
            .pointer
            .release_all()
            .into_iter()
            .map(|e| (own, e))
            .collect();
        for (id, gate) in self.gates.iter_mut() {
            out.extend(gate.release_all().into_iter().map(|e| (*id, e)));
        }
        out
    }
}

/// Where a pointer position lands.
#[derive(Clone, Copy)]
enum Target {
    /// A window: `0` is the proxy's own, anything else a menu or popover.
    Window(u64),
    Strip(u32),
}
