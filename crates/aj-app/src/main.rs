mod compose;
mod gpu;
mod ui;

use std::sync::Arc;

use aj_core::{Point, StrokeId};
use aj_engine::{Command, Engine};
use aj_render::Renderer;
use anyhow::Result;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use crate::compose::Chrome;
use crate::gpu::GpuState;
use crate::ui::{Action, draw_menu_bar, match_action};

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<GpuState>,
    renderer: Option<Renderer>,
    chrome: Option<Chrome>,
    engine: Option<Engine>,
    cursor: Option<PhysicalPosition<f64>>,
    active_stroke: Option<StrokeId>,
    mouse_down: bool,
    modifiers: ModifiersState,
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
            active_stroke: None,
            mouse_down: false,
            modifiers: ModifiersState::empty(),
        }
    }

    fn init(&mut self, window: Arc<Window>) -> Result<()> {
        let gpu = GpuState::new(&window)?;
        let renderer = Renderer::new(&gpu.device, gpu.config.format)?;
        let chrome = Chrome::new(&gpu, &window);
        self.gpu = Some(gpu);
        self.renderer = Some(renderer);
        self.chrome = Some(chrome);
        self.engine = Some(Engine::spawn());
        self.window = Some(window);
        Ok(())
    }

    fn render(&mut self) -> Result<()> {
        let Some(gpu) = self.gpu.as_mut() else { return Ok(()) };
        let Some(renderer) = self.renderer.as_mut() else { return Ok(()) };
        let Some(chrome) = self.chrome.as_mut() else { return Ok(()) };
        let Some(engine) = self.engine.as_ref() else { return Ok(()) };
        let Some(window) = self.window.as_ref() else { return Ok(()) };

        let frame = match gpu.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                return Ok(());
            }
            Err(err) => return Err(anyhow::anyhow!("surface error: {err:?}")),
        };

        let app_snapshot = engine.snapshot();

        // 1. Run egui for the frame and collect any menu-triggered actions.
        let raw_input = chrome.winit_state.take_egui_input(window);
        let mut pending_actions: Vec<Action> = Vec::new();
        let full_output = chrome.ctx.run(raw_input, |ctx| {
            draw_menu_bar(ctx, app_snapshot.history, &mut pending_actions);
        });
        for action in pending_actions {
            action.dispatch(engine);
        }

        // 2. Vello paints the scene into the surface texture (own submit).
        renderer.render(
            &gpu.device,
            &gpu.queue,
            &app_snapshot.scene,
            &frame,
            gpu.config.width,
            gpu.config.height,
        )?;

        // 3. egui overlays chrome on the same surface texture.
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
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.resize(size);
                }
                self.request_redraw();
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::KeyboardInput { event: key_event, .. } => {
                if egui_consumed || key_event.state != ElementState::Pressed || key_event.repeat {
                    return;
                }
                if let Some(action) = match_action(&key_event.logical_key, self.modifiers)
                    && let Some(engine) = self.engine.as_ref()
                {
                    let snap = engine.snapshot();
                    if action.enabled(snap.history) {
                        action.dispatch(engine);
                        self.request_redraw();
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Some(position);
                if self.mouse_down
                    && let Some(id) = self.active_stroke
                {
                    self.send(Command::AddSample { id, point: Point::new(position.x, position.y) });
                    self.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                if egui_consumed || self.pointer_over_chrome() {
                    return;
                }
                if let Some(pos) = self.cursor {
                    let id = StrokeId::next();
                    self.active_stroke = Some(id);
                    self.mouse_down = true;
                    self.send(Command::BeginStroke { id, point: Point::new(pos.x, pos.y) });
                    self.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                if let Some(id) = self.active_stroke.take() {
                    self.send(Command::EndStroke { id });
                }
                self.mouse_down = false;
                self.request_redraw();
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
