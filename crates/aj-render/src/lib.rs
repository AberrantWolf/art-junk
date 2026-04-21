//! Vello-based rendering pipeline for art-junk scenes.

use aj_core::SceneSnapshot;
use vello::kurbo::{Affine, BezPath, Stroke as KStroke};
use vello::peniko::Color;
use vello::{AaConfig, AaSupport, RenderParams, Renderer as VelloRenderer, RendererOptions, Scene};

pub struct Renderer {
    vello: VelloRenderer,
}

impl Renderer {
    pub fn new(device: &wgpu::Device, surface_format: wgpu::TextureFormat) -> anyhow::Result<Self> {
        let vello = VelloRenderer::new(
            device,
            RendererOptions {
                surface_format: Some(surface_format),
                use_cpu: false,
                antialiasing_support: AaSupport { area: true, msaa8: false, msaa16: false },
                num_init_threads: None,
            },
        )
        .map_err(|e| anyhow::anyhow!("Vello Renderer::new: {e:?}"))?;
        Ok(Self { vello })
    }

    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        snapshot: &SceneSnapshot,
        surface_texture: &wgpu::SurfaceTexture,
        width: u32,
        height: u32,
    ) -> anyhow::Result<()> {
        let mut scene = Scene::new();
        let stroke_style = KStroke::new(2.0);
        let cyan = Color::rgb8(0, 200, 220);
        for s in &snapshot.strokes {
            if s.points.len() < 2 {
                continue;
            }
            let mut path = BezPath::new();
            path.move_to(s.points[0]);
            for p in &s.points[1..] {
                path.line_to(*p);
            }
            scene.stroke(&stroke_style, Affine::IDENTITY, cyan, None, &path);
        }
        self.vello
            .render_to_surface(
                device,
                queue,
                &scene,
                surface_texture,
                &RenderParams {
                    base_color: Color::rgb8(15, 18, 23),
                    width,
                    height,
                    antialiasing_method: AaConfig::Area,
                },
            )
            .map_err(|e| anyhow::anyhow!("Vello render_to_surface: {e:?}"))?;
        Ok(())
    }
}
