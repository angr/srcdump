# Development and building notes

## Creating `uv_angr.zip`

```sh
uv venv --relocatable --python 3.14 env
uv pip install --python env/bin/python angr sqlalchemy
# zip the venv so its contents are at the archive root (bin/, lib/, …)
( cd env && zip -y -r ../uv_angr.zip . )   # -y preserves symlinks
```

srcdump auto-detects `uv_angr.zip` next to itself and extracts it on first run.
The archive must be self-contained for use on another machine:
Bundle the base interpreter (e.g. a `python-build-standalone` build) rather than relying on symlinks into a uv-managed Python that won't exist on the target.

> Why not a single frozen executable? angr's native extensions (`pyvex`,
> `unicorn`, `z3`, etc.) can't be imported from inside a zip, and PyInstaller-style
> onefile binaries re-extract on every process start — bad for srcdump's
> one-process-per-CPU model. A cached venv (uv or archive) is faster and more
> robust.

