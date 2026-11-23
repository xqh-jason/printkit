#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

# Dependencies: bash, python3 (3.6+ for f-strings), poppler (pdftotext, pdftoppm).
# Geometry acceptance: render the fixtures, extract text origins with
# pdftotext -bbox and vector bounds from 600 dpi rasters, then compare each
# value with tests/fixtures/geometry-expected.json. Deviations above the
# tolerances fail the script.

cargo build -q -p printkit

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

fixtures="$(python3 - <<'PY'
import json
with open('tests/fixtures/geometry-expected.json') as handle:
    data = json.load(handle)
print(' '.join(sorted({sample['fixture'] for sample in data['samples']})))
PY
)"

for fixture in $fixtures; do
  target/debug/printkit --data-dir "$work/data" pdf \
    -i "tests/fixtures/$fixture" -o "$work/$fixture.pdf" --json > "$work/$fixture.json"
done

python3 - "$work" <<'PY'
import json
import re
import subprocess
import sys
from html import unescape
from pathlib import Path

work = Path(sys.argv[1])
expected = json.loads(Path('tests/fixtures/geometry-expected.json').read_text())

PT_TO_MM = 25.4 / 72.0
PX_TO_MM = 25.4 / 600.0
DEFAULT_TOLERANCE = float(expected['tolerance_mm'])
LINE_HEIGHT_TOLERANCE = float(expected['line_height_tolerance_mm'])
SEARCH_MM = 3.0
THRESHOLD = 128

WORD = re.compile(
    r'<word xMin="([^"]+)" yMin="([^"]+)" xMax="([^"]+)" yMax="([^"]+)">(.+?)</word>',
    re.S,
)


def words(pdf: Path, page: int):
    xml = subprocess.run(
        ['pdftotext', '-bbox', '-f', str(page), '-l', str(page), str(pdf), '-'],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    result = []
    for match in WORD.finditer(xml):
        result.append(
            {
                'text': unescape(match.group(5)).strip(),
                'x': float(match.group(1)) * PT_TO_MM,
                'y': float(match.group(2)) * PT_TO_MM,
                'x_max': float(match.group(3)) * PT_TO_MM,
                'y_max': float(match.group(4)) * PT_TO_MM,
            }
        )
    return result


def find_word(items, text, page):
    matches = [item for item in items if item['text'] == text]
    if not matches:
        raise SystemExit(f'page {page}: word {text!r} not found')
    return matches[0]


def read_pgm(path: Path):
    data = path.read_bytes()
    if not data.startswith(b'P5'):
        raise SystemExit(f'{path}: not a P5 PGM')
    position = 2
    fields = []
    while len(fields) < 3:
        while data[position:position + 1].isspace():
            position += 1
        if data[position:position + 1] == b'#':
            while data[position:position + 1] != b'\n':
                position += 1
            continue
        start = position
        while not data[position:position + 1].isspace():
            position += 1
        fields.append(int(data[start:position]))
    position += 1
    width, height, _ = fields
    return width, height, data[position:]


def rasterize(pdf: Path, page: int, prefix: Path):
    subprocess.run(
        ['pdftoppm', '-r', '600', '-gray', '-f', str(page), '-l', str(page),
         str(pdf), str(prefix)],
        check=True,
        capture_output=True,
    )
    matches = sorted(prefix.parent.glob(prefix.name + '-*.pgm'))
    if not matches:
        raise SystemExit(f'page {page}: pdftoppm produced no image')
    return read_pgm(matches[0])


def vector_box(raster, box, tolerance_mm):
    width, height, pixels = raster
    left = max(0, int((box['x'] - SEARCH_MM) / PX_TO_MM))
    right = min(width, int((box['x'] + box['width'] + SEARCH_MM) / PX_TO_MM) + 1)
    top = max(0, int((box['y'] - SEARCH_MM) / PX_TO_MM))
    bottom = min(height, int((box['y'] + box['height'] + SEARCH_MM) / PX_TO_MM) + 1)
    min_x, max_x, min_y, max_y = right, -1, bottom, -1
    for y in range(top, bottom):
        row = pixels[y * width + left:y * width + right]
        for offset, value in enumerate(row):
            if value < THRESHOLD:
                x = left + offset
                min_x = min(min_x, x)
                max_x = max(max_x, x)
                min_y = min(min_y, y)
                max_y = max(max_y, y)
    if max_x < 0:
        raise SystemExit(f'no dark pixels inside the search window for {box}')
    return {
        'x': min_x * PX_TO_MM,
        'y': min_y * PX_TO_MM,
        'width': (max_x - min_x + 1) * PX_TO_MM,
        'height': (max_y - min_y + 1) * PX_TO_MM,
    }


failures = []
checked = 0
max_deviation = 0.0

for sample in expected['samples']:
    fixture = sample['fixture']
    pdf = work / f'{fixture}.pdf'
    if not pdf.exists():
        raise SystemExit(f'missing PDF for {fixture}')

    text_cache = {}
    for entry in sample.get('text', []):
        page = entry['page']
        if page not in text_cache:
            text_cache[page] = words(pdf, page)
        word = find_word(text_cache[page], entry['word'], page)
        deviation = abs(word['x'] - float(entry['x_mm']))
        checked += 1
        max_deviation = max(max_deviation, deviation)
        status = 'ok' if deviation <= DEFAULT_TOLERANCE else 'FAIL'
        print(f'{fixture} text {entry["word"]}: x deviation {deviation:.3f} mm [{status}]')
        if status != 'ok':
            failures.append(f'{fixture} text {entry["word"]}: {deviation:.3f} > {DEFAULT_TOLERANCE}')

    for entry in sample.get('line_height', []):
        page = entry['page']
        if page not in text_cache:
            text_cache[page] = words(pdf, page)
        first = find_word(text_cache[page], entry['first'], page)
        second = find_word(text_cache[page], entry['second'], page)
        measured = abs(second['y'] - first['y'])
        deviation = abs(measured - float(entry['line_height_mm']))
        checked += 1
        max_deviation = max(max_deviation, deviation)
        status = 'ok' if deviation <= LINE_HEIGHT_TOLERANCE else 'FAIL'
        print(
            f'{fixture} line height {entry["first"]}/{entry["second"]}: '
            f'measured {measured:.3f} mm, deviation {deviation:.3f} mm [{status}]'
        )
        if status != 'ok':
            failures.append(
                f'{fixture} line height {entry["first"]}/{entry["second"]}: '
                f'{deviation:.3f} > {LINE_HEIGHT_TOLERANCE}'
            )

    rasters = {}
    for entry in sample.get('vector', []):
        page = entry['page']
        if page not in rasters:
            rasters[page] = rasterize(pdf, page, work / f'{fixture}-page{page}')
        tolerance = float(entry.get('tolerance_mm', DEFAULT_TOLERANCE))
        measured = vector_box(rasters[page], entry['box_mm'], tolerance)
        deltas = {
            'x': abs(measured['x'] - entry['box_mm']['x']),
            'y': abs(measured['y'] - entry['box_mm']['y']),
            'width': abs(measured['width'] - entry['box_mm']['width']),
            'height': abs(measured['height'] - entry['box_mm']['height']),
        }
        deviation = max(deltas.values())
        checked += 1
        max_deviation = max(max_deviation, deviation)
        status = 'ok' if deviation <= tolerance else 'FAIL'
        print(
            f'{fixture} vector {entry["id"]}: '
            f'dx={deltas["x"]:.3f} dy={deltas["y"]:.3f} '
            f'dw={deltas["width"]:.3f} dh={deltas["height"]:.3f} mm '
            f'(limit {tolerance:.3f}) [{status}]'
        )
        if status != 'ok':
            failures.append(
                f'{fixture} vector {entry["id"]}: max deviation {deviation:.3f} > {tolerance}'
            )

if failures:
    print()
    for failure in failures:
        print(f'compare-geometry: {failure}', file=sys.stderr)
    raise SystemExit(1)

print()
print(
    f'compare-geometry: {checked} entries within tolerance '
    f'(max deviation {max_deviation:.3f} mm)'
)
PY

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
