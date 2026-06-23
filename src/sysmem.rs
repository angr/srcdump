//! System memory queries.

/// Available system memory in bytes, when determinable. Returns `None` on
/// unsupported platforms or on failure (callers then skip memory-based
/// heuristics and show "n/a").
pub fn available_bytes() -> Option<u64> {
    platform_available()
}

/// Linux: `MemAvailable` from `/proc/meminfo`.
#[cfg(target_os = "linux")]
fn platform_available() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

/// Windows: available physical memory via `GlobalMemoryStatusEx`.
#[cfg(windows)]
fn platform_available() -> Option<u64> {
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }
    let mut status: MemoryStatusEx = unsafe { std::mem::zeroed() };
    status.length = std::mem::size_of::<MemoryStatusEx>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 {
        Some(status.avail_phys)
    } else {
        None
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn platform_available() -> Option<u64> {
    None
}

/// Format a byte count as `GiB`/`MiB`.
pub fn human_bytes(bytes: u64) -> String {
    let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gib >= 1.0 {
        format!("{gib:.1} GiB")
    } else {
        format!("{:.0} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_units() {
        assert_eq!(human_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
        assert_eq!(human_bytes(512 * 1024 * 1024), "512 MiB");
    }
}
