"""Exceptions raised by django_rust_templates."""
from django.template.exceptions import TemplateSyntaxError


class UnsupportedTemplateError(TemplateSyntaxError):
    """The template uses a construct the native engine cannot execute exactly."""
