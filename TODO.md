# To Do

## Network Usage

* Commands that hit the network and can result in cache misses (i.e. "pydl available" and "pydl download") should either report progress or report activity so that users know that the command has not stuck.

## Self-Update

* Once two consecutive releases have shipped a `SHA256SUMS` manifest, flip the default of `pydl self-update --require-checksum` from off to on (see the rollout note in `pydl/src/cmd/self_update.rs`).
