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


# --- Raw block tags: custom compile functions calling parser.parse() -----


class _WrapNode(template.Node):
    def __init__(self, nodelist, fmt="[%s]"):
        self.nodelist = nodelist
        self.fmt = fmt

    def render(self, context):
        return self.fmt % self.nodelist.render(context)


@register.tag
def wrap(parser, token):
    nodelist = parser.parse(("endwrap",))
    parser.delete_first_token()
    return _WrapNode(nodelist)


class _ElseNode(template.Node):
    child_nodelists = ("nodelist_true", "nodelist_false")

    def __init__(self, var, nodelist_true, nodelist_false):
        self.var = var
        self.nodelist_true = nodelist_true
        self.nodelist_false = nodelist_false

    def render(self, context):
        if self.var.resolve(context):
            return self.nodelist_true.render(context)
        return self.nodelist_false.render(context)


@register.tag
def ifset(parser, token):
    var = parser.compile_filter(token.split_contents()[1])
    nodelist_true = parser.parse(["otherwise", "endifset"])
    token = parser.next_token()
    if token.contents == "otherwise":
        nodelist_false = parser.parse(["endifset"])
        parser.delete_first_token()
    else:
        nodelist_false = template.NodeList()
    return _ElseNode(var, nodelist_true, nodelist_false)


class _CountNode(template.Node):
    def __init__(self, nodelist, desc):
        self.nodelist = nodelist
        self.desc = desc

    def render(self, context):
        return "%s|%s" % (self.desc, self.nodelist.render(context))


@register.tag
def inspect_body(parser, token):
    """Introspects the parsed body like tree-walking tags do."""
    nodelist = parser.parse(("endinspect_body",))
    parser.delete_first_token()
    from django.template.base import TextNode, VariableNode

    desc = "%d:%d:%d:%s" % (
        len(nodelist),
        len(nodelist.get_nodes_by_type(VariableNode)),
        len(nodelist.get_nodes_by_type(TextNode)),
        nodelist.contains_nontext,
    )
    return _CountNode(nodelist, desc)


@register.tag
def drop_first(parser, token):
    """Edits the parsed body before rendering it."""
    nodelist = parser.parse(("enddrop_first",))
    parser.delete_first_token()
    if nodelist:
        nodelist.pop(0)
    return _WrapNode(nodelist, "<%s>")


@register.tag
def until_str(parser, token):
    """Stock `in` semantics on a str parse_until (substring match)."""
    nodelist = parser.parse("endstr")
    parser.delete_first_token()
    return _WrapNode(nodelist, "{%s}")


@register.tag
def raw_until(parser, token):
    """Consumes tokens itself, then parses the rest of the body."""
    texts = []
    while True:
        t = parser.next_token()
        if t.contents == "mid":
            break
        texts.append(t.contents)
    nodelist = parser.parse(("endraw_until",))
    parser.delete_first_token()
    return _WrapNode(nodelist, "(" + "".join(texts) + ")%s")


@register.inclusion_tag(Template("{{ name }}{% csrf_token %}"))
def chip_csrf(name):
    return {"name": name}


SHARED_CHIP = {"name": "shared"}


@register.inclusion_tag(Template("{{ name }}|{{ csrf_token }}"))
def chip_shared():
    # Stock `InclusionNode` writes `csrf_token` into the dict it returns.
    return SHARED_CHIP
