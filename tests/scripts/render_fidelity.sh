#!/usr/bin/env bash
# Geometry and raster checks for text alignment, frame clipping, page
# backgrounds and table auto columns. Requires poppler (pdftotext, pdftoppm).
set -euo pipefail
cd "$(dirname "$0")/../.."

for tool in pdftotext pdftoppm; do
  command -v "$tool" >/dev/null || {
    echo "render_fidelity: $tool is required" >&2
    exit 1
  }
done

cargo build -q -p printkit
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
bin=target/debug/printkit

for fixture in align overflow table-auto background image-fit; do
  "$bin" --data-dir "$work/data" pdf \
    -i "tests/fixtures/$fixture.json" -o "$work/$fixture.pdf" >/dev/null
done

python3 - "$work" <<'PY'
import re
import subprocess
import sys
from html import unescape
from pathlib import Path

work = Path(sys.argv[1])
PT_TO_MM = 25.4 / 72.0
TOL = 0.3
WORD = re.compile(r'<word xMin="([^"]+)" yMin="([^"]+)" xMax="([^"]+)" yMax="([^"]+)">(.+?)</word>', re.S)


def words(pdf):
    xml = subprocess.run(
        ['pdftotext', '-bbox', '-f', '1', '-l', '1', str(pdf), '-'],
        check=True, capture_output=True, text=True,
    ).stdout
    return [
        {
            'text': unescape(m.group(5)).strip(),
            'x': float(m.group(1)) * PT_TO_MM,
            'x_max': float(m.group(3)) * PT_TO_MM,
        }
        for m in WORD.finditer(xml)
    ]


def word(pdf, text):
    found = [entry for entry in words(pdf) if entry['text'] == text]
    if not found:
        raise SystemExit(f'{pdf.name}: word {text!r} not found')
    return found[0]


def raster(pdf):
    subprocess.run(
        ['pdftoppm', '-r', '150', '-gray', '-f', '1', '-l', '1', str(pdf), str(pdf.with_suffix(''))],
        check=True, capture_output=True,
    )
    path = sorted(pdf.parent.glob(pdf.stem + '-*.pgm'))[0]
    data = path.read_bytes()
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


def dark_between(pixels, width, x0_mm, x1_mm, y0_mm, y1_mm, dpi=150):
    x0 = max(0, int(x0_mm * dpi / 25.4))
    x1 = min(width, int(x1_mm * dpi / 25.4) + 1)
    y0 = max(0, int(y0_mm * dpi / 25.4))
    y1 = min(len(pixels) // width, int(y1_mm * dpi / 25.4) + 1)
    return sum(1 for y in range(y0, y1) for x in range(x0, x1) if pixels[y * width + x] < 128)


def pixel(pixels, width, x_mm, y_mm, dpi=150):
    x = int(x_mm * dpi / 25.4)
    y = int(y_mm * dpi / 25.4)
    return pixels[y * width + x]


def bbox(pixels, width, x0_mm, x1_mm, y0_mm, y1_mm, dpi=150):
    x0 = max(0, int(x0_mm * dpi / 25.4))
    x1 = min(width, int(x1_mm * dpi / 25.4) + 1)
    y0 = max(0, int(y0_mm * dpi / 25.4))
    y1 = min(len(pixels) // width, int(y1_mm * dpi / 25.4) + 1)
    xs, ys = [], []
    for y in range(y0, y1):
        for x in range(x0, x1):
            if pixels[y * width + x] < 128:
                xs.append(x)
                ys.append(y)
    if not xs:
        return None
    return ((max(xs) - min(xs) + 1) * 25.4 / dpi, (max(ys) - min(ys) + 1) * 25.4 / dpi)


failures = []
align = work / 'align.pdf'
left = word(align, 'LEFTMARK')
center = word(align, 'CENTERMARK')
right = word(align, 'RIGHTMARK')
center_x = (center['x'] + center['x_max']) / 2
for name, measured, expected in [
    ('left xMin', left['x'], 10.0),
    ('center center', center_x, 60.0),
    ('right xMax', right['x_max'], 110.0),
]:
    if abs(measured - expected) > TOL:
        failures.append(f'align {name}: {measured:.3f} mm, expected {expected:.3f}')

overflow = work / 'overflow.pdf'
width, _, pixels = raster(overflow)
outside = dark_between(pixels, width, 30.0, 210.0, 15.0, 35.0)
inside = dark_between(pixels, width, 10.0, 30.0, 15.0, 35.0)
if outside:
    failures.append(f'overflow: {outside} dark pixels outside the 30 mm frame edge')
if not inside:
    failures.append('overflow: no ink inside the frame')

background = work / 'background.pdf'
width, _, pixels = raster(background)
if pixel(pixels, width, 5.0, 5.0) >= 200:
    failures.append('background: page corner is white, background was not painted')
plain = work / 'align.pdf'
width2, _, pixels2 = raster(plain)
if pixel(pixels2, width2, 5.0, 5.0) < 200:
    failures.append('background: the no-background control is not white')

image = work / 'image-fit.pdf'
width_img, _, pixels_img = raster(image)
for index, name in enumerate(['contain', 'cover', 'fill', 'none']):
    box = bbox(pixels_img, width_img, 10.0, 30.0, 10.0 + index * 40.0, 30.0 + index * 40.0)
    if box is None:
        failures.append(f'image fit {name}: no ink')
        continue
    if name == 'contain' and (abs(box[0] - 20.0) > 0.5 or abs(box[1] - 10.0) > 0.5):
        failures.append(f'image fit contain: {box[0]:.2f}x{box[1]:.2f} mm')
    if name in ('cover', 'fill') and (abs(box[0] - 20.0) > 0.5 or abs(box[1] - 20.0) > 0.5):
        failures.append(f'image fit {name}: {box[0]:.2f}x{box[1]:.2f} mm')
    if name == 'none' and not (1.8 < box[0] / box[1] < 2.2):
        failures.append(f'image fit none lost its aspect ratio: {box[0]:.2f}x{box[1]:.2f} mm')

table = work / 'table-auto.pdf'
visible = word(table, 'VISIBLE_C')
if visible['x'] < 10.0 - TOL or visible['x_max'] > 60.0 + TOL:
    failures.append(
        f'table auto column is outside its frame: {visible["x"]:.3f}..{visible["x_max"]:.3f} mm'
    )

if failures:
    print('\n'.join(failures), file=sys.stderr)
    raise SystemExit(1)
print('render_fidelity: ok (align, clip, background, table auto, image fit)')
PY

# Exit explicitly: bash 5.3 skips EXIT traps at natural EOF, and cleanup must run.
exit 0
