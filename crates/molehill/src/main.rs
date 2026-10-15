#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod client;
mod gpcallback;
mod logo;
mod platform;
mod tray;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use client::{Client, ClientEvent};
use molehill_core::ipc::{
    Answer, ComponentInfo, Event, FieldKind, FormField, Prompt, Request, TunnelSpec, TunnelState, TunnelStatus,
};
use molehill_core::profile::{self, Profile, ProfileStore, Protocol};
use molehill_core::setup::{self, ManualOvpn, TlsKeyMode};
use molehill_core::{ovpn, paths, secrets};
use slint::{ComponentHandle, Model, ModelRc, SharedPixelBuffer, VecModel};
use tray::{Tray, TrayAction};

slint::include_modules!();

const LOG_LINES: usize = 500;

#[derive(Default)]
struct Tunnel {
    status: Option<TunnelStatus>,
    prompt: Option<(u64, Prompt)>,
    stats: Option<(u64, u64)>,
    log: VecDeque<String>,
    /// Problems found before anything reached the service.
    local_error: Option<String>,
    /// The fields of the current login form (OpenConnect), as the card edits them.
    form: Option<(u64, Rc<VecModel<FormFieldItem>>, Vec<FormField>)>,
}

#[derive(Default)]
struct State {
    store: ProfileStore,
    subtitles: HashMap<String, String>,
    tunnels: HashMap<String, Tunnel>,
    components: Vec<ComponentInfo>,
    components_checked: bool,
    /// component id → (message, busy, is error)
    progress: HashMap<String, (String, bool, bool)>,
    service_online: bool,
    error: Option<String>,
    editing: Option<String>,
    /// Config text waiting to be saved, with the editor source it came from.
    form_import: Option<(i32, String)>,
    fetch_seq: u64,
    save_after_fetch: bool,
    log_for: Option<String>,
    opened_urls: HashSet<(String, u64)>,
    /// GlobalProtect tunnels waiting for the browser (see gpcallback).
    gp_signins: HashSet<String>,
}

struct App {
    ui: AppWindow,
    client: Client,
    state: RefCell<State>,
    connections: Rc<VecModel<ConnectionItem>>,
    component_items: Rc<VecModel<ComponentItem>>,
    tray: RefCell<Option<Tray>>,
    menu: TrayMenu,
    menu_items: Rc<VecModel<MenuTunnel>>,
    menu_at: Cell<(i32, i32)>,
    menu_hidden_at: Cell<Option<Instant>>,
}

thread_local! {
    static APP: OnceCell<Rc<App>> = const { OnceCell::new() };
}

fn with_app(f: impl FnOnce(&Rc<App>)) {
    APP.with(|cell| {
        if let Some(app) = cell.get() {
            f(app)
        }
    });
}

/// Runs `f` on the UI thread with the app.
fn on_ui(f: impl FnOnce(&Rc<App>) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || with_app(f));
}

fn main() -> anyhow::Result<()> {
    // Started by the browser at the end of a GlobalProtect sign-in.
    if let Some(url) = std::env::args().skip(1).find(|a| a.starts_with(gpcallback::SCHEME)) {
        gpcallback::forward(&url);
        return Ok(());
    }
    if !platform::claim_single_instance() {
        return Ok(());
    }
    // A crash during a GlobalProtect sign-in leaves the handler registered.
    gpcallback::unregister();
    let start_hidden = std::env::args().any(|a| a == "--hidden");

    let ui = AppWindow::new()?;
    let px = logo::render(64, logo::ACCENT);
    ui.set_app_icon(slint::Image::from_rgba8(SharedPixelBuffer::clone_from_slice(&px, 64, 64)));

    let client = client::start(|ev| on_ui(move |app| app.on_client_event(ev)));
    let mut state = State::default();
    match ProfileStore::load() {
        Ok(store) => state.store = store,
        Err(e) => state.error = Some(format!("Could not read your profiles: {e:#}")),
    }

    let app = Rc::new(App {
        ui,
        client,
        state: RefCell::new(state),
        connections: Rc::new(VecModel::default()),
        component_items: Rc::new(VecModel::default()),
        tray: RefCell::new(None),
        menu: TrayMenu::new()?,
        menu_items: Rc::new(VecModel::default()),
        menu_at: Cell::new((0, 0)),
        menu_hidden_at: Cell::new(None),
    });
    app.menu.set_tunnels(ModelRc::from(app.menu_items.clone()));
    app.ui.set_connections(ModelRc::from(app.connections.clone()));
    app.ui.set_components(ModelRc::from(app.component_items.clone()));
    APP.with(|cell| {
        let _ = cell.set(app.clone());
    });
    app.wire_callbacks();
    app.refresh_subtitles();

    match Tray::new(|action| on_ui(move |app| app.on_tray(action))) {
        Ok(t) => *app.tray.borrow_mut() = Some(t),
        Err(e) => eprintln!("tray icon unavailable: {e:#}"),
    }

    // Closing the window keeps tunnels up and Molehill in the tray.
    app.ui.window().on_close_requested(|| slint::CloseRequestResponse::HideWindow);

    // Ticks connected-time counters, and re-checks component versions twice a day.
    let ticker = slint::Timer::default();
    ticker.start(slint::TimerMode::Repeated, Duration::from_secs(1), || with_app(|app| app.refresh()));
    let updates = slint::Timer::default();
    updates.start(slint::TimerMode::Repeated, Duration::from_secs(12 * 3600), || {
        with_app(|app| app.client.send(Request::QueryComponents { refresh: true }))
    });

    app.refresh();
    if !start_hidden {
        app.show();
    }
    slint::run_event_loop_until_quit()?;
    gpcallback::unregister();
    Ok(())
}

impl App {
    fn show(&self) {
        let _ = self.ui.show();
        // The native window only exists once the event loop has processed the show.
        platform::when_ready(self.ui.as_weak(), platform::MAIN_TITLE, 25, platform::style_main);
    }

    fn on_tray(&self, action: TrayAction) {
        match action {
            TrayAction::Show => {
                self.hide_menu();
                self.show();
            }
            TrayAction::Menu(x, y) => self.show_menu(x, y),
        }
    }

    fn show_menu(&self, x: i32, y: i32) {
        // A right-click on the icon while the menu is open closes it (the
        // click deactivated it a moment ago); don't reopen it right away.
        if self.menu_hidden_at.get().is_some_and(|t| t.elapsed() < Duration::from_millis(300)) {
            return;
        }
        self.menu_at.set((x, y));
        // Off-screen until it has its size and can be placed by the cursor.
        self.menu.window().set_position(slint::PhysicalPosition::new(-32000, -32000));
        let _ = self.menu.show();
        platform::when_ready(self.menu.as_weak(), platform::MENU_TITLE, 25, |hwnd| {
            platform::style_menu(hwnd);
            with_app(|app| platform::place_menu(hwnd, app.menu_at.get()));
        });
    }

    fn hide_menu(&self) {
        if self.menu.window().is_visible() {
            let _ = self.menu.hide();
            self.menu_hidden_at.set(Some(Instant::now()));
        }
    }

    fn wire_callbacks(self: &Rc<Self>) {
        let ui = &self.ui;
        let weak = Rc::downgrade(self);
        let app = move || weak.upgrade();

        let a = app.clone();
        ui.on_toggle(move |id| a().map(|s| s.toggle(&id)).unwrap_or_default());
        let a = app.clone();
        ui.on_master_toggle(move || a().map(|s| s.master_toggle()).unwrap_or_default());
        let a = app.clone();
        ui.on_new_connection(move || a().map(|s| s.open_editor(None)).unwrap_or_default());
        let a = app.clone();
        ui.on_edit(move |id| a().map(|s| s.open_editor(Some(id.to_string()))).unwrap_or_default());
        let a = app.clone();
        ui.on_show_log(move |id| a().map(|s| s.open_log(&id)).unwrap_or_default());
        let a = app.clone();
        ui.on_submit_credentials(move |id, pid, user, pass, remember| {
            a().map(|s| s.submit_credentials(&id, pid, &user, &pass, remember)).unwrap_or_default()
        });
        let a = app.clone();
        ui.on_submit_text(move |id, pid, value| a().map(|s| s.submit_text(&id, pid, &value)).unwrap_or_default());
        let a = app.clone();
        ui.on_form_field_edited(move |id, index, value| a().map(|s| s.form_field_edited(&id, index, &value)).unwrap_or_default());
        let a = app.clone();
        ui.on_submit_form(move |id, pid| a().map(|s| s.submit_form(&id, pid)).unwrap_or_default());
        let a = app.clone();
        ui.on_cancel_prompt(move |id, pid| a().map(|s| s.answer(&id, pid, Answer::Cancel)).unwrap_or_default());
        ui.on_open_url(|url| {
            let _ = open::that(url.as_str());
        });
        let a = app.clone();
        ui.on_open_components(move || {
            if let Some(s) = a() {
                s.ui.set_page(Page::Components);
                s.client.send(Request::QueryComponents { refresh: false });
            }
        });
        let a = app.clone();
        ui.on_check_updates(move || {
            if let Some(s) = a() {
                s.state.borrow_mut().components_checked = false;
                s.client.send(Request::QueryComponents { refresh: true });
                s.refresh();
            }
        });
        let a = app.clone();
        ui.on_install_component(move |id| {
            if let Some(s) = a() {
                s.state.borrow_mut().progress.insert(id.to_string(), ("Starting…".into(), true, false));
                s.client.send(Request::InstallComponent { component: id.to_string() });
                s.refresh();
            }
        });
        let a = app.clone();
        ui.on_browse_config(move || a().map(|s| s.browse_config()).unwrap_or_default());
        let a = app.clone();
        ui.on_save_profile(move || a().map(|s| s.save_profile()).unwrap_or_default());
        let a = app.clone();
        ui.on_fetch_profile(move || a().map(|s| s.fetch_profile(false)).unwrap_or_default());
        let a = app.clone();
        ui.on_browse_ca(move || {
            if let Some(s) = a() && let Some(text) = s.pick_text("Choose the CA certificate", &["crt", "pem", "cer"]) {
                s.ui.set_form_ca(text.into());
            }
        });
        let a = app.clone();
        ui.on_browse_tls_key(move || {
            if let Some(s) = a() && let Some(text) = s.pick_text("Choose the TLS key", &["key", "pem", "txt"]) {
                s.ui.set_form_tls_key(text.into());
            }
        });
        let a = app.clone();
        ui.on_delete_profile(move || a().map(|s| s.delete_profile()).unwrap_or_default());

        // Tray menu: every action closes it first.
        let menu = &self.menu;
        let a = app.clone();
        menu.on_toggle(move |id| {
            if let Some(s) = a() {
                s.hide_menu();
                s.toggle(&id);
            }
        });
        let a = app.clone();
        menu.on_open_app(move || {
            if let Some(s) = a() {
                s.hide_menu();
                s.show();
            }
        });
        let a = app.clone();
        menu.on_disconnect_all(move || {
            if let Some(s) = a() {
                s.hide_menu();
                s.client.send(Request::DisconnectAll);
            }
        });
        let a = app.clone();
        menu.on_dismiss(move || a().map(|s| s.hide_menu()).unwrap_or_default());
        menu.on_quit(|| {
            let _ = slint::quit_event_loop();
        });
    }

    // ── service events ──────────────────────────────────────────────

    fn on_client_event(&self, ev: ClientEvent) {
        {
            let mut st = self.state.borrow_mut();
            match ev {
                ClientEvent::Online => {
                    st.service_online = true;
                    self.client.send(Request::QueryComponents { refresh: false });
                }
                ClientEvent::Offline => {
                    st.service_online = false;
                    for t in st.tunnels.values_mut() {
                        t.status = None;
                        t.prompt = None;
                        t.stats = None;
                    }
                }
                ClientEvent::Event(ev) => self.on_event(&mut st, ev),
            }
        }
        self.refresh();
    }

    fn on_event(&self, st: &mut State, ev: Event) {
        match ev {
            Event::Welcome { tunnels, .. } => {
                for t in st.tunnels.values_mut() {
                    t.status = None;
                    t.prompt = None;
                }
                for s in tunnels {
                    let id = s.id.clone();
                    st.tunnels.entry(id).or_default().status = Some(s);
                }
            }
            Event::Status(s) => {
                if s.state != TunnelState::Authenticating && st.gp_signins.remove(&s.id) && st.gp_signins.is_empty() {
                    gpcallback::unregister();
                }
                let t = st.tunnels.entry(s.id.clone()).or_default();
                if !s.state.is_active() {
                    t.stats = None;
                    t.prompt = None;
                }
                t.status = Some(s);
            }
            Event::Prompt { id, prompt_id, prompt } => {
                if let Prompt::OpenUrl { url } = &prompt {
                    // The browser hands the result back via globalprotectcallback:.
                    if st.store.get(&id).is_some_and(|p| p.protocol == Protocol::GlobalProtect) {
                        st.gp_signins.insert(id.clone());
                        gpcallback::register();
                    }
                    if st.opened_urls.insert((id.clone(), prompt_id)) {
                        let _ = open::that(url);
                    }
                } else {
                    // Someone has to type something: make sure they can see it.
                    self.show();
                    self.ui.set_page(Page::List);
                }
                let username = st.store.get(&id).and_then(|p| p.username.clone());
                let t = st.tunnels.entry(id).or_default();
                if let Prompt::Form { fields, .. } = &prompt {
                    t.form = Some((prompt_id, form_model(fields, username.as_deref()), fields.clone()));
                }
                t.prompt = Some((prompt_id, prompt));
            }
            Event::PromptDone { id, prompt_id } => {
                if let Some(t) = st.tunnels.get_mut(&id)
                    && t.prompt.as_ref().is_some_and(|(p, _)| *p == prompt_id) {
                        t.prompt = None;
                    }
            }
            Event::AuthFailed { id } => {
                let _ = secrets::delete_password(&id);
            }
            Event::Stats { id, bytes_in, bytes_out } => {
                st.tunnels.entry(id).or_default().stats = Some((bytes_in, bytes_out));
            }
            Event::Log { id, line } => {
                let showing = st.log_for.as_deref() == Some(id.as_str());
                let t = st.tunnels.entry(id).or_default();
                if t.log.len() >= LOG_LINES {
                    t.log.pop_front();
                }
                t.log.push_back(line);
                if showing {
                    self.ui.set_log_text(join_log(&t.log).into());
                }
            }
            Event::Components { items } => {
                st.components = items;
                st.components_checked = true;
            }
            Event::ComponentProgress { component, message, finished, error } => {
                let entry = match error {
                    Some(e) => (e, false, true),
                    None => (message, !finished, false),
                };
                st.progress.insert(component, entry);
            }
            Event::Error { message } => st.error = Some(message),
        }
    }

    // ── actions ─────────────────────────────────────────────────────

    fn toggle(&self, id: &str) {
        let active = self.state.borrow().tunnels.get(id).and_then(|t| t.status.as_ref()).is_some_and(|s| s.state.is_active());
        if active {
            self.client.send(Request::Disconnect { id: id.to_string() });
        } else {
            self.connect(id);
        }
    }

    fn master_toggle(&self) {
        let (any_active, target) = {
            let st = self.state.borrow();
            let any = st.tunnels.values().any(|t| t.status.as_ref().is_some_and(|s| s.state.is_active()));
            let target = st
                .store
                .last_used
                .clone()
                .filter(|id| st.store.get(id).is_some())
                .or_else(|| st.store.profiles.first().map(|p| p.id.clone()));
            (any, target)
        };
        if any_active {
            self.client.send(Request::DisconnectAll);
        } else if let Some(id) = target {
            self.connect(&id);
        }
    }

    fn connect(&self, id: &str) {
        {
            let mut st = self.state.borrow_mut();
            st.error = None;
            let Some(p) = st.store.get(id).cloned() else { return };
            let online = st.service_online;
            let t = st.tunnels.entry(id.to_string()).or_default();
            t.local_error = None;
            let spec = match prepare(&p, online) {
                Ok(spec) => spec,
                Err(e) => {
                    t.local_error = Some(e);
                    drop(st);
                    self.refresh();
                    return;
                }
            };
            t.log.clear();
            t.stats = None;
            let optimistic = TunnelStatus {
                id: p.id.clone(),
                name: p.name.clone(),
                state: TunnelState::Connecting,
                detail: "Starting".into(),
                local_ip: None,
                since_unix: None,
            };
            t.status = Some(optimistic);
            self.client.send(Request::Connect { id: p.id.clone(), name: p.name.clone(), spec });
            st.store.last_used = Some(p.id);
            let _ = st.store.save();
        }
        self.refresh();
    }

    fn answer(&self, id: &str, prompt_id: i32, answer: Answer) {
        self.client.send(Request::Answer { id: id.to_string(), prompt_id: prompt_id as u64, answer });
        if let Some(t) = self.state.borrow_mut().tunnels.get_mut(id) {
            t.prompt = None;
        }
        self.refresh();
    }

    fn submit_text(&self, id: &str, prompt_id: i32, value: &str) {
        // Trusting a server certificate is remembered for the connection.
        {
            let mut st = self.state.borrow_mut();
            let pin = st.tunnels.get(id).and_then(|t| match &t.prompt {
                Some((_, Prompt::TrustCert { pin, .. })) if value == "trust" => Some(pin.clone()),
                _ => None,
            });
            if let Some(pin) = pin {
                if let Some(p) = st.store.profiles.iter_mut().find(|p| p.id == id) {
                    p.server_cert_pin = Some(pin);
                }
                let _ = st.store.save();
            }
        }
        self.answer(id, prompt_id, Answer::Text { value: value.to_string() });
    }

    fn form_field_edited(&self, id: &str, index: i32, value: &str) {
        let st = self.state.borrow();
        let Some((_, model, _)) = st.tunnels.get(id).and_then(|t| t.form.as_ref()) else { return };
        let Some(mut row) = model.row_data(index as usize) else { return };
        if row.kind == "select" {
            row.selected = row.options.iter().position(|o| o == value).unwrap_or(0) as i32;
        }
        row.value = value.into();
        row.focus = false;
        model.set_row_data(index as usize, row);
    }

    fn submit_form(&self, id: &str, prompt_id: i32) {
        let values = {
            let mut st = self.state.borrow_mut();
            let Some((_, model, fields)) = st.tunnels.get(id).and_then(|t| t.form.clone()) else { return };
            let mut values = Vec::new();
            let (mut user, mut pass) = (None, None);
            for (i, f) in fields.iter().enumerate() {
                let row = model.row_data(i).unwrap_or_default();
                let value = match f.kind {
                    FieldKind::Select => f.options.get(row.selected.max(0) as usize).map(|o| o.0.clone()).unwrap_or_default(),
                    _ => row.value.to_string(),
                };
                if f.kind == FieldKind::Text && user.is_none() && looks_like_username(&f.name) {
                    user = Some(value.clone());
                }
                let lname = format!("{} {}", f.name, f.label).to_ascii_lowercase();
                if f.kind == FieldKind::Password && pass.is_none() && lname.contains("pass") && !value.is_empty() {
                    pass = Some(value.clone());
                }
                values.push((f.name.clone(), value));
            }
            // Remember what was typed for next time, like the OpenVPN prompt does.
            if let Some(p) = st.store.profiles.iter_mut().find(|p| p.id == id) {
                if let Some(u) = user.filter(|u| !u.is_empty()) {
                    p.username = Some(u);
                }
                if let Some(pw) = pass.filter(|_| p.remember_password) {
                    let _ = secrets::save_password(id, p.username.as_deref().unwrap_or(""), &pw);
                }
                let _ = st.store.save();
            }
            values
        };
        self.answer(id, prompt_id, Answer::Form { values });
    }

    fn submit_credentials(&self, id: &str, prompt_id: i32, user: &str, pass: &str, remember: bool) {
        {
            let mut st = self.state.borrow_mut();
            if let Some(p) = st.store.profiles.iter_mut().find(|p| p.id == id) {
                p.username = (!user.trim().is_empty()).then(|| user.trim().to_string());
                if remember {
                    match secrets::save_password(id, user, pass) {
                        Ok(()) => p.remember_password = true,
                        Err(e) => st.error = Some(format!("{e:#}")),
                    }
                } else if p.remember_password {
                    p.remember_password = false;
                    let _ = secrets::delete_password(id);
                }
            }
            let _ = st.store.save();
        }
        let answer = Answer::Credentials { username: user.to_string(), password: pass.to_string() };
        self.answer(id, prompt_id, answer);
    }

    // ── editor ──────────────────────────────────────────────────────

    fn open_editor(&self, id: Option<String>) {
        let mut st = self.state.borrow_mut();
        let profile = id.as_deref().and_then(|id| st.store.get(id)).cloned();
        let p = profile.as_ref();
        let ui = &self.ui;
        ui.set_form_is_new(p.is_none());
        ui.set_form_error("".into());
        ui.set_form_name(p.map(|p| p.name.clone()).unwrap_or_default().into());
        let proto = p.map(|p| p.protocol).unwrap_or(Protocol::OpenVpn);
        ui.set_form_protocol(Protocol::ALL.iter().position(|x| *x == proto).unwrap_or(0) as i32);
        ui.set_form_gateway(p.and_then(|p| p.gateway.clone()).unwrap_or_default().into());
        ui.set_form_group(p.and_then(|p| p.group.clone()).unwrap_or_default().into());
        ui.set_form_username(p.and_then(|p| p.username.clone()).unwrap_or_default().into());
        ui.set_form_password("".into());
        ui.set_form_has_saved_password(p.is_some_and(|p| p.remember_password && secrets::load_password(&p.id).is_some()));
        ui.set_form_remember(p.is_none_or(|p| p.remember_password));
        ui.set_form_sso(p.is_some_and(|p| p.sso));
        let source = p.map(source_of).unwrap_or(0);
        ui.set_form_source(source);

        let config_text = p.and_then(|p| p.config.as_ref()).and_then(|c| std::fs::read_to_string(c).ok());
        let (config, info) = match (source, p.and_then(|p| p.config.as_ref())) {
            (0, Some(path)) => (
                format!("imported · {}", path.file_name().unwrap_or_default().to_string_lossy()),
                config_text.as_deref().map(describe_config).unwrap_or_else(|| "file missing, pick it again".into()),
            ),
            _ => (String::new(), String::new()),
        };
        ui.set_form_config(config.into());
        ui.set_form_config_info(info.into());

        ui.set_form_server(p.and_then(|p| p.server_url.clone()).unwrap_or_default().into());
        ui.set_form_insecure(p.is_some_and(|p| p.accept_invalid_cert));
        ui.set_form_fetch_busy(false);
        let fetched = match (source, &config_text) {
            (1, Some(t)) => format!("fetched · {}", describe_config(t)),
            _ => String::new(),
        };
        ui.set_form_fetch_info(fetched.into());

        let m = p.and_then(|p| p.manual.clone()).unwrap_or_default();
        ui.set_form_host(m.host.clone().into());
        ui.set_form_port(if m.port == 0 { "1194".into() } else { m.port.to_string().into() });
        ui.set_form_tcp(m.tcp);
        ui.set_form_ca(m.ca.clone().into());
        ui.set_form_tls_mode(match m.tls_mode {
            TlsKeyMode::None => 0,
            TlsKeyMode::TlsAuth => 1,
            TlsKeyMode::TlsCrypt => 2,
        });
        ui.set_form_tls_key(m.tls_key.clone().into());
        ui.set_form_extra(m.extra.clone().into());

        st.editing = profile.map(|p| p.id);
        st.form_import = None;
        st.fetch_seq += 1;
        st.save_after_fetch = false;
        ui.set_page(Page::Editor);
    }

    fn browse_config(&self) {
        let dialog = rfd::FileDialog::new()
            .set_title("Choose an OpenVPN profile")
            .add_filter("OpenVPN profile", &["ovpn", "conf"])
            .add_filter("All files", &["*"])
            .set_parent(&self.ui.window().window_handle());
        let Some(path) = dialog.pick_file() else { return };
        let ui = &self.ui;
        match ovpn::import(&path) {
            Err(e) => ui.set_form_error(format!("{e:#}").into()),
            Ok(text) => match ovpn::sanitize(&text) {
                Err(problems) => ui.set_form_error(format!("This profile can't be used:\n• {}", problems.join("\n• ")).into()),
                Ok(_) => {
                    ui.set_form_error("".into());
                    ui.set_form_config(path.display().to_string().into());
                    ui.set_form_config_info(describe_config(&text).into());
                    if ui.get_form_name().trim().is_empty() {
                        let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                        ui.set_form_name(stem.into());
                    }
                    self.state.borrow_mut().form_import = Some((0, text));
                }
            },
        }
    }

    /// Lets the user pick a small text file (certificate, key) and returns its contents.
    fn pick_text(&self, title: &str, extensions: &[&str]) -> Option<String> {
        let path = rfd::FileDialog::new()
            .set_title(title)
            .add_filter("Certificate or key", extensions)
            .add_filter("All files", &["*"])
            .set_parent(&self.ui.window().window_handle())
            .pick_file()?;
        match std::fs::read_to_string(&path) {
            Ok(text) => Some(text.trim().to_string()),
            Err(e) => {
                self.ui.set_form_error(format!("Couldn't read {}: {e}", path.display()).into());
                None
            }
        }
    }

    fn fetch_profile(&self, save_after: bool) {
        let ui = &self.ui;
        let address = ui.get_form_server().trim().to_string();
        let user = ui.get_form_username().trim().to_string();
        let mut pass = ui.get_form_password().to_string();
        let mut st = self.state.borrow_mut();
        if pass.is_empty() {
            if let Some(id) = &st.editing {
                pass = secrets::load_password(id).unwrap_or_default();
            }
        }
        if address.is_empty() {
            ui.set_form_error("Enter the server address.".into());
            return;
        }
        if user.is_empty() || pass.is_empty() {
            ui.set_form_error("Enter your username and password first. The server needs them to hand out your profile.".into());
            return;
        }
        st.fetch_seq += 1;
        st.save_after_fetch = save_after;
        let seq = st.fetch_seq;
        ui.set_form_error("".into());
        ui.set_form_fetch_busy(true);
        ui.set_form_fetch_info("Contacting the server…".into());
        let insecure = ui.get_form_insecure();
        std::thread::spawn(move || {
            let result = setup::fetch_access_server_profile(&address, &user, &pass, insecure).map_err(|e| format!("{e:#}"));
            on_ui(move |app| app.on_fetched(seq, result));
        });
    }

    fn on_fetched(&self, seq: u64, result: Result<String, String>) {
        let ui = &self.ui;
        let mut st = self.state.borrow_mut();
        if seq != st.fetch_seq {
            return;
        }
        ui.set_form_fetch_busy(false);
        let checked = result.and_then(|text| match ovpn::sanitize(&text) {
            Ok(_) => Ok(text),
            Err(p) => Err(format!("The server's profile can't be used:\n• {}", p.join("\n• "))),
        });
        match checked {
            Ok(text) => {
                ui.set_form_fetch_info(format!("fetched · {}", describe_config(&text)).into());
                if ui.get_form_name().trim().is_empty() {
                    let host = setup::server_base_url(&ui.get_form_server()).unwrap_or_default();
                    ui.set_form_name(host.trim_start_matches("https://").to_string().into());
                }
                st.form_import = Some((1, text));
                let save = std::mem::take(&mut st.save_after_fetch);
                drop(st);
                if save {
                    self.save_profile();
                }
            }
            Err(e) => {
                st.save_after_fetch = false;
                ui.set_form_fetch_info("".into());
                ui.set_form_error(e.into());
            }
        }
    }

    fn save_profile(&self) {
        let ui = &self.ui;
        let fail = |msg: String| ui.set_form_error(msg.into());
        let name = ui.get_form_name().trim().to_string();
        if name.is_empty() {
            return fail("Give the connection a name.".into());
        }
        let protocol = Protocol::ALL[ui.get_form_protocol().clamp(0, 3) as usize];
        if !protocol.supported() {
            return fail(format!("{} support is coming soon.", protocol.label()));
        }
        // -1: not an OpenVPN profile, so no config file at all.
        let source = if protocol == Protocol::OpenVpn { ui.get_form_source() } else { -1 };
        let gateway = ui.get_form_gateway().trim().to_string();
        let group = ui.get_form_group().trim().to_string();
        let username = ui.get_form_username().trim().to_string();
        let password = ui.get_form_password().to_string();
        let remember = ui.get_form_remember();

        let mut st = self.state.borrow_mut();
        let existing = st.editing.as_deref().and_then(|id| st.store.get(id)).cloned();
        let id = existing.as_ref().map(|p| p.id.clone()).unwrap_or_else(profile::new_id);
        let same_source = existing.as_ref().is_some_and(|p| source_of(p) == source && p.config.is_some());
        let (mut manual, mut server_url) = (None, None);

        // The new config text, or None to keep the current file.
        let new_text = match source {
            -1 => {
                if gateway.is_empty() || gateway.contains(char::is_whitespace) {
                    return fail("Enter the server address, e.g. vpn.example.com".into());
                }
                None
            }
            2 => {
                let port = ui.get_form_port().trim().to_string();
                let port = if port.is_empty() { Ok(1194) } else { port.parse::<u16>() };
                let Ok(port) = port else {
                    return fail("The port must be a number between 1 and 65535.".into());
                };
                let m = ManualOvpn {
                    host: ui.get_form_host().trim().to_string(),
                    port,
                    tcp: ui.get_form_tcp(),
                    ca: ui.get_form_ca().trim().to_string(),
                    tls_mode: match ui.get_form_tls_mode() {
                        1 => TlsKeyMode::TlsAuth,
                        2 => TlsKeyMode::TlsCrypt,
                        _ => TlsKeyMode::None,
                    },
                    tls_key: ui.get_form_tls_key().trim().to_string(),
                    extra: ui.get_form_extra().trim().to_string(),
                };
                if let Err(e) = m.validate() {
                    return fail(e.to_string());
                }
                let text = m.to_config();
                if let Err(p) = ovpn::sanitize(&text) {
                    return fail(format!("Some extra directives aren't allowed:\n• {}", p.join("\n• ")));
                }
                manual = Some(m);
                Some(text)
            }
            1 => {
                let address = ui.get_form_server().trim().to_string();
                server_url = Some(address.clone());
                match st.form_import.take() {
                    Some((1, text)) => Some(text),
                    other => {
                        st.form_import = other;
                        let unchanged = same_source && existing.as_ref().and_then(|p| p.server_url.as_deref()) == Some(address.as_str());
                        if !unchanged {
                            // Fetch first; on_fetched saves when it's done.
                            drop(st);
                            return self.fetch_profile(true);
                        }
                        None
                    }
                }
            }
            _ => match st.form_import.take() {
                Some((0, text)) => Some(text),
                other => {
                    st.form_import = other;
                    if !same_source {
                        return fail("Choose the .ovpn file.".into());
                    }
                    None
                }
            },
        };

        let mut config = existing.as_ref().and_then(|p| p.config.clone()).filter(|_| source >= 0);
        if let Some(text) = new_text {
            let path = paths::configs_dir().join(format!("{id}.ovpn"));
            if let Err(e) = std::fs::create_dir_all(paths::configs_dir()).and_then(|_| std::fs::write(&path, text)) {
                return fail(format!("Couldn't save the profile: {e}"));
            }
            config = Some(path);
        }

        if !remember {
            let _ = secrets::delete_password(&id);
        } else if !password.is_empty() {
            if let Err(e) = secrets::save_password(&id, &username, &password) {
                return fail(format!("{e:#}"));
            }
        }
        let server_cert_pin = existing.as_ref().filter(|p| p.gateway.as_deref() == Some(gateway.as_str())).and_then(|p| p.server_cert_pin.clone());
        st.store.upsert(Profile {
            id,
            name,
            protocol,
            config,
            gateway: (source < 0).then_some(gateway),
            group: (source < 0 && !group.is_empty()).then_some(group),
            server_cert_pin,
            accept_invalid_cert: source == 1 && ui.get_form_insecure(),
            server_url,
            manual,
            username: (!username.is_empty()).then_some(username),
            remember_password: remember,
            sso: matches!(protocol, Protocol::Fortinet | Protocol::GlobalProtect) && ui.get_form_sso(),
        });
        if let Err(e) = st.store.save() {
            return fail(format!("Couldn't save: {e:#}"));
        }
        st.form_import = None;
        drop(st);
        self.refresh_subtitles();
        ui.set_page(Page::List);
        self.refresh();
    }

    fn delete_profile(&self) {
        let mut st = self.state.borrow_mut();
        let Some(id) = st.editing.take() else { return };
        self.client.send(Request::Disconnect { id: id.clone() });
        if let Some(p) = st.store.remove(&id) {
            let _ = secrets::delete_password(&id);
            if let Some(cfg) = p.config {
                let _ = std::fs::remove_file(cfg);
            }
        }
        st.tunnels.remove(&id);
        let _ = st.store.save();
        drop(st);
        self.ui.set_page(Page::List);
        self.refresh();
    }

    fn open_log(&self, id: &str) {
        let mut st = self.state.borrow_mut();
        let name = st.store.get(id).map(|p| p.name.clone()).unwrap_or_default();
        let text = st.tunnels.get(id).map(|t| join_log(&t.log)).unwrap_or_default();
        st.log_for = Some(id.to_string());
        self.ui.set_log_title(format!("log · {name}").into());
        self.ui.set_log_text(if text.is_empty() { "Nothing yet. Connect to see OpenVPN's output here.".into() } else { text.into() });
        self.ui.set_page(Page::Log);
    }

    // ── view ────────────────────────────────────────────────────────

    fn refresh_subtitles(&self) {
        let mut st = self.state.borrow_mut();
        st.subtitles = st.store.profiles.iter().map(|p| (p.id.clone(), p.subtitle())).collect();
    }

    fn refresh(&self) {
        let st = self.state.borrow();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);

        let mut items = Vec::new();
        let (mut connected, mut busy) = (Vec::new(), Vec::new());
        let mut locked = false;
        for p in &st.store.profiles {
            let t = st.tunnels.get(&p.id);
            let status = t.and_then(|t| t.status.as_ref());
            let subtitle = st.subtitles.get(&p.id).cloned().filter(|s| !s.is_empty()).unwrap_or_else(|| p.protocol.label().into());
            let (state, text) = match status {
                Some(s) if s.state == TunnelState::Connected => {
                    connected.push(p.name.clone());
                    ("on", format!("Connected · {}", s.local_ip.clone().unwrap_or_else(|| s.detail.clone())))
                }
                Some(s) if s.state.is_busy() => {
                    busy.push(p.name.clone());
                    if s.state == TunnelState::Authenticating {
                        locked = true;
                    }
                    ("busy", if s.detail.is_empty() { format!("{:?}", s.state) } else { s.detail.clone() })
                }
                Some(s) if s.state == TunnelState::Failed => ("error", s.detail.clone()),
                _ => match t.and_then(|t| t.local_error.clone()) {
                    Some(e) => ("error", e),
                    None => ("off", subtitle),
                },
            };
            let info = match (status, t.and_then(|t| t.stats)) {
                (Some(s), stats) if s.state == TunnelState::Connected => {
                    let (i, o) = stats.unwrap_or((0, 0));
                    let up = s.since_unix.map(|since| format!("  ·  {}", duration(now.saturating_sub(since)))).unwrap_or_default();
                    format!("↓ {}  ↑ {}{up}", bytes(i), bytes(o))
                }
                _ => String::new(),
            };
            let mut item = ConnectionItem {
                id: p.id.clone().into(),
                name: p.name.clone().into(),
                tag: p.protocol.tag().to_uppercase().into(),
                state: state.into(),
                status: text.into(),
                info: info.into(),
                remember: p.remember_password,
                ..Default::default()
            };
            if let Some((pid, prompt)) = t.and_then(|t| t.prompt.as_ref()) {
                locked = true;
                item.prompt_id = *pid as i32;
                match prompt {
                    Prompt::Credentials { username, message } => {
                        item.prompt_kind = "credentials".into();
                        item.prompt_username = username.clone().or_else(|| p.username.clone()).unwrap_or_default().into();
                        item.prompt_message = message.clone().unwrap_or_default().into();
                    }
                    Prompt::Text { label, echo } => {
                        item.prompt_kind = "text".into();
                        item.prompt_label = label.clone().into();
                        item.prompt_echo = *echo;
                    }
                    Prompt::OpenUrl { url } => {
                        item.prompt_kind = "url".into();
                        item.prompt_url = url.clone().into();
                    }
                    Prompt::Form { banner, error, .. } => {
                        item.prompt_kind = "form".into();
                        item.prompt_label = banner.clone().unwrap_or_default().into();
                        item.prompt_message = error.clone().unwrap_or_default().into();
                        if let Some((_, model, _)) = t.and_then(|t| t.form.as_ref()).filter(|f| f.0 == *pid) {
                            item.prompt_fields = ModelRc::from(model.clone());
                        }
                    }
                    Prompt::TrustCert { reason, pin, .. } => {
                        item.prompt_kind = "cert".into();
                        item.prompt_label = reason.clone().into();
                        item.prompt_pin = pin.clone().into();
                    }
                }
            }
            items.push(item);
        }
        sync_model(&self.connections, items);

        let summary = if st.store.profiles.is_empty() {
            "No connections yet".to_string()
        } else {
            let up = match connected.len() {
                0 => None,
                1 => Some(format!("Connected to {}", connected[0])),
                n => Some(format!("{n} tunnels up")),
            };
            let going = match busy.len() {
                0 => None,
                1 => Some(format!("Connecting {}…", busy[0])),
                n => Some(format!("Connecting {n} VPNs…")),
            };
            match (up, going) {
                (Some(u), Some(g)) => format!("{g} · {u}"),
                (Some(u), None) => u,
                (None, Some(g)) => g,
                (None, None) => "All tunnels down".into(),
            }
        };
        let ui = &self.ui;
        ui.set_summary(summary.clone().into());
        ui.set_any_on(!connected.is_empty());
        ui.set_any_busy(!busy.is_empty());
        ui.set_locked(locked);
        ui.set_service_online(st.service_online);

        // Components
        // Only nag about engines the profiles actually use (OpenVPN when there are none yet).
        let needs = |engine: &str| match engine {
            "openconnect" => st.store.profiles.iter().any(|p| p.protocol.openconnect_name().is_some()),
            _ => st.store.profiles.is_empty() || st.store.profiles.iter().any(|p| p.protocol == Protocol::OpenVpn),
        };
        let missing = st.components.iter().find(|c| c.installed.is_none() && needs(&c.id));
        let (notice, attention) = if !st.service_online {
            ("The Molehill service isn't running. Reinstall Molehill, or run `molehill-service install` from an elevated terminal.".to_string(), false)
        } else if let Some(e) = &st.error {
            (e.clone(), false)
        } else if let Some(c) = missing {
            (format!("{} isn't installed yet. Molehill can install it for you.", c.name), true)
        } else if let Some(c) = st.components.iter().find(|c| c.update_available()) {
            (format!("{} {} is available.", c.name, c.latest.clone().unwrap_or_default()), true)
        } else {
            (String::new(), false)
        };
        ui.set_notice(notice.into());
        ui.set_components_attention(attention);

        let comps: Vec<ComponentItem> = st
            .components
            .iter()
            .map(|c| {
                let (message, busy, error) = st.progress.get(&c.id).cloned().unwrap_or_default();
                let version = match (&c.installed, &c.latest) {
                    (Some(i), Some(l)) if c.update_available() => format!("installed {i}  →  {l} available"),
                    (Some(i), Some(_)) => format!("installed {i} · up to date"),
                    (Some(i), None) => format!("installed {i} · couldn't check for updates"),
                    (None, Some(l)) => format!("not installed · latest {l}"),
                    (None, None) => "not installed".into(),
                };
                let action = if c.installed.is_none() { "install" } else if c.update_available() { "update" } else { "" };
                ComponentItem {
                    id: c.id.clone().into(),
                    name: c.name.clone().into(),
                    description: c.description.clone().into(),
                    version: version.into(),
                    action: action.into(),
                    busy,
                    message: message.into(),
                    error,
                }
            })
            .collect();
        sync_model(&self.component_items, comps);
        ui.set_components_status(
            if !st.service_online {
                "Start the Molehill service to manage components."
            } else if !st.components_checked {
                "Checking versions…"
            } else {
                ""
            }
            .into(),
        );

        if let Some(tray) = self.tray.borrow().as_ref() {
            let color = if !connected.is_empty() {
                logo::GREEN
            } else if !busy.is_empty() {
                logo::YELLOW
            } else if st.service_online {
                logo::ACCENT
            } else {
                logo::MUTED
            };
            tray.update(color, &format!("Molehill · {summary}"));
        }
        self.menu.set_summary(summary.into());
        self.menu.set_any_on(!connected.is_empty());
        self.menu.set_any_busy(!busy.is_empty());
        let menu_items: Vec<MenuTunnel> = self
            .connections
            .iter()
            .map(|c| MenuTunnel { id: c.id, name: c.name, state: c.state })
            .collect();
        sync_model(&self.menu_items, menu_items);
    }
}

/// Which editor source a profile was set up from: 0 file, 1 Access Server, 2 manual.
fn source_of(p: &Profile) -> i32 {
    if p.manual.is_some() {
        2
    } else if p.server_url.is_some() {
        1
    } else {
        0
    }
}

/// Builds what the service needs to start a tunnel, or explains why not.
fn prepare(p: &Profile, online: bool) -> Result<TunnelSpec, String> {
    if !online {
        return Err("The Molehill service isn't running.".into());
    }
    if !p.protocol.supported() {
        return Err(format!("{} support is coming soon.", p.protocol.label()));
    }
    let password = if p.remember_password { secrets::load_password(&p.id) } else { None };
    if let Some(protocol) = p.protocol.openconnect_name() {
        let server = p.gateway.clone().filter(|g| !g.is_empty()).ok_or("No server address. Edit the connection and add it.")?;
        return Ok(TunnelSpec::OpenConnect {
            protocol: protocol.into(),
            server,
            username: p.username.clone(),
            password,
            group: p.group.clone(),
            server_cert_pin: p.server_cert_pin.clone(),
            sso: p.sso,
        });
    }
    let config = p
        .config
        .as_ref()
        .and_then(|c| std::fs::read_to_string(c).ok())
        .ok_or("The profile file is missing. Edit the connection and choose the .ovpn again.")?;
    ovpn::sanitize(&config).map_err(|problems| problems.join("; "))?;
    Ok(TunnelSpec::OpenVpn { config, username: p.username.clone(), password })
}

fn describe_config(text: &str) -> String {
    let remotes = ovpn::remotes(text);
    match remotes.len() {
        0 => "no `remote` line found".into(),
        1 => format!("server {}", remotes[0]),
        n => format!("servers {} (+{} more)", remotes[0], n - 1),
    }
}

fn sync_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, items: Vec<T>) {
    if model.row_count() != items.len() {
        model.set_vec(items);
        return;
    }
    for (i, item) in items.into_iter().enumerate() {
        if model.row_data(i).as_ref() != Some(&item) {
            model.set_row_data(i, item);
        }
    }
}

fn join_log(log: &VecDeque<String>) -> String {
    log.iter().map(String::as_str).collect::<Vec<_>>().join("\n")
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

fn duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 { format!("{h}h {m:02}m") } else { format!("{m}m {s:02}s") }
}

/// A username field, by its name only (see the service's auto-fill rules).
fn looks_like_username(name: &str) -> bool {
    matches!(name.trim().to_ascii_lowercase().as_str(), "username" | "user" | "user_name" | "uname" | "login" | "email")
}

/// The card's editable copy of a server login form.
fn form_model(fields: &[FormField], username: Option<&str>) -> Rc<VecModel<FormFieldItem>> {
    let mut focused = false;
    let items: Vec<FormFieldItem> = fields
        .iter()
        .map(|f| {
            let labels: Vec<slint::SharedString> = f.options.iter().map(|o| o.1.clone().into()).collect();
            let selected = f.options.iter().position(|o| o.0 == f.value).unwrap_or(0);
            let (kind, value) = match f.kind {
                FieldKind::Select => ("select", labels.get(selected).cloned().unwrap_or_default().to_string()),
                FieldKind::Password => ("password", String::new()),
                FieldKind::Text => {
                    let fill = username.filter(|_| f.value.is_empty() && looks_like_username(&f.name));
                    ("text", fill.map(str::to_string).unwrap_or_else(|| f.value.clone()))
                }
            };
            let focus = !focused && kind != "select" && value.is_empty() && !f.prefilled;
            focused |= focus;
            FormFieldItem {
                name: f.name.clone().into(),
                label: f.label.clone().into(),
                kind: kind.into(),
                value: value.into(),
                options: ModelRc::new(VecModel::from(labels)),
                selected: selected as i32,
                focus,
                prefilled: f.prefilled,
            }
        })
        .collect();
    Rc::new(VecModel::from(items))
}
