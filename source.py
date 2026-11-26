"""Hosted Windows smoke for the extracted synthetic notebook package."""
import hashlib
import json
import os
import tempfile
import time
from pathlib import Path
from uuid import UUID

import psutil
from PIL import ImageGrab
from rapidocr_onnxruntime import RapidOCR


MARKER = "Amber Meadow"
SAMPLE = "7174bca8c2e54b1852e66bcbcc941a1429242f61e32e12dafb06ce050e65d08e"


def find_label(rows, text, *, toolbar=False, exact=False):
    wanted = text.casefold().replace(" ", "")
    for points, label, _ in rows:
        if toolbar and max(p[1] for p in points) > 170:
            continue
        observed = label.casefold().replace(" ", "")
        if (observed == wanted if exact else wanted in observed):
            return tuple(round(sum(p[i] for p in points) / 4) for i in (0, 1))
    return None


def edited_source(before):
    if hashlib.sha256(before.replace(b"\r\n", b"\n")).hexdigest() != SAMPLE:
        raise RuntimeError("Not the bundled synthetic Notebook.ipynb")
    sample = json.loads(before)
    sample["cells"][0]["source"] = [f"# {MARKER}"]
    return json.dumps(sample, indent=1)


def main():
    if (os.name, os.getenv("RUNNER_ENVIRONMENT"), os.getenv("RUNNER_OS"), os.getenv("RUNNER_ARCH")) != (
        "nt", "github-hosted", "Windows", "X64"
    ):
        raise RuntimeError("Use an isolated, standard GitHub-hosted Windows x64 runner")
    import ctypes
    import pywintypes
    import win32api
    import win32clipboard
    import win32con as wc
    import win32gui
    import win32process

    ctypes.windll.user32.SetThreadDpiAwarenessContext(ctypes.c_void_p(-4))
    root = Path(os.environ["RUNNER_TEMP"]).resolve()
    package = root / "preview"
    exe = (package / "Zed.exe").resolve(strict=True)
    notebook = (package / "Notebook.ipynb").resolve(strict=True)
    launcher = (package / "Open Notebook.cmd").resolve(strict=True)
    before = notebook.read_bytes()
    replacement = edited_source(before)
    proof = root / "proof"
    proof.mkdir(exist_ok=True)
    ocr = RapidOCR(intra_op_num_threads=1, inter_op_num_threads=1)
    temp = Path(tempfile.mkdtemp(prefix="launcher-test-", dir=root))
    os.environ["TEMP"] = os.environ["TMP"] = str(temp)
    system = Path(os.environ["SystemRoot"])
    os.environ["PATH"] = os.pathsep.join(map(str, [system / "System32", system]))
    for name in ("LOCAL_NOTEBOOK_DEV", "JUPYTER_SERVER", "JUPYTER_TOKEN", "PYTHONPATH",
                 "VIRTUAL_ENV", "CONDA_PREFIX", "CARGO_HOME", "RUSTUP_HOME",
                 "CARGO_MANIFEST_DIR", "CARGO_TARGET_DIR"):
        os.environ.pop(name, None)
    existing = set(psutil.pids())
    os.startfile(str(launcher), "open", cwd=str(temp))
    process = None
    try:
        deadline = time.monotonic() + 90
        while process is None and time.monotonic() < deadline:
            for p in psutil.process_iter(["name"]):
                if p.pid in existing or (p.info["name"] or "").lower() != "zed.exe":
                    continue
                try:
                    if Path(p.exe()) != exe:
                        continue
                    args = p.cmdline()
                except (psutil.AccessDenied, psutil.NoSuchProcess):
                    continue
                if "--user-data-dir" not in args or str(notebook).casefold() not in [a.casefold() for a in args]:
                    continue
                profile = Path(args[args.index("--user-data-dir") + 1])
                if profile.parent != temp or not profile.name.startswith("zed-notebook-"):
                    continue
                UUID(profile.name.removeprefix("zed-notebook-"))
                process = p
                break
            time.sleep(.2)
        if process is None:
            raise RuntimeError("The shell launcher did not open its bundled sample with a fresh profile")
        inherited_path = next(value for name, value in process.environ().items() if name.lower() == "path")
        if inherited_path.casefold() != os.environ["PATH"].casefold():
            raise RuntimeError("Packaged Zed did not inherit the system-only PATH")
        print(f"Shell opened {exe}, PID {process.pid}, profile {profile}, PATH={inherited_path}", flush=True)

        def collect(hwnd, handles):
            if (win32gui.IsWindowVisible(hwnd) and win32gui.GetWindowText(hwnd)
                    and win32process.GetWindowThreadProcessId(hwnd)[1] == process.pid):
                handles.append(hwnd)
            return True

        handles = []
        deadline = time.monotonic() + 90
        while not handles and time.monotonic() < deadline:
            win32gui.EnumWindows(collect, handles)
            time.sleep(.25)
        if not handles:
            raise RuntimeError("Launcher process has no visible window")
        hwnd = handles[0]
        win32gui.ShowWindow(hwnd, wc.SW_MAXIMIZE)
        for _ in range(15):
            if win32gui.GetForegroundWindow() == hwnd:
                break
            win32api.keybd_event(wc.VK_MENU, 0, 0, 0)
            win32api.keybd_event(wc.VK_MENU, 0, wc.KEYEVENTF_KEYUP, 0)
            try:
                win32gui.SetForegroundWindow(hwnd)
            except pywintypes.error:
                pass
            time.sleep(.3)

        def require_focus():
            if (not process.is_running() or win32gui.GetForegroundWindow() != hwnd
                    or win32process.GetWindowThreadProcessId(hwnd)[1] != process.pid):
                raise RuntimeError("Packaged Zed lost foreground ownership")

        def click(point, origin):
            require_focus()
            x, y = (p + offset for p, offset in zip(point, origin))
            if win32gui.GetAncestor(win32gui.WindowFromPoint((x, y)), wc.GA_ROOT) != hwnd:
                raise RuntimeError("Control is covered or outside the launcher window")
            win32api.SetCursorPos((x, y))
            win32api.mouse_event(wc.MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0)
            win32api.mouse_event(wc.MOUSEEVENTF_LEFTUP, 0, 0, 0, 0)
            print(f"Clicked ({x}, {y})", flush=True)

        def key(*keys):
            require_focus()
            for k in keys:
                win32api.keybd_event(k, 0, 0, 0)
            for k in reversed(keys):
                win32api.keybd_event(k, 0, wc.KEYEVENTF_KEYUP, 0)
            time.sleep(.25)

        def observe(stage, button, marker=False):
            deadline = time.monotonic() + 75
            while time.monotonic() < deadline:
                require_focus()
                left, top, right, bottom = win32gui.GetWindowRect(hwnd)
                bounds = (max(left, 0), max(top, 0), min(right, win32api.GetSystemMetrics(0)),
                          min(bottom, win32api.GetSystemMetrics(1)))
                image = ImageGrab.grab(bbox=bounds)
                require_focus()
                image.save(proof / f"source-{stage}.png")
                rows, _ = ocr(image)
                rows = rows or []
                labels = [label for _, label, _ in rows]
                (proof / f"source-{stage}.txt").write_text("\n".join(labels), encoding="utf-8")
                if stage == "launched":
                    restricted = find_label(rows, "Stay in Restricted Mode")
                    if restricted:
                        click(restricted, bounds[:2])
                        time.sleep(1)
                        continue
                point = find_label(rows, button, toolbar=True)
                if point and (not marker or find_label(rows, MARKER, exact=stage == "returned")):
                    print(f"{stage}: found {button}" + (f" and {MARKER}" if marker else ""), flush=True)
                    return point, bounds[:2]
                time.sleep(.5)
            raise RuntimeError(f"{stage}: missing {button} or expected text; see source-{stage}.png")

        click(*observe("launched", "Edit Source"))
        observe("raw", "Back to Notebook")
        key(wc.VK_CONTROL, ord("A"))
        win32clipboard.OpenClipboard()
        try:
            win32clipboard.EmptyClipboard()
            win32clipboard.SetClipboardText(replacement, wc.CF_UNICODETEXT)
        finally:
            win32clipboard.CloseClipboard()
        key(wc.VK_CONTROL, ord("V"))
        key(wc.VK_CONTROL, wc.VK_HOME)
        click(*observe("unsaved", "Back to Notebook", marker=True))
        observe("returned", "Edit Source", marker=True)
        if notebook.read_bytes() != before:
            raise RuntimeError("The edit reached disk")
        print(f"PASS: rendered unsaved heading after return; unchanged file SHA256={hashlib.sha256(before).hexdigest()}")
    finally:
        if process and process.is_running():
            process.terminate()
            process.wait(timeout=15)


if __name__ == "__main__":
    main()
