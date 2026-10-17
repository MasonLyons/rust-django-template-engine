# Contributing

Thanks for your interest! Bug reports, test cases and pull requests are all
welcome.

## Reporting a difference from Django

The engine's contract is byte-identical behaviour to stock Django, so any
difference counts as a bug. The most useful report is a minimal template and
context that render differently, along with your Python and Django versions:

```python
import django
from django.conf import settings

settings.configure()
django.setup()

from django.template import Context
from django.template.engine import Engine as StockEngine
from django_rust_templates import Engine as RustEngine

src, ctx = "{{ x|floatformat:2 }}", {"x": 1.005}
print(StockEngine().from_string(src).render(Context(ctx)))
print(RustEngine().from_string(src).render(Context(ctx)))
```

If the fuzzer finds a mismatch, the failing example Hypothesis prints is a
good report on its own. Rerun it with `DTL_FUZZ_SEED=<seed>`.

## Development setup

```bash
python -m venv .venv && . .venv/bin/activate
pip install maturin pytest hypothesis "Django==6.1.*"
maturin develop
```

Rerun `maturin develop` after every Rust change. Use `--release` before
measuring performance.

## Before opening a pull request

```bash
pytest                                                    # parity suite + fuzzer
cargo test --release                                      # Rust unit tests
python scripts/run_django_test_suite.py --branch stable/6.1.x
python scripts/run_django_test_suite.py --branch stable/5.2.x
```

- **Correctness first.** Every behavioural change needs a parity test in
  `tests/` that compares against stock Django; `tests/parity.py` has the
  helpers. Don't add entries to `scripts/known_failures-*.txt` to get a
  change through.
- **Performance changes need numbers.** Include `benchmarks/bench.py` output
  from before and after, ideally with `--only` for the affected scenarios. If
  the change targets a path the benchmark doesn't cover, add a scenario.
- **Style.** Match the surrounding code. Comments explain *why* and point at
  the stock Django behaviour being mirrored.

## Supporting a new Django release

1. Add the branch to `PATCH_NAMES`/`FAILURE_NAMES` in
   `scripts/run_django_test_suite.py`, along with a matching
   `django_tests_use_dtl-<version>.patch` (usually the previous patch, rebased
   onto the new `tests/runtests.py`).
2. Run the suite, fix the differences, and add the new version to the CI
   matrix.
