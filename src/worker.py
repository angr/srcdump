"""srcdump worker.

Run by the Rust orchestrator, one process per CPU. Each worker independently
loads the target binary with angr, recovers the CFG, deterministically sorts
the candidate functions, and then handles only its own stripe of that list
(functions[index::count]). Because every worker derives the identical sorted
list, the stripes partition the work with no coordination.

Output protocol: one JSON object per line on stdout. angr's own logging goes
to stderr, so stdout stays clean for the orchestrator to parse.

    {"kind": "meta", "file_format": "elf64-x86-64"}
    {"kind": "function", "addr": .., "name": .., "size": .., "is_plt": ..,
     "section": ".text", "text": "<decompiled source>" | null}
    {"kind": "error", "addr": .., "name": .., "message": ..}
    {"kind": "fatal", "message": ..}
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import sys


def emit(obj: dict) -> None:
    """Write one JSON message line to stdout and flush immediately."""
    sys.stdout.write(json.dumps(obj))
    sys.stdout.write("\n")
    sys.stdout.flush()


def parse_args(argv: list[str]) -> argparse.Namespace:
    ap = argparse.ArgumentParser(prog="srcdump-worker", add_help=False)
    # Required only for `build`; decompile/list workers load from --db.
    ap.add_argument("binary", nargs="?")
    ap.add_argument("--db", required=True)
    ap.add_argument("--index", type=int, default=0)
    ap.add_argument("--count", type=int, default=1)
    ap.add_argument("--mode", choices=("build", "decompile", "list"), default="decompile")
    ap.add_argument("--function", action="append", default=[])
    ap.add_argument("--load-libs", action="store_true")
    ap.add_argument("--include-plt", action="store_true")
    return ap.parse_args(argv)


def matches_filters(func, filters: list[str]) -> bool:
    """True if `func` should be kept given the user's -f filters.

    A filter matches on exact function name or on entry address (decimal or
    0x-prefixed hex). With no filters, everything passes.
    """
    if not filters:
        return True
    for raw in filters:
        if raw == func.name:
            return True
        try:
            if int(raw, 0) == func.addr:
                return True
        except ValueError:
            pass
    return False


# archinfo arch name -> objdump/BFD file-format name. Endianness-sensitive
# arches resolve through the `little` branch at lookup time.
def file_format(proj) -> str:
    mo = proj.loader.main_object
    backend = type(mo).__name__
    arch = proj.arch
    bits = arch.bits
    little = arch.memory_endness == "Iend_LE"

    if backend == "ELF":
        name = arch.name
        table = {
            "AMD64": "elf64-x86-64",
            "X86": "elf32-i386",
            "AARCH64": "elf64-littleaarch64" if little else "elf64-bigaarch64",
            "ARMEL": "elf32-littlearm" if little else "elf32-bigarm",
            "ARMHF": "elf32-littlearm",
            "ARMCortexM": "elf32-littlearm",
            "MIPS32": "elf32-littlemips" if little else "elf32-bigmips",
            "MIPS64": "elf64-littlemips" if little else "elf64-bigmips",
            "PPC32": "elf32-powerpcle" if little else "elf32-powerpc",
            "PPC64": "elf64-powerpcle" if little else "elf64-powerpc",
            "S390X": "elf64-s390",
            "RISCV64": "elf64-littleriscv",
            "RISCV32": "elf32-littleriscv",
        }
        fmt = table.get(name)
        if fmt is not None:
            return fmt
        endian = "little" if little else "big"
        return f"elf{bits}-{endian}{getattr(arch, 'linux_name', None) or name.lower()}"

    if backend == "PE":
        return "pei-x86-64" if bits == 64 else "pei-i386"
    if backend.startswith("MachO"):
        return "mach-o-x86-64" if bits == 64 else "mach-o-i386"
    return backend.lower()


def build_section_index(proj):
    """Sorted [(start, end, name)] of named sections, for address lookup."""
    mo = proj.loader.main_object
    secs = [
        (s.vaddr, s.vaddr + max(s.memsize, 1), s.name)
        for s in mo.sections
        if s.name
    ]
    secs.sort()
    return secs


def section_of(secs, addr):
    """Name of the section containing `addr`, or None."""
    for start, end, name in secs:
        if start <= addr < end:
            return name
    return None


def candidate_functions(proj, args) -> list:
    """Deterministically-ordered list of functions this run should consider."""
    main_obj = proj.loader.main_object
    out = []
    for func in proj.kb.functions.values():
        if func.is_simprocedure or func.is_syscall:
            continue
        if func.is_alignment:
            continue
        if func.is_plt and not args.include_plt:
            continue
        # Stay within the primary object so library code doesn't leak in when
        # --load-libs is set.
        if main_obj is not None and not main_obj.contains_addr(func.addr):
            continue
        if not matches_filters(func, args.function):
            continue
        out.append(func)
    out.sort(key=lambda f: f.addr)
    return out


def main(argv: list[str]) -> int:
    args = parse_args(argv)

    # Keep stdout clean: route all logging noise to stderr and quiet it down.
    logging.basicConfig(stream=sys.stderr, level=logging.ERROR)
    logging.getLogger("angr").setLevel(logging.ERROR)
    logging.getLogger("cle").setLevel(logging.ERROR)
    logging.getLogger("pyvex").setLevel(logging.ERROR)
    logging.getLogger("claripy").setLevel(logging.ERROR)

    try:
        import angr
        from angr.angrdb import AngrDB
    except Exception as exc:  # pragma: no cover - environment issue
        emit({"kind": "fatal", "message": f"failed to import angr: {exc!r}"})
        return 1

    if args.mode == "build":
        return build_db(angr, AngrDB, args)
    return decompile_db(AngrDB, args)


def build_db(angr, AngrDB, args) -> int:
    """Recover the CFG once and persist it to an AngrDB, atomically.

    Writes to a per-process temp file then renames into place, so a concurrent
    or interrupted build never leaves a half-written db behind.
    """
    if not args.binary:
        emit({"kind": "fatal", "message": "build mode requires a binary path"})
        return 1

    # Throttle CFG progress to whole-percent changes (angr fires it thousands
    # of times) so we don't flood the orchestrator with JSON lines.
    last_pct = [-1]

    def cfg_progress(percentage, text=None, **kwargs):
        pct = int(percentage)
        if pct != last_pct[0]:
            last_pct[0] = pct
            emit({"kind": "progress", "stage": "cfg", "percent": pct})

    try:
        emit({"kind": "progress", "stage": "load"})
        proj = angr.Project(args.binary, auto_load_libs=args.load_libs)
        emit({"kind": "progress", "stage": "cfg", "percent": 0})
        proj.analyses.CFGFast(
            force_smart_scan=False, normalize=True, progress_callback=cfg_progress
        )
    except Exception as exc:
        emit({"kind": "fatal", "message": f"loading/CFG failed: {exc!r}"})
        return 1

    emit({"kind": "progress", "stage": "save"})
    tmp = f"{args.db}.tmp.{os.getpid()}"
    try:
        AngrDB(project=proj).dump(tmp)
        os.replace(tmp, args.db)
    except Exception as exc:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        emit({"kind": "fatal", "message": f"saving angrdb failed: {exc!r}"})
        return 1
    return 0


def decompile_db(AngrDB, args) -> int:
    """Load the cached CFG and decompile (or list) this worker's stripe."""
    try:
        proj = AngrDB().load(args.db)
    except Exception as exc:
        emit({"kind": "fatal", "message": f"loading angrdb {args.db!r} failed: {exc!r}"})
        return 1

    cfg_model = proj.kb.cfgs.get_most_accurate()

    # Every worker reports the file format; the orchestrator keeps the first.
    emit({"kind": "meta", "file_format": file_format(proj)})

    sections = build_section_index(proj)
    functions = candidate_functions(proj, args)
    stripe = functions[args.index :: args.count]

    # Tell the orchestrator how many functions this worker will handle so it
    # can sum them into a grand total for the progress display.
    emit({"kind": "plan", "count": len(stripe)})

    for func in stripe:
        base = {
            "addr": int(func.addr),
            "name": func.name,
            "size": int(func.size or 0),
            "is_plt": bool(func.is_plt),
            "section": section_of(sections, func.addr),
        }
        if args.mode == "list":
            emit({"kind": "function", "text": None, **base})
            continue
        try:
            dec = proj.analyses.Decompiler(func, cfg=cfg_model)
            if dec.codegen is None or dec.codegen.text is None:
                emit({
                    "kind": "error",
                    "addr": base["addr"],
                    "name": base["name"],
                    "message": "decompiler produced no output",
                })
            else:
                emit({"kind": "function", "text": dec.codegen.text, **base})
        except Exception as exc:
            emit({
                "kind": "error",
                "addr": base["addr"],
                "name": base["name"],
                "message": repr(exc),
            })

    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
