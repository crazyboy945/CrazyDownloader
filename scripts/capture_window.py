"""截取一个已存在窗口（不启动程序），输出 PNG。纯 ctypes，无第三方依赖。

用法：
    python scripts/capture_window.py <窗口标题子串> <out.png>

配合 screenshot.py 的剪贴板注入参数，用于验证"检测到下载链接"弹窗是否正常出现。
"""

import ctypes
import struct
import sys
import time
import zlib
from ctypes import wintypes

KEY = sys.argv[1]
OUT = sys.argv[2]

user32 = ctypes.WinDLL("user32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)


def find_window(key):
    """返回标题包含 key 的**最大**可见顶层窗口（标题相同的可能不止一个）。"""
    found = []

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def cb(hwnd, _lparam):
        if not user32.IsWindowVisible(hwnd):
            return True
        n = user32.GetWindowTextLengthW(hwnd)
        if n <= 0:
            return True
        buf = ctypes.create_unicode_buffer(n + 1)
        user32.GetWindowTextW(hwnd, buf, n + 1)
        if key.lower() in buf.value.lower():
            r = wintypes.RECT()
            user32.GetWindowRect(hwnd, ctypes.byref(r))
            area = (r.right - r.left) * (r.bottom - r.top)
            found.append((area, hwnd, buf.value, (r.right - r.left, r.bottom - r.top)))
        return True

    user32.EnumWindows(cb, 0)
    for area, hwnd, title, size in sorted(found, reverse=True):
        print(f"  候选窗口：{title!r} 尺寸 {size[0]}x{size[1]}")
    if not found:
        return (None, None)
    best = max(found)
    return best[1], best[2]


if KEY == "-":
    # 整屏截图，便于先看清桌面上到底有什么
    hwnd, title = None, "(整屏)"
    x, y = 0, 0
    W = user32.GetSystemMetrics(0)
    H = user32.GetSystemMetrics(1)
else:
    hwnd, title = find_window(KEY)
    if not hwnd:
        print(f"没找到标题包含 {KEY!r} 的窗口", file=sys.stderr)
        sys.exit(1)
    print(f"窗口：{title!r}")

if hwnd:
    HWND_TOPMOST, HWND_NOTOPMOST = -1, -2
    flags = 0x0002 | 0x0001 | 0x0040  # NOMOVE | NOSIZE | SHOWWINDOW
    user32.ShowWindow(hwnd, 9)  # SW_RESTORE
    # 保持 TOPMOST 直到截完图，否则 BitBlt 抓到的仍是被遮挡的内容
    user32.SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, flags)
    user32.SetForegroundWindow(hwnd)
    time.sleep(0.9)

    rect = wintypes.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(rect))
    x, y = rect.left, rect.top
    W, H = rect.right - rect.left, rect.bottom - rect.top
    print(f"窗口位置：({x},{y}) 尺寸：{W}x{H}")

hdc_screen = user32.GetDC(0)
hdc_mem = gdi32.CreateCompatibleDC(hdc_screen)
hbmp = gdi32.CreateCompatibleBitmap(hdc_screen, W, H)
gdi32.SelectObject(hdc_mem, hbmp)

if hwnd:
    # PrintWindow 取窗口自身内容，不受遮挡影响（PW_RENDERFULLCONTENT=2，
    # 对 GPU 渲染的窗口才拿得到画面）；失败再退回屏幕 BitBlt。
    ok = user32.PrintWindow(hwnd, hdc_mem, 2)
    print(f"PrintWindow -> {ok}")
    if not ok:
        gdi32.BitBlt(hdc_mem, 0, 0, W, H, hdc_screen, x, y, 0x00CC0020)
else:
    gdi32.BitBlt(hdc_mem, 0, 0, W, H, hdc_screen, x, y, 0x00CC0020)


class BITMAPINFOHEADER(ctypes.Structure):
    _fields_ = [
        ("biSize", wintypes.DWORD),
        ("biWidth", ctypes.c_int),
        ("biHeight", ctypes.c_int),
        ("biPlanes", wintypes.WORD),
        ("biBitCount", wintypes.WORD),
        ("biCompression", wintypes.DWORD),
        ("biSizeImage", wintypes.DWORD),
        ("biXPelsPerMeter", ctypes.c_int),
        ("biYPelsPerMeter", ctypes.c_int),
        ("biClrUsed", wintypes.DWORD),
        ("biClrImportant", wintypes.DWORD),
    ]


class BITMAPINFO(ctypes.Structure):
    _fields_ = [("bmiHeader", BITMAPINFOHEADER), ("bmiColors", wintypes.DWORD * 3)]


bi = BITMAPINFO()
bi.bmiHeader.biSize = ctypes.sizeof(BITMAPINFOHEADER)
bi.bmiHeader.biWidth = W
bi.bmiHeader.biHeight = -H
bi.bmiHeader.biPlanes = 1
bi.bmiHeader.biBitCount = 32
bi.bmiHeader.biCompression = 0

buf = ctypes.create_string_buffer(W * H * 4)
gdi32.GetDIBits(hdc_mem, hbmp, 0, H, buf, ctypes.byref(bi), 0)
gdi32.DeleteObject(hbmp)
gdi32.DeleteDC(hdc_mem)
user32.ReleaseDC(0, hdc_screen)
if hwnd:
    # 截完再恢复普通层级
    user32.SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, flags)

# 可选：裁剪 + 放大（窗口坐标），便于在截图里精确读出控件位置
if len(sys.argv) >= 7:
    cx, cy, cw, ch = (int(v) for v in sys.argv[3:7])
    scale = int(sys.argv[7]) if len(sys.argv) > 7 else 3
    crop = bytearray(cw * ch * 4)
    for yy in range(ch):
        src = (cy + yy) * W * 4 + cx * 4
        crop[yy * cw * 4 : (yy + 1) * cw * 4] = buf[src : src + cw * 4]
    up_w, up_h = cw * scale, ch * scale
    up = bytearray(up_w * up_h * 4)
    for yy in range(up_h):
        srow = (yy // scale) * cw * 4
        for xx in range(up_w):
            s = srow + (xx // scale) * 4
            d = yy * up_w * 4 + xx * 4
            up[d : d + 4] = crop[s : s + 4]
    buf = up
    W, H = up_w, up_h

raw = bytearray()
stride = W * 4
for yy in range(H):
    raw.append(0)
    row = buf[yy * stride : (yy + 1) * stride]
    raw += bytes(b for i in range(0, stride, 4) for b in (row[i + 2], row[i + 1], row[i], 255))


def chunk(tag, data):
    return (
        struct.pack(">I", len(data))
        + tag
        + data
        + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    )


png = b"\x89PNG\r\n\x1a\n"
png += chunk(b"IHDR", struct.pack(">IIBBBBB", W, H, 8, 6, 0, 0, 0))
png += chunk(b"IDAT", zlib.compress(bytes(raw), 6))
png += chunk(b"IEND", b"")

with open(OUT, "wb") as f:
    f.write(png)
print(f"saved {OUT} ({W}x{H})")
