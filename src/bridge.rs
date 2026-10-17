//! Bridging between the Rust parser and Django's Python template machinery:
//! tag-library import/bridging, and the Python shim-parser plumbing used to
//! hand block tags (and `{% load %}`) to the *stock* compile functions so
//! their nodes (`ExtendsNode`, `BlockNode`, `simple_tag`/`inclusion_tag` nodes, …)
//! execute with full Django fidelity against the live `Context`.

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyDict, PyList, PyString, PyTuple};

use crate::lexer::Token;
use crate::render::value::PyFilterEntry;
use crate::varparse::PyVer;

/// `django.template.library.import_library(path)` → `Library`.
pub fn import_library(py: Python<'_>, path: &str) -> PyResult<Py<PyAny>> {
    py.import("django.template.library")?
        .call_method1("import_library", (path,))
        .map(Bound::unbind)
}

/// Introspection bundle derived once per registered filter function:
/// `(min_args, max_args)` exactly as Django's `FilterExpression.args_check`
/// computes it (`inspect.getfullargspec`, wrapped in `lazy_annotations()` on
/// 5.2) plus the attribute flags every bridge site re-reads.
///
/// Process-wide memo keyed by the function's address; the stored
/// value holds a strong reference to the function, so an address can never be
/// recycled under a live entry. Motivation: `{% load %}` bridging replays
/// *every* filter visible to the shim parser (builtins included — stock
/// `add_library` merges them) into the per-template overlay at *every*
/// compile (stock just dict-merges; only dtl introspects), and one
/// `getfullargspec`/flag-probe pass costs ~1 ms per compile without this.
// The four flags mirror `django.template.base.Filter` attributes one-to-one.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone)]
pub struct FilterMeta {
    pub arity: (usize, usize),
    pub needs_autoescape: bool,
    pub expects_localtime: bool,
    pub is_safe: bool,
    pub string_coerced: bool,
}

pub fn filter_meta(
    py: Python<'_>,
    func: &Bound<'_, PyAny>,
    ver: PyVer,
) -> PyResult<FilterMeta> {
    static MEMO: PyOnceLock<Py<PyDict>> = PyOnceLock::new();
    let memo = MEMO.get_or_try_init(py, || -> PyResult<Py<PyDict>> {
        Ok(PyDict::new(py).unbind())
    })?;
    let memo = memo.bind(py);
    let key = func.as_ptr() as usize;
    if let Some(hit) = memo.get_item(key)? {
        let arity = hit.get_item(0)?.extract()?;
        return Ok(FilterMeta {
            arity,
            needs_autoescape: hit.get_item(1)?.extract()?,
            expects_localtime: hit.get_item(2)?.extract()?,
            is_safe: hit.get_item(3)?.extract()?,
            string_coerced: hit.get_item(4)?.extract()?,
        });
    }
    let inspect = py.import("inspect")?;
    let unwrapped = inspect.call_method1("unwrap", (func,))?;
    let spec = if ver == PyVer::V52 {
        let cm = py
            .import("django.utils.inspect")?
            .getattr("lazy_annotations")?
            .call0()?;
        cm.call_method0("__enter__")?;
        let spec = inspect.call_method1("getfullargspec", (&unwrapped,));
        cm.call_method1("__exit__", (py.None(), py.None(), py.None()))?;
        spec?
    } else {
        inspect.call_method1("getfullargspec", (&unwrapped,))?
    };
    let args = spec.getattr("args")?;
    let defaults = spec.getattr("defaults")?;
    let alen = args.len()?;
    let dlen = if defaults.is_none() {
        0
    } else {
        defaults.len()?
    };
    let meta = FilterMeta {
        arity: (alen - dlen, alen),
        needs_autoescape: flag(func, "needs_autoescape")?,
        expects_localtime: flag(func, "expects_localtime")?,
        is_safe: flag(func, "is_safe")?,
        string_coerced: func.hasattr("__wrapped__").unwrap_or(false),
    };
    memo.set_item(
        key,
        (
            meta.arity,
            meta.needs_autoescape,
            meta.expects_localtime,
            meta.is_safe,
            meta.string_coerced,
            func,
        ),
    )?;
    Ok(meta)
}

fn flag(obj: &Bound<'_, PyAny>, name: &str) -> PyResult<bool> {
    match obj.getattr(name) {
        Ok(v) => v.extract(),
        Err(_) => Ok(false),
    }
}

/// Copy `lib.filters` into `out` (bridged Python filters).
#[allow(clippy::implicit_hasher)]
pub fn bridge_library_filters(
    py: Python<'_>,
    out: &mut std::collections::HashMap<String, PyFilterEntry>,
    lib: &Bound<'_, PyAny>,
    ver: PyVer,
) -> PyResult<()> {
    let filters = lib.getattr("filters")?;
    let items = filters.call_method0("items")?;
    for item in items.try_iter()? {
        let pair = item?;
        let name: String = pair.get_item(0)?.extract()?;
        let func = pair.get_item(1)?;
        let meta = filter_meta(py, &func, ver)?;
        let native = crate::filters::native_for(py, &func, &name)?;
        out.insert(
            name,
            PyFilterEntry {
                arity: meta.arity,
                needs_autoescape: meta.needs_autoescape,
                expects_localtime: meta.expects_localtime,
                is_safe: meta.is_safe,
                native,
                string_coerced: meta.string_coerced,
                func: func.unbind(),
            },
        );
    }
    Ok(())
}

/// Copy `lib.tags` into `out` (bridged compile functions). `load` rides the
/// bridged path: the stock compile func mutates the shim parser's `tags` and
/// `filters` dicts, which `ShimParser::sync_new_entries` harvests back.
#[allow(clippy::implicit_hasher)]
pub fn bridge_library_tags(
    _py: Python<'_>,
    out: &mut std::collections::HashMap<String, Py<PyAny>>,
    lib: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let tags = lib.getattr("tags")?;
    let items = tags.call_method0("items")?;
    for item in items.try_iter()? {
        let pair = item?;
        let name: String = pair.get_item(0)?.extract()?;
        out.insert(name, pair.get_item(1)?.unbind());
    }
    Ok(())
}

/// Build a real `django.template.base.Token` (used for exception
/// annotation `e.token` and for the shim token stream).
pub fn make_py_token(py: Python<'_>, token: &Token) -> Py<PyAny> {
    static TOKEN_CLS: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    static TYPES: PyOnceLock<[Py<PyAny>; 4]> = PyOnceLock::new();
    let cls = TOKEN_CLS.get_or_try_init(py, || -> PyResult<Py<PyAny>> {
        Ok(py.import("django.template.base")?.getattr("Token")?.unbind())
    });
    let types = TYPES.get_or_try_init(py, || -> PyResult<[Py<PyAny>; 4]> {
        let tt = py.import("django.template.base")?.getattr("TokenType")?;
        Ok([
            tt.getattr("TEXT")?.unbind(),
            tt.getattr("VAR")?.unbind(),
            tt.getattr("BLOCK")?.unbind(),
            tt.getattr("COMMENT")?.unbind(),
        ])
    });
    let (Ok(cls), Ok(types)) = (cls, types) else {
        return py.None();
    };
    let idx = match token.kind {
        crate::lexer::TokenKind::Text => 0,
        crate::lexer::TokenKind::Var => 1,
        crate::lexer::TokenKind::Block => 2,
        crate::lexer::TokenKind::Comment => 3,
    };
    let kind = types[idx].bind(py).clone();
    let contents = PyString::new(py, &token.contents).into_any();
    let pos: Py<PyAny> = match token.position {
        None => py.None(),
        Some((a, b)) => PyTuple::new(py, [a, b])
            .map_or_else(|_| py.None(), |t| t.into_any().unbind()),
    };
    let lineno = token
        .lineno
        .into_pyobject(py)
        .map_or_else(|_| py.None().into_bound(py), Bound::into_any);
    let args = PyTuple::new(py, [kind, contents, pos.into_bound(py), lineno]);
    match args {
        Ok(args) => cls
            .bind(py)
            .call(&args, None)
            .map_or_else(|_| py.None(), Bound::unbind),
        Err(_) => py.None(),
    }
}


/// `(shim tokens list, its element objects)`: see `ShimParser::trusted`.
type TrustedStream = (Py<PyList>, Vec<Py<PyAny>>);

/// A persistent `django.template.base.Parser` shim used to hand block tags to
/// stock (or third-party) Python compile functions with full fidelity.
///
/// The shim consumes the *same* token stream position as the Rust parser:
/// before each bridged call the not-yet-consumed Rust tokens are (re)loaded
/// into the shim's reversed stack; after the call, the shim's leftover stack
/// is mapped back onto the Rust parser's stack. `parser.parse()` recursion
/// inside a stock compile function (bodies of `if`/`for`/`block`/…)
/// therefore consumes real tokens from the real shim, including nested
/// bridged tags it dispatches itself.
pub struct ShimParser {
    /// The real `django.template.base.Parser`.
    pub parser: Py<PyAny>,
    /// `(len(tags), len(filters))` as of the last `sync_new_entries` walk;
    /// per-bridged-tag calls re-walk only when either dict changed size.
    sync_counts: std::cell::Cell<(usize, usize)>,
    /// Reusable shim token list captured after each successful `call_tag`:
    /// the shim's `tokens` attribute is exactly this list and element `i` is
    /// the token object standing for Rust token `i`. Stock parsing only pops
    /// from the back and appends real `Token`s, so the next call resyncs by
    /// checking list identity + prefix pointers and reusing the list, and
    /// leftover slots map back by pointer instead of getattr extraction.
    /// `None` initially and after any error.
    trusted: std::cell::RefCell<Option<TrustedStream>>,
    /// The shim's `_dtl_parse` hook: `parser.parse()` from a stock compile
    /// function runs on the Rust parser (see `crate::parser::ParseHook`).
    pub hook: Py<crate::parser::ParseHook>,
}

impl ShimParser {
    /// Construct `Parser(tokens, libraries, builtins, origin)` — the exact
    /// Django call shape from `Template.compile_nodelist` — on the
    /// `_nodes.ShimParser` subclass, whose `parse()` delegates to `hook`.
    pub fn new(
        py: Python<'_>,
        engine: &Bound<'_, PyAny>,
        origin: &Bound<'_, PyAny>,
    ) -> PyResult<Self> {
        static CLS: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
        let cls = CLS
            .get_or_try_init(py, || -> PyResult<_> {
                Ok(py.import("django_rust_templates._nodes")?.getattr("ShimParser")?.unbind())
            })?
            .bind(py);
        let libraries = engine.getattr("template_libraries")?;
        let builtins = engine.getattr("template_builtins")?;
        let empty = PyList::empty(py).into_any().unbind();
        let parser = cls
            .call1((empty, libraries, builtins, origin))?
            .unbind();
        let hook = Py::new(py, crate::parser::ParseHook::default())?;
        parser.bind(py).setattr("_dtl_parse", &hook)?;
        Ok(Self {
            parser,
            hook,
            sync_counts: std::cell::Cell::new((usize::MAX, usize::MAX)),
            trusted: std::cell::RefCell::new(None),
        })
    }
    /// Rust → shim: make the shim's `tokens` list mirror `tokens_rev` (the
    /// not-yet-consumed Rust tokens). Both stacks share the same layout
    /// (back of list = next token: stock `Parser.next_token` is
    /// `self.tokens.pop()`), so the vector is loaded verbatim. Called
    /// before each bridged compile call and when a `parse()` delegated to
    /// the Rust parser returns.
    pub fn load_tokens(&self, py: Python<'_>, tokens_rev: &[Token]) -> PyResult<()> {
        let p = self.parser.bind(py);
        let n = tokens_rev.len();
        // Trusted resync: keep the old list when `parser.tokens` is still
        // that list object, no longer than the stored objects, and its
        // first `n` slots still hold the same token objects (Rust parsing
        // between calls only pops from the back).
        let prev = self.trusted.borrow_mut().take();
        if let Some((lst, mut objs)) = prev {
            let lstb = lst.bind(py).clone();
            let mut ok = p.getattr("tokens")?.as_ptr() == lstb.as_ptr()
                && objs.len() >= n
                && lstb.len() == objs.len();
            if ok {
                for (i, o) in objs.iter().enumerate().take(n) {
                    if !lstb
                        .get_item(i)
                        .is_ok_and(|it| it.as_ptr() == o.bind(py).as_ptr())
                    {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                if lstb.len() > n {
                    lstb.del_slice(n, lstb.len())?;
                }
                objs.truncate(n);
                *self.trusted.borrow_mut() = Some((lst, objs));
                return Ok(());
            }
        }
        let list = PyList::empty(py);
        let mut objs = Vec::with_capacity(n);
        for t in tokens_rev {
            let o = make_py_token(py, t);
            list.append(o.bind(py))?;
            objs.push(o);
        }
        p.setattr("tokens", &list)?;
        *self.trusted.borrow_mut() = Some((list.unbind(), objs));
        Ok(())
    }

    /// Shim → Rust: map the shim's (possibly consumed / extended) `tokens`
    /// list back onto `tokens_rev`. Slots still holding the object loaded
    /// for Rust token `i` map back by pointer (`load_tokens` keeps
    /// `trusted` in lockstep with `tokens_rev`); anything else is
    /// extracted from the Python `Token`.
    pub fn harvest_tokens(&self, py: Python<'_>, tokens_rev: &mut Vec<Token>) -> PyResult<()> {
        let p = self.parser.bind(py);
        let prev = self.trusted.borrow_mut().take();
        let (list, mut objs) = match prev {
            Some((l, o)) if o.len() == tokens_rev.len() => (Some(l), o),
            _ => (None, Vec::new()),
        };
        let leftover_any = p.getattr("tokens")?;
        let leftover: Option<Bound<'_, PyList>> =
            if leftover_any.is_exact_instance_of::<PyList>() {
                Some(leftover_any.cast::<PyList>()?.clone())
            } else {
                None
            };
        // Fast path: still the loaded list, and its slots below
        // `min(len, objs.len())` hold the loaded objects (stock consumes
        // from the back): truncate both stacks in place, then extract any
        // tokens pushed beyond the loaded prefix.
        if let (Some(l), Some(t)) = (&leftover, &list) {
            if l.as_ptr() == t.bind(py).as_ptr() {
                let m = l.len();
                let keep = m.min(objs.len());
                let prefix_ok = (0..keep).all(|i| {
                    l.get_item(i)
                        .is_ok_and(|it| it.as_ptr() == objs[i].bind(py).as_ptr())
                });
                if prefix_ok {
                    tokens_rev.truncate(keep);
                    objs.truncate(keep);
                    for i in keep..m {
                        let item = l.get_item(i)?;
                        tokens_rev.push(extract_py_token(py, &item)?);
                        objs.push(item.unbind());
                    }
                    *self.trusted.borrow_mut() = Some((l.clone().unbind(), objs));
                    return Ok(());
                }
            }
        }
        let mut out: Vec<Token> = Vec::new();
        let mut objs2: Vec<Py<PyAny>> = Vec::new();
        for item in leftover_any.try_iter()? {
            let item = item?;
            out.push(extract_py_token(py, &item)?);
            objs2.push(item.unbind());
        }
        *tokens_rev = out;
        *self.trusted.borrow_mut() = match leftover {
            Some(l) if objs2.len() == l.len() => Some((l.unbind(), objs2)),
            _ => None,
        };
        Ok(())
    }

    /// Replay the shim's `{% load %}` side effects into the Rust overlays:
    /// copy every parser.filters/parser.tags entry not yet present into the
    /// per-template `FilterOverlay`/tag map. Called after a bridged tag call
    /// (the shim's `add_library` may have run inside stock code, e.g. a
    /// library's own compile func — or via the native load replay below).
    /// Entries bound to the same function object as the engine registry are
    /// skipped: the registry resolves them identically (builtins included),
    /// and materializing an overlay entry for each would cost a
    /// `filter_meta`/`native_for` pass per filter per compile.
    #[allow(clippy::implicit_hasher, clippy::too_many_arguments)]
    pub fn sync_new_entries(
        &self,
        py: Python<'_>,
        ver: PyVer,
        force: bool,
        tags: &mut std::collections::HashMap<String, Py<PyAny>>,
        base_tags: &std::collections::HashMap<String, Py<PyAny>>,
        filters: &mut std::collections::HashMap<String, PyFilterEntry>,
        registry: &crate::render::value::FilterRegistry,
    ) -> PyResult<Vec<String>> {
        let mut changed = Vec::new();
        let p = self.parser.bind(py);
        let py_tags = p.getattr("tags")?.cast_into::<PyDict>()?;
        let py_filters = p.getattr("filters")?.cast_into::<PyDict>()?;
        // Stock only mutates these dicts through `add_library` (i.e. a
        // `{% load %}`) — `force` marks that case, including same-length
        // re-registrations. Otherwise equal sizes mean no new entries since
        // the previous walk, so skip the ~180-entry scan every bridged tag
        // call would otherwise pay.
        let sizes = (py_tags.len(), py_filters.len());
        if !force && sizes == self.sync_counts.get() {
            return Ok(changed);
        }
        self.sync_counts.set(sizes);
        for (key, func) in py_tags.iter() {
            let borrowed = key.cast::<PyString>()?.to_str()?;
            let existing = tags
                .get(borrowed)
                .or_else(|| base_tags.get(borrowed));
            if let Some(existing) = existing {
                if existing.bind(py).as_ptr() == func.as_ptr() {
                    continue;
                }
                // Different object under an existing name: a re-registration
                // (e.g. `{% load i18n %}` overriding a builtin tag) — the
                // loaded stock func wins over the native one.
                changed.push(borrowed.to_owned());
            }
            tags.insert(borrowed.to_owned(), func.unbind());
        }
        for (key, func) in py_filters.iter() {
            let borrowed = key.cast::<PyString>()?.to_str()?;
            if let Some(e) = registry.entries.get(borrowed) {
                if e.func.bind(py).as_ptr() == func.as_ptr() {
                    continue;
                }
            }
            let name: String = borrowed.to_owned();
            if filters.contains_key(&name) {
                continue;
            }
            let meta = filter_meta(py, &func, ver)?;
            let native = crate::filters::native_for(py, &func, &name)?;
            filters.insert(
                name,
                PyFilterEntry {
                    arity: meta.arity,
                    needs_autoescape: meta.needs_autoescape,
                    expects_localtime: meta.expects_localtime,
                    is_safe: meta.is_safe,
                    native,
                    func: func.unbind(),
                    string_coerced: meta.string_coerced,
                },
            );
        }
        Ok(changed)
    }

    /// `parser.add_library(lib)` on the shim (used by the native `load`
    /// replay so the shim sees the same libraries for its own later parses).
    pub fn add_library(&self, py: Python<'_>, lib: &Bound<'_, PyAny>) -> PyResult<()> {
        self.parser.bind(py).call_method1("add_library", (lib,))?;
        Ok(())
    }

    /// The shim's `extra_data` dict (stock 6.1 `{% partialdef %}` writes
    /// `partials` here).
    pub fn extra_data(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Ok(self.parser.bind(py).getattr("extra_data")?.unbind())
    }

    /// Re-set the shim's origin (after construction, keeps it in sync with
    /// the Rust parser's origin).
    pub fn set_origin(&self, py: Python<'_>, origin: &Bound<'_, PyAny>) -> PyResult<()> {
        self.parser.bind(py).setattr("origin", origin)?;
        Ok(())
    }
}

fn py_token_kind(t: &Bound<'_, PyAny>) -> PyResult<crate::lexer::TokenKind> {
    use crate::lexer::TokenKind;
    let kind = t.getattr("token_type")?;
    let tt = py0_import_tokentype(t.py())?;
    if kind.eq(&tt.getattr("TEXT")?)? {
        Ok(TokenKind::Text)
    } else if kind.eq(&tt.getattr("VAR")?)? {
        Ok(TokenKind::Var)
    } else if kind.eq(&tt.getattr("BLOCK")?)? {
        Ok(TokenKind::Block)
    } else {
        Ok(TokenKind::Comment)
    }
}

/// Map a Python `Token` back onto the Rust `Token` (leftover shim stream).
fn extract_py_token(_py: Python<'_>, item: &Bound<'_, PyAny>) -> PyResult<Token> {
    Ok(Token {
        kind: py_token_kind(item)?,
        contents: item.getattr("contents")?.str()?.to_string(),
        lineno: item.getattr("lineno")?.extract().unwrap_or(0),
        position: match item.getattr("position") {
            Ok(p) if !p.is_none() => {
                let a = p.get_item(0)?.extract().unwrap_or(0);
                let b = p.get_item(1)?.extract().unwrap_or(0);
                Some((a, b))
            }
            _ => None,
        },
    })
}

fn py0_import_tokentype(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    py.import("django.template.base")?.getattr("TokenType")
}
