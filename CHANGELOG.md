# Changelog

All notable changes to this project are documented here.
Generated from git commits by [git-cliff](https://git-cliff.org).

## [0.1.19] - 2026-05-18


### Bug Fixes
- Fix formatting (4e420e7)


### Features
- Add more test coverage (25efdfd)


### Internal
- Analyse complexity (e89fd40)
- Enforce CRAP threshold (80e7137)
- Exclude functions (073467c)

## [0.1.18] - 2026-05-18


### Bug Fixes
- Fix #12: Clean up primary output and logging for various commands (bb0ea46)

## [0.1.17] - 2026-05-17


### Tooling
- Document --full flag, add incremental-fetch tests, fix clippy lints (8a5a6c0)

## [0.1.16] - 2026-05-17


### Internal
- Upgrade reqwest (026e3c8)
- Implement incremental refresh of releases (a9d0c4e)

## [0.1.15] - 2026-05-17


### Bug Fixes
- Fix #8: Remove UNC prefix for Windows local paths (ccca7b7)
- Fix #10: Add -C to set working directory (8fe2338)
- Fix #11: Add "pydl status" (3c29b02)


### Tooling
- Update documentation (525b156)

## [0.1.14] - 2026-05-15


### Internal
- Upgrade to latest dependencies (266a9ff)

## [0.1.13] - 2026-05-15


### Bug Fixes
- Fix #5, Fix #7: Improve output of pydl download (499659c)
- Fix #4: pydl pin writes primary output to stdout (0e85936)
- Fix #6: pydl available now only displays newest tag by default (f19a949)


### Internal
- Show absolute paths in pin and download output (f89c118)

## [0.1.12] - 2026-05-13


### Internal
- Clean up CHANGELOG.md (b886a2a)
- Prettify output of 'pydl available' (61fba5e)

## [0.1.11] - 2026-05-12


### Internal
- Require checksum when updating pydl (d5e1550)
- Incorporate latest SHA-256 checksums (6c7a3c1)

## [0.1.10] - 2026-05-12


### Tooling
- Update "pydl available" to report both Python and pydl releases (b6c98fd)

## [0.1.9] - 2026-05-12


### Tooling
- Update descriptions (82bae54)

## [0.1.8] - 2026-05-12


### Internal
- Set PER_PAGE=10 (9cae885)


### Tooling
- Update documentation (3d248b0)

## [0.1.7] - 2026-05-12


### Features
- Add "update" command and change semantics of other subcommands (7c99d31)


### Internal
- Use saturating_add (7f47014)
- Tweak backoff strategy (a289c77)
- Remove references to PBD from public-facing error messages etc. (fc6600e)

## [0.1.6] - 2026-05-12


### Bug Fixes
- Fix #2: 504 Gateway Timeout when running "pydl available" (20f9eb8)
- Fix #1: Flatten Windows release archive (9e1503b)
- Fix #3: Upgrade CI dependencies (0e69013)


### Features
- Add LICENSE (be77090)

## [0.1.5] - 2026-05-11


### Internal
- Respect RUST_LOG_STYLE (cf39984)
- Set explicit symlink policy (f518a3f)
- Self-update to temp file (a3e1f9a)
- Evict cached body on SHA-256 mismatch (0f9157f)
- Verify SHA-256 checksums in self-update (d871d23)
- Set PER_PAGE=100 (a5fee93)
- Use async method (18a7c6c)


### Tooling
- Update docs (757674b)

## [0.1.4] - 2026-05-11


### Bug Fixes
- Fix comment (f9e0d74)


### Internal
- Show path to cache artifacts (5b56f56)
- Report estimated wait time for rate limit to reset (5aa3000)
- Remove dead code (b871ecf)
- Derive version number in User-Agent header from CARGO_PKG_VERSION (f75dca0)
- Optimize auto_select_tag and auto_select_tag_embedded (87b4b1f)
- More to-dos (4370672)
- Create newest_embedded_tag and related refactors (1c83ce6)
- Use u64 for Version (4aff68f)
- Refactor parse_cache_control (92edb39)
- Simplify download_to_tmp (c15a2a1)
- Clarify why resolved tag is discarded (2516ff8)
- Installation instructions (aeb548c)
- Report more version information (887bef0)

## [0.1.2] - 2026-05-11


### Internal
- Implement self-update (263cf51)

## [0.1.1] - 2026-05-11


### Internal
- Use rustls-tls (be50e34)

## [0.1.0] - 2026-05-11


### Internal
- Initial commit (0e48fbe)


