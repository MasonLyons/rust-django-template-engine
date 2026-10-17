//! Native ports of `django.contrib.humanize` filters (installed only when the
//! registered function is the stock one; see `filters::native_for`).

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyInt, PyList, PyString, PyTuple};

use crate::render::context::Rctx;

/// The stock Python `intcomma` (fallback for everything but plain ints).
fn py_intcomma(py: Python<'_>) -> PyResult<&Bound<'_, PyAny>> {
    static F: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    Ok(F.get_or_try_init(py, || -> PyResult<Py<PyAny>> {
        Ok(py
            .import("django.contrib.humanize.templatetags.humanize")?
            .getattr("intcomma")?
            .unbind())
    })?
    .bind(py))
}

/// `intcomma(value)` for an exact `int` that fits `i64` (5.2: `int(value)`,
/// 6.1: `Decimal(value)`; both reach `number_format(..., use_l10n=True,
/// force_grouping=True)` with `str_number == str(value)`): the locale's
/// `NUMBER_GROUPING` / `THOUSAND_SEPARATOR` applied exactly like
/// `numberformat.format`. Floats, Decimals, strings, an explicit
/// `use_l10n` arg, `NUMBER_GROUPING == 0` or an unusual grouping value take
/// the Python function.
pub(crate) fn intcomma<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    if args.is_empty() && input.is_exact_instance_of::<PyInt>() {
        if let Ok(n) = input.extract::<i64>() {
            if let Some(s) = native_intcomma(py, n)? {
                return Ok(PyString::new(py, &s).into_any());
            }
        }
    }
    py_intcomma(py)?.call1((input,).into_pyobject(py)?.add_args(args)?)
}

trait AddArgs<'py> {
    fn add_args(self, args: &[Bound<'py, PyAny>]) -> PyResult<Bound<'py, PyTuple>>;
}

impl<'py> AddArgs<'py> for Bound<'py, PyTuple> {
    fn add_args(self, args: &[Bound<'py, PyAny>]) -> PyResult<Bound<'py, PyTuple>> {
        if args.is_empty() {
            return Ok(self);
        }
        let mut v: Vec<Bound<'py, PyAny>> = self.iter().collect();
        v.extend(args.iter().cloned());
        PyTuple::new(self.py(), v)
    }
}

fn native_intcomma(py: Python<'_>, n: i64) -> PyResult<Option<String>> {
    let Some(lang) = super::get_language(py) else {
        return Ok(None);
    };
    let grouping = super::get_format(py, "NUMBER_GROUPING", lang.as_deref(), true)?;
    // `intervals = list(grouping)` for a sequence, else `[grouping, 0]`.
    let mut intervals: Vec<i64> = if let Ok(g) = grouping.cast_exact::<PyInt>() {
        let g: i64 = g.extract()?;
        if g == 0 {
            return Ok(None);
        }
        vec![g, 0]
    } else if grouping.is_exact_instance_of::<PyTuple>()
        || grouping.is_exact_instance_of::<PyList>()
    {
        let mut v = Vec::new();
        for item in grouping.try_iter()? {
            let item = item?;
            let Ok(i) = item.cast_exact::<PyInt>() else {
                return Ok(None);
            };
            v.push(i.extract()?);
        }
        // `grouping != 0` holds for any sequence.
        if v.is_empty() {
            return Ok(None);
        }
        v
    } else {
        return Ok(None);
    };
    let sep = super::get_format(py, "THOUSAND_SEPARATOR", lang.as_deref(), true)?;
    let Ok(sep) = sep.cast_exact::<PyString>() else {
        return Ok(None);
    };
    let sep_rev: String = sep.to_str()?.chars().rev().collect();
    let digits = n.unsigned_abs().to_string();
    let mut active = intervals.remove(0);
    let mut gd = String::with_capacity(digits.len() * 2);
    let mut cnt: i64 = 0;
    for d in digits.chars().rev() {
        if cnt != 0 && cnt == active {
            if !intervals.is_empty() {
                let next = intervals.remove(0);
                if next != 0 {
                    active = next;
                }
            }
            gd.push_str(&sep_rev);
            cnt = 0;
        }
        gd.push(d);
        cnt += 1;
    }
    let mut out = String::with_capacity(gd.len() + 1);
    if n < 0 {
        out.push('-');
    }
    out.extend(gd.chars().rev());
    Ok(Some(out))
}
