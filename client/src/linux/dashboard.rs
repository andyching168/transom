//! The dashboard: pick a Mac (nearby or saved), connect, and browse its shared
//! windows. This is the egui counterpart of `win::connect` + `win::gallery`.
//!
//! Selector thumbnails are UI previews only. Interactive proxy windows never pass
//! through this downsampling path (the proxies sample the decoded frame 1:1).
//!
//! The struct holds all state and builds the UI from an `egui::Context`; it knows
//! nothing about winit or wgpu, so its filtering, thumbnailing and persistence are
//! unit-tested here and the window glue stays in `app`.

use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Layout, RichText, Sense, Stroke, StrokeKind,
    TextureHandle, TextureOptions, Vec2,
};

use crate::connections::{self, Connection};
use crate::model::Window;
use crate::wire::{Size, DEFAULT_CONTROL_PORT, DEFAULT_VIDEO_PORT};

const INK: Color32 = Color32::from_rgb(0x08, 0x13, 0x1F);
const PANEL: Color32 = Color32::from_rgb(0x0E, 0x1A, 0x29);
const CARD: Color32 = Color32::from_rgb(0x18, 0x23, 0x33);
const BLUE: Color32 = Color32::from_rgb(0x28, 0x69, 0xFF);
const TEXT: Color32 = Color32::from_rgb(0xF3, 0xF6, 0xFC);
const MUTED: Color32 = Color32::from_rgb(0xB8, 0xC6, 0xDF);
const LINE: Color32 = Color32::from_rgb(0x34, 0x43, 0x57);
const GREEN: Color32 = Color32::from_rgb(0x54, 0xE5, 0x77);

const RELEASES_URL: &str = "https://github.com/aydinmrnv/transom/releases";
/// Thumbnails are refreshed at the gallery rate, not the video rate.
const PREVIEW_INTERVAL: Duration = Duration::from_millis(250);
const SCAN_INTERVAL: Duration = Duration::from_secs(8);

/// What the dashboard asks the application to do.
pub enum Action {
    Connect(Connection),
    Disconnect,
    OpenWindow(u64),
    HideWindow(u64),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Windows,
    Macs,
    Settings,
}

pub struct Card {
    pub window: Window,
    pub opened: bool,
    /// RGBA8 thumbnail, `size.w * size.h * 4` bytes. Empty until a frame arrives.
    pub pixels: Vec<u8>,
    pub size: Size,
    texture: Option<TextureHandle>,
    dirty: bool,
}

impl Card {
    pub fn new(window: Window, opened: bool) -> Self {
        Self {
            window,
            opened,
            pixels: vec![],
            size: Size { w: 0, h: 0 },
            texture: None,
            dirty: false,
        }
    }

    /// Build a small selector thumbnail directly from decoded NV12. This keeps the
    /// expensive full-frame YUV→RGBA conversion off the video path; only the few
    /// pixels a thumbnail needs are converted, at the gallery rate.
    pub fn update_preview_nv12(&mut self, nv12: &[u8], stride: usize, display: Size) {
        match thumbnail_nv12(nv12, stride, display, self.window.source, 480, 270) {
            Some((pixels, size)) => {
                self.pixels = pixels;
                self.size = size;
            }
            None => {
                self.pixels.clear();
                self.size = Size { w: 0, h: 0 };
            }
        }
        self.dirty = true;
    }
}

/// Crop `source` out of an NV12 display frame and shrink it to fit
/// `max_w` x `max_h` (never enlarging). `None` if the rect is empty or outside
/// the frame, or the buffer is too short.
pub fn thumbnail_nv12(
    nv12: &[u8],
    stride: usize,
    display: Size,
    r: crate::wire::Rect,
    max_w: u32,
    max_h: u32,
) -> Option<(Vec<u8>, Size)> {
    let needed = stride.saturating_mul(display.h as usize * 3 / 2);
    if r.w == 0
        || r.h == 0
        || r.x.saturating_add(r.w) > display.w
        || r.y.saturating_add(r.h) > display.h
        || display.w % 2 != 0
        || display.h % 2 != 0
        || stride < display.w as usize
        || nv12.len() < needed
    {
        return None;
    }
    let ratio = (max_w as f64 / r.w as f64)
        .min(max_h as f64 / r.h as f64)
        .min(1.0);
    let w = (r.w as f64 * ratio).max(1.0) as u32;
    let h = (r.h as f64 * ratio).max(1.0) as u32;
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        let sy = r.y + (y as u64 * r.h as u64 / h as u64) as u32;
        for x in 0..w {
            let sx = r.x + (x as u64 * r.w as u64 / w as u64) as u32;
            let dst = ((y * w + x) * 4) as usize;
            pixels[dst..dst + 4].copy_from_slice(&nv12_to_rgba_pixel(
                nv12, stride, display.h, sx, sy,
            ));
        }
    }
    Some((pixels, Size { w, h }))
}

/// BT.709 limited range, the coefficients the proxy shader uses.
fn nv12_to_rgba_pixel(nv12: &[u8], stride: usize, height: u32, x: u32, y: u32) -> [u8; 4] {
    let y_offset = y as usize * stride + x as usize;
    let uv_offset = stride * height as usize + (y as usize / 2) * stride + (x as usize / 2) * 2;
    let yi = i32::from(nv12[y_offset]) - 16;
    let u = i32::from(nv12[uv_offset]) - 128;
    let v = i32::from(nv12[uv_offset + 1]) - 128;
    [
        ((298 * yi + 459 * v + 128) >> 8).clamp(0, 255) as u8,
        ((298 * yi - 55 * u - 136 * v + 128) >> 8).clamp(0, 255) as u8,
        ((298 * yi + 541 * u + 128) >> 8).clamp(0, 255) as u8,
        255,
    ]
}

pub struct Dashboard {
    saved: Vec<Connection>,
    nearby: Vec<Connection>,
    rows: Vec<Connection>,
    selected_device: usize,
    preferences: PathBuf,
    scan: Option<mpsc::Receiver<io::Result<Vec<Connection>>>>,
    next_scan: Instant,
    active: bool,
    connected: bool,
    status: String,
    discovery: String,
    cards: Vec<Card>,
    query: String,
    view: View,
    list: bool,
    sort_name: bool,
    last_preview: Instant,
    host_field: String,
    control_field: String,
    video_field: String,
    focus_host: bool,
    actions: VecDeque<Action>,
    themed: bool,
}

/// Where saved connections live: `$XDG_CONFIG_HOME/transom/connections.json`.
pub fn preferences_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("transom").join("connections.json")
}

impl Dashboard {
    pub fn new() -> Self {
        let preferences = preferences_path();
        let (saved, status) = match connections::load(&preferences) {
            Ok(s) => (s, "Choose a Mac to get started.".to_string()),
            Err(e) => (vec![], format!("Could not load saved Macs: {e}")),
        };
        let mut d = Self {
            saved,
            nearby: vec![],
            rows: vec![],
            selected_device: 0,
            preferences,
            scan: None,
            next_scan: Instant::now(),
            active: false,
            connected: false,
            status,
            discovery: "Looking for nearby Macs…".into(),
            cards: vec![],
            query: String::new(),
            view: View::Windows,
            list: false,
            sort_name: true,
            last_preview: Instant::now() - Duration::from_secs(1),
            host_field: String::new(),
            control_field: DEFAULT_CONTROL_PORT.to_string(),
            video_field: DEFAULT_VIDEO_PORT.to_string(),
            focus_host: false,
            actions: VecDeque::new(),
            themed: false,
        };
        d.rebuild_rows();
        d
    }

    // --- state the application drives -------------------------------------

    /// Poll discovery; returns the next user action, if any.
    pub fn tick(&mut self) -> Option<Action> {
        let result = self.scan.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(result) = result {
            self.scan = None;
            match result {
                Ok(found) => {
                    self.nearby = found;
                    self.discovery = if self.nearby.is_empty() {
                        "No nearby Macs. Start sharing in Transom Host.".into()
                    } else {
                        let count = self.nearby.len();
                        format!(
                            "{} {} available nearby",
                            count,
                            if count == 1 { "Mac" } else { "Macs" }
                        )
                    };
                    self.rebuild_rows();
                }
                Err(e) => self.discovery = format!("Discovery unavailable: {e}"),
            }
            self.next_scan = Instant::now() + SCAN_INTERVAL;
        }
        if self.scan.is_none() && Instant::now() >= self.next_scan {
            self.start_scan();
        }
        self.actions.pop_front()
    }

    pub fn set_status(&mut self, text: &str, active: bool) {
        if self.status != text {
            self.status = text.into();
        }
        self.active = active;
    }

    pub fn set_connected(&mut self, connected: bool) {
        if connected && !self.connected {
            self.view = View::Windows;
        }
        self.connected = connected;
    }

    pub fn remember(&mut self, c: Connection) {
        connections::remember(&mut self.saved, c.clone());
        if let Err(e) = connections::save(&self.preferences, &self.saved) {
            self.discovery = format!("Connected, but could not save this Mac: {e}");
        }
        self.rebuild_rows();
        if let Some(i) = self.rows.iter().position(|row| row.same_destination(&c)) {
            self.selected_device = i;
        }
    }

    pub fn set_windows(&mut self, windows: Vec<(Window, bool)>) {
        let mut old = std::mem::take(&mut self.cards);
        self.cards = windows
            .into_iter()
            .map(|(w, opened)| {
                if let Some(i) = old.iter().position(|c| c.window.id == w.id) {
                    let mut card = old.remove(i);
                    if card.window.source != w.source {
                        card.pixels.clear();
                        card.size = Size { w: 0, h: 0 };
                        card.dirty = true;
                    }
                    card.window = w;
                    card.opened = opened;
                    card
                } else {
                    Card::new(w, opened)
                }
            })
            .collect();
    }

    pub fn update_previews_nv12(&mut self, nv12: &[u8], stride: usize, display: Size) {
        if self.last_preview.elapsed() < PREVIEW_INTERVAL {
            return;
        }
        self.last_preview = Instant::now();
        for c in &mut self.cards {
            c.update_preview_nv12(nv12, stride, display);
        }
    }

    /// Whether anything on screen is animating or arriving on its own, so the
    /// dashboard should keep redrawing.
    pub fn wants_repaint(&self) -> bool {
        self.scan.is_some() || self.connected
    }

    fn selected(&self) -> Option<Connection> {
        self.rows.get(self.selected_device).cloned()
    }

    fn start_scan(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.scan = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(crate::discovery::scan());
        });
    }

    fn rebuild_rows(&mut self) {
        let selected = self.selected();
        self.rows = connections::available(&self.nearby, &self.saved);
        self.selected_device = selected
            .and_then(|old| self.rows.iter().position(|c| c.same_destination(&old)))
            .unwrap_or(0);
    }

    /// Indices of the cards matching the search, in display order.
    fn matches(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        let mut entries: Vec<_> = self
            .cards
            .iter()
            .enumerate()
            .filter(|(_, c)| c.window.title.to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect();
        if self.sort_name {
            entries.sort_by_key(|&i| self.cards[i].window.title.to_lowercase());
        }
        entries
    }

    // --- UI -------------------------------------------------------------------

    pub fn ui(&mut self, root: &mut egui::Ui) {
        if !self.themed {
            self.themed = true;
            apply_theme(root.ctx());
        }
        egui::Panel::left("nav")
            .exact_size(228.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(14.0))
            .show(root, |ui| self.nav(ui));
        egui::Panel::bottom("status")
            .frame(egui::Frame::new().fill(PANEL).inner_margin(egui::Margin::symmetric(16, 8)))
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&self.status).size(12.0).color(MUTED));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!(
                                "Disconnect: {}",
                                super::input::DISCONNECT_SHORTCUT
                            ))
                            .size(12.0)
                            .color(MUTED),
                        );
                    });
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(INK).inner_margin(24.0))
            .show(root, |ui| {
                self.header(ui);
                ui.add_space(14.0);
                match self.view {
                    View::Windows => self.windows_view(ui),
                    View::Macs => self.macs_view(ui),
                    View::Settings => self.settings_view(ui),
                }
            });
    }

    fn nav(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.label(RichText::new("Transom").size(24.0).strong().color(TEXT));
        ui.add_space(24.0);
        for (view, label) in [(View::Windows, "Windows"), (View::Macs, "Macs")] {
            if nav_button(ui, label, self.view == view).clicked() {
                self.view = view;
            }
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add_space(8.0);
            if nav_button(ui, "Settings", self.view == View::Settings).clicked() {
                self.view = View::Settings;
            }
        });
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        let selected = self.selected();
        let name = selected
            .as_ref()
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "No Mac selected".into());
        let nearby = selected
            .as_ref()
            .is_some_and(|c| self.nearby.iter().any(|n| n.same_destination(c)));
        let label = if self.connected {
            "Connected"
        } else if self.active {
            "Connecting…"
        } else if nearby {
            "Available nearby"
        } else if selected.is_some() {
            "Saved Mac"
        } else {
            "Select a device in Macs"
        };
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(name).size(26.0).strong().color(TEXT));
                ui.horizontal(|ui| {
                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                    ui.painter().circle_filled(
                        dot.center(),
                        4.0,
                        if self.connected { GREEN } else { MUTED },
                    );
                    ui.label(RichText::new(label).size(13.0).color(MUTED));
                });
                ui.label(
                    RichText::new(selected.as_ref().map(|c| c.host.as_str()).unwrap_or(""))
                        .size(12.0)
                        .color(MUTED),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let enabled = self.active || !self.rows.is_empty();
                let text = if self.active { "Disconnect" } else { "Connect" };
                let button = egui::Button::new(RichText::new(text).size(15.0).color(TEXT))
                    .fill(BLUE)
                    .min_size(Vec2::new(150.0, 42.0))
                    .corner_radius(8.0);
                if ui.add_enabled(enabled, button).clicked() {
                    if self.active {
                        self.actions.push_back(Action::Disconnect);
                    } else if let Some(c) = self.selected() {
                        self.actions.push_back(Action::Connect(c));
                    }
                }
            });
        });
    }

    fn windows_view(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Shared windows").size(22.0).strong().color(TEXT));
        let detail = if self.connected {
            format!(
                "{} windows · {} open on this PC",
                self.cards.len(),
                self.cards.iter().filter(|c| c.opened).count()
            )
        } else {
            "Connect to your Mac to browse its shared windows.".into()
        };
        ui.label(RichText::new(detail).size(13.0).color(MUTED));
        ui.add_space(10.0);

        let mut submit = false;
        ui.horizontal(|ui| {
            let search = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text("Search windows")
                    .desired_width(320.0)
                    .margin(egui::Margin::symmetric(10, 8)),
            );
            if search.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submit = true;
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.selectable_label(self.list, "List").clicked() {
                    self.list = true;
                }
                if ui.selectable_label(!self.list, "Grid").clicked() {
                    self.list = false;
                }
                egui::ComboBox::from_id_salt("sort")
                    .selected_text(if self.sort_name {
                        "Sort: Name"
                    } else {
                        "Sort: Host order"
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.sort_name, true, "Name");
                        ui.selectable_value(&mut self.sort_name, false, "Host order");
                    });
            });
        });
        ui.add_space(12.0);

        let matches = self.matches();
        // Enter in search opens its only result; it never disconnects.
        if submit && matches.len() == 1 {
            self.actions
                .push_back(Action::OpenWindow(self.cards[matches[0]].window.id));
        }
        if matches.is_empty() {
            let (title, detail) = if !self.query.is_empty() {
                ("No matching windows", "Try another window title.")
            } else if self.connected {
                (
                    "No shared windows",
                    "On your Mac, select apps with open windows in Transom Host.",
                )
            } else {
                (
                    "Connect to a Mac",
                    "Choose a nearby or saved device in Macs, then connect.",
                )
            };
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(title).size(20.0).strong().color(TEXT));
                ui.label(RichText::new(detail).size(14.0).color(MUTED));
            });
            return;
        }

        let ctx = ui.ctx().clone();
        let mut open = None;
        let mut hide = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.list {
                    for &i in &matches {
                        let card = &mut self.cards[i];
                        let (w, h) = (ui.available_width(), 76.0);
                        card_ui(ui, &ctx, card, Vec2::new(w, h), true, &mut open, &mut hide);
                        ui.add_space(8.0);
                    }
                } else {
                    let gap = 16.0;
                    let cols = (((ui.available_width() + gap) / (260.0 + gap)).floor() as usize)
                        .clamp(1, 6);
                    let width = (ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32;
                    for row in matches.chunks(cols) {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = gap;
                            for &i in row {
                                let card = &mut self.cards[i];
                                let size = Vec2::new(width, width * 0.72 + 4.0);
                                card_ui(ui, &ctx, card, size, false, &mut open, &mut hide);
                            }
                        });
                        ui.add_space(gap);
                    }
                }
            });
        if let Some(id) = open {
            self.actions.push_back(Action::OpenWindow(id));
        }
        if let Some(id) = hide {
            self.actions.push_back(Action::HideWindow(id));
        }
    }

    fn macs_view(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Macs").size(22.0).strong().color(TEXT));
        ui.label(
            RichText::new(if self.active {
                "Disconnect before selecting another Mac."
            } else {
                "Select a Mac, then connect. Nearby Macs appear automatically."
            })
            .size(13.0)
            .color(MUTED),
        );
        ui.label(
            RichText::new(if self.scan.is_some() {
                "Looking for nearby Macs…"
            } else {
                self.discovery.as_str()
            })
            .size(12.0)
            .color(MUTED),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.active, egui::Button::new("Add Mac"))
                .clicked()
            {
                self.view = View::Settings;
                self.focus_host = true;
            }
            if ui
                .add_enabled(self.scan.is_none(), egui::Button::new("Refresh Macs"))
                .clicked()
            {
                self.start_scan();
            }
            let can_forget = self
                .selected()
                .is_some_and(|c| self.saved.iter().any(|n| n.same_destination(&c)));
            if ui
                .add_enabled(
                    can_forget && !self.active,
                    egui::Button::new("Forget saved Mac"),
                )
                .clicked()
            {
                self.forget_selected();
            }
        });
        ui.add_space(10.0);

        if self.rows.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No Macs found").size(20.0).strong().color(TEXT));
                ui.label(
                    RichText::new("Start Transom Host on your Mac, or add its hostname manually.")
                        .size(14.0)
                        .color(MUTED),
                );
            });
            return;
        }
        let mut chosen = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (i, c) in self.rows.iter().enumerate() {
                    let selected = i == self.selected_device;
                    let status = if selected && self.connected {
                        "Connected"
                    } else if selected && self.active {
                        "Connecting…"
                    } else if selected {
                        "Selected"
                    } else if self.nearby.iter().any(|n| n.same_destination(c)) {
                        "Nearby"
                    } else {
                        "Saved"
                    };
                    let (rect, resp) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), 72.0),
                        if self.active { Sense::hover() } else { Sense::click() },
                    );
                    let hovered = resp.hovered() && !self.active;
                    ui.painter().rect(
                        rect,
                        CornerRadius::same(10),
                        if selected {
                            Color32::from_rgb(0x20, 0x38, 0x58)
                        } else if hovered {
                            Color32::from_rgb(0x1E, 0x2B, 0x3E)
                        } else {
                            CARD
                        },
                        Stroke::new(1.0, if selected { Color32::from_rgb(0x55, 0x8F, 0xFF) } else { LINE }),
                        StrokeKind::Inside,
                    );
                    let p = ui.painter();
                    p.text(
                        rect.left_top() + Vec2::new(20.0, 16.0),
                        Align2::LEFT_TOP,
                        &c.name,
                        FontId::proportional(16.0),
                        TEXT,
                    );
                    p.text(
                        rect.left_top() + Vec2::new(20.0, 42.0),
                        Align2::LEFT_TOP,
                        format!(
                            "{}  ·  {} / {}",
                            c.host,
                            c.control_port,
                            c.video_port
                                .map(|n| n.to_string())
                                .unwrap_or_else(|| "no video".into())
                        ),
                        FontId::proportional(12.0),
                        MUTED,
                    );
                    p.text(
                        rect.right_center() - Vec2::new(20.0, 0.0),
                        Align2::RIGHT_CENTER,
                        status,
                        FontId::proportional(13.0),
                        if status == "Connected" { GREEN } else { MUTED },
                    );
                    if resp.clicked() && !self.active {
                        chosen = Some(i);
                    }
                    ui.add_space(8.0);
                }
            });
        if let Some(i) = chosen {
            self.selected_device = i;
        }
    }

    fn forget_selected(&mut self) {
        let Some(c) = self.selected() else { return };
        self.saved.retain(|n| !n.same_destination(&c));
        self.status = match connections::save(&self.preferences, &self.saved) {
            Ok(()) => "Saved connection removed. Nearby Macs remain discoverable.".into(),
            Err(e) => format!("Could not save connections: {e}"),
        };
        self.rebuild_rows();
    }

    fn settings_view(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Settings").size(22.0).strong().color(TEXT));
        ui.add_space(14.0);
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(480.0);
                ui.label(RichText::new("Manual connection").size(16.0).strong().color(TEXT));
                ui.label(
                    RichText::new("Use this when your Mac does not appear in the nearby list.")
                        .size(13.0)
                        .color(MUTED),
                );
                ui.add_space(10.0);
                ui.label(RichText::new("Mac hostname or IP address").size(13.0).color(MUTED));
                let host = ui.add(
                    egui::TextEdit::singleline(&mut self.host_field)
                        .hint_text("Mac-Studio.local")
                        .desired_width(480.0)
                        .margin(egui::Margin::symmetric(10, 8)),
                );
                if std::mem::take(&mut self.focus_host) {
                    host.request_focus();
                }
                ui.add_space(8.0);
                let mut enter = host.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Control port").size(13.0).color(MUTED));
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.control_field)
                                .desired_width(210.0)
                                .margin(egui::Margin::symmetric(10, 8)),
                        );
                        enter |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    });
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Video port").size(13.0).color(MUTED));
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.video_field)
                                .desired_width(250.0)
                                .margin(egui::Margin::symmetric(10, 8)),
                        );
                        enter |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    });
                });
                ui.add_space(12.0);
                let go = ui
                    .add_enabled(
                        !self.active,
                        egui::Button::new(RichText::new("Connect").size(15.0).color(TEXT))
                            .fill(BLUE)
                            .min_size(Vec2::new(210.0, 40.0))
                            .corner_radius(8.0),
                    )
                    .clicked();
                if (go || enter) && !self.active {
                    match Connection::manual(&self.host_field, &self.control_field, &self.video_field)
                    {
                        Ok(c) => self.actions.push_back(Action::Connect(c)),
                        Err(e) => self.status = e,
                    }
                }
                ui.add_space(6.0);
                ui.label(
                    RichText::new(if self.active {
                        "Disconnect to start a manual connection."
                    } else {
                        "Leave video blank for control only."
                    })
                    .size(13.0)
                    .color(MUTED),
                );
            });
            ui.add_space(40.0);
            ui.vertical(|ui| {
                ui.label(RichText::new("Updates").size(16.0).strong().color(TEXT));
                ui.label(
                    RichText::new(concat!("Transom ", env!("CARGO_PKG_VERSION")))
                        .size(13.0)
                        .color(MUTED),
                );
                ui.add_space(8.0);
                if ui.button("Open releases page").clicked() {
                    self.status = match open_url(RELEASES_URL) {
                        Ok(()) => "Opened the releases page in your browser.".into(),
                        Err(e) => e,
                    };
                }
            });
        });
    }

    /// Upload any changed thumbnails as egui textures.
    fn sync_textures(card: &mut Card, ctx: &egui::Context) {
        if !card.dirty {
            return;
        }
        card.dirty = false;
        if card.pixels.is_empty() {
            card.texture = None;
            return;
        }
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [card.size.w as usize, card.size.h as usize],
            &card.pixels,
        );
        match &mut card.texture {
            Some(t) => t.set(image, TextureOptions::LINEAR),
            None => {
                card.texture = Some(ctx.load_texture(
                    format!("card-{}", card.window.id),
                    image,
                    TextureOptions::LINEAR,
                ))
            }
        }
    }
}

fn card_ui(
    ui: &mut egui::Ui,
    ctx: &egui::Context,
    card: &mut Card,
    size: Vec2,
    list: bool,
    open: &mut Option<u64>,
    hide: &mut Option<u64>,
) {
    Dashboard::sync_textures(card, ctx);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let focused = resp.hovered() || resp.has_focus();
    let id = card.window.id;
    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same(11),
        if focused { Color32::from_rgb(0x25, 0x3B, 0x60) } else { Color32::from_rgb(0x22, 0x2D, 0x3C) },
        Stroke::new(
            if focused { 2.0 } else { 0.8 },
            if focused { Color32::from_rgb(0x46, 0x85, 0xFF) } else { LINE },
        ),
        StrokeKind::Inside,
    );
    let area = if list {
        egui::Rect::from_min_size(rect.min + Vec2::new(12.0, 9.0), Vec2::new(92.0, size.y - 18.0))
    } else {
        egui::Rect::from_min_max(
            rect.min + Vec2::new(13.0, 12.0),
            rect.max - Vec2::new(13.0, 50.0),
        )
    };
    painter.rect_filled(area, CornerRadius::same(5), Color32::from_rgb(0x08, 0x0F, 0x19));
    if let Some(tex) = &card.texture {
        let (sw, sh) = (card.size.w as f32, card.size.h as f32);
        let k = (area.width() / sw).min(area.height() / sh);
        let fitted = egui::Rect::from_center_size(area.center(), Vec2::new(sw * k, sh * k));
        painter.image(
            tex.id(),
            fitted,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
    let title = if card.window.title.trim().is_empty() {
        "Untitled window"
    } else {
        card.window.title.as_str()
    };
    if list {
        painter.text(
            rect.min + Vec2::new(122.0, 14.0),
            Align2::LEFT_TOP,
            title,
            FontId::proportional(14.0),
            TEXT,
        );
        painter.text(
            rect.min + Vec2::new(122.0, 42.0),
            Align2::LEFT_TOP,
            if card.opened { "Open on this PC" } else { "Available to open" },
            FontId::proportional(12.0),
            MUTED,
        );
    } else {
        painter.text(
            egui::pos2(rect.min.x + 16.0, rect.max.y - 24.0),
            Align2::LEFT_CENTER,
            title,
            FontId::proportional(14.0),
            TEXT,
        );
    }
    if card.opened {
        painter.circle_filled(rect.right_top() + Vec2::new(-16.0, 18.0), 4.0, GREEN);
    }

    // The options button sits over the card; its click must not open the window.
    let menu_rect = egui::Rect::from_center_size(
        if list {
            egui::pos2(rect.max.x - 36.0, rect.center().y)
        } else {
            egui::pos2(rect.max.x - 28.0, rect.max.y - 24.0)
        },
        Vec2::splat(28.0),
    );
    let menu = ui.interact(menu_rect, resp.id.with("menu"), Sense::click());
    let dot_color = if menu.hovered() { TEXT } else { MUTED };
    for dy in [-6.0, 0.0, 6.0] {
        ui.painter()
            .circle_filled(menu_rect.center() + Vec2::new(0.0, dy), 1.8, dot_color);
    }
    let opened = card.opened;
    egui::Popup::menu(&menu).show(|ui| {
        if ui.button(if opened { "Show window" } else { "Open window" }).clicked() {
            *open = Some(id);
        }
        if opened && ui.button("Hide window").clicked() {
            *hide = Some(id);
        }
    });
    if resp.clicked() && !menu.hovered() && !menu.clicked() {
        *open = Some(id);
    }
}

fn nav_button(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    let button = egui::Button::new(RichText::new(label).size(15.0).color(if selected {
        TEXT
    } else {
        MUTED
    }))
    .fill(if selected { Color32::from_rgb(0x1B, 0x2E, 0x4B) } else { Color32::TRANSPARENT })
    .min_size(Vec2::new(ui.available_width(), 46.0))
    .corner_radius(8.0);
    ui.add(button)
}

fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = INK;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = Color32::from_rgb(0x11, 0x1D, 0x2C);
    visuals.selection.bg_fill = BLUE;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    visuals.override_text_color = None;
    ctx.set_visuals(visuals);
    install_system_fonts(ctx);
}

/// egui ships Latin glyphs only. Mac window titles are often CJK, so add the
/// system's CJK face (as found by fontconfig) as a fallback.
fn install_system_fonts(ctx: &egui::Context) {
    let Some((data, index)) = system_cjk_font() else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    let mut font = egui::FontData::from_owned(data);
    font.index = index;
    fonts.font_data.insert("system-cjk".into(), std::sync::Arc::new(font));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("system-cjk".into());
    }
    ctx.set_fonts(fonts);
}

fn system_cjk_font() -> Option<(Vec<u8>, u32)> {
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{file}\n%{index}", "sans-serif:lang=zh-tw"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8(out.stdout).ok()?;
    let mut lines = text.lines();
    let path = lines.next()?;
    // fontconfig packs a variable font's named instance into the high 16 bits;
    // the face index inside a collection is the low 16.
    let index = lines
        .next()
        .and_then(|n| n.trim().parse::<u32>().ok())
        .map_or(0, |n| n & 0xFFFF);
    Some((std::fs::read(path).ok()?, index))
}

fn open_url(url: &str) -> Result<(), String> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Could not open {url}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Window;
    use crate::wire::{Rect, WindowKind};

    fn window(id: u64, title: &str, source: Rect) -> Window {
        Window {
            id,
            title: title.into(),
            kind: WindowKind::Normal,
            source,
        }
    }
    const R: Rect = Rect { x: 0, y: 0, w: 2, h: 2 };

    #[test]
    fn thumbnail_crops_only_its_window_and_rejects_outside_rects() {
        // 4x2 display, NV12 (stride 4): Y row 0 = [16 235 16 235], row 1 the same.
        let nv12 = [16, 235, 16, 235, 16, 235, 16, 235, 128, 128, 128, 128];
        let display = Size { w: 4, h: 2 };
        let (px, size) = thumbnail_nv12(
            &nv12,
            4,
            display,
            Rect { x: 1, y: 0, w: 1, h: 2 },
            480,
            270,
        )
        .unwrap();
        assert_eq!(size, Size { w: 1, h: 2 });
        // Column 1 is full white (Y=235, neutral chroma).
        assert_eq!(px, vec![255, 255, 255, 255, 255, 255, 255, 255]);
        let outside = Rect { x: u32::MAX, y: 0, w: 1, h: 1 };
        assert!(thumbnail_nv12(&nv12, 4, display, outside, 480, 270).is_none());
        assert!(thumbnail_nv12(&nv12[..5], 4, display, R, 480, 270).is_none());
    }

    #[test]
    fn thumbnails_never_enlarge_and_keep_aspect() {
        let (w, h) = (1920u32, 1080u32);
        let nv12 = vec![128u8; (w * h * 3 / 2) as usize];
        let (_, size) = thumbnail_nv12(
            &nv12,
            w as usize,
            Size { w, h },
            Rect { x: 0, y: 0, w, h },
            480,
            270,
        )
        .unwrap();
        assert_eq!(size, Size { w: 480, h: 270 });
    }

    #[test]
    fn search_matches_case_insensitively_and_sorts_by_name() {
        let mut d = Dashboard::new();
        d.set_windows(vec![
            (window(1, "Xcode", R), false),
            (window(2, "Safari", R), true),
            (window(3, "Notes", R), false),
        ]);
        d.sort_name = true;
        assert_eq!(d.matches(), vec![2, 1, 0]);
        d.query = "SAF".into();
        assert_eq!(d.matches(), vec![1]);
        d.query.clear();
        d.sort_name = false;
        assert_eq!(d.matches(), vec![0, 1, 2]);
    }

    #[test]
    fn set_windows_keeps_a_card_preview_until_its_rect_changes() {
        let mut d = Dashboard::new();
        d.set_windows(vec![(window(1, "a", R), false)]);
        d.cards[0].pixels = vec![1; 4];
        d.cards[0].size = Size { w: 1, h: 1 };
        d.set_windows(vec![(window(1, "renamed", R), true)]);
        assert_eq!(d.cards[0].pixels.len(), 4);
        assert!(d.cards[0].opened);
        let moved = Rect { x: 4, ..R };
        d.set_windows(vec![(window(1, "renamed", moved), true)]);
        assert!(d.cards[0].pixels.is_empty());
    }

    #[test]
    fn preferences_live_under_the_xdg_config_dir() {
        let path = preferences_path();
        assert!(path.ends_with("transom/connections.json"), "{path:?}");
    }
}
