//! Vello-based rendering pipeline for art-junk scenes.

use aj_core::SceneSnapshot;
use vello::kurbo::{Affine, BezPath, Rect, Stroke as KStroke};
use vello::peniko::{Color, Mix};
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
        let page = snapshot.page;
        let page_rect = Rect::from_origin_size((0.0, 0.0), page.size);

        // Strokes optionally live inside a clip layer at the page rect. The border
        // is drawn *outside* the clip so it stays visible even when the clip would
        // otherwise cull strokes at the edge.
        if page.clip_to_bounds {
            scene.push_layer(Mix::Clip, 1.0, Affine::IDENTITY, &page_rect);
        }
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
        if page.clip_to_bounds {
            scene.pop_layer();
        }
        if page.show_bounds {
            let border_style = KStroke::new(1.0);
            let border_color = Color::rgb8(80, 90, 100);
            scene.stroke(&border_style, Affine::IDENTITY, border_color, None, &page_rect);
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
