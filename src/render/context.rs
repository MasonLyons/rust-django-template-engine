//! Live-Python render context machinery.
//!
//! The renderer operates on the live `django.template.context.Context`
//! instance itself (never a copy): `ContextDict` write-back via `__exit__`,
//! `context.autoescape` mutation by `{% autoescape %}`, `flatten()`, and
//! `render_context` scoping all behave exactly as stock Django.
//!
//! `get_item` mirrors `BaseContext.__getitem__` (`key in d` then `d[key]`,
//! top layer first). A snapshot of the `dicts` list accelerates the walk; it
//! holds references to the live layer objects (content mutations are visible)
//! and is only invalidated when layers are pushed/popped.

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyDict, PyList};

use crate::render::value::helpers;

pub struct Rctx<'py> {
    /// The live `django.template.context.Context`.
    pub ctx: Bound<'py, PyAny>,
    /// Engine of the rendering template (constant for the whole render tree).
    pub engine: Bound<'py, PyAny>,
    /// Live `context.dicts`.
    dicts: Bound<'py, PyList>,
    /// Cached `(render_context, render_context.dicts)`, fetched on first
    /// use per render. Stock binds `render_context` once in
    /// `Context.__init__` and mutates it in place (push/pop append to its
    /// `dicts`), so one fetch per render suffices; nodelists that never
    /// touch `render_context` skip both getattr calls entirely.
    rc: std::cell::RefCell<Option<(Bound<'py, PyAny>, Bound<'py, PyList>)>>,
    /// Cached `stock_protocol_types` verdict for this render's context
    /// (see `try_render_child_inline`); first include computes, later ones
    /// reuse. The two objects are fixed for the render's lifetime.
    pub(crate) stock_ok: std::cell::OnceCell<bool>,
    /// Snapshot of `dicts` entries for fast walks; invalidated by push/pop.
    snapshot: Vec<Bound<'py, PyAny>>,
    /// Debug rendering info of the template owning `nodes` being rendered:
    /// `(source, origin)`; `None` when `engine.debug` is false. Used to port
    /// `Node.render_annotated`'s `template_debug` annotation to Rust nodes.
    pub dbg: Option<(String, Py<PyAny>)>,
    /// Per-render cache of `not settings.USE_THOUSAND_SEPARATOR`: when true,
    /// stock's `numberformat.format` fast path returns every exact `int`
    /// unchanged (locale-independent `str(int)` output), so
    /// `render_value_in_context` may stringify ints natively.
    pub plain_int_str: Option<bool>,
    /// Native per-iteration counter slots of the enclosing `{% for %}`
    /// loops (innermost last), read by rewritten `Node::FlVar` output
    /// nodes. Pushed/popped by `do_for` around its iteration loop.
    pub forstack: Vec<ForState>,
    /// Per-render caches of `context.autoescape` / `use_l10n` / `use_tz`
    /// (read on every variable output). Only `{% autoescape %}` (which
    /// updates the cache itself) and Python code holding the context can
    /// change them; callers invalidate after bridged nodes and the
    /// Python-reentrant loader tags (`invalidate_flags`).
    ae_cache: std::cell::Cell<Option<bool>>,
    l10n_cache: std::cell::Cell<Option<Option<bool>>>,
    tz_cache: std::cell::Cell<Option<Option<bool>>>,
}

/// One active `{% for %}` loop's counter state (0-based index, item count).
#[derive(Clone, Copy)]
pub struct ForState {
    pub i: i64,
    pub len: i64,
}

impl<'py> Rctx<'py> {
    /// Bind to a live Context whose `render_context.push_state(template)` has
    /// happened (caller guarantees `context.template` is the rendering Rust
    /// template). `engine` is that template's engine.
    pub fn new(
        ctx: Bound<'py, PyAny>,
        engine: Bound<'py, PyAny>,
    ) -> PyResult<Self> {
        let dicts = ctx.getattr("dicts")?.cast_into::<PyList>()?;
        let snapshot = dicts.iter().collect();
        Ok(Self {
            ctx,
            engine,
            snapshot,
            dicts,
            rc: std::cell::RefCell::new(None),
            stock_ok: std::cell::OnceCell::new(),
            dbg: None,
            plain_int_str: None,
            forstack: Vec::new(),
            ae_cache: std::cell::Cell::new(None),
            l10n_cache: std::cell::Cell::new(None),
            tz_cache: std::cell::Cell::new(None),
        })
    }

    /// Cached `plain_int_str` decision for direct int renders (`FlVar`):
    /// exact-int dict entries stringify as `str(int)` under stock's
    /// numberformat fast path unless grouping is enabled.
    pub fn int_str_is_plain(&mut self) -> PyResult<bool> {
        if let Some(v) = self.plain_int_str {
            return Ok(v);
        }
        let py = self.py();
        let v = !helpers(py)?
            .settings
            .bind(py)
            .getattr("USE_THOUSAND_SEPARATOR")?
            .is_truthy()?;
        self.plain_int_str = Some(v);
        Ok(v)
    }

    /// Re-snapshot after any push/pop so the cached walk sees the new stack.
    pub fn invalidate(&mut self) -> PyResult<()> {
        self.dicts = self.ctx.getattr("dicts")?.cast_into::<PyList>()?;
        self.snapshot = self.dicts.iter().collect();
        Ok(())
    }

    /// `context.autoescape` (live attribute; the autoescape tag sets it).
    pub fn autoescape(&self) -> PyResult<bool> {
        if let Some(v) = self.ae_cache.get() {
            return Ok(v);
        }
        let v = self
            .ctx
            .getattr(pyo3::intern!(self.py(), "autoescape"))?
            .is_truthy()?;
        self.ae_cache.set(Some(v));
        Ok(v)
    }

    /// Record a known `context.autoescape` value (the autoescape tag just
    /// assigned it).
    pub fn set_autoescape_cache(&self, v: Option<bool>) {
        self.ae_cache.set(v);
    }

    /// `context.use_l10n` (None unless explicitly set).
    pub fn use_l10n(&self) -> PyResult<Option<bool>> {
        if let Some(v) = self.l10n_cache.get() {
            return Ok(v);
        }
        let v = opt_bool(&self.ctx, pyo3::intern!(self.py(), "use_l10n"))?;
        self.l10n_cache.set(Some(v));
        Ok(v)
    }

    pub fn use_tz(&self) -> PyResult<Option<bool>> {
        if let Some(v) = self.tz_cache.get() {
            return Ok(v);
        }
        let v = opt_bool(&self.ctx, pyo3::intern!(self.py(), "use_tz"))?;
        self.tz_cache.set(Some(v));
        Ok(v)
    }

    /// Drop the cached context flags: Python code holding the context
    /// (bridged nodes, Python-rendered templates/blocks) may have assigned
    /// them.
    pub fn invalidate_flags(&self) {
        self.ae_cache.set(None);
        self.l10n_cache.set(None);
        self.tz_cache.set(None);
    }

    #[must_use]
    pub fn py(&self) -> Python<'py> {
        self.ctx.py()
    }

    /// Port of `BaseContext.__getitem__`: top layer first, `key in d`
    /// containment, then `d[key]`. Returns `None` on miss (`KeyError`).
    pub fn get_item(&self, key: &str) -> PyResult<Option<Bound<'py, PyAny>>> {
        for d in self.snapshot.iter().rev() {
            if d.contains(key)? {
                return Ok(Some(d.get_item(key)?));
            }
        }
        Ok(None)
    }
    /// Native root `Context.__getitem__`: same snapshot walk as `get_item`,
    /// but a benign miss (or a layer raising one of the five exception
    /// classes tier 1 of `_resolve_lookup` swallows) yields `Ok(None)` so
    /// callers fall through to the attribute/list-index tiers exactly like
    /// stock's `KeyError` fall-through; any other error propagates.
    pub fn get_item_root(
        &self,
        key: &Bound<'py, pyo3::types::PyString>,
    ) -> PyResult<Option<Bound<'py, PyAny>>> {
        let py = self.py();
        let benign = |e: &pyo3::PyErr| {
            use pyo3::exceptions::{
                PyAttributeError, PyIndexError, PyKeyError, PyTypeError, PyValueError,
            };
            crate::filters::is_err::<PyTypeError>(py, e)
                || crate::filters::is_err::<PyAttributeError>(py, e)
                || crate::filters::is_err::<PyKeyError>(py, e)
                || crate::filters::is_err::<PyValueError>(py, e)
                || crate::filters::is_err::<PyIndexError>(py, e)
        };
        let ctx_dict = helpers(py)?.context_dict.as_ptr();
        for d in self.snapshot.iter().rev() {
            // Exact `dict` / `ContextDict` (which overrides no lookup
            // method): `key in d` + `d[key]` is one dict probe.
            if d.is_exact_instance_of::<PyDict>() || d.get_type().as_ptr() == ctx_dict {
                // SAFETY: both types are `dict` or a plain subclass of it.
                let dict = unsafe { d.cast_unchecked::<PyDict>() };
                match dict.get_item(key) {
                    Ok(Some(v)) => return Ok(Some(v)),
                    Ok(None) => continue,
                    Err(e) if benign(&e) => return Ok(None),
                    Err(e) => return Err(e),
                }
            }
            match d.contains(key) {
                Ok(true) => match d.get_item(key) {
                    Ok(v) => return Ok(Some(v)),
                    Err(e) if benign(&e) => return Ok(None),
                    Err(e) => return Err(e),
                },
                Ok(false) => {}
                Err(e) if benign(&e) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }


    /// `context[key] = value` → `self.dicts[-1][key] = value`.
    pub fn set_item(&self, key: &str, value: &Bound<'py, PyAny>) -> PyResult<()> {
        let n = self.dicts.len();
        if n > 0 {
            self.dicts.get_item(n - 1)?.set_item(key, value)?;
        }
        Ok(())
    }

    /// `set_item` with a prebuilt (interned) key.
    pub fn set_item_key(&self, key: &Bound<'py, pyo3::types::PyString>, value: &Bound<'py, PyAny>) -> PyResult<()> {
        let n = self.dicts.len();
        if n > 0 {
            self.dicts.get_item(n - 1)?.set_item(key, value)?;
        }
        Ok(())
    }

    /// Real `context.push(dict)` (returns the `ContextDict` for scoped use).
    pub fn push(&self, d: Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
        self.ctx.call_method1("push", (d,))
    }

    /// Real `context.pop()`.
    pub fn pop(&self) -> PyResult<()> {
        self.ctx.call_method0("pop")?;
        Ok(())
    }

    /// Native scoped layer push: appends `d` (or a fresh dict for `None`,
    /// like `context.push()`) to the live `context.dicts` and to the
    /// snapshot, which is exactly what `ContextDict.__init__` does. Only
    /// taken when `ctx` is an exact `django.template.context.Context` (a
    /// subclass may override `push`/`pop`) and the cached `dicts` list is
    /// still the live one; otherwise returns `false` and the caller must
    /// use the Python `push` path.
    pub fn push_layer(&mut self, d: Option<&Bound<'py, PyAny>>) -> PyResult<bool> {
        let py = self.py();
        if !exact_stock_context(py, &self.ctx)? {
            return Ok(false);
        }
        let dicts = self.ctx.getattr("dicts")?.cast_into::<PyList>()?;
        if dicts.as_ptr() != self.dicts.as_ptr() {
            return Ok(false);
        }
        let layer = match d {
            Some(d) => d.clone(),
            None => PyDict::new(py).into_any(),
        };
        dicts.append(&layer)?;
        self.snapshot.push(layer);
        Ok(true)
    }

    /// Counterpart of `push_layer`: pop the top layer natively. Returns
    /// `false` when the fast path is unavailable (type or list identity
    /// changed, or the stack is at the builtin layer — stock `pop` would
    /// raise `ContextPopException` there); the caller decides what the
    /// stock-equivalent fallback is.
    pub fn pop_layer(&mut self) -> PyResult<bool> {
        let py = self.py();
        if !exact_stock_context(py, &self.ctx)? {
            return Ok(false);
        }
        let dicts = self.ctx.getattr("dicts")?.cast_into::<PyList>()?;
        if dicts.as_ptr() != self.dicts.as_ptr() || dicts.len() < 2 {
            return Ok(false);
        }
        dicts.del_item(dicts.len() - 1)?;
        self.snapshot.pop();
        Ok(true)
    }

    // ---- render_context helpers (RenderContext: top-dict-only semantics) --

    /// `context.render_context`, fetched-and-cached on first use this render
    /// (see the `rc` field docs).
    pub fn render_context(&self) -> PyResult<Bound<'py, PyAny>> {
        Ok(self.rc_pair()?.0)
    }

    /// Populate-on-demand and return clones of `(rc, rc.dicts)`. The tuple
    /// is cached for the render's lifetime: stock mutates `render_context`
    /// in place (frames are appended to the same `dicts` list), so the pair
    /// stays live across any push/pop.
    pub(crate) fn rc_pair(&self) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyList>)> {
        {
            let slot = self.rc.borrow();
            if let Some((rc, rc_dicts)) = slot.as_ref() {
                return Ok((rc.clone(), rc_dicts.clone()));
            }
        }
        let rc = self.ctx.getattr("render_context")?;
        let rc_dicts = rc.getattr("dicts")?.cast_into::<PyList>()?;
        let pair = (rc, rc_dicts);
        *self.rc.borrow_mut() = Some(pair.clone());
        Ok(pair)
    }

    /// The top frame of `render_context.dicts` — `RenderContext` lookups
    /// (`__contains__`, `get`, `__getitem__`, `__setitem__`, `setdefault`)
    /// all see only this dict.
    pub fn rc_frame(&self) -> PyResult<Bound<'py, PyAny>> {
        let (_, rc_dicts) = self.rc_pair()?;
        let n = rc_dicts.len();
        if n == 0 {
            return Err(pyo3::exceptions::PyIndexError::new_err(
                "render_context has no frame",
            ));
        }
        rc_dicts.get_item(n - 1)
    }

    /// `node in context.render_context`.
    pub fn rc_contains(&self, key: &Bound<'py, PyAny>) -> PyResult<bool> {
        self.rc_frame()?.contains(key)
    }

    /// `context.render_context[key]` (`KeyError` propagates).
    pub fn rc_get(&self, key: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
        self.rc_frame()?.get_item(key)
    }

    /// `context.render_context[key] = value`.
    pub fn rc_set(&self, key: &Bound<'py, PyAny>, value: &Bound<'py, PyAny>) -> PyResult<()> {
        self.rc_frame()?.set_item(key, value)
    }

    /// `context.render_context.setdefault(key, default)` — on a miss writes
    /// `default` into the top frame and returns it; on a hit returns the
    /// stored value (mirrors `BaseContext.setdefault`).
    pub fn rc_setdefault(
        &self,
        key: &Bound<'py, PyAny>,
        default: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let frame = self.rc_frame()?;
        if let Ok(v) = frame.get_item(key) {
            return Ok(v);
        }
        frame.set_item(key, default)?;
        Ok(default.clone())
    }

    /// `key in context` (`BaseContext.__contains__`: top layer first).
    /// NOTE: `dicts.contains(key)` would compare the key against the layer
    /// *objects* (always False); must walk layers like the snapshot walk.
    pub fn context_has(&self, key: &str) -> PyResult<bool> {
        for d in self.snapshot.iter().rev() {
            if d.contains(key)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Port of `BaseContext.set_upward`: set in the highest layer that
    /// already has `key`, else the top layer.
    pub fn set_upward(&self, key: &str, value: &Bound<'py, PyAny>) -> PyResult<()> {
        for d in self.snapshot.iter().rev() {
            if d.contains(key)? {
                return d.set_item(key, value);
            }
        }
        self.set_item(key, value)
    }

    /// `context.get(key)` (`None` default → `Ok(None)`): the snapshot walk
    /// when the context is exactly `Context`/`RequestContext` (neither
    /// overrides `get`), the real method otherwise.
    pub fn get(&self, key: &str) -> PyResult<Option<Bound<'py, PyAny>>> {
        let py = self.py();
        if stock_get_type(py, &self.ctx)? {
            return self.get_item(key);
        }
        let v = self.ctx.call_method1("get", (key,))?;
        Ok(if v.is_none() { None } else { Some(v) })
    }

    /// `context.new(values)` — fresh sibling context (same autoescape/tz).
    pub fn new_context(&self, values: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
        if let Some(c) = native_new(&self.ctx, values)? {
            return Ok(c);
        }
        self.ctx.call_method1("new", (values,))
    }

    #[must_use]
    pub fn dicts(&self) -> &Bound<'py, PyList> {
        &self.dicts
    }
}

fn opt_bool(obj: &Bound<'_, PyAny>, attr: &Bound<'_, pyo3::types::PyString>) -> PyResult<Option<bool>> {
    let v = obj.getattr(attr)?;
    if v.is_none() {
        Ok(None)
    } else {
        Ok(Some(v.is_truthy()?))
    }
}

/// True when `ctx` is exactly `django.template.context.Context` (not a
/// subclass: subclasses may override `push`/`pop` semantics).
fn exact_stock_context(py: Python<'_>, ctx: &Bound<'_, PyAny>) -> PyResult<bool> {
    static CONTEXT_TYPE: PyOnceLock<Py<pyo3::types::PyType>> = PyOnceLock::new();
    let ty = CONTEXT_TYPE.get_or_try_init(py, || -> PyResult<Py<pyo3::types::PyType>> {
        Ok(py
            .import("django.template.context")?
            .getattr("Context")?
            .cast_into::<pyo3::types::PyType>()?
            .unbind())
    })?;
    Ok(ctx.get_type().as_ptr() == ty.bind(py).as_ptr())
}

/// True when `ctx` is exactly `Context` or `RequestContext` (stock `get`).
fn stock_get_type(py: Python<'_>, ctx: &Bound<'_, PyAny>) -> PyResult<bool> {
    static TYPES: PyOnceLock<(usize, usize, Py<PyAny>)> = PyOnceLock::new();
    let t = TYPES.get_or_try_init(py, || -> PyResult<(usize, usize, Py<PyAny>)> {
        let m = py.import("django.template.context")?;
        let a = m.getattr("Context")?;
        let b = m.getattr("RequestContext")?;
        let keep = pyo3::types::PyTuple::new(py, [&a, &b])?.into_any().unbind();
        Ok((a.as_ptr() as usize, b.as_ptr() as usize, keep))
    })?;
    let p = ctx.get_type().as_ptr() as usize;
    Ok(p == t.0 || p == t.1)
}

struct NewTypes {
    context: Py<PyAny>,
    request_context: Py<PyAny>,
    render_context: Py<PyAny>,
    base_context: Py<PyAny>,
    object_new: Py<PyAny>,
}

/// Native `Context.new(values)` for exact `Context`/`RequestContext`
/// (stock `copy(self)` + `_reset_dicts(values)`): `BaseContext.__copy__`
/// (shallow `__dict__` copy) plus `Context.__copy__`'s `render_context`
/// copy (itself a `BaseContext.__copy__`), then fresh `dicts`; the
/// `RequestContext.new` override drops `_processors_index`. `None` when
/// the shapes differ (caller runs the Python method).
fn native_new<'py>(
    ctx: &Bound<'py, PyAny>,
    values: &Bound<'py, PyAny>,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    let py = ctx.py();
    static T: PyOnceLock<NewTypes> = PyOnceLock::new();
    let t = T.get_or_try_init(py, || -> PyResult<NewTypes> {
        let m = py.import("django.template.context")?;
        Ok(NewTypes {
            context: m.getattr("Context")?.unbind(),
            request_context: m.getattr("RequestContext")?.unbind(),
            render_context: m.getattr("RenderContext")?.unbind(),
            base_context: m.getattr("BaseContext")?.unbind(),
            object_new: py.eval(c"object.__new__", None, None)?.unbind(),
        })
    })?;
    let cls = ctx.get_type();
    let is_request = cls.is(t.request_context.bind(py));
    if !is_request && !cls.is(t.context.bind(py)) {
        return Ok(None);
    }
    let dunder = pyo3::intern!(py, "__dict__");
    let Ok(d) = ctx.getattr(dunder)?.cast_into::<PyDict>() else {
        return Ok(None);
    };
    let Some(rc) = d.get_item("render_context")? else {
        return Ok(None);
    };
    if !rc.get_type().is(t.render_context.bind(py)) {
        return Ok(None);
    }
    let Ok(rcd) = rc.getattr(dunder)?.cast_into::<PyDict>() else {
        return Ok(None);
    };
    let Some(rc_dicts) = rcd.get_item("dicts")? else {
        return Ok(None);
    };
    let Ok(rc_dicts) = rc_dicts.cast_into::<PyList>() else {
        return Ok(None);
    };
    let new_rcd = rcd.copy()?;
    new_rcd.set_item("dicts", rc_dicts.get_slice(0, rc_dicts.len()))?;
    let new_rc = t.object_new.bind(py).call1((rc.get_type(),))?;
    new_rc.setattr(dunder, new_rcd)?;
    let nd = d.copy()?;
    nd.set_item("render_context", new_rc)?;
    let builtins = PyDict::new(py);
    builtins.set_item("True", true)?;
    builtins.set_item("False", false)?;
    builtins.set_item("None", py.None())?;
    let dicts = PyList::new(py, [builtins])?;
    if values.is_instance(t.base_context.bind(py))? {
        let vd = values.getattr("dicts")?;
        let n = vd.len()?;
        for i in 1..n {
            dicts.append(vd.get_item(i)?)?;
        }
    } else if !values.is_none() {
        dicts.append(values)?;
    }
    nd.set_item("dicts", dicts)?;
    if is_request {
        nd.del_item("_processors_index").ok();
    }
    let new = t.object_new.bind(py).call1((cls,))?;
    new.setattr(dunder, nd)?;
    Ok(Some(new))
}
