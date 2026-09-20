#!/usr/bin/env python3
"""Splice the site's shared parts — the nav and the footer — into every page.

They used to be copy-pasted into each page, which is how the nav came to sit in
two different formattings, how `index` grew a light-theme rule the other pages
never got, and why changing a word in the footer meant editing four files and
hoping. Each part is now written once under `scripts/` and this puts it where it
belongs. Run it through `make partials`.

A part's markup and its styles land in different places, so a page carries a
pair of markers for each: `<!-- nav:start -->` … `<!-- nav:end -->` around the
element, and `/* nav:start */` … `/* nav:end */` inside the page's own <style>.

`{{version}}` in a part's markup is filled in from `Cargo.toml`, so the version
in the footer follows the crate rather than being a number somebody has to
remember to retype on the way out of a release.
"""

import pathlib
import re
import sys

# `_of.html` is deliberately absent: nothing links to it and its nav still
# carries the GitHub icon the top bar dropped, so it is not this component.
PAGES = [
    "index.html",
    "alerts.html",
    "audit.html",
    "mcp.html",
    "serve.html",
    "setup-ga4.html",
    "pricing.html",
    "lovable.html",
    "burn.html",
    "success.html",
    "privacy.html",
    "terms.html",
]

# The page served at the site root. Its in-page anchors are written bare, so a
# click is a scroll rather than a navigation back to the page you are on.
ROOT_PAGE = "index.html"

# name -> whether the part also owns a block of CSS.
PARTS = {"nav": True, "footer": True}

root = pathlib.Path(__file__).resolve().parent.parent
docs = root / "docs"


def splice(text, start, end, body, page, indent=""):
    """Replace everything between two markers, keeping the markers.

    Any indentation already sitting in front of the closing marker is absorbed,
    so re-running cannot drift it a column further out each time.
    """
    pattern = re.compile(re.escape(start) + r".*?" + r"[ \t]*" + re.escape(end), re.S)
    if not pattern.search(text):
        sys.exit(f"{page}: no {start} … {end} markers — add them once by hand")
    return pattern.sub(lambda _: f"{start}\n{body}\n{indent}{end}", text, count=1)


def source(name, suffix):
    path = root / "scripts" / f"{name}.{suffix}"
    if not path.exists():
        sys.exit(f"missing {path.relative_to(root)}")
    return path.read_text().strip("\n")


def version():
    """The crate's version, from the first `version =` under `[package]`.

    Read rather than asked of cargo: this script runs on a machine that may not
    have a toolchain, and the answer is one line of TOML.
    """
    text = (root / "Cargo.toml").read_text()
    package = text.split("[package]", 1)[-1].split("\n[", 1)[0]
    found = re.search(r'^version\s*=\s*"([^"]+)"', package, re.M)
    if not found:
        sys.exit("no version under [package] in Cargo.toml")
    return found.group(1)


crate = version()
changed = 0
for name in PAGES:
    page = docs / name
    before = after = page.read_text()

    for part, has_css in PARTS.items():
        markup = source(part, "html").strip().replace("{{version}}", crate)
        if name == ROOT_PAGE:
            markup = markup.replace('href="/#', 'href="#')

        after = splice(
            after, f"<!-- {part}:start -->", f"<!-- {part}:end -->", markup, name
        )
        if has_css:
            after = splice(
                after,
                f"/* {part}:start */",
                f"/* {part}:end */",
                source(part, "css"),
                name,
                indent="  ",
            )

    if after != before:
        page.write_text(after)
        changed += 1
    print(f"  {name}: {'updated' if after != before else 'already current'}")

print(f"\n{'/'.join(PARTS)} spliced into {len(PAGES)} pages at v{crate}, {changed} changed")
