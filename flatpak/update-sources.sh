#!/usr/bin/env bash
# Regenerates flatpak/cargo-sources.json (every crate of Cargo.lock as a
# source of the Flatpak build, which runs offline) from the Cargo.lock of a
# tag, by default the one in flatpak/com.termoak.Termoak.yml:
#
#   flatpak/update-sources.sh [desktop-vX.Y.Z]
#
# Uses flatpak-cargo-generator (flatpak-builder-tools) if it is installed,
# otherwise the termoak-flatpak image of TermoakSSH/packages
# (docker/flatpak.Dockerfile), which has it.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
manifest="$root/flatpak/com.termoak.Termoak.yml"
tag="${1:-$(sed -n 's/^ *tag: *\(desktop-v[^ ]*\)$/\1/p' "$manifest" | head -1)}"
[ -n "$tag" ] || { echo "no tag given and none in $manifest" >&2; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
git -C "$root" fetch -q --tags origin 2>/dev/null || true
git -C "$root" show "$tag:Cargo.lock" >"$work/Cargo.lock"

if command -v flatpak-cargo-generator >/dev/null; then
  flatpak-cargo-generator "$work/Cargo.lock" -o "$work/cargo-sources.json"
else
  docker image inspect termoak-flatpak >/dev/null 2>&1 || {
    echo "flatpak-cargo-generator is missing: install flatpak-builder-tools or build the termoak-flatpak image (TermoakSSH/packages)" >&2
    exit 1
  }
  docker run --rm -v "$work:/work" -w /work termoak-flatpak \
    flatpak-cargo-generator Cargo.lock -o cargo-sources.json
fi
cp "$work/cargo-sources.json" "$root/flatpak/cargo-sources.json"
echo "flatpak/cargo-sources.json: $(grep -c '"type"' "$root/flatpak/cargo-sources.json") sources from the Cargo.lock of $tag"
