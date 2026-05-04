//! On-disc filter config: locate and load the nearest `.pydl.json`.
//!
//! `pydl pin` writes one of these in the current directory; the
//! `download`/`install`/`python`/`uninstall` subcommands read it back and
//! merge it field-by-field with the CLI flags the user passed.

use std::path::{Path, PathBuf};
use std::{fs, io};

use anyhow::{Context, Result};

use crate::filter::FilterConfig;

pub const CONFIG_FILENAME: &str = ".pydl.json";

/// Walk `start` and its ancestors looking for the nearest `.pydl.json`.
///
/// Returns the path of the first match, `None` if none of the ancestors
/// contains one. Any filesystem error other than `NotFound` is surfaced —
/// the walk only continues on `NotFound`, since that's the expected shape
/// of "no config in this directory".
pub fn find_config(start: &Path) -> Result<Option<PathBuf>> {
    let mut current: Option<&Path> = Some(start);
    while let Some(dir) = current {
        let candidate = dir.join(CONFIG_FILENAME);
        match fs::metadata(&candidate) {
            Ok(meta) if meta.is_file() => return Ok(Some(candidate)),
            Ok(_) => {
                // A directory (or symlink loop) at the candidate path;
                // skip it and keep walking upward.
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("checking {}", candidate.display()));
            }
        }
        current = dir.parent();
    }
    Ok(None)
}

/// Read and deserialize the given `.pydl.json` path into a `FilterConfig`.
///
/// Both I/O errors and JSON parse errors are wrapped with the file path so
/// the user knows where to look.
pub fn load_config(path: &Path) -> Result<FilterConfig> {
    let body = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str::<FilterConfig>(&body)
        .with_context(|| format!("parsing {} as .pydl.json", path.display()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn find_config_returns_none_for_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let hit = find_config(tmp.path()).unwrap();
        assert!(hit.is_none());
    }

    #[test]
    fn find_config_finds_file_in_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let here = tmp.path().join(CONFIG_FILENAME);
        fs::write(&here, "{}").unwrap();
        let hit = find_config(tmp.path()).unwrap().unwrap();
        assert_eq!(hit, here);
    }

    #[test]
    fn find_config_walks_upward() {
        // Structure: <tmp>/a/.pydl.json   <tmp>/a/b/c/
        // Starting at a/b/c we should hit the a/.pydl.json.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let a = root.join("a");
        let b_c = a.join("b").join("c");
        fs::create_dir_all(&b_c).unwrap();
        let cfg = a.join(CONFIG_FILENAME);
        fs::write(&cfg, "{}").unwrap();

        let hit = find_config(&b_c).unwrap().unwrap();
        assert_eq!(hit, cfg);
    }

    #[test]
    fn find_config_prefers_nearest() {
        // Two configs: <tmp>/a/.pydl.json and <tmp>/a/b/.pydl.json
        // Start at a/b/c and confirm the b/ one wins.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let a = root.join("a");
        let b = a.join("b");
        let c = b.join("c");
        fs::create_dir_all(&c).unwrap();
        let outer = a.join(CONFIG_FILENAME);
        let inner = b.join(CONFIG_FILENAME);
        fs::write(&outer, "{}").unwrap();
        fs::write(&inner, "{}").unwrap();

        let hit = find_config(&c).unwrap().unwrap();
        assert_eq!(hit, inner);
    }

    #[test]
    fn load_config_parses_valid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CONFIG_FILENAME);
        fs::write(
            &path,
            r#"{"tag": "20260414", "version": "3.14.4", "platform": true, "default_attrs": false}"#,
        )
        .unwrap();

        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.tag.as_deref(), Some("20260414"));
        assert_eq!(cfg.version, "3.14.4");
        assert!(cfg.platform);
        assert!(!cfg.default_attrs);
    }

    #[test]
    fn load_config_rejects_empty_object() {
        // `version` is mandatory, so a bare `{}` is no longer valid.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CONFIG_FILENAME);
        fs::write(&path, "{}").unwrap();
        let err = load_config(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("version"),
            "expected missing-version error: {msg}"
        );
    }

    #[test]
    fn load_config_parses_minimal_json() {
        // Only mandatory `version`; tag absent, booleans default to true.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CONFIG_FILENAME);
        fs::write(&path, r#"{"version": "3.14.4"}"#).unwrap();
        let cfg = load_config(&path).unwrap();
        assert_eq!(cfg.tag, None);
        assert_eq!(cfg.version, "3.14.4");
        assert!(cfg.platform);
        assert!(cfg.default_attrs);
    }

    #[test]
    fn load_config_rejects_corrupt_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CONFIG_FILENAME);
        fs::write(&path, "{not json").unwrap();

        let err = load_config(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&path.display().to_string()),
            "error should name the file path: {msg}"
        );
    }
}
