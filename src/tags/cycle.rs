//! `CycleNode` / `ResetCycleNode` / `IfChangedNode` render ports.
//! Stock keeps a `itertools.cycle` iterator per node in `render_context`;
//! dtl keeps a plain integer counter keyed by the node's `state_id` (see
//! `tags::mod` docs). `render_context` lookups walk every frame (stock
//! `BaseContext.__getitem__`), writes land in the top frame.

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyInt, PyList, PyString};

use super::{annotate, expr_res, expr_res_ign, state_key, Expr, TagPayload};
use crate::render::context::Rctx;
use crate::varparse::{PyConst, VarPart};

/// Top-frame `render_context` lookup (`RenderContext.__contains__` and
/// `__getitem__` only see `dicts[-1]`): `None` when absent.
fn rc_opt<'py>(
    rctx: &Rctx<'py>,
    key: &Bound<'py, PyAny>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    rctx.rc_frame()?.cast_into::<PyDict>()?.get_item(key)
}

/// The literal text of a bare quoted-string expression (`'a'` with no
/// filters/translate), whose render output is provably the literal itself.
fn const_literal_str(e: &Expr) -> Option<&str> {
    if !e.filters.is_empty() || !e.fe.filters.is_empty() || e.fe.is_var {
        return None;
    }
    match &e.fe.var {
        VarPart::Const(PyConst::Str(s)) => Some(s.as_str()),
        _ => None,
    }
}

/// `CycleNode.render` + `reset`.
pub(crate) fn cycle(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::Cycle {
        values,
        var_name,
        silent,
        state_id,
        token,
    } = p
    else {
        return Ok(());
    };
    let res = (|| -> PyResult<()> {
        let py = rctx.py();
        let key = state_key(py, *state_id).into_bound(py).into_any();
        let i: usize = match rc_opt(rctx, &key)? {
            Some(v) => v.extract()?,
            None => 0,
        };
        let idx = i % values.len();
        // Quoted-literal value (stock marks literals `mark_safe`): the
        // rendered text is the literal itself — no resolution, no
        // `render_value_in_context`, no temporary `PyString`.
        if var_name.is_none() && !*silent {
            if let Some(s) = const_literal_str(&values[idx]) {
                rctx.rc_set(&key, &PyInt::new(py, i + 1))?;
                out.push_str(s);
                return Ok(());
            }
        }
        let value = expr_res(rctx, &values[idx])?;
        if let Some(name) = var_name {
            rctx.set_upward(name, &value)?;
        }
        rctx.rc_set(&key, &PyInt::new(py, i + 1))?;
        if *silent {
            return Ok(());
        }
        out.push_str(&crate::render::value::render_value_in_context(rctx, &value)?);
        Ok(())
    })();
    res.inspect_err(|e| annotate(rctx, e, token))
}

/// `ResetCycleNode.render`: `node.reset(context)` → counter back to 0.
pub(crate) fn reset_cycle(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::ResetCycle { state_id, token } = p else {
        return Ok(());
    };
    let res = (|| -> PyResult<String> {
        let py = rctx.py();
        let key = state_key(py, *state_id).into_bound(py).into_any();
        rctx.rc_set(&key, &PyInt::new(py, 0))?;
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

/// `IfChangedNode.render`.
pub(crate) fn if_changed(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::IfChanged {
        values,
        t,
        f,
        state_id,
        token,
    } = p
    else {
        return Ok(());
    };
    let res = (|| -> PyResult<String> {
        let py = rctx.py();
        let key = state_key(py, *state_id).into_bound(py).into_any();
        // `_get_context_stack_frame`: the local forloop dict when inside a
        // loop, else render_context.
        let state_frame: Option<Bound<'_, PyAny>> =
            if rctx.context_has("forloop")? {
                rctx.get_item("forloop")?
            } else {
                None
            };

        let stored: Option<Bound<'_, PyAny>> = match &state_frame {
            Some(d) => d.cast::<PyDict>()?.get_item(&key)?,
            None => rc_opt(rctx, &key)?,
        };
        // `state_frame.setdefault(self)` — initial state None (key present).
        if stored.is_none() {
            match &state_frame {
                Some(d) => d.set_item(&key, py.None())?,
                None => rctx.rc_set(&key, &py.None().into_bound(py))?,
            }
        }

        let mut prerendered: Option<String> = None;
        let compare_to: Bound<'_, PyAny> = if values.is_empty() {
            let mut buf = String::new();
            crate::nodes::render_list(rctx, t, &mut buf)?;
            prerendered = Some(buf);
            PyString::new(py, prerendered.as_deref().unwrap_or("")).into_any()
        } else {
            let items = PyList::empty(py);
            for v in values {
                items.append(expr_res_ign(rctx, v, true)?)?;
            }
            items.into_any()
        };

        let changed = match &stored {
            None => true,
            Some(s) => compare_to.ne(s)?,
        };
        if changed {
            match &state_frame {
                Some(d) => d.set_item(&key, &compare_to)?,
                None => rctx.rc_set(&key, &compare_to)?,
            }
            // render true block if not already rendered
            if let Some(s) = prerendered {
                if !s.is_empty() {
                    return Ok(s);
                }
            }
            let mut buf = String::new();
            crate::nodes::render_list(rctx, t, &mut buf)?;
            let _ = &stored;
            return Ok(buf);
        }
        if !f.nodes.is_empty() {
            let mut buf = String::new();
            crate::nodes::render_list(rctx, f, &mut buf)?;
            return Ok(buf);
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
