//! List filters of `django.template.defaultfilters` (native ports).

use pyo3::prelude::*;
use pyo3::types::{PyInt, PyList, PyString};

use crate::render::context::Rctx;
use crate::render::value::{conditional_escape_str, escape_html, helpers, mark_safe_str};

use super::{is_err, is_ie, is_te, is_ve, utils};

fn empty_str(py: Python<'_>) -> Bound<'_, PyAny> {
    PyString::new(py, "").into_any()
}

/// `first`: `try: value[0] except IndexError: ""` (other errors propagate).
pub(crate) fn first<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    match input.get_item(0_i64) {
        Ok(v) => Ok(v),
        Err(e) if is_ie(input.py(), &e) => Ok(empty_str(input.py())),
        Err(e) => Err(e),
    }
}

/// `last`: `try: value[-1] except IndexError: ""`.
pub(crate) fn last<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    match input.get_item(-1_i64) {
        Ok(v) => Ok(v),
        Err(e) if is_ie(input.py(), &e) => Ok(empty_str(input.py())),
        Err(e) => Err(e),
    }
}

/// `length`: `try: len(value) except (ValueError, TypeError): 0`.
pub(crate) fn length<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    match input.len() {
        Ok(n) => Ok(n.into_pyobject(py)?.into_any()),
        Err(e) if is_ve(py, &e) || is_te(py, &e) => Ok(0_i64.into_pyobject(py)?.into_any()),
        Err(e) => Err(e),
    }
}

/// `random`: `random_module.choice(value)`, `IndexError` → `""`.
pub(crate) fn random<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    match utils(py)?.choice.bind(py).call1((input,)) {
        Ok(v) => Ok(v),
        Err(e) if is_ie(py, &e) => Ok(empty_str(py)),
        Err(e) => Err(e),
    }
}

/// `make_list`: `list(value)` on the stringified input — a `PyList` of
/// 1-char strings.
pub(crate) fn make_list<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let s = input.str()?;
    let chars: Vec<Bound<'py, PyString>> = s
        .to_string()
        .chars()
        .map(|c| PyString::new(py, c.encode_utf8(&mut [0u8; 4])))
        .collect();
    Ok(PyList::new(py, chars)?.into_any())
}

/// `slice`: parse `start:stop:step` (`int()` each part, empty → `None`),
/// then slice the value; any `ValueError`/`TypeError`/`KeyError` → the
/// value unchanged (stock catches all three around parse AND slice).
pub(crate) fn slice_filter<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let h = helpers(py)?;
    let u = utils(py)?;
    let plan: PyResult<Bound<'py, PyAny>> = (|| {
        let argstr = args[0].str()?;
        let parts: Vec<String> = argstr
            .to_string_lossy()
            .split(':')
            .map(str::to_string)
            .collect();
        if parts.len() > 3 {
            return Err(pyo3::exceptions::PyTypeError::new_err(
                "slice() takes at most 3 arguments",
            ));
        }
        let mut vals: Vec<Bound<'py, PyAny>> = Vec::with_capacity(3);
        for p in parts {
            if p.is_empty() {
                vals.push(py.None().into_bound(py));
            } else {
                vals.push(h.int_builtin.bind(py).call1((p,))?);
            }
        }
        while vals.len() < 3 {
            vals.push(py.None().into_bound(py));
        }
        u.slice_builtin
            .bind(py)
            .call1((&vals[0], &vals[1], &vals[2]))
    })();
    match plan.and_then(|sl| input.get_item(sl)) {
        Ok(v) => Ok(v),
        Err(e)
            if is_ve(py, &e) || is_te(py, &e) || is_err::<pyo3::exceptions::PyKeyError>(py, &e) =>
        {
            Ok(input.clone())
        }
        Err(e) => Err(e),
    }
}

/// `join`: under autoescape
/// `conditional_escape(arg).join([conditional_escape(v) …])`, else
/// `arg.join(value)`; `TypeError` → value; success is always `mark_safe`.
///
/// With a `str` separator the escaped join is assembled natively: exact
/// `str` items take the native `escape`, `SafeString` items pass through,
/// exact `int`s are escape-inert; anything else runs the real
/// `conditional_escape`. A non-`str` escape result makes stock's
/// `str.join` raise `TypeError` (after every item was escaped) → value.
pub(crate) fn join<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let autoescape = ae.unwrap_or(true);
    let arg = &args[0];
    let h = helpers(py)?;
    let safe_string = h.safe_string.bind(py);
    let str_arg = arg.is_exact_instance_of::<PyString>() || arg.get_type().is(safe_string);
    if autoescape && str_arg {
        let res = (|| -> PyResult<Option<String>> {
            let sep = conditional_escape_str(arg)?;
            let ce = h.conditional_escape.bind(py);
            let mut out = String::new();
            let mut first = true;
            let mut bad = false;
            for item in input.try_iter()? {
                let v = item?;
                if !first {
                    out.push_str(&sep);
                }
                first = false;
                if v.get_type().is(safe_string) {
                    out.push_str(&v.cast::<PyString>()?.to_cow()?);
                } else if let Ok(s) = v.cast_exact::<PyString>() {
                    let rs = s.to_cow()?;
                    match escape_html(&rs) {
                        Some(e) => out.push_str(&e),
                        None => out.push_str(&rs),
                    }
                } else if v.is_exact_instance_of::<PyInt>() {
                    out.push_str(&v.str()?.to_cow()?);
                } else {
                    let e = ce.call1((v,))?;
                    match e.cast::<PyString>() {
                        Ok(s) => out.push_str(&s.to_cow()?),
                        Err(_) => bad = true,
                    }
                }
            }
            Ok((!bad).then_some(out))
        })();
        return match res {
            Ok(Some(s)) => mark_safe_str(py, &s),
            Ok(None) => Ok(input.clone()),
            Err(e) if is_te(py, &e) => Ok(input.clone()),
            Err(e) => Err(e),
        };
    }
    let res = (|| -> PyResult<Bound<'py, PyAny>> {
        if autoescape {
            let ce = h.conditional_escape.bind(py);
            let sep = ce.call1((arg,))?;
            let mut items: Vec<Bound<'py, PyAny>> = Vec::new();
            for item in input.try_iter()? {
                let v = item?;
                items.push(ce.call1((v,))?);
            }
            sep.call_method1("join", (PyList::new(py, items)?,))
        } else {
            arg.call_method1("join", (input,))
        }
    })();
    match res {
        Ok(data) => crate::render::value::mark_safe_bound(&data),
        Err(e) if is_te(py, &e) => Ok(input.clone()),
        Err(e) => Err(e),
    }
}
