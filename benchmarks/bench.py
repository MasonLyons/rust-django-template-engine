"""Compile/render benchmark: stock Django vs the Rust engine (dtl).

Build the extension in release mode first, then run from the repo root:

    maturin develop --release
    python benchmarks/bench.py

or in Docker (release build, pinned to one core):

    docker compose run --rm bench

Some scenarios use the custom tag libraries from Django's own test suite;
a shallow clone of the matching Django branch is fetched into
``.django-cache/`` on first run (override with ``DTL_DJANGO_TESTS``).

Optional: ``--only <substr>`` (repeatable) filters scenarios; ``--json out``
dumps raw per-scenario results.

Each scenario is measured twice in separate subprocesses — one engine each —
so allocator layout / GC history from one engine can't bias the other's
numbers. Every scenario must produce byte-identical output on both engines
(verified in the parent before timing): a speedup on wrong output is
worthless. Render numbers are per-render microseconds (min of 7 timeit
repeats); compile numbers are per-parse microseconds.
"""

from __future__ import annotations

import argparse
import gc
import json
import os
import platform
import statistics
import subprocess
import sys
import timeit
from functools import partial
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REPEATS = 7
NUMBER_TARGET = 0.25  # seconds per timeit repeat


# --------------------------------------------------------------------------
# Scenario table
# --------------------------------------------------------------------------

_INCLUDE_BODY = '<div class="card">{% for tag in tags %}[{{ tag }}]{% endfor %}</div>'

PARENT_SRC = (
    "<html><head><title>{% block title %}{% endblock %}</title></head>\n"
    "<body>{% block content %}{% endblock %}</body></html>"
)

_SCENARIOS: dict[str, dict] = {}


def scenario(name, source, ctx=None, *, loader=None, e2e=False, cached=False):
    _SCENARIOS[name] = {
        "source": source,
        "ctx": ctx or (lambda: {}),
        "loader": loader or {},
        "e2e": e2e,
        "cached": cached,
    }


scenario("simple_var", "Hello {{ name }}!", lambda: {"name": "world"})

scenario(
    "filter_chain",
    "{{ text|default:'x'|upper|truncatechars:12 }}|{{ missing|default_if_none:'—' }}",
    lambda: {"text": "the quick brown fox jumps"},
)

MANY_VARS = " ".join("{{ v%02d }}" % i for i in range(40))

scenario(
    "many_vars",
    MANY_VARS,
    lambda: {("v%02d" % i): f"value{i}" for i in range(40)},
)

scenario(
    "loop",
    "{% for n in nums %}<li>#{{ forloop.counter }} {{ n|capfirst }}</li>{% endfor %}",
    lambda: {"nums": list(range(200))},
)

scenario(
    "if_branch",
    "{% for n in nums %}{% if n > 90 %}hi{% elif n > 50 %}mid{% else %}lo{% endif %}{% endfor %}",
    lambda: {"nums": list(range(500))},
)

scenario(
    "attr_lookup",
    "{% for p in people %}{{ p.name|upper }}: "
    "{% if p.is_staff %}admin{% else %}user{% endif %};"
    "{{ p.dept.title|default:'none' }} {% endfor %}",
    lambda: {
        "people": [
            {
                "name": f"person-{i}",
                "is_staff": i % 3 == 0,
                "dept": {"title": f"dept{i % 4}"} if i % 5 else None,
            }
            for i in range(100)
        ]
    },
)

scenario(
    "autoescape",
    "{% for r in rows %}<p>{{ raw }} | {{ safe }}</p>{% endfor %}",
    lambda: {
        "rows": list(range(100)),
        "raw": "<b>x&y</b>",
        "safe": __import__(
            "django.utils.safestring", fromlist=["mark_safe"]
        ).mark_safe("<i>s&i</i>"),
    },
)

scenario(
    "cycle_reset",
    "{% for n in nums %}{% cycle 'a' 'b' 'c' %}{% cycle 'x' 'y' %} {% endfor %}",
    lambda: {"nums": list(range(200))},
)

scenario(
    "firstof_widthratio",
    "{% firstof missing also_missing present %} {% widthratio a b c %}",
    lambda: {"present": "P", "a": 3, "b": 7, "c": 100},
)

scenario(
    "date_filter",
    "{% for d in dates %}{{ d|date:'D m, Y, fA' }} {% endfor %}",
    lambda: {
        "dates": [
            __import__("datetime", fromlist=["datetime"]).datetime(
                2026, 10, 14, 13, 37, i % 60
            )
            for i in range(50)
        ]
    },
)

scenario(
    "url_tag",
    "{% url 'named.client' 42 %}",
)

scenario(
    "include",
    "{% for i in items %}{% include '_bench_inc.html' %}{% endfor %}",
    lambda: {"items": list(range(100)), "tags": ["t1", "t2"]},
    loader={"_bench_inc.html": _INCLUDE_BODY},
)

scenario(
    "extends",
    '{% extends "_bench_parent.html" %}\n'
    "{% block title %}List{% endblock %}\n"
    "{% block content %}{% for n in nums %}<b>{{ n }}</b>{% endfor %}{% endblock %}",
    lambda: {"nums": list(range(100))},
    loader={"_bench_parent.html": PARENT_SRC},
)

scenario(
    "nested_table",
    "{% for row in rows %}<tr>{% for cell in row %}"
    "{% if cell %}{{ cell }}{% else %}-{% endif %},{% endfor %}</tr>{% endfor %}",
    lambda: {"rows": [[i, i * 2, "", i * 4] for i in range(50)]},
)

_WITH_CHAIN = (
    "{% with v0=n %}"
    + "".join(
        "{% with v" + str(i) + "=v" + str(i - 1) + "|add:1 %}" for i in range(1, 12)
    )
    + "{% if v11 > 10 %}{{ v11 }}{% else %}lo{% endif %}"
    + "{% endwith %}" * 12
)

scenario(
    "with_if_scopes",
    _WITH_CHAIN,
    lambda: {"n": 5},
)

scenario(
    "spaceless_table",
    "{% spaceless %}\n"
    "  <table>\n    <tr>\n"
    + "".join("      <td>%d</td>\n" % i for i in range(40))
    + "    </tr>\n  </table>\n{% endspaceless %}",
)

scenario(
    "bridged_simple_tag",
    "{% load custom %}{% for i in items %}{% one_param i %},{% endfor %}",
    lambda: {"items": list(range(50))},
)

# --- Paths outside the original 18 (cached loader, non-str values, bridged
# --- tags, include-with, index/None lookups). Added so the metric sees them.

_CACHED_BASE = (
    "<html><head><title>{% block title %}Site{% endblock %}</title></head>\n"
    "<body><nav>{% block nav %}<a>home</a>{% endblock %}</nav>\n"
    "<main>{% block content %}{% endblock %}</main>\n"
    "<footer>{% block footer %}(c) 2026{% endblock %}</footer></body></html>"
)
_CACHED_MID = (
    '{% extends "_bench_base.html" %}\n'
    "{% block title %}Section | {{ block.super }}{% endblock %}\n"
    "{% block nav %}{{ block.super }}<a>section</a>{% endblock %}\n"
    "{% block footer %}<p>section footer</p>{{ block.super }}{% endblock %}"
)

# Compile column for this row is a cache-hit get_template(), not a parse:
# that's the cost a cached-loader deployment actually pays per request.
scenario(
    "cached_extends",
    '{% extends "_bench_mid.html" %}\n'
    "{% block title %}{{ title }} | {{ block.super }}{% endblock %}\n"
    "{% block content %}{% for n in nums %}<b>{{ n }}</b>{% endfor %}{% endblock %}\n"
    "{% block footer %}{{ block.super }}<p>child</p>{% endblock %}",
    lambda: {"title": "List", "nums": list(range(5))},
    loader={"_bench_base.html": _CACHED_BASE, "_bench_mid.html": _CACHED_MID},
    cached=True,
)


def _value_types_ctx():
    import datetime
    import decimal

    from django.utils.translation import gettext_lazy

    class Obj:
        def __str__(self):
            return "obj-<str>"

    one = [
        datetime.date(2026, 1, 2),
        datetime.datetime(2026, 1, 2, 3, 4, 5),
        decimal.Decimal("19.99"),
        3.25,
        True,
        None,
        gettext_lazy("Home"),
        Obj(),
    ]
    return {"vals": one * 25}


scenario(
    "value_types",
    "{% for v in vals %}{{ v }};{% endfor %}",
    _value_types_ctx,
)

scenario(
    "csrf_token_loop",
    "<form>{% for i in items %}{% csrf_token %}{% endfor %}</form>",
    lambda: {"items": list(range(50)), "csrf_token": "tok3n-abc123"},
)

scenario(
    "inclusion_tag",
    "{% load inclusion %}{% for i in items %}{% inclusion_one_param i %}{% endfor %}",
    lambda: {"items": list(range(50))},
    loader={"inclusion.html": "{{ result }}\n"},
)

scenario(
    "include_with",
    "{% for p in people %}{% include '_bench_card.html' with name=p.name n=forloop.counter %}{% endfor %}",
    lambda: {"people": [{"name": f"person-{i}"} for i in range(100)]},
    loader={"_bench_card.html": "<li>{{ n }}: {{ name }}</li>"},
)

scenario(
    "index_none_lookup",
    "{% for r in rows %}{{ r.0 }}-{{ r.2 }}|{{ none.attr }}{{ none.attr.deeper }}"
    "{{ r.1.0 }};{% endfor %}",
    lambda: {"rows": [[i, [i * 2], "c%d" % i] for i in range(100)], "none": None},
)


# --- Model-like objects (attributes, methods, properties) and a realistic
# --- mixed page: real views pass objects, not dicts, and apply date /
# --- floatformat / urlize to non-str values.


class _BenchDept:
    def __init__(self, i):
        self.title = f"dept{i}"
        self.code = i

    def __str__(self):
        return self.title


class _BenchPerson:
    def __init__(self, i):
        import datetime
        import decimal

        self.pk = i
        self.name = f"person-{i}"
        self.email = f"p{i}@example.com"
        self.dept = _BenchDept(i % 4)
        self.joined = datetime.date(2020, 1, 1 + i % 28)
        self.salary = decimal.Decimal("1234.50") + i
        self._staff = i % 3 == 0
        self.tags = ["a", "b", "c"][: i % 4]

    @property
    def is_staff(self):
        return self._staff

    def get_absolute_url(self):
        return f"/people/{self.pk}/"

    def full(self):
        return self.name.title()


def _people_ctx():
    return {"people": [_BenchPerson(i) for i in range(100)]}


scenario(
    "object_attrs",
    "{% for p in people %}{{ p.name }} {{ p.dept.title }} {{ p.pk }};{% endfor %}",
    _people_ctx,
)

scenario(
    "object_methods",
    '{% for p in people %}<a href="{{ p.get_absolute_url }}">{{ p.full }}</a>{% endfor %}',
    _people_ctx,
)

scenario(
    "object_properties",
    "{% for p in people %}{% if p.is_staff %}staff{% else %}-{% endif %}"
    "{{ p.is_staff }} {{ p.missing }}{% endfor %}",
    _people_ctx,
)

_PAGE_BASE = (
    "<!doctype html><html><head><title>{% block title %}Site{% endblock %}</title></head>\n"
    '<body>{% include "_page_nav.html" %}\n'
    "<main>{% block content %}{% endblock %}</main>\n"
    "<footer>{% block footer %}&copy; {{ year }}{% endblock %}</footer></body></html>"
)
_PAGE_NAV = (
    '<nav>{% for item in menu %}<a href="{{ item.url }}"{% if item.active %} class="on"{% endif %}>'
    "{{ item.label }}</a>{% endfor %}{% if user.is_authenticated %}Hi {{ user.name }}"
    '{% else %}<a href="/login">login</a>{% endif %}</nav>'
)
_PAGE_ROW = (
    "<tr class=\"{% cycle 'odd' 'even' %}\"><td><a href=\"{{ p.get_absolute_url }}\">{{ p.full }}</a></td>"
    "<td>{{ p.email|urlize }}</td><td>{{ p.dept }}</td><td>{{ p.joined|date:\"Y-m-d\" }}</td>"
    "<td>{{ p.salary|floatformat:2 }}</td><td>{% if p.is_staff %}staff{% endif %}</td>"
    '<td>{{ p.tags|join:", "|default:"-" }}</td></tr>'
)


def _page_ctx():
    return {
        **_people_ctx(),
        "menu": [{"url": f"/s{i}/", "label": f"Section {i}", "active": i == 2} for i in range(6)],
        "user": {"is_authenticated": True, "name": "mason"},
        "year": 2026,
        "heading": "people directory",
        "intro": "line one\nline <two>",
        "total": 12345.678,
    }


# Cached loader + extends + include-per-row over model-like objects.
scenario(
    "realistic_page",
    '{% extends "_page_base.html" %}{% block title %}People | {{ block.super }}{% endblock %}\n'
    "{% block content %}<h1>{{ heading|title }}</h1><p>{{ intro|linebreaksbr }}</p>\n"
    '<table>{% for p in people %}{% include "_page_row.html" %}'
    "{% empty %}<tr><td>none</td></tr>{% endfor %}</table>\n"
    "<p>{{ people|length }} people, {{ total|floatformat }} total</p>{% endblock %}",
    _page_ctx,
    loader={"_page_base.html": _PAGE_BASE, "_page_nav.html": _PAGE_NAV, "_page_row.html": _PAGE_ROW},
    cached=True,
)


def _floatformat_ctx():
    import decimal

    vals = [decimal.Decimal("1234.50"), 3.14159, 1234567, decimal.Decimal("-0.005"), 2.5e-7]
    return {"vals": vals * 20}


scenario(
    "floatformat",
    "{% for v in vals %}{{ v|floatformat:2 }}|{{ v|floatformat }}|{{ v|floatformat:'-3' }}"
    "|{{ v|floatformat:'2g' }};{% endfor %}",
    _floatformat_ctx,
)

scenario(
    "urlize",
    "{% for t in texts %}<p>{{ t|urlize }}</p>{{ t|urlizetrunc:15 }}{% endfor %}",
    lambda: {
        "texts": [
            f"Mail p{i}@example.com or see https://example.com/docs/{i}/ (also"
            f" http://x.org/a-b_c.html), <b>bold</b> & more text here."
            for i in range(30)
        ]
    },
)

scenario(
    "join",
    "{% for r in rows %}{{ r|join:', ' }};{{ r|join:'<br>' }}{% endfor %}",
    lambda: {"rows": [["a", "<b>", "c&d", i, "plain text"] for i in range(100)]},
)


# --- Real-page paths outside the plain-dict scenarios above: i18n tags and
# --- lazy translations (labels, verbose names), aware datetimes, and the
# --- striptags / humanize filters.


def _i18n_ctx():
    from django.utils.translation import gettext_lazy

    labels = [gettext_lazy(w) for w in ("Yes", "No", "Home", "Log out", "Save")]
    return {"rows": [{"label": labels[i % 5], "n": i} for i in range(100)]}


scenario(
    "i18n_tags",
    "{% load i18n %}<nav>{% translate 'Home' %} {% translate 'Log out' as lo %}{{ lo }}</nav>"
    "{% for r in rows %}<td>{{ r.label }}</td><td>{% translate 'Save' %}</td>{% endfor %}",
    _i18n_ctx,
)


def _aware_ctx():
    import datetime

    base = datetime.datetime(2026, 3, 1, 12, 30, tzinfo=datetime.timezone.utc)
    return {"dts": [base - datetime.timedelta(hours=7 * i) for i in range(100)]}


scenario(
    "aware_dates",
    "{% for d in dts %}"
    '<time datetime="{{ d|date:\'c\' }}">{{ d|date:"N j, Y" }} {{ d|time:"H:i" }}</time>'
    "{{ d|date }}{% endfor %}",
    _aware_ctx,
)

# --- Bridged block tags (custom compile functions calling parser.parse())
# --- around native content: the body compiles on the Rust parser.

scenario(
    "localtime_block",
    "{% load tz %}{% localtime on %}{% for d in dts %}"
    '<time datetime="{{ d|date:\'c\' }}">{{ d|date:"N j, Y" }} {{ d|time:"H:i" }}</time>'
    "{{ d|date }}{% endfor %}{% endlocaltime %}",
    _aware_ctx,
)

scenario(
    "simple_block_tag",
    "{% load custom %}{% div id='people' %}{% for p in people %}{{ p.name|upper }}: "
    "{% if p.is_staff %}admin{% else %}user{% endif %};"
    "{{ p.dept.title|default:'none' }} {% endfor %}{% enddiv %}",
    lambda: {
        "people": [
            {
                "name": f"person-{i}",
                "is_staff": i % 3 == 0,
                "dept": {"title": f"dept{i % 4}"} if i % 5 else None,
            }
            for i in range(100)
        ]
    },
)

scenario(
    "striptags_intcomma",
    "{% load humanize %}{% for a in arts %}<p>{{ a.body|striptags|truncatewords:12 }}</p>"
    "<span>{{ a.views|intcomma }} views</span>{% endfor %}",
    lambda: {
        "arts": [
            {
                "body": f"<p>Lorem <b>ipsum</b> dolor sit amet, <a href=\"/x/{i}\">consectetur</a>"
                " adipiscing elit &amp; sed do eiusmod tempor.</p>",
                "views": i * 12345,
            }
            for i in range(50)
        ]
    },
)


scenario(
    "engine_e2e",
    "Hi {{ name }}! {% for n in nums %}{{ n }}{% endfor %}",
    lambda: {"name": "x", "nums": list(range(50))},
    e2e=True,
)


# --------------------------------------------------------------------------
# Bootstrap (parent and child)
# --------------------------------------------------------------------------


def django_tests_dir() -> str:
    """Django's ``tests/`` dir for the installed version (cloned if needed)."""
    if "DTL_DJANGO_TESTS" in os.environ:
        return os.environ["DTL_DJANGO_TESTS"]
    import django

    ver = "%d.%d" % django.VERSION[:2]
    clone = ROOT / ".django-cache" / ver
    if not (clone / "tests").is_dir():
        sys.path.insert(0, str(ROOT / "scripts"))
        from run_django_test_suite import ensure_clone

        ensure_clone(clone, f"stable/{ver}.x")
    return str(clone / "tests")


def bootstrap():
    tests_dir = django_tests_dir()
    os.chdir(tests_dir)
    if tests_dir not in sys.path:
        sys.path.insert(0, tests_dir)
    from django.conf import settings

    settings.configure(
        DEBUG=False,
        ALLOWED_HOSTS=["*"],
        USE_TZ=True,  # Django's default since 5.0
        ROOT_URLCONF="template_tests.urls",
        INSTALLED_APPS=["template_tests"],
        DATABASES={},
        TEMPLATES=[],
    )
    import django

    django.setup()


def build_engine(variant, loader=None, cached=False):
    from django.template.engine import Engine as StockEngine
    import django_rust_templates

    loaders = [("django.template.loaders.locmem.Loader", dict(loader or {}))]
    if cached:
        loaders = [("django.template.loaders.cached.Loader", loaders)]
    libraries = {
        "custom": "template_tests.templatetags.custom",
        "inclusion": "template_tests.templatetags.inclusion",
        "i18n": "django.templatetags.i18n",
        "tz": "django.templatetags.tz",
        "humanize": "django.contrib.humanize.templatetags.humanize",
    }
    if variant == "stock":
        return StockEngine(loaders=loaders, libraries=libraries)
    return django_rust_templates.Engine(loaders=loaders, libraries=libraries)


def prep(sdef):
    """Loader dict with the main template injected (if the scenario uses a
    loader at all)."""
    if not sdef["loader"]:
        return None
    return dict(sdef["loader"], **{"__bench_main.html": sdef["source"]})


def make_fns(variant, sdef):
    """Return (compile_fn, render_fn) for one engine. The render function
    builds a fresh `Context` per call: stateful tags (`cycle`, `ifchanged`)
    keep per-render state in `render_context`, so reusing one context across
    repeats would change output. `Context(...)` construction is negligible
    and identical on both sides."""
    from django.template import Context

    loader = prep(sdef)
    if sdef["e2e"]:
        main = dict(sdef["loader"], **{"__bench_main.html": sdef["source"]})
        eng = build_engine(variant, main, sdef["cached"])
        render = partial(eng.render_to_string, "__bench_main.html", sdef["ctx"]())
        return partial(eng.get_template, "__bench_main.html"), render
    eng = build_engine(variant, loader, sdef["cached"])
    ctx = sdef["ctx"]()
    if loader is not None:
        tmpl = eng.get_template("__bench_main.html")
        render = partial(lambda t, c: t.render(Context(c)), tmpl, ctx)
        return partial(eng.get_template, "__bench_main.html"), render
    tmpl = eng.from_string(sdef["source"])
    render = partial(lambda t, c: t.render(Context(c)), tmpl, ctx)
    return partial(eng.from_string, sdef["source"]), render


# --------------------------------------------------------------------------
# Measurement
# --------------------------------------------------------------------------


def time_min(fn):
    gc_was = gc.isenabled()
    gc.disable()
    try:
        for _ in range(3):
            fn()
        t = timeit.Timer(fn)
        one = max(t.timeit(number=1), 1e-6)
        n = max(1, int(NUMBER_TARGET / one))
        best = min(t.repeat(repeat=REPEATS, number=n)) / n
    finally:
        if gc_was:
            gc.enable()
    return best


def measure(variant, only=None):
    """Child entry point: one JSON line per scenario."""
    out = []
    for name, sdef in _SCENARIOS.items():
        if only and not any(o in name for o in only):
            continue
        compile_fn, render_fn = make_fns(variant, sdef)
        out.append(
            {
                "scenario": name,
                "variant": variant,
                "render_us": time_min(render_fn) * 1e6,
                "compile_us": time_min(compile_fn) * 1e6,
            }
        )
    return out


# --------------------------------------------------------------------------
# Parent orchestration
# --------------------------------------------------------------------------


def verify_outputs(only=None):
    bad = []
    for name, sdef in _SCENARIOS.items():
        if only and not any(o in name for o in only):
            continue
        _, render_stock = make_fns("stock", sdef)
        _, render_dtl = make_fns("dtl", sdef)
        a, b = str(render_stock()), str(render_dtl())
        if a != b:
            bad.append(f"{name}:\n stock={a[:120]!r}\n  dtl={b[:120]!r}")
    if bad:
        raise SystemExit("output mismatch:\n" + "\n".join(bad))


def child(variant, only):
    cmd = [sys.executable, os.path.abspath(__file__), "--child", variant]
    for o in only or []:
        cmd += ["--only", o]
    proc = subprocess.run(cmd, capture_output=True, text=True, cwd=ROOT)
    if proc.returncode != 0:
        sys.stderr.write(proc.stdout + proc.stderr)
        raise SystemExit(f"child {variant!r} failed rc={proc.returncode}")
    return [
        json.loads(line)
        for line in proc.stdout.splitlines()
        if line.startswith("{")
    ]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--child")
    ap.add_argument("--only", action="append")
    ap.add_argument("--json")
    ap.add_argument("--skip-verify", action="store_true")
    args = ap.parse_args()
    if args.json:
        args.json = os.path.abspath(args.json)

    bootstrap()

    if args.child:
        try:
            os.sched_setaffinity(0, {int(os.environ.get("DTL_BENCH_CORE", "0"))})
        except (AttributeError, OSError):
            pass  # not Linux: run unpinned
        for rec in measure(args.child, args.only):
            print(json.dumps(rec))
        return

    if not args.skip_verify:
        verify_outputs(args.only)
        print(
            "output parity: OK (both engines byte-identical on every scenario)",
            file=sys.stderr,
        )

    rows = {}
    for variant in ("stock", "dtl"):
        for rec in child(variant, args.only):
            rows.setdefault(rec["scenario"], {})[variant] = rec

    hdr = (
        f"{'scenario':<18} {'render stock':>13} {'render dtl':>13} {'speedup':>8}"
        f"   {'compile stock':>14} {'compile dtl':>13} {'speedup':>8}"
    )
    print(hdr)
    print("-" * len(hdr))
    ratios_r, ratios_c = [], []
    for name, by_variant in rows.items():
        s, d = by_variant["stock"], by_variant["dtl"]
        r = s["render_us"] / d["render_us"]
        c = s["compile_us"] / d["compile_us"]
        ratios_r.append(r)
        ratios_c.append(c)
        print(
            f"{name:<18} {s['render_us']:>10.1f} us {d['render_us']:>10.1f} us {r:>7.2f}x"
            f"   {s['compile_us']:>11.1f} us {d['compile_us']:>10.1f} us {c:>7.2f}x"
        )

    print()
    rg = statistics.geometric_mean(ratios_r)
    cg = statistics.geometric_mean(ratios_c)
    print(f"geomean speedup: render {rg:.2f}x   compile {cg:.2f}x")
    print(f"METRIC geomean={statistics.geometric_mean(ratios_r + ratios_c):.6f}")
    print(f"METRIC render_geomean={rg:.6f}")
    print(f"METRIC compile_geomean={cg:.6f}")
    print(f"METRIC worst_render={min(ratios_r):.6f}")
    print(f"METRIC worst_compile={min(ratios_c):.6f}")
    import django as _dj

    print(
        f"(Python {sys.version.split()[0]}, Django {_dj.get_version()}, "
        f"{platform.machine()}, min of {REPEATS} timeit repeats)"
    )

    if args.json:
        with open(args.json, "w") as fp:
            json.dump(rows, fp, indent=2)


if __name__ == "__main__":
    main()
