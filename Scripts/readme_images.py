"""README images from the real UI exe in mock mode (demo data, never the real store).

Shots: search (dark), home and timeline (dark), settings and home (light), plus the banner built from the search shot.
Windows are captured with PrintWindow and cropped to the DWM frame (the window rect includes the invisible resize
border, which showed up as black bars). Needs the UI built in Debug or Release and Python with pillow + uiautomation.

usage: python Scripts/readme_images.py [--exe PATH] [--out docs/images] [--only search,home,timeline,light,banner]
"""
import argparse
import ctypes
import ctypes.wintypes as wt
import os
import subprocess
import sys
import time

from PIL import Image, ImageDraw, ImageFilter, ImageFont

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
u32, g32, dwm = ctypes.windll.user32, ctypes.windll.gdi32, ctypes.windll.dwmapi
u32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))   # per-monitor v2: window rects in physical pixels
VP = ctypes.c_void_p
u32.GetWindowDC.restype = VP
u32.GetWindowDC.argtypes = [VP]
u32.ReleaseDC.argtypes = [VP, VP]
u32.PrintWindow.argtypes = [VP, VP, ctypes.c_uint]
g32.CreateCompatibleDC.restype = VP
g32.CreateCompatibleDC.argtypes = [VP]
g32.CreateCompatibleBitmap.restype = VP
g32.CreateCompatibleBitmap.argtypes = [VP, ctypes.c_int, ctypes.c_int]
g32.SelectObject.restype = VP
g32.SelectObject.argtypes = [VP, VP]
g32.DeleteObject.argtypes = [VP]
g32.DeleteDC.argtypes = [VP]
g32.GetDIBits.argtypes = [VP, VP, ctypes.c_uint, ctypes.c_uint, VP, VP, ctypes.c_uint]
DWMWA_EXTENDED_FRAME_BOUNDS = 9
PW_RENDERFULLCONTENT = 2


class BIH(ctypes.Structure):
    _fields_ = [("biSize", wt.DWORD), ("biWidth", wt.LONG), ("biHeight", wt.LONG), ("biPlanes", wt.WORD), ("biBitCount", wt.WORD),
                ("biCompression", wt.DWORD), ("biSizeImage", wt.DWORD), ("a", wt.LONG), ("b", wt.LONG), ("c", wt.DWORD), ("d", wt.DWORD)]


def shot(h, out):
    """PrintWindow capture of one window, cropped to its visible frame."""
    r = wt.RECT()
    u32.GetWindowRect(h, ctypes.byref(r))
    w, hh = r.right - r.left, r.bottom - r.top
    hdc = u32.GetWindowDC(h)
    mdc = g32.CreateCompatibleDC(hdc)
    bmp = g32.CreateCompatibleBitmap(hdc, w, hh)
    g32.SelectObject(mdc, bmp)
    u32.PrintWindow(h, mdc, PW_RENDERFULLCONTENT)
    bi = BIH(ctypes.sizeof(BIH), w, -hh, 1, 32, 0, 0, 0, 0, 0, 0)
    buf = ctypes.create_string_buffer(w * hh * 4)
    g32.GetDIBits(mdc, bmp, 0, hh, buf, ctypes.cast(ctypes.byref(bi), VP), 0)
    img = Image.frombuffer("RGBA", (w, hh), buf, "raw", "BGRA", 0, 1).convert("RGB")
    g32.DeleteObject(bmp)
    g32.DeleteDC(mdc)
    u32.ReleaseDC(h, hdc)
    f = wt.RECT()
    if dwm.DwmGetWindowAttribute(wt.HWND(h), DWMWA_EXTENDED_FRAME_BOUNDS, ctypes.byref(f), ctypes.sizeof(f)) == 0:
        img = img.crop((f.left - r.left, f.top - r.top, f.right - r.left, f.bottom - r.top))
    img.save(out, optimize=True)
    print("shot", out, img.size)
    return img


def windows(pid):
    found = []

    @ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)
    def cb(h, _):
        p = wt.DWORD()
        u32.GetWindowThreadProcessId(h, ctypes.byref(p))
        if p.value == pid and u32.IsWindowVisible(h) and u32.GetWindowTextLengthW(h) > 0:
            found.append(h)
        return True

    u32.EnumWindows(cb, 0)
    return found


def launch(exe, args, size=None, wait=6):
    proc = subprocess.Popen([exe, "--mock-capturable", *args])
    for _ in range(60):
        time.sleep(0.25)
        if windows(proc.pid):
            break
    time.sleep(wait)
    h = windows(proc.pid)[0]
    if size:
        u32.SetWindowPos(h, 0, 80, 60, size[0], size[1], 0x0014)
        time.sleep(1.5)
    return proc, h


def kill(proc):
    proc.kill()
    proc.wait()
    time.sleep(1)


def search_shot(exe, out):
    """The launcher only searches a prefilled query when an instance is already running: start hidden, then
    a second launch hands the query over."""
    main = subprocess.Popen([exe, "--mock-capturable", "--hidden"])
    try:
        time.sleep(7)
        subprocess.Popen([exe, "--mock-capturable", "--mock-query", "invoice"]).wait(30)
        time.sleep(4)
        return shot(windows(main.pid)[0], out)
    finally:
        kill(main)


def light_shots(exe, out_dir):
    import uiautomation as auto
    p, h = launch(exe, ["--page", "settings:appearance"], (1400, 900))
    try:
        root = auto.ControlFromHandle(h)
        combo = root.ComboBoxControl(searchDepth=40)
        combo.GetExpandCollapsePattern().Expand()
        time.sleep(0.8)
        root.ListItemControl(searchDepth=40, Name="Light").GetSelectionItemPattern().Select()
        time.sleep(0.8)
        root.ButtonControl(searchDepth=40, Name="Save").GetInvokePattern().Invoke()
        time.sleep(2.5)
        shot(h, os.path.join(out_dir, "settings-light.png"))
        root.ListItemControl(searchDepth=40, Name="Home").GetSelectionItemPattern().Select()
        time.sleep(2.5)
        shot(h, os.path.join(out_dir, "home-light.png"))
    finally:
        kill(p)


def banner(out_dir):
    """Icon, name and tagline on a dark gradient, the search shot on the right."""
    W, H = 1600, 520
    bg = Image.new("RGB", (W, H))
    d = ImageDraw.Draw(bg)
    for y in range(H):
        t = y / H
        d.line([(0, y), (W, y)], fill=(int(12 + 6 * t), int(22 + 4 * t), int(36 - 10 * t)))
    glow = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    ImageDraw.Draw(glow).ellipse((-200, -260, 760, 700), fill=(76, 194, 255, 60))
    bg = Image.alpha_composite(bg.convert("RGBA"), glow.filter(ImageFilter.GaussianBlur(120)))

    s = Image.open(os.path.join(out_dir, "search-dark.png")).convert("RGBA")
    sw = 820
    s = s.resize((sw, int(s.height * sw / s.width)), Image.LANCZOS)
    mask = Image.new("L", s.size, 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, s.width - 1, s.height - 1), radius=14, fill=255)
    s.putalpha(mask)
    shadow = Image.new("RGBA", (s.width + 80, s.height + 80), (0, 0, 0, 0))
    ImageDraw.Draw(shadow).rounded_rectangle((40, 50, s.width + 40, s.height + 50), radius=18, fill=(0, 0, 0, 170))
    shadow = shadow.filter(ImageFilter.GaussianBlur(22))
    x, y = W - sw - 60, max(30, (H - s.height) // 2)
    bg.alpha_composite(shadow, (x - 40, y - 40))
    bg.alpha_composite(s, (x, y))

    icon = Image.open(os.path.join(ROOT, "assets", "icons", "app.png")).convert("RGBA").resize((128, 128), Image.LANCZOS)
    bg.alpha_composite(icon, (80, 120))
    fonts = os.path.join(os.environ.get("WINDIR", r"C:\Windows"), "Fonts")
    title = ImageFont.truetype(os.path.join(fonts, "segoeuib.ttf"), 84)
    sub = ImageFont.truetype(os.path.join(fonts, "segoeui.ttf"), 28)
    d = ImageDraw.Draw(bg)
    d.text((80, 270), "BetterRecall", font=title, fill=(255, 255, 255))
    d.text((84, 378), "Search everything you have seen and heard.", font=sub, fill=(200, 220, 235))
    d.text((84, 420), "Local, encrypted, on your NPU.", font=sub, fill=(76, 194, 255))
    out = os.path.join(out_dir, "banner.png")
    bg.convert("RGB").save(out, optimize=True)
    print("banner", out)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--exe", default=os.path.join(ROOT, "artifacts", "bin", "BetterRecall.UI", "debug_win-x64", "BetterRecall.UI.exe"))
    ap.add_argument("--out", default=os.path.join(ROOT, "docs", "images"))
    ap.add_argument("--only", default="search,home,timeline,light,banner")
    a = ap.parse_args()
    steps = set(a.only.split(","))
    if not os.path.isfile(a.exe):
        sys.exit(f"UI exe not found: {a.exe} (build src/BetterRecall.UI first)")
    os.makedirs(a.out, exist_ok=True)
    if "search" in steps:
        search_shot(a.exe, os.path.join(a.out, "search-dark.png"))
    if "home" in steps:
        p, h = launch(a.exe, ["--page", "home"], (1400, 900))
        shot(h, os.path.join(a.out, "home-dark.png"))
        kill(p)
    if "timeline" in steps:
        p, h = launch(a.exe, ["--page", "timeline"], (1400, 900), wait=8)
        shot(h, os.path.join(a.out, "timeline-dark.png"))
        kill(p)
    if "light" in steps:
        light_shots(a.exe, a.out)
    if "banner" in steps:
        banner(a.out)


if __name__ == "__main__":
    main()
