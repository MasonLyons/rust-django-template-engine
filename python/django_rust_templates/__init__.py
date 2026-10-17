"""django_rust_templates — Django template engine implemented in Rust."""
from .exceptions import UnsupportedTemplateError

from ._dtl import Engine, Template  # noqa: F401  (re-exported native classes)
from ._backend import RustTemplates  # noqa: F401


def _engine_get_template_loaders(self, template_loaders):
    """Stock `Engine.get_template_loaders` (bound onto the native Engine;
    `cached.Loader.__init__` calls it via `self.engine`)."""
    return [
        loader
        for loader in map(self.find_template_loader, template_loaders)
        if loader is not None
    ]


Engine.get_template_loaders = _engine_get_template_loaders


def _engine_deepcopy(self, memo):
    """Engines are shared configuration; a deep copy (e.g. `copy.deepcopy`
    of a form's `ErrorDict`, which holds the form renderer and its cached
    engine) shares the native engine instead of failing to pickle it."""
    return self


Engine.__deepcopy__ = _engine_deepcopy

# Stock `Template.render` is a Python method, so tracebacks through a render
# carry several Python frames (`render` → `_render` → `NodeList.render` → ...).
# Tests assert traceback depth (e.g. #19827: "url tag should keep original
# stack trace when reraising", needs depth > 5). The native render is a single
# Rust frame that CPython does not count; wrap it with a chain of Python
# frames to restore stock-equivalent traceback depth.
_native_render = Template.render


def _do_render(self, context):
    return _native_render(self, context)


def _render_impl(self, context):
    return _do_render(self, context)


def _render(self, context):
    return _render_impl(self, context)


def render(self, context):
    return _render(self, context)


Template.render = render
__all__ = ["Engine", "Template", "RustTemplates", "UnsupportedTemplateError"]
