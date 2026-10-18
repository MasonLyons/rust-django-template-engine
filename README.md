# django-rust-templates

[![CI](https://github.com/MasonLyons/rust-django-template-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/MasonLyons/rust-django-template-engine/actions/workflows/ci.yml)
[![License: BSD-3-Clause](https://img.shields.io/badge/license-BSD--3--Clause-blue.svg)](LICENSE)

A drop-in replacement for Django's template engine, written in Rust.

It re-implements the Django template language natively: the lexer, parser,
node tree, renderer, and the built-in tags and filters. It produces
**byte-identical output** to stock Django and passes **100% of Django's own
template test suite** on Django 5.2 and 6.1. On the benchmark below it renders
**~15× faster** and compiles templates **~12× faster** than stock Django.

```python
TEMPLATES = [
    {
        "BACKEND": "django_rust_templates.RustTemplates",  # was django.template.backends.django.DjangoTemplates
        "DIRS": [BASE_DIR / "templates"],
        "APP_DIRS": True,
        "OPTIONS": {"context_processors": [...]},  # same options as DjangoTemplates
    },
]
```

> **Status:** early (0.1). Correctness is gated on Django's own test suite,
> but the project hasn't had wide production use yet.

## Contents

- [Installation](#installation)
- [Usage](#usage)
- [Compatibility](#compatibility)
- [Performance](#performance)
- [Testing](#testing)
- [How it works](#how-it-works)
- [Development](#development)
- [License](#license)

## Installation

```bash
pip install django-rust-templates
```

Prebuilt wheels are published for CPython 3.12, 3.13 and 3.14 on Linux
x86_64 and aarch64 (manylinux glibc ≥ 2.28 and musllinux). It requires
Django ≥ 5.2.

### Building from source

On other platforms pip falls back to the source distribution, or you can build
from git. Either way you need a Rust toolchain ([rustup](https://rustup.rs);
the pinned version in `rust-toolchain.toml` is installed automatically) and
Python ≥ 3.12.

```bash
pip install git+https://github.com/MasonLyons/rust-django-template-engine.git
```

Or from a checkout:

```bash
git clone https://github.com/MasonLyons/rust-django-template-engine.git
cd rust-django-template-engine
pip install .
```

## Usage

### As a Django backend

Swap the `BACKEND` in `TEMPLATES` (see above). `RustTemplates` accepts the
same `DIRS`, `APP_DIRS` and `OPTIONS` as `DjangoTemplates`: `context_processors`,
`debug`, `loaders`, `string_if_invalid`, `file_charset`, `libraries`,
`builtins` and `autoescape`. `render()`, `render_to_string()`, template
loaders, `{% load %}` and your custom tag libraries work unchanged.

### Forms

Django renders forms and widgets (`{{ form }}`, `{{ field }}`,
`form.as_div`) through `FORM_RENDERER`, whose default builds its own
*stock* template engine, so form HTML doesn't touch `TEMPLATES` at all.
To render it natively too:

```python
FORM_RENDERER = "django_rust_templates.forms.RustFormRenderer"
```

It loads the same built-in form templates as Django's default
`DjangoTemplates` renderer (and each app's `templates` directory), so
output is identical. Rendering an 8-field form is about 2.6× faster;
the rest of the time is Django's own form and widget code.

### Standalone

`Engine` mirrors `django.template.Engine` (Django settings must be
configured, as with the stock engine):

```python
from django.template import Context
from django_rust_templates import Engine

engine = Engine(dirs=["templates"], libraries={"mytags": "myapp.templatetags.mytags"})
template = engine.from_string("Hello {{ user.name|title }}!")
template.render(Context({"user": {"name": "ada lovelace"}}))  # 'Hello Ada Lovelace!'
```

## Compatibility

| | Supported |
|---|---|
| Django | 5.2 LTS, 6.1 |
| Python | 3.12, 3.13, 3.14 |
| Platforms | Linux x86_64 and aarch64 (e.g. AWS Graviton), tested in CI; anything Rust and PyO3 target should build |

- **Output parity.** Rendered output, error types and error messages
  (including line numbers and the Django 6.1 `Template: name,` prefix) match
  stock Django exactly. Any difference is a bug, so please
  [open an issue](https://github.com/MasonLyons/rust-django-template-engine/issues).
- **Built-in tags and filters** run natively, including `extends`/`block`,
  `include`, `for`, `if`, `with`, `cycle`, `url`, `csrf_token`, Django 6.1
  `partialdef`/`partial`, `{% translate "literal" %}`, `{% blocktranslate %}`,
  `{% static %}`, and filters such as `date`, `floatformat`, `urlize`, `join`,
  `striptags`, `truncatechars_html`/`truncatewords_html` and `filesizeformat`,
  plus humanize's `naturaltime` and, on integers, `intcomma`, `intword` and
  `ordinal`.
- **Custom and third-party tag libraries** keep working. `simple_tag` and
  `inclusion_tag` run on native helper nodes. Other tags (custom compile
  functions, `simple_block_tag`) are
  bridged to their Python implementation, so their own code runs at Python
  speed while the rest of the template stays native. When a bridged *block*
  tag (e.g. `{% localtime %}`, `{% cache %}`, `simple_block_tag`) calls
  `parser.parse()`, its body compiles on the Rust parser and renders
  natively.
- **The Python node API** (`template.nodelist`, `get_nodes_by_type`,
  `render_annotated`) is exposed through stock-compatible adapter nodes, so
  tooling that inspects compiled templates keeps working.

## Performance

Microseconds per operation; lower is better. **×** is the speedup over
stock Django.

| scenario | render stock | render rust | render × | compile stock | compile rust | compile × |
|---|---:|---:|---:|---:|---:|---:|
| simple_var | 12.4 µs | 2.6 µs | **4.74×** | 23.0 µs | 1.9 µs | **12.16×** |
| filter_chain | 71.1 µs | 5.8 µs | **12.20×** | 149.1 µs | 4.0 µs | **37.60×** |
| many_vars | 146.3 µs | 7.9 µs | **18.41×** | 512.9 µs | 31.5 µs | **16.26×** |
| loop | 4595.4 µs | 133.8 µs | **34.35×** | 96.6 µs | 5.4 µs | **17.96×** |
| if_branch | 1647.1 µs | 52.3 µs | **31.52×** | 113.1 µs | 7.7 µs | **14.71×** |
| attr_lookup | 1796.0 µs | 104.8 µs | **17.14×** | 164.1 µs | 9.2 µs | **17.76×** |
| autoescape | 697.9 µs | 41.8 µs | **16.69×** | 66.7 µs | 4.4 µs | **15.27×** |
| cycle_reset | 709.2 µs | 70.1 µs | **10.11×** | 115.7 µs | 6.6 µs | **17.51×** |
| firstof_widthratio | 29.9 µs | 3.0 µs | **9.88×** | 94.7 µs | 5.6 µs | **16.81×** |
| date_filter | 3038.9 µs | 91.9 µs | **33.06×** | 89.6 µs | 4.0 µs | **22.50×** |
| url_tag | 51.1 µs | 5.9 µs | **8.71×** | 39.8 µs | 2.5 µs | **16.20×** |
| include | 3075.3 µs | 313.4 µs | **9.81×** | 65.6 µs | 9.8 µs | **6.68×** |
| extends | 1626.7 µs | 40.3 µs | **40.38×** | 112.6 µs | 13.0 µs | **8.67×** |
| nested_table | 3516.5 µs | 105.3 µs | **33.40×** | 111.7 µs | 8.0 µs | **13.89×** |
| with_if_scopes | 89.1 µs | 10.7 µs | **8.36×** | 710.6 µs | 40.7 µs | **17.45×** |
| spaceless_table | 22.3 µs | 2.3 µs | **9.61×** | 11.9 µs | 1.7 µs | **7.15×** |
| bridged_simple_tag | 259.9 µs | 26.7 µs | **9.74×** | 72.6 µs | 17.1 µs | **4.24×** |
| cached_extends | 217.4 µs | 28.5 µs | **7.64×** | 1.2 µs | 0.3 µs | **3.64×** |
| value_types | 4411.6 µs | 276.0 µs | **15.98×** | 51.5 µs | 3.0 µs | **17.44×** |
| csrf_token_loop | 306.5 µs | 14.7 µs | **20.84×** | 43.1 µs | 3.2 µs | **13.28×** |
| inclusion_tag | 1085.7 µs | 155.2 µs | **6.99×** | 73.6 µs | 15.9 µs | **4.63×** |
| include_with | 3756.9 µs | 427.3 µs | **8.79×** | 101.5 µs | 14.7 µs | **6.91×** |
| index_none_lookup | 11742.6 µs | 202.2 µs | **58.08×** | 101.0 µs | 8.1 µs | **12.49×** |
| object_attrs | 2767.6 µs | 88.7 µs | **31.19×** | 78.2 µs | 6.1 µs | **12.75×** |
| object_methods | 1322.7 µs | 127.6 µs | **10.37×** | 65.6 µs | 5.2 µs | **12.70×** |
| object_properties | 2691.5 µs | 116.4 µs | **23.12×** | 97.3 µs | 7.5 µs | **13.00×** |
| realistic_page | 19678.7 µs | 1513.4 µs | **13.00×** | 1.2 µs | 0.3 µs | **3.56×** |
| floatformat | 12902.6 µs | 887.7 µs | **14.54×** | 221.1 µs | 8.3 µs | **26.79×** |
| urlize | 10992.3 µs | 302.1 µs | **36.38×** | 128.9 µs | 5.0 µs | **25.74×** |
| join | 3017.2 µs | 246.1 µs | **12.26×** | 139.8 µs | 5.1 µs | **27.39×** |
| i18n_tags | 3213.5 µs | 348.8 µs | **9.21×** | 168.5 µs | 14.3 µs | **11.82×** |
| aware_dates | 19351.8 µs | 1468.6 µs | **13.18×** | 226.5 µs | 8.3 µs | **27.31×** |
| localtime_block | 19014.6 µs | 1466.5 µs | **12.97×** | 244.5 µs | 53.3 µs | **4.58×** |
| simple_block_tag | 1808.2 µs | 121.4 µs | **14.89×** | 212.3 µs | 100.2 µs | **2.12×** |
| striptags_intcomma | 5095.0 µs | 616.6 µs | **8.26×** | 150.5 µs | 12.6 µs | **11.95×** |
| truncate_html_sizes | 14623.8 µs | 795.7 µs | **18.38×** | 203.6 µs | 15.9 µs | **12.84×** |
| engine_e2e | 865.5 µs | 46.0 µs | **18.82×** | 70.3 µs | 8.3 µs | **8.47×** |
| **geomean** | | | **15.06×** | | | **11.81×** |

*Linux x86_64 (AMD EPYC 7601), Python 3.13.15, Django 6.1.2, release build.*

**Methodology.** [`benchmarks/bench.py`](benchmarks/bench.py) first renders
every scenario on both engines and aborts unless the output is byte-identical,
because a speedup on wrong output is worthless. It then times each engine in
its own subprocess (so neither inherits the other's allocator or GC state),
pinned to one CPU core, and reports the minimum of 7 `timeit` repeats.
"Compile" is one template parse. In `cached_extends` and `realistic_page`
(cached loader) it is a cache-hit `get_template()`, the per-request cost in
production.

**Reading the numbers.**

- `realistic_page` is the closest to a real view: `{% extends %}` with
  `block.super`, an `{% include %}` per row, model-like objects with
  attributes, methods and properties, and the `date`, `floatformat`, `urlize`
  and `join` filters, all through the cached loader.
- The weakest rows are `simple_var` (4.7×, dominated by fixed per-render
  overhead) and `inclusion_tag` (7.0×: the tag function is Python, and each
  call builds a fresh `Context` for the included template).
- This measures template time only. A view's database queries and middleware
  are unaffected, so whole-request gains depend on how much of each request
  is spent rendering.
- [`benchmarks/realworld.py`](benchmarks/realworld.py) renders a full page
  through the backend with a request: context processors, `{% static %}`,
  `{% url %}`, i18n tags, messages, a form and humanize filters. Python code
  that the page calls (context processors, form and widget classes,
  translation catalog lookups, bridged tags) runs at the same speed on both
  engines, so this page is about 3.5× faster rather than 15×.

HTML escaping and the lexer's tag scanner use portable SIMD: `fearless_simd`
picks SSE2 on x86_64 and NEON on aarch64, and `memchr` picks the best
instruction set at runtime.

### Compared with Jinja2

Django also ships a Jinja2 backend, and Jinja2 is the usual choice when DTL
is too slow. [`benchmarks/jinja2_bench.py`](benchmarks/jinja2_bench.py) runs
the same scenarios through all three engines. Each scenario has a
hand-written Jinja2 template that does the same work in idiomatic Jinja2
(`loop.cycle`, `super()`, `p.full()`), and the script aborts unless all three
outputs match byte for byte. The one exception is how the entities are spelled:
markupsafe writes `&#39;` where Django writes `&#x27;`.

| scenario | render stock | render jinja2 | render rust | rust vs jinja2 |
|---|---:|---:|---:|---:|
| simple_var | 12.0 µs | 14.3 µs | 2.7 µs | **5.37×** |
| filter_chain | 70.5 µs | 55.7 µs | 5.8 µs | **9.55×** |
| many_vars | 145.0 µs | 41.8 µs | 7.5 µs | **5.55×** |
| loop | 4687.9 µs | 573.3 µs | 131.4 µs | **4.36×** |
| if_branch | 1653.0 µs | 52.3 µs | 52.3 µs | **1.00×** |
| attr_lookup | 1839.5 µs | 469.1 µs | 101.6 µs | **4.62×** |
| autoescape | 705.1 µs | 199.1 µs | 42.8 µs | **4.65×** |
| cycle_reset | 726.1 µs | 1095.8 µs | 69.6 µs | **15.75×** |
| firstof_widthratio | 30.1 µs | 18.8 µs | 3.0 µs | **6.22×** |
| date_filter | 2992.4 µs | 2406.9 µs | 90.9 µs | **26.47×** |
| url_tag | 50.2 µs | 52.8 µs | 6.0 µs | **8.78×** |
| include | 3006.5 µs | 865.6 µs | 292.6 µs | **2.96×** |
| extends | 1641.8 µs | 94.5 µs | 28.7 µs | **3.29×** |
| nested_table | 3562.6 µs | 127.7 µs | 108.5 µs | **1.18×** |
| with_if_scopes | 88.5 µs | 14.5 µs | 10.5 µs | **1.38×** |
| bridged_simple_tag | 262.9 µs | 121.2 µs | 26.7 µs | **4.54×** |
| cached_extends | 217.2 µs | 56.3 µs | 28.8 µs | **1.95×** |
| csrf_token_loop | 313.4 µs | 52.8 µs | 14.8 µs | **3.56×** |
| include_with | 3764.5 µs | 1162.4 µs | 390.4 µs | **2.98×** |
| index_none_lookup | 11763.6 µs | 220.9 µs | 193.1 µs | **1.14×** |
| object_attrs | 2788.8 µs | 231.4 µs | 86.1 µs | **2.69×** |
| object_methods | 1321.7 µs | 498.0 µs | 122.9 µs | **4.05×** |
| object_properties | 2684.8 µs | 350.2 µs | 114.0 µs | **3.07×** |
| realistic_page | 19678.5 µs | 14524.2 µs | 1500.0 µs | **9.68×** |
| floatformat | 12933.4 µs | 10654.0 µs | 885.6 µs | **12.03×** |
| urlize | 11253.3 µs | 10490.7 µs | 301.7 µs | **34.77×** |
| join | 3047.0 µs | 1034.8 µs | 251.4 µs | **4.12×** |
| i18n_tags | 3231.8 µs | 2616.2 µs | 348.3 µs | **7.51×** |
| aware_dates | 19275.8 µs | 15657.8 µs | 1482.7 µs | **10.56×** |
| striptags_intcomma | 5105.1 µs | 4305.4 µs | 602.6 µs | **7.14×** |
| truncate_html_sizes | 14623.5 µs | 13136.0 µs | 790.8 µs | **16.61×** |
| engine_e2e | 783.3 µs | 45.6 µs | 16.3 µs | **2.80×** |
| **geomean** | | | | **4.93×** |

*Same machine as above; Jinja2 3.1.6. Over the same rows, Jinja2 renders
3.35× faster than stock Django and this engine renders 16.54× faster.*

- **Compile.** Jinja2 compiles every template to Python bytecode. A fresh
  parse takes about 9× longer than stock Django and **149×** longer than
  this engine (geomean). A cache-hit `get_template()` is 2.8× faster here
  than in Jinja2.
- **Where Jinja2 keeps up.** `if_branch` (1.00×) is a loop of integer
  comparisons. Jinja2 turns `n > 90` into a Python `>` on the raw value,
  which is as fast as the native comparison here. Rows made of plain loops
  and lookups (`nested_table`, `index_none_lookup`, `with_if_scopes`) are
  also close to even.
- **Where it doesn't.** Jinja2 has no `date`, `floatformat`, `urlize`,
  `intcomma` or `truncatewords_html`, and its `filesizeformat` and
  `truncate` give different output from Django's. Django projects register
  Django's own filter functions in Jinja2 (as django-jinja does), and the
  benchmark does too, so those rows run Python code at stock speed.
  Here they are native, which is why `realistic_page`, `urlize`, `date_filter`,
  `aware_dates` and `truncate_html_sizes` are 9–35× faster than Jinja2.
- **Setup.** `autoescape=True`, `ChainableUndefined` (missing lookups render
  as empty, as in DTL), `auto_reload=False`, and the i18n extension wired to
  Django's gettext. Because Jinja2 always caches compiled templates, the
  Django engines use the cached loader in every loader-based row, so the
  stock `include` and `extends` rows are not comparable with the table above.
- **Skipped.** `spaceless_table`, `inclusion_tag` and `value_types` are left
  out. Jinja2 has no `spaceless` and no inclusion tags, and it prints
  `str(value)` where DTL localizes dates and numbers.

Run the benchmarks yourself:

```bash
maturin develop --release
python benchmarks/bench.py                   # full table
python benchmarks/bench.py --only realistic  # filter scenarios
python benchmarks/realworld.py               # end-to-end page via the backend
python benchmarks/jinja2_bench.py            # stock vs Jinja2 vs Rust (pip install jinja2)
cargo bench --bench compile_bench            # Rust-only lexer micro-benchmarks
```

The CI workflow can also be run by hand (**Actions → CI → Run workflow**). It
then runs the benchmark on both x86_64 and arm64 runners and uploads the
tables as artifacts. Shared CI runners are noisy, so treat those numbers as
indicative only.

## Testing

Correctness is checked at three levels, all run in CI on both Django versions
and both CPU architectures.

1. **Django's own template test suite.** The real `template_tests` and
   `template_loader` suites from Django's repository run with the engine
   swapped in (about 5,900 renders per run). A small patch to Django's
   `runtests.py` replaces `Engine`, `Template` and the `DjangoTemplates`
   backend; see [`scripts/`](scripts/). Results: **100%** on 5.2 and 6.1.
   ```bash
   python scripts/run_django_test_suite.py --branch stable/6.1.x
   python scripts/run_django_test_suite.py --branch stable/5.2.x
   ```
   The script shallow-clones the matching Django branch into `.django-cache/`
   on first run. `scripts/known_failures-*.txt` lists accepted failures; at
   the moment it holds only one, which depends on the environment and fails
   only when run as root.
2. **Parity tests.** `pytest` renders every case through stock Django and the
   Rust engine and asserts identical output or identical errors. Cases cover
   tags, filters, escaping edge cases at SIMD block boundaries and in UTF-8,
   lexer edge cases, loaders, inheritance, partials, custom tag libraries,
   the Python nodelist API and the backend. It also includes a
   [Hypothesis](https://hypothesis.readthedocs.io) **differential fuzzer**
   that generates random nested templates and contexts.
   ```bash
   pytest
   DTL_FUZZ_ITERS=20000 pytest tests/test_fuzz.py  # longer fuzz run
   ```
3. **Rust unit tests** for the lexer, the variable/filter parser and the
   SIMD routines, checked against reference implementations:
   ```bash
   cargo test --release
   ```

### With Docker

Every service rebuilds the extension from your working tree before it runs:

```bash
make test      # pytest on Django 5.2 and 6.1
make suite     # Django's own template suite on 5.2 and 6.1
make fuzz      # long fuzz run (DTL_FUZZ_ITERS=20000 make fuzz)
make bench     # release-build benchmarks
```

## How it works

```
template source ──► lexer ──► parser ──► Rust node tree ──► renderer ──► str
   (src/lexer.rs)   (SIMD tag scan)  (src/parser.rs,      (src/render/)
                                      src/tags/, src/filters/)
```

- **Lexer** (`src/lexer.rs`): an exact port of Django's `tag_re` splitting
  and verbatim handling, scanning for tags with `memchr`.
- **Parser** (`src/parser.rs`, `src/tags/`, `src/varparse.rs`): builds a
  native node tree. Variables and filter expressions are parsed once, with
  lookup keys interned.
- **Renderer** (`src/render/`): walks the tree against a Rust context stack
  that mirrors `Context`'s layering. Python values are resolved through
  PyO3, using type-slot checks and exception-free misses on hot paths.
  Strings and integers take native fast paths, and dates, decimals and lazy
  objects go through native ports of Django's localisation and escaping.
- **Bridge** (`src/bridge.rs`): tags without a native implementation are
  compiled by their Python compile function against a token stream that
  mirrors stock Django's, then rendered as Python nodes inside the native
  tree.
- **Python layer** (`python/django_rust_templates/`): the `RustTemplates`
  backend, stock-compatible adapter nodes for `template.nodelist`, and
  traceback-depth shims so error reporting matches stock Django.

## Development

```bash
python -m venv .venv && . .venv/bin/activate
pip install maturin pytest hypothesis "Django==6.1.*"
maturin develop            # debug build; use --release before benchmarking
pytest && cargo test
```

```
src/               Rust engine (lexer, parser, tags, filters, renderer, bridge)
python/            Python package: backend, adapter nodes, exceptions
tests/             pytest parity suite + Hypothesis fuzzer
scripts/           Django test-suite runner, runtests.py patches, known failures
benchmarks/        bench.py (engine vs stock Django), jinja2_bench.py, realworld.py
benches/           criterion micro-benchmarks
docker/            development image used by docker-compose.yml / Makefile
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

BSD-3-Clause, the same license as Django. See [LICENSE](LICENSE). Large parts
of this project are ports of Django's template engine; Django is © the Django
Software Foundation and individual contributors.
