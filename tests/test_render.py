"""Variables, filters, literals, autoescape, comments, verbatim and syntax
errors: byte-identical output and identical error messages vs stock Django."""

import pytest
from django.template import Context, TemplateSyntaxError

from parity import ENGINES, IS_61, assert_render_parity

RENDER_CASES = {
    "plain": ("hello world", {}),
    "var": ("x={{ x }}", {"x": "<b>"}),
    "var-filter": ("{{ x|default:'a' }}", {}),
    "var-upper": ("{{ x|upper }}", {"x": "Ab"}),
    "var-int": ("{{ x }}", {"x": 42}),
    "var-none": ("[{{ x }}]", {"x": None}),
    "var-true": ("{{ x }}", {"x": True}),
    "var-false": ("{{ x }}", {"x": False}),
    "literal-str": ('{{ "a<b" }}', {}),
    "literal-num": ("{{ 1.5 }} {{ -2 }}", {}),
    "dotted": ("{{ a.b.0 }}", {"a": {"b": ["<i>"]}}),
    "attr": ("{{ o.x }}", {"o": type("O", (), {"x": "v"})()}),
    "method": ("{{ o.f }}", {"o": type("O", (), {"f": lambda s: "m"})()}),
    "missing": ("[{{ nope }}|{{ nope.deeper }}]", {}),
    "autoescape-off": ("{% autoescape off %}{{ y }}{% endautoescape %}", {"y": "<i>"}),
    "autoescape-on": ("{% autoescape on %}{{ y }}{% endautoescape %}", {"y": "<i>"}),
    "safe": ("{{ y|safe }}", {"y": "<i>"}),
    "force_escape": ("{{ y|force_escape }}", {"y": "<i>"}),
    "filter-chain": ("{{ x|upper|lower }}", {"x": "Ab"}),
    "filter-arg-colon": ('{{ x|cut:":" }}', {"x": "a:b"}),
    "comment": ("a{# c #}b", {}),
    "comment-tag": ("a{% comment %}{{ x }}{% endcomment %}b", {"x": 1}),
    "verbatim": ("{% verbatim %}{{ z }}{% endverbatim %}", {"z": "!"}),
    "verbatim-named": ("{% verbatim v %}{% endverbatim %}{% endverbatim v %}", {}),
    "slicing": ("{{ x|slice:':2' }}", {"x": "abc"}),
    "join": ("{{ x|join:'-' }}", {"x": ["a", "<b>"]}),
    "length": ("{{ x|length }}", {"x": [1, 2, 3]}),
    "linebreaks": ("{{ x|linebreaks }}", {"x": "a\nb"}),
    "dict-values": ("{{ x|length }}", {"x": {"a": 1}}),
    "floatformat": ("{{ x|floatformat:2 }}|{{ x|floatformat:'-3' }}", {"x": 3.14159}),
    "urlize": ("{{ x|urlize }}", {"x": "see https://example.com/?a=1&b=<2> now"}),
    "date": ("{{ d|date:'Y-m-d D' }}", {"d": __import__("datetime").date(2024, 2, 29)}),
    "if-elif": ("{% if a > 1 %}A{% elif a == 1 %}E{% else %}O{% endif %}", {"a": 1}),
    "for-empty": ("{% for i in items %}{{ i }};{% empty %}none{% endfor %}", {"items": []}),
    "for-loopvars": (
        "{% for i in items %}{{ forloop.counter }}{{ forloop.last|yesno:'.,' }}{% endfor %}",
        {"items": "abc"},
    ),
    "with": ("{% with q=x|add:1 %}{{ q }}{% endwith %}", {"x": 1}),
    "cycle": ("{% for i in items %}{% cycle 'a' 'b' %}{% endfor %}", {"items": [1, 2, 3]}),
    "ifchanged": (
        "{% for i in items %}{% ifchanged %}{{ i }}{% endifchanged %}{% endfor %}",
        {"items": [1, 1, 2]},
    ),
    "spaceless": ("{% spaceless %}<p> <b>x</b> </p>{% endspaceless %}", {}),
    "firstof": ("{% firstof a b 'z' %}", {"b": "<B>"}),
    "widthratio": ("{% widthratio 175 200 100 %}", {}),
    "templatetag": ("{% templatetag openblock %}{% templatetag closevariable %}", {}),
}


@pytest.mark.parametrize("src,ctx", RENDER_CASES.values(), ids=RENDER_CASES.keys())
def test_render(src, ctx):
    assert_render_parity(src, ctx)


# --- HTML escaping: SIMD block scanner (16-byte blocks + scalar tail) ------

ESCAPE_INPUTS = {
    "empty": "",
    "clean-short": "hello",
    "clean-block": "a" * 16,
    "clean-long": "abcdefghij klmnopqrs " * 50,
    "all-specials": "&<>\"'" * 7,
    "special-first": "&" + "x" * 31,
    "special-last-in-block": "x" * 15 + "<" + "y" * 16,
    "special-first-of-tail": "x" * 32 + ">tail",
    "special-only-in-tail": "x" * 48 + "'",
    "dense": ("Tom & Jerry's <b>\"show\"</b> " + "x" * 30) * 20,
    "sparse": ("x" * 500 + "&") * 5,
    "utf8": "héllo <wörld> & ✓ \"ünïcode\" " * 9,
    "utf8-straddle": "é" * 8 + "<" + "✓" * 6 + "&",
    "existing-entities": "&amp; &lt; &#x27;",
}


@pytest.mark.parametrize("value", ESCAPE_INPUTS.values(), ids=ESCAPE_INPUTS.keys())
def test_autoescape(value):
    assert_render_parity("[{{ v }}]|{{ v|force_escape }}|{{ v|escape }}", {"v": value})


@pytest.mark.parametrize("value", ESCAPE_INPUTS.values(), ids=ESCAPE_INPUTS.keys())
def test_join_escapes_items(value):
    assert_render_parity("{{ items|join:sep }}", {"items": [value, value[:7]], "sep": "<&>"})


# --- Lexer: tag scanning edge cases ------------------------------------

LEXER_CASES = {
    "lone-braces": "{ } {{ }x {% %x {# #x",
    "css-js": "<style>.a { color: red; }</style><script>f({a: {b: 1}});</script>{{ x }}",
    "brace-at-end": "text {",
    "open-at-end": "text {{",
    "unclosed-var": "a {{ x b",
    "newline-in-tag": "a {{ x\n}} b {% if\nx %}",
    "newline-in-comment": "a {# x\n #} b",
    "tag-after-newline-break": "{{ x\n{{ x }}",
    "adjacent": "{{ x }}{{ x }}{% if x %}{% endif %}{# c #}{{ x }}",
    "nested-open": "{{ {{ x }}",
    "close-without-open": "}} %} #}",
    "triple-brace": "{{{ x }}}",
    "percent-in-var": "{{ '%}' }}",
    "verbatim-with-tags": "{% verbatim %}{% if %}{{ }}{% endverbatim %}{{ x }}",
    "long-text": ("lorem ipsum " * 200 + "{{ x }}\n") * 5,
    "utf8": "é{{ x }}✓{% if x %}ü{% endif %}",
}


@pytest.mark.parametrize("src", LEXER_CASES.values(), ids=LEXER_CASES.keys())
def test_lexer(src):
    assert_render_parity(src, {"x": "<X>"})


def test_line_numbers_in_errors():
    src = "line1\n{{ x }}\n{% if %}\n"
    for e in ENGINES.values():
        e.debug = True
    try:
        assert_render_parity(src)
    finally:
        for e in ENGINES.values():
            e.debug = False


# --- Syntax errors -------------------------------------------------------

ERROR_CASES = {
    "empty-var": ("{{ }}", "Empty variable tag on line 1"),
    "empty-block": ("{% %}", "Empty block tag on line 1"),
    "bad-filter": ("{{ x|nope }}", "Invalid filter: 'nope'"),
    "filter-arity": ("{{ x|default }}", "default requires 2 arguments, 1 provided"),
    "bad-tag": (
        "{% bogus %}",
        "Invalid block tag on line 1: 'bogus'. Did you forget to register or load this tag?",
    ),
    "unclosed-autoescape": (
        "{% autoescape on %}",
        "Unclosed tag on line 1: 'autoescape'. Looking for one of: endautoescape.",
    ),
    "autoescape-arg": ("{% autoescape x %}", "'autoescape' argument should be 'on' or 'off'"),
    "autoescape-args": (
        "{% autoescape on extra %}",
        "'autoescape' tag requires exactly one argument.",
    ),
    "endif-mismatch": ("{% if x %}a{% endfor %}", "endfor"),
    "unclosed-if": ("{% if x %}a", "Unclosed tag on line 1: 'if'"),
    "else-no-if": ("{% else %}", "else"),
}


@pytest.mark.parametrize("src,msg", ERROR_CASES.values(), ids=ERROR_CASES.keys())
def test_syntax_error(src, msg):
    errors = {}
    for key, engine in ENGINES.items():
        with pytest.raises(TemplateSyntaxError) as exc:
            engine.from_string(src).render(Context({}))
        errors[key] = (type(exc.value).__name__, str(exc.value))
    assert errors["rust"] == errors["stock"]
    assert msg in errors["stock"][1]


@pytest.mark.skipif(not IS_61, reason="Django 6.1 prefixes errors with the template name")
def test_error_template_name_prefix():
    from django.template.base import Origin

    from django_rust_templates import Template

    with pytest.raises(TemplateSyntaxError) as exc:
        Template("{% bogus %}", origin=Origin("t.html"), name="t.html", engine=ENGINES["rust"])
    assert str(exc.value).startswith("Template: t.html, ")
    assert exc.value.raw_error_message is not None
