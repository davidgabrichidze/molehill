//! `molehill-service.exe oc-auth`: signs in to a Cisco / Fortinet /
//! GlobalProtect server with libopenconnect and returns the session cookie.
//!
//! It runs as a child of the service so a mistake across the FFI boundary
//! can only take down this helper. The protocol is JSON lines: the service
//! writes an [`AuthRequest`], then [`HelperAnswer`]s whenever the helper
//! sends a prompt; the helper writes [`HelperEvent`]s to stdout.

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use molehill_core::ipc::{FieldKind, FormField};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthRequest {
    /// OpenConnect's install directory (holds libopenconnect-5.dll).
    pub dir: PathBuf,
    pub protocol: String,
    pub server: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub group: Option<String>,
    pub pin: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthResult {
    pub cookie: String,
    pub connect_url: String,
    /// `--resolve host:ip` so the tunnel doesn't look the name up again.
    pub resolve: Option<String>,
    pub pin: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HelperEvent {
    Log { line: String },
    Form { banner: Option<String>, error: Option<String>, fields: Vec<FormField> },
    TrustCert { pin: String, reason: String },
    OpenUrl { url: String },
    /// The saved password was filled in and the server rejected it.
    PasswordRejected,
    Done(AuthResult),
    Failed { message: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HelperAnswer {
    Form { values: Vec<(String, String)> },
    Trust { accept: bool },
    Cancel,
}

// ── libopenconnect ABI (openconnect.h, API 5.x) ──────────────────────────

const OC_FORM_OPT_TEXT: c_int = 1;
const OC_FORM_OPT_PASSWORD: c_int = 2;
const OC_FORM_OPT_SELECT: c_int = 3;
const OC_FORM_OPT_IGNORE: c_uint = 0x0001;
const OC_FORM_RESULT_OK: c_int = 0;
const OC_FORM_RESULT_CANCELLED: c_int = 1;
const OC_FORM_RESULT_NEWGROUP: c_int = 2;
const PRG_INFO: c_int = 1;

#[repr(C)]
struct OcFormOpt {
    next: *mut OcFormOpt,
    kind: c_int,
    name: *mut c_char,
    label: *mut c_char,
    value: *mut c_char,
    flags: c_uint,
    reserved: *mut c_void,
}

#[repr(C)]
struct OcChoice {
    name: *mut c_char,
    label: *mut c_char,
    // more fields follow; only the first two are read
}

#[repr(C)]
struct OcFormOptSelect {
    form: OcFormOpt,
    nr_choices: c_int,
    choices: *mut *mut OcChoice,
}

#[repr(C)]
struct OcAuthForm {
    banner: *mut c_char,
    message: *mut c_char,
    error: *mut c_char,
    auth_id: *mut c_char,
    method: *mut c_char,
    action: *mut c_char,
    opts: *mut OcFormOpt,
    authgroup_opt: *mut OcFormOptSelect,
    authgroup_selection: c_int,
}

type Vpn = *mut c_void;
type ValidateCb = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
type WriteConfigCb = unsafe extern "C" fn(*mut c_void, *const c_char, c_int) -> c_int;
type FormCb = unsafe extern "C" fn(*mut c_void, *mut OcAuthForm) -> c_int;
// Really variadic (`const char *fmt, ...`). On x64 Windows variadic args are
// passed like ordinary ones, so we take the first six as integers and do a
// small printf ourselves.
type ProgressCb = unsafe extern "C" fn(*mut c_void, c_int, *const c_char, usize, usize, usize, usize, usize, usize);
type BrowserCb = unsafe extern "C" fn(Vpn, *const c_char, *mut c_void) -> c_int;

struct Api {
    _lib: libloading::Library,
    init_ssl: unsafe extern "C" fn() -> c_int,
    vpninfo_new: unsafe extern "C" fn(*const c_char, Option<ValidateCb>, Option<WriteConfigCb>, Option<FormCb>, Option<ProgressCb>, *mut c_void) -> Vpn,
    vpninfo_free: unsafe extern "C" fn(Vpn),
    set_protocol: unsafe extern "C" fn(Vpn, *const c_char) -> c_int,
    parse_url: unsafe extern "C" fn(Vpn, *const c_char) -> c_int,
    obtain_cookie: unsafe extern "C" fn(Vpn) -> c_int,
    get_cookie: unsafe extern "C" fn(Vpn) -> *const c_char,
    get_hostname: unsafe extern "C" fn(Vpn) -> *const c_char,
    get_dnsname: unsafe extern "C" fn(Vpn) -> *const c_char,
    get_connect_url: unsafe extern "C" fn(Vpn) -> *const c_char,
    get_peer_cert_hash: unsafe extern "C" fn(Vpn) -> *const c_char,
    set_option_value: unsafe extern "C" fn(*mut OcFormOpt, *const c_char) -> c_int,
    set_external_browser_callback: Option<unsafe extern "C" fn(Vpn, Option<BrowserCb>) -> c_int>,
}

impl Api {
    fn load(dir: &std::path::Path) -> Result<Self> {
        use libloading::os::windows::{LOAD_WITH_ALTERED_SEARCH_PATH, Library};
        let path = dir.join("libopenconnect-5.dll");
        // Altered search path: its own DLLs (GnuTLS etc.) load from its folder.
        let lib: libloading::Library = unsafe { Library::load_with_flags(&path, LOAD_WITH_ALTERED_SEARCH_PATH) }
            .with_context(|| format!("loading {}", path.display()))?
            .into();
        macro_rules! sym {
            ($name:literal) => {
                *unsafe { lib.get(concat!($name, "\0").as_bytes()) }.with_context(|| format!("libopenconnect has no {}", $name))?
            };
        }
        Ok(Self {
            init_ssl: sym!("openconnect_init_ssl"),
            vpninfo_new: sym!("openconnect_vpninfo_new"),
            vpninfo_free: sym!("openconnect_vpninfo_free"),
            set_protocol: sym!("openconnect_set_protocol"),
            parse_url: sym!("openconnect_parse_url"),
            obtain_cookie: sym!("openconnect_obtain_cookie"),
            get_cookie: sym!("openconnect_get_cookie"),
            get_hostname: sym!("openconnect_get_hostname"),
            get_dnsname: sym!("openconnect_get_dnsname"),
            get_connect_url: sym!("openconnect_get_connect_url"),
            get_peer_cert_hash: sym!("openconnect_get_peer_cert_hash"),
            set_option_value: sym!("openconnect_set_option_value"),
            set_external_browser_callback: unsafe { lib.get(b"openconnect_set_external_browser_callback\0") }.ok().map(|s| *s),
            _lib: lib,
        })
    }
}

fn cstr(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn cstring(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

// ── helper state ─────────────────────────────────────────────────────────

struct Ctx {
    api: Api,
    req: AuthRequest,
    vpn: Vpn,
    input: std::io::Lines<std::io::StdinLock<'static>>,
    password_filled: bool,
    rejected_reported: bool,
    auto_submits: u32,
    group_switched: bool,
    cancelled: bool,
    last_error: Option<String>,
}

fn emit(ev: &HelperEvent) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", serde_json::to_string(ev).unwrap_or_default());
    let _ = out.flush();
}

impl Ctx {
    fn answer(&mut self) -> HelperAnswer {
        match self.input.next() {
            Some(Ok(line)) => serde_json::from_str(&line).unwrap_or(HelperAnswer::Cancel),
            _ => HelperAnswer::Cancel,
        }
    }

    fn set(&self, opt: *mut OcFormOpt, value: &str) {
        let v = cstring(value);
        unsafe { (self.api.set_option_value)(opt, v.as_ptr()) };
    }
}

fn same(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

fn select_choices(sel: *mut OcFormOptSelect) -> Vec<(String, String)> {
    let sel = unsafe { &*sel };
    (0..sel.nr_choices.max(0) as usize)
        .filter_map(|i| {
            let c = unsafe { *sel.choices.add(i) };
            if c.is_null() {
                return None;
            }
            let c = unsafe { &*c };
            let name = cstr(c.name).unwrap_or_default();
            let label = cstr(c.label).unwrap_or_else(|| name.clone());
            Some((name, label))
        })
        .collect()
}

fn looks_like_username(name: &str, label: &str) -> bool {
    let n = format!("{name} {label}").to_ascii_lowercase();
    n.contains("user") || n.contains("login") || n.contains("email")
}

unsafe extern "C" fn on_form(privdata: *mut c_void, form: *mut OcAuthForm) -> c_int {
    let ctx = unsafe { &mut *(privdata as *mut Ctx) };
    let form = unsafe { &mut *form };
    let error = cstr(form.error);
    if error.is_some() && ctx.password_filled && !ctx.rejected_reported {
        ctx.rejected_reported = true;
        emit(&HelperEvent::PasswordRejected);
    }

    // Pick the saved group first; the server sends a fresh form for it.
    if let Some(group) = ctx.req.group.clone().filter(|_| !form.authgroup_opt.is_null() && !ctx.group_switched) {
        let choices = select_choices(form.authgroup_opt);
        if let Some(i) = choices.iter().position(|(n, l)| same(n, &group) || same(l, &group)) {
            if i as c_int != form.authgroup_selection {
                ctx.group_switched = true;
                ctx.set(unsafe { &mut (*form.authgroup_opt).form }, &choices[i].0);
                return OC_FORM_RESULT_NEWGROUP;
            }
        }
    }

    let mut opts = Vec::new();
    let mut p = form.opts;
    while !p.is_null() {
        opts.push(p);
        p = unsafe { (*p).next };
    }

    let mut fields = Vec::new();
    let mut complete = true;
    for &op in &opts {
        let o = unsafe { &*op };
        if o.flags & OC_FORM_OPT_IGNORE != 0 || !matches!(o.kind, OC_FORM_OPT_TEXT | OC_FORM_OPT_PASSWORD | OC_FORM_OPT_SELECT) {
            continue;
        }
        let name = cstr(o.name).unwrap_or_default();
        let label = cstr(o.label).unwrap_or_else(|| name.clone());
        let current = cstr(o.value).unwrap_or_default();
        match o.kind {
            OC_FORM_OPT_TEXT => {
                let mut value = current;
                if value.is_empty() && looks_like_username(&name, &label) {
                    if let Some(u) = ctx.req.username.clone() {
                        ctx.set(op, &u);
                        value = u;
                    }
                }
                complete &= !value.is_empty();
                fields.push(FormField { name, label, kind: FieldKind::Text, value, options: vec![] });
            }
            OC_FORM_OPT_PASSWORD => {
                // The saved password goes into the first password field once;
                // anything after that (OTP, a retry) is for the user.
                match ctx.req.password.clone().filter(|_| !ctx.password_filled && error.is_none()) {
                    Some(pw) => {
                        ctx.set(op, &pw);
                        ctx.password_filled = true;
                    }
                    None => complete = false,
                }
                fields.push(FormField { name, label, kind: FieldKind::Password, value: String::new(), options: vec![] });
            }
            _ => {
                let options = select_choices(op as *mut OcFormOptSelect);
                let value = if current.is_empty() { options.first().map(|o| o.0.clone()).unwrap_or_default() } else { current };
                fields.push(FormField { name, label, kind: FieldKind::Select, value, options });
            }
        }
    }

    if complete && error.is_none() && ctx.auto_submits < 3 {
        ctx.auto_submits += 1;
        return OC_FORM_RESULT_OK;
    }

    let banner = [cstr(form.banner), cstr(form.message)].into_iter().flatten().collect::<Vec<_>>().join("\n");
    emit(&HelperEvent::Form { banner: (!banner.is_empty()).then_some(banner), error, fields });
    match ctx.answer() {
        HelperAnswer::Form { values } => {
            let group_name = (!form.authgroup_opt.is_null()).then(|| cstr(unsafe { (*form.authgroup_opt).form.name })).flatten();
            for &op in &opts {
                let name = cstr(unsafe { (*op).name }).unwrap_or_default();
                let Some((_, value)) = values.iter().find(|(n, _)| *n == name) else { continue };
                // A different group means a different form: ask the server for it.
                if group_name.as_deref() == Some(name.as_str()) {
                    let choices = select_choices(form.authgroup_opt);
                    if let Some(i) = choices.iter().position(|(n, _)| n == value) {
                        if i as c_int != form.authgroup_selection {
                            ctx.set(op, value);
                            return OC_FORM_RESULT_NEWGROUP;
                        }
                    }
                }
                ctx.set(op, value);
            }
            OC_FORM_RESULT_OK
        }
        _ => {
            ctx.cancelled = true;
            OC_FORM_RESULT_CANCELLED
        }
    }
}

unsafe extern "C" fn on_validate_cert(privdata: *mut c_void, reason: *const c_char) -> c_int {
    let ctx = unsafe { &mut *(privdata as *mut Ctx) };
    let pin = cstr(unsafe { (ctx.api.get_peer_cert_hash)(ctx.vpn) }).unwrap_or_default();
    if !pin.is_empty() && ctx.req.pin.as_deref() == Some(pin.as_str()) {
        return 0;
    }
    let reason = cstr(reason).unwrap_or_else(|| "certificate not trusted".into());
    emit(&HelperEvent::TrustCert { pin, reason });
    match ctx.answer() {
        HelperAnswer::Trust { accept: true } => 0,
        _ => {
            ctx.cancelled = true;
            1
        }
    }
}

unsafe extern "C" fn on_write_config(_: *mut c_void, _: *const c_char, _: c_int) -> c_int {
    0
}

unsafe extern "C" fn on_browser(_: Vpn, uri: *const c_char, _: *mut c_void) -> c_int {
    match cstr(uri) {
        Some(url) => {
            emit(&HelperEvent::OpenUrl { url });
            0
        }
        None => -1,
    }
}

unsafe extern "C" fn on_progress(privdata: *mut c_void, level: c_int, fmt: *const c_char, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize, a6: usize) {
    if level > PRG_INFO || fmt.is_null() {
        return;
    }
    let line = format_c(unsafe { CStr::from_ptr(fmt) }.to_bytes(), &[a1, a2, a3, a4, a5, a6]);
    let line = line.trim_end().to_string();
    if line.is_empty() {
        return;
    }
    if level == 0 {
        let ctx = unsafe { &mut *(privdata as *mut Ctx) };
        ctx.last_error = Some(line.clone());
    }
    emit(&HelperEvent::Log { line });
}

/// Minimal printf for progress messages: %s %d %i %u %x %c %% with optional
/// l/ll/z/h length modifiers. Anything fancier is shown as-is.
fn format_c(fmt: &[u8], args: &[usize]) -> String {
    let mut out = String::new();
    let mut args = args.iter();
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            let start = i;
            while i < fmt.len() && fmt[i] != b'%' {
                i += 1;
            }
            out.push_str(&String::from_utf8_lossy(&fmt[start..i]));
            continue;
        }
        let spec_start = i;
        i += 1;
        while i < fmt.len() && matches!(fmt[i], b'l' | b'z' | b'h' | b'-' | b'0'..=b'9') {
            i += 1;
        }
        let Some(&conv) = fmt.get(i) else { break };
        i += 1;
        let long = fmt[spec_start..i].contains(&b'l') || fmt[spec_start..i].contains(&b'z');
        match conv {
            b'%' => out.push('%'),
            b's' => {
                let p = *args.next().unwrap_or(&0) as *const c_char;
                out.push_str(&if p.is_null() { "(null)".into() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() });
            }
            b'd' | b'i' => {
                let v = *args.next().unwrap_or(&0);
                out.push_str(&if long { (v as i64).to_string() } else { (v as u32 as i32).to_string() });
            }
            b'u' => {
                let v = *args.next().unwrap_or(&0);
                out.push_str(&if long { (v as u64).to_string() } else { (v as u32).to_string() });
            }
            b'x' => {
                let v = *args.next().unwrap_or(&0);
                out.push_str(&if long { format!("{v:x}") } else { format!("{:x}", v as u32) });
            }
            b'c' => out.push(*args.next().unwrap_or(&0) as u8 as char),
            _ => out.push_str(&String::from_utf8_lossy(&fmt[spec_start..i])),
        }
    }
    out
}

/// Entry point of `oc-auth`.
pub fn run() -> Result<()> {
    let mut input = std::io::stdin().lock().lines();
    let first = input.next().ok_or_else(|| anyhow!("no request"))??;
    let req: AuthRequest = serde_json::from_str(&first)?;
    match authenticate(req, input) {
        Ok(result) => emit(&HelperEvent::Done(result)),
        Err(e) => emit(&HelperEvent::Failed { message: format!("{e:#}") }),
    }
    Ok(())
}

fn authenticate(req: AuthRequest, input: std::io::Lines<std::io::StdinLock<'static>>) -> Result<AuthResult> {
    let api = Api::load(&req.dir)?;
    unsafe { (api.init_ssl)() };
    let protocol = cstring(&req.protocol);
    let server = cstring(&req.server);
    let ctx = Box::into_raw(Box::new(Ctx {
        api,
        req,
        vpn: std::ptr::null_mut(),
        input,
        password_filled: false,
        rejected_reported: false,
        auto_submits: 0,
        group_switched: false,
        cancelled: false,
        last_error: None,
    }));
    // SAFETY: ctx lives until the end of this function; libopenconnect only
    // calls back into it from inside obtain_cookie, on this thread.
    let c = unsafe { &mut *ctx };
    let agent = cstring("Open AnyConnect VPN Agent");
    let vpn = unsafe {
        (c.api.vpninfo_new)(agent.as_ptr(), Some(on_validate_cert), Some(on_write_config), Some(on_form), Some(on_progress), ctx as *mut c_void)
    };
    if vpn.is_null() {
        bail!("libopenconnect couldn't start");
    }
    c.vpn = vpn;
    let result = (|| {
        if unsafe { (c.api.set_protocol)(vpn, protocol.as_ptr()) } != 0 {
            bail!("OpenConnect doesn't support the {} protocol", c.req.protocol);
        }
        if unsafe { (c.api.parse_url)(vpn, server.as_ptr()) } != 0 {
            bail!("“{}” isn't a valid server address", c.req.server);
        }
        if let Some(set_browser) = c.api.set_external_browser_callback {
            unsafe { set_browser(vpn, Some(on_browser)) };
        }
        let rc = unsafe { (c.api.obtain_cookie)(vpn) };
        if c.cancelled {
            bail!("Cancelled");
        }
        if rc != 0 {
            bail!("{}", c.last_error.clone().unwrap_or_else(|| "Sign-in failed".into()));
        }
        let cookie = cstr(unsafe { (c.api.get_cookie)(vpn) }).ok_or_else(|| anyhow!("the server returned no session cookie"))?;
        let connect_url = cstr(unsafe { (c.api.get_connect_url)(vpn) }).unwrap_or_else(|| c.req.server.clone());
        let host = cstr(unsafe { (c.api.get_hostname)(vpn) });
        let dns = cstr(unsafe { (c.api.get_dnsname)(vpn) });
        let resolve = match (dns, host) {
            (Some(d), Some(h)) if d != h => Some(format!("{d}:{h}")),
            _ => None,
        };
        let pin = cstr(unsafe { (c.api.get_peer_cert_hash)(vpn) });
        Ok(AuthResult { cookie, connect_url, resolve, pin })
    })();
    unsafe {
        (c.api.vpninfo_free)(vpn);
        drop(Box::from_raw(ctx));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_progress_messages() {
        let host = CString::new("vpn.example.com").unwrap();
        let s = format_c(b"Connected to %s:%d (%u%%)\n", &[host.as_ptr() as usize, 443, 99]);
        assert_eq!(s, "Connected to vpn.example.com:443 (99%)\n");
        assert_eq!(format_c(b"%ld left", &[(-5i64) as usize]), "-5 left");
    }
}
