//! Compile-time handlers for the built-in tags: ports of `do_*` in
//! `django/template/defaulttags.py` and `django/template/loader_tags.py`.
//!
//! Every handler receives the parser and the tag token, performs its own
//! splitting (plain `split()` vs `split_contents()`, per stock comment on
//! each tag), and returns the `TagPayload`; `Parser::compile_native`
//! pre-assigns the node id and wraps the payload in `Node::Tag`. Handlers
//! raise raw `TemplateSyntaxError`s without token annotation — `Parser::parse`
//! annotates (innermost wins), mirroring stock `Parser.error`.

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyString};
use regex::Regex;
use std::sync::LazyLock;

use crate::error::syntax_error;
use crate::lexer::{split_contents, SplitContents, Token};
use crate::nodes::{Node, NodeList};
use crate::parser::{token_kwargs, CompiledExpr, Parser};
use crate::varparse::PyVer;

use super::{expr, next_state_id, Expr, TagPayload};

type Res = Result<TagPayload, PyErr>;

/// `FxError` → `TemplateSyntaxError` (stock raises TSE for filter-syntax
/// and invalid-filter failures out of `compile_filter`).
fn fx(py: Python<'_>, e: &crate::varparse::FxError) -> PyErr {
    syntax_error(py, &e.to_string())
}

/// `parser.compile_filter` raising TSE directly.
fn compile(p: &Parser<'_>, py: Python<'_>, s: &str) -> Result<CompiledExpr, PyErr> {
    p.compile_filter(py, s).map_err(|e| fx(py, &e))
}

/// `token.split_contents()`. Stock raises `Unterminated string in tag:`
/// from the lexer on an unterminated `_()`/`translate()` literal.
fn split(py: Python<'_>, token: &Token) -> Result<Vec<String>, PyErr> {
    match split_contents(&token.contents) {
        SplitContents::Ok(v) => Ok(v),
        SplitContents::UnterminatedI18n => Err(syntax_error(
            py,
            &format!("Unterminated string in tag: {}", token.contents),
        )),
    }
}

/// Python `repr(str)` for `%r` message formatting.
fn py_repr(py: Python<'_>, s: &str) -> PyResult<String> {
    Ok(PyString::new(py, s).repr()?.to_string())
}

/// `parser.origin.template_name` as `Option<String>`.
fn origin_template_name(p: &Parser<'_>, py: Python<'_>) -> PyResult<Option<String>> {
    let tn = p.origin_obj().getattr(py, "template_name")?;
    let tn = tn.bind(py);
    if tn.is_none() {
        Ok(None)
    } else {
        Ok(Some(tn.extract()?))
    }
}

/// `loader_tags.construct_relative_path(current, name [, allow_recursion])`.
fn construct_relative(
    py: Python<'_>,
    current: Option<&str>,
    name: &str,
    allow_recursion: Option<bool>,
) -> PyResult<String> {
    let f = super::fns(py)?.construct_relative_path.bind(py).clone();
    let cur = match current {
        Some(s) => PyString::new(py, s).into_any(),
        None => py.None().into_bound(py),
    };
    let res = match allow_recursion {
        Some(r) => {
            let kw = PyDict::new(py);
            kw.set_item("allow_recursion", r)?;
            f.call((cur, PyString::new(py, name)), Some(&kw))?
        }
        None => f.call1((cur, PyString::new(py, name)))?,
    };
    res.extract()
}

/// `re.split(r" *, *", s)` — the `for` tag's loopvar split.
static LOOPVAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::unwrap_used)] // literal pattern, cannot fail
    Regex::new(r" *, *").unwrap()
});

/// `kwarg_re` from `django.template.base` (`url` tag arguments).
static URL_KWARG_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::unwrap_used)] // literal pattern, cannot fail
    Regex::new(r"^(?:(\w+)=)?(.+)").unwrap()
});

// ---------------------------------------------------------------------------
// loader_tags
// ---------------------------------------------------------------------------

/// `{% block name %}...{% endblock %}` — `do_block`.
pub(crate) fn block(py: Python<'_>, p: &mut Parser<'_>, token: &Token) -> Res {
    // "split_contents() isn't useful here": plain split.
    let bits: Vec<&str> = token.contents.split_whitespace().collect();
    if bits.len() != 2 {
        return Err(syntax_error(
            py,
            &format!("'{}' tag takes only one argument", bits[0]),
        ));
    }
    let name = bits[1].to_string();
    {
        let loaded = p.loaded_blocks_mut();
        if loaded.contains(&name) {
            return Err(syntax_error(
                py,
                &format!(
                    "'{}' tag with name '{}' appears more than once",
                    bits[0], name
                ),
            ));
        }
        loaded.push(name.clone());
    }
    let nodelist = p.parse(py, &["endblock"])?;

    // "This check is kept for backwards-compatibility. See #3100."
    let endblock = p.next_token_pub().unwrap_or_else(|| token.clone());
    let with_name = format!("endblock {name}");
    if endblock.contents != "endblock" && endblock.contents != with_name {
        return Err(p.invalid_block_tag(py, &endblock, "endblock", &["endblock", &with_name]));
    }
    Ok(TagPayload::Block {
        name,
        simple: super::blocks::body_is_simple(&nodelist),
        nodelist,
        token: token.clone(),
    })
}

/// `{% extends "name" %}` — `do_extends` / `ExtendsNode`.
pub(crate) fn extends(py: Python<'_>, p: &mut Parser<'_>, token: &Token) -> Res {
    let mut bits = split(py, token)?;
    if bits.len() != 2 {
        return Err(syntax_error(
            py,
            &format!("'{}' takes one argument", bits[0]),
        ));
    }
    bits[1] = construct_relative(py, origin_template_name(p, py)?.as_deref(), &bits[1], None)?;
    let compiled = compile(p, py, &bits[1])?;
    let token_text = bits[1].clone();
    let has_filters = !compiled.filters.is_empty();
    let var_text = compiled.fe.is_var.then(|| token_text.clone());
    let nodelist = p.parse(py, &[])?;
    if contains_native_extends(&nodelist) {
        return Err(syntax_error(
            py,
            &format!("'{}' cannot appear more than once in the same template", bits[0]),
        ));
    }
    Ok(TagPayload::Extends {
        parent: expr(compiled),
        token_text,
        has_filters,
        var_text,
        nodelist,
        token: token.clone(),
    })
}

/// `nodelist.get_nodes_by_type(ExtendsNode)` for the native tree (bridged
/// nodes never carry a native `ExtendsNode`).
fn contains_native_extends(list: &NodeList) -> bool {
    list.nodes.iter().any(|n| match n {
        Node::Tag { payload, .. } => {
            matches!(&**payload, TagPayload::Extends { .. })
                || super::tag_child_nodelists(payload)
                    .iter()
                    .any(|c| contains_native_extends(c))
        }
        Node::AutoEscape { nodelist, .. } => contains_native_extends(nodelist),
        _ => false,
    })
}

/// `{% include "name" [with k=v]... [only] %}` — `do_include`.
pub(crate) fn include(py: Python<'_>, p: &mut Parser<'_>, token: &Token) -> Res {
    let mut bits = split(py, token)?;
    if bits.len() < 2 {
        return Err(syntax_error(
            py,
            &format!(
                "{} tag takes at least one argument: the name of the template to be included.",
                py_repr(py, bits.first().map_or("", |b| b.as_str()))?
            ),
        ));
    }
    let mut options: Vec<String> = Vec::new();
    let mut with_kwargs: Vec<(String, CompiledExpr)> = Vec::new();
    let mut remaining = bits[2..].to_vec();
    while !remaining.is_empty() {
        let option = remaining.remove(0);
        if options.contains(&option) {
            return Err(syntax_error(
                py,
                &format!(
                    "The {} option was specified more than once.",
                    py_repr(py, &option)?
                ),
            ));
        }
        if option == "with" {
            let value = token_kwargs(py, p, &mut remaining, false).map_err(|e| fx(py, &e))?;
            if value.is_empty() {
                return Err(syntax_error(
                    py,
                    &format!(
                        "\"with\" in {} tag needs at least one keyword argument.",
                        py_repr(py, &bits[0])?
                    ),
                ));
            }
            with_kwargs = value;
        } else if option != "only" {
            return Err(syntax_error(
                py,
                &format!(
                    "Unknown argument for {} tag: {}.",
                    py_repr(py, &bits[0])?,
                    py_repr(py, &option)?
                ),
            ));
        }
        options.push(option);
    }
    let only = options.iter().any(|o| o == "only");
    let origin_name = origin_template_name(p, py)?;
    bits[1] = construct_relative(py, origin_name.as_deref(), &bits[1], Some(true))?;
    let template = expr(compile(p, py, &bits[1])?);
    Ok(TagPayload::Include {
        template,
        with: with_kwargs.into_iter().map(|(k, c)| (k, expr(c))).collect(),
        only,
        state_id: next_state_id(),
        origin_name,
        const_name: std::sync::OnceLock::new(),
        token: token.clone(),
    })
}

/// `{% partialdef name [inline] %}...{% endpartialdef %}` (6.1) —
/// `partialdef_func`.
pub(crate) fn partialdef(py: Python<'_>, p: &mut Parser<'_>, id: usize, token: &Token) -> Res {
    let bits = split(py, token)?;
    let (name, inline) = match bits.len() {
        0 | 1 => return Err(syntax_error(py, "'partialdef' tag requires a name")),
        2 => (bits[1].clone(), false),
        3 => {
            if bits[2] == "inline" {
                (bits[1].clone(), true)
            } else {
                return Err(syntax_error(
                    py,
                    "The 'inline' argument does not have any parameters; either use \
                     'inline' or remove it completely.",
                ));
            }
        }
        _ => return Err(syntax_error(py, "'partialdef' tag takes at most 2 arguments")),
    };
    let valid = [String::from("endpartialdef"), format!("endpartialdef {name}")];
    let source_start = token.position.as_ref().map(|pos| pos.0);
    let nodelist = {
        let until: Vec<&str> = valid.iter().map(String::as_str).collect();
        p.parse(py, &until)?
    };
    let end = p.next_token_pub().unwrap_or_else(|| token.clone());
    if end.contents != valid[0] && end.contents != valid[1] {
        let until: Vec<&str> = valid.iter().map(String::as_str).collect();
        return Err(p.invalid_block_tag(py, &end, "endpartialdef", &until));
    }
    let source_end = end.position.as_ref().map(|pos| pos.1);

    // `parser.extra_data.setdefault("partials", {})[name] = PartialTemplate`
    let partials = p.native_partials(py);
    let pb = partials.bind(py);
    if pb.contains(name.as_str())? {
        let origin_name = p.origin_obj().getattr(py, "name")?.bind(py).str()?.to_string();
        return Err(syntax_error(
            py,
            &format!("Partial '{name}' is already defined in the '{origin_name}' template."),
        ));
    }
    let proxy = py
        .import("django_rust_templates._nodes")?
        .getattr("PartialTemplate")?
        .call1((
            name.as_str(),
            p.origin_obj().bind(py).clone(),
            source_start,
            source_end,
            id,
        ))?;
    pb.set_item(name.as_str(), &proxy)?;
    Ok(TagPayload::PartialDef {
        name,
        inline,
        body: nodelist,
        token: token.clone(),
    })
}

/// `{% partial name %}` (6.1) — `partial_func`.
pub(crate) fn partial(py: Python<'_>, p: &mut Parser<'_>, token: &Token) -> Res {
    let bits = split(py, token)?;
    if bits.len() != 2 {
        return Err(syntax_error(py, "'partial' tag requires a single argument"));
    }
    // The stock `DeferredSubDict` resolves lazily: `{% partial %}` before
    // any `{% partialdef %}` still parses, failing at render if absent.
    let partials = p.native_partials(py);
    Ok(TagPayload::Partial {
        name: bits[1].clone(),
        partials,
        token: token.clone(),
    })
}

// ---------------------------------------------------------------------------
// defaulttags
// ---------------------------------------------------------------------------

/// `{% if %}...{% elif %}...{% else %}...{% endif %}` — `do_if`.
pub(crate) fn if_tag(py: Python<'_>, p: &mut Parser<'_>, token: &Token) -> Res {
    let bits = split(py, token)?[1..].to_vec();
    let condition = ifexpr_parse(py, p, &bits)?;
    let nodelist = p.parse(py, &["elif", "else", "endif"])?;
    let mut arms: Vec<(Option<crate::ifexpr::IfExpr>, NodeList)> = vec![(Some(condition), nodelist)];
    let mut t = p.next_token_pub().unwrap_or_else(|| token.clone());
    while t.contents.starts_with("elif") {
        let bits = split(py, &t)?[1..].to_vec();
        let condition = ifexpr_parse(py, p, &bits)?;
        let nodelist = p.parse(py, &["elif", "else", "endif"])?;
        arms.push((Some(condition), nodelist));
        t = p.next_token_pub().unwrap_or_else(|| token.clone());
    }
    if t.contents == "else" {
        let nodelist = p.parse(py, &["endif"])?;
        arms.push((None, nodelist));
        t = p.next_token_pub().unwrap_or_else(|| token.clone());
    }
    if t.contents != "endif" {
        return Err(syntax_error(
            py,
            &format!(
                "Malformed template tag at line {}: \"{}\"",
                t.lineno, t.contents
            ),
        ));
    }
    Ok(TagPayload::If {
        arms,
        token: token.clone(),
    })
}

/// `TemplateIfParser(parser, bits).parse()`.
fn ifexpr_parse(
    py: Python<'_>,
    p: &Parser<'_>,
    bits: &[String],
) -> Result<crate::ifexpr::IfExpr, PyErr> {
    crate::ifexpr::parse_condition(bits, |s| p.compile_filter(py, s))
        .map_err(|e| fx(py, &e))
}

/// `{% ifchanged [vars] %}...[{% else %}...]...{% endifchanged %}`.
pub(crate) fn ifchanged(py: Python<'_>, p: &mut Parser<'_>, token: &Token) -> Res {
    let bits = split(py, token)?;
    let t = p.parse(py, &["else", "endifchanged"])?;
    let f = match p.next_token_pub() {
        Some(t) if t.contents == "else" => {
            let nl = p.parse(py, &["endifchanged"])?;
            p.delete_first_token();
            nl
        }
        _ => NodeList::new(),
    };
    let values = bits[1..]
        .iter()
        .map(|b| compile(p, py, b).map(expr))
        .collect::<Result<Vec<Expr>, PyErr>>()?;
    Ok(TagPayload::IfChanged {
        values,
        t,
        f,
        state_id: next_state_id(),
        token: token.clone(),
    })
}

/// `{% for x in y [reversed] %}...[{% empty %}...]...{% endfor %}` — `do_for`.
pub(crate) fn for_tag(py: Python<'_>, p: &mut Parser<'_>, token: &Token) -> Res {
    let bits = split(py, token)?;
    if bits.len() < 4 {
        return Err(syntax_error(
            py,
            &format!(
                "'for' statements should have at least four words: {}",
                token.contents
            ),
        ));
    }
    let rev = bits[bits.len() - 1] == "reversed";
    let in_index = if rev { bits.len() - 3 } else { bits.len() - 2 };
    if bits[in_index] != "in" {
        return Err(syntax_error(
            py,
            &format!(
                "'for' statements should use the format 'for x in y': {}",
                token.contents
            ),
        ));
    }
    let joined = bits[1..in_index].join(" ");
    let loopvars: Vec<String> = LOOPVAR_RE.split(&joined).map(str::to_string).collect();
    for var in &loopvars {
        if var.is_empty() || var.contains([' ', '"', '\'', '|']) {
            return Err(syntax_error(
                py,
                &format!("'for' tag received an invalid argument: {}", token.contents),
            ));
        }
    }
    let seq_text = bits[in_index + 1].clone();
    let seq = expr(compile(p, py, &seq_text)?);
    let mut body = p.parse(py, &["empty", "endfor"])?;
    let empty = match p.next_token_pub() {
        Some(t) if t.contents == "empty" => {
            let nl = p.parse(py, &["endfor"])?;
            p.delete_first_token();
            nl
        }
        _ => NodeList::new(),
    };
    let (forloop_free, forloop_used) =
        super::forloop_scan(&body, &empty, &seq, &loopvars);
    // A loop variable literally named `forloop` rebinds the name (stock
    // pushes the loopvar layer above the `forloop` dict), so output nodes
    // must resolve through the context, not the native slot.
    if !loopvars.iter().any(|v| v == "forloop") {
        super::rewrite_flvars(&mut body, forloop_used);
    }
    let loopkeys = loopvars.iter().map(|v| PyString::intern(py, v).unbind()).collect();
    Ok(TagPayload::For {
        loopkeys,
        loopvars,
        seq,
        rev,
        body,
        empty,
        with_length: p.ver() == PyVer::V61,
        seq_text,
        forloop_free,
        forloop_used,
        token: token.clone(),
    })
}

/// `{% with a=b ... %}...{% endwith %}` — `do_with`.
pub(crate) fn with_tag(py: Python<'_>, p: &mut Parser<'_>, token: &Token) -> Res {
    let bits = split(py, token)?;
    let mut remaining = bits[1..].to_vec();
    let extra = token_kwargs(py, p, &mut remaining, true).map_err(|e| fx(py, &e))?;
    if extra.is_empty() {
        return Err(syntax_error(
            py,
            &format!(
                "{} expected at least one variable assignment",
                py_repr(py, &bits[0])?
            ),
        ));
    }
    if let Some(first) = remaining.first() {
        return Err(syntax_error(
            py,
            &format!(
                "{} received an invalid token: {}",
                py_repr(py, &bits[0])?,
                py_repr(py, first)?
            ),
        ));
    }
    let body = p.parse(py, &["endwith"])?;
    p.delete_first_token();
    Ok(TagPayload::With {
        values: extra.into_iter().map(|(k, c)| (k, expr(c))).collect(),
        body,
        token: token.clone(),
    })
}

/// `{% filter f %}...{% endfilter %}` — `do_filter`.
pub(crate) fn filter_tag(py: Python<'_>, p: &mut Parser<'_>, token: &Token) -> Res {
    // `_, rest = token.contents.split(None, 1)` — plain split, maxsplit 1.
    let trimmed = token.contents.trim_start();
    let rest = match trimmed.find(|c: char| c.is_whitespace()) {
        Some(pos) => trimmed[pos..].trim_start(),
        None => {
            return Err(PyValueError::new_err(
                "not enough values to unpack (expected 2, got 1)",
            ))
        }
    };
    let compiled = compile(p, py, &format!("var|{rest}"))?;
    for b in &compiled.filters {
        let filter_name = b
            .func
            .getattr(py, "_filter_name")
            .ok()
            .and_then(|v| v.bind(py).extract::<String>().ok());
        if matches!(filter_name.as_deref(), Some("escape" | "safe")) {
            let msg = if p.ver() == PyVer::V61 {
                format!(
                    "\"filter {}\" is not permitted. Use the \"autoescape\" tag instead.",
                    filter_name.unwrap_or_default()
                )
            } else {
                format!(
                    "\"filter {}\" is not permitted.  Use the \"autoescape\" tag instead.",
                    filter_name.unwrap_or_default()
                )
            };
            return Err(syntax_error(py, &msg));
        }
    }
    let body = p.parse(py, &["endfilter"])?;
    p.delete_first_token();
    Ok(TagPayload::Filter {
        fe: expr(compiled),
        body,
        token: token.clone(),
    })
}

/// `{% firstof a b [as c] %}` — `firstof`.
pub(crate) fn firstof(py: Python<'_>, p: &mut Parser<'_>, token: &Token) -> Res {
    let mut bits = split(py, token)?[1..].to_vec();
    let mut asvar: Option<String> = None;
    if bits.is_empty() {
        return Err(syntax_error(
            py,
            "'firstof' statement requires at least one argument",
        ));
    }
    if bits.len() >= 2 && bits[bits.len() - 2] == "as" {
        asvar = bits.pop();
        bits.pop();
    }
    let vars = bits
        .iter()
        .map(|b| compile(p, py, b).map(expr))
        .collect::<Result<Vec<Expr>, PyErr>>()?;
    Ok(TagPayload::FirstOf {
        vars,
        asvar,
        token: token.clone(),
    })
}

/// `{% cycle a b %}` / `{% cycle a b as name [silent] %}` — `cycle`.
pub(crate) fn cycle(py: Python<'_>, p: &mut Parser<'_>, token: &Token) -> Res {
    let mut args = split(py, token)?;
    if args.len() < 2 {
        return Err(syntax_error(
            py,
            "'cycle' tag requires at least two arguments",
        ));
    }
    if args.len() == 2 {
        // `{% cycle foo %}` reuse of a named cycle.
        let name = args[1].clone();
        if !p.cycle_has_named() {
            return Err(syntax_error(
                py,
                &format!("No named cycles in template. '{name}' is not defined"),
            ));
        }
        let sid = p.cycle_named(&name).ok_or_else(|| {
            syntax_error(py, &format!("Named cycle '{name}' does not exist"))
        })?;
        let (values, var_name, silent) = p.cycle_values_of(sid);
        return Ok(TagPayload::Cycle {
            values,
            var_name,
            silent,
            state_id: sid,
            token: token.clone(),
        });
    }
    let mut as_form = false;
    let mut silent = false;
    if args.len() > 4 {
        // `{% cycle ... as foo [silent] %}` case.
        if args[args.len() - 3] == "as" {
            if args[args.len() - 1] != "silent" {
                return Err(syntax_error(
                    py,
                    &format!(
                        "Only 'silent' flag is allowed after cycle's name, not '{}'.",
                        args[args.len() - 1]
                    ),
                ));
            }
            as_form = true;
            silent = true;
            args.pop();
        } else if args[args.len() - 2] == "as" {
            as_form = true;
            silent = false;
        }
    }
    let sid = next_state_id();
    let mut var_name: Option<String> = None;
    let values: Vec<Expr> = if as_form {
        let name = args[args.len() - 1].clone();
        var_name = Some(name.clone());
        let vals: Vec<Expr> = args[1..args.len() - 2]
            .iter()
            .map(|b| compile(p, py, b))
            .collect::<Result<Vec<CompiledExpr>, PyErr>>()?
            .into_iter()
            .map(expr)
            .collect();
        p.cycle_register(name, sid, vals.clone(), silent);
        vals
    } else {
        args[1..]
            .iter()
            .map(|b| compile(p, py, b))
            .collect::<Result<Vec<CompiledExpr>, PyErr>>()?
            .into_iter()
            .map(expr)
            .collect()
    };
    // `parser._last_cycle_node = node` — set on every new node.
    p.set_last_cycle(sid);
    Ok(TagPayload::Cycle {
        values,
        var_name,
        silent,
        state_id: sid,
        token: token.clone(),
    })
}

/// `{% resetcycle [name] %}` — `resetcycle`.
pub(crate) fn resetcycle(py: Python<'_>, p: &mut Parser<'_>, token: &Token) -> Res {
    let args = split(py, token)?;
    if args.len() > 2 {
        return Err(syntax_error(
            py,
            &format!(
                "{} tag accepts at most one argument.",
                py_repr(py, &args[0])?
            ),
        ));
    }
    let state_id = if args.len() == 2 {
        let name = &args[1];
        p.cycle_named(name).ok_or_else(|| {
            syntax_error(py, &format!("Named cycle '{name}' does not exist."))
        })?
    } else {
        p.last_cycle().ok_or_else(|| syntax_error(py, "No cycles in template."))?
    };
    Ok(TagPayload::ResetCycle {
        state_id,
        token: token.clone(),
    })
}

/// Accepted `{% templatetag %}` arguments (the error message lists this order).
const TEMPLATETAG_ARGS: [&str; 8] = [
    "openblock",
    "closeblock",
    "openvariable",
    "closevariable",
    "openbrace",
    "closebrace",
    "opencomment",
    "closecomment",
];

/// `{% templatetag %}` — `templatetag`.
pub(crate) fn templatetag(py: Python<'_>, _p: &mut Parser<'_>, token: &Token) -> Res {
    // "split_contents() isn't useful here": plain split.
    let bits: Vec<&str> = token.contents.split_whitespace().collect();
    if bits.len() != 2 {
        return Err(syntax_error(
            py,
            "'templatetag' statement takes one argument",
        ));
    }
    let arg = bits[1];
    if !TEMPLATETAG_ARGS.contains(&arg) {
        return Err(syntax_error(
            py,
            &format!(
                "Invalid templatetag argument: '{arg}'. Must be one of: \
                 ['openblock', 'closeblock', 'openvariable', 'closevariable', \
                 'openbrace', 'closebrace', 'opencomment', 'closecomment']"
            ),
        ));
    }
    Ok(TagPayload::Templatetag {
        arg: arg.to_string(),
        token: token.clone(),
    })
}

/// `{% spaceless %}...{% endspaceless %}` — `spaceless`.
pub(crate) fn spaceless(py: Python<'_>, p: &mut Parser<'_>, token: &Token) -> Res {
    let body = p.parse(py, &["endspaceless"])?;
    p.delete_first_token();
    Ok(TagPayload::Spaceless {
        body,
        lit: std::sync::OnceLock::new(),
        token: token.clone(),
    })
}

/// The final string of a `{% spaceless %}` whose every body node renders a
/// constant (`Text`, `Verbatim`, `Comment`): the per-render trim +
/// `strip_spaces_between_tags` become a per-template constant, computed on
/// first render (kept off the parse path).
pub(crate) fn literal_spaceless(body: &NodeList) -> Option<String> {
    let mut buf = String::new();
    for n in &body.nodes {
        match n {
            Node::Text { s } | Node::Verbatim { s } => buf.push_str(s),
            Node::Comment => {}
            _ => return None,
        }
    }
    Some(super::simple::strip_spaces_between_tags(
        buf.trim_matches(super::simple::py_ws),
    ))
}

/// `{% lorem [count] [method] [random] %}` — `lorem`.
pub(crate) fn lorem(py: Python<'_>, p: &mut Parser<'_>, token: &Token) -> Res {
    let mut bits = split(py, token)?;
    let tagname = bits[0].clone();
    // Random bit.
    let common = match bits.last() {
        Some(last) if last == "random" => {
            bits.pop();
            false
        }
        _ => true,
    };
    // Method bit.
    let method = match bits.last() {
        Some(m) if m == "w" || m == "p" || m == "b" => bits.pop().unwrap_or_else(|| "b".into()),
        _ => "b".to_string(),
    };
    // Count bit.
    let count = if bits.len() > 1 {
        bits.pop().unwrap_or_else(|| "1".into())
    } else {
        "1".to_string()
    };
    let count = compile(p, py, &count)?;
    if bits.len() != 1 {
        return Err(syntax_error(
            py,
            &format!("Incorrect format for {} tag", py_repr(py, &tagname)?),
        ));
    }
    Ok(TagPayload::Lorem {
        count: expr(count),
        method,
        common,
        token: token.clone(),
    })
}

/// `{% now "fmt" [as var] %}` — `now`.
pub(crate) fn now(py: Python<'_>, _p: &mut Parser<'_>, token: &Token) -> Res {
    let mut bits = split(py, token)?;
    let mut asvar: Option<String> = None;
    if bits.len() == 4 && bits[bits.len() - 2] == "as" {
        asvar = Some(bits[bits.len() - 1].clone());
        bits.truncate(2);
    }
    if bits.len() != 2 {
        return Err(syntax_error(py, "'now' statement takes one argument"));
    }
    // `format_string = bits[1][1:-1]` (strip the quotes; char-safe).
    let chars: Vec<char> = bits[1].chars().collect();
    let fmt: String = if chars.len() >= 2 {
        chars[1..chars.len() - 1].iter().collect()
    } else {
        String::new()
    };
    Ok(TagPayload::Now {
        fmt,
        asvar,
        token: token.clone(),
    })
}

/// i18n `{% translate "literal" [as var] %}` when `func` is the stock
/// `do_translate`; `Ok(None)` for every other form (variables, filters,
/// `noop`, `context`, escapes or quotes inside the literal), which compile
/// bridged. The literal goes through stock's double literal handling
/// (`compile_filter`, then `Variable("'%s'" % var)`), which is the identity
/// for a literal without backslashes or quote characters.
pub(crate) fn translate(
    py: Python<'_>,
    func: &Bound<'_, PyAny>,
    token: &Token,
) -> Option<TagPayload> {
    static DO_TRANSLATE: pyo3::sync::PyOnceLock<Option<Py<PyAny>>> =
        pyo3::sync::PyOnceLock::new();
    let stock = DO_TRANSLATE.get_or_init(py, || {
        py.import("django.templatetags.i18n")
            .and_then(|m| m.getattr("do_translate"))
            .ok()
            .map(Bound::unbind)
    });
    if !stock.as_ref().is_some_and(|f| func.is(f.bind(py))) {
        return None;
    }
    let SplitContents::Ok(bits) = split_contents(&token.contents) else {
        return None;
    };
    let asvar = match bits.len() {
        2 => None,
        4 if bits[2] == "as" => Some(bits[3].clone()),
        _ => return None,
    };
    let lit = &bits[1];
    let quote = lit.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    if lit.len() < 2 || !lit.ends_with(quote) {
        return None;
    }
    let inner = &lit[1..lit.len() - 1];
    if inner.contains(['\\', '"', '\'']) {
        return None;
    }
    Some(TagPayload::Translate {
        msgid: inner.replace('%', "%%"),
        asvar,
        token: token.clone(),
    })
}

/// `{% url 'view' args kw=v [as var] %}` — `url`.
pub(crate) fn url(py: Python<'_>, p: &mut Parser<'_>, token: &Token) -> Res {
    let bits = split(py, token)?;
    if bits.len() < 2 {
        return Err(syntax_error(
            py,
            &format!("'{}' takes at least one argument, a URL pattern name.", bits[0]),
        ));
    }
    let view = expr(compile(p, py, &bits[1])?);
    let mut bits = bits[2..].to_vec();
    let mut asvar: Option<String> = None;
    if bits.len() >= 2 && bits[bits.len() - 2] == "as" {
        asvar = bits.pop();
        bits.pop();
    }
    let mut args: Vec<Expr> = Vec::new();
    let mut kwargs: Vec<(String, Expr)> = Vec::new();
    for bit in &bits {
        let caps = URL_KWARG_RE
            .captures(bit)
            .ok_or_else(|| syntax_error(py, "Malformed arguments to url tag"))?;
        // `(A)`/`(?B)` alternation can't leave both empty: group 2 always
        // participates.
        let value = caps
            .get(2)
            .map_or("", |m| m.as_str())
            .to_string();
        match caps.get(1) {
            Some(n) => kwargs.push((n.as_str().to_string(), expr(compile(p, py, &value)?))),
            None => args.push(expr(compile(p, py, &value)?)),
        }
    }
    Ok(TagPayload::Url {
        view,
        args,
        kwargs,
        asvar,
        token: token.clone(),
    })
}

/// `{% widthratio this max width [as var] %}` — `widthratio`.
pub(crate) fn widthratio(py: Python<'_>, p: &mut Parser<'_>, token: &Token) -> Res {
    let bits = split(py, token)?;
    let mut asvar: Option<String> = None;
    let (this, max, width) = match bits.len() {
        4 => (bits[1].clone(), bits[2].clone(), bits[3].clone()),
        6 => {
            if bits[4] == "as" {
                asvar = Some(bits[5].clone());
                (bits[1].clone(), bits[2].clone(), bits[3].clone())
            } else {
                return Err(syntax_error(
                    py,
                    "Invalid syntax in widthratio tag. Expecting 'as' keyword",
                ));
            }
        }
        _ => return Err(syntax_error(py, "widthratio takes at least three arguments")),
    };
    Ok(TagPayload::WidthRatio {
        val: expr(compile(p, py, &this)?),
        max: expr(compile(p, py, &max)?),
        width: expr(compile(p, py, &width)?),
        asvar,
        token: token.clone(),
    })
}

/// `{% regroup list by attr as var %}` — `regroup`.
pub(crate) fn regroup(py: Python<'_>, p: &mut Parser<'_>, token: &Token) -> Res {
    let bits = split(py, token)?;
    if bits.len() != 6 {
        return Err(syntax_error(py, "'regroup' tag takes five arguments"));
    }
    let target = expr(compile(p, py, &bits[1])?);
    if bits[2] != "by" {
        return Err(syntax_error(
            py,
            "second argument to 'regroup' tag must be 'by'",
        ));
    }
    if bits[4] != "as" {
        return Err(syntax_error(
            py,
            "next-to-last argument to 'regroup' tag must be 'as'",
        ));
    }
    let var_name = bits[5].clone();
    // Stock evaluates `var_name.attr` in the per-item context.
    let expr_c = compile(p, py, &format!("{}.{}", bits[5], bits[3]))?;
    Ok(TagPayload::Regroup {
        target,
        expr: expr(expr_c),
        var_name,
        token: token.clone(),
    })
}

/// `{% csrf_token %}` — `csrf_token`.
#[allow(clippy::unnecessary_wraps)] // uniform handler signature
pub(crate) fn csrf(_py: Python<'_>, _p: &mut Parser<'_>, token: &Token) -> Res {
    Ok(TagPayload::Csrf {
        token: token.clone(),
    })
}

/// `{% debug %}` — `debug`.
#[allow(clippy::unnecessary_wraps)] // uniform handler signature
pub(crate) fn debug(_py: Python<'_>, _p: &mut Parser<'_>, token: &Token) -> Res {
    Ok(TagPayload::Debug {
        token: token.clone(),
    })
}


/// `true` iff `func` is the stock compile function `module.name`.
pub(crate) fn is_stock_func(
    py: Python<'_>,
    cell: &'static pyo3::sync::PyOnceLock<Option<Py<PyAny>>>,
    module: &str,
    name: &str,
    func: &Bound<'_, PyAny>,
) -> bool {
    let stock = cell.get_or_init(py, || {
        py.import(module)
            .and_then(|m| m.getattr(name))
            .ok()
            .map(Bound::unbind)
    });
    stock.as_ref().is_some_and(|f| func.is(f.bind(py)))
}

/// `{% static path [as varname] %}` when `func` is the stock `do_static`
/// (`StaticNode.handle_token`); `None` (compile bridged, which raises the
/// stock errors) for a missing path, an `as` without a name or a path that
/// does not compile.
pub(crate) fn static_tag(
    py: Python<'_>,
    p: &Parser<'_>,
    func: &Bound<'_, PyAny>,
    token: &Token,
) -> Option<TagPayload> {
    static DO_STATIC: pyo3::sync::PyOnceLock<Option<Py<PyAny>>> = pyo3::sync::PyOnceLock::new();
    if !is_stock_func(py, &DO_STATIC, "django.templatetags.static", "do_static", func) {
        return None;
    }
    let SplitContents::Ok(bits) = split_contents(&token.contents) else {
        return None;
    };
    if bits.len() < 2 {
        return None;
    }
    let path = p.compile_filter(py, &bits[1]).ok()?;
    // `if len(bits) >= 2 and bits[-2] == "as": varname = bits[3]`.
    let varname = if bits[bits.len() - 2] == "as" {
        Some(bits.get(3)?.clone())
    } else {
        None
    };
    Some(TagPayload::Static {
        path: expr(path),
        varname,
        token: token.clone(),
    })
}
