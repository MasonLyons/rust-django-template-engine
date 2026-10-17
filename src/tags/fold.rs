//! `FilterNode` render port.

use pyo3::prelude::*;
use pyo3::types::PyDict;

use super::{annotate, expr_res, py_str_of, with_scoped, TagPayload};
use crate::nodes;
use crate::render::context::Rctx;


/// `FilterNode.render`: render body, push `var=output`, resolve filter expr.
pub(crate) fn filter_tag(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::Filter { fe, body, token } = p else {
        return Ok(());
    };
    let res = (|| -> PyResult<String> {
        let mut buf = String::new();
        nodes::render_list(rctx, body, &mut buf)?;
        let d = PyDict::new(rctx.py());
        d.set_item("var", buf)?;
        let mut result = String::new();
        with_scoped(rctx, &Some(d.into_any()), |rctx| {
            let v = expr_res(rctx, fe)?;
            result = py_str_of(&v)?;
            Ok(())
        })?;
        Ok(result)
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
