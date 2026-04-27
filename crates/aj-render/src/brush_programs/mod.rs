//! Stroke compositor: per-stroke deposit into a linear-RGB substrate.
//!
//! ## Substrate
//!
//! A per-frame `Rgba16Float` "substrate" texture represents the canvas.
//! Cleared at frame start to `(1, 1, 1, 0)` — white paper, alpha = 0
//! ("no paint here yet"). Strokes deposit in z-order: each runs a brush-
//! specific fragment program that reads the current substrate and writes
//! the new substrate value. A final present pass composites the substrate
//! over the surface backdrop and sRGB-encodes for display.
//!
//! ## Brush programs
//!
//! Two programs ship in Phase A:
//!
//! - **Plain alpha-over** — straight-alpha lerp in linear RGB.
//! - **Pigment** — Kubelka–Munk K/S mix in 7-band reflectance space, with
//!   the substrate's RGB upsampled to bands at read time and the result
//!   integrated back to linear RGB before write. "Already dried" — no
//!   wet-on-wet diffusion; each stroke commits against the current
//!   substrate state.
//!
//! Both programs share one bind-group layout and one uniform buffer; the
//! pipeline switch at `apply_stroke` is the only per-program difference.
//! Adding a new brush program is a new WGSL file plus one extra pipeline.
//!
//! ## Ping-pong
//!
//! The brush fragment shaders read the substrate and write it. A render
//! attachment can't simultaneously be sampled, so we keep two copies of
//! the substrate and swap which side is "front" after each deposit.
//! `front_idx` tracks the side holding the most recent stroke's output.
//!
//! ## Why fragment passes (not compute)
//!
//! Compute storage textures gate behind device features that aren't
//! always available on WebGPU. Fragment-attachment writes work everywhere.
//! The shader pair is small enough that the extra rasterizer hop is
//! irrelevant to frame budget.

use aj_core::{BrushType, LinearRgba, Stroke};
use bytemuck::{Pod, Zeroable};

mod parity;

/// Substrate format. `Rgba16Float` gives K/S round-trip headroom and
/// avoids 8-bit banding from per-stroke read-modify-write. Memory cost is
/// 2× over `Rgba8` at our resolutions; immaterial.
const SUBSTRATE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// Format Vello demands for `render_to_texture` (storage + render
/// attachment + texture binding).
const COVERAGE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Format for the chrome buffer Vello renders into. Same constraint as
/// coverage — Vello requires `Rgba8Unorm` + `STORAGE_BINDING`.
const CHROME_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

const TEXTURE_LABEL: &str = "aj-render substrate";

/// Uniforms uploaded once per stroke. Layout matches the WGSL `Uniforms`
/// struct in `shaders/{plain,pigment}.wgsl`. `_pad` is explicit so std140
/// alignment isn't subject to compiler whim.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct BrushUniforms {
    /// Linear RGB of the brush color (used by plain).
    brush_lin: [f32; 4],
    /// 7-band reflectance bands 0..3 (used by pigment).
    brush_lo: [f32; 4],
    /// Bands 4..6 in xyz (w unused).
    brush_hi: [f32; 4],
    /// Overall stroke alpha — multiplied into per-pixel coverage to form
    /// the mix weight `t`.
    brush_alpha: f32,
    _pad: [f32; 3],
}

/// Compute the brush's per-band reflectance by routing the brush color
/// through the same spectral upsample the pigment shader uses on the
/// canvas side. Mirrors `aj_core::pigment::Pigment::from_linear_rgb`
/// precisely; the parity test in `parity.rs` enforces no drift.
fn brush_uniforms(color: LinearRgba) -> BrushUniforms {
    let p = aj_core::Pigment::from_linear_rgb(LinearRgba::new(color.r, color.g, color.b, 1.0));
    let bands = parity::pigment_canvas_reflectance(&p);
    BrushUniforms {
        brush_lin: [color.r, color.g, color.b, color.a],
        brush_lo: [bands[0], bands[1], bands[2], bands[3]],
        brush_hi: [bands[4], bands[5], bands[6], 0.0],
        brush_alpha: color.a,
        _pad: [0.0; 3],
    }
}

pub struct StrokeCompositor {
    width: u32,
    height: u32,

    substrate: [wgpu::Texture; 2],
    substrate_views: [wgpu::TextureView; 2],
    /// Side currently holding the most-recent stroke state. Reads consume
    /// `front_idx`; writes target `1 - front_idx`. Swapped after each
    /// `apply_stroke`.
    front_idx: usize,

    coverage: wgpu::Texture,
    coverage_view: wgpu::TextureView,

    chrome: wgpu::Texture,
    chrome_view: wgpu::TextureView,

    brush_bgl: wgpu::BindGroupLayout,
    brush_uniform_buf: wgpu::Buffer,
    plain_pipeline: wgpu::RenderPipeline,
    pigment_pipeline: wgpu::RenderPipeline,

    present_bgl: wgpu::BindGroupLayout,
    present_pipeline: wgpu::RenderPipeline,
}

impl StrokeCompositor {
    #[allow(clippy::too_many_lines)] // pipeline-construction boilerplate; splitting hurts readability without removing complexity.
    pub fn new(
        device: &wgpu::Device,
        surface_format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Self {
        let (w, h) = (width.max(1), height.max(1));

        let (substrate, substrate_views) = make_substrate_pair(device, w, h);
        let (coverage, coverage_view) = make_coverage(device, w, h);
        let (chrome, chrome_view) = make_chrome(device, w, h);

        let brush_uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("aj-render brush uniforms"),
            size: std::mem::size_of::<BrushUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let brush_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("aj-render brush bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                texture_entry(1), // coverage
                texture_entry(2), // substrate (read)
            ],
        });

        let plain_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("aj-render plain brush shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/plain.wgsl").into()),
        });
        let pigment_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("aj-render pigment brush shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/pigment.wgsl").into()),
        });

        let brush_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("aj-render brush pipeline layout"),
                bind_group_layouts: &[&brush_bgl],
                push_constant_ranges: &[],
            });
        let plain_pipeline = make_brush_pipeline(
            device,
            "aj-render plain pipeline",
            &brush_pipeline_layout,
            &plain_shader,
        );
        let pigment_pipeline = make_brush_pipeline(
            device,
            "aj-render pigment pipeline",
            &brush_pipeline_layout,
            &pigment_shader,
        );

        let present_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("aj-render present shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/present.wgsl").into()),
        });
        let present_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("aj-render present bgl"),
            entries: &[texture_entry(0), texture_entry(1)],
        });
        let present_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("aj-render present pipeline layout"),
                bind_group_layouts: &[&present_bgl],
                push_constant_ranges: &[],
            });
        let present_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("aj-render present pipeline"),
            layout: Some(&present_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &present_shader,
                entry_point: "vs_main",
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &present_shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        Self {
            width: w,
            height: h,
            substrate,
            substrate_views,
            front_idx: 0,
            coverage,
            coverage_view,
            chrome,
            chrome_view,
            brush_bgl,
            brush_uniform_buf,
            plain_pipeline,
            pigment_pipeline,
            present_bgl,
            present_pipeline,
        }
    }

    pub fn resize(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        let (w, h) = (width.max(1), height.max(1));
        if self.width == w && self.height == h {
            return;
        }
        let (substrate, substrate_views) = make_substrate_pair(device, w, h);
        let (coverage, coverage_view) = make_coverage(device, w, h);
        let (chrome, chrome_view) = make_chrome(device, w, h);
        self.substrate = substrate;
        self.substrate_views = substrate_views;
        self.coverage = coverage;
        self.coverage_view = coverage_view;
        self.chrome = chrome;
        self.chrome_view = chrome_view;
        self.front_idx = 0;
        self.width = w;
        self.height = h;
    }

    /// Coverage view that Vello should target for the next stroke.
    pub fn coverage_view(&self) -> &wgpu::TextureView {
        &self.coverage_view
    }

    /// Chrome view that Vello should target for page decorations (border,
    /// future on-canvas guides). Rendered once per frame after all strokes;
    /// composited on top of the substrate at present time.
    pub fn chrome_view(&self) -> &wgpu::TextureView {
        &self.chrome_view
    }

    /// Clear all transient buffers to their per-frame initial state. Both
    /// substrate ping-pong sides clear so the first deposit's read side is
    /// valid.
    pub fn begin_frame(&mut self, encoder: &mut wgpu::CommandEncoder) {
        for side in 0..2 {
            // Substrate clears to white-paper-no-paint. RGB = white so an
            // initial pigment stroke (which K/S-mixes against the substrate
            // RGB) sees the correct paper baseline; alpha = 0 says "no
            // paint here yet" so the present pass shows the backdrop.
            let _ = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("aj-render substrate clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.substrate_views[side],
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 1.0, g: 1.0, b: 1.0, a: 0.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
        }
        let _ = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("aj-render chrome clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.chrome_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        self.front_idx = 0;
    }

    /// Run the brush program for `stroke` against the substrate. The caller
    /// has already rendered this stroke's coverage into
    /// [`coverage_view`](Self::coverage_view). Reads the "front" substrate
    /// side, writes "back", swaps.
    pub fn apply_stroke(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        stroke: &Stroke,
    ) {
        let uniforms = brush_uniforms(stroke.brush.color);
        queue.write_buffer(&self.brush_uniform_buf, 0, bytemuck::bytes_of(&uniforms));

        let read = self.front_idx;
        let write = 1 - read;

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("aj-render brush bg"),
            layout: &self.brush_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.brush_uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&self.coverage_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&self.substrate_views[read]),
                },
            ],
        });

        // BrushType selects the program. `Unknown` (forward-compat fallback
        // for documents written by future builds) renders as plain — the
        // safe default that won't crash a load and matches the doc on
        // `BrushType::Unknown`.
        let pipeline = match stroke.brush.brush_type {
            BrushType::Pigment => &self.pigment_pipeline,
            _ => &self.plain_pipeline,
        };

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("aj-render brush pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.substrate_views[write],
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..3, 0..1);
        }

        self.front_idx = write;
    }

    /// Composite substrate over backdrop and chrome on top, sRGB-encode to
    /// the surface. Final pass before egui chrome overlays.
    pub fn present(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
    ) {
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("aj-render present bg"),
            layout: &self.present_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(
                        &self.substrate_views[self.front_idx],
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&self.chrome_view),
                },
            ],
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("aj-render present"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&self.present_pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

fn make_brush_pipeline(
    device: &wgpu::Device,
    label: &str,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: "vs_main",
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format: SUBSTRATE_FORMAT,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    })
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn make_substrate_pair(
    device: &wgpu::Device,
    w: u32,
    h: u32,
) -> ([wgpu::Texture; 2], [wgpu::TextureView; 2]) {
    let make = |idx: usize| {
        let label = format!("{TEXTURE_LABEL}-{idx}");
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(&label),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: SUBSTRATE_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        (tex, view)
    };
    let (t0, v0) = make(0);
    let (t1, v1) = make(1);
    ([t0, t1], [v0, v1])
}

fn make_coverage(device: &wgpu::Device, w: u32, h: u32) -> (wgpu::Texture, wgpu::TextureView) {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("aj-render coverage"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: COVERAGE_FORMAT,
        // STORAGE_BINDING is required by Vello's `render_to_texture`.
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::STORAGE_BINDING
            | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    (tex, view)
}

fn make_chrome(device: &wgpu::Device, w: u32, h: u32) -> (wgpu::Texture, wgpu::TextureView) {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("aj-render chrome"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: CHROME_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::STORAGE_BINDING
            | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    (tex, view)
}
