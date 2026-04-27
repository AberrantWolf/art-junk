//! Rendering pipeline for art-junk scenes.
//!
//! A linear-RGB substrate is the canvas's ground truth. Strokes deposit in
//! z-order via per-brush-type fragment programs (alpha-over, pigment K/S);
//! a present pass composites the substrate over the surface backdrop and
//! sRGB-encodes for display. There is one render path — brush type is a
//! per-stroke pipeline-state switch, not a top-level branch.

mod brush;
mod brush_programs;

use aj_core::SceneSnapshot;
use vello::kurbo::{Affine, Rect, Stroke as KStroke};
use vello::peniko::{Color, Fill, Mix};
use vello::{AaConfig, AaSupport, RenderParams, Renderer as VelloRenderer, RendererOptions, Scene};

use crate::brush_programs::StrokeCompositor;

pub struct Renderer {
    vello: VelloRenderer,
    surface_format: wgpu::TextureFormat,
    /// Lazily allocated on first render — needs surface dimensions to size
    /// the substrate. After allocation it survives across frames; resize
    /// reuses if dimensions match.
    compositor: Option<StrokeCompositor>,
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
        Ok(Self { vello, surface_format, compositor: None })
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
        let compositor = self.compositor.get_or_insert_with(|| {
            StrokeCompositor::new(device, self.surface_format, width, height)
        });
        compositor.resize(device, width, height);

        // 1. Begin frame: clear substrate to white-paper-no-paint, chrome to transparent.
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("aj-render begin-frame"),
        });
        compositor.begin_frame(&mut encoder);
        queue.submit(Some(encoder.finish()));

        // 2. For each stroke in z-order: rasterize coverage via Vello, then
        // dispatch the brush program for that stroke's BrushType. Vello clears
        // its target on every render so coverage submits per-stroke.
        // TODO(m4-tessellation-cache): tessellate_stroke runs every frame for
        // every stroke. For large scenes / long strokes this is wasteful; key
        // a cache on (stroke.id, samples.len(), brush-hash, screen-scale bucket)
        // and reuse the BezPath when inputs are unchanged.
        let page_rect = Rect::from_origin_size((0.0, 0.0), snapshot.page.size);
        for stroke in &snapshot.strokes {
            let path = brush::tessellate_stroke(stroke, world_to_screen);
            if path.elements().is_empty() {
                continue;
            }
            let mut scene = Scene::new();
            // Per-stroke clip respects the page's clip_to_bounds setting; the
            // page rect is in world coords so we apply world_to_screen.
            if snapshot.page.clip_to_bounds {
                scene.push_layer(Mix::Clip, 1.0, world_to_screen, &page_rect);
            }
            scene.fill(Fill::NonZero, world_to_screen, Color::WHITE, None, &path);
            if snapshot.page.clip_to_bounds {
                scene.pop_layer();
            }
            self.vello
                .render_to_texture(
                    device,
                    queue,
                    &scene,
                    compositor.coverage_view(),
                    &RenderParams {
                        base_color: Color::TRANSPARENT,
                        width,
                        height,
                        antialiasing_method: AaConfig::Area,
                    },
                )
                .map_err(|e| anyhow::anyhow!("Vello coverage render_to_texture: {e:?}"))?;

            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("aj-render brush-cmd"),
            });
            compositor.apply_stroke(device, queue, &mut encoder, stroke);
            queue.submit(Some(encoder.finish()));
        }

        // 3. Page chrome (border etc.) renders to a separate buffer that
        // present composites on top of the substrate. Chrome bypasses brush
        // programs because it isn't paint — it's an indicator the user reads
        // as UI, not artwork.
        let chrome_scene = build_chrome_scene(snapshot, world_to_screen);
        self.vello
            .render_to_texture(
                device,
                queue,
                &chrome_scene,
                compositor.chrome_view(),
                &RenderParams {
                    base_color: Color::TRANSPARENT,
                    width,
                    height,
                    antialiasing_method: AaConfig::Area,
                },
            )
            .map_err(|e| anyhow::anyhow!("Vello chrome render_to_texture: {e:?}"))?;

        // 4. Present: composite substrate over backdrop, chrome on top, sRGB-encode to surface.
        let surface_view =
            surface_texture.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("aj-render present-cmd"),
        });
        compositor.present(device, &mut encoder, &surface_view);
        queue.submit(Some(encoder.finish()));

        Ok(())
    }
}

/// Build a Vello scene containing only page chrome (border, future on-canvas
/// guides). Strokes do not appear here — they're routed through the brush
/// programs against the substrate. If `show_bounds` is off this returns an
/// empty scene; the chrome buffer is cleared at frame start regardless, so
/// rendering an empty scene is a no-op.
fn build_chrome_scene(snapshot: &SceneSnapshot, world_to_screen: Affine) -> Scene {
    let mut scene = Scene::new();
    let page = snapshot.page;
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
        let page_rect = Rect::from_origin_size((0.0, 0.0), page.size);
        scene.stroke(&border_style, world_to_screen, border_color, None, &page_rect);
    }
    scene
}
