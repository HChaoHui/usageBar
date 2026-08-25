#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEB_DIR="$ROOT_DIR/src-tauri/target/release/bundle/deb"
DRY_RUN=false

usage() {
  printf 'Usage: %s [--dry-run]\n' "$(basename "$0")"
  printf '  --dry-run  Check the package without installing it\n'
}

case "${1:-}" in
  "") ;;
  --dry-run) DRY_RUN=true ;;
  -h|--help)
    usage
    exit 0
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac

if ! command -v dpkg >/dev/null 2>&1 || ! command -v apt >/dev/null 2>&1; then
  printf 'Error: this installer requires an Ubuntu/Debian system.\n' >&2
  exit 1
fi

case "$(dpkg --print-architecture)" in
  amd64) PACKAGE_ARCH="amd64" ;;
  arm64) PACKAGE_ARCH="arm64" ;;
  *)
    printf 'Error: unsupported architecture: %s\n' "$(dpkg --print-architecture)" >&2
    exit 1
    ;;
esac

shopt -s nullglob
packages=("$DEB_DIR"/usageBar_*_"$PACKAGE_ARCH".deb)
shopt -u nullglob

if (( ${#packages[@]} == 0 )); then
  printf 'Error: no %s package found in %s\n' "$PACKAGE_ARCH" "$DEB_DIR" >&2
  printf 'Build it first with:\n  cd "%s/src-tauri" && cargo tauri build --bundles deb --ci\n' "$ROOT_DIR" >&2
  exit 1
fi

package_path="${packages[0]}"
for candidate in "${packages[@]:1}"; do
  if [[ "$candidate" -nt "$package_path" ]]; then
    package_path="$candidate"
  fi
done

package_name="$(dpkg-deb --field "$package_path" Package)"
package_version="$(dpkg-deb --field "$package_path" Version)"
package_arch="$(dpkg-deb --field "$package_path" Architecture)"

if [[ "$package_name" != "usage-bar" || "$package_arch" != "$PACKAGE_ARCH" ]]; then
  printf 'Error: unexpected package metadata in %s\n' "$package_path" >&2
  exit 1
fi

printf 'Package: %s %s (%s)\n' "$package_name" "$package_version" "$package_arch"
printf 'File: %s\n' "$package_path"

if [[ "$DRY_RUN" == true ]]; then
  printf 'Dry run complete; no system changes were made.\n'
  exit 0
fi

if (( EUID == 0 )); then
  apt install -y "$package_path"
else
  if ! command -v sudo >/dev/null 2>&1; then
    printf 'Error: sudo is required to install the package.\n' >&2
    exit 1
  fi
  sudo apt install -y "$package_path"
fi

printf '\nusageBar installed successfully. Launch it from the application menu or run: usagebar\n'
