#!/usr/bin/env python3
"""Render the geometry in assets/zcraft-icon.svg to a 64x64 RGBA PNG.
Uses only the Python standard library; run from any directory.
"""
import binascii
from pathlib import Path
import struct
import zlib

SIZE = 64
SAMPLES = 4
Z = [(16, 16), (48, 16), (48, 24), (40, 24), (40, 32), (32, 32),
     (32, 40), (48, 40), (48, 48), (16, 48), (16, 40), (24, 40),
     (24, 32), (32, 32), (32, 24), (16, 24)]


def rounded_rect(x, y, lo, hi, radius):
    if not (lo <= x < hi and lo <= y < hi):
        return False
    cx = min(max(x, lo + radius), hi - radius)
    cy = min(max(y, lo + radius), hi - radius)
    return (x - cx) ** 2 + (y - cy) ** 2 <= radius ** 2


def in_polygon(x, y):
    inside = False
    for (ax, ay), (bx, by) in zip(Z, Z[1:] + Z[:1]):
        if (ay > y) != (by > y) and x < (bx - ax) * (y - ay) / (by - ay) + ax:
            inside = not inside
    return inside


def sample(x, y):
    color = (0, 0, 0, 0)
    if rounded_rect(x, y, 2, 62, 12):
        color = (39, 69, 55, 255)
    if rounded_rect(x, y, 4, 60, 10):
        color = (19, 37, 30, 255)
    if in_polygon(x, y - 2):
        color = (36, 122, 76, 255)
    if in_polygon(x, y):
        color = (121, 226, 139, 255)
    return color


def chunk(kind, data):
    return (struct.pack(">I", len(data)) + kind + data
            + struct.pack(">I", binascii.crc32(kind + data) & 0xffffffff))


def main():
    raw = bytearray()
    for y in range(SIZE):
        raw.append(0)  # PNG scanline filter: none.
        for x in range(SIZE):
            samples = [sample(x + (sx + 0.5) / SAMPLES,
                              y + (sy + 0.5) / SAMPLES)
                       for sy in range(SAMPLES) for sx in range(SAMPLES)]
            alpha = sum(p[3] for p in samples)
            rgb = [round(sum(p[c] * p[3] for p in samples) / alpha)
                   if alpha else 0 for c in range(3)]
            raw.extend(rgb + [round(alpha / len(samples))])
    png = (b"\x89PNG\r\n\x1a\n"
           + chunk(b"IHDR", struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0))
           + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
           + chunk(b"IEND", b""))
    output = Path(__file__).resolve().parents[1] / "server-icon.png"
    output.write_bytes(png)
    print(f"{output}: {SIZE}x{SIZE}, {len(png)} bytes")


if __name__ == "__main__":
    main()
