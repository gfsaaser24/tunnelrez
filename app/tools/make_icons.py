"""Draw TunnelRez's tray glyphs and app icon, pixel-aligned at every size.

Run: python -I app/tools/make_icons.py   (needs Pillow). Writes app/icons/.
Each size is drawn on its own 8x grid whose geometry is snapped to whole target pixels, then box-reduced,
so 1 px strokes stay 1 px (crisp) instead of a blurry downscale of one big master.
"""
from pathlib import Path

from PIL import Image, ImageDraw

OUT = Path(__file__).resolve().parent.parent / "icons"
SS = 8  # supersampling factor
TRAY_SIZES = (16, 20, 24, 32)
GREEN = (108, 203, 95, 255)  # Fluent success, dark theme
RED = (255, 153, 164, 255)  # Fluent critical, dark theme
AMBER = (252, 225, 0, 255)


def canvas(size):
    return Image.new("RGBA", (size * SS, size * SS), (0, 0, 0, 0))


def px(v):
    return round(v * SS)


def tray(size, fg, state):
    """state: idle | streaming | alert. fg: glyph color (white on a dark taskbar, near-black on a light one)."""
    im = canvas(size)
    d = ImageDraw.Draw(im)
    s = size / 16
    stroke = {16: 1, 20: 1, 24: 1.5, 32: 2}[size]
    # Screen: 14x10 at 16 px, inset 1 px; stand below.
    x0, y0, x1, y1 = 1 * s, 2 * s, 15 * s, 12 * s
    x0, y0, x1, y1 = round(x0), round(y0), round(x1), round(y1)
    r = max(1.5, 2 * s)
    if state == "streaming":
        d.rounded_rectangle((px(x0), px(y0), px(x1) - 1, px(y1) - 1), radius=px(r), fill=fg)
        # A cut-out "stream" chevron in the middle of the filled screen.
        cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
        w = 2.2 * s
        h = 2.6 * s
        cut = Image.new("L", im.size, 0)
        dc = ImageDraw.Draw(cut)
        pts = [(cx - w * 0.6, cy - h), (cx + w * 0.9, cy), (cx - w * 0.6, cy + h)]
        dc.polygon([(px(a), px(b)) for a, b in pts], fill=255)
        alpha = im.getchannel("A")
        alpha = Image.composite(Image.new("L", im.size, 0), alpha, cut)
        im.putalpha(alpha)
    else:
        d.rounded_rectangle(
            (px(x0), px(y0), px(x1) - 1, px(y1) - 1), radius=px(r), outline=fg, width=px(stroke)
        )
    # Stand: one bar, centred.
    sy = round(13.5 * s) if size != 16 else 14
    sw = round(6 * s)
    sx = round((size - sw) / 2)
    d.rectangle((px(sx), px(sy), px(sx + sw) - 1, px(sy + stroke) - 1), fill=fg)
    if state in ("streaming", "alert"):
        col = GREEN if state == "streaming" else AMBER
        dot = round(6 * s)
        bx, by = size - dot, size - dot
        # Clear a ring around the dot so it reads on top of the glyph.
        ring = max(1, round(s))
        clear = Image.new("L", im.size, 0)
        ImageDraw.Draw(clear).ellipse(
            (px(bx - ring), px(by - ring), px(size + ring) - 1, px(size + ring) - 1), fill=255
        )
        alpha = Image.composite(Image.new("L", im.size, 0), im.getchannel("A"), clear)
        im.putalpha(alpha)
        d = ImageDraw.Draw(im)
        d.ellipse((px(bx), px(by), px(size) - 1, px(size) - 1), fill=col)
    return im.reduce(SS)


def app_icon(size):
    """Full-colour app icon: accent tile with the screen glyph and speed lines."""
    im = canvas(size)
    d = ImageDraw.Draw(im)
    s = size / 256
    # Vertical gradient tile.
    top, bot = (64, 156, 255), (0, 95, 184)
    tile = Image.new("RGBA", im.size)
    td = ImageDraw.Draw(tile)
    H = im.size[1]
    for y in range(H):
        t = y / max(1, H - 1)
        c = tuple(round(top[i] + (bot[i] - top[i]) * t) for i in range(3)) + (255,)
        td.line((0, y, im.size[0], y), fill=c)
    mask = Image.new("L", im.size, 0)
    inset = 8 * s
    ImageDraw.Draw(mask).rounded_rectangle(
        (px(inset), px(inset), px(size - inset) - 1, px(size - inset) - 1), radius=px(56 * s), fill=255
    )
    im.paste(tile, (0, 0), mask)
    d = ImageDraw.Draw(im)
    white = (255, 255, 255, 255)
    soft = (255, 255, 255, 150)
    sw = max(1.0, 14 * s)
    # Screen.
    d.rounded_rectangle(
        (px(78 * s), px(70 * s), px(206 * s), px(162 * s)), radius=px(16 * s), outline=white, width=px(sw)
    )
    # Stand.
    d.rounded_rectangle((px(118 * s), px(182 * s), px(166 * s), px(196 * s)), radius=px(7 * s), fill=white)
    # Play chevron inside the screen.
    cx, cy = 145 * s, 116 * s
    d.polygon(
        [(px(cx - 14 * s), px(cy - 22 * s)), (px(cx + 22 * s), px(cy)), (px(cx - 14 * s), px(cy + 22 * s))],
        fill=white,
    )
    # Speed lines.
    if size >= 32:
        for y, x0 in ((92, 40), (116, 26), (140, 40)):
            d.rounded_rectangle(
                (px(x0 * s), px((y - 6) * s), px(62 * s), px((y + 6) * s)), radius=px(6 * s), fill=soft
            )
    return im.reduce(SS)


def main():
    OUT.mkdir(exist_ok=True)
    for theme, fg in (("dark", (255, 255, 255, 255)), ("light", (0, 0, 0, 228))):
        for state in ("idle", "streaming", "alert"):
            for size in TRAY_SIZES:
                tray(size, fg, state).save(OUT / f"tray-{state}-{theme}-{size}.png")
    sizes = (16, 20, 24, 32, 40, 48, 64, 128, 256)
    imgs = [app_icon(sz) for sz in sizes]
    imgs[-1].save(OUT / "icon.ico", sizes=[(sz, sz) for sz in sizes], append_images=imgs[:-1])
    imgs[-1].save(OUT / "icon.png")
    app_icon(64).save(OUT / "app-64.png")
    # Preview sheet for review (not shipped).
    sheet = Image.new("RGBA", (40 * 3 * 4 + 20, 2 * 44 + 20), (32, 32, 32, 255))
    x = 10
    for state in ("idle", "streaming", "alert"):
        for size in TRAY_SIZES:
            sheet.paste(tray(size, (255, 255, 255, 255), state), (x, 10), tray(size, (255, 255, 255, 255), state))
            lt = tray(size, (0, 0, 0, 228), state)
            bg = Image.new("RGBA", (size, size), (243, 243, 243, 255))
            bg.alpha_composite(lt)
            sheet.paste(bg, (x, 54))
            x += 40
    sheet.save(OUT.parent / "tools" / "tray-preview.png")


if __name__ == "__main__":
    main()
