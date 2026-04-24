//! Vello-based rendering pipeline for art-junk scenes.

mod brush;

use aj_core::SceneSnapshot;
use vello::kurbo::{Affine, Rect, Stroke as KStroke};
use vello::peniko::{Color, Fill, Mix};
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

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        snapshot: &SceneSnapshot,
        world_to_screen: Affine,
        surface_texture: &wgpu::SurfaceTexture,
        width: u32,
        height: u32,
    ) -> anyhow::Result<()> {
        let mut scene = Scene::new();
        let page = snapshot.page;
        let page_rect = Rect::from_origin_size((0.0, 0.0), page.size);

        // Strokes optionally live inside a clip layer at the page rect (world-space).
        // The border is drawn *outside* the clip so it stays visible even when the
        // clip would otherwise cull strokes at the page edge.
        if page.clip_to_bounds {
            scene.push_layer(Mix::Clip, 1.0, world_to_screen, &page_rect);
        }
        // TODO(m4-tessellation-cache): tessellate_stroke runs every frame for
        // every stroke. For large scenes / long strokes this is wasteful; key
        // a cache on (stroke.id, samples.len(), brush-hash, screen-scale bucket)
        // and reuse the BezPath when inputs are unchanged.
        for s in &snapshot.strokes {
            let path = brush::tessellate_stroke(s, world_to_screen);
            if path.elements().is_empty() {
                continue;
            }
            let [r, g, b, a] = s.brush.color.to_srgb8();
            let color = Color::rgba8(r, g, b, a);
            scene.fill(Fill::NonZero, world_to_screen, color, None, &path);
        }
        if page.clip_to_bounds {
            scene.pop_layer();
        }
        if page.show_bounds {
            // Border is UI chrome, not content — it stays at a constant physical
            // pixel width regardless of zoom. We divide the desired screen-px width
            // by the effective uniform scale of `world_to_screen` so that after the
            // affine is applied, the stroked line lands at ~BORDER_PX physical px.
            // For a pure scale+translate affine `|det| = scale^2`, so `sqrt(|det|)`
            // recovers the scale; this remains the right formula if non-uniform
            // scale or rotation is ever added (it gives the geometric-mean scale).
            const BORDER_PX: f64 = 1.5;
            let scale = world_to_screen.determinant().abs().sqrt();
            let stroke_width = if scale > 0.0 { BORDER_PX / scale } else { BORDER_PX };
            let border_style = KStroke::new(stroke_width);
            let border_color = Color::rgb8(80, 90, 100);
            scene.stroke(&border_style, world_to_screen, border_color, None, &page_rect);
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
