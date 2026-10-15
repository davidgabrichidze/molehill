//! Renders the UI headlessly to PNG files, with demo data, for reviewing
//! changes and for the README. Nothing is shown on screen.
//!
//!     cargo run -p molehill --example snapshot -- [out-dir]

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, ModelRc, PhysicalSize, VecModel};

slint::include_modules!();

const SCALE: f32 = 1.25;

#[derive(Default, Clone)]
struct Headless {
    windows: Rc<RefCell<Vec<Rc<MinimalSoftwareWindow>>>>,
}

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        let w = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        w.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: SCALE });
        self.windows.borrow_mut().push(w.clone());
        Ok(w)
    }
}

fn save(window: &MinimalSoftwareWindow, size: (u32, u32), path: &Path) {
    window.set_size(PhysicalSize::new(size.0, size.1));
    slint::platform::update_timers_and_animations();
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        let (w, h) = (size.0 as usize, size.1 as usize);
        let mut buf = vec![PremultipliedRgbaColor::default(); w * h];
        renderer.render(&mut buf, w);
        let file = std::fs::File::create(path).expect("create png");
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), size.0, size.1);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let data: Vec<u8> = buf.iter().flat_map(|p| [p.red, p.green, p.blue, p.alpha]).collect();
        enc.write_header().unwrap().write_image_data(&data).unwrap();
    });
    println!("wrote {}", path.display());
}

fn item(id: &str, name: &str, state: &str, status: &str) -> ConnectionItem {
    ConnectionItem {
        id: id.into(),
        name: name.into(),
        tag: "OVPN".into(),
        state: state.into(),
        status: status.into(),
        ..Default::default()
    }
}

fn main() {
    let out = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("snapshots"));
    std::fs::create_dir_all(&out).unwrap();
    let platform = Headless::default();
    slint::platform::set_platform(Box::new(platform.clone())).unwrap();

    let ui = AppWindow::new().unwrap();
    ui.show().unwrap();
    let main_window = platform.windows.borrow()[0].clone();
    let size = ((440.0 * SCALE) as u32, (660.0 * SCALE) as u32);

    // List: one connected, one asking for credentials, one off.
    let mut on = item("a", "Acme Corp", "on", "Connected · 10.8.0.6");
    on.info = "↓ 182.4 MB  ↑ 12.9 MB  ·  1h 07m".into();
    let mut prompt = item("b", "Client VPN", "busy", "Waiting for you");
    prompt.prompt_kind = "credentials".into();
    prompt.prompt_id = 1;
    prompt.prompt_username = "shota".into();
    prompt.remember = true;
    let off = item("c", "Home lab", "off", "home.example.net:443");
    ui.set_connections(ModelRc::new(VecModel::from(vec![on.clone(), prompt, off.clone()])));
    ui.set_summary("Connecting Client VPN… · Connected to Acme Corp".into());
    ui.set_any_on(true);
    ui.set_any_busy(true);
    ui.set_locked(true);
    ui.set_service_online(true);
    save(&main_window, size, &out.join("list.png"));

    // Cisco: the server's login form, and an untrusted certificate.
    let opts = |v: &[&str]| ModelRc::new(VecModel::from(v.iter().map(|s| slint::SharedString::from(*s)).collect::<Vec<_>>()));
    let field = |label: &str, kind: &str, value: &str| FormFieldItem {
        name: label.into(),
        label: label.into(),
        kind: kind.into(),
        value: value.into(),
        options: opts(&[]),
        selected: 0,
        focus: false,
        prefilled: false,
    };
    let mut form = item("d", "Acme Cisco", "busy", "Waiting for you");
    form.tag = "CISCO".into();
    form.prompt_kind = "form".into();
    form.prompt_id = 2;
    form.prompt_label = "Please enter your username and password.".into();
    form.prompt_fields = ModelRc::new(VecModel::from(vec![
        FormFieldItem { options: opts(&["Employees", "Contractors", "Admins"]), kind: "select".into(), ..field("Group", "select", "Employees") },
        field("Username", "text", "shota"),
        field("Password", "password", ""),
        field("Second password (OTP)", "password", ""),
    ]));
    let mut cert = item("e", "Lab ASA", "busy", "Waiting for you");
    cert.tag = "CISCO".into();
    cert.prompt_kind = "cert".into();
    cert.prompt_id = 3;
    cert.prompt_label = "signer not found".into();
    cert.prompt_pin = "pin-sha256:Xy7hQ2v9w6uYpP3m0sRkq8bT1fN4cJ5eLzA2dGh6iKo=".into();
    ui.set_connections(ModelRc::new(VecModel::from(vec![form, cert])));
    ui.set_summary("Connecting 2 VPNs…".into());
    ui.set_any_on(false);
    save(&main_window, size, &out.join("cisco-prompts.png"));

    // Editor, Cisco.
    ui.set_page(Page::Editor);
    ui.set_form_is_new(true);
    ui.set_form_name("Acme Cisco".into());
    ui.set_form_protocol(1);
    ui.set_form_gateway("vpn.acme.example".into());
    ui.set_form_group("Employees".into());
    ui.set_form_username("shota".into());
    ui.set_form_remember(true);
    save(&main_window, size, &out.join("editor-cisco.png"));
    ui.set_form_protocol(0);

    // Editor, manual setup.
    ui.set_page(Page::Editor);
    ui.set_form_is_new(true);
    ui.set_form_name("Office".into());
    ui.set_form_source(2);
    ui.set_form_host("vpn.example.com".into());
    ui.set_form_port("1194".into());
    ui.set_form_ca("-----BEGIN CERTIFICATE-----\nMIIDSzCCAjOgAwIBAgIUQx…\n-----END CERTIFICATE-----".into());
    ui.set_form_tls_mode(2);
    ui.set_form_tls_key("-----BEGIN OpenVPN Static key V1-----\n6acef03f62675b4b…".into());
    ui.set_form_username("shota".into());
    ui.set_form_remember(true);
    save(&main_window, size, &out.join("editor-manual.png"));

    // Editor, Access Server.
    ui.set_form_source(1);
    ui.set_form_server("vpn.example.com".into());
    ui.set_form_fetch_info("fetched · server vpn.example.com:1194".into());
    save(&main_window, size, &out.join("editor-server.png"));

    // Tray menu.
    let menu = TrayMenu::new().unwrap();
    menu.show().unwrap();
    let menu_window = platform.windows.borrow().last().unwrap().clone();
    menu.set_summary("Connected to Acme Corp".into());
    menu.set_any_on(true);
    menu.set_tunnels(ModelRc::new(VecModel::from(vec![
        MenuTunnel { id: "a".into(), name: "Acme Corp".into(), state: "on".into() },
        MenuTunnel { id: "b".into(), name: "Client VPN".into(), state: "busy".into() },
        MenuTunnel { id: "c".into(), name: "Home lab".into(), state: "off".into() },
    ])));
    let h = menu.window().size().height.max(1);
    let menu_size = ((272.0 * SCALE) as u32, h.max((300.0 * SCALE) as u32));
    save(&menu_window, menu_size, &out.join("tray-menu.png"));
}
