//! Native implementations of Django's built-in filters.
//!
//! A native filter replaces the bridged Python call for the *stock* filters
//! in `django.template.defaultfilters` only. The overlay is installed at
//! registry-harvest time (`native_for`): an entry is native iff its name is
//! in `lookup` AND the registered function's `__module__` is
//! `django.template.defaultfilters` (a third-party filter shadowing a
//! built-in name keeps the Python path).
//!
//! - `stringfilter`-based natives receive the *stringified* input
//!   (`first = str(first)`) and return the body's result;
//! - stock marks the result in TWO layers: the generic outer layer
//!   (`getattr(func,"is_safe") and isinstance(raw_obj, SafeData)`) and the
//!   `stringfilter` inner layer (`isinstance(str(raw_obj), SafeData)` on the
//!   COERCED input). `apply_filter` reproduces both; `string_coerced` on the
//!   entry (from `__wrapped__`) selects the second layer;
//! - `needs_autoescape` filters receive `ae = Some(context.autoescape)`;
//!   the default `autoescape=True` kwarg is `unwrap_or(true)`.
//!
//! Natives that wrap Django utilities (escapejs, Truncator, dateformat, …)
//! call the *real* Python implementation, so branch deltas (e.g. 6.1's
//! `escapejs` C1 control range) apply automatically. `floatformat` and
//! `urlize` are full ports with per-branch detection and Python fallbacks
//! (see their modules; a mocked `Urlizer` disables the native path).

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::type_object::PyTypeInfo;
use pyo3::types::PyDict;

use crate::render::context::Rctx;

pub(crate) mod dates;
mod floatformat;
mod html;
mod lists;
mod misc;
mod numbers;
mod strings;
mod urlize;

/// A native built-in filter.
///
/// `input` is the live value (BEFORE `stringfilter` str-coercion — each
/// native applies its own layer); `args` are the resolved positional
/// arguments after the input (literals arrive `mark_safe`'d exactly like on
/// the bridged path); `ae` is `Some(context.autoescape)` for
/// `needs_autoescape` filters. The return value is what the Python function
/// would have returned (body-internal `mark_safe` calls included).
pub type NativeFn = for<'py> fn(
    &mut Rctx<'py>,
    &Bound<'py, PyAny>,
    &[Bound<'py, PyAny>],
    Option<bool>,
) -> PyResult<Bound<'py, PyAny>>;

/// Native implementation for a stock built-in name, `None` for filters that
/// stay bridged (`filesizeformat`, `striptags`, `dictsort`,
/// `dictsortreversed`, `truncatechars_html`, `truncatewords_html`) or
/// unknown names.
pub(crate) fn lookup(name: &str) -> Option<NativeFn> {
    match name {
        // strings
        "addslashes" => Some(strings::addslashes),
        "capfirst" => Some(strings::capfirst),
        "center" => Some(strings::center),
        "cut" => Some(strings::cut),
        "stringformat" => Some(strings::stringformat),
        "slugify" => Some(strings::slugify),
        "title" => Some(strings::title),
        "truncatechars" => Some(strings::truncatechars),
        "truncatewords" => Some(strings::truncatewords),
        "upper" => Some(strings::upper),
        "lower" => Some(strings::lower),
        "wordcount" => Some(strings::wordcount),
        "wordwrap" => Some(strings::wordwrap),
        "ljust" => Some(strings::ljust),
        "rjust" => Some(strings::rjust),
        // lists
        "first" => Some(lists::first),
        "join" => Some(lists::join),
        "last" => Some(lists::last),
        "length" => Some(lists::length),
        "make_list" => Some(lists::make_list),
        "random" => Some(lists::random),
        "slice" => Some(lists::slice_filter),
        "unordered_list" => Some(html::unordered_list),
        // numbers
        "add" => Some(numbers::add),
        "get_digit" => Some(numbers::get_digit),
        "divisibleby" => Some(numbers::divisibleby),
        "pluralize" => Some(numbers::pluralize),
        "floatformat" => Some(floatformat::floatformat),
        // html strings
        "escape" => Some(html::escape_filter),
        "escapeseq" => Some(html::escapeseq),
        "force_escape" => Some(html::force_escape),
        "escapejs" => Some(html::escapejs_filter),
        "json_script" => Some(html::json_script),
        "linebreaks" => Some(html::linebreaks_filter),
        "linebreaksbr" => Some(html::linebreaksbr),
        "linenumbers" => Some(html::linenumbers),
        "safe" => Some(html::safe),
        "safeseq" => Some(html::safeseq),
        "urlize" => Some(urlize::urlize),
        "urlizetrunc" => Some(urlize::urlizetrunc),
        "urlencode" => Some(html::urlencode),
        "iriencode" => Some(html::iriencode),
        // dates
        "date" => Some(dates::date),
        "time" => Some(dates::time),
        "timesince" => Some(dates::timesince_filter),
        "timeuntil" => Some(dates::timeuntil_filter),
        // logic / misc
        "default" => Some(misc::default),
        "default_if_none" => Some(misc::default_if_none),
        "yesno" => Some(misc::yesno),
        "phone2numeric" => Some(misc::phone2numeric),
        "pprint" => Some(misc::pprint),
        _ => None,
    }
}

/// The native overlay: a name in `lookup` maps to Rust ONLY when the
/// registered function really comes from `django.template.defaultfilters`.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn native_for(
    _py: Python<'_>,
    func: &Bound<'_, PyAny>,
    name: &str,
) -> PyResult<Option<NativeFn>> {
    let Some(nf) = lookup(name) else {
        return Ok(None);
    };
    let module: String = func
        .getattr("__module__")
        .and_then(|m| m.extract())
        .unwrap_or_default();
    if module == "django.template.defaultfilters" {
        Ok(Some(nf))
    } else {
        Ok(None)
    }
}

/// `True` iff `e` is an instance of exception type `T` (Python class check).
pub(crate) fn is_err<T: PyTypeInfo>(py: Python<'_>, e: &PyErr) -> bool {
    e.is_instance_of::<T>(py)
}

/// Snapshot of the live translation settings that can change the output of
/// lazily-translated filter text (Django resolves those per call, so caches
/// must key on the current values). `None` on any lookup failure — callers
/// then fall back to the Python path (which raises the same errors stock
/// would).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct TransState {
    pub lang: String,
    pub i18n: bool,
}

/// Live `(language, USE_I18N)` snapshot.
///
/// The language is read through `trans_real._active`'s backing contextvar
/// (~0.1 µs `.get`) instead of `get_language()` (~7 µs through the asgiref
/// `Local.__getattr__` chain). Stock semantics: `_active.value.to_language()`
/// when this thread set `_active`, else `settings.LANGUAGE_CODE` (which also
/// covers `USE_I18N=False`, where `trans_null` ignores `_active` entirely).
/// Any unexpected shape → `None`; callers then fall back to the Python path.
pub(crate) fn trans_state(py: Python<'_>) -> Option<TransState> {
    let i18n = setting(py, "USE_I18N")?.is_truthy().ok()?;
    if !i18n {
        return Some(TransState {
            lang: String::new(),
            i18n: false,
        });
    }
    let lang = match cvar_language(py) {
        CvarLang::Lang(Some(l)) => l,
        _ => setting(py, "LANGUAGE_CODE")?.extract().ok()?,
    };
    Some(TransState { lang, i18n: true })
}

/// Live `settings.<name>`.
///
/// Django caches every setting value it reads in `LazySettings.__dict__`
/// and pops those entries on `override_settings` / direct assignment, so
/// reading the instance `__dict__` directly sees every change while
/// skipping the proxy `__getattr__` (~0.4 us/lookup). Cold entries fall
/// back to plain getattr, which re-populates the cache.
pub(crate) fn setting<'py>(py: Python<'py>, name: &str) -> Option<Bound<'py, PyAny>> {
    static SETTINGS: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    static DICT: PyOnceLock<Py<pyo3::types::PyDict>> = PyOnceLock::new();
    let settings = SETTINGS
        .get_or_try_init(py, || -> PyResult<Py<PyAny>> {
            Ok(py.import("django.conf")?.getattr("settings")?.unbind())
        })
        .ok()?
        .bind(py);
    // The LazySettings instance `__dict__` is one live dict object mutated
    // in place (values cached on read, popped on override) — its identity
    // is stable, so only the contents are re-read per call.
    let cache = DICT
        .get_or_try_init(py, || -> PyResult<Py<pyo3::types::PyDict>> {
            settings
                .getattr("__dict__")?
                .cast_into::<pyo3::types::PyDict>()
                .map(Bound::unbind)
                .map_err(Into::into)
        })
        .ok()?
        .bind(py);
    match cache.get_item(name).ok().flatten() {
        Some(v) => Some(v),
        None => settings.getattr(name).ok(),
    }
}

/// Native `django.utils.translation.get_language()`: `Some(lang)` with
/// `lang = None` after `deactivate_all()` (whose `to_language` returns
/// None); outer `None` when the answer cannot be derived natively (callers
/// take the Python path).
pub(crate) fn get_language(py: Python<'_>) -> Option<Option<String>> {
    if !setting(py, "USE_I18N")?.is_truthy().ok()? {
        // `trans_null.get_language` → `settings.LANGUAGE_CODE`.
        return setting(py, "LANGUAGE_CODE")?.extract().ok();
    }
    match cvar_language(py) {
        CvarLang::Lang(l) => Some(l),
        CvarLang::Unset => setting(py, "LANGUAGE_CODE")?.extract().ok(),
        CvarLang::Unknown => None,
    }
}

/// Outcome of reading `trans_real._active` natively.
enum CvarLang {
    /// No translation active for this thread (stock falls back to
    /// `settings.LANGUAGE_CODE`).
    Unset,
    /// `_active.value.to_language()` (None after `deactivate_all()`).
    Lang(Option<String>),
    /// Unexpected shape / failed call.
    Unknown,
}

/// Language from `trans_real._active`'s contextvar: the stored `_Storage`
/// must be tagged to the current thread (`_CVar._storage()` semantics —
/// storage merely inherited from another thread stays invisible, exactly
/// like stock).
fn cvar_language(py: Python<'_>) -> CvarLang {
    static ACTIVE_CVAR: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    static GET_IDENT: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    let inner = || -> Option<CvarLang> {
        let cvar = ACTIVE_CVAR
            .get_or_try_init(py, || -> PyResult<Py<PyAny>> {
                local_cvar(py, "django.utils.translation.trans_real", "_active")
                    .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("no _active cvar"))
            })
            .ok()?
            .bind(py);
        let storage = cvar.call_method1("get", (py.None(),)).ok()?;
        if storage.is_none() {
            return Some(CvarLang::Unset);
        }
        let get_ident = GET_IDENT
            .get_or_try_init(py, || -> PyResult<Py<PyAny>> {
                Ok(py.import("threading")?.getattr("get_ident")?.unbind())
            })
            .ok()?
            .bind(py);
        let ident = get_ident.call0().ok()?;
        let tid = storage.getattr("thread_id").ok()?;
        if !tid.eq(&ident).ok()? {
            return Some(CvarLang::Unset);
        }
        let Some(active) = storage
            .getattr("data")
            .ok()?
            .cast_into::<PyDict>()
            .ok()?
            .get_item("value")
            .ok()?
        else {
            return Some(CvarLang::Unset);
        };
        if active.is_none() {
            return Some(CvarLang::Unset);
        }
        let l = active.call_method0("to_language").ok()?;
        if l.is_none() {
            return Some(CvarLang::Lang(None));
        }
        Some(CvarLang::Lang(Some(l.extract().ok()?)))
    };
    inner().unwrap_or(CvarLang::Unknown)
}

/// `django.utils.formats.get_format(format_type, lang, use_l10n=use_l10n)`
/// with `lang` already resolved the way stock resolves it (`get_language()`
/// when localizing, else `None`). A hit reads the live
/// `formats._format_cache` entry directly (the module global is rebound by
/// `reset_format_cache`, so it is re-read per call); a miss calls the real
/// `get_format`, which populates it.
pub(crate) fn get_format<'py>(
    py: Python<'py>,
    format_type: &str,
    lang: Option<&str>,
    use_l10n: bool,
) -> PyResult<Bound<'py, PyAny>> {
    static FORMATS: PyOnceLock<Py<PyDict>> = PyOnceLock::new();
    static GET_FORMAT: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    let globals = FORMATS
        .get_or_try_init(py, || -> PyResult<Py<PyDict>> {
            Ok(py.import("django.utils.formats")?.dict().unbind())
        })?
        .bind(py);
    let key = (format_type, lang).into_pyobject(py)?;
    if let Some(cache) = globals.get_item("_format_cache")? {
        if let Ok(cache) = cache.cast::<PyDict>() {
            if let Some(v) = cache.get_item(&key)? {
                return Ok(v);
            }
        }
    }
    let f = GET_FORMAT
        .get_or_try_init(py, || -> PyResult<Py<PyAny>> {
            Ok(py.import("django.utils.formats")?.getattr("get_format")?.unbind())
        })?
        .bind(py);
    let kw = PyDict::new(py);
    kw.set_item("use_l10n", use_l10n)?;
    f.call((format_type, lang), Some(&kw))
}

/// Fetch the `contextvars.ContextVar` backing an `asgiref.local.Local`
/// (`local._storage._data`); `None` if the shape is not the expected one.
pub(crate) fn local_cvar(py: Python<'_>, module: &str, name: &str) -> Option<Py<PyAny>> {
    let module_obj = py.import(module).ok()?;
    let local = module_obj.getattr(name).ok()?;
    let storage = local.getattr("_storage").ok()?;
    let cvar: Bound<'_, PyAny> = storage.getattr("_data").ok()?;
    Some(cvar.unbind())
}

/// Cached Python utilities used by the native filter bodies. One process
/// wide init; `PyOnceLock` (GIL-safe) like `value::HELPERS`.
pub(crate) struct Utils {
    pub escape: Py<PyAny>,
    pub escapejs: Py<PyAny>,
    pub json_script: Py<PyAny>,
    pub linebreaks: Py<PyAny>,
    pub urlize: Py<PyAny>,
    pub normalize_newlines: Py<PyAny>,
    pub wrap: Py<PyAny>,
    pub slugify: Py<PyAny>,
    pub phone2numeric: Py<PyAny>,
    pub truncator: Py<PyAny>,
    pub iri_to_uri: Py<PyAny>,
    pub quote: Py<PyAny>,
    pub choice: Py<PyAny>,
    pub pformat: Py<PyAny>,
    pub gettext: Py<PyAny>,
    pub pgettext: Py<PyAny>,
    pub date_format: Py<PyAny>,
    pub time_format: Py<PyAny>,
    pub df_format: Py<PyAny>,
    pub df_time_format: Py<PyAny>,
    pub timesince: Py<PyAny>,
    pub timeuntil: Py<PyAny>,
    pub float_builtin: Py<PyAny>,
    pub slice_builtin: Py<PyAny>,
    pub title_lower_re: regex::Regex,
    pub title_digit_re: regex::Regex,
    pub types_generator: Py<PyAny>,
    /// 6.1+ `center` uses `f"{value:^{width}}"` (odd pad RIGHT); 5.2 uses
    /// `str.center` (odd pad LEFT).
    pub center_via_format: bool,
}

static UTILS: PyOnceLock<Utils> = PyOnceLock::new();

pub(crate) fn is_ve(py: Python<'_>, e: &PyErr) -> bool {
    is_err::<pyo3::exceptions::PyValueError>(py, e)
}

pub(crate) fn is_te(py: Python<'_>, e: &PyErr) -> bool {
    is_err::<pyo3::exceptions::PyTypeError>(py, e)
}

pub(crate) fn is_ae(py: Python<'_>, e: &PyErr) -> bool {
    is_err::<pyo3::exceptions::PyAttributeError>(py, e)
}

pub(crate) fn is_ie(py: Python<'_>, e: &PyErr) -> bool {
    is_err::<pyo3::exceptions::PyIndexError>(py, e)
}

pub(crate) fn utils(py: Python<'_>) -> PyResult<&Utils> {
    UTILS.get_or_try_init(py, || -> PyResult<Utils> {
        let html_mod = py.import("django.utils.html")?;
        let text = py.import("django.utils.text")?;
        let encoding = py.import("django.utils.encoding")?;
        let formats = py.import("django.utils.formats")?;
        let dateformat = py.import("django.utils.dateformat")?;
        let timesince_mod = py.import("django.utils.timesince")?;
        Ok(Utils {
            escape: html_mod.getattr("escape")?.unbind(),
            escapejs: html_mod.getattr("escapejs")?.unbind(),
            json_script: html_mod.getattr("json_script")?.unbind(),
            linebreaks: html_mod.getattr("linebreaks")?.unbind(),
            urlize: html_mod.getattr("urlize")?.unbind(),
            normalize_newlines: text.getattr("normalize_newlines")?.unbind(),
            wrap: text.getattr("wrap")?.unbind(),
            slugify: text.getattr("slugify")?.unbind(),
            phone2numeric: text.getattr("phone2numeric")?.unbind(),
            truncator: text.getattr("Truncator")?.unbind(),
            iri_to_uri: encoding.getattr("iri_to_uri")?.unbind(),
            quote: py.import("urllib.parse")?.getattr("quote")?.unbind(),
            choice: py.import("random")?.getattr("choice")?.unbind(),
            pformat: py.import("pprint")?.getattr("pformat")?.unbind(),
            gettext: py
                .import("django.utils.translation")?
                .getattr("gettext")?
                .unbind(),
            pgettext: py
                .import("django.utils.translation")?
                .getattr("pgettext")?
                .unbind(),
            date_format: formats.getattr("date_format")?.unbind(),
            time_format: formats.getattr("time_format")?.unbind(),
            df_format: dateformat.getattr("format")?.unbind(),
            df_time_format: dateformat.getattr("time_format")?.unbind(),
            timesince: timesince_mod.getattr("timesince")?.unbind(),
            timeuntil: timesince_mod.getattr("timeuntil")?.unbind(),
            float_builtin: py.eval(c"float", None, None)?.unbind(),
            slice_builtin: py.eval(c"slice", None, None)?.unbind(),
            title_lower_re: regex::Regex::new(r"([a-z])'([A-Z])").map_err(|e| {
                pyo3::exceptions::PyRuntimeError::new_err(format!("title regex: {e}"))
            })?,
            title_digit_re: regex::Regex::new(r"\d([A-Z])").map_err(|e| {
                pyo3::exceptions::PyRuntimeError::new_err(format!("title regex: {e}"))
            })?,
            types_generator: py.import("types")?.getattr("GeneratorType")?.unbind(),
            center_via_format: {
                let ver = py.import("django")?.getattr("VERSION")?;
                let major: i64 = ver.get_item(0)?.extract()?;
                let minor: i64 = ver.get_item(1)?.extract()?;
                (major, minor) >= (6, 1)
            },
        })
    })
}
