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
  `{% static %}`, and filters such as `date`, `floatformat`, `urlize`, `join`
  and `striptags`, plus humanize's `intcomma` on integers and `naturaltime`.
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
| simple_var | 12.1 µs | 2.7 µs | **4.54×** | 23.1 µs | 1.8 µs | **12.63×** |
| filter_chain | 71.2 µs | 5.9 µs | **12.15×** | 148.3 µs | 4.0 µs | **37.05×** |
| many_vars | 141.5 µs | 7.8 µs | **18.23×** | 511.1 µs | 31.2 µs | **16.40×** |
| loop | 4704.7 µs | 134.5 µs | **34.97×** | 96.8 µs | 5.3 µs | **18.39×** |
| if_branch | 1655.6 µs | 52.9 µs | **31.28×** | 113.4 µs | 7.2 µs | **15.69×** |
| attr_lookup | 1808.8 µs | 106.1 µs | **17.05×** | 166.5 µs | 9.1 µs | **18.29×** |
| autoescape | 701.2 µs | 44.1 µs | **15.90×** | 66.8 µs | 4.3 µs | **15.62×** |
| cycle_reset | 698.5 µs | 69.2 µs | **10.10×** | 117.2 µs | 6.8 µs | **17.12×** |
| firstof_widthratio | 30.0 µs | 3.1 µs | **9.71×** | 93.7 µs | 5.7 µs | **16.39×** |
| date_filter | 2972.9 µs | 90.4 µs | **32.87×** | 89.9 µs | 4.0 µs | **22.74×** |
| url_tag | 50.4 µs | 6.0 µs | **8.39×** | 40.0 µs | 2.5 µs | **16.10×** |
| include | 3121.2 µs | 322.1 µs | **9.69×** | 65.4 µs | 9.6 µs | **6.80×** |
| extends | 1626.4 µs | 42.9 µs | **37.89×** | 112.8 µs | 12.5 µs | **8.99×** |
| nested_table | 3542.3 µs | 112.4 µs | **31.53×** | 113.6 µs | 8.2 µs | **13.91×** |
| with_if_scopes | 89.7 µs | 11.5 µs | **7.81×** | 712.6 µs | 38.8 µs | **18.36×** |
| spaceless_table | 22.9 µs | 2.3 µs | **9.74×** | 12.0 µs | 1.7 µs | **7.03×** |
| bridged_simple_tag | 258.7 µs | 27.2 µs | **9.50×** | 72.7 µs | 17.1 µs | **4.25×** |
| cached_extends | 216.8 µs | 28.4 µs | **7.62×** | 1.2 µs | 0.3 µs | **3.56×** |
| value_types | 4478.1 µs | 274.1 µs | **16.34×** | 51.9 µs | 3.0 µs | **17.23×** |
| csrf_token_loop | 309.1 µs | 15.4 µs | **20.09×** | 43.0 µs | 3.2 µs | **13.52×** |
| inclusion_tag | 1101.3 µs | 160.4 µs | **6.87×** | 72.9 µs | 15.2 µs | **4.80×** |
| include_with | 3798.3 µs | 436.9 µs | **8.69×** | 101.7 µs | 13.8 µs | **7.37×** |
| index_none_lookup | 11495.9 µs | 195.5 µs | **58.82×** | 100.8 µs | 8.2 µs | **12.30×** |
| object_attrs | 2762.9 µs | 83.2 µs | **33.20×** | 78.3 µs | 6.3 µs | **12.43×** |
| object_methods | 1311.7 µs | 126.9 µs | **10.34×** | 65.8 µs | 5.3 µs | **12.43×** |
| object_properties | 2664.3 µs | 116.5 µs | **22.87×** | 96.8 µs | 7.5 µs | **12.91×** |
| realistic_page | 19429.3 µs | 1500.5 µs | **12.95×** | 1.2 µs | 0.3 µs | **3.54×** |
| floatformat | 12700.7 µs | 928.3 µs | **13.68×** | 222.7 µs | 8.4 µs | **26.52×** |
| urlize | 11147.1 µs | 307.5 µs | **36.25×** | 127.7 µs | 5.1 µs | **24.97×** |
| join | 3024.7 µs | 255.1 µs | **11.86×** | 139.6 µs | 5.2 µs | **26.73×** |
| i18n_tags | 3214.0 µs | 341.3 µs | **9.42×** | 166.7 µs | 14.6 µs | **11.44×** |
| aware_dates | 19388.1 µs | 1502.2 µs | **12.91×** | 226.6 µs | 8.2 µs | **27.67×** |
| localtime_block | 19006.7 µs | 1505.1 µs | **12.63×** | 243.8 µs | 50.4 µs | **4.84×** |
| simple_block_tag | 1820.6 µs | 118.0 µs | **15.43×** | 212.1 µs | 100.6 µs | **2.11×** |
| striptags_intcomma | 5082.3 µs | 617.9 µs | **8.22×** | 151.1 µs | 12.4 µs | **12.21×** |
| engine_e2e | 869.3 µs | 47.1 µs | **18.44×** | 70.4 µs | 8.4 µs | **8.42×** |
| **geomean** | | | **14.78×** | | | **11.85×** |

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
- The weakest rows are `simple_var` (4.5×, dominated by fixed per-render
  overhead) and `inclusion_tag` (6.9×: the tag function is Python, and each
  call builds a fresh `Context` for the included template).
- This measures template time only. A view's database queries and middleware
  are unaffected, so whole-request gains depend on how much of each request
  is spent rendering.
- [`benchmarks/realworld.py`](benchmarks/realworld.py) renders a full page
  through the backend with a request: context processors, `{% static %}`,
  `{% url %}`, i18n tags, messages, a form and humanize filters. Python code
  that the page calls (context processors, form and widget classes,
  translation catalog lookups, bridged tags) runs at the same speed on both
  engines, so this page is about 3.5× faster rather than 14×.

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
| simple_var | 12.0 µs | 14.3 µs | 2.7 µs | **5.33×** |
| filter_chain | 71.1 µs | 55.6 µs | 6.1 µs | **9.15×** |
| many_vars | 144.3 µs | 42.3 µs | 7.8 µs | **5.39×** |
| loop | 4653.7 µs | 570.3 µs | 133.2 µs | **4.28×** |
| if_branch | 1701.7 µs | 52.0 µs | 53.2 µs | **0.98×** |
| attr_lookup | 1833.3 µs | 480.2 µs | 105.2 µs | **4.56×** |
| autoescape | 700.0 µs | 195.8 µs | 44.0 µs | **4.45×** |
| cycle_reset | 711.3 µs | 1087.3 µs | 69.1 µs | **15.73×** |
| firstof_widthratio | 29.6 µs | 18.8 µs | 3.1 µs | **6.06×** |
| date_filter | 3002.0 µs | 2410.8 µs | 90.2 µs | **26.73×** |
| url_tag | 49.5 µs | 52.6 µs | 6.2 µs | **8.49×** |
| include | 3004.0 µs | 868.7 µs | 292.5 µs | **2.97×** |
| extends | 1584.5 µs | 95.7 µs | 29.9 µs | **3.20×** |
| nested_table | 3510.4 µs | 129.2 µs | 113.9 µs | **1.13×** |
| with_if_scopes | 89.6 µs | 14.4 µs | 11.5 µs | **1.25×** |
| bridged_simple_tag | 262.9 µs | 121.0 µs | 26.6 µs | **4.54×** |
| cached_extends | 216.4 µs | 56.2 µs | 29.4 µs | **1.91×** |
| csrf_token_loop | 305.6 µs | 53.7 µs | 15.6 µs | **3.44×** |
| include_with | 3710.0 µs | 1154.2 µs | 409.9 µs | **2.82×** |
| index_none_lookup | 11651.0 µs | 228.6 µs | 182.8 µs | **1.25×** |
| object_attrs | 2772.3 µs | 229.0 µs | 84.3 µs | **2.72×** |
| object_methods | 1312.9 µs | 501.0 µs | 130.2 µs | **3.85×** |
| object_properties | 2654.3 µs | 356.5 µs | 115.0 µs | **3.10×** |
| realistic_page | 19718.0 µs | 14614.8 µs | 1545.2 µs | **9.46×** |
| floatformat | 12645.9 µs | 10625.2 µs | 915.1 µs | **11.61×** |
| urlize | 11024.5 µs | 10533.8 µs | 301.8 µs | **34.90×** |
| join | 3008.8 µs | 1017.1 µs | 244.0 µs | **4.17×** |
| i18n_tags | 3215.7 µs | 2570.9 µs | 347.5 µs | **7.40×** |
| aware_dates | 19147.8 µs | 15566.0 µs | 1548.9 µs | **10.05×** |
| striptags_intcomma | 5154.6 µs | 4344.5 µs | 618.3 µs | **7.03×** |
| engine_e2e | 784.8 µs | 45.8 µs | 18.0 µs | **2.54×** |
| **geomean** | | | | **4.64×** |

*Same machine as above; Jinja2 3.1.6. Over the same rows, Jinja2 renders
3.45× faster than stock Django and this engine renders 16.00× faster.*

- **Compile.** Jinja2 compiles every template to Python bytecode. A fresh
  parse takes about 9× longer than stock Django and **153×** longer than
  this engine (geomean). A cache-hit `get_template()` is 2.6× faster here
  than in Jinja2.
- **Where Jinja2 keeps up.** `if_branch` (0.98×) is a loop of integer
  comparisons. Jinja2 turns `n > 90` into a Python `>` on the raw value,
  which is as fast as the native comparison here. Rows made of plain loops
  and lookups (`nested_table`, `index_none_lookup`, `with_if_scopes`) are
  also close to even.
- **Where it doesn't.** Jinja2 has no `date`, `floatformat`, `urlize`,
  `intcomma` or Django-style `truncatewords`. Django projects register
  Django's own filter functions in Jinja2 (as django-jinja does), and the
  benchmark does too, so those rows run Python code at stock speed.
  Here they are native, which is why `realistic_page`, `urlize`, `date_filter`
  and `aware_dates` are 9–35× faster than Jinja2.
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
