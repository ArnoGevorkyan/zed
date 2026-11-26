import hashlib
import json
import ctypes
import os
from pathlib import Path
import subprocess
import sys
import time

import AppKit as A
import Metal
import Quartz as Q
import psutil

root = Path(os.environ["NOTEBOOK_ROOT"])
project = root / "project"
file = project / "Signal study.ipynb"
proof = root / "proof"
device = Metal.MTLCreateSystemDefaultDevice()
screen = Q.CGPreflightScreenCaptureAccess()
post = Q.CGPreflightPostEventAccess()
ax = bool(ctypes.CDLL("/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices").AXIsProcessTrusted())
print(f"Metal={device.name() if device else None}; display={Q.CGMainDisplayID()}; "
      f"screens={len(A.NSScreen.screens())}; screen_capture={screen}; accessibility={ax}; post_events={post}", flush=True)
if sys.argv[1] == "probe":
    sys.exit()

def run(*args, **kw):
    return subprocess.run(args, check=True, **kw)

def windows(app):
    rows = Q.CGWindowListCopyWindowInfo(Q.kCGWindowListOptionOnScreenOnly, Q.kCGNullWindowID) or []
    return [w for w in rows if w.get("kCGWindowOwnerPID") == app.pid and w["kCGWindowLayer"] == 0]

def launch(label):
    app = subprocess.Popen([os.environ["ZED_EXE"], "--user-data-dir", root/("profile-" + label), project, file],
                           stdout=(root/f"{label}.log").open("w"), stderr=subprocess.STDOUT)
    for _ in range(60):
        if app.poll() is not None:
            raise RuntimeError(f"Zed exited {app.returncode} before a window appeared")
        if windows(app):
            print(f"{label}: Zed pid={app.pid}, window={windows(app)[0]['kCGWindowNumber']}", flush=True)
            return app
        time.sleep(1)
    raise RuntimeError("No on-screen Zed window within 60 seconds")

def shot(app, name):
    if not screen:
        raise RuntimeError("Screen recording permission not granted (CGPreflightScreenCaptureAccess)")
    run("screencapture", "-x", "-o", "-l", str(windows(app)[0]["kCGWindowNumber"]), str(proof/f"{name}.png"))

def key(code, flags=0, delay=.6):
    if not ax or not post:
        raise RuntimeError("Accessibility or event-posting permission not granted")
    if A.NSWorkspace.sharedWorkspace().frontmostApplication().processIdentifier() != app.pid:
        raise RuntimeError("Zed is not the foreground application")
    for down in [True, False]:
        event = Q.CGEventCreateKeyboardEvent(None, code, down)
        Q.CGEventSetFlags(event, flags)
        Q.CGEventPost(Q.kCGHIDEventTap, event)
    time.sleep(delay)

def kernels():
    found = []
    for process in psutil.process_iter(["pid", "ppid", "cmdline"]):
        args = process.info["cmdline"] or []
        if any("ipykernel_launcher" in arg or "from ipykernel.kernelapp import" in arg for arg in args):
            found.append((process.pid, process.info["ppid"]))
    return found

def focus(app):
    native = A.NSRunningApplication.runningApplicationWithProcessIdentifier_(app.pid)
    native.activateWithOptions_(A.NSApplicationActivateIgnoringOtherApps)
    for _ in range(30):
        A.NSRunLoop.currentRunLoop().runUntilDate_(A.NSDate.dateWithTimeIntervalSinceNow_(0.2))
        if A.NSWorkspace.sharedWorkspace().frontmostApplication().processIdentifier() == app.pid:
            return
    raise RuntimeError("Could not activate the verified Zed window")

def paste(value):
    run("pbcopy", input=value.encode())
    key(9, cmd)

def saved(label, save_all=False):
    key(1, cmd | (option if save_all else 0), delay=1)
    (proof / (label + ".ipynb")).write_bytes(file.read_bytes())
    shot(app, label)

cmd, option, shift, ctrl = (Q.kCGEventFlagMaskCommand, Q.kCGEventFlagMaskAlternate,
                            Q.kCGEventFlagMaskShift, Q.kCGEventFlagMaskControl)
assert screen and post and ax and device
for case in ["Empty", "Rich"]:
    file = project / (case + ".ipynb")
    for phase in ["edit", "reopen"]:
        label = f"{case}-{phase}"
        config = root / ("profile-" + label) / "config"
        config.mkdir(parents=True)
        (config / "settings.json").write_text(json.dumps({"enable_language_server":False,
            "show_completions_on_input":False,"auto_update":False,"telemetry":{"diagnostics":False,"metrics":False}}))
        app = launch(label)
        try:
            time.sleep(8)
            focus(app)
            shot(app, label + "-prompt")
            key(1, cmd | ctrl, delay=2)
            shot(app, label + "-restricted")
            if phase == "reopen":
                (proof / (case.lower() + "-reopened.ipynb")).write_bytes(file.read_bytes())
            elif case == "Empty":
                key(46, cmd | shift)
                paste("# Mac structure")
                key(53)
                key(46, cmd)
                paste("value = 17")
                key(53)
                saved("empty-added")
                key(126, option)
                saved("empty-up")
                key(125, option)
                saved("empty-down")
                key(51)
                saved("empty-deleted")
                key(36)
                key(125, cmd)
                paste(" edited")
                key(53)
                key(6, cmd)
                saved("empty-undone")
                key(6, cmd | shift)
                saved("empty-redone")
                key(6, cmd)
                saved("empty-final", True)
            else:
                key(125, option)
                saved("rich-moved")
                key(51)
                saved("rich-deleted")
                key(6, cmd)
                saved("rich-undone", True)
            assert not kernels(), "Unexpected ipykernel"
        except Exception:
            if app.poll() is None and windows(app):
                shot(app, label + "-failure")
            raise
        finally:
            native = A.NSRunningApplication.runningApplicationWithProcessIdentifier_(app.pid)
            if native:
                native.terminate()
            app.wait(timeout=20)
