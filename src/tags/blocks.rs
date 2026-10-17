//! Native block inheritance for all-native `{% extends %}` chains.
//!
//! Stock `ExtendsNode.render` collects `{name: BlockNode}` maps with
//! `get_nodes_by_type`, stores them in a `BlockContext` under
//! `render_context["block_context"]`, and `BlockNode.render` pops/pushes
//! per-name stacks while rendering. When every template in the chain is a
//! native dtl `Template` (same engine, debug off), the same algorithm runs here on Rust data:
//!
//! * `NativeBlockContext` mirrors `BlockContext` (per-name stacks,
//!   `add_blocks` inserts at the front, `pop`/`push` at the back) and lives
//!   in the current render-context frame under a private key, so it has the
//!   same frame scoping as stock's `block_context` (isolated includes and
//!   nested `Template.render` calls never see it).
//! * `BlockProxy` (Python name `BlockNode`) is what `{{ block }}` resolves
//!   to: `name`, `nodelist`, `context`, `render(context)` and `super()`
//!   with stock semantics.
//!
//! Python code cannot observe the difference: no stock code reads
//! `block_context` outside `loader_tags`, and templates whose bridged
//! (stock) nodes contain `BlockNode`s are excluded, so every block in the
//! chain is a native node. Any mismatch (non-native parent, debug, a stock
//! `block_context` already in the frame) falls back to the Python-object
//! path in `loader.rs`, converting a native context built by an outer
//! level first.

use std::collections::HashMap;

use pyo3::intern;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyDict, PyList, PyString};

use super::{with_scoped, TagKind, TagPayload};
use crate::engine::Template;
use crate::nodes::{self, Node, NodeList};
use crate::render::context::Rctx;
use crate::render::value::mark_safe_str;

/// One `{% block %}` node of a template: name, parse id (adapter map key)
/// and the address of its body `NodeList` (inside the node's boxed payload,
/// so stable for the template's lifetime; templates are immutable after
/// compile).
pub(crate) struct BlockEntry {
    pub name: String,
    pub id: usize,
    pub nl: usize,
    pub simple: bool,
}

/// Pre-order scan of every `{% block %}` in `list` (the set stock
/// `nodelist.get_nodes_by_type(BlockNode)` finds). `None` when a bridged
/// stock node contains a `BlockNode`: such blocks are Python objects that
/// need the stock `BlockContext`.
pub(crate) fn index_blocks(py: Python<'_>, list: &NodeList) -> PyResult<Option<Vec<BlockEntry>>> {
    let mut out = Vec::new();
    Ok(collect(py, list, &mut out)?.then_some(out))
}

fn collect(py: Python<'_>, list: &NodeList, out: &mut Vec<BlockEntry>) -> PyResult<bool> {
    for node in &list.nodes {
        match node {
            Node::Bridged(n) => {
                let found = n
                    .bind(py)
                    .call_method1("get_nodes_by_type", (stock_block_node(py)?,))
                    .and_then(|f| f.len());
                if !matches!(found, Ok(0)) {
                    return Ok(false);
                }
            }
            Node::AutoEscape { nodelist, .. } => {
                if !collect(py, nodelist, out)? {
                    return Ok(false);
                }
            }
            Node::Tag { payload, id, .. } => {
                if let TagPayload::Block { name, nodelist, simple, .. } = &**payload {
                    out.push(BlockEntry {
                        name: name.clone(),
                        id: *id,
                        nl: std::ptr::from_ref(nodelist) as usize,
                        simple: *simple,
                    });
                }
                for child in super::tag_child_nodelists(payload) {
                    if !collect(py, child, out)? {
                        return Ok(false);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(true)
}

fn stock_block_node(py: Python<'_>) -> PyResult<&Bound<'_, PyAny>> {
    static CLS: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    Ok(CLS
        .get_or_try_init(py, || -> PyResult<_> {
            Ok(py.import("django.template.loader_tags")?.getattr("BlockNode")?.unbind())
        })?
        .bind(py))
}

fn stock_block_context(py: Python<'_>) -> PyResult<&Bound<'_, PyAny>> {
    static CLS: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    Ok(CLS
        .get_or_try_init(py, || -> PyResult<_> {
            Ok(py.import("django.template.loader_tags")?.getattr("BlockContext")?.unbind())
        })?
        .bind(py))
}

/// Parse-time scan of a `{% block %}` body: `true` when the stock
/// `with context.push(): context["block"] = ...` around it is
/// unobservable, so a native render may skip the layer and the `block`
/// binding. Requires that nothing in the body mentions a `block` variable
/// (any lookup rooted at `block`), writes the top context layer (`as`
/// forms, `cycle`, `regroup`), or hands the context to code this scan
/// cannot see (bridged nodes, `include`, `partial`, `debug`). Tags that
/// push their own layer (`for`, `with`) are scanned through; nested blocks
/// push and bind their own.
pub(crate) fn body_is_simple(list: &NodeList) -> bool {
    list.nodes.iter().all(simple_node)
}

fn simple_node(node: &Node) -> bool {
    match node {
        Node::Text { .. } | Node::Verbatim { .. } | Node::Comment => true,
        Node::Var { fe, filters, .. } => fe_ok(fe) && filters.iter().all(|f| args_ok(&f.args)),
        Node::FlVar { fe, .. } => fe_ok(fe),
        Node::AutoEscape { nodelist, .. } => body_is_simple(nodelist),
        Node::Bridged(_) => false,
        Node::Tag { payload, .. } => match &**payload {
            TagPayload::Block { .. }
            | TagPayload::Templatetag { .. }
            | TagPayload::Load { .. }
            | TagPayload::Csrf { .. } => true,
            TagPayload::Now { asvar, .. } | TagPayload::Translate { asvar, .. } => asvar.is_none(),
            TagPayload::If { arms, .. } => arms
                .iter()
                .all(|(c, nl)| c.as_ref().is_none_or(ifexpr_ok) && body_is_simple(nl)),
            TagPayload::For { seq, body, empty, .. } => {
                expr_ok(seq) && body_is_simple(body) && body_is_simple(empty)
            }
            TagPayload::With { values, body, .. } => {
                values.iter().all(|(_, e)| expr_ok(e)) && body_is_simple(body)
            }
            TagPayload::Filter { fe, body, .. } => expr_ok(fe) && body_is_simple(body),
            TagPayload::Spaceless { body, .. } => body_is_simple(body),
            TagPayload::FirstOf { vars, asvar, .. } => asvar.is_none() && vars.iter().all(expr_ok),
            TagPayload::Lorem { count, .. } => expr_ok(count),
            TagPayload::WidthRatio { val, max, width, asvar, .. } => {
                asvar.is_none() && expr_ok(val) && expr_ok(max) && expr_ok(width)
            }
            TagPayload::Url { view, args, kwargs, asvar, .. } => {
                asvar.is_none()
                    && expr_ok(view)
                    && args.iter().all(expr_ok)
                    && kwargs.iter().all(|(_, e)| expr_ok(e))
            }
            TagPayload::SimpleTag { spec, args, kwargs, target_var, .. } => {
                !spec.takes_context
                    && target_var.is_none()
                    && args.iter().all(expr_ok)
                    && kwargs.iter().all(|(_, e)| expr_ok(e))
            }
            _ => false,
        },
    }
}

fn names_block(lookups: &[String], var: &str) -> bool {
    lookups.first().map_or(var == "block", |r| r == "block")
}

fn args_ok(args: &[crate::varparse::FilterArg]) -> bool {
    args.iter().all(|a| match a {
        crate::varparse::FilterArg::Lookup { var, lookups, .. } => !names_block(lookups, var),
        _ => true,
    })
}

fn fe_ok(fe: &crate::varparse::FilterExpression) -> bool {
    let var_ok = match &fe.var {
        crate::varparse::VarPart::Lookup { var, lookups, .. } => !names_block(lookups, var),
        _ => true,
    };
    var_ok && fe.filters.iter().all(|f| args_ok(&f.args))
}

fn expr_ok(e: &super::Expr) -> bool {
    fe_ok(&e.fe) && e.filters.iter().all(|f| args_ok(&f.args))
}

fn ifexpr_ok(e: &crate::ifexpr::IfExpr) -> bool {
    use crate::ifexpr::IfExpr as E;
    match e {
        E::Leaf(c) => expr_ok(c),
        E::Not(a) => ifexpr_ok(a),
        E::Or(a, b)
        | E::And(a, b)
        | E::In(a, b)
        | E::NotIn(a, b)
        | E::Is(a, b)
        | E::IsNot(a, b)
        | E::Eq(a, b)
        | E::Ne(a, b)
        | E::Gt(a, b)
        | E::Ge(a, b)
        | E::Lt(a, b)
        | E::Le(a, b) => ifexpr_ok(a) && ifexpr_ok(b),
    }
}

/// A block of a specific template: the owning template keeps `nl` alive
/// (every deref of `nl` happens while a `BlockRef` holding it is live).
pub(crate) struct BlockRef {
    tmpl: Py<Template>,
    id: usize,
    nl: usize,
    simple: bool,
}

impl BlockRef {
    fn clone_ref(&self, py: Python<'_>) -> Self {
        Self { tmpl: self.tmpl.clone_ref(py), id: self.id, nl: self.nl, simple: self.simple }
    }

    /// The Python adapter (`_nodes.BlockNode`) for this block.
    fn adapter<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let t = self.tmpl.bind(py);
        crate::engine::ensure_adapters(t)?;
        let map = t.getattr("_dtl_node_by_id")?;
        map.get_item(i64::try_from(self.id).unwrap_or(i64::MAX))
    }
}

/// Native `BlockContext` (see module docs).
#[pyclass(module = "django_rust_templates._dtl")]
pub struct NativeBlockContext {
    blocks: HashMap<String, Vec<BlockRef>>,
}

impl NativeBlockContext {
    fn add_blocks(&mut self, py: Python<'_>, tmpl: &Bound<'_, Template>, index: &[BlockEntry]) {
        for e in index {
            self.blocks.entry(e.name.clone()).or_default().insert(
                0,
                BlockRef { tmpl: tmpl.clone().unbind(), id: e.id, nl: e.nl, simple: e.simple },
            );
        }
        let _ = py;
    }

    fn pop(&mut self, name: &str) -> Option<BlockRef> {
        self.blocks.get_mut(name).and_then(Vec::pop)
    }

    fn push(&mut self, name: &str, block: BlockRef) {
        self.blocks.entry(name.to_string()).or_default().push(block);
    }

    fn has_block(&self, name: &str) -> bool {
        self.blocks.get(name).is_some_and(|v| !v.is_empty())
    }

    fn find(&self, py: Python<'_>, nl: usize) -> Option<BlockRef> {
        self.blocks
            .values()
            .flatten()
            .find(|r| r.nl == nl)
            .map(|r| r.clone_ref(py))
    }
}

/// `{{ block }}` in a native chain (Python name `BlockNode`, like the stock
/// class whose name appears in the `block.super` error message).
#[pyclass(module = "django_rust_templates._dtl", name = "BlockNode")]
pub struct BlockProxy {
    name: String,
    block: BlockRef,
    context: Option<Py<PyAny>>,
}

#[pymethods]
impl BlockProxy {
    #[getter]
    fn name(&self) -> &str {
        &self.name
    }

    /// The block's stock-protocol `NodeList` (the adapter view).
    #[getter]
    fn nodelist<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.block.adapter(py)?.getattr("nodelist")
    }

    /// Absent (AttributeError) on a block rendered without inheritance,
    /// exactly like stock's un-cloned `BlockNode`.
    #[getter]
    fn context(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.context.as_ref().map(|c| c.clone_ref(py)).ok_or_else(|| {
            pyo3::exceptions::PyAttributeError::new_err(
                "'BlockNode' object has no attribute 'context'",
            )
        })
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        Ok(format!(
            "<Block Node: {}. Contents: {}>",
            self.name,
            self.nodelist(py)?.repr()?
        ))
    }

    /// Stock `BlockNode.render(context)` with this block as `self`.
    fn render(slf: &Bound<'_, Self>, context: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let py = slf.py();
        let this = slf.borrow();
        let engine = this.block.tmpl.bind(py).borrow().engine_ref(py);
        let mut rctx = Rctx::new(context.clone(), engine)?;
        let mut out = String::new();
        match frame_mode(&rctx)? {
            Mode::Native(nbc) => {
                let name = this.name.clone();
                let own = this.block.clone_ref(py);
                drop(this);
                let popped = nbc.borrow_mut().pop(&name);
                let blk = match &popped {
                    Some(b) => b.clone_ref(py),
                    None => own,
                };
                render_inherited(&mut rctx, &nbc, &name, blk, popped, &mut out)?;
            }
            Mode::Python(_) => {
                drop(this);
                return stock_render(slf, context);
            }
            Mode::NoBc => {
                let nl = this.block.nl;
                drop(this);
                with_scoped(&mut rctx, &None, |rctx| {
                    rctx.set_item_key(intern!(py, "block"), slf.as_any())?;
                    // SAFETY: see `BlockRef::nodelist`; `slf` holds the ref.
                    nodes::render_list(rctx, unsafe { &*(nl as *const NodeList) }, &mut out)
                })?;
            }
        }
        Ok(mark_safe_str(py, &out)?.unbind())
    }

    /// Stock `BlockNode.super()`.
    #[pyo3(name = "super")]
    fn super_(slf: &Bound<'_, Self>) -> PyResult<Py<PyAny>> {
        let py = slf.py();
        let this = slf.borrow();
        let Some(ctx) = this.context.as_ref().map(|c| c.bind(py).clone()) else {
            return Err(crate::error::syntax_error(
                py,
                "'BlockNode' object has no attribute 'context'. Did you use \
                 {{ block.super }} in a base template?",
            ));
        };
        let rc = ctx.getattr(intern!(py, "render_context"))?;
        let frame = top_frame(&rc)?;
        let has = if let Some(v) = frame.get_item(native_key(py))? {
            match v.cast::<NativeBlockContext>() {
                Ok(n) => n.borrow().has_block(&this.name),
                Err(_) => false,
            }
        } else {
            match frame.get_item(intern!(py, "block_context"))? {
                Some(bc) => !bc.call_method1("get_block", (&this.name,))?.is_none(),
                None => false,
            }
        };
        drop(this);
        if has {
            // `mark_safe(self.render(self.context))`: `render` already
            // returns a `SafeString`, which `mark_safe` returns unchanged.
            return Self::render(slf, &ctx);
        }
        Ok(PyString::new(py, "").into_any().unbind())
    }
}

/// Stock `BlockNode.render` against a stock `BlockContext` (the frame holds
/// Python block objects): run the stock method on a stock `BlockNode` with
/// this block's name and nodelist.
fn stock_render(slf: &Bound<'_, BlockProxy>, context: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let py = slf.py();
    let (name, nl) = {
        let this = slf.borrow();
        (this.name.clone(), this.block.adapter(py)?.getattr("nodelist")?)
    };
    let node = stock_block_node(py)?.call1((name, nl))?;
    Ok(node.call_method1(intern!(py, "render"), (context,))?.unbind())
}

fn native_key(py: Python<'_>) -> &Bound<'_, PyString> {
    intern!(py, "_dtl_native_block_context")
}

/// `render_context.dicts[-1]` (stock frames are plain dicts).
fn top_frame<'py>(rc: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyDict>> {
    let py = rc.py();
    let dicts = rc.getattr(intern!(py, "dicts"))?.cast_into::<PyList>()?;
    let n = dicts.len();
    if n == 0 {
        return Err(pyo3::exceptions::PyIndexError::new_err("list index out of range"));
    }
    Ok(dicts.get_item(n - 1)?.cast_into::<PyDict>()?)
}

enum Mode<'py> {
    Native(Bound<'py, NativeBlockContext>),
    Python(#[allow(dead_code)] Bound<'py, PyAny>),
    NoBc,
}

/// Which block context the current render-context frame carries.
fn frame_mode<'py>(rctx: &Rctx<'py>) -> PyResult<Mode<'py>> {
    let py = rctx.py();
    let frame = rctx.rc_frame()?.cast_into::<PyDict>()?;
    if let Some(v) = frame.get_item(native_key(py))? {
        if let Ok(n) = v.cast_into::<NativeBlockContext>() {
            return Ok(Mode::Native(n));
        }
    }
    match frame.get_item(intern!(py, "block_context"))? {
        Some(bc) if !bc.is_none() => Ok(Mode::Python(bc)),
        _ => Ok(Mode::NoBc),
    }
}

/// The `BlockRef` for the block node whose body is at `nl`: the template
/// on `render_context.template` first, then any template in the chain.
fn resolve_owner(
    rctx: &Rctx<'_>,
    nl: usize,
    nbc: Option<&Bound<'_, NativeBlockContext>>,
) -> PyResult<Option<BlockRef>> {
    let py = rctx.py();
    let cur = rctx.render_context()?.getattr(intern!(py, "template"))?;
    if let Ok(t) = cur.cast_exact::<Template>() {
        if let Some(idx) = Template::block_index(t)? {
            if let Some(e) = idx.iter().find(|e| e.nl == nl) {
                return Ok(Some(BlockRef { tmpl: t.clone().unbind(), id: e.id, nl, simple: e.simple }));
            }
        }
    }
    Ok(nbc.and_then(|n| n.borrow().find(py, nl)))
}

/// Native `BlockNode.render` for a node met while walking a nodelist.
/// `Ok(false)` = not handled (debug render, stock `block_context` in the
/// frame, or an owner that can't be resolved); the caller takes the
/// Python-object path.
pub(crate) fn try_block(
    rctx: &mut Rctx<'_>,
    name: &str,
    nodelist: &NodeList,
    simple: bool,
    out: &mut String,
) -> PyResult<bool> {
    if rctx.dbg.is_some() {
        return Ok(false);
    }
    let py = rctx.py();
    let nl = std::ptr::from_ref(nodelist) as usize;
    match frame_mode(rctx)? {
        Mode::Python(_) => Ok(false),
        Mode::Native(nbc) => {
            let popped = nbc.borrow_mut().pop(name);
            // Simple body (see `body_is_simple`): no layer, no binding.
            let (body_nl, body_simple) = match &popped {
                Some(b) => (b.nl, b.simple),
                None => (nl, simple),
            };
            if body_simple {
                // SAFETY: `popped` (or the node being walked) keeps the
                // owning template alive; see `BlockRef`.
                nodes::render_list(rctx, unsafe { &*(body_nl as *const NodeList) }, out)?;
                if let Some(p) = popped {
                    nbc.borrow_mut().push(name, p);
                }
                return Ok(true);
            }
            let blk = match popped {
                Some(b) => {
                    let r = b.clone_ref(py);
                    render_inherited(rctx, &nbc, name, r, Some(b), out)?;
                    return Ok(true);
                }
                None => match resolve_owner(rctx, nl, Some(&nbc))? {
                    Some(o) => o,
                    None => return Ok(false),
                },
            };
            render_inherited(rctx, &nbc, name, blk, None, out)?;
            Ok(true)
        }
        Mode::NoBc => {
            if simple {
                nodes::render_list(rctx, nodelist, out)?;
                return Ok(true);
            }
            let Some(own) = resolve_owner(rctx, nl, None)? else {
                return Ok(false);
            };
            let proxy = Py::new(py, BlockProxy { name: name.to_string(), block: own, context: None })?;
            with_scoped(rctx, &None, |rctx| {
                rctx.set_item_key(intern!(py, "block"), proxy.bind(py).as_any())?;
                nodes::render_list(rctx, nodelist, out)
            })?;
            Ok(true)
        }
    }
}

/// The inheritance branch of `BlockNode.render`: `block` is the popped
/// override (or the node itself when the stack is empty); `popped` is
/// pushed back after a successful render, like stock.
fn render_inherited(
    rctx: &mut Rctx<'_>,
    nbc: &Bound<'_, NativeBlockContext>,
    name: &str,
    block: BlockRef,
    popped: Option<BlockRef>,
    out: &mut String,
) -> PyResult<()> {
    let py = rctx.py();
    let nl = block.nl;
    let proxy = Py::new(
        py,
        BlockProxy { name: name.to_string(), block, context: Some(rctx.ctx.clone().unbind()) },
    )?;
    with_scoped(rctx, &None, |rctx| {
        rctx.set_item_key(intern!(py, "block"), proxy.bind(py).as_any())?;
        // SAFETY: see `BlockRef::nodelist`; `proxy` holds the template.
        nodes::render_list(rctx, unsafe { &*(nl as *const NodeList) }, out)?;
        if let Some(p) = popped {
            nbc.borrow_mut().push(name, p);
        }
        Ok(())
    })
}

/// Native `ExtendsNode.render` tail (after `get_parent`): `Ok(None)` when
/// the chain is not eligible (see module docs); the caller then runs the
/// stock-object path. `p` is the extends payload being rendered.
pub(crate) fn try_extends(
    rctx: &mut Rctx<'_>,
    p: &TagPayload,
    compiled_parent: &Bound<'_, PyAny>,
) -> PyResult<Option<String>> {
    let py = rctx.py();
    if rctx.dbg.is_some() {
        return Ok(None);
    }
    let Ok(parent) = compiled_parent.cast_exact::<Template>() else {
        return Ok(None);
    };
    if parent.borrow().engine_ref(py).as_ptr() != rctx.engine.as_ptr() {
        return Ok(None);
    }
    let rc = rctx.render_context()?;
    let cur = rc.getattr(intern!(py, "template"))?;
    let Ok(cur_t) = cur.cast_exact::<Template>() else {
        return Ok(None);
    };
    // The extends node must belong to `render_context.template` (stock
    // reads its blocks off the node itself; ours come from that template).
    let owns = cur_t.borrow().top_nodes().iter().any(|n| {
        matches!(n, Node::Tag { kind: TagKind::Extends, payload, .. } if std::ptr::eq(&**payload, p))
    });
    if !owns {
        return Ok(None);
    }
    let Some(child_idx) = Template::block_index(cur_t)? else {
        return Ok(None);
    };
    let Some(parent_idx) = Template::block_index(parent)? else {
        return Ok(None);
    };
    let frame = rctx.rc_frame()?.cast_into::<PyDict>()?;
    if frame.contains(intern!(py, "block_context"))? {
        return Ok(None);
    }
    let nbc = match frame.get_item(native_key(py))? {
        Some(v) => match v.cast_into::<NativeBlockContext>() {
            Ok(n) => n,
            Err(_) => return Ok(None),
        },
        None => {
            let n = Bound::new(py, NativeBlockContext { blocks: HashMap::new() })?;
            frame.set_item(native_key(py), &n)?;
            n
        }
    };
    {
        let mut b = nbc.borrow_mut();
        b.add_blocks(py, cur_t, &child_idx);
        // Stock: parent blocks are added unless the parent's first
        // non-text node is itself an `{% extends %}` (it adds them then).
        let pt = parent.borrow();
        for node in pt.top_nodes() {
            match node {
                Node::Text { .. } => {}
                Node::Tag { kind: TagKind::Extends, .. } => break,
                _ => {
                    b.add_blocks(py, parent, &parent_idx);
                    break;
                }
            }
        }
    }
    // `with render_context.push_state(parent, isolated_context=False):
    //     return parent._render(context)`
    // An instrumented `_render` (the test runner's signal-sending
    // `instrumented_test_render`) is called as such; its nodelist render
    // reaches the parent's blocks through the adapters, which land in
    // `try_block` against the same frame.
    rc.setattr(intern!(py, "template"), parent)?;
    let mut out = String::new();
    let res = if crate::engine::template_render_unpatched(py)? {
        let pt = parent.borrow();
        nodes::render_list(rctx, pt.nodelist_ref(), &mut out)
    } else {
        parent
            .call_method1(intern!(py, "_render"), (&rctx.ctx,))
            .and_then(|v| super::py_str_of(&v))
            .map(|s| out = s)
    };
    rc.setattr(intern!(py, "template"), &cur)?;
    res?;
    Ok(Some(out))
}

/// Before the stock-object extends path runs in a frame where an outer
/// level built a `NativeBlockContext`: replace it with the equivalent
/// stock `BlockContext` (same per-name stacks, adapter nodes in place of
/// native refs). No block has rendered yet at that point (extends is the
/// first non-text node), so no `BlockProxy` refers to it.
pub(crate) fn demote_to_stock(rctx: &Rctx<'_>) -> PyResult<()> {
    let py = rctx.py();
    let frame = rctx.rc_frame()?.cast_into::<PyDict>()?;
    let Some(v) = frame.get_item(native_key(py))? else {
        return Ok(());
    };
    let Ok(nbc) = v.cast_into::<NativeBlockContext>() else {
        return Ok(());
    };
    let bc = stock_block_context(py)?.call0()?;
    let blocks = bc.getattr("blocks")?;
    for (name, refs) in &nbc.borrow().blocks {
        let list = PyList::empty(py);
        for r in refs {
            list.append(r.adapter(py)?)?;
        }
        blocks.set_item(name, list)?;
    }
    frame.del_item(native_key(py))?;
    if !frame.contains(intern!(py, "block_context"))? {
        frame.set_item(intern!(py, "block_context"), bc)?;
    }
    Ok(())
}
