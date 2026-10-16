//! `molehill-service.exe oc-auth`: signs in to a Cisco / Fortinet /
//! GlobalProtect server with libopenconnect and returns the session cookie.
//!
//! It runs as a child of the service so a mistake across the FFI boundary
//! can only take down this helper. The protocol is JSON lines: the service
//! writes an [`AuthRequest`], then [`HelperAnswer`]s whenever the helper
//! sends a prompt; the helper writes [`HelperEvent`]s to stdout.

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::io::{BufRead, Write};

use anyhow::{Context, Result, anyhow, bail};
use molehill_core::ipc::{FieldKind, FormField};

pub use molehill_core::ocauth::{AuthRequest, AuthResult, HelperAnswer, HelperEvent};

/// What `authenticate` came to.
enum Next {
    Done(AuthResult),
    /// Sign in again with this request (GlobalProtect, after the browser).
    Again(AuthRequest),
}

// ── libopenconnect ABI (openconnect.h, API 5.x) ──────────────────────────

const OC_FORM_OPT_TEXT: c_int = 1;
const OC_FORM_OPT_PASSWORD: c_int = 2;
const OC_FORM_OPT_SELECT: c_int = 3;
const OC_FORM_OPT_TOKEN: c_int = 5;
// SAML forms: libopenconnect expects an embedded browser to fill these.
const OC_FORM_OPT_SSO_TOKEN: c_int = 6;
const OC_FORM_OPT_SSO_USER: c_int = 7;
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
    get_port: unsafe extern "C" fn(Vpn) -> c_int,
    set_client_cert: unsafe extern "C" fn(Vpn, *const c_char, *const c_char) -> c_int,
    set_option_value: unsafe extern "C" fn(*mut OcFormOpt, *const c_char) -> c_int,
    set_external_browser_callback: Option<unsafe extern "C" fn(Vpn, Option<BrowserCb>) -> c_int>,
    /// The bundled GnuTLS was built with Homebrew's CA file compiled in,
    /// which a Mac without Homebrew doesn't have; trust comes from the
    /// `.pkg`'s copy of the system roots instead.
    #[cfg(target_os = "macos")]
    set_system_trust: Option<unsafe extern "C" fn(Vpn, std::ffi::c_uint)>,
    #[cfg(target_os = "macos")]
    set_cafile: Option<unsafe extern "C" fn(Vpn, *const c_char) -> c_int>,
}

impl Api {
    fn load(dir: &std::path::Path) -> Result<Self> {
        #[cfg(windows)]
        use libloading::os::windows::{LOAD_WITH_ALTERED_SEARCH_PATH, Library};
        #[cfg(windows)]
        let path = dir.join("libopenconnect-5.dll");
        // Altered search path: its own DLLs (GnuTLS etc.) load from its folder.
        #[cfg(windows)]
        let lib: libloading::Library = unsafe { Library::load_with_flags(&path, LOAD_WITH_ALTERED_SEARCH_PATH) }
            .with_context(|| format!("loading {}", path.display()))?
            .into();
        // The bundled dylibs find each other through @loader_path.
        #[cfg(target_os = "macos")]
        let path = dir.join("libopenconnect.5.dylib");
        #[cfg(target_os = "macos")]
        let lib = unsafe { libloading::Library::new(&path) }.with_context(|| format!("loading {}", path.display()))?;
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
            get_port: sym!("openconnect_get_port"),
            set_client_cert: sym!("openconnect_set_client_cert"),
            set_option_value: sym!("openconnect_set_option_value"),
            set_external_browser_callback: unsafe { lib.get(b"openconnect_set_external_browser_callback\0") }.ok().map(|s| *s),
            #[cfg(target_os = "macos")]
            set_system_trust: unsafe { lib.get(b"openconnect_set_system_trust\0") }.ok().map(|s| *s),
            #[cfg(target_os = "macos")]
            set_cafile: unsafe { lib.get(b"openconnect_set_cafile\0") }.ok().map(|s| *s),
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
    input: Input,
    /// Name of the password field the saved password went into.
    password_field: Option<String>,
    /// `auth_id` of the form the saved password went into (usually "main").
    password_form: Option<String>,
    rejected_reported: bool,
    /// A different form followed the password form, so the server took the
    /// password (the next step is usually a 2FA challenge).
    password_accepted: bool,
    /// The server's error from a failed challenge, shown with the next one.
    challenge_error: Option<String>,
    auto_submits: u32,
    group_switched: bool,
    cancelled: bool,
    /// Fortinet SSO: only connect (and settle certificate trust), then stop
    /// at the first form.
    probing: bool,
    probed: bool,
    /// Selects the user answered in this sign-in (by field name), so the form
    /// the server sends back after a group change doesn't ask again.
    picked: std::collections::HashSet<String>,
    /// What the user typed into a form whose group they changed: the server
    /// answers a group change with a fresh form, which gets these back.
    carry: Vec<(String, String)>,
    /// The server sent a SAML form while in password mode.
    sso_needed: bool,
    last_error: Option<String>,
}

pub(crate) fn emit(ev: &HelperEvent) {
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
    /// Select: how many choices, and whether the current one is settled (the
    /// saved group, or picked by the user earlier in this sign-in).
    pub choices: usize,
    pub decided: bool,
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
            // A real choice nobody made yet is the user's to make.
            FieldKind::Select => {
                if f.choices > 1 && !f.decided {
                    complete = false;
                }
            }
        }
    }
    plan.auto_submit = !plan.fills.is_empty() && complete && !has_error && auto_submits < 5;
    plan
}

unsafe extern "C" fn on_form(privdata: *mut c_void, form: *mut OcAuthForm) -> c_int {
    let ctx = unsafe { &mut *(privdata as *mut Ctx) };
    if ctx.probing {
        ctx.probed = true;
        return OC_FORM_RESULT_CANCELLED;
    }
    let form = unsafe { &mut *form };
    let error = cstr(form.error);

    // Pick the saved group first; the server sends a fresh form for it.
    if let Some(group) = ctx.req.group.clone().filter(|_| !form.authgroup_opt.is_null() && !ctx.group_switched) {
        let choices = select_choices(form.authgroup_opt);
        if let Some(i) = choices.iter().position(|(n, l)| same(n, &group) || same(l, &group))
            && i as c_int != form.authgroup_selection {
                ctx.group_switched = true;
                ctx.set(unsafe { &mut (*form.authgroup_opt).form }, &choices[i].0);
                return OC_FORM_RESULT_NEWGROUP;
            }
    }

    let auth_id = cstr(form.auth_id).unwrap_or_default();
    // GlobalProtect's gateway choice: nothing to ask with one gateway, or
    // when the saved one is already picked.
    if auth_id == "_portal" && !form.authgroup_opt.is_null() {
        let choices = select_choices(form.authgroup_opt);
        let current = usize::try_from(form.authgroup_selection).ok().and_then(|i| choices.get(i));
        let saved = ctx.req.group.as_deref().is_some_and(|g| current.is_some_and(|(n, l)| same(n, g) || same(l, g)));
        if choices.len() == 1 || saved {
            if let Some((_, label)) = current.or(choices.first()) {
                emit(&HelperEvent::Log { line: format!("Using gateway {label}") });
            }
            return OC_FORM_RESULT_OK;
        }
    }
    // Every field, for the log; then the ones the user can see.
    let mut all = Vec::new();
    let mut opts = Vec::new();
    let mut p = form.opts;
    while !p.is_null() {
        let o = unsafe { &*p };
        if matches!(o.kind, OC_FORM_OPT_SSO_TOKEN | OC_FORM_OPT_SSO_USER) {
            ctx.sso_needed = true;
        }
        all.push(format!("{}:{}{}", cstr(o.name).unwrap_or_default(), o.kind, if o.flags != 0 { format!("/{:#x}", o.flags) } else { String::new() }));
        if o.flags & OC_FORM_OPT_IGNORE == 0 && matches!(o.kind, OC_FORM_OPT_TEXT | OC_FORM_OPT_PASSWORD | OC_FORM_OPT_SELECT | OC_FORM_OPT_TOKEN) {
            opts.push(p);
        }
        p = o.next;
    }
    if ctx.sso_needed {
        return OC_FORM_RESULT_CANCELLED;
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
            let name = cstr(o.name).unwrap_or_default();
            let current = cstr(o.value).unwrap_or_default();
            let (choices, decided) = if kind == FieldKind::Select {
                let list = select_choices(op as *mut OcFormOptSelect);
                let label = list.iter().find(|(n, _)| *n == current).map(|(_, l)| l.clone()).unwrap_or_default();
                let saved = ctx.req.group.as_deref().is_some_and(|g| !current.is_empty() && (same(&current, g) || same(&label, g)));
                (list.len(), saved || ctx.picked.contains(&name))
            } else {
                (0, false)
            };
            FieldInfo { name, kind, current, choices, decided }
        })
        .collect();

    // The fresh form after the user changed group: put back what they had
    // typed, and send it if that fills every field.
    if !ctx.carry.is_empty() && error.is_none() {
        let carry = std::mem::take(&mut ctx.carry);
        let mut complete = true;
        for (i, &op) in opts.iter().enumerate() {
            if infos[i].kind == FieldKind::Select {
                continue;
            }
            match carry.iter().find(|(n, _)| *n == infos[i].name) {
                Some((_, v)) => ctx.set(op, v),
                None => complete = false,
            }
        }
        if complete {
            emit(&HelperEvent::Log { line: format!("Server form \"{auth_id}\" for the new group: filled in with what you typed") });
            return OC_FORM_RESULT_OK;
        }
    }

    let back_to_login = ctx.password_form.as_deref() == Some(auth_id.as_str());
    if !back_to_login && ctx.password_form.is_some() {
        ctx.password_accepted = true;
    }
    // Some ASAs restart at the login form when the 2FA code is wrong. The
    // password got past it already, so fill it in again: the server then
    // sends a new code and asks for it.
    let retry = back_to_login && error.is_some() && ctx.password_accepted;
    if retry {
        ctx.password_accepted = false;
        ctx.challenge_error = error.clone();
    }
    // The same login form (by auth_id) coming back with an error, without a
    // challenge in between, means the saved password was wrong. A challenge
    // (auth_id "challenge", whose answer field libopenconnect also calls
    // "password") with an error doesn't.
    if back_to_login && error.is_some() && !retry && !ctx.rejected_reported && !ctx.req.saml_login {
        ctx.rejected_reported = true;
        emit(&HelperEvent::PasswordRejected);
    }

    let plan = plan_form(
        &infos,
        error.is_some() && !retry,
        ctx.req.username.as_deref(),
        ctx.req.password.as_deref(),
        ctx.password_field.is_some() && !retry,
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
            "Server form \"{}\" ({} {}): {} visible field(s) [{}], all fields [{}]{}{}{}",
            auth_id,
            cstr(form.method).unwrap_or_default(),
            cstr(form.action).unwrap_or_default(),
            infos.len(),
            infos.iter().map(|f| format!("{}:{:?}", f.name, f.kind)).collect::<Vec<_>>().join(", "),
            all.join(", "),
            cstr(form.message).map(|m| format!(", message: {m}")).unwrap_or_default(),
            error.as_deref().map(|e| format!(", error: {e}")).unwrap_or_default(),
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
    let error = error.or_else(|| ctx.challenge_error.take());
    let banner = [cstr(form.banner), cstr(form.message)].into_iter().flatten().collect::<Vec<_>>().join("\n");
    emit(&HelperEvent::Form { banner: (!banner.is_empty()).then_some(banner), error, fields });

    match ctx.answer() {
        HelperAnswer::Form { values } => {
            let group_name = (!form.authgroup_opt.is_null()).then(|| cstr(unsafe { (*form.authgroup_opt).form.name })).flatten();
            for (i, &op) in opts.iter().enumerate() {
                let name = &infos[i].name;
                let Some((_, value)) = values.iter().find(|(n, _)| n == name) else { continue };
                if infos[i].kind == FieldKind::Select {
                    ctx.picked.insert(name.clone());
                }
                // An empty box must not wipe what Molehill filled in.
                if value.is_empty() && filled.contains(&i) {
                    continue;
                }
                // A different group means a different form: ask the server for it.
                if group_name.as_deref() == Some(name.as_str()) {
                    let choices = select_choices(form.authgroup_opt);
                    if let Some(c) = choices.iter().position(|(n, _)| n == value)
                        && c as c_int != form.authgroup_selection {
                            // Keep what was typed (and what Molehill filled) for the
                            // new form, so the user doesn't type it all again.
                            ctx.carry = opts
                                .iter()
                                .enumerate()
                                .filter(|(j, _)| infos[*j].kind != FieldKind::Select)
                                .filter_map(|(j, &o)| {
                                    let typed = values.iter().find(|(n, _)| *n == infos[j].name).map(|(_, v)| v.clone()).filter(|v| !v.is_empty());
                                    let current = cstr(unsafe { (*o).value });
                                    typed.or(current).map(|v| (infos[j].name.clone(), v))
                                })
                                .collect();
                            ctx.set(op, value);
                            return OC_FORM_RESULT_NEWGROUP;
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

#[cfg_attr(not(windows), allow(dead_code))]
unsafe extern "C" fn on_progress(privdata: *mut c_void, level: c_int, fmt: *const c_char, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize, a6: usize) {
    // Debug output too (it explains odd server forms), minus anything secret.
    if level > PRG_DEBUG || fmt.is_null() {
        return;
    }
    let line = format_c(unsafe { CStr::from_ptr(fmt) }.to_bytes(), &[a1, a2, a3, a4, a5, a6]);
    unsafe { progress_line(privdata, level, line) };
}

// macOS: the C shim (src/oc_progress.c) is the variadic callback; it formats
// the message and calls `on_progress_line` with the result.
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn molehill_set_progress_line(f: unsafe extern "C" fn(*mut c_void, c_int, *const c_char));
    fn molehill_progress(privdata: *mut c_void, level: c_int, fmt: *const c_char, ...);
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn on_progress_line(privdata: *mut c_void, level: c_int, line: *const c_char) {
    if level > PRG_DEBUG || line.is_null() {
        return;
    }
    let line = unsafe { CStr::from_ptr(line) }.to_string_lossy().into_owned();
    unsafe { progress_line(privdata, level, line) };
}

/// The progress callback to hand libopenconnect.
#[cfg(windows)]
fn progress_cb() -> ProgressCb {
    on_progress
}

#[cfg(target_os = "macos")]
fn progress_cb() -> ProgressCb {
    unsafe { molehill_set_progress_line(on_progress_line) };
    // SAFETY: only ever called by libopenconnect, which calls it as the
    // variadic function it really is.
    unsafe { std::mem::transmute::<unsafe extern "C" fn(*mut c_void, c_int, *const c_char, ...), ProgressCb>(molehill_progress) }
}

/// A formatted progress message: logged, minus anything secret.
unsafe fn progress_line(privdata: *mut c_void, level: c_int, line: String) {
    let line = line.trim_end().to_string();
    if line.is_empty() {
        return;
    }
    let lower = line.to_ascii_lowercase();
    let sensitive = ["cookie", "password", "passwd", "secret", "token", "session"].iter().any(|w| lower.contains(w));
    // Debug lines carry values (headers, form fields): drop them outright.
    // Errors and info must stay visible, so those only lose any value part.
    if sensitive && level == PRG_DEBUG {
        return;
    }
    let line = if sensitive { redact(&line) } else { line };
    if level == 0 {
        let ctx = unsafe { &mut *(privdata as *mut Ctx) };
        ctx.last_error = Some(line.clone());
    }
    emit(&HelperEvent::Log { line });
}

/// Cuts whatever follows `=` or `: ` in a line that mentions a secret, so a
/// message like `cookie=abc…` becomes `cookie=…`.
fn redact(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    // Only a `key=value` shape carries a value; error text like "Failed to
    // obtain WebVPN cookie: HTTP 403" must stay readable.
    let cut = ["cookie", "password", "passwd", "secret", "token", "session"]
        .iter()
        .flat_map(|w| lower.match_indices(w).map(move |(i, _)| i + w.len()))
        .filter(|&end| lower.as_bytes().get(end) == Some(&b'='))
        .min();
    match cut {
        Some(end) => format!("{}…", &line[..=end]),
        None => line.to_string(),
    }
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
    let mut req: AuthRequest = serde_json::from_str(&first)?;
    loop {
        let (result, rest) = authenticate(req, input);
        input = rest;
        match result {
            Ok(Next::Again(next)) => req = next,
            Ok(Next::Done(result)) => break emit(&HelperEvent::Done(result)),
            Err(e) => break emit(&HelperEvent::Failed { message: format!("{e:#}") }),
        }
    }
    Ok(())
}

/// The URL to start at. A Fortinet realm goes into the login URL the way the
/// FortiGate itself redirects to it (`/remote/login?realm=…`); libopenconnect
/// picks it up from there.
fn login_url(req: &AuthRequest) -> String {
    let server = req.server.trim().trim_end_matches('/');
    let Some(realm) = req.group.as_deref().map(str::trim).filter(|r| !r.is_empty() && req.protocol == "fortinet") else {
        return server.to_string();
    };
    let host = server.strip_prefix("https://").unwrap_or(server);
    let host = host.split('/').next().unwrap_or(host);
    format!("https://{host}/remote/login?realm={}", crate::fortinet_sso::url_encode(realm))
}

type Input = std::io::Lines<std::io::StdinLock<'static>>;

/// One sign-in with libopenconnect. Hands `input` back for the next round.
fn authenticate(req: AuthRequest, input: Input) -> (Result<Next>, Input) {
    let api = match Api::load(&req.dir) {
        Ok(api) => api,
        Err(e) => return (Err(e), input),
    };
    unsafe { (api.init_ssl)() };
    let protocol = cstring(&req.protocol);
    let server = cstring(&login_url(&req));
    let ctx = Box::into_raw(Box::new(Ctx {
        api,
        req,
        vpn: std::ptr::null_mut(),
        input,
        password_field: None,
        password_form: None,
        rejected_reported: false,
        password_accepted: false,
        challenge_error: None,
        auto_submits: 0,
        group_switched: false,
        cancelled: false,
        probing: false,
        probed: false,
        picked: Default::default(),
        carry: Vec::new(),
        sso_needed: false,
        last_error: None,
    }));
    // SAFETY: ctx lives until the end of this function; libopenconnect only
    // calls back into it from inside obtain_cookie, on this thread.
    let c = unsafe { &mut *ctx };
    // The openconnect CLI's agent string. Some ASAs answer the XML login with
    // 404 for anything else, and the HTML fallback drops 2FA (SMS) fields.
    let agent = cstring("AnyConnect-compatible OpenConnect VPN Agent");
    let vpn = unsafe {
        (c.api.vpninfo_new)(agent.as_ptr(), Some(on_validate_cert), Some(on_write_config), Some(on_form), Some(progress_cb()), ctx as *mut c_void)
    };
    if vpn.is_null() {
        let ctx = unsafe { Box::from_raw(ctx) };
        return (Err(anyhow!("libopenconnect couldn't start")), ctx.input);
    }
    c.vpn = vpn;
    #[cfg(target_os = "macos")]
    if let (Some(trust), Some(cafile)) = (c.api.set_system_trust, c.api.set_cafile) {
        let file = cstring(&format!("{}/cacert.pem", molehill_core::paths::ENGINES_DIR));
        unsafe {
            trust(vpn, 0);
            cafile(vpn, file.as_ptr());
        }
    }
    let result = (|| {
        if unsafe { (c.api.set_protocol)(vpn, protocol.as_ptr()) } != 0 {
            bail!("OpenConnect doesn't support the {} protocol", c.req.protocol);
        }
        if unsafe { (c.api.parse_url)(vpn, server.as_ptr()) } != 0 {
            bail!("“{}” isn't a valid server address", c.req.server);
        }
        // A client certificate from the Windows store, for servers that ask
        // for one along with the password.
        if let Some(cert) = c.req.cert.as_deref() {
            let key = c.req.key.clone().unwrap_or_else(|| molehill_core::ocauth::key_for_cert(cert));
            let (cert_c, key_c) = (cstring(cert), cstring(&key));
            if unsafe { (c.api.set_client_cert)(vpn, cert_c.as_ptr(), key_c.as_ptr()) } != 0 {
                bail!("The client certificate couldn't be loaded");
            }
        }
        if let Some(set_browser) = c.api.set_external_browser_callback {
            unsafe { set_browser(vpn, Some(on_browser)) };
        }
        // Browser SSO that libopenconnect can't do: it only connects here, to
        // settle certificate trust; the rest happens outside it.
        let sso = c.req.sso && matches!(c.req.protocol.as_str(), "fortinet" | "gp");
        c.probing = sso;
        let rc = unsafe { (c.api.obtain_cookie)(vpn) };
        if c.sso_needed {
            bail!("This server signs in through the browser (SAML). Edit the connection and pick Browser SSO.");
        }
        if c.cancelled {
            bail!("Cancelled");
        }
        if sso {
            // A SAML-only GlobalProtect portal fails before any form, but
            // after the TLS handshake, so the certificate is known anyway.
            let pin = cstr(unsafe { (c.api.get_peer_cert_hash)(vpn) })
                .filter(|_| c.probed || c.req.protocol == "gp")
                .ok_or_else(|| anyhow!("{}", c.last_error.clone().unwrap_or_else(|| "Couldn't reach the server".into())))?;
            // The name, not the address libopenconnect resolved it to: the browser
            // and curl need it for the certificate and the cookies.
            let host = cstr(unsafe { (c.api.get_dnsname)(vpn) })
                .or_else(|| cstr(unsafe { (c.api.get_hostname)(vpn) }))
                .ok_or_else(|| anyhow!("no server name"))?;
            let port = unsafe { (c.api.get_port)(vpn) };
            let base = if port == 443 || port <= 0 { format!("https://{host}") } else { format!("https://{host}:{port}") };
            if c.req.protocol == "gp" {
                let req = c.req.clone();
                return crate::gp_sso::sso(&base, &pin, &req, || c.answer()).map(Next::Again);
            }
            return crate::fortinet_sso::sso(&base, &pin, c.req.group.as_deref()).map(Next::Done);
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
        Ok(Next::Done(AuthResult { cookie, connect_url, resolve, pin }))
    })();
    let ctx = unsafe {
        (c.api.vpninfo_free)(vpn);
        Box::from_raw(ctx)
    };
    (result, ctx.input)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(name: &str, kind: FieldKind) -> FieldInfo {
        FieldInfo { name: name.into(), kind, current: String::new(), choices: 0, decided: false }
    }

    fn plan(fields: &[FieldInfo], error: bool, password_used: bool) -> FillPlan {
        plan_form(fields, error, Some("shota"), Some("s3cret"), password_used, 0)
    }

    #[test]
    fn fills_and_submits_a_plain_login() {
        let p = plan(&[f("group_list", FieldKind::Select), f("username", FieldKind::Text), f("password", FieldKind::Password)], false, false);
        assert!(p.auto_submit);
        // …with a single group, or the saved one already selected.
        let decided = FieldInfo { choices: 3, decided: true, ..f("group_list", FieldKind::Select) };
        assert!(plan(&[decided, f("username", FieldKind::Text), f("password", FieldKind::Password)], false, false).auto_submit);
    }

    #[test]
    fn asks_when_the_server_offers_a_real_choice() {
        // Cisco's group list with no saved group: the user picks, even
        // though username and password could be filled in.
        let groups = FieldInfo { choices: 3, ..f("group_list", FieldKind::Select) };
        let p = plan(&[groups, f("username", FieldKind::Text), f("password", FieldKind::Password)], false, false);
        assert!(!p.auto_submit);
        assert_eq!(p.fills.len(), 2, "still prefilled, so only the choice is left");
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

#[cfg(test)]
mod redact_tests {
    #[test]
    fn keeps_errors_readable_but_cuts_values() {
        assert_eq!(super::redact("Failed to obtain WebVPN cookie: HTTP 403"), "Failed to obtain WebVPN cookie: HTTP 403");
        assert_eq!(super::redact("Set-Cookie: webvpn=abc; path=/"), "Set-Cookie: webvpn=abc; path=/");
        assert_eq!(super::redact("Got cookie=abcdef"), "Got cookie=…");
        assert_eq!(super::redact("Got session=12345 for user"), "Got session=…");
        assert_eq!(super::redact("Session terminated by server"), "Session terminated by server");
    }
}

#[cfg(all(test, target_os = "macos"))]
mod progress_shim_tests {
    use super::*;
    use std::sync::Mutex;

    static LINES: Mutex<Vec<(c_int, String)>> = Mutex::new(Vec::new());

    unsafe extern "C" fn capture(_: *mut c_void, level: c_int, line: *const c_char) {
        let line = unsafe { CStr::from_ptr(line) }.to_string_lossy().into_owned();
        LINES.lock().unwrap().push((level, line));
    }

    /// The shim must format real variadic arguments (strings, ints, longs)
    /// the way C does; this is what broke with fixed arguments on arm64.
    #[test]
    fn shim_formats_variadic_arguments() {
        unsafe { molehill_set_progress_line(capture) };
        let fmt = cstring("Connected to %s:%d (%ld bytes, %u%%)");
        let host = cstring("vpn.example.com");
        unsafe { molehill_progress(std::ptr::null_mut(), 1, fmt.as_ptr(), host.as_ptr(), 443 as c_int, 123_456_789_012i64, 7u32) };
        assert_eq!(LINES.lock().unwrap().pop(), Some((1, "Connected to vpn.example.com:443 (123456789012 bytes, 7%)".into())));
    }
}
