"""Bodies that bridged block tags parse with ``parser.parse()`` compile on the
Rust parser (``_nodes.ShimParser`` → ``ParseHook``) and render natively:
output, errors, introspection and inheritance must match stock exactly."""

import pytest
from django.template import Context, TemplateSyntaxError

from parity import ENGINES, IS_61, assert_parity, assert_render_parity

L = "{% load testlib %}"

CASES = {
    "wrap": (L + "a{% wrap %}{{ x }}|{{ x|upper }}{% endwrap %}b", {"x": "<i>"}),
    "wrap-empty": (L + "{% wrap %}{% endwrap %}", {}),
    "wrap-nested": (L + "{% wrap %}1{% wrap %}{{ x }}{% endwrap %}2{% endwrap %}", {"x": 3}),
    "wrap-in-for": (
        L + "{% for i in items %}{% wrap %}{{ forloop.counter }}{{ i }}"
        "{% cycle 'a' 'b' %}{% ifchanged i %}!{% endifchanged %}{% endwrap %}{% endfor %}",
        {"items": [1, 1, 2]},
    ),
    "for-in-wrap": (
        L + "{% wrap %}{% for i in items %}{{ forloop.counter }}{{ i }}{% endfor %}{% endwrap %}",
        {"items": "abc"},
    ),
    "autoescape-off": (
        L + "{% autoescape off %}{% wrap %}{{ x }}{% endwrap %}{% endautoescape %}{% wrap %}{{ x }}{% endwrap %}",
        {"x": "<b>"},
    ),
    "autoescape-inside": (L + "{% wrap %}{% autoescape off %}{{ x }}{% endautoescape %}{{ x }}{% endwrap %}", {"x": "<b>"}),
    "with-inside": (L + "{% wrap %}{% with y=x|add:1 %}{{ y }}{% endwith %}{{ y }}{% endwrap %}", {"x": 1}),
    "ifset-true": (L + "{% ifset x %}T{{ x }}{% otherwise %}F{% endifset %}", {"x": 1}),
    "ifset-false": (L + "{% ifset x %}T{{ x }}{% otherwise %}F{% endifset %}", {}),
    "ifset-noelse": (L + "{% ifset x %}T{% endifset %}.", {}),
    "inspect": (L + "{% inspect_body %}a{{ x }}{% if x %}{{ x }}{% endif %}b{% endinspect_body %}", {"x": 1}),
    "inspect-text": (L + "{% inspect_body %}text{% endinspect_body %}", {}),
    "drop-first": (L + "{% drop_first %}{{ x }}rest{{ x }}{% enddrop_first %}", {"x": 1}),
    "str-until": (L + "{% until_str %}{{ x }}{% endstr %}", {"x": 1}),
    "raw-until": (L + "{% raw_until %}a{{ b }}{% mid %}{{ x }}{% endraw_until %}", {"x": 1}),
    "load-inside": (L + "{% wrap %}{% load l10n %}{{ n|unlocalize }}{% endwrap %}{{ n|localize }}", {"n": 1000}),
    "localize": ("{% load l10n %}{% localize off %}{{ n }}{% for i in items %}{{ i }}{% endfor %}{% endlocalize %}", {"n": 1.5, "items": [1000, 2.5]}),
    "simple-tag-inside": (L + "{% wrap %}{% add 1 2 %}{% chip 'n' %}{% endwrap %}", {}),
    "include-inside": (L + "{% wrap %}{% include 'chip.html' with name=x %}{% endwrap %}", {"x": "<z>"}),
    "block-no-extends": (L + "{% wrap %}{% block a %}A{{ x }}{% endblock %}{% endwrap %}", {"x": 1}),
    "block-in-child": (
        L + '{% extends "base.html" %}{% wrap %}{% block one %}C1:{{ block.super }}{% endblock %}{% endwrap %}',
        {},
    ),
    "block-in-parent-body": (
        L + "{% wrap %}{% block one %}P{% endblock %}{% endwrap %}",
        {},
    ),
}


@pytest.mark.parametrize("src,ctx", CASES.values(), ids=CASES.keys())
def test_bridged_body(src, ctx):
    assert_render_parity(src, ctx)


def test_block_override_inside_parent_bridged_body():
    from django.template import engines  # noqa: F401
    loader = {
        "p.html": L + "<{% wrap %}{% block one %}P{% endblock %}{% endwrap %}>",
        "c.html": '{% extends "p.html" %}{% block one %}C:{{ block.super }}{% endblock %}',
    }
    from django.template.engine import Engine as StockEngine

    from django_rust_templates import Engine as RustEngine

    kw = dict(
        loaders=[("django.template.loaders.locmem.Loader", loader)],
        libraries={"testlib": "dtl_testlib"},
    )
    out = {
        name: cls(**kw).get_template("c.html").render(Context({}))
        for name, cls in (("stock", StockEngine), ("rust", RustEngine))
    }
    assert out["rust"] == out["stock"] == "<[C:P]>"


ERRORS = {
    "unclosed": L + "{% wrap %}{{ x }}",
    "unclosed-inner": L + "{% wrap %}{% if x %}{% endwrap %}",
    "bad-tag-inside": L + "{% wrap %}{% bogus %}{% endwrap %}",
    "bad-filter-inside": L + "{% wrap %}{{ x|nope }}{% endwrap %}",
    "unclosed-ifset": L + "{% ifset x %}a{% otherwise %}b",
}


@pytest.mark.parametrize("src", ERRORS.values(), ids=ERRORS.keys())
def test_bridged_body_errors(src):
    assert_parity(lambda e: e.from_string(src).render(Context({})))


@pytest.mark.parametrize("src", ERRORS.values(), ids=ERRORS.keys())
def test_bridged_body_errors_debug(src):
    def run(e):
        e.debug = True
        try:
            e.from_string(src)
        except TemplateSyntaxError as exc:
            return str(exc), exc.template_debug["line"], exc.template_debug["during"]
        finally:
            e.debug = False

    assert_parity(run)


def test_render_error_annotation_debug():
    src = L + "line1\n{% wrap %}\n{{ x.boom }}\n{% endwrap %}"

    class Boom:
        @property
        def boom(self):
            raise ValueError("boom")

    def run(e):
        e.debug = True
        try:
            e.from_string(src).render(Context({"x": Boom()}))
        except ValueError as exc:
            d = exc.template_debug
            return d["line"], d["during"], d["message"]
        finally:
            e.debug = False

    assert_parity(run)


def test_body_nodes_visible_to_tree_walkers():
    from django.template.base import VariableNode

    def run(e):
        t = e.from_string(L + "{{ a }}{% wrap %}{{ b }}{% wrap %}{{ c }}{% endwrap %}{% endwrap %}")
        return len(t.nodelist.get_nodes_by_type(VariableNode))

    assert_parity(run)


@pytest.mark.skipif(not IS_61, reason="partials are Django 6.1+")
def test_partialdef_inside_bridged_body():
    src = L + "{% wrap %}{% partialdef p inline %}P{{ x }}{% endpartialdef %}{% endwrap %}{% partial p %}"
    assert_render_parity(src, {"x": 1})

    def run(e):
        t = e.from_string(src)
        t.render(Context({"x": 1}))
        return t.extra_data["partials"]["p"].render(Context({"x": 2}))

    assert_parity(run)
