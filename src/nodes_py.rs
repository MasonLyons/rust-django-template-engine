//! Python-visible node tree: `Template.nodelist` returns a real
//! `django.template.base.NodeList` (subclass) of duck-typed adapter nodes so
//! that tree-walking code (`ExtendsNode`'s parent scan, `get_nodes_by_type`
//! suite tests, `node.origin` checks) works unchanged. Bridged Python nodes
//! pass through as themselves; Rust-native nodes are wrapped by adapter
//! classes in `django_rust_templates._nodes` whose `render` delegates back
//! into the Rust renderer via `Template._render_native(pos, context)`,
//! where `pos` is the node's position in the pre-order traversal of native
//! nodes (assigned once, when the adapter list is built and cached).

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyDict, PyList, PyTuple, PyType};

use crate::nodes::{Node, NodeList};

/// Lowercase tag name for adapter `__repr__`s (stock reprs name the tag).
fn kind_name(kind: crate::tags::TagKind) -> &'static str {
    use crate::tags::TagKind as K;
    match kind {
        K::Block => "block",
        K::Extends => "extends",
        K::Include => "include",
        K::Load => "load",
        K::If => "if",
        K::IfChanged => "ifchanged",
        K::For => "for",
        K::With => "with",
        K::Filter => "filter",
        K::FirstOf => "firstof",
        K::Cycle => "cycle",
        K::ResetCycle => "resetcycle",
        K::Spaceless => "spaceless",
        K::Templatetag => "templatetag",
        K::Now => "now",
        K::Lorem => "lorem",
        K::Url => "url",
        K::WidthRatio => "widthratio",
        K::Regroup => "regroup",
        K::Csrf => "csrf_token",
        K::Debug => "debug",
        K::PartialDef => "partialdef",
        K::Partial => "partial",
        K::SimpleTag => "simple_tag",
        K::InclusionTag => "inclusion_tag",
        K::Translate => "translate",
        K::Static => "static",
        K::BlockTranslate => "blocktranslate",
    }
}

/// Adapter class registry (`django_rust_templates._nodes`), initialized once.
struct Adapters {
    node_list: Py<PyType>,
    body_list: Py<PyType>,
    text: Py<PyType>,
    var: Py<PyType>,
    autoescape: Py<PyType>,
    comment: Py<PyType>,
    verbatim: Py<PyType>,
    tag_generic: Py<PyType>,
    tag_block: Py<PyType>,
    tag_extends: Py<PyType>,
    tag_simple: Py<PyType>,
    tag_inclusion: Py<PyType>,
}

impl Adapters {
    fn clone_ref(&self, py: Python<'_>) -> Self {
        Self {
            node_list: self.node_list.clone_ref(py),
            body_list: self.body_list.clone_ref(py),
            text: self.text.clone_ref(py),
            var: self.var.clone_ref(py),
            autoescape: self.autoescape.clone_ref(py),
            comment: self.comment.clone_ref(py),
            verbatim: self.verbatim.clone_ref(py),
            tag_generic: self.tag_generic.clone_ref(py),
            tag_block: self.tag_block.clone_ref(py),
            tag_extends: self.tag_extends.clone_ref(py),
            tag_simple: self.tag_simple.clone_ref(py),
            tag_inclusion: self.tag_inclusion.clone_ref(py),
        }
    }
}

fn adapters(py: Python<'_>) -> PyResult<Adapters> {
    static A: PyOnceLock<Adapters> = PyOnceLock::new();
    A.get_or_try_init(py, || -> PyResult<Adapters> {
        let m = py.import("django_rust_templates._nodes")?;
        Ok(Adapters {
            node_list: m.getattr("NodeListAdapter")?.cast_into::<PyType>()?.unbind(),
            body_list: m.getattr("NativeBodyNodeList")?.cast_into::<PyType>()?.unbind(),
            text: m.getattr("TextNode")?.cast_into::<PyType>()?.unbind(),
            var: m.getattr("VariableNode")?.cast_into::<PyType>()?.unbind(),
            autoescape: m.getattr("AutoEscapeControlNode")?.cast_into::<PyType>()?.unbind(),
            comment: m.getattr("CommentNode")?.cast_into::<PyType>()?.unbind(),
            verbatim: m.getattr("VerbatimNode")?.cast_into::<PyType>()?.unbind(),
            tag_generic: m.getattr("NativeTagNode")?.cast_into::<PyType>()?.unbind(),
            tag_block: m.getattr("BlockNode")?.cast_into::<PyType>()?.unbind(),
            tag_extends: m.getattr("ExtendsNode")?.cast_into::<PyType>()?.unbind(),
            tag_simple: m.getattr("SimpleNode")?.cast_into::<PyType>()?.unbind(),
            tag_inclusion: m.getattr("InclusionNode")?.cast_into::<PyType>()?.unbind(),
        })
    })
    .map(|a| a.clone_ref(py))
}

/// Build the Python `NodeList` view of a Rust `NodeList`. `template` is the
/// owning dtl `Template` (adapter nodes keep a handle for render
/// delegation); `origin` is copied onto every adapter node (Django sets
/// `node.origin` in `extend_nodelist`).
pub fn build_py_nodelist(
    py: Python<'_>,
    template: &Bound<'_, PyAny>,
    origin: &Bound<'_, PyAny>,
    list: &NodeList,
) -> PyResult<Py<PyList>> {
    let a = adapters(py)?;
    let (nl, id_map) = build_nodes(py, template, origin, list, &a, a.node_list.bind(py))?;
    template.setattr("_dtl_node_by_id", &id_map)?;
    Ok(nl.cast_into::<PyList>()?.unbind())
}

/// Adapter list of `list` as an instance of `cls`, plus the `{id: adapter}`
/// map of its native tag nodes. `owner` is what adapters delegate their
/// render to (`_render_native`).
fn build_nodes<'py>(
    py: Python<'py>,
    owner: &Bound<'py, PyAny>,
    origin: &Bound<'py, PyAny>,
    list: &NodeList,
    a: &Adapters,
    cls: &Bound<'py, PyType>,
) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyDict>)> {
    let mut counter = 0usize;
    let id_map = PyDict::new(py);
    let out = PyList::empty(py);
    convert_list(py, &mut counter, &id_map, owner, origin, list, a, &out)?;
    let nl = cls.call1((out,))?;
    nl.setattr("contains_nontext", list.contains_nontext)?;
    Ok((nl, id_map))
}

/// A nodelist the Rust parser compiled on behalf of a bridged compile
/// function (`parser.parse()` on the shim, see `parser::ParseHook`). The
/// stock node holds it as `_nodes.NativeBodyNodeList`, whose `render`
/// lands in `_render` here, so the body renders natively against the live
/// context. The owning `Template` keeps a handle too (`bodies`): native
/// `{% block %}` adapters and `{% partialdef %}` bodies inside it must be
/// reachable from the template.
#[pyclass(module = "django_rust_templates._dtl")]
pub struct NativeBody {
    pub(crate) nodelist: NodeList,
    engine: Py<PyAny>,
    origin: Py<PyAny>,
    /// Template source (debug engines only), for `Rctx::dbg` annotation.
    dbg_source: Option<std::sync::Arc<str>>,
    /// `{id: adapter}` for the native tag nodes in this body, set when the
    /// adapters are built; merged into the template's `_dtl_node_by_id`.
    pub(crate) id_map: PyOnceLock<Py<PyDict>>,
}

impl NativeBody {
    fn dbg(&self, py: Python<'_>) -> Option<(String, Py<PyAny>)> {
        self.dbg_source
            .as_ref()
            .map(|s| (s.to_string(), self.origin.clone_ref(py)))
    }
}

#[pymethods]
impl NativeBody {
    /// `NodeList.render(context)` of the whole body.
    fn _render(&self, py: Python<'_>, context: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let mut rctx =
            crate::render::context::Rctx::new(context.clone(), self.engine.bind(py).clone())?;
        rctx.dbg = self.dbg(py);
        let mut out = String::new();
        crate::nodes::render_list(&mut rctx, &self.nodelist, &mut out)?;
        Ok(crate::render::value::mark_safe_str(py, &out)?.unbind())
    }

    /// Adapter render delegation (same contract as `Template._render_native`).
    fn _render_native(&self, py: Python<'_>, pos: usize, context: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let engine = self.engine.bind(py).clone();
        render_native(py, &self.nodelist, &engine, pos, context, self.dbg(py))
    }

    /// The adapter nodes for `NativeBodyNodeList._dtl_fill` (called once).
    fn _adapters(slf: &Bound<'_, Self>) -> PyResult<Py<PyList>> {
        let py = slf.py();
        let a = adapters(py)?;
        let b = slf.borrow();
        let out = PyList::empty(py);
        let id_map = PyDict::new(py);
        let mut counter = 0usize;
        convert_list(py, &mut counter, &id_map, slf.as_any(), b.origin.bind(py), &b.nodelist, &a, &out)?;
        let _ = b.id_map.set(py, id_map.unbind());
        Ok(out.unbind())
    }
}

/// Wrap a body compiled for a bridged compile function: the
/// `NativeBodyNodeList` it gets back, and the `NativeBody` owning the nodes.
/// Adapters are built lazily, except for bodies holding a `{% block %}`:
/// block renders look their adapter up in the template's id map, which
/// `Template` fills from `NativeBody::id_map` before the first render.
pub fn wrap_body<'py>(
    py: Python<'py>,
    nodelist: NodeList,
    engine: &Bound<'py, PyAny>,
    origin: &Bound<'py, PyAny>,
    dbg_source: Option<&std::sync::Arc<str>>,
) -> PyResult<(Bound<'py, PyAny>, Py<NativeBody>)> {
    let a = adapters(py)?;
    let eager = contains_block(&nodelist);
    let contains_nontext = nodelist.contains_nontext;
    let body = Bound::new(
        py,
        NativeBody {
            nodelist,
            engine: engine.clone().unbind(),
            origin: origin.clone().unbind(),
            dbg_source: dbg_source.cloned(),
            id_map: PyOnceLock::new(),
        },
    )?;
    let nl = a.body_list.bind(py).call1((PyList::empty(py),))?;
    nl.setattr("contains_nontext", contains_nontext)?;
    nl.setattr("_dtl_body", &body)?;
    if eager {
        nl.call_method0("_dtl_fill")?;
    } else {
        nl.setattr("_dtl_lazy", true)?;
    }
    Ok((nl, body.unbind()))
}

/// Any native `{% block %}` in `list` (bodies of nested bridged tags are
/// separate `NativeBody`s with their own check).
fn contains_block(list: &NodeList) -> bool {
    list.nodes.iter().any(|n| match n {
        Node::AutoEscape { nodelist, .. } => contains_block(nodelist),
        Node::Tag { kind, payload, .. } => {
            *kind == crate::tags::TagKind::Block
                || crate::tags::tag_child_nodelists(payload).into_iter().any(contains_block)
        }
        _ => false,
    })
}

#[allow(clippy::too_many_arguments)] // threads builder state through the tree
fn convert_list(
    py: Python<'_>,
    counter: &mut usize,
    id_map: &Bound<'_, PyDict>,
    template: &Bound<'_, PyAny>,
    origin: &Bound<'_, PyAny>,
    list: &NodeList,
    a: &Adapters,
    out: &Bound<'_, PyList>,
) -> PyResult<()> {
    for node in &list.nodes {
        convert(py, counter, id_map, template, origin, node, a, out)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // threads builder state through the tree
fn convert(
    py: Python<'_>,
    counter: &mut usize,
    id_map: &Bound<'_, PyDict>,
    template: &Bound<'_, PyAny>,
    origin: &Bound<'_, PyAny>,
    node: &Node,
    a: &Adapters,
    out: &Bound<'_, PyList>,
) -> PyResult<()> {
    let obj = match node {
        Node::Bridged(n) => {
            out.append(n.bind(py).clone())?;
            return Ok(());
        }
        Node::Text { s } => a.text.bind(py).call1((template, s))?,
        Node::Var { .. } | Node::FlVar { .. } => a.var.bind(py).call1((template,))?,
        Node::AutoEscape { setting, nodelist } => {
            let pos = *counter;
            *counter += 1;
            let inner = PyList::empty(py);
            convert_list(py, counter, id_map, template, origin, nodelist, a, &inner)?;
            let inner_nl = a.node_list.bind(py).call1((inner,))?;
            inner_nl.setattr("contains_nontext", nodelist.contains_nontext)?;
            let node = a
                .autoescape
                .bind(py)
                .call1((template, *setting, inner_nl))?;
            node.setattr("_render_id", pos)?;
            node.setattr("origin", origin)?;
            out.append(node)?;
            return Ok(());
        }
        Node::Comment => a.comment.bind(py).call1((template,))?,
        Node::Verbatim { s } => a.verbatim.bind(py).call1((template, s))?,
        Node::Tag { kind, payload, id } => {
            return convert_tag(py, counter, id_map, template, origin, a, out, *kind, payload, *id);
        }
    };
    obj.setattr("_render_id", *counter)?;
    *counter += 1;
    obj.setattr("origin", origin)?;
    out.append(obj)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)] // mirrors convert's threading of builder state
#[allow(clippy::too_many_lines)] // one block per adapter shape
fn convert_tag(
    py: Python<'_>,
    counter: &mut usize,
    id_map: &Bound<'_, PyDict>,
    template: &Bound<'_, PyAny>,
    origin: &Bound<'_, PyAny>,
    a: &Adapters,
    out: &Bound<'_, PyList>,
    kind: crate::tags::TagKind,
    payload: &crate::tags::TagPayload,
    id: usize,
) -> PyResult<()> {
    let pos = *counter;
    *counter += 1;
    let children = crate::tags::tag_child_nodelists(payload);
    // For/IfChanged keep their stock per-arm attributes.
    let loop_names: &[&str] = match payload {
        crate::tags::TagPayload::For { .. } => &["nodelist_loop", "nodelist_empty"],
        crate::tags::TagPayload::IfChanged { .. } => &["nodelist_true", "nodelist_false"],
        _ => &[],
    };
    let mut loop_lists: Vec<Bound<'_, PyAny>> = Vec::new();
    let mut merged: Vec<Bound<'_, PyAny>> = Vec::new();
    for child in &children {
        let inner = PyList::empty(py);
        convert_list(py, counter, id_map, template, origin, child, a, &inner)?;
        let nl = a.node_list.bind(py).call1((inner.clone(),))?;
        nl.setattr("contains_nontext", child.contains_nontext)?;
        if loop_names.is_empty() {
            merged.extend(inner.iter());
        } else {
            loop_lists.push(nl);
        }
    }
    let merged_nl = if children.is_empty() || !loop_names.is_empty() {
        None
    } else {
        let m = PyList::new(py, merged.iter())?;
        let nl = a.node_list.bind(py).call1((m,))?;
        nl.setattr("contains_nontext", true)?;
        Some(nl)
    };
    // Stock ctor shapes: Block/Extends adapters must satisfy
    // loader.rs (name/nodelist/_dtl_template) and get_nodes_by_type.
    let adapter = match payload {
        crate::tags::TagPayload::Block { name, .. } => a
            .tag_block
            .bind(py)
            .call1((template, name.as_str(), merged_nl.clone()))?,
        crate::tags::TagPayload::Extends { .. } => a
            .tag_extends
            .bind(py)
            .call1((template, merged_nl.clone()))?,
        crate::tags::TagPayload::SimpleTag { spec, target_var, .. } => a.tag_simple.bind(py).call1((
            template,
            spec.func.bind(py),
            spec.takes_context,
            target_var.as_deref(),
        ))?,
        crate::tags::TagPayload::InclusionTag { spec, .. } => a.tag_inclusion.bind(py).call1((
            template,
            spec.func.bind(py),
            spec.takes_context,
            spec.filename.as_ref().map(|f| f.bind(py)),
        ))?,
        _ => a
            .tag_generic
            .bind(py)
            .call1((template, format!("<{}>", kind_name(kind))))?,
    };
    if let Some(nl) = &merged_nl {
        adapter.setattr("nodelist", nl)?;
    }
    for (name, nl) in loop_names.iter().zip(loop_lists.iter()) {
        adapter.setattr(*name, nl)?;
    }
    if matches!(payload, crate::tags::TagPayload::Extends { .. }) {
        adapter.setattr("_dtl_extends", true)?;
    }
    let child_names: Vec<&str> = if loop_names.is_empty() {
        if children.is_empty() {
            vec![]
        } else {
            vec!["nodelist"]
        }
    } else {
        loop_names.to_vec()
    };
    adapter.setattr("child_nodelists", PyTuple::new(py, child_names)?)?;
    adapter.setattr("_render_id", pos)?;
    adapter.setattr("origin", origin)?;
    id_map.set_item(i64::try_from(id).unwrap_or(i64::MAX), &adapter)?;
    out.append(adapter)?;
    Ok(())
}

/// Locate the native node at pre-order position `pos` (same traversal order
/// as `build_py_nodelist`; bridged nodes are transparent). Tag children are
/// visited through `tag_child_nodelists` — the exact order `convert` used.
fn locate<'a>(list: &'a NodeList, counter: &mut usize, pos: usize) -> Option<&'a Node> {
    for node in &list.nodes {
        if matches!(node, Node::Bridged(_)) {
            continue;
        }
        if *counter == pos {
            return Some(node);
        }
        *counter += 1;
        let children: Vec<&NodeList> = match node {
            Node::AutoEscape { nodelist, .. } => vec![nodelist],
            Node::Tag { payload, .. } => crate::tags::tag_child_nodelists(payload),
            _ => Vec::new(),
        };
        for child in children {
            if let Some(inner) = locate(child, counter, pos) {
                return Some(inner);
            }
        }
    }
    None
}

/// `Template._render_native(pos, context)`: render the single native node at
/// pre-order position `pos` (used by adapter nodes' render delegation).
pub fn render_native(
    py: Python<'_>,
    nodelist: &NodeList,
    engine: &Bound<'_, PyAny>,
    pos: usize,
    context: &Bound<'_, PyAny>,
    dbg: Option<(String, Py<PyAny>)>,
) -> PyResult<Py<PyAny>> {
    let mut counter = 0usize;
    let node = locate(nodelist, &mut counter, pos).ok_or_else(|| {
        pyo3::exceptions::PyKeyError::new_err(format!("native node {pos}"))
    })?;
    let mut rctx = crate::render::context::Rctx::new(context.clone(), engine.clone())?;
    rctx.dbg = dbg;
    let mut out = String::new();
    crate::nodes::render_node(&mut rctx, node, &mut out)?;
    Ok(crate::render::value::mark_safe_str(py, &out)?.unbind())
}
