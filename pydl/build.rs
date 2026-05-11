use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn main() {
    // Re-run wiring. Cargo handles the build-script source itself; everything
    // else needs explicit hints.
    println!("cargo:rerun-if-env-changed=PYDL_RELEASE_BUILD");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let target = std::env::var("TARGET").expect("TARGET set by cargo for build scripts");
    println!("cargo:rustc-env=PYDL_BUILD_TARGET={target}");

    let profile = std::env::var("PROFILE").expect("PROFILE set by cargo for build scripts");
    println!("cargo:rustc-env=PYDL_BUILD_PROFILE={profile}");

    let source = match std::env::var("PYDL_RELEASE_BUILD") {
        Ok(v) if !v.is_empty() => "official",
        _ => "local",
    };
    println!("cargo:rustc-env=PYDL_BUILD_SOURCE={source}");

    let commit = git_short_sha().unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=PYDL_BUILD_COMMIT={commit}");

    let timestamp = httpdate::fmt_http_date(build_timestamp());
    println!("cargo:rustc-env=PYDL_BUILD_TIMESTAMP={timestamp}");
}

/// Short git SHA with a `-dirty` suffix when the working tree has uncommitted
/// changes. Returns `None` when `git` isn't available or we're not in a
/// checkout (e.g. building a published crate tarball).
fn git_short_sha() -> Option<String> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let repo_root: PathBuf = Path::new(&manifest_dir).join("..");

    // Watch the refs cargo would otherwise miss. New commits update HEAD;
    // staging or unstaging changes the index. We deliberately don't watch
    // every tracked file — `git status --porcelain` is run at every build
    // anyway when these two tick.
    let git_dir = repo_root.join(".git");
    if git_dir.exists() {
        println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
        println!(
            "cargo:rerun-if-changed={}",
            git_dir.join("index").display()
        );
    }

    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(&repo_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    if sha.is_empty() {
        return None;
    }

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&repo_root)
        .output()
        .ok()
        .is_some_and(|o| o.status.success() && !o.stdout.is_empty());

    Some(if dirty { format!("{sha}-dirty") } else { sha })
}

/// Build timestamp. Honours `SOURCE_DATE_EPOCH` for reproducible builds, then
/// falls back to wall-clock now.
fn build_timestamp() -> SystemTime {
    if let Ok(s) = std::env::var("SOURCE_DATE_EPOCH")
        && let Ok(secs) = s.trim().parse::<u64>()
    {
        return UNIX_EPOCH + Duration::from_secs(secs);
    }
    SystemTime::now()
}
