#!/bin/sh
# pydl installer — fetches the latest release archive for the host platform
# and unpacks the `pydl` binary into $PYDL_INSTALL_DIR (default: ~/.local/bin).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/rcook/pydl/main/install.sh | sh
#   PYDL_INSTALL_DIR=/opt/bin sh install.sh
#
# Supported hosts (matching what .github/workflows/release.yaml publishes):
#   - macOS arm64                  → aarch64-apple-darwin
#   - Linux x86_64 (incl. WSL)     → x86_64-unknown-linux-musl
# Anything else exits 1 with a pointer to the Releases page.

set -eu

REPO="rcook/pydl"
RELEASES_URL="https://github.com/${REPO}/releases"
LATEST_API="https://api.github.com/repos/${REPO}/releases/latest"
INSTALL_DIR="${PYDL_INSTALL_DIR:-$HOME/.local/bin}"

err() {
    printf 'pydl-install: %s\n' "$*" >&2
}

require() {
    if ! command -v "$1" >/dev/null 2>&1; then
        err "required tool not found: $1"
        exit 1
    fi
}

require curl
require tar
require uname
require mktemp

os="$(uname -s)"
arch="$(uname -m)"

case "${os}/${arch}" in
    Darwin/arm64)
        target="aarch64-apple-darwin"
        ;;
    Linux/x86_64)
        target="x86_64-unknown-linux-musl"
        ;;
    *)
        err "unsupported host: ${os}/${arch}"
        err "see ${RELEASES_URL} for available archives"
        exit 1
        ;;
esac

# GitHub's latest-release API returns JSON with one `browser_download_url`
# field per asset. Pick the one whose URL contains the target triple.
asset_url="$(
    curl -fsSL "${LATEST_API}" \
        | grep -oE '"browser_download_url":[[:space:]]*"[^"]*"' \
        | cut -d'"' -f4 \
        | grep -F "${target}" \
        | head -n1
)"

if [ -z "${asset_url}" ]; then
    err "could not find a release asset matching ${target}"
    err "see ${RELEASES_URL}"
    exit 1
fi

tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT INT TERM

archive="${tmp}/pydl.tar.gz"
printf 'pydl-install: downloading %s\n' "${asset_url}"
curl -fsSL "${asset_url}" -o "${archive}"

mkdir -p "${INSTALL_DIR}"
tar -xzf "${archive}" -C "${INSTALL_DIR}"
chmod +x "${INSTALL_DIR}/pydl"

printf 'pydl-install: installed %s\n' "${INSTALL_DIR}/pydl"

# PATH hint, matching the shape of `dev.sh install-pydl`.
case ":${PATH:-}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        printf 'pydl-install: %s is not on your PATH; add it with:\n' "${INSTALL_DIR}"
        printf '  export PATH="%s:$PATH"\n' "${INSTALL_DIR}"
        ;;
esac
