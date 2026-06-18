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
        pointer::{PointerHandler, ThemedPointer, ThemeSpec, CursorIcon},
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

use cce_ui::widget::{
    Button, ContentBg, TextLabel, Element, ElementState, MouseButton, Key, NamedKey, KeyEvent, TextBox
};
use cce_ui::context::UiContext;

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    position: [f32; 2],
    color: [f32; 4],
    clip_circle: [f32; 3],
}

impl Vertex {
    const ATTRIBS: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![
        0 => Float32x2,
        1 => Float32x4,
        2 => Float32x3,
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
        Vertex { position: [x0, y0], color, clip_circle: [0.0; 3] },
        Vertex { position: [x1, y0], color, clip_circle: [0.0; 3] },
        Vertex { position: [x0, y1], color, clip_circle: [0.0; 3] },
        Vertex { position: [x1, y0], color, clip_circle: [0.0; 3] },
        Vertex { position: [x1, y1], color, clip_circle: [0.0; 3] },
        Vertex { position: [x0, y1], color, clip_circle: [0.0; 3] },
    ]
}

fn widget_vertices(w: &dyn Element, sw: f32, sh: f32) -> Vec<Vertex> {
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



#[derive(Debug, Clone)]
struct Session {
    name: String,
    exec: String,
    is_wayland: bool,
}

fn sanitize_exec(exec: &str) -> (String, Vec<String>) {
    let mut parts = Vec::new();
    for part in exec.split_whitespace() {
        if part.starts_with('%') {
            continue; // ignore desktop entry field codes
        }
        parts.push(part.to_string());
    }
    if parts.is_empty() {
        return (String::new(), Vec::new());
    }
    let cmd = parts.remove(0);
    (cmd, parts)
}

fn parse_desktop_file(path: &std::path::Path, is_wayland: bool) -> Result<Session, std::io::Error> {
    let content = std::fs::read_to_string(path)?;
    let mut name = None;
    let mut exec = None;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("Name=") {
            name = Some(line["Name=".len()..].to_string());
        } else if line.starts_with("Exec=") {
            exec = Some(line["Exec=".len()..].to_string());
        }
    }
    if let (Some(n), Some(e)) = (name, exec) {
        Ok(Session { name: n, exec: e, is_wayland })
    } else {
        Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid desktop file"))
    }
}

fn discover_sessions() -> Vec<Session> {
    let mut sessions = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/usr/share/wayland-sessions") {
        for entry in entries.flatten() {
            if entry.path().extension().map_or(false, |ext| ext == "desktop") {
                if let Ok(s) = parse_desktop_file(&entry.path(), true) {
                    sessions.push(s);
                }
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir("/usr/share/xsessions") {
        for entry in entries.flatten() {
            if entry.path().extension().map_or(false, |ext| ext == "desktop") {
                if let Ok(s) = parse_desktop_file(&entry.path(), false) {
                    sessions.push(s);
                }
            }
        }
    }
    sessions.push(Session {
        name: "Bash Shell".to_string(),
        exec: "bash".to_string(),
        is_wayland: true,
    });
    sessions
}

// ── Custom LoginCard Container Element ──
#[derive(Debug, Clone)]
struct LoginCard {
    x: f32, y: f32, w: f32, h: f32,
}

impl LoginCard {
    fn new() -> Self {
        Self { x: 0.0, y: 0.0, w: 0.0, h: 0.0 }
    }
}

impl Element for LoginCard {
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

impl Element for StatusLabel {
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

// ── Custom Session List Panel Element ──
#[derive(Debug, Clone)]
struct SessionList {
    x: f32, y: f32, w: f32, h: f32,
    sessions: Vec<Session>,
    selected_idx: usize,
    hovered_idx: Option<usize>,
}

impl SessionList {
    fn new(sessions: Vec<Session>) -> Self {
        Self {
            x: 0.0, y: 0.0, w: 0.0, h: 0.0,
            sessions,
            selected_idx: 0,
            hovered_idx: None,
        }
    }

    fn selected_session(&self) -> Option<&Session> {
        self.sessions.get(self.selected_idx)
    }
}

impl Element for SessionList {
    fn rect(&self) -> (f32, f32, f32, f32) { (self.x, self.y, self.w, self.h) }
    fn set_rect(&mut self, x: f32, y: f32, w: f32, h: f32) { self.x = x; self.y = y; self.w = w; self.h = h; }
    fn color(&self) -> [f32; 4] { [0.07, 0.07, 0.10, 0.70] } // Semi-transparent sleek dark card background

    fn extra_quads(&self) -> Vec<(f32, f32, f32, f32, [f32; 4])> {
        let mut quads = Vec::new();
        
        // 1. Panel borders (subtle blue accent)
        let border_color = [0.20, 0.40, 0.65, 0.5];
        quads.push((self.x, self.y, self.w, 1.5, border_color)); // top
        quads.push((self.x, self.y + self.h - 1.5, self.w, 1.5, border_color)); // bottom
        quads.push((self.x, self.y, 1.5, self.h, border_color)); // left
        quads.push((self.x + self.w - 1.5, self.y, 1.5, self.h, border_color)); // right
        
        let item_w = self.w - 20.0;
        
        // 2. Selected item background
        let selected_color = [0.20, 0.40, 0.65, 0.8]; // Solid blue highlight
        let sel_y = self.y + 40.0 + self.selected_idx as f32 * 36.0;
        quads.push((self.x + 10.0, sel_y, item_w, 32.0, selected_color));
        
        // 3. Hovered item background
        if let Some(h_idx) = self.hovered_idx {
            if h_idx != self.selected_idx && h_idx < self.sessions.len() {
                let hover_color = [1.0, 1.0, 1.0, 0.06]; // Subtle white overlay
                let h_y = self.y + 40.0 + h_idx as f32 * 36.0;
                quads.push((self.x + 10.0, h_y, item_w, 32.0, hover_color));
            }
        }
        
        quads
    }

    fn text_labels(&self) -> Vec<TextLabel> {
        let mut labels = Vec::new();
        
        // Header title
        labels.push(TextLabel {
            text: "SESSION MANAGER".to_string(),
            x: self.x + 15.0,
            y: self.y + 18.0,
            font_size: 11.0,
            color: [0x83, 0x83, 0x8a],
        });
        
        // Session items
        for (i, session) in self.sessions.iter().enumerate() {
            let item_y = self.y + 40.0 + i as f32 * 36.0;
            
            let display_name = if session.name == "Bash Shell" {
                "Bash Shell".to_string()
            } else {
                format!("{} ({})", session.name, if session.is_wayland { "Wayland" } else { "X11" })
            };
            
            let color = if i == self.selected_idx {
                [0xff, 0xff, 0xff]
            } else {
                [0xee, 0xee, 0xf5]
            };
            
            labels.push(TextLabel {
                text: display_name,
                x: self.x + 20.0,
                y: item_y + 10.0,
                font_size: 12.0,
                color,
            });
        }
        
        labels
    }

    fn on_cursor_moved(&mut self, px: f32, py: f32, ctx: &mut UiContext) -> bool {
        let old_hovered = self.hovered_idx;
        self.hovered_idx = None;
        if self.hit_test(px, py, ctx) {
            let item_w = self.w - 20.0;
            for i in 0..self.sessions.len() {
                let item_y = self.y + 40.0 + i as f32 * 36.0;
                let item_x = self.x + 10.0;
                if px >= item_x && px <= item_x + item_w && py >= item_y && py <= item_y + 32.0 {
                    self.hovered_idx = Some(i);
                    break;
                }
            }
        }
        self.hovered_idx != old_hovered
    }

    fn mouse_input(&mut self, button: MouseButton, state: ElementState, px: f32, py: f32, ctx: &mut UiContext) -> bool {
        if button == MouseButton::Left && state == ElementState::Pressed {
            if self.hit_test(px, py, ctx) {
                let item_w = self.w - 20.0;
                for i in 0..self.sessions.len() {
                    let item_y = self.y + 40.0 + i as f32 * 36.0;
                    let item_x = self.x + 10.0;
                    if px >= item_x && px <= item_x + item_w && py >= item_y && py <= item_y + 32.0 {
                        if self.selected_idx != i {
                            self.selected_idx = i;
                            return true;
                        }
                    }
                }
            }
        }
        false
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
    login_btn: Button,
    status_lbl: StatusLabel,
    session_list: SessionList,
    ui_context: cce_ui::context::UiContext,

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
    compositor_scale: f64,

    // State tracking
    login_success: bool,
    is_authenticating: bool,
}

impl State {
    async fn new(
        wayland_handle: &'static cce_ui::wayland::WaylandSurfaceHandle,
        pw: u32, ph: u32,
        scale: f64,
        compositor_scale: f64,
        sessions: Vec<Session>,
    ) -> Self {
        let lw = pw as f32 / scale as f32;
        let lh = ph as f32 / scale as f32;

        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..Default::default()
        });

        let surface = instance.create_surface(wayland_handle).expect("wgpu surface");
        let adapter = instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
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
            source: wgpu::ShaderSource::Wgsl(cce_ui::SHADER.into()),
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

        // Prepopulate username from last_user file if it exists
        let last_user_path = "/var/lib/cce-display-manager/last_user";
        let current_user = if std::path::Path::new(last_user_path).exists() {
            std::fs::read_to_string(last_user_path)
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| String::new())
        } else {
            let env_user = std::env::var("USER").unwrap_or_else(|_| String::new());
            if env_user == "root" || env_user == "cce-display-manager" {
                String::new()
            } else {
                env_user
            }
        };

        let last_session_path = "/var/lib/cce-display-manager/last_session";
        let last_session_exec = if std::path::Path::new(last_session_path).exists() {
            std::fs::read_to_string(last_session_path)
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| String::new())
        } else {
            String::new()
        };

        let mut selected_idx = 0;
        if !last_session_exec.is_empty() {
            if let Some(pos) = sessions.iter().position(|s| s.exec == last_session_exec) {
                selected_idx = pos;
            }
        }

        let bg = ContentBg::new();
        let card = LoginCard::new();
        let username_box = TextBox::new(current_user).with_label("USERNAME");
        let password_box = TextBox::new(String::new()).with_password(true).with_label("PASSWORD");
        let login_btn = Button::new(0.0, 0.0, 300.0, 36.0).with_label("Log In");
        let status_lbl = StatusLabel::new("Enter password to start".to_string());
        let mut session_list = SessionList::new(sessions);
        session_list.selected_idx = selected_idx;

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
            login_btn,
            status_lbl,
            session_list,
            ui_context: cce_ui::context::UiContext::new(),
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
            compositor_scale,
            login_success: false,
            is_authenticating: false,
        };

        state.apply_layout();
        state.upload_vertices();
        state
    }

    fn widgets_iter(&self) -> Vec<&dyn Element> {
        vec![
            &self.bg,
            &self.card,
            &self.username_box,
            &self.password_box,
            &self.login_btn,
            &self.status_lbl,
            &self.session_list,
        ]
    }

    fn widgets_iter_mut(&mut self) -> Vec<&mut dyn Element> {
        vec![
            &mut self.bg,
            &mut self.card,
            &mut self.username_box,
            &mut self.password_box,
            &mut self.login_btn,
            &mut self.status_lbl,
            &mut self.session_list,
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

        // Login button (full-width of the contents)
        self.login_btn.set_rect(content_x, card_y + 205.0, 300.0, 36.0);

        // Status message
        self.status_lbl.set_rect(content_x, card_y + 252.0, 300.0, 20.0);

        // Session list on top left
        let list_w = 260.0;
        let list_h = 40.0 + self.session_list.sessions.len() as f32 * 36.0;
        self.session_list.set_rect(30.0, 30.0, list_w, list_h);
    }

    fn collect_vertices(&self) -> Vec<Vertex> {
        let sw = self.width;
        let sh = self.height;
        let mut verts = Vec::new();
        for w in self.widgets_iter() {
            verts.extend(widget_vertices(w, sw, sh));
            for (qx, qy, qw, qh, qc) in w.all_quads(&self.ui_context) {
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
                left: (20.0 * scale_f32).round(),
                top: (self.physical_height as f32 - 24.0 * scale_f32).round(),
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
                left: (label.x * scale_f32).round(),
                top: (label.y * scale_f32).round(),
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

    pub fn widgets_cursor_moved(&mut self, cx: f32, cy: f32) -> bool {
        let ctx = &mut self.ui_context;
        let mut changed = false;
        if self.bg.cursor_moved(cx, cy, ctx) { changed = true; }
        if self.card.cursor_moved(cx, cy, ctx) { changed = true; }
        if self.username_box.cursor_moved(cx, cy, ctx) { changed = true; }
        if self.password_box.cursor_moved(cx, cy, ctx) { changed = true; }
        if self.login_btn.cursor_moved(cx, cy, ctx) { changed = true; }
        if self.status_lbl.cursor_moved(cx, cy, ctx) { changed = true; }
        if self.session_list.cursor_moved(cx, cy, ctx) { changed = true; }
        changed
    }

    pub fn widgets_mouse_input(&mut self, button: MouseButton, state: ElementState, cx: f32, cy: f32) -> bool {
        let ctx = &mut self.ui_context;
        let mut changed = false;
        if self.bg.mouse_input(button, state, cx, cy, ctx) { changed = true; }
        if self.card.mouse_input(button, state, cx, cy, ctx) { changed = true; }
        if self.username_box.mouse_input(button, state, cx, cy, ctx) { changed = true; }
        if self.password_box.mouse_input(button, state, cx, cy, ctx) { changed = true; }
        if self.login_btn.mouse_input(button, state, cx, cy, ctx) { changed = true; }
        if self.status_lbl.mouse_input(button, state, cx, cy, ctx) { changed = true; }
        if self.session_list.mouse_input(button, state, cx, cy, ctx) { changed = true; }
        changed
    }

    pub fn widgets_keyboard_input(&mut self, event: &KeyEvent) -> bool {
        let ctx = &mut self.ui_context;
        let mut changed = false;
        if self.bg.keyboard_input(event, ctx) { changed = true; }
        if self.card.keyboard_input(event, ctx) { changed = true; }
        if self.username_box.keyboard_input(event, ctx) { changed = true; }
        if self.password_box.keyboard_input(event, ctx) { changed = true; }
        if self.login_btn.keyboard_input(event, ctx) { changed = true; }
        if self.status_lbl.keyboard_input(event, ctx) { changed = true; }
        if self.session_list.keyboard_input(event, ctx) { changed = true; }
        changed
    }
}

#[derive(Debug, Clone)]
enum AuthEvent {
    Success { request_id: u64, username: String },
    Failure { request_id: u64, err_msg: String },
    Info { request_id: u64, msg: String },
}

fn authenticate_user(request_id: u64, username: String, password: String, sender: channel::Sender<AuthEvent>) {
    std::thread::spawn(move || {
        let service = "cce-display-manager";
        
        let mut auth = match PamSession::new(service, &username, &password, request_id, Some(sender.clone())) {
            Ok(a) => a,
            Err(_) => {
                match PamSession::new("login", &username, &password, request_id, Some(sender.clone())) {
                    Ok(a) => a,
                    Err(e) => {
                        let _ = sender.send(AuthEvent::Failure { request_id, err_msg: format!("PAM Init Error: {:?}", e) });
                        return;
                    }
                }
            }
        };

        match auth.authenticate() {
            Ok(_) => {
                let _ = sender.send(AuthEvent::Success { request_id, username });
            }
            Err(e) => {
                let _ = sender.send(AuthEvent::Failure { request_id, err_msg: format!("Authentication failed: {:?}", e) });
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
    pointer: Option<ThemedPointer>,
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
    auth_request_id: u64,
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
        let sys_config = load_system_config();
        let compositor_scale = scale_factor as f64;
        let layout_scale = sys_config.scale.unwrap_or(compositor_scale);
        
        surface.set_buffer_scale(scale_factor);
        if let Some(state) = &mut self.state {
            let logical_w = state.physical_width as f64 / state.compositor_scale;
            let logical_h = state.physical_height as f64 / state.compositor_scale;
            
            state.compositor_scale = compositor_scale;
            state.scale = layout_scale;
            
            let pw = (logical_w * compositor_scale) as u32;
            let ph = (logical_h * compositor_scale) as u32;
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
            let surface = self.compositor_state.create_surface(qh);
            let themed_pointer = self.seat_state.get_pointer_with_theme(
                qh,
                &seat,
                self.shm_state.wl_shm(),
                surface,
                ThemeSpec::System,
            ).unwrap();
            self.pointer = Some(themed_pointer);
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
                let ratio = state.compositor_scale / state.scale;
                state.cursor_x = (x * ratio) as f32;
                state.cursor_y = (y * ratio) as f32;
            }

            match &event.kind {
                PointerEventKind::Enter { .. } => {
                    if let Some(ref themed_pointer) = self.pointer {
                        let _ = themed_pointer.set_cursor(_conn, CursorIcon::Default);
                    }
                }
                PointerEventKind::Leave { .. } => {}
                PointerEventKind::Motion { .. } => {
                    if let Some(state) = &mut self.state {
                        let cx = state.cursor_x;
                        let cy = state.cursor_y;
                        let mut changed = false;
                        if state.widgets_cursor_moved(cx, cy) {
                            changed = true;
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
                            let hit_any = st.status_lbl.hit_test(cx, cy, &st.ui_context)
                                || st.login_btn.hit_test(cx, cy, &st.ui_context)
                                || st.session_list.hit_test(cx, cy, &st.ui_context)
                                || st.password_box.hit_test(cx, cy, &st.ui_context)
                                || st.username_box.hit_test(cx, cy, &st.ui_context)
                                || st.card.hit_test(cx, cy, &st.ui_context)
                                || st.bg.hit_test(cx, cy, &st.ui_context);
                            if hit_any {
                                st.ui_context.clear_focus();
                                st.bg.unfocus();
                                st.card.unfocus();
                                st.username_box.unfocus();
                                st.password_box.unfocus();
                                st.session_list.unfocus();
                                st.login_btn.unfocus();
                                st.status_lbl.unfocus();
                            }
                        }

                        // Process the click on the topmost hit widget
                        if st.status_lbl.hit_test(cx, cy, &st.ui_context) {
                            if st.status_lbl.mouse_input(btn, ElementState::Pressed, cx, cy, &mut st.ui_context) {
                                changed = true;
                            }
                            if btn == MouseButton::Left {
                                st.ui_context.set_focused(&mut st.status_lbl);
                                st.status_lbl.focus();
                            }
                        } else if st.login_btn.hit_test(cx, cy, &st.ui_context) {
                            if st.login_btn.mouse_input(btn, ElementState::Pressed, cx, cy, &mut st.ui_context) {
                                changed = true;
                            }
                            if btn == MouseButton::Left {
                                st.ui_context.set_focused(&mut st.login_btn);
                                st.login_btn.focus();
                            }
                        } else if st.session_list.hit_test(cx, cy, &st.ui_context) {
                            if st.session_list.mouse_input(btn, ElementState::Pressed, cx, cy, &mut st.ui_context) {
                                changed = true;
                            }
                            if btn == MouseButton::Left {
                                st.ui_context.set_focused(&mut st.session_list);
                                st.session_list.focus();
                            }
                        } else if st.password_box.hit_test(cx, cy, &st.ui_context) {
                            if st.password_box.mouse_input(btn, ElementState::Pressed, cx, cy, &mut st.ui_context) {
                                changed = true;
                            }
                            if btn == MouseButton::Left {
                                st.ui_context.set_focused(&mut st.password_box);
                                st.password_box.focus();
                            }
                        } else if st.username_box.hit_test(cx, cy, &st.ui_context) {
                            if st.username_box.mouse_input(btn, ElementState::Pressed, cx, cy, &mut st.ui_context) {
                                changed = true;
                            }
                            if btn == MouseButton::Left {
                                st.ui_context.set_focused(&mut st.username_box);
                                st.username_box.focus();
                            }
                        } else if st.card.hit_test(cx, cy, &st.ui_context) {
                            if st.card.mouse_input(btn, ElementState::Pressed, cx, cy, &mut st.ui_context) {
                                changed = true;
                            }
                            if btn == MouseButton::Left {
                                st.ui_context.set_focused(&mut st.card);
                                st.card.focus();
                            }
                        } else if st.bg.hit_test(cx, cy, &st.ui_context) {
                            if st.bg.mouse_input(btn, ElementState::Pressed, cx, cy, &mut st.ui_context) {
                                changed = true;
                            }
                            if btn == MouseButton::Left {
                                st.ui_context.set_focused(&mut st.bg);
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
                        if st.widgets_mouse_input(btn, ElementState::Released, cx, cy) {
                            changed = true;
                        }
                        if btn == MouseButton::Left {
                            // Handle login click
                            if st.login_btn.take_click() {
                                // Extract login username and password
                                let username = st.username_box.text.trim().to_string();
                                let password = st.password_box.text.trim().to_string();

                                if username.is_empty() {
                                    st.status_lbl.text = "Username cannot be empty".to_string();
                                    st.status_lbl.is_error = true;
                                    st.username_box.focus();
                                } else if password.is_empty() {
                                    st.status_lbl.text = "Password cannot be empty".to_string();
                                    st.status_lbl.is_error = true;
                                    st.password_box.focus();
                                } else {
                                    self.auth_request_id += 1;
                                    st.status_lbl.text = "Authenticating...".to_string();
                                    st.status_lbl.is_error = false;
                                    st.is_authenticating = true;
                                    st.login_btn.base_mut().unwrap().label = Some("Authenticating...".to_string());
                                    authenticate_user(self.auth_request_id, username, password, self.auth_sender.clone());
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

        // Check for Ctrl+C to abort/exit back to TTY
        if self.ctrl_pressed && (keysym == xkeysym::Keysym::c || keysym == xkeysym::Keysym::C) {
            eprintln!("[cce-display-manager] Ctrl+C pressed. Aborting greeter.");
            std::process::exit(130);
        }

        // Check for F5 to request daemon restart
        if keysym == xkeysym::Keysym::F5 {
            eprintln!("[cce-display-manager] F5 pressed. Requesting daemon restart.");
            std::process::exit(135);
        }

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
                } else if let Some(ch) = event.keysym.key_char() {
                    Key::Character(ch.to_string())
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

        let is_ctrl_p = self.ctrl_pressed && (keysym == xkeysym::Keysym::p || keysym == xkeysym::Keysym::P);
        let is_ctrl_n = self.ctrl_pressed && (keysym == xkeysym::Keysym::n || keysym == xkeysym::Keysym::N);

        if state == ElementState::Pressed {
            if let Some(st) = &mut self.state {
                // If we are currently in fingerprint authentication and the user starts typing a password,
                // cancel the fingerprint auth and let them type.
                if st.is_authenticating {
                    if st.password_box.text.is_empty() {
                        let is_typing = !self.ctrl_pressed && match &logical_key {
                            Key::Character(_) | Key::Named(NamedKey::Backspace) | Key::Named(NamedKey::Delete) | Key::Named(NamedKey::Space) => true,
                            _ => false,
                        };
                        if is_typing {
                            self.auth_request_id += 1;
                            st.is_authenticating = false;
                            st.login_btn.base_mut().unwrap().label = Some("Log In".to_string());
                            st.status_lbl.text = "Enter password to start".to_string();
                            st.status_lbl.is_error = false;
                        } else {
                            let is_nav = match &logical_key {
                                Key::Named(NamedKey::ArrowUp) | Key::Named(NamedKey::ArrowDown) | Key::Named(NamedKey::Tab) => true,
                                _ => is_ctrl_p || is_ctrl_n,
                            };
                            if !is_nav {
                                return;
                            }
                        }
                    } else {
                        return;
                    }
                }

                let mut changed = false;

                // Handle Up/Down or Ctrl+P/N navigation to cycle sessions
                let cycle_up = (logical_key == Key::Named(NamedKey::ArrowUp) || is_ctrl_p) && !st.session_list.sessions.is_empty();
                let cycle_down = (logical_key == Key::Named(NamedKey::ArrowDown) || is_ctrl_n) && !st.session_list.sessions.is_empty();

                if cycle_up {
                    let len = st.session_list.sessions.len();
                    st.session_list.selected_idx = (st.session_list.selected_idx + len - 1) % len;
                    st.session_list.hovered_idx = None;
                    changed = true;
                } else if cycle_down {
                    let len = st.session_list.sessions.len();
                    st.session_list.selected_idx = (st.session_list.selected_idx + 1) % len;
                    st.session_list.hovered_idx = None;
                    changed = true;
                } else if logical_key == Key::Named(NamedKey::Tab) {
                    let is_user_focused = st.username_box.focused(&st.ui_context);
                    if is_user_focused {
                        st.ui_context.set_focused(&mut st.password_box);
                        st.username_box.unfocus();
                        st.password_box.focus();
                    } else {
                        st.ui_context.set_focused(&mut st.username_box);
                        st.password_box.unfocus();
                        st.username_box.focus();
                    }
                    changed = true;
                } else if logical_key == Key::Named(NamedKey::Enter) && st.password_box.focused(&st.ui_context) {
                    // Process Enter in the password box to commit the buffer
                    st.password_box.keyboard_input(&custom_event, &mut st.ui_context);

                    // Extract login username and password
                    let username = st.username_box.text.trim().to_string();
                    let password = st.password_box.text.trim().to_string();

                    if username.is_empty() {
                        st.status_lbl.text = "Username cannot be empty".to_string();
                        st.status_lbl.is_error = true;
                        st.ui_context.set_focused(&mut st.username_box);
                        st.username_box.focus();
                    } else if password.is_empty() {
                        st.status_lbl.text = "Password cannot be empty".to_string();
                        st.status_lbl.is_error = true;
                        st.ui_context.set_focused(&mut st.password_box);
                        st.password_box.focus();
                    } else {
                        self.auth_request_id += 1;
                        st.status_lbl.text = "Authenticating...".to_string();
                        st.status_lbl.is_error = false;
                        st.is_authenticating = true;
                        st.login_btn.base_mut().unwrap().label = Some("Authenticating...".to_string());
                        authenticate_user(self.auth_request_id, username, password, self.auth_sender.clone());
                    }
                    changed = true;
                } else if logical_key == Key::Named(NamedKey::Enter) && st.username_box.focused(&st.ui_context) {
                    // Pressing enter in the username box commits and shifts focus to the password box
                    st.username_box.keyboard_input(&custom_event, &mut st.ui_context);
                    st.ui_context.set_focused(&mut st.password_box);
                    st.username_box.unfocus();
                    st.password_box.focus();
                    changed = true;
                } else {
                    if st.widgets_keyboard_input(&custom_event) {
                        changed = true;
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
    ) {
        self.pressed_key = None;
        self.ctrl_pressed = false;
        self.shift_pressed = false;
    }

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
            let pw = (width as f64 * state.compositor_scale) as u32;
            let ph = (height as f64 * state.compositor_scale) as u32;
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

#[derive(serde::Deserialize, Debug, Default)]
struct SystemConfig {
    scale: Option<f64>,
}

fn load_system_config() -> SystemConfig {
    let path = "/etc/cce.toml";
    if std::path::Path::new(path).exists() {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(config) = toml::from_str(&content) {
                return config;
            }
        }
    }
    SystemConfig::default()
}

fn run_greeter() {
    let sys_config = load_system_config();
    let layout_scale = sys_config.scale.unwrap_or(1.0);
    let cursor_size = (24.0 * layout_scale) as u32;
    std::env::set_var("XCURSOR_SIZE", cursor_size.to_string());

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
        auth_request_id: 0,
    };

    event_queue.roundtrip(&mut app).unwrap();

    let sys_config = load_system_config();
    eprintln!("[cce-display-manager] Loaded system config: {:?}", sys_config);
    let compositor_scale = cce_ui::wayland::detect_scale_factor(&app.output_state);
    eprintln!("[cce-display-manager] Detected compositor scale factor from Wayland: {}", compositor_scale);
    let layout_scale = sys_config.scale.unwrap_or(compositor_scale);
    eprintln!("[cce-display-manager] Final resolved layout scale factor: {}", layout_scale);
    let surface = app.compositor_state.create_surface(&qh);
    surface.set_buffer_scale(compositor_scale as i32);

    let pw = (1024.0 * compositor_scale) as u32;
    let ph = (768.0 * compositor_scale) as u32;

    let window = app.xdg_shell_state.create_window(surface.clone(), WindowDecorations::None, &qh);
    window.set_title("Clear Display Manager");
    window.set_app_id("cce-display-manager");
    window.set_min_size(Some((pw, ph)));
    window.commit();

    let wayland_handle = Box::leak(Box::new(cce_ui::wayland::WaylandSurfaceHandle {
        display_ptr: conn.backend().display_id().as_ptr() as *mut std::ffi::c_void,
        surface_ptr: surface.id().as_ptr() as *mut std::ffi::c_void,
    }));

    let sessions = discover_sessions();
    let state = pollster::block_on(State::new(wayland_handle, pw, ph, layout_scale, compositor_scale, sessions));

    app.window = Some(window);
    app.surface = Some(surface);
    app.state = Some(state);

    // Set initial focus now that State is in its final, stable memory location inside app.state
    if let Some(ref mut st) = app.state {
        st.password_box.focus();
        st.upload_vertices();

        // Start background fingerprint/empty-password authentication if username is prepopulated and fprintd is enabled!
        let username = st.username_box.text.trim().to_string();
        if !username.is_empty() {
            let has_fprint = std::fs::read_to_string("/etc/pam.d/cce-display-manager")
                .map(|content| {
                    content.lines().any(|line| {
                        let trimmed = line.trim();
                        trimmed.contains("pam_fprintd.so") && !trimmed.starts_with('#')
                    })
                })
                .unwrap_or(false);
            if has_fprint {
                app.auth_request_id += 1;
                st.is_authenticating = true;
                st.login_btn.base_mut().unwrap().label = Some("Authenticating...".to_string());
                st.status_lbl.text = "Scan finger to login or type password".to_string();
                authenticate_user(app.auth_request_id, username, String::new(), app.auth_sender.clone());
            }
        }
    }

    let mut event_loop = EventLoop::try_new().unwrap();
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn, event_queue).insert(loop_handle.clone()).unwrap();

    loop_handle.insert_source(auth_receiver, |event, _metadata, app_state| {
        match event {
            channel::Event::Msg(msg) => {
                let ev_request_id = match &msg {
                    AuthEvent::Success { request_id, .. } => *request_id,
                    AuthEvent::Failure { request_id, .. } => *request_id,
                    AuthEvent::Info { request_id, .. } => *request_id,
                };

                if ev_request_id != app_state.auth_request_id {
                    // Ignore stale/cancelled auth events
                    return;
                }

                if let Some(st) = &mut app_state.state {
                    match msg {
                        AuthEvent::Success { username, .. } => {
                            st.is_authenticating = false;
                            st.login_btn.base_mut().unwrap().label = Some("Log In".to_string());
                            st.status_lbl.text = format!("Welcome, {}!", username);
                            st.status_lbl.is_error = false;
                            st.login_success = true;
                            app_state.exit = true;
                        }
                        AuthEvent::Failure { err_msg, .. } => {
                            st.is_authenticating = false;
                            st.login_btn.base_mut().unwrap().label = Some("Log In".to_string());
                            st.status_lbl.text = err_msg;
                            st.status_lbl.is_error = true;
                            st.password_box.text.clear();
                            st.password_box.edit_buffer.clear();
                            st.password_box.focus();
                        }
                        AuthEvent::Info { msg, .. } => {
                            st.status_lbl.text = msg;
                            st.status_lbl.is_error = false;
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

    // Print authentication success data and exit
    if let Some(st) = app.state {
        if st.login_success {
            if let Some(session) = st.session_list.selected_session() {
                println!("AUTH_SUCCESS|{}|{}|{}|{}", st.username_box.text.trim(), session.exec, session.is_wayland, st.password_box.text.trim());
                std::process::exit(0);
            }
        }
    }
    std::process::exit(1);
}

struct PamSessionData {
    username: String,
    password: String,
    request_id: u64,
    sender: Option<channel::Sender<AuthEvent>>,
}

extern "C" fn pam_conversation_fn(
    num_msg: libc::c_int,
    msg: *mut *mut pam_sys::PamMessage,
    out_resp: *mut *mut pam_sys::PamResponse,
    appdata_ptr: *mut libc::c_void,
) -> libc::c_int {
    let data = unsafe { &*(appdata_ptr as *const PamSessionData) };
    let resp_size = std::mem::size_of::<pam_sys::PamResponse>();
    let resp = unsafe { libc::calloc(num_msg as usize, resp_size) as *mut pam_sys::PamResponse };
    if resp.is_null() {
        return pam_sys::PamReturnCode::BUF_ERR as libc::c_int;
    }

    for i in 0..num_msg as isize {
        unsafe {
            let m = &**msg.offset(i);
            let r = &mut *resp.offset(i);
            let style = m.msg_style;
            if style == pam_sys::PamMessageStyle::PROMPT_ECHO_ON as libc::c_int {
                let user_c = std::ffi::CString::new(data.username.clone()).unwrap();
                r.resp = libc::strdup(user_c.as_ptr());
            } else if style == pam_sys::PamMessageStyle::PROMPT_ECHO_OFF as libc::c_int {
                let pass_c = std::ffi::CString::new(data.password.clone()).unwrap();
                r.resp = libc::strdup(pass_c.as_ptr());
            } else if style == pam_sys::PamMessageStyle::ERROR_MSG as libc::c_int || style == pam_sys::PamMessageStyle::TEXT_INFO as libc::c_int {
                if !m.msg.is_null() {
                    let msg_str = std::ffi::CStr::from_ptr(m.msg).to_string_lossy().into_owned();
                    if let Some(ref sender) = data.sender {
                        let _ = sender.send(AuthEvent::Info { request_id: data.request_id, msg: msg_str });
                    }
                }
            }
        }
    }

    unsafe { *out_resp = resp };
    pam_sys::PamReturnCode::SUCCESS as libc::c_int
}

struct PamSession {
    handle: *mut pam_sys::PamHandle,
    _data: Box<PamSessionData>,
    has_open_session: bool,
}

impl PamSession {
    fn new(service: &str, username: &str, password: &str, request_id: u64, sender: Option<channel::Sender<AuthEvent>>) -> Result<Self, pam_sys::PamReturnCode> {
        let mut handle: *mut pam_sys::PamHandle = std::ptr::null_mut();
        let data = Box::new(PamSessionData {
            username: username.to_string(),
            password: password.to_string(),
            request_id,
            sender,
        });
        
        let conv = pam_sys::PamConversation {
            conv: Some(pam_conversation_fn),
            data_ptr: &*data as *const PamSessionData as *mut libc::c_void,
        };

        let rc = pam_sys::start(service, Some(username), &conv, &mut handle);
        if rc != pam_sys::PamReturnCode::SUCCESS {
            return Err(rc);
        }

        unsafe {
            let pass_c = std::ffi::CString::new(password).unwrap();
            let _ = pam_sys::raw::pam_set_item(handle, pam_sys::PamItemType::AUTHTOK as libc::c_int, pass_c.as_ptr() as *const libc::c_void);
            
            let raw_tty = std::fs::read_link("/proc/self/fd/0")
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "tty1".to_string());
            let is_real_tty = raw_tty.starts_with("tty");
            let tty_name = if is_real_tty { raw_tty } else { "tty1".to_string() };

            let tty_c = std::ffi::CString::new(tty_name).unwrap();
            let _ = pam_sys::raw::pam_set_item(handle, pam_sys::PamItemType::TTY as libc::c_int, tty_c.as_ptr() as *const libc::c_void);
        }

        Ok(Self { handle, _data: data, has_open_session: false })
    }

    fn putenv(&mut self, name_value: &str) -> Result<(), pam_sys::PamReturnCode> {
        let c_str = std::ffi::CString::new(name_value).unwrap();
        let rc = unsafe { pam_sys::raw::pam_putenv(self.handle, c_str.as_ptr()) };
        if rc == 0 {
            Ok(())
        } else {
            Err(unsafe { std::mem::transmute(rc as u8) })
        }
    }

    fn authenticate(&mut self) -> Result<(), pam_sys::PamReturnCode> {
        unsafe {
            let rc = pam_sys::authenticate(&mut *self.handle, pam_sys::PamFlag::NONE);
            if rc != pam_sys::PamReturnCode::SUCCESS {
                return Err(rc);
            }

            let rc = pam_sys::acct_mgmt(&mut *self.handle, pam_sys::PamFlag::NONE);
            if rc != pam_sys::PamReturnCode::SUCCESS {
                return Err(rc);
            }
        }
        Ok(())
    }

    fn open_session(&mut self) -> Result<(), pam_sys::PamReturnCode> {
        unsafe {
            let rc = pam_sys::setcred(&mut *self.handle, pam_sys::PamFlag::ESTABLISH_CRED);
            if rc != pam_sys::PamReturnCode::SUCCESS {
                return Err(rc);
            }

            let rc = pam_sys::open_session(&mut *self.handle, pam_sys::PamFlag::NONE);
            if rc != pam_sys::PamReturnCode::SUCCESS {
                return Err(rc);
            }

            // Follow openSSH and call pam_setcred before and after open_session
            let rc = pam_sys::setcred(&mut *self.handle, pam_sys::PamFlag::REINITIALIZE_CRED);
            if rc != pam_sys::PamReturnCode::SUCCESS {
                return Err(rc);
            }
        }
        self.has_open_session = true;
        Ok(())
    }

    fn get_env(&mut self) -> Vec<(String, String)> {
        let mut vec = Vec::new();
        unsafe {
            let env_list = pam_sys::getenvlist(&mut *self.handle);
            if !env_list.is_null() {
                let mut idx = 0;
                loop {
                    let env_ptr = *env_list.offset(idx);
                    if !env_ptr.is_null() {
                        idx += 1;
                        let env_str = std::ffi::CStr::from_ptr(env_ptr).to_string_lossy();
                        let split: Vec<_> = env_str.splitn(2, '=').collect();
                        if split.len() == 2 {
                            vec.push((split[0].to_string(), split[1].to_string()));
                        }
                    } else {
                        break;
                    }
                }
                pam_sys::raw::pam_misc_drop_env(env_list as *mut *mut libc::c_char);
            }
        }
        vec
    }
}

impl Drop for PamSession {
    fn drop(&mut self) {
        unsafe {
            if self.has_open_session {
                pam_sys::close_session(&mut *self.handle, pam_sys::PamFlag::NONE);
            }
            let rc = pam_sys::setcred(&mut *self.handle, pam_sys::PamFlag::DELETE_CRED);
            pam_sys::end(&mut *self.handle, rc);
        }
    }
}

fn run_daemon() {
    use users::os::unix::UserExt;
    let uid = users::get_current_uid();
    if uid != 0 {
        eprintln!("[cce-display-manager] Error: Daemon mode must be run as root (UID 0). Effective UID: {}", uid);
        eprintln!("[cce-display-manager] For local development/testing, run with: cargo run -- --greeter");
        std::process::exit(1);
    }

    let raw_tty = std::fs::read_link("/proc/self/fd/0")
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "tty1".to_string());
    let is_real_tty = raw_tty.starts_with("tty");
    let tty_name = if is_real_tty { raw_tty } else { "tty1".to_string() };

    // Redirect stdout and stderr of the daemon to a log file
    let log_path = format!("/tmp/cce-display-manager-daemon-{}.log", tty_name);
    if let Ok(log_file) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
    {
        use std::os::unix::io::AsRawFd;
        let fd = log_file.as_raw_fd();
        unsafe {
            libc::dup2(fd, 1);
            libc::dup2(fd, 2);
        }
    }

    println!("[cce-display-manager] Starting display manager daemon on {}...", tty_name);

    let runtime_dir = format!("/run/cce-display-manager-{}", tty_name);
    if !std::path::Path::new(&runtime_dir).exists() {
        std::fs::create_dir_all(&runtime_dir).expect("failed to create runtime dir");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700))
            .expect("failed to set runtime dir permissions");
    }
    // Ensure polkit-agent-helper-1 has SUID root permissions so cce-authenticator can authenticate sessions
    let helper_paths = [
        "/usr/lib/polkit-1/polkit-agent-helper-1",
        "/usr/lib/policykit-1/polkit-agent-helper-1",
    ];
    for path in &helper_paths {
        if std::path::Path::new(path).exists() {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(metadata) = std::fs::metadata(path) {
                let mut perms = metadata.permissions();
                let mode = perms.mode();
                if (mode & 0o4000) == 0 {
                    println!("[cce-display-manager] Restoring SUID root permissions to {} (current mode: {:o})", path, mode);
                    perms.set_mode(mode | 0o4000 | 0o0111);
                    if let Err(e) = std::fs::set_permissions(path, perms) {
                        eprintln!("[cce-display-manager] Failed to set permissions on {}: {}", path, e);
                    }
                }
            }
        }
    }

    loop {
        if is_real_tty {
            println!("[cce-display-manager] Waiting for {} to become the active TTY...", tty_name);
            loop {
                if let Ok(active_tty) = std::fs::read_to_string("/sys/class/tty/tty0/active") {
                    let active_tty = active_tty.trim();
                    if active_tty == tty_name {
                        println!("[cce-display-manager] {} is now active. Spawning greeter.", tty_name);
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }

        println!("[cce-display-manager] Spawning greeter session via cage...");
 
        let mut exe_path = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("/usr/bin/cce-display-manager"));
        if !exe_path.exists() {
            exe_path = std::path::PathBuf::from("/usr/bin/cce-display-manager");
        }

        let mut child = std::process::Command::new("cage")
            .arg("-s")
            .arg("--")
            .arg(exe_path)
            .arg("--greeter")
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env("LIBSEAT_BACKEND", "seatd")
            .env("WLR_DRM_NO_MODIFIERS", "1")
            .env("WLR_DRM_DEVICES", "/dev/dri/card1:/dev/dri/card0")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn cage compositor wrapper. Is cage installed?");

        let stdout = child.stdout.take().expect("failed to open child stdout");
        let reader = std::io::BufReader::new(stdout);
        let mut auth_success = None;

        use std::io::BufRead;
        for line in reader.lines() {
            if let Ok(line_str) = line {
                if line_str.starts_with("AUTH_SUCCESS|") {
                    let parts: Vec<&str> = line_str.split('|').collect();
                    if parts.len() == 5 {
                        let username = parts[1].to_string();
                        let exec = parts[2].to_string();
                        let is_wayland = parts[3].parse::<bool>().unwrap_or(true);
                        let password = parts[4].to_string();
                        println!("[greeter-stdout] AUTH_SUCCESS|{}|{}|{}", username, exec, is_wayland);
                        auth_success = Some((username, exec, is_wayland, password));
                    }
                } else {
                    println!("[greeter-stdout] {}", line_str);
                }
            }
        }

        let status = child.wait().expect("failed to wait on child process");
        println!("[cce-display-manager] Greeter session exited with status: {}", status);

        if status.code() == Some(130) {
            println!("[cce-display-manager] Abort requested via Ctrl+C. Exiting display manager daemon.");
            std::process::exit(0);
        }

        if status.code() == Some(135) {
            println!("[cce-display-manager] Restart requested via F5. Re-executing daemon...");
            let mut exe_path = std::path::PathBuf::from("/usr/bin/cce-display-manager");
            if !exe_path.exists() {
                exe_path = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("/usr/bin/cce-display-manager"));
            }
            let args: Vec<String> = std::env::args().collect();
            use std::os::unix::process::CommandExt;
            let mut cmd = std::process::Command::new(&exe_path);
            cmd.args(&args[1..]);
            let err = cmd.exec();
            eprintln!("[cce-display-manager] Failed to re-exec daemon: {:?}", err);
        }

        if auth_success.is_none() {
            // Sleep briefly to prevent high CPU usage if the greeter keeps crashing on startup
            std::thread::sleep(std::time::Duration::from_millis(1000));
        }

        if let Some((username, exec, is_wayland, password)) = auth_success {
            // Write last logged-in user and session to persistent files
            let var_lib = "/var/lib/cce-display-manager";
            if let Err(e) = std::fs::create_dir_all(var_lib) {
                eprintln!("[cce-display-manager] Failed to create var lib dir: {:?}", e);
            } else {
                if let Err(e) = std::fs::write(format!("{}/last_user", var_lib), &username) {
                    eprintln!("[cce-display-manager] Failed to write last_user file: {:?}", e);
                }
                if let Err(e) = std::fs::write(format!("{}/last_session", var_lib), &exec) {
                    eprintln!("[cce-display-manager] Failed to write last_session file: {:?}", e);
                }
            }

            println!("[cce-display-manager] Launching user session Exec: '{}' (Wayland: {}) for user: '{}'", exec, is_wayland, username);
            
            let pid = unsafe { libc::fork() };
            if pid < 0 {
                eprintln!("[cce-display-manager] Fork failed: {}", std::io::Error::last_os_error());
                continue;
            } else if pid == 0 {
                // Child process: execute PAM session and spawn the compositor/user session
                let service = if password.is_empty() {
                    "cce-display-manager-autologin"
                } else {
                    "cce-display-manager"
                };
                let mut auth = match PamSession::new(service, &username, &password, 0, None) {
                    Ok(a) => a,
                    Err(_) => {
                        let fallback_service = if password.is_empty() {
                            "ly-autologin"
                        } else {
                            "login"
                        };
                        match PamSession::new(fallback_service, &username, &password, 0, None) {
                            Ok(a) => a,
                            Err(e) => {
                                eprintln!("[cce-display-manager] PAM Init Error in child: {:?}", e);
                                std::process::exit(1);
                            }
                        }
                    }
                };

                let session_type_env = if is_wayland {
                    "XDG_SESSION_TYPE=wayland"
                } else {
                    "XDG_SESSION_TYPE=x11"
                };
                let _ = auth.putenv(session_type_env);
                let _ = auth.putenv("XDG_SESSION_CLASS=user");

                if let Err(e) = auth.authenticate() {
                    eprintln!("[cce-display-manager] PAM Authentication failed in child: {:?}", e);
                    std::process::exit(1);
                }

                if let Err(e) = auth.open_session() {
                    eprintln!("[cce-display-manager] PAM Session failed in child: {:?}", e);
                    std::process::exit(1);
                }

                let pam_env = auth.get_env();
                println!("[cce-display-manager] PAM Environment variables: {:?}", pam_env);

                if let Some((_, session_id)) = pam_env.iter().find(|(k, _)| k == "XDG_SESSION_ID") {
                    println!("[cce-display-manager] Explicitly activating logind session {} via loginctl...", session_id);
                    let _ = std::process::Command::new("loginctl")
                        .arg("activate")
                        .arg(session_id)
                        .status();
                }

                let user = match users::get_user_by_name(&username) {
                    Some(u) => u,
                    None => {
                        eprintln!("[cce-display-manager] Error: User '{}' not found in system.", username);
                        std::process::exit(1);
                    }
                };

                let user_uid = user.uid();
                let user_gid = user.primary_group_id();
                let home_dir = user.home_dir().to_path_buf();
                let shell = user.shell().to_str().unwrap_or("/bin/bash").to_string();

                let user_runtime_dir = format!("/run/user/{}", user_uid);
                
                let (cmd_bin, cmd_args): (String, Vec<String>) = if is_wayland {
                    sanitize_exec(&exec)
                } else {
                    let (client_bin, client_args) = sanitize_exec(&exec);
                    let xinit_bin = "/usr/sbin/xinit".to_string();
                    let mut args = vec![client_bin];
                    args.extend(client_args);
                    args.push("--".to_string());
                    args.push("-keeptty".to_string());
                    (xinit_bin, args)
                };

                if cmd_bin.is_empty() {
                    eprintln!("[cce-display-manager] Error: Resolved execution command is empty.");
                    std::process::exit(1);
                }

                println!("[cce-display-manager] Spawning session: {} with args {:?} for UID={}, GID={}", cmd_bin, cmd_args, user_uid, user_gid);

                use std::os::unix::process::CommandExt;
                let mut session_cmd = std::process::Command::new(&cmd_bin);
                session_cmd
                    .args(&cmd_args)
                    .envs(pam_env)
                    .current_dir(&home_dir)
                    .env("USER", &username)
                    .env("LOGNAME", &username)
                    .env("HOME", home_dir.to_str().unwrap())
                    .env("SHELL", &shell)
                    .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
                    .env("XDG_RUNTIME_DIR", &user_runtime_dir)
                    .env("XDG_SESSION_TYPE", if is_wayland { "wayland" } else { "x11" })
                    .env("XDG_SESSION_CLASS", "user")
                    .stdin(std::process::Stdio::inherit())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit());

                // Filter out sudo env vars so they don't leak into the user session
                for key in &["SUDO_USER", "SUDO_UID", "SUDO_GID", "SUDO_COMMAND"] {
                    session_cmd.env_remove(key);
                }

                let username_c = std::ffi::CString::new(username.clone()).unwrap();
                unsafe {
                    session_cmd.pre_exec(move || {
                        if libc::initgroups(username_c.as_ptr(), user_gid as libc::gid_t) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        if libc::setgid(user_gid as libc::gid_t) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        if libc::setuid(user_uid as libc::uid_t) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }

                match session_cmd.spawn() {
                    Ok(mut child_proc) => {
                        let _ = child_proc.wait();
                    }
                    Err(e) => {
                        eprintln!("[cce-display-manager] Failed to launch session: {}", e);
                    }
                }
                println!("[cce-display-manager] User session ended.");
                std::mem::drop(auth);
                std::process::exit(0);
            } else {
                // Parent process: block until the session worker child terminates
                let mut status: libc::c_int = 0;
                unsafe {
                    libc::waitpid(pid, &mut status, 0);
                }
                println!("[cce-display-manager] Session worker child (PID {}) exited with status: {}", pid, status);

                if let Some(vt) = tty_name.strip_prefix("tty").and_then(|s| s.parse::<u32>().ok()) {
                    println!("[cce-display-manager] Switching back to VT {}...", vt);
                    let _ = std::process::Command::new("chvt")
                        .arg(vt.to_string())
                        .status();
                }
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "--greeter" {
        run_greeter();
    } else {
        run_daemon();
    }
}
