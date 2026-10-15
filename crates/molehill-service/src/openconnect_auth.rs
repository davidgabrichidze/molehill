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
const OC_FORM_OPT_TOKEN: c_int = 5;
const OC_FORM_OPT_IGNORE: c_uint = 0x0001;
const OC_FORM_RESULT_OK: c_int = 0;
const OC_FORM_RESULT_CANCELLED: c_int = 1;
const OC_FORM_RESULT_NEWGROUP: c_int = 2;
const PRG_DEBUG: c_int = 2;

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
    /// Name of the password field the saved password went into.
    password_field: Option<String>,
    /// `auth_id` of the form the saved password went into (usually "main").
    password_form: Option<String>,
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

/// A username field, judged by its *name* only: labels like "Code sent to
/// user X" must not get the username.
pub fn is_username_field(name: &str) -> bool {
    matches!(name.trim().to_ascii_lowercase().as_str(), "username" | "user" | "user_name" | "uname" | "login" | "email")
}

/// One visible field of a server form, as the auto-fill logic sees it.
#[derive(Debug, Clone)]
pub struct FieldInfo {
    pub name: String,
    pub kind: FieldKind,
    /// What the server put in already.
    pub current: String,
}

#[derive(Debug, Default, PartialEq)]
pub struct FillPlan {
    /// (field index, value) to set.
    pub fills: Vec<(usize, String)>,
    /// Name of the password field that gets the saved password, if any.
    pub password_field: Option<String>,
    /// Submit without showing the form.
    pub auto_submit: bool,
}

/// Decides what to fill in a server login form and whether to submit it
/// without asking. The saved password goes into one password field, once;
/// anything else (OTP, second password, a challenge, a message-only or
/// select-only form) is shown to the user.
pub fn plan_form(
    fields: &[FieldInfo],
    has_error: bool,
    username: Option<&str>,
    password: Option<&str>,
    password_used: bool,
    auto_submits: u32,
) -> FillPlan {
    let mut plan = FillPlan::default();
    let mut complete = true;
    for (i, f) in fields.iter().enumerate() {
        match f.kind {
            FieldKind::Text => {
                if !f.current.is_empty() {
                    continue;
                }
                match username.filter(|u| !u.is_empty() && is_username_field(&f.name)) {
                    Some(u) => plan.fills.push((i, u.to_string())),
                    None => complete = false,
                }
            }
            FieldKind::Password => {
                let usable = !password_used && !has_error && plan.password_field.is_none();
                match password.filter(|p| usable && !p.is_empty()) {
                    Some(p) => {
                        plan.fills.push((i, p.to_string()));
                        plan.password_field = Some(f.name.clone());
                    }
                    None => complete = false,
                }
            }
            FieldKind::Select => {}
        }
    }
    plan.auto_submit = !plan.fills.is_empty() && complete && !has_error && auto_submits < 3;
    plan
}

unsafe extern "C" fn on_form(privdata: *mut c_void, form: *mut OcAuthForm) -> c_int {
    let ctx = unsafe { &mut *(privdata as *mut Ctx) };
    let form = unsafe { &mut *form };
    let error = cstr(form.error);

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

    let auth_id = cstr(form.auth_id).unwrap_or_default();
    // Every field, for the log; then the ones the user can see.
    let mut all = Vec::new();
    let mut opts = Vec::new();
    let mut p = form.opts;
    while !p.is_null() {
        let o = unsafe { &*p };
        all.push(format!("{}:{}{}", cstr(o.name).unwrap_or_default(), o.kind, if o.flags != 0 { format!("/{:#x}", o.flags) } else { String::new() }));
        if o.flags & OC_FORM_OPT_IGNORE == 0 && matches!(o.kind, OC_FORM_OPT_TEXT | OC_FORM_OPT_PASSWORD | OC_FORM_OPT_SELECT | OC_FORM_OPT_TOKEN) {
            opts.push(p);
        }
        p = o.next;
    }
    let infos: Vec<FieldInfo> = opts
        .iter()
        .map(|&op| {
            let o = unsafe { &*op };
            let kind = match o.kind {
                OC_FORM_OPT_TEXT => FieldKind::Text,
                OC_FORM_OPT_SELECT => FieldKind::Select,
                _ => FieldKind::Password,
            };
            FieldInfo { name: cstr(o.name).unwrap_or_default(), kind, current: cstr(o.value).unwrap_or_default() }
        })
        .collect();

    // The same login form (by auth_id) coming back with an error means the
    // saved password was wrong. A challenge (auth_id "challenge", whose answer
    // field libopenconnect also calls "password") with an error doesn't.
    if let Some(filled) = &ctx.password_form {
        if error.is_some() && *filled == auth_id && !ctx.rejected_reported {
            ctx.rejected_reported = true;
            emit(&HelperEvent::PasswordRejected);
        }
    }

    let plan = plan_form(
        &infos,
        error.is_some(),
        ctx.req.username.as_deref(),
        ctx.req.password.as_deref(),
        ctx.password_field.is_some(),
        ctx.auto_submits,
    );
    for (i, value) in &plan.fills {
        ctx.set(opts[*i], value);
    }
    if plan.password_field.is_some() {
        ctx.password_field = plan.password_field.clone();
        ctx.password_form = Some(auth_id.clone());
    }
    emit(&HelperEvent::Log {
        line: format!(
            "Server form \"{}\" ({} {}): {} visible field(s) [{}], all fields [{}]{}{}",
            auth_id,
            cstr(form.method).unwrap_or_default(),
            cstr(form.action).unwrap_or_default(),
            infos.len(),
            infos.iter().map(|f| format!("{}:{:?}", f.name, f.kind)).collect::<Vec<_>>().join(", "),
            all.join(", "),
            cstr(form.message).map(|m| format!(", message: {m}")).unwrap_or_default(),
            if plan.auto_submit { ", filled in automatically" } else { ", asking you" }
        ),
    });
    if plan.auto_submit {
        ctx.auto_submits += 1;
        return OC_FORM_RESULT_OK;
    }

    let filled: Vec<usize> = plan.fills.iter().map(|(i, _)| *i).collect();
    let fields: Vec<FormField> = opts
        .iter()
        .enumerate()
        .map(|(i, &op)| {
            let o = unsafe { &*op };
            let name = infos[i].name.clone();
            let label = cstr(o.label).unwrap_or_else(|| name.clone());
            let prefilled = filled.contains(&i);
            match infos[i].kind {
                FieldKind::Select => {
                    let options = select_choices(op as *mut OcFormOptSelect);
                    let current = cstr(o.value).unwrap_or_default();
                    let value = if current.is_empty() { options.first().map(|o| o.0.clone()).unwrap_or_default() } else { current };
                    FormField { name, label, kind: FieldKind::Select, value, options, prefilled }
                }
                // Never send a password back to the app, only that one is filled.
                FieldKind::Password => FormField { name, label, kind: FieldKind::Password, value: String::new(), options: vec![], prefilled },
                FieldKind::Text => FormField { name, label, kind: FieldKind::Text, value: cstr(o.value).unwrap_or_default(), options: vec![], prefilled },
            }
        })
        .collect();
    let banner = [cstr(form.banner), cstr(form.message)].into_iter().flatten().collect::<Vec<_>>().join("\n");
    emit(&HelperEvent::Form { banner: (!banner.is_empty()).then_some(banner), error, fields });

    match ctx.answer() {
        HelperAnswer::Form { values } => {
            let group_name = (!form.authgroup_opt.is_null()).then(|| cstr(unsafe { (*form.authgroup_opt).form.name })).flatten();
            for (i, &op) in opts.iter().enumerate() {
                let name = &infos[i].name;
                let Some((_, value)) = values.iter().find(|(n, _)| n == name) else { continue };
                // An empty box must not wipe what Molehill filled in.
                if value.is_empty() && filled.contains(&i) {
                    continue;
                }
                // A different group means a different form: ask the server for it.
                if group_name.as_deref() == Some(name.as_str()) {
                    let choices = select_choices(form.authgroup_opt);
                    if let Some(c) = choices.iter().position(|(n, _)| n == value) {
                        if c as c_int != form.authgroup_selection {
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
    // Debug output too (it explains odd server forms), minus anything secret.
    if level > PRG_DEBUG || fmt.is_null() {
        return;
    }
    let line = format_c(unsafe { CStr::from_ptr(fmt) }.to_bytes(), &[a1, a2, a3, a4, a5, a6]);
    let line = line.trim_end().to_string();
    if line.is_empty() {
        return;
    }
    let lower = line.to_ascii_lowercase();
    if level == PRG_DEBUG && ["cookie", "password", "passwd", "secret", "token", "session"].iter().any(|w| lower.contains(w)) {
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
        password_field: None,
        password_form: None,
        rejected_reported: false,
        auto_submits: 0,
        group_switched: false,
        cancelled: false,
        last_error: None,
    }));
    // SAFETY: ctx lives until the end of this function; libopenconnect only
    // calls back into it from inside obtain_cookie, on this thread.
    let c = unsafe { &mut *ctx };
    // The openconnect CLI's agent string. Some ASAs answer the XML login with
    // 404 for anything else, and the HTML fallback drops 2FA (SMS) fields.
    let agent = cstring("AnyConnect-compatible OpenConnect VPN Agent");
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

    fn f(name: &str, kind: FieldKind) -> FieldInfo {
        FieldInfo { name: name.into(), kind, current: String::new() }
    }

    fn plan(fields: &[FieldInfo], error: bool, password_used: bool) -> FillPlan {
        plan_form(fields, error, Some("shota"), Some("s3cret"), password_used, 0)
    }

    #[test]
    fn fills_and_submits_a_plain_login() {
        let p = plan(&[f("group_list", FieldKind::Select), f("username", FieldKind::Text), f("password", FieldKind::Password)], false, false);
        assert!(p.auto_submit);
        assert_eq!(p.fills, vec![(1, "shota".into()), (2, "s3cret".into())]);
        assert_eq!(p.password_field.as_deref(), Some("password"));
    }

    #[test]
    fn asks_for_a_second_password_in_the_same_form() {
        let p = plan(&[f("username", FieldKind::Text), f("password", FieldKind::Password), f("secondary_password", FieldKind::Password)], false, false);
        assert!(!p.auto_submit, "the OTP field must be shown");
        assert_eq!(p.fills.len(), 2);
    }

    #[test]
    fn shows_challenge_forms_after_the_password() {
        // A follow-up form with only a password-type answer field.
        let p = plan(&[f("answer", FieldKind::Password)], false, true);
        assert!(!p.auto_submit);
        assert!(p.fills.is_empty());
        // Message-only and select-only forms are shown too, never skipped.
        assert!(!plan(&[], false, true).auto_submit);
        assert!(!plan(&[f("method", FieldKind::Select)], false, true).auto_submit);
    }

    #[test]
    fn username_goes_only_into_username_fields() {
        // An OTP field named "answer" whose label mentions the user stays empty.
        let p = plan(&[f("answer", FieldKind::Text)], false, true);
        assert!(p.fills.is_empty() && !p.auto_submit);
        assert!(is_username_field("Username") && !is_username_field("user_code_sent"));
    }

    #[test]
    fn never_reuses_the_password_after_an_error() {
        let p = plan(&[f("username", FieldKind::Text), f("password", FieldKind::Password)], true, false);
        assert!(!p.auto_submit);
        assert!(p.fills.iter().all(|(i, _)| *i == 0));
    }

    #[test]
    fn formats_progress_messages() {
        let host = CString::new("vpn.example.com").unwrap();
        let s = format_c(b"Connected to %s:%d (%u%%)\n", &[host.as_ptr() as usize, 443, 99]);
        assert_eq!(s, "Connected to vpn.example.com:443 (99%)\n");
        assert_eq!(format_c(b"%ld left", &[(-5i64) as usize]), "-5 left");
    }
}
