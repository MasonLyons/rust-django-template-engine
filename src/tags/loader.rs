//! Render ports of the loader tags: `BlockNode`, `ExtendsNode`, `IncludeNode`,
//! `PartialDefNode`, `PartialNode` (`loader_tags.py` / `defaulttags.py`).
//!
//! Block/Extends are full Rust ports; `{{ block.super }}` keeps working
//! because the stock `BlockContext`/`BlockNode` machinery (via `_nodes.py`
//! adapters) is what these ports drive.

use pyo3::prelude::*;
use std::fmt::Write as _;
use pyo3::types::{PyDict, PyList, PyString, PyTuple};

use super::{
    adapter_for_id, annotate, expr_res, fns, py_str_of, state_key, with_scoped, TagPayload,
};
use crate::error::syntax_error;
use crate::nodes::{self, NodeList};
use crate::render::context::Rctx;


/// `BlockNode.render`. `id` is the parse-assigned node id; the matching
/// python adapter (ctor-compatible clone source) lives in the current
/// template's `_dtl_node_by_id` map.
pub(crate) fn block(
    rctx: &mut Rctx<'_>,
    p: &TagPayload,
    id: usize,
    out: &mut String,
) -> PyResult<()> {
    let TagPayload::Block { name, nodelist, token, simple } = p else {
        return Ok(());
    };
    match super::blocks::try_block(rctx, name, nodelist, *simple, out) {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(e) => {
            annotate(rctx, &e, token);
            return Err(e);
        }
    }
    let res = do_block(rctx, name, nodelist, id);
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

fn do_block(
    rctx: &mut Rctx<'_>,
    name: &str,
    nodelist: &NodeList,
    id: usize,
) -> PyResult<String> {
    let block_context = rctx
        .rc_frame()?
        .cast_into::<PyDict>()?
        .get_item("block_context")?;

    let mut result = String::new();
    with_scoped(rctx, &None, |rctx| match &block_context {
            None => {
                // No extends involved: render our own nodelist, exposing
                // `block` for `{{ block.super }}` (which raises the stock
                // "base template" error only when actually referenced).
                let own = own_block_adapter(rctx, id)?;
                rctx.set_item("block", &own)?;
                nodes::render_list(rctx, nodelist, &mut result)
            }
            Some(bc) => {
                let push = bc.call_method1("pop", (name,))?;
                let mut block = if push.is_none() {
                    own_block_adapter(rctx, id)?
                } else {
                    push.clone()
                };
                // "Create new block so we can store context without
                // thread-safety issues." dtl adapters take `template` first.
                block = clone_block(&block)?;
                block.setattr("context", &rctx.ctx)?;
                rctx.set_item("block", &block)?;
                let nl = block.getattr("nodelist")?;
                let rendered = nl.call_method1("render", (&rctx.ctx,))?;
                result = py_str_of(&rendered)?;
                if !push.is_none() {
                    bc.call_method1("push", (name, &push))?;
                }
                Ok(())
            }
        }
    )?;
    Ok(result)
}

/// `type(self)(block.name, block.nodelist)` — dtl `_nodes.BlockNode`
/// adapters carry their template in `_template` and need it first, plus
/// their `_render_id` so the clone renders the same native body.
fn clone_block<'py>(block: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    let cls = block.get_type();
    match block.getattr("_template") {
        Ok(t) if !t.is_none() => {
            let clone = cls.call1((t, block.getattr("name")?, block.getattr("nodelist")?))?;
            if let Ok(rid) = block.getattr("_render_id") {
                clone.setattr("_render_id", rid)?;
            }
            Ok(clone)
        }
        _ => cls.call1((block.getattr("name")?, block.getattr("nodelist")?)),
    }
}

/// The adapter for the block currently being rendered.
fn own_block_adapter<'py>(rctx: &Rctx<'py>, id: usize) -> PyResult<Bound<'py, PyAny>> {
    match adapter_for_id(rctx, id)? {
        Some(a) => Ok(a),
        None => Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
            "dtl: missing block adapter for node {id}"
        ))),
    }
}

/// `%r` of a python object (for message formatting).
fn py_repr(v: &Bound<'_, PyAny>) -> PyResult<String> {
    Ok(v.repr()?.to_string())
}

/// `ExtendsNode.render` (+ `get_parent`, `find_template`).
pub(crate) fn extends(
    rctx: &mut Rctx<'_>,
    p: &TagPayload,
    _id: usize,
    out: &mut String,
) -> PyResult<()> {
    let TagPayload::Extends {
        parent,
        token_text,
        has_filters,
        var_text,
        token,
        ..
    } = p
    else {
        return Ok(());
    };
    let res = do_extends(rctx, p, parent, token_text, *has_filters, var_text.as_deref());
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

fn do_extends(
    rctx: &mut Rctx<'_>,
    p: &TagPayload,
    parent_expr: &super::Expr,
    token_text: &str,
    has_filters: bool,
    var_text: Option<&str>,
) -> PyResult<String> {
    let py = rctx.py();
    let parent = expr_res(rctx, parent_expr)?;
    if !parent.is_truthy()? {
        let mut msg = format!(
            "Invalid template name in 'extends' tag: {}.",
            py_repr(&parent)?
        );
        if has_filters || var_text.is_some() {
            let _ = write!(msg, " Got this from the '{token_text}' variable.");
        }
        return Err(syntax_error(py, &msg));
    }
    // Template-instance shortcuts (stock isinstance(parent, Template) plus
    // the backends.django.Template passthrough).
    let compiled_parent: Bound<'_, PyAny> =
        if parent.is_instance_of::<crate::engine::Template>() {
            parent.clone()
        } else if let Some(inner) = parent.getattr_opt(pyo3::intern!(py, "template")).ok().flatten() {
            if inner.is_instance_of::<crate::engine::Template>() {
                inner
            } else {
                find_template_extends(rctx, &parent)?
            }
        } else {
            find_template_extends(rctx, &parent)?
        };

    if let Some(s) = super::blocks::try_extends(rctx, p, &compiled_parent)? {
        return Ok(s);
    }
    super::blocks::demote_to_stock(rctx)?;
    let block_context = ensure_block_context(rctx)?;

    // Own blocks (stock ExtendsNode.blocks): scan this template's adapter
    // nodelist for BlockNode instances.
    let cur = rctx.render_context()?.getattr("template")?;
    let nl = cur.getattr("nodelist")?;
    let blocks = blocks_by_name(py, &nl)?;
    block_context.call_method1("add_blocks", (blocks,))?;

    // Parent first-non-text-node rule.
    let parent_nl = compiled_parent.getattr("nodelist")?;
    for node in parent_nl.try_iter()? {
        let node = node?;
        if !is_instance_stock(py, &node, "django.template.base", "TextNode")? {
            let is_extends = is_instance_stock(
                py,
                &node,
                "django.template.loader_tags",
                "ExtendsNode",
            )? || node.getattr("_dtl_extends").is_ok_and(|v| v.is_truthy().unwrap_or(false));
            if !is_extends {
                let blocks = blocks_by_name(py, &parent_nl)?;
                block_context.call_method1("add_blocks", (blocks,))?;
            }
            break;
        }
    }

    // `with context.render_context.push_state(parent, isolated_context=False):
    //     return parent._render(context)`
    let rc = rctx.render_context()?;
    let kw = PyDict::new(py);
    kw.set_item("isolated_context", false)?;
    let state = rc.call_method("push_state", (compiled_parent.clone(),), Some(&kw))?;
    state.call_method0("__enter__")?;
    match compiled_parent.call_method1("_render", (&rctx.ctx,)) {
        Ok(v) => {
            state.call_method1("__exit__", (py.None(), py.None(), py.None()))?;
            py_str_of(&v)
        }
        Err(e) => {
            let _ = state.call_method1("__exit__", (py.None(), py.None(), py.None()));
            Err(e)
        }
    }
}

fn is_instance_stock(
    py: Python<'_>,
    node: &Bound<'_, PyAny>,
    module: &str,
    class: &str,
) -> PyResult<bool> {
    node.is_instance(&py.import(module)?.getattr(class)?)
}

fn blocks_by_name<'py>(py: Python<'py>, nl: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyDict>> {
    let block_node = py
        .import("django.template.loader_tags")?
        .getattr("BlockNode")?;
    let found = nl.call_method1("get_nodes_by_type", (block_node,))?;
    let dict = PyDict::new(py);
    for n in found.try_iter()? {
        let n = n?;
        dict.set_item(n.getattr("name")?, n)?;
    }
    Ok(dict)
}

fn ensure_block_context<'py>(rctx: &Rctx<'py>) -> PyResult<Bound<'py, PyAny>> {
    let py = rctx.py();
    let frame = rctx.rc_frame()?.cast_into::<PyDict>()?;
    if let Some(bc) = frame.get_item("block_context")? {
        return Ok(bc);
    }
    let bc = py
        .import("django.template.loader_tags")?
        .getattr("BlockContext")?
        .call0()?;
    frame.set_item("block_context", &bc)?;
    Ok(bc)
}

/// `ExtendsNode.find_template`.
fn find_template_extends<'py>(
    rctx: &Rctx<'py>,
    name: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = rctx.py();
    let rc = rctx.render_context()?;
    let key = pyo3::intern!(py, "extends_context");
    let frame = rctx.rc_frame()?.cast_into::<PyDict>()?;
    let history = if let Some(h) = frame.get_item(&key)? {
        h
    } else {
        let cur = rc.getattr(pyo3::intern!(py, "template"))?;
        let list = PyList::new(py, [cur.getattr(pyo3::intern!(py, "origin"))?])?;
        frame.set_item(&key, &list)?;
        list.into_any()
    };
    let (template, origin) =
        crate::engine::Engine::find_template_dispatch(&rctx.engine, name, &history)?;
    history.cast_into::<PyList>()?.append(origin)?;
    Ok(template)
}

/// `IncludeNode.render`.
pub(crate) fn include(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::Include { token, .. } = p else {
        return Ok(());
    };
    let res = do_include(rctx, p);
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

// Native include-depth guard. Stock dies with `RecursionError` because
// every `{% include %}` level costs ~6 CPython frames (`render` →
// `IncludeNode.render` → `_render` → …, measured on 6.1); the native
// render path creates no Python frames, so a self-including template
// would recurse until the C stack segfaults. The cap therefore mirrors
// stock's effective include depth: `sys.getrecursionlimit() / 6`.
thread_local! {
    static INCLUDE_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static INCLUDE_CAP: std::cell::Cell<u32> = const { std::cell::Cell::new(u32::MAX) };
}

/// RAII guard for the thread-local include depth counter.
struct IncludeDepth;

impl IncludeDepth {
    fn enter() -> PyResult<Self> {
        let d = INCLUDE_DEPTH.with(std::cell::Cell::get);
        if d == 0 {
            // Raw C-API recursion limit: stock reads the same value, but
            // `sys.getrecursionlimit()` pays an import and method call.
            let limit = u32::try_from(i64::from(unsafe { pyo3::ffi::Py_GetRecursionLimit() }.max(0)))
                .unwrap_or(1);
            INCLUDE_CAP.set((limit / 6).max(1));
        } else if d >= INCLUDE_CAP.with(std::cell::Cell::get) {
            return Err(pyo3::exceptions::PyRecursionError::new_err(
                "maximum recursion depth exceeded",
            ));
        }
        INCLUDE_DEPTH.with(|c| c.set(d + 1));
        Ok(Self)
    }
}

impl Drop for IncludeDepth {
    fn drop(&mut self) {
        INCLUDE_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

fn do_include(rctx: &mut Rctx<'_>, p: &super::TagPayload) -> PyResult<String> {
    let TagPayload::Include {
        template: template_expr,
        with,
        only,
        state_id,
        origin_name,
        const_name,
        ..
    } = p
    else {
        return Ok(String::new());
    };
    let _depth = IncludeDepth::enter()?;
    let py = rctx.py();
    // Bare constant-string name (the common case): resolve through the
    // rc cache directly, skipping the expression resolve and the render
    // quacks probe; the name tuple is built once per tag.
    if let Some(cached) =
        const_name_fast(rctx, template_expr, const_name, *state_id, *only, with.is_empty())?
    {
        return Ok(cached);
    }
    let mut template = expr_res(rctx, template_expr)?;
    // Native dtl Templates need neither the `render` quacks probe nor the
    // `hasattr("template")` backend unwrap (they have no `.template`
    // attribute; the inline render below bypasses `Template.render`).
    let native = template.cast_exact::<crate::engine::Template>().is_ok();
    let quacks = native
        || template
            .getattr("render")
            .map(|r| r.is_callable())
            .unwrap_or(false);
    if !quacks {
        // Cache under render_context.dicts[0][state_key][name_tuple].
        let mut name = if template.is_truthy()? {
            template.clone()
        } else {
            PyTuple::empty(py).into_any()
        };
        if let Ok(s) = name.cast::<PyString>() {
            // `construct_relative_path` returns `relative_name` verbatim
            // unless the stripped literal starts with `./` or `../`; that
            // early return is version-stable, so skip the Python call for
            // the common non-relative include. Relative names (and their
            // version-specific errors) go through Python unchanged.
            let sv = s.to_string_lossy();
            let trimmed = sv.trim_matches(['\'', '"']);
            if trimmed.starts_with("./") || trimmed.starts_with("../") {
                let cur_origin: Bound<'_, PyAny> = match origin_name {
                    Some(s) => PyString::new(py, s).into_any(),
                    None => py.None().into_bound(py),
                };
                let resolved = fns(py)?
                    .construct_relative_path
                    .bind(py)
                    .call1((cur_origin, &s))?;
                name = PyTuple::new(py, [resolved])?.into_any();
            } else {
                // crp returns the verbatim name here; still wrap as the
                // one-element `select_template` iterable stock builds.
                name = PyTuple::new(py, [s])?.into_any();
            }
        } else {
            let items: Vec<Bound<'_, PyAny>> = name.try_iter()?.collect::<PyResult<_>>()?;
            name = PyTuple::new(py, items)?.into_any();
        }
        template = select_cached_template(rctx, *state_id, &name)?;
    } else if !native && template.hasattr("template")? {
        // Use the base.Template of a backends.django.Template.
        template = template.getattr("template")?;
    }
    let values = build_include_values(rctx, with)?;
    if *only {
        let seed: Bound<'_, PyAny> = match &values {
            Some(v) => v.clone().into_any(),
            None => PyDict::new(py).into_any(),
        };
        let new_ctx = rctx.new_context(&seed)?;
        if let Some(s) = crate::engine::try_render_child(py, &template, &new_ctx, None)? {
            return Ok(s);
        }
        let v = template.call_method1("render", (&new_ctx,))?;
        return py_str_of(&v);
    }
    if native {
        if let Some(s) = crate::engine::try_render_child_inline(
            rctx,
            &template,
            values.as_ref().map(Bound::as_any),
        )? {
            return Ok(s);
        }
    }
    if let Some(s) =
        crate::engine::try_render_child(py, &template, &rctx.ctx, values.as_ref())?
    {
        return Ok(s);
    }
    let mut result = String::new();
    with_scoped(rctx, &values.map(Bound::into_any), |rctx| {
        let v = template.call_method1("render", (&rctx.ctx,))?;
        result = py_str_of(&v)?;
        Ok(())
    })?;
    Ok(result)
}

/// `select_cached_template`: rc-cache lookup + miss-fill (stock
/// `IncludeNode.render` lines 196–204). Shared by the generic and the
/// const-name paths.
fn select_cached_template<'py>(
    rctx: &mut Rctx<'py>,
    state_id: usize,
    name: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = rctx.py();
    let (rc, rc_dicts) = rctx.rc_pair()?;
    let base = rc_dicts.get_item(0)?.cast_into::<PyDict>()?;
    let key = state_key(py, state_id).into_bound(py);
    let cache = if let Some(c) = base.get_item(&key)? {
        c
    } else {
        let c = PyDict::new(py);
        base.set_item(&key, &c)?;
        c.into_any()
    };
    let dict = cache.cast_into::<PyDict>()?;
    if let Some(t) = dict.get_item(name)? {
        return Ok(t);
    }
    let engine = rc.getattr("template")?.getattr("engine")?;
    let t = engine.call_method1("select_template", (name,))?;
    dict.set_item(name, &t)?;
    Ok(t)
}

/// Constant-string fast path: when the include name is a bare quoted
/// literal (no filters/translate) and not relative, resolve the cache
/// entry directly — no `Variable` resolve, no quacks probe, and the name
/// tuple is allocated once per tag. `Ok(None)` = not constant, caller
/// takes the generic path. Constant errors (missing template) raise on
/// every render here, matching stock resolving the constant each time.
fn const_name_fast(
    rctx: &mut Rctx<'_>,
    template_expr: &super::Expr,
    slot: &std::sync::OnceLock<Option<pyo3::Py<pyo3::types::PyTuple>>>,
    state_id: usize,
    only: bool,
    with_empty: bool,
) -> PyResult<Option<String>> {
    if only || !with_empty {
        return Ok(None);
    }
    if template_expr.fe.is_var
        || !template_expr.fe.filters.is_empty()
        || !template_expr.filters.is_empty()
    {
        return Ok(None);
    }
    let name = match &template_expr.fe.var {
        crate::varparse::VarPart::Const(crate::varparse::PyConst::Str(s)) => s.as_str(),
        _ => return Ok(None),
    };
    // Empty string: stock takes the falsy-name path (empty tuple -> a
    // different TemplateDoesNotExist message). Relative names need the
    // origin-dependent `construct_relative_path`.
    if name.is_empty() || name.starts_with("./") || name.starts_with("../") {
        return Ok(None);
    }
    let py = rctx.py();
    let name_b = match slot.get() {
        Some(Some(t)) => t.bind(py).clone().into_any(),
        Some(None) => return Ok(None),
        None => {
            match PyTuple::new(py, [PyString::new(py, name)]).map(Bound::unbind) {
                Ok(t) => {
                    let _ = slot.set(Some(t.clone_ref(py)));
                    t.into_bound(py).into_any()
                }
                Err(e) => {
                    let _ = slot.set(None);
                    return Err(e);
                }
            }
        }
    };
    let template = select_cached_template(rctx, state_id, &name_b)?;
    // Cache hits are usually native dtl Templates: route the same inline
    // path object-named includes get (the old `native` probe was computed
    // from the string expr result, so string includes always paid a
    // fresh `Rctx::new` in `try_render_child`).
    if template.cast_exact::<crate::engine::Template>().is_ok() {
        if let Some(s) = crate::engine::try_render_child_inline(rctx, &template, None)? {
            return Ok(Some(s));
        }
    }
    if let Some(s) = crate::engine::try_render_child(rctx.py(), &template, &rctx.ctx, None)? {
        return Ok(Some(s));
    }
    let v = template.call_method1("render", (&rctx.ctx,))?;
    py_str_of(&v).map(Some)
}

/// Evaluate the `{% include %}` `with` pairs into a dict; `None` when
/// empty (stock pushes an empty, lookup-invisible `ContextDict`).
fn build_include_values<'py>(
    rctx: &mut Rctx<'py>,
    with: &[(String, super::Expr)],
) -> PyResult<Option<Bound<'py, PyDict>>> {
    if with.is_empty() {
        return Ok(None);
    }
    let py = rctx.py();
    let dict = PyDict::new(py);
    for (k, e) in with {
        dict.set_item(k, expr_res(rctx, e)?)?;
    }
    Ok(Some(dict))
}

/// `PartialDefNode.render`.
pub(crate) fn partial_def(
    rctx: &mut Rctx<'_>,
    p: &TagPayload,
    out: &mut String,
) -> PyResult<()> {
    let TagPayload::PartialDef {
        inline,
        body,
        token,
        ..
    } = p
    else {
        return Ok(());
    };
    if !*inline {
        return Ok(());
    }
    let res = (|| -> PyResult<String> {
        let mut buf = String::new();
        crate::nodes::render_list(rctx, body, &mut buf)?;
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

/// `PartialNode.render`.
pub(crate) fn partial(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::Partial {
        name, partials, token, ..
    } = p
    else {
        return Ok(());
    };
    let res = (|| -> PyResult<String> {
        let py = rctx.py();
        let record = partials.bind(py).get_item(name)?.ok_or_else(|| {
            syntax_error(
                py,
                &format!("Partial '{name}' is not defined in the current template."),
            )
        })?;
        let v = record.call_method1("render", (&rctx.ctx,))?;
        py_str_of(&v)
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
