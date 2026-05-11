//! Shared release-list data model, CLI filter arguments and filter logic
//! for the `pydl` subcommands that take filter flags.

use anyhow::{Result, anyhow, bail};
use clap::Parser;
use log::warn;
use serde::{Deserialize, Serialize};

use crate::asset::ParsedAsset;
use crate::platform::{DefaultAttrs, Platform};

/// CLI filter arguments shared by the `pydl` subcommands.
#[derive(Parser, Debug, Clone)]
// Each `--foo`/`--no-foo` pair needs two bools for clap's `overrides_with`;
// the trigger from clippy's `struct_excessive_bools` is a false positive here.
#[allow(clippy::struct_excessive_bools)]
pub struct FilterArgs {
    /// Limit output to the release with this tag (e.g. `20260414`).
    #[arg(short = 't', long, value_name = "TAG")]
    pub tag: Option<String>,

    /// Limit output to assets whose parsed Python version matches
    /// (e.g. `3.14.4` or `3.15.0a8`). Assets whose filenames don't follow the
    /// `python-build-standalone` naming scheme are always excluded when this
    /// filter is set.
    #[arg(short = 'v', long, value_name = "VERSION")]
    pub version: Option<String>,

    /// Filter assets down to those built for the current platform.
    /// Enabled by default; pass `--no-platform` to see every asset regardless
    /// of target triple.
    #[arg(long, overrides_with = "no_platform")]
    pub platform: bool,

    /// Disable the `--platform` filter. Inverse of `--platform`.
    #[arg(long = "no-platform", overrides_with = "platform")]
    pub no_platform: bool,

    /// Pick the canonical redistributable for the current platform — the
    /// flavour and target triple most users want. Enabled by default; pass
    /// `--no-default-attrs` to widen the filter and see every variant
    /// upstream ships (debug builds, alternative C runtimes, etc.). Implies
    /// `--platform`, since "canonical" is platform-specific.
    #[arg(long, overrides_with = "no_default_attrs")]
    pub default_attrs: bool,

    /// Disable the `--default-attrs` filter.
    #[arg(long = "no-default-attrs", overrides_with = "default_attrs")]
    pub no_default_attrs: bool,
}

/// Normalized, serializable form of the filter CLI flags.
///
/// `FilterArgs` carries two bools per `--foo`/`--no-foo` pair for clap's
/// `overrides_with` dance. `FilterConfig` collapses each pair into a single
/// boolean for JSON (de)serialization. `version` is mandatory on disc —
/// `pydl pin` always resolves to a concrete version before writing, and
/// loading a file without it is a hard error. `tag` is optional: when
/// present it pins a specific release; when absent the subcommands resolve
/// to the newest release that has the recorded `version`. Boolean defaults
/// match the CLI defaults: the on-disc form omits `platform` and
/// `default_attrs` when they're `true`, and a missing field deserializes
/// back to `true`. So `pydl pin` with no platform/default-attrs flags
/// produces a minimal `{"version": "…"}` document that round-trips to the
/// same behaviour as invoking `pydl <subcommand>` with no flags. A `false`
/// value (the user passed `--no-platform` / `--no-default-attrs`) is
/// always written explicitly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,

    /// Mandatory. Missing in the JSON is a deserialize error.
    pub version: String,

    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub platform: bool,

    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub default_attrs: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn default_true() -> bool {
    true
}

// Used by `skip_serializing_if` on the boolean fields above so a pin that
// just confirms the defaults round-trips to a minimal JSON document.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_true(b: &bool) -> bool {
    *b
}

impl Default for FilterConfig {
    /// The default `FilterConfig` has an **empty** `version`. Real configs
    /// always carry a concrete version — this default only exists so tests
    /// can compare against a baseline "the two boolean defaults" state.
    fn default() -> Self {
        Self {
            tag: None,
            version: String::new(),
            platform: true,
            default_attrs: true,
        }
    }
}

impl From<&FilterArgs> for FilterConfig {
    /// Build a `FilterConfig` reflecting the flags on `args`. `version` on
    /// the result is `args.version.clone().unwrap_or_default()` — callers
    /// that go on to *write* the config must have already resolved a
    /// non-empty version (see `pydl pin`). This conversion is used in
    /// tests and in read-side paths where the empty-string fallback is
    /// never exercised.
    fn from(args: &FilterArgs) -> Self {
        // The `--foo`/`--no-foo` pair: if `no_foo` was set, the feature is
        // off; otherwise on (including the case where neither flag was
        // given, which is the default-on state).
        Self {
            tag: args.tag.clone(),
            version: args.version.clone().unwrap_or_default(),
            platform: !args.no_platform,
            default_attrs: !args.no_default_attrs,
        }
    }
}

/// A GitHub release as returned by the REST API, reduced to the fields we use.
#[derive(Deserialize, Debug, Clone)]
pub struct Release {
    pub tag_name: String,
    pub name: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
    pub published_at: Option<String>,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

/// One asset attached to a release.
#[derive(Deserialize, Debug, Clone)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    /// The direct download URL (redirects to the CDN). GitHub's API always
    /// provides this for assets attached to published releases.
    pub browser_download_url: String,
}

/// Compound resolved filter set ready to pass to `filter_releases`.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedFilters<'a> {
    pub tag: Option<&'a str>,
    pub version: Option<&'a str>,
    pub platform: Option<Platform>,
    pub default_attrs: Option<DefaultAttrs>,
}

impl FilterArgs {
    /// Resolve the flag bundle into concrete filter values. Handles the
    /// `--default-attrs implies --platform` rule and warns if the default-attrs
    /// filter is requested without a resolvable platform.
    #[must_use]
    pub fn resolve(&self) -> ResolvedFilters<'_> {
        let platform = resolve_platform_filter(!self.no_platform);
        let default_attrs = if self.no_default_attrs {
            None
        } else if let Some(p) = platform {
            Some(p.default_attrs())
        } else {
            warn!(
                "--default-attrs requires a known platform; disabling (pass --no-default-attrs to silence)"
            );
            None
        };
        ResolvedFilters {
            tag: self.tag.as_deref(),
            version: self.version.as_deref(),
            platform,
            default_attrs,
        }
    }

    /// Whether any asset-level filter is set (anything other than the default
    /// "show summary across all releases" view).
    #[must_use]
    pub const fn any_asset_filter(&self, resolved: &ResolvedFilters<'_>) -> bool {
        resolved.tag.is_some()
            || resolved.version.is_some()
            || resolved.platform.is_some()
            || resolved.default_attrs.is_some()
    }

    /// Merge defaults from a `FilterConfig` into `self`, field-by-field.
    ///
    /// CLI-set fields are preserved; only unset fields inherit from the
    /// config. This gives the standard CLI-overrides-config precedence so
    /// callers can mix a pinned `.pydl.json` with ad-hoc flag overrides
    /// (e.g. "use the pinned version, but try a different tag").
    ///
    /// - `tag` / `version`: only copied from config when `self`'s field is
    ///   `None`.
    /// - `platform` / `default_attrs`: the `--foo`/`--no-foo` pair is only
    ///   overridden when *neither* form was passed on the CLI. Config
    ///   `platform: false` then sets `no_platform: true`, matching the
    ///   round-trip contract of `From<&FilterArgs> for FilterConfig`.
    #[must_use]
    pub fn with_defaults_from(mut self, cfg: &FilterConfig) -> Self {
        if self.tag.is_none() {
            self.tag.clone_from(&cfg.tag);
        }
        if self.version.is_none() {
            self.version = Some(cfg.version.clone());
        }
        if !self.platform && !self.no_platform && !cfg.platform {
            self.no_platform = true;
        }
        if !self.default_attrs && !self.no_default_attrs && !cfg.default_attrs {
            self.no_default_attrs = true;
        }
        self
    }
}

/// Merge `.pydl.json` defaults into `args` field-by-field.
///
/// The search is git-style: walk the current working directory and every
/// ancestor, returning the first `.pydl.json` found. Corrupt files are a
/// hard error. When a config is found, CLI-set fields on `args` are
/// preserved and only unset fields inherit from the config — see
/// [`FilterArgs::with_defaults_from`].
pub fn apply_config_defaults(args: FilterArgs) -> Result<FilterArgs> {
    let cwd = std::env::current_dir().map_err(|e| anyhow!("reading current directory: {e}"))?;
    let Some(path) = crate::config::find_config(&cwd)? else {
        return Ok(args);
    };
    log::debug!("loading filter defaults from {}", path.display());
    let cfg = crate::config::load_config(&path)?;
    Ok(args.with_defaults_from(&cfg))
}

/// When `filter` has a version pinned but no tag, auto-select the newest
/// release carrying that version under the current platform / default-attrs
/// filters.
///
/// No-ops when `filter.tag` is already set or `filter.version` is `None`
/// (the latter case falls through to the existing multi-match error in
/// `pick_single_asset`, whose candidate listing is already a useful hint).
///
/// Errors when `filter.version` is set but no release carries it under the
/// current filters — the user needs to widen the filter or pass `--tag`.
pub fn auto_select_tag(filter: &mut FilterArgs, releases: &[Release]) -> Result<()> {
    if filter.tag.is_some() || filter.version.is_none() {
        return Ok(());
    }
    let version = filter
        .version
        .clone()
        .expect("version checked is_some above");
    // Resolve the filter set once. Walk releases in their incoming order
    // (GitHub returns them newest-first) and stop on the first release
    // carrying a matching asset.
    let resolved = filter.resolve();
    for release in releases {
        let any_match = release.assets.iter().any(|asset| {
            resolved.version.is_none_or(|v| asset_matches_version(asset, v))
                && resolved
                    .platform
                    .is_none_or(|p| asset_matches_platform(asset, p))
                && resolved
                    .default_attrs
                    .is_none_or(|d| asset_matches_default_attrs(asset, d))
        });
        if any_match {
            log::debug!(
                "auto-selected tag={} for version={version} (no tag specified)",
                release.tag_name,
            );
            filter.tag = Some(release.tag_name.clone());
            return Ok(());
        }
    }
    anyhow::bail!(
        "no release carries version {version:?} under the current \
         platform / default-attrs filters — widen the filter or pass \
         --tag explicitly"
    );
}

fn resolve_platform_filter(enabled: bool) -> Option<Platform> {
    if !enabled {
        return None;
    }
    let platform = Platform::current();
    if platform.is_none() {
        warn!(
            "--platform is enabled but this host ({}/{}) isn't one python-build-standalone publishes for; disabling filter",
            std::env::consts::OS,
            std::env::consts::ARCH,
        );
    }
    platform
}

#[must_use]
pub fn asset_matches_version(asset: &Asset, version: &str) -> bool {
    ParsedAsset::parse(&asset.name).is_ok_and(|p| p.version == version)
}

#[must_use]
pub fn asset_matches_platform(asset: &Asset, platform: Platform) -> bool {
    ParsedAsset::parse(&asset.name).is_ok_and(|p| platform.matches_triple(&p.target_triple))
}

#[must_use]
pub fn asset_matches_default_attrs(asset: &Asset, attrs: DefaultAttrs) -> bool {
    let Ok(parsed) = ParsedAsset::parse(&asset.name) else {
        return false;
    };
    if parsed.flavour != attrs.flavour {
        return false;
    }
    attrs
        .triple_suffix
        .is_none_or(|s| parsed.target_triple.ends_with(s))
}

/// Apply all four filters.
///
/// Returns one entry per matching release along with its surviving assets.
/// Releases that end up with no assets after filtering are dropped. Returns
/// `Err` when `--tag` is set but no release has a matching `tag_name`.
pub fn filter_releases<'a>(
    releases: &'a [Release],
    filters: ResolvedFilters<'_>,
) -> Result<Vec<(&'a Release, Vec<&'a Asset>)>> {
    let candidates: Vec<&Release> = if let Some(tag) = filters.tag {
        vec![
            releases
                .iter()
                .find(|r| r.tag_name == tag)
                .ok_or_else(|| anyhow!("no release found with tag {tag:?}"))?,
        ]
    } else {
        releases.iter().collect()
    };

    let filtered = candidates
        .into_iter()
        .filter_map(|release| {
            let assets: Vec<&Asset> = release
                .assets
                .iter()
                .filter(|a| filters.version.is_none_or(|v| asset_matches_version(a, v)))
                .filter(|a| {
                    filters
                        .platform
                        .is_none_or(|p| asset_matches_platform(a, p))
                })
                .filter(|a| {
                    filters
                        .default_attrs
                        .is_none_or(|d| asset_matches_default_attrs(a, d))
                })
                .collect();
            (!assets.is_empty()).then_some((release, assets))
        })
        .collect();
    Ok(filtered)
}

/// Does an asset name match the given filter predicates?
///
/// Shared by the release-list and embedded-table filter paths. Exposed so
/// `pydl pin` can score embedded asset names without duplicating the
/// platform/default-attrs logic.
#[must_use]
pub fn name_matches_filters(name: &str, filters: ResolvedFilters<'_>) -> bool {
    let Ok(parsed) = ParsedAsset::parse(name) else {
        return false;
    };
    if let Some(v) = filters.version
        && parsed.version != v
    {
        return false;
    }
    if let Some(p) = filters.platform
        && !p.matches_triple(&parsed.target_triple)
    {
        return false;
    }
    if let Some(d) = filters.default_attrs {
        if parsed.flavour != d.flavour {
            return false;
        }
        if !d
            .triple_suffix
            .is_none_or(|s| parsed.target_triple.ends_with(s))
        {
            return false;
        }
    }
    true
}

/// Offline counterpart to [`filter_releases`]. Resolves the CLI filter flags
/// against the embedded checksum table instead of a live release list.
///
/// Returns `(tag, asset_name)` pairs for every embedded entry that matches
/// the filter. Order is tag-descending (newest-first), then asset-name
/// ascending for determinism. Errors only when `--tag` names a tag not
/// present in the embedded set.
pub fn filter_embedded(args: &FilterArgs) -> Result<Vec<(&'static str, &'static str)>> {
    let resolved = args.resolve();
    if let Some(tag) = resolved.tag
        && !crate::checksums::has_tag(tag)
    {
        bail!(
            "tag {tag:?} isn't in the embedded checksum set — run `pydl available` to see \
             what this binary supports, or rebuild with a newer ./checksums/ directory"
        );
    }
    let mut out: Vec<(&'static str, &'static str)> = crate::checksums::iter_embedded_assets()
        .filter(|(tag, _)| resolved.tag.is_none_or(|t| *tag == t))
        .filter(|(_, name)| name_matches_filters(name, resolved))
        .collect();
    // Newest tag first, then asset-name alphabetical within a tag.
    out.sort_by(|a, b| b.0.cmp(a.0).then_with(|| a.1.cmp(b.1)));
    Ok(out)
}

/// Offline counterpart to [`auto_select_tag`]: if only `--version` was
/// passed, walk the embedded table newest-first and set `filter.tag` to
/// the newest tag that carries an asset matching the filter.
///
/// No-ops when `filter.tag` is already set or `filter.version` is `None`.
/// Errors when a version is set but no embedded tag carries it under the
/// current platform / default-attrs filters.
pub fn auto_select_tag_embedded(filter: &mut FilterArgs) -> Result<()> {
    if filter.tag.is_some() || filter.version.is_none() {
        return Ok(());
    }
    let version = filter
        .version
        .clone()
        .expect("version checked is_some above");
    // Resolve the filter once and walk every embedded `(tag, name)` pair in
    // a single pass, tracking the lexicographically largest matching tag.
    // Tags are `YYYYMMDD` date stamps so max == newest.
    let resolved = filter.resolve();
    let newest = crate::checksums::iter_embedded_assets()
        .filter(|(_, name)| name_matches_filters(name, resolved))
        .map(|(tag, _)| tag)
        .max();
    if let Some(tag) = newest {
        log::debug!("auto-selected tag={tag} for version={version} (no tag specified)");
        filter.tag = Some(tag.to_owned());
        return Ok(());
    }
    bail!(
        "no embedded release carries version {version:?} under the current \
         platform / default-attrs filters — widen the filter or pass --tag explicitly"
    );
}

/// Pick the sole `(tag, asset_name)` pair from a filter-resolved hit list.
///
/// Errors with a candidate listing when the filter matches zero or multiple
/// assets in the embedded table. Mirrors
/// `pydl_common::install::pick_single_asset` but operates on the
/// embedded-table `(tag, asset_name)` shape instead of release-list assets.
pub fn pick_single_embedded(
    hits: &[(&'static str, &'static str)],
) -> Result<(&'static str, &'static str)> {
    use std::fmt::Write as _;
    match hits.len() {
        0 => bail!(
            "no embedded assets matched the filter — widen it (try --no-default-attrs or --no-platform)"
        ),
        1 => Ok(hits[0]),
        _ => {
            let mut msg = format!(
                "{} assets matched — narrow the filter. Candidates:\n",
                hits.len()
            );
            for (tag, name) in hits {
                writeln!(msg, "  {tag} / {name}").expect("write to String never fails");
            }
            bail!("{msg}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{Arch, Os};

    const LINUX_X86_64: Platform = Platform {
        os: Os::Linux,
        arch: Arch::X86_64,
    };
    const MACOS_AARCH64: Platform = Platform {
        os: Os::Darwin,
        arch: Arch::Aarch64,
    };
    const WINDOWS_X86_64: Platform = Platform {
        os: Os::Windows,
        arch: Arch::X86_64,
    };

    fn filters(
        tag: Option<&'static str>,
        version: Option<&'static str>,
        platform: Option<Platform>,
        default_attrs: Option<DefaultAttrs>,
    ) -> ResolvedFilters<'static> {
        ResolvedFilters {
            tag,
            version,
            platform,
            default_attrs,
        }
    }

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.to_owned(),
            size: 0,
            browser_download_url: String::new(),
        }
    }

    fn release(tag: &str, asset_names: &[&str]) -> Release {
        Release {
            tag_name: tag.to_owned(),
            name: Some(tag.to_owned()),
            draft: false,
            prerelease: false,
            published_at: None,
            assets: asset_names.iter().map(|n| asset(n)).collect(),
        }
    }

    fn sample_releases() -> Vec<Release> {
        vec![
            release(
                "20260414",
                &[
                    "cpython-3.15.0a8+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz",
                    "cpython-3.14.4+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz",
                    "cpython-3.14.4+20260414-aarch64-apple-darwin-install_only.tar.gz",
                    "SHA256SUMS",
                ],
            ),
            release(
                "20260101",
                &[
                    "cpython-3.14.4+20260101-x86_64-unknown-linux-gnu-install_only.tar.gz",
                    "cpython-3.13.2+20260101-x86_64-unknown-linux-gnu-install_only.tar.gz",
                    "SHA256SUMS",
                ],
            ),
            release(
                "20250601",
                &["cpython-3.12.0+20250601-x86_64-unknown-linux-gnu-install_only.tar.gz"],
            ),
        ]
    }

    fn rich_sample_releases() -> Vec<Release> {
        vec![release(
            "20260414",
            &[
                "cpython-3.14.4+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz",
                "cpython-3.14.4+20260414-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz",
                "cpython-3.14.4+20260414-x86_64-unknown-linux-gnu-pgo+lto-full.tar.zst",
                "cpython-3.14.4+20260414-x86_64-unknown-linux-musl-install_only.tar.gz",
                "cpython-3.14.4+20260414-aarch64-apple-darwin-install_only.tar.gz",
                "cpython-3.14.4+20260414-aarch64-apple-darwin-debug-full.tar.zst",
                "cpython-3.14.4+20260414-x86_64-pc-windows-msvc-install_only.tar.gz",
                "cpython-3.14.4+20260414-x86_64-pc-windows-msvc-pgo-full.tar.zst",
                "cpython-3.14.4+20260414-x86_64-pc-windows-gnu-install_only.tar.gz",
                "SHA256SUMS",
            ],
        )]
    }

    // ----- version -----

    #[test]
    fn asset_matches_version_exact() {
        let a = asset("cpython-3.14.4+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz");
        assert!(asset_matches_version(&a, "3.14.4"));
    }

    #[test]
    fn asset_matches_version_mismatch() {
        let a = asset("cpython-3.14.4+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz");
        assert!(!asset_matches_version(&a, "3.15.0a8"));
    }

    #[test]
    fn asset_matches_version_unparseable_is_false() {
        let a = asset("SHA256SUMS");
        assert!(!asset_matches_version(&a, "3.14.4"));
    }

    #[test]
    fn asset_matches_version_prerelease_tag() {
        let a = asset("cpython-3.15.0a8+20260414-aarch64-apple-darwin-install_only.tar.gz");
        assert!(asset_matches_version(&a, "3.15.0a8"));
        assert!(!asset_matches_version(&a, "3.15.0"));
    }

    // ----- tag / version combinations -----

    #[test]
    fn filter_by_tag_only_returns_all_assets_including_unparseable() {
        let rels = sample_releases();
        let groups = filter_releases(&rels, filters(Some("20260414"), None, None, None)).unwrap();
        assert_eq!(groups.len(), 1);
        let (rel, assets) = &groups[0];
        assert_eq!(rel.tag_name, "20260414");
        assert_eq!(assets.len(), 4);
        assert!(assets.iter().any(|a| a.name == "SHA256SUMS"));
    }

    #[test]
    fn filter_by_missing_tag_errors() {
        let rels = sample_releases();
        let err =
            filter_releases(&rels, filters(Some("nonexistent"), None, None, None)).unwrap_err();
        assert!(
            err.to_string().contains("no release found with tag"),
            "got: {err}"
        );
    }

    #[test]
    fn filter_by_version_only_spans_releases() {
        let rels = sample_releases();
        let groups = filter_releases(&rels, filters(None, Some("3.14.4"), None, None)).unwrap();
        assert_eq!(groups.len(), 2);
        let tags: Vec<&str> = groups.iter().map(|(r, _)| r.tag_name.as_str()).collect();
        assert_eq!(tags, vec!["20260414", "20260101"]);
    }

    #[test]
    fn filter_by_version_only_filters_assets_within_each_release() {
        let rels = sample_releases();
        let groups = filter_releases(&rels, filters(None, Some("3.14.4"), None, None)).unwrap();
        let (_, assets) = groups
            .iter()
            .find(|(r, _)| r.tag_name == "20260414")
            .unwrap();
        assert_eq!(assets.len(), 2);
        for a in assets {
            assert!(a.name.contains("3.14.4"));
        }
    }

    #[test]
    fn filter_by_version_only_drops_releases_with_no_matches() {
        let rels = sample_releases();
        let groups = filter_releases(&rels, filters(None, Some("3.14.4"), None, None)).unwrap();
        assert!(!groups.iter().any(|(r, _)| r.tag_name == "20250601"));
    }

    #[test]
    fn filter_by_version_with_no_matches_returns_empty() {
        let rels = sample_releases();
        let groups = filter_releases(&rels, filters(None, Some("9.9.9"), None, None)).unwrap();
        assert!(groups.is_empty());
    }

    #[test]
    fn filter_by_tag_and_version_intersects() {
        let rels = sample_releases();
        let groups = filter_releases(
            &rels,
            filters(Some("20260414"), Some("3.15.0a8"), None, None),
        )
        .unwrap();
        assert_eq!(groups.len(), 1);
        let (rel, assets) = &groups[0];
        assert_eq!(rel.tag_name, "20260414");
        assert_eq!(assets.len(), 1);
        assert!(assets[0].name.contains("3.15.0a8"));
    }

    #[test]
    fn filter_by_tag_and_nonmatching_version_returns_empty() {
        let rels = sample_releases();
        let groups =
            filter_releases(&rels, filters(Some("20250601"), Some("3.14.4"), None, None)).unwrap();
        assert!(groups.is_empty());
    }

    #[test]
    fn filter_by_tag_and_version_missing_tag_still_errors() {
        let rels = sample_releases();
        let err =
            filter_releases(&rels, filters(Some("nope"), Some("3.14.4"), None, None)).unwrap_err();
        assert!(
            err.to_string().contains("no release found with tag"),
            "got: {err}"
        );
    }

    // ----- platform -----

    #[test]
    fn asset_matches_platform_linux_accepts_gnu() {
        let a = asset("cpython-3.14.4+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz");
        assert!(asset_matches_platform(&a, LINUX_X86_64));
    }

    #[test]
    fn asset_matches_platform_unparseable_is_false() {
        let a = asset("SHA256SUMS");
        assert!(!asset_matches_platform(&a, LINUX_X86_64));
    }

    #[test]
    fn asset_matches_platform_rejects_micro_arch() {
        let a = asset("cpython-3.14.4+20260414-x86_64_v3-unknown-linux-gnu-install_only.tar.gz");
        assert!(!asset_matches_platform(&a, LINUX_X86_64));
    }

    #[test]
    fn asset_matches_platform_rejects_other_os() {
        let a = asset("cpython-3.14.4+20260414-x86_64-apple-darwin-install_only.tar.gz");
        assert!(!asset_matches_platform(&a, LINUX_X86_64));
    }

    #[test]
    fn filter_by_platform_only_linux_x86_64() {
        let rels = sample_releases();
        let groups = filter_releases(&rels, filters(None, None, Some(LINUX_X86_64), None)).unwrap();
        assert_eq!(groups.len(), 3);
        for (_, assets) in &groups {
            for a in assets {
                assert!(a.name.contains("x86_64-unknown-linux"));
            }
        }
    }

    #[test]
    fn filter_by_platform_only_macos_aarch64() {
        let rels = sample_releases();
        let groups =
            filter_releases(&rels, filters(None, None, Some(MACOS_AARCH64), None)).unwrap();
        assert_eq!(groups.len(), 1);
        let (rel, assets) = &groups[0];
        assert_eq!(rel.tag_name, "20260414");
        assert_eq!(assets.len(), 1);
        assert!(assets[0].name.contains("aarch64-apple-darwin"));
    }

    #[test]
    fn filter_by_platform_only_no_matches_returns_empty() {
        let rels = sample_releases();
        let groups =
            filter_releases(&rels, filters(None, None, Some(WINDOWS_X86_64), None)).unwrap();
        assert!(groups.is_empty());
    }

    #[test]
    fn filter_by_platform_excludes_unparseable_assets() {
        let rels = sample_releases();
        let groups = filter_releases(&rels, filters(None, None, Some(LINUX_X86_64), None)).unwrap();
        for (_, assets) in &groups {
            for a in assets {
                assert_ne!(a.name, "SHA256SUMS");
            }
        }
    }

    #[test]
    fn filter_by_tag_and_platform_intersects() {
        let rels = sample_releases();
        let groups = filter_releases(
            &rels,
            filters(Some("20260414"), None, Some(LINUX_X86_64), None),
        )
        .unwrap();
        assert_eq!(groups.len(), 1);
        let (_, assets) = &groups[0];
        assert_eq!(assets.len(), 2);
        for a in assets {
            assert!(a.name.contains("x86_64-unknown-linux"));
        }
    }

    #[test]
    fn filter_by_version_and_platform_intersects() {
        let rels = sample_releases();
        let groups = filter_releases(
            &rels,
            filters(None, Some("3.14.4"), Some(LINUX_X86_64), None),
        )
        .unwrap();
        assert_eq!(groups.len(), 2);
        for (_, assets) in &groups {
            assert_eq!(assets.len(), 1);
            assert!(assets[0].name.contains("3.14.4"));
            assert!(assets[0].name.contains("x86_64-unknown-linux"));
        }
    }

    #[test]
    fn filter_by_tag_version_and_platform_intersects() {
        let rels = sample_releases();
        let groups = filter_releases(
            &rels,
            filters(Some("20260414"), Some("3.14.4"), Some(LINUX_X86_64), None),
        )
        .unwrap();
        assert_eq!(groups.len(), 1);
        let (_, assets) = &groups[0];
        assert_eq!(assets.len(), 1);
        assert_eq!(
            assets[0].name,
            "cpython-3.14.4+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz"
        );
    }

    #[test]
    fn filter_by_tag_and_platform_missing_tag_still_errors() {
        let rels = sample_releases();
        let err = filter_releases(&rels, filters(Some("nope"), None, Some(LINUX_X86_64), None))
            .unwrap_err();
        assert!(
            err.to_string().contains("no release found with tag"),
            "got: {err}"
        );
    }

    // ----- default-attrs -----

    #[test]
    fn asset_matches_default_attrs_linux_picks_glibc_install_only() {
        let defaults = LINUX_X86_64.default_attrs();
        let win = asset("cpython-3.14.4+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz");
        assert!(asset_matches_default_attrs(&win, defaults));
    }

    #[test]
    fn asset_matches_default_attrs_linux_rejects_musl() {
        let defaults = LINUX_X86_64.default_attrs();
        let musl = asset("cpython-3.14.4+20260414-x86_64-unknown-linux-musl-install_only.tar.gz");
        assert!(!asset_matches_default_attrs(&musl, defaults));
    }

    #[test]
    fn asset_matches_default_attrs_linux_rejects_non_install_only() {
        let defaults = LINUX_X86_64.default_attrs();
        let pgo = asset("cpython-3.14.4+20260414-x86_64-unknown-linux-gnu-pgo+lto-full.tar.zst");
        assert!(!asset_matches_default_attrs(&pgo, defaults));
    }

    #[test]
    fn asset_matches_default_attrs_linux_rejects_stripped() {
        let defaults = LINUX_X86_64.default_attrs();
        let stripped =
            asset("cpython-3.14.4+20260414-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz");
        assert!(!asset_matches_default_attrs(&stripped, defaults));
    }

    #[test]
    fn asset_matches_default_attrs_macos_picks_install_only() {
        let defaults = MACOS_AARCH64.default_attrs();
        let win = asset("cpython-3.14.4+20260414-aarch64-apple-darwin-install_only.tar.gz");
        assert!(asset_matches_default_attrs(&win, defaults));
    }

    #[test]
    fn asset_matches_default_attrs_macos_rejects_debug() {
        let defaults = MACOS_AARCH64.default_attrs();
        let debug = asset("cpython-3.14.4+20260414-aarch64-apple-darwin-debug-full.tar.zst");
        assert!(!asset_matches_default_attrs(&debug, defaults));
    }

    #[test]
    fn asset_matches_default_attrs_windows_picks_msvc() {
        let defaults = WINDOWS_X86_64.default_attrs();
        let msvc = asset("cpython-3.14.4+20260414-x86_64-pc-windows-msvc-install_only.tar.gz");
        assert!(asset_matches_default_attrs(&msvc, defaults));
    }

    #[test]
    fn asset_matches_default_attrs_windows_rejects_gnu() {
        let defaults = WINDOWS_X86_64.default_attrs();
        let gnu = asset("cpython-3.14.4+20260414-x86_64-pc-windows-gnu-install_only.tar.gz");
        assert!(!asset_matches_default_attrs(&gnu, defaults));
    }

    #[test]
    fn asset_matches_default_attrs_unparseable_is_false() {
        let defaults = LINUX_X86_64.default_attrs();
        let sums = asset("SHA256SUMS");
        assert!(!asset_matches_default_attrs(&sums, defaults));
    }

    #[test]
    fn filter_by_default_attrs_linux_picks_single_asset() {
        let rels = rich_sample_releases();
        let groups = filter_releases(
            &rels,
            filters(
                None,
                None,
                Some(LINUX_X86_64),
                Some(LINUX_X86_64.default_attrs()),
            ),
        )
        .unwrap();
        assert_eq!(groups.len(), 1);
        let (_, assets) = &groups[0];
        assert_eq!(assets.len(), 1);
        assert_eq!(
            assets[0].name,
            "cpython-3.14.4+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz"
        );
    }

    #[test]
    fn filter_by_default_attrs_macos_picks_single_asset() {
        let rels = rich_sample_releases();
        let groups = filter_releases(
            &rels,
            filters(
                None,
                None,
                Some(MACOS_AARCH64),
                Some(MACOS_AARCH64.default_attrs()),
            ),
        )
        .unwrap();
        assert_eq!(groups.len(), 1);
        let (_, assets) = &groups[0];
        assert_eq!(assets.len(), 1);
        assert_eq!(
            assets[0].name,
            "cpython-3.14.4+20260414-aarch64-apple-darwin-install_only.tar.gz"
        );
    }

    #[test]
    fn filter_by_default_attrs_windows_picks_msvc() {
        let rels = rich_sample_releases();
        let groups = filter_releases(
            &rels,
            filters(
                None,
                None,
                Some(WINDOWS_X86_64),
                Some(WINDOWS_X86_64.default_attrs()),
            ),
        )
        .unwrap();
        assert_eq!(groups.len(), 1);
        let (_, assets) = &groups[0];
        assert_eq!(assets.len(), 1);
        assert_eq!(
            assets[0].name,
            "cpython-3.14.4+20260414-x86_64-pc-windows-msvc-install_only.tar.gz"
        );
    }

    #[test]
    fn filter_by_default_attrs_without_platform_is_allowed_but_useless() {
        let rels = rich_sample_releases();
        let groups = filter_releases(
            &rels,
            filters(None, None, None, Some(LINUX_X86_64.default_attrs())),
        )
        .unwrap();
        assert_eq!(groups.len(), 1);
        let (_, assets) = &groups[0];
        assert_eq!(assets.len(), 1);
        assert_eq!(
            assets[0].name,
            "cpython-3.14.4+20260414-x86_64-unknown-linux-gnu-install_only.tar.gz"
        );
    }

    // ----- FilterConfig (de)serialization -----

    #[test]
    fn filter_config_default_boolean_defaults() {
        // `FilterConfig::default()` is a tests-only baseline; its `version`
        // is empty because real configs always resolve a concrete version
        // before being written. What we check here is that the two boolean
        // filters default to their CLI-on values.
        let cfg = FilterConfig::default();
        assert!(cfg.tag.is_none());
        assert_eq!(cfg.version, "");
        assert!(cfg.platform);
        assert!(cfg.default_attrs);
    }

    #[test]
    fn filter_config_from_default_filter_args() {
        // `FilterArgs::default()` — all bools `false`, both options `None` —
        // represents the CLI-invoked-with-no-flags state, which maps to the
        // feature-on defaults.
        let args = FilterArgs {
            tag: None,
            version: None,
            platform: false,
            no_platform: false,
            default_attrs: false,
            no_default_attrs: false,
        };
        let cfg = FilterConfig::from(&args);
        assert_eq!(cfg, FilterConfig::default());
    }

    #[test]
    fn filter_config_from_filter_args_honors_no_flags() {
        let args = FilterArgs {
            tag: Some("20260414".to_owned()),
            version: Some("3.14.4".to_owned()),
            platform: false,
            no_platform: true,
            default_attrs: false,
            no_default_attrs: true,
        };
        let cfg = FilterConfig::from(&args);
        assert_eq!(cfg.tag.as_deref(), Some("20260414"));
        assert_eq!(cfg.version, "3.14.4");
        assert!(!cfg.platform);
        assert!(!cfg.default_attrs);
    }

    #[test]
    fn filter_config_roundtrips_through_json() {
        let original = FilterConfig {
            tag: Some("20260414".to_owned()),
            version: "3.14.4".to_owned(),
            platform: false,
            default_attrs: true,
        };
        let json = serde_json::to_string_pretty(&original).unwrap();
        let restored: FilterConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn filter_config_minimal_json_roundtrips() {
        // Minimal valid config: just the mandatory `version`. Tag is absent;
        // booleans take their `default = "default_true"` values.
        let original = FilterConfig {
            tag: None,
            version: "3.14.4".to_owned(),
            platform: true,
            default_attrs: true,
        };
        let json = serde_json::to_string_pretty(&original).unwrap();
        let restored: FilterConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn filter_config_missing_version_is_deserialize_error() {
        // `version` is mandatory in the JSON schema; missing it should fail
        // deserialization, not silently produce an empty string.
        let err = serde_json::from_str::<FilterConfig>(r#"{"tag": "20260414"}"#).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("version"), "expected version error: {msg}");
    }

    #[test]
    fn filter_config_bare_version_json_roundtrips() {
        // Minimal viable JSON: only `"version"`. Everything else defaults.
        let restored: FilterConfig = serde_json::from_str(r#"{"version": "3.14.4"}"#).unwrap();
        assert_eq!(restored.tag, None);
        assert_eq!(restored.version, "3.14.4");
        assert!(restored.platform);
        assert!(restored.default_attrs);
    }

    #[test]
    fn filter_config_elides_defaults_on_serialize() {
        // `tag` is elided when unset (it's `Option`). `version` is mandatory.
        // The two booleans are elided when they hold their default (`true`)
        // so the on-disc form stays minimal — only deviations from the
        // default land in the file.
        let cfg = FilterConfig {
            tag: None,
            version: "3.14.4".to_owned(),
            platform: true,
            default_attrs: true,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("tag"), "tag should be elided: {json}");
        assert!(json.contains("version"), "version must be present: {json}");
        assert!(
            !json.contains("platform"),
            "default platform=true should be elided: {json}"
        );
        assert!(
            !json.contains("default_attrs"),
            "default default_attrs=true should be elided: {json}"
        );
    }

    #[test]
    fn filter_config_writes_explicit_false_booleans() {
        // `false` deviates from the default and is always written, so
        // `--no-platform` / `--no-default-attrs` survive a pin → load cycle.
        let cfg = FilterConfig {
            tag: None,
            version: "3.14.4".to_owned(),
            platform: false,
            default_attrs: false,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"platform\":false"), "got: {json}");
        assert!(json.contains("\"default_attrs\":false"), "got: {json}");
        let restored: FilterConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, cfg);
    }

    fn bare_args() -> FilterArgs {
        FilterArgs {
            tag: None,
            version: None,
            platform: false,
            no_platform: false,
            default_attrs: false,
            no_default_attrs: false,
        }
    }

    // ----- with_defaults_from -----

    #[test]
    fn with_defaults_from_fills_in_unset_tag_and_version() {
        let args = bare_args();
        let cfg = FilterConfig {
            tag: Some("20260414".to_owned()),
            version: "3.14.4".to_owned(),
            platform: true,
            default_attrs: true,
        };
        let out = args.with_defaults_from(&cfg);
        assert_eq!(out.tag.as_deref(), Some("20260414"));
        assert_eq!(out.version.as_deref(), Some("3.14.4"));
        // platform/default_attrs on means "default", so no inverse flag set.
        assert!(!out.no_platform);
        assert!(!out.no_default_attrs);
    }

    #[test]
    fn with_defaults_from_translates_platform_false() {
        // config { platform: false } should set `no_platform: true` so
        // that FilterConfig::from(&out).platform round-trips to false.
        let args = bare_args();
        let cfg = FilterConfig {
            tag: None,
            version: "3.14.4".to_owned(),
            platform: false,
            default_attrs: true,
        };
        let out = args.with_defaults_from(&cfg);
        assert!(
            out.no_platform,
            "no_platform should be set when config.platform is false"
        );
        let round_tripped = FilterConfig::from(&out);
        assert!(!round_tripped.platform);
    }

    #[test]
    fn with_defaults_from_translates_default_attrs_false() {
        let args = bare_args();
        let cfg = FilterConfig {
            tag: None,
            version: "3.14.4".to_owned(),
            platform: true,
            default_attrs: false,
        };
        let out = args.with_defaults_from(&cfg);
        assert!(out.no_default_attrs);
        let round_tripped = FilterConfig::from(&out);
        assert!(!round_tripped.default_attrs);
    }

    #[test]
    fn with_defaults_from_all_defaults_is_noop() {
        let args = bare_args();
        let out = args.with_defaults_from(&FilterConfig::default());
        // Nothing set: the output is still the bare default state.
        assert_eq!(FilterConfig::from(&out), FilterConfig::default());
    }

    #[test]
    fn with_defaults_from_preserves_cli_set_tag() {
        // CLI passed `-t 20260414`; config has a different tag. CLI wins.
        let mut args = bare_args();
        args.tag = Some("20260414".to_owned());
        let cfg = FilterConfig {
            tag: Some("99991231".to_owned()),
            version: "3.14.4".to_owned(),
            platform: true,
            default_attrs: true,
        };
        let out = args.with_defaults_from(&cfg);
        assert_eq!(out.tag.as_deref(), Some("20260414"));
        // version was unset on the CLI, so inherited from config.
        assert_eq!(out.version.as_deref(), Some("3.14.4"));
    }

    #[test]
    fn with_defaults_from_preserves_cli_set_version() {
        // CLI passed `-v 3.15.0a8`; config pins 3.14.4. CLI wins.
        let mut args = bare_args();
        args.version = Some("3.15.0a8".to_owned());
        let cfg = FilterConfig {
            tag: Some("20260414".to_owned()),
            version: "3.14.4".to_owned(),
            platform: true,
            default_attrs: true,
        };
        let out = args.with_defaults_from(&cfg);
        assert_eq!(out.version.as_deref(), Some("3.15.0a8"));
        assert_eq!(out.tag.as_deref(), Some("20260414"));
    }

    #[test]
    fn with_defaults_from_preserves_cli_no_platform() {
        // CLI passed `--no-platform`; config has `platform: true`. CLI wins.
        let mut args = bare_args();
        args.no_platform = true;
        let cfg = FilterConfig {
            tag: None,
            version: "3.14.4".to_owned(),
            platform: true,
            default_attrs: true,
        };
        let out = args.with_defaults_from(&cfg);
        assert!(out.no_platform);
    }

    #[test]
    fn with_defaults_from_preserves_cli_platform_over_config_false() {
        // CLI passed `--platform` explicitly (re-asserting default-on);
        // config has `platform: false`. CLI wins — no_platform stays off.
        let mut args = bare_args();
        args.platform = true;
        let cfg = FilterConfig {
            tag: None,
            version: "3.14.4".to_owned(),
            platform: false,
            default_attrs: true,
        };
        let out = args.with_defaults_from(&cfg);
        assert!(!out.no_platform);
    }

    #[test]
    fn with_defaults_from_preserves_cli_no_default_attrs() {
        let mut args = bare_args();
        args.no_default_attrs = true;
        let cfg = FilterConfig {
            tag: None,
            version: "3.14.4".to_owned(),
            platform: true,
            default_attrs: true,
        };
        let out = args.with_defaults_from(&cfg);
        assert!(out.no_default_attrs);
    }

    // ----- auto_select_tag -----

    #[test]
    fn auto_select_tag_noop_when_tag_already_set() {
        let rels = sample_releases();
        let mut args = bare_args();
        args.tag = Some("20250601".to_owned());
        args.version = Some("3.14.4".to_owned());
        // --no-platform to avoid filtering by host platform in unit tests.
        args.no_platform = true;
        args.no_default_attrs = true;
        auto_select_tag(&mut args, &rels).unwrap();
        assert_eq!(args.tag.as_deref(), Some("20250601"));
    }

    #[test]
    fn auto_select_tag_noop_when_version_absent() {
        let rels = sample_releases();
        let mut args = bare_args();
        args.no_platform = true;
        args.no_default_attrs = true;
        auto_select_tag(&mut args, &rels).unwrap();
        assert!(args.tag.is_none());
    }

    #[test]
    fn auto_select_tag_picks_newest_release_carrying_version() {
        // `3.14.4` is in 20260414 (first, newest) and 20260101.
        let rels = sample_releases();
        let mut args = bare_args();
        args.version = Some("3.14.4".to_owned());
        args.no_platform = true;
        args.no_default_attrs = true;
        auto_select_tag(&mut args, &rels).unwrap();
        assert_eq!(args.tag.as_deref(), Some("20260414"));
    }

    #[test]
    fn auto_select_tag_skips_releases_without_the_version() {
        // `3.12.0` only appears in 20250601 (the oldest release). The two
        // newer releases don't carry it, so the walk must skip past them.
        let rels = sample_releases();
        let mut args = bare_args();
        args.version = Some("3.12.0".to_owned());
        args.no_platform = true;
        args.no_default_attrs = true;
        auto_select_tag(&mut args, &rels).unwrap();
        assert_eq!(args.tag.as_deref(), Some("20250601"));
    }

    #[test]
    fn auto_select_tag_errors_when_no_release_carries_version() {
        let rels = sample_releases();
        let mut args = bare_args();
        args.version = Some("9.9.9".to_owned());
        args.no_platform = true;
        args.no_default_attrs = true;
        let err = auto_select_tag(&mut args, &rels).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("9.9.9"), "expected version in error: {msg}");
        assert!(msg.contains("--tag"), "expected --tag hint in error: {msg}");
    }

    #[test]
    fn auto_select_tag_uses_only_release_when_version_is_unique() {
        // `3.13.2` only appears in 20260101 in the sample set. When the
        // walk skips releases that don't carry the version, it should find
        // and stop at the only candidate.
        let rels = sample_releases();
        let mut args = bare_args();
        args.version = Some("3.13.2".to_owned());
        args.no_platform = true;
        args.no_default_attrs = true;
        auto_select_tag(&mut args, &rels).unwrap();
        assert_eq!(args.tag.as_deref(), Some("20260101"));
    }
}
