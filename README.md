# srcdump

An `objdump`-like tool that prints **decompiled code** instead of disassembly,
using [angr](https://github.com/angr/angr) as the binary-loading and
decompilation backend.

```
$ srcdump ./program            # interactive TUI browser (when run in a terminal)
$ srcdump ./program | less      # plain dump (when piped/redirected)
$ srcdump -f main ./program     # just one function (by name or 0xADDR)
$ srcdump -l ./program          # list functions, don't decompile
$ srcdump -j16 ./big_binary     # 16 parallel workers
```

Run in a terminal, srcdump opens an interactive browser; piped or redirected to a
file, it dumps every function as text. See [Interactive TUI](#interactive-tui).

## How it works

`srcdump` is a thin Rust orchestrator. The heavy lifting (loading, CFG
recovery, decompilation) happens in angr, which is Python. To get real
parallelism past the GIL, the Rust binary fans the work out across **one Python
worker process per CPU**:

```
            ┌─────────── srcdump (Rust) ───────────┐
            │  parse CLI, resolve interpreter, merge   │
            └───────┬──────────┬──────────┬────────────┘
                    │          │          │   spawn N workers
              worker 0     worker 1     worker N-1   (python worker.py)
                    │          │          │
            each: Project → CFGFast → decompile functions[i::N]
                    │          │          │
                    └──── JSON lines ─────┘   (one object per function)
```

The CFG is recovered **once** and cached as an angrdb. A single build step runs
`Project → CFGFast → AngrDB.dump`; every decompile/list worker then *loads* that
db (≈0.06s) instead of rebuilding the CFG (≈0.5s+, and growing with binary
size). Each worker builds the *same* deterministically-sorted candidate function
list from the loaded db and decompiles only its stripe `functions[i::N]`. The
stripes partition the work exactly, with no shared state or coordination. The
orchestrator parses each worker's JSON-line output, merges it in address order,
and prints it.

### CFG cache

The angrdb is keyed by the SHA-256 of the binary's contents:

```
${XDG_CACHE_HOME:-~/.cache}/srcdump/<sha256>/db.adb
```

`--load-libs` recovers a different CFG, so it is cached separately as
`db.load_libs.adb`. The build is atomic (written to a temp file then renamed),
so concurrent or interrupted runs never leave a half-written db. Use `--rebuild`
to force regeneration and `--cache-dir <DIR>` to relocate the cache.

A function that fails to decompile is reported as a placeholder comment rather
than aborting the whole dump.

## Usage

```
srcdump [OPTIONS] <FILE>

Options:
  -f, --function <NAME|ADDR>  Only decompile these functions (repeatable)
  -l, --list                  List functions instead of decompiling
      --slowest <N>           Report the N slowest functions by decompile time
  -j, --jobs <N>              Parallel workers (default: CPU count)
      --load-libs             Enable angr's auto_load_libs (off by default)
      --include-plt           Include PLT stubs in the output
      --rebuild               Rebuild the cached CFG even if one exists
      --cache-dir <DIR>       Override the CFG cache location
      --python <PATH>         Interpreter to run workers with
      --progress <WHEN>       Progress display: auto (default), always, never
      --tui <WHEN>            Interactive browser: auto (default), always, never
```

### Interactive TUI

When stdout is a terminal and you run a plain whole-binary decompile, srcdump
opens an interactive browser instead of dumping text. Every function is listed
up front (from a fast catalog pass) and shown as **one continuous document** —
scrolling off the bottom of a function flows straight into the next. Each
function shows a **"⏳ waiting for result…"** banner until its decompilation
streams in:

```
 srcdump  ./program  elf64-x86-64   [4/312]   84/312 decompiled
┌Functions───────────────┐┌decompilation─────────────────────────────────────┐
│  ✓ 0x4015c0 main       ││ // 0x401670 <_start>  .text                       │
│  ✓ 0x401670 _start     ││ void _start(...) { ... }                          │
│▶ · 0x401d30 sub_401d30 ││ // 0x401d30 <sub_401d30>  .text                   │
│  · 0x401e90 sub_401e90 ││     ⏳ waiting for result…                        │
└────────────────────────┘└───────────────────────────────────────────────────┘
 j/k line   ./, function   f panel   t times   h help   q quit
```

| Key | Action |
|-----|--------|
| `j` / `k` | move the highlighted line down / up (crosses function boundaries) |
| `.` / `,` | jump to the next / previous function |
| `f` | toggle the function panel (sidebar) |
| `t` | toggle per-function decompilation times (off by default) |
| `h` | toggle the help panel |
| `q`, `Esc` | quit (terminates any outstanding workers) |

`--list` and `--slowest` are explicit reports that always print to stdout (no
TUI). Force or disable the browser with `--tui always` / `--tui never`. When the
TUI is active the stderr progress line is suppressed (the header shows progress).

### Progress

A live progress line is written to **stderr** (so redirected/piped stdout stays
clean), walking through the pipeline:

```
Loading binary…
Building CFG   42%
Decompiling 31/43  [█████████████████░░░░░░░]  72%  ETA 0:01
Decompiled 43 function(s) in 0:14
```

The function counter sums each worker's stripe, and the ETA is extrapolated from
the observed completion rate. On a warm cache the load/CFG stages are skipped.
Shown only on a TTY by default; force with `--progress always` or silence with
`--progress never`.

### Profiling decompile time

`--slowest <N>` decompiles the selected functions (whole binary by default, or
whatever `-f` selects), times each one, and prints the N slowest descending —
useful for finding which functions dominate a dump:

```
$ srcdump --slowest 5 ./program
// srcdump: ./program — 5 slowest of 312 function(s) by decompile time
RANK     TIME(ms)  ADDRESS             SECTION   NAME
   1      10798.3  0x0000000000401d30  .text     sub_401d30
   2       2018.3  0x0000000000404290  .text     sub_404290
   ...
```

### Choosing the interpreter

Workers need a Python with angr installed. Resolution order:

1. `--python <PATH>`
2. `$SRCDUMP_PYTHON`
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

- **CFG is recovered once and cached** (see *CFG cache* above); workers load it
  rather than recomputing it N times.
- **Per-worker angr import** still has a fixed cost (a few seconds), which caps
  speedup on small binaries; it pays off as the number/size of functions grows.
  A persistent worker pool could amortize the import — a future option.
- Planned: result streaming (`--stream`), a `--fast` decompiler-option
  passthrough, and an output-format toggle.

## Runtime dependencies

The worker interpreter needs angr **and SQLAlchemy** (the latter backs angrdb).
Install with `pip install sqlalchemy` if it's missing.
