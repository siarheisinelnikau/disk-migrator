//! Reactive CPU host: egui meshes -> software rasterizer -> softbuffer Win32 GDI.
//! No UI work while minimized/occluded; worker notifications retain dirty state.
use crate::app::{App, AppEvent};
use egui_software_backend::{BufferMutRef, ColorFieldOrder, EguiSoftwareRender};
use std::{num::NonZeroU32, rc::Rc, time::Instant};
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    window::{Window, WindowId},
};

pub enum Event {
    Repaint(Instant),
    App(AppEvent),
    Accessibility(egui_winit::accesskit_winit::Event),
}
impl From<egui_winit::accesskit_winit::Event> for Event {
    fn from(e: egui_winit::accesskit_winit::Event) -> Self {
        Self::Accessibility(e)
    }
}
#[derive(Default)]
struct Counters {
    layout: u64,
    raster: u64,
    present: u64,
}
struct Host {
    proxy: EventLoopProxy<Event>,
    app: App,
    window: Option<Rc<Window>>,
    state: Option<egui_winit::State>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    ctx: egui::Context,
    renderer: EguiSoftwareRender,
    visible: bool,
    occluded: bool,
    dirty: bool,
    deadline: Option<Instant>,
    closing: bool,
    counters: Counters,
}
impl Host {
    fn request(&mut self) {
        self.dirty = true;
        if self.visible
            && let Some(w) = &self.window
        {
            w.request_redraw();
        }
    }
    fn draw(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.visible {
            return Ok(());
        }
        let window = self.window.as_ref().unwrap();
        let size = window.inner_size();
        let (Some(width), Some(height)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return Ok(());
        };
        self.dirty = false;
        self.deadline = None;
        let state = self.state.as_mut().unwrap();
        let input = state.take_egui_input(window);
        let output = self.ctx.run(input, |ctx| self.app.ui(ctx));
        self.counters.layout += 1;
        state.handle_platform_output(window, output.platform_output);
        let primitives = self.ctx.tessellate(output.shapes, output.pixels_per_point);
        self.renderer.render_to_canvas(
            size.width as usize,
            size.height as usize,
            &primitives,
            &output.textures_delta,
            output.pixels_per_point,
        );
        self.counters.raster += 1;
        let surface = self.surface.as_mut().unwrap();
        surface.resize(width, height)?;
        let mut buffer = surface.buffer_mut()?;
        let pixels: &mut [[u8; 4]] = bytemuck::cast_slice_mut(&mut buffer);
        let mut target = BufferMutRef::new(pixels, size.width as usize, size.height as usize);
        self.renderer.blit_canvas_to_buffer(&mut target);
        buffer.present()?;
        self.counters.present += 1;
        // Consume only the next useful deadline. There is no unconditional redraw or polling.
        if let Some(root) = output.viewport_output.get(&egui::ViewportId::ROOT)
            && let Some(deadline) = Instant::now().checked_add(root.repaint_delay)
        {
            self.deadline = Some(deadline);
        }
        Ok(())
    }
}
impl ApplicationHandler<Event> for Host {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let icon = winit::window::Icon::from_rgba(
            include_bytes!("../assets/window.rgba").to_vec(),
            64,
            64,
        )
        .ok();
        let attrs = Window::default_attributes()
            .with_title("Disk Migrator")
            .with_visible(false)
            .with_window_icon(icon)
            .with_inner_size(winit::dpi::LogicalSize::new(1040, 800))
            .with_min_inner_size(winit::dpi::LogicalSize::new(780, 620));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                eprintln!("Window: {e}");
                event_loop.exit();
                return;
            }
        };
        let mut state = egui_winit::State::new(
            self.ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        );
        state.init_accesskit(event_loop, window.as_ref(), self.proxy.clone());
        let surface = softbuffer::Context::new(window.clone())
            .and_then(|c| softbuffer::Surface::new(&c, window.clone()));
        match surface {
            Ok(s) => self.surface = Some(s),
            Err(e) => {
                eprintln!("Software surface: {e}");
                event_loop.exit();
                return;
            }
        }
        self.state = Some(state);
        self.window = Some(window.clone());
        self.visible = true;
        window.set_visible(true);
        self.request();
        self.app.startup();
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
        match event {
            Event::Repaint(deadline) => {
                if self.visible {
                    self.deadline = Some(self.deadline.map_or(deadline, |d| d.min(deadline)));
                }
            }
            Event::App(e) => {
                self.app.on_event(e);
                self.request();
            }
            Event::Accessibility(e) => {
                use egui_winit::accesskit_winit::WindowEvent as A;
                match e.window_event {
                    A::InitialTreeRequested => self.ctx.enable_accesskit(),
                    A::ActionRequested(request) => {
                        if let Some(s) = &mut self.state {
                            s.on_accesskit_action_request(request);
                        }
                    }
                    A::AccessibilityDeactivated => {}
                }
                self.request();
            }
        }
        if self.closing && !self.app.busy() {
            event_loop.exit();
        }
        if self.app.exit_requested {
            event_loop.exit();
        }
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if self.window.as_ref().is_none_or(|w| w.id() != id) {
            return;
        }
        match &event {
            WindowEvent::CloseRequested => {
                if self.app.busy() {
                    self.closing = true;
                    self.app.request_close();
                    self.request();
                } else {
                    event_loop.exit();
                }
                return;
            }
            WindowEvent::Occluded(hidden) => {
                self.occluded = *hidden;
                self.visible = !*hidden
                    && !self
                        .window
                        .as_ref()
                        .unwrap()
                        .is_minimized()
                        .unwrap_or(false);
                self.deadline = None;
                if self.visible {
                    self.request();
                }
            }
            WindowEvent::Resized(size) => {
                self.visible = size.width > 0
                    && size.height > 0
                    && !self.occluded
                    && !self
                        .window
                        .as_ref()
                        .unwrap()
                        .is_minimized()
                        .unwrap_or(false);
                self.deadline = None;
                self.request();
            }
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.draw() {
                    eprintln!("Draw: {e}");
                    self.app.request_close();
                    event_loop.exit();
                }
                return;
            }
            _ => {}
        }
        let response = self
            .state
            .as_mut()
            .unwrap()
            .on_window_event(self.window.as_ref().unwrap(), &event);
        if response.repaint {
            self.request();
        }
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if !self.visible {
            self.deadline = None;
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }
        if self.dirty
            && let Some(w) = &self.window
        {
            w.request_redraw();
        }
        if let Some(deadline) = self.deadline {
            if deadline <= Instant::now() {
                self.deadline = None;
                self.request();
                event_loop.set_control_flow(ControlFlow::Wait);
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
            }
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
    fn exiting(&mut self, _: &ActiveEventLoop) {
        self.app.request_close();
        if std::env::var_os("DISK_MIGRATOR_RENDER_DIAGNOSTICS").is_some() {
            eprintln!(
                "backend=cpu-gdi layout={} raster={} present={}",
                self.counters.layout, self.counters.raster, self.counters.present
            );
        }
    }
}
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::<Event>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let ctx = egui::Context::default();
    let repaint_proxy = proxy.clone();
    ctx.set_request_repaint_callback(move |r| {
        if let Some(deadline) = Instant::now().checked_add(r.delay) {
            let _ = repaint_proxy.send_event(Event::Repaint(deadline));
        }
    });
    let app = App::new(proxy.clone());
    let mut host = Host {
        proxy,
        app,
        window: None,
        state: None,
        surface: None,
        ctx,
        renderer: EguiSoftwareRender::new(ColorFieldOrder::Bgra),
        visible: false,
        occluded: false,
        dirty: true,
        deadline: None,
        closing: false,
        counters: Counters::default(),
    };
    event_loop.run_app(&mut host)?;
    Ok(())
}
