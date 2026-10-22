//! `transom-client doctor`: check the Wayland session, GPU presentation, the HEVC
//! decoder and the LAN interfaces, then exit. The Linux counterpart of
//! `doctor.rs` on Windows (D3D11 / DPI / monitors).

use std::process::ExitCode;
use std::sync::Arc;

use egui_wgpu::wgpu;
use egui_winit::winit::application::ApplicationHandler;
use egui_winit::winit::dpi::LogicalSize;
use egui_winit::winit::event::WindowEvent;
use egui_winit::winit::event_loop::{ActiveEventLoop, EventLoop};
use egui_winit::winit::window::{Window, WindowId};

use super::gpu::Gpu;

pub fn run() -> ExitCode {
    let mut ok = true;
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| "(unset)".into());
    println!("session:      XDG_SESSION_TYPE={}", env("XDG_SESSION_TYPE"));
    println!("compositor:   WAYLAND_DISPLAY={}", env("WAYLAND_DISPLAY"));
    println!("desktop:      XDG_CURRENT_DESKTOP={}", env("XDG_CURRENT_DESKTOP"));
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        println!("  FAIL: no Wayland compositor to connect to (WAYLAND_DISPLAY is unset).");
        ok = false;
    }

    match super::decode::probe() {
        Ok(desc) => println!("hevc decode:  OK — {desc}"),
        Err(e) => {
            println!("hevc decode:  FAIL — {e}");
            ok = false;
        }
    }

    let nets = crate::discovery::local_interfaces();
    if nets.is_empty() {
        println!("network:      WARN — no private IPv4 interface found; use a manual connection");
    } else {
        println!(
            "network:      {} private IPv4 interface(s): {}",
            nets.len(),
            nets.iter().map(|ip| ip.to_string()).collect::<Vec<_>>().join(", ")
        );
    }

    let event_loop = match EventLoop::new() {
        Ok(l) => l,
        Err(e) => {
            println!("wayland:      FAIL — {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut probe = Probe { ok: true };
    if let Err(e) = event_loop.run_app(&mut probe) {
        println!("wayland:      FAIL — {e}");
        ok = false;
    }
    if ok && probe.ok {
        println!("\nAll checks passed.");
        ExitCode::SUCCESS
    } else {
        println!("\nSome checks failed.");
        ExitCode::FAILURE
    }
}

struct Probe {
    ok: bool,
}

impl ApplicationHandler for Probe {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        self.ok = probe_display(el);
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

fn probe_display(el: &ActiveEventLoop) -> bool {
    for m in el.available_monitors() {
        let size = m.size();
        println!(
            "monitor:      {} {}x{} px, scale {:.3}, {} mHz",
            m.name().unwrap_or_else(|| "(unnamed)".into()),
            size.width,
            size.height,
            m.scale_factor(),
            m.refresh_rate_millihertz().unwrap_or(0),
        );
    }
    let window = match el.create_window(
        Window::default_attributes()
            .with_title("Transom doctor")
            .with_inner_size(LogicalSize::new(320.0, 200.0)),
    ) {
        Ok(w) => Arc::new(w),
        Err(e) => {
            println!("window:       FAIL — {e}");
            return false;
        }
    };
    println!(
        "window:       OK — scale factor {:.3}, inner {}x{} px",
        window.scale_factor(),
        window.inner_size().width,
        window.inner_size().height
    );
    let config = egui_wgpu::WgpuConfiguration {
        wgpu_setup: egui_wgpu::WgpuSetup::CreateNew(
            egui_wgpu::WgpuSetupCreateNew::from_display_handle(el.owned_display_handle()),
        ),
        ..Default::default()
    };
    let instance = pollster::block_on(config.wgpu_setup.new_instance());
    let surface = match instance.create_surface(window.clone()) {
        Ok(s) => s,
        Err(e) => {
            println!("surface:      FAIL — {e}");
            return false;
        }
    };
    match pollster::block_on(Gpu::new(&instance, &config, &surface)) {
        Ok(gpu) => {
            let info = gpu.state.adapter.get_info();
            println!("gpu:          OK — {} ({:?})", info.name, info.backend);
            let caps = surface.get_capabilities(&gpu.state.adapter);
            println!("  format:     {:?} (non-sRGB: {})", gpu.format, !gpu.format.is_srgb());
            println!("  present:    {:?}", caps.present_modes);
            println!(
                "  max texture {}px — fits displays up to {}x{} NV12",
                gpu.max_texture_dimension(),
                gpu.max_texture_dimension(),
                gpu.max_texture_dimension() * 2 / 3
            );
            gpu.format != wgpu::TextureFormat::Rgba8UnormSrgb
        }
        Err(e) => {
            println!("gpu:          FAIL — {e}");
            false
        }
    }
}
