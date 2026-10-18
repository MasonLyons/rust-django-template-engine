"""Compile/render benchmark: stock Django vs Jinja2 vs the Rust engine (dtl).

Jinja2 is the other template engine Django ships a backend for, so this runs
the ``bench.py`` scenarios through it as well. Jinja2 can't parse DTL, so
every scenario below has a hand-written Jinja2 template that does the same
work in idiomatic Jinja2 (``loop.cycle``, ``super()``, ``p.full()``,
``{% with %}`` around ``{% include %}``, ...):

    maturin develop --release
    pip install jinja2
    python benchmarks/jinja2_bench.py

Rules:

- Contexts, timing and process isolation come from ``bench.py``: each engine
  runs in its own pinned subprocess, min of 7 ``timeit`` repeats.
- The Jinja2 environment is set up the way Django projects usually set it up
  (as django-jinja does): ``autoescape=True``, ``ChainableUndefined`` (missing
  lookups render as empty, like DTL), ``auto_reload=False``, the i18n
  extension wired to Django's gettext, and Django's own filter functions
  registered where Jinja2 has no equivalent with the same output (``date``,
  ``floatformat``, ``urlize``, ``intcomma``, ...). Those filters run the same
  Python code on Jinja2 as on stock Django.
- Before timing, every Jinja2 render must match stock Django byte for byte
  after one normalization: markupsafe writes ``&#39;``/``&#34;`` where Django
  writes ``&#x27;``/``&quot;``.
- Jinja2 keeps every compiled template in memory, so the Django engines here
  always use the cached loader (the production default), unlike some
  ``bench.py`` rows. "Compile" is ``from_string()`` for inline templates and
  a cache-hit ``get_template()`` for loader-based ones.
- Scenarios with no equivalent are left out: ``spaceless_table`` (Jinja2 has
  no ``spaceless``), ``inclusion_tag`` (no inclusion tags), and
  ``value_types`` (Jinja2 prints ``str(value)``, DTL localizes dates,
  decimals and floats).
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import statistics
import subprocess
import sys
from functools import partial
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bench  # noqa: E402

ROOT = bench.ROOT
VARIANTS = ("stock", "jinja2", "dtl")

# --------------------------------------------------------------------------
# Jinja2 translations of bench.py scenarios
# --------------------------------------------------------------------------

_JINJA: dict[str, dict] = {}


def jinja(name, source, loader=None):
    assert name in bench._SCENARIOS, name
    _JINJA[name] = {"source": source, "loader": loader or {}}


jinja("simple_var", "Hello {{ name }}!")
jinja(
    "filter_chain",
    "{{ text|default('x', true)|upper|truncatechars(12) }}|{{ missing|default_if_none('—') }}",
)
jinja("many_vars", bench.MANY_VARS)
jinja("loop", "{% for n in nums %}<li>#{{ loop.index }} {{ n|capfirst }}</li>{% endfor %}")
jinja(
    "if_branch",
    "{% for n in nums %}{% if n > 90 %}hi{% elif n > 50 %}mid{% else %}lo{% endif %}{% endfor %}",
)
jinja(
    "attr_lookup",
    "{% for p in people %}{{ p.name|upper }}: "
    "{% if p.is_staff %}admin{% else %}user{% endif %};"
    "{{ p.dept.title|default('none', true) }} {% endfor %}",
)
jinja("autoescape", "{% for r in rows %}<p>{{ raw }} | {{ safe }}</p>{% endfor %}")
jinja(
    "cycle_reset",
    "{% for n in nums %}{{ loop.cycle('a', 'b', 'c') }}{{ loop.cycle('x', 'y') }} {% endfor %}",
)
jinja(
    "firstof_widthratio",
    "{{ missing or also_missing or present }} {{ (a / b * c)|round|int }}",
)
jinja("date_filter", "{% for d in dates %}{{ d|date('D m, Y, fA') }} {% endfor %}")
jinja("url_tag", "{{ url('named.client', 42) }}")
jinja(
    "include",
    "{% for i in items %}{% include '_bench_inc.html' %}{% endfor %}",
    loader={"_bench_inc.html": bench._INCLUDE_BODY},
)
jinja(
    "extends",
    '{% extends "_bench_parent.html" %}\n'
    "{% block title %}List{% endblock %}\n"
    "{% block content %}{% for n in nums %}<b>{{ n }}</b>{% endfor %}{% endblock %}",
    loader={"_bench_parent.html": bench.PARENT_SRC},
)
jinja(
    "nested_table",
    "{% for row in rows %}<tr>{% for cell in row %}"
    "{% if cell %}{{ cell }}{% else %}-{% endif %},{% endfor %}</tr>{% endfor %}",
)
jinja(
    "with_if_scopes",
    "{% with v0=n %}"
    + "".join("{% with v" + str(i) + "=v" + str(i - 1) + " + 1 %}" for i in range(1, 12))
    + "{% if v11 > 10 %}{{ v11 }}{% else %}lo{% endif %}"
    + "{% endwith %}" * 12,
)
jinja("bridged_simple_tag", "{% for i in items %}{{ one_param(i) }},{% endfor %}")
jinja(
    "cached_extends",
    '{% extends "_bench_mid.html" %}\n'
    "{% block title %}{{ title }} | {{ super() }}{% endblock %}\n"
    "{% block content %}{% for n in nums %}<b>{{ n }}</b>{% endfor %}{% endblock %}\n"
    "{% block footer %}{{ super() }}<p>child</p>{% endblock %}",
    loader={
        "_bench_base.html": bench._CACHED_BASE,
        "_bench_mid.html": bench._CACHED_MID.replace("block.super", "super()"),
    },
)
jinja(
    "csrf_token_loop",
    "<form>{% for i in items %}"
    '<input type="hidden" name="csrfmiddlewaretoken" value="{{ csrf_token }}">'
    "{% endfor %}</form>",
)
jinja(
    "include_with",
    "{% for p in people %}{% with name=p.name, n=loop.index %}"
    "{% include '_bench_card.html' %}{% endwith %}{% endfor %}",
    loader={"_bench_card.html": "<li>{{ n }}: {{ name }}</li>"},
)
jinja(
    "index_none_lookup",
    "{% for r in rows %}{{ r[0] }}-{{ r[2] }}|{{ none.attr }}{{ none.attr.deeper }}"
    "{{ r[1][0] }};{% endfor %}",
)
jinja(
    "object_attrs",
    "{% for p in people %}{{ p.name }} {{ p.dept.title }} {{ p.pk }};{% endfor %}",
)
jinja(
    "object_methods",
    '{% for p in people %}<a href="{{ p.get_absolute_url() }}">{{ p.full() }}</a>{% endfor %}',
)
jinja(
    "object_properties",
    "{% for p in people %}{% if p.is_staff %}staff{% else %}-{% endif %}"
    "{{ p.is_staff }} {{ p.missing }}{% endfor %}",
)
jinja(
    "realistic_page",
    '{% extends "_page_base.html" %}{% block title %}People | {{ super() }}{% endblock %}\n'
    "{% block content %}<h1>{{ heading|title }}</h1><p>{{ intro|linebreaksbr }}</p>\n"
    '<table>{% for p in people %}{% include "_page_row.html" %}'
    "{% else %}<tr><td>none</td></tr>{% endfor %}</table>\n"
    "<p>{{ people|length }} people, {{ total|floatformat }} total</p>{% endblock %}",
    loader={
        "_page_base.html": bench._PAGE_BASE,
        "_page_nav.html": bench._PAGE_NAV,
        # DTL restarts a {% cycle %} on every {% include %}, so stock Django
        # prints "odd" on every row; a cycle here would change the output.
        "_page_row.html": (
            '<tr class="odd"><td><a href="{{ p.get_absolute_url() }}">{{ p.full() }}</a></td>'
            "<td>{{ p.email|urlize }}</td><td>{{ p.dept }}</td><td>{{ p.joined|date(\"Y-m-d\") }}</td>"
            "<td>{{ p.salary|floatformat(2) }}</td><td>{% if p.is_staff %}staff{% endif %}</td>"
            '<td>{{ p.tags|join(", ")|default("-", true) }}</td></tr>'
        ),
    },
)
jinja(
    "floatformat",
    "{% for v in vals %}{{ v|floatformat(2) }}|{{ v|floatformat }}|{{ v|floatformat('-3') }}"
    "|{{ v|floatformat('2g') }};{% endfor %}",
)
jinja("urlize", "{% for t in texts %}<p>{{ t|urlize }}</p>{{ t|urlizetrunc(15) }}{% endfor %}")
# DTL string literals are safe, so DTL doesn't escape the '<br>' separator.
jinja("join", "{% for r in rows %}{{ r|join(', ') }};{{ r|join('<br>'|safe) }}{% endfor %}")
jinja(
    "i18n_tags",
    "<nav>{{ _('Home') }} {% set lo = _('Log out') %}{{ lo }}</nav>"
    "{% for r in rows %}<td>{{ r.label }}</td><td>{{ _('Save') }}</td>{% endfor %}",
)
jinja(
    "aware_dates",
    "{% for d in dts %}"
    '<time datetime="{{ d|date(\'c\') }}">{{ d|date("N j, Y") }} {{ d|time("H:i") }}</time>'
    "{{ d|date }}{% endfor %}",
)
jinja(
    "striptags_intcomma",
    "{% for a in arts %}<p>{{ a.body|striptags|truncatewords(12) }}</p>"
    "<span>{{ a.views|intcomma }} views</span>{% endfor %}",
)
jinja(
    "truncate_html_sizes",
    "{% for a in arts %}<div>{{ a.body|truncatewords_html(8) }}</div>"
    "<p>{{ a.body|truncatechars_html(40) }}</p>"
    "<span>{{ a.size|filesizeformat }}, {{ a.views|intword }} views</span>{% endfor %}",
)
jinja("engine_e2e", "Hi {{ name }}! {% for n in nums %}{{ n }}{% endfor %}")


# Django filters with no same-output Jinja2 builtin. Jinja2's own ``urlize``,
# ``striptags`` and ``truncate`` behave differently, so Django's replace them.
_DJANGO_FILTERS = (
    "capfirst date default_if_none filesizeformat floatformat linebreaksbr striptags"
    " time truncatechars truncatechars_html truncatewords truncatewords_html urlize"
    " urlizetrunc"
).split()


def _django_filter(fn):
    """Wrap a Django filter function for Jinja2. DTL converts aware datetimes
    to the current time zone before calling ``expects_localtime`` filters
    (``date``, ``time``); a Jinja2 setup has to do the same."""
    if not getattr(fn, "expects_localtime", False):
        return fn
    from django.utils.timezone import template_localtime

    return lambda value, *args: fn(template_localtime(value), *args)


def build_jinja_env(loader):
    import jinja2
    from django.contrib.humanize.templatetags.humanize import intcomma, intword
    from django.template import defaultfilters
    from django.urls import reverse
    from django.utils.translation import gettext, ngettext
    from template_tests.templatetags.custom import one_param

    env = jinja2.Environment(
        loader=jinja2.DictLoader(loader),
        autoescape=True,
        undefined=jinja2.ChainableUndefined,
        auto_reload=False,
        extensions=["jinja2.ext.i18n"],
    )
    env.install_gettext_callables(gettext, ngettext, newstyle=True)
    for name in _DJANGO_FILTERS:
        env.filters[name] = _django_filter(getattr(defaultfilters, name))
    env.filters["intcomma"] = intcomma
    env.filters["intword"] = intword
    env.globals["url"] = lambda name, *args: reverse(name, args=args)
    env.globals["one_param"] = one_param
    return env


# --------------------------------------------------------------------------
# Engines
# --------------------------------------------------------------------------


def django_sdef(name):
    """bench.py's scenario, forced onto the cached loader when it loads
    templates (Jinja2 always caches compiled templates)."""
    sdef = bench._SCENARIOS[name]
    return dict(sdef, cached=sdef["cached"] or uses_loader(name))


def uses_loader(name):
    sdef = bench._SCENARIOS[name]
    return bool(sdef["loader"]) or sdef["e2e"]


def make_fns(variant, name):
    if variant != "jinja2":
        return bench.make_fns(variant, django_sdef(name))
    sdef, jdef = bench._SCENARIOS[name], _JINJA[name]
    ctx = sdef["ctx"]()
    if uses_loader(name):
        env = build_jinja_env(dict(jdef["loader"], **{"__bench_main.html": jdef["source"]}))
        get = partial(env.get_template, "__bench_main.html")
        if sdef["e2e"]:
            return get, lambda: get().render(ctx)
        return get, partial(get().render, ctx)
    env = build_jinja_env({})
    tmpl = env.from_string(jdef["source"])
    return partial(env.from_string, jdef["source"]), partial(tmpl.render, ctx)


def selected(only):
    return [n for n in _JINJA if not only or any(o in n for o in only)]


def measure(variant, only):
    out = []
    for name in selected(only):
        compile_fn, render_fn = make_fns(variant, name)
        out.append(
            {
                "scenario": name,
                "variant": variant,
                "render_us": bench.time_min(render_fn) * 1e6,
                "compile_us": bench.time_min(compile_fn) * 1e6,
            }
        )
    return out


def normalize_jinja(s):
    return s.replace("&#39;", "&#x27;").replace("&#34;", "&quot;")


def verify_outputs(only):
    bad = []
    for name in selected(only):
        want = str(make_fns("stock", name)[1]())
        got = {
            "dtl": str(make_fns("dtl", name)[1]()),
            "jinja2": normalize_jinja(make_fns("jinja2", name)[1]()),
        }
        for variant, out in got.items():
            if out != want:
                at = next((i for i, (a, b) in enumerate(zip(want, out)) if a != b), min(len(want), len(out)))
                bad.append(
                    f"{name} ({variant}) differs at {at}:\n"
                    f"  stock={want[max(0, at - 40):at + 80]!r}\n"
                    f"  {variant:>5}={out[max(0, at - 40):at + 80]!r}"
                )
    if bad:
        raise SystemExit("output mismatch:\n" + "\n".join(bad))


# --------------------------------------------------------------------------
# Parent orchestration
# --------------------------------------------------------------------------


def child(variant, only):
    cmd = [sys.executable, os.path.abspath(__file__), "--child", variant]
    for o in only or []:
        cmd += ["--only", o]
    proc = subprocess.run(cmd, capture_output=True, text=True, cwd=ROOT)
    if proc.returncode != 0:
        sys.stderr.write(proc.stdout + proc.stderr)
        raise SystemExit(f"child {variant!r} failed rc={proc.returncode}")
    return [json.loads(line) for line in proc.stdout.splitlines() if line.startswith("{")]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--child")
    ap.add_argument("--only", action="append")
    ap.add_argument("--json")
    ap.add_argument("--skip-verify", action="store_true")
    args = ap.parse_args()
    if args.json:
        args.json = os.path.abspath(args.json)

    bench.bootstrap()

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
        print("output parity: OK (all three engines match on every scenario)", file=sys.stderr)

    rows = {}
    for variant in VARIANTS:
        for rec in child(variant, args.only):
            rows.setdefault(rec["scenario"], {})[variant] = rec

    gm = statistics.geometric_mean
    # Compile rows are either a real parse or a cache-hit get_template();
    # the two differ by ~100x, so each gets its own geomean.
    groups = {
        "render": {"geomean": list(rows)},
        "compile": {
            "geomean (parse)": [n for n in rows if not uses_loader(n)],
            "geomean (cache hit)": [n for n in rows if uses_loader(n)],
        },
    }
    for kind, geomeans in groups.items():
        key = f"{kind}_us"
        hdr = (
            f"{kind:<19} {'stock':>11} {'jinja2':>11} {'dtl':>11}"
            f"   {'jinja2/stock':>12} {'dtl/stock':>10} {'dtl/jinja2':>11}"
        )
        print(hdr)
        print("-" * len(hdr))
        ratios = {}
        for name, by in rows.items():
            s, j, d = (by[v][key] for v in VARIANTS)
            ratios[name] = (s / j, s / d, j / d)
            print(
                f"{name:<19} {s:>8.1f} us {j:>8.1f} us {d:>8.1f} us"
                f"   {s / j:>11.2f}x {s / d:>9.2f}x {j / d:>10.2f}x"
            )
        for label, names in geomeans.items():
            if not names:
                continue
            j_s, d_s, d_j = (gm(col) for col in zip(*(ratios[n] for n in names)))
            print(
                f"{label:<19} {'':>11} {'':>11} {'':>11}"
                f"   {j_s:>11.2f}x {d_s:>9.2f}x {d_j:>10.2f}x"
            )
            tag = label.removeprefix("geomean").strip(" ()").replace(" ", "_")
            print(f"METRIC {kind}{'_' + tag if tag else ''}_dtl_vs_jinja2={d_j:.6f}")
        print()

    import django
    import jinja2

    print(
        f"(Python {sys.version.split()[0]}, Django {django.get_version()}, "
        f"Jinja2 {jinja2.__version__}, {platform.machine()}, "
        f"min of {bench.REPEATS} timeit repeats; Nx = times faster)"
    )

    if args.json:
        with open(args.json, "w") as fp:
            json.dump(rows, fp, indent=2)


if __name__ == "__main__":
    main()
