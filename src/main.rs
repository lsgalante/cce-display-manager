use cce_ui::widget::{
    Button, ContentBg, TextLabel, Element, ElementState, MouseButton, Key, NamedKey, KeyEvent, TextBox,
    Widget, Container, focus, MouseScrollDelta
};
use cce_ui::context::UiContext;
use wayland_client::QueueHandle;
use cce_ui::engine::{EngineState, LogicalPosition, LogicalSize, WindowSettings, Vertex, quad_vertices};
use calloop::channel;





fn widget_vertices(w: &dyn Element, sw: f32, sh: f32) -> Vec<Vertex> {
    let (x, y, ww, h) = w.rect();
    quad_vertices(x, y, ww, h, sw, sh, w.color()).to_vec()
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
    base: Widget,
}

impl LoginCard {
    fn new() -> Self {
        Self { base: Widget::new() }
    }
}

impl Element for LoginCard {
    fn base(&self) -> Option<&Widget> { Some(&self.base) }
    fn base_mut(&mut self) -> Option<&mut Widget> { Some(&mut self.base) }
    fn as_ptr(&self) -> *mut (dyn Element + 'static) {
        self as *const Self as *mut Self as *mut (dyn Element + 'static)
    }
    fn as_ptr_mut(&mut self) -> *mut (dyn Element + 'static) {
        self as *mut Self as *mut (dyn Element + 'static)
    }
    fn color(&self) -> [f32; 4] { [0.25, 0.25, 0.28, 0.75] } // Premium gray card background with transparency

    fn extra_quads(&self) -> Vec<(f32, f32, f32, f32, [f32; 4])> {
        vec![]
    }


    fn paint_self(&self, _ui: &UiContext, ctx: &mut cce_ui::scene::paint::PaintCtx) {
        // A container with OWN (non-aggregating) labels: the default paint_self drops
        // container text — it assumes container text_labels aggregate children. Emit the
        // card's header labels here. Geometry intentionally stays out: the card plate is
        // drawn via custom_vertices (circular clip disabled), not the display list.
        for tl in self.own_labels() {
            ctx.text(tl.text, tl.x, tl.y, tl.font_size, tl.color);
        }
    }
}

#[derive(Debug, Clone)]
struct StatusLabel {
    base: Widget,
    pub text: String,
    pub is_error: bool,
}

impl StatusLabel {
    fn new(text: String) -> Self {
        Self { base: Widget::new(), text, is_error: false }
    }
}

impl Element for StatusLabel {
    // Leaf legacy widget: own labels via paint_self (cce-ui's default no longer drains
    // the text getters).
    fn paint_self(&self, ui: &UiContext, ctx: &mut cce_ui::scene::paint::PaintCtx) {
        cce_ui::scene::painter::paint_legacy_leaf(
            self, ui, ctx,
            cce_ui::scene::painter::fonted_leaf_labels(self, ui, self.own_labels()),
        );
    }

    fn base(&self) -> Option<&Widget> { Some(&self.base) }
    fn base_mut(&mut self) -> Option<&mut Widget> { Some(&mut self.base) }
    fn as_ptr(&self) -> *mut (dyn Element + 'static) {
        self as *const Self as *mut Self as *mut (dyn Element + 'static)
    }
    fn as_ptr_mut(&mut self) -> *mut (dyn Element + 'static) {
        self as *mut Self as *mut (dyn Element + 'static)
    }
    fn color(&self) -> [f32; 4] { [0.0, 0.0, 0.0, 0.0] } // Transparent background

}

#[derive(Debug, Clone)]
struct SessionList {
    base: Widget,
    sessions: Vec<Session>,
    selected_idx: usize,
    hovered_idx: Option<usize>,
}

impl SessionList {
    fn new(sessions: Vec<Session>) -> Self {
        Self {
            base: Widget::new(),
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
    // Leaf legacy widget: own labels via paint_self (cce-ui's default no longer drains
    // the text getters).
    fn paint_self(&self, ui: &UiContext, ctx: &mut cce_ui::scene::paint::PaintCtx) {
        cce_ui::scene::painter::paint_legacy_leaf(
            self, ui, ctx,
            cce_ui::scene::painter::fonted_leaf_labels(self, ui, self.own_labels()),
        );
    }

    fn base(&self) -> Option<&Widget> { Some(&self.base) }
    fn base_mut(&mut self) -> Option<&mut Widget> { Some(&mut self.base) }
    fn as_ptr(&self) -> *mut (dyn Element + 'static) {
        self as *const Self as *mut Self as *mut (dyn Element + 'static)
    }
    fn as_ptr_mut(&mut self) -> *mut (dyn Element + 'static) {
        self as *mut Self as *mut (dyn Element + 'static)
    }
    fn color(&self) -> [f32; 4] { [0.07, 0.07, 0.10, 0.70] } // Semi-transparent sleek dark card background

    fn extra_quads(&self) -> Vec<(f32, f32, f32, f32, [f32; 4])> {
        let mut quads = Vec::new();
        
        // 1. Panel borders (subtle blue accent)
        let border_color = [0.20, 0.40, 0.65, 0.5];
        quads.push((self.base.x, self.base.y, self.base.w, 1.5, border_color)); // top
        quads.push((self.base.x, self.base.y + self.base.h - 1.5, self.base.w, 1.5, border_color)); // bottom
        quads.push((self.base.x, self.base.y, 1.5, self.base.h, border_color)); // left
        quads.push((self.base.x + self.base.w - 1.5, self.base.y, 1.5, self.base.h, border_color)); // right
        
        let item_w = self.base.w - 20.0;
        
        // 2. Selected item background
        let selected_color = [0.20, 0.40, 0.65, 0.8]; // Solid blue highlight
        let sel_y = self.base.y + 40.0 + self.selected_idx as f32 * 36.0;
        quads.push((self.base.x + 10.0, sel_y, item_w, 32.0, selected_color));
        
        // 3. Hovered item background
        if let Some(h_idx) = self.hovered_idx {
            if h_idx != self.selected_idx && h_idx < self.sessions.len() {
                let hover_color = [1.0, 1.0, 1.0, 0.06]; // Subtle white overlay
                let h_y = self.base.y + 40.0 + h_idx as f32 * 36.0;
                quads.push((self.base.x + 10.0, h_y, item_w, 32.0, hover_color));
            }
        }
        
        quads
    }


    fn on_cursor_moved(&mut self, px: f32, py: f32, ctx: &mut UiContext) -> bool {
        let old_hovered = self.hovered_idx;
        self.hovered_idx = None;
        if self.hit_test(px, py, ctx) {
            let item_w = self.base.w - 20.0;
            for i in 0..self.sessions.len() {
                let item_y = self.base.y + 40.0 + i as f32 * 36.0;
                let item_x = self.base.x + 10.0;
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
                let item_w = self.base.w - 20.0;
                for i in 0..self.sessions.len() {
                    let item_y = self.base.y + 40.0 + i as f32 * 36.0;
                    let item_x = self.base.x + 10.0;
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
    bg: cce_ui::widget::Adapted<ContentBg>,
    card: LoginCard,
    username_box: cce_ui::widget::Adapted<TextBox>,
    password_box: cce_ui::widget::Adapted<TextBox>,
    login_btn: cce_ui::widget::Adapted<cce_ui::widget::Button>,
    status_lbl: StatusLabel,
    session_list: SessionList,
    ui_context: cce_ui::context::UiContext,
    root_container: Container,


    cursor_x: f32,
    cursor_y: f32,

    width: f32,
    height: f32,
    physical_width: u32,
    physical_height: u32,
    scale: f64,

    // State tracking
    login_success: bool,
    is_authenticating: bool,
    auth_request_id: u64,
    auth_sender: channel::Sender<AuthEvent>,
    auth_receiver: Option<channel::Channel<AuthEvent>>,

}

impl State {
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

    /// (Re-)register the widget tree at the widgets' CURRENT addresses. `new()` cannot do
    /// this — it would capture pointers into its own stack frame that dangle once the State
    /// moves — so this runs at the top of every frame. register/link are id-keyed and
    /// idempotent, and everything that resolves id→ptr afterwards (the paint walk's descent,
    /// propagate_event, the all_* child aggregation) then reads live widgets.
    fn relink_tree(&mut self) {
        let ctx = &mut self.ui_context;
        focus::link_parent_child(&mut self.root_container, &mut self.card, ctx);
        focus::link_parent_child(&mut self.root_container, &mut self.session_list, ctx);
        focus::link_parent_child(&mut self.card, &mut self.username_box, ctx);
        focus::link_parent_child(&mut self.card, &mut self.password_box, ctx);
        focus::link_parent_child(&mut self.card, &mut self.login_btn, ctx);
        focus::link_parent_child(&mut self.card, &mut self.status_lbl, ctx);
        // Initial focus: new() only set the box's own flag; point the context at the live
        // widget carrying it. Never fires once a runtime set_focused/clear_focus has run
        // (clear_focus also clears both flags).
        if self.ui_context.focused_widget.is_none() {
            if self.username_box.base().is_some_and(|b| b.focused) {
                self.ui_context.set_focused(&mut self.username_box);
            } else if self.password_box.base().is_some_and(|b| b.focused) {
                self.ui_context.set_focused(&mut self.password_box);
            }
        }
    }

    #[allow(dead_code)]
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

        // Root container spans the whole screen
        self.root_container.set_rect(0.0, 0.0, sw, sh);

        // Center card configuration
        let card_w = 360.0;
        let card_h = 280.0;
        let card_x = (sw - card_w) / 2.0;
        let card_y = (sh - card_h) / 2.0;
        
        // Card is full screen to render aspect-ratio centered oval custom graphic
        self.card.set_rect(0.0, 0.0, sw, sh);

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

    pub fn widgets_cursor_moved(&mut self, cx: f32, cy: f32) -> bool {
        let mut changed = false;
        if self.bg.cursor_moved(cx, cy, &mut self.ui_context) {
            changed = true;
        }
        let event = cce_ui::widget::Event::PointerMove {
            x: cx,
            y: cy,
            local_x: cx,
            local_y: cy,
        };
        let root_ptr = self.root_container.as_ptr_mut();
        if self.ui_context.propagate_event(&event, root_ptr) {
            changed = true;
        }
        changed
    }

    pub fn widgets_mouse_input(&mut self, button: MouseButton, state: ElementState, cx: f32, cy: f32) -> bool {
        let mut changed = false;
        if self.bg.mouse_input(button, state, cx, cy, &mut self.ui_context) {
            changed = true;
        }
        let event = cce_ui::widget::Event::MouseButton {
            button,
            state,
            x: cx,
            y: cy,
            local_x: cx,
            local_y: cy,
        };
        let root_ptr = self.root_container.as_ptr_mut();
        let handled = self.ui_context.propagate_event(&event, root_ptr);
        if button == MouseButton::Left && state == ElementState::Pressed {
            if !handled {
                self.ui_context.clear_focus();
                self.username_box.unfocus();
                self.password_box.unfocus();
                changed = true;
            }
        }
        if handled {
            changed = true;
        }
        changed
    }

    pub fn widgets_keyboard_input(&mut self, event: &KeyEvent) -> bool {
        let mut changed = false;
        if self.bg.keyboard_input(event, &mut self.ui_context) {
            changed = true;
        }
        let ui_event = cce_ui::widget::Event::KeyInput(event.clone());
        let root_ptr = self.root_container.as_ptr_mut();
        if self.ui_context.propagate_event(&ui_event, root_ptr) {
            changed = true;
        }
        changed
    }
    fn trigger_auth(&mut self) {
        let username = self.username_box.text.trim().to_string();
        let password = self.password_box.text.trim().to_string();

        if username.is_empty() {
            self.status_lbl.text = "Username cannot be empty".to_string();
            self.status_lbl.is_error = true;
            self.ui_context.set_focused(&mut self.username_box);
            self.username_box.focus();
        } else if password.is_empty() {
            if is_fprint_enabled() {
                self.auth_request_id += 1;
                self.status_lbl.text = "Scan finger to login or type password".to_string();
                self.status_lbl.is_error = false;
                self.is_authenticating = true;
                self.login_btn.base_mut().unwrap().label = Some("Authenticating...".to_string());
                authenticate_user(self.auth_request_id, username, password, self.auth_sender.clone());
            } else {
                self.status_lbl.text = "Password cannot be empty".to_string();
                self.status_lbl.is_error = true;
                self.ui_context.set_focused(&mut self.password_box);
                self.password_box.focus();
            }
        } else {
            self.auth_request_id += 1;
            self.status_lbl.text = "Authenticating...".to_string();
            self.status_lbl.is_error = false;
            self.is_authenticating = true;
            self.login_btn.base_mut().unwrap().label = Some("Authenticating...".to_string());
            authenticate_user(self.auth_request_id, username, password, self.auth_sender.clone());
        }
    }
}

impl cce_ui::engine::Application for State {
    type Message = String;

    fn new(_qh: &QueueHandle<cce_ui::engine::EngineState<Self>>, _sender: channel::Sender<Self::Message>) -> Self {
        let (auth_sender, auth_receiver) = channel::channel::<AuthEvent>();

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

        let sessions = discover_sessions();
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

        let mut app = Self {
            bg,
            card,
            username_box,
            password_box,
            login_btn,
            status_lbl,
            session_list,
            ui_context: cce_ui::context::UiContext::new(),
            root_container: Container::new(),
            cursor_x: 0.0,
            cursor_y: 0.0,
            width: 1024.0,
            height: 768.0,
            physical_width: 1024,
            physical_height: 768,
            scale: 1.0,
            login_success: false,
            is_authenticating: false,
            auth_request_id: 0,
            auth_sender,
            auth_receiver: Some(auth_receiver),
        };

        // The widget tree is NOT linked here: `app` is a stack local inside new(), so any
        // pointer registered now (tree registry, ui_context.focused_widget) dangles the
        // moment the State moves to its final address. relink_tree() registers the live
        // addresses at the top of every frame instead. Only the widgets' own focus FLAGS
        // (which move with the struct) are set here; relink_tree points focused_widget at
        // the flagged box.
        let has_username = !app.username_box.text.trim().to_string().is_empty();
        if has_username {
            app.password_box.focus();
        } else {
            app.username_box.focus();
        }

        // Start background fingerprint/empty-password authentication if username is prepopulated and fprintd is enabled!
        let username = app.username_box.text.trim().to_string();
        if !username.is_empty() {
            if is_fprint_enabled() {
                app.auth_request_id += 1;
                app.is_authenticating = true;
                app.login_btn.base_mut().unwrap().label = Some("Authenticating...".to_string());
                app.status_lbl.text = "Scan finger to login or type password".to_string();
                authenticate_user(app.auth_request_id, username, String::new(), app.auth_sender.clone());
            }
        }

        app
    }

    fn settings(&self) -> WindowSettings {
        WindowSettings {
            title: "CCE Display Manager".to_string(),
            app_id: "cce-display-manager".to_string(),
            width: 1024,
            height: 768,
            fullscreen: false,
            min_size: Some((1024, 768)),
        }
    }

    fn update(&mut self, _msg: Self::Message, _needs_rebuild: &mut bool, _exit: &mut bool) {}

    fn tick(&mut self, _dt: f32, _needs_rebuild: &mut bool) {}

    fn display_list(&mut self, size: LogicalSize, scale: f64) -> Option<cce_ui::scene::paint::DisplayList> {
        // Phase 6ah single paint path: the widget geometry (the legacy view_rounded_quads
        // then view() bodies, in the wrapper's order) and all text are this one list. The
        // card — the soft radial-glow blob with the circular clip disabled — stays in
        // custom_vertices, appended on top exactly as before (it is the escape-hatch layer,
        // not part of the display-list geometry).
        use cce_ui::scene::layout::Rect;
        self.relink_tree();
        if (self.width - size.width as f32).abs() > 0.001 || (self.height - size.height as f32).abs() > 0.001 || (self.scale - scale).abs() > 0.001 {
            self.width = size.width as f32;
            self.height = size.height as f32;
            self.physical_width = (size.width * scale as f32) as u32;
            self.physical_height = (size.height * scale as f32) as u32;
            self.scale = scale;
            self.apply_layout();
        }

        let mut pc = cce_ui::scene::paint::PaintCtx::new();

        for w in self.widgets_iter() {
            let is_card = w.base().map_or(false, |b| std::ptr::eq(b, &self.card.base));
            if is_card {
                continue;
            }
            for (qx, qy, qw, qh, qr, qc, qcorners) in w.all_rounded_quads(&self.ui_context) {
                let rect = Rect { x: qx, y: qy, width: qw, height: qh };
                if qr > 0.1 {
                    pc.rounded_rect(rect, qr, qcorners, qc);
                } else {
                    pc.quad(rect, qc);
                }
            }
        }

        for w in self.widgets_iter() {
            let is_card = w.base().map_or(false, |b| std::ptr::eq(b, &self.card.base));
            if is_card {
                continue;
            }
            for (qx, qy, qw, qh, qc) in w.all_quads(&self.ui_context) {
                pc.quad(Rect { x: qx, y: qy, width: qw, height: qh }, qc);
            }
        }

        pc.text_with(
            "Press Tab to switch fields • Session selector: Click current session label".to_string(),
            20.0,
            self.height - 24.0,
            11.0,
            [0x60, 0x60, 0x6e],
            None,
            None,
        );
        pc.text_with(
            concat!("Built ", env!("CCE_BUILD_DATE")).to_string(),
            self.width - 130.0,
            self.height - 24.0,
            11.0,
            [0x60, 0x60, 0x6e],
            None,
            None,
        );
        // Widget text via the paint walk, over the TRUE roots: root_container descends into
        // the card (whose paint_self override carries its header labels) and its input
        // children, plus the session list; bg is a standalone leaf. Walking the flat
        // widgets_iter would emit the card's children twice (once via descent, once as
        // standalone roots).
        cce_ui::scene::painter::append_widget_text(&self.ui_context, &self.bg, &mut pc);
        cce_ui::scene::painter::append_widget_text(&self.ui_context, &self.root_container, &mut pc);

        Some(pc.finish())
    }

    fn display_list_text(&self) -> bool {
        true
    }

    fn custom_vertices(&mut self, verts: &mut Vec<Vertex>, _size: LogicalSize, _scale: f64) {
        let sw = self.width;
        let sh = self.height;

        let mut card_verts = widget_vertices(&self.card, sw, sh);
        for v in &mut card_verts {
            v.clip_circle = [-999.0, 0.0, 0.0];
        }
        verts.extend(card_verts);

        for (qx, qy, qw, qh, qc) in self.card.all_quads(&self.ui_context) {
            let mut q_verts = quad_vertices(qx, qy, qw, qh, sw, sh, qc).to_vec();
            for v in &mut q_verts {
                v.clip_circle = [-999.0, 0.0, 0.0];
            }
            verts.extend(q_verts);
        }
    }

    fn register_sources(&mut self, handle: &calloop::LoopHandle<'_, EngineState<Self>>) {
        if let Some(auth_receiver) = self.auth_receiver.take() {
            handle.insert_source(auth_receiver, |event, _metadata, engine_state| {
                let app = engine_state.inner.as_mut().unwrap();
                let mut redraw = false;
                match event {
                    channel::Event::Msg(msg) => {
                        let ev_request_id = match &msg {
                            AuthEvent::Success { request_id, .. } => *request_id,
                            AuthEvent::Failure { request_id, .. } => *request_id,
                            AuthEvent::Info { request_id, .. } => *request_id,
                        };

                        if ev_request_id != app.auth_request_id {
                            return;
                        }

                        match msg {
                            AuthEvent::Success { username, .. } => {
                                app.is_authenticating = false;
                                app.login_btn.base_mut().unwrap().label = Some("Log In".to_string());
                                app.status_lbl.text = format!("Welcome, {}!", username);
                                app.status_lbl.is_error = false;
                                app.login_success = true;
                                if let Some(session) = app.session_list.selected_session() {
                                    println!("AUTH_SUCCESS|{}|{}|{}|{}", app.username_box.text.trim(), session.exec, session.is_wayland, app.password_box.text.trim());
                                    std::process::exit(0);
                                }
                            }
                            AuthEvent::Failure { err_msg, .. } => {
                                app.is_authenticating = false;
                                app.login_btn.base_mut().unwrap().label = Some("Log In".to_string());
                                app.status_lbl.text = err_msg;
                                app.status_lbl.is_error = true;
                                app.password_box.text.clear();
                                app.password_box.edit_buffer.clear();
                                app.ui_context.set_focused(&mut app.password_box);
                                app.password_box.focus();
                            }
                            AuthEvent::Info { msg, .. } => {
                                app.status_lbl.text = msg;
                                app.status_lbl.is_error = false;
                            }
                        }
                        redraw = true;
                    }
                    channel::Event::Closed => {}
                }
                if redraw {
                    engine_state.redraw = true;
                }
            }).unwrap();
        }
    }

    fn handle_pointer_move(&mut self, pos: LogicalPosition, needs_rebuild: &mut bool) {
        let lx = pos.x as f32;
        let ly = pos.y as f32;
        self.cursor_x = lx;
        self.cursor_y = ly;
        if self.widgets_cursor_moved(lx, ly) {
            *needs_rebuild = true;
        }
    }

    fn handle_mouse_input(&mut self, button: MouseButton, state: ElementState, pos: LogicalPosition, needs_rebuild: &mut bool) -> Option<Self::Message> {
        if self.is_authenticating {
            return None;
        }
        let lx = pos.x as f32;
        let ly = pos.y as f32;
        let mut changed = false;
        if self.widgets_mouse_input(button, state, lx, ly) {
            changed = true;
        }
        if button == MouseButton::Left && state == ElementState::Pressed {
            if self.login_btn.take_click() {
                self.trigger_auth();
                changed = true;
            }
        }
        if changed {
            *needs_rebuild = true;
        }
        None
    }

    fn handle_mouse_wheel(&mut self, _delta: &MouseScrollDelta, _pos: LogicalPosition, _needs_rebuild: &mut bool) {}

    fn handle_key_input(&mut self, event: &KeyEvent, needs_rebuild: &mut bool) -> Option<Self::Message> {
        let logical_key = &event.logical_key;
        let ctrl_pressed = event.ctrl;

        // Check for Ctrl+C to abort/exit back to TTY
        if ctrl_pressed && (logical_key == &Key::Character("c".to_string()) || logical_key == &Key::Character("C".to_string())) {
            log::error!("Ctrl+C pressed. Aborting greeter.");
            std::process::exit(130);
        }

        // Check for F5 to request daemon restart
        if logical_key == &Key::Named(NamedKey::F5) {
            log::info!("F5 pressed. Requesting daemon restart.");
            std::process::exit(135);
        }

        let is_ctrl_p = ctrl_pressed && (logical_key == &Key::Character("p".to_string()) || logical_key == &Key::Character("P".to_string()));
        let is_ctrl_n = ctrl_pressed && (logical_key == &Key::Character("n".to_string()) || logical_key == &Key::Character("N".to_string()));

        if event.state == ElementState::Pressed {
            // If we are currently in fingerprint authentication and the user starts typing a password,
            // cancel the fingerprint auth and let them type.
            if self.is_authenticating {
                if self.password_box.text.is_empty() {
                    let is_typing = !ctrl_pressed && match logical_key {
                        Key::Character(_) | Key::Named(NamedKey::Backspace) | Key::Named(NamedKey::Delete) | Key::Named(NamedKey::Space) => true,
                        _ => false,
                    };
                    if is_typing {
                        self.auth_request_id += 1;
                        self.is_authenticating = false;
                        self.login_btn.base_mut().unwrap().label = Some("Log In".to_string());
                        self.status_lbl.text = "Enter password to start".to_string();
                        self.status_lbl.is_error = false;
                    } else {
                        let is_nav = match logical_key {
                            Key::Named(NamedKey::ArrowUp) | Key::Named(NamedKey::ArrowDown) | Key::Named(NamedKey::Tab) => true,
                            _ => is_ctrl_p || is_ctrl_n,
                        };
                        if !is_nav {
                            return None;
                        }
                    }
                } else {
                    return None;
                }
            }

            let mut changed = false;

            // Handle Up/Down or Ctrl+P/N navigation to cycle sessions
            let cycle_up = (logical_key == &Key::Named(NamedKey::ArrowUp) || is_ctrl_p) && !self.session_list.sessions.is_empty();
            let cycle_down = (logical_key == &Key::Named(NamedKey::ArrowDown) || is_ctrl_n) && !self.session_list.sessions.is_empty();

            if cycle_up {
                let len = self.session_list.sessions.len();
                self.session_list.selected_idx = (self.session_list.selected_idx + len - 1) % len;
                self.session_list.hovered_idx = None;
                changed = true;
            } else if cycle_down {
                let len = self.session_list.sessions.len();
                self.session_list.selected_idx = (self.session_list.selected_idx + 1) % len;
                self.session_list.hovered_idx = None;
                changed = true;
            } else if logical_key == &Key::Named(NamedKey::Tab) {
                let is_user_focused = self.username_box.focused(&self.ui_context);
                if is_user_focused {
                    self.ui_context.set_focused(&mut self.password_box);
                    self.username_box.unfocus();
                    self.password_box.focus();
                } else {
                    self.ui_context.set_focused(&mut self.username_box);
                    self.password_box.unfocus();
                    self.username_box.focus();
                }
                changed = true;
            } else if logical_key == &Key::Named(NamedKey::Enter) && self.password_box.focused(&self.ui_context) {
                self.password_box.keyboard_input(event, &mut self.ui_context);
                self.trigger_auth();
                changed = true;
            } else if logical_key == &Key::Named(NamedKey::Enter) && self.username_box.focused(&self.ui_context) {
                self.username_box.keyboard_input(event, &mut self.ui_context);
                self.ui_context.set_focused(&mut self.password_box);
                self.username_box.unfocus();
                self.password_box.focus();
                changed = true;
            } else {
                if self.widgets_keyboard_input(event) {
                    changed = true;
                }
            }

            if changed {
                *needs_rebuild = true;
            }
        }

        None
    }

    fn clear_color(&self) -> [f32; 4] {
        [0.03, 0.03, 0.05, 1.0]
    }
}

#[derive(Debug, Clone)]
enum AuthEvent {
    Success { request_id: u64, username: String },
    Failure { request_id: u64, err_msg: String },
    Info { request_id: u64, msg: String },
}

fn is_fprint_enabled() -> bool {
    std::fs::read_to_string("/etc/pam.d/cce-display-manager")
        .map(|content| {
            content.lines().any(|line| {
                let trimmed = line.trim();
                trimmed.contains("pam_fprintd.so") && !trimmed.starts_with('#')
            })
        })
        .unwrap_or(false)
}

fn authenticate_user(request_id: u64, username: String, password: String, sender: channel::Sender<AuthEvent>) {
    std::thread::spawn(move || {
        let service = if password.is_empty() {
            "cce-display-manager"
        } else {
            "cce-display-manager-password"
        };
        
        let mut auth = match PamSession::new(service, &username, &password, request_id, Some(sender.clone())) {
            Ok(a) => a,
            Err(e) => {
                let _ = sender.send(AuthEvent::Failure { request_id, err_msg: format!("{:?}", e) });
                return;
            }
        };

        if let Err(e) = auth.authenticate() {
            let _ = sender.send(AuthEvent::Failure { request_id, err_msg: format!("{:?}", e) });
            return;
        }

        if let Err(e) = auth.open_session() {
            let _ = sender.send(AuthEvent::Failure { request_id, err_msg: format!("{:?}", e) });
            return;
        }

        let _ = sender.send(AuthEvent::Success { request_id, username });
    });
}

#[derive(serde::Deserialize, Debug, Default)]
struct SystemConfig {
    scale: Option<f64>,
}

fn load_system_config() -> SystemConfig {
    let path = "/etc/cce/cce.json";
    if std::path::Path::new(path).exists() {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(config) = serde_json::from_str(&content) {
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

    cce_ui::engine::run::<State>();

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
        log::error!("Error: Daemon mode must be run as root (UID 0). Effective UID: {}", uid);
        log::info!("For local development/testing, run with: cargo run -- --greeter");
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

    log::info!("Starting display manager daemon on {}...", tty_name);

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
                    log::info!("Restoring SUID root permissions to {} (current mode: {:o})", path, mode);
                    perms.set_mode(mode | 0o4000 | 0o0111);
                    if let Err(e) = std::fs::set_permissions(path, perms) {
                        log::error!("Failed to set permissions on {}: {}", path, e);
                    }
                }
            }
        }
    }

    loop {
        if is_real_tty {
            log::info!("Waiting for {} to become the active TTY...", tty_name);
            loop {
                if let Ok(active_tty) = std::fs::read_to_string("/sys/class/tty/tty0/active") {
                    let active_tty = active_tty.trim();
                    if active_tty == tty_name {
                        log::info!("{} is now active. Spawning greeter.", tty_name);
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }

        log::info!("Spawning greeter session via cage...");
 
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
                        log::info!("[greeter-stdout] AUTH_SUCCESS|{}|{}|{}", username, exec, is_wayland);
                        auth_success = Some((username, exec, is_wayland, password));
                    }
                } else {
                    log::info!("[greeter-stdout] {}", line_str);
                }
            }
        }

        let status = child.wait().expect("failed to wait on child process");
        log::info!("Greeter session exited with status: {}", status);

        if status.code() == Some(130) {
            log::info!("Abort requested via Ctrl+C. Exiting display manager daemon.");
            std::process::exit(0);
        }

        if status.code() == Some(135) {
            log::info!("Restart requested via F5. Re-executing daemon...");
            let mut exe_path = std::path::PathBuf::from("/usr/bin/cce-display-manager");
            if !exe_path.exists() {
                exe_path = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("/usr/bin/cce-display-manager"));
            }
            let args: Vec<String> = std::env::args().collect();
            use std::os::unix::process::CommandExt;
            let mut cmd = std::process::Command::new(&exe_path);
            cmd.args(&args[1..]);
            let err = cmd.exec();
            log::error!("Failed to re-exec daemon: {:?}", err);
        }

        if auth_success.is_none() {
            // Sleep briefly to prevent high CPU usage if the greeter keeps crashing on startup
            std::thread::sleep(std::time::Duration::from_millis(1000));
        }

        if let Some((username, exec, is_wayland, password)) = auth_success {
            // Write last logged-in user and session to persistent files
            let var_lib = "/var/lib/cce-display-manager";
            if let Err(e) = std::fs::create_dir_all(var_lib) {
                log::error!("Failed to create var lib dir: {:?}", e);
            } else {
                if let Err(e) = std::fs::write(format!("{}/last_user", var_lib), &username) {
                    log::error!("Failed to write last_user file: {:?}", e);
                }
                if let Err(e) = std::fs::write(format!("{}/last_session", var_lib), &exec) {
                    log::error!("Failed to write last_session file: {:?}", e);
                }
            }

            log::info!("Launching user session Exec: '{}' (Wayland: {}) for user: '{}'", exec, is_wayland, username);
            
            let pid = unsafe { libc::fork() };
            if pid < 0 {
                log::error!("Fork failed: {}", std::io::Error::last_os_error());
                continue;
            } else if pid == 0 {
                // Child process: execute PAM session and spawn the compositor/user session
                let user = match users::get_user_by_name(&username) {
                    Some(u) => u,
                    None => {
                        log::error!("Error: User '{}' not found in system.", username);
                        std::process::exit(1);
                    }
                };

                let user_uid = user.uid();
                let user_gid = user.primary_group_id();
                let home_dir = user.home_dir().to_path_buf();
                let shell = user.shell().to_str().unwrap_or("/bin/bash").to_string();

                let user_runtime_dir = format!("/run/user/{}", user_uid);

                // Set environment variables in the session worker process before PAM open_session.
                // This is crucial for pam_gnome_keyring.so / pam_kwallet5.so to run successfully.
                std::env::set_var("USER", &username);
                std::env::set_var("LOGNAME", &username);
                std::env::set_var("HOME", home_dir.to_str().unwrap_or(""));
                std::env::set_var("SHELL", &shell);
                std::env::set_var("XDG_RUNTIME_DIR", &user_runtime_dir);

                let service = if password.is_empty() {
                    "cce-display-manager-autologin"
                } else {
                    "cce-display-manager-password"
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
                                log::error!("PAM Init Error in child: {:?}", e);
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
                    log::error!("PAM Authentication failed in child: {:?}", e);
                    std::process::exit(1);
                }

                if let Err(e) = auth.open_session() {
                    log::error!("PAM Session failed in child: {:?}", e);
                    std::process::exit(1);
                }

                let pam_env = auth.get_env();
                log::info!("PAM Environment variables: {:?}", pam_env);

                if let Some((_, session_id)) = pam_env.iter().find(|(k, _)| k == "XDG_SESSION_ID") {
                    log::info!("Explicitly activating logind session {} via loginctl...", session_id);
                    let _ = std::process::Command::new("loginctl")
                        .arg("activate")
                        .arg(session_id)
                        .status();
                }
                
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
                    log::error!("Error: Resolved execution command is empty.");
                    std::process::exit(1);
                }

                log::info!("Spawning session: {} with args {:?} for UID={}, GID={}", cmd_bin, cmd_args, user_uid, user_gid);

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
                        log::error!("Failed to launch session: {}", e);
                    }
                }
                log::info!("User session ended.");
                std::mem::drop(auth);
                std::process::exit(0);
            } else {
                // Parent process: block until the session worker child terminates
                let mut status: libc::c_int = 0;
                unsafe {
                    libc::waitpid(pid, &mut status, 0);
                }
                log::info!("Session worker child (PID {}) exited with status: {}", pid, status);

                if let Some(vt) = tty_name.strip_prefix("tty").and_then(|s| s.parse::<u32>().ok()) {
                    log::info!("Switching back to VT {}...", vt);
                    let _ = std::process::Command::new("chvt")
                        .arg(vt.to_string())
                        .status();
                }
            }
        }
    }
}

fn main() {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "--greeter" {
        run_greeter();
    } else {
        run_daemon();
    }
}

impl LoginCard {
    fn own_labels(&self) -> Vec<TextLabel> {
        let sw = self.base.w;
        let sh = self.base.h;
        let card_x = (sw - 360.0) / 2.0;
        let card_y = (sh - 300.0) / 2.0;
        vec![
            TextLabel {
                text: "CCE DISPLAY MANAGER".to_string(),
                x: card_x + 30.0,
                y: card_y + 30.0,
                font_size: 15.0,
                color: [0xee, 0xee, 0xf5],
            },
            TextLabel {
                text: "Authenticate to begin your session".to_string(),
                x: card_x + 30.0,
                y: card_y + 50.0,
                font_size: 11.0,
                color: [0x83, 0x83, 0x8a],
            },
        ]
    }
}

impl StatusLabel {
    fn own_labels(&self) -> Vec<TextLabel> {
        let col = if self.is_error {
            [0xee, 0x5c, 0x5c] // Soft red
        } else {
            [0x83, 0x83, 0x8a] // Dim text
        };
        vec![TextLabel {
            text: self.text.clone(),
            x: self.base.x,
            y: self.base.y,
            font_size: 11.0,
            color: col,
        }]
    }
}

impl SessionList {
    fn own_labels(&self) -> Vec<TextLabel> {
        let mut labels = Vec::new();
        
        // Header title
        labels.push(TextLabel {
            text: "SESSION MANAGER".to_string(),
            x: self.base.x + 15.0,
            y: self.base.y + 18.0,
            font_size: 11.0,
            color: [0x83, 0x83, 0x8a],
        });
        
        // Session items
        for (i, session) in self.sessions.iter().enumerate() {
            let item_y = self.base.y + 40.0 + i as f32 * 36.0;
            
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
                x: self.base.x + 20.0,
                y: item_y + 10.0,
                font_size: 12.0,
                color,
            });
        }
        
        labels
    }
}
