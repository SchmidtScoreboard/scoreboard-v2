#!/usr/bin/env python3
"""Generate the app icon: a 14x14 LED-matrix miniature of the scoreboard.

Layout mimics the board's hockey frame during CHI @ STL with a power play:
two team rows (left accent stripe in the team's secondary color, body in its
primary) over a clock line with the yellow power-play badge at the right.
The grid runs edge to edge — only the inter-cell gap remains at the borders.

Regenerate with:  python3 scripts/gen_app_icon.py
"""

from PIL import Image, ImageDraw

OUT = "ios/ScoreboardApp/ScoreboardApp/Assets.xcassets/AppIcon.appiconset/AppIcon.png"

SIZE = 1024
N = 14          # grid is N x N cells
CELL = 60
GAP = 14        # 14*60 + 13*14 = 1022; 1px margin each side
MARGIN = (SIZE - (N * CELL + (N - 1) * GAP)) // 2
RADIUS = 13

BG = (8, 8, 12)
UNLIT = (30, 30, 37)
WHITE = (240, 240, 244)
PP_YELLOW = (255, 204, 0)

# Team colors as the server catalog carries them (primary, secondary).
CHI = ((0xCF, 0x0A, 0x2C), (0xFF, 0xD1, 0x00))  # Blackhawks
STL = ((0x00, 0x2F, 0x87), (0xFC, 0xB5, 0x14))  # Blues


def cell_color(col: int, row: int):
    # Team rows: 3 cells tall, 2-cell accent stripe on the LEFT (like the
    # panel's accent), body running to the right edge.
    for rows, (primary, secondary) in ((range(1, 4), CHI), (range(6, 9), STL)):
        if row in rows:
            return secondary if col < 2 else primary
    # Clock line, left-aligned, with the power-play badge at the right edge.
    if row == 12:
        if col < 6:
            return WHITE
        if col >= 12:
            return PP_YELLOW
    return UNLIT


def main():
    img = Image.new("RGB", (SIZE, SIZE), BG)
    d = ImageDraw.Draw(img)
    for row in range(N):
        for col in range(N):
            x = MARGIN + col * (CELL + GAP)
            y = MARGIN + row * (CELL + GAP)
            d.rounded_rectangle(
                (x, y, x + CELL, y + CELL), radius=RADIUS, fill=cell_color(col, row)
            )
    img.save(OUT)
    print(f"wrote {OUT}")


if __name__ == "__main__":
    main()
