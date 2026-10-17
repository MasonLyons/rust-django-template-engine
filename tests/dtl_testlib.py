"""Template tag library used by the parity tests (registered as ``testlib``)."""

from django import template
from django.template import Template

register = template.Library()


@register.simple_tag(takes_context=True)
def ctx_tag(context, prefix="p"):
    return "%s:%s" % (prefix, context.get("who", "?"))


@register.simple_tag
def add(a, b=1):
    return a + b


@register.inclusion_tag("chip.html")
def chip(name):
    return {"name": name}


@register.inclusion_tag(Template("<b>{{ name }}</b>"))
def chip_t(name):
    return {"name": name}
