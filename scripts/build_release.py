#!/usr/bin/env python3
"""Build srcdump release artifacts for the current platform.

Produces, in ``dist/``:

* **slim** -- ``srcdump-<ver>-<triple>.<tar.gz|zip>`` containing just the
  ``srcdump`` executable.
* **full** -- ``srcdump-<ver>-<triple>-full.<tar.gz|zip>`` containing
  ``srcdump``, ``uv``, and ``uv_angr.zip`` (an offline-installable, fully
  self-contained angr environment that srcdump auto-detects when it sits next
  to the binary).

The interesting part is ``uv_angr.zip``: a plain ``uv`` venv is *not*
relocatable (its ``bin/python`` is an absolute symlink into uv's managed Python
store), so this script bundles a standalone interpreter *inside* the archive and
rewrites the venv to reference it with relative paths. The result survives being
unzipped on a machine that has never seen uv or Python.

This is deliberately a standalone, stdlib-only Python script (it shells out to
``cargo`` and ``uv``) so it can be run and eyeballed by hand on each platform.

Usage:
    python3 scripts/build_release.py [options]

    --python-version X.Y   Python for the bundled env (default: 3.14)
    --angr-version SPEC    angr version to bundle, e.g. "==9.2.182"
                           (default: latest from PyPI)
    --srcdump PATH         Use this prebuilt srcdump instead of `cargo build`
    --uv PATH              Use this uv binary instead of PATH/download
    --slim-only            Build only the slim archive
    --full-only            Build only the full archive
    --keep-stage           Keep the unzipped bundle for inspection
    --out DIR              Output directory (default: <repo>/dist)
"""

from __future__ import annotations

import argparse
import os
import platform
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
import zipfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
IS_WINDOWS = os.name == "nt"
EXE = ".exe" if IS_WINDOWS else ""


# ---------------------------------------------------------------------------
# Small helpers
# ---------------------------------------------------------------------------


def log(msg: str) -> None:
    print(f"\033[1m==>\033[0m {msg}" if sys.stdout.isatty() else f"==> {msg}", flush=True)


def run(cmd, **kw) -> subprocess.CompletedProcess:
    printable = " ".join(str(c) for c in cmd)
    print(f"    $ {printable}", flush=True)
    return subprocess.run([str(c) for c in cmd], check=True, **kw)


def die(msg: str) -> "None":
    sys.exit(f"build_release: {msg}")


def read_version() -> str:
    """The [package] version from Cargo.toml."""
    in_pkg = False
    for line in (REPO / "Cargo.toml").read_text().splitlines():
        s = line.strip()
        if s.startswith("["):
            in_pkg = s == "[package]"
        elif in_pkg and s.startswith("version"):
            return s.split("=", 1)[1].strip().strip('"')
    die("could not find [package] version in Cargo.toml")


def target_triple() -> str:
    """Rust/uv target triple for the host, matching env.rs::uv_target_triple."""
    machine = platform.machine().lower()
    arch = {"amd64": "x86_64", "x86_64": "x86_64", "arm64": "aarch64", "aarch64": "aarch64"}.get(
        machine
    )
    if arch is None:
        die(f"unsupported CPU architecture: {platform.machine()}")
    system = platform.system()
    if system == "Linux":
        return f"{arch}-unknown-linux-gnu"
    if system == "Darwin":
        return f"{arch}-apple-darwin"
    if system == "Windows":
        return f"{arch}-pc-windows-msvc"
    die(f"unsupported OS: {system}")


# ---------------------------------------------------------------------------
# srcdump + uv binaries
# ---------------------------------------------------------------------------


def build_srcdump(override: Path | None) -> Path:
    if override:
        if not override.is_file():
            die(f"--srcdump {override}: not a file")
        return override
    log("building srcdump (cargo build --release)")
    run(["cargo", "build", "--release"], cwd=REPO)
    binary = REPO / "target" / "release" / f"srcdump{EXE}"
    if not binary.is_file():
        die(f"cargo did not produce {binary}")
    return binary


def resolve_uv(override: Path | None, triple: str, workdir: Path) -> Path:
    if override:
        if not override.is_file():
            die(f"--uv {override}: not a file")
        return override
    on_path = shutil.which("uv")
    if on_path:
        return Path(on_path)
    log(f"downloading uv for {triple}")
    ext = "zip" if IS_WINDOWS else "tar.gz"
    url = f"https://github.com/astral-sh/uv/releases/latest/download/uv-{triple}.{ext}"
    archive = workdir / f"uv.{ext}"
    with urllib.request.urlopen(url, timeout=120) as resp, open(archive, "wb") as out:
        shutil.copyfileobj(resp, out)
    dest = workdir / f"uv{EXE}"
    _extract_uv(archive, dest)
    if not IS_WINDOWS:
        dest.chmod(0o755)
    return dest


def _extract_uv(archive: Path, dest: Path) -> None:
    want = f"uv{EXE}"
    if archive.name.endswith(".zip"):
        with zipfile.ZipFile(archive) as zf:
            member = next((n for n in zf.namelist() if Path(n).name == want), None)
            if member is None:
                die(f"{want} not found inside {archive.name}")
            with zf.open(member) as src, open(dest, "wb") as out:
                shutil.copyfileobj(src, out)
    else:
        with tarfile.open(archive) as tf:
            member = next((m for m in tf.getmembers() if Path(m.name).name == want), None)
            if member is None:
                die(f"{want} not found inside {archive.name}")
            with tf.extractfile(member) as src, open(dest, "wb") as out:
                shutil.copyfileobj(src, out)


# ---------------------------------------------------------------------------
# uv_angr.zip: a relocatable, self-contained angr environment
# ---------------------------------------------------------------------------


def build_uv_angr(uv: Path, workdir: Path, py_version: str, angr_spec: str) -> Path:
    """Stage a self-contained angr venv and zip it into uv_angr.zip.

    Layout of the produced archive (= the venv root srcdump renames into place):

        bin/python            -> ../.python/<cpython>/bin/pythonX.Y   (relative)
        lib/.../site-packages/...   (angr + deps)
        pyvenv.cfg            home = .python/<cpython>/bin            (relative)
        .python/<cpython>/    bundled standalone interpreter
    """
    stage = workdir / "stage"
    if stage.exists():
        shutil.rmtree(stage)

    # 1. A standalone (relocatable) interpreter, installed off to the side.
    py_store = workdir / "py-install"
    log(f"installing standalone CPython {py_version}")
    run([uv, "python", "install", "--install-dir", py_store, py_version])
    cpython_dir, interpreter = _find_standalone(py_store, py_version)

    # 2. A relocatable venv at the archive root, from that interpreter.
    log("creating relocatable venv")
    run([uv, "venv", "--relocatable", "--python", interpreter, stage])

    # 3. Bundle a copy of the interpreter *inside* the venv root.
    log("bundling the interpreter into the venv")
    bundled_root = stage / ".python" / cpython_dir.name
    bundled_root.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(cpython_dir, bundled_root, symlinks=True)

    # 4. Repoint the venv at the bundled interpreter with relative paths.
    if IS_WINDOWS:
        bundled_bin = bundled_root  # python.exe lives at the cpython root
    else:
        bundled_bin = bundled_root / "bin"
        bundled_exe = bundled_bin / interpreter.name
        venv_py = stage / "bin" / "python"
        venv_py.unlink()
        venv_py.symlink_to(os.path.relpath(bundled_exe, venv_py.parent))
    _rewrite_home(stage / "pyvenv.cfg", os.path.relpath(bundled_bin, stage))

    # 5. Install angr (+ sqlalchemy, which srcdump's AngrDB cache needs) into it.
    venv_python = stage / ("Scripts" if IS_WINDOWS else "bin") / f"python{EXE}"
    log(f"installing angr{angr_spec or ' (latest)'} into the bundled env")
    run([uv, "pip", "install", "--python", venv_python, f"angr{angr_spec}", "sqlalchemy"])

    # 6. Zip it up, preserving symlinks and executable bits.
    out_zip = workdir / "uv_angr.zip"
    log("zipping uv_angr.zip (preserving symlinks + modes)")
    _zip_tree(stage, out_zip)
    return out_zip


def _find_standalone(py_store: Path, py_version: str) -> tuple[Path, Path]:
    """Return (cpython_dir, interpreter_path) for the freshly installed Python.

    uv lays down a real ``cpython-X.Y.Z-...`` directory plus a ``cpython-X.Y-...``
    symlink alias; we want the real directory.
    """
    candidates = [
        d for d in py_store.glob("cpython-*") if d.is_dir() and not d.is_symlink()
    ]
    if not candidates:
        die(f"no standalone CPython found under {py_store}")
    cpython_dir = sorted(candidates)[-1]
    if IS_WINDOWS:
        interpreter = cpython_dir / "python.exe"
    else:
        # Prefer the fully-qualified name (python3.14), fall back to python3.
        bindir = cpython_dir / "bin"
        interpreter = bindir / f"python{py_version}"
        if not interpreter.exists():
            interpreter = next(
                (p for p in sorted(bindir.glob("python3.*")) if not p.is_symlink()),
                bindir / "python3",
            )
    if not interpreter.exists():
        die(f"interpreter not found at {interpreter}")
    return cpython_dir, interpreter


def _rewrite_home(pyvenv_cfg: Path, rel_home: str) -> None:
    """Point pyvenv.cfg's ``home`` at the bundled interpreter (relative path)."""
    lines = pyvenv_cfg.read_text().splitlines()
    out = []
    for line in lines:
        if line.split("=", 1)[0].strip() == "home":
            out.append(f"home = {rel_home}")
        else:
            out.append(line)
    pyvenv_cfg.write_text("\n".join(out) + "\n")


def _iter_tree(root: Path):
    """Yield (path, kind) for every entry under root, never following symlinks."""
    for entry in sorted(os.scandir(root), key=lambda e: e.name):
        p = Path(entry.path)
        if entry.is_symlink():
            yield p, "link"
        elif entry.is_dir(follow_symlinks=False):
            yield p, "dir"
            yield from _iter_tree(p)
        else:
            yield p, "file"


def _zip_tree(root: Path, out_zip: Path) -> None:
    with zipfile.ZipFile(out_zip, "w", compression=zipfile.ZIP_DEFLATED) as zf:
        for path, kind in _iter_tree(root):
            rel = path.relative_to(root).as_posix()
            st = path.lstat()
            mode = st.st_mode & 0xFFFF
            if kind == "link":
                zi = zipfile.ZipInfo(rel)
                zi.create_system = 3  # unix, so unix_mode() survives the round-trip
                zi.external_attr = mode << 16
                zf.writestr(zi, os.readlink(path))
            elif kind == "dir":
                zi = zipfile.ZipInfo(rel + "/")
                zi.create_system = 3
                zi.external_attr = (mode << 16) | 0x10  # MS-DOS directory bit
                zf.writestr(zi, b"")
            else:
                # zf.write streams the file and preserves its mode via from_file.
                zf.write(path, rel)


# ---------------------------------------------------------------------------
# Packaging
# ---------------------------------------------------------------------------


def package(name: str, members: list[Path], out_dir: Path) -> Path:
    """Bundle members at the archive root: zip on Windows, tar.gz elsewhere."""
    out_dir.mkdir(parents=True, exist_ok=True)
    if IS_WINDOWS:
        archive = out_dir / f"{name}.zip"
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as zf:
            for m in members:
                zf.write(m, m.name)
    else:
        archive = out_dir / f"{name}.tar.gz"
        with tarfile.open(archive, "w:gz") as tf:
            for m in members:
                ti = tf.gettarinfo(str(m), arcname=m.name)
                if m.suffix == "" or os.access(m, os.X_OK):
                    ti.mode |= 0o111  # keep executables executable
                with open(m, "rb") as f:
                    tf.addfile(ti, f)
    return archive


# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------


def main() -> None:
    ap = argparse.ArgumentParser(description="Build srcdump release artifacts.")
    ap.add_argument("--python-version", default="3.14")
    ap.add_argument("--angr-version", default="", help='e.g. "==9.2.182" (default: latest)')
    ap.add_argument("--srcdump", type=Path)
    ap.add_argument("--uv", type=Path)
    ap.add_argument("--slim-only", action="store_true")
    ap.add_argument("--full-only", action="store_true")
    ap.add_argument("--keep-stage", action="store_true")
    ap.add_argument("--out", type=Path, default=REPO / "dist")
    ap.add_argument(
        "--archive-version",
        default=None,
        help="version label used in archive filenames (default: Cargo.toml version). "
        "CI passes the release tag so slim and full names stay consistent.",
    )
    args = ap.parse_args()

    if args.slim_only and args.full_only:
        die("--slim-only and --full-only are mutually exclusive")
    if args.angr_version and not args.angr_version[0] in "=<>~!":
        args.angr_version = "==" + args.angr_version

    version = args.archive_version or read_version()
    triple = target_triple()
    base = f"srcdump-{version}-{triple}"
    log(f"srcdump {version} for {triple}")

    workdir = Path(args.keep_stage and (args.out / "_build") or tempfile.mkdtemp(prefix="srcdump-rel-"))
    workdir.mkdir(parents=True, exist_ok=True)
    produced: list[Path] = []
    try:
        srcdump = build_srcdump(args.srcdump)

        if not args.full_only:
            produced.append(package(base, [srcdump], args.out))

        if not args.slim_only:
            uv = resolve_uv(args.uv, triple, workdir)
            uv_angr = build_uv_angr(uv, workdir, args.python_version, args.angr_version)
            produced.append(package(f"{base}-full", [srcdump, uv, uv_angr], args.out))
    finally:
        if not args.keep_stage:
            shutil.rmtree(workdir, ignore_errors=True)

    log("done; produced:")
    for p in produced:
        print(f"    {p}  ({p.stat().st_size / 1e6:.1f} MB)")
    if args.keep_stage:
        print(f"    (build tree kept at {workdir})")


if __name__ == "__main__":
    main()
