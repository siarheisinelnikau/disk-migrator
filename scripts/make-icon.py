"""Generate original disk-and-transfer artwork, with no third-party assets."""
import struct
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

def pixels(n):
    data = bytearray()
    for y in range(n):
        for x in range(n):
            u, v = (x + .5) / n, (y + .5) / n
            color = (0, 0, 0, 0)
            # Two simple storage trays, and a bold forward arrow.
            if .10 < u < .90 and (.15 < v < .44 or .60 < v < .89):
                color = (22, 49, 68, 255)
            if .18 < u < .30 and (.25 < v < .34 or .70 < v < .79):
                color = (132, 226, 200, 255)
            if .38 < u < .73 and .46 < v < .58:
                color = (0, 119, 106, 255)
            if .65 < u < .90 and abs(v - .52) < (.90 - u) * .65:
                color = (0, 119, 106, 255)
            data.extend(color)
    return data

icons = []
for size in (16, 20, 24, 32, 48, 64, 128, 256):
    rgba = pixels(size)
    bitmap = bytearray()
    for y in reversed(range(size)):
        for x in range(size):
            r, g, b, a = rgba[(y * size + x) * 4:(y * size + x + 1) * 4]
            bitmap.extend((b, g, r, a))
    mask = bytes(((size + 31) // 32) * 4 * size)
    dib = struct.pack('<IiiHHIIiiII', 40, size, size * 2, 1, 32, 0, len(bitmap), 0, 0, 0, 0)
    icons.append((size, dib + bitmap + mask))
offset = 6 + len(icons) * 16
entries, bodies = bytearray(), bytearray()
for size, body in icons:
    entries.extend(struct.pack('<BBBBHHII', size % 256, size % 256, 0, 0, 1, 32, len(body), offset))
    bodies.extend(body)
    offset += len(body)
(ROOT / 'assets' / 'app.ico').write_bytes(struct.pack('<HHH', 0, 1, len(icons)) + entries + bodies)
(ROOT / 'assets' / 'window.rgba').write_bytes(pixels(64))
