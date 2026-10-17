"""Python duck-types for Rust-compiled nodes.

The Rust engine renders natively without these; they exist so that
``template.nodelist`` exposes Django's node protocol (``get_nodes_by_type``,
``child_nodelists``, ``token``/``origin``, ``render_annotated``) to code that
walks template trees — e.g. ``ExtendsNode``'s parent scan and the suite's
``test_nodelist`` tests. Each adapter subclasses the stock node class of the
same name so ``isinstance`` and ``get_nodes_by_type``'s exact-type comparison
match Django's types; nodes whose stock ``render`` needs parse-time state
(variables, autoescape toggling) delegate back into the Rust renderer via
``Template._render_native(pos, context)``, where the Rust side assigns
``_render_id`` (pre-order position among native nodes) when it builds the
adapter list.
"""
from django.template.base import Node
from django.template.base import NodeList as _NodeList
from django.template.base import TextNode as _TextNode
from django.template.base import VariableNode as _VariableNode
from django.template.defaulttags import AutoEscapeControlNode as _AutoEscapeControlNode
from django.template.defaulttags import CommentNode as _CommentNode
from django.template.defaulttags import VerbatimNode as _VerbatimNode
from django.template.library import InclusionNode as _stock_InclusionNode
from django.template.library import SimpleNode as _stock_SimpleNode
from django.template.loader_tags import BlockNode as _stock_BlockNode
from django.template.loader_tags import ExtendsNode as _stock_ExtendsNode
import warnings

try:
    from django.utils.deprecation import django_file_prefixes
except ImportError:  # Django < 6.1
    def django_file_prefixes():
        return ()


class NodeListAdapter(_NodeList):
    """``NodeList`` with Rust-node support (identical protocol otherwise)."""


class _RustNode(Node):
    # Assigned by the Rust builder when the nodelist view is created.
    _render_id = -1

    def __init__(self, template):
        self._template = template

    def render_annotated(self, context):
        return self._template._render_native(self._render_id, context)


class TextNode(_RustNode, _TextNode):
    child_nodelists = ()

    def __init__(self, template, s):
        _RustNode.__init__(self, template)
        self.s = s

    def __repr__(self):
        return "<%s: %r>" % (self.__class__.__name__, self.s[:25])

    def render(self, context):
        return self.s


class VariableNode(_RustNode, _VariableNode):
    child_nodelists = ()

    def __init__(self, template):
        # Django's VariableNode carries its FilterExpression; the real
        # resolution happens natively in _render_native.
        self.filter_expression = None
        self._template = template

    def render(self, context):
        return self.render_annotated(context)


class AutoEscapeControlNode(_RustNode, _AutoEscapeControlNode):
    def __init__(self, template, setting, nodelist):
        _RustNode.__init__(self, template)
        self.setting = setting
        self.nodelist = nodelist

    def render(self, context):
        # Native render owns the autoescape toggle for the subtree.
        return self.render_annotated(context)


class CommentNode(_RustNode, _CommentNode):
    child_nodelists = ()

    def render(self, context):
        return ""


class VerbatimNode(_RustNode, _VerbatimNode):
    child_nodelists = ()

    def __init__(self, template, content):
        _RustNode.__init__(self, template)
        self.content = content

    def render(self, context):
        return self.content


class NativeTagNode(_RustNode, Node):
    """Generic native-tag adapter: repr names the tag, render delegates to
    the Rust renderer. Child nodelists/attributes are set by the Rust
    builder (stock attr names per tag)."""

    def __init__(self, template, kind_repr=None):
        _RustNode.__init__(self, template)
        self._kind_repr = kind_repr

    def __repr__(self):
        return self._kind_repr or "<%s>" % self.__class__.__name__

    def render(self, context):
        return self.render_annotated(context)


class SimpleNode(_RustNode, _stock_SimpleNode):
    """Native ``simple_tag`` node adapter (stock attrs except the parsed
    ``args``/``kwargs``, which live on the Rust node)."""

    def __init__(self, template, func, takes_context, target_var):
        _RustNode.__init__(self, template)
        self.func = func
        self.takes_context = takes_context
        self.target_var = target_var

    def render(self, context):
        return self.render_annotated(context)


class InclusionNode(_RustNode, _stock_InclusionNode):
    """Native ``inclusion_tag`` node adapter (see ``SimpleNode``)."""

    def __init__(self, template, func, takes_context, filename):
        _RustNode.__init__(self, template)
        self.func = func
        self.takes_context = takes_context
        self.filename = filename

    def render(self, context):
        return self.render_annotated(context)


class BlockNode(_RustNode, _stock_BlockNode):
    """Native `{% block %}` adapter: duck-type of the stock BlockNode
    (`name`, `nodelist`) so ExtendsNode's `blocks_by_name` scan and
    `clone_block` (loader.rs) work unchanged."""

    def __init__(self, template, name, nodelist):
        _RustNode.__init__(self, template)
        self.name = name
        self.nodelist = nodelist

    def __repr__(self):
        return "<Block: %s>" % self.name

    def render(self, context):
        # Block rendering goes through ExtendsNode/BlockContext, which
        # clone these adapters; a block rendered directly (no extends)
        # renders its own body natively.
        return self.render_annotated(context)


class ExtendsNode(_RustNode, _stock_ExtendsNode):
    """Native `{% extends %}` adapter. The stock ctor is NOT called (it
    needs a parsed parent name); the Rust renderer owns the extends
    machinery, this adapter only satisfies tree-walking (`nodelist`,
    `must_be_first`, `_dtl_extends` marker)."""

    def __init__(self, template, nodelist):
        _RustNode.__init__(self, template)
        self.nodelist = nodelist

    def render(self, context):
        return self.render_annotated(context)


try:  # Django >= 6.1
    from django.template.base import PartialTemplate as _stock_PartialTemplate
except ImportError:  # Django < 6.1
    _stock_PartialTemplate = object


class PartialTemplate(_stock_PartialTemplate):
    """Duck-type of the stock 6.1 ``PartialTemplate`` built by native
    ``{% partialdef %}`` (subclass so ``isinstance`` checks match).
    ``_template`` is backfilled on the dtl Template at first render; the
    body is rendered by Rust via the def's node id."""

    def __init__(self, name, origin, source_start, source_end, body_def_id):
        self.name = name
        self.origin = origin
        self._source_start = source_start
        self._source_end = source_end
        self._body_def_id = body_def_id
        self._template = None

    class _ProxyNodeList:
        """Minimal ``NodeList`` stand-in: stock ``PartialTemplate._render``
        (patched by ``setup_test_environment`` into the instrumented
        renderer) calls ``self.nodelist.render(context)``; route that to
        the Rust body renderer so the ``template_rendered`` signal fires
        exactly like stock. Defined lazily per access; no state."""

        __slots__ = ("_proxy",)

        def __init__(self, proxy):
            self._proxy = proxy

        def render(self, context):
            return self._proxy._render_body(context)

    @property
    def nodelist(self):
        return self._ProxyNodeList(self)

    def _render_body(self, context):
        return self._template._render_partial_body(self._body_def_id, context)

    if _stock_PartialTemplate is object:  # Django < 6.1: replicate stock render

        def _render(self, context):
            return self._render_body(context)

        def render(self, context):
            with context.render_context.push_state(self):
                if context.template is None:
                    with context.bind_template(self):
                        context.template_name = self.name
                        return self._render(context)
                return self._render(context)


    def get_exception_info(self, exception, token):
        template = self.origin.loader.get_template(self.origin.template_name)
        return template.get_exception_info(exception, token)

    def find_partial_source(self, full_source):
        if (
            self._source_start is not None
            and self._source_end is not None
            and 0 <= self._source_start <= self._source_end <= len(full_source)
        ):
            return full_source[self._source_start:self._source_end]
        return ""

    @property
    def source(self):
        """Display the source of this template (the partial body)."""
        template = self.origin.loader.get_template(self.origin.template_name)
        if not template.engine.debug:
            warnings.warn(
                "PartialTemplate.source is only available when template "
                "debugging is enabled.",
                RuntimeWarning,
                skip_file_prefixes=django_file_prefixes(),
            )
        return self.find_partial_source(template.source)
