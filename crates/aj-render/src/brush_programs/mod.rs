//! Stroke compositor: per-layer substrates + inter-layer composite.
//!
//! ## Substrates (per-layer)
//!
//! Each layer in the snapshot owns its own `Rgba16Float` substrate texture
//! (ping-pong, for the brush program read/write swap). Substrates are
//! cached across frames keyed by `LayerId`; allocate-if-missing at
//! `begin_frame`, GC orphan substrates whose layer is no longer in the
//! snapshot. Cleared per frame to `(1, 1, 1, 0)` — white paper, alpha = 0.
//!
//! ## Brush programs
//!
//! Three programs: **plain** (linear-RGB alpha-over), **highlighter**
//! (multiplicative tint), **pigment** (Kubelka–Munk K/S in 7-band space).
//! All share one bind-group layout and one uniform buffer; pipeline switch
//! at `apply_stroke` is the only per-program difference. Adding a new brush
//! program is a new WGSL file plus one pipeline registration.
//!
//! ## Inter-layer composite
//!
//! After all strokes deposit, an inter-layer composite pass walks visible
//! layers bottom-to-top, blending each layer's substrate into a `composite`
//! ping-pong accumulator. Each `BlendMode` is its own WGSL fragment program;
//! `BlendMode::Normal` ships in C2, `OklabMix` in C4. Per-layer `opacity`
//! modulates the blend weight. The active-stroke's layer is force-included
//! in the composite even if `visible = false`, so hiding a layer mid-drag
//! doesn't make the in-flight stroke vanish under the user's pen.
//!
//! ## Present
//!
//! Final pass composites the accumulator over the surface backdrop in
//! linear RGB, sRGB-encodes for the `*_Unorm` surface, and overlays chrome
//! (page border, future on-canvas guides). Output is what the user sees,
//! before egui chrome paints on top.
//!
//! ## Why fragment passes (not compute)
//!
//! Compute storage textures gate behind device features that aren't
//! always available on WebGPU. Fragment-attachment writes work everywhere.

use std::collections::HashMap;

use aj_core::{BlendMode, BrushType, Layer, LayerId, LinearRgba, Stroke};
use bytemuck::{Pod, Zeroable};

mod parity;

/// Substrate + composite format. `Rgba16Float` gives K/S round-trip
/// headroom and avoids 8-bit banding from per-stroke read-modify-write.
const SUBSTRATE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// Format Vello demands for `render_to_texture` (storage + render
/// attachment + texture binding).
const COVERAGE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Format for the chrome buffer Vello renders into. Same constraint as
/// coverage — Vello requires `Rgba8Unorm` + `STORAGE_BINDING`.
const CHROME_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

const TEXTURE_LABEL: &str = "aj-render substrate";

/// Brush uniforms uploaded once per stroke. Layout matches the WGSL
/// `Uniforms` struct in `shaders/{plain,highlighter,pigment}.wgsl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct BrushUniforms {
    brush_lin: [f32; 4],
    brush_lo: [f32; 4],
    brush_hi: [f32; 4],
    brush_alpha: f32,
    _pad: [f32; 3],
}

/// Inter-layer blend uniforms uploaded once per visible layer at composite
/// time.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct BlendUniforms {
    /// Per-layer opacity, multiplied into the per-pixel substrate alpha at
    /// blend time. `0.0` makes the layer invisible (a no-op blend); `1.0`
    /// uses the substrate alpha as-is.
    opacity: f32,
    _pad: [f32; 3],
}

/// Compute the brush's per-band reflectance for the pigment shader, plus
/// the linear RGB payload the plain/highlighter shaders use.
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

/// One layer's substrate ping-pong. Two textures so the brush program can
/// read from one side and write the other within the same render pass; the
/// host swaps `front_idx` after each deposit.
struct LayerSubstrate {
    /// Owns the underlying textures so the views below remain valid for the
    /// lifetime of this struct. The compositor never accesses them
    /// directly — all reads/writes go through `views`.
    #[allow(dead_code)]
    textures: [wgpu::Texture; 2],
    views: [wgpu::TextureView; 2],
    front_idx: usize,
}

pub struct StrokeCompositor {
    width: u32,
    height: u32,

    /// Per-layer substrate cache. Keyed by `LayerId`; allocate-if-missing
    /// at `begin_frame`, drop orphans whose layer is no longer present.
    substrates: HashMap<LayerId, LayerSubstrate>,

    coverage: wgpu::Texture,
    coverage_view: wgpu::TextureView,

    chrome: wgpu::Texture,
    chrome_view: wgpu::TextureView,

    /// Inter-layer composite accumulator, ping-pong. Each layer's blend
    /// reads `composite[front]` + the layer's substrate, writes
    /// `composite[1-front]`, swaps. Both sides cleared at `begin_frame`
    /// so a frame with zero visible layers presents transparent (the
    /// backdrop shows through).
    composite: [wgpu::Texture; 2],
    composite_views: [wgpu::TextureView; 2],
    composite_front_idx: usize,

    brush_bgl: wgpu::BindGroupLayout,
    brush_uniform_buf: wgpu::Buffer,
    plain_pipeline: wgpu::RenderPipeline,
    highlighter_pipeline: wgpu::RenderPipeline,
    pigment_pipeline: wgpu::RenderPipeline,

    blend_bgl: wgpu::BindGroupLayout,
    blend_uniform_buf: wgpu::Buffer,
    /// Pipeline per `BlendMode`. C2 ships only `Normal`. `Unknown` and any
    /// not-yet-shipping variant fall back to `Normal` at composite time.
    blend_pipelines: HashMap<BlendMode, wgpu::RenderPipeline>,

    present_bgl: wgpu::BindGroupLayout,
    present_pipeline: wgpu::RenderPipeline,
}

impl StrokeCompositor {
    #[allow(clippy::too_many_lines)] // pipeline-construction boilerplate.
    pub fn new(
        device: &wgpu::Device,
        surface_format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Self {
        let (w, h) = (width.max(1), height.max(1));

        let (coverage, coverage_view) = make_coverage(device, w, h);
        let (chrome, chrome_view) = make_chrome(device, w, h);
        let (composite, composite_views) = make_composite_pair(device, w, h);

        let brush_uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("aj-render brush uniforms"),
            size: std::mem::size_of::<BrushUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let blend_uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("aj-render blend uniforms"),
            size: std::mem::size_of::<BlendUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let brush_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("aj-render brush bgl"),
            entries: &[
                uniform_entry(0),
                texture_entry(1), // coverage
                texture_entry(2), // substrate (read)
            ],
        });
        let blend_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("aj-render blend bgl"),
            entries: &[
                uniform_entry(0),
                texture_entry(1), // composite_below
                texture_entry(2), // layer_above (this layer's substrate)
            ],
        });

        let plain_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("aj-render plain brush shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/plain.wgsl").into()),
        });
        let highlighter_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("aj-render highlighter brush shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/highlighter.wgsl").into()),
        });
        let pigment_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("aj-render pigment brush shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/pigment.wgsl").into()),
        });
        let blend_normal_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("aj-render blend-normal shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/blend_normal.wgsl").into()),
        });
        let blend_oklab_mix_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("aj-render blend-oklab-mix shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/blend_oklab_mix.wgsl").into()),
        });

        let brush_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("aj-render brush pipeline layout"),
                bind_group_layouts: &[&brush_bgl],
                push_constant_ranges: &[],
            });
        let blend_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("aj-render blend pipeline layout"),
                bind_group_layouts: &[&blend_bgl],
                push_constant_ranges: &[],
            });

        let plain_pipeline = make_substrate_pipeline(
            device,
            "aj-render plain pipeline",
            &brush_pipeline_layout,
            &plain_shader,
        );
        let highlighter_pipeline = make_substrate_pipeline(
            device,
            "aj-render highlighter pipeline",
            &brush_pipeline_layout,
            &highlighter_shader,
        );
        let pigment_pipeline = make_substrate_pipeline(
            device,
            "aj-render pigment pipeline",
            &brush_pipeline_layout,
            &pigment_shader,
        );
        let blend_normal_pipeline = make_substrate_pipeline(
            device,
            "aj-render blend-normal pipeline",
            &blend_pipeline_layout,
            &blend_normal_shader,
        );
        let blend_oklab_mix_pipeline = make_substrate_pipeline(
            device,
            "aj-render blend-oklab-mix pipeline",
            &blend_pipeline_layout,
            &blend_oklab_mix_shader,
        );

        let mut blend_pipelines: HashMap<BlendMode, wgpu::RenderPipeline> = HashMap::new();
        blend_pipelines.insert(BlendMode::Normal, blend_normal_pipeline);
        blend_pipelines.insert(BlendMode::OklabMix, blend_oklab_mix_pipeline);
        // BlendMode::Unknown intentionally absent from the map; lookup
        // failures fall back to Normal at composite time, mirroring the
        // BrushType::Unknown → plain fallback.

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
            substrates: HashMap::new(),
            coverage,
            coverage_view,
            chrome,
            chrome_view,
            composite,
            composite_views,
            composite_front_idx: 0,
            brush_bgl,
            brush_uniform_buf,
            plain_pipeline,
            highlighter_pipeline,
            pigment_pipeline,
            blend_bgl,
            blend_uniform_buf,
            blend_pipelines,
            present_bgl,
            present_pipeline,
        }
    }

    pub fn resize(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        let (w, h) = (width.max(1), height.max(1));
        if self.width == w && self.height == h {
            return;
        }
        // Drop everything; substrates re-allocate at new size on next
        // begin_frame's alloc-if-missing pass. Composite + coverage +
        // chrome are reallocated up-front since they're shared (not
        // per-layer).
        self.substrates.clear();
        let (coverage, coverage_view) = make_coverage(device, w, h);
        let (chrome, chrome_view) = make_chrome(device, w, h);
        let (composite, composite_views) = make_composite_pair(device, w, h);
        self.coverage = coverage;
        self.coverage_view = coverage_view;
        self.chrome = chrome;
        self.chrome_view = chrome_view;
        self.composite = composite;
        self.composite_views = composite_views;
        self.composite_front_idx = 0;
        self.width = w;
        self.height = h;
    }

    /// Coverage view that Vello should target for the next stroke.
    pub fn coverage_view(&self) -> &wgpu::TextureView {
        &self.coverage_view
    }

    /// Chrome view that Vello should target for page decorations.
    pub fn chrome_view(&self) -> &wgpu::TextureView {
        &self.chrome_view
    }

    /// Per-frame setup: ensure a substrate exists for every `LayerId` in
    /// `keep`, drop orphans, clear all substrates + the chrome buffer +
    /// both composite ping-pong sides. `keep` is typically the snapshot's
    /// layer ids plus the active-stroke's layer (defensive — should be a
    /// subset of layer ids, but the active-stroke target falling outside
    /// `snapshot.layers` would orphan its substrate mid-drag).
    pub fn begin_frame(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        keep: &[LayerId],
    ) {
        // 1. GC orphan substrates whose ids aren't in `keep`.
        self.substrates.retain(|id, _| keep.contains(id));

        // 2. Allocate-if-missing for every kept id, then clear both ping-
        //    pong sides of every substrate. We clear both sides so the
        //    first `apply_stroke` against this layer reads a valid white-
        //    paper canvas regardless of front_idx.
        for &id in keep {
            self.substrates
                .entry(id)
                .or_insert_with(|| make_layer_substrate(device, self.width, self.height));
        }
        for sub in self.substrates.values_mut() {
            for side in 0..2 {
                let _ = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("aj-render substrate clear"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &sub.views[side],
                        resolve_target: None,
                        ops: wgpu::Operations {
                            // RGB = white paper (so an initial pigment
                            // stroke against this canvas sees the right
                            // baseline); alpha = 0 says "no paint".
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 1.0,
                                g: 1.0,
                                b: 1.0,
                                a: 0.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
            }
            sub.front_idx = 0;
        }

        // 3. Clear chrome.
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

        // 4. Clear both composite ping-pong sides. Cleared regardless so a
        //    frame with zero visible layers presents transparent (backdrop
        //    shows through) instead of stale composite from the previous
        //    frame.
        for side in 0..2 {
            let _ = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("aj-render composite clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.composite_views[side],
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
        }
        self.composite_front_idx = 0;
    }

    /// Run the brush program for `stroke` against the named layer's
    /// substrate. The caller has already rasterised this stroke's coverage
    /// into [`coverage_view`](Self::coverage_view). Reads the layer's
    /// "front" substrate, writes "back", swaps `front_idx` after the pass.
    /// Returns silently (warns) if `target` is missing — should not happen
    /// in practice because `begin_frame` allocated for every layer in the
    /// snapshot.
    pub fn apply_stroke(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        stroke: &Stroke,
        target: LayerId,
    ) {
        let Some(sub) = self.substrates.get_mut(&target) else {
            log::warn!("apply_stroke: no substrate for layer {target:?}; stroke skipped");
            return;
        };

        let uniforms = brush_uniforms(stroke.brush.color);
        queue.write_buffer(&self.brush_uniform_buf, 0, bytemuck::bytes_of(&uniforms));

        let read = sub.front_idx;
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
                    resource: wgpu::BindingResource::TextureView(&sub.views[read]),
                },
            ],
        });

        let pipeline = match stroke.brush.brush_type {
            BrushType::Highlighter => &self.highlighter_pipeline,
            BrushType::Pigment => &self.pigment_pipeline,
            // BrushType::Normal and forward-compat Unknown both render as
            // plain alpha-over (the safe default).
            _ => &self.plain_pipeline,
        };

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("aj-render brush pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &sub.views[write],
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

        sub.front_idx = write;
    }

    /// Walk visible layers bottom-to-top, blending each into the composite
    /// accumulator using its `BlendMode` and `opacity`. The active stroke's
    /// target layer is force-included even if `visible = false`, so an
    /// in-flight stroke stays visible when the user toggles its layer's
    /// visibility off.
    pub fn composite_layers<'a, I>(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        layers: I,
        active_stroke_layer: Option<LayerId>,
    ) where
        I: IntoIterator<Item = &'a Layer>,
    {
        for layer in layers {
            let effective_visible = layer.visible || active_stroke_layer == Some(layer.id);
            if !effective_visible {
                continue;
            }
            let Some(sub) = self.substrates.get(&layer.id) else {
                log::warn!("composite_layers: no substrate for layer {:?}; skipped", layer.id);
                continue;
            };

            // Pipeline lookup: Unknown blend mode falls back to Normal.
            let pipeline = self
                .blend_pipelines
                .get(&layer.blend_mode)
                .or_else(|| self.blend_pipelines.get(&BlendMode::Normal))
                .expect("Normal blend pipeline always present");

            let uniforms = BlendUniforms { opacity: layer.opacity.clamp(0.0, 1.0), _pad: [0.0; 3] };
            queue.write_buffer(&self.blend_uniform_buf, 0, bytemuck::bytes_of(&uniforms));

            let read = self.composite_front_idx;
            let write = 1 - read;

            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("aj-render blend bg"),
                layout: &self.blend_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.blend_uniform_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&self.composite_views[read]),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(&sub.views[sub.front_idx]),
                    },
                ],
            });

            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("aj-render blend pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &self.composite_views[write],
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, &bind_group, &[]);
                pass.draw(0..3, 0..1);
            }

            self.composite_front_idx = write;
        }
    }

    /// Composite-over-backdrop in linear → sRGB → chrome on top → surface.
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
                        &self.composite_views[self.composite_front_idx],
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

fn make_substrate_pipeline(
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

fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
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

fn make_layer_substrate(device: &wgpu::Device, w: u32, h: u32) -> LayerSubstrate {
    let make = |idx: usize| {
        let label = format!("{TEXTURE_LABEL}-layer-{idx}");
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
    LayerSubstrate { textures: [t0, t1], views: [v0, v1], front_idx: 0 }
}

fn make_composite_pair(
    device: &wgpu::Device,
    w: u32,
    h: u32,
) -> ([wgpu::Texture; 2], [wgpu::TextureView; 2]) {
    let make = |idx: usize| {
        let label = format!("aj-render composite-{idx}");
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
