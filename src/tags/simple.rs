//! Render ports of the "simple" built-in tags from
//! `django/template/defaulttags.py`: `CsrfTokenNode`, `DebugNode`, `NowNode`,
//! `LoremNode`, `SpacelessNode`, `TemplateTagNode`, `FirstOfNode`,
//! `WidthRatioNode`, `URLNode`, `RegroupNode`.

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::exceptions::PyAttributeError;
use pyo3::types::{PyBool, PyDict, PyInt, PyList, PyString, PyTuple};

use std::cell::RefCell;
use std::collections::HashMap;

use super::{annotate, context_get, expr_res, expr_res_ign, fns, py_str_of, TagPayload};
use crate::error::syntax_error;
use crate::filters::local_cvar;
use crate::render::context::Rctx;
use crate::render::value::{self, helpers};

// --- python bits cached per process -----------------------------------------

struct Bits {
    timezone: Py<PyAny>,
    datetime: Py<PyAny>,
    float: Py<PyAny>,
    round: Py<PyAny>,
    reverse: Py<PyAny>,
    get_resolver: Py<PyAny>,
    urlconf_cvar: Option<Py<PyAny>>,
    prefix_cvar: Option<Py<PyAny>>,
    lang_cvar: Option<Py<PyAny>>,
    thread_ident: Py<PyAny>,
    settings: Py<PyAny>,
    safe_string: Py<PyAny>,
    no_reverse_match: Py<PyAny>,
    grouped_result: Py<PyAny>,
    pformat: Py<PyAny>,
    sys_modules: Py<PyAny>,
}

static BITS: PyOnceLock<Bits> = PyOnceLock::new();

fn bits(py: Python<'_>) -> PyResult<&'static Bits> {
    BITS.get_or_try_init(py, || -> PyResult<Bits> {
        Ok(Bits {
            timezone: py.import("django.utils.timezone")?.unbind().into(),
            datetime: py.import("datetime")?.getattr("datetime")?.unbind(),
            float: py.import("builtins")?.getattr("float")?.unbind(),
            round: py.import("builtins")?.getattr("round")?.unbind(),
            reverse: py.import("django.urls")?.getattr("reverse")?.unbind(),
            get_resolver: py.import("django.urls")?.getattr("get_resolver")?.unbind(),
            urlconf_cvar: local_cvar(py, "django.urls.base", "_urlconfs"),
            prefix_cvar: local_cvar(py, "django.urls.base", "_prefixes"),
            lang_cvar: local_cvar(py, "django.utils.translation.trans_real", "_active"),
            thread_ident: py.import("threading")?.getattr("get_ident")?.unbind(),
            settings: py.import("django.conf")?.getattr("settings")?.unbind(),
            safe_string: py
                .import("django.utils.safestring")?
                .getattr("SafeString")?
                .unbind(),
            no_reverse_match: py
                .import("django.urls")?
                .getattr("NoReverseMatch")?
                .unbind(),
            grouped_result: py
                .import("collections")?
                .getattr("namedtuple")?
                .call(("GroupedResult", ["grouper", "list"]), None)?
                .unbind(),
            pformat: py.import("pprint")?.getattr("pformat")?.unbind(),
            sys_modules: py.import("sys")?.getattr("modules")?.unbind(),
        })
    })
}

// ---------------------------------------------------------------------------

/// `CsrfTokenNode.render`.
pub(crate) fn csrf(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::Csrf { token } = p else {
        return Ok(());
    };
    let res = do_csrf(rctx);
    match res {
        Ok(s) => {
            out.push_str(&s);
            Ok(())
        }
        Err(e) => {
            annotate(rctx, &e, token);
            Err(e)
        }
    }
}

fn do_csrf(rctx: &mut Rctx<'_>) -> PyResult<String> {
    let py = rctx.py();
    let f = fns(py)?;
    let csrf_token = context_get(rctx, "csrf_token")?;
    if let Some(t) = csrf_token {
        // Common case natively: an exact `str`/`SafeString` token other
        // than the sentinel — `format_html` is `conditional_escape` + plain
        // `str.format` substitution (`format(str, "")` is the identity).
        if let Ok(s) = t.cast::<PyString>() {
            let exact = t.is_exact_instance_of::<PyString>()
                || t.get_type().as_ptr() == helpers(py)?.safe_string.bind(py).as_ptr();
            if exact {
                if let Ok(rs) = s.to_str() {
                    if !rs.is_empty() && rs != "NOTPROVIDED" {
                        let esc = value::conditional_escape_str(&t)?;
                        return Ok(format!(
                            r#"<input type="hidden" name="csrfmiddlewaretoken" value="{esc}">"#
                        ));
                    }
                }
            }
        }
        if t.is_truthy()? {
            if t.eq(PyString::new(py, "NOTPROVIDED"))? {
                return py_str_of(&f.format_html.bind(py).call0()?);
            }
            let v = f.format_html.bind(py).call1((
                PyString::new(
                    py,
                    r#"<input type="hidden" name="csrfmiddlewaretoken" value="{}">"#,
                ),
                &t,
            ))?;
            return py_str_of(&v);
        }
    }
    if f.settings.bind(py).getattr("DEBUG")?.is_truthy()? {
        let ver61 = rctx
            .engine
            .getattr("ver")
            .map(|v| v.eq("6.1").unwrap_or(false))
            .unwrap_or(false);
        let msg = if ver61 {
            "A {% csrf_token %} was used in a template, but the context did not provide the value. This is usually caused by not using RequestContext."
        } else {
            "A {% csrf_token %} was used in a template, but the context did not provide the value.  This is usually caused by not using RequestContext."
        };
        let _ = f.warnings.bind(py).call_method1("warn", (msg,));
    }
    Ok(String::new())
}

/// `DebugNode.render`.
pub(crate) fn debug(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::Debug { token } = p else {
        return Ok(());
    };
    let res = do_debug(rctx);
    match res {
        Ok(s) => {
            out.push_str(&s);
            Ok(())
        }
        Err(e) => {
            annotate(rctx, &e, token);
            Err(e)
        }
    }
}

fn do_debug(rctx: &Rctx<'_>) -> PyResult<String> {
    let py = rctx.py();
    let f = fns(py)?;
    if !f.settings.bind(py).getattr("DEBUG")?.is_truthy()? {
        return Ok(String::new());
    }
    let b = bits(py)?;
    let mut out = String::new();
    // `for val in context` == reversed(dicts), builtins layer included.
    for d in rctx.dicts().iter().rev() {
        let pf = b.pformat.bind(py).call1((d,))?;
        let esc = f.escape.bind(py).call1((pf,))?;
        out.push_str(&py_str_of(&esc)?);
    }
    out.push_str("\n\n");
    let pf = b.pformat.bind(py).call1((b.sys_modules.bind(py),))?;
    let esc = f.escape.bind(py).call1((pf,))?;
    out.push_str(&py_str_of(&esc)?);
    Ok(out)
}

/// `NowNode.render`.
pub(crate) fn now(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::Now {
        fmt, asvar, token, ..
    } = p
    else {
        return Ok(());
    };
    let res = (|| -> PyResult<String> {
        let py = rctx.py();
        let b = bits(py)?;
        let f = fns(py)?;
        let tz = if f.settings.bind(py).getattr("USE_TZ")?.is_truthy()? {
            Some(b.timezone.bind(py).call_method0("get_current_timezone")?)
        } else {
            None
        };
        let kwargs = PyDict::new(py);
        kwargs.set_item("tz", tz)?;
        let dt = b.datetime.bind(py).call_method("now", (), Some(&kwargs))?;
        let formatted =
            crate::filters::dates::date(rctx, &dt, &[PyString::new(py, fmt).into_any()], None)?;
        if let Some(name) = asvar {
            rctx.set_item(name, &formatted)?;
            Ok(String::new())
        } else {
            py_str_of(&formatted)
        }
    })();
    match res {
        Ok(s) => {
            out.push_str(&s);
            Ok(())
        }
        Err(e) => {
            annotate(rctx, &e, token);
            Err(e)
        }
    }
}

/// `LoremNode.render`.
pub(crate) fn lorem(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::Lorem {
        count,
        method,
        common,
        token,
    } = p
    else {
        return Ok(());
    };
    let res = (|| -> PyResult<String> {
        let py = rctx.py();
        // module attributes resolved per render (mock-patching friendly).
        let lib = py.import("django.utils.lorem_ipsum")?;
        let cnt = expr_res(rctx, count)
            .and_then(|v| helpers(py)?.int_builtin.bind(py).call1((v,)));
        let count_obj = match cnt {
            Ok(c) => c,
            Err(e)
                if e.is_instance_of::<pyo3::exceptions::PyValueError>(py)
                    || e.is_instance_of::<pyo3::exceptions::PyTypeError>(py) =>
            {
                PyInt::new(py, 1).into_any().clone()
            }
            Err(e) => return Err(e),
        };
        let kwargs = PyDict::new(py);
        kwargs.set_item("common", PyBool::new(py, *common))?;
        if method == "w" {
            let v = lib.getattr("words")?.call((count_obj,), Some(&kwargs))?;
            return py_str_of(&v);
        }
        let paras = lib.getattr("paragraphs")?.call((count_obj,), Some(&kwargs))?;
        let mut items: Vec<String> = Vec::new();
        for item in paras.try_iter()? {
            let s = py_str_of(&item?)?;
            if method == "p" {
                items.push(format!("<p>{s}</p>"));
            } else {
                items.push(s);
            }
        }
        Ok(items.join("\n\n"))
    })();
    match res {
        Ok(s) => {
            out.push_str(&s);
            Ok(())
        }
        Err(e) => {
            annotate(rctx, &e, token);
            Err(e)
        }
    }
}

/// Whitespace test matching Python's `re` `\s` for `str` patterns (i.e.
/// `str.isspace()`): Unicode `White_Space` plus the legacy C0 separators
/// U+001C..U+001F, which Python counts but `char::is_whitespace` does not.
pub(crate) fn py_ws(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `django.utils.html.strip_spaces_between_tags` = `re.sub(r">\s+<", "><", s)`.
/// Greedy `\s+` never needs backtracking (`<` is not whitespace) and matches
/// cannot overlap, so one linear scan is exact.
pub(crate) fn strip_spaces_between_tags(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '>' {
            let mut j = i + 1;
            while j < chars.len() && py_ws(chars[j]) {
                j += 1;
            }
            if j > i + 1 && j < chars.len() && chars[j] == '<' {
                out.push_str("><");
                i = j + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// `SpacelessNode.render`.
pub(crate) fn spaceless(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::Spaceless { body, lit, token } = p else {
        return Ok(());
    };
    // Decide on first render (parse stays scan-only): a literal body has a
    // context-independent final string.
    if let Some(s) = lit.get_or_init(|| crate::tags::parse::literal_spaceless(body)) {
        out.push_str(s);
        return Ok(());
    }
    let res = (|| -> PyResult<String> {
        let mut buf = String::new();
        crate::nodes::render_list(rctx, body, &mut buf)?;
        Ok(strip_spaces_between_tags(buf.trim_matches(py_ws)))
    })();
    match res {
        Ok(s) => {
            out.push_str(&s);
            Ok(())
        }
        Err(e) => {
            annotate(rctx, &e, token);
            Err(e)
        }
    }
}

/// `TemplateTagNode.render`.
pub(crate) fn templatetag(_rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) {
    let TagPayload::Templatetag { arg, .. } = p else {
        return;
    };
    out.push_str(match arg.as_str() {
        "openblock" => "{%",
        "closeblock" => "%}",
        "openvariable" => "{{",
        "closevariable" => "}}",
        "openbrace" => "{",
        "closebrace" => "}",
        "opencomment" => "{#",
        "closecomment" => "#}",
        _ => "",
    });
}

/// `FirstOfNode.render`.
pub(crate) fn first_of(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::FirstOf {
        vars, asvar, token, ..
    } = p
    else {
        return Ok(());
    };
    let res = (|| -> PyResult<String> {
        let mut first = String::new();
        for v in vars {
            let value = expr_res_ign(rctx, v, true)?;
            if value.is_truthy()? {
                first = value::render_value_in_context(rctx, &value)?;
                break;
            }
        }
        if let Some(name) = asvar {
            rctx.set_item(name, &PyString::new(rctx.py(), &first).into_any())?;
            Ok(String::new())
        } else {
            Ok(first)
        }
    })();
    match res {
        Ok(s) => {
            out.push_str(&s);
            Ok(())
        }
        Err(e) => {
            annotate(rctx, &e, token);
            Err(e)
        }
    }
}

/// `WidthRatioNode.render`.
#[allow(clippy::many_single_char_names)] // h/b mirror the stock helper pairs
pub(crate) fn width_ratio(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::WidthRatio {
        val,
        max,
        width,
        asvar,
        token,
    } = p
    else {
        return Ok(());
    };
    let res = (|| -> PyResult<String> {
        let py = rctx.py();
        let h = helpers(py)?;
        let b = bits(py)?;
        let resolved = (|| -> PyResult<(Bound<'_, PyAny>, Bound<'_, PyAny>, Bound<'_, PyAny>)> {
            let value = expr_res(rctx, val)?;
            let max_value = expr_res(rctx, max)?;
            let mw_obj = expr_res(rctx, width)?;
            let mw = h.int_builtin.bind(py).call1((mw_obj,))?;
            Ok((value, max_value, mw))
        })();
        let (value, max_value, max_width) = match resolved {
            Ok(t) => t,
            Err(e) if value::is_var_does_not_exist(py, &e) => return Ok(String::new()),
            Err(e)
                if e.is_instance_of::<pyo3::exceptions::PyValueError>(py)
                    || e.is_instance_of::<pyo3::exceptions::PyTypeError>(py) =>
            {
                return Err(syntax_error(py, "widthratio final argument must be a number"));
            }
            Err(e) => return Err(e),
        };
        // Exact-number fast path over the resolved values: stock computes
        // `str(round(float(v) / float(m) * w))`; IEEE doubles and
        // round-half-to-even match bit-for-bit (Python `round()` on a
        // float with no digits is banker's rounding of the binary value,
        // and results below 2**53 are exact ints either way). m == 0 hits
        // stock's ZeroDivisionError branch (result 0) before anything
        // else. Bools/strings/Decimals/big ints, widths beyond i32, and
        // results >= 2**53 (incl. NaN/inf) fall through to Python.
        let mut result: Option<String> = None;
        if let (Ok(v), Ok(m), Ok(w)) = (
            value.extract::<f64>(),
            max_value.extract::<f64>(),
            max_width.extract::<i32>(),
        ) {
            if m == 0.0 {
                result = Some("0".to_string());
            } else {
                let r = (v / m * f64::from(w)).round_ties_even();
                if r.abs() < 9_007_199_254_740_992.0 {
                    #[allow(clippy::cast_possible_truncation)] // |r| < 2**53 above
                    {
                        result = Some((r as i64).to_string());
                    }
                }
            }
        }
        let result = match result {
            Some(s) => s,
            // float(value), float(max_value), (v/m)*mw, str(round(ratio)) —
            // in Python, so OverflowError / big ints behave.
            None => (|| -> PyResult<String> {
                let v = b.float.bind(py).call1((value,))?;
                let m = b.float.bind(py).call1((max_value,))?;
                let ratio = v.div(&m)?.mul(&max_width)?;
                py_str_of(&b.round.bind(py).call1((ratio,))?)
            })()
            .or_else(|e| {
                if e.is_instance_of::<pyo3::exceptions::PyZeroDivisionError>(py) {
                    Ok("0".to_string())
                } else if e.is_instance_of::<pyo3::exceptions::PyValueError>(py)
                    || e.is_instance_of::<pyo3::exceptions::PyTypeError>(py)
                    || e.is_instance_of::<pyo3::exceptions::PyOverflowError>(py)
                {
                    Ok(String::new())
                } else {
                    Err(e)
                }
            })?,
        };
        if let Some(name) = asvar {
            rctx.set_item(name, &PyString::new(py, &result).into_any())?;
            Ok(String::new())
        } else {
            Ok(result)
        }
    })();
    match res {
        Ok(s) => {
            out.push_str(&s);
            Ok(())
        }
        Err(e) => {
            annotate(rctx, &e, token);
            Err(e)
        }
    }
}

// --- `{% url %}` reverse memo ------------------------------------------------
//
// Stock `reverse(viewname, args=…, kwargs=…, current_app=…)` reduces to
// `resolver._reverse_with_prefix(viewname, prefix, *args)` whenever the
// view name is an exact `str` with no ':' — the namespace loop is then
// provably empty (`ns_pattern` stays ""), `current_app` is never read, and
// no `get_ns_resolver` call happens. That result is a pure function of
// (resolver object, script prefix, language, view string, args):
// - `set_urlconf`, a `ROOT_URLCONF` change and `clear_url_caches` all
//   rotate or drop the resolver *object* (`get_resolver` is an lru_cache
//   wrapper), so pointer identity is a sound staleness check — a strong ref
//   is kept so a compared pointer can never name a recycled object;
// - `activate`/`deactivate` change the language, which re-buckets
//   `reverse_dict` inside a live resolver, so language is compared by value;
// - `set_script_prefix` stores a per-thread prefix used verbatim as
//   `_prefix`, also compared by value.
// The env is read through the `asgiref` contextvars behind `get_urlconf` /
// `get_script_prefix` / `get_language` (their `Local.__getattr__` costs
// ~6 us per access) and compared against a memo entry; a miss pays one
// direct `_reverse_with_prefix` call, a hit pays a dict lookup. `reverse`
// also raises `ValueError` when args and kwargs are both non-empty; the
// fast path requires empty kwargs so that branch stays unreachable.

/// Memo key for one reverse arg: exact `int` (bounded) or exact `str`.
/// Other objects (bool, lazy str, model instances with `__str__`) fall
/// through to the stock call so their conversion semantics are unchanged.
#[derive(PartialEq, Eq, Hash)]
enum ArgKey {
    Int(i64),
    Str(String),
}

#[derive(PartialEq, Eq, Hash)]
struct ReverseKey {
    view: String,
    args: Vec<ArgKey>,
}

/// Environment inputs a cached URL was computed under. `resolver` is
/// compared by pointer identity (kept alive by this strong ref); the rest
/// by `==`.
struct ReverseEnv {
    resolver: Py<PyAny>,
    prefix: Py<PyString>,
    language: Py<PyAny>,
}

struct UrlMemo {
    env: ReverseEnv,
    map: HashMap<ReverseKey, String>,
}

const URL_MEMO_CAP: usize = 4096;

thread_local! {
    // Sentinel env that never matches a real resolver; replaced on first use.
    static URL_MEMO: RefCell<Option<UrlMemo>> = const { RefCell::new(None) };
}

/// Read `data["value"]` from an `asgiref.local._CVar` `ContextVar` (already
/// imported by `Bits`), replicating `_CVar._storage()`: storage tagged to
/// another thread was merely inherited and must stay invisible. `None`
/// means the value is unset (stock `getattr(..., default)` path).
fn cvar_value<'py>(
    py: Python<'_>,
    cvar: &Bound<'py, PyAny>,
    ident: &Bound<'py, PyAny>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    let storage = cvar.call_method1("get", (py.None(),))?;
    if storage.is_none() {
        return Ok(None);
    }
    let tid = match storage.getattr("thread_id") {
        Ok(t) => t,
        Err(e) if e.is_instance_of::<PyAttributeError>(py) => return Ok(None),
        Err(e) => return Err(e),
    };
    if !tid.eq(ident)? {
        return Ok(None);
    }
    let data = storage.getattr("data")?.cast_into::<PyDict>()?;
    data.get_item("value")
}

/// Exact `str` or exact `SafeString` (lexer output for literals; its
/// `__str__`/`__eq__`/`__hash__` are the content), so content-keyed memoing
/// matches stock's by-value `reverse_dict` lookup. Any other type bails.
fn str_like(py: Python<'_>, b: &Bits, obj: &Bound<'_, PyAny>) -> bool {
    let p = obj.get_type().as_ptr();
    p == py.get_type::<PyString>().as_ptr() || p == b.safe_string.bind(py).as_ptr()
}

fn reverse_fast<'py>(
    py: Python<'py>,
    b: &Bits,
    view_name: &Bound<'py, PyAny>,
    arg_objs: &[Bound<'py, PyAny>],
    // Bail unless the view is an exact `str` without ':' and every arg is
    // a plain int/str — see block comment.
) -> PyResult<Option<String>> {
    if !str_like(py, b, view_name) {
        return Ok(None);
    }
    let view = view_name.cast::<PyString>()?.to_str()?;
    if view.contains(':') {
        return Ok(None);
    }
    let mut keys: Vec<ArgKey> = Vec::with_capacity(arg_objs.len());
    for a in arg_objs {
        match arg_key(py, b, a)? {
            Some(k) => keys.push(k),
            None => return Ok(None),
        }
    }
    // Env: get_urlconf() → get_resolver(urlconf); prefix/language from the
    // contextvars, mirroring stock fallbacks. Any unexpected shape bails.
    let (Some(urlconf_cvar), Some(prefix_cvar), Some(lang_cvar)) = (
        b.urlconf_cvar.as_ref(),
        b.prefix_cvar.as_ref(),
        b.lang_cvar.as_ref(),
    ) else {
        return Ok(None);
    };
    let ident = b.thread_ident.bind(py).call0()?;
    let urlconf = cvar_value(py, urlconf_cvar.bind(py), &ident)?;
    let resolver = match urlconf.as_ref() {
        Some(u) => b.get_resolver.bind(py).call1((u,))?,
        None => b.get_resolver.bind(py).call1((py.None(),))?,
    };
    let prefix = match cvar_value(py, prefix_cvar.bind(py), &ident)? {
        Some(p) => p.cast_into::<PyString>()?,
        None => PyString::new(py, "/").into_any().cast_into::<PyString>()?,
    };
    let mut language = b.settings.bind(py).getattr("LANGUAGE_CODE")?;
    if let Some(active) = cvar_value(py, lang_cvar.bind(py), &ident)? {
        if let Ok(l) = active.call_method0("to_language") {
            language = l;
        }
    }
    let key = ReverseKey {
        view: view.into(),
        args: keys,
    };
    let same_env = URL_MEMO.with(|slot| -> PyResult<bool> {
        match slot.borrow().as_ref() {
            Some(m) => Ok(m.env.resolver.as_ptr() == resolver.as_ptr()
                && m.env.prefix.bind(py).as_any().eq(&prefix)?
                && m.env.language.bind(py).as_any().eq(&language)?),
            None => Ok(false),
        }
    })?;
    if same_env {
        if let Some(hit) =
            URL_MEMO.with(|slot| slot.borrow().as_ref().and_then(|m| m.map.get(&key).cloned()))
        {
            return Ok(Some(hit));
        }
    }
    let mut call_args: Vec<Bound<'py, PyAny>> = Vec::with_capacity(arg_objs.len() + 2);
    call_args.push(view_name.clone());
    call_args.push(prefix.as_any().clone());
    call_args.extend_from_slice(arg_objs);
    let args_tuple = PyTuple::new(py, call_args)?;
    let out_obj = resolver.call_method1("_reverse_with_prefix", &args_tuple)?;
    let url = py_str_of(&out_obj)?;
    URL_MEMO.with(|slot| {
        let mut memo = slot.borrow_mut();
        match memo.as_mut() {
            // Environment was verified equal above; same env keeps the map.
            // Otherwise install a fresh map for the new env (bounded churn:
            // an alternating env simply re-memoises).
            Some(m) if same_env => {
                if m.map.len() < URL_MEMO_CAP {
                    m.map.insert(key, url.clone());
                }
            }
            _ => {
                *memo = Some(UrlMemo {
                    env: ReverseEnv {
                        resolver: resolver.unbind(),
                        prefix: prefix.unbind(),
                        language: language.unbind(),
                    },
                    map: {
                        let mut m = HashMap::new();
                        m.insert(key, url.clone());
                        m
                    },
                });
            }
        }
        PyResult::Ok(())
    })?;
    Ok(Some(url))
}

/// Extract a memo key for one reverse arg: exact `int` (bounded) or a
/// `str_like` string. Anything else returns `None` so the caller takes
/// the stock path.
fn arg_key(py: Python<'_>, b: &Bits, obj: &Bound<'_, PyAny>) -> PyResult<Option<ArgKey>> {
    if obj.get_type().as_ptr() == py.get_type::<PyInt>().as_ptr() {
        return Ok(obj.extract::<i64>().ok().map(ArgKey::Int));
    }
    if str_like(py, b, obj) {
        let s: String = obj.cast::<PyString>()?.to_str()?.into();
        return Ok(Some(ArgKey::Str(s)));
    }
    Ok(None)
}

/// `URLNode.render`. The memo only engages for exact-`str` view names
/// without ':' and empty kwargs; every other shape runs the stock path
/// unchanged.
pub(crate) fn url(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::Url {
        view,
        args,
        kwargs,
        asvar,
        token,
    } = p
    else {
        return Ok(());
    };
    let res = (|| -> PyResult<String> {
        let py = rctx.py();
        let b = bits(py)?;
        let mut arg_objs = Vec::with_capacity(args.len());
        for a in args {
            arg_objs.push(expr_res(rctx, a)?);
        }
        let kw = PyDict::new(py);
        for (k, v) in kwargs {
            kw.set_item(k, expr_res(rctx, v)?)?;
        }
        let view_name = expr_res(rctx, view)?;
        // Memo fast path; `None` means the shape is not provably
        // equivalent and the stock call below runs unchanged.
        let mut fast = if kwargs.is_empty() {
            match reverse_fast(py, b, &view_name, &arg_objs) {
                Ok(u) => u,
                Err(e) => {
                    if e.is_instance(py, b.no_reverse_match.bind(py)) && asvar.is_some() {
                        Some(String::new())
                    } else {
                        return Err(e);
                    }
                }
            }
        } else {
            None
        };
        if fast.is_none() {
            // Stock: `context.request` (the RequestContext attribute; plain
            // Context raises AttributeError → None), then current_app →
            // resolver_match.namespace → None.
            let mut current_app: Option<Bound<'_, PyAny>> = None;
            if let Ok(request) = rctx.ctx.getattr("request") {
                if let Ok(ca) = request.getattr("current_app") {
                    current_app = Some(ca);
                } else if let Ok(ns) = request
                    .getattr("resolver_match")
                    .and_then(|rm| rm.getattr("namespace"))
                {
                    current_app = Some(ns);
                }
            }
            let call_kwargs = PyDict::new(py);
            call_kwargs.set_item("args", PyList::new(py, &arg_objs)?)?;
            call_kwargs.set_item("kwargs", kw)?;
            call_kwargs.set_item("current_app", current_app)?;
            match b.reverse.bind(py).call((view_name,), Some(&call_kwargs)) {
                Ok(v) => fast = Some(py_str_of(&v)?),
                Err(e) => {
                    if !e.is_instance(py, b.no_reverse_match.bind(py)) {
                        return Err(e);
                    }
                    if asvar.is_none() {
                        return Err(e);
                    }
                }
            }
        }
        let url = fast.unwrap_or_default();
        if let Some(name) = asvar {
            rctx.set_item(name, &PyString::new(py, &url).into_any())?;
            Ok(String::new())
        } else if rctx.autoescape()? {
            let h = value::helpers(py)?;
            let v = h.conditional_escape.bind(py).call1((&url,))?;
            py_str_of(&v)
        } else {
            Ok(url)
        }
    })();
    match res {
        Ok(s) => {
            out.push_str(&s);
            Ok(())
        }
        Err(e) => {
            annotate(rctx, &e, token);
            Err(e)
        }
    }
}

/// `RegroupNode.render`.
pub(crate) fn regroup(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::Regroup {
        target,
        expr,
        var_name,
        token,
    } = p
    else {
        return Ok(());
    };
    let res = (|| -> PyResult<String> {
        let py = rctx.py();
        let b = bits(py)?;
        let obj_list = expr_res_ign(rctx, target, true)?;
        if obj_list.is_none() {
            rctx.set_item(var_name, &PyList::empty(py).into_any())?;
            return Ok(String::new());
        }
        // groupby with key=lambda obj: context[var]=obj; expr.resolve.
        let results = PyList::empty(py);
        let mut cur_key: Option<Bound<'_, PyAny>> = None;
        let mut cur_list: Vec<Bound<'_, PyAny>> = Vec::new();
        let flush = |cur_key: &mut Option<Bound<'_, PyAny>>,
                     cur_list: &mut Vec<Bound<'_, PyAny>>|
         -> PyResult<()> {
            if let Some(k) = cur_key.take() {
                let kwargs = PyDict::new(py);
                kwargs.set_item("grouper", k)?;
                kwargs.set_item("list", PyList::new(py, std::mem::take(cur_list))?)?;
                results.append(b.grouped_result.bind(py).call((), Some(&kwargs))?)?;
            }
            Ok(())
        };
        for item in obj_list.try_iter()? {
            let item = item?;
            rctx.set_item(var_name, &item)?;
            let key = expr_res_ign(rctx, expr, true)?;
            match &cur_key {
                Some(k) if k.eq(&key)? => cur_list.push(item),
                _ => {
                    flush(&mut cur_key, &mut cur_list)?;
                    cur_key = Some(key);
                    cur_list.push(item);
                }
            }
        }
        flush(&mut cur_key, &mut cur_list)?;
        rctx.set_item(var_name, &results)?;
        Ok(String::new())
    })();
    match res {
        Ok(s) => {
            out.push_str(&s);
            Ok(())
        }
        Err(e) => {
            annotate(rctx, &e, token);
            Err(e)
        }
    }
}
