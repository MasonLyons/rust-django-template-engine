//! HTML filters of `django.template.defaultfilters` (native ports).
//!
//! All escaping/URL logic delegates to the real `django.utils.html` /
//! `django.utils.encoding` functions, so `SafeData` semantics, branch
//! deltas and test mocks behave exactly as on the bridged path.

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyInt, PyList, PyString, PyTuple};

use crate::render::context::Rctx;
use crate::render::value::{helpers, is_safe_data, mark_safe_bound};

use super::utils;

fn py_int(py: Python<'_>, i: i64) -> Bound<'_, PyAny> {
    PyInt::new(py, i).into_any()
}

/// `escape`: `conditional_escape(str(value))` (marking only).
pub(crate) fn escape_filter<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let s = input.str()?;
    helpers(py)?.conditional_escape.bind(py).call1((s,))
}

/// `escapeseq`: `[conditional_escape(obj) for obj in value]` (raw items).
pub(crate) fn escapeseq<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let ce = helpers(py)?.conditional_escape.bind(py).clone();
    let mut items: Vec<Bound<'py, PyAny>> = Vec::new();
    for item in input.try_iter()? {
        items.push(ce.call1((item?,))?);
    }
    Ok(PyList::new(py, items)?.into_any())
}

/// `force_escape`: `escape(str(value))`.
pub(crate) fn force_escape<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let s = input.str()?;
    utils(py)?.escape.bind(py).call1((s,))
}

/// `escapejs`: `escapejs(str(value))` (hex-encodes per branch tables).
pub(crate) fn escapejs_filter<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let s = input.str()?;
    utils(py)?.escapejs.bind(py).call1((s,))
}

/// `json_script(value, element_id=None)`: `django.utils.html.json_script`.
pub(crate) fn json_script<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let eid = args
        .first()
        .cloned()
        .unwrap_or_else(|| py.None().into_bound(py));
    utils(py)?.json_script.bind(py).call1((input, eid))
}

/// `linebreaks`: `mark_safe(linebreaks(value, autoescape and not SafeData))`.
pub(crate) fn linebreaks_filter<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let s = input.str()?;
    let eff = ae.unwrap_or(true) && !is_safe_data(&s)?;
    let out = utils(py)?.linebreaks.bind(py).call1((s, eff))?;
    mark_safe_bound(&out)
}

/// `linebreaksbr`: normalize newlines, escape if effective autoescape,
/// `\n` → `<br>`, `mark_safe`.
pub(crate) fn linebreaksbr<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let s = input.str()?;
    let u = utils(py)?;
    let eff = ae.unwrap_or(true) && !is_safe_data(&s)?;
    let mut v = u.normalize_newlines.bind(py).call1((s,))?;
    if eff {
        v = u.escape.bind(py).call1((v,))?;
    }
    let v = v.call_method1("replace", ("\n", "<br>"))?;
    mark_safe_bound(&v)
}

/// `linenumbers`: `"%0{w}d. %s"` per line (w = digits of line count),
/// escaping each line only under effective autoescape; `mark_safe` join.
pub(crate) fn linenumbers<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let s = input.str()?;
    let safe_input = is_safe_data(&s)?;
    let raw_ae = ae.unwrap_or(true);
    let u = utils(py)?;
    let lines = s.call_method1("split", ("\n",))?;
    let n = lines.len()?;
    let width = n.to_string().len();
    let fmt = PyString::new(py, &format!("%0{width}d. %s"));
    let mut out: Vec<Bound<'py, PyAny>> = Vec::with_capacity(n);
    for (i, line) in lines.try_iter()?.enumerate() {
        let line = line?;
        let shown = if !raw_ae || safe_input {
            line
        } else {
            u.escape.bind(py).call1((line,))?
        };
        let args = PyTuple::new(py, [py_int(py, (i + 1).try_into()?), shown])?;
        out.push(fmt.call_method1("__mod__", (args,))?);
    }
    let joined = PyString::new(py, "\n").call_method1("join", (PyList::new(py, out)?,))?;
    mark_safe_bound(&joined)
}

/// `safe`: `mark_safe(str(value))`.
pub(crate) fn safe<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let s = input.str()?;
    mark_safe_bound(&s)
}

/// `safeseq`: `[mark_safe(obj) for obj in value]`.
pub(crate) fn safeseq<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let mut items: Vec<Bound<'py, PyAny>> = Vec::new();
    for item in input.try_iter()? {
        items.push(mark_safe_bound(&item?)?);
    }
    Ok(PyList::new(py, items)?.into_any())
}

/// `urlencode(value, safe=None)`: `quote(str(value))` (or `quote(value,
/// safe=arg)` when arg given and not None).
pub(crate) fn urlencode<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let s = input.str()?;
    let quote = utils(py)?.quote.bind(py).clone();
    match args.first() {
        Some(a) if !a.is_none() => {
            let kwargs = PyDict::new(py);
            kwargs.set_item("safe", a)?;
            quote.call((s,), Some(&kwargs))
        }
        _ => quote.call1((s,)),
    }
}

/// `iriencode`: `iri_to_uri(str(value))`.
pub(crate) fn iriencode<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    _ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let s = input.str()?;
    utils(py)?.iri_to_uri.bind(py).call1((s,))
}

type Walked<'py> = Vec<(Bound<'py, PyAny>, Option<Bound<'py, PyAny>>)>;

/// Port of `defaultfilters._walk_items`: pair each item with the following
/// item iff it is a list/tuple/generator child.
fn walk_items<'py>(py: Python<'py>, item_list: &Bound<'py, PyAny>) -> PyResult<Walked<'py>> {
    let gen_type = utils(py)?.types_generator.bind(py).clone();
    let mut out = Vec::new();
    let mut it = item_list.try_iter()?;
    let Some(first) = it.next() else {
        return Ok(out);
    };
    let mut item: Bound<'py, PyAny> = first?;
    loop {
        let nxt = it.next();
        let Some(res) = nxt else {
            out.push((item, None));
            break;
        };
        let next_item = res?;
        let looks_nested = next_item.is_instance_of::<PyList>()
            || next_item.is_instance_of::<PyTuple>()
            || next_item.is_instance(&gen_type)?;
        if looks_nested && next_item.try_iter().is_ok() {
            out.push((item, Some(next_item.clone())));
            match it.next() {
                Some(r) => item = r?,
                None => break,
            }
        } else {
            out.push((item, None));
            item = next_item;
        }
    }
    Ok(out)
}

/// Port of `unordered_list.list_formatter`.
fn list_formatter<'py>(
    py: Python<'py>,
    autoescape: bool,
    item_list: &Bound<'py, PyAny>,
    tabs: usize,
) -> PyResult<Bound<'py, PyAny>> {
    let indent = "\t".repeat(tabs);
    let ce = helpers(py)?.conditional_escape.bind(py).clone();
    let mut output: Vec<String> = Vec::new();
    for (item, children) in walk_items(py, item_list)? {
        let sublist = match &children {
            Some(c) if c.is_truthy()? => {
                let inner = list_formatter(py, autoescape, c, tabs + 1)?;
                format!("\n{indent}<ul>\n{}\n{indent}</ul>\n{indent}", inner.str()?)
            }
            _ => String::new(),
        };
        let esc: Bound<'py, PyAny> = if autoescape {
            ce.call1((item,))?
        } else {
            item.clone()
        };
        output.push(format!("{indent}<li>{}{}</li>", esc.str()?, sublist));
    }
    PyString::new(py, "\n").call_method1("join", (PyList::new(py, output)?,))
}

/// `unordered_list(value, autoescape=True)`; `mark_safe` result.
pub(crate) fn unordered_list<'py>(
    _rc: &mut Rctx<'py>,
    input: &Bound<'py, PyAny>,
    _args: &[Bound<'py, PyAny>],
    ae: Option<bool>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = input.py();
    let out = list_formatter(py, ae.unwrap_or(true), input, 1)?;
    mark_safe_bound(&out)
}
