//! Native built-in block tags: payload types and render dispatch.
//!
//! Each `{% tag %}` compiles to `Node::Tag { kind, payload, id }`;
//! `render_tag` ports the stock `Node.render` methods from
//! `django/template/defaulttags.py` / `loader_tags.py` verbatim (message
//! strings and branch differences included). Compile-side handlers live in
//! `tags/parse.rs`.
//!
//! `state_id` values come from a process-global atomic and key per-node
//! `render_context` state (cycle counters, include caches) in place of stock's
//! node-object keys: dtl nodes are immutable payloads, so state cannot live
//! on the node itself.

pub(crate) mod blocks;
pub(crate) mod cycle;
pub(crate) mod fold;
pub(crate) mod helper;
pub(crate) mod iter;
pub(crate) mod loader;
pub(crate) mod loadtags;
pub(crate) mod parse;
pub(crate) mod simple;

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::PyInt;

use crate::ifexpr::IfExpr;
use crate::lexer::Token;
use crate::nodes::NodeList;
use crate::render::context::Rctx;

pub(crate) type Expr = std::sync::Arc<crate::parser::CompiledExpr>;

/// Shared `render_context` state identity (see module docs).
pub(crate) fn next_state_id() -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn state_key(py: Python<'_>, id: usize) -> Py<PyInt> {
    PyInt::new(py, i64::try_from(id).unwrap_or(i64::MAX)).unbind()
}

pub(crate) fn expr(c: crate::parser::CompiledExpr) -> Expr {
    std::sync::Arc::new(c)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum TagKind {
    Block,
    Extends,
    If,
    IfChanged,
    For,
    With,
    Filter,
    FirstOf,
    Cycle,
    ResetCycle,
    Include,
    Load,
    Now,
    Lorem,
    Spaceless,
    Templatetag,
    Url,
    WidthRatio,
    Regroup,
    Csrf,
    Debug,
    PartialDef,
    Partial,
    SimpleTag,
    InclusionTag,
}
pub enum TagPayload {
    Block {
        name: String,
        nodelist: NodeList,
        /// `blocks::body_is_simple(nodelist)`, decided at parse.
        simple: bool,
        token: Token,
    },
    Extends {
        parent: Expr,
        /// Raw parent-name token text (stock `parent_name.token`).
        token_text: String,
        /// `bool(parent_name.filters)`.
        has_filters: bool,
        /// Variable text when the parent name is a variable (else `None`).
        var_text: Option<String>,
        nodelist: NodeList,
        token: Token,
    },
    If {
        arms: Vec<(Option<IfExpr>, NodeList)>,
        token: Token,
    },
    IfChanged {
        values: Vec<Expr>,
        t: NodeList,
        f: NodeList,
        state_id: usize,
        token: Token,
    },
    For {
        loopvars: Vec<String>,
        /// `loopvars` interned at parse time (per-iteration binding keys).
        loopkeys: Vec<Py<pyo3::types::PyString>>,
        seq: Expr,
        rev: bool,
        body: NodeList,
        empty: NodeList,
        /// 6.1 only: `forloop.length` key.
        with_length: bool,
        /// Raw sequence token text (adapter repr).
        #[allow(dead_code)] // adapter repr reserved; native repr path unused
        seq_text: String,
        /// Parse-time scan: the loop provably never reads `forloop`
        /// (see `forloop_used` in `tags::parse`).
        forloop_free: bool,
        /// Per-key parse-time usage mask over the mutated `forloop` keys
        /// (see `forloop_used` in `tags::parse`): bit 0 `counter0`,
        /// 1 `counter`, 2 `revcounter`, 3 `revcounter0`, 4 `first`,
        /// 5 `last`; bit 7 = unknown reference, update everything.
        /// Only meaningful when `forloop_free` is false.
        forloop_used: u16,
        token: Token,
    },
    With {
        values: Vec<(String, Expr)>,
        body: NodeList,
        token: Token,
    },
    Filter {
        fe: Expr,
        body: NodeList,
        token: Token,
    },
    FirstOf {
        vars: Vec<Expr>,
        asvar: Option<String>,
        token: Token,
    },
    Cycle {
        values: Vec<Expr>,
        var_name: Option<String>,
        silent: bool,
        state_id: usize,
        token: Token,
    },
    ResetCycle {
        state_id: usize,
        token: Token,
    },
    Include {
        template: Expr,
        with: Vec<(String, Expr)>,
        only: bool,
        state_id: usize,
        /// Token origin's `template_name` captured at parse (stock
        /// `self.origin.template_name` in `construct_relative_path`).
        origin_name: Option<String>,
        /// Slot for the `select_template` name tuple of a bare constant
        /// string name (built once; `None` after a failed attempt = not a
        /// constant name or the tuple raised at render).
        const_name: std::sync::OnceLock<Option<pyo3::Py<pyo3::types::PyTuple>>>,
        token: Token,
    },
    Load {
        #[allow(dead_code)] // kept for parity; leaf render cannot raise
        token: Token,
    },
    Now {
        fmt: String,
        asvar: Option<String>,
        token: Token,
    },
    Lorem {
        count: Expr,
        method: String,
        common: bool,
        token: Token,
    },
    Spaceless {
        body: NodeList,
        /// Decided on first render: `Some(final string)` when every body
        /// node renders a constant (see `tags::parse::literal_spaceless`),
        /// `None` = render the body each time.
        lit: std::sync::OnceLock<Option<String>>,
        token: Token,
    },
    Templatetag {
        arg: String,
        #[allow(dead_code)] // kept for parity; leaf render cannot raise
        token: Token,
    },
    Url {
        view: Expr,
        args: Vec<Expr>,
        kwargs: Vec<(String, Expr)>,
        asvar: Option<String>,
        token: Token,
    },
    WidthRatio {
        val: Expr,
        max: Expr,
        width: Expr,
        asvar: Option<String>,
        token: Token,
    },
    Regroup {
        target: Expr,
        expr: Expr,
        var_name: String,
        token: Token,
    },
    Csrf {
        token: Token,
    },
    Debug {
        token: Token,
    },
    PartialDef {
        #[allow(dead_code)] // kept for parity with stock node attrs
        name: String,
        inline: bool,
        body: NodeList,
        token: Token,
    },
    Partial {
        name: String,
        /// Live `extra_data["partials"]` dict (parser-owned).
        partials: pyo3::Py<pyo3::types::PyDict>,
        token: Token,
    },
    /// Stock `Library.simple_tag` node (`SimpleNode`), see `tags::helper`.
    SimpleTag {
        spec: std::sync::Arc<helper::HelperSpec>,
        args: Vec<Expr>,
        kwargs: Vec<(pyo3::Py<pyo3::types::PyString>, Expr)>,
        target_var: Option<String>,
        token: Token,
    },
    /// Stock `Library.inclusion_tag` node (`InclusionNode`); `state_id`
    /// keys the per-render template cache (stock keys it by the node).
    InclusionTag {
        spec: std::sync::Arc<helper::HelperSpec>,
        args: Vec<Expr>,
        kwargs: Vec<(pyo3::Py<pyo3::types::PyString>, Expr)>,
        state_id: usize,
        token: Token,
    },
}

/// `child_nodelists` per payload kind, stock attribute order (Block
/// `nodelist`; If the `nodelist` property = arms in order; For
/// `nodelist_loop`/`nodelist_empty`; `IfChanged` `nodelist_true`/
/// `nodelist_false`; With/Filter/Spaceless/PartialDef `nodelist`;
/// leaves have none). Shared by the Python adapter builder, `locate`
/// and the partial-def DFS so every traversal agrees.
pub(crate) fn tag_child_nodelists(p: &TagPayload) -> Vec<&NodeList> {
    match p {
        TagPayload::Block { nodelist, .. } | TagPayload::Extends { nodelist, .. } => {
            vec![nodelist]
        }
        TagPayload::If { arms, .. } => arms.iter().map(|(_, nl)| nl).collect(),
        TagPayload::IfChanged { t, f, .. } => vec![t, f],
        TagPayload::For { body, empty, .. } => vec![body, empty],
        TagPayload::With { body, .. }
        | TagPayload::Filter { body, .. }
        | TagPayload::Spaceless { body, .. }
        | TagPayload::PartialDef { body, .. } => vec![body],
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// `forloop`-free scan (parse time)
// ---------------------------------------------------------------------------

use crate::nodes::{FlKey, Node};
use crate::render::value::BoundFilter;
use crate::varparse::{FilterArg, FilterExpression, VarPart};

/// `true` when a root lookup name is `forloop`. `var` holds the full dotted
/// path; the root is `lookups[0]` (defensive: bare `var` string when the
/// lookup vector is empty).
fn lookup_uses_forloop(var: &str, lookups: &[String]) -> bool {
    lookups.first().map_or_else(|| var == "forloop", |r| r == "forloop")
}

// Per-key `forloop` usage scan: which of the per-iteration counter keys a
// loop provably reads (see `forloop_scan`).
pub(crate) const FL_COUNTER0: u16 = 1;
pub(crate) const FL_COUNTER: u16 = 2;
pub(crate) const FL_REVCOUNTER: u16 = 4;
pub(crate) const FL_REVCOUNTER0: u16 = 8;
pub(crate) const FL_FIRST: u16 = 16;
pub(crate) const FL_LAST: u16 = 32;
/// Arbitrary observation of the `forloop` dict (rendering the dict itself,
/// unknown attributes, opaque render code): every key must stay current.
pub(crate) const FL_ALL: u16 =
    FL_COUNTER0 | FL_COUNTER | FL_REVCOUNTER | FL_REVCOUNTER0 | FL_FIRST | FL_LAST;

#[derive(Default)]
struct ForloopScan {
    /// The dict object is observed at all (must exist; entry-time keys).
    dict: bool,
    /// Per-iteration counter keys read somewhere below this loop.
    mask: u16,
}

impl ForloopScan {
    fn all(&mut self) {
        self.dict = true;
        self.mask |= FL_ALL;
    }
}

/// Record a root-`forloop` reference found `depth` loops below the loop
/// being scanned: `parentloop` steps are consumed so `forloop.parentloop
/// .counter` inside a nested loop marks the outer loop's `counter`.
/// A bare `forloop` at any depth renders the whole parentloop chain,
fn scan_root_ref(scan: &mut ForloopScan, lookups: &[String], depth: usize) {
    let mut attrs: &[String] = lookups.split_first().map_or(&[][..], |(_, rest)| rest);
    let mut remaining = depth;
    loop {
        let Some((head, tail)) = attrs.split_first() else {
            // Bare `forloop`: renders/copies the whole dict. The dict at
            // `depth` embeds every ancestor dict via `parentloop`, so
            // every ancestor loop (this one included) must fully
            // populate its own dict.
            scan.all();
            return;
        };
        if remaining > 0 {
            if head != "parentloop" {
                // Nested loop's own dict reference; not our loop.
                return;
            }
            attrs = tail;
            remaining -= 1;
            continue;
        }
        // Depth 0: `head` selects one of our dict's keys.
        let bit = match head.as_str() {
            "counter0" => FL_COUNTER0,
            "counter" => FL_COUNTER,
            "revcounter" => FL_REVCOUNTER,
            "revcounter0" => FL_REVCOUNTER0,
            "first" => FL_FIRST,
            "last" => FL_LAST,
            // Entry-time keys: `parentloop` is set once (and any deeper path
            // reads the grandparent loop's dict, not ours); `length` (6.1)
            // is constant across the loop. Absent key (5.2 `length`) is
            // unconditionally undefined. Either way nothing to pin.
            "parentloop" | "length" => {
                scan.dict = true;
                return;
            }
            // Unknown attribute: a dict method, a copy/traversal observing
            // every entry, or undefined-but-observable — assume the worst.
            _ => {
                scan.all();
                return;
            }
        };
        scan.dict = true;
        scan.mask |= bit;
        return;
    }
}

fn scan_filter_expr(
    fe: &FilterExpression,
    filters: &[BoundFilter],
    scan: &mut ForloopScan,
    depth: usize,
) {
    if let VarPart::Lookup { var, lookups, .. } = &fe.var {
        if lookup_uses_forloop(var, lookups) {
            scan_root_ref(scan, lookups, depth);
        }
    }
    for bf in filters {
        for arg in &bf.args {
            if let FilterArg::Lookup { var, lookups, .. } = arg {
                if lookup_uses_forloop(var, lookups) {
                    scan_root_ref(scan, lookups, depth);
                }
            }
        }
    }
}

fn scan_expr(e: &Expr, scan: &mut ForloopScan, depth: usize) {
    scan_filter_expr(&e.fe, &e.filters, scan, depth);
}

fn scan_ifexpr(e: &IfExpr, scan: &mut ForloopScan, depth: usize) {
    match e {
        IfExpr::Leaf(c) => scan_expr(c, scan, depth),
        IfExpr::Not(a) => scan_ifexpr(a, scan, depth),
        IfExpr::Or(a, b)
        | IfExpr::And(a, b)
        | IfExpr::In(a, b)
        | IfExpr::NotIn(a, b)
        | IfExpr::Is(a, b)
        | IfExpr::IsNot(a, b)
        | IfExpr::Eq(a, b)
        | IfExpr::Ne(a, b)
        | IfExpr::Gt(a, b)
        | IfExpr::Ge(a, b)
        | IfExpr::Lt(a, b)
        | IfExpr::Le(a, b) => {
            scan_ifexpr(a, scan, depth);
            scan_ifexpr(b, scan, depth);
        }
    }
}

fn scan_nodelist(list: &NodeList, scan: &mut ForloopScan, depth: usize) {
    for node in &list.nodes {
        scan_node(node, scan, depth);
    }
}

fn scan_node(node: &Node, scan: &mut ForloopScan, depth: usize) {
    match node {
        Node::Text { .. } | Node::Verbatim { .. } | Node::Comment => {}
        Node::AutoEscape { nodelist, .. } => scan_nodelist(nodelist, scan, depth),
        Node::Var { fe, filters, .. } => scan_filter_expr(fe, filters, scan, depth),
        // Rewritten direct-slot read (see `rewrite_flvars`): at depth 0 it
        // is this loop's own key (mark it: the generic fallback path and
        // nested outer scans need the dict current); deeper belongs to a
        // child loop's slot stack.
        Node::FlVar { key, .. } => {
            if depth == 0 {
                scan.dict = true;
                scan.mask |= fl_bit(*key);
            }
        }
        // Arbitrary user render code: can read anything from the context.
        Node::Bridged(_) => scan.all(),
        Node::Tag { payload, .. } => scan_payload(payload, scan, depth),
    }
}

/// Conservative: assumes the worst on ANY possibility that render code
/// observes `forloop` values — content not visible to this parse
/// (`include`, block overrides, `{% partial %}`, `{% debug %}`) and tags
/// whose state lives in the enclosing `forloop` dict (`if_changed` reads
/// `last`, which must stay current).
fn scan_payload(p: &TagPayload, scan: &mut ForloopScan, depth: usize) {
    match p {
        // Rendered content unknown to this parse, or state keyed off the
        // loop dict: every key may be observed.
        TagPayload::Include { .. }
        | TagPayload::Block { .. }
        | TagPayload::Partial { .. }
        | TagPayload::Debug { .. }
        | TagPayload::Extends { .. }
        | TagPayload::IfChanged { .. } => scan.all(),
        // Cycle state uses the render context, not the loop dict, but keep
        // stock's loop-frame presence for exactness (entry-time only). The
        // cycle values resolve against this loop's scope: scan for key reads.
        TagPayload::Cycle { values, .. } => {
            scan.dict = true;
            for e in values {
                scan_expr(e, scan, depth);
            }
        }
        TagPayload::ResetCycle { .. } => scan.dict = true,
        TagPayload::Csrf { .. }
        | TagPayload::Now { .. }
        | TagPayload::Templatetag { .. }
        | TagPayload::Load { .. } => {}
        TagPayload::If { arms, .. } => {
            for (cond, nl) in arms {
                if let Some(cond) = cond {
                    scan_ifexpr(cond, scan, depth);
                }
                scan_nodelist(nl, scan, depth);
            }
        }
        // The child loop's own `forloop` references belong to its dict;
        // descend at `depth + 1` so only `parentloop` paths mark us.
        // The child `seq` evaluates in our scope (before its push).
        TagPayload::For { seq, body, empty, .. } => {
            scan_expr(seq, scan, depth);
            scan_nodelist(body, scan, depth + 1);
            scan_nodelist(empty, scan, depth + 1);
        }
        TagPayload::With { values, body, .. } => {
            for (_, e) in values {
                scan_expr(e, scan, depth);
            }
            scan_nodelist(body, scan, depth);
        }
        TagPayload::Filter { fe, body, .. } => {
            scan_expr(fe, scan, depth);
            scan_nodelist(body, scan, depth);
        }
        TagPayload::FirstOf { vars, .. } => {
            for e in vars {
                scan_expr(e, scan, depth);
            }
        }
        TagPayload::Lorem { count, .. } => scan_expr(count, scan, depth),
        TagPayload::Spaceless { body, .. } | TagPayload::PartialDef { body, .. } => {
            scan_nodelist(body, scan, depth);
        }
        TagPayload::Url {
            view, args, kwargs, ..
        } => {
            scan_expr(view, scan, depth);
            for e in args {
                scan_expr(e, scan, depth);
            }
            for (_, e) in kwargs {
                scan_expr(e, scan, depth);
            }
        }
        TagPayload::WidthRatio { val, max, width, .. } => {
            scan_expr(val, scan, depth);
            scan_expr(max, scan, depth);
            scan_expr(width, scan, depth);
        }
        TagPayload::Regroup { target, expr, .. } => {
            scan_expr(target, scan, depth);
            scan_expr(expr, scan, depth);
        }
        // A `takes_context` function sees the whole context.
        TagPayload::SimpleTag { spec, args, kwargs, .. }
        | TagPayload::InclusionTag { spec, args, kwargs, .. } => {
            if spec.takes_context {
                scan.all();
            }
            for e in args {
                scan_expr(e, scan, depth);
            }
            for (_, e) in kwargs {
                scan_expr(e, scan, depth);
            }
        }
    }
}

/// Dict mask bit for a direct-slot key.
fn fl_bit(key: FlKey) -> u16 {
    match key {
        FlKey::Counter0 => FL_COUNTER0,
        FlKey::Counter => FL_COUNTER,
        FlKey::Revcounter => FL_REVCOUNTER,
        FlKey::Revcounter0 => FL_REVCOUNTER0,
        FlKey::First => FL_FIRST,
        FlKey::Last => FL_LAST,
    }
}

/// `{{ forloop.counter }}`-family output nodes inside `body` whose key bit
/// is in `mask` become `Node::FlVar` (native slot read; generic dict path
/// kept as fallback). Never descends into child `For` bodies (their own
/// parse pass rewrote those at their own depth) or subtrees that may
/// rebind the `forloop` name (`flag`), keeping stock context-resolution
/// semantics.
pub(crate) fn rewrite_flvars(body: &mut NodeList, mask: u16) {
    rewrite_flag(body, mask, false);
}

fn rewrite_flag(body: &mut NodeList, mask: u16, flag: bool) {
    for node in &mut body.nodes {
        rewrite_flvar_node(node, mask, flag);
    }
}

#[allow(clippy::too_many_lines)] // one match arm per descendent payload
fn rewrite_flvar_node(node: &mut Node, mask: u16, flag: bool) {
    if let Node::Var { fe, filters, token } = node {
        if flag || !filters.is_empty() {
            return;
        }
        let key = match &fe.var {
            // `lookups[0]` is the root segment of the dotted path.
            VarPart::Lookup { lookups, translate: false, .. }
                if lookups.len() == 2 && lookups[0] == "forloop" =>
            {
                match lookups[1].as_str() {
                    "counter0" => Some(FlKey::Counter0),
                    "counter" => Some(FlKey::Counter),
                    "revcounter" => Some(FlKey::Revcounter),
                    "revcounter0" => Some(FlKey::Revcounter0),
                    "first" => Some(FlKey::First),
                    "last" => Some(FlKey::Last),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(key) = key.filter(|k| mask & fl_bit(*k) != 0) {
            *node = Node::FlVar { key, fe: fe.clone(), token: token.clone() };
        }
        return;
    }
    let Node::Tag { payload, .. } = node else { return };
    // Any tag in this subtree that can rebind the `forloop` name sends the
    // whole subtree through generic context resolution.
    let flag = flag
        || match &**payload {
            TagPayload::With { values, .. } => values.iter().any(|(k, _)| k == "forloop"),
            TagPayload::Regroup { var_name, .. } => var_name == "forloop",
            TagPayload::Cycle { var_name: Some(v), .. } => v == "forloop",
            TagPayload::Url { asvar: Some(v), .. }
            | TagPayload::WidthRatio { asvar: Some(v), .. }
            | TagPayload::SimpleTag { target_var: Some(v), .. } => v == "forloop",
            TagPayload::SimpleTag { spec, .. } => spec.takes_context,
            _ => false,
        };
    match &mut **payload {
        TagPayload::If { arms, .. } => {
            for (_, nl) in arms {
                rewrite_flag(nl, mask, flag);
            }
        }
        TagPayload::IfChanged { t, f, .. } => {
            rewrite_flag(t, mask, flag);
            rewrite_flag(f, mask, flag);
        }
        TagPayload::With { body, .. }
        | TagPayload::Filter { body, .. }
        | TagPayload::Spaceless { body, .. } => rewrite_flag(body, mask, flag),
        _ => {}
    }
}

/// Parse-time decision for `For`: `(forloop_free, forloop_used)`.
/// `forloop_free` = the loop provably never observes `forloop` at all
/// (skip the dict entirely); otherwise `forloop_used` selects which
/// per-iteration counter keys the render code reads (all bits = the
/// pre-scan behaviour). A loop variable named `forloop` rebinds the name
/// for arbitrary render code: assume all keys are observable.
pub(crate) fn forloop_scan(
    body: &NodeList,
    empty: &NodeList,
    seq: &Expr,
    loopvars: &[String],
) -> (bool, u16) {
    let mut scan = ForloopScan::default();
    scan_nodelist(body, &mut scan, 0);
    scan_nodelist(empty, &mut scan, 0);
    scan_expr(seq, &mut scan, 0);
    if loopvars.iter().any(|v| v == "forloop") {
        scan.all();
    }
    (!scan.dict && scan.mask == 0, scan.mask)
}

/// Parse-time decision for child templates rendered inline (include):
/// `true` when the nodelist provably never reads or writes
/// `render_context` state, so the inline renderer may skip the
/// `rc.template` rebinding and the extra rc dicts layer. Taints:
/// `Bridged` nodes (arbitrary stock render code), block/extends/partial
/// machinery, nested include engine resolution, `{% debug %}`, and rc
/// state writers (`cycle`/`resetcycle`/`if_changed`); with the rc layer
/// skipped those would observe or write the parent template's frame.
/// Context writes are safe: they go through `ctx.dicts`, which the inline
/// renderer maintains exactly.
pub(crate) fn never_touches_rc(list: &NodeList) -> bool {
    list.nodes.iter().all(node_never_touches_rc)
}

fn node_never_touches_rc(node: &Node) -> bool {
    match node {
        // Plain output/constant text and native slot reads (the FlVar
        // fallback only touches the context): never render_context state.
        Node::Text { .. } | Node::Verbatim { .. } | Node::Comment | Node::Var { .. }
        | Node::FlVar { .. } => true,
        Node::AutoEscape { nodelist, .. } => never_touches_rc(nodelist),
        // Rendered by stock code: arbitrary rc access.
        Node::Bridged(_) => false,
        Node::Tag { payload, .. } => payload_never_touches_rc(payload),
    }
}

fn payload_never_touches_rc(p: &TagPayload) -> bool {
    match p {
        // rc.template readers (block/extends machinery resolves state via
        // `rc.template`; nested include engine resolution on cache miss;
        // `{% debug %}` dumps it) and rc state writers (`cycle`,
        // `resetcycle`, `if_changed`, `block_context`): with the rc layer
        // skipped these would read/write the parent frame.
        TagPayload::Block { .. }
        | TagPayload::Extends { .. }
        | TagPayload::Include { .. }
        | TagPayload::Partial { .. }
        | TagPayload::PartialDef { .. }
        | TagPayload::Debug { .. }
        | TagPayload::Cycle { .. }
        | TagPayload::ResetCycle { .. }
        | TagPayload::IfChanged { .. }
        // Writes its template cache into the rc frame.
        | TagPayload::InclusionTag { .. } => false,
        // The function may reach `context.render_context`.
        TagPayload::SimpleTag { spec, .. } => !spec.takes_context,
        // Everything else writes only through the `Context` layer stack
        // (`set_item`/`set_upward` -> `ctx.dicts`), which the inline
        // renderer maintains exactly, and reads at most context values.
        TagPayload::Csrf { .. }
        | TagPayload::Templatetag { .. }
        | TagPayload::Load { .. }
        | TagPayload::FirstOf { .. }
        | TagPayload::Now { .. }
        | TagPayload::Url { .. }
        | TagPayload::WidthRatio { .. }
        | TagPayload::Regroup { .. }
        | TagPayload::Lorem { .. } => true,
        TagPayload::If { arms, .. } => arms.iter().all(|(_, nl)| never_touches_rc(nl)),
        TagPayload::For { body, empty, .. } => {
            never_touches_rc(body) && never_touches_rc(empty)
        }
        TagPayload::With { body, .. }
        | TagPayload::Filter { body, .. }
        | TagPayload::Spaceless { body, .. } => never_touches_rc(body),
    }
}


/// `render_annotated`-compatible error annotation for Rust tag renders:
/// attach `token` (and debug `template_debug`) like stock's wrapper does.
pub(crate) fn annotate(rctx: &Rctx<'_>, e: &PyErr, token: &Token) {
    let py = rctx.py();
    let exc = e.value(py);
    if !matches!(exc.hasattr("token"), Ok(true)) {
        let _ = exc.setattr("token", crate::bridge::make_py_token(py, token).into_bound(py));
    }
    crate::nodes::annotate_error(rctx, e, token);
}

pub(crate) fn render_tag(
    rctx: &mut Rctx<'_>,
    kind: TagKind,
    p: &TagPayload,
    id: usize,
    out: &mut String,
) -> PyResult<()> {
    let r = dispatch(rctx, kind, p, id, out);
    // Python-reentrant tags hand the live context to Python code (parent
    // templates, `block.super`, Python-rendered includes/partials) that
    // may assign `autoescape` / `use_l10n` / `use_tz`.
    if matches!(
        kind,
        TagKind::Block | TagKind::Extends | TagKind::Include | TagKind::Partial
    ) {
        rctx.invalidate_flags();
    }
    r
}

// ---------------------------------------------------------------------------
// shared render helpers
// ---------------------------------------------------------------------------

/// Cached Python callables used by tag renders.
pub(crate) struct PyFns {
    pub escape: Py<PyAny>,
    pub format_html: Py<PyAny>,
    pub settings: Py<PyAny>,
    pub warnings: Py<PyAny>,
    pub construct_relative_path: Py<PyAny>,
}

static PY_FN: PyOnceLock<PyFns> = PyOnceLock::new();

pub(crate) fn fns(py: Python<'_>) -> PyResult<&'static PyFns> {
    PY_FN.get_or_try_init(py, || -> PyResult<PyFns> {
        let html = py.import("django.utils.html")?;
        Ok(PyFns {
            escape: html.getattr("escape")?.unbind(),
            format_html: html.getattr("format_html")?.unbind(),
            settings: py.import("django.conf")?.getattr("settings")?.unbind(),
            warnings: py.import("warnings")?.unbind().into(),
            construct_relative_path: py
                .import("django.template.loader_tags")?
                .getattr("construct_relative_path")?
                .unbind(),
        })
    })
}

/// `with context.push([dict]):` — exact `ContextDict.__exit__` semantics
/// (pop-on-error included). `None` pushes an empty frame.
#[allow(clippy::ref_option)] // callers already hold `Option<Bound>`; avoid re-borrow gymnastics
pub(crate) fn with_scoped<'py, F>(
    rctx: &mut Rctx<'py>,
    d: &Option<Bound<'py, PyAny>>,
    f: F,
) -> PyResult<()>
where
    F: FnOnce(&mut Rctx<'py>) -> PyResult<()>,
{
    if rctx.push_layer(d.as_ref())? {
        let res = f(rctx);
        let pop = if rctx.pop_layer().unwrap_or(false) {
            Ok(())
        } else {
            // The stack or its list was swapped mid-body: pop the current
            // top through the real Python `pop`, exactly like the
            // stock `ContextDict.__exit__` does (its errors are ignored).
            let _ = rctx.pop();
            rctx.invalidate()
        };
        res?;
        return pop;
    }
    let pushed = match d {
        Some(d) => rctx.push(d.clone())?,
        None => rctx.ctx.call_method0("push")?,
    };
    rctx.invalidate()?;
    let res = f(rctx);
    let _ = pushed.call_method0("__exit__");
    let pop = rctx.invalidate();
    res?;
    pop
}

/// The adapter node carrying id `id` on the template currently on
/// `render_context.template` (`_dtl_node_by_id` map, built by `nodes_py`).
pub(crate) fn adapter_for_id<'py>(
    rctx: &Rctx<'py>,
    id: usize,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    let rc = rctx.render_context()?;
    let cur = rc.getattr("template")?;
    let Some(map) = cur.getattr("_dtl_node_by_id").ok() else {
        return Ok(None);
    };
    if map.is_none() {
        return Ok(None);
    }
    let dict = map.cast_into::<pyo3::types::PyDict>()?;
    match dict.get_item(i64::try_from(id).unwrap_or(i64::MAX))? {
        Some(n) if !n.is_none() => Ok(Some(n)),
        _ => Ok(None),
    }
}

/// `context.get(key)` on the live Context (default `None`).
pub(crate) fn context_get<'py>(
    rctx: &Rctx<'py>,
    key: &str,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    // Stored `None` reads as the default either way.
    Ok(rctx.get(key)?.filter(|v| !v.is_none()))
}

/// Python `str(obj)`.
pub(crate) fn py_str_of(v: &Bound<'_, PyAny>) -> PyResult<String> {
    Ok(v.str()?.to_string())
}

/// `expr.resolve(context)` (raising).
pub(crate) fn expr_res<'py>(
    rctx: &mut Rctx<'py>,
    e: &Expr,
) -> PyResult<Bound<'py, PyAny>> {
    crate::render::value::filter_expression_resolve(rctx, &e.fe, &e.filters)
}

/// `expr.resolve(context, ignore_failures=f)`.
pub(crate) fn expr_res_ign<'py>(
    rctx: &mut Rctx<'py>,
    e: &Expr,
    f: bool,
) -> PyResult<Bound<'py, PyAny>> {
    crate::render::value::filter_expression_resolve_ignoring(rctx, &e.fe, &e.filters, f)
}

#[allow(clippy::too_many_lines)]
fn dispatch(
    rctx: &mut Rctx<'_>,
    kind: TagKind,
    p: &TagPayload,
    id: usize,
    out: &mut String,
) -> PyResult<()> {
    match kind {
        TagKind::Csrf => simple::csrf(rctx, p, out),
        TagKind::Debug => simple::debug(rctx, p, out),
        TagKind::Now => simple::now(rctx, p, out),
        TagKind::Lorem => simple::lorem(rctx, p, out),
        TagKind::Spaceless => simple::spaceless(rctx, p, out),
        TagKind::Templatetag => {
            simple::templatetag(rctx, p, out);
            Ok(())
        }
        TagKind::FirstOf => simple::first_of(rctx, p, out),
        TagKind::WidthRatio => simple::width_ratio(rctx, p, out),
        TagKind::Url => simple::url(rctx, p, out),
        TagKind::Regroup => simple::regroup(rctx, p, out),
        TagKind::Load => {
            loadtags::load(rctx, p, out);
            Ok(())
        }
        TagKind::Cycle => cycle::cycle(rctx, p, out),
        TagKind::ResetCycle => cycle::reset_cycle(rctx, p, out),
        TagKind::IfChanged => cycle::if_changed(rctx, p, out),
        TagKind::For => iter::for_tag(rctx, p, out),
        TagKind::If => iter::if_tag(rctx, p, out),
        TagKind::With => iter::with_tag(rctx, p, out),
        TagKind::Filter => fold::filter_tag(rctx, p, out),
        TagKind::Include => loader::include(rctx, p, out),
        TagKind::Block => loader::block(rctx, p, id, out),
        TagKind::Extends => loader::extends(rctx, p, id, out),
        TagKind::PartialDef => loader::partial_def(rctx, p, out),
        TagKind::Partial => loader::partial(rctx, p, out),
        TagKind::SimpleTag => helper::simple(rctx, p, out),
        TagKind::InclusionTag => helper::inclusion(rctx, p, out),
    }
}
