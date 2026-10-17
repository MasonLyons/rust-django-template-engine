//! Node tree and rendering — ports of `Node`/`NodeList`/`TextNode`/
//! `VariableNode`/`AutoEscapeControlNode`/`CommentNode`.
//!
//! Rendering appends into a `String`; the final `Template.render` wraps the
//! result with `mark_safe`, mirroring `NodeList.render`'s `SafeString` join.
use pyo3::exceptions::PyUnicodeDecodeError;
use pyo3::prelude::*;

use crate::lexer::Token;

use crate::render::context::Rctx;
use crate::render::value::{self, BoundFilter};
use crate::tags::{TagKind, TagPayload};
use crate::varparse::FilterExpression;

pub struct NodeList {
    pub nodes: Vec<Node>,
    /// `NodeList.contains_nontext` — set by the parser's extend step.
    pub contains_nontext: bool,
}

impl NodeList {
    #[must_use]
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            contains_nontext: false,
        }
    }
}

impl Default for NodeList {
    fn default() -> Self {
        Self::new()
    }
}

/// A per-iteration `forloop` counter key rendered directly as
/// `{{ forloop.counter }}`-style output. Parse time rewrites such `Var`
/// nodes (inside the owning `{% for %}` body only) into `Node::FlVar`,
/// which reads the loop's native slot stack instead of the `forloop`
/// dict; the generic dict path stays the fallback (empty slot stack,
/// thousand-separator grouping enabled).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlKey {
    Counter0,
    Counter,
    Revcounter,
    Revcounter0,
    First,
    Last,
}

pub enum Node {
    Text {
        s: String,
    },
    Var {
        fe: FilterExpression,
        filters: Vec<BoundFilter>,
        /// The source token, kept for `render_annotated`-style annotation.
        token: Token,
    },
    /// `{{ forloop.counter }}`-family output node (see `FlKey`). `fe` is
    /// the original `Var`'s expression: the Python adapter conversion
    /// renders through it, and it is the generic fallback whenever the
    /// native slot stack is unavailable or int grouping is enabled.
    FlVar {
        key: FlKey,
        fe: FilterExpression,
        token: Token,
    },
    /// `{% autoescape on|off %}` (`AutoEscapeControlNode`).
    AutoEscape {
        setting: bool,
        nodelist: Box<NodeList>,
    },
    /// `{% verbatim %}` — body rendered once against an empty `Context` at
    /// parse time (`VerbatimNode`); the output is constant text.
    Verbatim {
        s: String,
    },
    /// `{% comment %}...{% endcomment %}`.
    Comment,
    /// Bridged (stock) node — e.g. `simple_tag`/`inclusion_tag` nodes from a
    /// third-party library — rendered via `render_annotated`.
    Bridged(Py<PyAny>),
    /// A natively compiled built-in block tag (`{% if %}`, `{% for %}`, …).
    /// `id` is the pre-order counter shared with the Python adapter builders.
    Tag {
        kind: TagKind,
        payload: Box<TagPayload>,
        id: usize,
    },
}

/// Render `list`, appending output to `out` (the `SafeString("".join(...))`
/// of `NodeList.render`, without the intermediate allocations).
pub fn render_list(rctx: &mut Rctx<'_>, list: &NodeList, out: &mut String) -> PyResult<()> {
    for node in &list.nodes {
        render_node(rctx, node, out)?;
    }
    Ok(())
}

pub fn render_node(rctx: &mut Rctx<'_>, node: &Node, out: &mut String) -> PyResult<()> {
    match node {
        Node::Text { s } | Node::Verbatim { s } => {
            out.push_str(s);
            Ok(())
        }
        Node::Comment => Ok(()),
        Node::AutoEscape {
            setting,
            nodelist,
        } => {
            // AutoEscapeControlNode.render: mutate/restore context.autoescape.
            let ctx = rctx.ctx.clone();
            let old = ctx.getattr("autoescape")?;
            ctx.setattr("autoescape", *setting)?;
            rctx.set_autoescape_cache(Some(*setting));
            let res = render_list(rctx, nodelist, out);
            rctx.set_autoescape_cache(None);
            ctx.setattr("autoescape", old)?;
            res
        }
        Node::Var { fe, filters, token } => {
            // VariableNode.render: resolve + render_value_in_context, with
            // UnicodeDecodeError swallowing to "".
            match resolve_and_stringify(rctx, fe, filters) {
                Ok(s) => {
                    out.push_str(&s);
                    Ok(())
                }
                Err(e) if e.is_instance_of::<PyUnicodeDecodeError>(rctx.py()) => Ok(()),
                Err(e) => {
                    annotate_error(rctx, &e, token);
                    Err(e)
                }
            }
        }
        Node::FlVar { key, fe, token } => match rctx.forstack.last().copied() {
            // Native slot read: the owning loop is on the slot stack.
            // Counters render as `str(int)` (exact-int dict entries under
            // stock's numberformat fast path) unless grouping is on;
            // bools are the literal `str(True)`/`str(False)` (escaping is
            // identity on them either way).
            Some(st) => match key {
                FlKey::First => {
                    out.push_str(if st.i == 0 { "True" } else { "False" });
                    Ok(())
                }
                FlKey::Last => {
                    out.push_str(if st.i + 1 == st.len { "True" } else { "False" });
                    Ok(())
                }
                _ => {
                    if rctx.int_str_is_plain()? {
                        let v = match key {
                            FlKey::Counter0 => st.i,
                            FlKey::Counter => st.i + 1,
                            FlKey::Revcounter => st.len - st.i,
                            _ => st.len - st.i - 1,
                        };
                        out.push_str(&v.to_string());
                        Ok(())
                    } else {
                        // Grouped ints must go through stock's numberformat
                        // on the (current) dict entry: generic path.
                        fl_generic(rctx, fe, token, out)
                    }
                }
            },
            None => fl_generic(rctx, fe, token, out),
        },
        Node::Bridged(n) => {
            // Node.render_annotated: stock error annotation happens inside.
            let r = n.bind(rctx.py()).call_method1("render_annotated", (&rctx.ctx,));
            rctx.invalidate_flags();
            let s = r?;
            out.push_str(&s.to_string());
            Ok(())
        }
        Node::Tag { kind, payload, id } => {
            crate::tags::render_tag(rctx, *kind, payload, *id, out)
        }
    }
}
fn resolve_and_stringify(
    rctx: &mut Rctx<'_>,
    fe: &FilterExpression,
    filters: &[BoundFilter],
) -> PyResult<String> {
    let obj = value::filter_expression_resolve(rctx, fe, filters)?;
    value::render_value_in_context(rctx, &obj)
}

/// `Var`-identical generic render of a rewritten `FlVar` (empty slot stack,
/// e.g. Python-adapter delegation, or grouped ints): the `forloop` dict
/// entry (kept current by the mask) resolves through the stock chain.
fn fl_generic(
    rctx: &mut Rctx<'_>,
    fe: &FilterExpression,
    token: &Token,
    out: &mut String,
) -> PyResult<()> {
    match resolve_and_stringify(rctx, fe, &[]) {
        Ok(s) => {
            out.push_str(&s);
            Ok(())
        }
        Err(e) if e.is_instance_of::<PyUnicodeDecodeError>(rctx.py()) => Ok(()),
        Err(e) => {
            annotate_error(rctx, &e, token);
            Err(e)
        }
    }
}

/// Port of the debug-annotation half of stock `Node.render_annotated`. Stock
/// records `e._culprit_node` at the innermost catching frame and later frames
/// only annotate when `render_context.template.origin` matches the culprit's
/// origin, annotating with the culprit's token. We mirror that with culprit
/// attributes on the exception: the first (innermost) native annotator records
/// `(source, origin, token)`; later annotators must match that culprit origin
/// against `render_context.template.origin` (e.g. a `{% block %}` of the
/// PARENT rendering a CHILD block body must not annotate with the parent's
/// block token). Best-effort: annotation failure never masks the error.
pub(crate) fn annotate_error(rctx: &Rctx<'_>, e: &PyErr, token: &Token) {
    let py = rctx.py();
    let Some((source, origin)) = &rctx.dbg else {
        return;
    };
    let exc = e.value(py);
    if exc.hasattr("template_debug").unwrap_or(false) {
        return;
    }
    // Innermost annotator records the culprit.
    if !exc.hasattr("_culprit_token").unwrap_or(false) {
        let tok = crate::bridge::make_py_token(py, token);
        if exc
            .setattr("_culprit_token", tok)
            .and_then(|()| exc.setattr("_culprit_source", source.as_str()))
            .and_then(|()| exc.setattr("_culprit_origin", origin.bind(py).clone()))
            .is_err()
        {
            return;
        }
    }
    // `context.render_context.template.origin == culprit.origin`
    let Ok(rc) = rctx.ctx.getattr("render_context") else {
        return;
    };
    let Ok(rt) = rc.getattr("template") else {
        return;
    };
    let Ok(culprit_origin) = exc.getattr("_culprit_origin") else {
        return;
    };
    let Ok(rt_origin) = rt.getattr("origin") else {
        return;
    };
    if !rt_origin.eq(culprit_origin.clone()).unwrap_or(false) {
        return;
    }
    let Ok(culprit_token) = exc.getattr("_culprit_token") else {
        return;
    };
    let Ok(culprit_source) = exc.getattr("_culprit_source") else {
        return;
    };
    let Ok(culprit_source) = culprit_source.extract::<String>() else {
        return;
    };
    if let Ok(info) =
        crate::engine::exception_info(py, &culprit_source, &rt_origin, exc, &culprit_token)
    {
        let _ = exc.setattr("template_debug", info);
    }
}
