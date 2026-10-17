"""End-to-end page render through the Django template backend.

``bench.py`` times isolated templates. This renders one realistic list page
the way a view does: ``backend.get_template(...).render(context, request)``
with context processors (request, auth, messages, i18n, static, csrf),
``{% extends %}``/``{% include %}`` through the cached loader, ``{% static %}``,
``{% url %}``, ``{% translate %}``/``{% blocktranslate %}``, ``{% csrf_token %}``,
a lazy ``request.user``, flashed messages, a search form, and model-like
objects run through ``date``, ``naturaltime``, ``striptags``, ``truncatewords``,
``intcomma`` and ``pluralize``.

    maturin develop --release
    python benchmarks/realworld.py                # both engines
    python benchmarks/realworld.py --stock-forms  # Rust engine, stock FORM_RENDERER

Each engine runs in its own subprocess (pinned to one core); the parent
checks that both produce identical HTML (ignoring the random CSRF token)
before printing timings. Times include building the view context (a
paginator, the form, two messages), which costs the same on both sides.
"""

from __future__ import annotations

import argparse
import datetime
import gc
import json
import os
import re
import subprocess
import sys
import timeit

TEMPLATES = {
    "base.html": """{% load static i18n %}<!doctype html>
<html lang="{{ LANGUAGE_CODE|default:'en' }}"><head><meta charset="utf-8">
<title>{% block title %}{% translate "My Site" %}{% endblock %}</title>
<link rel="stylesheet" href="{% static 'css/site.css' %}">
<script src="{% static 'js/app.js' %}" defer></script>
{% block extra_head %}{% endblock %}</head>
<body class="{% block body_class %}{% endblock %}">
{% include "partials/nav.html" %}
{% if messages %}<ul class="messages">{% for message in messages %}<li{% if message.tags %} class="{{ message.tags }}"{% endif %}>{{ message }}</li>{% endfor %}</ul>{% endif %}
<main>{% block content %}{% endblock %}</main>
{% include "partials/footer.html" %}
</body></html>""",
    "partials/nav.html": """{% load i18n %}<nav><a href="{% url 'home' %}">{% translate "Home" %}</a>
{% for item in nav_items %}<a href="{% url item.view %}"{% if request.path == item.path %} class="active"{% endif %}>{{ item.label }}</a>{% endfor %}
{% if user.is_authenticated %}<span>{% blocktranslate with name=user.get_username %}Signed in as {{ name }}{% endblocktranslate %}</span>
{% if perms.auth.add_user %}<a href="/admin/">admin</a>{% endif %}
<form method="post" action="{% url 'logout' %}">{% csrf_token %}<button>{% translate "Log out" %}</button></form>
{% else %}<a href="{% url 'login' %}?next={{ request.path|urlencode }}">{% translate "Log in" %}</a>{% endif %}</nav>""",
    "partials/footer.html": """{% load i18n %}<footer>&copy; {% now "Y" %} {{ site_name }} &middot; {% translate "All rights reserved." %}</footer>""",
    "partials/article_card.html": """{% load humanize %}<article class="card{% if article.featured %} featured{% endif %}">
<h2><a href="{{ article.get_absolute_url }}">{{ article.title }}</a></h2>
<p class="meta">{{ article.author.get_full_name|default:article.author.username }} &middot; <time datetime="{{ article.published|date:'c' }}">{{ article.published|date:"N j, Y" }}</time> &middot; {{ article.published|naturaltime }}</p>
<p>{{ article.body|striptags|truncatewords:30 }}</p>
<p>{{ article.views|intcomma }} view{{ article.views|pluralize }} &middot; {{ article.comment_count }} comment{{ article.comment_count|pluralize }}</p>
<ul class="tags">{% for tag in article.tag_list %}<li><a href="{% url 'tag' tag.slug %}">{{ tag.name }}</a></li>{% empty %}<li>untagged</li>{% endfor %}</ul>
</article>""",
    "list.html": """{% extends "base.html" %}{% load i18n %}
{% block title %}{% translate "Articles" %} | {{ block.super }}{% endblock %}
{% block body_class %}list{% endblock %}
{% block content %}
<h1>{{ page_title }}</h1>
<form method="get" action="">{{ search_form.as_div }}<button>{% translate "Search" %}</button></form>
{% for article in articles %}{% include "partials/article_card.html" %}{% empty %}<p>{% translate "No articles." %}</p>{% endfor %}
{% if is_paginated %}<nav class="pages">{% if page_obj.has_previous %}<a href="?page={{ page_obj.previous_page_number }}">&laquo;</a>{% endif %}
{% blocktranslate with n=page_obj.number total=page_obj.paginator.num_pages %}Page {{ n }} of {{ total }}{% endblocktranslate %}
{% if page_obj.has_next %}<a href="?page={{ page_obj.next_page_number }}">&raquo;</a>{% endif %}</nav>{% endif %}
{% endblock %}""",
}

BACKENDS = {
    "stock": "django.template.backends.django.DjangoTemplates",
    "dtl": "django_rust_templates.RustTemplates",
}


def _view(request, *args, **kwargs):  # URL targets only; never called
    raise NotImplementedError


def urlpatterns_():
    from django.urls import path

    return [
        path("", _view, name="home"),
        path("login/", _view, name="login"),
        path("logout/", _view, name="logout"),
        path("articles/", _view, name="articles"),
        path("about/", _view, name="about"),
        path("contact/", _view, name="contact"),
        path("tag/<slug:slug>/", _view, name="tag"),
        path("a/<int:pk>/", _view, name="article"),
    ]


def setup(engine):
    import django
    from django.conf import settings

    settings.configure(
        DEBUG=False,
        SECRET_KEY="bench",
        ROOT_URLCONF=__name__,
        USE_TZ=True,
        USE_I18N=True,
        LANGUAGE_CODE="en-us",
        INSTALLED_APPS=[
            "django.contrib.auth",
            "django.contrib.contenttypes",
            "django.contrib.sessions",
            "django.contrib.messages",
            "django.contrib.staticfiles",
            "django.contrib.humanize",
        ],
        STATIC_URL="/static/",
        DATABASES={},
        TEMPLATES=[
            {
                "BACKEND": BACKENDS[engine],
                "OPTIONS": {
                    "context_processors": [
                        "django.template.context_processors.debug",
                        "django.template.context_processors.request",
                        "django.contrib.auth.context_processors.auth",
                        "django.contrib.messages.context_processors.messages",
                        "django.template.context_processors.i18n",
                        "django.template.context_processors.static",
                        "django.template.context_processors.csrf",
                    ],
                    "loaders": [
                        (
                            "django.template.loaders.cached.Loader",
                            [("django.template.loaders.locmem.Loader", TEMPLATES)],
                        )
                    ],
                },
            }
        ],
    )
    django.setup()


urlpatterns: list = []


class Author:
    def __init__(self, i):
        self.username = f"user{i}"
        self.first = f"First{i}"
        self.last = f"Last{i}"

    def get_full_name(self):
        return f"{self.first} {self.last}".strip()


class Tag:
    def __init__(self, i):
        self.name = f"tag {i}"
        self.slug = f"tag-{i}"


class Article:
    def __init__(self, i, now):
        self.pk = i
        self.title = f"Article <{i}> & stuff"
        self.featured = i % 5 == 0
        self.author = Author(i % 7)
        self.published = now - datetime.timedelta(hours=i * 7)
        self.body = ("<p>Lorem ipsum dolor sit amet, consectetur adipiscing elit. " * 6) + "</p>"
        self.views = i * 1234
        self.comment_count = i % 3
        self.tag_list = [Tag(j) for j in range(i % 4)]

    def get_absolute_url(self):
        return f"/a/{self.pk}/"


def build(engine, stock_forms):
    from django import forms
    from django.contrib import messages
    from django.contrib.auth.models import User
    from django.contrib.messages.storage.fallback import FallbackStorage
    from django.contrib.sessions.backends.signed_cookies import SessionStore
    from django.core.paginator import Paginator
    from django.forms.renderers import DjangoTemplates
    from django.template import engines
    from django.test import RequestFactory
    from django.utils.functional import SimpleLazyObject

    if engine == "dtl" and not stock_forms:
        from django_rust_templates.forms import RustFormRenderer

        renderer = RustFormRenderer()
    else:
        renderer = DjangoTemplates()

    class SearchForm(forms.Form):
        q = forms.CharField(required=False, label="Search")
        category = forms.ChoiceField(
            choices=[("", "All"), ("a", "A"), ("b", "B")], required=False
        )

    # Shared by both children (set by the parent) so the HTML compares equal;
    # `naturaltime` still measures against the real clock.
    now = datetime.datetime.fromisoformat(os.environ["DTL_RW_NOW"])
    articles = [Article(i, now) for i in range(25)]
    user = User(id=1, username="mason", first_name="M", is_superuser=True, is_active=True)
    request = RequestFactory().get("/articles/", {"q": "x"})
    request.session = SessionStore()
    request.user = SimpleLazyObject(lambda: user)

    def context():
        request._messages = FallbackStorage(request)
        messages.info(request, "Welcome back!")
        messages.success(request, "Saved <ok>.")
        page = Paginator(articles, 10).page(2)
        return {
            "articles": page.object_list,
            "page_obj": page,
            "is_paginated": True,
            "page_title": "Latest articles",
            "search_form": SearchForm(request.GET, renderer=renderer),
            "site_name": "Example",
            "nav_items": [
                {"view": n, "path": f"/{n}/", "label": n.title()}
                for n in ("articles", "about", "contact")
            ],
        }

    template = engines.all()[0].get_template("list.html")
    return lambda: template.render(context(), request)


def run_child(engine, stock_forms):
    setup(engine)
    global urlpatterns
    urlpatterns = urlpatterns_()
    try:
        os.sched_setaffinity(0, {int(os.environ.get("DTL_BENCH_CORE", "0"))})
    except (AttributeError, OSError):
        pass
    fn = build(engine, stock_forms)
    html = fn()
    for _ in range(5):
        fn()
    one = timeit.timeit(fn, number=1)
    number = max(1, int(0.25 / one))
    gc.disable()
    best = min(timeit.repeat(fn, number=number, repeat=7)) / number * 1e6
    gc.enable()
    print(json.dumps({"engine": engine, "us": best, "html": html}))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--child")
    ap.add_argument("--stock-forms", action="store_true",
                    help="keep Django's default FORM_RENDERER on the Rust side")
    args = ap.parse_args()
    if args.child:
        run_child(args.child, args.stock_forms)
        return
    # Half past the current hour: hour-granular `naturaltime` output can't
    # flip between the two child runs.
    now = datetime.datetime.now(datetime.timezone.utc).replace(minute=0, second=0, microsecond=0)
    os.environ["DTL_RW_NOW"] = (now - datetime.timedelta(minutes=30)).isoformat()
    res = {}
    for engine in ("stock", "dtl"):
        cmd = [sys.executable, __file__, "--child", engine]
        if args.stock_forms:
            cmd.append("--stock-forms")
        out = subprocess.run(cmd, check=True, capture_output=True, text=True).stdout
        res[engine] = json.loads(out.strip().splitlines()[-1])

    def norm(s):
        return re.sub(r'name="csrfmiddlewaretoken" value="[^"]+"', "", s)

    if norm(res["stock"]["html"]) != norm(res["dtl"]["html"]):
        raise SystemExit("output differs between engines")
    s, d = res["stock"]["us"], res["dtl"]["us"]
    forms = "stock" if args.stock_forms else "RustFormRenderer"
    print(f"realworld page (forms: {forms}): stock {s:.1f} us, rust {d:.1f} us, {s / d:.2f}x")


if __name__ == "__main__":
    main()
