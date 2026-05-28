use glyphon::{
    Attrs, Buffer, Cache, FontSystem, Metrics, Resolution, SwashCache, TextArea, TextAtlas,
    TextBounds, TextRenderer, Viewport,
};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm, delegate_xdg_shell, delegate_xdg_window, delegate_output,
    registry::{ProvidesRegistryState, RegistryState},
    output::{OutputHandler, OutputState},
    seat::{
        keyboard::KeyboardHandler,
        pointer::PointerHandler,
        Capability, SeatHandler, SeatState,
    },
    shell::{
        xdg::{
            window::{Window as XdgWindow, WindowConfigure, WindowHandler, WindowDecorations},
            XdgShell,
        },
        WaylandSurface,
    },
    shm::{Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface},
    Connection, QueueHandle, Proxy,
};
use calloop::{EventLoop, channel};
use calloop_wayland_source::WaylandSource;

use clear_ui::widget::{
    Button, ContentBg, TextLabel, Widget, ElementState, MouseButton, Key, NamedKey, KeyEvent, TextBox
};

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    position: [f32; 2],
    color: [f32; 4],
}

impl Vertex {
    const ATTRIBS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![
        0 => Float32x2,
        1 => Float32x4,
    ];

    fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBS,
        }
    }
}

fn quad_vertices(
    x: f32, y: f32, w: f32, h: f32,
    surface_w: f32, surface_h: f32,
    color: [f32; 4],
) -> [Vertex; 6] {
    let x0 = (x / surface_w) * 2.0 - 1.0;
    let y0 = 1.0 - (y / surface_h) * 2.0;
    let x1 = ((x + w) / surface_w) * 2.0 - 1.0;
    let y1 = 1.0 - ((y + h) / surface_h) * 2.0;

    [
        Vertex { position: [x0, y0], color },
        Vertex { position: [x1, y0], color },
        Vertex { position: [x0, y1], color },
        Vertex { position: [x1, y0], color },
        Vertex { position: [x1, y1], color },
        Vertex { position: [x0, y1], color },
    ]
}

fn widget_vertices(w: &dyn Widget, sw: f32, sh: f32) -> Vec<Vertex> {
    let (x, y, ww, h) = w.rect();
    quad_vertices(x, y, ww, h, sw, sh, w.color()).to_vec()
}

fn make_text_buffer(font_system: &mut FontSystem, text: &str, size: f32) -> Buffer {
    let metrics = Metrics::new(size, size * 1.4);
    let mut buffer = Buffer::new(font_system, metrics);
    buffer.set_text(font_system, text, Attrs::new(), glyphon::Shaping::Advanced);
    buffer.shape_until_scroll(font_system, true);
    buffer
}



// ── Custom LoginCard Container Widget ──
#[derive(Debug, Clone)]
struct LoginCard {
    x: f32, y: f32, w: f32, h: f32,
}

impl LoginCard {
    fn new() -> Self {
        Self { x: 0.0, y: 0.0, w: 0.0, h: 0.0 }
    }
}

impl Widget for LoginCard {
    fn rect(&self) -> (f32, f32, f32, f32) { (self.x, self.y, self.w, self.h) }
    fn set_rect(&mut self, x: f32, y: f32, w: f32, h: f32) { self.x = x; self.y = y; self.w = w; self.h = h; }
    fn color(&self) -> [f32; 4] { [0.07, 0.07, 0.10, 0.90] } // Sleek dark card background

    fn extra_quads(&self) -> Vec<(f32, f32, f32, f32, [f32; 4])> {
        let border_color = [0.20, 0.40, 0.65, 0.6]; // Premium blue border accent
        vec![
            // Top border
            (self.x, self.y, self.w, 1.5, border_color),
            // Bottom border
            (self.x, self.y + self.h - 1.5, self.w, 1.5, border_color),
            // Left border
            (self.x, self.y, 1.5, self.h, border_color),
            // Right border
            (self.x + self.w - 1.5, self.y, 1.5, self.h, border_color),
        ]
    }

    fn text_labels(&self) -> Vec<TextLabel> {
        vec![
            TextLabel {
                text: "CLEAR DISPLAY MANAGER".to_string(),
                x: self.x + 30.0,
                y: self.y + 30.0,
                font_size: 15.0,
                color: [0xee, 0xee, 0xf5],
            },
            TextLabel {
                text: "Authenticate to begin your session".to_string(),
                x: self.x + 30.0,
                y: self.y + 50.0,
                font_size: 11.0,
                color: [0x83, 0x83, 0x8a],
            },
        ]
    }
}

// ── Custom Status Message Indicator ──
#[derive(Debug, Clone)]
struct StatusLabel {
    x: f32, y: f32, w: f32, h: f32,
    pub text: String,
    pub is_error: bool,
}

impl StatusLabel {
    fn new(text: String) -> Self {
        Self { x: 0.0, y: 0.0, w: 0.0, h: 0.0, text, is_error: false }
    }
}

impl Widget for StatusLabel {
    fn rect(&self) -> (f32, f32, f32, f32) { (self.x, self.y, self.w, self.h) }
    fn set_rect(&mut self, x: f32, y: f32, w: f32, h: f32) { self.x = x; self.y = y; self.w = w; self.h = h; }
    fn color(&self) -> [f32; 4] { [0.0, 0.0, 0.0, 0.0] } // Transparent background

    fn text_labels(&self) -> Vec<TextLabel> {
        let col = if self.is_error {
            [0xee, 0x5c, 0x5c] // Soft red
        } else {
            [0x83, 0x83, 0x8a] // Dim text
        };
        vec![TextLabel {
            text: self.text.clone(),
            x: self.x,
            y: self.y,
            font_size: 11.0,
            color: col,
        }]
    }
}

// ── App State and Renderer ──
struct State {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    render_pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    vertex_count: u32,

    bg: ContentBg,
    card: LoginCard,
    username_box: TextBox,
    password_box: TextBox,
    session_btn: Button,
    login_btn: Button,
    status_lbl: StatusLabel,

    font_system: FontSystem,
    swash_cache: SwashCache,
    text_atlas: TextAtlas,
    text_renderer: TextRenderer,
    text_viewport: Viewport,

    info_buffer: Buffer,

    cursor_x: f32,
    cursor_y: f32,

    width: f32,
    height: f32,
    physical_width: u32,
    physical_height: u32,
    scale: f64,

    // Sessions and State tracking
    sessions: Vec<&'static str>,
    session_idx: usize,
    login_success: bool,
    is_authenticating: bool,
}

impl State {
    async fn new(wayland_handle: &'static clear_ui::wayland::WaylandSurfaceHandle, pw: u32, ph: u32, scale: f64) -> Self {
        let lw = pw as f32 / scale as f32;
        let lh = ph as f32 / scale as f32;

        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..Default::default()
        });

        let surface = instance.create_surface(wayland_handle).expect("wgpu surface");
        let adapter = instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }).await.expect("adapter");

        let (device, queue) = adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("GPU Device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits()),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
        }, None).await.expect("device");

        let mut config = surface.get_default_config(&adapter, pw, ph).expect("config");
        config.present_mode = wgpu::PresentMode::Fifo;
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Shader"),
            source: wgpu::ShaderSource::Wgsl(clear_ui::SHADER.into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Pipeline Layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });

        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Render Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[Vertex::desc()],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState { count: 1, mask: !0, alpha_to_coverage_enabled: false },
            multiview: None,
            cache: None,
        });

        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let mut text_atlas = TextAtlas::new(&device, &queue, &cache, config.format);
        let text_renderer = TextRenderer::new(&mut text_atlas, &device, wgpu::MultisampleState::default(), None);

        let mut text_viewport = Viewport::new(&device, &cache);
        text_viewport.update(&queue, Resolution { width: pw, height: ph });

        let info_buffer = make_text_buffer(&mut font_system, "Press Tab to switch fields • Session selector: Click current session label", 11.0);

        // Prepopulate username if environment has USER
        let current_user = std::env::var("USER").unwrap_or_else(|_| "clear".to_string());

        let bg = ContentBg::new();
        let card = LoginCard::new();
        let username_box = TextBox::new(current_user).with_label("USERNAME");
        let password_box = TextBox::new(String::new()).with_password(true).with_label("PASSWORD");
        let session_btn = Button::new(0.0, 0.0, 130.0, 32.0).with_label("Session: River WM");
        let login_btn = Button::new(0.0, 0.0, 130.0, 32.0).with_label("Log In");
        let status_lbl = StatusLabel::new("Enter password to start".to_string());

        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Vertex Buffer"),
            size: 1,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let mut state = Self {
            surface,
            device,
            queue,
            config,
            render_pipeline,
            vertex_buffer,
            vertex_count: 0,
            bg,
            card,
            username_box,
            password_box,
            session_btn,
            login_btn,
            status_lbl,
            font_system,
            swash_cache,
            text_atlas,
            text_renderer,
            text_viewport,
            info_buffer,
            cursor_x: 0.0,
            cursor_y: 0.0,
            width: lw,
            height: lh,
            physical_width: pw,
            physical_height: ph,
            scale,
            sessions: vec!["River WM", "Bash Shell"],
            session_idx: 0,
            login_success: false,
            is_authenticating: false,
        };

        state.apply_layout();
        state.upload_vertices();
        state
    }

    fn widgets_iter(&self) -> Vec<&dyn Widget> {
        vec![
            &self.bg,
            &self.card,
            &self.username_box,
            &self.password_box,
            &self.session_btn,
            &self.login_btn,
            &self.status_lbl,
        ]
    }

    fn widgets_iter_mut(&mut self) -> Vec<&mut dyn Widget> {
        vec![
            &mut self.bg,
            &mut self.card,
            &mut self.username_box,
            &mut self.password_box,
            &mut self.session_btn,
            &mut self.login_btn,
            &mut self.status_lbl,
        ]
    }

    fn apply_layout(&mut self) {
        let sw = self.width;
        let sh = self.height;

        // Background spans the whole screen
        self.bg.set_rect(0.0, 0.0, sw, sh);

        // Center card configuration
        let card_w = 360.0;
        let card_h = 280.0;
        let card_x = (sw - card_w) / 2.0;
        let card_y = (sh - card_h) / 2.0;
        self.card.set_rect(card_x, card_y, card_w, card_h);

        // Child components inside login card
        let content_x = card_x + 30.0;
        
        // Username text box
        self.username_box.set_rect(content_x, card_y + 80.0, 300.0, 36.0);
        
        // Password password box
        self.password_box.set_rect(content_x, card_y + 145.0, 300.0, 36.0);

        // Session toggler button
        self.session_btn.set_rect(content_x, card_y + 205.0, 140.0, 32.0);

        // Login button
        self.login_btn.set_rect(content_x + 160.0, card_y + 205.0, 140.0, 32.0);

        // Status message
        self.status_lbl.set_rect(content_x, card_y + 252.0, 300.0, 20.0);
    }

    fn collect_vertices(&self) -> Vec<Vertex> {
        let sw = self.width;
        let sh = self.height;
        let mut verts = Vec::new();
        for w in self.widgets_iter() {
            verts.extend(widget_vertices(w, sw, sh));
            for (qx, qy, qw, qh, qc) in w.extra_quads() {
                verts.extend(quad_vertices(qx, qy, qw, qh, sw, sh, qc));
            }
        }
        verts
    }

    fn upload_vertices(&mut self) {
        let verts = self.collect_vertices();
        self.vertex_count = verts.len() as u32;
        if self.vertex_count == 0 { return; }
        
        let data = bytemuck::cast_slice(&verts);
        let needed = data.len() as wgpu::BufferAddress;
        if needed > self.vertex_buffer.size() {
            self.vertex_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("Vertex Buffer"),
                size: needed,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        self.queue.write_buffer(&self.vertex_buffer, 0, data);
    }

    fn prepare_text(&mut self) {
        let scale_f32 = self.scale as f32;

        let viewport = Resolution { width: self.physical_width, height: self.physical_height };
        self.text_viewport.update(&self.queue, viewport);

        let mut areas: Vec<TextArea> = vec![
            TextArea {
                buffer: &self.info_buffer,
                left: 20.0 * scale_f32,
                top: self.physical_height as f32 - 24.0 * scale_f32,
                scale: scale_f32,
                bounds: TextBounds {
                    left: 0, top: 0,
                    right: self.physical_width as i32,
                    bottom: self.physical_height as i32,
                },
                default_color: glyphon::Color::rgb(0x60, 0x60, 0x6e),
                custom_glyphs: &[],
            }
        ];

        let mut widget_labels: Vec<TextLabel> = Vec::new();
        for w in self.widgets_iter() {
            widget_labels.extend(w.text_labels());
        }

        let mut widget_buffers: Vec<Buffer> = Vec::new();
        for label in &widget_labels {
            widget_buffers.push(make_text_buffer(&mut self.font_system, &label.text, label.font_size));
        }

        for (buf, label) in widget_buffers.iter().zip(widget_labels.iter()) {
            areas.push(TextArea {
                buffer: buf,
                left: label.x * scale_f32,
                top: label.y * scale_f32,
                scale: scale_f32,
                bounds: TextBounds {
                    left: 0, top: 0,
                    right: self.physical_width as i32,
                    bottom: self.physical_height as i32,
                },
                default_color: glyphon::Color::rgb(label.color[0], label.color[1], label.color[2]),
                custom_glyphs: &[],
            });
        }

        self.text_renderer
            .prepare(&self.device, &self.queue, &mut self.font_system, &mut self.text_atlas, &self.text_viewport, areas, &mut self.swash_cache)
            .unwrap();
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width > 0 && height > 0 {
            self.physical_width = width;
            self.physical_height = height;
            self.width = width as f32 / self.scale as f32;
            self.height = height as f32 / self.scale as f32;
            self.config.width = width;
            self.config.height = height;
            self.surface.configure(&self.device, &self.config);
            self.apply_layout();
            self.upload_vertices();
        }
    }

    fn render(&mut self) {
        self.prepare_text();

        let output = match self.surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            Err(wgpu::SurfaceError::Timeout) => return,
            Err(e) => {
                eprintln!("Surface error: {e:?}");
                return;
            }
        };

        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Encoder"),
        });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.03,
                            g: 0.03,
                            b: 0.05,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            if self.vertex_count > 0 {
                pass.set_pipeline(&self.render_pipeline);
                pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
                pass.draw(0..self.vertex_count, 0..1);
            }

            self.text_renderer
                .render(&self.text_atlas, &self.text_viewport, &mut pass)
                .unwrap();
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();
    }
}

#[derive(Debug)]
enum AuthEvent {
    Success { username: String },
    Failure(String),
}

fn authenticate_user(username: String, password: String, sender: channel::Sender<AuthEvent>) {
    std::thread::spawn(move || {
        let service = "clear-display-manager";
        
        let mut auth = match pam::Authenticator::with_password(service) {
            Ok(a) => a,
            Err(_) => {
                match pam::Authenticator::with_password("login") {
                    Ok(a) => a,
                    Err(e) => {
                        let _ = sender.send(AuthEvent::Failure(format!("PAM Init Error: {}", e)));
                        return;
                    }
                }
            }
        };

        auth.get_handler().set_credentials(username.clone(), password);

        match auth.authenticate() {
            Ok(_) => {
                match auth.open_session() {
                    Ok(_) => {
                        let _ = sender.send(AuthEvent::Success { username });
                    }
                    Err(e) => {
                        let _ = sender.send(AuthEvent::Failure(format!("PAM Session Error: {}", e)));
                    }
                }
            }
            Err(e) => {
                let _ = sender.send(AuthEvent::Failure(format!("Authentication failed: {}", e)));
            }
        }
    });
}

// ── AppState and Client Callbacks ──
struct AppState {
    registry_state: RegistryState,
    compositor_state: CompositorState,
    xdg_shell_state: XdgShell,
    shm_state: Shm,
    seat_state: SeatState,
    output_state: OutputState,

    seats: Vec<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,

    window: Option<XdgWindow>,
    surface: Option<wl_surface::WlSurface>,

    state: Option<State>,
    exit: bool,
    redraw: bool,
    ctrl_pressed: bool,
    shift_pressed: bool,
    pressed_key: Option<PressedKey>,
    auth_sender: channel::Sender<AuthEvent>,
}

#[allow(dead_code)]
struct PressedKey {
    logical_key: Key,
    text: Option<String>,
    first_pressed: std::time::Instant,
    last_repeated: std::time::Instant,
}

impl CompositorHandler for AppState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        scale_factor: i32,
    ) {
        surface.set_buffer_scale(scale_factor);
        if let Some(state) = &mut self.state {
            state.scale = scale_factor as f64;
            let pw = (state.width as f64 * state.scale) as u32;
            let ph = (state.height as f64 * state.scale) as u32;
            state.resize(pw, ph);
        }
        self.redraw = true;
    }

    fn transform_changed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _surface: &wl_surface::WlSurface, _new_transform: wl_output::Transform) {}
    fn frame(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _surface: &wl_surface::WlSurface, _time: u32) {}
    fn surface_enter(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _surface: &wl_surface::WlSurface, _output: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _surface: &wl_surface::WlSurface, _output: &wl_output::WlOutput) {}
}

impl OutputHandler for AppState {
    fn output_state(&mut self) -> &mut OutputState { &mut self.output_state }
    fn new_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: wl_output::WlOutput) {}
    fn update_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: wl_output::WlOutput) {}
}

impl SeatHandler for AppState {
    fn seat_state(&mut self) -> &mut SeatState { &mut self.seat_state }
    
    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.seats.push(seat);
    }

    fn new_capability(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            let pointer = self.seat_state.get_pointer(qh, &seat).unwrap();
            self.pointer = Some(pointer);
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            let keyboard = self.seat_state.get_keyboard(qh, &seat, None).unwrap();
            self.keyboard = Some(keyboard);
        }
    }

    fn remove_capability(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Pointer { self.pointer = None; }
        if capability == Capability::Keyboard { self.keyboard = None; }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.seats.retain(|s| s != &seat);
    }
}

impl ShmHandler for AppState {
    fn shm_state(&mut self) -> &mut Shm { &mut self.shm_state }
}

impl PointerHandler for AppState {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[smithay_client_toolkit::seat::pointer::PointerEvent],
    ) {
        use smithay_client_toolkit::seat::pointer::PointerEventKind;
        for event in events {
            let (x, y) = event.position;
            if let Some(state) = &mut self.state {
                state.cursor_x = x as f32;
                state.cursor_y = y as f32;
            }

            match &event.kind {
                PointerEventKind::Enter { .. } => {}
                PointerEventKind::Leave { .. } => {}
                PointerEventKind::Motion { .. } => {
                    if let Some(state) = &mut self.state {
                        let cx = state.cursor_x;
                        let cy = state.cursor_y;
                        let mut changed = false;
                        for w in state.widgets_iter_mut() {
                            if w.cursor_moved(cx, cy) {
                                changed = true;
                            }
                        }
                        if changed {
                            state.upload_vertices();
                            self.redraw = true;
                        }
                    }
                }
                PointerEventKind::Press { button, .. } => {
                    let btn = match *button {
                        272 => MouseButton::Left,
                        273 => MouseButton::Right,
                        274 => MouseButton::Middle,
                        _ => continue,
                    };
                    if let Some(st) = &mut self.state {
                        if st.is_authenticating {
                            continue;
                        }
                        let mut changed = false;
                        let cx = st.cursor_x;
                        let cy = st.cursor_y;
                        if btn == MouseButton::Left {
                            // Unfocus other elements if a click is made
                            let hit_any = st.status_lbl.hit_test(cx, cy)
                                || st.login_btn.hit_test(cx, cy)
                                || st.session_btn.hit_test(cx, cy)
                                || st.password_box.hit_test(cx, cy)
                                || st.username_box.hit_test(cx, cy)
                                || st.card.hit_test(cx, cy)
                                || st.bg.hit_test(cx, cy);
                            if hit_any {
                                st.bg.unfocus();
                                st.card.unfocus();
                                st.username_box.unfocus();
                                st.password_box.unfocus();
                                st.session_btn.unfocus();
                                st.login_btn.unfocus();
                                st.status_lbl.unfocus();
                            }
                        }

                        // Process the click on the topmost hit widget
                        if st.status_lbl.hit_test(cx, cy) {
                            if st.status_lbl.mouse_input(btn, ElementState::Pressed, cx, cy) {
                                changed = true;
                            }
                            if btn == MouseButton::Left {
                                st.status_lbl.focus();
                            }
                        } else if st.login_btn.hit_test(cx, cy) {
                            if st.login_btn.mouse_input(btn, ElementState::Pressed, cx, cy) {
                                changed = true;
                            }
                            if btn == MouseButton::Left {
                                st.login_btn.focus();
                            }
                        } else if st.session_btn.hit_test(cx, cy) {
                            if st.session_btn.mouse_input(btn, ElementState::Pressed, cx, cy) {
                                changed = true;
                            }
                            if btn == MouseButton::Left {
                                st.session_btn.focus();
                            }
                        } else if st.password_box.hit_test(cx, cy) {
                            if st.password_box.mouse_input(btn, ElementState::Pressed, cx, cy) {
                                changed = true;
                            }
                            if btn == MouseButton::Left {
                                st.password_box.focus();
                            }
                        } else if st.username_box.hit_test(cx, cy) {
                            if st.username_box.mouse_input(btn, ElementState::Pressed, cx, cy) {
                                changed = true;
                            }
                            if btn == MouseButton::Left {
                                st.username_box.focus();
                            }
                        } else if st.card.hit_test(cx, cy) {
                            if st.card.mouse_input(btn, ElementState::Pressed, cx, cy) {
                                changed = true;
                            }
                            if btn == MouseButton::Left {
                                st.card.focus();
                            }
                        } else if st.bg.hit_test(cx, cy) {
                            if st.bg.mouse_input(btn, ElementState::Pressed, cx, cy) {
                                changed = true;
                            }
                            if btn == MouseButton::Left {
                                st.bg.focus();
                            }
                        }

                        if changed {
                            st.upload_vertices();
                            self.redraw = true;
                        }
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    let btn = match *button {
                        272 => MouseButton::Left,
                        273 => MouseButton::Right,
                        274 => MouseButton::Middle,
                        _ => continue,
                    };
                    if let Some(st) = &mut self.state {
                        if st.is_authenticating {
                            continue;
                        }
                        let cx = st.cursor_x;
                        let cy = st.cursor_y;
                        let mut changed = false;
                        for w in st.widgets_iter_mut() {
                            if w.mouse_input(btn, ElementState::Released, cx, cy) {
                                changed = true;
                            }
                        }
                        if btn == MouseButton::Left {
                            // Handle session select cycle click
                            if st.session_btn.take_click() {
                                st.session_idx = (st.session_idx + 1) % st.sessions.len();
                                let session_name = st.sessions[st.session_idx];
                                st.session_btn.label = Some(format!("Session: {}", session_name));
                                changed = true;
                            }
                            // Handle login click
                            if st.login_btn.take_click() {
                                // Extract login username and password
                                let username = st.username_box.text.trim().to_string();
                                let password = st.password_box.text.trim().to_string();

                                if username.is_empty() {
                                    st.status_lbl.text = "Username cannot be empty".to_string();
                                    st.status_lbl.is_error = true;
                                } else if password.is_empty() {
                                    st.status_lbl.text = "Password cannot be empty".to_string();
                                    st.status_lbl.is_error = true;
                                } else {
                                    st.status_lbl.text = "Authenticating...".to_string();
                                    st.status_lbl.is_error = false;
                                    st.is_authenticating = true;
                                    st.login_btn.label = Some("Authenticating...".to_string());
                                    authenticate_user(username, password, self.auth_sender.clone());
                                }
                                changed = true;
                            }
                        }
                        if changed {
                            st.upload_vertices();
                            self.redraw = true;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

impl AppState {
    fn handle_key(&mut self, event: smithay_client_toolkit::seat::keyboard::KeyEvent, state: ElementState) {
        let keysym = event.keysym;
        let logical_key = match keysym {
            xkeysym::Keysym::BackSpace => Key::Named(NamedKey::Backspace),
            xkeysym::Keysym::Tab => Key::Named(NamedKey::Tab),
            xkeysym::Keysym::Return | xkeysym::Keysym::KP_Enter => Key::Named(NamedKey::Enter),
            xkeysym::Keysym::Escape => Key::Named(NamedKey::Escape),
            xkeysym::Keysym::space => Key::Named(NamedKey::Space),
            xkeysym::Keysym::Left => Key::Named(NamedKey::ArrowLeft),
            xkeysym::Keysym::Right => Key::Named(NamedKey::ArrowRight),
            xkeysym::Keysym::Up => Key::Named(NamedKey::ArrowUp),
            xkeysym::Keysym::Down => Key::Named(NamedKey::ArrowDown),
            xkeysym::Keysym::Delete => Key::Named(NamedKey::Delete),
            xkeysym::Keysym::Home => Key::Named(NamedKey::Home),
            xkeysym::Keysym::End => Key::Named(NamedKey::End),
            _ => {
                if let Some(ref text) = event.utf8 {
                    Key::Character(text.clone())
                } else {
                    return;
                }
            }
        };

        let text = event.utf8.clone();

        let custom_event = KeyEvent {
            state,
            logical_key: logical_key.clone(),
            text: text.clone(),
            repeat: false,
            ctrl: self.ctrl_pressed,
            shift: self.shift_pressed,
        };

        if state == ElementState::Pressed {
            if let Some(st) = &mut self.state {
                if st.is_authenticating {
                    return;
                }
                let mut changed = false;

                // Handle Tab navigation between username and password boxes
                if logical_key == Key::Named(NamedKey::Tab) {
                    let is_user_focused = clear_ui::widget::focus::is_focused(&st.username_box);
                    if is_user_focused {
                        st.username_box.unfocus();
                        st.password_box.focus();
                    } else {
                        st.password_box.unfocus();
                        st.username_box.focus();
                    }
                    changed = true;
                } else {
                    for w in st.widgets_iter_mut() {
                        if w.keyboard_input(&custom_event) {
                            changed = true;
                        }
                    }
                }

                if changed {
                    st.upload_vertices();
                    self.redraw = true;
                }
            }

            self.pressed_key = Some(PressedKey {
                logical_key,
                text,
                first_pressed: std::time::Instant::now(),
                last_repeated: std::time::Instant::now(),
            });
        } else {
            self.pressed_key = None;
        }
    }
}

impl KeyboardHandler for AppState {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw_modifiers: &[u32],
        _keysyms: &[xkeysym::Keysym],
    ) {}

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {}

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: smithay_client_toolkit::seat::keyboard::KeyEvent,
    ) {
        self.handle_key(event, ElementState::Pressed);
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: smithay_client_toolkit::seat::keyboard::KeyEvent,
    ) {
        self.handle_key(event, ElementState::Released);
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        modifiers: smithay_client_toolkit::seat::keyboard::Modifiers,
        _layout: u32,
    ) {
        self.ctrl_pressed = modifiers.ctrl;
        self.shift_pressed = modifiers.shift;
    }
}

impl WindowHandler for AppState {
    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &XdgWindow,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        let (w, h) = configure.new_size;
        let width = w.map(|v| v.get()).unwrap_or(1024);
        let height = h.map(|v| v.get()).unwrap_or(768);
        if let Some(state) = &mut self.state {
            let pw = (width as f64 * state.scale) as u32;
            let ph = (height as f64 * state.scale) as u32;
            state.resize(pw, ph);
        }
        self.redraw = true;
    }

    fn request_close(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _window: &XdgWindow) {
        self.exit = true;
    }
}

impl ProvidesRegistryState for AppState {
    fn registry(&mut self) -> &mut RegistryState { &mut self.registry_state }
    fn runtime_add_global(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _name: u32, _interface: &str, _version: u32) {}
    fn runtime_remove_global(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _name: u32, _interface: &str) {}
}

delegate_compositor!(AppState);
delegate_xdg_shell!(AppState);
delegate_xdg_window!(AppState);
delegate_shm!(AppState);
delegate_seat!(AppState);
delegate_pointer!(AppState);
delegate_keyboard!(AppState);
delegate_registry!(AppState);
delegate_output!(AppState);

fn main() {
    let conn = Connection::connect_to_env().expect("Wayland connection");
    let (globals, mut event_queue) = registry_queue_init(&conn).expect("registry init");
    let qh = event_queue.handle();

    let compositor_state = CompositorState::bind(&globals, &qh).unwrap();
    let xdg_shell_state = XdgShell::bind(&globals, &qh).unwrap();
    let shm_state = Shm::bind(&globals, &qh).unwrap();
    let seat_state = SeatState::new(&globals, &qh);
    let output_state = OutputState::new(&globals, &qh);

    let (auth_sender, auth_receiver) = channel::channel::<AuthEvent>();

    let mut app = AppState {
        registry_state: RegistryState::new(&globals),
        compositor_state,
        xdg_shell_state,
        shm_state,
        seat_state,
        output_state,
        seats: Vec::new(),
        pointer: None,
        keyboard: None,
        window: None,
        surface: None,
        state: None,
        exit: false,
        redraw: true,
        ctrl_pressed: false,
        shift_pressed: false,
        pressed_key: None,
        auth_sender,
    };

    event_queue.roundtrip(&mut app).unwrap();

    let scale = clear_ui::wayland::detect_scale_factor(&app.output_state);
    let surface = app.compositor_state.create_surface(&qh);
    surface.set_buffer_scale(scale as i32);

    let pw = (1024.0 * scale) as u32;
    let ph = (768.0 * scale) as u32;

    let window = app.xdg_shell_state.create_window(surface.clone(), WindowDecorations::None, &qh);
    window.set_title("Clear Display Manager");
    window.set_app_id("clear-display-manager");
    window.set_min_size(Some((pw, ph)));
    window.commit();

    let wayland_handle = Box::leak(Box::new(clear_ui::wayland::WaylandSurfaceHandle {
        display_ptr: conn.backend().display_id().as_ptr() as *mut std::ffi::c_void,
        surface_ptr: surface.id().as_ptr() as *mut std::ffi::c_void,
    }));

    let state = pollster::block_on(State::new(wayland_handle, pw, ph, scale));

    app.window = Some(window);
    app.surface = Some(surface);
    app.state = Some(state);

    let mut event_loop = EventLoop::try_new().unwrap();
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn, event_queue).insert(loop_handle.clone()).unwrap();

    loop_handle.insert_source(auth_receiver, |event, _metadata, app_state| {
        match event {
            channel::Event::Msg(msg) => {
                if let Some(st) = &mut app_state.state {
                    st.is_authenticating = false;
                    st.login_btn.label = Some("Log In".to_string());
                    match msg {
                        AuthEvent::Success { username } => {
                            st.status_lbl.text = format!("Welcome, {}!", username);
                            st.status_lbl.is_error = false;
                            st.login_success = true;
                            app_state.exit = true;
                        }
                        AuthEvent::Failure(err_msg) => {
                            st.status_lbl.text = err_msg;
                            st.status_lbl.is_error = true;
                        }
                    }
                    st.upload_vertices();
                    app_state.redraw = true;
                }
            }
            channel::Event::Closed => {}
        }
    }).unwrap();

    loop {
        event_loop.dispatch(std::time::Duration::from_millis(16), &mut app).unwrap();
        if app.exit { break; }

        if app.redraw {
            app.redraw = false;
            if let Some(st) = &mut app.state {
                st.render();
            }
        }
    }

    // Launch desktop session if authentication succeeded
    if let Some(st) = app.state {
        if st.login_success {
            let session = st.sessions[st.session_idx];
            println!("[clear-display-manager] Authenticated successfully. Launching session: {}", session);
            
            // Session launch logic
            match session {
                "River WM" => {
                    let script_path = "/home/lsgalante/Dropbox/Clear/clear-window-manager/start-river.sh";
                    println!("[clear-display-manager] Executing session command: {}", script_path);
                    let mut child = std::process::Command::new(script_path)
                        .spawn()
                        .expect("failed to execute start-river.sh");
                    let _ = child.wait();
                }
                _ => {
                    println!("[clear-display-manager] Executing default bash session");
                    let mut child = std::process::Command::new("/bin/bash")
                        .spawn()
                        .expect("failed to execute bash");
                    let _ = child.wait();
                }
            }
        }
    }
}
