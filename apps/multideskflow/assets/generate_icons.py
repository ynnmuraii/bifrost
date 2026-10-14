"""Regenerate shipped PNG/ICO/raw RGBA from icon.svg: python generate_icons.py.

Uses only Python's standard library. The source SVG deliberately contains only
rounded rectangles; four-times supersampling keeps the small tray sizes crisp.
"""

import math
from pathlib import Path
import struct
import xml.etree.ElementTree as ET
import zlib

ROOT = Path(__file__).resolve().parent
SIZES = (16, 20, 24, 32, 40, 48, 64, 256)
SAMPLES = 4


def distance(x, y, rect, expansion=0):
    """Signed distance to a rounded rectangle (negative inside)."""
    left, top, width, height, radius = rect
    half_w = width / 2 + expansion
    half_h = height / 2 + expansion
    radius = max(0, radius + expansion)
    dx = abs(x - left - width / 2) - half_w + radius
    dy = abs(y - top - height / 2) - half_h + radius
    return math.hypot(max(dx, 0), max(dy, 0)) + min(max(dx, dy), 0) - radius


def color(value):
    return tuple(bytes.fromhex(value.removeprefix("#")))


def render(size, rectangles):
    pixels = bytearray()
    for py in range(size):
        for px in range(size):
            sums = [0, 0, 0, 0]
            for sy in range(SAMPLES):
                for sx in range(SAMPLES):
                    x = (px + (sx + 0.5) / SAMPLES) * 32 / size
                    y = (py + (sy + 0.5) / SAMPLES) * 32 / size
                    sample = (0, 0, 0, 0)
                    for bounds, fill, stroke, thickness in rectangles:
                        if fill and distance(x, y, bounds) <= 0:
                            sample = (*fill, 255)
                        if stroke and distance(x, y, bounds, thickness / 2) <= 0 and distance(x, y, bounds, -thickness / 2) >= 0:
                            sample = (*stroke, 255)
                    for channel in range(4):
                        sums[channel] += sample[channel]
            alpha = sums[3]
            pixels.extend(round(sums[c] * 255 / alpha) if alpha else 0 for c in range(3))
            pixels.append(round(alpha / SAMPLES**2))
    return bytes(pixels)


def chunk(kind, payload):
    return struct.pack(">I", len(payload)) + kind + payload + struct.pack(">I", zlib.crc32(kind + payload))


def png(size, rgba):
    scanlines = b"".join(b"\0" + rgba[y * size * 4:(y + 1) * size * 4] for y in range(size))
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(scanlines, 9)) + chunk(b"IEND", b"")


def main():
    (ROOT.parents[2] / "packaging" / "multideskflow.svg").write_bytes((ROOT / "icon.svg").read_bytes())
    rectangles = []
    for element in ET.parse(ROOT / "icon.svg").getroot():
        if element.tag != "{http://www.w3.org/2000/svg}rect":
            raise ValueError("Only rounded rectangles are supported by this icon generator")
        bounds = tuple(float(element.get(key, "0")) for key in ("x", "y", "width", "height", "rx"))
        fill = element.get("fill", "none")
        stroke = element.get("stroke", "none")
        rectangles.append((bounds, color(fill) if fill != "none" else None, color(stroke) if stroke != "none" else None, float(element.get("stroke-width", "0"))))
    images = []
    for size in SIZES:
        rgba = render(size, rectangles)
        image = png(size, rgba)
        (ROOT / f"icon-{size}.png").write_bytes(image)
        if size == 32:
            (ROOT / "icon-32.rgba").write_bytes(rgba)
        images.append(image)
    offset = 6 + len(SIZES) * 16
    directory = bytearray(struct.pack("<HHH", 0, 1, len(SIZES)))
    for size, image in zip(SIZES, images):
        directory.extend(struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(image), offset))
        offset += len(image)
    (ROOT / "icon.ico").write_bytes(bytes(directory) + b"".join(images))


if __name__ == "__main__":
    main()
