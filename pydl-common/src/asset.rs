// Parser for python-build-standalone release asset names.
//
// Grammar (informal):
//
//   <implementation>-<version>+<build_tag>-<target_triple>-<flavour>.<ext>[.sha256]
//
// The ambiguity is that both `target_triple` and `flavour` can contain `-` and
// `+`, so we can't split on separators alone. We handle that by suffix-stripping
// a known extension, then a known flavour and treating whatever is left between
// the build tag and the flavour as the target triple.
//
// Examples the parser accepts:
//
//   cpython-3.12.0+20231002-x86_64-unknown-linux-gnu-install_only.tar.gz
//   cpython-3.11.4+20230826-x86_64-pc-windows-msvc-pgo+lto-full.tar.zst
//   cpython-3.13.0+20241008-aarch64-apple-darwin-install_only.tar.gz.sha256

use std::cmp::Reverse;

use anyhow::{Result, anyhow, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Extension {
    TarGz,
    TarZst,
}

impl Extension {
    fn strip(name: &str) -> Option<(&str, Self)> {
        name.strip_suffix(".tar.zst")
            .map(|stem| (stem, Self::TarZst))
            .or_else(|| name.strip_suffix(".tar.gz").map(|stem| (stem, Self::TarGz)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAsset {
    pub implementation: String,
    pub version: String,
    pub build_tag: String,
    pub target_triple: String,
    pub flavour: &'static str,
    pub extension: Extension,
    pub is_checksum: bool,
}

/// Closed set of build flavours we recognize. `strip_suffix` is a literal match,
/// so entries can never collide on a common tail (e.g. `install_only_stripped`
/// and `install_only` are both safe to list regardless of order). Extend this
/// list as new flavours appear upstream.
const KNOWN_FLAVOURS: &[&str] = &[
    "install_only_stripped",
    "install_only",
    "freethreaded+debug-pgo+lto-full",
    "freethreaded+pgo+lto-full",
    "freethreaded+debug-full",
    "freethreaded+noopt-full",
    "debug-pgo+lto-full",
    "pgo+lto-full",
    "debug-pgo-full",
    "pgo-full",
    "lto-full",
    "debug-full",
    "noopt-full",
];

/// Trailing non-numeric portion of a version segment.
///
/// `Pre` sorts *less than* `Final` so `3.15.0a8 < 3.15.0` (an alpha precedes
/// its final release). Enum discriminant order is the `Ord` order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VersionTrailer {
    Pre(String),
    Final,
}

/// Ordered version key parsed from a `version` string like `3.10.0` or
/// `3.15.0a8`. Designed for direct `Ord` comparison via slice lexicographic
/// ordering — no custom cmp function needed.
///
/// Each dot-separated segment becomes `(numeric_prefix, trailer)`. Segments
/// with no numeric prefix (`"abc"`) get `0` for the number and the whole
/// string as the trailer.
pub type VersionKey = Vec<(u32, VersionTrailer)>;

#[must_use]
pub fn parse_version_key(version: &str) -> VersionKey {
    version
        .split('.')
        .map(|segment| {
            let prefix_len = segment.bytes().take_while(u8::is_ascii_digit).count();
            let (num_str, trailer) = segment.split_at(prefix_len);
            let num = num_str.parse::<u32>().unwrap_or(0);
            let trailer = if trailer.is_empty() {
                VersionTrailer::Final
            } else {
                VersionTrailer::Pre(trailer.to_owned())
            };
            (num, trailer)
        })
        .collect()
}

/// Does this parsed version key represent a prerelease?
///
/// A version is a prerelease iff any dotted segment carries a non-numeric
/// trailer (e.g. `3.15.0a8` → last segment parses as `(0, Pre("a8"))`). The
/// `CPython` convention only ever attaches the trailer to the final
/// segment, but this predicate is deliberately lenient — any `Pre` anywhere
/// counts.
#[must_use]
pub fn is_prerelease_key(key: &VersionKey) -> bool {
    key.iter()
        .any(|(_, trailer)| matches!(trailer, VersionTrailer::Pre(_)))
}

/// Sort key for asset-list ordering.
///
/// Assets whose names parse sort first by semantic version in **descending**
/// order (newer Python versions float to the top), then by the full name as
/// a deterministic tiebreaker (flavour/triple). Assets that fail to parse
/// (e.g. `SHA256SUMS`) group at the end, still sorted by name among
/// themselves.
///
/// The version is wrapped in `Reverse` so a standard ascending `sort` on the
/// whole key yields descending version order without extra effort at call
/// sites.
#[must_use]
pub fn asset_sort_key(name: &str) -> (bool, Reverse<VersionKey>, String) {
    ParsedAsset::parse(name).map_or_else(
        |_| (true, Reverse(Vec::new()), name.to_owned()),
        |p| {
            (
                false,
                Reverse(parse_version_key(&p.version)),
                name.to_owned(),
            )
        },
    )
}

impl ParsedAsset {
    pub fn parse(name: &str) -> Result<Self> {
        let (core, is_checksum) = name
            .strip_suffix(".sha256")
            .map_or((name, false), |stem| (stem, true));

        let (stem, extension) =
            Extension::strip(core).ok_or_else(|| anyhow!("unrecognized extension in {name:?}"))?;

        let (implementation, rest) = stem
            .split_once('-')
            .ok_or_else(|| anyhow!("missing '-' after implementation in {name:?}"))?;

        let (version, rest) = rest
            .split_once('+')
            .ok_or_else(|| anyhow!("missing '+' between version and build tag in {name:?}"))?;

        let (build_tag, rest) = rest
            .split_once('-')
            .ok_or_else(|| anyhow!("missing '-' after build tag in {name:?}"))?;

        let (triple, flavour) = KNOWN_FLAVOURS
            .iter()
            .find_map(|flavour| {
                let prefix = rest.strip_suffix(*flavour)?;
                let triple = prefix.strip_suffix('-')?;
                Some((triple, *flavour))
            })
            .ok_or_else(|| anyhow!("unknown flavour in {name:?}"))?;

        if triple.is_empty() {
            bail!("empty target triple in {name:?}");
        }

        Ok(Self {
            implementation: implementation.to_owned(),
            version: version.to_owned(),
            build_tag: build_tag.to_owned(),
            target_triple: triple.to_owned(),
            flavour,
            extension,
            is_checksum,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_linux_install_only() {
        let parsed = ParsedAsset::parse(
            "cpython-3.12.0+20231002-x86_64-unknown-linux-gnu-install_only.tar.gz",
        )
        .unwrap();
        assert_eq!(parsed.implementation, "cpython");
        assert_eq!(parsed.version, "3.12.0");
        assert_eq!(parsed.build_tag, "20231002");
        assert_eq!(parsed.target_triple, "x86_64-unknown-linux-gnu");
        assert_eq!(parsed.flavour, "install_only");
        assert_eq!(parsed.extension, Extension::TarGz);
        assert!(!parsed.is_checksum);
    }

    #[test]
    fn parses_install_only_stripped() {
        let parsed = ParsedAsset::parse(
            "cpython-3.12.0+20231002-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz",
        )
        .unwrap();
        assert_eq!(parsed.flavour, "install_only_stripped");
        assert_eq!(parsed.target_triple, "x86_64-unknown-linux-gnu");
    }

    #[test]
    fn parses_windows_triple_with_three_internal_dashes() {
        let parsed = ParsedAsset::parse(
            "cpython-3.12.0+20231002-x86_64-pc-windows-msvc-install_only.tar.gz",
        )
        .unwrap();
        assert_eq!(parsed.target_triple, "x86_64-pc-windows-msvc");
        assert_eq!(parsed.flavour, "install_only");
    }

    #[test]
    fn parses_macos_aarch64() {
        let parsed =
            ParsedAsset::parse("cpython-3.12.0+20231002-aarch64-apple-darwin-install_only.tar.gz")
                .unwrap();
        assert_eq!(parsed.target_triple, "aarch64-apple-darwin");
        assert_eq!(parsed.flavour, "install_only");
    }

    #[test]
    fn parses_pgo_plus_lto_with_internal_plus() {
        let parsed = ParsedAsset::parse(
            "cpython-3.11.4+20230826-x86_64-unknown-linux-gnu-pgo+lto-full.tar.zst",
        )
        .unwrap();
        assert_eq!(parsed.flavour, "pgo+lto-full");
        assert_eq!(parsed.extension, Extension::TarZst);
        assert_eq!(parsed.target_triple, "x86_64-unknown-linux-gnu");
    }

    #[test]
    fn parses_debug_full() {
        let parsed = ParsedAsset::parse(
            "cpython-3.11.4+20230826-x86_64-unknown-linux-gnu-debug-full.tar.zst",
        )
        .unwrap();
        assert_eq!(parsed.flavour, "debug-full");
    }

    #[test]
    fn parses_musl_target() {
        let parsed = ParsedAsset::parse(
            "cpython-3.12.0+20231002-x86_64-unknown-linux-musl-install_only.tar.gz",
        )
        .unwrap();
        assert_eq!(parsed.target_triple, "x86_64-unknown-linux-musl");
    }

    #[test]
    fn parses_i686_windows_pgo() {
        let parsed =
            ParsedAsset::parse("cpython-3.12.0+20231002-i686-pc-windows-msvc-pgo-full.tar.zst")
                .unwrap();
        assert_eq!(parsed.target_triple, "i686-pc-windows-msvc");
        assert_eq!(parsed.flavour, "pgo-full");
    }

    #[test]
    fn parses_checksum_sidecar() {
        let parsed = ParsedAsset::parse(
            "cpython-3.12.0+20231002-x86_64-unknown-linux-gnu-install_only.tar.gz.sha256",
        )
        .unwrap();
        assert!(parsed.is_checksum);
        assert_eq!(parsed.flavour, "install_only");
        assert_eq!(parsed.extension, Extension::TarGz);
    }

    #[test]
    fn parses_freethreaded_variant() {
        let parsed = ParsedAsset::parse(
            "cpython-3.13.0+20241008-x86_64-unknown-linux-gnu-freethreaded+pgo+lto-full.tar.zst",
        )
        .unwrap();
        assert_eq!(parsed.flavour, "freethreaded+pgo+lto-full");
        assert_eq!(parsed.target_triple, "x86_64-unknown-linux-gnu");
    }

    #[test]
    fn rejects_unknown_extension() {
        let err = ParsedAsset::parse("something.zip").unwrap_err().to_string();
        assert!(err.contains("unrecognized extension"), "got: {err}");
    }

    #[test]
    fn rejects_missing_build_tag_separator() {
        let err = ParsedAsset::parse("cpython-3.12.0-x86_64-unknown-linux-gnu-install_only.tar.gz")
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing '+'"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_flavour() {
        let err = ParsedAsset::parse(
            "cpython-3.12.0+20231002-x86_64-unknown-linux-gnu-weirdflavour.tar.gz",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown flavour"), "got: {err}");
    }

    #[test]
    fn rejects_extensionless_manifest() {
        let err = ParsedAsset::parse("SHA256SUMS").unwrap_err().to_string();
        assert!(err.contains("unrecognized extension"), "got: {err}");
    }

    #[test]
    fn rejects_empty_target_triple() {
        let err = ParsedAsset::parse("cpython-3.12.0+20231002--install_only.tar.gz")
            .unwrap_err()
            .to_string();
        assert!(err.contains("empty target triple"), "got: {err}");
    }

    // ----- version-ordered sort key -----

    #[test]
    fn version_key_orders_3_10_after_3_9() {
        // The whole point: lexicographic "3.10.0" < "3.9.1" is wrong;
        // numeric "3.10.0" > "3.9.1" is right.
        assert!(parse_version_key("3.9.1") < parse_version_key("3.10.0"));
    }

    #[test]
    fn version_key_orders_patch_levels() {
        assert!(parse_version_key("3.10.0") < parse_version_key("3.10.1"));
        assert!(parse_version_key("3.10.9") < parse_version_key("3.10.20"));
    }

    #[test]
    fn version_key_treats_prerelease_as_less_than_final() {
        // 3.15.0a8 is an alpha of 3.15.0 and should sort before 3.15.0.
        assert!(parse_version_key("3.15.0a8") < parse_version_key("3.15.0"));
        // Two alphas: 3.15.0a7 < 3.15.0a8 (string compare on the trailer).
        assert!(parse_version_key("3.15.0a7") < parse_version_key("3.15.0a8"));
    }

    #[test]
    fn version_key_handles_different_segment_counts() {
        // `3.10` (two segments) compared to `3.10.0` (three). Shorter vec is
        // less under lexicographic ordering, which is what we want:
        // 3.10 "comes before" 3.10.0 in a listing. Assert that explicitly.
        assert!(parse_version_key("3.10") < parse_version_key("3.10.0"));
    }

    #[test]
    fn is_prerelease_key_flags_alpha_and_beta_and_rc() {
        assert!(is_prerelease_key(&parse_version_key("3.15.0a8")));
        assert!(is_prerelease_key(&parse_version_key("3.15.0b1")));
        assert!(is_prerelease_key(&parse_version_key("3.15.0rc2")));
    }

    #[test]
    fn is_prerelease_key_does_not_flag_final_releases() {
        assert!(!is_prerelease_key(&parse_version_key("3.14.4")));
        assert!(!is_prerelease_key(&parse_version_key("3.10.0")));
        assert!(!is_prerelease_key(&parse_version_key("3")));
    }

    #[test]
    fn asset_sort_key_parseable_before_unparseable() {
        // Parseable assets come first; unparseable names (like SHA256SUMS)
        // group at the end but still sort among themselves.
        let parseable =
            asset_sort_key("cpython-3.10.0+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz");
        let unparseable = asset_sort_key("SHA256SUMS");
        assert!(parseable < unparseable);
    }

    #[test]
    fn asset_sort_key_primary_is_version_descending() {
        // Newer Python versions float to the top: 3.10.0 < 3.9.1 in the key,
        // so that ascending `sort` places 3.10.0 before 3.9.1.
        let newer =
            asset_sort_key("cpython-3.10.0+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz");
        let older =
            asset_sort_key("cpython-3.9.1+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz");
        assert!(newer < older, "3.10.0 should precede 3.9.1 (descending)");
    }

    #[test]
    fn asset_sort_key_final_before_prerelease_under_descending_order() {
        // With descending version ordering, the final release should come
        // before its alpha: 3.15.0 < 3.15.0a8 in key order.
        let final_rel =
            asset_sort_key("cpython-3.15.0+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz");
        let alpha = asset_sort_key(
            "cpython-3.15.0a8+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz",
        );
        assert!(final_rel < alpha, "3.15.0 should precede 3.15.0a8");
    }

    #[test]
    fn asset_sort_key_secondary_is_full_name() {
        // Same version — stable deterministic tiebreaker by name.
        let debug =
            asset_sort_key("cpython-3.14.4+20260414-x86_64-unknown-linux-gnu-debug-full.tar.zst");
        let install =
            asset_sort_key("cpython-3.14.4+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz");
        assert!(debug < install);
    }

    #[test]
    fn asset_sort_key_orders_freethreaded_between_same_version() {
        // Real-world adjacency check inside a single version: freethreaded
        // variants should sit next to the plain ones since the name is
        // the tiebreaker after version.
        let plain =
            asset_sort_key("cpython-3.13.13+20260414-aarch64-apple-darwin-install_only.tar.gz");
        let freethreaded = asset_sort_key(
            "cpython-3.13.13+20260414-aarch64-apple-darwin-freethreaded+pgo+lto-full.tar.zst",
        );
        // `freethreaded…` comes before `install_only` alphabetically at the
        // flavour level; version component is identical so the name tiebreak
        // kicks in.
        assert!(freethreaded < plain);
    }
}
