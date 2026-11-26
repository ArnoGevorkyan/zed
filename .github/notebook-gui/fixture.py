import json
import os
from pathlib import Path
import re
import sys

ROOT = Path(os.environ["NOTEBOOK_ROOT"])
PROOF = ROOT / "proof"
PROJECT = ROOT / "project"
ATTACHMENT = r'{"cell_type":"markdown","id":"attachment","metadata": { "tags":["keep"] }, "source":["Résumé 👋\n","![pixel](attachment:pixel.png)"], "attachments":{"pixel.png":{"image/png":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aJNsAAAAASUVORK5CYII="}}}'
STORED = r'{"cell_type":"code","id":"stored","metadata":{"collapsed": false},"source":"print(17)\r\n","execution_count": 9,"outputs": [ { "output_type":"stream","name":"stdout","text":["stored 17\n"] } ]}'
RAW = r'{ "cell_type" : "raw", "id": "raw","metadata": {"tags":["leave"]},"source":"e\u0301" }'
HEADER = '{\r\n  "nbformat":4, "nbformat_minor":5,\r\n "metadata" : { "language_info": {"name":"python"}, "audit":{"unicode":"λ","value":1.2300} },\r\n "cells" : ['
FOOTER = ']\r\n}\r\n'


def create():
    PROJECT.mkdir(parents=True)
    PROOF.mkdir()
    for name, cells in [("Empty", []), ("Rich", [ATTACHMENT, STORED, RAW])]:
        data = (HEADER + ',\r\n '.join(cells) + FOOTER).encode()
        (PROJECT / f"{name}.ipynb").write_bytes(data)
        (PROOF / f"original-{name}.ipynb").write_bytes(data)


def read(label):
    data = (PROOF / f"{label}.ipynb").read_bytes()
    return data, json.loads(data)


def check():
    for group, labels in [
        ("Empty", [("empty-added", "mc"), ("empty-up", "cm"), ("empty-down", "mc"),
                   ("empty-deleted", "m"), ("empty-undone", "mc"), ("empty-redone", "m"),
                   ("empty-final", "mc"), ("empty-reopened", "mc")]),
        ("Rich", [("rich-moved", "sar"), ("rich-deleted", "sr"),
                  ("rich-undone", "sar"), ("rich-reopened", "sar")]),
    ]:
        _, original = read("original-" + group)
        by_kind = {}
        for label, order in labels:
            data, saved = read(label)
            assert {**saved, "cells": []} == {**original, "cells": []}, label
            assert data.startswith(HEADER.encode()) and data.endswith(FOOTER.encode()), label
            assert len(saved["cells"]) == len(order), (label, len(saved["cells"]), order)
            ids = [c["id"] for c in saved["cells"]]
            assert len(ids) == len(set(ids)), (label, ids)
            for mark, cell in zip(order, saved["cells"], strict=True):
                if group == "Rich":
                    raw = {"a": ATTACHMENT, "s": STORED, "r": RAW}[mark]
                    assert cell == json.loads(raw) and raw.encode() in data, (label, mark)
                else:
                    text = "".join(cell["source"])
                    later = label in {"empty-undone", "empty-redone", "empty-final", "empty-reopened"}
                    assert (cell["cell_type"], text) == {
                        "m": ("markdown", "# Windows structure" + (" edited" if later else "")),
                        "c": ("code", "value = 17")
                    }[mark], (label, cell)
                    assert re.fullmatch("[A-Za-z0-9_-]{1,64}", cell["id"]), (label, cell["id"])
                    if mark == "c":
                        assert cell["execution_count"] is None and cell["outputs"] == [], label
                    assert by_kind.setdefault(mark, {**cell, "source": None}) == {**cell, "source": None}, (label, mark)
            print(label, ids, "OK")
    for before, after in [("empty-undone", "empty-final"),
                          ("empty-final", "empty-reopened"), ("rich-moved", "rich-undone"),
                          ("rich-undone", "rich-reopened")]:
        assert read(before)[0] == read(after)[0], (before, after)


if __name__ == "__main__":
    {"create": create, "check": check}[sys.argv[1]]()
