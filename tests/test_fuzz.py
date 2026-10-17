"""Differential fuzzing: random templates and contexts must render
identically (or fail identically) on stock Django and the Rust engine.

``DTL_FUZZ_ITERS`` sets the number of examples (default 300) and
``DTL_FUZZ_SEED`` pins the seed for reproduction.
"""

import os

from hypothesis import HealthCheck, given, seed, settings
from hypothesis import strategies as st

from parity import assert_render_parity

ITERS = int(os.environ.get("DTL_FUZZ_ITERS", "300"))
SEED = os.environ.get("DTL_FUZZ_SEED")

NAMES = ["a", "b", "items", "obj", "n"]
FILTERS = [
    "upper", "lower", "title", "length", "first", "last", "safe", "escape",
    "force_escape", "striptags", "linebreaksbr", "urlize", "wordcount",
    "default:'-'", "default_if_none:'?'", "join:', '", "cut:' '", "add:1",
    "floatformat", "floatformat:2", "slice:':2'", "truncatechars:5", "yesno",
    "pluralize", "addslashes", "capfirst", "center:9", "ljust:6", "make_list",
    "stringformat:'s'", "divisibleby:2",
]

text = st.text(alphabet="ab &<>\"'{}%#\n é✓", max_size=40)
var_name = st.sampled_from(NAMES).flatmap(
    lambda n: st.sampled_from([n, f"{n}.0", f"{n}.x", f"{n}.upper", f"{n}.items"])
)


@st.composite
def var_tag(draw):
    expr = draw(var_name)
    for f in draw(st.lists(st.sampled_from(FILTERS), max_size=3)):
        expr += "|" + f
    return "{{ " + expr + " }}"


def block(children):
    cond = st.sampled_from(["a", "not a", "n > 1", "items", "a == b", "obj.x"])
    return st.one_of(
        st.tuples(cond, children, children).map(
            lambda t: f"{{% if {t[0]} %}}{t[1]}{{% else %}}{t[2]}{{% endif %}}"
        ),
        st.tuples(st.sampled_from(NAMES), children, children).map(
            lambda t: f"{{% for it in {t[0]} %}}{{{{ it }}}}{t[1]}"
            f"{{% empty %}}{t[2]}{{% endfor %}}"
        ),
        st.tuples(st.sampled_from(NAMES), children).map(
            lambda t: f"{{% with v={t[0]} %}}{{{{ v }}}}{t[1]}{{% endwith %}}"
        ),
        st.tuples(st.sampled_from(["on", "off"]), children).map(
            lambda t: f"{{% autoescape {t[0]} %}}{t[1]}{{% endautoescape %}}"
        ),
        children.map(lambda c: f"{{# {c[:10]} #}}"),
    )


template = st.recursive(
    st.one_of(text, var_tag()),
    lambda children: st.one_of(
        block(children), st.lists(children, min_size=2, max_size=4).map("".join)
    ),
    max_leaves=12,
)

scalar = st.one_of(
    st.none(), st.booleans(), st.integers(-5, 5), st.floats(-1e3, 1e3, allow_nan=False),
    st.text(alphabet="ab &<>\"'é\n", max_size=30),
)
value = st.one_of(
    scalar,
    st.lists(scalar, max_size=4),
    st.dictionaries(st.sampled_from(["x", "0", "items"]), scalar, max_size=3),
)
context = st.fixed_dictionaries({}, optional={n: value for n in NAMES})


def _maybe_seed(fn):
    return seed(int(SEED))(fn) if SEED else fn


@_maybe_seed
@settings(
    max_examples=ITERS,
    deadline=None,
    suppress_health_check=[HealthCheck.too_slow, HealthCheck.data_too_large],
)
@given(src=template, ctx=context)
def test_random_templates(src, ctx):
    assert_render_parity(src, ctx)
