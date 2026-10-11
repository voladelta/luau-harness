"""The manifest lists the live segments, oldest first. It is replaced atomically on every change."""
import json
import os

NAME = "MANIFEST.json"


def segment_name(sid):
    return f"seg-{sid:06d}.dat"


def load(directory):
    path = os.path.join(directory, NAME)
    if not os.path.exists(path):
        return {"segments": [], "next_id": 1}
    with open(path) as f:
        return json.load(f)


def save(directory, manifest):
    path = os.path.join(directory, NAME)
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(manifest, f)
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, path)
