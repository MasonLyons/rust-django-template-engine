//! Logic/misc filters of `django.template.defaultfilters` (native ports).

use pyo3::prelude::*;
use pyo3::types::PyString;

use crate::render::context::Rctx;

use super::utils;

/// `default(value, arg)`: `value or arg`.
pub(crate) fn default<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    if input.is_truthy()? {
        Ok(input.clone())
    } else {
        Ok(args[0].clone())
    }
}

/// `default_if_none(value, arg)`.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn default_if_none<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    if input.is_none() {
        Ok(args[0].clone())
    } else {
        Ok(input.clone())
    }
}

/// `yesno(value, arg=None)`: split on commas; <2 parts → value; None →
/// maybe (falls back to no for 2-bit); truthiness otherwise. `gettext` runs
/// at call time when arg is omitted (translation state matters).
pub(crate) fn yesno<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let arg: Bound<'py, PyAny> = match args.first() {
        Some(a) if !a.is_none() => a.clone(),
        _ => utils(py)?.gettext.bind(py).call1(("yes,no,maybe",))?,
    };
    let bits: Vec<String> = arg
        .str()?
        .to_string_lossy()
        .split(',')
        .map(str::to_string)
        .collect();
    if bits.len() < 2 {
        return Ok(input.clone());
    }
    let (yes, no, maybe): (String, String, String) = if bits.len() == 3 {
        (bits[0].clone(), bits[1].clone(), bits[2].clone())
    } else {
        (bits[0].clone(), bits[1].clone(), bits[1].clone())
    };
    if input.is_none() {
        return Ok(PyString::new(py, &maybe).into_any());
    }
    let pick = if input.is_truthy()? { &yes } else { &no };
    Ok(PyString::new(py, pick).into_any())
}

/// `phone2numeric(value)`: `django.utils.text.phone2numeric` on the raw
/// value (non-string input raises `AttributeError` — propagated, not
/// stringified; NOT a `@stringfilter` filter).
pub(crate) fn phone2numeric<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    utils(py)?.phone2numeric.bind(py).call1((input,))
}

/// `pprint(value)`: `pformat`; any exception →
/// `"Error in formatting: {type}: {msg}"`.
pub(crate) fn pprint<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    match utils(py)?.pformat.bind(py).call1((input,)) {
        Ok(v) => Ok(v),
        Err(e) => {
            let val = e.value(py);
            let cls: String = val.get_type().name()?.extract()?;
            let msg: String = val.str()?.extract()?;
            Ok(PyString::new(py, &format!("Error in formatting: {cls}: {msg}")).into_any())
        }
    }
}
