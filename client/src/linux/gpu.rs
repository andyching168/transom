//! The shared wgpu device and the 1:1 blit pipeline.
//!
//! This is where the product's promise is kept or lost on Linux, as `win::gpu`
//! is on Windows (invariants I-1):
//!
//!  * The surface is configured at the **exact physical client size** (done in
//!    `proxy`), and the frame is drawn at native size, so presentation never
//!    scales. Under Wayland fractional scaling winit maps that physical buffer
//!    onto the logical surface with `wp_viewporter`, so it stays 1:1.
//!  * There is **no sampler at all**: the shader fetches texels with
//!    `textureLoad` at integer coordinates, which cannot filter.
//!  * The target format is the *non-sRGB* one, and the shader writes display
//!    values directly, so no hidden gamma conversion touches the pixels.
//!
//! One device is shared by every window; each owns only its surface. The decoded
//! frame (the whole VDS) is one shared texture (`SourceTexture`); each proxy
//! samples its sub-rect out of it.

use egui_wgpu::wgpu;
use wgpu::util::DeviceExt;

/// What a proxy window should draw this frame.
#[derive(Clone, Copy)]
pub enum RenderMode {
    /// Sample the shared source texture at `crop` (x, y, w, h in source pixels).
    Source { crop: [u32; 4] },
    /// 1px checkerboard generated from physical pixel position — the M0 probe for
    /// the 1:1 guarantee. Independent of any host.
    Checkerboard,
    /// A flat diagnostic fill, shown before the first frame arrives.
    Waiting,
}

const SHADER: &str = r#"
struct Params {
    crop: vec4<u32>,        // x, y, w, h of the source sub-rect, in source pixels
    target_size: vec2<u32>, // pixels the crop is mapped onto
    source_height: u32,     // luma rows; chroma rows follow
    mode: u32,
};
@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var src: texture_2d<f32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    // Fullscreen triangle; uv spans [0,1] over the viewport, (0,0) at top-left.
    let uv = vec2<f32>(f32((vid << 1u) & 2u), f32(vid & 2u));
    var o: VsOut;
    o.uv = uv;
    o.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}

@fragment
fn fs_main(i: VsOut) -> @location(0) vec4<f32> {
    if (params.mode == 1u) {
        let cx = u32(i.pos.x);
        let cy = u32(i.pos.y);
        let c = f32((cx + cy) & 1u);
        return vec4<f32>(c, c, c, 1.0);
    }
    if (params.mode == 2u) {
        return vec4<f32>(0.08, 0.10, 0.14, 1.0);
    }
    // Integer pixel mapping: output pixel (px, py) shows source pixel
    // crop.xy + floor(pos * crop.wh / target). Equal sizes make this the identity.
    let px = u32(i.pos.x);
    let py = u32(i.pos.y);
    let sx = min(params.crop.x + (px * params.crop.z) / max(params.target_size.x, 1u),
                 params.crop.x + params.crop.z - 1u);
    let sy = min(params.crop.y + (py * params.crop.w) / max(params.target_size.y, 1u),
                 params.crop.y + params.crop.w - 1u);
    let y = textureLoad(src, vec2<u32>(sx, sy), 0).r * 255.0 - 16.0;
    let cxy = vec2<u32>(sx & ~1u, params.source_height + (sy >> 1u));
    let u = textureLoad(src, cxy, 0).r * 255.0 - 128.0;
    let v = textureLoad(src, cxy + vec2<u32>(1u, 0u), 0).r * 255.0 - 128.0;
    // BT.709 limited range, the same coefficients as the Windows shader.
    let r = clamp((298.0 * y + 459.0 * v + 128.0) / (256.0 * 255.0), 0.0, 1.0);
    let g = clamp((298.0 * y - 55.0 * u - 136.0 * v + 128.0) / (256.0 * 255.0), 0.0, 1.0);
    let b = clamp((298.0 * y + 541.0 * u + 128.0) / (256.0 * 255.0), 0.0, 1.0);
    return vec4<f32>(r, g, b, 1.0);
}
"#;

const PARAMS_SIZE: u64 = 32;

pub struct Gpu {
    pub state: egui_wgpu::RenderState,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    params: wgpu::Buffer,
    /// The surface format every proxy and the dashboard present in.
    pub format: wgpu::TextureFormat,
}

impl Gpu {
    /// Create the device against `first_surface` so the adapter is guaranteed able
    /// to present to the compositor.
    pub async fn new(
        instance: &wgpu::Instance,
        config: &egui_wgpu::WgpuConfiguration,
        first_surface: &wgpu::Surface<'static>,
    ) -> Result<Gpu, String> {
        let state = egui_wgpu::RenderState::create(
            config,
            instance,
            Some(first_surface),
            egui_wgpu::RendererOptions::default(),
        )
        .await
        .map_err(|e| format!("{e}"))?;
        let format = state.target_format;
        if format.is_srgb() {
            eprintln!("gpu: surface format {format:?} is sRGB; pixel values may be gamma-shifted");
        }
        let device = &state.device;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("transom blit"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("transom blit layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT | wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: std::num::NonZeroU64::new(PARAMS_SIZE),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("transom blit pipeline layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("transom blit"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            multiview_mask: None,
            cache: None,
        });
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("transom blit params"),
            size: PARAMS_SIZE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Ok(Gpu {
            state,
            pipeline,
            layout,
            params,
            format,
        })
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.state.device
    }
    pub fn queue(&self) -> &wgpu::Queue {
        &self.state.queue
    }
    pub fn max_texture_dimension(&self) -> u32 {
        self.state.device.limits().max_texture_dimension_2d
    }

    /// Configure `surface` at exactly `width` x `height` physical pixels, picking
    /// the lowest-latency present mode that does not stall a multi-window client
    /// on one window's vblank.
    pub fn configure(&self, surface: &wgpu::Surface<'static>, width: u32, height: u32) {
        let caps = surface.get_capabilities(&self.state.adapter);
        let mut config = surface
            .get_default_config(&self.state.adapter, width.max(1), height.max(1))
            .expect("the surface was created for this adapter");
        config.format = self.format;
        config.view_formats = vec![];
        config.usage |= wgpu::TextureUsages::COPY_SRC;
        config.present_mode = [
            wgpu::PresentMode::Mailbox,
            wgpu::PresentMode::Immediate,
            wgpu::PresentMode::Fifo,
        ]
        .into_iter()
        .find(|m| caps.present_modes.contains(m))
        .unwrap_or(wgpu::PresentMode::Fifo);
        config.desired_maximum_frame_latency = 1;
        surface.configure(&self.state.device, &config);
    }

    /// Draw one frame into `surface` at `target` size (<= the surface) and present.
    /// Returns false if the surface needs reconfiguring (outdated or lost).
    pub fn draw(
        &self,
        surface: &wgpu::Surface<'static>,
        surface_size: (u32, u32),
        target: (u32, u32),
        clear: Option<[f64; 4]>,
        mode: RenderMode,
        source: Option<&SourceTexture>,
    ) -> bool {
        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                return false
            }
            _ => return true,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let (mode_id, crop) = match mode {
            RenderMode::Source { crop } if source.is_some() => (0u32, crop),
            RenderMode::Checkerboard => (1, [0; 4]),
            _ => (2, [0; 4]),
        };
        let source_height = source.map(|s| s.height).unwrap_or(0);
        let mut bytes = [0u8; PARAMS_SIZE as usize];
        for (i, v) in [
            crop[0],
            crop[1],
            crop[2],
            crop[3],
            target.0,
            target.1,
            source_height,
            mode_id,
        ]
        .into_iter()
        .enumerate()
        {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_ne_bytes());
        }
        self.queue().write_buffer(&self.params, 0, &bytes);

        // The sampled texture must exist even for the diagnostic modes.
        let fallback;
        let texture_view = match source {
            Some(s) => &s.view,
            None => {
                fallback = self.fallback_view();
                &fallback
            }
        };
        let bind_group = self.device().create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("transom blit bind group"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(texture_view),
                },
            ],
        });

        let mut encoder = self
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("transom blit"),
            });
        {
            let load = match clear {
                Some([r, g, b, a]) => wgpu::LoadOp::Clear(wgpu::Color { r, g, b, a }),
                None => wgpu::LoadOp::Clear(wgpu::Color::BLACK),
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("transom blit"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load,
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let w = target.0.min(surface_size.0).max(1);
            let h = target.1.min(surface_size.1).max(1);
            pass.set_viewport(0.0, 0.0, w as f32, h as f32, 0.0, 1.0);
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue().submit([encoder.finish()]);
        self.queue().present(frame);
        true
    }

    /// Developer aid (`TRANSOM_DUMP_DASHBOARD`): write a rendered surface texture
    /// to a binary PPM so the UI can be inspected without a screenshot tool.
    pub fn dump_ppm(&self, texture: &wgpu::Texture, path: &str) -> Result<(), String> {
        let (w, h) = (texture.width(), texture.height());
        let row = (w * 4).div_ceil(256) * 256;
        let buffer = self.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("transom dump"),
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.queue().submit([encoder.finish()]);
        buffer.map_async(wgpu::MapMode::Read, .., |_| {});
        self.device()
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| e.to_string())?;
        let data = buffer.get_mapped_range(..).map_err(|e| e.to_string())?;
        let bgra = matches!(texture.format(), wgpu::TextureFormat::Bgra8Unorm);
        let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
        for y in 0..h as usize {
            for x in 0..w as usize {
                let p = &data[y * row as usize + x * 4..][..4];
                let rgb = if bgra { [p[2], p[1], p[0]] } else { [p[0], p[1], p[2]] };
                out.extend_from_slice(&rgb);
            }
        }
        std::fs::write(path, out).map_err(|e| e.to_string())
    }

    fn fallback_view(&self) -> wgpu::TextureView {
        self.device()
            .create_texture_with_data(
                self.queue(),
                &wgpu::TextureDescriptor {
                    label: Some("transom blit fallback"),
                    size: wgpu::Extent3d {
                        width: 2,
                        height: 2,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::R8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                &[0u8; 4],
            )
            .create_view(&wgpu::TextureViewDescriptor::default())
    }
}

/// The whole virtual display as one NV12 frame in a single `R8` texture: `height`
/// luma rows followed by `height / 2` rows of interleaved U,V.
pub struct SourceTexture {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    pub width: u32,
    pub height: u32,
}

impl SourceTexture {
    pub fn new(gpu: &Gpu, width: u32, height: u32) -> Result<Self, String> {
        let rows = height + height / 2;
        let max = gpu.max_texture_dimension();
        if width == 0 || height == 0 || width > max || rows > max {
            return Err(format!(
                "display {width}x{height} does not fit this GPU's {max}px texture limit"
            ));
        }
        let texture = gpu.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("transom source"),
            size: wgpu::Extent3d {
                width,
                height: rows,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Ok(Self {
            texture,
            view,
            width,
            height,
        })
    }

    /// Upload one packed NV12 frame (`stride` bytes per row, Y then UV).
    pub fn update_nv12(&self, gpu: &Gpu, nv12: &[u8], stride: usize) {
        let rows = self.height + self.height / 2;
        if stride < self.width as usize || nv12.len() < stride * rows as usize {
            return;
        }
        gpu.queue().write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            nv12,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride as u32),
                rows_per_image: Some(rows),
            },
            wgpu::Extent3d {
                width: self.width,
                height: rows,
                depth_or_array_layers: 1,
            },
        );
    }
}
