#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."

# Regression for the measured-row pagination fix and the collapsed-border box
# model. A 50-row table whose cells wrap to three lines must keep every row and
# end each page inside the paper; the footer lands on the last page only and the
# repeated header returns on page two. A 90-character unbreakable token must not
# leave the table's item frame.
#
# Dependencies: bash, python3, poppler (pdftotext, pdftoppm, pdfinfo).

for tool in pdftotext pdftoppm pdfinfo; do
  command -v "$tool" > /dev/null || { echo "table_fit: $tool is required" >&2; exit 1; }
done

cargo build -q -p printkit

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
PRINTKIT="target/debug/printkit --data-dir $work/data"

for fixture in table-wrap-auto.json table-wrap-fixed.json; do
  stem="${fixture%.json}"
  $PRINTKIT pdf -i "tests/fixtures/$fixture" -o "$work/$stem.pdf" --json \
    > "$work/$stem.json"
done
$PRINTKIT pdf -i tests/fixtures/table-word.json -o "$work/table-word.pdf" --json \
  > "$work/table-word.json"

python3 - "$work" <<'PY'
import json
import subprocess
import sys
from pathlib import Path

work = Path(sys.argv[1])
PT_TO_MM = 25.4 / 72.0
PX_TO_MM = 25.4 / 150.0
THRESHOLD = 160
MARKERS = [f"{index:03}" for index in range(1, 51)]


def text(pdf: Path, page=None):
    args = ["pdftotext"]
    if page is not None:
        args += ["-f", str(page), "-l", str(page)]
    args += [str(pdf), "-"]
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


def pdf_pages(pdf: Path) -> int:
    info = subprocess.run(["pdfinfo", str(pdf)], check=True, capture_output=True, text=True)
    for line in info.stdout.splitlines():
        if line.startswith("Pages:"):
            return int(line.split()[1])
    raise SystemExit(f"{pdf}: pdfinfo reported no page count")


def ink_bottom_mm(pdf: Path, page: int) -> float:
    prefix = work / f"{pdf.stem}-p{page}"
    subprocess.run(
        ["pdftoppm", "-r", "150", "-gray", "-f", str(page), "-l", str(page),
         str(pdf), str(prefix)],
        check=True,
        capture_output=True,
    )
    raster = sorted(prefix.parent.glob(prefix.name + "-*.pgm"))[0]
    data = raster.read_bytes()
    position = 2
    fields = []
    while len(fields) < 3:
        while data[position:position + 1].isspace():
            position += 1
        start = position
        while not data[position:position + 1].isspace():
            position += 1
        fields.append(int(data[start:position]))
    position += 1
    width, height, _ = fields
    pixels = data[position:]
    bottom = 0
    for y in range(height):
        row = pixels[y * width:(y + 1) * width]
        if min(row) < THRESHOLD:
            bottom = y
    return (bottom + 1) * PX_TO_MM


for stem in ("table-wrap-auto", "table-wrap-fixed"):
    pdf = work / f"{stem}.pdf"
    result = json.loads((work / f"{stem}.json").read_text())
    pages = pdf_pages(pdf)
    assert result["pages"] == pages, f"{stem}: JSON pages {result['pages']} != PDF {pages}"
    body = text(pdf)
    missing = [marker for marker in MARKERS if marker not in body]
    assert not missing, f"{stem}: {len(missing)} rows lost: {missing[:5]}"
    assert pages >= 2, f"{stem}: 50 wrapped rows must not fit one page"
    assert "No" in text(pdf, 2), f"{stem}: page 2 does not repeat the header"
    for page in range(1, pages):
        assert "END" not in text(pdf, page), f"{stem}: footer leaked onto page {page}"
    assert "END" in text(pdf, pages), f"{stem}: footer is not on the last page"
    for page in range(1, pages + 1):
        bottom = ink_bottom_mm(pdf, page)
        assert bottom <= 297.5, f"{stem}: page {page} ink reaches {bottom:.1f} mm"
    print(f"{stem}: {pages} pages, 50 rows, {len(MARKERS)} markers, ink inside A4")

pdf = work / "table-word.pdf"
bbox = subprocess.run(
    ["pdftotext", "-bbox", str(pdf), "-"], check=True, capture_output=True, text=True
).stdout
import re

rights = [float(value) * PT_TO_MM for value in re.findall(r'xMax="([0-9.]+)"', bbox)]
assert rights, "table-word: pdftotext -bbox produced no words"
frame_right = 20.0 + 170.0
assert max(rights) <= frame_right, (
    f"table-word: a word reaches {max(rights):.2f} mm, outside the {frame_right:.0f} mm item"
)
assert pdf_pages(pdf) == 1, "table-word: the long token must stay on one page"
print(f"table-word: max word right {max(rights):.2f} mm inside {frame_right:.0f} mm")
PY

echo "table_fit: ok"

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
