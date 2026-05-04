# Recurring Maintenance Tasks

* Use Oxford spelling but _not_ Oxford comma; use "disc" spelling over "disk"
* Ensure README.md and DEV.md are up to date
* Ensure README.md is user-focused and DEV.md is developer-focused
* Suggest refinements to README.md and DEV.md
* When cutting a release: bump `[workspace.package].version` in `Cargo.toml`, run `cargo update --workspace`, commit, then push tag `v<X.Y.Z>` — the release and changelog workflows fire on the tag
