//! egui chrome overlay on top of Vello's already-rendered surface.
//!
//! v1 uses a two-submit composition: `aj_render::Renderer::render` submits the
//! Vello pass against the surface; this module opens a second encoder and appends
//! the egui-wgpu draw pass with `LoadOp::Load` so the menu bar overlays the
//! drawing without re-clearing it. A single-encoder variant lands when aj-render
//! grows a texture-target path (deferred per the plan).

use egui_wgpu::ScreenDescriptor;
use winit::window::Window;

use crate::gpu::GpuState;

pub struct Chrome {
    pub ctx: egui::Context,
    pub winit_state: egui_winit::State,
    pub renderer: egui_wgpu::Renderer,
}

impl Chrome {
    pub fn new(gpu: &GpuState, window: &Window) -> Self {
        let ctx = egui::Context::default();
        let viewport_id = ctx.viewport_id();
        #[allow(clippy::cast_possible_truncation)]
        let scale = window.scale_factor() as f32;
        let winit_state =
            egui_winit::State::new(ctx.clone(), viewport_id, window, Some(scale), None, None);
        let renderer = egui_wgpu::Renderer::new(&gpu.device, gpu.config.format, None, 1, false);
        Self { ctx, winit_state, renderer }
    }

    pub fn paint(
        &mut self,
        gpu: &GpuState,
        window: &Window,
        surface_texture: &wgpu::SurfaceTexture,
        full_output: egui::FullOutput,
    ) {
        self.winit_state.handle_platform_output(window, full_output.platform_output);

        let pixels_per_point = self.ctx.pixels_per_point();
        let paint_jobs = self.ctx.tessellate(full_output.shapes, pixels_per_point);

        let screen = ScreenDescriptor {
            size_in_pixels: [gpu.config.width, gpu.config.height],
            pixels_per_point,
        };

        for (id, delta) in &full_output.textures_delta.set {
            self.renderer.update_texture(&gpu.device, &gpu.queue, *id, delta);
        }

        let view = surface_texture.texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("aj-app chrome"),
        });

        let staging = self.renderer.update_buffers(
            &gpu.device,
            &gpu.queue,
            &mut encoder,
            &paint_jobs,
            &screen,
        );

        {
            let mut rpass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("aj-app egui pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                })
                .forget_lifetime();
            self.renderer.render(&mut rpass, &paint_jobs, &screen);
        }

        for id in &full_output.textures_delta.free {
            self.renderer.free_texture(id);
        }

        gpu.queue.submit(staging.into_iter().chain(std::iter::once(encoder.finish())));
    }
}
