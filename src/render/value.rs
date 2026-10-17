//! `Variable` / `FilterExpression` resolution and output coercion — exact
//! ports of `django.template.base.Variable.resolve/_resolve_lookup`,
//! `FilterExpression.resolve`, and `render_value_in_context`.
//!
//! Values flow as live Python objects; the live `Context` object is the root
//! of lookup chains (`current = context`), so `BaseContext` guards,
//! `Context.__getitem__` semantics, property re-raise, and callable handling
//! match stock Django exactly.

use fearless_simd::{dispatch, u8x16, Level, Simd, SimdBase, SimdMask};
use pyo3::exceptions::{
    PyAttributeError, PyIndexError, PyKeyError, PyTypeError, PyValueError,
};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::type_object::PyTypeInfo;
use pyo3::types::{
    PyBool, PyDateTime, PyDict, PyFloat, PyInt, PyList, PyModule, PyString, PyTuple, PyType,
    PyTzInfoAccess,
};

use crate::filters::NativeFn;
use crate::render::context::Rctx;
use crate::varparse::{FilterArg, Keys, PyConst, VarPart};

/// Python singletons, functions, and type objects resolved on first use.
pub(crate) struct Helpers {
    pub signature: Py<PyAny>,
    pub mark_safe: Py<PyAny>,
    pub safe_data: Py<PyAny>,
    pub safe_string: Py<PyAny>,
    pub basectx_type: Py<PyAny>,
    pub context_dict: Py<PyAny>,
    pub vdx: Py<PyAny>,
    pub gettext_lazy: Py<PyAny>,
    pub localize: Py<PyAny>,
    pub template_localtime: Py<PyAny>,
    pub conditional_escape: Py<PyAny>,
    pub int_builtin: Py<PyAny>,
    pub dir_fn: Py<PyAny>,
    pub object_dir: Py<PyAny>,
    pub settings: Py<PyAny>,
}

static HELPERS: PyOnceLock<Helpers> = PyOnceLock::new();

pub(crate) fn helpers(py: Python<'_>) -> PyResult<&Helpers> {
    HELPERS.get_or_try_init(py, || -> PyResult<Helpers> {
        let translation = py.import("django.utils.translation")?;
        let safestring = py.import("django.utils.safestring")?;
        Ok(Helpers {
            dir_fn: py.eval(c"dir", None, None)?.unbind(),
            object_dir: py.eval(c"object.__dir__", None, None)?.unbind(),
            signature: py.import("inspect")?.getattr("signature")?.unbind(),
            mark_safe: safestring.getattr("mark_safe")?.unbind(),
            safe_data: safestring.getattr("SafeData")?.unbind(),
            safe_string: safestring.getattr("SafeString")?.unbind(),
            basectx_type: py
                .import("django.template.context")?
                .getattr("BaseContext")?
                .unbind(),
            context_dict: py
                .import("django.template.context")?
                .getattr("ContextDict")?
                .unbind(),
            vdx: py
                .import("django.template.base")?
                .getattr("VariableDoesNotExist")?
                .unbind(),
            gettext_lazy: translation.getattr("gettext_lazy")?.unbind(),
            localize: py
                .import("django.utils.formats")?
                .getattr("localize")?
                .unbind(),
            template_localtime: py
                .import("django.utils.timezone")?
                .getattr("template_localtime")?
                .unbind(),
            conditional_escape: py
                .import("django.utils.html")?
                .getattr("conditional_escape")?
                .unbind(),
            int_builtin: py.eval(c"int", None, None)?.unbind(),
            settings: py.import("django.conf")?.getattr("settings")?.unbind(),
        })
    })
}

fn matches_err<T: PyTypeInfo>(py: Python<'_>, e: &PyErr) -> bool {
    e.is_instance_of::<T>(py)
}

// Not exposed by pyo3-ffi; both are part of CPython's public C API.
extern "C" {
    static mut PyMethod_Type: pyo3::ffi::PyTypeObject;
    fn PyMethod_Function(m: *mut pyo3::ffi::PyObject) -> *mut pyo3::ffi::PyObject;
}

/// For an exact bound method wrapping a plain Python function, the
/// function itself; else `None`. `method_getattro` serves any name the
/// `method` type lacks (e.g. `alters_data`) via `getattr(__func__, name)`,
/// but its own miss raises an AttributeError that `getattr_opt` then
/// clears; the function's generic getattro never raises on a miss.
fn bound_method_func<'py>(obj: &Bound<'py, PyAny>) -> Option<Bound<'py, PyAny>> {
    use pyo3::ffi;
    // SAFETY: type-object pointer comparisons; `PyMethod_Function` returns
    // a borrowed reference kept alive by the live method object.
    unsafe {
        if ffi::Py_TYPE(obj.as_ptr()) != std::ptr::addr_of_mut!(PyMethod_Type) {
            return None;
        }
        let f = PyMethod_Function(obj.as_ptr());
        if f.is_null() || ffi::Py_TYPE(f) != std::ptr::addr_of_mut!(ffi::PyFunction_Type) {
            return None;
        }
        Some(Bound::from_borrowed_ptr(obj.py(), f))
    }
}

fn truthy_attr(obj: &Bound<'_, PyAny>, name: &Bound<'_, PyString>) -> bool {
    // `getattr_opt`: a missing attribute (the common case) raises no
    // AttributeError object.
    obj.getattr_opt(name)
        .ok()
        .flatten()
        .is_some_and(|v| v.is_truthy().unwrap_or(false))
}

pub(crate) fn mark_safe_bound<'py>(v: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    let py = v.py();
    let h = helpers(py)?;
    // Fast path matching `mark_safe` semantics for exact `str` input:
    // `hasattr(str, "__html__")` is False and a `str` is not callable, so
    // stock returns `SafeString(v)`. Skipping the `keep_lazy` wrapper and
    // the function body on filter outputs (always plain strings here)
    // saves the lazy-wrapper and `hasattr`/`callable` probes per call.
    if v.is_exact_instance_of::<PyString>() {
        return h.safe_string.bind(py).call1((v,));
    }
    h.mark_safe.bind(py).call1((v,))
}

pub(crate) fn mark_safe_str<'py>(
    py: Python<'py>,
    s: &str,
) -> PyResult<Bound<'py, PyAny>> {
    let v: Bound<'py, PyAny> = PyString::new(py, s).into_any();
    mark_safe_bound(&v)
}

pub(crate) fn is_safe_data(v: &Bound<'_, PyAny>) -> PyResult<bool> {
    let py = v.py();
    v.is_instance(helpers(py)?.safe_data.bind(py))
}

/// VDX instance test via the real class object.
pub(crate) fn is_var_does_not_exist(py: Python<'_>, e: &PyErr) -> bool {
    helpers(py)
        .map(|h| e.is_instance(py, h.vdx.bind(py)))
        .unwrap_or(false)
}

fn py_str(v: &Bound<'_, PyAny>) -> PyResult<String> {
    Ok(v.str()?.to_string())
}

/// Port of `django.utils.html.escape` (i.e. `html.escape`, quote=True):
/// five entities, `&` first so inserted entities never re-escape. Specials
/// are ASCII, so UTF-8 sequences pass through intact. Returns `None` when
/// the input has nothing to escape.
///
/// Scanned in 16-byte blocks with `fearless_simd` at the compile-time
/// baseline level (SSE2 on `x86_64`, NEON on `aarch64`, scalar elsewhere):
/// one vector compare per block yields a bitmask of specials, so clean
/// blocks are skipped outright and each special is reached via
/// `trailing_zeros` instead of a bytewise walk. The baseline level needs no
/// runtime detection, which keeps short strings as cheap as the scalar loop.
pub(crate) fn escape_html(s: &str) -> Option<String> {
    dispatch!(Level::baseline(), simd => escape_html_simd(simd, s))
}

const ESC_BLOCK: usize = 16;

#[expect(
    clippy::inline_always,
    reason = "must inline into the `dispatch!` closure to compile under its SIMD level"
)]
#[inline(always)]
fn escape_html_simd<S: Simd>(simd: S, s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let (blocks, tail) = bytes.as_chunks::<ESC_BLOCK>();
    let first = blocks.iter().position(|b| special_mask(simd, b) != 0);
    if first.is_none() && !tail.iter().any(|&b| is_html_special(b)) {
        return None;
    }
    let mut out = String::with_capacity(s.len() + 16);
    let mut last = 0;
    for (k, block) in blocks.iter().enumerate().skip(first.unwrap_or(blocks.len())) {
        let mut m = special_mask(simd, block);
        while m != 0 {
            let i = k * ESC_BLOCK + m.trailing_zeros() as usize;
            push_escaped(&mut out, s, &mut last, i);
            m &= m - 1;
        }
    }
    let base = blocks.len() * ESC_BLOCK;
    for (j, &b) in tail.iter().enumerate() {
        if is_html_special(b) {
            push_escaped(&mut out, s, &mut last, base + j);
        }
    }
    out.push_str(&s[last..]);
    Some(out)
}

/// Bit `j` set iff `block[j]` is one of the five specials.
#[expect(
    clippy::inline_always,
    reason = "must inline into the `dispatch!` closure to compile under its SIMD level"
)]
#[inline(always)]
fn special_mask<S: Simd>(simd: S, block: &[u8; ESC_BLOCK]) -> u64 {
    let v = u8x16::from_slice(simd, block);
    (v.simd_eq(b'&') | v.simd_eq(b'<') | v.simd_eq(b'>') | v.simd_eq(b'"') | v.simd_eq(b'\''))
        .to_bitmask()
}

#[inline]
fn is_html_special(b: u8) -> bool {
    matches!(b, b'&' | b'<' | b'>' | b'"' | b'\'')
}

/// Flush the clean text in `s[*last..i]`, then the entity for `s[i]`.
#[inline]
fn push_escaped(out: &mut String, s: &str, last: &mut usize, i: usize) {
    out.push_str(&s[*last..i]);
    out.push_str(match s.as_bytes()[i] {
        b'&' => "&amp;",
        b'<' => "&lt;",
        b'>' => "&gt;",
        b'"' => "&quot;",
        _ => "&#x27;",
    });
    *last = i + 1;
}

/// `str(conditional_escape(v))`: exact `SafeString` passes through
/// (`__html__` returns itself), exact `str` takes the native `escape`
/// port; anything else (lazy `Promise`s, `__html__` objects, subclasses)
/// runs the stock function.
pub(crate) fn conditional_escape_str(v: &Bound<'_, PyAny>) -> PyResult<String> {
    let py = v.py();
    let h = helpers(py)?;
    if v.get_type().as_ptr() == h.safe_string.bind(py).as_ptr() {
        if let Ok(s) = v.cast::<PyString>() {
            if let Ok(rs) = s.to_str() {
                return Ok(rs.to_string());
            }
        }
    } else if let Ok(s) = v.cast_exact::<PyString>() {
        if let Ok(rs) = s.to_str() {
            return Ok(escape_html(rs).unwrap_or_else(|| rs.to_string()));
        }
    }
    py_str(&h.conditional_escape.bind(py).call1((v,))?)
}

fn vdx_missing(py: Python<'_>, bit: &str, current: &Bound<'_, PyAny>) -> PyErr {
    match helpers(py) {
        Ok(h) => h
            .vdx
            .bind(py)
            .call1((
                "Failed lookup for key [%s] in %r",
                (bit, current.clone()),
            ))
            .map_or_else(std::convert::identity, PyErr::from_value),
        Err(e) => e,
    }
}

// ---------------------------------------------------------------------------
// Variable.resolve
// ---------------------------------------------------------------------------

/// Big-int literal → Python int (varparse stores arbitrary precision).
fn bigint_to_py<'py>(py: Python<'py>, digits: &str) -> PyResult<Bound<'py, PyAny>> {
    helpers(py)?
        .int_builtin
        .bind(py)
        .call1((digits,))
}

/// `int(BigInt)` without the string round-trip: exact `int` fits native
/// i64/u64 for every parse-time literal in practice; the string path stays
/// for anything wider.
fn bigint_to_native<'py>(py: Python<'py>, i: &num_bigint::BigInt) -> PyResult<Bound<'py, PyAny>> {
    use num_traits::cast::ToPrimitive;
    use pyo3::types::PyInt;
    if let Some(v) = i.to_i64() {
        return Ok(PyInt::new(py, v).into_any());
    }
    if let Some(v) = i.to_u64() {
        return Ok(PyInt::new(py, v).into_any());
    }
    bigint_to_py(py, &i.to_string())
}

/// `translate` handling in `Variable.resolve`: `gettext_lazy(msgid)`.
/// (`message_context`/pgettext is impossible for a bare `_( ... )` var.)
fn translate_value<'py>(
    py: Python<'py>,
    v: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let h = helpers(py)?;
    let is_safe = is_safe_data(v)?;
    let s = py_str(v)?;
    let msgid = s.replace('%', "%%");
    let msgid_obj: Bound<'_, PyAny> = if is_safe {
        mark_safe_str(py, &msgid)?.into_any()
    } else {
        PyString::new(py, &msgid).into_any()
    };
    h.gettext_lazy.bind(py).call1((msgid_obj,))
}

/// Memoized `mark_safe` for parse-time string literals. Stock bakes the
/// `SafeString` once per `Variable` construction (`base.py:922`); without a
/// memo, dtl re-wraps the same literal on every evaluation.
static STR_SAFE: PyOnceLock<Py<PyDict>> = PyOnceLock::new();

fn memo_mark_safe<'py>(py: Python<'py>, s: &str) -> PyResult<Bound<'py, PyAny>> {
    let cache = STR_SAFE.get_or_try_init(py, || -> PyResult<Py<PyDict>> {
        Ok(PyDict::new(py).unbind())
    })?;
    let key = PyString::new(py, s);
    let cache = cache.bind(py);
    if let Some(v) = cache.get_item(&key)? {
        return Ok(v);
    }
    let v = mark_safe_str(py, s)?;
    cache.set_item(&key, &v)?;
    Ok(v)
}

/// Constant value materialization: what `Variable(s).resolve({})` produces.
pub(crate) fn const_resolve<'py>(py: Python<'py>, c: &PyConst) -> PyResult<Bound<'py, PyAny>> {
    Ok(match c {
        PyConst::Str(s) => memo_mark_safe(py, s)?,
        PyConst::Translate(s) => {
            // Variable('_("x")').resolve({}) resolves literal "x" (marked
            // safe) then gettext_lazy(mark_safe("x")).
            let msgid = s.replace('%', "%%");
            let m = mark_safe_str(py, &msgid)?;
            helpers(py)?.gettext_lazy.bind(py).call1((m,))?
        }
    })
}

/// `Variable.resolve` for any `VarPart` (constants, numeric literals,
/// lookups). `VariableDoesNotExist` propagates as a VDX `PyErr`.
pub(crate) fn variable_resolve<'py>(
    rctx: &mut Rctx<'py>,
    part: &VarPart,
) -> PyResult<Bound<'py, PyAny>> {
    let py = rctx.py();
    let value = match part {
        VarPart::Const(c) => const_resolve(py, c)?,
        VarPart::Int(i) => bigint_to_native(py, i)?,
        VarPart::Float(f) => f.into_pyobject(py)?.into_any(),
        VarPart::Lookup {
            lookups,
            translate,
            keys,
            ..
        } => lookup_resolve(rctx, lookups, keys, *translate)?,
    };
    Ok(value)
}

/// `Variable.resolve` for a lookup variable (shared by filter arguments).
fn lookup_resolve<'py>(
    rctx: &mut Rctx<'py>,
    lookups: &[String],
    keys: &Keys,
    translate: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let py = rctx.py();
    let v = match resolve_lookup(rctx, lookups, keys) {
        Ok(v) => v,
        Err(Miss::NotFound { bit, current }) => {
            return Err(vdx_missing(py, &bit, &current));
        }
        Err(Miss::Raise(e)) => return Err(e),
    };
    if translate {
        translate_value(py, &v)
    } else {
        Ok(v)
    }
}

/// `resolve_lookup` failure carried as a Rust value. A provable
/// `VariableDoesNotExist` lookup miss never materialises the Python
/// exception (message formatting needs a `%r` of the lookup target) unless
/// a Python-visible consumer actually catches it; the miss only carries the
/// bit and its repr target. Anything else the stock catch-all would see is
/// kept as a `PyErr`.
enum Miss<'py> {
    NotFound {
        bit: String,
        current: Bound<'py, PyAny>,
    },
    Raise(PyErr),
}

impl From<PyErr> for Miss<'_> {
    fn from(e: PyErr) -> Self {
        Miss::Raise(e)
    }
}

/// Port of `Variable._resolve_lookup`. Root is the live Context object.
fn resolve_lookup<'py>(
    rctx: &mut Rctx<'py>,
    lookups: &[String],
    keys: &Keys,
) -> Result<Bound<'py, PyAny>, Miss<'py>> {
    let py = rctx.py();
    let mut current: Bound<'py, PyAny> = rctx.ctx.clone();

    for (bit, key) in lookups.iter().zip(keys.get(py, lookups)) {
        // Django wraps the whole body (lookup + callable handling) in one
        // try/except; on silent_variable_failure it assigns
        // string_if_invalid and falls out of the loop entirely.
        let step = lookup_step(rctx, &current, bit, key.bind(py));
        current = match step {
            Ok(v) => v,
            Err(m @ Miss::NotFound { .. }) => return Err(m),
            Err(Miss::Raise(e)) => {
                if e.value(rctx.py())
                    .getattr("silent_variable_failure")
                    .is_ok_and(|x| x.is_truthy().unwrap_or(false))
                {
                    return rctx.engine.getattr("string_if_invalid").map_err(Miss::Raise);
                }
                return Err(Miss::Raise(e));
            }
        };
    }
    Ok(current)
}

/// One loop iteration: `lookup_bit` + callable handling, wrapped like
/// Django's single try/except around both.
fn lookup_step<'py>(
    rctx: &mut Rctx<'py>,
    current: &Bound<'py, PyAny>,
    bit: &str,
    key: &Bound<'py, PyString>,
) -> Result<Bound<'py, PyAny>, Miss<'py>> {
    let mut current = lookup_bit(rctx, current, bit, key)?;
    if current.is_callable() {
        current = handle_callable(rctx, &current)?;
    }
    Ok(current)
}

/// Three-tier lookup: getitem → attribute → list-index. Escapes to the
/// caller's catch-all with VDX or pass-through exceptions.
fn lookup_bit<'py>(
    rctx: &mut Rctx<'py>,
    current: &Bound<'py, PyAny>,
    bit: &str,
    key: &Bound<'py, PyString>,
) -> Result<Bound<'py, PyAny>, Miss<'py>> {
    let py = current.py();

    // Exact list/tuple: `current[bit]` with a str bit is always a TypeError
    // and neither type has (or can gain) an attribute named like an int, so
    // a numeric bit reaches tier 3 in stock; index directly.
    let seq = current.is_exact_instance_of::<PyList>() || current.is_exact_instance_of::<PyTuple>();
    if seq {
        if let Some(n) = ascii_int_bit(bit) {
            return seq_index(current, n).ok_or_else(|| Miss::NotFound {
                bit: bit.to_string(),
                current: current.clone(),
            });
        }
    }
    // tier 1: dict lookup — only if the metaclass implements __getitem__
    if !seq && type_has_getitem(current) {
        if current.as_ptr() == rctx.ctx.as_ptr() {
            // Root: native port of `Context.__getitem__` — stock runs the
            // Python method once per variable per render.
            if let Some(v) = rctx.get_item_root(key)? {
                return Ok(v);
            }
        } else {
            // Exact dict: a miss is `None`, not a raised KeyError.
            let got = match current.cast_exact::<PyDict>() {
                Ok(d) => d.get_item(key),
                Err(_) => current.get_item(key).map(Some),
            };
            match got {
                Ok(Some(v)) => return Ok(v),
                Ok(None) => {}
                Err(e)
                    if matches_err::<PyTypeError>(py, &e)
                        || matches_err::<PyAttributeError>(py, &e)
                        || matches_err::<PyKeyError>(py, &e)
                        || matches_err::<PyValueError>(py, &e)
                        || matches_err::<PyIndexError>(py, &e) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    // tier 2: attribute lookup
    // Stock tests `isinstance(context, BaseContext)` twice on the miss
    // path (class-attr branch + dir() guard); compute once. Root identity
    // short-circuits the isinstance call for the common `{{ var }}` walk;
    // exact builtin containers are never contexts.
    let is_ctx = current.as_ptr() == rctx.ctx.as_ptr()
        || (!seq
            && !current.is_exact_instance_of::<PyDict>()
            && current.is_instance(helpers(py)?.basectx_type.bind(py))?);
    let attr: PyResult<Option<Bound<'py, PyAny>>> = if is_ctx {
        // "Don't return class attributes if the class is the context":
        // truthy `getattr(type(current), bit)` raises AttributeError,
        // funneling control to tier 3; a missing/falsy class attribute
        // falls through to the instance getattr below. A context never
        // takes the dir() re-raise, so any AttributeError means tier 3 and
        // `getattr_opt`'s `None` is the same outcome without the raise.
        match current.get_type().getattr_opt(key) {
            Ok(Some(v)) => match v.is_truthy() {
                Ok(true) => Ok(None),
                Ok(false) => current.getattr_opt(key),
                Err(e) => Err(e),
            },
            Ok(None) | Err(_) => Ok(None),
        }
    } else if let Some(probe) = generic_attr_probe(current, key) {
        probe
    } else {
        current.getattr(key).map(Some)
    };
    match attr {
        Ok(Some(v)) => return Ok(v),
        // Provable miss: no AttributeError object, nothing in dir().
        Ok(None) => {}
        Err(attr_err) => {
            if !matches_err::<PyTypeError>(py, &attr_err)
                && !matches_err::<PyAttributeError>(py, &attr_err)
            {
                return Err(attr_err.into());
            }
            // Reraise if the exception was raised by a @property
            if !is_ctx && py_in_dir(current, bit)? {
                return Err(attr_err.into());
            }
        }
    }

    // tier 3: list-index lookup. Classify `int(bit)` without calling the
    // builtin when the outcome is provable: ASCII alnum/underscore leads
    // (lexer bits) that are not digit/sign fail `int()` outright → stock
    // raises VDX; digit/sign leads take a Rust i64 parse (identical value,
    // fallback builtin for underscores/big ints); non-ASCII leads may parse
    // as unicode digits, so the builtin decides.
    let head = bit.as_bytes().first().copied();
    let idx = if matches!(head, Some(b) if b.is_ascii_digit() || b == b'+' || b == b'-') {
        match bit.parse::<i64>() {
            Ok(n) => PyInt::new(py, n).into_any(),
            Err(_) => int_or_vdx(py, bit, current)?,
        }
    } else if matches!(head, Some(b) if b.is_ascii_alphabetic() || b == b'_') {
        return Err(Miss::NotFound {
            bit: bit.to_string(),
            current: current.clone(),
        });
    } else {
        // non-ASCII lead: unicode digits parse under int(); builtin decides.
        int_or_vdx(py, bit, current)?
    };
    // `current[int(bit)]`: `get_item` on a non-subscriptable raises
    // TypeError exactly like the subscript bytecode (`call_method1` would
    // raise AttributeError on e.g. None, which stock does not).
    match current.get_item(idx) {
        Ok(v) => Ok(v),
        Err(e)
            if matches_err::<PyIndexError>(py, &e)
                || matches_err::<PyValueError>(py, &e)
                || matches_err::<PyKeyError>(py, &e)
                || matches_err::<PyTypeError>(py, &e) =>
        {
            Err(Miss::NotFound {
                bit: bit.to_string(),
                current: current.clone(),
            })
        }
        Err(e) => Err(e.into()),
    }
}

// Exported by every supported CPython (3.12+) but skipped by pyo3-ffi as
// private. Borrowed result, never raises.
extern "C" {
    fn _PyType_Lookup(tp: *mut pyo3::ffi::PyTypeObject, name: *mut pyo3::ffi::PyObject) -> *mut pyo3::ffi::PyObject;
}

/// Tier-2 `getattr(current, bit)` without the AttributeError on a miss,
/// for the common shape where that is provably equivalent: the type keeps
/// the generic `object.__getattribute__` and default `__dir__`, and no
/// MRO class defines `bit`. Then the lookup can only consult the instance
/// `__dict__` (no descriptor, `__getattr__` or other code runs), so a miss
/// means `bit` is in neither the instance dict nor any class dict — i.e.
/// not in `dir(current)` — and stock proceeds to tier 3: `Ok(None)`.
/// `None` → take the stock raising path.
fn generic_attr_probe<'py>(
    current: &Bound<'py, PyAny>,
    key: &Bound<'py, PyString>,
) -> Option<PyResult<Option<Bound<'py, PyAny>>>> {
    use pyo3::ffi;
    // SAFETY: slot reads and borrowed type-cache lookups on live objects.
    unsafe {
        let tp = ffi::Py_TYPE(current.as_ptr());
        if (*tp).tp_getattro.map(|f| f as usize) != Some(ffi::PyObject_GenericGetAttr as *const () as usize) {
            return None;
        }
        if !_PyType_Lookup(tp, key.as_ptr()).is_null() {
            return None;
        }
    }
    match current.getattr_opt(key) {
        // A custom `__dir__` could still list `bit` (stock then re-raises
        // the real AttributeError): repeat the lookup raising. Nothing but
        // the instance dict is consulted, so the repeat runs no code.
        Ok(None) if !default_dir(current) => Some(current.getattr(key).map(Some)),
        r => Some(r),
    }
}

/// `type(v).__dir__ is object.__dir__`.
fn default_dir(v: &Bound<'_, PyAny>) -> bool {
    let py = v.py();
    let Ok(h) = helpers(py) else { return false };
    // SAFETY: borrowed type-cache lookup on a live type.
    unsafe {
        _PyType_Lookup(pyo3::ffi::Py_TYPE(v.as_ptr()), pyo3::intern!(py, "__dir__").as_ptr())
            == h.object_dir.as_ptr()
    }
}

/// `bit` as stock's tier-3 `int(bit)` when it is a plain ASCII decimal with
/// optional sign (what the lexer produces for `lst.1`); `None` otherwise
/// (underscores, unicode digits, big ints and non-numbers keep the
/// general path).
fn ascii_int_bit(bit: &str) -> Option<isize> {
    let b = bit.as_bytes();
    let digits = match b.first() {
        Some(b'+' | b'-') => &b[1..],
        _ => b,
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    bit.parse::<isize>().ok()
}

/// `seq[n]` for an exact list/tuple with Python's negative-index rule;
/// `None` = IndexError.
fn seq_index<'py>(seq: &Bound<'py, PyAny>, n: isize) -> Option<Bound<'py, PyAny>> {
    let len = isize::try_from(seq.len().ok()?).ok()?;
    let i = if n < 0 { n + len } else { n };
    if i < 0 || i >= len {
        return None;
    }
    let i = usize::try_from(i).ok()?;
    if let Ok(l) = seq.cast_exact::<PyList>() {
        return l.get_item(i).ok();
    }
    seq.cast_exact::<PyTuple>().ok()?.get_item(i).ok()
}

/// Tier-1 guard `hasattr(type(current), "__getitem__")`, answered from the
/// type's subscript slots when that provably gives the same tier-1
/// outcome; `hasattr` costs ~220 ns through pyo3 per dotted step.
///
/// Valid when `current` is not itself a class (`type[x]` subscripts reach
/// `__class_getitem__` without any slot) and the metaclass keeps
/// `type.__getattribute__` (no custom `__getattr__` hooks to run). Then:
/// - slot set ⇒ `__getitem__` sits in the class MRO (slots and class
///   dicts are kept in sync; `__getitem__ = None` sets the slot too), so
///   `hasattr` is True and stock subscripts — as do we;
/// - slot empty ⇒ either `hasattr` is False (skip), or only the metaclass
///   defines `__getitem__` and stock's `current[bit]` raises the
///   "not subscriptable" `TypeError` tier 1 swallows — skip either way.
fn type_has_getitem(current: &Bound<'_, PyAny>) -> bool {
    use pyo3::ffi;
    // SAFETY: `current` is a live object; its type and metatype objects
    // are live for as long as it is, and the slot tables are plain reads.
    unsafe {
        let tp = ffi::Py_TYPE(current.as_ptr());
        let meta = ffi::Py_TYPE(tp.cast::<ffi::PyObject>());
        let generic = (meta == std::ptr::addr_of_mut!(ffi::PyType_Type)
            || (*meta).tp_getattro.map(|f| f as usize)
                == ffi::PyType_Type.tp_getattro.map(|f| f as usize))
            && ffi::PyType_Check(current.as_ptr()) == 0;
        if generic {
            let m = (*tp).tp_as_mapping;
            if !m.is_null() && (*m).mp_subscript.is_some() {
                return true;
            }
            let s = (*tp).tp_as_sequence;
            return !s.is_null() && (*s).sq_item.is_some();
        }
    }
    current.get_type().hasattr("__getitem__").unwrap_or(false)
}

/// `int(bit)` mapped to VDX on failure (stock's tier-3 outcome).
fn int_or_vdx<'py>(
    py: Python<'py>,
    bit: &str,
    current: &Bound<'py, PyAny>,
) -> Result<Bound<'py, PyAny>, Miss<'py>> {
    match helpers(py)?.int_builtin.bind(py).call1((bit,)) {
        Ok(v) => Ok(v),
        Err(_) => Err(Miss::NotFound {
            bit: bit.to_string(),
            current: current.clone(),
        }),
    }
}

fn py_in_dir(v: &Bound<'_, PyAny>, bit: &str) -> PyResult<bool> {
    let py = v.py();
    let h = helpers(py)?;
    // Default `__dir__` merges exactly: instance `__dict__` ∪ every MRO
    // class `__dict__` (slots appear there as descriptors). Probing those
    // dicts directly skips building a list, sorting it and scanning it;
    // `Bound::contains` is the `in` operator, so read-only mapping proxies
    // on classes answer correctly. A class that overrides `__dir__` still
    // goes through the real `dir()`.
    if v.get_type().getattr("__dir__")?.as_ptr() != h.object_dir.bind(py).as_ptr() {
        let names = h.dir_fn.bind(py).call1((v,))?;
        return names.cast_into::<PyList>()?.contains(pyo3::types::PyString::new(py, bit));
    }
    if let Ok(d) = v.getattr("__dict__") {
        if d.contains(bit)? {
            return Ok(true);
        }
    }
    let mro = v.get_type().getattr("__mro__")?;
    for cls in mro.try_iter()? {
        if cls?.getattr("__dict__")?.contains(bit)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// callable handling exactly as in `_resolve_lookup`.
fn handle_callable<'py>(
    rctx: &mut Rctx<'py>,
    current: &Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = current.py();
    let func = bound_method_func(current);
    let probe = func.as_ref().unwrap_or(current);
    if truthy_attr(probe, pyo3::intern!(py, "do_not_call_in_templates")) {
        return Ok(current.clone());
    }
    if truthy_attr(probe, pyo3::intern!(py, "alters_data")) {
        return rctx.engine.getattr("string_if_invalid");
    }
    match current.call0() {
        Ok(v) => Ok(v),
        Err(e) if matches_err::<PyTypeError>(py, &e) => {
            let h = helpers(py)?;
            match h.signature.bind(py).call1((current,)) {
                Err(se) if matches_err::<PyValueError>(py, &se) => {
                    rctx.engine.getattr("string_if_invalid")
                }
                Err(se) => Err(se),
                Ok(sig) => match sig.call_method0("bind") {
                    // Arguments *were* required → invalid method call.
                    Err(_) => rctx.engine.getattr("string_if_invalid"),
                    // The TypeError was not about arguments: re-raise.
                    Ok(_) => Err(e),
                },
            }
        }
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Filter registry + FilterExpression.resolve
// ---------------------------------------------------------------------------

/// A registered Python filter with Django's function flags.
#[allow(clippy::struct_excessive_bools)]
pub struct PyFilterEntry {
    pub func: Py<PyAny>,
    /// `(min_args, max_args)` including the implicit input argument.
    pub arity: (usize, usize),
    pub needs_autoescape: bool,
    pub expects_localtime: bool,
    pub is_safe: bool,
    /// Native implementation replacing `func` for stock built-ins.
    pub native: Option<NativeFn>,
    /// True iff the registered function is `stringfilter`-wrapped.
    pub string_coerced: bool,
}

/// Engine-side registry consulted at parse time (existence + arity).
#[derive(Default)]
pub struct FilterRegistry {
    pub entries: std::collections::HashMap<String, PyFilterEntry>,
}

impl crate::varparse::FilterLookup for FilterRegistry {
    fn filter_arity(&self, name: &str) -> Option<(usize, usize)> {
        self.entries.get(name).map(|e| e.arity)
    }
}

/// A filter bound at node-construction time (parse time), exactly like
/// Django binding `filter_func` inside `FilterExpression.__init__`.
#[allow(clippy::struct_excessive_bools)]
pub struct BoundFilter {
    pub func: Py<PyAny>,
    pub needs_autoescape: bool,
    pub expects_localtime: bool,
    pub is_safe: bool,
    pub native: Option<NativeFn>,
    /// True iff the Python function is `stringfilter`-wrapped (detected via
    /// `functools.wraps`-set `__wrapped__`); such filters' inner `mark_safe`
    /// layer checks the COERCED input.
    pub string_coerced: bool,
    pub args: Vec<FilterArg>,
}

/// The single lookup bit of a plain `{{ ident }}` variable, or `None`.
/// Requires a single dot-free non-translate lookup whose leading byte is
/// an ASCII letter or `_` — then `int(bit)` provably fails (Python `int`
/// needs digits, an explicit sign or whitespace first), so a tier-2 miss
/// is a definitive `TemplateVariableDoesNotExist` with no exception dance.
/// Subscripted (`a['b']`) and dotted variables keep the general path.
fn plain_ident_bit<'a, 'py>(
    py: Python<'py>,
    part: &'a VarPart,
) -> Option<(&'a str, Bound<'py, PyString>)> {
    match part {
        VarPart::Lookup {
            lookups,
            translate,
            keys,
            ..
        } => {
            if *translate || lookups.len() != 1 {
                return None;
            }
            let bit = lookups[0].as_str();
            let leads = matches!(bit.as_bytes().first(), Some(c) if c.is_ascii_alphabetic() || *c == b'_');
            leads.then(|| (bit, keys.get(py, lookups)[0].bind(py).clone()))
        }
        _ => None,
    }
}

/// Root-context tier-1 + tier-2 walk for `plain_ident_bit` variables,
/// mirroring `_resolve_lookup` with `current` at the `Context`: stock's
/// class-attribute branch (`isinstance(current, BaseContext) and
/// getattr(type(current), bit)`) and the `@property` `dir()` guard are both
/// in the `except (TypeError, AttributeError)` scope of the attribute
/// tier; the ctx skips the `dir()` re-raise, and the provably non-numeric
/// bit misses tier 3 outright. `Ok(Some(v))` = hit (callables handled
/// like `lookup_step`); `Ok(None)` = the VDX outcome without raising.
fn root_fast_lookup<'py>(
    rctx: &mut Rctx<'py>,
    key: &Bound<'py, PyString>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    let py = rctx.py();
    if let Some(v) = rctx.get_item_root(key)? {
        return Ok(Some(if v.is_callable() {
            handle_callable(rctx, &v)?
        } else {
            v
        }));
    }
    // `getattr_opt`: a missing class or instance attribute (the usual
    // `{{ undefined }}` case) is the AttributeError outcome without one
    // being raised; other errors still surface as `Err`.
    let falls_through = match rctx.ctx.get_type().getattr_opt(key) {
        Ok(Some(cls)) => match cls.is_truthy() {
            // Truthy class attribute → stock raises AttributeError →
            // tier 3 → provable miss. TypeError/AttributeError from the
            // truthiness test are caught by stock the same way.
            Ok(true) => false,
            Ok(false) => true,
            Err(e)
                if matches_err::<PyTypeError>(py, &e)
                    || matches_err::<PyAttributeError>(py, &e) =>
            {
                false
            }
            Err(e) => return Err(e),
        },
        // Missing (or TypeError-y) class attribute → AttributeError caught
        // before the instance getattr → tier 3 → provable miss.
        Ok(None) => false,
        Err(e)
            if matches_err::<PyAttributeError>(py, &e) || matches_err::<PyTypeError>(py, &e) =>
        {
            false
        }
        Err(e) => return Err(e),
    };
    if !falls_through {
        return Ok(None);
    }
    // Falsy class attribute: stock runs the real instance getattr.
    match rctx.ctx.getattr_opt(key) {
        Ok(Some(v)) => Ok(Some(if v.is_callable() {
            handle_callable(rctx, &v)?
        } else {
            v
        })),
        Ok(None) => Ok(None),
        Err(e)
            if matches_err::<PyTypeError>(py, &e) || matches_err::<PyAttributeError>(py, &e) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}


/// `FilterExpression.resolve`'s `VariableDoesNotExist` catch outcome when
/// not ignoring failures: `Some(v)` = early-return `v` (truthy
/// `string_if_invalid`, `%s`-formatted with the variable text when it has
/// a placeholder — stock skips the filters); `None` = falsy handle, the
/// handle itself becomes the filter input.
fn vdx_outcome<'py>(
    rctx: &Rctx<'py>,
    fe: &crate::varparse::FilterExpression,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    let py = rctx.py();
    let string_if_invalid = rctx.engine.getattr("string_if_invalid")?;
    if string_if_invalid.is_truthy()? {
        let sii = py_str(&string_if_invalid)?;
        if sii.contains("%s") {
            let var_text = var_part_text(&fe.var);
            let fmt = PyString::new(py, &sii);
            return fmt
                .call_method1("__mod__", (PyString::new(py, &var_text),))
                .map(Some);
        }
        return Ok(Some(string_if_invalid));
    }
    Ok(None)
}
/// Port of `FilterExpression.resolve(context, ignore_failures)`.
/// `ignore_failures=true`: a var-lookup `VDX` yields `None` (`string_if_invalid`
/// skipped) and filters still apply; non-var VDX propagates.
pub(crate) fn filter_expression_resolve_ignoring<'py>(
    rctx: &mut Rctx<'py>,
    fe: &crate::varparse::FilterExpression,
    filters: &[BoundFilter],
    ignore_failures: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let py = rctx.py();
    let mut obj: Bound<'py, PyAny> = if fe.is_var {
        // Plain `{{ ident }}` variables: run the root tier-1/tier-2 walk
        // directly. A leading-letter ASCII identifier can never parse as
        // `int()` in tier 3, so a tier-2 miss is a provable lookup miss —
        // no `VariableDoesNotExist` has to be constructed, raised and
        // caught. Misses mirror the VDX catch below (`None` under
        // `ignore_failures`, else `string_if_invalid`; truthy `%s`
        // formats skip filters, exactly as stock).
        if let Some((bit, key)) = plain_ident_bit(py, &fe.var) {
            // Stock's catch-all around the lookup loop maps any error with
            // `silent_variable_failure` to `string_if_invalid` (filters
            // still apply to it); same here, without the VDX path.
            let fast = root_fast_lookup(rctx, &key).or_else(|e| {
                if e.value(py)
                    .getattr("silent_variable_failure")
                    .is_ok_and(|x| x.is_truthy().unwrap_or(false))
                {
                    rctx.engine.getattr("string_if_invalid").map(Some)
                } else {
                    Err(e)
                }
            })?;
            match fast {
                Some(v) => v,
                None if ignore_failures => py.None().into_bound(py),
                None => {
                    let string_if_invalid = rctx.engine.getattr("string_if_invalid")?;
                    if string_if_invalid.is_truthy()? {
                        let sii = py_str(&string_if_invalid)?;
                        if sii.contains("%s") {
                            let fmt = PyString::new(py, &sii);
                            return fmt.call_method1("__mod__", (PyString::new(py, bit),));
                        }
                        return Ok(string_if_invalid);
                    }
                    string_if_invalid
                }
            }
        } else {
            // Dotted / subscripted lookups: a provable lookup miss travels
            // as a Rust sentinel (no `VariableDoesNotExist` object built,
            // no `%r` of the miss target) and maps straight to the
            // `string_if_invalid` outcome. Errors raised by user code keep
            // the stock path: stock's catch-all re-raises them from
            // `Variable.resolve` into `FilterExpression.resolve`, where a
            // VDX is caught here and anything else escapes.
            let step = match &fe.var {
                VarPart::Lookup {
                    lookups,
                    translate: false,
                    keys,
                    ..
                } => resolve_lookup(rctx, lookups, keys),
                other => variable_resolve(rctx, other).map_err(Miss::Raise),
            };
            match step {
                Ok(v) => v,
                Err(Miss::NotFound { .. }) if ignore_failures => py.None().into_bound(py),
                Err(Miss::NotFound { .. }) => match vdx_outcome(rctx, fe)? {
                    Some(v) => return Ok(v),
                    None => rctx.engine.getattr("string_if_invalid")?,
                },
                Err(Miss::Raise(e)) => {
                    if !is_var_does_not_exist(py, &e) {
                        return Err(e);
                    }
                    if ignore_failures {
                        py.None().into_bound(py)
                    } else {
                        match vdx_outcome(rctx, fe)? {
                            Some(v) => return Ok(v),
                            None => rctx.engine.getattr("string_if_invalid")?,
                        }
                    }
                }
            }
        }
    } else {
        match &fe.var {
            VarPart::Const(c) => const_resolve(py, c)?,
            other => variable_resolve(rctx, other)?,
        }
    };

    for bf in filters {
        obj = apply_filter(rctx, bf, obj)?;
    }
    Ok(obj)
}

/// `FilterExpression.resolve(context)` (raising variant).
pub(crate) fn filter_expression_resolve<'py>(
    rctx: &mut Rctx<'py>,
    fe: &crate::varparse::FilterExpression,
    filters: &[BoundFilter],
) -> PyResult<Bound<'py, PyAny>> {
    filter_expression_resolve_ignoring(rctx, fe, filters, false)
}

fn var_part_text(part: &VarPart) -> String {
    match part {
        VarPart::Lookup { var, .. } => var.clone(),
        VarPart::Int(i) => i.to_string(),
        VarPart::Float(f) => format!("{f}"),
        VarPart::Const(_) => String::new(),
    }
}

fn filter_arg_value<'py>(
    rctx: &mut Rctx<'py>,
    arg: &FilterArg,
) -> PyResult<Bound<'py, PyAny>> {
    let py = rctx.py();
    Ok(match arg {
        // `if not lookup: arg_vals.append(mark_safe(arg))`
        FilterArg::Const(PyConst::Str(s)) => memo_mark_safe(py, s)?,
        FilterArg::Const(PyConst::Translate(s)) => {
            const_resolve(py, &PyConst::Translate(s.clone()))?
        }
        FilterArg::Int(i) => bigint_to_native(py, i)?,
        FilterArg::Float(f) => f.into_pyobject(py)?.into_any(),
        FilterArg::Lookup {
            lookups,
            translate,
            keys,
            ..
        } => lookup_resolve(rctx, lookups, keys, *translate)?,
    })
}

fn apply_filter<'py>(
    rctx: &mut Rctx<'py>,
    bf: &BoundFilter,
    obj: Bound<'py, PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let py = rctx.py();
    let func = bf.func.bind(py).clone();

    let mut arg_vals: Vec<Bound<'py, PyAny>> = Vec::with_capacity(bf.args.len());
    for arg in &bf.args {
        arg_vals.push(filter_arg_value(rctx, arg)?);
    }

    // expects_localtime / needs_autoescape per FilterExpression.resolve
    let obj = if bf.expects_localtime {
        let kwargs = PyDict::new(py);
        kwargs.set_item("use_tz", opt_to_py(py, rctx.use_tz()?))?;
        helpers(py)?
            .template_localtime
            .bind(py)
            .call((obj,), Some(&kwargs))?
    } else {
        obj
    };

    let input_was_safe = bf.is_safe && is_safe_data(&obj)?;
    if let Some(nf) = bf.native {
        // Native built-in filter: one Rust call replaces the whole
        // Python chain (wrapper + stringfilter layer + body). Stock's two
        // `mark_safe` layers check DIFFERENT objects: the `stringfilter`
        // inner `_dec` layer marks `func(str(first))` when the COERCED
        // string is SafeData (e.g. a plain object whose `__str__` returns a
        // SafeString), while the generic outer layer checks the RAW obj.
        // Both conditions also require `is_safe`, which `register.filter`
        // stamps on the wrapper and on `unwrap(func)`.
        let ae = if bf.needs_autoescape {
            Some(rctx.autoescape()?)
        } else {
            None
        };
        let mut new_obj = nf(rctx, &obj, &arg_vals, ae)?;
        if bf.is_safe && bf.string_coerced && !input_was_safe && is_safe_data(obj.str()?.as_any())? {
            new_obj = mark_safe_bound(&new_obj)?;
        }
        return Ok(if input_was_safe {
            mark_safe_bound(&new_obj)?
        } else {
            new_obj
        });
    }
    let new_obj = if bf.needs_autoescape {
        let kwargs = PyDict::new(py);
        kwargs.set_item("autoescape", rctx.autoescape()?)?;
        let mut a = vec![obj];
        a.extend(arg_vals);
        func.call(&PyTuple::new(py, a)?, Some(&kwargs))?
    } else {
        let mut a = vec![obj];
        a.extend(arg_vals);
        func.call(&PyTuple::new(py, a)?, None)?
    };
    Ok(if input_was_safe {
        mark_safe_bound(&new_obj)?
    } else {
        new_obj
    })
}

// ---------------------------------------------------------------------------
// render_value_in_context
// ---------------------------------------------------------------------------

/// Port of `render_value_in_context(value, context)`.
pub(crate) fn render_value_in_context<'py>(
    rctx: &mut Rctx<'py>,
    value: &Bound<'py, PyAny>,
) -> PyResult<String> {
    let py = rctx.py();
    let h = helpers(py)?;
    // Fast paths skipping the three Python calls (`template_localtime`
    // touches only datetimes, `localize` returns any str unchanged, and
    // exact str/SafeString escaping below).
    // 1) exact SafeString: never escaped (safe), rendered as its contents.
    // 2) exact str: autoescape on → native port of `html.escape` over the
    //    five entities (single pass: replacements never re-scan inserted
    //    text); autoescape off → the whole chain is the identity. Lone
    //    surrogates (to_str error) keep the Python path.
    if value.get_type().as_ptr() == h.safe_string.bind(py).as_ptr() {
        if let Ok(s) = value.cast::<PyString>() {
            return Ok(s.to_string_lossy().into_owned());
        }
    }
    if let Ok(s) = value.cast_exact::<PyString>() {
        if !rctx.autoescape()? {
            return Ok(s.to_string_lossy().into_owned());
        }
        if let Ok(rs) = s.to_str() {
            return Ok(match escape_html(rs) {
                Some(esc) => esc,
                None => rs.to_string(),
            });
        }
    }
    // 3) exact int (bool excluded): `template_localtime` passes ints through
    //    untouched, and `numberformat.format`'s int fast path returns every
    //    int unchanged (output `str(int)`, locale-independent) unless
    //    `USE_THOUSAND_SEPARATOR` enables grouping. Escaping is identity on
    //    the digit/minus output. Flag cached per render (LazyObject getattr).
    if let Ok(i) = value.cast_exact::<PyInt>() {
        let plain = match rctx.plain_int_str {
            Some(v) => v,
            None => {
                let v = !h
                    .settings
                    .bind(py)
                    .getattr("USE_THOUSAND_SEPARATOR")?
                    .is_truthy()?;
                rctx.plain_int_str = Some(v);
                v
            }
        };
        if plain {
            return Ok(i.str()?.to_string_lossy().into_owned());
        }
    }
    let value = match native_render(rctx, value)? {
        Native::Done(s) => return Ok(s),
        Native::Python => value.clone(),
        Native::Localtimed(v) => return python_render(rctx, v, true),
    };
    python_render(rctx, value, false)
}

/// Stock chain: `template_localtime` (unless already applied) → `localize`
/// → `conditional_escape(str(...))` / `str(...)`.
fn python_render<'py>(
    rctx: &mut Rctx<'py>,
    mut value: Bound<'py, PyAny>,
    localtimed: bool,
) -> PyResult<String> {
    let py = rctx.py();
    let h = helpers(py)?;
    if !localtimed {
        let kwargs = PyDict::new(py);
        kwargs.set_item("use_tz", opt_to_py(py, rctx.use_tz()?))?;
        value = h
            .template_localtime
            .bind(py)
            .call((value.clone(),), Some(&kwargs))?;
    }

    let kwargs = PyDict::new(py);
    kwargs.set_item("use_l10n", opt_to_py(py, rctx.use_l10n()?))?;
    value = h.localize.bind(py).call((value.clone(),), Some(&kwargs))?;

    if rctx.autoescape()? {
        if !value.is_instance_of::<PyString>() {
            value = value.str()?.into_any().clone();
        }
        py_str(&h.conditional_escape.bind(py).call1((value,))?)
    } else {
        py_str(&value)
    }
}

/// Outcome of the native `render_value_in_context` attempt.
enum Native<'py> {
    /// Fully rendered natively.
    Done(String),
    /// Run the whole stock chain.
    Python,
    /// `template_localtime` already ran (result carried); run the rest.
    Localtimed(Bound<'py, PyAny>),
}

/// Type objects for the native `render_value_in_context` dispatch.
struct ValTypes {
    decimal: Py<PyType>,
    date: Py<PyType>,
    datetime: Py<PyType>,
    time: Py<PyType>,
}

fn val_types(py: Python<'_>) -> PyResult<&ValTypes> {
    static VT: PyOnceLock<ValTypes> = PyOnceLock::new();
    VT.get_or_try_init(py, || -> PyResult<ValTypes> {
        let dt = py.import("datetime")?;
        let ty = |m: &Bound<'_, PyModule>, n: &str| -> PyResult<Py<PyType>> {
            Ok(m.getattr(n)?.cast_into::<PyType>()?.unbind())
        };
        Ok(ValTypes {
            decimal: ty(&py.import("decimal")?, "Decimal")?,
            date: ty(&dt, "date")?,
            datetime: ty(&dt, "datetime")?,
            time: ty(&dt, "time")?,
        })
    })
}

/// Autoescape the native output (`conditional_escape` of a plain `str` is
/// `escape`), or pass it through when autoescape is off.
fn finish(rctx: &Rctx<'_>, s: String) -> PyResult<String> {
    if rctx.autoescape()? {
        if let Some(esc) = escape_html(&s) {
            return Ok(esc);
        }
    }
    Ok(s)
}

/// Native `render_value_in_context` for the common non-`str`/`int` value
/// types, byte-identical with the stock chain:
/// - `None` / exact `bool`: `localize` returns `str(value)` (bool) or the
///   value itself (None) — `"None"`/`"True"`/`"False"`, escape-inert;
/// - exact `float` / `Decimal`: `localize` → `number_format` →
///   `numberformat.format` without grouping (`USE_THOUSAND_SEPARATOR` off)
///   and without `decimal_pos`: `str(value)` with the locale
///   `DECIMAL_SEPARATOR` substituted (`str(value)` when `use_l10n` is
///   False). Exponent notation, non-finite or huge `Decimal`s keep Python;
/// - exact `datetime` / `date`: `template_localtime` is the identity for
///   naive values (aware ones take the real call), then `date_format` with
///   `DATETIME_FORMAT` / `DATE_FORMAT` through the native dateformat port;
/// - any other object whose class is none of `str`/`int`/`float`/
///   `Decimal`/`date`/`time` (both `isinstance` steps miss, so
///   `template_localtime` and `localize` return it unchanged): `str()`
///   then the native escape (`__html__` honoured via `conditional_escape`
///   for non-plain `str` results).
fn native_render<'py>(
    rctx: &mut Rctx<'py>,
    value: &Bound<'py, PyAny>,
) -> PyResult<Native<'py>> {
    let py = rctx.py();
    if value.is_none() {
        return Ok(Native::Done("None".to_string()));
    }
    if let Ok(b) = value.cast_exact::<PyBool>() {
        return Ok(Native::Done(
            if b.is_true() { "True" } else { "False" }.to_string(),
        ));
    }
    let ty = value.get_type();
    let vt = val_types(py)?;
    let is_float = value.is_exact_instance_of::<PyFloat>();
    if is_float || ty.is(vt.decimal.bind(py)) {
        let repr = value.str()?;
        let Ok(s) = repr.to_str() else {
            return Ok(Native::Python);
        };
        // `localize(value, use_l10n=False)` → `str(value)`.
        if rctx.use_l10n()? == Some(false) {
            return Ok(Native::Done(s.to_string()));
        }
        // float: `"e" in str(number).lower()` → Decimal path (Python).
        // Decimal: `str` equals `"{:f}"` exactly when it has no exponent;
        // NaN/Infinity (5.2 raises, 6.1 returns str) and > 200-digit
        // values keep the Python path (100 chars bounds that sum by 200).
        if s.bytes().any(|c| matches!(c, b'e' | b'E' | b'N' | b'I' | b'n' | b'i'))
            || (!is_float && s.len() > 100)
        {
            return Ok(Native::Python);
        }
        if !rctx.int_str_is_plain()? {
            return Ok(Native::Python);
        }
        let Some(lang) = crate::filters::get_language(py) else {
            return Ok(Native::Python);
        };
        let sep = crate::filters::get_format(py, "DECIMAL_SEPARATOR", lang.as_deref(), true)?;
        let Ok(sep) = sep.cast_exact::<PyString>() else {
            return Ok(Native::Python);
        };
        let Ok(sep) = sep.to_str() else {
            return Ok(Native::Python);
        };
        let (sign, digits) = match s.strip_prefix('-') {
            Some(rest) => ("-", rest),
            None => ("", s),
        };
        let mut out = String::with_capacity(s.len() + sep.len());
        out.push_str(sign);
        match digits.split_once('.') {
            Some((int_part, dec_part)) => {
                out.push_str(int_part);
                if !dec_part.is_empty() {
                    out.push_str(sep);
                    out.push_str(dec_part);
                }
            }
            None => out.push_str(digits),
        }
        return finish(rctx, out).map(Native::Done);
    }
    let is_dt = ty.is(vt.datetime.bind(py));
    if is_dt || ty.is(vt.date.bind(py)) {
        let mut v = value.clone();
        if is_dt && !value.cast::<PyDateTime>()?.get_tzinfo().is_none() {
            // Possibly aware: the real `template_localtime` decides.
            let kwargs = PyDict::new(py);
            kwargs.set_item("use_tz", opt_to_py(py, rctx.use_tz()?))?;
            v = helpers(py)?
                .template_localtime
                .bind(py)
                .call((value.clone(),), Some(&kwargs))?;
            if !v.get_type().is(vt.datetime.bind(py)) {
                return Ok(Native::Localtimed(v));
            }
        }
        let l10n = rctx.use_l10n()?.unwrap_or(true);
        let lang = if l10n {
            match crate::filters::get_language(py) {
                Some(l) => l,
                None => return Ok(Native::Localtimed(v)),
            }
        } else {
            None
        };
        let fmt_type = if is_dt { "DATETIME_FORMAT" } else { "DATE_FORMAT" };
        let fmt = crate::filters::get_format(py, fmt_type, lang.as_deref(), l10n)?;
        let Ok(fmt) = fmt.cast_exact::<PyString>() else {
            return Ok(Native::Localtimed(v));
        };
        let Ok(fmt) = fmt.to_str() else {
            return Ok(Native::Localtimed(v));
        };
        return match crate::filters::dates::format_exact(py, &v, fmt)? {
            Some(s) => finish(rctx, s).map(Native::Done),
            None => Ok(Native::Localtimed(v)),
        };
    }
    // Generic objects: both `isinstance` chains miss iff the class is none
    // of the localizable bases AND `__class__` is not overridden (a proxy
    // reporting another class, e.g. `SimpleLazyObject`, keeps Python).
    match value.getattr(pyo3::intern!(py, "__class__")) {
        Ok(c) if c.is(&ty) => {}
        _ => return Ok(Native::Python),
    }
    if ty.is_subclass_of::<PyString>()?
        || ty.is_subclass_of::<PyInt>()?
        || ty.is_subclass_of::<PyFloat>()?
        || ty.is_subclass(vt.decimal.bind(py))?
        || ty.is_subclass(vt.date.bind(py))?
        || ty.is_subclass(vt.time.bind(py))?
    {
        return Ok(Native::Python);
    }
    let s = value.str()?;
    if !rctx.autoescape()? {
        return py_str(s.as_any()).map(Native::Done);
    }
    if s.is_exact_instance_of::<PyString>() {
        if let Ok(rs) = s.to_str() {
            return Ok(Native::Done(match escape_html(rs) {
                Some(esc) => esc,
                None => rs.to_string(),
            }));
        }
    }
    let h = helpers(py)?;
    if s.get_type().is(h.safe_string.bind(py)) {
        return Ok(Native::Done(s.to_string_lossy().into_owned()));
    }
    py_str(&h.conditional_escape.bind(py).call1((s,))?).map(Native::Done)
}

pub(crate) fn opt_to_py(py: Python<'_>, v: Option<bool>) -> Py<PyAny> {
    match v {
        None => py.None(),
        Some(b) => PyBool::new(py, b).to_owned().into_any().unbind(),
    }
}

#[cfg(test)]
mod tests {
    use super::escape_html;

    fn reference(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#x27;")
    }

    #[test]
    fn escape_html_matches_reference() {
        let mut cases: Vec<String> = ["", "plain", "&<>\"'", "é<✓>&", "&amp;"]
            .iter()
            .map(ToString::to_string)
            .collect();
        // A special at every offset across block boundaries and the tail.
        for len in [15, 16, 17, 31, 32, 33, 47, 50] {
            for pos in 0..len {
                let mut s = "x".repeat(len);
                s.replace_range(pos..=pos, "<");
                cases.push(s);
            }
        }
        cases.push("é".repeat(9) + "'" + &"✓".repeat(7));
        for s in &cases {
            let got = escape_html(s);
            let want = reference(s);
            if want == *s {
                assert_eq!(got, None, "{s:?}");
            } else {
                assert_eq!(got.as_deref(), Some(want.as_str()), "{s:?}");
            }
        }
    }
}
