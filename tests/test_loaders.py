"""Loaders, include/extends/block, {% load %} with custom tag libraries,
Django 6.1 partials, and the Python-visible nodelist."""

import pytest
from django.template import Context
from django.template.base import NodeList, TextNode, VariableNode

from parity import ENGINES, IS_61, assert_parity, assert_render_parity

STRING_CASES = {
    # custom libraries (simple / inclusion tags bridged to Python)
    "load-ok": ("{% load testlib %}{% ctx_tag %}|{% ctx_tag 'q' %}", {"who": "w"}),
    "load-add": ("{% load testlib %}{% add 2 3 %}/{% add 2 %}", {}),
    "load-inclusion-tag": ("{% load testlib %}{% chip 'c' %}", {}),
    "load-inclusion-template-obj": ("{% load testlib %}{% chip_t 'd' %}", {}),
    "load-missing": ("{% load nosuchlib %}", {}),
    "load-missing-2nd": ("{% load testlib nosuchlib %}", {}),
    "load-noargs": ("{% load %}", {}),
    "load-filter": ("{% load testlib %}{{ 2|add:1 }}", {}),
    # include
    "include": ('{% include "inc.html" %}', {"x": 1, "y": 2}),
    "include-with": ('{% include "inc.html" x=9 %}', {"x": 1, "y": 2}),
    "include-only": ('{% include "only.html" x=5 only %}', {"x": 1}),
    "include-isolated": ('{% include "iso.html" x=5 only %}', {"x": 1}),
    "include-missing": ('{% include "nosuch.html" %}', {}),
    "include-noargs": ("{% include %}", {}),
    "include-conditional": ('{% if x %}{% include "inc.html" %}{% endif %}', {"x": True, "y": 4}),
    # extends / block
    "extends-variable": ("{% extends parent %}", {"parent": "base.html"}),
    "block-without-base": ("{% block x %}v{% endblock %}", {}),
    "block-unclosed": ("{% block x %}oops", {}),
    "extends-missing": ('{% extends "nosuch.html" %}', {}),
    "extends-not-first": ('pre{% extends "base.html" %}', {}),
    "extends-twice": ('{% extends "a.html" %}{% extends "b.html" %}', {}),
}


@pytest.mark.parametrize("src,ctx", STRING_CASES.values(), ids=STRING_CASES.keys())
def test_string_template(src, ctx):
    assert_render_parity(src, ctx)


def _render(name, ctx=None):
    return lambda e: e.get_template(name).render(Context(ctx or {}))


ENGINE_CASES = {
    "extends-basic": _render("child.html"),
    "extends-chain": _render("leaf.html"),
    "extends-block-in-if": _render("nestchild.html", {"flag": True}),
    "include-relative": _render("rel/t.html", {"x": 3}),
    "select-template": lambda e: e.select_template(["nope.html", "inc.html"]).render(
        Context({"x": "a", "y": "b"})
    ),
    "get-missing": lambda e: e.get_template("nosuch.html"),
    "select-missing": lambda e: e.select_template(["n1.html", "n2.html"]),
    "select-empty": lambda e: e.select_template([]),
    "get-empty": lambda e: e.get_template(""),
    "find-template": lambda e: str(e.find_template("inc.html")[1]),
    "render-to-string": lambda e: e.render_to_string("base.html", {}),
    "repr": lambda e: repr(e.get_template("inc.html")),
    "get-template-twice": lambda e: [
        e.get_template("inc.html").render(Context({"x": 1, "y": 2})),
        e.get_template("inc.html").render(Context({"x": 3, "y": 4})),
    ],
}


@pytest.mark.parametrize("fn", ENGINE_CASES.values(), ids=ENGINE_CASES.keys())
def test_engine_api(fn):
    assert_parity(fn)


PARTIAL_CASES = {
    "partialdef": lambda e: e.from_string(
        "{% partialdef p %}PP{% endpartialdef %}x{% partial p %}"
    ).render(Context()),
    "partialdef-inline": lambda e: e.from_string(
        "{% partialdef p inline %}PP{% endpartialdef %}|{% partial p %}"
    ).render(Context()),
    "partial-missing": lambda e: e.from_string("{% partial nosuch %}").render(Context()),
    "partialdef-duplicate": lambda e: e.from_string(
        "{% partialdef p %}a{% endpartialdef %}{% partialdef p %}b{% endpartialdef %}"
    ),
    "partialdef-bad-arg": lambda e: e.from_string("{% partialdef p x %}a{% endpartialdef %}"),
    "partial-by-hash": _render("twopart.html#p1", {"x": "X"}),
    "partial-whole-file": _render("twopart.html", {"x": "X"}),
    "partial-extra-data": lambda e: sorted(
        e.get_template("twopart.html").extra_data.get("partials", {})
    ),
    "partial-hash-missing": lambda e: e.get_template("twopart.html#nope"),
}


@pytest.mark.skipif(not IS_61, reason="template partials are new in Django 6.1")
@pytest.mark.parametrize("fn", PARTIAL_CASES.values(), ids=PARTIAL_CASES.keys())
def test_partials(fn):
    assert_parity(fn)


def test_nodelist_is_stock_compatible():
    t = ENGINES["rust"].from_string("a{{ x }}{% if f %}{{ y }}{% endif %}b")
    nl = t.nodelist
    assert isinstance(nl, NodeList)
    assert t.nodelist is nl
    assert len(nl) == 4
    assert all(n.origin is not None for n in nl)
    assert len(nl.get_nodes_by_type(VariableNode)) == 2
    assert len(nl.get_nodes_by_type(TextNode)) == 2
    assert len(nl.get_nodes_by_type(object)) == 5
    ctx = Context({"x": "X", "f": True, "y": "Y"})
    t.render(ctx)
    assert nl[1].render_annotated(ctx) == "X"
    assert nl[3].render_annotated(ctx) == "b"


def test_render_count_increments():
    from django_rust_templates import _dtl

    before = _dtl.render_count()
    ENGINES["rust"].from_string("z").render(Context())
    assert _dtl.render_count() == before + 1


def test_django_backend():
    """``RustTemplates`` works as a ``TEMPLATES`` backend."""
    from django.template.backends.django import DjangoTemplates

    from django_rust_templates import RustTemplates
    from parity import TEMPLATES_DIR

    params = {"NAME": "t", "DIRS": [TEMPLATES_DIR], "APP_DIRS": False, "OPTIONS": {}}
    stock, rust = DjangoTemplates(dict(params)), RustTemplates(dict(params))
    for name, ctx in [("child.html", {}), ("inc.html", {"x": "<a>", "y": 2})]:
        assert rust.get_template(name).render(ctx) == stock.get_template(name).render(ctx)
    assert rust.from_string("{{ v }}").render({"v": "<&>"}) == "&lt;&amp;&gt;"
