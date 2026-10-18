//! Native `simple_tag` / `inclusion_tag` nodes (`django/template/library.py`).
//!
//! A registered tag whose compile function is exactly the closure built by
//! stock `Library.simple_tag` / `Library.inclusion_tag` (identified by its
//! code object, with `SimpleNode`/`InclusionNode`/`parse_bits` still the
//! stock objects) is compiled here: the closure's `getfullargspec` cells are
//! read once per function, `parse_bits` is ported, and render runs
//! `TagHelperNode.get_resolved_arguments` + `SimpleNode.render` /
//! `InclusionNode.render` without Python node code. Any parse failure
//! returns `None` so the caller replays the stock compile function on the
//! shim parser, which raises the exact stock error. Everything else (custom
//! compile functions, `simple_block_tag`, patched library internals) keeps
//! the bridged path.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyDict, PyFunction, PyString, PyTuple};

use super::{expr, expr_res, state_key, Expr, TagPayload};
use crate::lexer::{split_contents, SplitContents, Token};
use crate::parser::{token_kwargs, Parser};
use crate::render::context::Rctx;
use crate::render::value;
use crate::varparse::PyVer;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HelperKind {
    Simple,
    Inclusion,
}

/// The stock compile closure's captured `getfullargspec` state.
pub struct HelperSpec {
    pub kind: HelperKind,
    pub func: Py<PyAny>,
    pub takes_context: bool,
    /// Positional params (`context` already stripped when `takes_context`).
    params: Vec<String>,
    varargs: bool,
    varkw: bool,
    /// `len(defaults)` when `defaults is not None`.
    defaults: Option<usize>,
    kwonly: Vec<String>,
    /// Keys of `kwonly_defaults` (`None`/empty dict both falsy).
    kwonly_defaults: Vec<String>,
    /// `InclusionNode.filename`.
    pub filename: Option<Py<PyAny>>,
}

/// Process-wide references the detection compares against.
struct Refs {
    simple_code: Py<PyAny>,
    inclusion_code: Py<PyAny>,
    library_globals: Py<PyAny>,
    simple_node: Py<PyAny>,
    inclusion_node: Py<PyAny>,
    parse_bits: Py<PyAny>,
    simple_render: Py<PyAny>,
    inclusion_render: Py<PyAny>,
    resolved_args: Py<PyAny>,
}

fn refs(py: Python<'_>) -> PyResult<&'static Refs> {
    static R: PyOnceLock<Refs> = PyOnceLock::new();
    R.get_or_try_init(py, || -> PyResult<Refs> {
        let lib = py.import("django.template.library")?;
        let ns = PyDict::new(py);
        ns.set_item("Library", lib.getattr("Library")?)?;
        py.run(
            c"lib = Library()\ndef _dtl_probe(): pass\nlib.simple_tag(_dtl_probe)\nsimple = lib.tags['_dtl_probe'].__code__\nlib.inclusion_tag('x')(_dtl_probe)\ninclusion = lib.tags['_dtl_probe'].__code__\n",
            Some(&ns),
            None,
        )?;
        let get = |k: &str| -> PyResult<Py<PyAny>> {
            ns.get_item(k)?
                .map(Bound::unbind)
                .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err(k.to_string()))
        };
        let simple_node = lib.getattr("SimpleNode")?;
        let inclusion_node = lib.getattr("InclusionNode")?;
        Ok(Refs {
            simple_code: get("simple")?,
            inclusion_code: get("inclusion")?,
            library_globals: lib.dict().into_any().unbind(),
            simple_render: simple_node.getattr("render")?.unbind(),
            inclusion_render: inclusion_node.getattr("render")?.unbind(),
            resolved_args: lib
                .getattr("TagHelperNode")?
                .getattr("get_resolved_arguments")?
                .unbind(),
            simple_node: simple_node.unbind(),
            inclusion_node: inclusion_node.unbind(),
            parse_bits: lib.getattr("parse_bits")?.unbind(),
        })
    })
}

type Memo = HashMap<usize, (Py<PyAny>, Option<Arc<HelperSpec>>)>;

/// The `HelperSpec` of a stock `simple_tag`/`inclusion_tag` compile
/// function, `None` for anything else. Memoized per function object (the
/// memo holds a strong reference, so addresses cannot be recycled).
pub(crate) fn helper_spec(
    py: Python<'_>,
    f: &Bound<'_, PyAny>,
    ver: PyVer,
) -> Option<Arc<HelperSpec>> {
    static MEMO: Mutex<Option<Memo>> = Mutex::new(None);
    let key = f.as_ptr() as usize;
    if let Ok(g) = MEMO.lock() {
        if let Some((_, spec)) = g.as_ref().and_then(|m| m.get(&key)) {
            return spec.clone();
        }
    }
    let spec = detect(py, f, ver).ok().flatten().map(Arc::new);
    if let Ok(mut g) = MEMO.lock() {
        g.get_or_insert_with(HashMap::new)
            .insert(key, (f.clone().unbind(), spec.clone()));
    }
    spec
}

fn detect(py: Python<'_>, f: &Bound<'_, PyAny>, ver: PyVer) -> PyResult<Option<HelperSpec>> {
    if !f.is_exact_instance_of::<PyFunction>() {
        return Ok(None);
    }
    let r = refs(py)?;
    let code = f.getattr("__code__")?;
    let kind = if code.is(r.simple_code.bind(py)) {
        HelperKind::Simple
    } else if code.is(r.inclusion_code.bind(py)) {
        HelperKind::Inclusion
    } else {
        return Ok(None);
    };
    // The closure must still build stock nodes via stock `parse_bits`.
    let globals = f.getattr("__globals__")?;
    if !globals.is(r.library_globals.bind(py)) {
        return Ok(None);
    }
    let g = |k: &str| globals.get_item(k);
    let (node_cls, render_ref) = match kind {
        HelperKind::Simple => (&r.simple_node, &r.simple_render),
        HelperKind::Inclusion => (&r.inclusion_node, &r.inclusion_render),
    };
    let node = g(match kind {
        HelperKind::Simple => "SimpleNode",
        HelperKind::Inclusion => "InclusionNode",
    })?;
    if !node.is(node_cls.bind(py))
        || !g("parse_bits")?.is(r.parse_bits.bind(py))
        || !node.getattr("render")?.is(render_ref.bind(py))
        || !node.getattr("get_resolved_arguments")?.is(r.resolved_args.bind(py))
    {
        return Ok(None);
    }
    let names: Vec<String> = code.getattr("co_freevars")?.extract()?;
    let closure = f.getattr("__closure__")?;
    if closure.is_none() {
        return Ok(None);
    }
    let mut cells: HashMap<String, Bound<'_, PyAny>> = HashMap::new();
    for (i, n) in names.into_iter().enumerate() {
        cells.insert(n, closure.get_item(i)?.getattr("cell_contents")?);
    }
    let cell = |k: &str| -> PyResult<Bound<'_, PyAny>> {
        cells
            .get(k)
            .cloned()
            .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err(k.to_string()))
    };
    let takes_context = cell("takes_context")?.is_truthy()?;
    let mut params: Vec<String> = cell("params")?.extract()?;
    // 5.2 strips `context` inside `parse_bits` (raising there when it is
    // missing); 6.1 already stripped it in the decorator.
    if ver == PyVer::V52 && takes_context {
        if params.first().map(String::as_str) == Some("context") {
            params.remove(0);
        } else {
            return Ok(None);
        }
    }
    let defaults = cell("defaults")?;
    let kwd = cell("kwonly_defaults")?;
    let kwonly_defaults: Vec<String> = if kwd.is_truthy()? {
        kwd.try_iter()?
            .map(|k| k.and_then(|k| k.extract::<String>()))
            .collect::<PyResult<_>>()?
    } else {
        Vec::new()
    };
    Ok(Some(HelperSpec {
        kind,
        func: cell("func")?.unbind(),
        takes_context,
        params,
        varargs: !cell("varargs")?.is_none(),
        varkw: !cell("varkw")?.is_none(),
        defaults: if defaults.is_none() {
            None
        } else {
            Some(defaults.len()?)
        },
        kwonly: cell("kwonly")?.extract()?,
        kwonly_defaults,
        filename: match kind {
            HelperKind::Inclusion => Some(cell("filename")?.unbind()),
            HelperKind::Simple => None,
        },
    }))
}

/// Port of the stock compile closure + `parse_bits`. `None` on any stock
/// error path (the caller re-runs the stock compile function to raise it).
pub(crate) fn parse(
    py: Python<'_>,
    p: &Parser<'_>,
    spec: &Arc<HelperSpec>,
    token: &Token,
) -> Option<TagPayload> {
    let SplitContents::Ok(mut bits) = split_contents(&token.contents) else {
        return None;
    };
    bits.remove(0);
    let mut target_var = None;
    if spec.kind == HelperKind::Simple && bits.len() >= 2 && bits[bits.len() - 2] == "as" {
        target_var = bits.pop();
        bits.pop();
    }
    let mut args: Vec<Expr> = Vec::new();
    let mut kwargs: Vec<(Py<PyString>, Expr)> = Vec::new();
    let mut kw_names: Vec<String> = Vec::new();
    let mut unhandled_params: Vec<&str> = spec.params.iter().map(String::as_str).collect();
    let mut unhandled_kwargs: Vec<&str> = spec
        .kwonly
        .iter()
        .filter(|k| !spec.kwonly_defaults.contains(k))
        .map(String::as_str)
        .collect();
    for bit in &bits {
        let mut one = vec![bit.clone()];
        let mut kw = token_kwargs(py, p, &mut one, false).ok()?;
        if let Some((param, value)) = kw.pop() {
            let known = spec.params.contains(&param) || spec.kwonly.contains(&param);
            if (!known && !spec.varkw) || kw_names.contains(&param) {
                return None;
            }
            if let Some(i) = unhandled_params.iter().position(|x| *x == param) {
                unhandled_params.remove(i);
            } else if let Some(i) = unhandled_kwargs.iter().position(|x| *x == param) {
                unhandled_kwargs.remove(i);
            }
            kwargs.push((PyString::intern(py, &param).unbind(), expr(value)));
            kw_names.push(param);
        } else {
            if !kwargs.is_empty() {
                return None;
            }
            args.push(expr(p.compile_filter(py, bit).ok()?));
            if unhandled_params.is_empty() {
                if !spec.varargs {
                    return None;
                }
            } else {
                unhandled_params.remove(0);
            }
        }
    }
    if let Some(n) = spec.defaults {
        // `unhandled_params[:-n]` (`n == 0` slices to empty).
        let keep = if n == 0 { 0 } else { unhandled_params.len().saturating_sub(n) };
        unhandled_params.truncate(keep);
    }
    if !unhandled_params.is_empty() || !unhandled_kwargs.is_empty() {
        return None;
    }
    Some(match spec.kind {
        HelperKind::Simple => TagPayload::SimpleTag {
            spec: spec.clone(),
            args,
            kwargs,
            target_var,
            token: token.clone(),
        },
        HelperKind::Inclusion => TagPayload::InclusionTag {
            spec: spec.clone(),
            args,
            kwargs,
            state_id: super::next_state_id(),
            token: token.clone(),
        },
    })
}

/// `TagHelperNode.get_resolved_arguments` + `self.func(*args, **kwargs)`.
fn call_func<'py>(
    rctx: &mut Rctx<'py>,
    spec: &HelperSpec,
    args: &[Expr],
    kwargs: &[(Py<PyString>, Expr)],
) -> PyResult<Bound<'py, PyAny>> {
    let py = rctx.py();
    let mut resolved: Vec<Bound<'py, PyAny>> =
        Vec::with_capacity(args.len() + usize::from(spec.takes_context));
    if spec.takes_context {
        resolved.push(rctx.ctx.clone());
    }
    for a in args {
        resolved.push(expr_res(rctx, a)?);
    }
    let kw = if kwargs.is_empty() {
        None
    } else {
        let d = PyDict::new(py);
        for (k, e) in kwargs {
            d.set_item(k.bind(py), expr_res(rctx, e)?)?;
        }
        Some(d)
    };
    let out = spec
        .func
        .bind(py)
        .call(PyTuple::new(py, resolved)?, kw.as_ref());
    if spec.takes_context {
        // The function holds the live context and may assign its flags.
        rctx.invalidate_flags();
    }
    out
}

/// `SimpleNode.render`.
pub(crate) fn simple(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::SimpleTag {
        spec,
        args,
        kwargs,
        target_var,
        token,
    } = p
    else {
        return Ok(());
    };
    let res = (|| -> PyResult<()> {
        let output = call_func(rctx, spec, args, kwargs)?;
        if let Some(t) = target_var {
            rctx.set_item(t, &output)?;
            return Ok(());
        }
        if rctx.autoescape()? {
            out.push_str(&value::conditional_escape_str(&output)?);
        } else {
            out.push_str(&output.str()?.to_string_lossy());
        }
        Ok(())
    })();
    if let Err(e) = &res {
        crate::nodes::annotate_error(rctx, e, token);
    }
    res
}

/// `InclusionNode.render`.
pub(crate) fn inclusion(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::InclusionTag {
        spec,
        args,
        kwargs,
        state_id,
        token,
    } = p
    else {
        return Ok(());
    };
    let res = do_inclusion(rctx, spec, args, kwargs, *state_id, out);
    // The included template is rendered through the Python-visible
    // protocol and may assign context flags.
    rctx.invalidate_flags();
    if let Err(e) = &res {
        crate::nodes::annotate_error(rctx, e, token);
    }
    res
}

fn do_inclusion(
    rctx: &mut Rctx<'_>,
    spec: &HelperSpec,
    args: &[Expr],
    kwargs: &[(Py<PyString>, Expr)],
    state_id: usize,
    out: &mut String,
) -> PyResult<()> {
    let py = rctx.py();
    let dict = call_func(rctx, spec, args, kwargs)?;
    let key = state_key(py, state_id).into_bound(py).into_any();
    let frame = rctx.rc_frame()?.cast_into::<PyDict>()?;
    let t = match frame.get_item(&key)? {
        Some(t) if !t.is_none() => t,
        _ => {
            let t = select_template(rctx, spec)?;
            frame.set_item(&key, &t)?;
            t
        }
    };
    let (new_ctx, new_dicts) = rctx.new_context_dicts(&dict)?;
    // Copy across the CSRF token, if present.
    let k_csrf = pyo3::intern!(py, "csrf_token");
    if let Some(tok) = rctx.get_key(k_csrf)?.filter(|v| !v.is_none()) {
        match new_dicts {
            // `Context.__setitem__`: `self.dicts[-1][key] = value`.
            Some(d) => d.get_item(d.len() - 1)?.set_item(k_csrf, tok)?,
            None => new_ctx.set_item(k_csrf, tok)?,
        }
    }
    if let Some(s) = crate::engine::try_render_child(py, &t, &new_ctx, None)? {
        out.push_str(&s);
        return Ok(());
    }
    let v = t.call_method1("render", (&new_ctx,))?;
    out.push_str(&v.str()?.to_string_lossy());
    Ok(())
}

/// The `InclusionNode.render` template selection on a render-context miss.
fn select_template<'py>(rctx: &Rctx<'py>, spec: &HelperSpec) -> PyResult<Bound<'py, PyAny>> {
    let py = rctx.py();
    let filename = spec
        .filename
        .as_ref()
        .map_or_else(|| py.None().into_bound(py), |f| f.bind(py).clone());
    let engine = || -> PyResult<Bound<'py, PyAny>> {
        rctx.ctx.getattr("template")?.getattr("engine")
    };
    if filename.is_exact_instance_of::<PyString>() {
        return engine()?.call_method1("get_template", (&filename,));
    }
    // `Template` is the library module's global (test environments
    // rebind it), read at render time like stock.
    let template_cls = refs(py)?.library_globals.bind(py).get_item("Template")?;
    if filename.is_instance(&template_cls)? {
        return Ok(filename);
    }
    if let Some(inner) = filename.getattr_opt("template")? {
        if inner.is_instance(&template_cls)? {
            return Ok(inner);
        }
    }
    let iterable = py.import("collections.abc")?.getattr("Iterable")?;
    if !filename.is_instance_of::<PyString>() && filename.is_instance(&iterable)? {
        return engine()?.call_method1("select_template", (&filename,));
    }
    engine()?.call_method1("get_template", (&filename,))
}
