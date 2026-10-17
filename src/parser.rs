//! Parser: token stream → node tree. Exact port of
//! `django.template.base.Parser` (`parse` loop, `extend_nodelist`, `error`,
//! `invalid_block_tag`, `unclosed_block_tag`, `skip_past`, `compile_filter`) plus
//! `token_kwargs`.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString, PyTuple};
use regex::Regex;

/// Static compiled regex from a literal pattern (valid by construction).
macro_rules! static_re {
    ($name:ident, $pat:literal) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| {
            #[allow(clippy::unwrap_used)] // literal pattern, cannot fail
            let compiled = Regex::new($pat).unwrap();
            compiled
        });
    };
}

use crate::bridge::{self, ShimParser};
use crate::error::{syntax_error, syntax_error_type};
use crate::lexer::{get_text_list, Token, TokenKind};
use crate::nodes::{Node, NodeList};
use crate::render::value::{BoundFilter, FilterRegistry, PyFilterEntry};
use crate::tags::{Expr, TagKind};
use crate::varparse::{self, FxError, FilterExpression, FilterLookup, PyVer};

/// A filter expression with its filters resolved to callables (parse-time,
/// like Django binding `filter_func` in `FilterExpression.__init__`).
pub struct CompiledExpr {
    pub fe: FilterExpression,
    pub filters: Vec<BoundFilter>,
}

/// Natively compiled block tags available before Python-library bridging.
/// Handlers mirror defaulttags verbatim.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NativeTag {
    AutoEscape,
    Comment,
    Verbatim,
    Block,
    Extends,
    Include,
    Load,
    If,
    IfChanged,
    For,
    With,
    Filter,
    FirstOf,
    Cycle,
    ResetCycle,
    Spaceless,
    Templatetag,
    Now,
    Lorem,
    Url,
    WidthRatio,
    Regroup,
    Csrf,
    Debug,
    PartialDef,
    Partial,
}

fn native_tags() -> &'static HashMap<&'static str, NativeTag> {
    static TAGS: LazyLock<HashMap<&'static str, NativeTag>> = LazyLock::new(|| {
        HashMap::from([
            ("autoescape", NativeTag::AutoEscape),
            ("comment", NativeTag::Comment),
            ("verbatim", NativeTag::Verbatim),
            ("block", NativeTag::Block),
            ("extends", NativeTag::Extends),
            ("include", NativeTag::Include),
            ("load", NativeTag::Load),
            ("if", NativeTag::If),
            ("ifchanged", NativeTag::IfChanged),
            ("for", NativeTag::For),
            ("with", NativeTag::With),
            ("filter", NativeTag::Filter),
            ("firstof", NativeTag::FirstOf),
            ("cycle", NativeTag::Cycle),
            ("resetcycle", NativeTag::ResetCycle),
            ("spaceless", NativeTag::Spaceless),
            ("templatetag", NativeTag::Templatetag),
            ("now", NativeTag::Now),
            ("lorem", NativeTag::Lorem),
            ("url", NativeTag::Url),
            ("widthratio", NativeTag::WidthRatio),
            ("regroup", NativeTag::Regroup),
            ("csrf_token", NativeTag::Csrf),
            ("debug", NativeTag::Debug),
            ("partialdef", NativeTag::PartialDef),
            ("partial", NativeTag::Partial),
        ])
    });
    &TAGS
}

pub struct Parser<'a> {
    /// Reversed token stack; `next_token` pops the back (Django parity).
    tokens: Vec<Token>,
    pub filters: &'a FilterRegistry,
    /// Engine's bridged tag map, borrowed for the whole parse (no
    /// per-template deep clone of every `Py` handle). `{% load %}` and
    /// bridged compiles write into the `tags` overlay instead.
    base_tags: &'a HashMap<String, Py<PyAny>>,
    /// Tags added/re-registered during this parse (overlay consulted
    /// before `base_tags`, like `local_filters` over the filter registry).
    tags: HashMap<String, Py<PyAny>>,
    /// Filters added by `{% load %}` during this parse (overlay on the
    /// Engine registry; `find_filter` consults the loaded dict first).
    local_filters: HashMap<String, PyFilterEntry>,
    /// The dtl `Engine` (shim construction: `template_libraries` /
    /// `template_builtins`).
    engine: Py<PyAny>,
    /// Lazy real-`Parser` shim for bridged compile funcs.
    shim: Option<ShimParser>,
    /// The shim's `extra_data` after parsing (6.1 `{% partialdef %}`).
    shim_extra: Option<Py<PyAny>>,
    command_stack: Vec<(String, Token)>,
    /// `django.template.base.Origin` for this parse (`must_be_first` errors).
    origin: Py<PyAny>,
    ver: PyVer,
    /// Held for parity; drives `template_debug`.
    #[allow(dead_code)]
    debug: bool,
    /// Parse-order counter for `Node::Tag` ids (`_dtl_node_by_id` / partial
    /// body lookup keys).
    next_id: usize,
    /// `Parser.__loaded_blocks`: duplicate `{% block %}` names.
    loaded_blocks: Vec<String>,
    /// `parser._named_cycle_nodes`: name → the named cycle's state id.
    named_cycles: HashMap<String, usize>,
    /// `hasattr(parser, "_named_cycle_nodes")` parity for `{% cycle name %}`.
    ever_named_cycle: bool,
    /// `parser._last_cycle_node`.
    last_cycle: Option<usize>,
    /// Named-cycle payloads (state id → values/var-name/silent), so a
    /// `{% cycle name %}` reuse compiles a node with the SAME state id.
    cycle_values: HashMap<usize, (Vec<Expr>, Option<String>, bool)>,
    /// Tag names a `{% load %}` re-registered with a different function:
    /// dispatched bridged (the loaded stock func wins), not natively.
    load_overrides: HashSet<String>,
    /// `extra_data["partials"]` built by native `{% partialdef %}` (6.1).
    native_partials: Option<Py<PyDict>>,
    /// Libraries added by the native `{% load %}` fast path that the shim
    /// parser has not seen yet; `ensure_shim` replays them (`add_library`,
    /// in order) before any stock compile function runs.
    pending_libs: Vec<Py<PyAny>>,
    /// Bodies compiled natively for bridged compile functions
    /// (`ParseHook`); handed to the `Template`.
    bodies: Vec<Py<crate::nodes_py::NativeBody>>,
    /// Template source for body error annotation (debug engines only).
    dbg_source: Option<std::sync::Arc<str>>,
}

impl<'a> Parser<'a> {
    #[must_use]
    pub fn new(
        tokens: Vec<Token>,
        filters: &'a FilterRegistry,
        engine: Py<PyAny>,
        base_tags: &'a HashMap<String, Py<PyAny>>,
        origin: Py<PyAny>,
        ver: PyVer,
        debug: bool,
    ) -> Self {
        // Parser.__init__: self.tokens = list(reversed(tokens))
        Self {
            tokens: tokens.into_iter().rev().collect(),
            filters,
            base_tags,
            tags: HashMap::new(),
            local_filters: HashMap::new(),
            engine,
            shim: None,
            shim_extra: None,
            command_stack: Vec::new(),
            origin,
            ver,
            debug,
            next_id: 0,
            loaded_blocks: Vec::new(),
            named_cycles: HashMap::new(),
            ever_named_cycle: false,
            last_cycle: None,
            cycle_values: HashMap::new(),
            load_overrides: HashSet::new(),
            native_partials: None,
            pending_libs: Vec::new(),
            bodies: Vec::new(),
            dbg_source: None,
        }
    }

    /// Source for debug annotation of natively compiled bridged bodies.
    pub fn set_dbg_source(&mut self, source: &str) {
        self.dbg_source = Some(source.into());
    }

    /// The `NativeBody`s compiled during this parse.
    pub fn take_bodies(&mut self) -> Vec<Py<crate::nodes_py::NativeBody>> {
        std::mem::take(&mut self.bodies)
    }

    pub(crate) fn next_token(&mut self) -> Option<Token> {
        self.tokens.pop()
    }

    // -----------------------------------------------------------------
    // Accessors for the tag handlers in `tags::parse` (fields stay private).
    // -----------------------------------------------------------------
    /// Bridged tag compile func: parse-local overlay first, then the
    /// borrowed Engine map.
    pub(crate) fn tag_func(&self, py: Python<'_>, name: &str) -> Option<Py<PyAny>> {
        self.tags
            .get(name)
            .or_else(|| self.base_tags.get(name))
            .map(|f| f.clone_ref(py))
    }

    /// `next_token` for tag handlers consuming their closing tags.
    pub(crate) fn next_token_pub(&mut self) -> Option<Token> {
        self.next_token()
    }

    /// The parse's `django.template.base.Origin`.
    pub(crate) fn origin_obj(&self) -> &Py<PyAny> {
        &self.origin
    }

    /// `Parser.__loaded_blocks` (duplicate `{% block %}` names).
    pub(crate) fn loaded_blocks_mut(&mut self) -> &mut Vec<String> {
        &mut self.loaded_blocks
    }

    pub(crate) fn ver(&self) -> PyVer {
        self.ver
    }

    /// `hasattr(parser, "_named_cycle_nodes")` parity.
    pub(crate) fn cycle_has_named(&self) -> bool {
        self.ever_named_cycle
    }

    pub(crate) fn cycle_named(&self, name: &str) -> Option<usize> {
        self.named_cycles.get(name).copied()
    }

    /// The named cycle's payload (values share `Arc`s with the defining
    /// node, and the state id is shared too — stock stores the node itself).
    pub(crate) fn cycle_values_of(&self, sid: usize) -> (Vec<Expr>, Option<String>, bool) {
        self.cycle_values
            .get(&sid)
            .cloned()
            .unwrap_or_default()
    }

    /// Record a `{% cycle ... as name %}`: later `{% cycle name %}` reuses
    /// the values and the SAME state id (one shared render state).
    pub(crate) fn cycle_register(
        &mut self,
        name: String,
        sid: usize,
        vals: Vec<Expr>,
        silent: bool,
    ) {
        // Stock reuse hands back the SAME node, so the reuse sees the
        // defining node's var_name — store it here.
        self.cycle_values.insert(sid, (vals, Some(name.clone()), silent));
        self.named_cycles.insert(name, sid);
        self.ever_named_cycle = true;
    }

    pub(crate) fn set_last_cycle(&mut self, sid: usize) {
        self.last_cycle = Some(sid);
    }

    pub(crate) fn last_cycle(&self) -> Option<usize> {
        self.last_cycle
    }
    fn prepend_token(&mut self, token: Token) {
        self.tokens.push(token);
    }

    /// `del self.tokens[-1]` — consumed by block handlers after their
    /// closing tag.
    pub fn delete_first_token(&mut self) {
        self.tokens.pop();
    }

    #[must_use]
    pub fn has_tokens(&self) -> bool {
        !self.tokens.is_empty()
    }

    /// Exact port of the 6.1 stock warning call (`warnings.warn(...,
    /// skip_file_prefixes=django_file_prefixes())`).
    fn warn_double_dot(
        &self,
        py: Python<'_>,
        token: &Token,
    ) -> PyResult<()> {
        let kwargs = PyDict::new(py);
        kwargs.set_item(
            "skip_file_prefixes",
            py.import("django.utils.deprecation")?
                .getattr("django_file_prefixes")?
                .call0()?,
        )?;
        let name = self
            .origin
            .getattr(py, "name")?
            .bind(py)
            .str()?
            .extract::<String>()?;
        let msg = format!(
            "Support for double-dot lookups '..' which maps to a lookup of the empty \
             string is deprecated.\n  Template: {name}\n  Line: {}",
            token.lineno,
        );
        py.import("warnings")?.getattr("warn")?.call(
            (
                PyString::new(py, &msg),
                py.import("django.utils.deprecation")?
                    .getattr("RemovedInDjango2028Warning")?,
            ),
            Some(&kwargs),
        )?;
        Ok(())
    }

    /// Port of `Parser.parse(parse_until)`.
    pub fn parse(&mut self, py: Python<'_>, parse_until: &[&str]) -> PyResult<NodeList> {
        let mut nodelist = NodeList::new();
        while let Some(token) = self.next_token() {
            match token.kind {
                TokenKind::Text => {
                    self.extend_nodelist(
                        py,
                        &mut nodelist,
                        Node::Text {
                            s: token.contents.clone(),
                        },
                        &token,
                    )?;
                }
                TokenKind::Comment => {}
                TokenKind::Var => {
                    if token.contents.is_empty() {
                        return Err(self.error_msg(
                            py,
                            &token,
                            &format!("Empty variable tag on line {}", token.lineno),
                        ));
                    }
                    let expr = self
                        .compile_filter(py, &token.contents)
                        .map_err(|e| self.error_msg(py, &token, &e.to_string()))?;
                    // 6.1 DeprecationWarning for `..` double-dot lookups.
                    if let varparse::VarPart::Lookup { var, .. } = &expr.fe.var {
                        if self.ver == PyVer::V61 && var.contains("..") {
                            self.warn_double_dot(py, &token)?;
                        }
                    }
                    self.extend_nodelist(
                        py,
                        &mut nodelist,
                        Node::Var {
                            fe: expr.fe,
                            filters: expr.filters,
                            token: token.clone(),
                        },
                        &token,
                    )?;
                }
                TokenKind::Block => {
                    let command = match token.contents.split_whitespace().next() {
                        Some(c) => c.to_string(),
                        None => {
                            return Err(self.error_msg(
                                py,
                                &token,
                                &format!("Empty block tag on line {}", token.lineno),
                            ))
                        }
                    };
                    if parse_until.contains(&command.as_str()) {
                        self.prepend_token(token);
                        return Ok(nodelist);
                    }
                    self.command_stack.push((command.clone(), token.clone()));
                    // Loaded re-registrations beat the native table;
                    // 6.1-only tags (`partialdef`/`partial`) fall through to
                    // bridged dispatch on 5.2 — which has no such stock tag —
                    // and from there to `invalid_block_tag`.
                    let native = if self.load_overrides.contains(&command) {
                        None
                    } else {
                        native_tags().get(command.as_str()).copied().filter(|t| match t {
                            NativeTag::PartialDef | NativeTag::Partial => self.ver == PyVer::V61,
                            _ => true,
                        })
                    };
                    let node = match native {
                        Some(tag) => self.compile_native(py, tag, &token),
                        None => match self.tag_func(py, &command) {
                            Some(f) => self.compile_func_tag(py, f.bind(py), &token),
                            None => Err(self.invalid_block_tag(
                                py,
                                &token,
                                &command,
                                parse_until,
                            )),
                        },
                    };
                    match node {
                        Ok(node) => {
                            self.extend_nodelist(py, &mut nodelist, node, &token)?;
                            self.command_stack.pop();
                        }
                        Err(e) => {
                            // Parser.parse wraps handler errors via
                            // self.error(token, e) — innermost token wins.
                            return Err(self.annotate_token(py, e, &token));
                        }
                    }
                }
            }
        }
        if !parse_until.is_empty() {
            return Err(self.unclosed_block_tag(py, parse_until));
        }
        Ok(nodelist)
    }

    #[allow(clippy::too_many_lines)] // one compact arm per native tag
    fn compile_native(&mut self, py: Python<'_>, tag: NativeTag, token: &Token) -> PyResult<Node> {
        match tag {
            NativeTag::AutoEscape => {
                // do_autoescape: "token.split_contents() isn't useful here"
                // — the tag takes literal args only, so plain split matches.
                let args: Vec<&str> = token.contents.split_whitespace().collect();
                if args.len() != 2 {
                    return Err(self.error_msg(
                        py,
                        token,
                        "'autoescape' tag requires exactly one argument.",
                    ));
                }
                if args[1] != "on" && args[1] != "off" {
                    return Err(self.error_msg(
                        py,
                        token,
                        "'autoescape' argument should be 'on' or 'off'",
                    ));
                }
                let nodelist = self.parse(py, &["endautoescape"])?;
                self.delete_first_token();
                Ok(Node::AutoEscape {
                    setting: args[1] == "on",
                    nodelist: Box::new(nodelist),
                })
            }
            // The remaining arms are the native built-in tags: each
            // handler in `tags::parse` is a verbatim port of its stock
            // compile function and returns a `TagPayload`.
            NativeTag::Block => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::block(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::Block,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::Extends => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::extends(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::Extends,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::Include => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::include(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::Include,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::Load => {
                self.native_load(py, token)?;
                Ok(Node::Tag {
                    kind: TagKind::Load,
                    payload: Box::new(crate::tags::TagPayload::Load { token: token.clone() }),
                    id: 0,
                })
            }
            NativeTag::If => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::if_tag(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::If,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::IfChanged => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::ifchanged(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::IfChanged,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::For => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::for_tag(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::For,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::With => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::with_tag(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::With,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::Filter => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::filter_tag(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::Filter,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::FirstOf => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::firstof(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::FirstOf,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::Cycle => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::cycle(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::Cycle,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::ResetCycle => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::resetcycle(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::ResetCycle,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::Spaceless => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::spaceless(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::Spaceless,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::Templatetag => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::templatetag(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::Templatetag,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::Now => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::now(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::Now,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::Lorem => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::lorem(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::Lorem,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::Url => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::url(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::Url,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::WidthRatio => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::widthratio(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::WidthRatio,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::Regroup => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::regroup(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::Regroup,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::Csrf => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::csrf(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::Csrf,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::Debug => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::debug(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::Debug,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::PartialDef => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::partialdef(py, self, id, token)?;
                Ok(Node::Tag {
                    kind: TagKind::PartialDef,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::Partial => {
                let id = self.next_id;
                self.next_id += 1;
                let payload = crate::tags::parse::partial(py, self, token)?;
                Ok(Node::Tag {
                    kind: TagKind::Partial,
                    payload: Box::new(payload),
                    id,
                })
            }
            NativeTag::Comment => {
                self.skip_past(py, "endcomment")?;
                Ok(Node::Comment)
            }
            NativeTag::Verbatim => {
                let nodelist = self.parse(py, &["endverbatim"])?;
                self.delete_first_token();
                // VerbatimNode(nodelist.render(Context())): inside verbatim
                // context is the concatenated text.
                let mut s = String::new();
                for n in &nodelist.nodes {
                    if let Node::Text { s: t } = n {
                        s.push_str(t);
                    }
                }
                Ok(Node::Verbatim { s })
            }
        }
    }

    /// Port of `Parser.skip_past(endtag)`.
    fn skip_past(&mut self, py: Python<'_>, endtag: &str) -> PyResult<()> {
        while let Some(token) = self.next_token() {
            if token.kind == TokenKind::Block && token.contents == endtag {
                return Ok(());
            }
        }
        Err(self.unclosed_block_tag(py, &[endtag]))
    }
    /// Port of `Parser.extend_nodelist` (`must_be_first`, `contains_nontext`
    /// and token/origin stamping). Bridged (Python) nodes need the stamping
    /// and the `must_be_first` getattr; native Rust nodes have no Python
    /// attribute surface, except native `{% extends %}` which is
    /// `must_be_first`.
    fn extend_nodelist(
        &mut self,
        py: Python<'_>,
        nodelist: &mut NodeList,
        node: Node,
        token: &Token,
    ) -> PyResult<()> {
        let must_be_first = match &node {
            Node::Tag {
                kind: TagKind::Extends,
                ..
            } => true,
            Node::Bridged(n) => n
                .bind(py)
                .getattr("must_be_first")
                .is_ok_and(|v| v.is_truthy().unwrap_or(false)),
            _ => false,
        };
        if must_be_first && nodelist.contains_nontext {
            let tn = self.origin.bind(py).getattr("template_name")?;
            let origin_str = if tn.is_truthy()? {
                tn.repr()?.to_string()
            } else {
                "the template".to_string()
            };
            return Err(self.error_msg(
                py,
                token,
                &format!("{{% {} %}} must be the first tag in {}.", token.contents, origin_str),
            ));
        }
        if let Node::Bridged(n) = &node {
            let nb = n.bind(py);
            let _ = nb.setattr("token", bridge::make_py_token(py, token));
            nb.setattr("origin", self.origin.bind(py))?;
        }
        if !matches!(node, Node::Text { .. }) {
            nodelist.contains_nontext = true;
        }
        nodelist.nodes.push(node);
        Ok(())
    }

    /// `raise self.error(token, msg)`.
    fn error_msg(&self, py: Python<'_>, token: &Token, msg: &str) -> PyErr {
        let e = syntax_error(py, msg);
        self.annotate_token(py, e, token)
    }

    /// Port of `Parser.error`: annotate with the originating token unless
    /// one is already set (innermost wins — a compile error inside an `if`
    /// body keeps its own token).
    // Mirrors Django's `Parser.error` method shape.
    #[allow(clippy::unused_self)]
    fn annotate_token(&self, py: Python<'_>, e: PyErr, token: &Token) -> PyErr {
        let exc = e.value(py);
        if !exc.hasattr("token").unwrap_or(false) {
            let _ = exc.setattr("token", bridge::make_py_token(py, token));
        }
        e
    }

    /// Port of `Parser.invalid_block_tag`.
    pub(crate) fn invalid_block_tag(
        &self,
        py: Python<'_>,
        token: &Token,
        command: &str,
        parse_until: &[&str],
    ) -> PyErr {
        let msg = if parse_until.is_empty() {
            format!(
                "Invalid block tag on line {}: '{}'. Did you forget to \
                 register or load this tag?",
                token.lineno, command
            )
        } else {
            let expected: Vec<String> = parse_until.iter().map(|p| format!("'{p}'")).collect();
            format!(
                "Invalid block tag on line {}: '{}', expected {}. Did you \
                 forget to register or load this tag?",
                token.lineno,
                command,
                get_text_list(&expected, "or")
            )
        };
        self.error_msg(py, token, &msg)
    }

    /// Port of `Parser.unclosed_block_tag`.
    fn unclosed_block_tag(&mut self, py: Python<'_>, parse_until: &[&str]) -> PyErr {
        let Some((command, token)) = self.command_stack.pop() else {
            return syntax_error(
                py,
                "internal error: unclosed_block_tag with empty command_stack",
            );
        };
        let msg = format!(
            "Unclosed tag on line {}: '{}'. Looking for one of: {}.",
            token.lineno,
            command,
            parse_until.join(", ")
        );
        self.error_msg(py, &token, &msg)
    }

    /// `compile_filter(token)`: parse + bind filter callables (existence and
    /// arity checked here, like `find_filter`/`args_check`). `{% load %}`-added
    /// filters (`local_filters`) shadow the Engine registry, mirroring stock
    /// `find_filter` checking `self.filters` (parser-level) only.
    pub fn compile_filter(
        &self,
        py: Python<'_>,
        token: &str,
    ) -> Result<CompiledExpr, FxError> {
        let lookup = Lookup {
            local: &self.local_filters,
            registry: self.filters,
        };
        let fe = varparse::compile_filter(token, self.ver, &lookup)?;
        let mut filters = Vec::with_capacity(fe.filters.len());
        for spec in &fe.filters {
            let entry = lookup
                .entry(&spec.name)
                .ok_or_else(|| FxError::InvalidFilter(spec.name.clone()))?;
            filters.push(BoundFilter {
                func: entry.func.clone_ref(py),
                needs_autoescape: entry.needs_autoescape,
                expects_localtime: entry.expects_localtime,
                is_safe: entry.is_safe,
                native: entry.native,
                string_coerced: entry.string_coerced,
                args: spec.args.clone(),
            });
        }
        Ok(CompiledExpr { fe, filters })
    }

    /// Hand `token` to a bridged Python compile function via the shim parser.
    /// The shim is created lazily (first bridged tag) and reused, so its
    /// `add_library` side effects (from stock `{% load %}`) persist across
    /// the whole template parse.
    fn compile_bridged(
        &mut self,
        py: Python<'_>,
        func: &Bound<'_, PyAny>,
        token: &Token,
    ) -> PyResult<Node> {
        let node = self.call_shim(py, func, token)?;
        let shim = self
            .shim
            .as_ref()
            .ok_or_else(|| syntax_error(py, "internal error: missing shim parser"))?;
        shim.sync_new_entries(
            py,
            self.ver,
            false,
            &mut self.tags,
            self.base_tags,
            &mut self.local_filters,
            self.filters,
        )?;
        self.shim_extra = Some(shim.extra_data(py)?);
        Ok(Node::Bridged(node))
    }

    /// Run `compile_func(shim, token)` on the shim parser: load the
    /// not-yet-consumed tokens into it, arm the `ParseHook` so `parse()`
    /// calls inside continue on this parser, and map the leftover shim
    /// stream back afterwards.
    fn call_shim(
        &mut self,
        py: Python<'_>,
        compile_func: &Bound<'_, PyAny>,
        token: &Token,
    ) -> PyResult<Py<PyAny>> {
        self.ensure_shim(py)?;
        let (p, hook) = {
            let shim = self
                .shim
                .as_ref()
                .ok_or_else(|| syntax_error(py, "internal error: missing shim parser"))?;
            shim.load_tokens(py, &self.tokens)?;
            (shim.parser.clone_ref(py), shim.hook.clone_ref(py))
        };
        let p = p.bind(py);
        let tok = bridge::make_py_token(py, token);
        // Stock parse loop bookkeeping: the command stack identifies the
        // unclosed tag when inner parse() recursion fails.
        let command = token.contents.split_whitespace().next().unwrap_or("");
        let stack = p.getattr("command_stack")?;
        stack.call_method1("append", ((command, tok.bind(py)),))?;
        // While Python runs, this parser is reached only through the armed
        // hook (nothing below touches `self` until the call returns).
        let prev = hook.get().arm(std::ptr::from_mut(self));
        let res = compile_func.call1((p, tok.bind(py)));
        hook.get().swap(prev);
        let shim = self
            .shim
            .as_ref()
            .ok_or_else(|| syntax_error(py, "internal error: missing shim parser"))?;
        let node = match res {
            Ok(node) => node,
            Err(e) => {
                // Best effort: a compile function catching this error
                // keeps parsing from the shim's stream.
                let _ = shim.harvest_tokens(py, &mut self.tokens);
                return Err(e);
            }
        };
        stack.call_method0("pop")?;
        shim.harvest_tokens(py, &mut self.tokens)?;
        Ok(node.unbind())
    }

    /// `parser.parse(parse_until)` called on the shim by a bridged compile
    /// function (via `ParseHook`): pick up the shim's token stream, parse
    /// natively, hand the leftover stream back, and return the body as a
    /// `NativeBodyNodeList`. `None` (stock loop) for a `parse_until` other
    /// than `None` or a list/tuple of `str`, where stock `in` semantics
    /// differ from name matching.
    fn hook_parse(
        &mut self,
        py: Python<'_>,
        parse_until: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Option<Py<PyAny>>> {
        let mut until: Vec<String> = Vec::new();
        if let Some(pu) = parse_until.filter(|v| !v.is_none()) {
            let items: Vec<Bound<'_, PyAny>> = if let Ok(t) = pu.cast_exact::<PyTuple>() {
                t.iter().collect()
            } else if let Ok(l) = pu.cast_exact::<PyList>() {
                l.iter().collect()
            } else {
                return Ok(None);
            };
            for it in items {
                if !it.is_exact_instance_of::<PyString>() {
                    return Ok(None);
                }
                until.push(it.extract()?);
            }
        }
        self.shim
            .as_ref()
            .ok_or_else(|| syntax_error(py, "internal error: missing shim parser"))?
            .harvest_tokens(py, &mut self.tokens)?;
        let refs: Vec<&str> = until.iter().map(String::as_str).collect();
        let parsed = self.parse(py, &refs);
        // `{% load %}` inside the body is parser-global in stock: the
        // compile function's own later `compile_filter` calls see it.
        self.ensure_shim(py)?;
        self.shim
            .as_ref()
            .ok_or_else(|| syntax_error(py, "internal error: missing shim parser"))?
            .load_tokens(py, &self.tokens)?;
        let nodelist = parsed?;
        let (nl, body) = crate::nodes_py::wrap_body(
            py,
            nodelist,
            self.engine.bind(py),
            self.origin.bind(py),
            self.dbg_source.as_ref(),
        )?;
        self.bodies.push(body);
        Ok(Some(nl.unbind()))
    }

    /// A registered compile function: stock `simple_tag`/`inclusion_tag`
    /// closures compile natively (`tags::helper`); anything else — and
    /// any helper parse that would raise — runs bridged.
    fn compile_func_tag(
        &mut self,
        py: Python<'_>,
        func: &Bound<'_, PyAny>,
        token: &Token,
    ) -> PyResult<Node> {
        if let Some(payload) = crate::tags::parse::translate(py, func, token) {
            let id = self.next_id;
            self.next_id += 1;
            return Ok(Node::Tag {
                kind: TagKind::Translate,
                payload: Box::new(payload),
                id,
            });
        }
        if let Some(spec) = crate::tags::helper::helper_spec(py, func, self.ver) {
            if let Some(payload) = crate::tags::helper::parse(py, self, &spec, token) {
                let kind = match spec.kind {
                    crate::tags::helper::HelperKind::Simple => TagKind::SimpleTag,
                    crate::tags::helper::HelperKind::Inclusion => TagKind::InclusionTag,
                };
                let id = self.next_id;
                self.next_id += 1;
                return Ok(Node::Tag {
                    kind,
                    payload: Box::new(payload),
                    id,
                });
            }
        }
        self.compile_bridged(py, func, token)
    }

    /// Lazily build the real-`Parser` shim; its `add_library` side effects
    /// persist across the whole template parse.
    fn ensure_shim(&mut self, py: Python<'_>) -> PyResult<()> {
        if self.shim.is_none() {
            self.shim = Some(ShimParser::new(
                py,
                self.engine.bind(py),
                self.origin.bind(py),
            )?);
        }
        if !self.pending_libs.is_empty() {
            if let Some(shim) = &self.shim {
                for lib in self.pending_libs.drain(..) {
                    shim.add_library(py, lib.bind(py))?;
                }
            }
        }
        Ok(())
    }

    /// `{% load a b ... %}` without the shim: when `load` is the stock
    /// compile function and every name is a registered library, apply stock
    /// `add_library` semantics (`tags.update` / `filters.update`) to the
    /// overlays directly. The `from` form, unknown names (error messages)
    /// and patched `load` take the shim path (`Ok(false)`).
    fn native_load_fast(&mut self, py: Python<'_>, token: &Token) -> PyResult<bool> {
        // `str.split()` also splits on \x1c-\x1f (not Unicode White_Space).
        if token.contents.chars().any(|c| ('\x1c'..='\x1f').contains(&c)) {
            return Ok(false);
        }
        let bits: Vec<&str> = token.contents.split_whitespace().collect();
        if bits.len() >= 4 && bits[bits.len() - 2] == "from" {
            return Ok(false);
        }
        let Some(func) = self.tag_func(py, "load") else {
            return Ok(false);
        };
        if !func.bind(py).is(stock_load(py)?) {
            return Ok(false);
        }
        let libraries = self.engine.bind(py).getattr("template_libraries")?;
        let libraries = libraries.cast::<PyDict>()?;
        let mut libs = Vec::with_capacity(bits.len().saturating_sub(1));
        for name in &bits[1..] {
            match libraries.get_item(*name)? {
                Some(lib) => libs.push(lib),
                None => return Ok(false),
            }
        }
        for lib in libs {
            for item in lib.getattr("tags")?.cast_into::<PyDict>()?.iter() {
                let (key, f) = item;
                let name = key.cast::<PyString>()?.to_str()?;
                let existing = self.tags.get(name).or_else(|| self.base_tags.get(name));
                if let Some(existing) = existing {
                    if existing.bind(py).is(&f) {
                        continue;
                    }
                    // Re-registration (e.g. overriding a builtin tag):
                    // the loaded stock func wins over the native one.
                    self.load_overrides.insert(name.to_owned());
                }
                self.tags.insert(name.to_owned(), f.unbind());
            }
            for (key, f) in lib.getattr("filters")?.cast_into::<PyDict>()?.iter() {
                let name = key.cast::<PyString>()?.to_str()?;
                if let Some(e) = self.filters.entries.get(name) {
                    if e.func.bind(py).is(&f) {
                        self.local_filters.remove(name);
                        continue;
                    }
                }
                if let Some(e) = self.local_filters.get(name) {
                    if e.func.bind(py).is(&f) {
                        continue;
                    }
                }
                let meta = bridge::filter_meta(py, &f, self.ver)?;
                let native = crate::filters::native_for(py, &f, name)?;
                self.local_filters.insert(
                    name.to_owned(),
                    PyFilterEntry {
                        arity: meta.arity,
                        needs_autoescape: meta.needs_autoescape,
                        expects_localtime: meta.expects_localtime,
                        is_safe: meta.is_safe,
                        native,
                        string_coerced: meta.string_coerced,
                        func: f.unbind(),
                    },
                );
            }
            self.pending_libs.push(lib.unbind());
        }
        Ok(true)
    }

    /// Native `{% load %}`: stock `do_load` runs on the shim parser (so
    /// `add_library` semantics — tags, filters, libraries — are stock's),
    /// then the shim's dicts are mirrored back. A tag whose function object
    /// changed goes into `load_overrides`: dispatch then routes that name
    /// bridged so the loaded implementation wins over the native one.
    /// Filters added by `{% load %}` land in `local_filters` via
    /// `sync_new_entries` (which only fills missing entries).
    pub(crate) fn native_load(&mut self, py: Python<'_>, token: &Token) -> PyResult<()> {
        if self.native_load_fast(py, token)? {
            return Ok(());
        }
        let func = self
            .tag_func(py, "load")
            .ok_or_else(|| syntax_error(py, "internal error: no bridged 'load' tag"))?;
        self.call_shim(py, func.bind(py), token)?;
        let shim = self
            .shim
            .as_ref()
            .ok_or_else(|| syntax_error(py, "internal error: missing shim parser"))?;
        // `sync_new_entries` merges both shim dicts into the overlays and
        // reports re-registered tag names; dispatch routes those bridged so
        // the loaded stock func wins over the native one.
        let changed = shim.sync_new_entries(
            py,
            self.ver,
            true,
            &mut self.tags,
            self.base_tags,
            &mut self.local_filters,
            self.filters,
        )?;
        self.load_overrides.extend(changed);
        Ok(())
    }

    /// The `extra_data["partials"]` dict built by native `{% partialdef %}`,
    /// created on first use (so `{% partial %}` can bind it before any def).
    pub(crate) fn native_partials(&mut self, py: Python<'_>) -> Py<PyDict> {
        if let Some(d) = &self.native_partials {
            return d.clone_ref(py);
        }
        let d = PyDict::new(py).unbind();
        self.native_partials = Some(d.clone_ref(py));
        d
    }

    /// The shim's `extra_data` after parsing plus the native `{% partialdef %}`
    /// partials dict (6.1). Native entries win on key collisions (only
    /// reachable when a template mixes bridged and native partial defs).
    #[must_use]
    pub fn take_extra_data(&mut self, py: Python<'_>) -> Option<Py<PyAny>> {
        let native = self.native_partials.take();
        let shim = self.shim_extra.take();
        match (shim, native) {
            (shim, None) => shim,
            (None, Some(native)) => {
                let d = PyDict::new(py);
                let _ = d.set_item("partials", native);
                Some(d.into_any().unbind())
            }
            (Some(shim), Some(native)) => {
                if let Some(md) = shim
                    .bind(py)
                    .get_item("partials")
                    .ok()
                    .and_then(|v| v.cast_into::<PyDict>().ok())
                {
                    for (k, v) in native.bind(py) {
                        let _ = md.set_item(k, v);
                    }
                } else {
                    let _ = shim.bind(py).set_item("partials", native);
                }
                Some(shim)
            }
        }
    }

}
/// `django.template.defaulttags.load` (the stock `{% load %}` compile func).
fn stock_load(py: Python<'_>) -> PyResult<&Bound<'_, PyAny>> {
    static F: pyo3::sync::PyOnceLock<Py<PyAny>> = pyo3::sync::PyOnceLock::new();
    Ok(F.get_or_try_init(py, || -> PyResult<Py<PyAny>> {
        Ok(py.import("django.template.defaulttags")?.getattr("load")?.unbind())
    })?
    .bind(py))
}

/// Port of `django.template.base.token_kwargs`.
///
/// # Panics
/// Never; the static kwarg regex is a valid literal pattern.
pub fn token_kwargs(
    py: Python<'_>,
    parser: &Parser<'_>,
    bits: &mut Vec<String>,
    support_legacy: bool,
) -> Result<Vec<(String, CompiledExpr)>, FxError> {
    // `^` mirrors Python `re.match` anchoring.
    static_re!(KWARG_RE, r"^(?:(\w+)=)?(.+)");
    if bits.is_empty() {
        return Ok(Vec::new());
    }
    let kwarg_format = KWARG_RE
        .captures(&bits[0])
        .is_some_and(|c| c.get(1).is_some_and(|m| !m.as_str().is_empty()));
    if !kwarg_format {
        if !support_legacy {
            return Ok(Vec::new());
        }
        if bits.len() < 3 || bits[1] != "as" {
            return Ok(Vec::new());
        }
    }
    // Python dict semantics: insertion order preserved; a duplicate key
    // updates in place (keeps its first insertion position).
    let mut kwargs: Vec<(String, CompiledExpr)> = Vec::new();
    while !bits.is_empty() {
        let (key, value) = if kwarg_format {
            match KWARG_RE.captures(&bits[0]) {
                Some(c) => match (c.get(1), c.get(2)) {
                    (Some(k), Some(v)) if !k.as_str().is_empty() => {
                        let k = k.as_str().to_string();
                        let v = v.as_str().to_string();
                        bits.remove(0);
                        (k, v)
                    }
                    _ => return Ok(kwargs),
                },
                None => return Ok(kwargs),
            }
        } else {
            if bits.len() < 3 || bits[1] != "as" {
                return Ok(kwargs);
            }
            let key = bits[2].clone();
            let value = bits[0].clone();
            bits.drain(0..3);
            (key, value)
        };
        let compiled = parser.compile_filter(py, &value)?;
        if let Some(slot) = kwargs.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = compiled;
        } else {
            kwargs.push((key, compiled));
        }
        if !kwarg_format && !bits.is_empty() {
            if bits[0] != "and" {
                return Ok(kwargs);
            }
            bits.remove(0);
        }
    }
    Ok(kwargs)
}

/// `find_filter` lookup: per-parse loaded filters shadow the Engine-wide
/// bridged registry.
struct Lookup<'r> {
    local: &'r HashMap<String, PyFilterEntry>,
    registry: &'r FilterRegistry,
}

impl Lookup<'_> {
    fn entry(&self, name: &str) -> Option<&PyFilterEntry> {
        self.local
            .get(name)
            .or_else(|| self.registry.entries.get(name))
    }
}

impl FilterLookup for Lookup<'_> {
    fn filter_arity(&self, name: &str) -> Option<(usize, usize)> {
        self.entry(name).map(|e| e.arity)
    }
}

/// 6.1 `compile_nodelist` error prefixing: `Template: {origin}, ` onto
/// `TemplateSyntaxError` args (raw message kept as `raw_error_message`).
pub fn prefix_error_61(py: Python<'_>, origin: &Bound<'_, PyAny>, e: &PyErr) -> PyResult<()> {
    let stype = syntax_error_type(py)?;
    if !e.is_instance(py, stype.bind(py)) {
        return Ok(());
    }
    let unknown = py.import("django.template.base")?.getattr("UNKNOWN_SOURCE")?;
    let name = origin.getattr("name")?;
    if name.eq(&unknown)? {
        return Ok(());
    }
    let exc = e.value(py);
    let args = exc.getattr("args")?;
    let nargs = args.len()?;
    if nargs == 0 {
        return Ok(());
    }
    let raw = args.get_item(0)?;
    exc.setattr("raw_error_message", &raw)?;
    let msg = format!("Template: {}, {}", name.str()?, raw.str()?);
    let mut items: Vec<Bound<'_, PyAny>> = vec![PyString::new(py, &msg).into_any()];
    for i in 1..nargs {
        items.push(args.get_item(i)?);
    }
    exc.setattr("args", PyTuple::new(py, items)?)?;
    Ok(())
}

/// The shim's `_dtl_parse`. While the Rust parser runs a bridged compile
/// function (`Parser::call_shim`), it is armed with that parser and the
/// running thread, and `parser.parse()` on the shim continues natively
/// (`Parser::hook_parse`). Disarmed — and a call from any other thread —
/// returns `None` (stock loop).
#[pyclass(module = "django_rust_templates._dtl", frozen)]
#[derive(Default)]
pub struct ParseHook {
    armed: std::sync::Mutex<Option<(usize, std::thread::ThreadId)>>,
}

type Armed = Option<(usize, std::thread::ThreadId)>;

impl ParseHook {
    /// Swap the armed state, returning the previous one.
    fn swap(&self, state: Armed) -> Armed {
        let mut g = self
            .armed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::replace(&mut *g, state)
    }

    fn arm(&self, parser: *mut Parser<'_>) -> Armed {
        self.swap(Some((parser as usize, std::thread::current().id())))
    }
}

#[pymethods]
impl ParseHook {
    #[pyo3(signature = (parse_until=None))]
    fn __call__(
        &self,
        py: Python<'_>,
        parse_until: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Option<Py<PyAny>>> {
        // Disarm for the Rust parse below: only a nested `call_shim`
        // (re-arming for its own compile call) may hand the pointer out.
        let state = self.swap(None);
        let Some((ptr, thread)) = state else {
            return Ok(None);
        };
        if thread != std::thread::current().id() {
            self.swap(state);
            return Ok(None);
        }
        // SAFETY: armed only by `call_shim` for the duration of its
        // compile-function call, from the `&mut Parser` it holds and does
        // not touch until that call returns; disarmed while this Rust parse
        // runs, so no second `&mut` is ever handed out concurrently.
        let parser = unsafe { &mut *(ptr as *mut Parser<'static>) };
        let res = parser.hook_parse(py, parse_until);
        self.swap(state);
        res
    }
}
