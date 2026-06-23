use std::process::Command;

/// Embed the git commit (and a `-dirty` marker for uncommitted changes) into the
/// version string so `--version` identifies the exact build. Falls back to just
/// the crate version when git isn't available (e.g. a packaged crate).
fn main() {
    let desc = git_desc().unwrap_or_default();
    println!("cargo:rustc-env=SRCDUMP_GIT_DESC={desc}");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}

fn git_desc() -> Option<String> {
    let hash = git(&["rev-parse", "--short", "HEAD"])?;
    if hash.is_empty() {
        return None;
    }
    let dirty = git(&["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    Some(if dirty {
        format!(" ({hash}-dirty)")
    } else {
        format!(" ({hash})")
    })
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}
