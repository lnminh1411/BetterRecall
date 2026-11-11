# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""Draws the BetterRecall icons from the Segoe Fluent Icons clock-history glyph (U+E81C).

  assets/icons/app.ico         dark glyph on a cyan rounded square, 16-256 px (exe, windows, taskbar)
  assets/icons/app.png         same at 256 px (UI title bar, home page)
  assets/icons/tray-dark.ico   white glyph, transparent (dark taskbar)
  assets/icons/tray-light.ico  black glyph, transparent (light taskbar)

Usage: python Scripts/make_icons.py   (Windows, needs Pillow; reads C:\\Windows\\Fonts\\SegoeIcons.ttf)
"""
from __future__ import annotations

from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "assets" / "icons"
FONT = Path(r"C:\Windows\Fonts\SegoeIcons.ttf")
GLYPH = "\ue81c"
CYAN = (76, 194, 255, 255)
INK = (10, 27, 35, 255)
APP_SIZES = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
TRAY_SIZES = [16, 20, 24, 32, 40, 48, 64]
SS = 4  # supersampling


def glyph(size: int, colour, fill_ratio: float) -> Image.Image:
    big = size * SS
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    font = ImageFont.truetype(str(FONT), int(big * fill_ratio))
    d = ImageDraw.Draw(img)
    l, t, r, b = d.textbbox((0, 0), GLYPH, font=font)
    d.text(((big - (r - l)) / 2 - l, (big - (b - t)) / 2 - t), GLYPH, font=font, fill=colour)
    return img


def app(size: int) -> Image.Image:
    big = size * SS
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    ImageDraw.Draw(img).rounded_rectangle((0, 0, big - 1, big - 1), radius=int(big * 0.22), fill=CYAN)
    img.alpha_composite(glyph(size, INK, 0.62))
    return img.resize((size, size), Image.LANCZOS)


def tray(size: int, colour) -> Image.Image:
    return glyph(size, colour, 0.92).resize((size, size), Image.LANCZOS)


def save_ico(frames: list[Image.Image], path: Path) -> None:
    frames = sorted(frames, key=lambda f: f.width, reverse=True)
    frames[0].save(path, format="ICO", sizes=[(f.width, f.height) for f in frames], append_images=frames[1:])


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    save_ico([app(s) for s in APP_SIZES], OUT / "app.ico")
    app(256).save(OUT / "app.png")
    save_ico([tray(s, (255, 255, 255, 255)) for s in TRAY_SIZES], OUT / "tray-dark.ico")
    save_ico([tray(s, (0, 0, 0, 255)) for s in TRAY_SIZES], OUT / "tray-light.ico")
    for f in sorted(OUT.iterdir()):
        print(f, f.stat().st_size)


if __name__ == "__main__":
    main()
