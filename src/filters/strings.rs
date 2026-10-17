//! String filters of `django.template.defaultfilters` (native ports).

use pyo3::prelude::*;
use pyo3::types::{PySlice, PyString};

use crate::render::context::Rctx;
use crate::render::value::{helpers, is_safe_data, mark_safe_bound};

use std::cell::RefCell;

use unicode_normalization::UnicodeNormalization;

use super::{is_err, trans_state, utils, TransState};

fn int_of<'py>(py: Python<'py>, v: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    helpers(py)?.int_builtin.bind(py).call1((v,))
}

/// `addslashes`: `value.replace("\\", "\\\\").replace('"', '\\"').replace("'", "\\'")`
pub(crate) fn addslashes<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let s = input.str()?;
    s.call_method1("replace", ("\\", "\\\\"))?
        .call_method1("replace", ("\"", "\\\""))?
        .call_method1("replace", ("'", "\\'"))
}

/// `capfirst`: `value and value[0].upper() + value[1:]`
pub(crate) fn capfirst<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let s = input.str()?;
    if s.len()? == 0 {
        return Ok(s.into_any());
    }
    // ASCII first char: Python `str.upper()` on a single ASCII char is
    // exactly `to_ascii_uppercase`, so build the whole result natively.
    // (Surrogate strings: `to_str` fails → stock path below.)
    if let Ok(st) = s.to_str() {
        let b = st.as_bytes()[0];
        if b < 128 {
            return Ok(
                PyString::new(py, &format!("{}{}", (b.to_ascii_uppercase() as char), &st[1..]))
                    .into_any(),
            );
        }
    }
    let first = s.get_item(0)?.call_method0("upper")?;
    let rest = s.get_item(PySlice::new(py, 1, isize::MAX, 1))?;
    first.add(&rest)
}

/// `center`: 6.1 `f"{value:^{int(arg)}}"` (with width ≤ 0 short-circuit),
/// 5.2 `value.center(int(arg))` — `str.center` is byte-identical to `^`
/// formatting (extra pad on the LEFT), so one call covers both branches.
pub(crate) fn center<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let s = input.str()?;
    let width = int_of(py, &args[0])?;
    if utils(py)?.center_via_format {
        if width.le(0_i64)? {
            return Ok(s.into_any());
        }
        let spec = PyString::new(py, &format!("^{width}"));
        py.eval(c"format", None, None)?.call((&s, spec), None)
    } else {
        s.call_method1("center", (width,))
    }
}

/// `cut` (`register.filter` no flags → outer generic mark inert):
/// `safe = isinstance(value, SafeData); value = value.replace(arg, "");`
/// `if safe and arg != ";": return mark_safe(value); return value`
pub(crate) fn cut<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let s = input.str()?;
    let safe = is_safe_data(&s)?;
    let v = s.call_method1("replace", (&args[0], ""))?;
    if safe && !args[0].eq(";")? {
        return mark_safe_bound(&v);
    }
    Ok(v)
}

/// `stringformat`: `("%" + str(arg)) % value`, `ValueError`/`TypeError` → `""`.
pub(crate) fn stringformat<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let py_e: PyResult<Bound<'py, PyAny>> = (|| {
        let value = if input.is_instance_of::<pyo3::types::PyTuple>() {
            input.str()?.into_any()
        } else {
            input.clone()
        };
        let spec = PyString::new(py, &format!("%{}", args[0].str()?));
        spec.call_method1("__mod__", (value,))
    })();
    match py_e {
        Ok(v) => Ok(v),
        Err(e)
            if is_err::<pyo3::exceptions::PyValueError>(py, &e)
                || is_err::<pyo3::exceptions::PyTypeError>(py, &e) =>
        {
            Ok(PyString::new(py, "").into_any())
        }
        Err(e) => Err(e),
    }
}

/// `slugify`: `_slugify(value)`.
pub(crate) fn slugify<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let s = input.str()?;
    utils(py)?.slugify.bind(py).call1((s,))
}

/// `title`: `re.sub("([a-z])'([A-Z])", lower, re.sub(r"\d([A-Z])", lower, value.title()))`
pub(crate) fn title<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    use regex::Captures;
    let py = input.py();
    let u = utils(py)?;
    let t = input.str()?.call_method0("title")?;
    let t = t.to_string();
    let o1 = u
        .title_lower_re
        .replace_all(&t, |c: &Captures| c[0].to_lowercase());
    let o2 = u
        .title_digit_re
        .replace_all(&o1, |c: &Captures| c[0].to_lowercase());
    Ok(PyString::new(py, &o2).into_any())
}

/// Stock `add_truncation_text(text, truncate)` for an already-resolved
/// `truncate` string.
fn add_truncation_text(text: &str, truncate: &str) -> String {
    const KEY: &str = "%(truncated_text)s";
    if let Some(pos) = truncate.find(KEY) {
        // `truncate % {"truncated_text": text}`: the named key is
        // substituted, a stray `%` elsewhere raises in Python — our
        // translations never contain one, but callers only pass the
        // resolved pgettext result (the filter never passes a custom
        // truncate string), so native replace is exact.
        let mut out = String::with_capacity(text.len() + truncate.len());
        out.push_str(&truncate[..pos]);
        out.push_str(text);
        out.push_str(&truncate[pos + KEY.len()..]);
        return out;
    }
    if text.ends_with(truncate) {
        return text.to_string();
    }
    format!("{text}{truncate}")
}

/// Translated ellipsis `pgettext("String to return when truncating text",
/// "%(truncated_text)s…")`, cached per `(language, USE_I18N)` snapshot; a
/// `None` snapshot (settings lookup failed — stock raises there too) never
/// hits the cache.
struct TruncCache {
    /// Translated ellipsis format plus the non-combining width of
    /// `add_truncation_text("", format)` (placeholder substituted with the
    /// empty text), per `calculate_truncate_chars_length`.
    ellipsis: Option<(String, i64)>,
    state: Option<TransState>,
}

thread_local! {
    static TRUNC: RefCell<TruncCache> = const {
        RefCell::new(TruncCache { ellipsis: None, state: None })
    };
}

fn translated_ellipsis(py: Python<'_>) -> PyResult<Option<(String, i64)>> {
    let state = trans_state(py);
    let hit = TRUNC.with(|c| {
        let c = c.borrow();
        match (&c.ellipsis, c.state.as_ref()) {
            (Some(s), Some(prev)) if Some(prev) == state.as_ref() => Some(s.clone()),
            _ => None,
        }
    });
    if let Some(v) = hit {
        return Ok(Some(v));
    }
    let format = utils(py)?
        .pgettext
        .bind(py)
        .call1(("String to return when truncating text", "%(truncated_text)s…"))?
        .extract::<String>()?;
    let width = add_truncation_text("", &format)
        .chars()
        .filter(|c| unicode_normalization::char::canonical_combining_class(*c) == 0)
        .count();
    let v = (format, i64::try_from(width).unwrap_or(i64::MAX));
    TRUNC.with(|c| {
        let mut c = c.borrow_mut();
        c.ellipsis = Some(v.clone());
        c.state = state;
    });
    Ok(Some(v))
}

fn trunc_python<'py>(
    py: Python<'py>,
    s: &Bound<'py, PyString>,
    length: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let trunc = utils(py)?.truncator.bind(py).call1((s,))?;
    trunc.call_method1("chars", (length,))
}

/// Port of `Truncator.chars(length)` + `_text_chars` (html=False). Exact-
/// `str` inputs with valid UTF-8 only; everything else keeps the Python
/// `Truncator`. The translated ellipsis (and its cache-state probe) is only
/// resolved when truncation actually fires: pass 1 just counts
/// non-combining chars, pass 2 recomputes the cut offset with the width.
pub(crate) fn truncatechars<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let s = input.str()?;
    let length = match int_of(py, &args[0]) {
        Ok(v) => v,
        Err(e) if is_err::<pyo3::exceptions::PyValueError>(py, &e) => return Ok(s.into_any()),
        Err(e) => return Err(e),
    };
    let Ok(length_i) = length.extract::<i64>() else {
        // Huge ints still work in Python; keep the stock path.
        return trunc_python(py, &s, &length);
    };
    if length_i <= 0 {
        return Ok(PyString::new(py, "").into_any());
    }
    // Non-exact strings keep the Python `Truncator` (stock `Truncator.__init__`
    // runs `str(text)` again and attribute shadowing stays stock).
    if !input.is_exact_instance_of::<PyString>() {
        return trunc_python(py, &s, &length);
    }
    let Ok(raw) = s.to_str() else {
        return trunc_python(py, &s, &length);
    };
    let normed;
    let text = if unicode_normalization::is_nfc_quick(raw.chars())
        == unicode_normalization::IsNormalized::Yes
    {
        raw
    } else {
        normed = raw.nfc().collect::<String>();
        normed.as_str()
    };
    // Pass 1: does it truncate at all (non-combining count > length)?
    let mut count: i64 = 0;
    let mut truncates = false;
    for c in text.chars() {
        if unicode_normalization::char::canonical_combining_class(c) != 0 {
            continue;
        }
        count += 1;
        if count > length_i {
            truncates = true;
            break;
        }
    }
    if !truncates {
        return Ok(PyString::new(py, text).into_any());
    }
    // Pass 2: resolve the ellipsis and locate the cut offset (stock never
    // cuts mid combining sequence).
    let Some((ellipsis, width)) = translated_ellipsis(py)? else {
        return trunc_python(py, &s, &length);
    };
    let truncate_len = (length_i - width).max(0);
    let mut s_len: i64 = 0;
    let mut end_index: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if unicode_normalization::char::canonical_combining_class(c) != 0 {
            continue;
        }
        s_len += 1;
        if end_index.is_none() && s_len > truncate_len {
            end_index = Some(i);
        }
        if s_len > length_i {
            let cut = text.get(..end_index.unwrap_or(0)).unwrap_or("");
            let out = add_truncation_text(cut, &ellipsis);
            return Ok(PyString::new(py, &out).into_any());
        }
    }
    // Defensive: pass 1 proved truncation, so the loop always returns.
    Ok(PyString::new(py, text).into_any())
}

/// `truncatewords`: `Truncator(value).words(length, truncate=" …")`.
pub(crate) fn truncatewords<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let s = input.str()?;
    let length = match int_of(py, &args[0]) {
        Ok(v) => v,
        Err(e) if is_err::<pyo3::exceptions::PyValueError>(py, &e) => return Ok(s.into_any()),
        Err(e) => return Err(e),
    };
    let kwargs = pyo3::types::PyDict::new(py);
    kwargs.set_item("truncate", " …")?;
    let trunc = utils(py)?.truncator.bind(py).call1((s,))?;
    trunc.call_method("words", (length,), Some(&kwargs))
}

/// `upper`: `value.upper()`.
pub(crate) fn upper<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    input.str()?.call_method0("upper")
}

/// `lower`: `value.lower()`.
pub(crate) fn lower<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    input.str()?.call_method0("lower")
}

/// `wordcount`: `len(value.split())`.
pub(crate) fn wordcount<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let words = input.str()?.call_method0("split")?;
    let n: i64 = words.len()?.try_into()?;
    Ok(n.into_pyobject(input.py())?.into_any())
}

/// `wordwrap`: `wrap(value, int(arg))`.
pub(crate) fn wordwrap<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let s = input.str()?;
    let width = int_of(py, &args[0])?;
    utils(py)?.wrap.bind(py).call1((s, width))
}

/// `ljust`: `value.ljust(int(arg))`.
pub(crate) fn ljust<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let s = input.str()?;
    let w = int_of(py, &args[0])?;
    s.call_method1("ljust", (w,))
}

/// `rjust`: `value.rjust(int(arg))`.
pub(crate) fn rjust<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let s = input.str()?;
    let w = int_of(py, &args[0])?;
    s.call_method1("rjust", (w,))
}
