//! The Mac app's menu bar (File, Edit, View, …), drawn as a strip along the top
//! of its proxy window.
//!
//! The menu bar is not part of any Mac window, so it is not in the stream; the
//! host sends the titles as data (`menuBar`) and this draws them. It is painted by
//! the same egui context as the dashboard, because the font atlas lives in the
//! renderer shared by every window and two contexts would overwrite each other's
//! glyphs. A click is answered by hit-testing the rectangles recorded while
//! painting, not by egui input handling, so a strip needs no event plumbing.
//!
//! The strip covers the top of the Mac window, where its title bar is: the
//! compositor already draws this window's title bar, so nothing here is needed.

use egui_wgpu::wgpu;

use super::gpu::Gpu;
use crate::wire::MenuItem;

/// Height of the strip in logical pixels (scaled by the window's scale factor).
pub const HEIGHT_POINTS: f32 = 28.0;

const BACKGROUND: egui::Color32 = egui::Color32::from_rgb(0x1E, 0x24, 0x2E);
const HOVER: egui::Color32 = egui::Color32::from_rgb(0x34, 0x40, 0x55);
const TEXT: egui::Color32 = egui::Color32::from_rgb(0xF3, 0xF6, 0xFC);
const DISABLED: egui::Color32 = egui::Color32::from_rgb(0x7A, 0x86, 0x99);

/// Where one title was drawn, in the window's physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slot {
    pub index: u32,
    pub x0: u32,
    pub x1: u32,
    pub enabled: bool,
}

#[derive(Debug, Default)]
pub struct MenuStrip {
    pub items: Vec<MenuItem>,
    /// Filled in by `paint`; empty until the first frame.
    pub slots: Vec<Slot>,
    /// The item under the pointer, for the highlight.
    pub hover: Option<u32>,
}

impl MenuStrip {
    pub fn height(scale: f64) -> u32 {
        (HEIGHT_POINTS as f64 * scale).round().max(1.0) as u32
    }

    /// The enabled item under client pixel `x`, if the point is in the strip.
    pub fn item_at(&self, x: i32, y: i32, scale: f64) -> Option<u32> {
        if y < 0 || y as u32 >= Self::height(scale) || x < 0 {
            return None;
        }
        self.slots
            .iter()
            .find(|s| s.enabled && (x as u32) >= s.x0 && (x as u32) < s.x1)
            .map(|s| s.index)
    }

    /// The left edge of item `index` and the y just below the strip, for placing
    /// the menu it opened.
    pub fn anchor(&self, index: u32, scale: f64) -> Option<(u32, u32)> {
        self.slots
            .iter()
            .find(|s| s.index == index)
            .map(|s| (s.x0, Self::height(scale)))
    }

    /// Paint the strip over the frame in `view` (which must already hold the
    /// window's content) and record where each title went. Returns command
    /// buffers to submit ahead of `encoder`'s.
    pub fn paint(
        &mut self,
        gpu: &Gpu,
        ctx: &egui::Context,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        size: (u32, u32),
        scale: f64,
    ) -> Vec<wgpu::CommandBuffer> {
        let scale_f = scale as f32;
        let mut raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(size.0 as f32 / scale_f, size.1 as f32 / scale_f),
            )),
            ..Default::default()
        };
        raw.viewports.insert(
            egui::ViewportId::ROOT,
            egui::ViewportInfo {
                native_pixels_per_point: Some(scale_f),
                ..Default::default()
            },
        );

        let items = self.items.clone();
        let hover = self.hover;
        let mut slots: Vec<Slot> = Vec::with_capacity(items.len());
        let mut output = ctx.run_ui(raw, |ui| {
            let painter = ui.painter().clone();
            let bar = egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(size.0 as f32 / scale_f, HEIGHT_POINTS),
            );
            painter.rect_filled(bar, egui::CornerRadius::same(0), BACKGROUND);
            let font = egui::FontId::proportional(14.0);
            let mut x = 6.0_f32;
            for item in &items {
                let colour = if item.enabled { TEXT } else { DISABLED };
                // Measure first so the highlight can sit behind the text.
                let galley = painter.layout_no_wrap(item.title.clone(), font.clone(), colour);
                let width = galley.size().x + 20.0;
                let slot = egui::Rect::from_min_size(
                    egui::pos2(x, 0.0),
                    egui::vec2(width, HEIGHT_POINTS),
                );
                if hover == Some(item.index) && item.enabled {
                    painter.rect_filled(slot, egui::CornerRadius::same(4), HOVER);
                }
                painter.galley(
                    egui::pos2(x + 10.0, (HEIGHT_POINTS - galley.size().y) / 2.0),
                    galley,
                    colour,
                );
                slots.push(Slot {
                    index: item.index,
                    x0: (x * scale_f).round() as u32,
                    x1: ((x + width) * scale_f).round() as u32,
                    enabled: item.enabled,
                });
                x += width;
            }
        });
        self.slots = slots;

        let state = &gpu.state;
        let mut textures = std::mem::take(&mut output.textures_delta);
        {
            let mut renderer = state.renderer.write();
            for (id, deltas) in textures.set.drain() {
                for delta in &deltas {
                    renderer.update_texture(&state.device, &state.queue, id, delta);
                }
            }
        }
        let prims = ctx.tessellate(std::mem::take(&mut output.shapes), output.pixels_per_point);
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [size.0.max(1), size.1.max(1)],
            pixels_per_point: output.pixels_per_point,
        };
        let extra = state
            .renderer
            .write()
            .update_buffers(&state.device, &state.queue, encoder, &prims, &screen);
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("transom menu strip"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let mut pass = pass.forget_lifetime();
            state.renderer.read().render(&mut pass, &prims, &screen);
        }
        let mut renderer = state.renderer.write();
        for id in textures.free.drain() {
            renderer.free_texture(&id);
        }
        // Buffers egui wants submitted ahead of the frame; the caller submits them
        // with the encoder this pass was recorded into.
        extra
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip() -> MenuStrip {
        let mut s = MenuStrip::default();
        s.slots = vec![
            Slot { index: 1, x0: 6, x1: 60, enabled: true },
            Slot { index: 2, x0: 60, x1: 110, enabled: false },
            Slot { index: 3, x0: 110, x1: 170, enabled: true },
        ];
        s
    }

    #[test]
    fn height_scales_with_the_window() {
        assert_eq!(MenuStrip::height(1.0), 28);
        assert_eq!(MenuStrip::height(1.5), 42);
        assert_eq!(MenuStrip::height(2.0), 56);
    }

    #[test]
    fn a_click_picks_the_enabled_item_under_it() {
        let s = strip();
        assert_eq!(s.item_at(30, 10, 1.0), Some(1));
        assert_eq!(s.item_at(120, 10, 1.0), Some(3));
        // Disabled, between items, past the last, or below the strip.
        assert_eq!(s.item_at(80, 10, 1.0), None);
        assert_eq!(s.item_at(2, 10, 1.0), None);
        assert_eq!(s.item_at(400, 10, 1.0), None);
        assert_eq!(s.item_at(30, 28, 1.0), None);
        assert_eq!(s.item_at(30, -1, 1.0), None);
    }

    #[test]
    fn a_menu_is_anchored_under_its_item() {
        let s = strip();
        assert_eq!(s.anchor(3, 1.0), Some((110, 28)));
        assert_eq!(s.anchor(3, 2.0), Some((110, 56)));
        assert_eq!(s.anchor(9, 1.0), None);
    }
}
