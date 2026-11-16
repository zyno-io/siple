#!/usr/bin/env python3
"""Generates the Siple app icon (phone handset glyph) into packaging/icons/.

Outputs: siple-1024.png, siple.ico (Windows, multi-size). The macOS .icns is built from the
PNG by scripts/package-macos.sh with sips + iconutil. The outputs are checked in, so Pillow
(`pip install pillow`) is only needed when the artwork changes.
"""
import math
import pathlib

from PIL import Image, ImageDraw, ImageFilter

OUT = pathlib.Path(__file__).resolve().parent.parent / "packaging" / "icons"
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


def main():
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


if __name__ == "__main__":
    main()
