//! `ForNode` / `IfNode` / `WithNode` render ports.

use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyInt, PyList, PyString};

use super::{annotate, expr_res_ign, with_scoped, Expr, TagPayload};
use crate::nodes::{self, NodeList};
use crate::render::context::{ForState, Rctx};
use crate::render::value;
/// `ForNode.render`.
pub(crate) fn for_tag(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::For {
        loopvars,
        loopkeys,
        seq,
        rev,
        body,
        empty,
        with_length,
        forloop_free,
        forloop_used,
        token,
        ..
    } = p
    else {
        return Ok(());
    };
    let res = do_for(
        rctx, loopvars, loopkeys, seq, *rev, body, empty, *with_length, *forloop_free, *forloop_used,
    );
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

#[allow(clippy::too_many_arguments)]
fn do_for(
    rctx: &mut Rctx<'_>,
    loopvars: &[String],
    loopkeys: &[Py<PyString>],
    seq: &Expr,
    rev: bool,
    body: &NodeList,
    empty: &NodeList,
    with_length: bool,
    forloop_free: bool,
    forloop_used: u16,
) -> PyResult<String> {
    let py = rctx.py();
    let mut out = String::new();
    with_scoped(rctx, &None, |rctx| {
        let mut values = expr_res_ign(rctx, seq, true)?;
        if values.is_none() {
            values = PyList::empty(py).into_any();
        }
        // `if not hasattr(values, "__len__"): values = list(values)`
        if !values.hasattr("__len__")? {
            let items: Vec<Bound<'_, PyAny>> = values.try_iter()?.collect::<PyResult<_>>()?;
            values = PyList::new(py, items)?.into_any();
        }
        let mut items: Vec<Bound<'_, PyAny>> = values.try_iter()?.collect::<PyResult<_>>()?;
        let len_values = items.len();
        if len_values < 1 {
            return nodes::render_list(rctx, empty, &mut out);
        }
        if rev {
            items.reverse();
        }
        let num_loopvars = loopvars.len();
        let unpack = num_loopvars > 1;
        // loop_dict: plain dict bound as `context["forloop"]` in the pushed
        // frame; mutated in place each iteration (stock aliasing). The
        // parse-time scan proves nobody reads it for `forloop_free` loops,
        // so skip creation, the parentloop fetch and all per-iter updates.
        let loop_dict = if forloop_free {
            None
        } else {
            let parentloop = match rctx.get_item("forloop")? {
                Some(p) => p,
                None => PyDict::new(py).into_any(),
            };
            let d = PyDict::new(py);
            d.set_item("parentloop", &parentloop)?;
            if with_length {
                let len_i64 = i64::try_from(len_values).unwrap_or(i64::MAX);
                d.set_item("length", PyInt::new(py, len_i64))?;
            }
            rctx.set_item("forloop", &d)?;
            Some(d)
        };
        let len_i64 = i64::try_from(len_values).unwrap_or(i64::MAX);
        // Native counter slot for rewritten `FlVar` nodes: lives for the
        // whole iteration span (dict above stays current for fallbacks).
        rctx.forstack.push(ForState { i: 0, len: len_i64 });
        let slot = rctx.forstack.len() - 1;
        for (i, item) in items.iter().enumerate() {
            rctx.forstack[slot].i = i64::try_from(i).unwrap_or(i64::MAX);
            if let Some(loop_dict) = &loop_dict {
                fl_update(py, loop_dict, forloop_used, i, len_i64, len_values)?;
            }
            let mut pop_context = false;
            if unpack {
                // len(item), TypeError -> 1 (stock).
                let len_item = match values_len(item) {
                    Ok(n) => n,
                    Err(e)
                        if e.is_instance_of::<pyo3::exceptions::PyTypeError>(py) =>
                    {
                        1
                    }
                    Err(e) => return Err(e),
                };
                if num_loopvars != len_item {
                    return Err(pyo3::exceptions::PyValueError::new_err(format!(
                        "Need {num_loopvars} values to unpack in for loop; got {len_item}. "
                    )));
                }
                let d = PyDict::new(py);
                for (name, val) in loopkeys.iter().zip(item.try_iter()?) {
                    d.set_item(name.bind(py), val?)?;
                }
                let d = d.into_any();
                if !rctx.push_layer(Some(&d))? {
                    rctx.push(d)?;
                    rctx.invalidate()?;
                }
                pop_context = true;
            } else {
                rctx.set_item_key(loopkeys[0].bind(py), item)?;
            }
            let r = nodes::render_list(rctx, body, &mut out);
            if pop_context && !rctx.pop_layer()? {
                let p = rctx.pop();
                rctx.invalidate()?;
                p?;
            }
            r?;
        }
        rctx.forstack.pop();
        Ok(())
    })?;
    // stock: mark_safe("".join(nodelist)) — do_render's outer mark_safe
    // makes the safe-string distinction invisible here.
    Ok(out)
}

/// Per-iteration `forloop` dict updates for the keys the parse-time scan
/// proved the body reads (`forloop_used` mask).
fn fl_update(
    py: Python<'_>,
    loop_dict: &Bound<'_, PyDict>,
    mask: u16,
    i: usize,
    len_i64: i64,
    len_values: usize,
) -> PyResult<()> {
    let i64v = i64::try_from(i).unwrap_or(i64::MAX);
    if mask & super::FL_COUNTER0 != 0 {
        loop_dict.set_item("counter0", PyInt::new(py, i64v))?;
    }
    if mask & super::FL_COUNTER != 0 {
        loop_dict.set_item("counter", PyInt::new(py, i64v + 1))?;
    }
    if mask & super::FL_REVCOUNTER != 0 {
        loop_dict.set_item("revcounter", PyInt::new(py, len_i64 - i64v))?;
    }
    if mask & super::FL_REVCOUNTER0 != 0 {
        loop_dict.set_item("revcounter0", PyInt::new(py, len_i64 - i64v - 1))?;
    }
    if mask & super::FL_FIRST != 0 {
        loop_dict.set_item("first", PyBool::new(py, i == 0))?;
    }
    if mask & super::FL_LAST != 0 {
        loop_dict.set_item("last", PyBool::new(py, i == len_values - 1))?;
    }
    Ok(())
}

fn values_len(v: &Bound<'_, PyAny>) -> PyResult<usize> {
    v.len()
}

/// `IfNode.render`: first matching arm renders; VDX inside a condition is
// not possible (ifexpr.eval already folds try/except → False).
pub(crate) fn if_tag(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::If { arms, token } = p else {
        return Ok(());
    };
    let res = (|| -> PyResult<String> {
        for (cond, nodelist) in arms {
            let matched = match cond {
                Some(c) => match c.eval(rctx) {
                    Ok(v) => v.is_truthy()?,
                    Err(e) if value::is_var_does_not_exist(rctx.py(), &e) => false,
                    Err(e) => return Err(e),
                },
                None => true,
            };
            if matched {
                let mut buf = String::new();
                nodes::render_list(rctx, nodelist, &mut buf)?;
                return Ok(buf);
            }
        }
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

/// `WithNode.render`.
pub(crate) fn with_tag(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::With {
        values,
        body,
        token,
    } = p
    else {
        return Ok(());
    };
    let res = (|| -> PyResult<String> {
        let py = rctx.py();
        let d = PyDict::new(py);
        for (k, v) in values {
            d.set_item(k, expr_res_ign(rctx, v, false)?)?;
        }
        let mut buf = String::new();
        with_scoped(rctx, &Some(d.into_any()), |rctx| {
            nodes::render_list(rctx, body, &mut buf)
        })?;
        Ok(buf)
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
