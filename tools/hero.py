# Usage: python3 -m venv .venv && .venv/bin/pip install pillow==12.* fonttools
#        cargo test -- --ignored export_hero_screen
#        .venv/bin/python -I tools/hero.py target/hero docs/hero.png FONTS_DIR
# FONTS_DIR holds JetBrainsMono-Regular.ttf, JetBrainsMono-Bold.ttf (Ghostty's
# default font) and SymbolsNerdFontMono-Regular.ttf (its built-in icons).
"""Draws one frame of `tuimail --demo`, as exported by the
`export_hero_screen` test, the way Ghostty shows it: every cell's character
in JetBrains Mono, box lines and blocks drawn as Ghostty draws them, Nerd
Font icons at Ghostty's size, links underlined. As tuitube's tools/hero.py,
without the pictures."""

import json
import os
import sys

from fontTools.ttLib import TTFont
from PIL import Image, ImageDraw, ImageFont

SCREEN, OUT, FONTS = sys.argv[1], sys.argv[2], sys.argv[3]
PAD = 28
RADIUS = 26
SIZE = 26.6

screen = json.load(open(os.path.join(SCREEN, "screen.json")))
COLS, ROWS = screen["cols"], screen["rows"]
CW, CH = screen["cell"]
BG, FG = screen["bg"], screen["fg"]

REGULAR = os.path.join(FONTS, "JetBrainsMono-Regular.ttf")
ICONS = os.path.join(FONTS, "SymbolsNerdFontMono-Regular.ttf")
regular = ImageFont.truetype(REGULAR, SIZE)
bold = ImageFont.truetype(os.path.join(FONTS, "JetBrainsMono-Bold.ttf"), SIZE)
icons = ImageFont.truetype(ICONS, SIZE)
covered = set(TTFont(REGULAR).getBestCmap())
icon_cmap = set(TTFont(ICONS).getBestCmap())
# What neither has comes from a system font, as Ghostty falls back.
FALLBACK = "/System/Library/Fonts/Apple Symbols.ttf"
fallback = ImageFont.truetype(FALLBACK, 30)
ascent, descent = regular.getmetrics()
BASELINE = (CH - (ascent + descent)) // 2 + ascent

width, height = COLS * CW + 2 * PAD, ROWS * CH + 2 * PAD
canvas = Image.new("RGB", (width, height), BG)
draw = ImageDraw.Draw(canvas)
cells = screen["cells"]


def cell_box(x, y):
    left, top = PAD + x * CW, PAD + y * CH
    return left, top, left + CW, top + CH


def rectangle(box, color):
    if box[2] >= box[0] and box[3] >= box[1]:
        draw.rectangle(box, fill=color)


def line_shape(ch, x, y, color):
    """Box lines and blocks, drawn to the cell's edges as Ghostty does, so
    they join up; True if `ch` is one."""
    left, top, right, bottom = cell_box(x, y)
    mx, my = left + CW // 2, top + CH // 2
    if ch == "─":
        rectangle([left, my - 1, right - 1, my], color)
    elif ch == "━":
        rectangle([left, my - 2, right - 1, my + 1], color)
    elif ch == "│":
        rectangle([mx - 1, top, mx, bottom - 1], color)
    elif ch == "┃":
        rectangle([mx - 2, top, mx + 1, bottom - 1], color)
    elif ch in "├┤":
        rectangle([mx - 1, top, mx, bottom - 1], color)
        if ch == "├":
            rectangle([mx, my - 1, right - 1, my], color)
        else:
            rectangle([left, my - 1, mx, my], color)
    elif ch in "╭╮╰╯":
        r = CW // 2
        # The arc's center, and the straight bits from its ends to the
        # cell's edges.
        cx = mx + r if ch in "╭╰" else mx - r
        cy = my + r if ch in "╭╮" else my - r
        start = {"╭": 180, "╮": 270, "╰": 90, "╯": 0}[ch]
        draw.arc([cx - r, cy - r, cx + r, cy + r], start, start + 90, fill=color, width=2)
        if ch in "╭╮":
            rectangle([mx - 1, cy, mx, bottom - 1], color)
        else:
            rectangle([mx - 1, top, mx, cy], color)
        if ch in "╭╰":
            rectangle([cx, my - 1, right - 1, my], color)
        else:
            rectangle([left, my - 1, cx, my], color)
    elif ch == "▀":
        rectangle([left, top, right - 1, top + CH // 2 - 1], color)
    elif ch == "▄":
        rectangle([left, top + CH // 2, right - 1, bottom - 1], color)
    elif ch == "▌":
        rectangle([left, top, left + CW // 2 - 1, bottom - 1], color)
    elif ch == "▏":
        rectangle([left, top, left + 1, bottom - 1], color)
    elif ch == "█":
        rectangle([left, top, right - 1, bottom - 1], color)
    else:
        return False
    return True


def blank(x, y):
    return x >= COLS or cells[y * COLS + x][0] in ("", " ")


def draw_icon(x, y, ch, color):
    """A Nerd Font icon, as Ghostty draws one: as big as a cell's height
    allows, over two cells when the next one is empty, with room left
    before the text after it."""
    left, top, _, _ = cell_box(x, y)
    room = 2 * CW if blank(x + 1, y) else CW
    l, t, r, b = icons.getbbox(ch)
    scale = min(room * 0.62 / max(r - l, 1), CH * 0.56 / max(b - t, 1))
    font = ImageFont.truetype(ICONS, SIZE * scale)
    l, t, r, b = font.getbbox(ch)
    gx = left + (room * 0.86 - (r - l)) / 2 - l
    gy = top + (CH - (b - t)) / 2 - t
    draw.text((gx, gy), ch, font=font, fill=color)


def draw_background(x, y):
    bg = cells[y * COLS + x][2]
    if bg:
        left, top, right, bottom = cell_box(x, y)
        draw.rectangle([left, top, right - 1, bottom - 1], fill=bg)


def draw_glyph(x, y):
    ch, fg, _, heavy, _, underlined = cells[y * COLS + x]
    left, top, right, _ = cell_box(x, y)
    color = fg or FG
    if underlined:
        line = top + BASELINE + 4
        draw.rectangle([left, line, right - 1, line + 1], fill=color)
    if ch in ("", " ") or line_shape(ch, x, y, color):
        return
    point = ord(ch[0])
    if 0xE000 <= point <= 0xF8FF or point >= 0xF0000:
        draw_icon(x, y, ch, color)
        return
    font = bold if heavy else regular
    if point not in covered:
        font = icons if point in icon_cmap else fallback
    draw.text((left, top + BASELINE), ch, font=font, fill=color, anchor="ls")


# Backgrounds first, then characters, so an icon wider than its cell isn't
# painted over by the next cell's background.
for y in range(ROWS):
    for x in range(COLS):
        draw_background(x, y)
for y in range(ROWS):
    for x in range(COLS):
        draw_glyph(x, y)

# Rounded corners, like a window.
mask = Image.new("L", canvas.size, 0)
ImageDraw.Draw(mask).rounded_rectangle([0, 0, width - 1, height - 1], RADIUS, fill=255)
out = Image.new("RGBA", canvas.size, (0, 0, 0, 0))
out.paste(canvas, (0, 0), mask)
os.makedirs(os.path.dirname(OUT) or ".", exist_ok=True)
out.save(OUT, optimize=True)
print(f"wrote {OUT} ({width}×{height})")
