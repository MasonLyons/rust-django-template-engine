"""Shared setup: every test renders the same input through stock Django and
through the Rust engine, and asserts identical output (or identical
exception type and message)."""

from pathlib import Path

import django
from django.conf import settings

TEMPLATES_DIR = Path(__file__).parent / "templates"

settings.configure(
    DEBUG=False,
    USE_I18N=False,
    USE_TZ=False,
    ALLOWED_HOSTS=["testserver"],
    TEMPLATES=[
        {
            "BACKEND": "django.template.backends.django.DjangoTemplates",
            "DIRS": [TEMPLATES_DIR],
            "OPTIONS": {},
        }
    ],
)
django.setup()

from django.template import Context  # noqa: E402
from django.template.engine import Engine as StockEngine  # noqa: E402

from django_rust_templates import Engine as RustEngine  # noqa: E402

IS_61 = django.VERSION[:2] >= (6, 1)

ENGINE_KW = dict(
    dirs=[str(TEMPLATES_DIR)],
    libraries={"testlib": "dtl_testlib"},
    string_if_invalid="",
    autoescape=True,
)

ENGINES = {"stock": StockEngine(**ENGINE_KW), "rust": RustEngine(**ENGINE_KW)}


def capture(fn):
    """Run ``fn`` and return a comparable outcome: ``("ok", value)`` or
    ``(exception type name, message, loader "tried" list)``."""
    try:
        return ("ok", fn())
    except Exception as e:  # noqa: BLE001
        tried = getattr(e, "tried", None)
        if tried is not None:
            try:
                tried = [(str(o), m) for o, m in tried]
            except Exception:  # noqa: BLE001
                tried = len(tried)
        return (type(e).__name__, str(e), tried)


def assert_parity(fn):
    """Assert ``fn(engine)`` behaves identically on both engines."""
    want = capture(lambda: fn(ENGINES["stock"]))
    got = capture(lambda: fn(ENGINES["rust"]))
    assert got == want


def assert_render_parity(src, ctx=None):
    assert_parity(lambda e: e.from_string(src).render(Context(ctx or {})))
