//! Integer filters of `django.template.defaultfilters` (native ports).

use pyo3::prelude::*;
use pyo3::types::PyString;

use crate::render::context::Rctx;
use crate::render::value::helpers;

use super::{is_ie, is_te, is_ve, utils};

/// `add`: `int(value) + int(arg)`; on `ValueError`/`TypeError` fall back to
/// `value + arg`; any failure of the fallback → `""`.
pub(crate) fn add<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let int_builtin = helpers(py)?.int_builtin.bind(py).clone();
    match int_builtin
        .call1((input,))
        .and_then(|vi| int_builtin.call1((&args[0],)).and_then(|ai| vi.add(&ai)))
    {
        Ok(v) => Ok(v),
        Err(e) if is_ve(py, &e) || is_te(py, &e) => match input.add(&args[0]) {
            Ok(v) => Ok(v),
            Err(inner) => {
                drop(inner);
                Ok(PyString::new(py, "").into_any())
            }
        },
        Err(e) => Err(e),
    }
}

/// `get_digit`: int-convert arg then value (`ValueError` → raw input);
/// `arg < 1` → int value; else `int(str(value)[-arg])`, out-of-range index
/// → `0`.
pub(crate) fn get_digit<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let ib = helpers(py)?.int_builtin.bind(py).clone();
    let conv: PyResult<(Bound<'py, PyAny>, Bound<'py, PyAny>)> = (|| {
        let ai = ib.call1((&args[0],))?;
        let vi = ib.call1((input,))?;
        Ok((ai, vi))
    })();
    let (ai, vi) = match conv {
        Ok(t) => t,
        Err(e) if is_ve(py, &e) => return Ok(input.clone()),
        Err(e) => return Err(e),
    };
    if ai.lt(1_i64)? {
        return Ok(vi);
    }
    if ai.gt(isize::MAX)? {
        return Err(pyo3::exceptions::PyOverflowError::new_err(
            "Python int too large to convert to C ssize_t",
        ));
    }
    let digits = ai.extract::<isize>()?;
    match vi.str()?.get_item(-digits) {
        Ok(ch) => ib.call1((ch,)),
        Err(e) if is_ie(py, &e) => Ok(0_i64.into_pyobject(py)?.into_any()),
        Err(e) => Err(e),
    }
}

/// `divisibleby`: `int(value) % int(arg) == 0`; errors propagate.
pub(crate) fn divisibleby<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let int_builtin = helpers(py)?.int_builtin.bind(py).clone();
    let vi = int_builtin.call1((input,))?;
    let ai = int_builtin.call1((&args[0],))?;
    let divisible = vi.rem(&ai)?.eq(0_i64)?;
    Ok(pyo3::types::PyBool::new(py, divisible)
        .to_owned()
        .into_any())
}

/// `pluralize(value, arg="s")`: float compare first, then `len()`;
/// unsizeable/invalid → `""`.
pub(crate) fn pluralize<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let arg: String = match args.first() {
        Some(a) => a.str()?.extract()?,
        None => "s".to_string(),
    };
    let full: String = if arg.contains(',') {
        arg
    } else {
        format!(",{arg}")
    };
    let bits: Vec<&str> = full.split(',').collect();
    let empty = || Ok(PyString::new(py, "").into_any());
    if bits.len() > 2 {
        return empty();
    }
    let (sing, plur) = (bits[0], bits[1]);
    let sing_obj = || PyString::new(py, sing).into_any();
    let plur_obj = || PyString::new(py, plur).into_any();
    match utils(py)?.float_builtin.bind(py).call1((input,)) {
        Ok(f) => {
            if f.eq(1.0_f64)? {
                Ok(sing_obj())
            } else {
                Ok(plur_obj())
            }
        }
        Err(e) if is_ve(py, &e) => empty(),
        Err(e) if is_te(py, &e) => match input.len() {
            Ok(n) => {
                if n == 1 {
                    Ok(sing_obj())
                } else {
                    Ok(plur_obj())
                }
            }
            Err(inner) if is_te(py, &inner) => empty(),
            Err(inner) => Err(inner),
        },
        Err(e) => Err(e),
    }
}
