#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod client;
#[cfg(target_os = "macos")]
#[path = "compat_macos.rs"]
mod compat;
mod certs;
#[cfg(windows)]
mod gpcallback;
#[cfg(target_os = "macos")]
#[path = "gpcallback_macos.rs"]
mod gpcallback;
mod localauth;
#[cfg(windows)]
mod selfupdate;
#[cfg(target_os = "macos")]
#[path = "selfupdate_macos.rs"]
mod selfupdate;
// The taskbar badge and window icon use it on Windows; the tray on both.
#[cfg_attr(not(windows), allow(dead_code))]
mod logo;
#[cfg(windows)]
mod platform;
#[cfg(target_os = "macos")]
#[path = "platform_macos.rs"]
mod platform;
mod settings;
mod tray;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use client::{Client, ClientEvent};
use molehill_core::ipc::{
    Answer, ComponentInfo, Event, FieldKind, FormField, Prompt, Request, TunnelSpec, TunnelState, TunnelStatus,
};
use molehill_core::APP_VERSION;
use molehill_core::profile::{self, Profile, ProfileStore, Protocol};
use molehill_core::setup::{self, ManualOvpn, TlsKeyMode};
use molehill_core::{ovpn, paths, secrets};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use tray::{Tray, TrayAction, TrayState};

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
    /// A newer Molehill on GitHub, and what its update is doing.
    app_update: Option<molehill_core::update::Release>,
    app_update_status: String,
    app_update_busy: bool,
    /// Version already announced with a notification this session.
    app_update_notified: Option<String>,
    settings: settings::Settings,
    /// A prompt arrived: bring the list up once the event is handled.
    show_list: bool,
    /// Sign-ins running here, as the user (client certificates), by profile id.
    local_auth: HashMap<String, localauth::LocalAuth>,
    /// What to hand the service once such a sign-in is done.
    pending_specs: HashMap<String, TunnelSpec>,
    /// The editor's certificate choices (after "None"), as cert URIs.
    cert_choices: Vec<String>,
    /// A certificate passphrase typed during a sign-in, saved once it works.
    typed_cert_passwords: HashMap<String, String>,
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
    selfupdate::set_app_id();
    // COM on the UI thread, for the taskbar badge (ITaskbarList3).
    #[cfg(windows)]
    let _ = unsafe { windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED) };
    // A crash during a GlobalProtect sign-in leaves the handler registered.
    gpcallback::unregister();
    let start_hidden = std::env::args().any(|a| a == "--hidden");

    let ui = AppWindow::new()?;

    let client = client::start(|ev| on_ui(move |app| app.on_client_event(ev)));
    let mut state = State { settings: settings::load(), ..Default::default() };
    match ProfileStore::load() {
        Ok(store) => state.store = store,
        Err(e) => {
            // Don't let the next save overwrite a file that may be repairable.
            let path = paths::profiles_file();
            let backup = path.with_extension("toml.bad");
            let _ = std::fs::rename(&path, &backup);
            state.error = Some(format!("Could not read your profiles ({e:#}). The file was moved to {}.", backup.display()));
        }
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
    {
        let st = app.state.borrow();
        platform::set_saved_placement(st.settings.window_pos, st.settings.window_height);
    }
    app.apply_theme();
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
        with_app(|app| {
            app.client.send(Request::QueryComponents { refresh: true });
            app.check_app_update();
        })
    });
    // Molehill's own update: shortly after start, then with the timer above.
    slint::Timer::single_shot(Duration::from_secs(5), || with_app(|app| app.check_app_update()));

    app.refresh();
    if !start_hidden {
        app.show();
    }
    slint::run_event_loop_until_quit()?;
    gpcallback::unregister();
    Ok(())
}

impl App {
    // ── theme, window placement, order ──────────────────────────────

    /// Applies the theme to both windows and the title bar. Also runs from
    /// the window procedure (Windows changed its theme), so it must not
    /// insist on the state: if that's busy, the next refresh catches up.
    pub fn apply_theme(&self) {
        let Ok(st) = self.state.try_borrow() else { return };
        let mode = st.settings.theme;
        drop(st);
        let dark = match mode {
            settings::ThemeMode::System => settings::system_prefers_dark(),
            settings::ThemeMode::Light => false,
            settings::ThemeMode::Dark => true,
        };
        self.ui.global::<Theme>().set_dark(dark);
        self.menu.global::<Theme>().set_dark(dark);
        self.ui.set_theme_label(mode.label().into());
        platform::set_dark(dark);
        if let Some(h) = platform::main_hwnd(self.ui.window()) {
            platform::title_bar(h, dark);
        }
    }

    fn cycle_theme(&self) {
        let mut st = self.state.borrow_mut();
        st.settings.theme = st.settings.theme.next();
        settings::save(&st.settings);
        drop(st);
        self.apply_theme();
    }

    /// From the window procedure after a move or resize; skipped (not
    /// crashed) if the state happens to be busy.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn remember_window(&self, pos: (i32, i32), height: f64) {
        let Ok(mut st) = self.state.try_borrow_mut() else { return };
        st.settings.window_pos = Some(pos);
        st.settings.window_height = Some(height);
        settings::save(&st.settings);
    }

    /// Moves a connection by `steps` places (negative is up) and saves the order.
    fn reorder(&self, id: &str, steps: i32) {
        let mut st = self.state.borrow_mut();
        let list = &mut st.store.profiles;
        let Some(from) = list.iter().position(|p| p.id == id) else { return };
        let to = (from as i64 + steps as i64).clamp(0, list.len() as i64 - 1) as usize;
        if to == from {
            return;
        }
        let p = list.remove(from);
        list.insert(to, p);
        if let Err(e) = st.store.save() {
            st.error = Some(format!("Couldn't save the new order: {e:#}"));
        }
        drop(st);
        self.refresh();
    }

    // ── Molehill's own updates ──────────────────────────────────────

    fn check_app_update(&self) {
        selfupdate::check(|result| on_ui(move |app| app.on_app_update_checked(result)));
    }

    fn on_app_update_checked(&self, result: Result<Option<molehill_core::update::Release>, String>) {
        let mut st = self.state.borrow_mut();
        // Offline or rate-limited: try again with the next check, quietly.
        let Ok(found) = result else { return };
        if let Some(r) = &found
            && st.app_update_notified.as_deref() != Some(r.version.as_str())
        {
            st.app_update_notified = Some(r.version.clone());
            selfupdate::notify(r);
        }
        if !st.app_update_busy {
            st.app_update = found;
        }
        drop(st);
        self.refresh();
    }

    fn update_app(&self) {
        let mut st = self.state.borrow_mut();
        let Some(release) = st.app_update.clone() else { return };
        if st.app_update_busy {
            return;
        }
        st.app_update_busy = true;
        st.app_update_status = "Downloading…".into();
        drop(st);
        self.refresh();
        selfupdate::install(release, |p| {
            on_ui(move |app| {
                let mut st = app.state.borrow_mut();
                match p {
                    selfupdate::Progress::Downloading(n) => st.app_update_status = format!("Downloading… {n}%"),
                    selfupdate::Progress::Failed(e) => {
                        st.app_update_busy = false;
                        st.app_update_status = e;
                    }
                    selfupdate::Progress::Launched => {
                        // The installer replaces our files and starts us again.
                        st.app_update_status = "Installing…".into();
                        drop(st);
                        let _ = slint::quit_event_loop();
                        return;
                    }
                }
                drop(st);
                app.refresh();
            })
        });
    }

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
        ui.on_update_app(move || a().map(|s| s.update_app()).unwrap_or_default());
        let a = app.clone();
        ui.on_reorder(move |id, steps| a().map(|s| s.reorder(&id, steps)).unwrap_or_default());
        let a = app.clone();
        ui.on_cycle_theme(move || a().map(|s| s.cycle_theme()).unwrap_or_default());
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
        let a = app.clone();
        ui.on_form_cert_picked(move |index| a().map(|s| s.cert_picked(index)).unwrap_or_default());
        let a = app.clone();
        ui.on_cancel_editor(move || a().map(|s| s.cancel_editor()).unwrap_or_default());

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
                s.disconnect_all();
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
        if std::mem::take(&mut self.state.borrow_mut().show_list) {
            self.show();
            self.ui.set_page(Page::List);
        }
        self.refresh();
    }

    fn on_event(&self, st: &mut State, ev: Event) {
        match ev {
            Event::Welcome { version, tunnels } => {
                if version != APP_VERSION {
                    st.error = Some(format!("The Molehill service is version {version}, the app is {APP_VERSION}. Restart the service or reinstall."));
                }
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
                    // Someone has to type something: make sure they can see it,
                    // once the state is released (showing reads it).
                    st.show_list = true;
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
            if self.cancel_local_auth(id) {
                return;
            }
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
            self.disconnect_all();
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
            // A client certificate: sign in here, as the user (see localauth).
            if let (Some(cert), TunnelSpec::OpenConnect { protocol, server, username, password, group, server_cert_pin, .. }) =
                (p.client_cert.clone().filter(|_| !p.sso), &spec)
            {
                let Some(dir) = paths::openconnect_dir() else {
                    t.status = None;
                    t.local_error = Some("OpenConnect is not installed. Install it from Components.".into());
                    drop(st);
                    self.refresh();
                    return;
                };
                let req = molehill_core::ocauth::AuthRequest {
                    dir,
                    protocol: protocol.clone(),
                    server: server.clone(),
                    username: username.clone(),
                    password: password.clone(),
                    group: group.clone(),
                    pin: server_cert_pin.clone(),
                    sso: false,
                    saml_login: false,
                    key: Some(molehill_core::ocauth::key_for_cert(&cert)),
                    cert: Some(cert),
                    cert_password: molehill_core::cert_secrets::load(&p.id),
                };
                let id = p.id.clone();
                match localauth::LocalAuth::start(&req, move |m| {
                    let id = id.clone();
                    on_ui(move |app| app.on_local_auth(&id, m));
                }) {
                    Ok(la) => {
                        st.local_auth.insert(p.id.clone(), la);
                        st.pending_specs.insert(p.id.clone(), spec);
                    }
                    Err(e) => {
                        t.status = None;
                        t.local_error = Some(e);
                    }
                }
            } else {
                self.client.send(Request::Connect { id: p.id.clone(), name: p.name.clone(), spec });
            }
            st.store.last_used = Some(p.id);
            let _ = st.store.save();
        }
        self.refresh();
    }

    /// A message from a sign-in running here (client certificate), fed in
    /// as the service's own events would be.
    fn on_local_auth(&self, id: &str, m: localauth::Message) {
        use molehill_core::ocauth::HelperEvent;
        let status = |state: TunnelState, detail: &str| {
            let name = self.state.borrow().store.get(id).map(|p| p.name.clone()).unwrap_or_default();
            Event::Status(TunnelStatus { id: id.into(), name, state, detail: detail.into(), local_ip: None, since_unix: None })
        };
        let prompt = |prompt: Prompt| {
            let mut st = self.state.borrow_mut();
            let la = st.local_auth.get_mut(id)?;
            la.prompt_seq += 1;
            Some(Event::Prompt { id: id.into(), prompt_id: la.prompt_seq, prompt })
        };
        let events: Vec<Event> = match m {
            localauth::Message::Event(HelperEvent::Log { line }) => vec![Event::Log { id: id.into(), line }],
            localauth::Message::Event(HelperEvent::Form { banner, error, fields }) => {
                prompt(Prompt::Form { banner, error, fields }).into_iter().chain([status(TunnelState::Authenticating, "Waiting for you")]).collect()
            }
            localauth::Message::Event(HelperEvent::TrustCert { pin, reason }) => {
                let host = self.state.borrow().local_auth.get(id).map(|l| l.server.clone()).unwrap_or_default();
                prompt(Prompt::TrustCert { host, pin, reason }).into_iter().chain([status(TunnelState::Authenticating, "Waiting for you")]).collect()
            }
            localauth::Message::Event(HelperEvent::OpenUrl { url }) => {
                prompt(Prompt::OpenUrl { url }).into_iter().chain([status(TunnelState::Authenticating, "Finish signing in in your browser")]).collect()
            }
            localauth::Message::Event(HelperEvent::PasswordRejected) => vec![Event::AuthFailed { id: id.into() }],
            localauth::Message::Event(HelperEvent::Done(result)) => {
                let mut st = self.state.borrow_mut();
                st.local_auth.remove(id);
                let typed = st.typed_cert_passwords.remove(id);
                if let Some(pass) = typed.filter(|_| st.store.get(id).is_some_and(|p| p.remember_password)) {
                    let _ = molehill_core::cert_secrets::save(id, &pass);
                }
                let spec = st.pending_specs.remove(id);
                let name = st.store.get(id).map(|p| p.name.clone()).unwrap_or_default();
                drop(st);
                if let Some(TunnelSpec::OpenConnect { protocol, server, username, password, group, server_cert_pin, sso, .. }) = spec {
                    let spec = TunnelSpec::OpenConnect { protocol, server, username, password, group, server_cert_pin, sso, auth: Some(Box::new(result)) };
                    self.client.send(Request::Connect { id: id.into(), name, spec });
                }
                vec![status(TunnelState::Connecting, "Starting the tunnel")]
            }
            localauth::Message::Event(HelperEvent::Failed { message }) => {
                let mut st = self.state.borrow_mut();
                st.local_auth.remove(id);
                st.typed_cert_passwords.remove(id);
                st.pending_specs.remove(id);
                drop(st);
                if message == "Cancelled" {
                    vec![status(TunnelState::Disconnected, "")]
                } else {
                    vec![status(TunnelState::Failed, &message)]
                }
            }
            // Ended without a verdict: it crashed or was killed.
            localauth::Message::Ended => {
                let mut st = self.state.borrow_mut();
                let was_running = st.local_auth.remove(id).is_some();
                st.pending_specs.remove(id);
                drop(st);
                if was_running { vec![status(TunnelState::Failed, "The sign-in helper stopped unexpectedly")] } else { vec![] }
            }
        };
        for ev in events {
            self.on_client_event(ClientEvent::Event(ev));
        }
    }

    /// Every tunnel, including sign-ins running here.
    fn disconnect_all(&self) {
        let ids: Vec<String> = self.state.borrow().local_auth.keys().cloned().collect();
        for id in ids {
            self.cancel_local_auth(&id);
        }
        self.client.send(Request::DisconnectAll);
    }

    /// Stops a sign-in running here; true if there was one.
    fn cancel_local_auth(&self, id: &str) -> bool {
        let mut st = self.state.borrow_mut();
        let Some(la) = st.local_auth.remove(id) else { return false };
        st.pending_specs.remove(id);
        drop(st);
        la.cancel();
        let name = self.state.borrow().store.get(id).map(|p| p.name.clone()).unwrap_or_default();
        let ev = Event::Status(TunnelStatus { id: id.into(), name, state: TunnelState::Disconnected, detail: String::new(), local_ip: None, since_unix: None });
        self.on_client_event(ClientEvent::Event(ev));
        true
    }

    fn answer(&self, id: &str, prompt_id: i32, answer: Answer) {
        // A sign-in running here takes the answer itself.
        let local = {
            let mut st = self.state.borrow_mut();
            // A certificate passphrase (GnuTLS names the field after its
            // form): keep it until the sign-in works, then save it.
            if st.local_auth.contains_key(id)
                && let Answer::Form { values } = &answer
                && let Some((_, pass)) = values.iter().find(|(name, _)| is_cert_passphrase_field(name))
            {
                st.typed_cert_passwords.insert(id.to_string(), pass.clone());
            }
            match st.local_auth.get_mut(id) {
                Some(la) => {
                    use molehill_core::ocauth::HelperAnswer;
                    let reply = match answer.clone() {
                        Answer::Form { values } => Some(HelperAnswer::Form { values }),
                        Answer::Text { value } => Some(HelperAnswer::Trust { accept: value == "trust" }),
                        Answer::Cancel => None,
                        Answer::Credentials { .. } => Some(HelperAnswer::Cancel),
                    };
                    if let Some(r) = &reply {
                        la.answer(r);
                    }
                    Some(reply.is_some())
                }
                None => None,
            }
        };
        match local {
            Some(false) => {
                self.cancel_local_auth(id);
                return;
            }
            Some(true) => {
                if let Some(t) = self.state.borrow_mut().tunnels.get_mut(id) {
                    t.prompt = None;
                    if let Some(s) = t.status.as_mut() {
                        s.detail = "Signing in".into();
                    }
                }
                self.refresh();
                return;
            }
            None => {}
        }
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
            let (mut user, mut pass, mut group) = (None, None, None);
            for (i, f) in fields.iter().enumerate() {
                let row = model.row_data(i).unwrap_or_default();
                let value = match f.kind {
                    FieldKind::Select => f.options.get(row.selected.max(0) as usize).map(|o| o.0.clone()).unwrap_or_default(),
                    _ => row.value.to_string(),
                };
                // Cisco's group list, GlobalProtect's gateway: remembered as the
                // profile's group so the next sign-in doesn't ask (editable there).
                if f.kind == FieldKind::Select && matches!(f.name.as_str(), "group_list" | "authgroup" | "gateway") {
                    group = Some(value.clone()).filter(|v| !v.is_empty());
                }
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
                if group.is_some() {
                    p.group = group;
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
                if remember && !pass.is_empty() {
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
        let proto = p.map(|p| p.protocol).unwrap_or(Protocol::AnyConnect);
        ui.set_form_protocol(Protocol::ALL.iter().position(|x| *x == proto).unwrap_or(0) as i32);
        ui.set_form_gateway(p.and_then(|p| p.gateway.clone()).unwrap_or_default().into());
        ui.set_form_group(p.and_then(|p| p.group.clone()).unwrap_or_default().into());
        ui.set_form_username(p.and_then(|p| p.username.clone()).unwrap_or_default().into());
        ui.set_form_password("".into());
        ui.set_form_has_saved_password(p.is_some_and(|p| p.remember_password && secrets::load_password(&p.id).is_some()));
        ui.set_form_remember(p.is_none_or(|p| p.remember_password));
        ui.set_form_sso(p.is_some_and(|p| p.sso));
        // Certificates in the user's store; a saved one that's gone is kept
        // as a choice so saving doesn't silently drop it.
        let mut certs = certs::list();
        let saved = p.and_then(|p| p.client_cert.clone());
        if let Some(uri) = &saved
            && !certs.iter().any(|c| &c.uri == uri)
        {
            certs.push(certs::Cert { uri: uri.clone(), label: certs::saved_label(uri) });
        }
        let index = saved.as_ref().and_then(|u| certs.iter().position(|c| &c.uri == u)).map(|i| i as i32 + 1).unwrap_or(0);
        #[cfg(windows)]
        let options: Vec<slint::SharedString> = std::iter::once("None".into()).chain(certs.iter().map(|c| c.label.clone().into())).collect();
        #[cfg(target_os = "macos")]
        let options: Vec<slint::SharedString> = std::iter::once("None".into())
            .chain(certs.iter().map(|c| c.label.clone().into()))
            .chain(std::iter::once(certs::CHOOSE_FILE.into()))
            .collect();
        ui.set_form_cert_options(ModelRc::new(VecModel::from(options)));
        ui.set_form_cert_index(index);
        st.cert_choices = certs.into_iter().map(|c| c.uri).collect();
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
        if pass.is_empty()
            && let Some(id) = &st.editing {
                pass = secrets::load_password(id).unwrap_or_default();
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

    /// Back or Cancel in the editor: a fetch still running must not save.
    fn cancel_editor(&self) {
        let mut st = self.state.borrow_mut();
        st.fetch_seq += 1;
        st.save_after_fetch = false;
        st.form_import = None;
        st.editing = None;
        drop(st);
        self.ui.set_form_fetch_busy(false);
        self.ui.set_page(Page::List);
    }

    fn on_fetched(&self, seq: u64, result: Result<String, String>) {
        let ui = &self.ui;
        let mut st = self.state.borrow_mut();
        if seq != st.fetch_seq || ui.get_page() != Page::Editor {
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
        } else if !password.is_empty()
            && let Err(e) = secrets::save_password(&id, &username, &password) {
                return fail(format!("{e:#}"));
            }
        let server_cert_pin = existing.as_ref().filter(|p| p.gateway.as_deref() == Some(gateway.as_str())).and_then(|p| p.server_cert_pin.clone());
        let client_cert = (protocol != Protocol::OpenVpn)
            .then(|| usize::try_from(ui.get_form_cert_index() - 1).ok().and_then(|i| st.cert_choices.get(i).cloned()))
            .flatten();
        // A different certificate: the old one's copy and passphrase go.
        if let Some(old) = existing.as_ref().and_then(|p| p.client_cert.clone()).filter(|old| client_cert.as_ref() != Some(old)) {
            certs::forget(&old);
            let _ = molehill_core::cert_secrets::delete(&id);
        }
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
            client_cert,
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

    /// The editor's certificate choice changed. On macOS its last entry opens
    /// the file dialog; on Windows every entry is a store certificate.
    #[cfg(windows)]
    fn cert_picked(&self, _index: i32) {}

    #[cfg(target_os = "macos")]
    fn cert_picked(&self, index: i32) {
        let count = self.state.borrow().cert_choices.len();
        if usize::try_from(index - 1).ok() != Some(count) {
            return;
        }
        let picked = rfd::FileDialog::new()
            .set_title("Choose the client certificate")
            .add_filter("Certificate (.p12, .pfx)", &["p12", "pfx"])
            .pick_file();
        let mut st = self.state.borrow_mut();
        match picked.map(|p| certs::import(&p)) {
            Some(Ok(uri)) => {
                st.cert_choices.push(uri.clone());
                let options: Vec<slint::SharedString> = std::iter::once("None".into())
                    .chain(st.cert_choices.iter().map(|u| certs::saved_label(u).into()))
                    .chain(std::iter::once(certs::CHOOSE_FILE.into()))
                    .collect();
                self.ui.set_form_cert_options(ModelRc::new(VecModel::from(options)));
                self.ui.set_form_cert_index(count as i32 + 1);
            }
            Some(Err(e)) => {
                self.ui.set_form_error(format!("Couldn't copy the certificate: {e}").into());
                self.ui.set_form_cert_index(0);
            }
            // Cancelled: back to the previous choice, or None.
            None => self.ui.set_form_cert_index(if count > 0 { count as i32 } else { 0 }),
        }
    }

    fn delete_profile(&self) {
        let name = self.ui.get_form_name().to_string();
        // A plain Yes/No: custom button labels fall back to OK/Cancel on a
        // plain Windows message box, which then answers "Ok".
        let answer = rfd::MessageDialog::new()
            .set_title("Delete connection")
            .set_description(format!("Delete “{name}” and its saved password? This can't be undone."))
            .set_level(rfd::MessageLevel::Warning)
            .set_buttons(rfd::MessageButtons::YesNo)
            .show();
        if !matches!(answer, rfd::MessageDialogResult::Yes | rfd::MessageDialogResult::Ok) {
            return;
        }
        let mut st = self.state.borrow_mut();
        st.fetch_seq += 1;
        st.save_after_fetch = false;
        let Some(id) = st.editing.take() else { return };
        self.client.send(Request::Disconnect { id: id.clone() });
        if let Some(p) = st.store.remove(&id) {
            let _ = secrets::delete_password(&id);
            let _ = molehill_core::cert_secrets::delete(&id);
            if let Some(cert) = &p.client_cert {
                certs::forget(cert);
            }
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
        self.ui.set_log_text(if text.is_empty() { "Nothing yet. Connect to see the VPN engine's output here.".into() } else { text.into() });
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
        let (mut connected, mut busy, mut failed) = (Vec::new(), Vec::new(), 0usize);
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
                Some(s) if s.state == TunnelState::Failed => {
                    failed += 1;
                    ("error", s.detail.clone())
                }
                _ => match t.and_then(|t| t.local_error.clone()) {
                    Some(e) => {
                        failed += 1;
                        ("error", e)
                    }
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
        ui.set_app_update(st.app_update.as_ref().map(|r| r.version.clone()).unwrap_or_default().into());
        ui.set_app_update_busy(st.app_update_busy);
        ui.set_app_update_status(st.app_update_status.clone().into());
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

        // One state for the tray icon and the marks in the window and menu.
        // Activity wins (connecting shows even while another tunnel is up).
        let mark = if !st.service_online {
            TrayState::Offline
        } else if !busy.is_empty() {
            TrayState::Connecting
        } else if connected.len() > 1 {
            TrayState::ConnectedMany
        } else if connected.len() == 1 {
            TrayState::Connected
        } else if failed > 0 {
            TrayState::Failed
        } else {
            TrayState::Idle
        };
        let tint = match mark {
            TrayState::Idle => logo::ACCENT,
            TrayState::Connecting => logo::YELLOW,
            TrayState::Connected => logo::GREEN,
            TrayState::ConnectedMany => logo::VIOLET,
            TrayState::Failed => logo::CORAL,
            TrayState::Offline => logo::WHITE_MARK,
        };
        // The title bar icon, and a badge on the taskbar button (no badge:
        // nothing connected). The Start menu shows the .exe's own icon.
        platform::set_window_icon_tint(self.ui.window(), tint);
        let badge = match mark {
            TrayState::Idle => None,
            TrayState::Connecting => Some("Connecting"),
            TrayState::Connected => Some("Connected"),
            TrayState::ConnectedMany => Some("Several VPNs connected"),
            TrayState::Failed => Some("A connection failed"),
            TrayState::Offline => Some("Service not running"),
        };
        platform::set_taskbar_badge(self.ui.window(), badge.map(|label| (tint, label)));
        let tint = slint::Color::from_rgb_u8(tint[0], tint[1], tint[2]);
        self.ui.set_mark_tint(tint);
        self.menu.set_mark_tint(tint);
        if let Some(tray) = self.tray.borrow().as_ref() {
            tray.update(mark, &format!("Molehill · {summary}"));
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
            auth: None,
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

/// The password field GnuTLS shows for a certificate file's passphrase
/// (openconnect's `request_passphrase`).
fn is_cert_passphrase_field(name: &str) -> bool {
    matches!(name, "openconnect_pkcs12" | "openconnect_secondary_pkcs12" | "openconnect_pem" | "openconnect_secondary_pem")
}
