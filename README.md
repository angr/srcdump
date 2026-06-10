# sourcedump

An `objdump`-like tool that prints **decompiled code** instead of disassembly,
using [angr](https://github.com/angr/angr) as the binary-loading and
decompilation backend.

```
$ sourcedump /bin/true            # decompile the whole binary
$ sourcedump -f main /bin/true     # just one function (by name or 0xADDR)
$ sourcedump -l /bin/true          # list functions, don't decompile
$ sourcedump -j16 ./big_binary     # 16 parallel workers
```

## How it works

`sourcedump` is a thin Rust orchestrator. The heavy lifting (loading, CFG
recovery, decompilation) happens in angr, which is Python. To get real
parallelism past the GIL, the Rust binary fans the work out across **one Python
worker process per CPU**:

```
            ┌─────────── sourcedump (Rust) ───────────┐
            │  parse CLI, resolve interpreter, merge   │
            └───────┬──────────┬──────────┬────────────┘
                    │          │          │   spawn N workers
              worker 0     worker 1     worker N-1   (python worker.py)
                    │          │          │
            each: Project → CFGFast → decompile functions[i::N]
                    │          │          │
                    └──── JSON lines ─────┘   (one object per function)
```

Every worker independently loads the binary, recovers the CFG, and builds the
*same* deterministically-sorted candidate function list — then decompiles only
its stripe `functions[i::N]`. The stripes partition the work exactly, with no
shared state or coordination. The orchestrator parses each worker's JSON-line
output, merges it in address order, and prints it.

A function that fails to decompile is reported as a placeholder comment rather
than aborting the whole dump.

## Usage

```
sourcedump [OPTIONS] <FILE>

Options:
  -f, --function <NAME|ADDR>  Only decompile these functions (repeatable)
  -l, --list                  List functions instead of decompiling
  -j, --jobs <N>              Parallel workers (default: CPU count)
      --load-libs             Enable angr's auto_load_libs (off by default)
      --include-plt           Include PLT stubs in the output
      --python <PATH>         Interpreter to run workers with
```

### Choosing the interpreter

Workers need a Python with angr installed. Resolution order:

1. `--python <PATH>`
2. `$SOURCEDUMP_PYTHON`
3. `$VIRTUAL_ENV/bin/python` (i.e. an activated virtualenv)
4. `python3` on `PATH`

## Building

```
cargo build --release
```

The Python worker (`src/worker.py`) is embedded into the binary at compile time,
so the resulting executable is self-contained apart from needing an angr-capable
interpreter at runtime.

## Performance notes & roadmap

Decompilation dominates runtime, so parallel workers are the main speed lever.
Current tradeoffs and planned work:

- **CFG is recomputed per worker.** Cheap relative to decompilation, but it is
  redundant. A shared CFG cache (serialize once, load in each worker) is the
  first optimization target.
- **Per-worker angr import + CFG startup** caps speedup on small binaries; it
  pays off as the number/size of functions grows.
- Planned: result streaming (`--stream`), a `--fast` decompiler-option
  passthrough, and an output-format toggle.
