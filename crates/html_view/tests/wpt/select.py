"""Select WPT reftests Maze's original view can run and vendor them.

Usage: python3 -I wpt_select.py <wpt checkout> <out dir> [dir ...]

A test qualifies when it and its references are HTML (or XHTML that parses the same
as HTML: no self-closing non-void tags, no CDATA) that Maze
can render the way it renders mail: no script (also no `reftest-wait`), no Ahem or
other web fonts, no external style sheets, no form controls, frames, SVG, canvas or
media, no vertical writing modes and no printing. Local images are fine (the runner
inlines them as data: URLs). Prints counts and what was skipped why; with an out dir,
copies the tests, references and images, keeping WPT's paths.
"""

import collections
import os
import re
import shutil
import sys

root = os.path.abspath(sys.argv[1])
out = sys.argv[2] if len(sys.argv) > 2 and sys.argv[2] != "-" else None
dirs = sys.argv[3:]

LINK = re.compile(r"<link\b[^>]*>", re.I)
REL = re.compile(r"""\brel\s*=\s*["']?(match|mismatch)""", re.I)
HREF = re.compile(r"""\bhref\s*=\s*["']?([^"'\s>]+)""", re.I)
BLOCKERS = [
    ("script", re.compile(r"<script\b|reftest-wait|\bon[a-z]+\s*=", re.I)),
    ("ahem/webfont", re.compile(r"ahem|@font-face|/fonts/", re.I)),
    ("stylesheet", re.compile(r"""<link\b[^>]*rel\s*=\s*["']?stylesheet|@import""", re.I)),
    ("form/frame/media", re.compile(r"<(input|select|button|textarea|form|iframe|frame|object|embed|svg|canvas|video|audio|math)\b", re.I)),
    ("writing-mode", re.compile(r"writing-mode|text-orientation|direction\s*:\s*rtl|dir\s*=\s*[\"']?rtl", re.I)),
    ("print", re.compile(r"@page|@media\s+print|reftest-paged", re.I)),
    ("xml-only", re.compile(r"<!\[CDATA\[|<(?!(?:area|base|br|col|embed|hr|img|input|link|meta|source|track|wbr)\b)[a-z][a-z0-9]*\b[^>]*/>", re.I)),
]
HTML = (".html", ".htm", ".xht", ".xhtml")
IMAGE = re.compile(r"""(?:src\s*=\s*["']?|url\(\s*["']?)([^"')\s>]+\.(?:png|gif|jpe?g))""", re.I)


def read(path):
    with open(path, encoding="utf-8", errors="replace") as f:
        return f.read()


def resolve(base_file, href):
    href = href.split("#")[0].split("?")[0]
    if href.startswith("/"):
        return os.path.normpath(os.path.join(root, href.lstrip("/")))
    return os.path.normpath(os.path.join(os.path.dirname(base_file), href))


def blocker(text):
    for name, pattern in BLOCKERS:
        if pattern.search(text):
            return name
    return None


selected = []  # (test, [(kind, ref)], images)
skipped = collections.Counter()
missing = collections.Counter()
for d in dirs:
    for dirpath, _, files in os.walk(os.path.join(root, d)):
        for name in sorted(files):
            if not name.endswith(HTML) or "-ref" in name or "/reference" in dirpath:
                continue
            path = os.path.join(dirpath, name)
            text = read(path)
            refs = []
            for link in LINK.findall(text):
                rel, href = REL.search(link), HREF.search(link)
                if rel and href:
                    refs.append((rel.group(1).lower(), resolve(path, href.group(1))))
            if not refs:
                skipped["not a reftest"] += 1
                continue
            reason = blocker(text)
            files_needed = [path]
            for _, ref in refs:
                if not os.path.exists(ref):
                    reason = reason or "reference missing"
                    missing[os.path.relpath(os.path.dirname(ref), root)] += 1
                    continue
                if not ref.endswith(HTML):
                    reason = reason or "non-HTML reference"
                    continue
                reason = reason or blocker(read(ref))
                files_needed.append(ref)
            images = []
            for f in files_needed:
                for img in IMAGE.findall(read(f)):
                    if re.match(r"^[a-z]+:", img):
                        reason = reason or "remote image"
                        continue
                    p = resolve(f, img)
                    if os.path.exists(p):
                        images.append(p)
                    else:
                        reason = reason or "image missing"
            if reason:
                skipped[reason] += 1
                continue
            selected.append((path, refs, images))

print(f"selected {len(selected)} reftests")
for reason, n in skipped.most_common():
    print(f"  skipped {n:5}  {reason}")
for d, n in missing.most_common(10):
    print(f"  missing refs in {d}: {n}")
per_dir = collections.Counter(os.path.relpath(os.path.dirname(t), root) for t, _, _ in selected)
for d, n in sorted(per_dir.items()):
    print(f"  {n:4}  {d}")

if out:
    copied = set()
    for test, refs, images in selected:
        for f in [test, *(r for _, r in refs), *images]:
            if f in copied:
                continue
            copied.add(f)
            dest = os.path.join(out, os.path.relpath(f, root))
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            shutil.copyfile(f, dest)
    shutil.copyfile(os.path.join(root, "LICENSE.md"), os.path.join(out, "LICENSE.md"))
    print(f"copied {len(copied)} files to {out}")
