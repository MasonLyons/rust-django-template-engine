# django-rust-templates

[![CI](https://github.com/MasonLyons/rust-django-template-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/MasonLyons/rust-django-template-engine/actions/workflows/ci.yml)
[![License: BSD-3-Clause](https://img.shields.io/badge/license-BSD--3--Clause-blue.svg)](LICENSE)

A drop-in replacement for Django's template engine, written in Rust.

It re-implements the Django template language natively: the lexer, parser,
node tree, renderer, and the built-in tags and filters. It produces
**byte-identical output** to stock Django and passes **100% of Django's own
template test suite** on Django 5.2 and 6.1. On the benchmark below it renders
**~14× faster** and compiles templates **~14× faster** than stock Django.

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
> but the project hasn't had wide production use yet. Not on PyPI yet, so
> install from source.

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

You need a Rust toolchain ([rustup](https://rustup.rs); the pinned version in
`rust-toolchain.toml` is installed automatically) and Python ≥ 3.12.

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
  `partialdef`/`partial`, and filters such as `date`, `floatformat`,
  `urlize` and `join`.
- **Custom and third-party tag libraries** keep working. `simple_tag` and
  `inclusion_tag` run on native helper nodes. Other tags (custom compile
  functions, `simple_block_tag`) are bridged to their Python implementation,
  so their own code runs at Python speed while the rest of the template stays
  native.
- **The Python node API** (`template.nodelist`, `get_nodes_by_type`,
  `render_annotated`) is exposed through stock-compatible adapter nodes, so
  tooling that inspects compiled templates keeps working.

## Performance

Microseconds per operation; lower is better. **×** is the speedup over
stock Django.

| scenario | render stock | render rust | render × | compile stock | compile rust | compile × |
|---|---:|---:|---:|---:|---:|---:|
| simple_var | 14.4 µs | 3.2 µs | **4.55×** | 26.3 µs | 1.6 µs | **16.87×** |
| filter_chain | 81.3 µs | 6.6 µs | **12.33×** | 168.5 µs | 4.2 µs | **40.53×** |
| many_vars | 176.4 µs | 9.2 µs | **19.07×** | 592.8 µs | 34.2 µs | **17.31×** |
| loop | 5274.4 µs | 172.8 µs | **30.52×** | 109.2 µs | 5.9 µs | **18.44×** |
| if_branch | 2072.0 µs | 105.4 µs | **19.67×** | 130.7 µs | 7.9 µs | **16.60×** |
| attr_lookup | 2143.6 µs | 123.8 µs | **17.31×** | 187.3 µs | 8.1 µs | **22.99×** |
| autoescape | 848.6 µs | 44.6 µs | **19.01×** | 77.0 µs | 4.1 µs | **18.70×** |
| cycle_reset | 813.4 µs | 72.7 µs | **11.19×** | 137.6 µs | 6.3 µs | **21.83×** |
| firstof_widthratio | 36.1 µs | 3.8 µs | **9.40×** | 108.2 µs | 5.2 µs | **20.73×** |
| date_filter | 3431.6 µs | 144.2 µs | **23.79×** | 102.1 µs | 3.9 µs | **26.26×** |
| url_tag | 56.4 µs | 10.1 µs | **5.56×** | 46.8 µs | 2.7 µs | **17.20×** |
| include | 3552.4 µs | 360.5 µs | **9.85×** | 76.6 µs | 10.3 µs | **7.42×** |
| extends | 1871.8 µs | 47.9 µs | **39.04×** | 133.9 µs | 14.1 µs | **9.49×** |
| nested_table | 3941.3 µs | 128.0 µs | **30.79×** | 130.1 µs | 8.9 µs | **14.62×** |
| with_if_scopes | 109.4 µs | 13.2 µs | **8.31×** | 822.6 µs | 45.8 µs | **17.94×** |
| spaceless_table | 27.4 µs | 3.0 µs | **9.10×** | 14.2 µs | 1.7 µs | **8.26×** |
| bridged_simple_tag | 323.6 µs | 32.8 µs | **9.88×** | 83.4 µs | 16.5 µs | **5.05×** |
| cached_extends | 249.2 µs | 32.2 µs | **7.75×** | 1.6 µs | 0.4 µs | **3.68×** |
| value_types | 5125.7 µs | 504.0 µs | **10.17×** | 60.0 µs | 3.0 µs | **20.21×** |
| csrf_token_loop | 388.4 µs | 26.7 µs | **14.54×** | 49.9 µs | 3.3 µs | **15.35×** |
| inclusion_tag | 1249.3 µs | 304.4 µs | **4.10×** | 84.5 µs | 17.2 µs | **4.91×** |
| include_with | 4315.5 µs | 609.0 µs | **7.09×** | 118.7 µs | 14.8 µs | **8.02×** |
| index_none_lookup | 13787.5 µs | 202.4 µs | **68.11×** | 118.3 µs | 8.9 µs | **13.23×** |
| object_attrs | 3170.7 µs | 100.7 µs | **31.48×** | 89.0 µs | 6.8 µs | **13.12×** |
| object_methods | 1634.9 µs | 156.0 µs | **10.48×** | 77.7 µs | 4.9 µs | **15.99×** |
| object_properties | 3141.4 µs | 132.7 µs | **23.68×** | 112.1 µs | 8.0 µs | **13.95×** |
| realistic_page | 22233.2 µs | 1586.5 µs | **14.01×** | 1.7 µs | 0.5 µs | **3.65×** |
| floatformat | 14537.9 µs | 1002.8 µs | **14.50×** | 247.7 µs | 7.7 µs | **32.36×** |
| urlize | 12457.2 µs | 307.1 µs | **40.56×** | 145.2 µs | 4.8 µs | **30.27×** |
| join | 3801.9 µs | 283.5 µs | **13.41×** | 158.8 µs | 5.0 µs | **31.63×** |
| engine_e2e | 996.3 µs | 51.6 µs | **19.32×** | 79.8 µs | 9.9 µs | **8.07×** |
| **geomean** | | | **14.49×** | | | **14.06×** |

*Linux x86_64 (AMD EPYC 7601), Python 3.13.16, Django 6.1.2, release build.*

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
- The weakest rows are `inclusion_tag` (4.1×), `simple_var` (4.6×, dominated
  by fixed per-render overhead) and `url_tag` (5.6×, URL reversing is Django
  code).
- This measures template time only. A view's database queries and middleware
  are unaffected, so whole-request gains depend on how much of each request
  is spent rendering.

HTML escaping and the lexer's tag scanner use portable SIMD: `fearless_simd`
picks SSE2 on x86_64 and NEON on aarch64, and `memchr` picks the best
instruction set at runtime.

Run the benchmarks yourself:

```bash
maturin develop --release
python benchmarks/bench.py                   # full table
python benchmarks/bench.py --only realistic  # filter scenarios
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
benchmarks/        bench.py (engine vs stock Django)
benches/           criterion micro-benchmarks
docker/            development image used by docker-compose.yml / Makefile
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

BSD-3-Clause, the same license as Django. See [LICENSE](LICENSE). Large parts
of this project are ports of Django's template engine; Django is © the Django
Software Foundation and individual contributors.
