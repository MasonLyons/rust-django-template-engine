//! Port of `django.template.smartif` (identical on 5.2/6.1) plus the
//! `TemplateIfParser`/`TemplateLiteral` layer from `defaulttags`.
//!
//! The tokenizer merges `is`+`not` and `not`+`in` into single tokens, then a
//! top-down operator-precedence parse builds a tree of `IfExpr` nodes. Leaves
//! are `CompiledExpr`s resolved with `ignore_failures=True`; infix/prefix
//! evaluation swallows ordinary exceptions and yields `False`, exactly like
//! smartif's `try/except Exception`.

use pyo3::prelude::*;
use pyo3::pyclass::CompareOp;
use crate::parser::CompiledExpr;
use crate::render::context::Rctx;
use crate::render::value::filter_expression_resolve_ignoring;
use crate::varparse::FxError;

/// smartif `OPERATORS`, keyed by the merged token id.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IfOp {
    Or,
    And,
    Not,
    In,
    NotIn,
    Is,
    IsNot,
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

impl IfOp {
    /// Left binding power (smartif `infix`/`prefix` bp).
    fn lbp(self) -> u8 {
        match self {
            IfOp::Or => 6,
            IfOp::And => 7,
            IfOp::Not => 8,
            IfOp::In | IfOp::NotIn => 9,
            IfOp::Is
            | IfOp::IsNot
            | IfOp::Eq
            | IfOp::Ne
            | IfOp::Gt
            | IfOp::Ge
            | IfOp::Lt
            | IfOp::Le => 10,
        }
    }

    /// smartif `id` (used in the three TDPA error messages).
    fn id(self) -> &'static str {
        match self {
            IfOp::Or => "or",
            IfOp::And => "and",
            IfOp::Not => "not",
            IfOp::In => "in",
            IfOp::NotIn => "not in",
            IfOp::Is => "is",
            IfOp::IsNot => "is not",
            IfOp::Eq => "==",
            IfOp::Ne => "!=",
            IfOp::Gt => ">",
            IfOp::Ge => ">=",
            IfOp::Lt => "<",
            IfOp::Le => "<=",
        }
    }

    fn parse_id(s: &str) -> Option<Self> {
        Some(match s {
            "or" => IfOp::Or,
            "and" => IfOp::And,
            "not" => IfOp::Not,
            "in" => IfOp::In,
            "not in" => IfOp::NotIn,
            "is" => IfOp::Is,
            "is not" => IfOp::IsNot,
            "==" => IfOp::Eq,
            "!=" => IfOp::Ne,
            ">" => IfOp::Gt,
            ">=" => IfOp::Ge,
            "<" => IfOp::Lt,
            "<=" => IfOp::Le,
            _ => return None,
        })
    }
}

/// A compiled `if` condition: smartif's tree, leaves bound to `CompiledExpr`.
#[derive(Clone)]
pub enum IfExpr {
    /// smartif `TemplateLiteral`: resolve with `ignore_failures=True`.
    Leaf(std::sync::Arc<CompiledExpr>),
    Or(Box<IfExpr>, Box<IfExpr>),
    And(Box<IfExpr>, Box<IfExpr>),
    Not(Box<IfExpr>),
    In(Box<IfExpr>, Box<IfExpr>),
    NotIn(Box<IfExpr>, Box<IfExpr>),
    Is(Box<IfExpr>, Box<IfExpr>),
    IsNot(Box<IfExpr>, Box<IfExpr>),
    Eq(Box<IfExpr>, Box<IfExpr>),
    Ne(Box<IfExpr>, Box<IfExpr>),
    Gt(Box<IfExpr>, Box<IfExpr>),
    Ge(Box<IfExpr>, Box<IfExpr>),
    Lt(Box<IfExpr>, Box<IfExpr>),
    Le(Box<IfExpr>, Box<IfExpr>),
}

impl IfExpr {
    /// smartif `eval`. Infix/prefix operators wrap the whole computation in
    /// `try/except Exception -> False`; a leaf uses `ignore_failures=True`
    /// so it cannot raise `VariableDoesNotExist`.
    pub fn eval<'py>(&self, rctx: &mut Rctx<'py>) -> PyResult<Bound<'py, PyAny>> {
        match self {
            IfExpr::Leaf(c) => filter_expression_resolve_ignoring(rctx, &c.fe, &c.filters, true),
            IfExpr::Or(a, b) => or_and(rctx, a, b, true),
            IfExpr::And(a, b) => or_and(rctx, a, b, false),
            IfExpr::Not(a) => guarded(rctx, |rc| {
                let v = a.eval(rc)?;
                Ok(pybool(rc.py(), !v.is_truthy()?))
            }),
            IfExpr::In(a, b) => guarded(rctx, |rc| {
                let l = a.eval(rc)?;
                let r = b.eval(rc)?;
                Ok(pybool(rc.py(), r.contains(&l)?))
            }),
            IfExpr::NotIn(a, b) => guarded(rctx, |rc| {
                let l = a.eval(rc)?;
                let r = b.eval(rc)?;
                Ok(pybool(rc.py(), !r.contains(&l)?))
            }),
            IfExpr::Is(a, b) => guarded(rctx, |rc| {
                let l = a.eval(rc)?;
                let r = b.eval(rc)?;
                Ok(pybool(rc.py(), l.is(&r)))
            }),
            IfExpr::IsNot(a, b) => guarded(rctx, |rc| {
                let l = a.eval(rc)?;
                let r = b.eval(rc)?;
                Ok(pybool(rc.py(), !l.is(&r)))
            }),
            IfExpr::Eq(a, b) => rich(rctx, a, b, CompareOp::Eq),
            IfExpr::Ne(a, b) => rich(rctx, a, b, CompareOp::Ne),
            IfExpr::Gt(a, b) => rich(rctx, a, b, CompareOp::Gt),
            IfExpr::Ge(a, b) => rich(rctx, a, b, CompareOp::Ge),
            IfExpr::Lt(a, b) => rich(rctx, a, b, CompareOp::Lt),
            IfExpr::Le(a, b) => rich(rctx, a, b, CompareOp::Le),
        }
    }
}
fn guarded<'py, F>(rctx: &mut Rctx<'py>, f: F) -> PyResult<Bound<'py, PyAny>>
where
    F: FnOnce(&mut Rctx<'py>) -> PyResult<Bound<'py, PyAny>>,
{
    match f(rctx) {
        Ok(v) => Ok(v),
        Err(e) if e.is_instance_of::<pyo3::exceptions::PyException>(rctx.py()) => {
            Ok(pybool(rctx.py(), false))
        }
        Err(e) => Err(e),
    }
}

/// `Bound` bool (`PyBool::new` hands back a `Borrowed`).
fn pybool(py: Python<'_>, b: bool) -> Bound<'_, PyAny> {
    pyo3::types::PyBool::new(py, b).to_owned().into_any()
}

#[allow(clippy::many_single_char_names)] // a/b/l/r mirror the smartif operands
fn rich<'py>(
    rctx: &mut Rctx<'py>,
    a: &IfExpr,
    b: &IfExpr,
    op: pyo3::pyclass::CompareOp,
) -> PyResult<Bound<'py, PyAny>> {
    guarded(rctx, |rc| {
        let l = a.eval(rc)?;
        let r = b.eval(rc)?;
        let v = l.rich_compare(&r, op)?;
        // smartif lambdas return the comparison value (`x == y`), which is
        // the raw rich-compare result.
        Ok(v.into_any())
    })
}

/// `or`/`and` short-circuit returning the operand value (Python semantics).
fn or_and<'py>(
    rctx: &mut Rctx<'py>,
    a: &IfExpr,
    b: &IfExpr,
    is_or: bool,
) -> PyResult<Bound<'py, PyAny>> {
    guarded(rctx, |rc| {
        let x = a.eval(rc)?;
        let keep = if is_or { x.is_truthy()? } else { !x.is_truthy()? };
        if keep {
            Ok(x)
        } else {
            b.eval(rc)
        }
    })
}

// ---- TDPA parser (smartif.IfParser) -------------------------------------

/// `IfParser` over `create_var` = `compile_filter`, as `TemplateIfParser`.
///
/// # Errors
/// `FxError::Syntax` with the verbatim smartif message on a malformed
/// expression.
pub fn parse_condition(
    bits: &[String],
    mut compile: impl FnMut(&str) -> Result<CompiledExpr, FxError>,
) -> Result<IfExpr, FxError> {
    let toks = tokenize(bits, &mut compile)?;
    let mut p = P {
        toks,
        pos: 0,
    };
    let e = p.expression(0)?;
    if p.pos < p.toks.len() {
        return Err(FxError::Syntax(format!(
            "Unused '{}' at end of if expression.",
            p.display(p.pos)
        )));
    }
    Ok(e)
}

enum Tok {
    Op(IfOp),
    Leaf {
        text: String,
        expr: std::sync::Arc<CompiledExpr>,
    },
}

struct P {
    toks: Vec<Tok>,
    pos: usize,
}

impl P {
    /// The `display()` for error messages (`TemplateLiteral` returns text;
    /// an operator returns its id). There is no `EndToken` here: exhaustion is
    /// signalled by `pos == toks.len()`.
    fn display(&self, i: usize) -> String {
        match &self.toks[i] {
            Tok::Op(op) => op.id().to_string(),
            Tok::Leaf { text, .. } => text.clone(),
        }
    }

    /// `IfParser.expression(rbp)`. `EndToken.nud` raises "Unexpected end of
    /// expression in if tag." — here: `pos >= len` before a nud.
    fn expression(&mut self, rbp: u8) -> Result<IfExpr, FxError> {
        let mut left = self.nud()?;
        loop {
            let i = self.pos;
            let lbp = match self.toks.get(i) {
                Some(Tok::Op(op)) => op.lbp(),
                _ => 0,
            };
            if rbp >= lbp {
                break;
            }
            let Tok::Op(op) = self.toks[i] else {
                unreachable!("lbp>0 is an operator");
            };
            self.pos += 1;
            left = self.led(op, left)?;
        }
        Ok(left)
    }

    fn nud(&mut self) -> Result<IfExpr, FxError> {
        let i = self.pos;
        if i >= self.toks.len() {
            return Err(FxError::Syntax(
                "Unexpected end of expression in if tag.".to_string(),
            ));
        }
        self.pos += 1;
        match &self.toks[i] {
            Tok::Leaf { expr, .. } => Ok(IfExpr::Leaf(expr.clone())),
            Tok::Op(IfOp::Not) => Ok(IfExpr::Not(Box::new(self.expression(IfOp::Not.lbp())?))),
            Tok::Op(op) => Err(FxError::Syntax(format!(
                "Not expecting '{}' in this position in if tag.",
                op.id()
            ))),
        }
    }

    fn led(&mut self, op: IfOp, left: IfExpr) -> Result<IfExpr, FxError> {
        if op == IfOp::Not {
            // `prefix(8, ...)` — `not` is never infix.
            return Err(FxError::Syntax(
                "Not expecting 'not' as infix operator in if tag.".to_string(),
            ));
        }
        let right = Box::new(self.expression(op.lbp())?);
        let l = Box::new(left);
        Ok(match op {
            IfOp::Or => IfExpr::Or(l, right),
            IfOp::Not => unreachable!("rejected above"),
            IfOp::And => IfExpr::And(l, right),
            IfOp::In => IfExpr::In(l, right),
            IfOp::NotIn => IfExpr::NotIn(l, right),
            IfOp::Is => IfExpr::Is(l, right),
            IfOp::IsNot => IfExpr::IsNot(l, right),
            IfOp::Eq => IfExpr::Eq(l, right),
            IfOp::Ne => IfExpr::Ne(l, right),
            IfOp::Gt => IfExpr::Gt(l, right),
            IfOp::Ge => IfExpr::Ge(l, right),
            IfOp::Lt => IfExpr::Lt(l, right),
            IfOp::Le => IfExpr::Le(l, right),
        })
    }
}

/// Merge `is`+`not` / `not`+`in` and translate each token via
/// `translate_token` (operator or `create_var`).
fn tokenize(
    bits: &[String],
    compile: &mut impl FnMut(&str) -> Result<CompiledExpr, FxError>,
) -> Result<Vec<Tok>, FxError> {
    let n = bits.len();
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let mut token = bits[i].as_str();
        if token == "is" && i + 1 < n && bits[i + 1] == "not" {
            token = "is not";
            i += 1;
        } else if token == "not" && i + 1 < n && bits[i + 1] == "in" {
            token = "not in";
            i += 1;
        }
        match IfOp::parse_id(token) {
            Some(op) => out.push(Tok::Op(op)),
            None => out.push(Tok::Leaf {
                text: token.to_string(),
                expr: std::sync::Arc::new(compile(token)?),
            }),
        }
        i += 1;
    }
    Ok(out)
}
