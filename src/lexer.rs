//! Lexer: exact port of `django.template.base` tokenization
//! (`tag_re` split semantics + `Lexer.create_token` verbatim handling),
//! plus ports of `django.utils.text.smart_split`/`unescape_string_literal`.

use num_bigint::BigInt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Text,
    Var,
    Block,
    Comment,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub contents: String,
    pub lineno: u32,
    /// (start, end) byte offsets in the source; Some only in debug mode
    /// (mirrors `Lexer` vs `DebugLexer`).
    pub position: Option<(usize, usize)>,
}

/// Django's `tag_re.split` split points + alternation semantics, then
/// `Lexer.create_token` with verbatim state. `debug` records token positions.
pub fn tokenize(source: &str, debug: bool) -> Vec<Token> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut tokens: Vec<Token> = Vec::new();
    let mut pos = 0usize; // start of current (implicit) text piece
    let mut lineno = 1u32;
    let mut verbatim: Option<String> = None;

    while let Some((start, end)) = find_tag(bytes, pos) {
        // text piece before the tag
        if start > pos {
            let s = &source[pos..start];
            tokens.push(Token {
                kind: TokenKind::Text,
                contents: s.to_string(),
                lineno,
                position: if debug { Some((pos, start)) } else { None },
            });
            lineno += count_nl(s);
        }
        let raw = &source[start..end];
        let inner = raw[2..raw.len() - 2].trim();
        let kind;
        let contents;
        match raw.as_bytes()[0] {
            b'{' if raw.as_bytes().get(1) == Some(&b'%') => {
                if let Some(v) = &verbatim {
                    if inner != v.as_str() {
                        kind = TokenKind::Text;
                        contents = raw.to_string();
                    } else {
                        verbatim = None;
                        kind = TokenKind::Block;
                        contents = inner.to_string();
                    }
                } else if inner == "verbatim" || inner.starts_with("verbatim ") {
                    verbatim = Some(format!("end{inner}"));
                    kind = TokenKind::Block;
                    contents = inner.to_string();
                } else {
                    kind = TokenKind::Block;
                    contents = inner.to_string();
                }
            }
            _ => {
                if verbatim.is_some() {
                    kind = TokenKind::Text;
                    contents = raw.to_string();
                } else if raw.as_bytes()[1] == b'{' {
                    kind = TokenKind::Var;
                    contents = inner.to_string();
                } else {
                    kind = TokenKind::Comment;
                    contents = inner.to_string();
                }
            }
        }
        tokens.push(Token {
            kind,
            contents,
            lineno,
            position: if debug { Some((start, end)) } else { None },
        });
        lineno += count_nl(raw);
        pos = end;
    }
    // trailing text
    if pos < len {
        let s = &source[pos..len];
        tokens.push(Token {
            kind: TokenKind::Text,
            contents: s.to_string(),
            lineno,
            position: if debug { Some((pos, len)) } else { None },
        });
    }
    tokens
}

fn count_nl(s: &str) -> u32 {
    s.as_bytes().iter().filter(|&&b| b == b'\n').count() as u32
}

/// Find the next tag: leftmost-first alternation of
/// `{% .*? %}` | `{{ .*? }}` | `{# .*? #}` with `.` not matching `\n`.
/// Returns byte range of the whole match (including delimiters).
fn find_tag(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while let Some(rel) = memchr::memchr(b'{', &bytes[i..]) {
        i += rel;
        let close = match bytes.get(i + 1) {
            Some(b'%') => b'%',
            Some(b'{') => b'}',
            Some(b'#') => b'#',
            Some(_) => {
                i += 1;
                continue;
            }
            None => return None,
        };
        if let Some(rel) = find_close(bytes, i + 2, close, b'}') {
            return Some((i, rel + 2));
        }
        i += 1;
    }
    None
}

/// Find the first occurrence of `c1 c2` on one line at or after `from`
/// (the lazy `.*?` plus closing delimiter, without crossing `\n`).
fn find_close(bytes: &[u8], from: usize, c1: u8, c2: u8) -> Option<usize> {
    let mut j = from;
    while let Some(rel) = memchr::memchr2(b'\n', c1, bytes.get(j..)?) {
        j += rel;
        if bytes[j] == b'\n' {
            return None;
        }
        if bytes.get(j + 1) == Some(&c2) {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// Port of `django.utils.text.smart_split` (regex-free, same semantics).
pub fn smart_split(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let len = b.len();
    let mut bits: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < len {
        if b[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // Alt 1: ([^\s'"]* quoted [^\s'"]*)+ with >= 1 repetition.
        if let Some(end) = try_alt1(b, i) {
            bits.push(text[i..end].to_string());
            i = end;
            continue;
        }
        // Alt 2: \S+
        let start = i;
        while i < len && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        bits.push(text[start..i].to_string());
    }
    bits
}

fn is_quote(c: u8) -> bool {
    c == b'\'' || c == b'"'
}

/// Tries the `((?:[^\s'"]* quoted [^\s'"]*)+)` alternative at `i`; returns the
/// exclusive end byte index on success. Backtracking is not needed: the
/// maximal prefix/suffix runs and the deterministic quoted scan make the
/// greedy match unique.
fn try_alt1(b: &[u8], i: usize) -> Option<usize> {
    let len = b.len();
    let mut pos = i;
    let mut reps = 0usize;
    loop {
        // [^\s'"]*
        while pos < len && !b[pos].is_ascii_whitespace() && !is_quote(b[pos]) {
            pos += 1;
        }
        // quoted literal: quote ( non-quote-non-backslash | \. )* quote
        if pos >= len || !is_quote(b[pos]) {
            break;
        }
        let quote = b[pos];
        let mut j = pos + 1;
        let mut closed = false;
        while j < len {
            match b[j] {
                b'\\' => j += 2,
                c if c == quote => {
                    closed = true;
                    j += 1;
                    break;
                }
                _ => j += 1,
            }
        }
        if !closed || j > len {
            break;
        }
        pos = j;
        reps += 1;
        // loop continues; next iteration's prefix may be empty
    }
    if reps == 0 {
        None
    } else {
        Some(pos)
    }
}

/// Port of `Token.split_contents`: `smart_split` plus joining of
/// translation-marked pieces (`_(" ... ")` may contain spaces).
pub fn split_contents(contents: &str) -> SplitContents {
    let raw = smart_split(contents);
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    let mut idx = 0usize;
    while idx < raw.len() {
        let bit = raw[idx].clone();
        idx += 1;
        if bit.starts_with("_(\"") || bit.starts_with("(_('") {
            let sentinel = format!("{})", &bit[2..3]);
            let mut trans_bit = vec![bit];
            let mut terminated = trans_bit[0].ends_with(&sentinel);
            while !terminated {
                if idx >= raw.len() {
                    return SplitContents::UnterminatedI18n;
                }
                let next = raw[idx].clone();
                idx += 1;
                terminated = next.ends_with(&sentinel);
                trans_bit.push(next);
            }
            out.push(trans_bit.join(" "));
        } else {
            out.push(bit);
        }
    }
    SplitContents::Ok(out)
}

pub enum SplitContents {
    Ok(Vec<String>),
    /// `next(bits)` would raise StopIteration inside split_contents.
    UnterminatedI18n,
}

/// Port of `django.utils.text.unescape_string_literal`.
/// Returns Err(()) exactly when Python raises ValueError.
pub fn unescape_string_literal(s: &str) -> Result<String, ()> {
    let b = s.as_bytes();
    if b.is_empty() || (b[0] != b'"' && b[0] != b'\'') || b[b.len() - 1] != b[0] {
        return Err(());
    }
    let quote = b[0];
    let inner = &s[1..s.len() - 1];
    // .replace(r"\<quote>", "<quote>").replace(r"\\", "\\") — note Django's
    // chained replaces are NOT escape-aware; port them literally.
    let q = (quote as char).to_string();
    let step = inner.replace(&format!("\\{q}"), &q).replace("\\\\", "\\");
    Ok(step)
}

/// Port of `django.utils.text.get_text_list` (non-translated "or").
pub fn get_text_list(items: &[String], last_word: &str) -> String {
    match items.len() {
        0 => String::new(),
        1 => items[0].clone(),
        n => {
            let head = items[..n - 1].join(", ");
            format!("{head} {last_word} {}", items[n - 1])
        }
    }
}

/// Python `int(str)` semantics restricted to token shapes our grammar can
/// produce: optional sign, Unicode-decimal digits with PEP 515 underscores.
pub fn py_int_literal(var: &str) -> Option<BigInt> {
    let (sign, digits) = split_signed(var)?;
    let cleaned = validate_underscores(digits)?;
    if cleaned.is_empty() {
        return None;
    }
    let mut acc = BigInt::from(0);
    for c in cleaned.chars() {
        let d = c.to_digit(10)?;
        acc = acc * 10u32 + d;
    }
    Some(if sign < 0 { -acc } else { acc })
}

/// Python `float(str)` semantics for token shapes: specials, underscores
/// between digits; rejects trailing `.` (Django adds that rule itself).
pub fn py_float_literal(var: &str) -> Option<f64> {
    let lowered = var.to_ascii_lowercase();
    let cleaned = validate_underscores(lowered.as_str())?;
    // Python specials (after optional sign)
    let (sign, rest) = split_signed_clean(&cleaned);
    let v: f64 = match rest {
        "inf" | "infinity" => f64::INFINITY,
        "nan" => f64::NAN,
        _ => cleaned.parse::<f64>().ok()?,
    };
    Some(if sign < 0 { -v } else { v })
}

fn split_signed(var: &str) -> Option<(i32, &str)> {
    match var.as_bytes().first() {
        Some(b'-') => Some((-1, &var[1..])),
        Some(b'+') => Some((1, &var[1..])),
        _ => Some((1, var)),
    }
}

fn split_signed_clean(var: &str) -> (i32, &str) {
    match var.as_bytes().first() {
        Some(b'-') => (-1, &var[1..]),
        Some(b'+') => (1, &var[1..]),
        _ => (1, var),
    }
}

/// Enforce PEP 515 underscore rules (each `_` sits between two Nd chars) and
/// return the string with underscores removed. Also rejects leading `_`.
fn validate_underscores(s: &str) -> Option<String> {
    if s.is_empty() {
        return Some(String::new());
    }
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    for (idx, c) in chars.iter().enumerate() {
        if *c == '_' {
            let prev_digit = idx > 0 && is_digit_c(chars[idx - 1]);
            let next_digit = idx + 1 < chars.len() && is_digit_c(chars[idx + 1]);
            if !(prev_digit && next_digit) {
                return None;
            }
        } else {
            out.push(*c);
        }
    }
    Some(out)
}

fn is_digit_c(c: char) -> bool {
    c.to_digit(10).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytewise reference for `find_tag` (the pre-`memchr` implementation).
    fn find_tag_ref(b: &[u8], from: usize) -> Option<(usize, usize)> {
        (from..b.len()).find_map(|i| {
            let close = match (b[i], b.get(i + 1)) {
                (b'{', Some(b'%')) => b'%',
                (b'{', Some(b'{')) => b'}',
                (b'{', Some(b'#')) => b'#',
                _ => return None,
            };
            (i + 2..b.len().saturating_sub(1))
                .take_while(|&j| b[j] != b'\n')
                .find(|&j| b[j] == close && b[j + 1] == b'}')
                .map(|j| (i, j + 2))
        })
    }

    #[test]
    fn find_tag_matches_reference() {
        let cases = [
            "", "{", "{{", "{{}}", "{%%}", "{##}", "a {{ x }} b", "{ {% x %} }",
            "{{ x\n}} {{ y }}", "{{{ x }}}", "}} %} #}", "{% '%}' %}", "{{ é }}✓{#\n#}",
            "{{ x }", "{# a #} {% b %} {{ c }}", "x{", "x{{y}",
        ];
        for s in cases {
            for from in 0..=s.len() {
                assert_eq!(
                    find_tag(s.as_bytes(), from),
                    find_tag_ref(s.as_bytes(), from),
                    "{s:?} from {from}"
                );
            }
        }
    }

    #[test]
    fn text_and_tags() {
        let toks = tokenize("a {{ b }} c {% if x %}d{% endif %}", false);
        let kinds: Vec<_> = toks.iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Text,
                TokenKind::Var,
                TokenKind::Text,
                TokenKind::Block,
                TokenKind::Text,
                TokenKind::Block,
            ]
        );
        assert_eq!(toks[1].contents, "b");
        assert_eq!(toks[3].contents, "if x");
    }

    #[test]
    fn verbatim_mode() {
        let toks = tokenize("{% verbatim %}{{ x }}{% endverbatim %}", false);
        assert_eq!(toks.len(), 3);
        assert_eq!(toks[1].kind, TokenKind::Text);
        assert_eq!(toks[1].contents, "{{ x }}");
        assert_eq!(toks[2].contents, "endverbatim");
    }

    #[test]
    fn unterminated_tags_are_text() {
        let toks = tokenize("{{ x", false);
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].kind, TokenKind::Text);
    }

    #[test]
    fn comment_kind() {
        let toks = tokenize("{# c #}", false);
        assert_eq!(toks[0].kind, TokenKind::Comment);
        assert_eq!(toks[0].contents, "c");
    }

    #[test]
    fn linenumbers() {
        let toks = tokenize("a\nb {{ x }}\nc", false);
        assert_eq!(toks[0].lineno, 1);
        assert_eq!(toks[1].lineno, 2);
        assert_eq!(toks[2].lineno, 2);
    }

    #[test]
    fn smart_split_cases() {
        assert_eq!(
            smart_split(r#"This is "a person\'s" test."#),
            vec!["This", "is", r#""a person\'s""#, "test."]
        );
        assert_eq!(
            smart_split(r#"A "\"funky\" style" test."#),
            vec!["A", r#""\"funky\" style""#, "test."]
        );
        assert_eq!(smart_split("a  b\tc"), vec!["a", "b", "c"]);
        assert_eq!(smart_split(r#""abc"#), vec![r#""abc"#]);
    }

    #[test]
    fn split_contents_i18n_join() {
        match split_contents("trans \"hello world\" end") {
            SplitContents::Ok(v) => assert_eq!(v, vec!["trans", "\"hello world\"", "end"]),
            _ => panic!(),
        }
        match split_contents("trans _(\"hello world\") end") {
            SplitContents::Ok(v) => assert_eq!(v, vec!["trans", "_(\"hello world\")", "end"]),
            _ => panic!(),
        }
        match split_contents("_('a b')") {
            SplitContents::Ok(v) => assert_eq!(v, vec!["_('a b')"]),
            _ => panic!(),
        }
    }

    #[test]
    fn unescape_cases() {
        assert_eq!(unescape_string_literal("\"abc\"").unwrap(), "abc");
        assert_eq!(unescape_string_literal("'abc'").unwrap(), "abc");
        assert_eq!(unescape_string_literal("\"a \\\"bc\\\"\"").unwrap(), "a \"bc\"");
        assert_eq!(unescape_string_literal("'\\'ab\\' c'").unwrap(), "'ab' c");
        assert!(unescape_string_literal("abc").is_err());
        assert!(unescape_string_literal("\"abc").is_err());
    }

    #[test]
    fn int_and_float_literals() {
        assert_eq!(py_int_literal("42").unwrap().to_string(), "42");
        assert_eq!(py_int_literal("-7").unwrap().to_string(), "-7");
        assert_eq!(py_int_literal("+7").unwrap().to_string(), "7");
        assert_eq!(py_int_literal("1_000").unwrap().to_string(), "1000");
        assert!(py_int_literal("").is_none());
        assert!(py_int_literal("_1").is_none());
        assert!(py_int_literal("1_").is_none());
        assert_eq!(py_float_literal("2."), Some(2.0));
        assert_eq!(py_float_literal(".5"), Some(0.5));
        assert_eq!(py_float_literal("1e3"), Some(1000.0));
        assert_eq!(py_float_literal("inf"), Some(f64::INFINITY));
        assert_eq!(py_float_literal("1_0.5"), Some(10.5));
        assert!(py_float_literal("1_").is_none());
    }
}
