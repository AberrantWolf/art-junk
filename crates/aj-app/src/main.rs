use std::sync::Arc;

use aj_core::{Point, StrokeId};
use aj_engine::{Command, Engine};
use aj_render::Renderer;
use anyhow::{Context, Result};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

struct GpuState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
}

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<GpuState>,
    renderer: Option<Renderer>,
    engine: Option<Engine>,
    cursor: Option<PhysicalPosition<f64>>,
    active_stroke: Option<StrokeId>,
    mouse_down: bool,
}

impl App {
    fn new() -> Self {
        Self {
            window: None,
            gpu: None,
            renderer: None,
            engine: None,
            cursor: None,
            active_stroke: None,
            mouse_down: false,
        }
    }

    fn init_gpu(&mut self, window: &Arc<Window>) -> Result<()> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let surface = instance.create_surface(window.clone()).context("create_surface failed")?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .context("no suitable GPU adapter")?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("aj-app device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .context("request_device failed")?;

        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let format = [wgpu::TextureFormat::Rgba8Unorm, wgpu::TextureFormat::Bgra8Unorm]
            .into_iter()
            .find(|f| caps.formats.contains(f))
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: caps.present_modes[0],
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        self.gpu = Some(GpuState { surface, device, queue, config });
        Ok(())
    }

    fn resize(&mut self, new_size: PhysicalSize<u32>) {
        let Some(gpu) = self.gpu.as_mut() else { return };
        gpu.config.width = new_size.width.max(1);
        gpu.config.height = new_size.height.max(1);
        gpu.surface.configure(&gpu.device, &gpu.config);
    }

    fn render(&mut self) -> Result<()> {
        let Some(gpu) = self.gpu.as_mut() else { return Ok(()) };
        let Some(renderer) = self.renderer.as_mut() else { return Ok(()) };
        let Some(engine) = self.engine.as_ref() else { return Ok(()) };

        let frame = match gpu.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                return Ok(());
            }
            Err(err) => return Err(anyhow::anyhow!("surface error: {err:?}")),
        };
        let snapshot = engine.snapshot();
        renderer.render(
            &gpu.device,
            &gpu.queue,
            &snapshot,
            &frame,
            gpu.config.width,
            gpu.config.height,
        )?;
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
        if let Err(err) = self.init_gpu(&window) {
            log::error!("gpu init failed: {err:?}");
            event_loop.exit();
            return;
        }
        let Some(gpu) = self.gpu.as_ref() else {
            event_loop.exit();
            return;
        };
        match Renderer::new(&gpu.device, gpu.config.format) {
            Ok(r) => self.renderer = Some(r),
            Err(err) => {
                log::error!("renderer init failed: {err:?}");
                event_loop.exit();
                return;
            }
        }
        self.engine = Some(Engine::spawn());
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                self.resize(size);
                self.request_redraw();
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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App::new();
    event_loop.run_app(&mut app)?;
    Ok(())
}
