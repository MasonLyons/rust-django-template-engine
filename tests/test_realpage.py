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
from django.utils.safestring import mark_safe
from django.utils.translation import gettext_lazy, pgettext_lazy

from django_rust_templates import Engine as RustEngine
from django_rust_templates.forms import RustFormRenderer
from parity import capture

LIBS = {
    "i18n": "django.templatetags.i18n",
    "humanize": "django.contrib.humanize.templatetags.humanize",
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
