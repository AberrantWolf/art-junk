mod compose;
mod gpu;
mod shortcuts;
mod ui;
mod viewport;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use aj_core::{Point, PointerId, Size, StrokeId, Vec2};
use aj_engine::{Command, Engine};
use aj_render::Renderer;
use anyhow::Result;
#[cfg(target_os = "macos")]
use stylus_junk::MacTabletBackend;
use stylus_junk::{Phase, StylusAdapter, StylusEvent};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use crate::compose::Chrome;
use crate::gpu::GpuState;
use crate::shortcuts::AppAction;
use crate::ui::{Action, BrushAction, ViewAction, draw_menu_bar};
use crate::viewport::{Viewport, ZOOM_STEP};

/// Each "line" of scroll-wheel movement represents this many physical pixels when
/// panning. Arbitrary but matches the rough default in most desktop apps.
const PIXELS_PER_LINE: f64 = 20.0;
/// For Ctrl+scroll zoom, treat each scroll line as one `ZOOM_STEP` ratchet.
const ZOOM_PER_LINE: f64 = 1.0;
/// Trackpad pixel-delta scroll comes through finer-grained; divide by this to get an
/// equivalent "line" count for the zoom step.
const PIXELS_PER_ZOOM_LINE: f64 = 80.0;

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<GpuState>,
    renderer: Option<Renderer>,
    chrome: Option<Chrome>,
    engine: Option<Engine>,
    cursor: Option<PhysicalPosition<f64>>,
    /// Wrapped in `Rc<RefCell<_>>` because the macOS `NSEvent` monitor
    /// callback needs shared mutable access to emit pen events. On other
    /// platforms the indirection is a small runtime cost for code uniformity.
    stylus: Rc<RefCell<StylusAdapter>>,
    /// The pointer currently driving an in-progress stroke, paired with the
    /// stroke id so we route adapter events to the right engine command.
    active: Option<(PointerId, StrokeId)>,
    /// Holds the `NSEvent` monitor + focus-loss observer alive for the app's
    /// lifetime. Dropped before the window so the monitor is removed while we
    /// still hold a `MainThreadMarker`. Named with a leading underscore to
    /// signal we never read it — its role is the `Drop` it performs on app
    /// exit.
    #[cfg(target_os = "macos")]
    mac_tablet: Option<MacTabletBackend>,
    modifiers: ModifiersState,
    viewport: Viewport,
    dpi_scale: f64,
    /// Whether the right-side brush settings panel is drawn. Pure UI state —
    /// not part of the document, not in the engine. Toggled via
    /// `View > Brush Panel`.
    brush_panel_visible: bool,
}

impl App {
    fn new() -> Self {
        Self {
            window: None,
            gpu: None,
            renderer: None,
            chrome: None,
            engine: None,
            cursor: None,
            stylus: Rc::new(RefCell::new(StylusAdapter::new())),
            active: None,
            #[cfg(target_os = "macos")]
            mac_tablet: None,
            modifiers: ModifiersState::empty(),
            viewport: Viewport::default(),
            dpi_scale: 1.0,
            brush_panel_visible: true,
        }
    }

    fn init(&mut self, window: Arc<Window>) -> Result<()> {
        let gpu = GpuState::new(&window)?;
        let renderer = Renderer::new(&gpu.device, gpu.config.format)?;
        let chrome = Chrome::new(&gpu, &window);
        self.dpi_scale = window.scale_factor();
        // Fit the default 1920×1080 page to the window at startup so the user
        // doesn't see a partial corner of the document. Uses the default page
        // size from aj-core since the engine hasn't published a snapshot yet.
        let window_px = Size::new(f64::from(gpu.config.width), f64::from(gpu.config.height));
        self.viewport.zoom_to_fit(window_px, aj_core::Page::default().size, self.dpi_scale);
        self.gpu = Some(gpu);
        self.renderer = Some(renderer);
        self.chrome = Some(chrome);
        self.engine = Some(Engine::spawn());
        self.window = Some(window);
        #[cfg(target_os = "macos")]
        {
            // SAFETY: winit guarantees this method runs on the main thread,
            // which is what `MainThreadMarker::new()` verifies.
            if let Some(mtm) = stylus_junk::MainThreadMarker::new() {
                match MacTabletBackend::install(self.stylus.clone(), mtm) {
                    Ok(backend) => self.mac_tablet = Some(backend),
                    Err(err) => log::warn!("macOS tablet backend install failed: {err}"),
                }
            } else {
                log::warn!("MacTabletBackend: skipped install, not on main thread");
            }
        }
        Ok(())
    }

    fn render(&mut self) -> Result<()> {
        // Phase A: run egui, collect actions. Scoped so the mutable `chrome` borrow
        // ends before we call `apply_view_action` (which takes &mut self).
        let (
            app_snapshot,
            full_output,
            pending_edit,
            pending_view,
            pending_brush,
            page,
            live_brush,
        );
        {
            let Some(chrome) = self.chrome.as_mut() else { return Ok(()) };
            let Some(engine) = self.engine.as_ref() else { return Ok(()) };
            let Some(window) = self.window.as_ref() else { return Ok(()) };
            app_snapshot = engine.snapshot();
            page = app_snapshot.scene.page;
            live_brush = app_snapshot.scene.brush;
            let panel_visible = self.brush_panel_visible;
            let raw_input = chrome.winit_state.take_egui_input(window);
            let mut edit: Vec<Action> = Vec::new();
            let mut view: Vec<ViewAction> = Vec::new();
            let mut brush: Vec<BrushAction> = Vec::new();
            full_output = chrome.ctx.run(raw_input, |ctx| {
                draw_menu_bar(ctx, app_snapshot.history, page, panel_visible, &mut edit, &mut view);
                crate::ui::brush_panel::draw(ctx, live_brush, panel_visible, &mut brush);
            });
            pending_edit = edit;
            pending_view = view;
            pending_brush = brush;
        }

        // Phase B: dispatch actions. No chrome borrow held, so `apply_view_action`
        // (which mutates self.viewport and may send commands) is legal.
        if let Some(engine) = self.engine.as_ref() {
            for action in pending_edit {
                action.dispatch(engine);
            }
            for action in pending_brush {
                action.dispatch(engine, live_brush);
            }
        }
        for view_action in pending_view {
            self.apply_view_action(view_action, page);
        }

        // Phase C: paint to the surface.
        let world_to_screen = self.viewport.to_affine(self.dpi_scale);
        let Some(gpu) = self.gpu.as_mut() else { return Ok(()) };
        let Some(renderer) = self.renderer.as_mut() else { return Ok(()) };
        let Some(chrome) = self.chrome.as_mut() else { return Ok(()) };
        let Some(window) = self.window.as_ref() else { return Ok(()) };

        let frame = match gpu.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                return Ok(());
            }
            Err(err) => return Err(anyhow::anyhow!("surface error: {err:?}")),
        };

        renderer.render(
            &gpu.device,
            &gpu.queue,
            &app_snapshot.scene,
            world_to_screen,
            &frame,
            gpu.config.width,
            gpu.config.height,
        )?;

        chrome.paint(gpu, window, &frame, full_output);

        frame.present();
        Ok(())
    }

    fn send(&self, cmd: Command) {
        if let Some(engine) = self.engine.as_ref() {
            engine.send(cmd);
        }
    }

    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    /// True if the cursor is currently hovering over egui chrome (menu open, button, etc.).
    fn pointer_over_chrome(&self) -> bool {
        self.chrome
            .as_ref()
            .is_some_and(|c| c.ctx.is_pointer_over_area() || c.ctx.wants_pointer_input())
    }

    /// Current inner window size in physical pixels, as a `kurbo::Size`.
    fn window_size_px(&self) -> Size {
        self.gpu.as_ref().map_or(Size::ZERO, |g| {
            Size::new(f64::from(g.config.width), f64::from(g.config.height))
        })
    }

    /// Apply a view action: updates viewport state (and/or sends a page command).
    /// Runs on the main thread after egui; safe to mutate `self.viewport` directly.
    fn apply_view_action(&mut self, action: ViewAction, page: aj_core::Page) {
        match action {
            ViewAction::ZoomIn => self.zoom_by(ZOOM_STEP),
            ViewAction::ZoomOut => self.zoom_by(1.0 / ZOOM_STEP),
            ViewAction::ZoomTo100 => {
                self.viewport.zoom_to(1.0, self.window_size_px(), self.dpi_scale);
            }
            ViewAction::ZoomToFit => {
                self.viewport.zoom_to_fit(self.window_size_px(), page.size, self.dpi_scale);
            }
            ViewAction::ResetView => self.viewport.reset(),
            ViewAction::TogglePageBounds => {
                self.send(Command::SetShowBounds(!page.show_bounds));
            }
            ViewAction::ToggleClipToBounds => {
                self.send(Command::SetClipToBounds(!page.clip_to_bounds));
            }
            ViewAction::ToggleBrushPanel => {
                self.brush_panel_visible = !self.brush_panel_visible;
            }
        }
        self.request_redraw();
    }

    /// Zoom by a factor, anchored at the current cursor position if available,
    /// otherwise at the window center.
    fn zoom_by(&mut self, factor: f64) {
        let anchor = self.cursor.map_or_else(
            || {
                let w = self.window_size_px();
                Point::new(w.width / 2.0, w.height / 2.0)
            },
            |c| Point::new(c.x, c.y),
        );
        self.viewport.zoom_at_cursor(anchor, factor, self.dpi_scale);
    }

    /// Handle a mouse-wheel event: Ctrl+scroll is zoom, plain scroll is pan. Gated
    /// against chrome hover in the caller.
    fn handle_wheel(&mut self, delta: MouseScrollDelta) {
        let ctrl_held = if cfg!(target_os = "macos") {
            self.modifiers.control_key() || self.modifiers.super_key()
        } else {
            self.modifiers.control_key()
        };
        // Normalize both delta variants into "line-equivalent" units (for zoom
        // ratcheting) and physical pixels (for pan).
        let (lines_y, pan_px) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (
                f64::from(y),
                Vec2::new(f64::from(x) * PIXELS_PER_LINE, f64::from(y) * PIXELS_PER_LINE),
            ),
            MouseScrollDelta::PixelDelta(p) => (p.y / PIXELS_PER_ZOOM_LINE, Vec2::new(p.x, p.y)),
        };
        if ctrl_held {
            let factor = ZOOM_STEP.powf(lines_y * ZOOM_PER_LINE);
            self.zoom_by(factor);
        } else {
            // Natural scroll: wheel-down (positive y) reveals content below =
            // translate world origin up = translate_pt.y increases.
            self.viewport.pan(pan_px, self.dpi_scale);
        }
        self.request_redraw();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("art-junk")
            .with_inner_size(LogicalSize::new(1280, 800));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(err) => {
                log::error!("create_window failed: {err:?}");
                event_loop.exit();
                return;
            }
        };
        if let Err(err) = self.init(window) {
            log::error!("app init failed: {err:?}");
            event_loop.exit();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // egui sees the event first so it can update hover/focus state.
        let egui_consumed =
            if let (Some(chrome), Some(window)) = (self.chrome.as_mut(), self.window.as_ref()) {
                let response = chrome.winit_state.on_window_event(window, &event);
                if response.repaint {
                    window.request_redraw();
                }
                response.consumed
            } else {
                false
            };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => self.on_resized(size),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.dpi_scale = scale_factor;
                self.request_redraw();
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::KeyboardInput { event: ref key_event, .. } => {
                self.on_keyboard(egui_consumed, key_event);
            }
            WindowEvent::CursorMoved { position, .. } => {
                // Keep the raw cursor for zoom-anchor use even when egui owns input;
                // the stylus adapter is fed separately below.
                self.cursor = Some(position);
                self.route_input(egui_consumed, &event);
            }
            WindowEvent::MouseInput { .. }
            | WindowEvent::Touch(_)
            | WindowEvent::CursorLeft { .. } => {
                self.route_input(egui_consumed, &event);
            }
            WindowEvent::MouseWheel { delta, .. }
                if !egui_consumed && !self.pointer_over_chrome() =>
            {
                self.handle_wheel(delta);
            }
            // delta may be NaN per winit docs — guard before it propagates into
            // translate_pt and silently breaks the viewport.
            WindowEvent::PinchGesture { delta, .. }
                if !egui_consumed && !self.pointer_over_chrome() && delta.is_finite() =>
            {
                // macOS reports small per-tick deltas (~0.01); `1 + delta` is the
                // natural zoom factor (positive = pinch-out = zoom in).
                self.zoom_by(1.0 + delta);
            }
            WindowEvent::RedrawRequested => {
                if let Err(err) = self.render() {
                    log::error!("render: {err:?}");
                }
            }
            _ => {}
        }
    }
}

impl App {
    fn on_resized(&mut self, size: winit::dpi::PhysicalSize<u32>) {
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.resize(size);
        }
        self.request_redraw();
    }

    fn on_keyboard(&mut self, egui_consumed: bool, key_event: &winit::event::KeyEvent) {
        if egui_consumed || key_event.state != ElementState::Pressed || key_event.repeat {
            return;
        }
        let Some(app_action) =
            shortcuts::resolve(&key_event.logical_key, key_event.physical_key, self.modifiers)
        else {
            return;
        };
        match app_action {
            AppAction::Edit(action) => {
                if let Some(engine) = self.engine.as_ref() {
                    let snap = engine.snapshot();
                    if action.enabled(snap.history) {
                        action.dispatch(engine);
                        self.request_redraw();
                    }
                }
            }
            AppAction::View(view_action) => {
                let page =
                    self.engine.as_ref().map(|e| e.snapshot().scene.page).unwrap_or_default();
                self.apply_view_action(view_action, page);
            }
            AppAction::Brush(brush_action) => {
                if let Some(engine) = self.engine.as_ref() {
                    let snap = engine.snapshot();
                    brush_action.dispatch(engine, snap.scene.brush);
                    self.request_redraw();
                }
            }
        }
    }

    /// Feed an input event to the stylus adapter, then dispatch resulting
    /// stylus events to the engine. Chrome ownership (egui-consumed or pointer-
    /// over-chrome) gates *new* strokes, but an in-progress stroke always runs
    /// to completion — otherwise dragging the cursor across the menu bar mid-
    /// stroke would strand the stroke forever.
    fn route_input(&mut self, egui_consumed: bool, event: &WindowEvent) {
        let chrome_owns_input = egui_consumed || self.pointer_over_chrome();
        // Scope the borrow narrowly: if the NSEvent monitor fires while this
        // borrow is held, its `try_borrow_mut` would fail and the sample
        // would be logged and dropped. Releasing the borrow before the
        // dispatch loop below (which runs user code) avoids that collision.
        let events: Vec<StylusEvent> = {
            let mut adapter = self.stylus.borrow_mut();
            if chrome_owns_input && !adapter.is_tracking_pointer() {
                return;
            }
            adapter.on_window_event(event);
            adapter.drain().collect()
        };
        for stylus_event in events {
            self.on_stylus_event(stylus_event);
        }
    }

    fn on_stylus_event(&mut self, ev: StylusEvent) {
        match ev {
            StylusEvent::Sample { sample, phase, caps } => {
                self.on_sample(sample, phase, caps);
            }
            StylusEvent::Revise { pointer_id, update_index, revision } => {
                if let Some((pid, id)) = self.active
                    && pid == pointer_id
                {
                    self.send(Command::ReviseSample { stroke_id: id, update_index, revision });
                }
            }
            // `#[non_exhaustive]`: future variants (predicted samples, bulk
            // revisions) land without breaking this match.
            _ => {}
        }
    }

    fn on_sample(&mut self, mut sample: aj_core::Sample, phase: Phase, caps: aj_core::ToolCaps) {
        let world = self.viewport.screen_to_world(sample.position.into(), self.dpi_scale);
        sample.position = world.into();
        let pointer_id = sample.pointer_id;

        match phase {
            Phase::Down => {
                let id = StrokeId::next();
                self.active = Some((pointer_id, id));
                // Freeze the current live brush into this stroke. Subsequent
                // slider / shortcut changes won't alter what this stroke
                // renders with; only the next BeginStroke picks up changes.
                let brush =
                    self.engine.as_ref().map(|e| e.snapshot().scene.brush).unwrap_or_default();
                self.send(Command::BeginStroke { id, sample, caps, brush });
                self.request_redraw();
            }
            Phase::Move => {
                if let Some((pid, id)) = self.active
                    && pid == pointer_id
                {
                    self.send(Command::AddSample { id, sample });
                    self.request_redraw();
                }
            }
            Phase::Up | Phase::Cancel => {
                if let Some((pid, id)) = self.active
                    && pid == pointer_id
                {
                    self.send(Command::EndStroke { id });
                    self.active = None;
                    self.request_redraw();
                }
            }
            // `Phase::Hover` isn't emitted in Milestone 1 (no pen-proximity
            // backends yet); when a pen backend lands, a dedicated arm can drive
            // cursor/brush-preview UI without starting a stroke. `Phase` is
            // `#[non_exhaustive]`, so future variants also fall through here.
            _ => {}
        }
    }
}

fn main() -> Result<()> {
    // Quiet by default: warnings-and-above from dependencies, info-and-above from our own
    // crates. Opt in to verbose third-party logging with e.g. `RUST_LOG=info` or target
    // specific crates with `RUST_LOG=wgpu_core=debug,aj_engine=trace`.
    let default_filter = "warn,aj_app=info,aj_core=info,aj_engine=info,aj_render=info";
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter))
        .init();
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App::new();
    event_loop.run_app(&mut app)?;
    Ok(())
}
