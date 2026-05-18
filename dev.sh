#!/usr/bin/env bash
# Developer wrapper for common workspace tasks. Run `./dev.sh help` to see the
# list of subcommands. See DEV.md for the workflow details.

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: ./dev.sh <command> [args...]

Commands:
  build                    cargo build --workspace --all-targets
  test [args...]           cargo test --workspace --all-targets [args...]
  lint                     cargo clippy --workspace --all-targets -- -D warnings
  fmt                      cargo +nightly fmt --all
  fmt-check                cargo +nightly fmt --all -- --check
  check                    fmt-check + lint + test (what CI should run)
  pydl [args...]             cargo run -p pydl --quiet -- [args...]
  get-checksums [args...]    cargo run -p get-checksums -- [args...]
  check-checksums [args...]  cargo run -p check-checksums -- [args...]
  crap [args...]           generate coverage + run cargo-crap [args...]
  install-pydl               build pydl in release mode and copy it to ~/.local/bin
  clean                      cargo clean
  help                       show this help

Environment:
  CARGO     override the cargo binary (default: cargo). Does not affect fmt /
            fmt-check / check, which invoke `cargo +nightly fmt` for grouped
            import ordering (see rustfmt.toml).
EOF
}

CARGO="${CARGO:-cargo}"

cmd="${1:-help}"
shift || true

case "$cmd" in
    build)
        "$CARGO" build --workspace --all-targets "$@"
        ;;
    test)
        "$CARGO" test --workspace --all-targets "$@"
        ;;
    lint)
        "$CARGO" clippy --workspace --all-targets -- -D warnings
        ;;
    fmt)
        cargo +nightly fmt --all
        ;;
    fmt-check)
        cargo +nightly fmt --all -- --check
        ;;
    check)
        cargo +nightly fmt --all -- --check
        "$CARGO" clippy --workspace --all-targets -- -D warnings
        "$CARGO" test --workspace --all-targets
        ;;
    pydl)
        "$CARGO" run -p pydl --quiet -- "$@"
        ;;
    get-checksums)
        "$CARGO" run -p get-checksums -- "$@"
        ;;
    check-checksums)
        "$CARGO" run -p check-checksums -- "$@"
        ;;
    crap)
        cargo llvm-cov --workspace --lcov --output-path lcov.info
        cargo crap --workspace --lcov lcov.info "$@"
        ;;
    install-pydl)
        "$CARGO" build -p pydl --release
        dest_dir="${HOME}/.local/bin"
        mkdir -p "$dest_dir"
        install -m 0755 target/release/pydl "$dest_dir/pydl"
        echo "installed pydl -> $dest_dir/pydl"
        case ":${PATH:-}:" in
            *":$dest_dir:"*) ;;
            *) echo "note: $dest_dir is not on your PATH" >&2 ;;
        esac
        ;;
    clean)
        "$CARGO" clean
        ;;
    help|-h|--help)
        usage
        ;;
    *)
        echo "error: unknown command '$cmd'" >&2
        usage
        exit 2
        ;;
esac
