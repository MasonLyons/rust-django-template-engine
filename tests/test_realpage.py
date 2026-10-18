"""Paths real pages hit that the plain-dict cases don't: i18n tags and lazy
translations, aware datetimes, striptags, humanize intcomma, and the form
renderer. Rendered with ``USE_I18N``/``USE_TZ`` on (the native fast paths
read the active language/timezone themselves) across several locales."""

import copy
import datetime
import zoneinfo
from decimal import Decimal

import pytest
from django import forms
from django.forms.renderers import DjangoTemplates
from django.template import Context
from django.template.engine import Engine as StockEngine
from django.test.utils import override_settings
from django.utils import timezone, translation
from django.utils.functional import empty
from django.utils.safestring import mark_safe
from django.utils.translation import gettext_lazy, pgettext_lazy

from django_rust_templates import Engine as RustEngine
from django_rust_templates.forms import RustFormRenderer
from parity import capture

LIBS = {
    "i18n": "django.templatetags.i18n",
    "humanize": "django.contrib.humanize.templatetags.humanize",
    "static": "django.templatetags.static",
    "tz": "django.templatetags.tz",
    "l10n": "django.templatetags.l10n",
}
ENGINES = {"stock": StockEngine(libraries=LIBS), "rust": RustEngine(libraries=LIBS)}
LANGS = [None, "de", "fr", "es", "ja"]


@pytest.fixture(autouse=True)
def i18n_tz():
    with override_settings(USE_I18N=True, USE_TZ=True, TIME_ZONE="America/Chicago"):
        yield


def assert_parity(src, ctx=None, lang=None, tz=None):
    def run(engine):
        with translation.override(lang):
            if tz is None:
                return engine.from_string(src).render(Context(ctx or {}))
            with timezone.override(tz):
                return engine.from_string(src).render(Context(ctx or {}))

    assert capture(lambda: run(ENGINES["rust"])) == capture(lambda: run(ENGINES["stock"]))


TRANSLATE_CASES = {
    "literal": "{% translate 'Home' %}|{% trans \"Log out\" %}|{% translate 'January' %}",
    "as-var": "{% translate 'Yes' as y %}[{{ y }}|{{ y|upper }}]",
    "percent": "{% translate '100% sure %%' %}",
    "html": "{% translate '<b>' %}{% autoescape off %}{% translate '<b>' %}{% endautoescape %}",
    "empty": "[{% translate '' %}]",
    "noop": "{% translate 'Home' noop %}",
    "context": "{% translate 'May' context 'month name' %}",
    "filter": "{% translate 'Home'|upper %}",
    "quote": "{% translate \"it's\" %}",
    "variable": "{% translate word %}",
}


@pytest.mark.parametrize("lang", LANGS)
@pytest.mark.parametrize("src", TRANSLATE_CASES.values(), ids=TRANSLATE_CASES.keys())
def test_translate_tag(src, lang):
    assert_parity("{% load i18n %}" + src, {"word": "No"}, lang=lang)


def test_translate_after_active_rebind():
    """`setting_changed` replaces `trans_real._active` with a new `Local`."""
    src = "{% load i18n %}{% translate 'No' %}{{ lazy }}"
    ctx = {"lazy": gettext_lazy("Yes")}
    assert_parity(src, ctx, lang="de")
    with override_settings(LANGUAGES=[("de", "German"), ("nl", "Dutch")]):
        assert_parity(src, ctx, lang="nl")
    assert_parity(src, ctx, lang="fr")


LAZY = [
    gettext_lazy("Yes"),
    gettext_lazy("<b>x</b>"),
    gettext_lazy(mark_safe("<b>Home</b>")),
    gettext_lazy("a\r\nb"),
    gettext_lazy(""),
    pgettext_lazy("month name", "May"),
]


@pytest.mark.parametrize("lang", LANGS)
def test_lazy_strings(lang):
    src = (
        "{% for v in vals %}[{{ v }}|{{ v|upper }}]{% endfor %}"
        "{% autoescape off %}{% for v in vals %}{{ v }}{% endfor %}{% endautoescape %}"
    )
    assert_parity(src, {"vals": LAZY}, lang=lang)


AWARE = datetime.datetime(2026, 3, 8, 7, 30, 15, 123, tzinfo=datetime.timezone.utc)
DATE_CASES = {
    "custom": '{{ d|date:"N j, Y H:i" }}',
    "iso": "{{ d|date:'c' }}|{{ d|date:'\\c c' }}",
    "named": "{{ d|date }}|{{ d|date:'SHORT_DATE_FORMAT' }}|{{ d|date:'' }}",
    "time": "{{ d|time }}|{{ d|time:'H:i' }}|{{ d|time:'c' }}",
    "plain": "{{ d }}",
    "date-obj": "{{ day|date }}|{{ day|date:'c' }}",
    "naive": "{{ naive|date:'c' }}|{{ naive }}",
}


@pytest.mark.parametrize("tz", [None, "Asia/Kolkata", "America/New_York"])
@pytest.mark.parametrize("lang", [None, "de", "ja"])
@pytest.mark.parametrize("src", DATE_CASES.values(), ids=DATE_CASES.keys())
def test_dates(src, lang, tz):
    ctx = {
        "d": AWARE,
        "day": datetime.date(2024, 2, 29),
        "naive": datetime.datetime(2024, 2, 29, 23, 59),
    }
    assert_parity(src, ctx, lang=lang, tz=zoneinfo.ZoneInfo(tz) if tz else None)


def test_dates_use_tz_off():
    with override_settings(USE_TZ=False):
        assert_parity("{{ d }}|{{ d|date:'c' }}", {"d": AWARE})


def test_now_year():
    assert_parity("{% now 'Y' %}", tz=zoneinfo.ZoneInfo("Pacific/Kiritimati"))


STRIP_INPUTS = [
    "<p>Hello <b>world</b></p>",
    "a &amp; b<br/>c<br />d &#39;e&#x27;",
    '<a href="/x?a=1&amp;b=2" class=\'c\' data-x=y disabled>t</a>',
    "no tags",
    "a < b",
    "a < b > c",
    "<!-- c --><p>x</p>",
    "<script>alert(1)</script>after",
    "<title>T</title>",
    "AT&T <b>x</b>",
    "<p>&amp</p>",
    '<a title="a>b">x</a>',
    "</p >",
    "<p",
    "<p>x</p><",
    "<<b>>",
    mark_safe("<i>safe</i> &amp;"),
]


@pytest.mark.parametrize("value", STRIP_INPUTS)
def test_striptags(value):
    assert_parity("{{ v|striptags }}|{{ v|striptags|truncatewords:2 }}", {"v": value})


INTCOMMA_VALUES = [0, 7, -1234, 1234567, -(2**63), 2**80, True, 1.5, Decimal("1234.5"), "12345", "x", None]


@pytest.mark.parametrize("lang", [None, "de", "de-ch", "fr", "en-in", "hi"])
def test_intcomma(lang):
    src = "{% load humanize %}{% for v in vals %}[{{ v|intcomma }}|{{ v|intcomma:False }}]{% endfor %}"
    assert_parity(src, {"vals": INTCOMMA_VALUES}, lang=lang)


class WidgetForm(forms.Form):
    name = forms.CharField(max_length=50, help_text="Your <b>name</b>")
    email = forms.EmailField()
    age = forms.IntegerField(min_value=0)
    bio = forms.CharField(widget=forms.Textarea)
    color = forms.ChoiceField(choices=[(str(i), f"c{i}") for i in range(4)])
    agree = forms.BooleanField()
    when = forms.SplitDateTimeField()
    multi = forms.MultipleChoiceField(
        choices=[("a", "A"), ("b", "B")], widget=forms.CheckboxSelectMultiple
    )
    pick = forms.ChoiceField(
        choices=[("g", [("1", "one"), ("2", "two")]), ("3", "three")], widget=forms.RadioSelect
    )


@pytest.mark.parametrize("data", [None, {"name": "x<y", "email": "bad", "age": "-1", "multi": ["a"]}])
@pytest.mark.parametrize("method", ["as_div", "as_p", "as_table", "as_ul", "__str__"])
def test_form_renderer(method, data):
    stock = getattr(WidgetForm(data, renderer=DjangoTemplates()), method)()
    rust = getattr(WidgetForm(data, renderer=RustFormRenderer()), method)()
    assert str(rust) == str(stock)


def test_form_errors_deepcopy():
    form = WidgetForm({"email": "bad"}, renderer=RustFormRenderer())
    errors = form.errors
    str(errors)  # instantiates the renderer's engine
    assert copy.deepcopy(errors) == errors


def test_dates_after_deactivate_all():
    """`deactivate_all()` renders untranslated names even when
    `LANGUAGE_CODE` has a catalog (cached format programs must not leak)."""
    src = '{{ d|date:"F D" }}'
    with override_settings(LANGUAGE_CODE="de"):
        assert_parity(src, {"d": AWARE})
        with translation.override(None, deactivate=True):
            translation.deactivate_all()
            assert_parity(src, {"d": AWARE})


def test_dates_mixed_trans_resolution():
    """`_trans` resolves each function once, by `USE_I18N` at first use, and
    never re-resolves. With `gettext` pinned to `trans_null` but `pgettext`
    still unresolved, a later `USE_I18N=True` run mixes both (`N` uses
    `pgettext_lazy`); cached programs must not leak across languages."""
    from django.utils.translation import _trans

    saved = dict(_trans.__dict__)
    try:
        _trans.__dict__.clear()
        with override_settings(USE_I18N=False):
            translation.gettext("x")  # pins gettext to trans_null
        with override_settings(USE_I18N=True):
            for lang in (None, "de", "ja", "de"):
                assert_parity('{{ d|date:"N F" }}', {"d": AWARE}, lang=lang)
    finally:
        _trans.__dict__.clear()
        _trans.__dict__.update(saved)


NT_NOW = datetime.datetime(2012, 3, 9, 22, 30)


class MockDateTime(datetime.datetime):
    """humanize_tests' fixed clock (`humanize.datetime` is patched)."""

    @classmethod
    def now(cls, tz=None):
        if tz is None or tz.utcoffset(NT_NOW) is None:
            return NT_NOW
        return NT_NOW.replace(tzinfo=tz) + tz.utcoffset(NT_NOW)


class NaiveTZ(datetime.tzinfo):
    def utcoffset(self, dt):
        return None


NT_DELTAS = [
    datetime.timedelta(0),
    datetime.timedelta(microseconds=1),
    datetime.timedelta(seconds=1),
    datetime.timedelta(seconds=30),
    datetime.timedelta(minutes=1, seconds=30),
    datetime.timedelta(minutes=2),
    datetime.timedelta(hours=1, minutes=30, seconds=30),
    datetime.timedelta(hours=23, minutes=50, seconds=50),
    datetime.timedelta(days=1),
    datetime.timedelta(days=2, hours=6),
    datetime.timedelta(days=9, seconds=1),
    datetime.timedelta(days=30),
    datetime.timedelta(days=61, minutes=3),
    datetime.timedelta(days=500),
    datetime.timedelta(days=865, microseconds=999999),
    datetime.timedelta(days=3653),
]


def naturaltime_values():
    vals = []
    for d in NT_DELTAS:
        vals += [NT_NOW - d, NT_NOW + d]
    chicago = zoneinfo.ZoneInfo("America/Chicago")
    for d in NT_DELTAS[::3]:
        vals += [
            (NT_NOW - d).replace(tzinfo=datetime.timezone.utc),
            (NT_NOW + d).replace(tzinfo=chicago),
            (NT_NOW - d).replace(tzinfo=zoneinfo.ZoneInfo("Asia/Kolkata")),
        ]
    vals += [
        NT_NOW.replace(tzinfo=NaiveTZ()),
        datetime.datetime(2012, 1, 31, 12),
        datetime.datetime(2011, 2, 28, 23, 59, 59, 999999),
        datetime.datetime(2012, 2, 29, 22, 30),
        datetime.date(2012, 3, 1),
        "test",
        None,
        0,
    ]
    return vals


@pytest.mark.parametrize("use_tz", [True, False])
@pytest.mark.parametrize("lang", [None, "de", "cs", "fr", "ja", "ru", "ar"])
def test_naturaltime(lang, use_tz):
    from django.contrib.humanize.templatetags import humanize

    src = "{% load humanize %}{% for v in vals %}[{{ v|naturaltime }}|{{ v|naturaltime|length }}]{% endfor %}"
    orig = humanize.datetime
    humanize.datetime = MockDateTime
    try:
        with override_settings(USE_TZ=use_tz):
            for v in naturaltime_values():
                assert_parity(src, {"vals": [v]}, lang=lang)
    finally:
        humanize.datetime = orig


def test_naturaltime_real_clock():
    now = timezone.now()
    vals = [now - datetime.timedelta(days=3, hours=5, seconds=30), now + datetime.timedelta(days=40, seconds=30),
            timezone.make_naive(now) - datetime.timedelta(hours=5, seconds=30)]
    src = "{% load humanize %}{% for v in vals %}[{{ v|naturaltime }}]{% endfor %}"
    for lang in [None, "de", "pl"]:
        assert_parity(src, {"vals": vals}, lang=lang)


def test_naturaltime_i18n_off():
    from django.contrib.humanize.templatetags import humanize

    orig = humanize.datetime
    humanize.datetime = MockDateTime
    try:
        with override_settings(USE_I18N=False):
            src = "{% load humanize %}{% for v in vals %}[{{ v|naturaltime }}]{% endfor %}"
            assert_parity(src, {"vals": naturaltime_values()}, lang="de")
    finally:
        humanize.datetime = orig


BT_ENGINES = {
    name: {
        "stock": StockEngine(libraries=LIBS, string_if_invalid=sii),
        "rust": RustEngine(libraries=LIBS, string_if_invalid=sii),
    }
    for name, sii in [("blank", ""), ("invalid", "INV(%s)"), ("plain-invalid", "INV")]
}

BLOCKTRANS_CASES = {
    "plain": "{% blocktranslate %}Hello & <b>{{ name }}</b>!{% endblocktranslate %}",
    "with": "{% blocktranslate with n=name|upper m=html %}{{ n }}: {{ m }}{% endblocktranslate %}",
    "legacy-with": "{% blocktrans with name as n and html as m %}{{ n }}/{{ m }}{% endblocktrans %}",
    "count": (
        "{% for c in counts %}{% blocktranslate count n=c %}{{ n }} item{% plural %}{{ n }} items"
        "{% endblocktranslate %};{% endfor %}"
    ),
    "legacy-count": "{% blocktrans count counts|length as n %}one {{ n }}{% plural %}{{ n }} many{% endblocktrans %}",
    "count-with": (
        "{% blocktranslate with who=name count n=counts.2 %}{{ who }} has {{ n }} cat"
        "{% plural %}{{ who }} has {{ n }} cats{% endblocktranslate %}"
    ),
    "catalog-plural": (
        "{% for c in counts %}{% blocktranslate count max=c %}Ensure that there are no more than "
        "{{ max }} digit in total.{% plural %}Ensure that there are no more than {{ max }} digits "
        "in total.{% endblocktranslate %}|{% endfor %}"
    ),
    "catalog": (
        "{% blocktranslate with model_name=name field_label='x' %}{{ model_name }} with this "
        "{{ field_label }} already exists.{% endblocktranslate %}"
    ),
    "context": "{% blocktranslate context 'abbrev. month' %}May{% endblocktranslate %}",
    "context-var": "{% blocktranslate context ctx %}May{% endblocktranslate %}",
    "context-empty": "{% blocktranslate context '' %}May{% endblocktranslate %}",
    "context-count": (
        "{% blocktranslate context 'month name' count n=1 %}May{% plural %}Mays"
        "{% endblocktranslate %}"
    ),
    "trimmed": (
        "{% blocktranslate trimmed with n=name %}\n  Hello\n   {{ n }}  \n\n world\n"
        "{% endblocktranslate %}"
    ),
    "asvar": "{% blocktranslate asvar v with n=html %}<{{ n }}>{% endblocktranslate %}[{{ v }}|{{ v|upper }}]",
    "asvar-count": (
        "{% blocktranslate count n=3 asvar v %}a{% plural %}{{ n }} b{% endblocktranslate %}"
        "[{{ v }}]{{ n }}"
    ),
    "percent": "{% blocktranslate %}100% of {{ pct }}%% done{% endblocktranslate %}",
    "missing": "{% blocktranslate %}[{{ nope }}][{{ name.upper }}][{{ a.b }}]{% endblocktranslate %}",
    "values": (
        "{% blocktranslate with d=day f=flt dt=aware %}{{ d }} {{ f }} {{ dt }} {{ lazy }} {{ none }}"
        "{% endblocktranslate %}"
    ),
    "autoescape-off": (
        "{% autoescape off %}{% blocktranslate with h=html %}{{ h }} {{ name }}{% endblocktranslate %}"
        "{% endautoescape %}"
    ),
    "empty": "[{% blocktranslate %}{% endblocktranslate %}]",
    "empty-plural": "{% blocktranslate count n=2 %}one {{ n }}{% plural %}{% endblocktranslate %}[{{ n }}]",
    "forloop": "{% for x in counts %}{% blocktranslate %}{{ forloop }}{{ x }}{% endblocktranslate %}{% endfor %}",
    "nested-in-with": (
        "{% with name='inner' %}{% blocktranslate with name=name|add:'!' %}{{ name }}"
        "{% endblocktranslate %}{{ name }}{% endwith %}{{ name }}"
    ),
    "decimal-count": (
        "{% blocktranslate count n=dec %}{{ n }} unit{% plural %}{{ n }} units{% endblocktranslate %}"
        "{% blocktranslate count n=flt %}{{ n }} unit{% plural %}{{ n }} units{% endblocktranslate %}"
    ),
    # Render-time errors.
    "count-not-number": "{% blocktranslate count n=name %}a{% plural %}b{% endblocktranslate %}",
    "bad-format": "{% blocktranslate %}x {{ a) }}{% endblocktranslate %}",
    "bad-format-asvar": "{% blocktranslate asvar v %}x {{ a) }}{% endblocktranslate %}[{{ v }}]",
    # Compile errors (stock parser messages).
    "unknown-option": "{% blocktranslate foo %}x{% endblocktranslate %}",
    "dup-option": "{% blocktranslate trimmed trimmed %}x{% endblocktranslate %}",
    "with-empty": "{% blocktranslate with %}x{% endblocktranslate %}",
    "count-two": "{% blocktranslate count a=1 b=2 %}x{% plural %}y{% endblocktranslate %}",
    "context-missing": "{% blocktranslate context %}x{% endblocktranslate %}",
    "asvar-missing": "{% blocktranslate asvar %}x{% endblocktranslate %}",
    "no-plural": "{% blocktranslate count n=1 %}x{% endblocktranslate %}",
    "inner-tag": "{% blocktranslate %}x{% if 1 %}y{% endif %}{% endblocktranslate %}",
    "plural-without-count": "{% blocktranslate %}x{% plural %}y{% endblocktranslate %}",
    "comment": "{% blocktranslate %}x{# c #}y{% endblocktranslate %}",
    "unclosed": "{% blocktranslate %}x {{ name }}",
    "unclosed-plural": "{% blocktranslate count n=1 %}x{% plural %}y",
    "wrong-end": "{% blocktrans %}x{% endblocktranslate %}",
    "bad-filter": "{% blocktranslate with a=b|nofilter %}x{% endblocktranslate %}",
}


@pytest.mark.parametrize("sii", BT_ENGINES)
@pytest.mark.parametrize("lang", LANGS + ["pl"])
@pytest.mark.parametrize("src", BLOCKTRANS_CASES.values(), ids=BLOCKTRANS_CASES.keys())
def test_blocktranslate(src, lang, sii):
    ctx = {
        "name": "Ann",
        "html": "<i>&</i>",
        "counts": [0, 1, 2, 5, 21, 112],
        "ctx": "abbrev. month",
        "pct": 50,
        "day": datetime.date(2024, 2, 29),
        "flt": 1.5,
        "dec": Decimal("1.0"),
        "aware": AWARE,
        "lazy": gettext_lazy("Yes"),
        "none": None,
    }
    engines = BT_ENGINES[sii]

    def run(engine):
        with translation.override(lang):
            return engine.from_string("{% load i18n %}" + src).render(Context(ctx))

    assert capture(lambda: run(engines["rust"])) == capture(lambda: run(engines["stock"]))


def test_blocktranslate_i18n_off():
    with override_settings(USE_I18N=False):
        for src in list(BLOCKTRANS_CASES.values())[:14]:
            assert_parity("{% load i18n %}" + src, {"name": "Ann", "counts": [1, 2, 3], "ctx": "x"}, lang="de")


STATIC_CASES = [
    "{% static 'css/site.css' %}",
    "{% static path %}|{% static path %}",
    "{% static 'a b/ü?x=1&y=2#f' %}",
    "{% static '/abs/x.js' %}|{% static '' %}|{% static '../up.css' %}",
    "{% static 'x.css' as u %}[{{ u }}]{% autoescape off %}{% static amp %}{% endautoescape %}",
    "{% static amp as u %}[{{ u }}]",
    "{% static amp %}",
    "{% static missing %}",
    "{% static num %}",
    "{% static %}",
    "{% static 'x' as %}",
    "{% static 'x' extra bits %}",
    "{% static 'x'|upper %}",
]


@pytest.mark.parametrize("installed", [True, False])
@pytest.mark.parametrize("static_url", ["/static/", "https://cdn.example.com/s/", "/st&tic/"])
@pytest.mark.parametrize("src", STATIC_CASES)
def test_static(src, static_url, installed):
    from django.contrib.staticfiles.storage import staticfiles_storage

    apps = ["django.contrib.staticfiles"] if installed else []
    ctx = {"path": "img/logo.png", "amp": "a&b.css", "num": 7}
    with override_settings(INSTALLED_APPS=apps, STATIC_URL=static_url):
        # `STATIC_URL` is read when the storage is set up.
        staticfiles_storage._wrapped = empty
        assert_parity("{% load static %}" + src, ctx)


def tz_filter_values():
    from django.templatetags.tz import datetimeobject

    aware = datetime.datetime(2024, 3, 10, 7, 30, 15, 123456, tzinfo=datetime.timezone.utc)
    forced = datetimeobject(2024, 7, 1, 23, 5, tzinfo=datetime.timezone.utc)
    forced.convert_to_local_time = True
    flagged = datetimeobject(2024, 7, 1, 23, 5, tzinfo=datetime.timezone.utc)
    flagged.convert_to_local_time = 0
    return [
        aware,
        aware.astimezone(zoneinfo.ZoneInfo("Asia/Kolkata")),
        datetime.datetime(2024, 11, 3, 1, 30),  # naive, DST fold in Chicago
        datetime.date(2024, 1, 2),
        # Constructed directly: no instance flag (converts), set True, falsy.
        datetimeobject(2024, 7, 1, 23, 5, tzinfo=datetime.timezone.utc),
        datetimeobject(2024, 7, 1, 23, 5),
        forced,
        flagged,
        "2024-01-01",
        None,
    ]


TZ_FILTER_CASES = {
    "localtime": "{{ v|localtime }}",
    "utc": "{{ v|utc }}",
    "timezone-name": "{{ v|timezone:'Europe/Paris' }}",
    "timezone-obj": "{{ v|timezone:tz }}",
    "timezone-bad": "[{{ v|timezone:'Nowhere/Nope' }}]",
    "localtime-off": "{% localtime off %}{{ v }}|{{ v|localtime }}{% endlocaltime %}",
    "date": "{{ v|timezone:'Asia/Tokyo'|date:'Y-m-d H:i:s.u e O T U c r' }}",
    "date-default": "{{ v|localtime|date }}|{{ v|utc|time }}|{{ v|utc|time:'H:i e' }}",
    "plain": "{{ v }}",
    "l10n-off": "{% localize off %}{{ v|utc }}{% endlocalize %}",
    "autoescape-off": "{% autoescape off %}{{ v|localtime }}{% endautoescape %}",
}


@pytest.mark.parametrize("use_tz", [True, False])
@pytest.mark.parametrize("lang", [None, "de", "ja"])
@pytest.mark.parametrize("src", TZ_FILTER_CASES.values(), ids=TZ_FILTER_CASES.keys())
def test_tz_filters(src, lang, use_tz):
    """`localtime`/`utc`/`timezone` return a `tz.datetimeobject`, which the
    native datetime paths treat like an exact datetime."""
    engines_libs = "{% load tz l10n %}"
    ctx_tz = zoneinfo.ZoneInfo("Australia/Adelaide")
    with override_settings(USE_TZ=use_tz):
        for v in tz_filter_values():
            for active in (None, "Pacific/Auckland"):
                assert_parity(engines_libs + src, {"v": v, "tz": ctx_tz}, lang=lang, tz=active)


def test_tz_datetimeobject_subclass_stays_python():
    """A further subclass of `datetimeobject` isn't the stock type."""
    from django.templatetags.tz import datetimeobject

    class Odd(datetimeobject):
        def __str__(self):
            return "odd"

    v = Odd(2024, 7, 1, 23, 5, tzinfo=datetime.timezone.utc)
    assert_parity("{% load tz %}{{ v }}|{{ v|date:'c' }}|{{ v|localtime }}", {"v": v}, lang="de")


ORDINAL_VALUES = [
    *range(0, 30), 100, 101, 102, 103, 104, 111, 112, 113, 114, 121, 1011, 2**62, 2**63, 10**30,
    -1, -11, -(2**63), True, False, "3", "x", 3.7, Decimal("12"), None, [],
]


@pytest.mark.parametrize("lang", [None, "de", "fr", "es", "ja", "nl", "ar"])
def test_ordinal(lang):
    src = (
        "{% load humanize %}{% for v in vals %}[{{ v|ordinal }}|{{ v|ordinal|length }}"
        "|{% autoescape off %}{{ v|ordinal }}{% endautoescape %}]{% endfor %}"
    )
    assert_parity(src, {"vals": ORDINAL_VALUES}, lang=lang)


def test_ordinal_i18n_off():
    with override_settings(USE_I18N=False):
        src = "{% load humanize %}{% for v in vals %}[{{ v|ordinal }}]{% endfor %}"
        assert_parity(src, {"vals": ORDINAL_VALUES}, lang="de")


@pytest.mark.parametrize("tmpl", ["<{}>", "{}{{x}}", "{0}.", "no placeholder", "{}-{}"])
def test_ordinal_patched_globals(tmpl):
    """Patched `pgettext` (any template shape) / `mark_safe` keep stock output."""
    from django.contrib.humanize.templatetags import humanize

    src = "{% load humanize %}{% for v in vals %}[{{ v|ordinal }}]{% endfor %}"
    orig_pg, orig_ms = humanize.pgettext, humanize.mark_safe
    humanize.pgettext = lambda ctx, msg: tmpl
    try:
        assert_parity(src, {"vals": [1, 2, 11, 23]}, lang="de")
        humanize.mark_safe = str
        assert_parity(src, {"vals": [1, 2, 11, 23]}, lang="de")
    finally:
        humanize.pgettext, humanize.mark_safe = orig_pg, orig_ms
