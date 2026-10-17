//! Engine and Template pyclasses: bridging of Python filter libraries into
//! the Rust filter registry at Engine init, and the render protocol executed
//! against the live `django.template.context.Context`.

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyDict, PyList, PyModule, PyString, PyType, PyTuple};

use crate::bridge;
use crate::error::{syntax_error, template_does_not_exist_type};
use pyo3::exceptions::{PyAttributeError, PyKeyError, PyTypeError, PyValueError};
use crate::lexer::tokenize;
use crate::nodes::{self, Node, NodeList};
use crate::parser::{self, Parser};
use crate::render::context::Rctx;
use crate::render::value::{mark_safe_str, FilterRegistry};
use crate::varparse::PyVer;
use crate::RENDER_COUNT;
use pyo3::intern;
use std::sync::atomic::Ordering;

/// `Origin(UNKNOWN_SOURCE)` — default template origin. The class and the
/// sentinel string are process-stable; cached so each `from_string` pays
/// only the constructor call (origin instances must stay fresh: stock
/// `Template.origin` is per-template, equality is by name/loader).
fn unknown_origin(py: Python<'_>) -> PyResult<Py<PyAny>> {
    static ORIGIN: PyOnceLock<(Py<PyAny>, Py<PyAny>)> = PyOnceLock::new();
    let (origin_cls, unknown) = ORIGIN.get_or_try_init(py, || -> PyResult<_> {
        let base = py.import("django.template.base")?;
        Ok((
            base.getattr("Origin")?.unbind(),
            base.getattr("UNKNOWN_SOURCE")?.unbind(),
        ))
    })?;
    Ok(origin_cls.bind(py).call1((unknown,))?.unbind())
}
/// Module handles for the loader `Template` rebinding (process-stable).
struct LoaderMods {
    base: Py<PyModule>,
    tmpl: Py<PyModule>,
    lbase: Py<PyModule>,
    /// `(cached.Loader, its get_template, its cache_key)`, resolved on the
    /// first lookup that needs it.
    cached: PyOnceLock<Option<(Py<PyType>, Py<PyAny>, Py<PyAny>)>>,
}

impl LoaderMods {
    fn cached_loader(
        &self,
        py: Python<'_>,
    ) -> PyResult<Option<&(Py<PyType>, Py<PyAny>, Py<PyAny>)>> {
        self.cached
            .get_or_try_init(py, || -> PyResult<_> {
                let Ok(m) = py.import("django.template.loaders.cached") else {
                    return Ok(None);
                };
                let cls = m.getattr("Loader")?.cast_into::<PyType>()?;
                let gt = cls.getattr("get_template")?.unbind();
                let ck = cls.getattr("cache_key")?.unbind();
                Ok(Some((cls.unbind(), gt, ck)))
            })
            .map(Option::as_ref)
    }
}

/// The text of an exact `str` or `SafeString` (template string literals
/// resolve to `SafeString`, whose `str()`/hash/eq are plain `str`'s), else
/// `None`.
fn plain_str<'a>(v: &'a Bound<'_, PyAny>) -> PyResult<Option<&'a str>> {
    let py = v.py();
    let ok = v.is_exact_instance_of::<PyString>()
        || v.get_type().as_ptr() == crate::render::value::helpers(py)?.safe_string.as_ptr();
    if !ok {
        return Ok(None);
    }
    Ok(Some(v.cast::<PyString>()?.to_str()?))
}

fn loader_mods(py: Python<'_>) -> PyResult<&'static LoaderMods> {
    static MODS: PyOnceLock<LoaderMods> = PyOnceLock::new();
    MODS.get_or_try_init(py, || -> PyResult<_> {
        Ok(LoaderMods {
            base: py.import("django.template.base")?.unbind(),
            tmpl: py.import("django.template")?.unbind(),
            lbase: py.import("django.template.loaders.base")?.unbind(),
            cached: PyOnceLock::new(),
        })
    })
}

/// `Template._render` as registered: native extends renders the parent
/// inline only while it is in place (the test runner's
/// `instrumented_test_render` replaces it to record rendered templates).
static TEMPLATE_RENDER: PyOnceLock<Py<PyAny>> = PyOnceLock::new();

pub(crate) fn template_render_unpatched(py: Python<'_>) -> PyResult<bool> {
    let Some(orig) = TEMPLATE_RENDER.get(py) else {
        return Ok(false);
    };
    Ok(py
        .get_type::<Template>()
        .getattr(intern!(py, "_render"))?
        .is(orig.bind(py)))
}

/// `Engine.find_template` as registered (patch detection for the direct
/// call in `get_template`).
static ENGINE_FIND_TEMPLATE: PyOnceLock<Py<PyAny>> = PyOnceLock::new();

#[pyclass(module = "django_rust_templates._dtl", dict)]
pub struct Engine {
    /// Parsed Django major.minor branch (filters/tags behave per branch).
    pub ver: PyVer,
    /// Bridged Python filter registry (native filters are layered on top).
    pub registry: FilterRegistry,
    /// `template_libraries`: name → Library object (for `{% load %}`).
    pub template_libraries: Py<PyDict>,
    // Attributes mirrored from django.template.engine.Engine.
    #[pyo3(get, set)]
    pub dirs: Py<PyAny>,
    #[pyo3(get, set)]
    pub app_dirs: bool,
    #[pyo3(get, set)]
    pub context_processors: Py<PyAny>,
    #[pyo3(get, set)]
    pub debug: bool,
    #[pyo3(get, set)]
    pub loaders: Py<PyAny>,
    #[pyo3(get, set)]
    pub string_if_invalid: Py<PyAny>,
    #[pyo3(get, set)]
    pub file_charset: String,
    #[pyo3(get, set)]
    pub libraries: Py<PyDict>,
    #[pyo3(get, set)]
    pub builtins: Py<PyList>,
    #[pyo3(get, set)]
    pub autoescape: bool,
    /// Bridged tag compile functions (stock `Parser.tags`), bridged from the
    /// builtin libraries; `{% load %}` additions ride the shim parser.
    pub tags: std::collections::HashMap<String, Py<PyAny>>,
    /// `template_loaders`, cached like stock `cached_property`.
    loaders_cache: std::sync::OnceLock<Py<PyList>>,
    /// `template_builtins` (the imported `Library` objects), computed once at
    /// construction like stock's `cached_property`: the loader attribute is
    /// read by every stock `Parser.__init__`, which happens per loader-fetched
    /// `Template` parse — importing the builtin libraries there re-ran 3
    /// `import_library` calls per parse. `builtins` is documented stock config
    /// (fixed at construction), so this never goes stale.
    builtins_cache: Py<PyList>,
}

impl Engine {
    const DEFAULT_BUILTINS: [&'static str; 3] = [
        "django.template.defaulttags",
        "django.template.defaultfilters",
        "django.template.loader_tags",
    ];
    fn improperly_configured(py: Python<'_>, msg: &str) -> PyErr {
        match py
            .import("django.core.exceptions")
            .and_then(|m| m.getattr("ImproperlyConfigured"))
        {
            Ok(cls) => match cls.cast_into::<PyType>() {
                Ok(t) => PyErr::from_type(t, (msg.to_string(),)),
                Err(e) => e.into(),
            },
            Err(e) => e,
        }
    }

    /// Build a `TemplateDoesNotExist` instance, optionally with `tried`.
    fn tde(py: Python<'_>, name: &Bound<'_, PyAny>, tried: Option<&Bound<'_, PyList>>) -> PyErr {
        let cls = match template_does_not_exist_type(py) {
            Ok(c) => c,
            Err(e) => return e,
        };
        let clsb = cls.bind(py);
        let res = match tried {
            Some(t) => {
                let kwargs = PyDict::new(py);
                if let Err(e) = kwargs.set_item("tried", t) {
                    return e;
                }
                clsb.call((name,), Some(&kwargs))
            }
            None => clsb.call1((name,)),
        };
        match res {
            Ok(inst) => PyErr::from_value(inst),
            Err(e) => e,
        }
    }

    /// Cached-loader hit, answered without any Python-level call: when the
    /// first template loader is an exact, unpatched
    /// `django.template.loaders.cached.Loader`, `Loader.get_template`'s hit
    /// branch is `get_template_cache.get(cache_key(name, skip))` returned
    /// verbatim (truthy entries that are not `TemplateDoesNotExist`). The
    /// key is `str(name)` when no `skip` origin matches `name`; matching
    /// origins (a hashed key), non-`str` names, and any entry other than a
    /// native `Template` take the full `find_template` path.
    fn cached_loader_hit(
        slf: &Bound<'_, Self>,
        name: &Bound<'_, PyAny>,
        skip: Option<&Py<PyAny>>,
    ) -> PyResult<Option<(Py<PyAny>, Py<PyAny>)>> {
        let py = slf.py();
        match plain_str(name)? {
            Some(s) if !s.is_empty() => {}
            _ => return Ok(None),
        }
        let loader = {
            let e = slf.borrow();
            let Some(loaders) = e.loaders_cache.get() else {
                return Ok(None);
            };
            let loaders = loaders.bind(py);
            if loaders.is_empty() {
                return Ok(None);
            }
            loaders.get_item(0)?
        };
        let mods = loader_mods(py)?;
        let Some(cl) = mods.cached_loader(py)? else {
            return Ok(None);
        };
        let cls = cl.0.bind(py);
        if loader.get_type().as_ptr() != cls.as_ptr()
            || !cls.getattr(intern!(py, "get_template"))?.is(cl.1.bind(py))
            || !cls.getattr(intern!(py, "cache_key"))?.is(cl.2.bind(py))
        {
            return Ok(None);
        }
        let Ok(d) = loader.getattr(intern!(py, "__dict__"))?.cast_into::<PyDict>() else {
            return Ok(None);
        };
        if d.contains(intern!(py, "get_template"))? || d.contains(intern!(py, "cache_key"))? {
            return Ok(None);
        }
        let Some(cache) = d.get_item(intern!(py, "get_template_cache"))? else {
            return Ok(None);
        };
        let Ok(cache) = cache.cast_exact::<PyDict>() else {
            return Ok(None);
        };
        if let Some(skip) = skip {
            let skip = skip.bind(py);
            if !skip.is_instance_of::<PyList>() && !skip.is_instance_of::<PyTuple>() && !skip.is_none() {
                return Ok(None);
            }
            if skip.is_truthy()? {
                for origin in skip.try_iter()? {
                    if origin?.getattr(intern!(py, "template_name"))?.eq(name)? {
                        return Ok(None);
                    }
                }
            }
        }
        let Some(hit) = cache.get_item(name)? else {
            return Ok(None);
        };
        let Ok(t) = hit.cast_exact::<Template>() else {
            return Ok(None);
        };
        let origin = t.borrow().origin.clone_ref(py);
        Ok(Some((hit.unbind(), origin)))
    }

    /// `engine.find_template(name, skip=skip)` for `ExtendsNode`: the native
    /// method directly unless patched (see `find_template_unpatched`).
    pub(crate) fn find_template_dispatch<'py>(
        engine: &Bound<'py, PyAny>,
        name: &Bound<'py, PyAny>,
        skip: &Bound<'py, PyAny>,
    ) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyAny>)> {
        let py = engine.py();
        if let Ok(e) = engine.cast_exact::<Self>() {
            if Self::find_template_unpatched(e)? {
                let (t, o) = Self::find_template(e, name, None, Some(skip.clone().unbind()))?;
                return Ok((t.into_bound(py), o.into_bound(py)));
            }
        }
        let kwargs = PyDict::new(py);
        kwargs.set_item("skip", skip)?;
        engine.call_method("find_template", (name,), Some(&kwargs))?.extract()
    }

    /// True when `self.find_template` resolves to the native method (no
    /// instance-level `mock.patch.object(engine, "find_template")` and no
    /// class-level replacement), so `get_template` may call it directly.
    fn find_template_unpatched(slf: &Bound<'_, Self>) -> PyResult<bool> {
        let py = slf.py();
        let Some(orig) = ENGINE_FIND_TEMPLATE.get(py) else {
            return Ok(false);
        };
        if !slf.get_type().getattr(intern!(py, "find_template"))?.is(orig.bind(py)) {
            return Ok(false);
        }
        let d = slf.as_any().getattr(intern!(py, "__dict__"))?;
        Ok(!d.cast::<PyDict>().is_ok_and(|d| d.contains(intern!(py, "find_template")).unwrap_or(true)))
    }

    fn find_template_inner(
        slf: &Bound<'_, Self>,
        name: &Bound<'_, PyAny>,
        skip: Option<Py<PyAny>>,
    ) -> PyResult<(Py<PyAny>, Py<PyAny>)> {
        let py = slf.py();
        let tde_ty = template_does_not_exist_type(py)?;
        let tried = PyList::empty(py);
        let kwargs = PyDict::new(py);
        kwargs.set_item("skip", skip.unwrap_or_else(|| py.None()))?;
        for loader in slf.getattr("template_loaders")?.try_iter()? {
            match loader?.call_method("get_template", (name,), Some(&kwargs)) {
                Ok(template) => {
                    let origin = template.getattr("origin")?.unbind();
                    return Ok((template.unbind(), origin));
                }
                Err(e) if e.is_instance(py, tde_ty.bind(py)) => {
                    for t in e.value(py).getattr("tried")?.try_iter()? {
                        tried.append(t?)?;
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Err(Self::tde(py, name, Some(&tried)))
    }
}

#[pymethods]
impl Engine {
    #[new]
    #[pyo3(signature = (dirs=None, app_dirs=false, context_processors=None, debug=false,
                        loaders=None, string_if_invalid="", file_charset="utf-8",
                        libraries=None, builtins=None, autoescape=true))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        dirs: Option<Py<PyAny>>,
        app_dirs: bool,
        context_processors: Option<Py<PyAny>>,
        debug: bool,
        loaders: Option<Py<PyAny>>,
        string_if_invalid: &str,
        file_charset: &str,
        libraries: Option<Bound<'_, PyDict>>,
        builtins: Option<Vec<String>>,
        autoescape: bool,
    ) -> PyResult<Self> {
        // Django: `app_dirs must not be set when loaders is defined.`
        if loaders.is_some() && app_dirs {
            let exc = py
                .import("django.core.exceptions")?
                .getattr("ImproperlyConfigured")?
                .cast_into::<PyType>()?;
            return Err(PyErr::from_type(
                exc,
                "app_dirs must not be set when loaders is defined.",
            ));
        }
        let v = py.import("django")?.getattr("VERSION")?;
        let major: u32 = v.get_item(0)?.extract()?;
        let minor: u32 = v.get_item(1)?.extract()?;
        let ver = PyVer::from_tuple((major, minor));
        // loaders defaults (kept as Py; resolved lazily into loader objects).
        let loaders_val: Py<PyAny> = if let Some(l) = loaders {
            l
        } else {
                let mut names = vec!["django.template.loaders.filesystem.Loader"];
                if app_dirs {
                    names.push("django.template.loaders.app_directories.Loader");
                }
                let inner = PyList::new(py, names)?.into_any().unbind();
                let pair = PyTuple::new(
                    py,
                    [
                        PyString::new(py, "django.template.loaders.cached.Loader").into_any(),
                        inner.into_bound(py),
                    ],
                )?;
                PyList::new(py, [pair])?.into_any().unbind()
        };
        let libs = libraries.unwrap_or_else(|| PyDict::new(py));
        let mut builtin_paths: Vec<String> =
            Self::DEFAULT_BUILTINS.iter().map(ToString::to_string).collect();
        if let Some(extra) = builtins {
            builtin_paths.extend(extra);
        }
        // Bridge every builtin library: filters into the Rust registry and
        // tags into the bridged-tag map (stock Parser gets Library objects).
        let mut registry = FilterRegistry::default();
        let mut tags = std::collections::HashMap::new();
        let mut builtin_libs = Vec::with_capacity(builtin_paths.len());
        for path in &builtin_paths {
            let lib = bridge::import_library(py, path)?;
            bridge::bridge_library_filters(py, &mut registry.entries, lib.bind(py), ver)?;
            bridge::bridge_library_tags(py, &mut tags, lib.bind(py))?;
            builtin_libs.push(lib);
        }
        // template_libraries = {name: import_library(path)}
        let tlibs = PyDict::new(py);
        for (name, path) in libs.iter() {
            let path: String = path.extract()?;
            tlibs.set_item(name, bridge::import_library(py, &path)?)?;
        }
        let builtins_list = PyList::new(py, builtin_paths)?;
        let builtins_cache = PyList::new(py, builtin_libs)?.unbind();
        Ok(Self {
            ver,
            registry,
            tags,
            loaders_cache: std::sync::OnceLock::new(),
            builtins_cache,
            template_libraries: tlibs.unbind(),
            dirs: dirs.unwrap_or_else(|| PyList::empty(py).into_any().unbind()),
            app_dirs,
            context_processors: context_processors
                .unwrap_or_else(|| PyList::empty(py).into_any().unbind()),
            debug,
            loaders: loaders_val,
            string_if_invalid: PyString::new(py, string_if_invalid)
                .into_any()
                .unbind(),
            file_charset: file_charset.to_string(),
            libraries: libs.unbind(),
            builtins: builtins_list.unbind(),
            autoescape,
        })
    }

    #[getter]
    fn ver(&self) -> &'static str {
        match self.ver {
            PyVer::V52 => "5.2",
            PyVer::V61 => "6.1",
        }
    }

    #[getter]
    fn template_builtins(&self, py: Python<'_>) -> Py<PyList> {
        self.builtins_cache.clone_ref(py)
    }

    /// `Engine.from_string`: compile source into a Rust Template.
    fn from_string(slf: &Bound<'_, Self>, source: &str) -> PyResult<Template> {
        let e = slf.borrow();
        let origin = unknown_origin(slf.py())?;
        Template::compile(
            slf.py(),
            source,
            &e.registry,
            slf.clone().into_any().unbind(),
            &e.tags,
            origin,
            None,
            e.debug,
            e.ver,
        )
    }
    /// Stock `Engine.__repr__` (6.1 engine.py).
    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        let rep = |v: &Bound<'_, PyAny>| -> PyResult<String> { v.repr()?.extract() };
        let bool_str = |b: bool| if b { "True" } else { "False" };
        let dirs = self.dirs.bind(py);
        let cps = self.context_processors.bind(py);
        let libs = self.libraries.bind(py);
        let builtins = self.builtins.bind(py);
        Ok(format!(
            "<Engine:{} app_dirs={}{} debug={} loaders={} string_if_invalid={} \
             file_charset={}{}{} autoescape={}>",
            if dirs.is_truthy()? {
                format!(" dirs={}", rep(dirs)?)
            } else {
                String::new()
            },
            bool_str(self.app_dirs),
            if cps.is_truthy()? {
                format!(" context_processors={}", rep(cps)?)
            } else {
                String::new()
            },
            bool_str(self.debug),
            rep(self.loaders.bind(py))?,
            rep(self.string_if_invalid.bind(py))?,
            rep(&PyString::new(py, &self.file_charset))?,
            if libs.is_truthy()? {
                format!(" libraries={}", rep(libs)?)
            } else {
                String::new()
            },
            if builtins.is_truthy()? {
                format!(" builtins={}", rep(builtins)?)
            } else {
                String::new()
            },
            bool_str(self.autoescape),
        ))
    }

    /// Stock `Engine.template_context_processors` (`cached_property`):
    /// `_builtin_context_processors + tuple(self.context_processors)`, each
    /// import_string()-ed. Tuple type matters: `RequestContext` adds its own
    /// tuple to it.
    #[getter]
    fn template_context_processors(
        slf: &Bound<'_, Self>,
        py: Python<'_>,
    ) -> PyResult<Py<PyTuple>> {
        let ctx_mod = py.import("django.template.context")?;
        let import_string = py
            .import("django.utils.module_loading")?
            .getattr("import_string")?;
        let mut items: Vec<Bound<'_, PyAny>> = ctx_mod
            .getattr("_builtin_context_processors")?
            .try_iter()?
            .collect::<Result<_, _>>()?;
        let extra: Vec<Bound<'_, PyAny>> = slf
            .borrow()
            .context_processors
            .bind(py)
            .try_iter()?
            .collect::<Result<_, _>>()?;
        items.extend(extra);
        let resolved = items
            .iter()
            .map(|p| import_string.call1((p,)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(PyTuple::new(py, resolved)?.unbind())
    }
    /// Python-visible mirror of `template_libraries` (stock attribute).
    #[getter]
    fn template_libraries(&self, py: Python<'_>) -> Py<PyDict> {
        self.template_libraries.clone_ref(py)
    }

    /// `template_loaders` — the stock `cached_property`, materialized on
    /// first access and cached forever.
    #[getter]
    fn template_loaders(slf: &Bound<'_, Self>, py: Python<'_>) -> PyResult<Py<PyList>> {
        if let Some(cached) = slf.borrow().loaders_cache.get() {
            return Ok(cached.clone_ref(py));
        }
        let out = PyList::empty(py);
        for item in slf.borrow().loaders.bind(py).try_iter()? {
            let loader = Self::find_template_loader(slf, &item?)?;
            if !loader.is_none(py) {
                out.append(loader)?;
            }
        }
        let val = out.unbind();
        let _ = slf.borrow().loaders_cache.set(val.clone_ref(py));
        Ok(val)
    }

    /// Port of `Engine.find_template_loader`.
    fn find_template_loader(
        slf: &Bound<'_, Self>,
        loader: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let py = slf.py();
        let (target, args): (Py<PyAny>, Vec<Py<PyAny>>) =
            if loader.is_instance_of::<PyTuple>() || loader.is_instance_of::<PyList>() {
                let items: Vec<Bound<'_, PyAny>> =
                    loader.try_iter()?.collect::<Result<Vec<_>, _>>()?;
                if items.is_empty() {
                    return Err(PyValueError::new_err(
                        "not enough values to unpack (expected at least 1, got 0)",
                    ));
                }
                (
                    items[0].clone().unbind(),
                    items[1..].iter().map(|a| a.clone().unbind()).collect(),
                )
            } else {
                (loader.clone().unbind(), Vec::new())
            };
        if target.bind(py).is_instance_of::<PyString>() {
            let path: String = target.bind(py).extract()?;
            let loader_class = py
                .import("django.utils.module_loading")?
                .call_method1("import_string", (path,))?;
            let mut full: Vec<Bound<'_, PyAny>> = vec![slf.clone().into_any()];
            for a in &args {
                full.push(a.bind(py).clone());
            }
            return Ok(loader_class.call1(PyTuple::new(py, full)?)?.unbind());
        }
        let msg = format!(
            "Invalid value in template loaders configuration: {}",
            target.bind(py).repr()?
        );
        Err(Self::improperly_configured(py, &msg))
    }

    /// Port of `Engine.find_template` (the `dirs` parameter exists but is
    /// unused by stock code).
    #[allow(clippy::needless_pass_by_value)]
    #[pyo3(signature = (name, dirs=None, skip=None))]
    fn find_template(
        slf: &Bound<'_, Self>,
        name: &Bound<'_, PyAny>,
        dirs: Option<Py<PyAny>>,
        skip: Option<Py<PyAny>>,
    ) -> PyResult<(Py<PyAny>, Py<PyAny>)> {
        let _ = dirs;
        let py = slf.py();
        if let Some(hit) = Self::cached_loader_hit(slf, name, skip.as_ref())? {
            return Ok(hit);
        }
        // Loaders build their `Template` from a module-global. While we hold
        // the GIL, point that global at the native `Template` for the
        // duration of the fetch, so loader-fetched templates are parsed
        // natively once (the Django test suite achieves the same with a
        // global rebinding at startup; here it stays scoped to the fetch and
        // is invisible to any other user of stock `Template`). The
        // `get_template` re-wrap remains as a fallback for loaders that
        // captured the stock class in their own globals. Only cache misses
        // get here once a cached loader is warm (see `cached_loader_hit`).
        let dtl_tm = py.get_type::<Template>();
        let mods = loader_mods(py)?;
        let base_mod = mods.base.bind(py);
        let saved = if base_mod.getattr("Template")?.is(&dtl_tm) {
            None
        } else {
            let tmpl_mod = mods.tmpl.bind(py);
            let lbase_mod = mods.lbase.bind(py);
            let saved = (
                base_mod.getattr("Template")?.unbind(),
                tmpl_mod.getattr("Template")?.unbind(),
                lbase_mod.getattr("Template")?.unbind(),
            );
            base_mod.setattr("Template", &dtl_tm)?;
            tmpl_mod.setattr("Template", &dtl_tm)?;
            lbase_mod.setattr("Template", &dtl_tm)?;
            Some(saved)
        };
        let out = Self::find_template_inner(slf, name, skip);
        if let Some((a, b, c)) = saved {
            let _ = base_mod.setattr("Template", a.bind(py));
            let _ = mods.tmpl.bind(py).setattr("Template", b.bind(py));
            let _ = mods.lbase.bind(py).setattr("Template", c.bind(py));
        }
        out
    }

    /// Port of `Engine.get_template` (6.1 splits `name#partial`).
    fn get_template(
        slf: &Bound<'_, Self>,
        template_name: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let py = slf.py();
        let ver = slf.borrow().ver;
        let mut name = template_name.clone().unbind();
        let mut partial = py.None();
        // Exact `str` without `#`: `partition` would return `(name, "", "")`.
        let plain = plain_str(template_name)?.is_some_and(|s| !s.contains('#'));
        if ver == PyVer::V61 && plain {
            if !template_name.is_truthy()? {
                return Err(Self::tde(py, template_name, None));
            }
        } else if ver == PyVer::V61 {
            match template_name.call_method1("partition", ("#",)) {
                Ok(parts) => {
                    name = parts.get_item(0)?.unbind();
                    partial = parts.get_item(2)?.unbind();
                }
                Err(e) if e.is_instance_of::<PyAttributeError>(py) => {
                    return Err(Self::tde(py, template_name, None));
                }
                Err(e) => return Err(e),
            }
            if !name.is_truthy(py)? {
                return Err(Self::tde(py, template_name, None));
            }
        }
        // Dynamic dispatch so `mock.patch.object(engine, "find_template", ...)`
        // works like it does on the stock Python Engine (`#[pyclass(dict)]`
        // makes the instance setattr that mock performs succeed).
        let (mut template, origin): (Py<PyAny>, Py<PyAny>) = if Self::find_template_unpatched(slf)? {
            Self::find_template(slf, name.bind(py), None, None)?
        } else {
            slf.getattr("find_template")?
                .call1((name.bind(py),))?
                .extract()?
        };
        if template.bind(py).is_exact_instance_of::<Template>() {
            // Native template (cached-loader hit or natively parsed): none
            // of the re-wrap fallbacks below apply.
        } else if !template.bind(py).hasattr("render")? {
            let kwargs = PyDict::new(py);
            kwargs.set_item("engine", slf)?;
            template = py
                .get_type::<Template>()
                .call((template, origin.clone_ref(py), name.clone_ref(py)), Some(&kwargs))?
                .unbind();
        } else if !template.bind(py).is_instance_of::<Template>()
            && template.bind(py).get_type().as_ptr()
            == loader_mods(py)?.base.bind(py).getattr("Template")?.as_ptr()
        {
            // Exact stock `Template` from a loader: in the patched suite the
            // loaders construct native Templates directly; here the loader
            // built a stock one, so re-parse the source natively (the same
            // object the global patch would have produced) and memoize the
            // wrap on the stock object to keep `get_template` identity
            // stable. Any native-parse failure falls back to the stock
            // template.
            let st = template.bind(py);
            if let Ok(prev) = st.getattr("_dtl_native") {
                template = prev.unbind();
            } else {
                let e = slf.borrow();
                let wrapped = Template::compile(
                    py,
                    &st.getattr("source")?.extract::<String>()?,
                    &e.registry,
                    slf.clone().into_any().unbind(),
                    &e.tags,
                    origin.clone_ref(py),
                    Some(st.getattr("name")?.unbind()),
                    e.debug,
                    e.ver,
                );
                if let Ok(t) = wrapped {
                    let t = Py::new(py, t)?.into_any();
                    st.setattr("_dtl_native", &t)?;
                    template = t;
                }
            }
        }
        if !partial.is_truthy(py)? {
            return Ok(template);
        }
        let extra = match template.bind(py).getattr("extra_data") {
            Ok(d) => d,
            Err(e) if e.is_instance_of::<PyAttributeError>(py) => PyDict::new(py).into_any(),
            Err(e) => return Err(e),
        };
        let res = extra.get_item("partials").and_then(|m| m.get_item(&partial));
        match res {
            Ok(p) => {
                p.setattr("engine", slf)?;
                // Proxy renders its body through the owning template.
                p.setattr("_template", template.bind(py).clone())?;
                Ok(p.unbind())
            }
            Err(e)
                if e.is_instance_of::<PyKeyError>(py) || e.is_instance_of::<PyTypeError>(py) =>
            {
                let tried = PyList::new(py, [name.bind(py).clone()])?;
                Err(Self::tde(py, partial.bind(py), Some(&tried)))
            }
            Err(e) => Err(e),
        }
    }

    /// Port of `Engine.select_template`.
    fn select_template(
        slf: &Bound<'_, Self>,
        template_name_list: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let py = slf.py();
        if !template_name_list.is_truthy()? {
            return Err(Self::tde(
                py,
                &PyString::new(py, "No template names provided"),
                None,
            ));
        }
        let tde_ty = template_does_not_exist_type(py)?;
        let mut not_found: Vec<String> = Vec::new();
        for name in template_name_list.try_iter()? {
            let name = name?;
            match slf.getattr("get_template")?.call1((&name,)) {
                Ok(t) => return Ok(t.unbind()),
                Err(e) if e.is_instance(py, tde_ty.bind(py)) => {
                    let s: String = e.value(py).getattr("args")?.get_item(0)?.extract()?;
                    if !not_found.contains(&s) {
                        not_found.push(s);
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Err(Self::tde(
            py,
            &PyString::new(py, &not_found.join(", ")),
            None,
        ))
    }

    /// Port of `Engine.render_to_string` (test-suite helper).
    #[pyo3(signature = (template_name, context=None))]
    #[allow(clippy::needless_pass_by_value)]
    fn render_to_string(
        slf: &Bound<'_, Self>,
        template_name: &Bound<'_, PyAny>,
        context: Option<Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let py = slf.py();
        let t = if template_name.is_instance_of::<PyList>()
            || template_name.is_instance_of::<PyTuple>()
        {
            slf.call_method1("select_template", (template_name,))?.unbind()
        } else {
            slf.call_method1("get_template", (template_name,))?.unbind()
        };
        let ctx_cls = py.import("django.template.context")?.getattr("Context")?;
        let ctx = match &context {
            Some(c) if c.is_instance(&ctx_cls)? => c.clone(),
            other => {
                let kwargs = PyDict::new(py);
                kwargs.set_item("autoescape", slf.borrow().autoescape)?;
                ctx_cls
                    .call(
                        (other
                            .as_ref()
                            .map_or_else(|| py.None().into_bound(py), |c| c.clone().into_any()),),
                        Some(&kwargs),
                    )?
            }
        };
        t.bind(py).call_method1("render", (&ctx,)).map(Bound::unbind)
    }
}

#[pyclass(module = "django_rust_templates._dtl", dict)]
pub struct Template {
    nodelist: NodeList,
    engine: Py<PyAny>,
    origin: Py<PyAny>,
    name: Py<PyAny>,
    source: String,
    debug: bool,
    extra_data: std::sync::OnceLock<Py<PyAny>>,
    py_nodelist: std::sync::OnceLock<Py<PyList>>,
    /// Parse-time scan: the nodelist provably never reads or writes
    /// `render_context` state (`never_touches_rc`), so an inline include
    /// render may skip the rc rebinding (see `try_render_child_inline`).
    rc_quiet: bool,
    /// Every `{% block %}` of the template (`None`: a bridged node holds
    /// stock blocks), computed on first native extends/block render.
    block_index: PyOnceLock<Option<std::sync::Arc<[crate::tags::blocks::BlockEntry]>>>,
}

impl Template {
    pub(crate) fn block_index(
        slf: &Bound<'_, Self>,
    ) -> PyResult<Option<std::sync::Arc<[crate::tags::blocks::BlockEntry]>>> {
        let py = slf.py();
        let t = slf.borrow();
        Ok(t.block_index
            .get_or_try_init(py, || -> PyResult<_> {
                Ok(crate::tags::blocks::index_blocks(py, &t.nodelist)?.map(Into::into))
            })?
            .clone())
    }

    pub(crate) fn engine_ref<'py>(&self, py: Python<'py>) -> Bound<'py, PyAny> {
        self.engine.bind(py).clone()
    }

    pub(crate) fn top_nodes(&self) -> &[Node] {
        &self.nodelist.nodes
    }

    pub(crate) fn nodelist_ref(&self) -> &NodeList {
        &self.nodelist
    }

    #[allow(clippy::too_many_arguments)]
    fn compile(
        py: Python<'_>,
        source: &str,
        registry: &FilterRegistry,
        engine: Py<PyAny>,
        tags: &std::collections::HashMap<String, Py<PyAny>>,
        origin: Py<PyAny>,
        name: Option<Py<PyAny>>,
        debug: bool,
        ver: PyVer,
    ) -> PyResult<Self> {
        let tokens = tokenize(source, debug);
        let mut p = Parser::new(
            tokens,
            registry,
            engine.clone_ref(py),
            tags,
            origin.clone_ref(py),
            ver,
            debug,
        );
        match p.parse(py, &[]) {
            Ok(nodelist) => {
                // Debug renders read `rc.template` in `annotate_error`.
                // `.super` (`BlockNode.super`) reads the render-context
                // frame the quiet path skips; a textual check covers every
                // expression position.
                let rc_quiet = !debug
                    && !source.contains(".super")
                    && crate::tags::never_touches_rc(&nodelist);
                Ok(Self {
                    nodelist,
                    extra_data: std::sync::OnceLock::from(
                        p.take_extra_data(py)
                            .unwrap_or_else(|| PyDict::new(py).into_any().unbind()),
                    ),
                    py_nodelist: std::sync::OnceLock::new(),
                    rc_quiet,
                    block_index: PyOnceLock::new(),
                    engine,
                    origin,
                    name: name.unwrap_or_else(|| py.None()),
                    source: source.to_string(),
                    debug,
                })
            }
            Err(e) => {
                // Stock compile_nodelist order: template_debug first,
                // then (6.1) the raw_error_message prefix.
                if debug {
                    let exc = e.value(py);
                    if let Ok(tok) = exc.getattr("token") {
                        if let Ok(info) =
                            exception_info(py, source, origin.bind(py), exc, &tok)
                        {
                            let _ = exc.setattr("template_debug", info);
                        }
                    }
                }
                // 6.1 compile_nodelist prefixes TemplateSyntaxError args.
                if ver == PyVer::V61 {
                    let _ = parser::prefix_error_61(py, origin.bind(py), &e);
                }
                Err(e)
            }
        }
    }
}

#[pymethods]
impl Template {
    #[new]
    #[pyo3(signature = (template_string, origin=None, name=None, engine=None))]
    fn py_new(
        py: Python<'_>,
        template_string: &Bound<'_, PyAny>,
        origin: Option<Py<PyAny>>,
        name: Option<Py<PyAny>>,
        engine: Option<Py<PyAny>>,
    ) -> PyResult<Self> {
        // Template.__init__: engine=None → Engine.get_default();
        // origin=None → Origin(UNKNOWN_SOURCE); source = str(template_string).
        let engine = match engine {
            Some(e) => e,
            None => py
                .import("django.template.engine")?
                .getattr("Engine")?
                .call_method0("get_default")?
                .unbind(),
        };
        let origin = match origin {
            Some(o) => o,
            None => unknown_origin(py)?,
        };
        let source = template_string.str()?.to_string();
        let debug = engine.bind(py).getattr("debug")?.is_truthy()?;
        let ver = match engine.bind(py).getattr("ver") {
            Ok(v) => {
                let s = v.str().map(|s| s.to_string()).unwrap_or_default();
                if s == "6.1" {
                    PyVer::V61
                } else {
                    PyVer::V52
                }
            }
            Err(_) => PyVer::V52,
        };
        let eb = engine.bind(py);
        match eb.cast::<Engine>() {
            Ok(eng) => {
                let reg = eng.borrow();
                Self::compile(
                    py,
                    &source,
                    &reg.registry,
                    engine,
                    &reg.tags,
                    origin,
                    name,
                    debug,
                    ver,
                )
            }
            Err(_) => Err(syntax_error(
                py,
                "django_rust_templates.Template requires a django_rust_templates Engine",
            )),
        }
    }

    #[getter]
    fn source(&self) -> &str {
        &self.source
    }

    #[getter]
    fn origin(&self, py: Python<'_>) -> Py<PyAny> {
        self.origin.clone_ref(py)
    }

    #[getter]
    fn engine(&self, py: Python<'_>) -> Py<PyAny> {
        self.engine.clone_ref(py)
    }

    #[getter]
    fn name(&self, py: Python<'_>) -> Py<PyAny> {
        self.name.clone_ref(py)
    }
    /// Stock `Template.get_exception_info(exception, token)`: the
    /// `exc.template_debug` dict the stock debug render path builds via
    /// `Node.render_annotated`. Only reached when `engine.debug`.
    fn get_exception_info(
        &self,
        py: Python<'_>,
        exception: &Bound<'_, PyAny>,
        token: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyDict>> {
        exception_info(py, &self.source, self.origin.bind(py), exception, token)
    }


    fn __repr__(&self) -> String {
        let head: String = self.source.chars().take(20).collect();
        format!(
            "<Template template_string=\"{}...\">",
            head.replace('\n', "")
        )
    }

    /// `Template.nodelist` — the Python-visible adapter list, built once so
    /// node identity is stable across renders.
    #[getter]
    fn nodelist(slf: &Bound<'_, Self>) -> PyResult<Py<PyList>> {
        ensure_adapters(slf)
    }

    /// Stock `_render(context)`: render without re-entering the
    /// `push_state`/`bind_template` protocol (stock nodes call it inside theirs).
    fn _render(&self, py: Python<'_>, context: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        do_render(py, self, context)
    }

    /// Called by `_nodes._RustNode` adapters: render the native node at
    /// pre-order index `pos`.
    fn _render_native(
        &self,
        py: Python<'_>,
        pos: usize,
        context: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let engine = self.engine.bind(py).clone();
        let dbg = self
            .debug
            .then(|| (self.source.clone(), self.origin.clone_ref(py)));
        crate::nodes_py::render_native(py, &self.nodelist, &engine, pos, context, dbg)
    }

    /// `DtlPartialTemplate._render` delegate: render the body of the
    /// `{% partialdef %}` whose node id is `def_id`.
    fn _render_partial_body(
        &self,
        py: Python<'_>,
        def_id: usize,
        context: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let body = find_partial_body(&self.nodelist, def_id)
            .ok_or_else(|| PyKeyError::new_err(format!("partial def {def_id}")))?;
        let mut rctx = Rctx::new(context.clone(), self.engine.bind(py).clone())?;
        if self.debug {
            rctx.dbg = Some((self.source.clone(), self.origin.clone_ref(py)));
        }
        let mut out = String::new();
        nodes::render_list(&mut rctx, body, &mut out)?;
        Ok(mark_safe_str(py, &out)?.unbind())
    }

    /// 6.1 `Template.extra_data` — the shim-parser dict (`{% partialdef %}`).
    #[getter]
    fn extra_data(&self, py: Python<'_>) -> Py<PyAny> {
        self.extra_data
            .get()
            .map_or_else(|| py.None(), |d| d.clone_ref(py))
    }

    /// `Template.render(context)` — exact protocol:
    /// `with context.render_context.push_state(self): if context.template is
    /// None: with context.bind_template(self): context.template_name =
    /// self.name; return self._render(context) else: return self._render`
    ///
    /// Fast path: for a stock `Context` + `RenderContext` (exact types, so
    /// not `RequestContext`'s context-processor `bind_template` nor any
    /// subclass override) the two `@contextmanager` generators are inlined
    /// as the raw attribute mutations they perform — the code is identical
    /// on 5.2 and 6.1 (`Context.bind_template` sets `self.template`;
    /// `RenderContext.push_state` saves/restores `self.template` and
    /// push/pops an empty frame on `rc.dicts`). `push()` always appends to
    /// `dicts` (len >= 1 at entry ⇒ the `pop()` `ContextPopException` check
    /// cannot fire) and the only node-visible effect is the fresh top frame.
    fn render(slf: &Bound<'_, Self>, context: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        static RENDER_DESCR: PyOnceLock<(Py<pyo3::types::PyType>, Py<PyAny>)> =
            PyOnceLock::new();
        RENDER_COUNT.fetch_add(1, Ordering::Relaxed);
        let py = slf.py();
        ensure_adapters(slf)?;
        let t = slf.borrow();
        // Interned: hit on every render (fast path below).
        let rc = context.getattr(intern!(py, "render_context"))?;
        // Parse-time `DtlPartialTemplate` proxies need a handle on their
        // owning Template to render their bodies (idempotent).
        if let Some(ed) = t.extra_data.get() {
            if let Some(partials) = ed_partials(ed.bind(py)) {
                if let Ok(d) = partials.cast::<pyo3::types::PyDict>() {
                    for value in d.values() {
                        let _ = value.setattr("_template", slf.clone().into_any());
                    }
                }
            }
        }
        // `self._render` must stay an attribute call for subclasses: each
        // bound call adds a traceback entry the suite counts (#19827 needs
        // depth > 5). For the exact native type, the class-level method
        // descriptor is cached and invoked directly (same C call path, one
        // fewer getattr frame) to skip the per-render bound-method alloc.
        let render_body = || -> PyResult<Py<PyAny>> {
            let (ty, descr) = RENDER_DESCR.get_or_try_init(
                py,
                || -> PyResult<(Py<pyo3::types::PyType>, Py<PyAny>)> {
                    let tyb = slf.get_type();
                    let d = tyb.getattr("_render")?.unbind();
                    Ok((tyb.unbind(), d))
                },
            )?;
            if slf.get_type().as_ptr() == ty.bind(py).as_ptr() {
                descr.bind(py).call1((slf, context)).map(Bound::unbind)
            } else {
                slf.getattr("_render")?.call1((context,)).map(Bound::unbind)
            }
        };
        if stock_protocol_types(py, context, &rc)? {
            // Quiet template (parse-proven rc-free, `never_touches_rc`):
            // nothing during the body reads `rc.template` or the fresh rc
            // frame (the Context binder below is separate and always
            // runs), so the rebinding and frame push/pop are skipped.
            let quiet = t.rc_quiet;
            let state = if quiet {
                None
            } else {
                let prev = rc.getattr(intern!(py, "template"))?;
                rc.setattr(intern!(py, "template"), slf)?;
                let rc_dicts = rc.getattr(intern!(py, "dicts"))?.cast_into::<PyList>()?;
                rc_dicts.append(PyDict::new(py))?;
                Some((prev, rc_dicts))
            };
            let bound = context.getattr(intern!(py, "template"))?.is_none();
            if bound {
                context.setattr(intern!(py, "template"), slf)?;
                let name = slf.getattr(intern!(py, "name"))?;
                context.setattr(intern!(py, "template_name"), &name)?;
            }
            let res = render_body();
            // Mirrors stock's finally order: binder first, then state.
            if bound {
                context.setattr("template", py.None())?;
            }
            if let Some((prev, rc_dicts)) = &state {
                let n = rc_dicts.len();
                if n > 0 {
                    rc_dicts.del_item(n - 1)?;
                }
                rc.setattr("template", prev)?;
            }
            return res;
        }
        let state = rc.call_method("push_state", (slf.clone().into_any(),), None)?;
        state.call_method0("__enter__")?;
        let res = (|| -> PyResult<Py<PyAny>> {
            if context.getattr("template")?.is_none() {
                let binder = context.call_method("bind_template", (slf.clone().into_any(),), None)?;
                binder.call_method0("__enter__")?;
                let name = slf.getattr("name")?;
                context.setattr("template_name", &name)?;
                let r = render_body();
                binder.call_method1("__exit__", (py.None(), py.None(), py.None()))?;
                r
            } else {
                render_body()
            }
        })();
        if res.is_err() {
            let _ = state.call_method1("__exit__", (py.None(), py.None(), py.None()));
            return res;
        }
        state.call_method1("__exit__", (py.None(), py.None(), py.None()))?;
        res
    }
}

/// `extra_data["partials"]` without raising a `KeyError` on the (usual)
/// miss.
fn ed_partials<'py>(ed: &Bound<'py, PyAny>) -> Option<Bound<'py, PyAny>> {
    match ed.cast::<PyDict>() {
        Ok(d) => d.get_item(intern!(ed.py(), "partials")).ok().flatten(),
        Err(_) => ed.get_item("partials").ok(),
    }
}

/// Exact-type test for the inlined render protocol: `type(context)` is
/// `django.template.context.Context` and `type(rc)` is `RenderContext`.
fn stock_protocol_types(py: Python<'_>, context: &Bound<'_, PyAny>, rc: &Bound<'_, PyAny>) -> PyResult<bool> {
    static CLASSES: PyOnceLock<[Py<PyType>; 2]> = PyOnceLock::new();
    let classes = CLASSES.get_or_try_init(py, || -> PyResult<[Py<PyType>; 2]> {
        let m = py.import("django.template.context")?;
        Ok([
            m.getattr("Context")?.cast_into::<PyType>()?.unbind(),
            m.getattr("RenderContext")?.cast_into::<PyType>()?.unbind(),
        ])
    })?;
    Ok(context.get_type().as_ptr() == classes[0].bind(py).as_ptr()
        && rc.get_type().as_ptr() == classes[1].bind(py).as_ptr())
}

/// `self._render(context)` → `SafeString("".join(nodes))`.
fn do_render(
    py: Python<'_>,
    t: &Template,
    context: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    let s = render_nodes_to_string(py, t, context)?;
    Ok(mark_safe_str(py, &s)?.unbind())
}

/// Shared native render core: fresh `Rctx` over the live `context`,
/// `render_list` into a String.
fn render_nodes_to_string(
    py: Python<'_>,
    t: &Template,
    context: &Bound<'_, PyAny>,
) -> PyResult<String> {
    let mut rctx = Rctx::new(context.clone(), t.engine.bind(py).clone())?;
    if t.debug {
        rctx.dbg = Some((t.source.clone(), t.origin.clone_ref(py)));
    }
    let mut out = String::new();
    nodes::render_list(&mut rctx, &t.nodelist, &mut out)?;
    Ok(out)
}

/// `{% include %}` fast path: render a dtl child `Template` natively
/// against `context`, inlining the stock render protocol (`push_state` /
/// `bind_template` as raw mutations — same reasoning as the
/// `Template::render` fast path) and the `with context.push(values)` layer
/// (`values` is appended to the live `dicts` and popped again here, exactly
/// like a `ContextDict`; the parent's `Rctx` snapshot never sees it).
/// Returns `None` when `template` is not an exact dtl `Template` or the
/// context is not the stock type — the caller must go through the Python
/// `template.render(context)` call then.
pub(crate) fn try_render_child(
    py: Python<'_>,
    template: &Bound<'_, PyAny>,
    context: &Bound<'_, PyAny>,
    values: Option<&Bound<'_, PyDict>>,
) -> PyResult<Option<String>> {
    let Ok(child) = template.cast_exact::<Template>() else {
        return Ok(None);
    };
    let rc = context.getattr("render_context")?;
    if !stock_protocol_types(py, context, &rc)? {
        return Ok(None);
    }
    RENDER_COUNT.fetch_add(1, Ordering::Relaxed);
    ensure_adapters(child)?;
    {
        let t = child.borrow();
        if let Some(ed) = t.extra_data.get() {
            if let Some(partials) = ed_partials(ed.bind(py)) {
                if let Ok(d) = partials.cast::<PyDict>() {
                    for value in d.values() {
                        let _ = value.setattr("_template", child.clone().into_any());
                    }
                }
            }
        }
    }
    let ctx_dicts = context.getattr("dicts")?.cast_into::<PyList>()?;
    if let Some(v) = values {
        ctx_dicts.append(v)?;
    }
    let prev_rc_template = rc.getattr("template")?;
    rc.setattr("template", child)?;
    let rc_dicts = rc.getattr("dicts")?.cast_into::<PyList>()?;
    rc_dicts.append(PyDict::new(py))?;
    let bound = context.getattr("template")?.is_none();
    if bound {
        context.setattr("template", child)?;
        let name = child.getattr("name")?;
        context.setattr("template_name", &name)?;
    }
    let res: PyResult<String> = {
        let t = child.borrow();
        render_nodes_to_string(py, &t, context)
    };
    // Unwind in stock order: binder, then state; the `push(values)` layer
    // is outermost (entered before, exited after the render).
    if bound {
        context.setattr("template", py.None())?;
    }
    let n = rc_dicts.len();
    if n > 0 {
        rc_dicts.del_item(n - 1)?;
    }
    rc.setattr("template", &prev_rc_template)?;
    if values.is_some() {
        let m = ctx_dicts.len();
        if m > 0 {
            ctx_dicts.del_item(m - 1)?;
        }
    }
    Ok(Some(res?))
}

/// `{% include %}` (non-`only`) fast path for a native dtl `Template`
/// sharing the parent's engine and a stock `Context`: inlines the stock
/// render protocol (same raw mutations as the `Template::render` fast
/// path) and renders the child nodelist reusing the parent's `Rctx`
/// bindings — no second `Rctx::new` per include. `values` (the `with`
/// dict) is `None` for a `with`-less include: stock pushes an empty
/// `ContextDict`, which is invisible to context lookups, so the push is
/// skipped outright. Returns `None` when a precondition fails (foreign
/// engine, non-stock context, swapped layer list) and the caller must use
/// the generic Python protocol.
pub(crate) fn try_render_child_inline<'py>(
    rctx: &mut Rctx<'py>,
    template: &Bound<'py, PyAny>,
    values: Option<&Bound<'py, PyAny>>,
) -> PyResult<Option<String>> {
    let py = rctx.py();
    let Ok(child) = template.cast_exact::<Template>() else {
        return Ok(None);
    };
    let stock = if let Some(v) = rctx.stock_ok.get() {
        *v
    } else {
        let v = stock_protocol_types(py, &rctx.ctx, &rctx.render_context()?)?;
        let _ = rctx.stock_ok.set(v);
        v
    };
    if !stock {
        return Ok(None);
    }
    let quiet = {
        let t = child.borrow();
        if t.engine.bind(py).as_ptr() != rctx.engine.as_ptr() {
            return Ok(None);
        }
        if let Some(ed) = t.extra_data.get() {
            if let Some(partials) = ed_partials(ed.bind(py)) {
                if let Ok(d) = partials.cast::<PyDict>() {
                    for value in d.values() {
                        let _ = value.setattr("_template", child.clone().into_any());
                    }
                }
            }
        }
        // Parse-proven rc-free flag; adapters are built at template
        // construction time or on first full render — only build here when
        // absent (saves a clone_ref of the proxy list per include).
        if t.py_nodelist.get().is_none() {
            drop(t);
            ensure_adapters(child)?;
        }
        child.borrow().rc_quiet
    };
    RENDER_COUNT.fetch_add(1, Ordering::Relaxed);
    // Stock `with context.push(**values)`: the empty layer a `with`-less
    // include pushes is a lookup-invisible `ContextDict` (no key exists in
    // it), so both push and pop are skipped outright — only
    // `len(context.dicts)` would differ, unread during render (snapshots
    // below the top are never reindexed by stock).
    let pushed = values.is_some();
    if pushed && !rctx.push_layer(values)? {
        return Ok(None);
    }
    // Quiet child (parse-proven rc-free): stock rebinds `rc.template` and
    // pushes an empty rc layer per nested render, but nothing in the
    // child can observe either — the parent template is the only rc
    // reader during the body and its binding is restored around the
    // render — so both are skipped.
    let prev_rc_template = if quiet {
        None
    } else {
        let (rc, rc_dicts) = rctx.rc_pair()?;
        let prev = rc.getattr("template")?;
        rc.setattr("template", child)?;
        rc_dicts.append(PyDict::new(py))?;
        Some((prev, rc_dicts))
    };
    let bound = rctx.ctx.getattr("template")?.is_none();
    if bound {
        rctx.ctx.setattr("template", child)?;
        let name = child.getattr("name")?;
        rctx.ctx.setattr("template_name", &name)?;
    }
    let prev_dbg = rctx.dbg.take();
    let mut out = String::new();
    let body = {
        let t = child.borrow();
        if t.debug {
            rctx.dbg = Some((t.source.clone(), t.origin.clone_ref(py)));
        }
        let r = nodes::render_list(rctx, &t.nodelist, &mut out);
        drop(t);
        rctx.dbg = prev_dbg;
        r
    };
    // Unwind in stock order: binder, then state, then the `push(values)`
    // layer (entered first, exited last).
    if bound {
        rctx.ctx.setattr("template", py.None())?;
    }
    if let Some((prev, rc_dicts)) = &prev_rc_template {
        let n = rc_dicts.len();
        if n > 0 {
            rc_dicts.del_item(n - 1)?;
        }
        let (rc, _) = rctx.rc_pair()?;
        rc.setattr("template", prev)?;
    }
    match body {
        Ok(()) => {
            if pushed {
                rctx.pop_layer()?;
            }
            Ok(Some(out))
        }
        Err(e) => {
            if pushed {
                let _ = rctx.pop_layer();
            }
            Err(e)
        }
    }
}

/// DFS for the `{% partialdef %}` body with node id `def_id` (descending
/// through every node that has child nodelists).
fn find_partial_body(list: &NodeList, def_id: usize) -> Option<&NodeList> {
    for node in &list.nodes {
        match node {
            Node::Tag { payload, id, kind } => {
                if *kind == crate::tags::TagKind::PartialDef && *id == def_id {
                    if let crate::tags::TagPayload::PartialDef { body, .. } = &**payload {
                        return Some(body);
                    }
                }
                for child in crate::tags::tag_child_nodelists(payload) {
                    if let Some(b) = find_partial_body(child, def_id) {
                        return Some(b);
                    }
                }
            }
            Node::AutoEscape { nodelist, .. } => {
                if let Some(b) = find_partial_body(nodelist, def_id) {
                    return Some(b);
                }
            }
            _ => {}
        }
    }
    None
}

/// Port of stock `Template.get_exception_info` body. Token positions are
/// stock char offsets (our lexer records byte offsets — converted here);
/// only reached in debug mode, where tokens always carry positions.
pub fn exception_info(
    py: Python<'_>,
    source: &str,
    origin: &Bound<'_, PyAny>,
    exception: &Bound<'_, PyAny>,
    token: &Bound<'_, PyAny>,
) -> PyResult<Py<PyDict>> {
    let (start_b, end_b): (usize, usize) = token.getattr("position")?.extract()?;
    let start = source[..start_b.min(source.len())].chars().count();
    let end = source[..end_b.min(source.len())].chars().count();
    let chars: Vec<char> = source.chars().collect();
    let len = chars.len();
    let cl = |i: usize| i.min(len);
    // `linebreak_iter`: 0, then every "\n" + 1, then len + 1.
    let mut bounds: Vec<usize> = vec![0];
    for (i, c) in chars.iter().enumerate() {
        if *c == '\n' {
            bounds.push(i + 1);
        }
    }
    bounds.push(len + 1);
    let context_lines = 10usize;
    let mut upto = 0usize;
    let mut line = 0usize;
    let mut before = String::new();
    let mut during = String::new();
    let mut after = String::new();
    let source_lines = PyList::empty(py);
    for (num, &next) in bounds.iter().enumerate() {
        if start >= upto && end <= next {
            line = num;
            before = chars[cl(upto)..cl(start)].iter().collect();
            during = chars[cl(start)..cl(end)].iter().collect();
            after = chars[cl(end)..cl(next)].iter().collect();
        }
        let text: String = chars[cl(upto)..cl(next)].iter().collect();
        source_lines.append((num, text))?;
        upto = next;
    }
    let total = source_lines.len();
    let top = line.saturating_sub(context_lines).max(1);
    let bottom = (line + 1 + context_lines).min(total);
    // Stock message = str(exception.args[0]), which on a reraised
    // TemplateSyntaxError is "<name>: <msg>" after the 6.1 prefix.
    let fallback = PyString::new(py, "(Could not get exception message)");
    let message: Py<PyAny> = match exception.getattr("args").and_then(|a| a.get_item(0)) {
        Ok(m) if !m.is_none() => m.str()?.into_any().unbind(),
        _ => fallback.into_any().unbind(),
    };
    let d = PyDict::new(py);
    d.set_item("message", message)?;
    d.set_item("source_lines", source_lines.get_slice(top, bottom))?;
    d.set_item("before", before)?;
    d.set_item("during", during)?;
    d.set_item("after", after)?;
    d.set_item("top", top)?;
    d.set_item("bottom", bottom)?;
    d.set_item("total", total)?;
    d.set_item("line", line)?;
    d.set_item("name", origin.getattr("name")?)?;
    d.set_item("start", start)?;
    d.set_item("end", end)?;
    Ok(d.unbind())
}

/// Build the Python adapter tree (and `_dtl_node_by_id` map) once per
/// template: native `{% block %}` renders look their adapter up on
/// `render_context.template`, so the map must exist before any render.
pub(crate) fn ensure_adapters(slf: &Bound<'_, Template>) -> PyResult<Py<PyList>> {
    let py = slf.py();
    {
        let t = slf.borrow();
        if let Some(l) = t.py_nodelist.get() {
            return Ok(l.clone_ref(py));
        }
    }
    let list = {
        let t = slf.borrow();
        let handle = slf.clone().into_any();
        crate::nodes_py::build_py_nodelist(py, &handle, t.origin.bind(py), &t.nodelist)?
    };
    {
        let t = slf.borrow();
        let _ = t.py_nodelist.set(list.clone_ref(py));
    }
    Ok(list)
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Engine>()?;
    m.add_class::<Template>()?;
    let py = m.py();
    let _ = TEMPLATE_RENDER.set(py, py.get_type::<Template>().getattr("_render")?.unbind());
    let _ = ENGINE_FIND_TEMPLATE.set(py, py.get_type::<Engine>().getattr("find_template")?.unbind());
    Ok(())
}
