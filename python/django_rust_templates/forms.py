"""Form renderer on the native Rust engine.

Django's default ``FORM_RENDERER`` (``django.forms.renderers.DjangoTemplates``)
builds its own *stock* template engine for the built-in form and widget
templates, so every ``{{ form }}`` / ``{{ field }}`` renders through stock
Django even when ``TEMPLATES`` uses ``RustTemplates``. Point
``FORM_RENDERER`` here to render them natively::

    FORM_RENDERER = "django_rust_templates.forms.RustFormRenderer"
"""
from __future__ import annotations

from django.forms.renderers import BaseRenderer, EngineMixin

from ._backend import RustTemplates


class RustFormRenderer(EngineMixin, BaseRenderer):
    """``django.forms.renderers.DjangoTemplates`` with the native backend:
    loads templates from the built-in form templates directory, followed by
    each installed app's ``templates`` directory."""

    backend = RustTemplates
