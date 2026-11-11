#!/usr/bin/env python3
"""Render the Claude Deck app icon (1024x1024 PNG) with no dependencies.

A macOS-style rounded square holding a 2x2 deck of terminal tiles; the
top-left tile is Claude orange with a prompt chevron.
Usage: make_icon.py OUT.png
"""
import math
import struct
import sys
import zlib

N = 1024


def rrect(px, py, cx, cy, hw, hh, r):
    """Signed distance to a rounded rectangle."""
    qx, qy = abs(px - cx) - hw + r, abs(py - cy) - hh + r
    outside = math.hypot(max(qx, 0), max(qy, 0))
    return outside + min(max(qx, qy), 0) - r


def segment(px, py, ax, ay, bx, by, half_width):
    dx, dy = bx - ax, by - ay
    t = max(0, min(1, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)))
    return math.hypot(px - ax - t * dx, py - ay - t * dy) - half_width


def cover(d):
    return max(0.0, min(1.0, 0.5 - d))


def mix(c1, c2, t):
    return tuple(a + (b - a) * t for a, b in zip(c1, c2))


def over(dst, color, alpha):
    r, g, b, a = dst
    out_a = alpha + a * (1 - alpha)
    if out_a == 0:
        return (0, 0, 0, 0)
    blend = lambda s, d: (s * alpha + d * a * (1 - alpha)) / out_a
    return (blend(color[0], r), blend(color[1], g), blend(color[2], b), out_a)


def hexc(h):
    return tuple(int(h[i:i + 2], 16) / 255 for i in (1, 3, 5))


BODY_TOP, BODY_BOTTOM = hexc("#3B3F5E"), hexc("#16171F")
TILE, TILE_EDGE = hexc("#0F1017"), hexc("#2C2F45")
ORANGE_TOP, ORANGE_BOTTOM = hexc("#E8875F"), hexc("#C4623F")
WHITE, LINE = hexc("#FFFFFF"), hexc("#5A6080")

TILE_SIZE, GAP = 290, 34
ORIGIN = 512 - TILE_SIZE - GAP / 2
TILES = [(ORIGIN + c * (TILE_SIZE + GAP), ORIGIN + r * (TILE_SIZE + GAP)) for r in (0, 1) for c in (0, 1)]


def pixel(x, y):
    px, py = x + 0.5, y + 0.5
    body = rrect(px, py, 512, 512, 412, 412, 185)
    if body > 1:
        return (0, 0, 0, 0)
    out = over((0, 0, 0, 0), mix(BODY_TOP, BODY_BOTTOM, (py - 100) / 824), cover(body))

    for i, (tx, ty) in enumerate(TILES):
        cx, cy, h = tx + TILE_SIZE / 2, ty + TILE_SIZE / 2, TILE_SIZE / 2
        d = rrect(px, py, cx, cy, h, h, 52)
        if d > 1:
            continue
        a = cover(d)
        if i == 0:
            out = over(out, mix(ORANGE_TOP, ORANGE_BOTTOM, (py - ty) / TILE_SIZE), a)
            # ">" chevron and cursor bar
            lx, ly = tx + 78, ty + 92
            chev = min(
                segment(px, py, lx, ly, lx + 62, ly + 54, 17),
                segment(px, py, lx + 62, ly + 54, lx, ly + 108, 17),
            )
            out = over(out, WHITE, cover(chev))
            out = over(out, WHITE, cover(rrect(px, py, tx + 196, ty + 200, 34, 15, 15)))
        else:
            out = over(out, TILE_EDGE, a)
            out = over(out, TILE, cover(d + 5))
            # faint lines of "output"
            for j, width in enumerate((150, 105, 128)):
                lx, ly = tx + 62, ty + 92 + j * 54
                line = rrect(px, py, lx + width / 2, ly, width / 2, 11, 11)
                out = over(out, LINE, cover(line) * 0.9)
    return out


def write_png(path, rows):
    raw = b"".join(b"\x00" + bytes(row) for row in rows)
    chunk = lambda tag, data: (
        struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    )
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", struct.pack(">IIBBBBB", N, N, 8, 6, 0, 0, 0)))
        f.write(chunk(b"IDAT", zlib.compress(raw, 9)))
        f.write(chunk(b"IEND", b""))


rows = []
for y in range(N):
    row = []
    for x in range(N):
        r, g, b, a = pixel(x, y)
        row += [round(r * 255), round(g * 255), round(b * 255), round(a * 255)]
    rows.append(row)
write_png(sys.argv[1], rows)
