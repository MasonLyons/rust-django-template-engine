//! Native ports of `django.contrib.humanize` filters (installed only when the
//! registered function is the stock one; see `filters::native_for`).

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyInt, PyList, PyString, PyTuple};

use crate::render::context::Rctx;

/// The stock Python `intcomma` (fallback for everything but plain ints).
fn py_intcomma(py: Python<'_>) -> PyResult<&Bound<'_, PyAny>> {
    static F: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    Ok(F.get_or_try_init(py, || -> PyResult<Py<PyAny>> {
        Ok(py
            .import("django.contrib.humanize.templatetags.humanize")?
            .getattr("intcomma")?
            .unbind())
    })?
    .bind(py))
}

/// `intcomma(value)` for an exact `int` that fits `i64` (5.2: `int(value)`,
/// 6.1: `Decimal(value)`; both reach `number_format(..., use_l10n=True,
/// force_grouping=True)` with `str_number == str(value)`): the locale's
/// `NUMBER_GROUPING` / `THOUSAND_SEPARATOR` applied exactly like
/// `numberformat.format`. Floats, Decimals, strings, an explicit
/// `use_l10n` arg, `NUMBER_GROUPING == 0` or an unusual grouping value take
/// the Python function.
pub(crate) fn intcomma<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    if args.is_empty() && input.is_exact_instance_of::<PyInt>() {
        if let Ok(n) = input.extract::<i64>() {
            if let Some(s) = native_intcomma(py, n)? {
                return Ok(PyString::new(py, &s).into_any());
            }
        }
    }
    py_intcomma(py)?.call1((input,).into_pyobject(py)?.add_args(args)?)
}

trait AddArgs<'py> {
    fn add_args(self, args: &[Bound<'py, PyAny>]) -> PyResult<Bound<'py, PyTuple>>;
}

impl<'py> AddArgs<'py> for Bound<'py, PyTuple> {
    fn add_args(self, args: &[Bound<'py, PyAny>]) -> PyResult<Bound<'py, PyTuple>> {
        if args.is_empty() {
            return Ok(self);
        }
        let mut v: Vec<Bound<'py, PyAny>> = self.iter().collect();
        v.extend(args.iter().cloned());
        PyTuple::new(self.py(), v)
    }
}

fn native_intcomma(py: Python<'_>, n: i64) -> PyResult<Option<String>> {
    let Some(lang) = super::get_language(py) else {
        return Ok(None);
    };
    let grouping = super::get_format(py, "NUMBER_GROUPING", lang.as_deref(), true)?;
    // `intervals = list(grouping)` for a sequence, else `[grouping, 0]`.
    let mut intervals: Vec<i64> = if let Ok(g) = grouping.cast_exact::<PyInt>() {
        let g: i64 = g.extract()?;
        if g == 0 {
            return Ok(None);
        }
        vec![g, 0]
    } else if grouping.is_exact_instance_of::<PyTuple>()
        || grouping.is_exact_instance_of::<PyList>()
    {
        let mut v = Vec::new();
        for item in grouping.try_iter()? {
            let item = item?;
            let Ok(i) = item.cast_exact::<PyInt>() else {
                return Ok(None);
            };
            v.push(i.extract()?);
        }
        // `grouping != 0` holds for any sequence.
        if v.is_empty() {
            return Ok(None);
        }
        v
    } else {
        return Ok(None);
    };
    let sep = super::get_format(py, "THOUSAND_SEPARATOR", lang.as_deref(), true)?;
    let Ok(sep) = sep.cast_exact::<PyString>() else {
        return Ok(None);
    };
    let sep_rev: String = sep.to_str()?.chars().rev().collect();
    let digits = n.unsigned_abs().to_string();
    let mut active = intervals.remove(0);
    let mut gd = String::with_capacity(digits.len() * 2);
    let mut cnt: i64 = 0;
    for d in digits.chars().rev() {
        if cnt != 0 && cnt == active {
            if !intervals.is_empty() {
                let next = intervals.remove(0);
                if next != 0 {
                    active = next;
                }
            }
            gd.push_str(&sep_rev);
            cnt = 0;
        }
        gd.push(d);
        cnt += 1;
    }
    let mut out = String::with_capacity(gd.len() + 1);
    if n < 0 {
        out.push('-');
    }
    out.extend(gd.chars().rev());
    Ok(Some(out))
}

/// `(context, singular, plural)` of an `ngettext_lazy` / `npgettext_lazy`
/// proxy (the msgids carry U+00A0 and may change between releases, so they
/// are read from the stock objects rather than restated here).
type NMsg = (Option<String>, String, String);

/// Msgids of `NaturalTimeFormatter`'s lazy strings, index 0 = past,
/// 1 = future.
struct NtMsgs {
    /// `"%(delta)s ago"` / `"%(delta)s from now"` (`gettext_lazy`).
    day: [String; 2],
    /// `past-second` … `past-hour` / `future-*` (`ngettext_lazy`, number
    /// key `"count"`).
    second: [NMsg; 2],
    minute: [NMsg; 2],
    hour: [NMsg; 2],
    /// `past_substrings` / `future_substrings` in `TIME_STRINGS_KEYS` order
    /// (`npgettext_lazy`, number key `"num"`).
    units: [Vec<NMsg>; 2],
}

/// Stock objects the native `naturaltime` checks against or calls.
struct NaturalTime {
    /// `django.contrib.humanize.templatetags.humanize` globals (the live
    /// `datetime` class, patched by the humanize tests, is read per call).
    globals: Py<pyo3::types::PyDict>,
    /// `NaturalTimeFormatter` as imported (a replaced class → Python).
    formatter: Py<PyAny>,
    /// `NaturalTimeFormatter.string_for` (patched → Python).
    string_for: Py<PyAny>,
    /// `NaturalTimeFormatter.time_strings` (its `"now"` entry is returned
    /// as is, a lazy string, like stock).
    time_strings: Py<PyAny>,
    /// `django.template.defaultfilters` globals: humanize calls
    /// `defaultfilters.timesince` / `.timeuntil`.
    defaultfilters: Py<pyo3::types::PyDict>,
    timesince: Py<PyAny>,
    timeuntil: Py<PyAny>,
    utc: Py<PyAny>,
    /// `None` when the lazy strings don't have the expected shape
    /// (native path disabled).
    msgs: Option<NtMsgs>,
}

const TIME_STRINGS_KEYS: [&str; 6] = ["year", "month", "week", "day", "hour", "minute"];

fn nt_msgs(formatter: &Bound<'_, PyAny>) -> Option<NtMsgs> {
    let ts = formatter.getattr("time_strings").ok()?;
    let kw = |p: Bound<'_, PyAny>| -> Option<NMsg> {
        let kw = p.getattr("_kw").ok()?.cast_into::<pyo3::types::PyDict>().ok()?;
        let get = |k: &str| -> Option<String> { kw.get_item(k).ok()??.extract().ok() };
        let n = kw.len();
        let ctx = get("context");
        if n != 2 + usize::from(ctx.is_some()) {
            return None;
        }
        Some((ctx, get("singular")?, get("plural")?))
    };
    let nmsg = |k: &str| -> Option<NMsg> {
        let m = kw(ts.get_item(k).ok()?)?;
        m.0.is_none().then_some(m)
    };
    let gmsg = |k: &str| -> Option<String> {
        let p = ts.get_item(k).ok()?;
        let args = p.getattr("_args").ok()?.cast_into::<PyTuple>().ok()?;
        let kw = p.getattr("_kw").ok()?.cast_into::<pyo3::types::PyDict>().ok()?;
        if args.len() != 1 || !kw.is_empty() {
            return None;
        }
        args.get_item(0).ok()?.extract().ok()
    };
    let units = |attr: &str| -> Option<Vec<NMsg>> {
        let d = formatter.getattr(attr).ok()?;
        TIME_STRINGS_KEYS
            .iter()
            .map(|k| kw(d.get_item(k).ok()?).filter(|m| m.0.is_some()))
            .collect()
    };
    Some(NtMsgs {
        day: [gmsg("past-day")?, gmsg("future-day")?],
        second: [nmsg("past-second")?, nmsg("future-second")?],
        minute: [nmsg("past-minute")?, nmsg("future-minute")?],
        hour: [nmsg("past-hour")?, nmsg("future-hour")?],
        units: [units("past_substrings")?, units("future_substrings")?],
    })
}

fn natural_time(py: Python<'_>) -> PyResult<&NaturalTime> {
    static NT: PyOnceLock<NaturalTime> = PyOnceLock::new();
    NT.get_or_try_init(py, || -> PyResult<NaturalTime> {
        let m = py.import("django.contrib.humanize.templatetags.humanize")?;
        let formatter = m.getattr("NaturalTimeFormatter")?;
        let ts = py.import("django.utils.timesince")?;
        Ok(NaturalTime {
            globals: m.dict().unbind(),
            string_for: formatter.getattr("string_for")?.unbind(),
            time_strings: formatter.getattr("time_strings")?.unbind(),
            msgs: nt_msgs(&formatter),
            formatter: formatter.unbind(),
            defaultfilters: py.import("django.template.defaultfilters")?.dict().unbind(),
            timesince: ts.getattr("timesince")?.unbind(),
            timeuntil: ts.getattr("timeuntil")?.unbind(),
            utc: py.import("datetime")?.getattr("timezone")?.getattr("utc")?.unbind(),
        })
    })
}
/// The stock Python `naturaltime`.
fn py_naturaltime(py: Python<'_>) -> PyResult<&Bound<'_, PyAny>> {
    static F: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    Ok(F.get_or_try_init(py, || -> PyResult<Py<PyAny>> {
        Ok(py
            .import("django.contrib.humanize.templatetags.humanize")?
            .getattr("naturaltime")?
            .unbind())
    })?
    .bind(py))
}

/// `naturaltime(value)` (`NaturalTimeFormatter.string_for`) for an exact
/// `datetime.datetime`: the branch logic and `timesince` run natively, the
/// datetime arithmetic and comparisons through the real (possibly mocked)
/// `datetime` objects, and the lazy `ngettext_lazy`/`npgettext_lazy`
/// strings become direct catalog lookups (`filters::ngettext` & co.) with
/// the stock msgids, identical on 5.2 and 6.1. Anything else (non-datetime
/// values, subclasses, a patched formatter or `timesince`) takes the
/// Python function.
pub(crate) fn naturaltime<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    if args.is_empty() && input.is_exact_instance_of::<pyo3::types::PyDateTime>() {
        if let Some(r) = native_naturaltime(py, input)? {
            return Ok(r);
        }
    }
    py_naturaltime(py)?.call1((input,).into_pyobject(py)?.add_args(args)?)
}

/// `django.utils.timezone.is_aware`.
fn is_aware(v: &Bound<'_, PyAny>) -> PyResult<bool> {
    Ok(!v.call_method0(pyo3::intern!(v.py(), "utcoffset"))?.is_none())
}

/// `ngettext_lazy(singular, plural, key) % {key: n}` /
/// `npgettext_lazy(...)` (`NumberAwareString.__mod__`): translate for `n`,
/// then `%`-format, keeping the translation when it has no placeholder
/// (`TypeError`).
fn n_format<'py>(
    tr: &super::Translator<'py>,
    msg: &NMsg,
    key: &str,
    n: i64,
) -> PyResult<Bound<'py, PyAny>> {
    let py = tr.py();
    let num = PyInt::new(py, n).into_any();
    let (context, singular, plural) = msg;
    let translated = match context {
        Some(c) => tr.npgettext(c, singular, plural, &num)?,
        None => tr.ngettext(singular, plural, &num)?,
    };
    let rhs = pyo3::types::PyDict::new(py);
    rhs.set_item(key, &num)?;
    match translated.rem(&rhs) {
        Ok(r) => Ok(r),
        Err(e) if super::is_te(py, &e) => Ok(translated),
        Err(e) => Err(e),
    }
}

/// `gettext_lazy(message) % {key: value}`.
fn g_format<'py>(
    tr: &super::Translator<'py>,
    message: &str,
    key: &str,
    value: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let rhs = pyo3::types::PyDict::new(tr.py());
    rhs.set_item(key, value)?;
    tr.gettext(message)?.rem(&rhs)
}

/// Python `float.__floordiv__` (the quotient half of `_float_div_mod`).
fn py_float_floordiv(vx: f64, wx: f64) -> f64 {
    let m = vx % wx;
    let mut div = (vx - m) / wx;
    if m != 0.0 && (wx < 0.0) != (m < 0.0) {
        div -= 1.0;
    }
    if div == 0.0 {
        return 0.0_f64.copysign(vx / wx);
    }
    let mut fd = div.floor();
    if div - fd > 0.5 {
        fd += 1.0;
    }
    fd
}

fn native_naturaltime<'py>(
    py: Python<'py>,
    value: &Bound<'py, PyAny>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    use pyo3::types::{PyDeltaAccess, PyTzInfoAccess};
    let nt = natural_time(py)?;
    let globals = nt.globals.bind(py);
    let Some(formatter) = globals.get_item("NaturalTimeFormatter")? else {
        return Ok(None);
    };
    if !formatter.is(nt.formatter.bind(py))
        || !formatter.getattr("string_for")?.eq(nt.string_for.bind(py))?
    {
        return Ok(None);
    }
    let df = nt.defaultfilters.bind(py);
    let same = |name: &str, f: &Py<PyAny>| -> PyResult<bool> {
        Ok(df.get_item(name)?.is_some_and(|g| g.is(f.bind(py))))
    };
    if !same("timesince", &nt.timesince)? || !same("timeuntil", &nt.timeuntil)? {
        return Ok(None);
    }
    let Some(dt_cls) = globals.get_item("datetime")? else {
        return Ok(None);
    };
    let dt = value.cast::<pyo3::types::PyDateTime>()?;
    // `is_aware(value)`; tzinfo None ⇒ utcoffset() None.
    let aware = dt.get_tzinfo().is_some() && is_aware(value)?;
    let tz = if aware { nt.utc.bind(py).clone() } else { py.None().into_bound(py) };
    let now = dt_cls.call_method1(pyo3::intern!(py, "now"), (tz,))?;
    let past = value.lt(&now)?;
    let delta = if past { now.sub(value)? } else { value.sub(&now)? };
    let delta = delta.cast_into::<pyo3::types::PyDelta>()?;
    let (days, seconds) = (delta.get_days(), i64::from(delta.get_seconds()));
    let Some(msgs) = nt.msgs.as_ref() else {
        return Ok(None);
    };
    let side = usize::from(!past);
    let tr = super::Translator::new(py);
    if days != 0 {
        let joined = timesince(&tr, value, &now, !past, &msgs.units[side])?;
        return g_format(&tr, &msgs.day[side], "delta", &joined).map(Some);
    }
    if seconds == 0 {
        return nt.time_strings.bind(py).get_item("now").map(Some);
    }
    let (msg, count) = if seconds < 60 {
        (&msgs.second[side], seconds)
    } else if seconds / 60 < 60 {
        (&msgs.minute[side], seconds / 60)
    } else {
        (&msgs.hour[side], seconds / 60 / 60)
    };
    n_format(&tr, msg, "count", count).map(Some)
}

/// `django.utils.timesince.timesince(d, now, reversed, time_strings)` with
/// the default `depth=2`; `units` are the `time_strings` msgids in
/// `TIME_STRINGS_KEYS` order, `d` is an exact datetime and `now` a
/// (possibly mocked) datetime.
fn timesince<'py>(
    tr: &super::Translator<'py>,
    d: &Bound<'py, PyAny>,
    now: &Bound<'py, PyAny>,
    reversed: bool,
    units: &[NMsg],
) -> PyResult<Bound<'py, PyAny>> {
    use pyo3::types::{PyDateAccess, PyDateTime, PyDelta, PyDeltaAccess, PyTimeAccess, PyTzInfoAccess};
    const CHUNKS: [f64; 4] = [604_800.0, 86_400.0, 3_600.0, 60.0];
    const MONTHS_DAYS: [u8; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let py = tr.py();
    let unit = |i: usize, n: i64| -> PyResult<String> {
        let s = n_format(tr, &units[i], "num", n)?;
        // `avoid_wrapping`.
        Ok(s.str()?.to_str()?.replace(' ', "\u{a0}"))
    };
    let mut now = now.clone();
    if is_aware(&now)? && is_aware(d)? {
        let tzinfo = d.cast::<PyDateTime>()?.get_tzinfo();
        now = now.call_method1(pyo3::intern!(py, "astimezone"), (tzinfo,))?;
    }
    let (d, now) = if reversed { (now, d.clone()) } else { (d.clone(), now) };
    let delta = now.sub(&d)?.cast_into::<PyDelta>()?;
    let since = i64::from(delta.get_days()) * 86_400 + i64::from(delta.get_seconds());
    if since <= 0 {
        return Ok(PyString::new(py, &unit(5, 0)?).into_any());
    }
    let dd = d.cast::<PyDateTime>()?;
    let nd = now.cast::<PyDateTime>()?;
    let mut total_months = (i64::from(nd.get_year()) - i64::from(dd.get_year())) * 12
        + (i64::from(nd.get_month()) - i64::from(dd.get_month()));
    let dtime = (dd.get_hour(), dd.get_minute(), dd.get_second(), dd.get_microsecond());
    let ntime = (nd.get_hour(), nd.get_minute(), nd.get_second(), nd.get_microsecond());
    if dd.get_day() > nd.get_day() || (dd.get_day() == nd.get_day() && dtime > ntime) {
        total_months -= 1;
    }
    let (years, months) = (total_months.div_euclid(12), total_months.rem_euclid(12));
    let pivot = if years != 0 || months != 0 {
        let mut pivot_year = i64::from(dd.get_year()) + years;
        let mut pivot_month = i64::from(dd.get_month()) + months;
        if pivot_month > 12 {
            pivot_month -= 12;
            pivot_year += 1;
        }
        let (Ok(py_year), Ok(py_month)) = (i32::try_from(pivot_year), u8::try_from(pivot_month))
        else {
            return Err(pyo3::exceptions::PyValueError::new_err("year is out of range"));
        };
        let day = MONTHS_DAYS[usize::from(py_month) - 1].min(dd.get_day());
        PyDateTime::new(
            py,
            py_year,
            py_month,
            day,
            dd.get_hour(),
            dd.get_minute(),
            dd.get_second(),
            0,
            dd.get_tzinfo().as_ref(),
        )?
        .into_any()
    } else {
        d.clone()
    };
    let mut remaining: f64 = now
        .sub(&pivot)?
        .call_method0(pyo3::intern!(py, "total_seconds"))?
        .extract()?;
    let mut partials: [i64; 6] = [years, months, 0, 0, 0, 0];
    for (i, chunk) in CHUNKS.iter().enumerate() {
        let q = py_float_floordiv(remaining, *chunk);
        // `int(q)`: truncation of an integral float.
        #[allow(clippy::cast_possible_truncation)]
        let count = q as i64;
        partials[i + 2] = count;
        remaining -= chunk * q;
    }
    let Some(mut i) = partials.iter().position(|&v| v != 0) else {
        return Ok(PyString::new(py, &unit(5, 0)?).into_any());
    };
    let mut parts: Vec<String> = Vec::with_capacity(2);
    while i < TIME_STRINGS_KEYS.len() && parts.len() < 2 {
        let v = partials[i];
        if v == 0 {
            break;
        }
        parts.push(unit(i, v)?);
        i += 1;
    }
    let sep = tr.gettext(", ")?;
    sep.call_method1(pyo3::intern!(py, "join"), (parts,))
}

/// The stock Python `ordinal` (fallback for everything but plain ints).
fn py_ordinal(py: Python<'_>) -> PyResult<&Bound<'_, PyAny>> {
    static F: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    Ok(F.get_or_try_init(py, || -> PyResult<Py<PyAny>> {
        Ok(py
            .import("django.contrib.humanize.templatetags.humanize")?
            .getattr("ordinal")?
            .unbind())
    })?
    .bind(py))
}

/// `True` while the humanize module's `pgettext` / `mark_safe` globals are
/// the stock functions (`ordinal` resolves both at call time).
fn ordinal_globals_stock(py: Python<'_>) -> PyResult<bool> {
    static G: PyOnceLock<(Py<pyo3::types::PyDict>, Py<PyAny>, Py<PyAny>)> = PyOnceLock::new();
    let (globals, pgettext, mark_safe) = G.get_or_try_init(py, || -> PyResult<_> {
        Ok((
            py.import("django.contrib.humanize.templatetags.humanize")?
                .dict()
                .unbind(),
            py.import("django.utils.translation")?.getattr("pgettext")?.unbind(),
            py.import("django.utils.safestring")?.getattr("mark_safe")?.unbind(),
        ))
    })?;
    let globals = globals.bind(py);
    let same = |name: &str, f: &Py<PyAny>| -> PyResult<bool> {
        Ok(globals.get_item(name)?.is_some_and(|g| g.is(f.bind(py))))
    };
    Ok(same("pgettext", pgettext)? && same("mark_safe", mark_safe)?)
}

/// `(context, msgid)` of the one ordinal template stock picks for `n >= 0`
/// (it translates all ten `n % 10` templates, then indexes one: the other
/// nine lookups have no visible effect).
fn ordinal_msg(n: i64) -> (&'static str, &'static str) {
    const BY_LAST: [(&str, &str); 10] = [
        ("ordinal 0", "{}th"),
        ("ordinal 1", "{}st"),
        ("ordinal 2", "{}nd"),
        ("ordinal 3", "{}rd"),
        ("ordinal 4", "{}th"),
        ("ordinal 5", "{}th"),
        ("ordinal 6", "{}th"),
        ("ordinal 7", "{}th"),
        ("ordinal 8", "{}th"),
        ("ordinal 9", "{}th"),
    ];
    if n == 1 {
        ("ordinal is 1", "{}st")
    } else if matches!(n % 100, 11..=13) {
        ("ordinal 11, 12, 13", "{}th")
    } else {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // 0..=9
        BY_LAST[(n % 10) as usize]
    }
}

/// `ordinal(value)` for an exact `int` that fits `i64` (`int(value)` is
/// the identity): negatives → plain `str(value)`, else the selected
/// template translated through `Translator::pgettext`, `.format(value)`
/// (natively when the translation holds exactly one `{}` and no other
/// brace), then `mark_safe`. Other inputs or patched module globals take
/// the Python function.
pub(crate) fn ordinal<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    if args.is_empty() && input.is_exact_instance_of::<PyInt>() {
        if let Ok(n) = input.extract::<i64>() {
            if n < 0 {
                return Ok(PyString::new(py, &n.to_string()).into_any());
            }
            if ordinal_globals_stock(py)? {
                let (context, msgid) = ordinal_msg(n);
                let tmpl = super::Translator::new(py).pgettext(context, msgid)?;
                let formatted = match tmpl.cast_exact::<PyString>().ok().and_then(|s| {
                    let s = s.to_str().ok()?;
                    let (pre, post) = s.split_once("{}")?;
                    let plain = |p: &str| !p.contains(['{', '}']);
                    (plain(pre) && plain(post)).then(|| format!("{pre}{n}{post}"))
                }) {
                    Some(s) => PyString::new(py, &s).into_any(),
                    None => tmpl.call_method1(pyo3::intern!(py, "format"), (input,))?,
                };
                return crate::render::value::mark_safe_bound(&formatted);
            }
        }
    }
    py_ordinal(py)?.call1((input,).into_pyobject(py)?.add_args(args)?)
}
