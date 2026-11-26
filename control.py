import base64
import json
import os
import sys
from io import BytesIO
from pathlib import Path

from jupyter_client.manager import run_kernel
from PIL import Image

root = Path(os.environ["NOTEBOOK_ROOT"])
cells = json.loads((root/"proof/original.ipynb").read_bytes())["cells"]
messages = []
with run_kernel(kernel_name="mac-check") as client:
    for cell, expected in zip(cells[1:3], (1, 2)):
        code = "".join(cell["source"]).replace("gain = 2", "gain = 3")
        reply = client.execute_interactive(code, output_hook=messages.append, timeout=60)
        assert reply["content"]["status"] == "ok", reply["content"]
        assert reply["content"]["execution_count"] == expected
stdout = "".join(m["content"]["text"] for m in messages if m["msg_type"] == "stream" and m["content"]["name"] == "stdout")
assert stdout == "Calibrated readings: [3, 6, 9, 12, 15]\n", stdout
encoded, = [m["content"]["data"]["image/png"] for m in messages if m["msg_type"] == "display_data"]
data = base64.b64decode(encoded)
image = Image.open(BytesIO(data))
image.load()
assert image.format == "PNG" and image.width > 250 and image.height > 100
(root/"proof/kernel-control.png").write_bytes(data)
print(f"Standalone Jupyter client (not Zed): counts 1/2, {stdout.strip()}, PNG {image.size}, {sys.executable}")
