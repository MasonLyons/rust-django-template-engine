//! Native `floatformat` (port of `defaultfilters.floatformat` +
//! `formats.number_format` + `numberformat.format`).
//!
//! The value is taken through its exact decimal digit string (`str(int)`,
//! `repr(float)` via `str`, `str(Decimal)` or the input `str`), so
//! `Decimal(str(text))` is reproduced digit-for-digit: `as_tuple()`'s
//! coefficient/exponent, the 200-digit cutoff, `int(d) - d` and
//! `quantize(exp, ROUND_HALF_UP)` are all integer digit-string operations.
//! Localization stays native too: `get_language()` / `get_format()` are
//! the live-cache helpers shared with `{{ value }}` rendering, and the
//! separator/grouping layout is a straight port of `numberformat.format`.
//!
//! Anything outside the proven grammar (non-ASCII / whitespace /
//! underscore strings, NaN/Infinity, exponents beyond the cutoff, exotic
//! args, non-`str` separators, lazy inputs, subclasses) calls the real
//! Python filter, which raises / returns exactly what stock does.

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyFloat, PyInt, PyList, PyString, PyTuple, PyType};

use crate::render::context::Rctx;
use crate::render::value::{helpers, mark_safe_str};

struct Ff {
    py_floatformat: Py<PyAny>,
    decimal: Py<PyType>,
}

fn ff(py: Python<'_>) -> PyResult<&Ff> {
    static FF: PyOnceLock<Ff> = PyOnceLock::new();
    FF.get_or_try_init(py, || -> PyResult<Ff> {
        Ok(Ff {
            py_floatformat: py
                .import("django.template.defaultfilters")?
                .getattr("floatformat")?
                .unbind(),
            decimal: py
                .import("decimal")?
                .getattr("Decimal")?
                .cast_into::<PyType>()?
                .unbind(),
        })
    })
}

/// A finite `Decimal` as `as_tuple()` reports it: sign, coefficient digits
/// (ASCII, no leading zeros, never empty) and exponent.
struct Dec {
    neg: bool,
    coef: Vec<u8>,
    exp: i64,
}

/// Strict `Decimal(str)` grammar: `[+-]? (digits [. digits?] | . digits)
/// ([eE] [+-]? digits)?`, ASCII only, no whitespace/underscores. Exponents
/// are bounded (the 200 cutoff returns long before they matter); `None`
/// sends the call to Python.
fn parse_decimal(s: &str) -> Option<Dec> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut neg = false;
    if let Some(&c) = b.first() {
        if c == b'+' || c == b'-' {
            neg = c == b'-';
            i = 1;
        }
    }
    let int_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = &b[int_start..i];
    let mut frac_digits: &[u8] = &[];
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let fs = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = &b[fs..i];
    }
    if int_digits.is_empty() && frac_digits.is_empty() {
        return None;
    }
    let mut e: i64 = 0;
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        let mut eneg = false;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            eneg = b[i] == b'-';
            i += 1;
        }
        let es = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == es || i - es > 6 {
            return None;
        }
        e = s[es..i].parse().ok()?;
        if eneg {
            e = -e;
        }
    }
    if i != b.len() {
        return None;
    }
    let mut coef: Vec<u8> = Vec::with_capacity(int_digits.len() + frac_digits.len());
    coef.extend_from_slice(int_digits);
    coef.extend_from_slice(frac_digits);
    let lead = coef.iter().take_while(|&&c| c == b'0').count();
    let lead = lead.min(coef.len() - 1);
    coef.drain(..lead);
    let exp = e - i64::try_from(frac_digits.len()).ok()?;
    if exp.abs() > 200 || coef.len() > 200 {
        return None;
    }
    Some(Dec { neg, coef, exp })
}

/// Parsed filter arg: `(p, force_grouping, use_l10n)`; `None` → Python.
fn parse_arg(args: &[Bound<'_, PyAny>]) -> Option<(i64, bool, bool)> {
    let Some(arg) = args.first() else {
        return Some((-1, false, true));
    };
    if let Ok(i) = arg.cast_exact::<PyInt>() {
        let p: i64 = i.extract().ok()?;
        return (p.abs() <= 1000).then_some((p, false, true));
    }
    // exact `str` or `SafeString` (string literals arrive mark_safe'd)
    let s = arg.cast::<PyString>().ok()?;
    let ty = arg.get_type();
    if !(arg.is_exact_instance_of::<PyString>()
        || ty.is(helpers(arg.py()).ok()?.safe_string.bind(arg.py())))
    {
        return None;
    }
    let s = s.to_str().ok()?;
    if s.is_empty() {
        return None; // `arg[-1]` IndexError
    }
    let (rest, grouping, l10n) = if s.ends_with("gu") || s.ends_with("ug") {
        (&s[..s.len() - 2], true, false)
    } else if let Some(r) = s.strip_suffix('g') {
        (r, true, true)
    } else if let Some(r) = s.strip_suffix('u') {
        (r, false, false)
    } else {
        (s, false, true)
    };
    if rest.is_empty() {
        return Some((-1, grouping, l10n));
    }
    let digits = rest.strip_prefix(['+', '-']).unwrap_or(rest);
    if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut p: i64 = digits.parse().ok()?;
    if rest.starts_with('-') {
        p = -p;
    }
    Some((p, grouping, l10n))
}

/// Non-negative `i64` → `usize` (all values here are bounded by the
/// 200-digit cutoff or the 4-digit precision limit).
fn to_usize(v: i64) -> usize {
    usize::try_from(v).unwrap_or(0)
}

fn len_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// `"%d" % int(d)`.
fn int_string(dec: &Dec) -> String {
    let len = len_i64(dec.coef.len());
    let mut digits: String = if dec.exp >= 0 {
        if dec.coef == b"0" {
            "0".to_string()
        } else {
            let mut s = String::from_utf8_lossy(&dec.coef).into_owned();
            s.extend(std::iter::repeat_n('0', to_usize(dec.exp)));
            s
        }
    } else if len + dec.exp > 0 {
        String::from_utf8_lossy(&dec.coef[..to_usize(len + dec.exp)]).into_owned()
    } else {
        "0".to_string()
    };
    if dec.neg && digits != "0" {
        digits.insert(0, '-');
    }
    digits
}

/// `int(d) - d == 0`.
fn fraction_is_zero(dec: &Dec) -> bool {
    if dec.exp >= 0 {
        return true;
    }
    let frac_len = to_usize(-dec.exp);
    let len = dec.coef.len();
    if frac_len >= len {
        return dec.coef == b"0";
    }
    dec.coef[len - frac_len..].iter().all(|&c| c == b'0')
}

/// Coefficient of `abs(d).quantize(Decimal(1).scaleb(-places), ROUND_HALF_UP)`.
fn quantize_coef(dec: &Dec, places: usize) -> Vec<u8> {
    let target = -len_i64(places);
    if dec.exp >= target {
        if dec.coef == b"0" {
            return b"0".to_vec();
        }
        let mut digits = dec.coef.clone();
        digits.extend(std::iter::repeat_n(b'0', to_usize(dec.exp - target)));
        return digits;
    }
    let dropped = to_usize(target - dec.exp);
    let len = dec.coef.len();
    let (mut kept, first_dropped) = match dropped.cmp(&len) {
        std::cmp::Ordering::Greater => (b"0".to_vec(), b'0'),
        std::cmp::Ordering::Equal => (b"0".to_vec(), dec.coef[0]),
        std::cmp::Ordering::Less => (dec.coef[..len - dropped].to_vec(), dec.coef[len - dropped]),
    };
    if first_dropped >= b'5' {
        let mut pos = kept.len();
        loop {
            if pos == 0 {
                kept.insert(0, b'1');
                break;
            }
            pos -= 1;
            if kept[pos] == b'9' {
                kept[pos] = b'0';
            } else {
                kept[pos] += 1;
                break;
            }
        }
    }
    kept
}

/// The `number` string stock builds from `rounded_d.as_tuple()`.
fn rounded_number(dec: &Dec, places: usize) -> String {
    let coef = quantize_coef(dec, places);
    let mut s = String::with_capacity(coef.len() + places + 3);
    if dec.neg && coef != b"0" {
        s.push('-');
    }
    let pad = (places + 1).saturating_sub(coef.len());
    let mut digits = Vec::with_capacity(pad + coef.len());
    digits.extend(std::iter::repeat_n(b'0', pad));
    digits.extend_from_slice(&coef);
    let split = digits.len() - places;
    s.push_str(std::str::from_utf8(&digits[..split]).unwrap_or_default());
    s.push('.');
    s.push_str(std::str::from_utf8(&digits[split..]).unwrap_or_default());
    s
}

/// `numberformat.format` grouping intervals; `None` for shapes the port
/// does not model (→ Python).
#[allow(clippy::option_option)]
fn intervals(grouping: &Bound<'_, PyAny>) -> Option<Option<Vec<i64>>> {
    if let Ok(i) = grouping.cast_exact::<PyInt>() {
        let g: i64 = i.extract().ok()?;
        return Some((g != 0).then(|| vec![g, 0]));
    }
    let items: Vec<Bound<'_, PyAny>> = if let Ok(t) = grouping.cast_exact::<PyTuple>() {
        t.iter().collect()
    } else if let Ok(l) = grouping.cast_exact::<PyList>() {
        l.iter().collect()
    } else {
        return None;
    };
    let mut v = Vec::with_capacity(items.len());
    for it in items {
        v.push(it.cast_exact::<PyInt>().ok()?.extract::<i64>().ok()?);
    }
    if v.is_empty() {
        return None; // `intervals.pop(0)` IndexError
    }
    Some(Some(v))
}

fn exact_str<'a>(v: &'a Bound<'_, PyAny>) -> Option<&'a str> {
    v.cast_exact::<PyString>().ok()?.to_str().ok()
}

/// `formats.number_format(number, decimal_pos, use_l10n, force_grouping)`
/// for a `str` number; `None` → Python.
fn number_format(
    rc: &mut Rctx<'_>,
    number: &str,
    decimal_pos: usize,
    use_l10n: bool,
    force_grouping: bool,
) -> PyResult<Option<String>> {
    let py = rc.py();
    let lang = if use_l10n {
        match super::get_language(py) {
            Some(l) => l,
            None => return Ok(None),
        }
    } else {
        None
    };
    let sep_obj = super::get_format(py, "DECIMAL_SEPARATOR", lang.as_deref(), use_l10n)?;
    let Some(decimal_sep) = exact_str(&sep_obj) else {
        return Ok(None);
    };
    let want_grouping = (use_l10n && !rc.int_str_is_plain()?) || force_grouping;
    let mut grouping: Option<(Vec<i64>, String)> = None;
    if want_grouping {
        let g = super::get_format(py, "NUMBER_GROUPING", lang.as_deref(), use_l10n)?;
        let Some(iv) = intervals(&g) else {
            return Ok(None);
        };
        let ts_obj = super::get_format(py, "THOUSAND_SEPARATOR", lang.as_deref(), use_l10n)?;
        if let Some(iv) = iv {
            let Some(ts) = exact_str(&ts_obj) else {
                return Ok(None);
            };
            grouping = Some((iv, ts.to_string()));
        }
    }
    let (sign, body) = match number.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", number),
    };
    let (int_part, mut dec_part) = match body.split_once('.') {
        Some((i, d)) => (i, d[..d.len().min(decimal_pos)].to_string()),
        None => (body, String::new()),
    };
    dec_part.extend(std::iter::repeat_n('0', decimal_pos - dec_part.len()));
    let mut out = String::with_capacity(number.len() + 16);
    out.push_str(sign);
    match grouping {
        Some((mut iv, thousand_sep)) => {
            let rev_sep: String = thousand_sep.chars().rev().collect();
            let mut active = iv.remove(0);
            let mut gd = String::with_capacity(int_part.len() * 2);
            let mut cnt: i64 = 0;
            for digit in int_part.chars().rev() {
                if cnt != 0 && cnt == active {
                    if !iv.is_empty() {
                        let nxt = iv.remove(0);
                        if nxt != 0 {
                            active = nxt;
                        }
                    }
                    gd.push_str(&rev_sep);
                    cnt = 0;
                }
                gd.push(digit);
                cnt += 1;
            }
            out.extend(gd.chars().rev());
        }
        None => out.push_str(int_part),
    }
    if !dec_part.is_empty() {
        out.push_str(decimal_sep);
        out.push_str(&dec_part);
    }
    Ok(Some(out))
}

fn fast<'py>(
    rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
) -> PyResult<Option<Bound<'py, PyAny>>> {
    let py = rc.py();
    let Some((p, force_grouping, use_l10n)) = parse_arg(args) else {
        return Ok(None);
    };
    // `input_val = str(text)`: the exact str (or SafeString, whose `__str__`
    // returns itself) is its own `str`; numbers stringify natively.
    let (input_val, text): (Bound<'py, PyAny>, String) =
        if input.is_exact_instance_of::<PyString>()
            || input.get_type().is(helpers(py)?.safe_string.bind(py))
        {
            let Ok(s) = input.cast::<PyString>()?.to_str() else {
                return Ok(None);
            };
            if s.is_empty() {
                // Decimal("") and float("") both fail → "".
                return Ok(Some(PyString::new(py, "").into_any()));
            }
            (input.clone(), s.to_string())
        } else if input.is_exact_instance_of::<PyInt>()
            || input.is_exact_instance_of::<PyFloat>()
            || input.get_type().is(ff(py)?.decimal.bind(py))
        {
            let s = input.str()?;
            let t = s.to_str()?.to_string();
            (s.into_any(), t)
        } else {
            return Ok(None);
        };
    let Some(d) = parse_decimal(&text) else {
        return Ok(None);
    };
    if len_i64(d.coef.len()) + d.exp.abs() > 200 {
        return Ok(Some(input_val));
    }
    let frac_zero = fraction_is_zero(&d);
    let out = if frac_zero && p <= 0 {
        number_format(rc, &int_string(&d), 0, use_l10n, force_grouping)?
    } else {
        let places = to_usize(p.abs());
        number_format(rc, &rounded_number(&d, places), places, use_l10n, force_grouping)?
    };
    match out {
        Some(s) => Ok(Some(mark_safe_str(py, &s)?)),
        None => Ok(None),
    }
}

/// `floatformat(text, arg=-1)`.
pub(crate) fn floatformat<'py>(
    rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    if let Some(v) = fast(rc, input, args)? {
        return Ok(v);
    }
    let py = rc.py();
    let mut a: Vec<Bound<'py, PyAny>> = Vec::with_capacity(1 + args.len());
    a.push(input.clone());
    a.extend(args.iter().cloned());
    ff(py)?.py_floatformat.bind(py).call1(PyTuple::new(py, a)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(s: &str, p: i64) -> String {
        let d = parse_decimal(s).unwrap();
        if fraction_is_zero(&d) && p <= 0 {
            int_string(&d)
        } else {
            rounded_number(&d, to_usize(p.abs()))
        }
    }

    #[test]
    fn rounding() {
        assert_eq!(num("34.23234", -1), "34.2");
        assert_eq!(num("34.00000", -1), "34");
        assert_eq!(num("34.26000", -1), "34.3");
        assert_eq!(num("0.5", 0), "1.");
        assert_eq!(num("-0.001", 2), "0.00");
        assert_eq!(num("9.995", 2), "10.00");
        assert_eq!(num("1E+5", 2), "100000.00");
        assert_eq!(num("-1.5", 0), "-2.");
        assert_eq!(num("0.0005", 3), "0.001");
    }
}
