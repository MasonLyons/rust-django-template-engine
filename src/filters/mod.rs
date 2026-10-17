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
use pyo3::types::{PyDict, PyString};

use crate::render::context::Rctx;

pub(crate) mod dates;
mod floatformat;
mod html;
mod humanize;
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
/// stay bridged (`filesizeformat`, `dictsort`,
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
        "striptags" => Some(strings::striptags),
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
        return Ok(humanize_native(func));
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

/// Natives for stock `django.contrib.humanize` filters, matched by module
/// and function name.
fn humanize_native(func: &Bound<'_, PyAny>) -> Option<NativeFn> {
    let module: String = func.getattr("__module__").and_then(|m| m.extract()).ok()?;
    if module != "django.contrib.humanize.templatetags.humanize" {
        return None;
    }
    let qualname: String = func.getattr("__qualname__").and_then(|m| m.extract()).ok()?;
    match qualname.as_str() {
        "intcomma" => Some(humanize::intcomma),
        "naturaltime" => Some(humanize::naturaltime),
        _ => None,
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
    /// Bit 0: `gettext` resolved to `trans_real`; bit 1: `pgettext` did.
    /// `_trans` resolves each function independently (on first use, by the
    /// `USE_I18N` of that moment), so they can disagree after
    /// `override_settings(USE_I18N=...)`; month/weekday tables mix both.
    pub real: u8,
}

/// Live `(language, USE_I18N)` snapshot.
///
/// The language is read through `trans_real._active`'s backing contextvar
/// (~0.1 µs `.get`) instead of `get_language()` (~7 µs through the asgiref
/// `Local.__getattr__` chain). Stock semantics: `_active.value.to_language()`
/// when this thread set `_active`, else `settings.LANGUAGE_CODE` (which also
/// covers `trans_null`, which ignores `_active` entirely; see `trans_impl`).
/// Any unexpected shape → `None`; callers then fall back to the Python path.
pub(crate) fn trans_state(py: Python<'_>) -> Option<TransState> {
    let mut real = 0u8;
    for (bit, name) in [(1u8, "gettext"), (2u8, "pgettext")] {
        if let TransImpl::Real = trans_impl(py, name)? {
            real |= bit;
        }
    }
    if real == 0 {
        return Some(TransState {
            lang: String::new(),
            real,
        });
    }
    // The catalog `trans_real.gettext` uses: the active translation, else
    // `_default` (`settings.LANGUAGE_CODE`) — not `get_language()`, which
    // `_trans` may route to `trans_null` independently.
    let lang = match cvar_language(py) {
        CvarLang::Lang(Some(l)) => l,
        // `deactivate_all()`: a `NullTranslations` (untranslated text).
        CvarLang::Lang(None) => "\0deactivated".to_string(),
        CvarLang::Unset => setting(py, "LANGUAGE_CODE")?.extract().ok()?,
        CvarLang::Unknown => return None,
    };
    Some(TransState { lang, real })
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

/// Which module `django.utils.translation._trans.<name>` dispatches to.
enum TransImpl {
    Real,
    Null,
}

/// `Trans.__getattr__` resolves each function once (by `settings.USE_I18N`
/// at that moment) and caches it on `_trans`; Django never clears that cache
/// when `USE_I18N` changes later. Follow the cached choice, and use the
/// setting only for a not-yet-resolved name. `None` for anything else (a
/// patched function): callers take the Python path.
fn trans_impl(py: Python<'_>, name: &str) -> Option<TransImpl> {
    static MODS: PyOnceLock<(Py<PyDict>, Py<PyDict>, Py<PyDict>)> = PyOnceLock::new();
    let (trans, real, null) = MODS
        .get_or_try_init(py, || -> PyResult<_> {
            let tr = py.import("django.utils.translation")?;
            Ok((
                tr.getattr("_trans")?.getattr("__dict__")?.cast_into::<PyDict>()?.unbind(),
                py.import("django.utils.translation.trans_real")?.dict().unbind(),
                py.import("django.utils.translation.trans_null")?.dict().unbind(),
            ))
        })
        .ok()?;
    let Some(resolved) = trans.bind(py).get_item(name).ok()? else {
        return Some(if setting(py, "USE_I18N")?.is_truthy().ok()? {
            TransImpl::Real
        } else {
            TransImpl::Null
        });
    };
    let is = |d: &Py<PyDict>| {
        d.bind(py)
            .get_item(name)
            .ok()
            .flatten()
            .is_some_and(|f| f.is(&resolved))
    };
    if is(real) {
        Some(TransImpl::Real)
    } else if is(null) {
        Some(TransImpl::Null)
    } else {
        None
    }
}

/// Native `django.utils.translation.get_language()`: `Some(lang)` with
/// `lang = None` after `deactivate_all()` (whose `to_language` returns
/// None); outer `None` when the answer cannot be derived natively (callers
/// take the Python path).
pub(crate) fn get_language(py: Python<'_>) -> Option<Option<String>> {
    if let TransImpl::Null = trans_impl(py, "get_language")? {
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
    let inner = || -> Option<CvarLang> {
        let cvar = TRANS_ACTIVE.cvar(py)?;
        let Some(active) = local_value(py, &cvar)? else {
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

/// `active_translation.gettext(eol)` — the body of `trans_real.gettext`
/// minus the asgiref `Local` chain: once `_trans` has resolved `gettext` to
/// `trans_real.gettext`, the active translation is read from
/// `trans_real._active`'s contextvar (else the module's `_default`) and its
/// own `.gettext` is called. `Ok(None)` (caller takes the real Python
/// function) for an unresolved/patched `_trans` (e.g. `USE_I18N=False`
/// resolves to `trans_null`), an empty message or an unset `_default`.
fn fast_gettext<'py>(py: Python<'py>, eol: &str) -> PyResult<Option<Bound<'py, PyAny>>> {
    static MODS: PyOnceLock<(Py<PyDict>, Py<PyDict>, Py<PyAny>)> = PyOnceLock::new();
    if eol.is_empty() {
        return Ok(None);
    }
    let (trans_dict, real_dict, real_gettext) = MODS.get_or_try_init(py, || -> PyResult<_> {
        let tr = py.import("django.utils.translation")?;
        let real = py.import("django.utils.translation.trans_real")?;
        Ok((
            tr.getattr("_trans")?.getattr("__dict__")?.cast_into::<PyDict>()?.unbind(),
            real.dict().unbind(),
            real.getattr("gettext")?.unbind(),
        ))
    })?;
    // `Trans.__getattr__` caches the resolved function on first use;
    // anything else there (trans_null, a patch) → Python path.
    let Some(resolved) = trans_dict.bind(py).get_item("gettext")? else {
        return Ok(None);
    };
    if !resolved.is(real_gettext.bind(py)) {
        return Ok(None);
    }
    let Some(cvar) = TRANS_ACTIVE.cvar(py) else {
        return Ok(None);
    };
    let Some(active) = local_value(py, &cvar) else {
        return Ok(None);
    };
    let obj = match active {
        Some(a) => a,
        None => match real_dict.bind(py).get_item("_default")? {
            Some(d) if !d.is_none() => d,
            _ => return Ok(None),
        },
    };
    Ok(Some(obj.call_method1(pyo3::intern!(py, "gettext"), (eol,))?))
}

/// Which translation `django.utils.translation.<fname>` would consult.
enum TransObj<'py> {
    /// `_trans.<fname>` is `trans_real.<fname>`: the active translation
    /// object (else `_default`).
    Real(Bound<'py, PyAny>),
    /// `_trans.<fname>` is `trans_null.<fname>` (no translation at all).
    Null,
}

/// The public `django.utils.translation` module functions (Python path).
fn translation_fn<'py>(py: Python<'py>, name: &str) -> PyResult<Bound<'py, PyAny>> {
    static MOD: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    MOD.get_or_try_init(py, || -> PyResult<Py<PyAny>> {
        Ok(py.import("django.utils.translation")?.into_any().unbind())
    })?
    .bind(py)
    .getattr(name)
}

/// `CONTEXT_SEPARATOR in result` for a translation result.
fn has_ctx_sep(r: &Bound<'_, PyAny>) -> PyResult<bool> {
    r.contains("\x04")
}

/// Native `django.utils.translation.gettext` / `pgettext` / `ngettext` /
/// `npgettext` for plain `str` messages: once `_trans` has resolved the
/// function to `trans_real`, the active translation object's own method is
/// called directly (skipping the module functions and the asgiref `Local`
/// chain); `trans_null` is answered natively; anything else (unresolved,
/// patched, unreadable state) calls the real module function. The active
/// translation is read once per `Translator`, so callers making several
/// lookups for one value share it.
#[allow(clippy::option_option)] // not read / unreadable / set, like `local_value`
pub(crate) struct Translator<'py> {
    py: Python<'py>,
    /// `_active.value` read from its contextvar: outer `None` = not read
    /// yet, `Some(None)` = unreadable.
    active: std::cell::OnceCell<Option<Option<Bound<'py, PyAny>>>>,
}

impl<'py> Translator<'py> {
    pub(crate) fn new(py: Python<'py>) -> Self {
        Self {
            py,
            active: std::cell::OnceCell::new(),
        }
    }

    pub(crate) fn py(&self) -> Python<'py> {
        self.py
    }

    /// The translation `trans_real.<fname>` would use: `_active.value` when
    /// set, else `_default`. An `_active.value` explicitly set to `None` is
    /// skipped by `do_ntranslate` (`ntranslate`) but crashes `gettext`, so
    /// the latter takes the Python path. `None` also for a patched `_trans`,
    /// an unreadable `_active` or an unset `_default` (the Python function
    /// then initializes it).
    fn obj(&self, fname: &str, ntranslate: bool) -> PyResult<Option<TransObj<'py>>> {
        static REAL: PyOnceLock<Py<PyDict>> = PyOnceLock::new();
        let py = self.py;
        match trans_impl(py, fname) {
            None => return Ok(None),
            Some(TransImpl::Null) => return Ok(Some(TransObj::Null)),
            Some(TransImpl::Real) => {}
        }
        let active = self.active.get_or_init(|| {
            let cvar = TRANS_ACTIVE.cvar(py)?;
            local_value(py, &cvar)
        });
        let Some(active) = active else {
            return Ok(None);
        };
        match active {
            Some(a) if !a.is_none() => return Ok(Some(TransObj::Real(a.clone()))),
            Some(_) if !ntranslate => return Ok(None),
            _ => {}
        }
        let real = REAL
            .get_or_try_init(py, || -> PyResult<Py<PyDict>> {
                Ok(py.import("django.utils.translation.trans_real")?.dict().unbind())
            })?
            .bind(py);
        Ok(match real.get_item("_default")? {
            Some(d) if !d.is_none() => Some(TransObj::Real(d)),
            _ => None,
        })
    }

    /// `gettext(message)`.
    pub(crate) fn gettext(&self, message: &str) -> PyResult<Bound<'py, PyAny>> {
        let py = self.py;
        let eol = eol_normalize(message);
        if !eol.is_empty() {
            match self.obj("gettext", false)? {
                Some(TransObj::Real(t)) => {
                    return t.call_method1(pyo3::intern!(py, "gettext"), (eol.as_ref(),));
                }
                Some(TransObj::Null) => return Ok(PyString::new(py, message).into_any()),
                None => {}
            }
        }
        translation_fn(py, "gettext")?.call1((message,))
    }

    /// `pgettext(context, message)`.
    pub(crate) fn pgettext(&self, context: &str, message: &str) -> PyResult<Bound<'py, PyAny>> {
        let py = self.py;
        match self.obj("pgettext", false)? {
            Some(TransObj::Null) => Ok(PyString::new(py, message).into_any()),
            // `trans_real.pgettext` → its module-global `gettext` (never an
            // empty message: it holds the separator).
            Some(TransObj::Real(t)) => {
                let ctxt = format!("{context}\x04{message}");
                let r = t.call_method1(
                    pyo3::intern!(py, "gettext"),
                    (eol_normalize(&ctxt).as_ref(),),
                )?;
                if has_ctx_sep(&r)? {
                    return Ok(PyString::new(py, message).into_any());
                }
                Ok(r)
            }
            None => translation_fn(py, "pgettext")?.call1((context, message)),
        }
    }

    /// `ngettext(singular, plural, number)`.
    pub(crate) fn ngettext(
        &self,
        singular: &str,
        plural: &str,
        number: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let py = self.py;
        match self.obj("ngettext", true)? {
            Some(TransObj::Real(t)) => {
                t.call_method1(pyo3::intern!(py, "ngettext"), (singular, plural, number))
            }
            Some(TransObj::Null) => Ok(PyString::new(
                py,
                if number.eq(1)? { singular } else { plural },
            )
            .into_any()),
            None => translation_fn(py, "ngettext")?.call1((singular, plural, number)),
        }
    }

    /// `npgettext(context, singular, plural, number)`.
    pub(crate) fn npgettext(
        &self,
        context: &str,
        singular: &str,
        plural: &str,
        number: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let py = self.py;
        match self.obj("npgettext", true)? {
            // `trans_real.npgettext` → its module-global `ngettext`
            // (`do_ntranslate`): the contextual pair, then the bare one when
            // the result still holds the separator (untranslated).
            Some(TransObj::Real(t)) => {
                let name = pyo3::intern!(py, "ngettext");
                let r = t.call_method1(
                    name,
                    (
                        format!("{context}\x04{singular}"),
                        format!("{context}\x04{plural}"),
                        number,
                    ),
                )?;
                if has_ctx_sep(&r)? {
                    return t.call_method1(name, (singular, plural, number));
                }
                Ok(r)
            }
            Some(TransObj::Null) => Ok(PyString::new(
                py,
                if number.eq(1)? { singular } else { plural },
            )
            .into_any()),
            None => translation_fn(py, "npgettext")?.call1((context, singular, plural, number)),
        }
    }
}

fn eol_normalize(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains('\r') {
        std::borrow::Cow::Owned(s.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// `str(django.utils.translation.gettext(mark_safe(msgid)))`.
pub(crate) fn gettext_safe<'py>(py: Python<'py>, msgid: &str) -> PyResult<Bound<'py, PyString>> {
    if let Some(r) = fast_gettext(py, &eol_normalize(msgid))? {
        return r.str();
    }
    let safe = crate::render::value::mark_safe_bound(&PyString::new(py, msgid).into_any())?;
    utils(py)?.gettext.bind(py).call1((safe,))?.str()
}

/// `str(value)` for a `gettext_lazy(message)` proxy, through the fast
/// gettext path: the proxy's `__str__` is `str(gettext(*_args, **_kw))`,
/// and `trans_real.gettext` returns `mark_safe(result)` for a `SafeData`
/// message. `Ok(None)` for anything else (other lazy types, extra args,
/// message types other than exact `str`/`SafeString`, or no fast path) —
/// the caller then runs plain `str(value)`.
pub(crate) fn lazy_gettext_str<'py>(
    value: &Bound<'py, PyAny>,
) -> PyResult<Option<Bound<'py, PyString>>> {
    static PROXY: PyOnceLock<Option<Py<pyo3::types::PyType>>> = PyOnceLock::new();
    let py = value.py();
    let Some(proxy) = PROXY.get_or_init(py, || {
        py.import("django.utils.translation")
            .and_then(|m| m.getattr("gettext_lazy")?.call1(("",)))
            .map(|p| p.get_type().unbind())
            .ok()
    }) else {
        return Ok(None);
    };
    if !value.get_type().is(proxy.bind(py)) {
        return Ok(None);
    }
    let Ok(args) = value.getattr(pyo3::intern!(py, "_args")) else {
        return Ok(None);
    };
    let Ok(args) = args.cast_into::<pyo3::types::PyTuple>() else {
        return Ok(None);
    };
    if args.len() != 1 {
        return Ok(None);
    }
    match value.getattr(pyo3::intern!(py, "_kw")) {
        Ok(kw) => match kw.cast::<PyDict>() {
            Ok(d) if d.is_empty() => {}
            _ => return Ok(None),
        },
        Err(_) => return Ok(None),
    }
    let msg = args.get_item(0)?;
    let h = crate::render::value::helpers(py)?;
    let safe = msg.get_type().is(h.safe_string.bind(py));
    if !safe && !msg.is_exact_instance_of::<PyString>() {
        return Ok(None);
    }
    let Ok(text) = msg.cast::<PyString>()?.to_str() else {
        return Ok(None);
    };
    let Some(r) = fast_gettext(py, &eol_normalize(text))? else {
        return Ok(None);
    };
    if safe {
        return crate::render::value::mark_safe_bound(&r)?.str().map(Some);
    }
    r.str().map(Some)
}

/// `getattr(local, "value")` for an asgiref `Local` given its backing
/// contextvar (see `LocalSlot`): `Some(None)` when unset for this thread,
/// `Some(Some(v))` when set, outer `None` on an unexpected shape. The stored
/// `_Storage` must be tagged to the current thread (`_CVar._storage()`
/// semantics — storage merely inherited from another thread stays
/// invisible, exactly like stock).
#[allow(clippy::option_option)] // unset vs set vs unreadable, like `get_language`
pub(crate) fn local_value<'py>(
    py: Python<'py>,
    cvar: &Bound<'py, PyAny>,
) -> Option<Option<Bound<'py, PyAny>>> {
    static GET_IDENT: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    let storage = cvar.call_method1("get", (py.None(),)).ok()?;
    if storage.is_none() {
        return Some(None);
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
        return Some(None);
    }
    storage
        .getattr("data")
        .ok()?
        .cast_into::<PyDict>()
        .ok()?
        .get_item("value")
        .ok()
}

/// Native `django.utils.timezone.template_localtime(value, use_tz)` for an
/// EXACT `datetime.datetime`; `Ok(None)` means "call the Python function"
/// (other types, or state that can't be read natively).
///
/// Stock: convert iff `use_tz` (or `settings.USE_TZ` when None) and
/// `value.utcoffset() is not None` (an exact datetime has no
/// `convert_to_local_time` attribute), via `value.astimezone(tz)` with
/// `tz = getattr(_active, "value", get_default_timezone())`. The active
/// zone is read from `timezone._active`'s contextvar (~0.2 µs) instead of
/// the asgiref `Local.__getattr__` chain (~5 µs).
pub(crate) fn template_localtime_exact<'py>(
    value: &Bound<'py, PyAny>,
    use_tz: Option<bool>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    use pyo3::types::{PyDateTime, PyTzInfoAccess};
    let py = value.py();
    if value.get_type().as_ptr() != py.get_type::<PyDateTime>().as_ptr() {
        return Ok(None);
    }
    let dt = value.cast::<PyDateTime>()?;
    // tzinfo None ⇒ utcoffset() None ⇒ naive ⇒ identity.
    if dt.get_tzinfo().is_none() {
        return Ok(Some(value.clone()));
    }
    let use_tz = match use_tz {
        Some(b) => b,
        None => match setting(py, "USE_TZ") {
            Some(v) => v.is_truthy()?,
            None => return Ok(None),
        },
    };
    if !use_tz || value.call_method0(pyo3::intern!(py, "utcoffset"))?.is_none() {
        return Ok(Some(value.clone()));
    }
    let Some(tz) = current_timezone(py)? else {
        return Ok(None);
    };
    Ok(Some(value.call_method1(pyo3::intern!(py, "astimezone"), (tz,))?))
}

/// `django.utils.timezone.get_current_timezone()` with `_active` read from
/// its contextvar; `Ok(None)` when that can't be done natively.
pub(crate) fn current_timezone(py: Python<'_>) -> PyResult<Option<Bound<'_, PyAny>>> {
    static TZ_MOD: PyOnceLock<Py<PyDict>> = PyOnceLock::new();
    let Some(cvar) = TZ_ACTIVE.cvar(py) else {
        return Ok(None);
    };
    let Some(active) = local_value(py, &cvar) else {
        return Ok(None);
    };
    // Stock evaluates the `getattr` default eagerly: always call it.
    let globals = TZ_MOD
        .get_or_try_init(py, || -> PyResult<Py<PyDict>> {
            Ok(py.import("django.utils.timezone")?.dict().unbind())
        })?
        .bind(py);
    let Some(gdt) = globals.get_item("get_default_timezone")? else {
        return Ok(None);
    };
    let default = gdt.call0()?;
    Ok(Some(active.unwrap_or(default)))
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

/// The `contextvars.ContextVar` backing a module-global `asgiref.local.Local`
/// (`local._storage._data`).
///
/// Django rebinds some of these globals at runtime (`trans_real._active =
/// Local()` on `setting_changed` and in the translation autoreloader), so
/// the global is re-read on every call and the cvar is re-derived whenever
/// it names a different `Local` object.
pub(crate) struct LocalSlot {
    module: &'static str,
    name: &'static str,
    dict: PyOnceLock<Py<PyDict>>,
    cached: std::sync::Mutex<Option<(Py<PyAny>, Py<PyAny>)>>,
}

impl LocalSlot {
    pub(crate) const fn new(module: &'static str, name: &'static str) -> Self {
        Self {
            module,
            name,
            dict: PyOnceLock::new(),
            cached: std::sync::Mutex::new(None),
        }
    }

    /// The current cvar; `None` if the global is missing or not shaped like
    /// a contextvar-backed `Local`.
    pub(crate) fn cvar<'py>(&self, py: Python<'py>) -> Option<Bound<'py, PyAny>> {
        let dict = self
            .dict
            .get_or_try_init(py, || -> PyResult<Py<PyDict>> {
                Ok(py.import(self.module)?.dict().unbind())
            })
            .ok()?
            .bind(py);
        let local = dict.get_item(self.name).ok()??;
        let mut cached = self.cached.lock().ok()?;
        if let Some((l, c)) = cached.as_ref() {
            if l.bind(py).is(&local) {
                return Some(c.bind(py).clone());
            }
        }
        let cvar = local.getattr("_storage").ok()?.getattr("_data").ok()?;
        *cached = Some((local.unbind(), cvar.clone().unbind()));
        Some(cvar)
    }
}

pub(crate) static TRANS_ACTIVE: LocalSlot =
    LocalSlot::new("django.utils.translation.trans_real", "_active");
pub(crate) static TZ_ACTIVE: LocalSlot = LocalSlot::new("django.utils.timezone", "_active");

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
    pub strip_tags: Py<PyAny>,
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
            strip_tags: html_mod.getattr("strip_tags")?.unbind(),
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
