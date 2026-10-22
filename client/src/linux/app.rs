//! The application: the winit event loop, the dashboard window, and the proxy
//! lifecycle driven by the control stream. The Linux/Wayland counterpart of
//! `win::app`.
//!
//! Threading model: everything UI-side runs on the winit thread. The `Session`'s
//! reader threads decode the protocol and post `SessionEvent`s onto an `mpsc`; a
//! short timer tick drains that channel and turns `ModelEvent`s into proxy
//! windows, exactly as the Windows pump does.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_wgpu::wgpu;
use egui_winit::winit::application::ApplicationHandler;
use egui_winit::winit::dpi::{LogicalSize, PhysicalSize};
use egui_winit::winit::event::{ElementState, MouseButton as WinitButton, MouseScrollDelta, WindowEvent};
use egui_winit::winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use egui_winit::winit::keyboard::{ModifiersState, PhysicalKey};
use egui_winit::winit::platform::wayland::WindowAttributesExtWayland;
use egui_winit::winit::window::{Window as WinitWindow, WindowId};

use super::dashboard::{Action, Dashboard};
use super::decode::DecoderWorker;
use super::gpu::{Gpu, SourceTexture};
use super::input;
use super::proxy::Proxy;
use super::resize::Request;
use crate::connections::Connection;
use crate::model::{ModelEvent, Window};
use crate::session::{Session, SessionEvent, VideoEvent};
use crate::wire::{ClientMessage, InputEvent, MouseButton, Rect, ResizePhase, Size};

type ConnectResult = Result<(Connection, Session, Receiver<SessionEvent>), String>;

/// The app id compositors group windows by (and match against a .desktop file).
const APP_ID: &str = "transom";

/// Reconnect backoff after the control channel drops.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// How often the event loop wakes to poll the session, decoder and dashboard.
const TICK: Duration = Duration::from_millis(8);

pub struct AppConfig {
    pub host: String,
    pub control_port: u16,
    pub video_port: Option<u16>,
    pub checkerboard: bool,
    /// Open every shared window as soon as it appears, without the dashboard.
    pub open_all: bool,
}

struct DashWindow {
    window: Arc<WinitWindow>,
    surface: wgpu::Surface<'static>,
    size: (u32, u32),
    ctx: egui::Context,
    state: egui_winit::State,
    next_repaint: Option<Instant>,
}

pub struct App {
    gpu: Gpu,
    dash_window: DashWindow,
    dashboard: Dashboard,
    cfg: AppConfig,
    selected: Option<Connection>,
    connecting: Option<Receiver<ConnectResult>>,
    active: bool,
    notice: Option<String>,
    video_notice: Option<String>,
    session: Option<Session>,
    rx: Option<Receiver<SessionEvent>>,
    proxies: HashMap<u64, Proxy>,
    windows: HashMap<u64, Window>,
    window_to_id: HashMap<WindowId, u64>,
    source: Option<SourceTexture>,
    decoder: Option<DecoderWorker>,
    vds: Option<Size>,
    modifiers: ModifiersState,
    /// Keys the host believes are down, per proxy, so focus loss can release them
    /// (Wayland never delivers the key-up of a key released while unfocused).
    held_keys: HashMap<u64, HashSet<u32>>,
    /// Latest unsent hover move per proxy; one per tick preserves the newest
    /// position without a synchronous TCP write for every raw pointer event.
    pending_mouse_moves: HashMap<u64, InputEvent>,
    reconnect_at: Option<Instant>,
    now_epoch: Instant,
    /// Video-health counters: if access units keep arriving but none ever decode,
    /// warn once instead of silently showing the placeholder forever.
    video_in: u64,
    video_decoded: u64,
    warned_no_decode: bool,
    frame_dirty: bool,
    dump_frames: u32,
}

pub fn run(cfg: Option<AppConfig>) -> Result<(), String> {
    let event_loop = EventLoop::new().map_err(|e| format!("Could not connect to Wayland: {e}"))?;
    event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + TICK));

    // The dashboard window comes first: the GPU device is chosen against its
    // surface so the adapter is guaranteed able to present to this compositor.
    #[allow(deprecated)]
    let window = event_loop
        .create_window(
            WinitWindow::default_attributes()
                .with_title("Transom")
                .with_name(APP_ID, "transom-dashboard")
                .with_inner_size(LogicalSize::new(1280.0, 820.0))
                .with_min_inner_size(LogicalSize::new(900.0, 620.0)),
        )
        .map_err(|e| format!("Could not open Transom: {e}"))?;
    let window = Arc::new(window);

    let wgpu_config = egui_wgpu::WgpuConfiguration {
        wgpu_setup: egui_wgpu::WgpuSetup::CreateNew(
            egui_wgpu::WgpuSetupCreateNew::from_display_handle(event_loop.owned_display_handle()),
        ),
        ..Default::default()
    };
    let instance = pollster::block_on(wgpu_config.wgpu_setup.new_instance());
    let surface = instance
        .create_surface(window.clone())
        .map_err(|e| format!("Could not create a surface: {e}"))?;
    let gpu = pollster::block_on(Gpu::new(&instance, &wgpu_config, &surface))
        .map_err(|e| format!("Could not initialize graphics: {e}"))?;
    let info = gpu.state.adapter.get_info();
    eprintln!("gpu: {} ({:?}), surface {:?}", info.name, info.backend, gpu.format);

    let size = window.inner_size();
    gpu.configure(&surface, size.width, size.height);
    let ctx = egui::Context::default();
    let state = egui_winit::State::new(
        ctx.clone(),
        egui::ViewportId::ROOT,
        &*window,
        Some(window.scale_factor() as f32),
        window.theme(),
        Some(gpu.max_texture_dimension() as usize),
    );

    let mut app = App::new(
        gpu,
        DashWindow {
            window,
            surface,
            size: (size.width, size.height),
            ctx,
            state,
            next_repaint: Some(Instant::now()),
        },
        cfg,
    );
    app.connect();
    event_loop
        .run_app(&mut app)
        .map_err(|e| format!("Event loop failed: {e}"))
}

impl App {
    fn new(gpu: Gpu, dash_window: DashWindow, cfg: Option<AppConfig>) -> App {
        let active = cfg.is_some();
        let cfg = cfg.unwrap_or(AppConfig {
            host: String::new(),
            control_port: crate::wire::DEFAULT_CONTROL_PORT,
            video_port: Some(crate::wire::DEFAULT_VIDEO_PORT),
            checkerboard: false,
            open_all: false,
        });
        let selected = if active {
            Connection::manual(
                &cfg.host,
                &cfg.control_port.to_string(),
                &cfg.video_port.map(|p| p.to_string()).unwrap_or_default(),
            )
            .ok()
        } else {
            None
        };
        App {
            gpu,
            dash_window,
            dashboard: Dashboard::new(),
            cfg,
            selected,
            connecting: None,
            active,
            notice: None,
            video_notice: None,
            session: None,
            rx: None,
            proxies: HashMap::new(),
            windows: HashMap::new(),
            window_to_id: HashMap::new(),
            source: None,
            decoder: None,
            vds: None,
            modifiers: ModifiersState::empty(),
            held_keys: HashMap::new(),
            pending_mouse_moves: HashMap::new(),
            reconnect_at: None,
            now_epoch: Instant::now(),
            video_in: 0,
            video_decoded: 0,
            warned_no_decode: false,
            frame_dirty: false,
            dump_frames: 0,
        }
    }

    /// Milliseconds since the app started — the client's monotonic `ts` clock
    /// (protocol.md §4; opaque to the host).
    fn now_ms(&self) -> u64 {
        self.now_epoch.elapsed().as_millis() as u64
    }

    /// Attempt to (re)connect the session. Failures are retried on the backoff; a
    /// live host is not required for the window manager to be up.
    fn connect(&mut self) {
        if !self.active || self.connecting.is_some() {
            return;
        }
        let Some(mut connection) = self.selected.clone() else {
            return;
        };
        self.dashboard
            .set_status(&format!("Connecting to {}…", connection.name), true);
        let (tx, rx) = mpsc::channel();
        self.connecting = Some(rx);
        std::thread::spawn(move || {
            let result = (|| {
                // Rediscover saved Bonjour identities before using an endpoint:
                // a DHCP lease may now belong to a different machine.
                if !connection.id.starts_with("manual:") {
                    connection = crate::discovery::scan()
                        .map_err(|e| e.to_string())?
                        .into_iter()
                        .find(|c| c.id == connection.id)
                        .ok_or(
                            "Mac is offline or not sharing. Open Transom Host and press Start.",
                        )?;
                }
                let (session, events) = Session::connect(
                    &connection.host,
                    connection.control_port,
                    connection.video_port,
                )
                .map_err(|e| e.to_string())?;
                Ok((connection, session, events))
            })();
            // If the user cancelled, dropping the failed delivery closes sockets.
            let _ = tx.send(result);
        });
    }

    // --- dashboard actions --------------------------------------------------

    fn poll_dashboard(&mut self, el: &ActiveEventLoop) {
        match self.dashboard.tick() {
            Some(Action::Connect(c)) => {
                self.disconnect();
                self.cfg.host = c.host.clone();
                self.cfg.control_port = c.control_port;
                self.cfg.video_port = c.video_port;
                self.selected = Some(c);
                self.active = true;
                self.connect();
            }
            Some(Action::Disconnect) => self.disconnect_to_dashboard(),
            Some(Action::OpenWindow(id)) => {
                if let Some(proxy) = self.proxies.get(&id) {
                    proxy.window.set_minimized(false);
                    proxy.window.focus_window();
                } else if let Some(w) = self.windows.get(&id).cloned() {
                    if let Err(e) = self.create_proxy(el, id, w.source, &w.title) {
                        self.notice = Some(format!("Could not open window: {e}"));
                    }
                }
                self.refresh_gallery();
                self.update_status();
            }
            Some(Action::HideWindow(id)) => {
                self.destroy_proxy(id);
                self.refresh_gallery();
                self.update_status();
            }
            None => {}
        }
        let result = self.connecting.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(result) = result {
            self.connecting = None;
            match result {
                Ok((c, session, rx)) => {
                    self.cfg.host = c.host.clone();
                    self.cfg.control_port = c.control_port;
                    self.cfg.video_port = c.video_port;
                    self.selected = Some(c.clone());
                    self.session = Some(session);
                    self.rx = Some(rx);
                    self.reconnect_at = None;
                    self.notice = None;
                    self.dashboard.set_connected(true);
                    self.dashboard.remember(c);
                    self.update_status();
                }
                Err(e) => {
                    self.dashboard
                        .set_status(&format!("Could not connect: {e} Retrying…"), true);
                    self.reconnect_at = Some(Instant::now() + RECONNECT_DELAY);
                }
            }
            self.request_dashboard_redraw();
        }
    }

    fn update_status(&mut self) {
        if self.selected.is_some() {
            let state = if let Some(n) = &self.notice {
                n.clone()
            } else if let Some(n) = &self.video_notice {
                n.clone()
            } else if self.cfg.video_port.is_none() {
                "Control only · video is off".into()
            } else if self.video_decoded > 0 {
                "Streaming".into()
            } else {
                "Waiting for video…".into()
            };
            self.dashboard.set_status(&state, true);
            self.request_dashboard_redraw();
        }
    }

    fn request_dashboard_redraw(&mut self) {
        self.dash_window.next_repaint = Some(Instant::now());
    }

    fn clear_session(&mut self) {
        self.dashboard.set_connected(false);
        if let Some(s) = self.session.take() {
            s.shutdown();
        }
        self.rx = None;
        self.decoder = None;
        self.source = None;
        self.vds = None;
        self.video_in = 0;
        self.video_decoded = 0;
        self.warned_no_decode = false;
        self.notice = None;
        self.video_notice = None;
        let ids: Vec<_> = self.proxies.keys().copied().collect();
        for id in ids {
            self.destroy_proxy(id);
        }
        self.windows.clear();
        self.refresh_gallery();
        self.pending_mouse_moves.clear();
        self.held_keys.clear();
    }

    fn disconnect(&mut self) {
        self.active = false;
        self.connecting = None;
        self.reconnect_at = None;
        self.clear_session();
    }

    fn disconnect_to_dashboard(&mut self) {
        self.disconnect();
        self.dashboard
            .set_status("Disconnected. Your Mac apps are still open.", false);
        self.dash_window.window.set_minimized(false);
        self.dash_window.window.focus_window();
        self.request_dashboard_redraw();
    }

    /// Send a message to the host, if connected.
    fn send(&self, msg: &ClientMessage) {
        if let Some(s) = &self.session {
            if let Err(e) = s.send(msg) {
                eprintln!("send failed: {e}");
            }
        }
    }

    // --- session event handling ---------------------------------------------

    fn drain_session(&mut self, el: &ActiveEventLoop) {
        if !self.active {
            return;
        }
        // Reconnect if it's time; network work never blocks the UI thread.
        if self.session.is_none() {
            if self
                .reconnect_at
                .map(|t| Instant::now() >= t)
                .unwrap_or(true)
            {
                self.connect();
            }
            return;
        }

        // Move the receiver out to avoid borrowing self while we mutate it.
        let Some(rx) = self.rx.take() else { return };
        let mut disconnected = false;
        for _ in 0..64 {
            match rx.try_recv() {
                Ok(SessionEvent::Control(ev)) => self.apply_model_event(el, ev),
                Ok(SessionEvent::Video(v)) => self.apply_video(v),
                Ok(SessionEvent::ControlClosed(reason)) => {
                    self.dashboard.set_status(
                        &format!("Connection lost{}. Retrying…", suffix(reason)),
                        true,
                    );
                    disconnected = true;
                    break;
                }
                Ok(SessionEvent::VideoClosed(reason)) => {
                    self.dashboard.set_status(
                        &format!("Video unavailable{}. Retrying…", suffix(reason)),
                        true,
                    );
                    // A video channel can die independently while control stays
                    // open. Reconnect the whole session so the host sends a fresh
                    // hvcC config and the decoder can recover without restarting.
                    disconnected = true;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            self.clear_session();
            self.reconnect_at = Some(Instant::now() + RECONNECT_DELAY);
            self.request_dashboard_redraw();
        } else {
            self.rx = Some(rx);
        }
    }

    fn apply_model_event(&mut self, el: &ActiveEventLoop, ev: ModelEvent) {
        match ev {
            ModelEvent::Connected { vds } => {
                self.vds = Some(vds);
                self.ensure_source(vds);
            }
            ModelEvent::WindowAdded(w) => {
                if self.cfg.open_all && !self.proxies.contains_key(&w.id) {
                    if let Err(e) = self.create_proxy(el, w.id, w.source, &w.title) {
                        self.notice = Some(format!("Could not open window: {e}"));
                    }
                }
                self.windows.insert(w.id, w);
            }
            ModelEvent::WindowRectChanged { id, source, .. } => {
                if let Some(w) = self.windows.get_mut(&id) {
                    w.source = source;
                }
                self.update_source_rect(id, source);
            }
            ModelEvent::WindowTitleChanged { id, title } => {
                if let Some(w) = self.windows.get_mut(&id) {
                    w.title = title.clone();
                }
                self.update_title(id, &title);
            }
            ModelEvent::WindowFocused { .. } => {}
            ModelEvent::WindowRemoved { id } => {
                self.windows.remove(&id);
                self.destroy_proxy(id);
            }
            ModelEvent::Resynced { removed } => {
                for id in removed {
                    self.windows.remove(&id);
                    self.destroy_proxy(id);
                }
            }
            ModelEvent::HostError { code, message } => {
                self.notice = Some(format!("Host error {code}: {message}"));
            }
        }
        self.refresh_gallery();
        self.update_status();
    }

    fn refresh_gallery(&mut self) {
        let mut windows: Vec<_> = self.windows.values().cloned().collect();
        windows.sort_by_key(|w| w.id);
        self.dashboard.set_windows(
            windows
                .into_iter()
                .map(|w| {
                    let opened = self.proxies.contains_key(&w.id);
                    (w, opened)
                })
                .collect(),
        );
        self.request_dashboard_redraw();
    }

    fn apply_video(&mut self, v: VideoEvent) {
        match v {
            VideoEvent::Config { hvcc } => {
                if let Some(vds) = self.vds {
                    match DecoderWorker::start(hvcc, vds.w, vds.h) {
                        Ok(d) => self.decoder = Some(d),
                        Err(e) => {
                            self.video_notice = Some(format!("Cannot start video decoder: {e}"));
                            self.update_status();
                        }
                    }
                }
            }
            VideoEvent::Frame {
                data,
                keyframe,
                pts_micros,
                ..
            } => {
                self.video_in += 1;
                if self.video_in <= 3 {
                    eprintln!(
                        "video: access unit {} ({} bytes, keyframe={keyframe})",
                        self.video_in,
                        data.len()
                    );
                }
                if let Some(decoder) = self.decoder.as_ref() {
                    decoder.submit(data, keyframe, pts_micros);
                }
                // A newly connected client must wait for a complete keyframe.
                // Preserve any more specific error reported by the worker.
                if !self.warned_no_decode
                    && self.notice.is_none()
                    && self.video_notice.is_none()
                    && self.video_decoded == 0
                    && self.video_in >= 120
                {
                    self.warned_no_decode = true;
                    self.video_notice = Some(
                        "Video is arriving but no picture has decoded yet. Waiting for a complete keyframe…"
                            .into(),
                    );
                    self.update_status();
                    eprintln!(
                        "video: received {} access units; waiting for a decodable keyframe",
                        self.video_in
                    );
                }
            }
        }
    }

    /// Upload at most the newest completed decode. The decoder keeps NV12 intact;
    /// the shader performs the colour conversion while each proxy samples its crop.
    fn poll_decoder(&mut self) {
        let needs_keyframe = self
            .decoder
            .as_ref()
            .map(DecoderWorker::take_keyframe_request)
            .unwrap_or(false);
        if needs_keyframe {
            self.send(&ClientMessage::RequestKeyframe);
            self.video_notice = Some("Video fell behind; recovering…".into());
            self.update_status();
            eprintln!("video: dropped stale frames; requested a fresh keyframe");
        }
        if let Some(error) = self.decoder.as_ref().and_then(DecoderWorker::take_error) {
            self.video_notice = Some(error);
            self.update_status();
        }
        let frame = self.decoder.as_ref().and_then(DecoderWorker::take_frame);
        if let (Some(frame), Some(source)) = (frame, self.source.as_ref()) {
            source.update_nv12(&self.gpu, &frame.nv12, frame.stride);
            if let Some(vds) = self.vds {
                self.dashboard
                    .update_previews_nv12(&frame.nv12, frame.stride, vds);
            }
            self.frame_dirty = true;
            self.video_decoded += 1;
            let recovered = self.video_notice.take().is_some();
            if self.video_decoded == 1 {
                eprintln!("video: first decoded frame uploaded to the display texture");
            }
            if self.video_decoded == 1 || recovered {
                self.update_status();
            }
        }
    }

    fn ensure_source(&mut self, vds: Size) {
        let need_new = match &self.source {
            Some(s) => s.width != vds.w || s.height != vds.h,
            None => true,
        };
        if need_new {
            match SourceTexture::new(&self.gpu, vds.w, vds.h) {
                Ok(t) => self.source = Some(t),
                Err(e) => {
                    eprintln!("source texture creation failed: {e}");
                    self.video_notice = Some(format!("Cannot show video: {e}"));
                }
            }
        }
    }

    // --- proxy lifecycle -----------------------------------------------------

    fn create_proxy(
        &mut self,
        el: &ActiveEventLoop,
        id: u64,
        source: Rect,
        title: &str,
    ) -> Result<(), String> {
        // Fit an oversized Retina source to a visibly windowed target. The helper
        // preserves exact size whenever it already fits and preserves aspect ratio
        // when it does not; after creation we ask the host to relayout to this
        // physical size so the transient fit snaps back to 1:1. Wayland exposes no
        // work area, so the monitor's physical size stands in for it.
        let monitor = el
            .primary_monitor()
            .or_else(|| el.available_monitors().next())
            .map(|m| m.size())
            .filter(|s| s.width > 0 && s.height > 0)
            .unwrap_or(PhysicalSize::new(1920, 1080));
        let fitted = crate::window_fit::initial_proxy_size(
            Size {
                w: source.w,
                h: source.h,
            },
            Size {
                w: monitor.width,
                h: monitor.height,
            },
        );
        let clamped = fitted.w != source.w || fitted.h != source.h;

        let title_text = if title.trim().is_empty() {
            "Transom"
        } else {
            title
        };
        let window = el
            .create_window(
                WinitWindow::default_attributes()
                    .with_title(title_text)
                    .with_name(APP_ID, "transom-proxy")
                    .with_inner_size(PhysicalSize::new(fitted.w, fitted.h)),
            )
            .map_err(|e| e.to_string())?;
        let window = Arc::new(window);
        let window_id = window.id();
        let mut proxy = Proxy::new(&self.gpu, window, source, self.cfg.checkerboard)?;
        // The window may already be a different size from the one requested (the
        // compositor decides); adopt it so the surface matches it exactly.
        proxy.desired = fitted;
        self.window_to_id.insert(window_id, id);
        self.proxies.insert(id, proxy);

        // If we had to shrink the window to fit the monitor, ask the host to resize
        // the Mac window to the fitted size. The host relayouts natively and reports
        // back the ACTUAL geometry (I-4), which snaps the surface to an exact 1:1
        // blit — the product's geometry-mirroring, applied at birth.
        if clamped {
            self.send(&ClientMessage::RequestResize {
                id,
                size: fitted,
                phase: ResizePhase::End,
            });
        }
        let scale = self
            .proxies
            .get(&id)
            .map(|p| p.window.scale_factor())
            .unwrap_or(1.0);
        println!(
            "window {id}: source {}x{} px -> {}x{} px proxy{} at {:.2}x scale",
            source.w,
            source.h,
            fitted.w,
            fitted.h,
            if clamped {
                " [clamped to monitor, requested host resize]"
            } else {
                ""
            },
            scale
        );
        Ok(())
    }

    fn destroy_proxy(&mut self, id: u64) {
        self.pending_mouse_moves.remove(&id);
        // Release anything the host thinks is held, so a closed window can't leave
        // a modifier or button stuck on the Mac.
        self.release_held(id);
        self.held_keys.remove(&id);
        if let Some(proxy) = self.proxies.remove(&id) {
            self.window_to_id.remove(&proxy.window.id());
        }
    }

    fn update_title(&mut self, id: u64, title: &str) {
        let Some(proxy) = self.proxies.get(&id) else {
            return;
        };
        proxy.window.set_title(if title.trim().is_empty() {
            "Transom"
        } else {
            title
        });
    }

    /// Host reported ACTUAL geometry for `id`. Update the source sub-rect, and if
    /// the size changed and we're not mid-drag, snap the window's client size to
    /// match so the blit is 1:1 (I-4: "asked 2560x1440, got 2560x1438").
    fn update_source_rect(&mut self, id: u64, source: Rect) {
        if let Some(proxy) = self.proxies.get_mut(&id) {
            proxy.set_source(&self.gpu, source);
            self.frame_dirty = true;
        }
    }

    fn render_proxy(&mut self, id: u64) {
        let source = self.source.as_ref();
        if let Some(proxy) = self.proxies.get_mut(&id) {
            proxy.render(&self.gpu, source);
        }
    }

    // --- window events -------------------------------------------------------

    fn proxy_event(&mut self, id: u64, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                // Close the local view, never the remote document. The card can
                // reopen it.
                self.destroy_proxy(id);
                self.refresh_gallery();
            }
            WindowEvent::Resized(size) => {
                if std::env::var_os("TRANSOM_DEBUG").is_some() {
                    eprintln!("debug: window {id} Resized {}x{} decorated={:?}", size.width, size.height,
                        self.proxies.get(&id).map(|p| p.window.is_decorated()));
                }
                let requests = match self.proxies.get_mut(&id) {
                    Some(proxy) => proxy.resized(&self.gpu, size.width, size.height, Instant::now()),
                    None => return,
                };
                for Request { phase, size } in requests {
                    self.send(&ClientMessage::RequestResize { id, size, phase });
                }
                self.render_proxy(id);
            }
            WindowEvent::ScaleFactorChanged {
                scale_factor,
                mut inner_size_writer,
            } => {
                // Keep the *physical* size fixed across a scale change (a monitor
                // switch, fractional-scale arrival). Winit would otherwise keep
                // the logical size, changing the pixel count and, with it, the
                // size we mirror to the Mac (I-2).
                if let Some(proxy) = self.proxies.get_mut(&id) {
                    let want = proxy.desired;
                    proxy.tracker.expect(want);
                    let _ = inner_size_writer.request_inner_size(PhysicalSize::new(want.w, want.h));
                    eprintln!("window {id}: scale factor {scale_factor:.3}, holding {}x{} px", want.w, want.h);
                }
            }
            WindowEvent::RedrawRequested => self.render_proxy(id),
            WindowEvent::Focused(true) => {
                // Becoming active: ask the host to raise the Mac window so focus
                // and key routing line up (protocol.md §4 focus/raise).
                self.send(&ClientMessage::RequestFocus { id });
            }
            WindowEvent::Focused(false) => {
                // Key and button releases will not arrive while unfocused.
                self.release_held(id);
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                let (x, y) = (position.x as i32, position.y as i32);
                let Some(proxy) = self.proxies.get_mut(&id) else {
                    return;
                };
                proxy.cursor = (x, y);
                if proxy.tracker.resizing {
                    return;
                }
                let content = Size {
                    w: proxy.source.w,
                    h: proxy.source.h,
                };
                if let Some(event) = proxy.pointer.motion(x, y, content) {
                    // Coalesce hover motion instead of a TCP write per event.
                    self.pending_mouse_moves.insert(id, event);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    WinitButton::Left => MouseButton::Left,
                    WinitButton::Right => MouseButton::Right,
                    WinitButton::Middle => MouseButton::Middle,
                    _ => return,
                };
                let Some(proxy) = self.proxies.get_mut(&id) else {
                    return;
                };
                if proxy.tracker.resizing {
                    return;
                }
                let content = Size {
                    w: proxy.source.w,
                    h: proxy.source.h,
                };
                let (x, y) = proxy.cursor;
                let event = match state {
                    ElementState::Pressed => proxy.pointer.press(button, x, y, content),
                    ElementState::Released => proxy.pointer.release(button, x, y, content),
                };
                if let Some(event) = event {
                    // This event already carries its authoritative coordinates,
                    // so an older queued hover can be discarded.
                    self.pending_mouse_moves.remove(&id);
                    self.send_input(id, event);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let Some(proxy) = self.proxies.get_mut(&id) else {
                    return;
                };
                if proxy.tracker.resizing {
                    return;
                }
                // Windows reports a positive horizontal delta for a wheel turned
                // right, where winit's positive x moves the *content* right, so
                // x is negated; y agrees in sign on both.
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => {
                        proxy.scroll.add_lines(-(x as f64), y as f64)
                    }
                    MouseScrollDelta::PixelDelta(p) => proxy.scroll.add_pixels(-p.x, p.y),
                };
                if dx == 0 && dy == 0 {
                    return;
                }
                let content = Size {
                    w: proxy.source.w,
                    h: proxy.source.h,
                };
                let (x, y) = proxy.cursor;
                if let Some(event) = proxy.pointer.scroll(x, y, dx, dy, content) {
                    self.pending_mouse_moves.remove(&id);
                    self.send_input(id, event);
                }
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                let pressed = event.state == ElementState::Pressed;
                // Consume the shortcut before it can reach the Mac. Socket
                // shutdown also releases the host's held modifier state.
                if pressed
                    && input::disconnect_chord(
                        code,
                        self.modifiers.control_key(),
                        self.modifiers.alt_key(),
                        self.modifiers.shift_key(),
                        self.modifiers.super_key(),
                    )
                {
                    self.disconnect_to_dashboard();
                    return;
                }
                let Some(vk) = input::vk_for_code(code) else {
                    return;
                };
                let held = self.held_keys.entry(id).or_default();
                if pressed {
                    // A press winit synthesised on focus gain was not made by the
                    // user in this window; do not forward it.
                    if is_synthetic {
                        return;
                    }
                    held.insert(vk);
                    self.send_input(id, InputEvent::KeyDown { vk });
                } else if held.remove(&vk) {
                    // Only releases of presses the host saw are forwarded, which
                    // also covers the releases winit synthesises on focus loss.
                    self.send_input(id, InputEvent::KeyUp { vk });
                }
            }
            _ => {}
        }
    }

    /// Release every key and button the host believes is down for `id`.
    fn release_held(&mut self, id: u64) {
        let keys: Vec<u32> = self
            .held_keys
            .get_mut(&id)
            .map(|k| k.drain().collect())
            .unwrap_or_default();
        for vk in keys {
            self.send_input(id, InputEvent::KeyUp { vk });
        }
        let releases = self
            .proxies
            .get_mut(&id)
            .map(|p| p.pointer.release_all())
            .unwrap_or_default();
        if !releases.is_empty() {
            self.pending_mouse_moves.remove(&id);
        }
        for event in releases {
            self.send_input(id, event);
        }
    }

    fn send_input(&self, id: u64, event: InputEvent) {
        self.send(&ClientMessage::Input {
            id,
            event,
            ts: self.now_ms(),
        });
    }

    fn flush_mouse_moves(&mut self) {
        for (id, event) in std::mem::take(&mut self.pending_mouse_moves) {
            self.send_input(id, event);
        }
    }

    /// Close any resize gesture that has gone quiet, and apply a snap that was
    /// deferred while it ran.
    fn settle_resizes(&mut self) {
        let now = Instant::now();
        let mut requests = vec![];
        for (&id, proxy) in self.proxies.iter_mut() {
            if let Some((request, finished)) = proxy.tracker.tick(now) {
                requests.push((id, request));
                if !finished.resized {
                    // The host changed the window's size while it was being
                    // dragged; the snap was deferred until now.
                    proxy.snap_if_needed(&self.gpu);
                }
            }
        }
        for (id, Request { phase, size }) in requests {
            self.send(&ClientMessage::RequestResize { id, size, phase });
            self.frame_dirty = true;
        }
    }

    // --- per-tick work ---------------------------------------------------------

    fn tick(&mut self, el: &ActiveEventLoop) {
        self.poll_dashboard(el);
        self.drain_session(el);
        self.poll_decoder();
        self.settle_resizes();
        self.flush_mouse_moves();
        if std::mem::take(&mut self.frame_dirty) {
            let ids: Vec<u64> = self.proxies.keys().copied().collect();
            for id in ids {
                self.render_proxy(id);
            }
        }
        if self.dashboard_wants_frame() {
            self.dash_window.window.request_redraw();
        }
        el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + TICK));
    }

    fn dashboard_wants_frame(&mut self) -> bool {
        match self.dash_window.next_repaint {
            Some(t) if Instant::now() >= t => {
                self.dash_window.next_repaint = None;
                true
            }
            _ => false,
        }
    }

    // --- dashboard painting ----------------------------------------------------

    fn paint_dashboard(&mut self) {
        let dw = &mut self.dash_window;
        let raw = dw.state.take_egui_input(&dw.window);
        let dashboard = &mut self.dashboard;
        let mut output = dw.ctx.run_ui(raw, |ui| dashboard.ui(ui));
        dw.state
            .handle_platform_output(&dw.window, std::mem::take(&mut output.platform_output));

        // egui asks to be called again after `repaint_delay` (animations, hover).
        if let Some(v) = output.viewport_output.get(&egui::ViewportId::ROOT) {
            dw.next_repaint = Instant::now().checked_add(v.repaint_delay);
        }
        if self.dashboard.wants_repaint() {
            let soon = Instant::now() + Duration::from_millis(250);
            dw.next_repaint = Some(dw.next_repaint.map_or(soon, |t| t.min(soon)));
        }

        // Texture uploads must be applied even if this frame cannot be drawn, or
        // egui's view of the atlas drifts from the GPU's.
        let mut textures = std::mem::take(&mut output.textures_delta);
        let state = &self.gpu.state;
        {
            let mut renderer = state.renderer.write();
            for (id, deltas) in textures.set.drain() {
                for delta in &deltas {
                    renderer.update_texture(&state.device, &state.queue, id, delta);
                }
            }
        }

        let pixels_per_point = output.pixels_per_point;
        let prims = dw.ctx.tessellate(std::mem::take(&mut output.shapes), pixels_per_point);
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [dw.size.0.max(1), dw.size.1.max(1)],
            pixels_per_point,
        };
        match dw.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                let mut encoder =
                    state
                        .device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("transom dashboard"),
                        });
                let extra = state.renderer.write().update_buffers(
                    &state.device,
                    &state.queue,
                    &mut encoder,
                    &prims,
                    &screen,
                );
                {
                    let view = frame
                        .texture
                        .create_view(&wgpu::TextureViewDescriptor::default());
                    let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("transom dashboard"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color {
                                    r: 0.03,
                                    g: 0.07,
                                    b: 0.12,
                                    a: 1.0,
                                }),
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
                state
                    .queue
                    .submit(extra.into_iter().chain([encoder.finish()]));
                if let Some(path) = std::env::var_os("TRANSOM_DUMP_DASHBOARD") {
                    self.dump_frames += 1;
                    if self.dump_frames == 40 {
                        let r = self.gpu.dump_ppm(&frame.texture, &path.to_string_lossy());
                        eprintln!("dashboard dump: {r:?}");
                        std::process::exit(0);
                    }
                }
                state.queue.present(frame);
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.gpu.configure(&dw.surface, dw.size.0, dw.size.1);
                dw.next_repaint = Some(Instant::now());
            }
            _ => {}
        }
        let mut renderer = state.renderer.write();
        for id in textures.free.drain() {
            renderer.free_texture(&id);
        }
    }

    fn dashboard_event(&mut self, el: &ActiveEventLoop, event: WindowEvent) {
        let response = self
            .dash_window
            .state
            .on_window_event(&self.dash_window.window, &event);
        if response.repaint {
            self.request_dashboard_redraw();
        }
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(size) => {
                self.dash_window.size = (size.width.max(1), size.height.max(1));
                self.gpu
                    .configure(&self.dash_window.surface, self.dash_window.size.0, self.dash_window.size.1);
                self.request_dashboard_redraw();
            }
            WindowEvent::ScaleFactorChanged { .. } => self.request_dashboard_redraw(),
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    if event.state == ElementState::Pressed
                        && input::disconnect_chord(
                            code,
                            self.modifiers.control_key(),
                            self.modifiers.alt_key(),
                            self.modifiers.shift_key(),
                            self.modifiers.super_key(),
                        )
                        && (self.active || self.session.is_some())
                    {
                        self.disconnect_to_dashboard();
                    }
                }
            }
            WindowEvent::RedrawRequested => self.paint_dashboard(),
            _ => {}
        }
    }
}

fn suffix(reason: Option<String>) -> String {
    reason.map(|r| format!(": {r}")).unwrap_or_default()
}

impl ApplicationHandler for App {
    fn resumed(&mut self, _el: &ActiveEventLoop) {}

    fn window_event(&mut self, el: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        if window_id == self.dash_window.window.id() {
            self.dashboard_event(el, event);
        } else if let Some(&id) = self.window_to_id.get(&window_id) {
            self.proxy_event(id, event);
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        self.tick(el);
    }

    fn exiting(&mut self, _el: &ActiveEventLoop) {
        // Clean shutdown of the session's threads.
        if let Some(s) = self.session.take() {
            s.shutdown();
        }
    }
}
