//! Native `truncatechars_html` / `truncatewords_html`: a port of
//! `TruncateCharsHTMLParser` / `TruncateWordsHTMLParser` for "simple" HTML
//! (the grammar the native `striptags` accepts: plain start/end tags with
//! ordinary attributes, `&name;` / `&#N;` / `&#xH;` references). On that
//! grammar every `HTMLParser` version Django supports makes the same
//! callbacks: one `handle_data` per text run between tags (charrefs already
//! converted), `handle_starttag` / `handle_startendtag` / `handle_endtag`
//! per tag. Anything else (comments, declarations, raw-text elements, a
//! stray `<` or `&`, an unknown entity, a NUL) runs the stock `Truncator`.

use pyo3::prelude::*;
use pyo3::types::PyString;

use unicode_normalization::UnicodeNormalization;

use crate::render::context::Rctx;
use crate::render::value::{escape_html, helpers};

use super::strings::{add_truncation_text, entity_end, int_of, tag_end, translated_ellipsis};
use super::{is_err, utils};

/// `django.utils.html.VOID_ELEMENTS`.
const VOID_ELEMENTS: [&str; 16] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr", "frame", "spacer",
];

fn is_void(name: &str) -> bool {
    VOID_ELEMENTS.contains(&name)
}

enum Tok<'a> {
    /// Raw start tag text (`get_starttag_text()`), lowercased name, `/>`.
    Start(&'a str, String, bool),
    End(String),
    /// Raw text run (charrefs not yet converted).
    Data(&'a str),
}

/// Split simple HTML into tokens; `None` → not simple, use Python.
fn tokenize(text: &str) -> Option<Vec<Tok<'_>>> {
    if text.contains('\0') {
        return None;
    }
    let b = text.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    let mut last = 0;
    while let Some(off) = memchr::memchr2(b'<', b'&', &b[i..]) {
        let p = i + off;
        if b[p] == b'&' {
            i = entity_end(b, p)?;
            continue;
        }
        let e = tag_end(b, p)?;
        if last < p {
            toks.push(Tok::Data(&text[last..p]));
        }
        let end_tag = b[p + 1] == b'/';
        let name_start = if end_tag { p + 2 } else { p + 1 };
        let name_end = b[name_start..]
            .iter()
            .position(|c| !c.is_ascii_alphanumeric())
            .map_or(b.len(), |n| name_start + n);
        let name = text[name_start..name_end].to_ascii_lowercase();
        if end_tag {
            toks.push(Tok::End(name));
        } else {
            toks.push(Tok::Start(&text[p..e], name, b[e - 2] == b'/'));
        }
        i = e;
        last = e;
    }
    if last < text.len() {
        toks.push(Tok::Data(&text[last..]));
    }
    Some(toks)
}

/// `html.unescape` code points that come back as themselves.
fn plain_codepoint(c: u32) -> Option<char> {
    let ok = matches!(c, 0x9 | 0xa | 0xc | 0x20..=0x7e | 0xa0..=0xd7ff)
        || ((0xe000..=0x10_ffff).contains(&c)
            && !(0xfdd0..=0xfdef).contains(&c)
            && (c & 0xfffe) != 0xfffe);
    if ok {
        char::from_u32(c)
    } else {
        None
    }
}

fn named_entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "copy" => '\u{a9}',
        "reg" => '\u{ae}',
        "trade" => '\u{2122}',
        "hellip" => '\u{2026}',
        "mdash" => '\u{2014}',
        "ndash" => '\u{2013}',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201c}',
        "rdquo" => '\u{201d}',
        "laquo" => '\u{ab}',
        "raquo" => '\u{bb}',
        "middot" => '\u{b7}',
        "bull" => '\u{2022}',
        "times" => '\u{d7}',
        "deg" => '\u{b0}',
        "euro" => '\u{20ac}',
        "pound" => '\u{a3}',
        "sect" => '\u{a7}',
        _ => return None,
    })
}

/// `html.unescape(raw)` for the references `entity_end` accepts; `None`
/// for ones whose stock result is not modelled here.
fn unescape(raw: &str) -> Option<std::borrow::Cow<'_, str>> {
    if !raw.contains('&') {
        return Some(std::borrow::Cow::Borrowed(raw));
    }
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while let Some(off) = memchr::memchr(b'&', &bytes[i..]) {
        let p = i + off;
        out.push_str(&raw[i..p]);
        let e = entity_end(bytes, p)?;
        let body = &raw[p + 1..e - 1];
        let ch = if let Some(num) = body.strip_prefix('#') {
            let v = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse::<u32>().ok()?,
            };
            plain_codepoint(v)?
        } else {
            named_entity(body)?
        };
        out.push(ch);
        i = e;
    }
    out.push_str(&raw[i..]);
    Some(std::borrow::Cow::Owned(out))
}

fn escaped(s: &str) -> String {
    escape_html(s).unwrap_or_else(|| s.to_string())
}

/// Python `str.isspace` (what `\s` matches in a `str` pattern).
fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// `re.split(r"(?<=\S)\s+(?=\S)", data)`.
fn split_words(data: &str) -> Vec<&str> {
    let Some(first) = data.find(|c: char| !py_isspace(c)) else {
        return vec![data];
    };
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut chars = data[first..]
        .char_indices()
        .map(|(i, c)| (i + first, c))
        .peekable();
    while let Some((i, c)) = chars.next() {
        if !py_isspace(c) {
            continue;
        }
        // Whitespace run starting at `i` (preceded by a non-space).
        let mut end = i + c.len_utf8();
        while let Some(&(j, d)) = chars.peek() {
            if !py_isspace(d) {
                break;
            }
            end = j + d.len_utf8();
            chars.next();
        }
        if end < data.len() {
            pieces.push(&data[start..i]);
            start = end;
        }
    }
    pieces.push(&data[start..]);
    pieces
}

enum Mode<'e> {
    /// `TruncateCharsHTMLParser`: `length`, the ellipsis format, total
    /// code points of the input (`len(self.rawdata)`).
    Chars {
        length: i64,
        processed: i64,
        ellipsis: &'e str,
        raw_len: i64,
    },
    Words,
}

fn clen(s: &str) -> i64 {
    i64::try_from(s.chars().count()).unwrap_or(i64::MAX)
}

/// Run the parser callbacks over `toks`; `None` → Python.
fn truncate(toks: &[Tok<'_>], mut remaining: i64, mut mode: Mode<'_>) -> Option<String> {
    let mut out = String::new();
    let mut out_len: i64 = 0;
    let mut tags: Vec<&str> = Vec::new();
    let push = |out: &mut String, out_len: &mut i64, s: &str| {
        out.push_str(s);
        *out_len += clen(s);
    };
    let mut done = false;
    for tok in toks {
        match tok {
            Tok::Start(raw, name, self_closing) => {
                push(&mut out, &mut out_len, raw);
                if !is_void(name) {
                    tags.push(name);
                    if *self_closing {
                        push(&mut out, &mut out_len, &format!("</{name}>"));
                        if tags.last() == Some(&name.as_str()) {
                            tags.pop();
                        }
                    }
                }
            }
            Tok::End(name) => {
                if !is_void(name) {
                    push(&mut out, &mut out_len, &format!("</{name}>"));
                    if tags.last() == Some(&name.as_str()) {
                        tags.pop();
                    }
                }
            }
            Tok::Data(raw) => {
                let data = unescape(raw)?;
                match &mut mode {
                    Mode::Chars {
                        length,
                        processed,
                        ellipsis,
                        raw_len,
                    } => {
                        let data_len = clen(&data);
                        *processed += data_len;
                        if *processed == *length && out_len + data_len == *raw_len {
                            out.push_str(&data);
                            done = true;
                        } else {
                            let keep = usize::try_from(remaining).unwrap_or(0);
                            let head = match data.char_indices().nth(keep) {
                                Some((at, _)) => &data[..at],
                                None => &data[..],
                            };
                            let output = escaped(head);
                            if remaining < data_len {
                                out.push_str(&add_truncation_text(&output, ellipsis));
                                done = true;
                            } else {
                                remaining -= data_len;
                                push(&mut out, &mut out_len, &output);
                            }
                        }
                    }
                    Mode::Words => {
                        let words = split_words(&data);
                        let n = i64::try_from(words.len()).unwrap_or(i64::MAX);
                        let keep = usize::try_from(remaining).unwrap_or(0).min(words.len());
                        let output = escaped(&words[..keep].join(" "));
                        if remaining < n {
                            out.push_str(&add_truncation_text(&output, " …"));
                            done = true;
                        } else {
                            remaining -= n;
                            push(&mut out, &mut out_len, &output);
                        }
                    }
                }
                if done {
                    break;
                }
            }
        }
    }
    if done {
        for tag in tags.iter().rev() {
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
    }
    Some(out)
}

/// The `str` the stock `Truncator` would wrap, when it is an exact `str`
/// or `SafeString` (both stringify to their own contents).
fn plain_text<'a>(s: &'a Bound<'_, PyString>) -> PyResult<Option<&'a str>> {
    let py = s.py();
    if !(s.is_exact_instance_of::<PyString>() || s.get_type().is(helpers(py)?.safe_string.bind(py)))
    {
        return Ok(None);
    }
    Ok(s.to_str().ok())
}

/// Shared prologue: `(str(value), int(arg), int(arg) as i64)`, or `Err(v)`
/// for a stock early return `v` (`int(arg)` `ValueError` → value,
/// `length <= 0` → "").
type Prologue<'py> =
    Result<(Bound<'py, PyString>, Bound<'py, PyAny>, Option<i64>), Bound<'py, PyAny>>;

fn prologue<'py>(input: &Bound<'py, PyAny>, arg: &Bound<'py, PyAny>) -> PyResult<Prologue<'py>> {
    let py = input.py();
    let s = input.str()?;
    let length = match int_of(py, arg) {
        Ok(v) => v,
        Err(e) if is_err::<pyo3::exceptions::PyValueError>(py, &e) => return Ok(Err(s.into_any())),
        Err(e) => return Err(e),
    };
    let n = length.extract::<i64>().ok();
    if n.is_some_and(|n| n <= 0) {
        return Ok(Err(PyString::new(py, "").into_any()));
    }
    Ok(Ok((s, length, n)))
}

fn truncator_html<'py>(
    s: &Bound<'py, PyString>,
    method: &str,
    length: &Bound<'py, PyAny>,
    truncate: Option<&str>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = s.py();
    let kwargs = pyo3::types::PyDict::new(py);
    kwargs.set_item("html", true)?;
    if let Some(t) = truncate {
        kwargs.set_item("truncate", t)?;
    }
    let trunc = utils(py)?.truncator.bind(py).call1((s,))?;
    trunc.call_method(method, (length,), Some(&kwargs))
}

/// `truncatechars_html`: `Truncator(value).chars(length, html=True)`.
pub(crate) fn truncatechars_html<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let (s, length, n) = match prologue(input, &args[0])? {
        Ok(v) => v,
        Err(early) => return Ok(early),
    };
    if let (Some(n), Some(raw)) = (n, plain_text(&s)?) {
        let normed;
        let text = if unicode_normalization::is_nfc_quick(raw.chars())
            == unicode_normalization::IsNormalized::Yes
        {
            raw
        } else {
            normed = raw.nfc().collect::<String>();
            normed.as_str()
        };
        if let Some(toks) = tokenize(text) {
            if let Some((ellipsis, width)) = translated_ellipsis(py)? {
                let mode = Mode::Chars {
                    length: n,
                    processed: 0,
                    ellipsis: &ellipsis,
                    raw_len: clen(text),
                };
                if let Some(out) = truncate(&toks, (n - width).max(0), mode) {
                    return Ok(PyString::new(py, &out).into_any());
                }
            }
        }
    }
    truncator_html(&s, "chars", &length, None)
}

/// `truncatewords_html`: `Truncator(value).words(length, html=True,
/// truncate=" …")`.
pub(crate) fn truncatewords_html<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let (s, length, n) = match prologue(input, &args[0])? {
        Ok(v) => v,
        Err(early) => return Ok(early),
    };
    if let (Some(n), Some(text)) = (n, plain_text(&s)?) {
        if let Some(toks) = tokenize(text) {
            if let Some(out) = truncate(&toks, n, Mode::Words) {
                return Ok(PyString::new(py, &out).into_any());
            }
        }
    }
    truncator_html(&s, "words", &length, Some(" …"))
}

#[cfg(test)]
mod tests {
    use super::split_words;

    #[test]
    fn words_split() {
        assert_eq!(split_words("a b"), vec!["a", "b"]);
        assert_eq!(split_words("  a  b  "), vec!["  a", "b  "]);
        assert_eq!(split_words("   "), vec!["   "]);
        assert_eq!(split_words("ab"), vec!["ab"]);
        assert_eq!(split_words("a\u{a0}\nb\tc"), vec!["a", "b", "c"]);
    }
}
