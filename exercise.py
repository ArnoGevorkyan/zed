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

from PIL import Image, ImageChops, ImageDraw, ImageGrab

user = ctypes.windll.user32
user.SetProcessDPIAware()
root = Path(os.environ["RUNNER_TEMP"])
proof = root / "proof"
proof.mkdir(exist_ok=True)
candidate = os.environ.get("SCROLL_CANDIDATE") == "1"
failures = []

def check(name, actual, expected):
    passed = actual == expected
    line = f"{name}: observed {actual}, expected {expected}: {'PASS' if passed else 'FAIL'}"
    with (proof/"checks.txt").open("a") as f:
        print(line,file=f)
    if not passed:
        failures.append(name)


def notebook(count, name):
    cells = []
    for i in range(count):
        width = 1900 if count > 1 and i == 0 else 200
        im = Image.new("RGB", (width, 48), (16+i, 94, 137))
        draw = ImageDraw.Draw(im)
        for x in range(0, width, 100):
            if x // 100 % 2:
                draw.rectangle((x,0,x+99,47), fill=(16+i,94,103))
            draw.text((x+4, 16), f"output {i:02}: {x}", fill="white")
        buf = io.BytesIO()
        im.save(buf, format="PNG")
        data = base64.b64encode(buf.getvalue()).decode()
        cells.append(dict(cell_type="code", id=f"cell-{i}", source=[f"# Cell {i:02}"], execution_count=i+1, metadata={}, outputs=[dict(output_type="display_data", metadata={}, data={"image/png": data})]))
        if i == 1:
            cells[-1]["outputs"].append(dict(output_type="display_data",metadata={},data={"text/markdown":"**Plain Markdown result** without a link or editor."}))
        if i == 2:
            cells[-1] = dict(cell_type="markdown",id="cell-2",source=["**Rendered Markdown cell 02**"],metadata={})
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


def markers(im):
    marks = {}
    for y in range(134,700):
        r,g,b = im.getpixel((63,y))
        if 16 <= r < 46 and g == 94 and b in (103,137):
            marks.setdefault(r-16, []).append(y)
    return {i:(min(ys),max(ys)) for i,ys in marks.items()}


def capture(name):
    im = shot(name)
    m = markers(im)
    print(name, m, flush=True)
    with (proof/"observations.txt").open("a") as f:
        print(name, m, file=f)
    return im,m


def drag(x,y,x2,y2):
    move(x,y)
    time.sleep(.2)
    mouse(2)
    for j in range(1,13):
        move(x+(x2-x)*j/12,y+(y2-y)*j/12)
        time.sleep(.04)
    mouse(4)


def top(name):
    wheel(110)
    wheel(110)
    im,m = capture(name)
    assert 0 in m, (name,m)
    return im,m


def run_inputs(count,name):
    im,initial = top(name+"-top")
    if count > 1:
        wheel(-4)
        _,m = capture(name+"-wheel-down")
        check("ordinary wheel moves a long page", m != initial, True)
        top(name+"-wheel-up")
    drag(1018,155,1018,650)
    _,m = capture(name+"-vertical-drag")
    check(name+" vertical thumb moves the page",m != initial,candidate and count>1)
    im,initial = top(name+"-restore")
    y = initial[0][1]+5
    crop = (70,initial[0][0]+3,970,initial[0][1]-6)
    drag(210,y,800,y)
    after,m = capture(name+"-horizontal-drag")
    changed = ImageChops.difference(im.crop(crop),after.crop(crop)).getbbox() is not None
    check(name+" horizontal thumb pans only the output", (changed,m==initial), (candidate and count>1,True))
    move(230,initial[0][0]+8)
    mouse(0x1000,-480)
    time.sleep(.4)
    wheel_left,m_left = capture(name+"-horizontal-wheel-left")
    mouse(0x1000,480)
    wheel_im,m = capture(name+"-horizontal-wheel")
    wheel_changed = ImageChops.difference(wheel_left.crop(crop),wheel_im.crop(crop)).getbbox() is not None
    check(name+" horizontal wheel pans only the output",(wheel_changed,m_left==m==initial),(count>1,True))
    if count == 1:
        wheel(-8)
        _,m = capture(name+"-wheel-down")
        check("short wheel leaves page in place",m,initial)
        move(600,500)
        mouse(32); mouse(64)
        move(600,580)
        time.sleep(.8)
        _,m = capture(name+"-middle")
        check("short middle click leaves page in place",m,initial)
        key(27)
        return
    for section in ["page","output","margin","mime-fallback","markdown-cell"]:
        _,m = top(name+"-"+section+"-start")
        y1,y2 = m[1]
        x,y = {"page":(800,(y1+y2)//2),"output":(120,(y1+y2)//2),
               "margin":(19,(y1+y2)//2),"mime-fallback":(150,y2+13),"markdown-cell":(153,y2+70)}[section]
        move(x,y)
        mouse(32); mouse(64)
        move(x,y+52)
        time.sleep(.45)
        _,moving = capture(name+"-"+section+"-moving")
        check(section+" causes vertical movement",moving != m,candidate)
        if section in ("page","margin"):
            mouse(2); mouse(4)
        else:
            key(27)
        _,stopped = capture(name+"-"+section+"-cancel")
        move(x,y+108)
        time.sleep(.65)
        _,later = capture(name+"-"+section+"-stopped")
        check(section+" remains still after cancellation",stopped,later)


def exercise(count, name):
    path = notebook(count, name)
    dest = root / ("profile-"+name)
    dest.mkdir()
    proc = subprocess.Popen([str(root/"preview"/"Zed.exe"), "--user-data-dir",str(dest),str(path)],cwd=root)
    try:
        focus(proc.pid)
        time.sleep(8)
        focus(proc.pid)
        key(13)
        time.sleep(1)
        snap = shot(name+"-open")
        assert snap.size == (1024,768), snap.size
        before = path.read_bytes()
        run_inputs(count,name)
        key(90)
        key(17,83)
        assert path.read_bytes() == before, "mode probe or mouse input edited the saved notebook"
        top(name+"-selection-visible-again")
        key(13)
        key(35)
        key(90)
        key(17,83)
        sources = ["".join(c["source"]) for c in json.loads(path.read_text())["cells"]]
        expected_sources = ["".join(c["source"]) for c in json.loads(before)["cells"]]
        expected_sources[0] += "z"
        check(name+" deliberate edit affects only initially selected cell",sources,expected_sources)
        if sources == expected_sources:
            (proof/(name+"-selection.txt")).write_text("No edits on plain Z + save in command mode; explicit Enter/Edit/Z changed only the originally selected cell 00.")
        if count > 1:
            _,m = top(name+"-editing-start")
            move(19,182)
            mouse(32); mouse(64)
            move(19,198)
            time.sleep(.4)
            _,moving=capture(name+"-editing-moving")
            check("editing: margin activates scroll",moving!=m,candidate)
            key(27)
            _,stopped=capture(name+"-editing-escape")
            move(19,239)
            time.sleep(.7)
            _,later=capture(name+"-editing-stopped")
            check("editing: first Escape stops scroll",later,stopped)
    finally:
        proc.terminate()
        proc.wait(15)


for count,name in [(30,"long"),(1,"short")]:
    exercise(count,name)
if failures:
    raise RuntimeError(f"Interaction assertions failed: {failures}")
