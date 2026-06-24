#!/usr/bin/env python3
"""Write a `<file>.sha256` next to each given file (sha256sum-compatible format).

A tiny, cross-platform stand-in for `sha256sum`/`shasum` so release jobs don't
depend on which is installed on the runner. Files already ending in `.sha256`
are skipped, so `sha256sum.py dist/*` is idempotent.
"""

import hashlib
import os
import sys


def digest(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def main(argv: list[str]) -> int:
    if not argv:
        sys.exit("usage: sha256sum.py FILE...")
    for path in argv:
        if path.endswith(".sha256"):
            continue
        out = f"{path}.sha256"
        with open(out, "w") as fh:
            fh.write(f"{digest(path)}  {os.path.basename(path)}\n")
        print(f"{out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
