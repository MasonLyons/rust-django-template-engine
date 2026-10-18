//! i18n `{% blocktranslate %}` / `{% blocktrans %}`: native port of
//! `django.templatetags.i18n.do_block_translate` and `BlockTranslateNode`,
//! taken only when the registered compile function is the stock one.
//!
//! Parsing is a dry run over the upcoming raw tokens first: any form that
//! would make the stock parser raise (or that it accepts by accident, such
//! as running out of tokens) compiles bridged instead, so every error comes
//! from the stock code. The message ids are fixed at parse time; rendering
//! resolves the `with`/`count`/`context` expressions, looks the messages up
//! through `filters::Translator` and `%`-formats with Python's own `%`.

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyDict, PyString};

use super::{annotate, expr, expr_res_ign, with_scoped, Expr, TagPayload};
use crate::error::syntax_error;
use crate::lexer::{split_contents, SplitContents, Token, TokenKind};
use crate::parser::{token_kwargs, Parser};
use crate::render::context::Rctx;
use crate::render::value;

use super::simple::py_ws;

/// Parsed `BlockTranslateNode`.
pub struct BlockTranslate {
    /// `with` keyword arguments (`extra_context`), in order.
    pub extra: Vec<(String, Expr)>,
    /// Singular message id (`render_token_list(self.singular)`, trimmed).
    pub singular: String,
    /// Variable names of the singular message.
    pub vars: Vec<String>,
    /// `count`: present only when the stock node would pluralize
    /// (`self.plural and self.countervar and self.counter`).
    pub plural: Option<Plural>,
    /// `context` expression (`message_context`).
    pub context: Option<Expr>,
    pub asvar: Option<String>,
    /// `bits[0]` (`blocktranslate` or `blocktrans`).
    pub tag_name: String,
    pub token: Token,
}

pub struct Plural {
    pub countervar: String,
    pub counter: Expr,
    pub msg: String,
    pub vars: Vec<String>,
}

impl BlockTranslate {
    /// Every expression the node resolves.
    pub(crate) fn exprs(&self) -> impl Iterator<Item = &Expr> {
        self.extra
            .iter()
            .map(|(_, e)| e)
            .chain(self.plural.iter().map(|p| &p.counter))
            .chain(self.context.iter())
    }

    /// Whether rendering reads the context key `key` verbatim (message
    /// variables are looked up with `key in context` / `context[key]`).
    pub(crate) fn reads_key(&self, key: &str) -> bool {
        self.vars
            .iter()
            .chain(self.plural.iter().flat_map(|p| p.vars.iter()))
            .any(|v| v == key)
    }
}

fn py_strip(s: &str) -> &str {
    s.trim_matches(py_ws)
}

/// `render_token_list`: TEXT `%` doubled, VAR → `%(name)s`; `trimmed`
/// applies the real `translation.trim_whitespace`.
fn token_list(py: Python<'_>, tokens: &[&Token], trimmed: bool) -> PyResult<(String, Vec<String>)> {
    let mut msg = String::new();
    let mut vars = Vec::new();
    for t in tokens {
        match t.kind {
            TokenKind::Text => msg.push_str(&t.contents.replace('%', "%%")),
            TokenKind::Var => {
                msg.push_str("%(");
                msg.push_str(&t.contents);
                msg.push_str(")s");
                vars.push(t.contents.clone());
            }
            _ => {}
        }
    }
    if trimmed {
        msg = py
            .import("django.utils.translation")?
            .getattr("trim_whitespace")?
            .call1((msg,))?
            .extract()?;
    }
    Ok((msg, vars))
}

/// Native `do_block_translate` for the stock compile function; `None` →
/// compile bridged (unsupported or erroneous forms).
#[allow(clippy::too_many_lines)] // one linear port of the stock function
pub(crate) fn parse(
    py: Python<'_>,
    p: &mut Parser<'_>,
    func: &Bound<'_, PyAny>,
    token: &Token,
) -> Option<TagPayload> {
    static DO_BLOCK_TRANSLATE: PyOnceLock<Option<Py<PyAny>>> = PyOnceLock::new();
    let (module, name) = ("django.templatetags.i18n", "do_block_translate");
    if !super::parse::is_stock_func(py, &DO_BLOCK_TRANSLATE, module, name, func) {
        return None;
    }
    let SplitContents::Ok(bits) = split_contents(&token.contents) else {
        return None;
    };
    let mut remaining: Vec<String> = bits[1..].to_vec();
    let mut seen: Vec<String> = Vec::new();
    let mut extra = Vec::new();
    let mut count = None;
    let mut context = None;
    let mut trimmed = false;
    let mut asvar = None;
    while !remaining.is_empty() {
        let option = remaining.remove(0);
        if seen.contains(&option) {
            return None;
        }
        match option.as_str() {
            "with" => {
                let v = token_kwargs(py, p, &mut remaining, true).ok()?;
                if v.is_empty() {
                    return None;
                }
                extra = v;
            }
            "count" => {
                let mut v = token_kwargs(py, p, &mut remaining, true).ok()?;
                if v.len() != 1 {
                    return None;
                }
                count = v.pop();
            }
            "context" => {
                if remaining.is_empty() {
                    return None;
                }
                let v = remaining.remove(0);
                context = Some(p.compile_filter(py, &v).ok()?);
            }
            "trimmed" => trimmed = true,
            "asvar" => {
                if remaining.is_empty() {
                    return None;
                }
                asvar = Some(remaining.remove(0));
            }
            _ => return None,
        }
        seen.push(option);
    }
    let tag_name = bits[0].clone();
    let end_tag = format!("end{tag_name}");
    // Dry run over the raw tokens: (singular, [plural]) then the end tag.
    let up: Vec<&Token> = p.upcoming().collect();
    let mut i = 0;
    let body = |i: &mut usize| -> Option<(Vec<&Token>, &Token)> {
        let start = *i;
        loop {
            let t = *up.get(*i)?;
            *i += 1;
            if matches!(t.kind, TokenKind::Var | TokenKind::Text) {
                continue;
            }
            // Comment tokens compare their text in stock; stay bridged.
            if t.kind != TokenKind::Block {
                return None;
            }
            return Some((up[start..*i - 1].to_vec(), t));
        }
    };
    let (singular, mut term) = body(&mut i)?;
    let mut plural_tokens = Vec::new();
    if count.is_some() {
        if py_strip(&term.contents) != "plural" {
            return None;
        }
        let (pl, t) = body(&mut i)?;
        plural_tokens = pl;
        term = t;
    }
    if py_strip(&term.contents) != end_tag {
        return None;
    }
    let (singular, vars) = token_list(py, &singular, trimmed).ok()?;
    let plural = match count {
        Some((countervar, counter)) if !plural_tokens.is_empty() => {
            let (msg, pvars) = token_list(py, &plural_tokens, trimmed).ok()?;
            Some(Plural {
                countervar,
                counter: expr(counter),
                msg,
                vars: pvars,
            })
        }
        _ => None,
    };
    for _ in 0..i {
        p.next_token_pub();
    }
    Some(TagPayload::BlockTranslate(Box::new(BlockTranslate {
        extra: extra.into_iter().map(|(k, c)| (k, expr(c))).collect(),
        singular,
        vars,
        plural,
        context: context.map(expr),
        asvar,
        tag_name,
        token: token.clone(),
    })))
}

fn decimal_type(py: Python<'_>) -> PyResult<&Bound<'_, PyAny>> {
    static D: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    Ok(D.get_or_try_init(py, || -> PyResult<Py<PyAny>> {
        Ok(py.import("decimal")?.getattr("Decimal")?.unbind())
    })?
    .bind(py))
}

fn repr(py: Python<'_>, s: &str) -> PyResult<String> {
    Ok(PyString::new(py, s).repr()?.to_string())
}

/// `BlockTranslateNode.render(context, nested)`.
#[allow(clippy::too_many_lines)] // one linear port of the stock method
fn render<'py>(rctx: &mut Rctx<'py>, bt: &BlockTranslate, nested: bool) -> PyResult<String> {
    let py = rctx.py();
    let tr = crate::filters::Translator::new(py);
    let message_context = match &bt.context {
        Some(c) => Some(expr_res_ign(rctx, c, false)?),
        None => None,
    };
    let message_context = match message_context {
        Some(mc) if mc.is_truthy()? => Some(mc.str()?.to_string()),
        _ => None,
    };
    let extra = PyDict::new(py);
    for (k, e) in &bt.extra {
        extra.set_item(k, expr_res_ign(rctx, e, false)?)?;
    }
    let mut result: Option<Bound<'py, PyAny>> = None;
    let data = PyDict::new(py);
    with_scoped(rctx, &Some(extra.into_any()), |rctx| {
        let mut vars: Vec<&String> = bt.vars.iter().collect();
        let translated = if let Some(pl) = &bt.plural {
            let count = expr_res_ign(rctx, &pl.counter, false)?;
            let numeric = count.is_instance(decimal_type(py)?)?
                || count.is_instance_of::<pyo3::types::PyFloat>()
                || count.is_instance_of::<pyo3::types::PyInt>();
            if !numeric {
                return Err(syntax_error(
                    py,
                    &format!(
                        "{} argument to {} tag must be a number.",
                        repr(py, &pl.countervar)?,
                        repr(py, &bt.tag_name)?
                    ),
                ));
            }
            rctx.set_item(&pl.countervar, &count)?;
            vars.extend(pl.vars.iter());
            match &message_context {
                Some(c) => tr.npgettext(c, &bt.singular, &pl.msg, &count)?,
                None => tr.ngettext(&bt.singular, &pl.msg, &count)?,
            }
        } else {
            match &message_context {
                Some(c) => tr.pgettext(c, &bt.singular)?,
                None => tr.gettext(&bt.singular)?,
            }
        };
        let default_value = rctx.engine.getattr(pyo3::intern!(py, "string_if_invalid"))?;
        for v in vars {
            let val = if rctx.context_has(v)? {
                rctx.get_item(v)?.unwrap_or_else(|| py.None().into_bound(py))
            } else if default_value.contains("%s")? {
                default_value.rem(v)?
            } else {
                default_value.clone()
            };
            let s = value::render_value_in_context(rctx, &val)?;
            data.set_item(v, s)?;
        }
        result = Some(translated);
        Ok(())
    })?;
    let result = result.unwrap_or_else(|| PyString::new(py, "").into_any());
    let formatted = match result.rem(&data) {
        Ok(r) => r.str()?.to_string(),
        Err(e)
            if e.is_instance_of::<pyo3::exceptions::PyKeyError>(py)
                || e.is_instance_of::<pyo3::exceptions::PyValueError>(py) =>
        {
            if nested {
                return Err(syntax_error(
                    py,
                    &format!(
                        "{} is unable to format string returned by gettext: {} using {}",
                        repr(py, &bt.tag_name)?,
                        result.repr()?,
                        data.repr()?
                    ),
                ));
            }
            let ov = py
                .import("django.utils.translation")?
                .getattr("override")?
                .call1((py.None(),))?;
            ov.call_method0("__enter__")?;
            let r = render(rctx, bt, true);
            match &r {
                Ok(_) => {
                    ov.call_method1("__exit__", (py.None(), py.None(), py.None()))?;
                }
                Err(e) => {
                    let exc = e.value(py);
                    ov.call_method1("__exit__", (exc.get_type(), exc, exc.getattr("__traceback__")?))?;
                }
            }
            r?
        }
        Err(e) => return Err(e),
    };
    if let Some(name) = &bt.asvar {
        let v = value::mark_safe_bound(&PyString::new(py, &formatted).into_any())?;
        rctx.set_item(name, &v)?;
        return Ok(String::new());
    }
    Ok(formatted)
}

/// `BlockTranslateNode.render`.
pub(crate) fn block_translate(rctx: &mut Rctx<'_>, p: &TagPayload, out: &mut String) -> PyResult<()> {
    let TagPayload::BlockTranslate(bt) = p else {
        return Ok(());
    };
    match render(rctx, bt, false) {
        Ok(s) => {
            out.push_str(&s);
            Ok(())
        }
        Err(e) => {
            annotate(rctx, &e, &bt.token);
            Err(e)
        }
    }
}
