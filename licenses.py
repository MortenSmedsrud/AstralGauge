#!/usr/bin/env python3
"""Renders `licenses.txt` from `cargo about generate --format json` output.

Usage (invoked by licenses.sh):
    cargo about generate --format json --locked > about.json
    python3 licenses.py about.json > licenses.txt

cargo-about groups every dependency by its *license file text*: crates
whose LICENSE files are identical share one entry. The rendered document
keeps one full license text per license kind, but every "Used by" line
carries the copyright notice of its group's license file (falling back
to the crate's `authors` metadata). The notice-retention clauses of the
permissive licenses (MIT, BSD, ISC, Apache-2.0, Boost, ...) require the
copyright notices of the bundled software to be distributed with it, so
the file must list them, not only the license texts.

The static "Notes" section at the end carries facts the pipeline cannot
derive automatically: the copyright and reserved font names of the
default fonts embedded via epaint_default_fonts (the crate ships
generic license files without copyright headers) and the availability of
the corresponding source for the MPL-2.0 crates.
"""

import json
import re
import sys

SEPARATOR = "-" * 60

MIT_TEXT = """\
Permission is hereby granted, free of charge, to any person obtaining a
copy of this software and associated documentation files (the
"Software"), to deal in the Software without restriction, including
without limitation the rights to use, copy, modify, merge, publish,
distribute, sublicense, and/or sell copies of the Software, and to
permit persons to whom the Software is furnished to do so, subject to
the following conditions:

The above copyright notice and this permission notice shall be included
in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
"""

HEADER = """\
AstralGauge - licenses
======================

This software is licensed under the MIT License:

Copyright (c) 2026 Morten Smedsrud

{mit}

Third-party licenses
====================

The sections below list the licenses of the third-party crates this
software is built with, generated from Cargo.lock by cargo-about and
licenses.py (see about.toml for the accepted license set).
""".format(mit=MIT_TEXT)

NOTES = """\
{}
Notes
{}

Bundled default fonts (shipped in epaint_default_fonts and embedded in
the binary). The crate's license files for the fonts are generic copies
without copyright headers, so the notices are listed here:

- NotoEmoji-Regular.ttf
  SIL Open Font License 1.1 (see section above)
  Copyright 2013 Google Inc. All Rights Reserved.
  Reserved Font Name: "Noto" (a trademark of Google Inc.)

- Ubuntu-Light.ttf
  Ubuntu Font Licence 1.0 (see section above)
  Copyright 2011 Canonical Ltd.
  "Ubuntu" and "Canonical" are registered trademarks of Canonical Ltd.

- Hack-Regular.ttf
  MIT (see section above) for the Hack work:
  Copyright (c) 2018 Source Foundry Authors
  The Bitstream Vera Sans Mono glyphs are licensed under the Bitstream
  Vera License (full text below):
  Copyright (c) 2003 by Bitstream, Inc. All Rights Reserved.
  Reserved font names: "Bitstream" and "Vera".
  The DejaVu project work in the font was committed to the public domain.

- emoji-icon-font.ttf
  MIT (see section above)
  Copyright (c) 2014 John Slegers

MPL-2.0: the corresponding source of the crates listed in the MPL-2.0
section is published on crates.io; the exact versions are recorded in
Cargo.lock. dwrote is a Windows-only dependency (the Windows font backend
of crossfont); it is not compiled into this Linux-only build.

BITSTREAM VERA LICENSE

Copyright (c) 2003 by Bitstream, Inc. All Rights Reserved. Bitstream Vera
is a trademark of Bitstream, Inc.

Permission is hereby granted, free of charge, to any person obtaining a
copy of the fonts accompanying this license ("Fonts") and associated
documentation files (the "Font Software"), to reproduce and distribute
the Font Software, including without limitation the rights to use, copy,
merge, publish, distribute, and/or sell copies of the Font Software, and
to permit persons to whom the Fonts are furnished to do so, subject to
the following conditions:

The above copyright and trademark notices and this permission notice
shall be included in all copies of one or more of the Font Software
typefaces.

The Font Software may be modified, altered, or added to, and in
particular the designs of glyphs or characters in the Fonts may be
modified and additional glyphs or characters may be added to the Fonts,
only if the fonts are renamed to names not containing either the words
"Bitstream" or the word "Vera".

This License becomes null and void to the extent applicable to Fonts or
Font Software that has been modified and is distributed under the
"Bitstream Vera" names.

The Font Software may be sold as part of a larger software package but
no copy of one or more of the Font Software typefaces may be sold by
itself.

THE FONT SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO ANY WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT
OF COPYRIGHT, PATENT, TRADEMARK, OR OTHER RIGHT. IN NO EVENT SHALL
BITSTREAM OR THE GNOME FOUNDATION BE LIABLE FOR ANY CLAIM, DAMAGES OR
OTHER LIABILITY, INCLUDING ANY GENERAL, SPECIAL, INDIRECT, INCIDENTAL,
OR CONSEQUENTIAL DAMAGES, WHETHER IN AN ACTION OF CONTRACT, TORT OR
OTHERWISE, ARISING OUT OF OR IN CONNECTION WITH THE USE OR INABILITY TO
USE, OR DEALINGS IN, THE FONT SOFTWARE.

Except as contained in this notice, the names of Gnome, the Gnome
Foundation, and Bitstream Inc., shall not be used in advertising or
otherwise to promote the sale, use or other dealings in this Font
Software without prior written authorization from the Gnome Foundation
or Bitstream Inc., respectively. For further information, contact:
fonts at gnome dot org.
""".format(SEPARATOR, SEPARATOR)

# A copyright notice line: starts with "copyright" (the standard
# placement at the top of a license file) and identifies its holder via
# a year, a (c) or a ©. This keeps boilerplate lines such as "COPYRIGHT
# AND PERMISSION NOTICE" and body text mentioning "the copyright owner"
# (which does not start the line) from being picked up.
COPYRIGHT_LINE_RE = re.compile(r"(19|20)\d{2}|\(c\)|©", re.IGNORECASE)


def is_copyright_line(stripped):
    return stripped.lower().startswith("copyright") and bool(
        COPYRIGHT_LINE_RE.search(stripped)
    )


# License kinds whose displayed text must keep a leading copyright line:
# for Unicode-3.0 the notice "Copyright (c) <year> Unicode, Inc." is part
# of the license itself ("this copyright and permission notice appear
# with all copies"), so it stays in the body. For the other kinds a
# leading copyright line is the *crate's* copyright and belongs only in
# the "Used by" lines, so it is stripped from the displayed text.
KEEP_LEADING_COPYRIGHT = {"Unicode-3.0"}


def group_copyright(text):
    """The copyright notice of a group's license file, if it has one."""
    for line in text.splitlines()[:12]:
        stripped = line.strip()
        if is_copyright_line(stripped):
            return stripped
    return None


def strip_leading_copyright(text):
    """Drop the leading per-crate copyright block from a license text.

    Removes the first copyright line and the "All rights reserved" / blank
    lines that directly follow it, so the section body shows the license
    terms while the crate-specific copyright stays in the "Used by" lines.
    """
    lines = text.splitlines()
    start = None
    for i, line in enumerate(lines[:12]):
        if is_copyright_line(line.strip()):
            start = i
            break
    if start is None:
        return text
    end = start
    while end < len(lines):
        stripped = lines[end].strip()
        if (
            is_copyright_line(stripped)
            or stripped.lower().startswith("all rights reserved")
            or stripped == ""
        ):
            end += 1
        else:
            break
    return "\n".join(lines[:start] + lines[end:])


def holder_for(crate, copyright_notice):
    """Copyright notice for a "Used by" line, or None."""
    if copyright_notice:
        return copyright_notice
    authors = crate.get("authors") or []
    if authors:
        return ", ".join(authors)
    return None


def render(data):
    out = [HEADER.rstrip("\n"), ""]

    # Group entries by license kind, preserving cargo-about's order.
    kinds = {}
    order = []
    for entry in data["licenses"]:
        kind = entry["id"]
        if kind not in kinds:
            kinds[kind] = {"name": entry["name"], "entries": []}
            order.append(kind)
        kinds[kind]["entries"].append(entry)

    for kind in order:
        info = kinds[kind]
        first = next(
            (e for e in info["entries"] if e.get("first_of_kind")),
            info["entries"][0],
        )
        out.append(SEPARATOR)
        out.append(f"{info['name']} ({kind})")
        out.append(SEPARATOR)
        body = first["text"].strip("\n")
        if kind not in KEEP_LEADING_COPYRIGHT:
            body = strip_leading_copyright(body).strip("\n")
        out.append(body)
        out.append("")
        out.append("Used by:")
        seen = set()
        for entry in info["entries"]:
            notice = group_copyright(entry.get("text", ""))
            for used in entry["used_by"]:
                crate = used["crate"]
                # The root package is listed in the header, not here.
                if not (crate.get("source") or "").startswith("registry+"):
                    continue
                key = (crate["name"], crate["version"])
                if key in seen:
                    continue
                seen.add(key)
                holder = holder_for(crate, notice)
                line = f"  {crate['name']} {crate['version']}"
                if holder:
                    line += f" — {holder}"
                out.append(line)
        out.append("")

    out.append(NOTES.rstrip("\n"))
    return "\n".join(out) + "\n"


def main():
    if len(sys.argv) != 2:
        sys.exit(f"usage: {sys.argv[0]} <about.json>")
    with open(sys.argv[1], encoding="utf-8") as f:
        data = json.load(f)
    sys.stdout.write(render(data))


if __name__ == "__main__":
    main()
