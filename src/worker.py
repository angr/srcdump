"""sourcedump worker.

Run by the Rust orchestrator, one process per CPU. Each worker independently
loads the target binary with angr, recovers the CFG, deterministically sorts
the candidate functions, and then handles only its own stripe of that list
(functions[index::count]). Because every worker derives the identical sorted
list, the stripes partition the work with no coordination.

Output protocol: one JSON object per line on stdout. angr's own logging goes
to stderr, so stdout stays clean for the orchestrator to parse.

    {"kind": "function", "addr": .., "name": .., "size": .., "is_plt": ..,
     "text": "<decompiled source>" | null}
    {"kind": "error", "addr": .., "name": .., "message": ..}
    {"kind": "fatal", "message": ..}
"""

from __future__ import annotations

import argparse
import json
import logging
import sys


def emit(obj: dict) -> None:
    """Write one JSON message line to stdout and flush immediately."""
    sys.stdout.write(json.dumps(obj))
    sys.stdout.write("\n")
    sys.stdout.flush()


def parse_args(argv: list[str]) -> argparse.Namespace:
    ap = argparse.ArgumentParser(prog="sourcedump-worker", add_help=False)
    ap.add_argument("binary")
    ap.add_argument("--index", type=int, required=True)
    ap.add_argument("--count", type=int, required=True)
    ap.add_argument("--mode", choices=("decompile", "list"), default="decompile")
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


def candidate_functions(proj, cfg, args) -> list:
    """Deterministically-ordered list of functions this run should consider."""
    main_obj = proj.loader.main_object
    out = []
    for func in cfg.kb.functions.values():
        if func.is_simprocedure or func.is_syscall:
            continue
        if getattr(func, "is_alignment", False):
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
    except Exception as exc:  # pragma: no cover - environment issue
        emit({"kind": "fatal", "message": f"failed to import angr: {exc!r}"})
        return 1

    try:
        proj = angr.Project(args.binary, auto_load_libs=args.load_libs)
        cfg = proj.analyses.CFGFast(normalize=True)
    except Exception as exc:
        emit({"kind": "fatal", "message": f"loading/CFG failed: {exc!r}"})
        return 1

    functions = candidate_functions(proj, cfg, args)
    stripe = functions[args.index :: args.count]

    for func in stripe:
        base = {
            "addr": int(func.addr),
            "name": func.name,
            "size": int(func.size or 0),
            "is_plt": bool(func.is_plt),
        }
        if args.mode == "list":
            emit({"kind": "function", "text": None, **base})
            continue
        try:
            dec = proj.analyses.Decompiler(func, cfg=cfg.model)
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
