//! Rendering of results to stdout, objdump-flavored.

use crate::backend::DecompileResult;
use crate::highlight::{self, ansi_code};
use crate::model::FunctionInfo;
use std::io::{self, Write};
use std::path::Path;

/// Section label to use when a function isn't inside any named section.
const UNKNOWN_SECTION: &str = "<unknown>";

fn section_name(func: &FunctionInfo) -> &str {
    func.section.as_deref().unwrap_or(UNKNOWN_SECTION)
}

/// objdump's banner line: `<path>:     file format <fmt>`.
fn print_header(out: &mut impl Write, file: &Path, res: &DecompileResult) -> io::Result<()> {
    let fmt = res.file_format.as_deref().unwrap_or("unknown");
    writeln!(out, "{}:     file format {}", file.display(), fmt)
}

/// Print decompiled functions grouped by section, in address order. When
/// `color`, the code is syntax-highlighted with ANSI escapes.
pub fn print_decompilation(
    out: &mut impl Write,
    file: &Path,
    res: &DecompileResult,
    color: bool,
) -> io::Result<()> {
    print_header(out, file, res)?;

    // Functions arrive sorted by address and sections occupy disjoint address
    // ranges, so a section boundary is just a change in the section name of
    // consecutive functions.
    let mut current: Option<&str> = None;
    for func in &res.functions {
        let sec = section_name(func);
        if current != Some(sec) {
            writeln!(out, "\nDecompilation of section {sec}:")?;
            current = Some(sec);
        }
        writeln!(
            out,
            "\n//{} 0x{:x} <{}>{}",
            if func.is_plt { " [plt]" } else { "" },
            func.addr,
            func.name,
            if func.size > 0 {
                format!("  ({} bytes)", func.size)
            } else {
                String::new()
            }
        )?;
        if let Some(text) = &func.text {
            if color {
                write_highlighted(out, text, func)?;
            } else {
                writeln!(out, "{}", text.trim_end())?;
            }
        }
    }

    if !res.errors.is_empty() {
        writeln!(out, "\n// --- {} function(s) failed to decompile ---", res.errors.len())?;
        for err in &res.errors {
            writeln!(out, "// 0x{:x} <{}>: {}", err.addr, err.name, err.message)?;
        }
    }
    Ok(())
}

/// Write one function's code with ANSI syntax highlighting.
fn write_highlighted(out: &mut impl Write, text: &str, func: &FunctionInfo) -> io::Result<()> {
    for line in highlight::highlight_lines(text.trim_end(), &func.spans) {
        for tok in line {
            match ansi_code(tok.kind) {
                Some(code) => write!(out, "\x1b[{code}m{}\x1b[0m", tok.text)?,
                None => write!(out, "{}", tok.text)?,
            }
        }
        writeln!(out)?;
    }
    Ok(())
}

/// Print a function listing (no decompilation), grouped by section.
pub fn print_listing(out: &mut impl Write, file: &Path, res: &DecompileResult) -> io::Result<()> {
    print_header(out, file, res)?;

    let mut current: Option<&str> = None;
    for func in &res.functions {
        let sec = section_name(func);
        if current != Some(sec) {
            writeln!(out, "\nDecompilation of section {sec}:\n")?;
            writeln!(out, "{:<18}  {:>8}  NAME", "ADDRESS", "SIZE")?;
            current = Some(sec);
        }
        writeln!(
            out,
            "0x{:016x}  {:>8}  {}{}",
            func.addr,
            func.size,
            func.name,
            if func.is_plt { "  [plt]" } else { "" }
        )?;
    }
    Ok(())
}

/// Print the `n` slowest functions to decompile, descending by time.
pub fn print_slowest(
    out: &mut impl Write,
    file: &Path,
    res: &DecompileResult,
    n: usize,
) -> io::Result<()> {
    // Only functions that were actually decompiled carry a timing.
    let mut timed: Vec<&FunctionInfo> = res
        .functions
        .iter()
        .filter(|f| f.decompile_ms.is_some())
        .collect();
    timed.sort_by(|a, b| {
        let (ta, tb) = (a.decompile_ms.unwrap_or(0.0), b.decompile_ms.unwrap_or(0.0));
        tb.partial_cmp(&ta).unwrap_or(std::cmp::Ordering::Equal)
    });

    let shown = n.min(timed.len());
    writeln!(
        out,
        "// srcdump: {} — {} slowest of {} function(s) by decompile time",
        file.display(),
        shown,
        timed.len()
    )?;
    writeln!(
        out,
        "{:>4}  {:>11}  {:<18}  {:<8}  NAME",
        "RANK", "TIME(ms)", "ADDRESS", "SECTION"
    )?;
    for (i, func) in timed.iter().take(n).enumerate() {
        writeln!(
            out,
            "{:>4}  {:>11.1}  0x{:016x}  {:<8}  {}{}",
            i + 1,
            func.decompile_ms.unwrap_or(0.0),
            func.addr,
            section_name(func),
            func.name,
            if func.is_plt { "  [plt]" } else { "" }
        )?;
    }
    Ok(())
}
