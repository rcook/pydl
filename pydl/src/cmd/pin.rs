use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;
use log::info;
use pydl_common::asset::{ParsedAsset, asset_sort_key, is_prerelease_key, parse_version_key};
use pydl_common::checksums::{embedded_tags, has_tag, iter_embedded_assets};
use pydl_common::config::CONFIG_FILENAME;
use pydl_common::filter::{FilterArgs, FilterConfig, name_matches_filters};

#[derive(Parser, Debug)]
pub struct Args {
    #[command(flatten)]
    pub filter: FilterArgs,

    /// Overwrite `.pydl.json` if it already exists. Without this, `pin`
    /// refuses to clobber an existing file.
    #[arg(short = 'f', long)]
    pub force: bool,
}

// `args` by value matches the dispatch shape of every other subcommand.
#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> Result<()> {
    let Args { filter, force } = args;

    // Remember whether --tag was CLI-supplied so we can decide whether to
    // emit `tag` in the file (vs. letting the pin float to the newest
    // embedded tag that carries the selected version).
    let user_supplied_tag = filter.tag.is_some();

    // Step 1: pick the tag (or validate user-supplied one) against the
    // embedded checksum set — `pin` is an offline operation.
    let tag = if let Some(requested) = filter.tag.as_deref() {
        if !has_tag(requested) {
            bail!(
                "tag {requested:?} isn't in the embedded checksum set — run `pydl available` to see \
                 what this binary supports, or rebuild with a newer ./checksums/ directory"
            );
        }
        requested.to_owned()
    } else {
        // Tags are YYYYMMDD date stamps; lexicographic max == newest.
        embedded_tags()
            .into_iter()
            .max()
            .context("no embedded release tags available")?
            .to_owned()
    };

    // Step 2: gather candidate asset names for the selected tag under the
    // platform/default-attrs filters from the CLI. Deliberately omit the
    // version filter here — we validate/default it separately below so we
    // can tell "version wrong" apart from "platform/default-attrs left no
    // candidates".
    let mut probe = filter.clone();
    probe.version = None;
    let resolved = probe.resolve();
    let candidate_names: Vec<&'static str> = iter_embedded_assets()
        .filter(|(t, _)| *t == tag)
        .filter(|(_, name)| name_matches_filters(name, resolved))
        .map(|(_, name)| name)
        .collect();
    if candidate_names.is_empty() {
        bail!(
            "release {tag:?} has no assets matching the current platform/default-attrs filters — \
             try --no-platform or --no-default-attrs"
        );
    }

    // Step 3: pick the version (or validate user-supplied one) against the
    // candidate names.
    let version = if let Some(requested) = filter.version.as_deref() {
        let found = candidate_names
            .iter()
            .any(|name| ParsedAsset::parse(name).is_ok_and(|p| p.version == requested));
        if !found {
            bail!(
                "no asset with version {requested:?} found in release {tag:?} \
                 (under the current platform/default-attrs filters)"
            );
        }
        requested.to_owned()
    } else {
        pick_newest_version(&candidate_names).with_context(|| {
            format!("no parseable assets in release {tag:?} to infer a version from")
        })?
    };

    info!("selected tag={tag}, version={version}");

    let config = FilterConfig {
        // Only emit `tag` when the user explicitly passed --tag. With no
        // --tag the pin should remain floating: future subcommand runs will
        // re-resolve "newest embedded tag that has this version".
        tag: user_supplied_tag.then(|| tag.clone()),
        version,
        platform: !filter.no_platform,
        default_attrs: !filter.no_default_attrs,
    };

    write_config(Path::new("."), &config, force)
}

/// Given a non-empty slice of asset names, return the newest
/// **non-prerelease** Python version among them.
///
/// `asset_sort_key` already orders parseable assets before unparseable ones
/// and places the newest version first (descending), so iterating sorted
/// and picking the first parseable *final* hit gives the right answer.
/// Prereleases (`3.15.0a8`, `3.15.0b1`, `3.15.0rc2`, …) are deliberately
/// skipped to match the user expectation that an unpinned `pydl pin` picks
/// a stable Python. If the release *only* contains prereleases, we bail
/// with an actionable message pointing at `-v` as the override.
fn pick_newest_version(names: &[&str]) -> Result<String> {
    let mut sorted: Vec<&str> = names.to_vec();
    sorted.sort_by_cached_key(|n| asset_sort_key(n));
    for name in &sorted {
        let Ok(parsed) = ParsedAsset::parse(name) else {
            continue;
        };
        if !is_prerelease_key(&parse_version_key(&parsed.version)) {
            return Ok(parsed.version);
        }
    }
    bail!(
        "the selected release has only prerelease Python versions available — \
         pass an explicit `--version` to opt into one"
    );
}

/// Serialize `config` as JSON and write it to `<dir>/.pydl.json`.
///
/// When `force` is `false`, an existing file at that path is preserved and
/// the function errors out. When `force` is `true`, the existing file is
/// truncated and replaced. Factored out so tests can target a tempdir
/// without mutating process-wide `cwd`.
fn write_config(dir: &Path, config: &FilterConfig, force: bool) -> Result<()> {
    let path: PathBuf = dir.join(CONFIG_FILENAME);
    let body = serde_json::to_string_pretty(config).context("serializing filter config to JSON")?;

    // We distinguish "replaced an existing file" from "wrote a new one"
    // purely for the log line. Check existence before opening; the race
    // window doesn't matter because the honest value is what was on disc
    // when we started writing.
    let replaced = force && path.exists();

    // Without `--force`: `create_new(true)` gives us an atomic "fail if
    // exists" check at the kernel level — no race between `exists()` and
    // `create()`. With `--force`: `create(true).truncate(true)` overwrites.
    let open_result = if force {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
    } else {
        OpenOptions::new().write(true).create_new(true).open(&path)
    };
    let mut file = match open_result {
        Ok(f) => f,
        Err(e) if !force && e.kind() == io::ErrorKind::AlreadyExists => {
            bail!(
                "{} already exists — refusing to overwrite (pass --force to replace)",
                path.display()
            );
        }
        Err(e) => {
            return Err(e).with_context(|| format!("creating {}", path.display()));
        }
    };

    file.write_all(body.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("writing {}", path.display()))?;

    if replaced {
        info!("wrote {} (overwrote existing file)", path.display());
    } else {
        info!("wrote {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn sample_config() -> FilterConfig {
        FilterConfig {
            tag: Some("20260414".to_owned()),
            version: "3.14.4".to_owned(),
            platform: true,
            default_attrs: true,
        }
    }

    #[test]
    fn writes_config_file_with_filters() {
        let tmp = tempfile::tempdir().unwrap();
        let config = sample_config();
        write_config(tmp.path(), &config, false).unwrap();

        let body = fs::read_to_string(tmp.path().join(CONFIG_FILENAME)).unwrap();
        let parsed: FilterConfig = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed, config);
        // File ends with a trailing newline (POSIX-friendly).
        assert!(body.ends_with('\n'));
    }

    #[test]
    fn fails_if_config_already_exists_and_preserves_original() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(CONFIG_FILENAME), "original contents").unwrap();

        let err = write_config(tmp.path(), &sample_config(), false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("already exists"), "unexpected error: {msg}");
        assert!(
            msg.contains("--force"),
            "error should suggest --force: {msg}"
        );

        // Existing file untouched.
        let body = fs::read_to_string(tmp.path().join(CONFIG_FILENAME)).unwrap();
        assert_eq!(body, "original contents");
    }

    #[test]
    fn writes_minimal_config_omits_tag_and_default_booleans() {
        // A config with no `tag` and the boolean defaults (`true`) should
        // serialize to a minimal `{"version": "..."}` document. Each
        // default-valued field is elided so only deviations land on disc.
        let tmp = tempfile::tempdir().unwrap();
        let config = FilterConfig {
            tag: None,
            version: "3.14.4".to_owned(),
            platform: true,
            default_attrs: true,
        };
        write_config(tmp.path(), &config, false).unwrap();
        let body = fs::read_to_string(tmp.path().join(CONFIG_FILENAME)).unwrap();
        assert!(!body.contains("tag"), "tag should be elided: {body}");
        assert!(
            !body.contains("platform"),
            "default platform=true should be elided: {body}"
        );
        assert!(
            !body.contains("default_attrs"),
            "default default_attrs=true should be elided: {body}"
        );
        assert!(body.contains("\"version\""), "got: {body}");

        // The minimal document still round-trips to a config equal to the
        // input — the elided defaults are restored on deserialization.
        let parsed: FilterConfig = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn writes_explicit_false_booleans_so_no_flags_survive_round_trip() {
        // `--no-platform` / `--no-default-attrs` flip the booleans to false,
        // which deviates from the default and must be written explicitly.
        let tmp = tempfile::tempdir().unwrap();
        let config = FilterConfig {
            tag: None,
            version: "3.14.4".to_owned(),
            platform: false,
            default_attrs: false,
        };
        write_config(tmp.path(), &config, false).unwrap();
        let body = fs::read_to_string(tmp.path().join(CONFIG_FILENAME)).unwrap();
        assert!(body.contains("\"platform\""), "got: {body}");
        assert!(body.contains("\"default_attrs\""), "got: {body}");
        let parsed: FilterConfig = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn force_overwrites_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(CONFIG_FILENAME), "stale contents").unwrap();

        let config = sample_config();
        write_config(tmp.path(), &config, true).unwrap();

        // The file now contains the newly-serialized config, not the stale bytes.
        let body = fs::read_to_string(tmp.path().join(CONFIG_FILENAME)).unwrap();
        let parsed: FilterConfig = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed, config);
        assert!(
            !body.contains("stale"),
            "stale bytes should be gone: {body}"
        );
    }

    #[test]
    fn force_writes_when_file_absent() {
        // --force on a nonexistent file must still succeed (it's semantically
        // "write regardless", not "require existing file").
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), &sample_config(), true).unwrap();
        let body = fs::read_to_string(tmp.path().join(CONFIG_FILENAME)).unwrap();
        let parsed: FilterConfig = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed, sample_config());
    }

    #[test]
    fn force_fully_truncates_shorter_replacement() {
        // Guard against "write the new body over the old one without
        // truncating" bugs: a shorter new body must not leave trailing
        // bytes from the old one.
        let tmp = tempfile::tempdir().unwrap();
        let long_stale = "A".repeat(10_000);
        fs::write(tmp.path().join(CONFIG_FILENAME), &long_stale).unwrap();

        write_config(tmp.path(), &sample_config(), true).unwrap();
        let body = fs::read_to_string(tmp.path().join(CONFIG_FILENAME)).unwrap();
        assert!(body.len() < long_stale.len(), "file should be truncated");
        assert!(!body.contains('A'), "no stale bytes should remain: {body}");
    }

    #[test]
    fn pick_newest_version_returns_highest() {
        let names = [
            "cpython-3.10.0+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz",
            "cpython-3.14.4+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz",
            "cpython-3.12.1+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz",
        ];
        assert_eq!(pick_newest_version(&names).unwrap(), "3.14.4");
    }

    #[test]
    fn pick_newest_version_prefers_final_over_prerelease() {
        // 3.15.0 beats 3.15.0a8 in descending version order.
        let names = [
            "cpython-3.15.0a8+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz",
            "cpython-3.15.0+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz",
        ];
        assert_eq!(pick_newest_version(&names).unwrap(), "3.15.0");
    }

    #[test]
    fn pick_newest_version_skips_unparseable_names() {
        // Unparseable names should be skipped, with the first parseable one
        // in sort order winning.
        let names = [
            "SHA256SUMS",
            "cpython-3.10.0+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz",
        ];
        assert_eq!(pick_newest_version(&names).unwrap(), "3.10.0");
    }

    #[test]
    fn pick_newest_version_skips_prerelease_to_pick_lower_final() {
        // 3.15.0a8 would sort newest under descending order, but it's a
        // prerelease; `pick_newest_version` should skip past it and return
        // the 3.14.4 final even though that's numerically lower.
        let names = [
            "cpython-3.15.0a8+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz",
            "cpython-3.14.4+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz",
        ];
        assert_eq!(pick_newest_version(&names).unwrap(), "3.14.4");
    }

    #[test]
    fn pick_newest_version_errors_when_only_prereleases_available() {
        // All-prerelease set: the helper refuses to return one and points at
        // the `-v` escape hatch.
        let names = [
            "cpython-3.15.0a8+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz",
            "cpython-3.15.0b1+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz",
        ];
        let err = pick_newest_version(&names).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("prerelease"), "got: {msg}");
        assert!(
            msg.contains("--version") || msg.contains("-v"),
            "got: {msg}"
        );
    }
}
