use cce_ui::widget::{
    Button, ContentBg, WidgetHost, ElementState, MouseButton, Key, NamedKey, KeyEvent, TextBox,
    focus, MouseScrollDelta
};
use wayland_client::QueueHandle;
use cce_ui::engine::{EngineState, LogicalPosition, LogicalSize, WindowSettings, Vertex, quad_vertices};
use calloop::channel;





fn widget_vertices(w: &dyn WidgetHost, sw: f32, sh: f32) -> Vec<Vertex> {
    let (x, y, ww, h) = w.rect();
    quad_vertices(x, y, ww, h, sw, sh, w.color()).to_vec()
}




#[derive(Debug, Clone)]
struct Session {
    name: String,
    exec: String,
    is_wayland: bool,
}

/// Parse a greeter `AUTH_SUCCESS|user|exec|is_wayland|password` line.
///
/// The password is the LAST field and is taken verbatim to the end of the
/// line (`splitn`), because it may itself contain `|` — a plain `split`
/// silently produced six fields and dropped the login (the greeter had
/// already authenticated, so the user just hung at a dead greeter).
fn parse_auth_success(line: &str) -> Option<(String, String, bool, String)> {
    let mut parts = line.splitn(5, '|');
    if parts.next() != Some("AUTH_SUCCESS") {
        return None;
    }
    let username = parts.next()?.to_string();
    let exec = parts.next()?.to_string();
    let is_wayland = parts.next()?.parse::<bool>().unwrap_or(true);
    let password = parts.next()?.to_string();
    Some((username, exec, is_wayland, password))
}

/// The greeter's half of [`parse_auth_success`]: the line it prints on stdout
/// for the daemon. The password goes VERBATIM — no trimming: a password with a
/// leading or trailing space is a password, and trimming it made that account
/// unable to log in here at all.
fn auth_success_line(username: &str, exec: &str, is_wayland: bool, password: &str) -> String {
    format!("AUTH_SUCCESS|{}|{}|{}|{}", username, exec, is_wayland, password)
}

/// The PAM service the session worker opens the session on. An empty password
/// is the fingerprint path (or a compositor-restart relaunch): the greeter
/// already verified the user, so the session opens on the autologin stack.
fn session_pam_service(password: &str) -> &'static str {
    if password.is_empty() {
        "cce-display-manager-autologin"
    } else {
        "cce-display-manager-password"
    }
}

/// A logind session id as `XDG_SESSION_ID` carries it — only then is it passed
/// to `loginctl`. Ids are short alphanumeric strings ("15", "c3").
fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.chars().all(|c| c.is_ascii_alphanumeric())
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

// ── Custom LoginCard Container WidgetHost (narrow traits, wrapped in Adapted) ──
#[derive(Debug, Clone)]
struct LoginCard;

impl LoginCard {
    fn new() -> cce_ui::widget::Adapted<LoginCard> {
        cce_ui::widget::Adapted::new(LoginCard)
    }
}

impl cce_ui::widget::Layout for LoginCard {}

impl cce_ui::widget::Paint for LoginCard {
    fn color(&self) -> [f32; 4] { [0.25, 0.25, 0.28, 0.75] } // Premium gray card background with transparency

    fn paint(&self, rect: cce_ui::scene::layout::Rect, pc: &mut cce_ui::scene::paint::PaintCtx) {
        // Only the card's header labels: the card plate (soft radial-glow blob) is drawn
        // via custom_vertices, not the display list — and the direct all_quads read in
        // custom_vertices relies on this paint emitting NO plain quads, like the legacy
        // empty extra_quads. The card is laid out full-screen; the header centers off it.
        let card_x = (rect.width - 360.0) / 2.0;
        let card_y = (rect.height - 300.0) / 2.0;
        pc.text("CCE DISPLAY MANAGER".to_string(), card_x + 30.0, card_y + 30.0, 15.0, [0xee, 0xee, 0xf5]);
        pc.text("Authenticate to begin your session".to_string(), card_x + 30.0, card_y + 50.0, 11.0, [0x83, 0x83, 0x8a]);
    }
}

impl cce_ui::widget::Input for LoginCard {}

#[derive(Debug, Clone)]
struct StatusLabel {
    pub text: String,
    pub is_error: bool,
}

impl StatusLabel {
    fn new(text: String) -> cce_ui::widget::Adapted<StatusLabel> {
        cce_ui::widget::Adapted::new(Self { text, is_error: false })
    }
}

impl cce_ui::widget::Layout for StatusLabel {}

impl cce_ui::widget::Paint for StatusLabel {
    fn color(&self) -> [f32; 4] { [0.0, 0.0, 0.0, 0.0] } // Transparent background

    fn paint(&self, rect: cce_ui::scene::layout::Rect, pc: &mut cce_ui::scene::paint::PaintCtx) {
        let col = if self.is_error {
            [0xee, 0x5c, 0x5c] // Soft red
        } else {
            [0x83, 0x83, 0x8a] // Dim text
        };
        pc.text(self.text.clone(), rect.x, rect.y, 11.0, col);
    }
}

impl cce_ui::widget::Input for StatusLabel {}

#[derive(Debug, Clone)]
struct SessionList {
    sessions: Vec<Session>,
    selected_idx: usize,
    hovered_idx: Option<usize>,
}

impl SessionList {
    fn new(sessions: Vec<Session>) -> cce_ui::widget::Adapted<SessionList> {
        cce_ui::widget::Adapted::new(Self {
            sessions,
            selected_idx: 0,
            hovered_idx: None,
        })
    }

    fn selected_session(&self) -> Option<&Session> {
        self.sessions.get(self.selected_idx)
    }

    /// Row rect of item `i` within the laid-out panel rect (header is 40px tall).
    fn item_rect(&self, rect: cce_ui::scene::layout::Rect, i: usize) -> (f32, f32, f32, f32) {
        (rect.x + 10.0, rect.y + 40.0 + i as f32 * 36.0, rect.width - 20.0, 32.0)
    }
}

impl cce_ui::widget::Layout for SessionList {}

impl cce_ui::widget::Paint for SessionList {
    fn color(&self) -> [f32; 4] { [0.07, 0.07, 0.10, 0.70] } // Semi-transparent sleek dark card background

    fn paint(&self, rect: cce_ui::scene::layout::Rect, pc: &mut cce_ui::scene::paint::PaintCtx) {
        use cce_ui::scene::layout::Rect;
        // The legacy panel never drew its base color through the display getters (no
        // rounded corners, extra_quads only) — same here: borders, selection, hover.
        let border_color = [0.20, 0.40, 0.65, 0.5];
        pc.quad(Rect { x: rect.x, y: rect.y, width: rect.width, height: 1.5 }, border_color); // top
        pc.quad(Rect { x: rect.x, y: rect.y + rect.height - 1.5, width: rect.width, height: 1.5 }, border_color); // bottom
        pc.quad(Rect { x: rect.x, y: rect.y, width: 1.5, height: rect.height }, border_color); // left
        pc.quad(Rect { x: rect.x + rect.width - 1.5, y: rect.y, width: 1.5, height: rect.height }, border_color); // right

        let item_w = rect.width - 20.0;

        // Selected item background
        let selected_color = [0.20, 0.40, 0.65, 0.8]; // Solid blue highlight
        let sel_y = rect.y + 40.0 + self.selected_idx as f32 * 36.0;
        pc.quad(Rect { x: rect.x + 10.0, y: sel_y, width: item_w, height: 32.0 }, selected_color);

        // Hovered item background
        if let Some(h_idx) = self.hovered_idx {
            if h_idx != self.selected_idx && h_idx < self.sessions.len() {
                let hover_color = [1.0, 1.0, 1.0, 0.06]; // Subtle white overlay
                let h_y = rect.y + 40.0 + h_idx as f32 * 36.0;
                pc.quad(Rect { x: rect.x + 10.0, y: h_y, width: item_w, height: 32.0 }, hover_color);
            }
        }

        // Header title + session rows
        pc.text("SESSION MANAGER".to_string(), rect.x + 15.0, rect.y + 18.0, 11.0, [0x83, 0x83, 0x8a]);
        for (i, session) in self.sessions.iter().enumerate() {
            let item_y = rect.y + 40.0 + i as f32 * 36.0;
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
            pc.text(display_name, rect.x + 20.0, item_y + 10.0, 12.0, color);
        }
    }
}

impl cce_ui::widget::Input for SessionList {
    fn on_event(&mut self, event: &cce_ui::widget::Event, ectx: &mut cce_ui::widget::EventCtx) -> bool {
        match event {
            // Hover row tracking — the legacy on_cursor_moved override, against the
            // routed rect (a move outside the panel clears the hover, as before).
            cce_ui::widget::Event::PointerMove { x, y, .. } => {
                let old_hovered = self.hovered_idx;
                self.hovered_idx = None;
                let r = ectx.rect;
                if *x >= r.x && *x <= r.x + r.width && *y >= r.y && *y <= r.y + r.height {
                    for i in 0..self.sessions.len() {
                        let (ix, iy, iw, ih) = self.item_rect(r, i);
                        if *x >= ix && *x <= ix + iw && *y >= iy && *y <= iy + ih {
                            self.hovered_idx = Some(i);
                            break;
                        }
                    }
                }
                self.hovered_idx != old_hovered
            }
            // Presses arrive hit-gated to the panel rect; select the clicked row.
            cce_ui::widget::Event::MouseButton {
                button: MouseButton::Left,
                state: ElementState::Pressed,
                x,
                y,
                ..
            } => {
                for i in 0..self.sessions.len() {
                    let (ix, iy, iw, ih) = self.item_rect(ectx.rect, i);
                    if *x >= ix && *x <= ix + iw && *y >= iy && *y <= iy + ih {
                        if self.selected_idx != i {
                            self.selected_idx = i;
                            return true;
                        }
                    }
                }
                false
            }
            _ => false,
        }
    }
}

// ── App State and Renderer ──
struct State {
    bg: cce_ui::widget::Adapted<ContentBg>,
    card: cce_ui::widget::Adapted<LoginCard>,
    username_box: cce_ui::widget::Adapted<TextBox>,
    password_box: cce_ui::widget::Adapted<TextBox>,
    login_btn: cce_ui::widget::Adapted<cce_ui::widget::Button>,
    status_lbl: cce_ui::widget::Adapted<StatusLabel>,
    session_list: cce_ui::widget::Adapted<SessionList>,
    ui_context: cce_ui::context::UiContext,


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
    // The fingerprint attempt runs in a helper *process* (`--fprint-auth`),
    // not a thread: pam_authenticate blocks inside pam_fprintd and cannot be
    // interrupted, but a process can be killed — and killing it drops its
    // D-Bus connection, which is what makes fprintd release the sensor claim.
    fprint_child: Option<std::process::Child>,
    /// The password the in-flight authentication is checking: what the
    /// daemon's session worker must be handed on success. Empty for a
    /// fingerprint attempt. NOT the password box's text at success time —
    /// a fingerprint can succeed while a password is half-typed, and handing
    /// the worker that partial password sent it down the password PAM stack
    /// to fail the login the greeter had just accepted.
    auth_password: String,
}

impl State {
    fn widgets_iter(&self) -> Vec<&dyn WidgetHost> {
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
        // Root Container DISSOLVED (Phase 6ax): the card and the session list are the two
        // dispatch/walk roots; register them directly (link_parent_child used to do it as a
        // side effect of the root links).
        ctx.register_widget(self.bg.id(), self.bg.as_ptr_mut());
        ctx.register_widget(self.card.id(), self.card.as_ptr_mut());
        ctx.register_widget(self.session_list.id(), self.session_list.as_ptr_mut());
        focus::link_parent_child(&mut self.card, &mut self.username_box, ctx);
        focus::link_parent_child(&mut self.card, &mut self.password_box, ctx);
        focus::link_parent_child(&mut self.card, &mut self.login_btn, ctx);
        focus::link_parent_child(&mut self.card, &mut self.status_lbl, ctx);
        // Initial focus: new() only set the box's own flag; point the context at the live
        // widget carrying it. Never fires once a runtime set_focused/clear_focus has run
        // (clear_focus also clears both flags).
        if self.ui_context.focused_widget.is_none() {
            if self.username_box.base().focused {
                self.ui_context.set_focused(&mut self.username_box);
            } else if self.password_box.base().focused {
                self.ui_context.set_focused(&mut self.password_box);
            }
        }
    }

    #[allow(dead_code)]
    fn widgets_iter_mut(&mut self) -> Vec<&mut dyn WidgetHost> {
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
        let event = cce_ui::widget::Event::PointerMove {
            x: cx,
            y: cy,
            local_x: cx,
            local_y: cy,
        };
        // Routed (6bd shrink): the background rides the same router as the other roots.
        let bg_root = self.bg.id();
        if self.ui_context.propagate_event(&event, bg_root) {
            changed = true;
        }
        let sl_root = self.session_list.id();
        let card_root = self.card.id();
        if self.ui_context.propagate_event(&event, sl_root) {
            changed = true;
        }
        if self.ui_context.propagate_event(&event, card_root) {
            changed = true;
        }
        changed
    }

    pub fn widgets_mouse_input(&mut self, button: MouseButton, state: ElementState, cx: f32, cy: f32) -> bool {
        let mut changed = false;
        let event = cce_ui::widget::Event::MouseButton {
            button,
            state,
            x: cx,
            y: cy,
            local_x: cx,
            local_y: cy,
        };
        // Routed (6bd shrink); the bg result stays outside `handled` so the
        // unfocus-on-missed-press rule below keys on the session list + card only.
        let bg_root = self.bg.id();
        if self.ui_context.propagate_event(&event, bg_root) {
            changed = true;
        }
        let sl_root = self.session_list.id();
        let card_root = self.card.id();
        let handled = self.ui_context.propagate_event(&event, sl_root)
            || self.ui_context.propagate_event(&event, card_root);
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
        let ui_event = cce_ui::widget::Event::KeyInput(event.clone());
        // Fully short-circuited (the 6ac rule): every propagate call delivers KeyInput
        // to the ctx-focused widget first, so a non-short-circuited chain would insert
        // a typed key once per root.
        let bg_root = self.bg.id();
        let sl_root = self.session_list.id();
        let card_root = self.card.id();
        if self.ui_context.propagate_event(&ui_event, bg_root) {
            changed = true;
        } else if self.ui_context.propagate_event(&ui_event, sl_root) {
            changed = true;
        } else if self.ui_context.propagate_event(&ui_event, card_root) {
            changed = true;
        }
        changed
    }
    fn trigger_auth(&mut self) {
        let username = self.username_box.text.trim().to_string();
        let password = self.password_box.text.clone();

        if username.is_empty() {
            self.status_lbl.text = "Username cannot be empty".to_string();
            self.status_lbl.is_error = true;
            self.ui_context.set_focused(&mut self.username_box);
            self.username_box.focus();
        } else if password.is_empty() {
            if is_fprint_enabled() {
                self.start_fprint_auth();
            } else {
                self.status_lbl.text = "Password cannot be empty".to_string();
                self.status_lbl.is_error = true;
                self.ui_context.set_focused(&mut self.password_box);
                self.password_box.focus();
            }
        } else {
            // A typed password supersedes any fingerprint attempt still
            // running; release the sensor so it is not left claimed.
            self.cancel_fprint_auth();
            self.auth_request_id += 1;
            self.auth_password = password.clone();
            self.status_lbl.text = "Authenticating...".to_string();
            self.status_lbl.is_error = false;
            self.is_authenticating = true;
            self.login_btn.base_mut().label = Some("Authenticating...".to_string());
            authenticate_user(self.auth_request_id, username, password, self.auth_sender.clone());
        }
    }

    /// Start (or restart) the fingerprint attempt for the username in the box.
    fn start_fprint_auth(&mut self) {
        let username = self.username_box.text.trim().to_string();
        self.cancel_fprint_auth();
        self.auth_request_id += 1;
        self.auth_password.clear();
        self.status_lbl.text = "Scan finger to login or type password".to_string();
        self.status_lbl.is_error = false;
        self.is_authenticating = true;
        self.login_btn.base_mut().label = Some("Authenticating...".to_string());
        match spawn_fprint_helper(self.auth_request_id, &username, self.auth_sender.clone()) {
            Ok(child) => self.fprint_child = Some(child),
            Err(e) => {
                log::error!("Failed to spawn fingerprint helper: {}", e);
                self.is_authenticating = false;
                self.login_btn.base_mut().label = Some("Log In".to_string());
                self.status_lbl.text = "Fingerprint unavailable — type password".to_string();
                self.status_lbl.is_error = true;
            }
        }
    }

    /// Kill a running fingerprint helper, if any. Its exit drops the D-Bus
    /// connection pam_fprintd used to claim the sensor, so fprintd releases the
    /// device for the next attempt. Any late events it already queued are
    /// dropped by the request-id check in the auth event handler.
    fn cancel_fprint_auth(&mut self) {
        if let Some(mut child) = self.fprint_child.take() {
            let _ = child.kill();
            let _ = child.wait();
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
            fprint_child: None,
            auth_password: String::new(),
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

        // Start the background fingerprint attempt if the username is
        // prepopulated and fprintd is enabled — unless this greeter is a rapid
        // respawn of one that just did the same. The daemon relaunches the
        // greeter whenever it exits without AUTH_SUCCESS (crash, F5, Ctrl+C),
        // and every relaunch used to fire a fresh fingerprint attempt on its
        // own: three respawns in 90s were three attempts nobody asked for.
        // After a respawn the user starts it explicitly (Enter on an empty
        // password box).
        let username = app.username_box.text.trim().to_string();
        if !username.is_empty() && is_fprint_enabled() {
            if fprint_autostart_recently() {
                log::info!("Greeter respawned within {}s of the last fingerprint auto-start; not auto-starting", FPRINT_AUTOSTART_COOLDOWN.as_secs());
                app.status_lbl.text = "Press Enter to scan finger, or type password".to_string();
            } else {
                mark_fprint_autostart();
                app.start_fprint_auth();
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
            let is_card = w.base().id() == self.card.id();
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
            let is_card = w.base().id() == self.card.id();
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
        // Widget text via the paint walk, over the TRUE roots (root Container dissolved,
        // Phase 6ax): the card descends into its input children via the walk; the session
        // list and bg are standalone leaves. Walking the flat widgets_iter would emit the
        // card's children twice (once via descent, once as standalone roots).
        cce_ui::scene::painter::append_widget_text(&self.ui_context, &self.bg, &mut pc);
        cce_ui::scene::painter::append_widget_text(&self.ui_context, &self.card, &mut pc);
        cce_ui::scene::painter::append_widget_text(&self.ui_context, &self.session_list, &mut pc);

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
                                app.fprint_child = None;
                                app.is_authenticating = false;
                                app.login_btn.base_mut().label = Some("Log In".to_string());
                                app.status_lbl.text = format!("Welcome, {}!", username);
                                app.status_lbl.is_error = false;
                                app.login_success = true;
                                if let Some(session) = app.session_list.selected_session() {
                                    println!("{}", auth_success_line(app.username_box.text.trim(), &session.exec, session.is_wayland, &app.auth_password));
                                    std::process::exit(0);
                                }
                            }
                            AuthEvent::Failure { err_msg, .. } => {
                                let was_fprint = app.fprint_child.is_some();
                                if let Some(mut child) = app.fprint_child.take() {
                                    let _ = child.wait();
                                }
                                app.is_authenticating = false;
                                app.login_btn.base_mut().label = Some("Log In".to_string());
                                app.status_lbl.text = if was_fprint {
                                    // Raw PAM codes ("AUTHINFO_UNAVAIL") told the
                                    // user nothing, least of all how to retry.
                                    format!("{} — press Enter to scan again, or type password", fprint_failure_text(&err_msg))
                                } else {
                                    err_msg
                                };
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
                        self.cancel_fprint_auth();
                        self.auth_request_id += 1;
                        self.is_authenticating = false;
                        self.login_btn.base_mut().label = Some("Log In".to_string());
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
                let kev = cce_ui::widget::Event::KeyInput(event.clone());
                let root = self.password_box.id();
                let _ = self.ui_context.propagate_event(&kev, root);
                self.trigger_auth();
                changed = true;
            } else if logical_key == &Key::Named(NamedKey::Enter) && self.username_box.focused(&self.ui_context) {
                let kev = cce_ui::widget::Event::KeyInput(event.clone());
                let root = self.username_box.id();
                let _ = self.ui_context.propagate_event(&kev, root);
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

/// PAM service for the fingerprint attempt. It must be fingerprint-ONLY
/// (`auth requisite pam_fprintd.so`, no system-local-login include in the auth
/// stack): the old layout had pam_fprintd `sufficient` above the include, so a
/// miss fell through into pam_unix with an empty password — one pam_faillock
/// strike per miss, and after three the correct password was rejected too.
const FPRINT_PAM_SERVICE: &str = "cce-display-manager-fprint";
const PASSWORD_PAM_SERVICE: &str = "cce-display-manager-password";

/// A greeter that starts within this window of the previous auto-start is a
/// respawn; it does not auto-start the fingerprint attempt again.
const FPRINT_AUTOSTART_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(20);
const FPRINT_AUTOSTART_STAMP: &str = "/run/cce-display-manager/fprint-autostart";

/// Human text for the verdict the fingerprint helper reports (a PamReturnCode
/// Debug name, or a helper-level message).
fn fprint_failure_text(code: &str) -> String {
    match code {
        // pam_fprintd: verify timed out, or the user has no enrolled prints.
        "AUTHINFO_UNAVAIL" => "No fingerprint read (timed out or none enrolled)".to_string(),
        "MAXTRIES" | "AUTH_ERR" => "Fingerprint not recognized".to_string(),
        "SERVICE_ERR" | "SYSTEM_ERR" => "Fingerprint reader unavailable".to_string(),
        other => format!("Fingerprint failed ({})", other),
    }
}

fn fprint_autostart_recently() -> bool {
    std::fs::metadata(FPRINT_AUTOSTART_STAMP)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
        .map(|age| age < FPRINT_AUTOSTART_COOLDOWN)
        .unwrap_or(false)
}

fn mark_fprint_autostart() {
    if let Some(dir) = std::path::Path::new(FPRINT_AUTOSTART_STAMP).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(FPRINT_AUTOSTART_STAMP, b"") {
        log::warn!("Could not write {}: {}", FPRINT_AUTOSTART_STAMP, e);
    }
}

fn is_fprint_enabled() -> bool {
    std::fs::read_to_string(format!("/etc/pam.d/{}", FPRINT_PAM_SERVICE))
        .map(|content| {
            content.lines().any(|line| {
                let trimmed = line.trim();
                trimmed.contains("pam_fprintd.so") && !trimmed.starts_with('#')
            })
        })
        .unwrap_or(false)
}

/// Password authentication, in a thread. Fingerprint goes through
/// `spawn_fprint_helper` instead — never call this with an empty password.
fn authenticate_user(request_id: u64, username: String, password: String, sender: channel::Sender<AuthEvent>) {
    debug_assert!(!password.is_empty(), "empty password must go through the fingerprint helper");
    std::thread::spawn(move || {
        let service = PASSWORD_PAM_SERVICE;

        let mut auth = match PamSession::new(service, &username, &password, request_id, Some(sender.clone())) {
            Ok(a) => a,
            Err(e) => {
                let _ = sender.send(AuthEvent::Failure { request_id, err_msg: format!("{:?}", e) });
                return;
            }
        };

        // authenticate() + acct_mgmt() is the whole credential check. Do NOT
        // open a PAM session here: the daemon's session worker
        // (launch_session) opens the real one. The greeter used to call
        // open_session() too, which registered a throwaway logind session
        // with this process as leader and ran pam_gnome_keyring's
        // auto_start — a fork() out of this multi-threaded Vulkan process
        // that then setuid()s and exec()s gnome-keyring-daemon. That child
        // could wedge before exec (seen 2026-09-18), and gkr-pam waits on
        // its pipes with no timeout, so the login froze on
        // "Authenticating..." after the password had been accepted.
        if let Err(e) = auth.authenticate() {
            let _ = sender.send(AuthEvent::Failure { request_id, err_msg: format!("{:?}", e) });
            return;
        }

        let _ = sender.send(AuthEvent::Success { request_id, username });
    });
}

/// Line protocol between the greeter and its `--fprint-auth` helper (on the
/// helper's stdout — which is a pipe to the greeter, NOT the greeter's own
/// stdout, which carries AUTH_SUCCESS to the daemon).
const FPRINT_LINE_INFO: &str = "INFO|";
const FPRINT_LINE_OK: &str = "OK";
const FPRINT_LINE_FAIL: &str = "FAIL|";

/// Spawn `<self> --fprint-auth <user>` and forward its result lines as
/// AuthEvents tagged with `request_id`. The helper is bound to the greeter with
/// PR_SET_PDEATHSIG so a crashed or respawned greeter cannot leave it running
/// with the sensor claimed (that "Device was already claimed" state made every
/// later attempt fail instantly).
fn spawn_fprint_helper(request_id: u64, username: &str, sender: channel::Sender<AuthEvent>) -> std::io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("/usr/bin/cce-display-manager"));
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--fprint-auth")
        .arg(username)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());
    unsafe {
        cmd.pre_exec(|| {
            // Runs in the child between fork and exec; PDEATHSIG survives exec.
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Parent already gone (raced between fork and prctl)? Then die now.
            if libc::getppid() == 1 {
                libc::_exit(1);
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let username = username.to_string();
    std::thread::spawn(move || {
        use std::io::BufRead;
        let mut concluded = false;
        for line in std::io::BufReader::new(stdout).lines() {
            let line = match line { Ok(l) => l, Err(_) => break };
            if let Some(msg) = line.strip_prefix(FPRINT_LINE_INFO) {
                let _ = sender.send(AuthEvent::Info { request_id, msg: msg.to_string() });
            } else if line == FPRINT_LINE_OK {
                concluded = true;
                let _ = sender.send(AuthEvent::Success { request_id, username: username.clone() });
            } else if let Some(msg) = line.strip_prefix(FPRINT_LINE_FAIL) {
                concluded = true;
                let _ = sender.send(AuthEvent::Failure { request_id, err_msg: msg.to_string() });
            }
        }
        if !concluded {
            // EOF without a verdict: killed (cancelled) or crashed. A cancel
            // has already bumped auth_request_id, so this is dropped there.
            let _ = sender.send(AuthEvent::Failure { request_id, err_msg: "Fingerprint helper exited".to_string() });
        }
    });
    Ok(child)
}

/// `--fprint-auth <user>`: run the fingerprint-only PAM service to a verdict
/// and report it on stdout. Authenticate + account check only — the greeter
/// prints AUTH_SUCCESS with an empty password and the daemon opens the real
/// session on cce-display-manager-autologin, so opening one here would just
/// register a throwaway logind session under cage.
fn run_fprint_helper(username: &str) -> ! {
    use std::io::Write;
    let (sender, receiver) = channel::channel::<AuthEvent>();
    let user = username.to_string();
    let worker = std::thread::spawn(move || {
        let mut auth = PamSession::new(FPRINT_PAM_SERVICE, &user, "", 0, Some(sender.clone()))
            .map_err(|e| format!("{:?}", e))?;
        auth.authenticate().map_err(|e| format!("{:?}", e))
    });
    let mut out = std::io::stdout();
    // Forward conversation messages (e.g. "Place your finger on the sensor")
    // until the worker's sender is dropped, i.e. the verdict is in.
    while let Ok(ev) = receiver.recv() {
        if let AuthEvent::Info { msg, .. } = ev {
            let _ = writeln!(out, "{}{}", FPRINT_LINE_INFO, msg.replace('\n', " "));
            let _ = out.flush();
        }
    }
    let verdict = match worker.join() {
        Ok(Ok(())) => FPRINT_LINE_OK.to_string(),
        Ok(Err(e)) => format!("{}{}", FPRINT_LINE_FAIL, e),
        Err(_) => format!("{}fingerprint worker panicked", FPRINT_LINE_FAIL),
    };
    let _ = writeln!(out, "{}", verdict);
    let _ = out.flush();
    std::process::exit(if verdict == FPRINT_LINE_OK { 0 } else { 1 });
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
    // cage reports a scale-1 output, so on a HiDPI panel the greeter would lay
    // out in physical pixels (everything half-size). cce-ui's forced-scale mode
    // scales layout/rendering by the system scale while keeping buffer_scale 1.
    if layout_scale > 1.0 && std::env::var("CCE_FORCE_SCALE").is_err() {
        std::env::set_var("CCE_FORCE_SCALE", layout_scale.to_string());
    }

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
            // unwrap_or_default, not unwrap: an interior NUL in the typed
            // password would otherwise panic across this extern "C" boundary
            // (process abort). An empty response just fails authentication.
            if style == pam_sys::PamMessageStyle::PROMPT_ECHO_ON as libc::c_int {
                let user_c = std::ffi::CString::new(data.username.clone()).unwrap_or_default();
                r.resp = libc::strdup(user_c.as_ptr());
            } else if style == pam_sys::PamMessageStyle::PROMPT_ECHO_OFF as libc::c_int {
                let pass_c = std::ffi::CString::new(data.password.clone()).unwrap_or_default();
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
            let pass_c = std::ffi::CString::new(password).unwrap_or_default();
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

/// PID of the greeter's `cage` process while a greeter is showing, else 0.
/// Shared with the resume watchdog so it can force a clean greeter respawn
/// after sleep without racing the daemon's blocking read of the greeter's
/// stdout. Set right after the cage is spawned, cleared once it is reaped.
static GREETER_CAGE_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

fn run_daemon() {
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

    // Redirect stdout and stderr of the daemon to a log file. /var/log, not
    // /tmp: only root can create names there, so a local user cannot pre-place
    // a file or symlink at the predictable path for root to open and truncate.
    let log_path = format!("/var/log/cce-display-manager-{}.log", tty_name);
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
    // Set when the compositor requested a restart (`ccectl restart-compositor`
    // wrote the flag file and exited): the next loop iteration relaunches the
    // same session directly — no greeter, autologin PAM service.
    let mut pending_relaunch: Option<(String, String, bool)> = None;

    // Recover the greeter across suspend/resume: resume leaves the greeter's
    // cage DRM-paused and it cannot reliably reacquire the seat on its own.
    if is_real_tty {
        spawn_resume_watchdog(tty_name.clone());
    }

    loop {
        if let Some((username, exec, is_wayland)) = pending_relaunch.take() {
            log::info!(
                "Compositor restart requested: relaunching '{}' for {} without the greeter",
                exec, username
            );
            launch_session(username, exec, is_wayland, String::new(), &tty_name, &mut pending_relaunch);
            continue;
        }

        if is_real_tty {
            // Actively claim tty1 rather than passively waiting for it: after a
            // resume (or any stray VT switch) tty1 may not be foreground, and a
            // greeter cage spawned onto an inactive VT comes up DRM-paused.
            log::info!("Ensuring {} is the active TTY before spawning greeter...", tty_name);
            ensure_vt_active(&tty_name);
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
            // The greeter runs as root: cce-ui's default bundled-fonts dir
            // ($HOME/Dropbox/Fonts) doesn't exist for root, and an empty font
            // db panics on the first shaped glyph. Point it at a system
            // location and load installed system fonts as a fallback.
            .env("CCE_FONTS_DIR", "/usr/share/fonts/cce")
            .env("CCE_LOAD_SYSTEM_FONTS", "1")
            .env("LIBSEAT_BACKEND", "seatd")
            .env("WLR_DRM_NO_MODIFIERS", "1")
            .env("WLR_DRM_DEVICES", "/dev/dri/card1:/dev/dri/card0")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn cage compositor wrapper. Is cage installed?");
        GREETER_CAGE_PID.store(child.id() as i32, std::sync::atomic::Ordering::SeqCst);

        let stdout = child.stdout.take().expect("failed to open child stdout");
        let reader = std::io::BufReader::new(stdout);
        let mut auth_success = None;

        use std::io::BufRead;
        for line in reader.lines() {
            if let Ok(line_str) = line {
                if line_str.starts_with("AUTH_SUCCESS|") {
                    if let Some((username, exec, is_wayland, password)) = parse_auth_success(&line_str) {
                        log::info!("[greeter-stdout] AUTH_SUCCESS|{}|{}|{}", username, exec, is_wayland);
                        auth_success = Some((username, exec, is_wayland, password));
                        // Don't read to EOF: the greeter has already exited,
                        // but cage can linger indefinitely after its child is
                        // gone (observed wedged until a manual VT switch — the
                        // "login hangs until Ctrl+Alt+F2" failure). Stop
                        // reading and terminate it ourselves below.
                        break;
                    }
                    // Never echo the raw line: field 5 is the password.
                    log::warn!("[greeter-stdout] malformed AUTH_SUCCESS line (redacted); login attempt dropped");
                } else {
                    log::info!("[greeter-stdout] {}", line_str);
                }
            }
        }

        if auth_success.is_some() {
            terminate_greeter(&mut child);
        }
        let status = child.wait().expect("failed to wait on child process");
        GREETER_CAGE_PID.store(0, std::sync::atomic::Ordering::SeqCst);
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

            launch_session(username, exec, is_wayland, password, &tty_name, &mut pending_relaunch);
        }
    }
}

/// Ask the greeter's cage to exit, escalating to SIGKILL if it doesn't. Cage
/// exiting cleanly releases the seat/VT via seatd (which cleans the VT up
/// without switching away); a wedged cage would otherwise block the login
/// handoff forever.
fn terminate_greeter(child: &mut std::process::Child) {
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    for _ in 0..30 {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    }
    log::warn!("cage did not exit within 3s of SIGTERM; killing it");
    let _ = child.kill();
}

/// Per-message state machine over `busctl monitor` text output, detecting a
/// logind resume: `PrepareForSleep(false)`. Each D-Bus message opens with a
/// `Type=` header line (which resets us), a signal's header also carries
/// `Member=...` (we arm only for `PrepareForSleep`), and the body carries the
/// `BOOLEAN` payload. `PrepareForSleep(true)` precedes suspend and `(false)`
/// follows resume, so we fire only on the `false`. Fail-safe: a format we do
/// not recognise simply never fires (degrading to the pre-fix behaviour, never
/// a spurious teardown).
struct ResumeSignalParser {
    armed: bool,
}

impl ResumeSignalParser {
    fn new() -> Self {
        Self { armed: false }
    }

    /// Feed one output line; returns true exactly when a resume message completes.
    fn feed(&mut self, line: &str) -> bool {
        if line.contains("Type=") {
            self.armed = false;
        }
        if line.contains("PrepareForSleep") {
            self.armed = true;
        } else if self.armed && line.contains("BOOLEAN") {
            let resume = line.contains("false");
            self.armed = false;
            return resume;
        }
        false
    }
}

/// Watch logind's `PrepareForSleep` signal and, on resume, recover the greeter.
/// Resume-from-suspend leaves the greeter's `cage` DRM-paused ("Atomic commit
/// failed: Permission denied" looping on "Disabling seat"); it cannot reliably
/// reacquire the seat on its own, so on resume we force the greeter's VT active
/// and tear the cage down, letting the daemon loop spawn a fresh one on an
/// active VT -- the same known-good state a service restart produces. This is a
/// no-op while a user session is live (`GREETER_CAGE_PID == 0`): the running
/// compositor owns the seat then, and we must not fight it. Uses `busctl`
/// (always present with systemd) rather than a D-Bus crate to keep this
/// login-critical binary's dependency surface minimal.
fn spawn_resume_watchdog(tty_name: String) {
    std::thread::spawn(move || loop {
        let spawned = std::process::Command::new("busctl")
            .args(["monitor", "--system", "org.freedesktop.login1"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                log::warn!("resume watchdog: could not start busctl ({}); retrying in 5s", e);
                std::thread::sleep(std::time::Duration::from_secs(5));
                continue;
            }
        };
        if let Some(stdout) = child.stdout.take() {
            use std::io::BufRead;
            let reader = std::io::BufReader::new(stdout);
            let mut parser = ResumeSignalParser::new();
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if parser.feed(&line) {
                    on_resume(&tty_name);
                }
            }
        }
        let _ = child.wait();
        log::warn!("resume watchdog: busctl monitor exited; restarting in 2s");
        std::thread::sleep(std::time::Duration::from_secs(2));
    });
}

/// Force the greeter's VT active and, if a greeter `cage` is up, tear it down so
/// the daemon loop respawns a clean one. See `spawn_resume_watchdog`. No-op when
/// a user session owns the seat (`GREETER_CAGE_PID == 0`).
fn on_resume(tty_name: &str) {
    use std::sync::atomic::Ordering;
    let pid = GREETER_CAGE_PID.load(Ordering::SeqCst);
    if pid <= 0 {
        return;
    }
    log::info!("Resume from sleep detected while greeter is up; forcing {} active and respawning greeter", tty_name);
    ensure_vt_active(tty_name);
    // SIGTERM first; a DRM-wedged cage can ignore it, so escalate to SIGKILL.
    // Re-check the PID before escalating so we never signal a cage the daemon
    // has already reaped and replaced with a fresh one.
    unsafe { libc::kill(pid, libc::SIGTERM); }
    for _ in 0..30 {
        if GREETER_CAGE_PID.load(Ordering::SeqCst) != pid {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if GREETER_CAGE_PID.load(Ordering::SeqCst) == pid {
        log::warn!("resume watchdog: greeter cage {} did not exit on SIGTERM; killing", pid);
        unsafe { libc::kill(pid, libc::SIGKILL); }
    }
}

/// The greeter/cage teardown (or a stray VT switch) can leave the session's VT
/// inactive; a logind session on an inactive VT never activates, so the
/// compositor sits DRM-paused on a black screen. Force the VT active before
/// handing the seat to the user session.
fn ensure_vt_active(tty_name: &str) {
    let Some(vt) = tty_name.strip_prefix("tty").and_then(|s| s.parse::<u32>().ok()) else {
        return;
    };
    for _ in 0..20 {
        if let Ok(active) = std::fs::read_to_string("/sys/class/tty/tty0/active") {
            if active.trim() == tty_name {
                return;
            }
        }
        let _ = std::process::Command::new("chvt").arg(vt.to_string()).status();
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    log::warn!("could not make {} the active VT", tty_name);
}

/// End a finished session's logind session: stop whatever it left running.
///
/// The worker closes PAM when the session's command exits, but that only marks
/// the logind session `closing` — with logind's default KillUserProcesses=no,
/// every process the session started and did not reap lives on in its scope.
/// Every login leaked that way (by 2026-09-25, eight sessions stuck `closing`,
/// held open by 16 orphaned 1Password helpers, 1.9 GB). Terminating from the
/// DAEMON, not the worker: pam_systemd moved the worker into the session's
/// scope, so it would be terminating itself.
///
/// Also on a compositor-restart relaunch: the new compositor starts its
/// clients fresh in the new session (nothing survives into it from the old
/// scope — the leaked sessions held nothing but the orphans), so the old one
/// has nothing worth keeping. If clients ever reconnect ACROSS a restart, they
/// will have to be carried into the new session rather than left in this one.
fn terminate_session(session_id: &str) {
    if !valid_session_id(session_id) {
        if !session_id.is_empty() {
            log::warn!("not terminating session with unexpected id {:?}", session_id);
        }
        return;
    }
    match std::process::Command::new("loginctl").args(["terminate-session", session_id]).status() {
        Ok(s) if s.success() => log::info!("Terminated logind session {}", session_id),
        // Already gone (every process exited on its own) is the good case.
        Ok(s) => log::info!("loginctl terminate-session {} exited {}", session_id, s),
        Err(e) => log::warn!("could not run loginctl to end session {}: {}", session_id, e),
    }
}

/// Fork the session worker (PAM open_session + user-session spawn) and wait
/// for it — shared by the greeter login path and the compositor-restart
/// relaunch path. If the session left a restart flag (`ccectl
/// restart-compositor` writes it before a clean exit), arm
/// `pending_relaunch` so the daemon loop relaunches this same session
/// directly, greeter skipped (empty password → the autologin PAM service).
fn launch_session(
    username: String,
    exec: String,
    is_wayland: bool,
    password: String,
    tty_name: &str,
    pending_relaunch: &mut Option<(String, String, bool)>,
) {
    use users::os::unix::UserExt;
    log::info!("Launching user session Exec: '{}' (Wayland: {}) for user: '{}'", exec, is_wayland, username);
    // A stale flag from a previous session must not trigger a phantom relaunch.
    let flag_path = format!("/tmp/cce-restart-requested-{}", username);
    let _ = std::fs::remove_file(&flag_path);

    ensure_vt_active(tty_name);

    // The worker reports its logind session id up this pipe, so that once the
    // session is over the daemon can end it (see `terminate_session`).
    // O_CLOEXEC: the user session the worker execs must not inherit the write
    // end, or the daemon's read would wait on every process the session ever
    // spawns.
    let mut id_pipe = [-1 as libc::c_int; 2];
    let have_pipe = unsafe { libc::pipe2(id_pipe.as_mut_ptr(), libc::O_CLOEXEC) } == 0;
    if !have_pipe {
        log::warn!("no session-id pipe ({}); the session will not be ended on exit", std::io::Error::last_os_error());
    }

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        log::error!("Fork failed: {}", std::io::Error::last_os_error());
        if have_pipe {
            unsafe {
                libc::close(id_pipe[0]);
                libc::close(id_pipe[1]);
            }
        }
        return;
    } else if pid == 0 {
                if have_pipe {
                    unsafe { libc::close(id_pipe[0]) };
                }
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

                // No fallback service: pam_start does not fail for a missing
                // stack (PAM falls back to /etc/pam.d/other), so the old
                // retry on ly's `ly-autologin` / `login` could never run.
                let service = session_pam_service(&password);
                let mut auth = match PamSession::new(service, &username, &password, 0, None) {
                    Ok(a) => a,
                    Err(e) => {
                        log::error!("PAM Init Error in child: {:?}", e);
                        std::process::exit(1);
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

                if have_pipe {
                    if let Some((_, session_id)) = pam_env.iter().find(|(k, _)| k == "XDG_SESSION_ID") {
                        let line = format!("{}\n", session_id);
                        unsafe {
                            libc::write(id_pipe[1], line.as_ptr() as *const libc::c_void, line.len());
                        }
                    }
                    unsafe { libc::close(id_pipe[1]) };
                }

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
        // Parent process: take the session id the worker reports (EOF once
        // it closes its end — after reporting, or by exiting), then block
        // until the worker terminates.
        let mut session_id = String::new();
        if have_pipe {
            use std::io::Read;
            use std::os::unix::io::FromRawFd;
            unsafe { libc::close(id_pipe[1]) };
            let mut reader = unsafe { std::fs::File::from_raw_fd(id_pipe[0]) };
            let _ = reader.read_to_string(&mut session_id);
        }
        let mut status: libc::c_int = 0;
        unsafe {
            libc::waitpid(pid, &mut status, 0);
        }
        log::info!("Session worker child (PID {}) exited with status: {}", pid, status);
        terminate_session(session_id.trim());

        // Compositor-requested restart: honor the flag only when it is a
        // regular file owned by the session user (anyone can create names in
        // /tmp). symlink_metadata, not metadata: a plain stat follows
        // symlinks, so another user's link pointing at any file the session
        // user owns would pass the owner check.
        if let Ok(meta) = std::fs::symlink_metadata(&flag_path) {
            use std::os::unix::fs::MetadataExt;
            let owner_ok = meta.file_type().is_file()
                && users::get_user_by_name(&username)
                    .map_or(false, |u| u.uid() == meta.uid());
            let _ = std::fs::remove_file(&flag_path);
            if owner_ok {
                *pending_relaunch = Some((username, exec, is_wayland));
                return; // relaunching immediately — no VT switch back
            }
            log::warn!("Ignoring restart flag {} with wrong owner", flag_path);
        }

        if let Some(vt) = tty_name.strip_prefix("tty").and_then(|s| s.parse::<u32>().ok()) {
            log::info!("Switching back to VT {}...", vt);
            let _ = std::process::Command::new("chvt")
                .arg(vt.to_string())
                .status();
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
    } else if args.len() > 2 && args[1] == "--fprint-auth" {
        run_fprint_helper(&args[2]);
    } else {
        run_daemon();
    }
}




#[cfg(test)]
mod tests {
    use super::parse_auth_success;

    #[test]
    fn auth_success_plain() {
        let got = parse_auth_success("AUTH_SUCCESS|lucas|startcce|true|hunter2");
        assert_eq!(
            got,
            Some(("lucas".into(), "startcce".into(), true, "hunter2".into()))
        );
    }

    #[test]
    fn auth_success_password_with_pipes() {
        // The password is the last field and may contain the separator.
        let got = parse_auth_success("AUTH_SUCCESS|lucas|startcce|true|a|b|c");
        assert_eq!(
            got,
            Some(("lucas".into(), "startcce".into(), true, "a|b|c".into()))
        );
    }

    #[test]
    fn auth_success_empty_password_fingerprint_path() {
        let got = parse_auth_success("AUTH_SUCCESS|lucas|startcce|true|");
        assert_eq!(
            got,
            Some(("lucas".into(), "startcce".into(), true, String::new()))
        );
    }

    #[test]
    fn auth_success_malformed() {
        assert_eq!(parse_auth_success("AUTH_SUCCESS|lucas|startcce"), None);
        assert_eq!(parse_auth_success("AUTH_SUCCESS|"), None);
        assert_eq!(parse_auth_success("NOT_A_THING|x|y|z|w"), None);
    }

    /// The greeter's line and the daemon's parse agree, and a password is
    /// carried exactly — spaces at either end, and the `|` separator, intact.
    #[test]
    fn auth_success_round_trips_the_password_verbatim() {
        for pw in ["hunter2", " leading", "trailing ", "  both  ", "a|b", ""] {
            let line = super::auth_success_line("lucas", "startcce", true, pw);
            assert_eq!(
                parse_auth_success(&line),
                Some(("lucas".into(), "startcce".into(), true, pw.to_string())),
                "{pw:?}"
            );
        }
    }

    #[test]
    fn an_empty_password_opens_on_the_autologin_stack() {
        assert_eq!(super::session_pam_service(""), "cce-display-manager-autologin");
        assert_eq!(super::session_pam_service("x"), "cce-display-manager-password");
        assert_eq!(super::session_pam_service(" "), "cce-display-manager-password");
    }

    #[test]
    fn only_plain_session_ids_reach_loginctl() {
        for ok in ["15", "c3", "2"] {
            assert!(super::valid_session_id(ok), "{ok}");
        }
        for bad in ["", "15 --all", "-h", "1;rm", "../x", &"9".repeat(40)] {
            assert!(!super::valid_session_id(bad), "{bad}");
        }
    }

    /// One keyring provider: the TPM-sealed gnome-keyring-daemon unit (see
    /// the README). pam_gnome_keyring in a login stack started a SECOND
    /// daemon at every login, which failed to unlock (the keyring's password
    /// is the sealed one, not the login password) and raced the unit.
    #[test]
    fn no_pam_stack_starts_a_keyring() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/pam");
        let mut seen = 0;
        for entry in std::fs::read_dir(dir).expect("pam/") {
            let path = entry.unwrap().path();
            let text = std::fs::read_to_string(&path).unwrap();
            seen += 1;
            for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
                assert!(
                    !line.contains("pam_gnome_keyring") && !line.contains("pam_kwallet"),
                    "{}: {line}",
                    path.display()
                );
            }
        }
        assert!(seen >= 4, "the four stacks were read");
    }

    #[test]
    fn auth_success_bad_bool_defaults_wayland() {
        let got = parse_auth_success("AUTH_SUCCESS|lucas|startcce|banana|pw");
        assert_eq!(got.map(|t| t.2), Some(true));
    }
}


#[cfg(test)]
mod resume_parser_tests {
    use super::ResumeSignalParser;

    // A representative PrepareForSleep signal as `busctl monitor` prints it.
    fn feed_all(lines: &[&str]) -> usize {
        let mut p = ResumeSignalParser::new();
        lines.iter().filter(|l| p.feed(l)).count()
    }

    #[test]
    fn detects_resume_false() {
        let msg = [
            "\u{2023} Type=signal  Endian=l  Flags=1  Version=1  Cookie=42",
            "  Sender=:1.3  Path=/org/freedesktop/login1  Interface=org.freedesktop.login1.Manager  Member=PrepareForSleep",
            "  MESSAGE \"b\" {",
            "          BOOLEAN false;",
            "  };",
        ];
        assert_eq!(feed_all(&msg), 1, "resume (false) must fire once");
    }

    #[test]
    fn ignores_suspend_true() {
        let msg = [
            "\u{2023} Type=signal  Endian=l  Flags=1  Version=1  Cookie=41",
            "  Sender=:1.3  Path=/org/freedesktop/login1  Interface=org.freedesktop.login1.Manager  Member=PrepareForSleep",
            "  MESSAGE \"b\" {",
            "          BOOLEAN true;",
            "  };",
        ];
        assert_eq!(feed_all(&msg), 0, "suspend (true) must not fire");
    }

    #[test]
    fn ignores_other_signal_with_boolean() {
        // A different signal carrying a BOOLEAN false must not be mistaken for
        // a resume: the Type= header resets us and there is no PrepareForSleep.
        let msg = [
            "\u{2023} Type=signal  Endian=l  Flags=1  Version=1  Cookie=99",
            "  Sender=:1.3  Path=/org/freedesktop/login1  Interface=org.freedesktop.login1.Manager  Member=SessionRemoved",
            "  MESSAGE \"b\" {",
            "          BOOLEAN false;",
            "  };",
        ];
        assert_eq!(feed_all(&msg), 0, "unrelated signal must not fire");
    }

    #[test]
    fn full_cycle_fires_once_on_resume() {
        // Suspend then resume, back to back: exactly one fire, on resume.
        let mut p = ResumeSignalParser::new();
        let stream = [
            "\u{2023} Type=signal  Cookie=1",
            "  Interface=org.freedesktop.login1.Manager  Member=PrepareForSleep",
            "          BOOLEAN true;",
            "\u{2023} Type=signal  Cookie=2",
            "  Interface=org.freedesktop.login1.Manager  Member=PrepareForSleep",
            "          BOOLEAN false;",
        ];
        let fires: usize = stream.iter().filter(|l| p.feed(l)).count();
        assert_eq!(fires, 1);
    }
}
