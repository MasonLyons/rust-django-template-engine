//! FilterExpression / Variable compilation — exact port of
//! `django.template.base.FilterExpression.__init__` + `Variable.__init__`,
//! including the `filter_re` finditer coverage rules ("Could not parse some
//! characters" / "Could not parse the remainder"). Branch differences
//! (5.2 `num` token vs 6.1 `+`/`-` in var chars) are handled per `PyVer`.

use num_bigint::BigInt;

use crate::lexer::unescape_string_literal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyVer {
    V52,
    V61,
}

impl PyVer {
    pub fn from_tuple(t: (u32, u32)) -> PyVer {
        if t.0 >= 6 {
            PyVer::V61
        } else {
            PyVer::V52
        }
    }
}

/// Compile-time resolved value (Django: `Variable(constant).resolve({})`).
#[derive(Debug, Clone, PartialEq)]
pub enum PyConst {
    Str(String),
    /// `_("…")` constant: translated at render time via gettext.
    Translate(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum VarPart {
    /// Quoted string / i18n constant (`is_var == false`).
    Const(PyConst),
    /// int literal token — still a `Variable`, so `is_var == true`.
    Int(BigInt),
    Float(f64),
    Lookup {
        var: String,
        lookups: Vec<String>,
        /// `_(x)` form: gettext the resolved value at render time.
        translate: bool,
        /// Interned Python `str` per lookup bit.
        keys: Keys,
    },
}

/// Interned Python `str` objects for a parsed name list (lookup bits, loop
/// variables), built once on first render: parsing itself runs without
/// Python in the Rust-only tests and benches. Interned keys hit CPython's
/// type attribute cache and dict pointer-equality fast paths, and skip a
/// `str` allocation per lookup. Equality, `Debug` and `Clone` ignore the
/// cache (a clone re-interns on its first use).
#[derive(Default)]
pub struct Keys(std::sync::OnceLock<Box<[pyo3::Py<pyo3::types::PyString>]>>);

impl Keys {
    /// One interned key per name, in order.
    pub fn get<'a>(&'a self, py: pyo3::Python<'_>, names: &[String]) -> &'a [pyo3::Py<pyo3::types::PyString>] {
        self.0.get_or_init(|| {
            names
                .iter()
                .map(|n| pyo3::types::PyString::intern(py, n).unbind())
                .collect()
        })
    }
}

impl Clone for Keys {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for Keys {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keys")
    }
}

/// A parsed `FilterExpression`.
#[derive(Debug, Clone)]
pub struct FilterExpression {
    pub token: String,
    pub var: VarPart,
    pub is_var: bool,
    pub filters: Vec<FilterSpec>,
}

#[derive(Debug, Clone)]
pub struct FilterSpec {
    pub name: String,
    pub args: Vec<FilterArg>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FilterArg {
    Const(PyConst),
    Int(BigInt),
    Float(f64),
    Lookup {
        var: String,
        lookups: Vec<String>,
        translate: bool,
        keys: Keys,
    },
}

/// Parse errors carrying exact Django messages.
#[derive(Debug, thiserror::Error)]
pub enum FxError {
    #[error("{0}")]
    Syntax(String),
    #[error("Invalid filter: '{0}'")]
    InvalidFilter(String),
}

/// Registry abstraction: parse-time filter existence + arity (Django's
/// `find_filter` + `args_check`).
pub trait FilterLookup {
    /// `(min_args, max_args)` INCLUDING the implicit input argument,
    /// matching `getfullargspec(unwrap(func))` semantics; `usize::MAX` for
    /// unbounded (varargs).
    fn filter_arity(&self, name: &str) -> Option<(usize, usize)>;
}

pub struct NoFilters;
impl FilterLookup for NoFilters {
    fn filter_arity(&self, _name: &str) -> Option<(usize, usize)> {
        None
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

enum Match {
    Const { end: usize },
    Var { end: usize },
    Part { name: String, raw_arg: Option<RawArg>, end: usize },
}

enum RawArg {
    Const(String),
    Var(String),
}

pub fn compile_filter(
    token: &str,
    ver: PyVer,
    reg: &dyn FilterLookup,
) -> Result<FilterExpression, FxError> {
    // Pass 1: reproduce `filter_re.finditer(token)` — pure matching.
    // Pass 2: Django's coverage checks + find_filter/args_check timing.
    let mut ms: Vec<(usize, Match)> = Vec::new();
    let mut p = 0usize;
    while p < token.len() {
        if p == 0 {
            if let Some((_, end)) = match_constant(token, 0) {
                ms.push((0, Match::Const { end }));
                p = end;
                continue;
            }
            if let Some((_, end)) = match_var(token, 0, ver) {
                ms.push((0, Match::Var { end }));
                p = end;
                continue;
            }
        }
        if let Some((spec, end)) = match_part(token, p, ver) {
            ms.push((p, Match::Part { name: spec.0, raw_arg: spec.1, end }));
            p = end;
            continue;
        }
        p = next_char_boundary(token, p);
    }

    let mut upto = 0usize;
    let mut var: Option<VarPart> = None;
    let mut filters: Vec<FilterSpec> = Vec::new();
    for (start, m) in ms {
        if upto != start {
            return Err(FxError::Syntax(format!(
                "Could not parse some characters: {}|{}|{}",
                &token[..upto],
                &token[upto..start],
                &token[start..]
            )));
        }
        match m {
            Match::Const { end } => {
                var = Some(compile_constant(&token[start..end])?);
                upto = end;
            }
            Match::Var { end } => {
                var = Some(compile_variable(&token[start..end], ver)?);
                upto = end;
            }
            Match::Part { name, raw_arg, end } => {
                if var.is_none() {
                    return Err(FxError::Syntax(format!(
                        "Could not find variable at start of {token}."
                    )));
                }
                let mut args: Vec<FilterArg> = Vec::new();
                match raw_arg {
                    Some(RawArg::Const(c)) => args.push(compile_constant(&c)?.into_arg()),
                    Some(RawArg::Var(v)) => args.push(compile_variable(&v, ver)?.into_arg()),
                    None => {}
                }
                let (min, max) = reg
                    .filter_arity(&name)
                    .ok_or_else(|| FxError::InvalidFilter(name.clone()))?;
                let provided = args.len() + 1;
                if provided < min || provided > max {
                    return Err(FxError::Syntax(format!(
                        "{name} requires {min} arguments, {provided} provided"
                    )));
                }
                filters.push(FilterSpec { name, args });
                upto = end;
            }
        }
    }

    if upto != token.len() {
        return Err(FxError::Syntax(format!(
            "Could not parse the remainder: '{}' from '{token}'",
            &token[upto..]
        )));
    }
    let var = match var {
        Some(v) => v,
        None => {
            return Err(FxError::Syntax(format!(
                "Could not find variable at start of {token}."
            )))
        }
    };
    let is_var = !matches!(var, VarPart::Const(_));
    Ok(FilterExpression { token: token.to_string(), var, is_var, filters })
}

fn next_char_boundary(s: &str, mut p: usize) -> usize {
    if p >= s.len() {
        return s.len();
    }
    p += 1;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}

// ---------------------------------------------------------------------------
// Regex-equivalent matchers
// ---------------------------------------------------------------------------

/// `constant_string` at `pos`: `_("dq")` | `_('sq')` | "dq" | 'sq' (in order).
fn match_constant(token: &str, pos: usize) -> Option<(String, usize)> {
    let b = token.as_bytes();
    let len = b.len();
    if pos >= len {
        return None;
    }
    if pos + 3 <= len && b[pos] == b'_' && b[pos + 1] == b'(' {
        let q = b[pos + 2];
        if q == b'"' || q == b'\'' {
            if let Some(qend) = scan_quoted(b, pos + 2) {
                if qend < len && b[qend] == b')' {
                    return Some((token[pos..=qend].to_string(), qend + 1));
                }
            }
        }
    }
    let q = b[pos];
    if q == b'"' || q == b'\'' {
        if let Some(end) = scan_quoted(b, pos) {
            return Some((token[pos..end].to_string(), end));
        }
    }
    None
}

/// `"[^"\\]*(?:\\.[^"\\]*)*"` starting at the quote; exclusive end byte.
fn scan_quoted(b: &[u8], start: usize) -> Option<usize> {
    let quote = b[start];
    let len = b.len();
    let mut j = start + 1;
    loop {
        if j >= len {
            return None;
        }
        match b[j] {
            b'\\' => j += 2,
            c if c == quote => return Some(j + 1),
            _ => j += 1,
        }
    }
}

/// var / var_arg token at `pos` per branch grammar.
fn match_var(token: &str, pos: usize, ver: PyVer) -> Option<(String, usize)> {
    if pos >= token.len() {
        return None;
    }
    match ver {
        PyVer::V52 => {
            if let Some(end) = scan_var_chars(token, pos, false) {
                return Some((token[pos..end].to_string(), end));
            }
            scan_num_token(token, pos)
        }
        PyVer::V61 => scan_var_chars(token, pos, true)
            .map(|end| (token[pos..end].to_string(), end)),
    }
}

/// Python `\w` (Unicode alphanumeric + `_`) plus `.` (and `+`/`-` on 6.1).
fn scan_var_chars(token: &str, pos: usize, plus_minus: bool) -> Option<usize> {
    let rest = &token[pos..];
    let mut end = pos;
    for (off, c) in rest.char_indices() {
        let ok = c.is_alphanumeric()
            || c == '_'
            || c == '.'
            || (plus_minus && (c == '+' || c == '-'));
        if !ok {
            let _ = off;
            break;
        }
        end = pos + off + c.len_utf8();
    }
    if end > pos {
        Some(end)
    } else {
        None
    }
}

/// 5.2 `num`: `[-+.]?\d[\d.e]*` (greedy).
fn scan_num_token(token: &str, pos: usize) -> Option<(String, usize)> {
    let b = token.as_bytes();
    let len = b.len();
    let mut i = pos;
    if i < len && matches!(b[i], b'-' | b'+' | b'.') {
        i += 1;
    }
    if i >= len || !b[i].is_ascii_digit() {
        return None;
    }
    i += 1;
    while i < len && matches!(b[i], b'0'..=b'9' | b'.' | b'e' | b'E') {
        i += 1;
    }
    Some((token[pos..i].to_string(), i))
}

/// Filter part at exactly `pos`: `\s*\|\s*\w+(:arg)?`.
/// `:` followed by a non-arg does NOT consume the colon (the optional group
/// matches empty in Python), so the match ends after the name.
fn match_part(token: &str, pos: usize, ver: PyVer) -> Option<((String, Option<RawArg>), usize)> {
    let len = token.len();
    let mut p = scan_ws(token, pos);
    if p >= len || !token[p..].starts_with('|') {
        return None;
    }
    p += 1;
    p = scan_ws(token, p);
    let name_start = p;
    let name_end = scan_var_chars(token, name_start, false).unwrap_or(name_start);
    if name_end == name_start {
        return None;
    }
    let name = &token[name_start..name_end];
    let mut raw_arg: Option<RawArg> = None;
    let mut end = name_end;
    if name_end < len && token.as_bytes()[name_end] == b':' {
        let after = name_end + 1;
        if let Some((c, cend)) = match_constant(token, after) {
            raw_arg = Some(RawArg::Const(c));
            end = cend;
        } else if let Some((v, vend)) = match_var(token, after, ver) {
            raw_arg = Some(RawArg::Var(v));
            end = vend;
        }
    }
    Some(((name.to_string(), raw_arg), end))
}

fn scan_ws(token: &str, mut p: usize) -> usize {
    let len = token.len();
    while p < len && token.is_char_boundary(p) {
        let Some(c) = token[p..].chars().next() else { break };
        if c.is_whitespace() {
            p += c.len_utf8();
        } else {
            break;
        }
    }
    p
}

// ---------------------------------------------------------------------------
// Variable / constant compilation (Variable.__init__ semantics)
// ---------------------------------------------------------------------------

trait IntoArg {
    fn into_arg(self) -> FilterArg;
}

impl IntoArg for VarPart {
    fn into_arg(self) -> FilterArg {
        match self {
            VarPart::Const(c) => FilterArg::Const(c),
            VarPart::Int(i) => FilterArg::Int(i),
            VarPart::Float(f) => FilterArg::Float(f),
            VarPart::Lookup { var, lookups, translate, keys } => {
                FilterArg::Lookup { var, lookups, translate, keys }
            }
        }
    }
}

/// `Variable(constant).resolve({})`.
fn compile_constant(c: &str) -> Result<VarPart, FxError> {
    if c.starts_with("_(") && c.ends_with(')') {
        let inner = &c[2..c.len() - 1];
        if let Ok(s) = unescape_string_literal(inner) {
            return Ok(VarPart::Const(PyConst::Translate(s)));
        }
    }
    match unescape_string_literal(c) {
        Ok(s) => Ok(VarPart::Const(PyConst::Str(s))),
        Err(()) => Err(FxError::Syntax(format!("Could not parse constant: '{c}'"))),
    }
}

/// `Variable(var)` semantics → VarPart.
pub fn compile_variable(var: &str, ver: PyVer) -> Result<VarPart, FxError> {
    if let Some(num) = try_numeric(var) {
        return Ok(num);
    }
    let mut translated = false;
    let mut v = var.to_string();
    if v.starts_with("_(") && v.ends_with(')') {
        translated = true;
        v = v[2..v.len() - 1].to_string();
    }
    if let Ok(s) = unescape_string_literal(&v) {
        if translated {
            return Ok(VarPart::Const(PyConst::Translate(s)));
        }
        return Ok(VarPart::Const(PyConst::Str(s)));
    }
    if v.contains("._") || v.starts_with('_') {
        return Err(FxError::Syntax(format!(
            "Variables and attributes may not begin with underscores: '{v}'"
        )));
    }
    if ver == PyVer::V61 {
        for c in ['+', '-'] {
            if v.contains(c) {
                return Err(FxError::Syntax(format!(
                    "Invalid character ('{c}') in variable name: '{v}'"
                )));
            }
        }
    }
    let lookups: Vec<String> = v.split('.').map(str::to_string).collect();
    Ok(VarPart::Lookup { var: var.to_string(), lookups, translate: translated, keys: Keys::default() })
}

/// Python numeric parse per Variable.__init__: float if '.' or e/E present
/// (reject trailing '.'), else int; None when both fail.
fn try_numeric(var: &str) -> Option<VarPart> {
    let lower = var.to_ascii_lowercase();
    if lower.contains('.') || lower.contains('e') {
        if var.ends_with('.') {
            return None; // Django raises ValueError for "2."
        }
        return crate::lexer::py_float_literal(var).map(VarPart::Float);
    }
    crate::lexer::py_int_literal(var).map(VarPart::Int)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct OneFilter;
    impl FilterLookup for OneFilter {
        fn filter_arity(&self, name: &str) -> Option<(usize, usize)> {
            match name {
                "upper" => Some((1, 1)),
                "default" => Some((2, 2)),
                "b" => Some((1, 1)),
                _ => None,
            }
        }
    }

    fn cf(tok: &str) -> Result<FilterExpression, FxError> {
        compile_filter(tok, PyVer::V52, &OneFilter)
    }

    fn msg(r: Result<FilterExpression, FxError>) -> String {
        match r {
            Ok(_) => "OK".to_string(),
            Err(FxError::Syntax(s)) => s,
            Err(FxError::InvalidFilter(s)) => format!("Invalid filter: '{s}'"),
        }
    }

    #[test]
    fn simple_var() {
        let fe = cf("a.b").unwrap();
        assert!(fe.is_var);
        assert!(fe.filters.is_empty());
        match &fe.var {
            VarPart::Lookup { lookups, .. } => {
                assert_eq!(lookups, &["a".to_string(), "b".to_string()])
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn int_var_is_var() {
        let fe = cf("42").unwrap();
        assert!(fe.is_var);
        assert!(matches!(fe.var, VarPart::Int(_)));
        let fe = cf("-1").unwrap();
        assert!(matches!(fe.var, VarPart::Int(_)));
    }

    #[test]
    fn float_var() {
        let fe = cf("1.5").unwrap();
        assert!(matches!(fe.var, VarPart::Float(_)));
        let fe = cf("2.").unwrap();
        match &fe.var {
            VarPart::Lookup { lookups, .. } => {
                assert_eq!(lookups, &["2".to_string(), String::new()])
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn constant_not_var() {
        let fe = cf("'hello'").unwrap();
        assert!(!fe.is_var);
        match &fe.var {
            VarPart::Const(PyConst::Str(s)) => assert_eq!(s, "hello"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn i18n_constant() {
        let fe = cf("_('hi')").unwrap();
        assert!(!fe.is_var);
        assert!(matches!(&fe.var, VarPart::Const(PyConst::Translate(s)) if s == "hi"));
    }

    #[test]
    fn filters_chain() {
        let fe = cf("x|upper|default:'a b'").unwrap();
        assert_eq!(fe.filters.len(), 2);
        assert_eq!(fe.filters[1].name, "default");
        assert_eq!(fe.filters[1].args.len(), 1);
    }

    #[test]
    fn filter_arg_colon_in_quoted() {
        let fe = cf("x|default:\"a:b c\"").unwrap();
        match &fe.filters[0].args[0] {
            FilterArg::Const(PyConst::Str(s)) => assert_eq!(s, "a:b c"),
            other => panic!("{other:?}"),
        }
    }

    // Ground-truth expectations below were generated by running Django's own
    // filter_re.finditer + walk rules on both branches (verified in session).
    #[test]
    fn remainder_error() {
        assert_eq!(
            msg(cf("a|b c")),
            "Could not parse the remainder: ' c' from 'a|b c'"
        );
    }

    #[test]
    fn some_characters_error() {
        assert_eq!(
            msg(cf("x y|upper")),
            "Could not parse some characters: x| y||upper"
        );
    }

    #[test]
    fn invalid_filter() {
        assert_eq!(msg(cf("x|nope")), "Invalid filter: 'nope'");
    }

    #[test]
    fn arity_error() {
        assert_eq!(
            msg(cf("x|default")),
            "default requires 2 arguments, 1 provided"
        );
        assert_eq!(
            msg(cf("x|upper:'a'")),
            "upper requires 1 arguments, 2 provided"
        );
    }

    #[test]
    fn underscore_var_error() {
        assert_eq!(
            msg(cf("_x")),
            "Variables and attributes may not begin with underscores: '_x'"
        );
        assert!(msg(cf("a._b")).contains("may not begin with underscores"));
    }

    #[test]
    fn v61_plusminus() {
        let fe = compile_filter("+1", PyVer::V61, &OneFilter).unwrap();
        assert!(matches!(fe.var, VarPart::Int(_)));
        let e = compile_filter("a-b", PyVer::V61, &OneFilter).unwrap_err();
        assert_eq!(
            format!("{e}"),
            "Invalid character ('-') in variable name: 'a-b'"
        );
        // 5.2: var 'a' matches, remainder '-b' (verified against 5.2 filter_re)
        let e = compile_filter("a-b", PyVer::V52, &OneFilter).unwrap_err();
        assert!(format!("{e}").starts_with("Could not parse the remainder"));
    }

    #[test]
    fn filter_arg_lookup() {
        let fe = cf("x|default:y").unwrap();
        assert!(matches!(&fe.filters[0].args[0], FilterArg::Lookup { var, .. } if var == "y"));
    }

    #[test]
    fn leading_pipe() {
        assert_eq!(msg(cf("|x")), "Could not find variable at start of |x.");
    }

    #[test]
    fn num_token_52() {
        let fe = cf("1e").unwrap();
        match &fe.var {
            VarPart::Lookup { var, .. } => assert_eq!(var, "1e"),
            other => panic!("{other:?}"),
        }
        assert!(cf("2.e").is_ok());
    }
}
