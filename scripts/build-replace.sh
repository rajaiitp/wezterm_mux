#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
INSTALL_DIR=${WEZTERM_BIN_DIR:-"$HOME/.local/bin"}
BINARIES=(wezterm wezterm-gui wezterm-mux-server)

tmp_file=""
cleanup() {
    if [[ -n "$tmp_file" ]]; then
        rm -f -- "$tmp_file"
    fi
}
trap cleanup EXIT

usage() {
    cat <<EOF
Usage: $(basename "$0")

Build the release CLI, GUI, and mux server, then atomically replace the
matching binaries in WEZTERM_BIN_DIR (default: $HOME/.local/bin).

Environment:
  WEZTERM_BIN_DIR   Destination directory for the replaced binaries.
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    usage
    exit 0
fi
if [[ $# -ne 0 ]]; then
    usage >&2
    exit 2
fi

cd -- "$ROOT"
printf 'Building release binaries in %s...\n' "$ROOT"
cargo build --release \
    -p wezterm \
    -p wezterm-gui \
    -p wezterm-mux-server

mkdir -p -- "$INSTALL_DIR"
for binary in "${BINARIES[@]}"; do
    source="$ROOT/target/release/$binary"
    destination="$INSTALL_DIR/$binary"
    [[ -x "$source" ]] || { printf 'missing build output: %s\n' "$source" >&2; exit 1; }

    tmp_file=$(mktemp "$INSTALL_DIR/.${binary}.new.XXXXXX")
    # `-p` preserves mode and timestamps on both BSD/macOS and GNU cp.
    cp -p "$source" "$tmp_file"
    chmod 755 "$tmp_file"
    mv -f -- "$tmp_file" "$destination"
    tmp_file=""
    printf 'Replaced %s\n' "$destination"
done

cat <<'EOF'
Build and replacement complete.
Existing WezTerm GUI/mux processes keep their old mapped binaries; restart them
when you want the new build to take effect.
EOF
