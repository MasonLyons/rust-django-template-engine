//! Date/time filters of `django.template.defaultfilters` (native ports).
//!
//! `expects_localtime` conversion already happened in `apply_filter` before
//! the native is called, matching stock order.
//!
//! `date`/`time` first try a native re-implementation of the stock
//! `django.utils.dateformat` format engine, restricted to:
//! - values that are EXACTLY `datetime.datetime` (subclasses may shadow
//!   attributes; `date`/`time` objects keep their `TypeError` semantics via
//!   the Python path),
//! - format strings that are custom (not in `FORMAT_SETTINGS`, so
//!   `get_format` returns them verbatim) or a `FORMAT_SETTINGS` name /
//!   empty arg resolved through the live `get_format` cache, and contain no
//!   timezone-dependent characters (`e O T Z I r U`).
//!
//! Everything else falls back to the stock Python calls verbatim.

use pyo3::prelude::*;
use pyo3::types::{PyDate, PyDateAccess, PyDateTime, PyString, PyTimeAccess};
use std::cell::RefCell;
use std::collections::HashMap;

use crate::render::context::Rctx;

use super::{is_ae, is_te, is_ve, trans_state, utils};

/// `value in (None, "")` — `==` semantics per element, identity for None.
fn none_or_empty(v: &Bound<'_, PyAny>) -> PyResult<bool> {
    if v.is_none() {
        return Ok(true);
    }
    v.eq(PyString::new(v.py(), ""))
}

// ---------------------------------------------------------------------------
/// Chars whose stock implementation consults `self.timezone`/tzinfo/epoch or
/// needs an external call — always fall back when present in a format.
const FALLBACK_CHARS: [char; 7] = ['e', 'O', 'T', 'Z', 'I', 'r', 'U'];

/// Specifiers only defined on stock `DateFormat` (not `TimeFormat`): the
/// `time` filter must fall back for these (stock `getattr` raises
/// `AttributeError` → the filter returns `""`).
const DATE_ONLY_CHARS: [char; 23] = [
    'b', 'c', 'd', 'D', 'E', 'F', 'j', 'L', 'l', 'm', 'M', 'n', 'N', 'o', 'r', 'S', 't', 'U', 'w',
    'W', 'y', 'Y', 'z',
];

/// Chars matched by stock `re_formatchars`.
const FORMAT_CHARS: &str = "aAbcdDeEfFgGhHiIjlLmMnNoOPrsStTUuwWyYzZ";
/// `formats.FORMAT_SETTINGS` — a format arg naming one of these is resolved
/// per-locale/settings by stock `get_format`, so it never takes the fast
/// path.
const FORMAT_SETTINGS: [&str; 14] = [
    "DECIMAL_SEPARATOR",
    "THOUSAND_SEPARATOR",
    "NUMBER_GROUPING",
    "FIRST_DAY_OF_WEEK",
    "MONTH_DAY_FORMAT",
    "TIME_FORMAT",
    "DATE_FORMAT",
    "DATETIME_FORMAT",
    "SHORT_DATE_FORMAT",
    "SHORT_DATETIME_FORMAT",
    "YEAR_MONTH_FORMAT",
    "DATE_INPUT_FORMATS",
    "TIME_INPUT_FORMATS",
    "DATETIME_INPUT_FORMATS",
];

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i32, m: u32) -> u32 {
    const DIM: [u32; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if m == 2 && is_leap(y) {
        29
    } else {
        DIM[m as usize]
    }
}

fn day_of_year(y: i32, m: u32, d: u32) -> u32 {
    const DOY: [u32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    d + DOY[(m - 1) as usize] + u32::from(m > 2 && is_leap(y))
}

/// Day of week, Sunday = 0 (Sakamoto; proleptic Gregorian, year >= 1).
fn weekday_sun(y: i32, m: u32, d: u32) -> u32 {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let mut yy = i64::from(y);
    if m < 3 {
        yy -= 1;
    }
    // The sum is non-negative for y >= 1 (proleptic Gregorian), % 7 in
    // i64 then try_from to u32.
    u32::try_from(
        (yy + yy / 4 - yy / 100 + yy / 400 + i64::from(T[(m - 1) as usize]) + i64::from(d)) % 7,
    )
    .unwrap_or(0)
}

fn weekday_mon(y: i32, m: u32, d: u32) -> u32 {
    let s = weekday_sun(y, m, d);
    (s + 6) % 7
}

fn iso_year_week(y: i32, m: u32, d: u32) -> (i32, u32) {
    let ord = i64::from(day_of_year(y, m, d));
    let wd = i64::from(weekday_mon(y, m, d)) + 1; // Mon=1..Sun=7
    let week = (ord - wd + 10) / 7;
    if week < 1 {
        let py = y - 1;
        (py, if long_year(py) { 53 } else { 52 })
    } else if week == 53 && !long_year(y) {
        (y + 1, 1)
    } else {
        (y, u32::try_from(week).unwrap_or(1))
    }
}

/// A year has 53 ISO weeks iff Jan 1 is a Thursday, or a Wednesday of a leap
/// year.
fn long_year(y: i32) -> bool {
    let jan1 = weekday_sun(y, 1, 1);
    jan1 == 4 || (jan1 == 3 && is_leap(y))
}

fn ordinal(day: u32) -> &'static str {
    if (11..=13).contains(&day) {
        "th"
    } else {
        match day % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        }
    }
}

/// Python `str.title()` approximation: uppercase the first cased char of
/// each word, lowercase the rest. Month tables only contain simple words;
/// Unicode title-case subtleties go through the fallback path untouched.
fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_cased = false;
    for c in s.chars() {
        if c.is_alphabetic() {
            if prev_cased {
                out.extend(c.to_lowercase());
            } else {
                out.extend(c.to_uppercase());
            }
            prev_cased = true;
        } else {
            out.push(c);
            prev_cased = false;
        }
    }
    out
}

/// One compiled format piece; translated strings are snapshotted when the
/// program is built (cache is keyed by the live language code).
enum Piece {
    Lit(String),
    Year,
    Year2,
    IsoYear,
    Month02,
    Month,
    Day02,
    Day,
    Hour24_02,
    Hour24,
    Hour12_02,
    Hour12,
    Minute02,
    Second02,
    Micro06,
    DayOfYear,
    WeekdaySun,
    IsoWeek,
    LeapYear,
    DaysInMonth,
    Ordinal,
    /// `a`: [a.m., p.m.]
    AmPm([String; 2]),
    /// `A`: [AM, PM]
    AmPmCap([String; 2]),
    /// `P`: [midnight, noon, a.m., p.m.]
    Pm([String; 4]),
    DayAbbr([String; 7]),
    DayLong([String; 7]),
    MonthFull([String; 12]),
    Month3([String; 12]),
    Month3Title([String; 12]),
    MonthAlt([String; 12]),
    MonthAp([String; 12]),
    /// `f`: 12-hour time, minutes omitted when zero.
    TwelveHour,
    /// `c`: `value.isoformat()`.
    Iso,
}

/// Datetime fields read once per value (C-level accessors on the exact
/// `datetime.datetime` type).
struct Fields {
    y: i32,
    m: u32,
    d: u32,
    h: u32,
    mi: u32,
    s: u32,
    us: u32,
    wd: u32, // Monday = 0, like `datetime.weekday()`
    /// `value.isoformat()`, read only when the format may contain `c`.
    iso: Option<String>,
}

/// `value.isoformat()` when `fmt` may use the `c` specifier.
fn iso_for(value: &Bound<'_, PyAny>, fmt: &str) -> PyResult<Option<String>> {
    if fmt.contains('c') {
        Ok(Some(value.call_method0(pyo3::intern!(value.py(), "isoformat"))?.extract()?))
    } else {
        Ok(None)
    }
}

/// Resolve a lazy gettext table into owned strings: `tbl[k]` `str()` for
/// each `k` in keys.
fn load_table(tbl: &Bound<'_, PyAny>, keys: &[u32]) -> PyResult<Vec<String>> {
    let mut v = Vec::with_capacity(keys.len());
    for k in keys {
        v.push(tbl.get_item(*k)?.to_string());
    }
    Ok(v)
}

fn arr<const N: usize>(v: Vec<String>) -> PyResult<[String; N]> {
    v.try_into()
        .map_err(|_| pyo3::exceptions::PyRuntimeError::new_err("table length"))
}

/// Compile `fmt` into pieces, or `None` when a piece needs the Python path.
fn compile_format(py: Python<'_>, fmt: &str) -> PyResult<Option<Vec<Piece>>> {
    let chars: Vec<char> = fmt.chars().collect();
    let mut items: Vec<Piece> = Vec::new();
    let mut lit = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        let escaped = i > 0 && chars[i - 1] == '\\';
        if FORMAT_CHARS.contains(c) && !escaped {
            if !lit.is_empty() {
                items.push(Piece::Lit(unescape(std::mem::take(&mut lit))));
            }
            let piece = match c {
                'Y' => Piece::Year,
                'y' => Piece::Year2,
                'o' => Piece::IsoYear,
                'm' => Piece::Month02,
                'n' => Piece::Month,
                'd' => Piece::Day02,
                'j' => Piece::Day,
                'H' => Piece::Hour24_02,
                'G' => Piece::Hour24,
                'h' => Piece::Hour12_02,
                'g' => Piece::Hour12,
                'i' => Piece::Minute02,
                's' => Piece::Second02,
                'u' => Piece::Micro06,
                'z' => Piece::DayOfYear,
                'w' => Piece::WeekdaySun,
                'W' => Piece::IsoWeek,
                'L' => Piece::LeapYear,
                't' => Piece::DaysInMonth,
                'S' => Piece::Ordinal,
                'f' => Piece::TwelveHour,
                'c' => Piece::Iso,
                'a' | 'A' | 'P' | 'D' | 'l' | 'F' | 'b' | 'M' | 'E' | 'N' => {
                    match translated_piece(py, c)? {
                        Some(p) => p,
                        None => return Ok(None),
                    }
                }
                _ => return Ok(None),
            };
            items.push(piece);
            i += 1;
            continue;
        }
        lit.push(c);
        i += 1;
    }
    if !lit.is_empty() {
        items.push(Piece::Lit(unescape(lit)));
    }
    Ok(Some(items))
}

/// Translated pieces: `gettext` calls and table snapshots, done once per
/// program (programs are keyed by the active language).
fn translated_piece(py: Python<'_>, c: char) -> PyResult<Option<Piece>> {
    let gettext = utils(py)?.gettext.bind(py);
    let g = |s: &str| -> PyResult<String> { Ok(gettext.call1((s,))?.to_string()) };
    Ok(Some(match c {
        'a' => Piece::AmPm([g("a.m.")?, g("p.m.")?]),
        'A' => Piece::AmPmCap([g("AM")?, g("PM")?]),
        'P' => Piece::Pm([g("midnight")?, g("noon")?, g("a.m.")?, g("p.m.")?]),
        _ => {
            let dates = py.import("django.utils.dates")?;
            let weekdays: [u32; 7] = [0, 1, 2, 3, 4, 5, 6];
            let months: [u32; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
            match c {
                'D' => Piece::DayAbbr(arr(load_table(
                    &dates.getattr("WEEKDAYS_ABBR")?,
                    &weekdays,
                )?)?),
                'l' => Piece::DayLong(arr(load_table(
                    &dates.getattr("WEEKDAYS")?,
                    &weekdays,
                )?)?),
                'F' => Piece::MonthFull(arr(load_table(
                    &dates.getattr("MONTHS")?,
                    &months,
                )?)?),
                'b' => Piece::Month3(arr(load_table(
                    &dates.getattr("MONTHS_3")?,
                    &months,
                )?)?),
                'M' => {
                    let t = load_table(&dates.getattr("MONTHS_3")?, &months)?
                        .into_iter()
                        .map(|s| title_case(&s))
                        .collect();
                    Piece::Month3Title(arr(t)?)
                }
                'E' => Piece::MonthAlt(arr(load_table(
                    &dates.getattr("MONTHS_ALT")?,
                    &months,
                )?)?),
                'N' => Piece::MonthAp(arr(load_table(
                    &dates.getattr("MONTHS_AP")?,
                    &months,
                )?)?),
                _ => return Ok(None),
            }
        }
    }))
}

/// `re_escaped.sub(r"\1", s)`: drop a backslash before any non-newline char
/// (a trailing backslash and a backslash before a newline are kept).
fn unescape(s: String) -> String {
    if !s.contains('\\') {
        return s;
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' && it.peek().is_some_and(|n| *n != '\n') {
            out.push(it.next().unwrap_or('\0'));
        } else {
            out.push(c);
        }
    }
    out
}

fn render_program(items: &[Piece], f: &Fields) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for p in items {
        match p {
            Piece::Lit(s) => out.push_str(s),
            Piece::Year => {
                let _ = write!(out, "{:04}", f.y);
            }
            Piece::Year2 => {
                let _ = write!(out, "{:02}", f.y.rem_euclid(100));
            }
            Piece::IsoYear => {
                let (y, _) = iso_year_week(f.y, f.m, f.d);
                let _ = write!(out, "{y}");
            }
            Piece::Month02 => {
                let _ = write!(out, "{:02}", f.m);
            }
            Piece::Month => {
                let _ = write!(out, "{}", f.m);
            }
            Piece::Day02 => {
                let _ = write!(out, "{:02}", f.d);
            }
            Piece::Day => {
                let _ = write!(out, "{}", f.d);
            }
            Piece::Hour24_02 => {
                let _ = write!(out, "{:02}", f.h);
            }
            Piece::Hour24 => {
                let _ = write!(out, "{}", f.h);
            }
            Piece::Hour12_02 => {
                let h = f.h % 12;
                let _ = write!(out, "{:02}", if h == 0 { 12 } else { h });
            }
            Piece::Hour12 => {
                let h = f.h % 12;
                let _ = write!(out, "{}", if h == 0 { 12 } else { h });
            }
            Piece::Minute02 => {
                let _ = write!(out, "{:02}", f.mi);
            }
            Piece::Second02 => {
                let _ = write!(out, "{:02}", f.s);
            }
            Piece::Micro06 => {
                let _ = write!(out, "{:06}", f.us);
            }
            Piece::DayOfYear => {
                let _ = write!(out, "{}", day_of_year(f.y, f.m, f.d));
            }
            Piece::WeekdaySun => {
                let _ = write!(out, "{}", (f.wd + 1) % 7);
            }
            Piece::IsoWeek => {
                let (_, w) = iso_year_week(f.y, f.m, f.d);
                let _ = write!(out, "{w}");
            }
            Piece::LeapYear => out.push_str(if is_leap(f.y) { "True" } else { "False" }),
            Piece::DaysInMonth => {
                let _ = write!(out, "{}", days_in_month(f.y, f.m));
            }
            Piece::Ordinal => out.push_str(ordinal(f.d)),
            Piece::AmPm(s) | Piece::AmPmCap(s) => out.push_str(&s[usize::from(f.h > 11)]),
            Piece::Pm(s) => {
                let ampm = &s[2 + usize::from(f.h > 11)];
                let h = f.h % 12;
                let hh = if h == 0 { 12 } else { h };
                if f.mi == 0 && f.h == 0 {
                    out.push_str(&s[0]);
                } else if f.mi == 0 && f.h == 12 {
                    out.push_str(&s[1]);
                } else if f.mi != 0 {
                    let _ = write!(out, "{hh}:{:02} {ampm}", f.mi);
                } else {
                    let _ = write!(out, "{hh} {ampm}");
                }
            }
            Piece::TwelveHour => {
                let h = f.h % 12;
                let hh = if h == 0 { 12 } else { h };
                if f.mi != 0 {
                    let _ = write!(out, "{hh}:{:02}", f.mi);
                } else {
                    let _ = write!(out, "{hh}");
                }
            }
            Piece::Iso => out.push_str(f.iso.as_deref().unwrap_or_default()),
            Piece::DayAbbr(t) | Piece::DayLong(t) => out.push_str(&t[f.wd as usize]),
            Piece::MonthFull(t)
            | Piece::Month3(t)
            | Piece::Month3Title(t)
            | Piece::MonthAlt(t)
            | Piece::MonthAp(t) => out.push_str(&t[f.m as usize - 1]),
        }
    }
    out
}

/// Program cache, invalidated when the language (or `USE_I18N`) changes.
/// Entries are always for the current `(lang, i18n)` (the whole map is
/// cleared when either changes), so the format string alone keys the map —
/// no key allocation on a hit.
struct Cache {
    lang: String,
    real: u8,
    programs: HashMap<String, Vec<Piece>>,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache {
        lang: String::new(),
        real: 0,
        programs: HashMap::new(),
    });
}


/// True if any unescaped `DATE_ONLY_CHARS` specifier occurs (same `(?<!\\)`
/// lookbehind as stock `re_formatchars`).
fn has_date_only_specifier(fmt: &str) -> bool {
    let mut prev_backslash = false;
    for c in fmt.chars() {
        if DATE_ONLY_CHARS.contains(&c) && !prev_backslash {
            return true;
        }
        prev_backslash = c == '\\';
    }
    false
}
/// Time specifiers: stock `DateFormat` on a plain `date` reads
/// `data.hour`/… for these and raises `AttributeError`.
const TIME_CHARS: [char; 11] = ['a', 'A', 'f', 'g', 'G', 'h', 'H', 'i', 'P', 's', 'u'];

/// Native render of `value` with resolved custom format `fmt`; `Ok(None)`
/// means "use the stock Python path".
fn try_native(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
    fmt: &str,
    date_only_ok: bool,
) -> PyResult<Option<String>> {
    // Exact datetime.datetime or stock `tz.datetimeobject` only (cast
    // would also accept arbitrary subclasses).
    if !crate::filters::is_stock_datetime(value) {
        return Ok(None);
    }
    let dt = value.cast::<PyDateTime>()?;
    if fmt.contains(FALLBACK_CHARS) {
        return Ok(None);
    }
    // Stock `time` filter builds a `TimeFormat`: date-only specifiers
    // (`Y`, `F`, …) raise AttributeError there, and the filter swallows it
    // into `""` — the whole output, regardless of preceding pieces. Only
    // unescaped format chars count (stock `(?<!\\)` lookbehind).
    if !date_only_ok && has_date_only_specifier(fmt) {
        return Ok(Some(String::new()));
    }
    let fields = Fields {
        y: dt.get_year(),
        m: u32::from(dt.get_month()),
        d: u32::from(dt.get_day()),
        h: u32::from(dt.get_hour()),
        mi: u32::from(dt.get_minute()),
        s: u32::from(dt.get_second()),
        us: dt.get_microsecond(),
        wd: weekday_mon(dt.get_year(), u32::from(dt.get_month()), u32::from(dt.get_day())),
        iso: iso_for(value, fmt)?,
    };
    run_program(py, fmt, &fields)
}

/// `dateformat.format(value, fmt)` for an EXACT `datetime.datetime` (or
/// stock `tz.datetimeobject`) or `datetime.date` (`localize`'s `date_format` step); `Ok(None)` → stock
/// Python path (timezone/isoformat specifiers, time specifiers on a plain
/// date, other types).
pub(crate) fn format_exact(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
    fmt: &str,
) -> PyResult<Option<String>> {
    if crate::filters::is_stock_datetime(value) {
        return try_native(py, value, fmt, true);
    }
    if value.get_type().as_ptr() != py.get_type::<PyDate>().as_ptr() {
        return Ok(None);
    }
    if fmt.contains(FALLBACK_CHARS) || fmt.contains(TIME_CHARS) {
        return Ok(None);
    }
    let d = value.cast::<PyDate>()?;
    let (y, m, dd) = (d.get_year(), u32::from(d.get_month()), u32::from(d.get_day()));
    let fields = Fields {
        y,
        m,
        d: dd,
        h: 0,
        mi: 0,
        s: 0,
        us: 0,
        wd: weekday_mon(y, m, dd),
        iso: iso_for(value, fmt)?,
    };
    run_program(py, fmt, &fields)
}

/// Render `fmt` against `fields` through the per-language program cache.
fn run_program(py: Python<'_>, fmt: &str, fields: &Fields) -> PyResult<Option<String>> {
    let Some(state) = trans_state(py) else {
        return Ok(None);
    };
    let (lang, real) = (state.lang, state.real);
    // Cache hit fast path: rendering is pure Rust (no Python re-entry), so
    // the borrow may be held.
    let hit = CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.real != real || c.lang != lang {
            c.real = real;
            c.lang.clone_from(&lang);
            c.programs.clear();
        }
        c.programs.get(fmt).map(|items| render_program(items, fields))
    });
    if hit.is_some() {
        return Ok(hit);
    }
    let items = match compile_format(py, fmt) {
        Ok(Some(items)) => items,
        Ok(None) => return Ok(None),
        Err(e) => {
            // Translation machinery failed mid-build (e.g. gettext not
            // initialized); stock raises or handles it — propagate so the
            // caller's fallback sees identical errors.
            e.restore(py);
            return Ok(None);
        }
    };
    let out = render_program(&items, fields);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.programs.insert(fmt.to_string(), items);
    });
    Ok(Some(out))
}

/// Resolve the `date`/`time` filter arg to the format string stock
/// `get_format(arg or default_name)` returns. A custom string (not in
/// `FORMAT_SETTINGS`) comes back verbatim (stock `getattr` on the locale
/// format modules misses). An empty/None arg or a `FORMAT_SETTINGS` name is
/// read through the live `_format_cache` (`filters::get_format`), keyed by
/// the active language like stock. `Ok(None)` → Python path (non-string
/// arg, non-string format value, language not readable natively).
fn resolve_format(
    py: Python<'_>,
    arg: &Bound<'_, PyAny>,
    default_name: &str,
) -> PyResult<Option<String>> {
    let name: String = if arg.is_none() {
        default_name.to_string()
    } else {
        // Literal args are SafeStrings (a `str` subclass).
        let Ok(s) = arg.cast::<PyString>() else {
            return Ok(None);
        };
        let fmt: String = s.extract()?;
        if fmt.is_empty() {
            default_name.to_string()
        } else if FORMAT_SETTINGS.contains(&fmt.as_str()) {
            fmt
        } else {
            return Ok(Some(fmt));
        }
    };
    let Some(lang) = super::get_language(py) else {
        return Ok(None);
    };
    let v = super::get_format(py, &name, lang.as_deref(), true)?;
    let Ok(v) = v.cast_exact::<PyString>() else {
        return Ok(None);
    };
    Ok(Some(v.extract()?))
}

/// `date(value, arg=None)`: `formats.date_format` → builtin `format`
/// (`django.utils.dateformat.format`) → `""`.
pub(crate) fn date<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    if none_or_empty(input)? {
        return Ok(PyString::new(py, "").into_any());
    }
    let arg = args
        .first()
        .cloned()
        .unwrap_or_else(|| py.None().into_bound(py));
    if let Some(fmt) = resolve_format(py, &arg, "DATE_FORMAT")? {
        // Exact `datetime` and `datetime.date` (`date_format` →
        // `dateformat.format` on either is the same program run).
        if let Some(s) = format_exact(py, input, &fmt)? {
            return Ok(PyString::new(py, &s).into_any());
        }
    }
    let u = utils(py)?;
    match u.date_format.bind(py).call1((input, &arg)) {
        Ok(v) => Ok(v),
        Err(e) if is_ae(py, &e) => match u.df_format.bind(py).call1((input, &arg)) {
            Ok(v) => Ok(v),
            Err(e2) if is_ae(py, &e2) => Ok(PyString::new(py, "").into_any()),
            Err(e2) => Err(e2),
        },
        Err(e) => Err(e),
    }
}

/// `time(value, arg=None)`: `formats.time_format` →
/// `django.utils.dateformat.time_format` → `""`, catching
/// `(AttributeError, TypeError)`.
pub(crate) fn time<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    if none_or_empty(input)? {
        return Ok(PyString::new(py, "").into_any());
    }
    let arg = args
        .first()
        .cloned()
        .unwrap_or_else(|| py.None().into_bound(py));
    if let Some(fmt) = resolve_format(py, &arg, "TIME_FORMAT")? {
        if let Some(s) = try_native(py, input, &fmt, false)? {
            return Ok(PyString::new(py, &s).into_any());
        }
    }
    let u = utils(py)?;
    match u.time_format.bind(py).call1((input, &arg)) {
        Ok(v) => Ok(v),
        Err(e) if is_ae(py, &e) || is_te(py, &e) => {
            match u.df_time_format.bind(py).call1((input, &arg)) {
                Ok(v) => Ok(v),
                Err(e2) if is_ae(py, &e2) || is_te(py, &e2) => {
                    Ok(PyString::new(py, "").into_any())
                }
                Err(e2) => Err(e2),
            }
        }
        Err(e) => Err(e),
    }
}

/// `timesince(value, arg=None)`: falsy → `""`; `arg` truthy →
/// `timesince(value, arg)` else `timesince(value)`; `(ValueError,
/// TypeError)` → `""`.
pub(crate) fn timesince_filter<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    if !input.is_truthy()? {
        return Ok(PyString::new(py, "").into_any());
    }
    let u = utils(py)?;
    let f = u.timesince.bind(py);
    let res = match args.first() {
        Some(a) if a.is_truthy()? => f.call1((input, a)),
        _ => f.call1((input,)),
    };
    match res {
        Ok(v) => Ok(v),
        Err(e) if is_ve(py, &e) || is_te(py, &e) => Ok(PyString::new(py, "").into_any()),
        Err(e) => Err(e),
    }
}

/// `timeuntil(value, arg=None)`: like `timesince` but `timeuntil(value,
/// arg)` always with the (possibly None) arg.
pub(crate) fn timeuntil_filter<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    if !input.is_truthy()? {
        return Ok(PyString::new(py, "").into_any());
    }
    let arg = args
        .first()
        .cloned()
        .unwrap_or_else(|| py.None().into_bound(py));
    match utils(py)?.timeuntil.bind(py).call1((input, &arg)) {
        Ok(v) => Ok(v),
        Err(e) if is_ve(py, &e) || is_te(py, &e) => Ok(PyString::new(py, "").into_any()),
        Err(e) => Err(e),
    }
}
