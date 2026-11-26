import base64
import ctypes
import hashlib
import io
import json
import os
import subprocess
import time
from ctypes import wintypes
from pathlib import Path

from PIL import Image, ImageDraw, ImageGrab

user = ctypes.windll.user32
user.SetProcessDPIAware()
root = Path(os.environ["RUNNER_TEMP"])
proof = root / "proof"
proof.mkdir(exist_ok=True)


def notebook(count, name):
    cells = []
    for i in range(count):
        width = 1900 if count > 1 and i == 0 else 200
        im = Image.new("RGB", (width, 48), (16+i, 94, 137))
        draw = ImageDraw.Draw(im)
        for x in range(0, width, 100):
            draw.text((x+4, 16), f"output {i:02}: {x}", fill="white")
        buf = io.BytesIO()
        im.save(buf, format="PNG")
        data = base64.b64encode(buf.getvalue()).decode()
        cells.append(dict(cell_type="code", id=f"cell-{i}", source=[f"# Cell {i:02}"], execution_count=i+1, metadata={}, outputs=[dict(output_type="display_data", metadata={}, data={"image/png": data})]))
    path = root / (name + ".ipynb")
    path.write_text(json.dumps(dict(cells=cells, nbformat=4, nbformat_minor=5, metadata={"language_info":{"name":"python"}})))
    return path


def windows(pid):
    handles = []
    def found(hwnd, _):
        owner = wintypes.DWORD()
        user.GetWindowThreadProcessId(hwnd, ctypes.byref(owner))
        if owner.value == pid and user.IsWindowVisible(hwnd) and user.GetWindowTextLengthW(hwnd):
            handles.append(hwnd)
        return True
    user.EnumWindows(ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)(found), 0)
    return handles


def focus(pid):
    for _ in range(40):
        parts = windows(pid)
        if parts:
            hwnd = parts[0]
            user.ShowWindow(hwnd, 3)
            if user.GetForegroundWindow() == hwnd:
                return
            user.keybd_event(18,0,0,0)
            user.keybd_event(18,0,2,0)
            user.SetForegroundWindow(hwnd)
        time.sleep(.3)
    raise RuntimeError("Zed did not acquire focus")


def shot(name):
    time.sleep(.35)
    im = ImageGrab.grab().convert("RGB")
    im.save(proof / (name+".png"))
    return im


def move(x,y):
    if not user.SetCursorPos(int(x),int(y)):
        raise RuntimeError("Cursor move failed")


def mouse(flag,data=0):
    user.mouse_event(flag,0,0,data,0)


def wheel(steps):
    move(30,410)
    mouse(0x800, 120*steps)
    time.sleep(.5)


def key(*keys):
    for k in keys: user.keybd_event(k,0,0,0)
    for k in reversed(keys): user.keybd_event(k,0,2,0)
    time.sleep(.4)


def exercise(count, name):
    path = notebook(count, name)
    dest = root / ("profile-"+name)
    dest.mkdir()
    proc = subprocess.Popen([str(root/"preview"/"Zed.exe"), "--user-data-dir",str(dest),str(path)],cwd=root)
    try:
        focus(proc.pid)
        time.sleep(8)
        focus(proc.pid)
        snap = shot(name+"-open")
        assert snap.size == (1024,768), snap.size
        wheel(-3)
        shot(name+"-wheel")
        wheel(100)
        shot(name+"-return")
    finally:
        proc.terminate()
        proc.wait(15)


for count,name in [(30,"long"),(1,"short")]:
    exercise(count,name)
