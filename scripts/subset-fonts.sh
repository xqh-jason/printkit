#!/usr/bin/env bash
# Generate the bundled font subsets from the source fonts in assets/fonts.
#
# This script is an offline build tool. Cargo builds the generated WOFF2 files
# straight from the repository, so the build itself needs neither Python nor
# HarfBuzz. Run it only when the source fonts or the character set change, then
# commit both the subset files and their coverage manifests.
#
# Requirements: python3 with fontTools and Brotli (pip install "fonttools[brotli]").
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
fonts="$root/assets/fonts"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# OFL-1.1 condition 3 forbids a Modified Version from using the Reserved Font
# Name "Source". The Source Han subset is therefore renamed in its name table
# and CFF names after subsetting. Use --rename-only to apply the rename to the
# committed subset without regenerating the charset or CID manifests.
rename_source_han_subset() {
    python3 - "$fonts/SourceHanSansSC-Regular-subset.woff2" <<'PY'
import sys

from fontTools.ttLib import TTFont

# OFL-1.1 condition 3: a Modified Version may not use the Reserved Font Name
# "Source". Rename every field that identifies the face and leave nameID 0
# (Adobe's copyright and trademark statement) and the version fields untouched,
# because those are attribution, not the primary font name. `recalcTimestamp`
# is off so the rewritten file is byte-identical between runs.
NAMES = {
    1: "printkit Han Sans SC",
    2: "Regular",
    3: "2.005;PRNT;printkitHanSansSC-Regular;PRINTKIT",
    4: "printkit Han Sans SC Regular",
    6: "printkitHanSansSC-Regular",
}

path = sys.argv[1]
font = TTFont(path, recalcTimestamp=False)
for record in font["name"].names:
    value = NAMES.get(record.nameID)
    if value is not None:
        record.string = value.encode("utf-16-be")
if "CFF " not in font:
    raise SystemExit(f"{path}: expected a CFF-flavoured font")
cff = font["CFF "].cff
cff.fontNames[0] = "printkitHanSansSC-Regular"
top = cff.topDictIndex[0]
top.FamilyName = "printkit Han Sans SC"
top.FullName = "printkit Han Sans SC Regular"
font.flavor = "woff2"
font.save(path)

check = TTFont(path, lazy=True)
names = {record.nameID: record.toUnicode() for record in check["name"].names}
for name_id in (1, 3, 4, 6):
    if "Source" in names[name_id]:
        raise SystemExit(f"{path}: nameID {name_id} still uses the Reserved Font Name")
print(f"{path}: renamed modified version to {names[1]!r}")
PY
}

if [ "${1:-}" = "--rename-only" ]; then
    rename_source_han_subset
    exit 0
fi

if ! python3 -c 'import fontTools, brotli' >/dev/null 2>&1; then
    echo "fontTools with Brotli is required: pip install 'fonttools[brotli]'" >&2
    exit 1
fi

# Character set: ASCII printable plus every double-byte cell of GB2312. That
# covers the 6763 common hanzi (levels 1 and 2), punctuation, digits and the
# full-width forms in one reviewable list.
python3 - "$fonts/subset-charset.txt" <<'PY'
import sys

chars = {chr(code) for code in range(0x20, 0x7F)}
for high in range(0xA1, 0xFF):
    for low in range(0xA1, 0xFF):
        try:
            chars.add(bytes((high, low)).decode("gb2312"))
        except UnicodeDecodeError:
            pass
with open(sys.argv[1], "w", encoding="utf-8") as handle:
    handle.write("\n".join(sorted(chars)) + "\n")
print(f"subset-charset.txt: {len(chars)} characters")
PY

# The published Noto Sans Mono release is a variable font. Instance it at the
# default Regular axis values before subsetting so the WOFF2 carries one static
# face, which keeps the file small and the layout stable across WebView versions.
# `--no-recalc-timestamp` keeps the generated file byte-identical between runs,
# so a regeneration that changes nothing produces no diff to review.
python3 -m fontTools.varLib.instancer -q --no-recalc-timestamp \
    "$fonts/NotoSansMono-Variable.ttf" wght=400 wdth=100 \
    -o "$work/NotoSansMono-Regular.ttf"

# Ligature substitution is deliberately dropped: a shaped `fi` ligature has no
# one-to-one Unicode mapping in the PDF, so `pdftotext` would lose both
# characters. Kerning and mark positioning stay because they do not merge
# characters.
subset() {
    local source="$1" output="$2"
    shift 2
    python3 -m fontTools.subset "$source" \
        --text-file="$fonts/subset-charset.txt" \
        --flavor=woff2 \
        --layout-features='kern,mark,mkmk,ccmp,locl' \
        --no-hinting \
        "$@" \
        --output-file="$fonts/$output"
    printf '%s: %s\n' "$output" "$(ls -lh "$fonts/$output" | awk '{print $5}')"
}

# CFF outlines are de-subroutinised so the web renderer does not depend on
# subroutinisation support when embedding the font into the PDF.
subset "$fonts/SourceHanSansSC-Regular.otf" "SourceHanSansSC-Regular-subset.woff2" --desubroutinize
subset "$work/NotoSansMono-Regular.ttf" "NotoSansMono-Regular-subset.woff2"

# Every `subset` call above regenerates the name table with the upstream names,
# so the OFL rename must run again after them.
rename_source_han_subset

# One manifest per subset records exactly which characters the embedded face
# can address. `render` checks text against these lists instead of the input
# charset, because the two fonts do not cover the same characters.
python3 - "$fonts" <<'PY'
import sys
from pathlib import Path

from fontTools.ttLib import TTFont

fonts = Path(sys.argv[1])
requested = set((fonts / "subset-charset.txt").read_text(encoding="utf-8").splitlines())
for stem in ("SourceHanSansSC-Regular", "NotoSansMono-Regular"):
    font = TTFont(fonts / f"{stem}-subset.woff2")
    chars = [chr(cp) for cp in sorted(font.getBestCmap()) if cp >= 0x20]
    (fonts / f"{stem}-subset.charset.txt").write_text(
        "\n".join(chars) + "\n", encoding="utf-8"
    )
    # The cmap is the manifest of what the embedded face can address. It can be
    # longer than subset-charset.txt: layout closure keeps glyphs that are only
    # reachable through substitution features. Print that delta by code point so
    # the two counts are never read as a rounding difference.
    beyond = [c for c in chars if c not in requested]
    delta = (
        f" (+{len(beyond)} beyond subset-charset.txt: "
        f"{' '.join(f'U+{ord(c):04X}' for c in beyond)})"
        if beyond
        else ""
    )
    print(f"{stem}-subset.charset.txt: {len(chars)} code points{delta}")

# A CID-keyed CFF font uses the CID from its charset as the text code, while
# the PDF renderer writes its ToUnicode CMap from subset glyph ids. `crates/pdf`
# needs the CID-to-character table to replace that map. The cmap covers one
# glyph per character, but layout closure keeps substitutes that the renderer
# can select instead: the `locl` feature rewrites ASCII digits to their
# proportional forms, and those glyphs have no cmap entry of their own. Follow
# single-substitution lookups to a fixed point so every text code the embedded
# face can emit maps back to the character it draws.
source_han = TTFont(fonts / "SourceHanSansSC-Regular-subset.woff2")
glyphs: dict[str, int] = {}
for cp, name in sorted(source_han.getBestCmap().items()):
    if cp >= 0x20:
        glyphs.setdefault(name, cp)

substitutions: dict[str, str] = {}
for lookup in source_han["GSUB"].table.LookupList.Lookup:
    for subtable in lookup.SubTable:
        table = getattr(subtable, "ExtSubTable", None) or subtable
        mapping = getattr(table, "mapping", None)
        if mapping:
            substitutions.update(mapping)

changed = True
while changed:
    changed = False
    for source, target in substitutions.items():
        if source not in glyphs:
            continue
        if target in glyphs:
            if glyphs[target] != glyphs[source]:
                raise SystemExit(
                    f"{target} draws both U+{glyphs[source]:04X} "
                    f"(via {source}) and U+{glyphs[target]:04X}"
                )
            continue
        glyphs[target] = glyphs[source]
        changed = True

cids = sorted(
    (int(name[3:], 10), cp)
    for name, cp in glyphs.items()
    if name.startswith("cid")
)
if cids[-1][0] > 0xFFFF:
    raise SystemExit(f"CID {cids[-1][0]} does not fit a 16-bit text code")
(fonts / "SourceHanSansSC-Regular-subset.cids.txt").write_text(
    "".join(f"{cid:04x} {cp:04x}\n" for cid, cp in cids), encoding="ascii"
)
print(f"SourceHanSansSC-Regular-subset.cids.txt: {len(cids)} mappings")
PY

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
