"""模拟鼠标点击 / 按键，用于人工核对界面交互（纯 ctypes，无第三方依赖）。

用法：
    python scripts/ui_click.py click <x> <y>     # 屏幕坐标，左键单击
    python scripts/ui_click.py key <vk>          # 按下并松开某个虚拟键（如 27 = ESC）
"""

import ctypes
import sys
import time
from ctypes import wintypes

user32 = ctypes.WinDLL("user32", use_last_error=True)
MOUSEEVENTF_LEFTDOWN = 0x0002
MOUSEEVENTF_LEFTUP = 0x0004
KEYEVENTF_KEYUP = 0x0002

def bring_to_front(key):
    """把标题包含 key 的窗口临时置顶，否则点击会落到遮挡它的窗口上。"""
    found = []

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def cb(hwnd, _lparam):
        n = user32.GetWindowTextLengthW(hwnd)
        if n <= 0 or not user32.IsWindowVisible(hwnd):
            return True
        buf = ctypes.create_unicode_buffer(n + 1)
        user32.GetWindowTextW(hwnd, buf, n + 1)
        if key.lower() in buf.value.lower():
            found.append(hwnd)
        return True

    user32.EnumWindows(cb, 0)
    if not found:
        return None
    hwnd = found[0]
    flags = 0x0002 | 0x0001 | 0x0040
    user32.ShowWindow(hwnd, 9)
    user32.SetWindowPos(hwnd, -1, 0, 0, 0, 0, flags)  # HWND_TOPMOST

    # 后台进程直接 SetForegroundWindow 会被系统拒绝，
    # 通过 AttachThreadInput 挂到当前前台线程上才能真的抢到焦点
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    fg = user32.GetForegroundWindow()
    fg_thread = user32.GetWindowThreadProcessId(fg, None)
    my_thread = kernel32.GetCurrentThreadId()
    attached = user32.AttachThreadInput(my_thread, fg_thread, True)
    user32.BringWindowToTop(hwnd)
    user32.SetForegroundWindow(hwnd)
    user32.SetFocus(hwnd)
    if attached:
        user32.AttachThreadInput(my_thread, fg_thread, False)
    time.sleep(0.6)
    return hwnd


def find_main(key):
    found = []

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def cb(hwnd, _lparam):
        n = user32.GetWindowTextLengthW(hwnd)
        if n <= 0 or not user32.IsWindowVisible(hwnd):
            return True
        buf = ctypes.create_unicode_buffer(n + 1)
        user32.GetWindowTextW(hwnd, buf, n + 1)
        if key.lower() in buf.value.lower():
            r = wintypes.RECT()
            user32.GetWindowRect(hwnd, ctypes.byref(r))
            found.append(((r.right - r.left) * (r.bottom - r.top), hwnd))
        return True

    user32.EnumWindows(cb, 0)
    return max(found)[1] if found else None


action = sys.argv[1]

if action == "msgclick":
    # 直接给窗口投递鼠标消息（不受遮挡/前台限制），参数为窗口坐标
    x, y = int(sys.argv[2]), int(sys.argv[3])
    hwnd = find_main("rust-dm")
    if not hwnd:
        print("没找到窗口", file=sys.stderr)
        sys.exit(1)
    # 窗口坐标 → 客户区坐标：用客户区原点与窗口原点的差值算偏移（DPI 无关）
    wr = wintypes.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(wr))
    origin = wintypes.POINT(0, 0)
    user32.ClientToScreen(hwnd, ctypes.byref(origin))
    cx = x - (origin.x - wr.left)
    cy = y - (origin.y - wr.top)
    client = wintypes.POINT(cx, cy)
    lp = ((cy & 0xFFFF) << 16) | (cx & 0xFFFF)
    user32.PostMessageW(hwnd, 0x0200, 0, lp)  # WM_MOUSEMOVE
    time.sleep(0.1)
    user32.PostMessageW(hwnd, 0x0201, 1, lp)  # WM_LBUTTONDOWN
    time.sleep(0.08)
    user32.PostMessageW(hwnd, 0x0202, 0, lp)  # WM_LBUTTONUP
    print(f"msgclick window=({x},{y}) client=({client.x},{client.y})")
elif action == "click":
    x, y = int(sys.argv[2]), int(sys.argv[3])
    bring_to_front("rust-dm")
    user32.SetCursorPos(x, y)
    time.sleep(0.3)
    # 第一次点击可能只用于激活窗口（Windows 会吞掉这次按下），所以点两次
    for _ in range(2):
        user32.mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0)
        time.sleep(0.06)
        user32.mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0)
        time.sleep(0.45)
    print(f"clicked ({x},{y})")
elif action == "wclick":
    # 窗口相对坐标点击（自动换算成屏幕坐标，避免窗口位置漂移带来误差）
    wx, wy = int(sys.argv[2]), int(sys.argv[3])
    hwnd = find_main("rust-dm")
    if not hwnd:
        print("没找到窗口", file=sys.stderr)
        sys.exit(1)
    # 先置前（可能会移动窗口），再读窗口矩形，否则点击坐标会偏
    bring_to_front("rust-dm")
    hwnd = find_main("rust-dm")
    wr = wintypes.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(wr))
    x, y = wr.left + wx, wr.top + wy
    user32.SetCursorPos(x, y)
    time.sleep(0.4)
    # 第一次点击可能只用于把窗口激活（系统会吞掉这次按下），所以点两次；
    # 间隔拉长到 0.6s，避免被识别成双击
    for _ in range(2):
        user32.mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0)
        time.sleep(0.06)
        user32.mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0)
        time.sleep(0.6)
    print(f"wclick window=({wx},{wy}) screen=({x},{y})")
elif action == "ctrlkey":
    bring_to_front("rust-dm")
    time.sleep(0.4)
    vk = int(sys.argv[2])
    VK_CONTROL = 0x11
    user32.keybd_event(VK_CONTROL, 0, 0, 0)
    time.sleep(0.08)
    user32.keybd_event(vk, 0, 0, 0)
    time.sleep(0.08)
    user32.keybd_event(vk, 0, KEYEVENTF_KEYUP, 0)
    time.sleep(0.05)
    user32.keybd_event(VK_CONTROL, 0, KEYEVENTF_KEYUP, 0)
    print(f"ctrl+{chr(vk)}")
else:
    vk = int(sys.argv[2])
    user32.keybd_event(vk, 0, 0, 0)
    time.sleep(0.05)
    user32.keybd_event(vk, 0, KEYEVENTF_KEYUP, 0)
    print(f"key {vk}")
