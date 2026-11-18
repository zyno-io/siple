#!/usr/bin/env python3
"""Generates the Siple app icon (phone handset glyph) into packaging/icons/.

Outputs: siple-1024.png, siple.ico (Windows, multi-size). The macOS .icns is built from the
PNG by scripts/package-macos.sh with sips + iconutil. The outputs are checked in, so Pillow
(`pip install pillow`) is only needed when the artwork changes.

It also writes the MSIX visual assets to packaging/windows/msix-assets/ (tile and app-list
logos at every scale / target size, derived from siple-1024.png; also checked in so CI needs no
Pillow). `--msix-only` regenerates just those from the existing siple-1024.png.
"""
import sys
import math
import pathlib

from PIL import Image, ImageDraw, ImageFilter

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "packaging" / "icons"
MSIX_OUT = ROOT / "packaging" / "windows" / "msix-assets"
S = 4096  # supersampled canvas; downscaled at the end


def lerp(a, b, t):
    return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))


def background():
    # Vertical teal -> deep blue gradient inside a macOS-style rounded square (824/1024 body).
    top, bottom = (38, 198, 168), (24, 84, 168)
    grad = Image.new("RGB", (1, S))
    for y in range(S):
        grad.putpixel((0, y), lerp(top, bottom, y / (S - 1)))
    grad = grad.resize((S, S))
    mask = Image.new("L", (S, S), 0)
    m = int(S * 100 / 1024)
    ImageDraw.Draw(mask).rounded_rectangle(
        (m, m, S - m, S - m), radius=int(S * 185 / 1024), fill=255
    )
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    img.paste(grad, (0, 0), mask)
    return img, mask


def handset():
    """White handset: a thick arc with an earpiece and mouthpiece block at the ends."""
    layer = Image.new("L", (S, S), 0)
    d = ImageDraw.Draw(layer)
    cx, cy = S / 2, S / 2 + S * 0.10
    r = S * 0.215          # arc radius (centre line)
    w = S * 0.095          # arc thickness
    a0, a1 = 212, 328      # degrees, opening downward in screen space
    pts = []
    for i in range(401):
        a = math.radians(a0 + (a1 - a0) * i / 400)
        pts.append((cx + r * math.cos(a), cy + r * math.sin(a)))
    for p in pts:  # overlapping disks give a perfectly smooth thick arc
        d.ellipse((p[0] - w / 2, p[1] - w / 2, p[0] + w / 2, p[1] + w / 2), fill=255)
    # end pieces: chunky rounded blocks pointing down (towards ear / mouth)
    bw, bh = S * 0.15, S * 0.19
    for p, sign in ((pts[0], 1), (pts[-1], -1)):
        x = p[0] + sign * w * 0.15
        d.rounded_rectangle(
            (x - bw / 2, p[1] - bh * 0.15, x + bw / 2, p[1] + bh * 0.85),
            radius=int(bw * 0.35),
            fill=255,
        )
    layer = layer.rotate(135, resample=Image.BICUBIC, center=(S / 2, S / 2))
    return layer


def msix_assets():
    """Writes the MSIX logos. Names follow the resource qualifiers makepri indexes
    (<Name>.scale-<N>.png, <Name>.targetsize-<N>[_altform-unplated].png)."""
    MSIX_OUT.mkdir(parents=True, exist_ok=True)
    src = Image.open(OUT / "siple-1024.png").convert("RGBA")
    m = 100  # transparent margin of the rounded square in the 1024 source
    body = src.crop((m, m, 1024 - m, 1024 - m))  # the rounded square itself

    def canvas(w, h, frac):
        """body scaled to `frac` of the shorter side, centred on a transparent w x h canvas."""
        side = max(1, round(min(w, h) * frac))
        img = Image.new("RGBA", (w, h), (0, 0, 0, 0))
        sq = body.resize((side, side), Image.LANCZOS)
        img.paste(sq, ((w - side) // 2, (h - side) // 2), sq)
        return img

    scales = (100, 125, 150, 200, 400)
    written = []

    def save(name, w, h, frac):
        canvas(w, h, frac).save(MSIX_OUT / name)
        written.append(name)

    # (base name, width, height at scale 100, fraction of the shorter side the glyph fills)
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


def main():
    if "--msix-only" in sys.argv:
        msix_assets()
        return
    OUT.mkdir(parents=True, exist_ok=True)
    img, mask = background()
    g = handset()
    # soft shadow
    shadow = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    shadow.paste((0, 20, 60, 110), (0, int(S * 0.012)), g)
    shadow = shadow.filter(ImageFilter.GaussianBlur(S * 0.012))
    img = Image.alpha_composite(img, shadow)
    white = Image.new("RGBA", (S, S), (255, 255, 255, 255))
    img.paste(white, (0, 0), g)
    png = img.resize((1024, 1024), Image.LANCZOS)
    png.save(OUT / "siple-1024.png")
    png.save(
        OUT / "siple.ico",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )
    print("wrote", OUT)
    msix_assets()


if __name__ == "__main__":
    main()
