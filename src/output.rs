//! Rendering of results to stdout, objdump-flavored.

use crate::backend::DecompileResult;
use std::io::{self, Write};
use std::path::Path;

/// Print decompiled functions (and per-function errors) in address order.
pub fn print_decompilation(out: &mut impl Write, file: &Path, res: &DecompileResult) -> io::Result<()> {
    writeln!(out, "// sourcedump: {}", file.display())?;
    writeln!(out)?;

    for func in &res.functions {
        writeln!(
            out,
            "//{} 0x{:x} <{}>{}",
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
            writeln!(out, "{}", text.trim_end())?;
        }
        writeln!(out)?;
    }

    if !res.errors.is_empty() {
        writeln!(out, "// --- {} function(s) failed to decompile ---", res.errors.len())?;
        for err in &res.errors {
            writeln!(out, "// 0x{:x} <{}>: {}", err.addr, err.name, err.message)?;
        }
    }
    Ok(())
}

/// Print a function listing (no decompilation), objdump -t flavored.
pub fn print_listing(out: &mut impl Write, file: &Path, res: &DecompileResult) -> io::Result<()> {
    writeln!(out, "// sourcedump function list: {}", file.display())?;
    writeln!(out, "{:<18}  {:>8}  NAME", "ADDRESS", "SIZE")?;
    for func in &res.functions {
        writeln!(
            out,
            "0x{:016x}  {:>8}  {}{}",
            func.addr,
            func.size,
            func.name,
            if func.is_plt { "  [plt]" } else { "" }
        )?;
    }
    writeln!(out, "// {} function(s)", res.functions.len())?;
    Ok(())
}
