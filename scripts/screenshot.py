"""启动 rust-dm，截取它自己的窗口，输出 PNG（纯 ctypes，无第三方依赖）。

用法：
    python scripts/screenshot.py <exe> <out.png> [wait_secs]

仅用于人工核对界面渲染（例如中文字体是否正常），不属于产品代码。
"""

import ctypes
import struct
import subprocess
import sys
import time
import zlib
from ctypes import wintypes

EXE = sys.argv[1]
OUT = sys.argv[2]
WAIT = float(sys.argv[3]) if len(sys.argv) > 3 else 8.0
# 第四个参数（可选）：程序起来后往剪贴板里塞一段文本，用于验证剪贴板监听
CLIP = sys.argv[4] if len(sys.argv) > 4 else None
TITLE_KEY = "rust-dm"

user32 = ctypes.WinDLL("user32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)

user32.GetSystemMetrics.restype = ctypes.c_int
SCREEN_W = user32.GetSystemMetrics(0)
SCREEN_H = user32.GetSystemMetrics(1)


def find_window(key: str, timeout: float):
    """按标题子串找可见窗口，返回 hwnd。"""
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
            found.append(hwnd)
            return False
        return True

    deadline = time.time() + timeout
    while time.time() < deadline:
        found.clear()
        user32.EnumWindows(cb, 0)
        if found:
            return found[0]
        time.sleep(0.5)
    return None


proc = subprocess.Popen([EXE])
hwnd = find_window(TITLE_KEY, max(WAIT, 15.0))
if not hwnd:
    print("没找到窗口，退回全屏截图", file=sys.stderr)

if hwnd:
    # 还原并置顶：SetForegroundWindow 在后台进程上常被系统拒绝，
    # 所以先临时切成 TOPMOST 再切回来，确保截到的是本程序窗口。
    HWND_TOPMOST, HWND_NOTOPMOST = -1, -2
    SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW = 0x0002, 0x0001, 0x0040
    flags = SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW
    user32.ShowWindow(hwnd, 9)  # SW_RESTORE
    user32.SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, flags)
    user32.SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, flags)
    user32.SetForegroundWindow(hwnd)
    time.sleep(1.5)
    if CLIP:
        # 程序启动时会把已有剪贴板记为"已见过"，所以要等它起来再写入
        subprocess.run(
            [
                "powershell",
                "-NoProfile",
                "-Command",
                "Set-Clipboard -Value '" + CLIP.replace("'", "''") + "'",
            ],
            check=False,
            capture_output=True,
        )
        time.sleep(2.0)
    rect = wintypes.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(rect))
    x, y = rect.left, rect.top
    W, H = rect.right - rect.left, rect.bottom - rect.top
else:
    x, y, W, H = 0, 0, SCREEN_W, SCREEN_H

hdc_screen = user32.GetDC(0)
hdc_mem = gdi32.CreateCompatibleDC(hdc_screen)
hbmp = gdi32.CreateCompatibleBitmap(hdc_screen, W, H)
gdi32.SelectObject(hdc_mem, hbmp)
gdi32.BitBlt(hdc_mem, 0, 0, W, H, hdc_screen, x, y, 0x00CC0020)  # SRCCOPY


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
bi.bmiHeader.biHeight = -H  # 负号 = 自顶向下
bi.bmiHeader.biPlanes = 1
bi.bmiHeader.biBitCount = 32
bi.bmiHeader.biCompression = 0

buf = ctypes.create_string_buffer(W * H * 4)
gdi32.GetDIBits(hdc_mem, hbmp, 0, H, buf, ctypes.byref(bi), 0)

gdi32.DeleteObject(hbmp)
gdi32.DeleteDC(hdc_mem)
user32.ReleaseDC(0, hdc_screen)

raw = bytearray()
stride = W * 4
for yy in range(H):
    raw.append(0)  # filter type 0
    row = buf[yy * stride : (yy + 1) * stride]
    # BGRA -> RGBA
    raw += bytes(
        b for i in range(0, stride, 4) for b in (row[i + 2], row[i + 1], row[i], 255)
    )


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

proc.terminate()
try:
    proc.wait(timeout=5)
except Exception:
    proc.kill()

print(f"saved {OUT} ({W}x{H}, {len(png)} bytes)")
