import hashlib
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
    app = subprocess.Popen([os.environ["ZED_EXE"], "--user-data-dir", root/"profile", project, file],
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

def replace_first_line(value):
    key(126, cmd)
    key(124, cmd | shift)
    run("pbcopy", input=value.encode())
    key(9, cmd)

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
    time.sleep(1)

cmd, shift = Q.kCGEventFlagMaskCommand, Q.kCGEventFlagMaskShift
app = launch("first")
time.sleep(6)
shot(app, "01-open")
focus(app)
key(36, delay=3)
shot(app, "02-trust")
key(36)
replace_first_line("# Calibrated signal study")
key(53)
key(125)
key(36)
replace_first_line("gain = 3")
shot(app, "03-edits")
assert not kernels(), "Unexpected pre-existing kernel"
key(36, shift)
for _ in range(90):
    if kernels():
        break
    time.sleep(1)
assert kernels(), "Zed did not start an ipykernel"
assert any(app.pid in [p.pid for p in psutil.Process(pid).parents()] for pid, _ in kernels())
print(f"Zed pid={app.pid}, kernel pid and parent={kernels()}", flush=True)
time.sleep(8)
shot(app, "04-stdout")
key(36, cmd, delay=18)
shot(app, "05-chart")
key(1, cmd, delay=2)
(proof/"saved.ipynb").write_bytes(file.read_bytes())
run(sys.executable, "-I", Path(__file__).with_name("fixture.py"), "check")
key(15, cmd | shift, delay=5)
shot(app, "06-stop")
assert not kernels(), "Kernel survived Stop"
key(12, cmd)
app.wait(timeout=30)
digest = hashlib.sha256(file.read_bytes()).hexdigest()
app = launch("reopen")
time.sleep(4)
focus(app)
shot(app, "07-reopen")
key(125)
key(125)
shot(app, "08-reopened-chart")
assert not kernels(), "Reopening started a kernel"
assert digest == hashlib.sha256(file.read_bytes()).hexdigest(), "Reopening changed the file"
print("Reopened with kernel stopped and file unchanged: " + digest)
key(12, cmd)
app.wait(timeout=30)
