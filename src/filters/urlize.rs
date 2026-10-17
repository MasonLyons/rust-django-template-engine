//! Native `urlize` / `urlizetrunc`: a port of `django.utils.html.Urlizer`
//! for the 5.2 and 6.1 branches.
//!
//! The text is split exactly like `word_split_re` (`[\s<>"']+`, Python
//! `str.isspace` semantics); separators and words without `.`/`@`/`:`
//! only escape. Other words run the native `handle_word` when every step is
//! provably identical for them:
//! - the word is ASCII (no Unicode `\w`/case-folding/IDNA questions);
//! - `trim_punctuation` never reaches the HTML-entity semicolon branch
//!   (that needs `html.unescape`);
//! - URL candidates contain no `&` (`html.unescape` is the identity) and
//!   only characters `smart_urlquote` leaves untouched (no `%`, `?`, `#`,
//!   `[`, `]`, lowercase scheme), so `urlsplit`/`quote`/`urlunsplit`
//!   round-trip them unchanged;
//! - e-mail domains are not `[...]` literals (IP validation);
//! - 6.x's `www.`/bare-domain branch emits stock's
//!   `RemovedInDjango2028Warning` (when `URLIZE_ASSUME_HTTPS` is off) via
//!   `warnings.warn_explicit` with the filename/lineno/module/registry that
//!   `stacklevel=2` resolves to, once per distinct word per call.
//!
//! Every other word runs the real `urlizer(word, ...)` (one word → one
//! `handle_word` call; memoised per call like stock's `local_cache`). The whole native path is disabled when
//! the `Urlizer` class, the module `urlizer` instance or its regexes differ
//! from the known stock shapes (e.g. a test mocking `handle_word`): the
//! class is re-verified whenever its `tp_version_tag` changes.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU32, Ordering};

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyDict, PyString, PyType};

use crate::render::context::Rctx;
use crate::render::value::{escape_html, helpers, is_safe_data, mark_safe_bound, mark_safe_str};

use super::utils;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Branch {
    /// 5.2: `simple_url_2_re` is `\w[^@]+\.(tld)`, wrapping punctuation
    /// overwrites `trail`, no HTTPS-transition warning.
    V52,
    /// 6.0+/6.1: DomainNameValidator-based `simple_url_2_re`, `deque`
    /// trail, `URLIZE_ASSUME_HTTPS` transition.
    V61,
}

struct UrlizerState {
    /// `None` when the stock shapes were not recognised: always Python.
    branch: Option<Branch>,
    class: Py<PyType>,
    instance: Py<PyAny>,
    instance_dict: Py<PyDict>,
    html_dict: Py<PyDict>,
    originals: Vec<(Py<PyString>, Py<PyAny>)>,
    max_url_length: usize,
    validated_tag: AtomicU32,
    /// 6.x HTTPS-transition warning, emitted natively exactly like
    /// `warnings.warn(..., stacklevel=2)` from `handle_word` (attributed to
    /// the `self.handle_word(` call line in `Urlizer.__call__`).
    warn: Option<WarnInfo>,
}

struct WarnInfo {
    warn_explicit: Py<PyAny>,
    message: Py<PyAny>,
    category: Py<PyAny>,
    filename: Py<PyAny>,
    lineno: Py<PyAny>,
    module: Py<PyAny>,
}

const WARN_PROBE: &std::ffi::CStr = c"
import dis, warnings
from django.utils import html as _h
_hw = _h.Urlizer.handle_word.__code__
_msgs = [c for c in _hw.co_consts if isinstance(c, str) and c.startswith('The default protocol will be changed')]
_cats = [n for n in _hw.co_names if n.startswith('RemovedInDjango')]
_lines = [i.positions.lineno for i in dis.get_instructions(_h.Urlizer.__call__)
          if i.opname in ('LOAD_ATTR', 'LOAD_METHOD') and i.argval == 'handle_word']
result = None
if len(_msgs) == 1 and len(_cats) == 1 and len(_lines) == 1 and _h.__dict__.get('warnings') is warnings:
    result = (warnings.warn_explicit, _msgs[0], _h.__dict__[_cats[0]],
              _h.Urlizer.__call__.__code__.co_filename, _lines[0], _h.__name__)
";

fn warn_info(py: Python<'_>) -> PyResult<Option<WarnInfo>> {
    let g = PyDict::new(py);
    py.run(WARN_PROBE, Some(&g), None)?;
    let Some(r) = g.get_item("result")? else {
        return Ok(None);
    };
    if r.is_none() {
        return Ok(None);
    }
    let t = r.cast_into::<pyo3::types::PyTuple>()?;
    Ok(Some(WarnInfo {
        warn_explicit: t.get_item(0)?.unbind(),
        message: t.get_item(1)?.unbind(),
        category: t.get_item(2)?.unbind(),
        filename: t.get_item(3)?.unbind(),
        lineno: t.get_item(4)?.unbind(),
        module: t.get_item(5)?.unbind(),
    }))
}

fn emit_https_warning(py: Python<'_>, st: &UrlizerState, w: &WarnInfo) -> PyResult<()> {
    let globals = st.html_dict.bind(py);
    let registry = globals
        .call_method1("setdefault", ("__warningregistry__", PyDict::new(py)))?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("module", w.module.bind(py))?;
    kwargs.set_item("registry", registry)?;
    kwargs.set_item("module_globals", globals)?;
    w.warn_explicit.bind(py).call(
        (
            w.message.bind(py),
            w.category.bind(py),
            w.filename.bind(py),
            w.lineno.bind(py),
        ),
        Some(&kwargs),
    )?;
    Ok(())
}

const CLASS_ATTRS: [&str; 15] = [
    "__call__",
    "handle_word",
    "trim_url",
    "trim_punctuation",
    "is_email_simple",
    "trailing_punctuation_chars",
    "wrapping_punctuation",
    "simple_url_re",
    "simple_url_2_re",
    "word_split_re",
    "mailto_template",
    "url_template",
    "wrapping_punctuation_openings",
    "trailing_punctuation_chars_no_semicolon",
    "trailing_punctuation_chars_has_semicolon",
];

const CACHED_PROPS: [&str; 3] = [
    "wrapping_punctuation_openings",
    "trailing_punctuation_chars_no_semicolon",
    "trailing_punctuation_chars_has_semicolon",
];

fn pattern_of(re: &Bound<'_, PyAny>) -> PyResult<(String, i64)> {
    Ok((re.getattr("pattern")?.extract()?, re.getattr("flags")?.extract()?))
}

fn detect_branch(py: Python<'_>, class: &Bound<'_, PyType>, instance: &Bound<'_, PyAny>) -> PyResult<Option<Branch>> {
    const IGNORECASE_UNICODE: i64 = 2 | 32;
    let ul = "\u{a1}-\u{ffff}";
    let hostname_re = format!("[a-z{ul}0-9](?:[a-z{ul}0-9-]{{0,61}}[a-z{ul}0-9])?");
    let domain_re = format!(r"(?:\.(?!-)[a-z{ul}0-9-]{{1,63}}(?<!-))*");
    let tld_no_fqdn_re = format!(r"\.(?!-)(?:[a-z{ul}-]{{2,63}}|xn--[a-z0-9]{{1,59}})(?<!-)");
    let v52_re = r"^www\.|^(?!http)\w[^@]+\.(com|edu|gov|int|mil|net|org)($|/.*)$".to_string();
    let v61_re = format!(
        r"^www\.|^(?!http)(?:{hostname_re})(?:{domain_re})\.(com|edu|gov|int|mil|net|org)($|/.*)$"
    );
    let simple2 = pattern_of(&class.getattr("simple_url_2_re")?)?;
    let regex_branch = if simple2 == (v52_re, IGNORECASE_UNICODE) {
        Branch::V52
    } else if simple2 == (v61_re, IGNORECASE_UNICODE) {
        Branch::V61
    } else {
        return Ok(None);
    };
    if pattern_of(&class.getattr("simple_url_re")?)? != (r"^https?://\[?\w".to_string(), IGNORECASE_UNICODE)
        || pattern_of(&class.getattr("word_split_re")?)? != (r#"([\s<>"']+)"#.to_string(), 32)
    {
        return Ok(None);
    }
    let ev = py.import("django.core.validators")?.getattr("EmailValidator")?;
    let user_re = concat!(
        r"(^[-!#$%&'*+/=?^_`{}|~0-9A-Z]+(\.[-!#$%&'*+/=?^_`{}|~0-9A-Z]+)*\Z",
        r#"|^"([\001-\010\013\014\016-\037!#-\[\]-\177]|\\[\001-\011\013\014\016-\177])"#,
        r#"*"\Z)"#
    );
    let dom_re = format!(r"^{hostname_re}{domain_re}{tld_no_fqdn_re}\Z");
    if pattern_of(&ev.getattr("user_regex")?)? != (user_re.to_string(), IGNORECASE_UNICODE)
        || pattern_of(&ev.getattr("domain_regex")?)? != (dom_re, IGNORECASE_UNICODE)
        || ev.getattr("domain_allowlist")?.extract::<Vec<String>>()? != ["localhost"]
    {
        return Ok(None);
    }
    let plain = |name: &str, want: &str| -> PyResult<bool> {
        Ok(class.getattr(name)?.extract::<String>()? == want)
    };
    if !plain("trailing_punctuation_chars", ".,:;!")?
        || !plain("mailto_template", "mailto:{local}@{domain}")?
        || !plain("url_template", "<a href=\"{href}\"{attrs}>{url}</a>")?
        || class
            .getattr("wrapping_punctuation")?
            .extract::<Vec<(String, String)>>()?
            != [("(".to_string(), ")".to_string()), ("[".to_string(), "]".to_string())]
    {
        return Ok(None);
    }
    // Trail semantics probe: 5.2 overwrites `trail` on a wrapping strip,
    // 6.x prepends.
    let (lead, middle, trail): (String, String, String) =
        instance.call_method1("trim_punctuation", ("a.b).",))?.extract()?;
    let trail_branch = match (lead.as_str(), middle.as_str(), trail.as_str()) {
        ("", "a.b", ")") => Branch::V52,
        ("", "a.b", ").") => Branch::V61,
        _ => return Ok(None),
    };
    // The HTTPS transition (warning + protocol switch) exists iff the
    // transitional setting does.
    let has_https_setting = py
        .import("django.conf.global_settings")?
        .hasattr("URLIZE_ASSUME_HTTPS")?;
    let warn_branch = if has_https_setting { Branch::V61 } else { Branch::V52 };
    if regex_branch != trail_branch || regex_branch != warn_branch {
        return Ok(None);
    }
    Ok(Some(regex_branch))
}

fn state(py: Python<'_>) -> PyResult<&UrlizerState> {
    static ST: PyOnceLock<UrlizerState> = PyOnceLock::new();
    ST.get_or_try_init(py, || -> PyResult<UrlizerState> {
        let html_mod = py.import("django.utils.html")?;
        let class = html_mod.getattr("Urlizer")?.cast_into::<PyType>()?;
        let instance = html_mod.getattr("urlizer")?;
        let instance_dict = instance.getattr("__dict__")?.cast_into::<PyDict>()?;
        let class_dict = class.getattr("__dict__")?;
        let mut originals = Vec::with_capacity(CLASS_ATTRS.len());
        for name in CLASS_ATTRS {
            let key = PyString::new(py, name);
            let v = class_dict.get_item(&key)?;
            originals.push((key.unbind(), v.unbind()));
        }
        let mut branch = detect_branch(py, &class, &instance).unwrap_or(None);
        let warn = if branch == Some(Branch::V61) {
            warn_info(py).unwrap_or(None)
        } else {
            None
        };
        if branch == Some(Branch::V61) && warn.is_none() {
            branch = None;
        }
        let max_url_length: usize = py
            .import("django.utils.http")?
            .getattr("MAX_URL_LENGTH")?
            .extract()?;
        Ok(UrlizerState {
            branch,
            class: class.unbind(),
            instance: instance.unbind(),
            instance_dict: instance_dict.unbind(),
            html_dict: html_mod.dict().unbind(),
            originals,
            max_url_length,
            validated_tag: AtomicU32::new(0),
            warn,
        })
    })
}

/// The stock `Urlizer` is in place (no mock/patch of the class, the module
/// instance or its instance attributes).
fn pristine(py: Python<'_>, st: &UrlizerState) -> PyResult<bool> {
    let class = st.class.bind(py);
    // SAFETY: `class` is a live type object; the field read is plain data.
    let tag = unsafe { (*class.as_ptr().cast::<pyo3::ffi::PyTypeObject>()).tp_version_tag };
    if tag == 0 || tag != st.validated_tag.load(Ordering::Relaxed) {
        let class_dict = class.getattr("__dict__")?;
        for (k, v) in &st.originals {
            match class_dict.get_item(k.bind(py)) {
                Ok(cur) if cur.is(v.bind(py)) => {}
                _ => return Ok(false),
            }
        }
        if tag != 0 {
            st.validated_tag.store(tag, Ordering::Relaxed);
        }
    }
    match st.html_dict.bind(py).get_item("urlizer")? {
        Some(i) if i.is(st.instance.bind(py)) => {}
        _ => return Ok(false),
    }
    for k in st.instance_dict.bind(py).keys() {
        let Ok(k) = k.cast::<PyString>() else {
            return Ok(false);
        };
        if !CACHED_PROPS.contains(&k.to_str()?) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Python `str.isspace` (what `\s` matches in a `str` pattern): Unicode
/// `White_Space` plus the `\x1c`–`\x1f` separators.
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

fn is_sep(c: char) -> bool {
    matches!(c, '<' | '>' | '"' | '\'') || is_py_space(c)
}

/// Marker: this word needs the real `handle_word`.
struct NeedPython;

struct Opts {
    branch: Branch,
    limit: Option<i64>,
    nofollow: bool,
    autoescape: bool,
    safe_input: bool,
    max_url_length: usize,
    assume_https: Option<bool>,
}

fn push_escaped(out: &mut String, s: &str) {
    match escape_html(s) {
        Some(e) => out.push_str(&e),
        None => out.push_str(s),
    }
}

/// The non-URL outcome: `mark_safe(word)` / `escape(word)` / `word`.
fn push_plain(out: &mut String, word: &str, o: &Opts) {
    if !o.safe_input && o.autoescape {
        push_escaped(out, word);
    } else {
        out.push_str(word);
    }
}

/// `trim_punctuation` on an ASCII word.
fn trim_punctuation(word: &str, branch: Branch) -> Result<(&str, String, String), NeedPython> {
    let middle0 = word.trim_start_matches(['(', '[']);
    let lead = &word[..word.len() - middle0.len()];
    let mut middle = middle0.to_string();
    let mut trail = String::new();
    let count = |c: u8| i64::try_from(middle0.bytes().filter(|&b| b == c).count()).unwrap_or(i64::MAX);
    let mut counts = [(count(b'('), count(b')'), ')'), (count(b'['), count(b']'), ']')];
    let mut trimmed = true;
    while trimmed && !middle.is_empty() {
        trimmed = false;
        for (open, close, closing) in &mut counts {
            if *open < *close && middle.ends_with(*closing) {
                let strip = *close - *open;
                let n = middle.len();
                let k = usize::try_from(strip).unwrap_or(n).min(n);
                let piece = middle.split_off(n - k);
                match branch {
                    Branch::V52 => trail = piece,
                    Branch::V61 => trail.insert_str(0, &piece),
                }
                trimmed = true;
                *close -= strip;
            }
        }
        let has_amp = middle.contains('&');
        let rlen = if has_amp {
            middle.trim_end_matches(['.', ',', ':', '!']).len()
        } else {
            middle.trim_end_matches(['.', ',', ':', ';', '!']).len()
        };
        if rlen != middle.len() {
            let piece = middle.split_off(rlen);
            trail.insert_str(0, &piece);
            trimmed = true;
        }
        if middle.ends_with(';') {
            // Possible HTML entity: `html.unescape` decides.
            return Err(NeedPython);
        }
    }
    Ok((lead, middle, trail))
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// `^https?://\[?\w` (IGNORECASE) on ASCII.
fn simple_url(m: &str) -> bool {
    let rest = if starts_with_ci(m, "https://") {
        &m[8..]
    } else if starts_with_ci(m, "http://") {
        &m[7..]
    } else {
        return false;
    };
    let rest = rest.strip_prefix('[').unwrap_or(rest);
    rest.bytes()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// Hostname / `domain_re` label: 1–63 of `[A-Za-z0-9-]`, no leading or
/// trailing hyphen.
fn label_ok(l: &str) -> bool {
    (1..=63).contains(&l.len())
        && l.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        && !l.starts_with('-')
        && !l.ends_with('-')
}

const GTLDS: [&str; 7] = ["com", "edu", "gov", "int", "mil", "net", "org"];

/// `simple_url_2_re.match` on ASCII.
fn simple_url_2(m: &str, branch: Branch) -> bool {
    if starts_with_ci(m, "www.") {
        return true;
    }
    if starts_with_ci(m, "http") {
        return false;
    }
    match branch {
        Branch::V61 => {
            let host = m.split_once('/').map_or(m, |(h, _)| h);
            let labels: Vec<&str> = host.split('.').collect();
            let Some((tld, rest)) = labels.split_last() else {
                return false;
            };
            !rest.is_empty()
                && GTLDS.iter().any(|t| t.eq_ignore_ascii_case(tld))
                && rest.iter().all(|l| label_ok(l))
        }
        Branch::V52 => {
            // `\w[^@]+\.(tld)($|/.*)$`
            let b = m.as_bytes();
            if !b.first().is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') {
                return false;
            }
            for i in 2..b.len() {
                if b[i - 1] == b'@' {
                    return false;
                }
                if b[i] == b'.'
                    && i + 4 <= b.len()
                    && GTLDS.iter().any(|t| t.as_bytes().eq_ignore_ascii_case(&b[i + 1..i + 4]))
                    && (i + 4 == b.len() || b[i + 4] == b'/')
                {
                    return true;
                }
            }
            false
        }
    }
}

/// `EmailValidator(allowlist=[])(value)` succeeds, on ASCII.
fn is_email_simple(value: &str) -> Result<bool, NeedPython> {
    if value.is_empty() || value.len() > 320 {
        return Ok(false);
    }
    let Some((user, domain)) = value.rsplit_once('@') else {
        return Ok(false);
    };
    // dot-atom (the quoted-string form needs `"`, which never survives the
    // word split).
    let atom_ok = |a: &str| {
        !a.is_empty()
            && a.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-!#$%&'*+/=?^_`{}|~".contains(&c))
    };
    if !user.split('.').all(atom_ok) {
        return Ok(false);
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if let Some((tld, rest)) = labels.split_last() {
        let tld_ok = (tld.len() >= 2
            && tld.len() <= 63
            && tld.bytes().all(|c| c.is_ascii_alphabetic() || c == b'-')
            && !tld.starts_with('-')
            && !tld.ends_with('-'))
            || (starts_with_ci(tld, "xn--")
                && (1..=59).contains(&(tld.len() - 4))
                && tld[4..].bytes().all(|c| c.is_ascii_alphanumeric()));
        if !rest.is_empty() && tld_ok && rest.iter().all(|l| label_ok(l)) {
            return Ok(true);
        }
    }
    if domain.starts_with('[') {
        return Err(NeedPython); // literal IPv4/IPv6 form
    }
    Ok(false)
}

/// `smart_urlquote(url)` when it is provably the identity (see module
/// docs); `None` otherwise.
fn smart_urlquote_identity(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let ok = rest.bytes().all(|c| {
        c.is_ascii_alphanumeric() || b"_.-~!$'()*+,;=:/@".contains(&c)
    });
    ok.then_some(url)
}

/// `urllib.parse.quote(s, safe="")` on ASCII.
fn quote_all(out: &mut String, s: &str) {
    for c in s.bytes() {
        if c.is_ascii_alphanumeric() || b"_.-~".contains(&c) {
            out.push(c as char);
        } else {
            let _ = write!(out, "%{c:02X}");
        }
    }
}

/// Native `handle_word`, appending its result to `out`.
fn handle_word(
    out: &mut String,
    word: &str,
    o: &Opts,
    needs_warn: &mut bool,
) -> Result<(), NeedPython> {
    if !word.bytes().any(|c| matches!(c, b'.' | b'@' | b':')) {
        push_plain(out, word, o);
        return Ok(());
    }
    if !word.is_ascii() {
        return Err(NeedPython);
    }
    let (lead, middle, trail) = trim_punctuation(word, o.branch)?;
    let mut nofollow_attr = if o.nofollow { " rel=\"nofollow\"" } else { "" };
    let url: Option<String> = if middle.len() <= o.max_url_length && simple_url(&middle) {
        if middle.contains('&') {
            return Err(NeedPython);
        }
        Some(smart_urlquote_identity(&middle).ok_or(NeedPython)?.to_string())
    } else if middle.len() <= o.max_url_length && simple_url_2(&middle, o.branch) {
        if middle.contains('&') {
            return Err(NeedPython);
        }
        let protocol = match o.branch {
            Branch::V52 => "http",
            Branch::V61 => match o.assume_https {
                Some(true) => "https",
                Some(false) => {
                    *needs_warn = true;
                    "http"
                }
                None => return Err(NeedPython),
            },
        };
        let full = format!("{protocol}://{middle}");
        smart_urlquote_identity(&full).ok_or(NeedPython)?;
        Some(full)
    } else if !middle.contains(':') && is_email_simple(&middle)? {
        let (local, domain) = middle.rsplit_once('@').ok_or(NeedPython)?;
        let mut u = String::from("mailto:");
        quote_all(&mut u, local);
        u.push('@');
        quote_all(&mut u, domain);
        nofollow_attr = "";
        Some(u)
    } else {
        None
    };
    let Some(url) = url else {
        push_plain(out, word, o);
        return Ok(());
    };
    // trim_url (ASCII: chars == bytes)
    let trimmed: String = match o.limit {
        Some(limit) if i64::try_from(middle.len()).unwrap_or(i64::MAX) > limit => {
            let keep = usize::try_from((limit - 1).max(0)).unwrap_or(0);
            format!("{}…", &middle[..keep.min(middle.len())])
        }
        _ => middle.clone(),
    };
    let esc = o.autoescape && !o.safe_input;
    let push_maybe = |out: &mut String, s: &str| {
        if esc {
            push_escaped(out, s);
        } else {
            out.push_str(s);
        }
    };
    push_maybe(out, lead);
    out.push_str("<a href=\"");
    push_escaped(out, &url);
    out.push('"');
    out.push_str(nofollow_attr);
    out.push('>');
    push_maybe(out, &trimmed);
    out.push_str("</a>");
    push_maybe(out, &trail);
    Ok(())
}

/// `urlizer(text, ...)` with the per-word Python fallback; `None` when the
/// native path is unavailable for this call.
fn urlize_native(
    py: Python<'_>,
    st: &UrlizerState,
    text: &str,
    o: &Opts,
    limit_obj: &Bound<'_, PyAny>,
) -> PyResult<String> {
    let mut out = String::with_capacity(text.len() + text.len() / 2);
    let mut memo: Vec<(String, String)> = Vec::new();
    let mut warned: Vec<&str> = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        // word
        let wend = rest.find(is_sep).unwrap_or(rest.len());
        let word = &rest[..wend];
        if !word.is_empty() {
            let mark = out.len();
            let mut needs_warn = false;
            let res = handle_word(&mut out, word, o, &mut needs_warn);
            if needs_warn && res.is_ok() && !warned.contains(&word) {
                // stock: one `handle_word` (and warning) per distinct word
                // per call (`local_cache`).
                let w = st.warn.as_ref().ok_or_else(|| {
                    pyo3::exceptions::PyRuntimeError::new_err("urlize warning info missing")
                })?;
                emit_https_warning(py, st, w)?;
                warned.push(word);
            }
            if res.is_err() {
                out.truncate(mark);
                if let Some((_, r)) = memo.iter().find(|(w, _)| w == word) {
                    out.push_str(r);
                } else {
                    // Through `urlizer(word, ...)` rather than a direct
                    // `handle_word` call: a separator-free word splits to
                    // itself, and the `stacklevel=2` warning frame stays
                    // `Urlizer.__call__` exactly as in stock.
                    let kwargs = PyDict::new(py);
                    kwargs.set_item("trim_url_limit", limit_obj)?;
                    kwargs.set_item("nofollow", o.nofollow)?;
                    kwargs.set_item("autoescape", o.autoescape)?;
                    let w = if o.safe_input {
                        mark_safe_str(py, word)?
                    } else {
                        PyString::new(py, word).into_any()
                    };
                    let r = st
                        .instance
                        .bind(py)
                        .call((w,), Some(&kwargs))?
                        .str()?
                        .to_str()?
                        .to_string();
                    out.push_str(&r);
                    memo.push((word.to_string(), r));
                }
            }
        }
        rest = &rest[wend..];
        // separator run
        let send = rest.find(|c| !is_sep(c)).unwrap_or(rest.len());
        push_plain(&mut out, &rest[..send], o);
        rest = &rest[send..];
    }
    Ok(out)
}

fn urlize_common<'py>(
    s: Bound<'py, PyString>,
    limit_obj: Option<Bound<'py, PyAny>>,
    ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = s.py();
    let autoescape = ae.unwrap_or(true);
    let st = state(py)?;
    let limit = match &limit_obj {
        None => Some(None),
        Some(l) => l.extract::<i64>().ok().map(Some),
    };
    if let (Some(branch), Some(limit), Ok(text)) = (st.branch, limit, s.to_str()) {
        if pristine(py, st)? {
            let assume_https = match branch {
                Branch::V61 => super::setting(py, "URLIZE_ASSUME_HTTPS")
                    .and_then(|v| v.is_truthy().ok()),
                Branch::V52 => None,
            };
            let o = Opts {
                branch,
                limit,
                nofollow: true,
                autoescape,
                safe_input: is_safe_data(&s)?,
                max_url_length: st.max_url_length,
                assume_https,
            };
            let none = py.None().into_bound(py);
            let out = urlize_native(py, st, text, &o, limit_obj.as_ref().unwrap_or(&none))?;
            return mark_safe_str(py, &out);
        }
    }
    let kwargs = PyDict::new(py);
    if let Some(l) = limit_obj {
        kwargs.set_item("trim_url_limit", l)?;
    }
    kwargs.set_item("nofollow", true)?;
    kwargs.set_item("autoescape", autoescape)?;
    let out = utils(py)?.urlize.bind(py).call((s,), Some(&kwargs))?;
    mark_safe_bound(&out)
}

/// `urlize`: `mark_safe(_urlize(value, nofollow=True, autoescape=ae))`.
pub(crate) fn urlize<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    urlize_common(input.str()?, None, ae)
}

/// `urlizetrunc(limit)`: like `urlize` with `trim_url_limit=int(limit)`.
pub(crate) fn urlizetrunc<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    // stock: `value` is str-coerced (stringfilter) before `int(limit)`.
    let s = input.str()?;
    let limit = helpers(py)?.int_builtin.bind(py).call1((&args[0],))?;
    urlize_common(s, Some(limit), ae)
}
