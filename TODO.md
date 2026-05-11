# To Do

## Documentation

* Installation using `curl` or `Invoke-WebRequest` or equivalent

## Version Reporting

* `pydl --version` should report if the build is a debug or release build and whether it was installed from GitHub or built locally

## Self-Update

* Publish a SHA-256 checksum file alongside each release and have `pydl self-update` verify the downloaded archive against it (today the command trusts HTTPS only — see the `TODO` in `pydl/src/cmd/self_update.rs`).

## Network Usage

* Commands that hit the network and can result in cache misses (i.e. "pydl available" and "pydl download") should either report progress or report activity so that users know that the command has not stuck.
