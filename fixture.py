import base64
import os
import io
import json
import sys
from pathlib import Path

import nbformat
from PIL import Image, ImageDraw

root = Path(os.environ["NOTEBOOK_ROOT"])
target = root / "project" / "Signal study.ipynb"
archive = root / "proof" / "original.ipynb"
point = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAAAAAA6fptVAAAACklEQVR4nGP4DwABAQEAsTj2FAAAAABJRU5ErkJggg=="

def initial():
    thumbnail = Image.new("RGB", (220, 72), "#ecf5f4")
    draw = ImageDraw.Draw(thumbnail)
    for i, height in enumerate([15, 29, 23, 36, 42]):
        draw.rectangle((16+i*40, 58-height, 39+i*40, 58), fill="#18798a")
    preview = io.BytesIO()
    thumbnail.save(preview, "PNG")
    return {"nbformat": 4, "nbformat_minor": 5, "metadata": {
        "kernelspec": {"display_name": "Python (Notebook proof)", "name": "notebook-proof", "language": "python"},
        "language_info": {"name": "python", "version": "3.12.3"},
        "observatory": {"instrument": "bench", "calibrations": [19, 23], "site": None},
    }, "cells": [
        {"id": "title", "cell_type": "markdown", "metadata": {"tags": ["field-notes"]},
         "attachments": {"reference.png": {"image/png": point}},
         "source": ["# Signal study\n", "Compare five readings after calibration."]},
        {"id": "calibrate", "cell_type": "code", "metadata": {"tags": ["calibration"]},
         "source": ["gain = 2\n", "readings = [gain * n for n in range(1, 6)]\n", "print('Calibrated readings:', readings)"],
         "outputs": [], "execution_count": None},
        {"id": "plot", "cell_type": "code", "metadata": {"tags": ["plot"]},
         "source": ["%matplotlib inline\n", "import matplotlib.pyplot as plt\n", "plt.figure(figsize=(4, 2))\n",
                    "plt.plot(range(1, 6), readings, 'o-', color='#12738a')\n", "plt.title('Calibrated signal')\n", "plt.show()"],
         "outputs": [], "execution_count": None},
        {"id": "archive", "cell_type": "code", "metadata": {"audit": {"retain": True}},
         "source": "print('Archived reading')", "outputs": [
             {"output_type": "display_data", "metadata": {"archive": [1, 2]},
              "data": {"text/plain": "Archived reference", "image/png": base64.b64encode(preview.getvalue()).decode(),
                       "application/vnd.lab+json": {"ratio": 0.7}}}
         ], "execution_count": 17},
        {"id": "protocol", "cell_type": "raw", "metadata": {"format": "text/plain"}, "source": ["Protocol v2\n", "Retain exactly."]},
    ]}

def text(value):
    return "".join(value)

def excluding_edited_values(path, executed):
    data = path.read_bytes().decode("utf-8")
    fields = {"title": ["source"], "calibrate": ["source"], "plot": []}
    if executed:
        for cell in ("calibrate", "plot"):
            fields[cell] += ["outputs", "execution_count"]
    spans = []
    for cell, keys in fields.items():
        start = data.index(f'   "id": "{cell}",')
        for key in keys:
            prefix = f'\n   "{key}": '
            offset = data.index(prefix, start) + len(prefix)
            _, end = json.JSONDecoder().raw_decode(data, idx=offset)
            spans.append((offset, end))
    for start, end in sorted(spans, reverse=True):
        data = data[:start] + "<changed value>" + data[end:]
    return data

if sys.argv[1] == "create":
    assert not target.exists()
    original = initial()
    nbformat.validate(original)
    target.write_bytes((json.dumps(original, indent=1, ensure_ascii=False) + "\n").encode("utf-8"))
    archive.write_bytes(target.read_bytes())
elif sys.argv[1] in {"check", "managed", "passive"}:
    before = json.loads(archive.read_text(encoding="utf-8"))
    observed = Path(sys.argv[2]) if len(sys.argv) > 2 else (target if sys.argv[1] == "check" else root / "offline/project/Signal study.ipynb")
    after = json.loads(observed.read_text(encoding="utf-8"))
    assert excluding_edited_values(archive, sys.argv[1] != "passive") == excluding_edited_values(observed, sys.argv[1] != "passive")
    print("PASS: bytes outside explicitly edited JSON values are identical")
    nbformat.validate(after)
    for key in before.keys() - {"cells"}:
        assert before[key] == after[key], key
    assert before.keys() == after.keys()
    assert [c["id"] for c in before["cells"]] == [c["id"] for c in after["cells"]]
    cells = after["cells"]
    assert text(cells[0]["source"]) == text(before["cells"][0]["source"]).replace("Signal study", "Calibrated signal study")
    assert text(cells[1]["source"]) == text(before["cells"][1]["source"]).replace("gain = 2", "gain = 3")
    if sys.argv[1] == "passive":
        for i in [0, 1]:
            before["cells"][i]["source"] = cells[i]["source"]
        assert after == before
        print("PASS: Markdown and Python edits saved offline; archived PNG, existing count, custom MIME bundle, metadata and untouched cells all match; no new results or execution counts")
        sys.exit(0)
    for i, skip in enumerate([{"source"}, {"source", "execution_count", "outputs"}, {"execution_count", "outputs"}, set(), set()]):
        expected = before["cells"][i]
        actual = cells[i]
        assert actual.keys() == expected.keys(), i
        for key in expected.keys() - skip:
            assert actual[key] == expected[key], (i, key)
    assert [c["execution_count"] for c in cells[1:3]] == [1, 2]
    stdout = "".join(text(out["text"]) for out in cells[1]["outputs"] if out["output_type"] == "stream" and out["name"] == "stdout")
    assert stdout == "Calibrated readings: [3, 6, 9, 12, 15]\n", stdout
    assert not any(out["output_type"] == "error" for c in cells[1:3] for out in c["outputs"])
    image, = [out["data"]["image/png"] for out in cells[2]["outputs"] if out["output_type"] == "display_data" and "image/png" in out["data"]]
    data = base64.b64decode(text(image), validate=True)
    im = Image.open(io.BytesIO(data))
    im.load()
    assert im.format == "PNG" and im.width > 250 and im.height > 100
    print(f"PASS: edited Markdown and Python, counts 1 and 2, stdout {stdout.strip()!r}, decoded {im.width}x{im.height} PNG, untouched cells/attachments/metadata/fields match")
