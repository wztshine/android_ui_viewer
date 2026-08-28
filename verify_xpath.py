#!/usr/bin/env python3
"""Verify that the XPath strings uiviewer generates are recognized by lxml.

uiautomator2's ``d.xpath()`` evaluates expressions with lxml (libxml2, full
XPath 1.0). This script mirrors uiviewer's XPath generator and checks that
every generated expression resolves to exactly the intended node, including
the tricky values: text with special whitespace (newlines/tabs) and quotes.

Run the built-in demo::

    uv run --with lxml python3 verify_xpath.py

Check real dumps / pasted XPath strings::

    uv run --with lxml python3 verify_xpath.py dump.xml [xpath...]
"""

from __future__ import annotations

import sys

import lxml.etree as etree


def xpath_literal(v: str) -> str:
    """Return `v` as an XPath 1.0 string literal, reproducing it verbatim.

    XPath 1.0 literals have no escape sequence, so the delimiter is picked
    around the value's own quotes: ``'...'`` unless the value contains a single
    quote, then ``"..."`` unless it also contains a double quote, then
    ``concat()`` stitches the value back together.
    """
    if "'" not in v:
        return f"'{v}'"
    if '"' not in v:
        return f'"{v}"'
    args = []
    for i, part in enumerate(v.split("'")):
        if i > 0:
            args.append("\"'\"")
        if part:
            args.append(f"'{part}'")
    return f"concat({', '.join(args)})"


def node_attr(el: etree._Element, key: str) -> str | None:
    """Return the `key` attribute value, skipping empty/'null' placeholders."""
    v = el.get(key)
    return v if v and v != "null" else None


def build_xpath(preds: list[tuple[str, str]]) -> str:
    """Render ordered (attr, value) predicates into an XPath expression."""
    return "//*" + "".join(f"[@{k}={xpath_literal(v)}]" for k, v in preds)


def count_matches(root: etree._Element, preds: list[tuple[str, str]]) -> int:
    """Count `<node>` elements matching every predicate, capped at 2."""
    count = 0
    for el in root.iter("node"):
        if all(el.get(k) == v for k, v in preds):
            count += 1
            if count >= 2:
                break
    return count


def generate_xpath(el: etree._Element, root: etree._Element) -> str | None:
    """Mirror uiviewer's generate_xpath: the first unique candidate wins."""
    t = node_attr(el, "text")
    r = node_attr(el, "resource-id")
    d = node_attr(el, "content-desc")
    c = node_attr(el, "class")
    state = [(k, node_attr(el, k)) for k in ("checked", "selected") if node_attr(el, k)]
    base: list[list[tuple[str, str]]] = []
    if t:
        base.append([("text", t)])
    if r:
        base.append([("resource-id", r)])
    if d:
        base.append([("content-desc", d)])
    if r and t:
        base.append([("resource-id", r), ("text", t)])
    if c and t:
        base.append([("class", c), ("text", t)])
    if c and r:
        base.append([("class", c), ("resource-id", r)])
    if r and d:
        base.append([("resource-id", r), ("content-desc", d)])
    if c and r and d:
        base.append([("class", c), ("resource-id", r), ("content-desc", d)])
    for preds in base:
        if count_matches(root, preds) == 1:
            return build_xpath(preds)
    if state:
        for preds in base:
            extended = preds + state
            if count_matches(root, extended) == 1:
                return build_xpath(extended)
        if c:
            candidate = [("class", c)] + state
            if count_matches(root, candidate) == 1:
                return build_xpath(candidate)
    return None


def verify_one(root: etree._Element, xpath: str, expected_rid: str | None) -> bool:
    """Parse `xpath` strictly, evaluate it, report and return success.

    Parsing is the user-visible guarantee: the string copied from the GUI must
    compile in the engine (lxml / uiautomator2), then resolve to exactly the
    intended node.
    """
    try:
        compiled = etree.XPath(xpath)
    except etree.XPathError as e:
        print(f"PARSE-FAIL  {xpath!r}  ->  {e}")
        return False
    nodes = compiled(root)
    unique = len(nodes) == 1
    rid_ok = expected_rid is None or (nodes and nodes[0].get("resource-id") == expected_rid)
    ok = unique and rid_ok
    detail = f" rid={nodes[0].get('resource-id')}" if nodes else ""
    print(f"{'PASS' if ok else 'FAIL'}  {xpath!r}  ->  {len(nodes)} match(es){detail}")
    return ok


def run_demo() -> int:
    """Verify every generated XPath against a dump with tricky text values."""
    demo = """<hierarchy>
  <node text="设置" resource-id="com.x:id/title" class="android.widget.TextView" bounds="[0,0][100,50]"/>
  <node text="第1行&#10;第2行" resource-id="com.x:id/multi" class="android.widget.TextView" bounds="[0,60][100,110]"/>
  <node text="tab&#9;here" resource-id="com.x:id/tabbed" class="android.widget.TextView" bounds="[0,120][100,170]"/>
  <node text="cr&#13;here" resource-id="com.x:id/crlf" class="android.widget.TextView" bounds="[0,180][100,230]"/>
  <node text="Don't" resource-id="com.x:id/squote" class="android.widget.TextView" bounds="[0,240][100,290]"/>
  <node text="He said &quot;hi&quot;" resource-id="com.x:id/dquote" class="android.widget.TextView" bounds="[0,300][100,350]"/>
  <node text="He said &quot;Don't&quot;" resource-id="com.x:id/both" class="android.widget.TextView" bounds="[0,360][100,410]"/>
  <node text="设置" resource-id="com.x:id/dup" class="android.widget.TextView" bounds="[0,420][100,470]"/>
</hierarchy>"""
    root = etree.fromstring(demo.encode("utf-8"))
    # (resource-id, the exact XPath string uiviewer must emit)
    checks = [
        # "设置" appears twice (title + dup), so title's text is non-unique and
        # the generator correctly falls back to the unique resource-id.
        ("com.x:id/title", "//*[@resource-id='com.x:id/title']"),
        ("com.x:id/multi", "//*[@text='第1行\n第2行']"),
        ("com.x:id/tabbed", "//*[@text='tab\there']"),
        ("com.x:id/crlf", "//*[@text='cr\rhere']"),
        ("com.x:id/squote", "//*[@text=\"Don't\"]"),
        ("com.x:id/dquote", "//*[@text='He said \"hi\"']"),
        ("com.x:id/both", "//*[@text=concat('He said \"Don', \"'\", 't\"')]"),
    ]
    ok = True
    for rid, expected in checks:
        el = root.xpath(f"//*[@resource-id='{rid}']")[0]
        got = generate_xpath(el, root)
        if got != expected:
            ok = False
            print(f"FAIL  expected {expected!r}, generated {got!r}")
        ok &= verify_one(root, got, rid)
    # Non-unique duplicate text must fall back to its resource-id.
    dup = root.xpath("//*[@resource-id='com.x:id/dup']")[0]
    got = generate_xpath(dup, root)
    ok &= got == "//*[@resource-id='com.x:id/dup']"
    ok &= verify_one(root, got, "com.x:id/dup")
    # A dump that serializes whitespace literally (not via entities) still
    # works: lxml and uiviewer (roxmltree) both apply XML attribute-value
    # normalization, so the generated XPath uses the same collapsed value.
    literal = (
        b"<hierarchy><node text=\"line1\nline2\" resource-id=\"com.x:id/raw\""
        b' class="android.widget.TextView" /></hierarchy>'
    )
    root2 = etree.fromstring(literal)
    el2 = root2.xpath("//*[@resource-id='com.x:id/raw']")[0]
    got2 = generate_xpath(el2, root2)
    ok &= got2 == "//*[@text='line1 line2']"
    ok &= verify_one(root2, got2, "com.x:id/raw")
    return 0 if ok else 1


def main(argv: list[str]) -> int:
    """Entry point: demo by default, or verify XPath(s) against a dump file."""
    if len(argv) < 2:
        return run_demo()
    root = etree.parse(argv[1]).getroot()
    xpaths = argv[2:]
    if not xpaths:
        print(f"{argv[1]}: no XPath given; showing the generated XPath per node")
        for el in root.iter("node"):
            rid = el.get("resource-id") or "-"
            print(f"rid={rid:<40} xpath={generate_xpath(el, root)!r}")
        return 0
    ok = True
    for xpath in xpaths:
        ok &= verify_one(root, xpath, None)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))