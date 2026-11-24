#!/usr/bin/env python3
"""Generates the Siple app icon into packaging/icons/ (and the derived assets elsewhere).

The artwork: a white handset on a teal-to-blue tile with continuous (superellipse) corners, and
one green status lamp (the lamp every Siple screen uses for "registered / live"). Geometry is
computed, not hand placed: the handset is built from a center line, filleted, and then centered
on the tile by measurement (70 % bounding box, 30 % center of mass: the handset is heavy at its
bow, so pure box centering reads low-left).

Outputs (all checked in, so CI needs no Pillow; `pip install pillow` only to change the art):
  packaging/icons/siple-1024.png      macOS / Linux icon: 824 px tile on a 1024 canvas (Apple grid)
  packaging/icons/siple.ico           Windows: tile filling the frame; small frames use the
                                      simplified mark (bigger handset, no lamp)
  packaging/icons/siple-mark-256.png  the simplified mark, tile only: in-app brand mark and tray
  packaging/icons/siple-small-128.png  the simplified mark on the Apple grid: macOS 16/32 pt frames
                                      and the tray / menu-bar base image
  packaging/windows/msix-assets/      MSIX tile and app-list logos (`--msix-only` redoes just these)
  packaging/linux/hicolor/<N>x<N>/apps/io.zyno.siple.png   N = 64, 128, 256, 512: the Flatpak / Linux
                                      app icon (Flatpak exports PNGs up to 512 px); `--hicolor-only`
                                      redoes just these, resized from the committed siple-1024.png

`--preview DIR` also writes a contact sheet of every size to DIR/icon-sheet.png.
"""
import math
import pathlib
import sys

from PIL import Image, ImageChops, ImageDraw, ImageFilter, ImageFont

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "packaging" / "icons"
HICOLOR_OUT = ROOT / "packaging" / "linux" / "hicolor"
HICOLOR_SIZES = (64, 128, 256, 512)
MSIX_OUT = ROOT / "packaging" / "windows" / "msix-assets"
GLYPH_FONT = ROOT / "crates" / "siple" / "assets" / "fonts" / "Phosphor-Fill.ttf"
PHONE = "\ue3b8"  # Phosphor "phone"; theme::icon::PHONE

S = 4096  # supersampled working canvas
TILE = 824 / 1024  # Apple macOS icon grid: 824 pt tile centered on a 1024 canvas

# Brand colors. The gradient runs from the top-left (teal) to the bottom-right (deep blue);
# the in-app accent (#17769A light / #3FB2D4 dark) sits inside this range.
TEAL = (40, 204, 178)
MID = (22, 142, 186)
BLUE = (24, 74, 166)
LAMP = (61, 214, 140)  # theme DARK.ok.led


# ---- shapes ----------------------------------------------------------------------------------


def squircle_mask(size, n=5.0):
    """A superellipse |x|^n + |y|^n = 1 filling `size` (Apple-like continuous corners)."""
    m = Image.new("L", (size, size), 0)
    a = size / 2
    pts = []
    for i in range(2048):
        t = 2 * math.pi * i / 2048
        c, s = math.cos(t), math.sin(t)
        x = a + a * math.copysign(abs(c) ** (2 / n), c)
        y = a + a * math.copysign(abs(s) ** (2 / n), s)
        pts.append((x, y))
    ImageDraw.Draw(m).polygon(pts, fill=255)
    return m


def disk(d, c, r, fill=255):
    d.ellipse((c[0] - r, c[1] - r, c[0] + r, c[1] + r), fill=fill)


def handset(size, bold=False):
    """White handset on an L canvas of `size`: the Phosphor "phone" (fill) glyph, the same
    handset the app draws on its Call and Answer buttons, so icon, brand mark and controls
    share one shape. `bold` thickens it slightly for the small-size mark."""
    font = ImageFont.truetype(str(GLYPH_FONT), round(size * 0.9))
    m = Image.new("L", (size, size), 0)
    ImageDraw.Draw(m).text((size * 0.05, size * 0.05), PHONE, font=font, fill=255)
    if bold:
        m = m.filter(ImageFilter.MaxFilter(max(3, (size // 400) | 1)))
    return m


def centre_on(mask, size, frac, offset=(0.0, 0.0)):
    """Scales `mask` so its bounding box spans `frac` of `size` (larger side) and centers it
    by 70 % bounding box, 30 % center of mass. Returns an L canvas of `size`."""
    bbox = mask.getbbox()
    crop = mask.crop(bbox)
    bw, bh = crop.size
    s = frac * size / max(bw, bh)
    crop = crop.resize((max(1, round(bw * s)), max(1, round(bh * s))), Image.LANCZOS)
    bw, bh = crop.size
    # center of mass of the scaled crop
    hist_x = [0.0] * bw
    hist_y = [0.0] * bh
    px = crop.load()
    tot = 0.0
    step = max(1, bw // 400)
    for y in range(0, bh, step):
        for x in range(0, bw, step):
            v = px[x, y]
            if v:
                hist_x[x] += v
                hist_y[y] += v
                tot += v
    mx = sum(i * v for i, v in enumerate(hist_x)) / tot
    my = sum(i * v for i, v in enumerate(hist_y)) / tot
    ax = 0.7 * (bw / 2) + 0.3 * mx
    ay = 0.7 * (bh / 2) + 0.3 * my
    out = Image.new("L", (size, size), 0)
    out.paste(crop, (round(size / 2 - ax + offset[0] * size), round(size / 2 - ay + offset[1] * size)))
    return out


# ---- tile ------------------------------------------------------------------------------------


def lerp(a, b, t):
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))


def tile(size, lamp=True, bold=False, glyph=0.56):
    """The square tile (no outer margin): RGBA `size` px."""
    # Diagonal gradient, top-left to bottom-right, eased so the middle stays bright.
    g = Image.new("RGB", (256, 256))
    gp = g.load()
    for y in range(256):
        for x in range(256):
            t = (x * 0.35 + y * 0.65) / 255
            gp[x, y] = lerp(TEAL, MID, t / 0.5) if t < 0.5 else lerp(MID, BLUE, (t - 0.5) / 0.5)
    img = g.resize((size, size), Image.BICUBIC).convert("RGBA")
    # Soft light from the top: a wide white glow fading by mid tile.
    glow = Image.new("L", (size, size), 0)
    ImageDraw.Draw(glow).ellipse(
        (-0.25 * size, -0.85 * size, 1.25 * size, 0.55 * size), fill=34
    )
    glow = glow.filter(ImageFilter.GaussianBlur(size * 0.12))
    img = Image.alpha_composite(img, Image.merge("RGBA", (*[Image.new("L", (size, size), 255)] * 3, glow)))

    glyph_mask = centre_on(handset(size, bold=bold), size, glyph)
    # Shadow: soft, offset down, deep blue.
    sh = glyph_mask.filter(ImageFilter.GaussianBlur(size * 0.018))
    sh = sh.point(lambda v: v * 0.42)
    shadow = Image.new("RGBA", (size, size), (6, 30, 70, 0))
    shadow.putalpha(sh)
    shadow = ImageChops.offset(shadow, 0, round(size * 0.016))
    img = Image.alpha_composite(img, shadow)
    white = Image.new("RGBA", (size, size), (255, 255, 255, 0))
    white.putalpha(glyph_mask)
    img = Image.alpha_composite(img, white)

    if lamp:
        # On the handset's axis of symmetry (the diagonal), facing the cups.
        c = (size * 0.755, size * 0.245)
        r = size * 0.046
        halo = Image.new("L", (size, size), 0)
        disk(ImageDraw.Draw(halo), c, r * 1.55, fill=95)
        halo = halo.filter(ImageFilter.GaussianBlur(r * 0.45))
        hl = Image.new("RGBA", (size, size), (*LAMP, 0))
        hl.putalpha(halo)
        img = Image.alpha_composite(img, hl)
        led = Image.new("L", (size, size), 0)
        disk(ImageDraw.Draw(led), c, r)
        lr = Image.new("RGBA", (size, size), (*LAMP, 0))
        lr.putalpha(led)
        img = Image.alpha_composite(img, lr)
        # specular dot
        spec = Image.new("L", (size, size), 0)
        disk(ImageDraw.Draw(spec), (c[0] - r * 0.32, c[1] - r * 0.34), r * 0.34, fill=170)
        spec = spec.filter(ImageFilter.GaussianBlur(r * 0.14))
        sp = Image.new("RGBA", (size, size), (255, 255, 255, 0))
        sp.putalpha(spec)
        img = Image.alpha_composite(img, sp)

    # Hairline inner highlight along the top edge, darker rim at the bottom: a lit object.
    mask = squircle_mask(size)
    inner = mask.filter(ImageFilter.GaussianBlur(size * 0.004))
    edge = ImageChops.subtract(mask, ImageChops.offset(inner, 0, round(size * 0.006)))
    hi = Image.new("RGBA", (size, size), (255, 255, 255, 0))
    hi.putalpha(edge.point(lambda v: v * 0.30))
    img = Image.alpha_composite(img, hi)

    img.putalpha(ImageChops.multiply(img.getchannel("A"), mask))
    return img


def full_icon(t=None):
    """1024 canvas, tile on the Apple grid, with the system-like drop shadow under the tile."""
    t = tile(S) if t is None else t.resize((S, S), Image.LANCZOS)
    side = round(S * TILE)
    t = t.resize((side, side), Image.LANCZOS)
    canvas = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    off = (S - side) // 2
    sh = Image.new("L", (S, S), 0)
    sh.paste(squircle_mask(side), (off, off + round(S * 0.012)))
    sh = sh.filter(ImageFilter.GaussianBlur(S * 0.014)).point(lambda v: v * 0.30)
    shadow = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    shadow.putalpha(sh)
    canvas = Image.alpha_composite(canvas, shadow)
    canvas.alpha_composite(t, (off, off))
    return canvas.resize((1024, 1024), Image.LANCZOS)


def mark(px):
    """The simplified tile for small sizes: bolder, bigger handset, no lamp."""
    return tile(2048, lamp=False, bold=True, glyph=0.62).resize((px, px), Image.LANCZOS)


# ---- outputs ---------------------------------------------------------------------------------


def ico_frames():
    frames = []
    big = tile(S)
    for px in (16, 20, 24, 32, 40, 48, 64, 128, 256):
        if px <= 40:
            src = mark(px * 8)
            margin = 0.0
        else:
            src = big
            margin = 0.03
        side = round(px * (1 - 2 * margin))
        img = Image.new("RGBA", (px, px), (0, 0, 0, 0))
        t = src.resize((side, side), Image.LANCZOS)
        img.alpha_composite(t, ((px - side) // 2, (px - side) // 2))
        frames.append(img)
    return frames


def msix_assets():
    """Writes the MSIX logos. Names follow the resource qualifiers makepri indexes
    (<Name>.scale-<N>.png, <Name>.targetsize-<N>[_altform-unplated].png)."""
    MSIX_OUT.mkdir(parents=True, exist_ok=True)
    big = tile(2048)
    small = mark(1024)

    def canvas(w, h, frac):
        side = max(1, round(min(w, h) * frac))
        src = small if side <= 48 else big
        img = Image.new("RGBA", (w, h), (0, 0, 0, 0))
        sq = src.resize((side, side), Image.LANCZOS)
        img.alpha_composite(sq, ((w - side) // 2, (h - side) // 2))
        return img

    scales = (100, 125, 150, 200, 400)
    written = []

    def save(name, w, h, frac):
        canvas(w, h, frac).save(MSIX_OUT / name)
        written.append(name)

    for base, w, h, frac in (
        ("Square44x44Logo", 44, 44, 1.0),
        ("StoreLogo", 50, 50, 1.0),
        ("SmallTile", 71, 71, 0.8),
        ("Square150x150Logo", 150, 150, 0.7),
        ("Wide310x150Logo", 310, 150, 0.8),
        ("LargeTile", 310, 310, 0.6),
    ):
        for sc in scales:
            save(f"{base}.scale-{sc}.png", round(w * sc / 100), round(h * sc / 100), frac)
    for t in (16, 24, 32, 48, 256):
        save(f"Square44x44Logo.targetsize-{t}.png", t, t, 1.0)
        save(f"Square44x44Logo.targetsize-{t}_altform-unplated.png", t, t, 1.0)
    print("wrote", len(written), "files to", MSIX_OUT)


def preview(dirpath):
    d = pathlib.Path(dirpath)
    d.mkdir(parents=True, exist_ok=True)
    icon = Image.open(OUT / "siple-1024.png")
    m = Image.open(OUT / "siple-mark-256.png")
    sheet = Image.new("RGBA", (1600, 760), (236, 240, 244, 255))
    dark = Image.new("RGBA", (800, 330), (16, 22, 29, 255))
    sheet.paste(dark, (800, 430))
    sheet.alpha_composite(icon.resize((512, 512), Image.LANCZOS), (20, 20))
    x = 560
    for px in (256, 128, 64, 32):
        sheet.alpha_composite(icon.resize((px, px), Image.LANCZOS), (x, 40))
        x += px + 24
    x = 40
    for bg_y in (560, 600):
        pass
    for i, px in enumerate((64, 48, 32, 26, 20, 16)):
        sheet.alpha_composite(m.resize((px, px), Image.LANCZOS), (40 + i * 90, 600))
        sheet.alpha_composite(m.resize((px, px), Image.LANCZOS), (840 + i * 90, 600))
    sheet.save(d / "icon-sheet.png")
    print("preview", d / "icon-sheet.png")


def hicolor():
    """Resizes the committed siple-1024.png into the hicolor sizes; touches nothing else."""
    icon = Image.open(OUT / "siple-1024.png").convert("RGBA")
    for px in HICOLOR_SIZES:
        d = HICOLOR_OUT / f"{px}x{px}" / "apps"
        d.mkdir(parents=True, exist_ok=True)
        icon.resize((px, px), Image.LANCZOS).save(d / "io.zyno.siple.png", optimize=True)
        print("wrote", d / "io.zyno.siple.png")


def main():
    if "--hicolor-only" in sys.argv:
        hicolor()
        return
    if "--msix-only" in sys.argv:
        msix_assets()
        return
    OUT.mkdir(parents=True, exist_ok=True)
    full_icon().save(OUT / "siple-1024.png", optimize=True)
    mark(256).save(OUT / "siple-mark-256.png", optimize=True)
    # macOS .icns 16 and 32 pt frames (package-macos.sh): the simplified mark on the Apple grid.
    full_icon(mark(2048)).resize((128, 128), Image.LANCZOS).save(OUT / "siple-small-128.png", optimize=True)
    frames = ico_frames()
    frames[-1].save(OUT / "siple.ico", sizes=[f.size for f in frames], append_images=frames[:-1])
    print("wrote", OUT)
    msix_assets()
    if "--preview" in sys.argv:
        preview(sys.argv[sys.argv.index("--preview") + 1])


if __name__ == "__main__":
    main()
