#!/usr/bin/env bash
# Builds and publishes the desktop app without GitHub Actions, with the
# version in Cargo.toml, as the desktop-vX.Y.Z release: signed AppImage,
# .exe, .app and .dmg, and the latest.json update manifest.
#
#   scripts/release-local.sh status
#   scripts/release-local.sh version desktop [X.Y.Z]
#   scripts/release-local.sh build desktop [linux] [windows] [macos]
#   scripts/release-local.sh publish desktop
#   scripts/release-local.sh download desktop <run-id>
#
# status shows the version, the latest tag and how many commits touched the
# app since then.
#
# version shows the version or changes it (and Cargo.lock).
#
# build compiles it into dist/desktop/:
#   linux    x86_64 (AppImage)
#   windows  x86_64, with mingw-w64 from Linux
#   macos    universal .app; only on a Mac
# Without platforms: linux and windows (plus macos on a Mac). linux and
# windows are built in Docker (scripts/builder.Dockerfile, Ubuntu 22.04) so
# they run on Ubuntu 22.04+ and Debian 12+; with DOCKER=0 they are built on
# this machine (which needs what that Dockerfile installs).
#
# publish creates the <component>-vX.Y.Z release with the contents of
# dist/<component>/, using the GitHub API (curl, no `gh`). It is created as a
# draft and published once all files are uploaded. If the release already
# exists, the files are added to it.
# The release is signed with termoak-release (of the termoak-update crate, in
# TermoakSSH/core, at the tag this app depends on), carries latest.json and is
# marked as "Latest" on GitHub. Linux and Windows can be published from a PC
# and macOS added later from a Mac: publishing again adds the files and
# updates latest.json. See docs/RELEASING.md.
#
# download puts into dist/desktop/ the files of a release.yml run that did not
# get to publish (the id is in the run's URL).
#
# Variables:
#   TERMOAK_UPDATE_PUBKEY  public key; compiled into the app (build)
#   TERMOAK_UPDATE_URL     https://YOUR-SERVER/updates/latest.json (build)
#   TERMOAK_UPDATE_SECRET  secret key (publish)
#   TERMOAK_OFFICIAL_SERVER  official server of the "Sign in to Termoak" button
#                 (build; default https://termoak.com; e.g. https://next.termoak.com)
#   GITHUB_TOKEN  GitHub token (publish and download). If unset, it is read
#                 from ~/.config/termoak/github-token. Fine-grained, with access to
#                 the TermoakSSH repositories, Contents: Read and write (and
#                 Actions: Read for download)
#   REPO          owner/repository (default: TermoakSSH/desktop)
#   COMMIT        commit to tag (default: HEAD)
#   VERSION       version for download and publish (default: the manifest's)
#
# Compatible with macOS's bash 3.2.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

die() { echo "error: $*" >&2; exit 1; }
say() { printf '\n==> %s\n' "$*"; }

COMPONENTS="desktop"

# --- components ---------------------------------------------------------------

manifest_of() { # component
  case "$1" in
    desktop) echo Cargo.toml ;;
    *) die "unknown component: ${1:-} (desktop)" ;;
  esac
}

title_of() { echo Desktop; }

# Code the app depends on (for `status`).
paths_of() { echo "src locales resources vendor assets build.rs Cargo.toml Cargo.lock rust-toolchain.toml scripts/package-desktop.sh"; }

version_of() { # component
  local file v
  file="$(manifest_of "$1")"
  if [[ "$1" == android ]]; then
    v="$(sed -n 's/^termoakVersion=\(.*\)$/\1/p' "$file" | head -1)"
  elif [[ "$1" == ios ]]; then
    v="$(sed -n 's/^ *MARKETING_VERSION: *"\(.*\)"$/\1/p' "$file" | head -1)"
  else
    v="$(sed -n 's/^version = "\(.*\)"/\1/p' "$file" | head -1)"
  fi
  [[ -n "$v" ]] || die "$file has no version = \"X.Y.Z\" line of its own (version.workspace = true?)"
  echo "$v"
}

# A component's latest published tag.
last_tag_of() { git tag -l "$1-v*" --sort=-v:refname | head -1; }

# Component and version from the command line. In build, `version` and `tag`
# always come from the manifest; in download and publish they can be changed
# with VERSION (e.g. for binaries from an earlier release.yml run).
select_component() { # component command
  component="${1:-}"
  [[ -n "$component" ]] || die "missing component: desktop"
  manifest_of "$component" >/dev/null
  version="$(version_of "$component")"
  if [[ -n "${VERSION:-}" && "$2" != build ]]; then
    version="${VERSION#v}"
  fi
  tag="$component-v$version"
  dist="$root/dist/$component"
}

# Inside Docker the binaries go to another folder so they don't mix with the
# ones built on the host (different glibc).
if [[ -n "${BUILD_TARGET_DIR:-}" ]]; then
  desktop_target="$BUILD_TARGET_DIR/desktop"
else
  desktop_target="$root/target"
fi

# The build image already has them installed (and rustup is read-only there).
add_targets() {
  [[ -n "${TERMOAK_BUILDER:-}" ]] || rustup target add "$@" >/dev/null
}

# Defaults for the official Termoak releases, so nothing has to be exported:
# the public key and the update URL are public; the secret key is read from
# ~/.config/termoak/update-secret (mode 600) when TERMOAK_UPDATE_SECRET is unset.
config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/termoak"
export TERMOAK_UPDATE_PUBKEY="${TERMOAK_UPDATE_PUBKEY:-zwbxnEY2xFDcaICtxAW8BVhccqXWdjXae4bsDB+Kkt4=}"
export TERMOAK_UPDATE_URL="${TERMOAK_UPDATE_URL:-https://termoak.com/updates/latest.json}"
if [[ -z "${TERMOAK_UPDATE_SECRET:-}" && -f "$config_dir/update-secret" ]]; then
  TERMOAK_UPDATE_SECRET="$(tr -d '[:space:]' <"$config_dir/update-secret")"
  export TERMOAK_UPDATE_SECRET
fi

check_update_env() {
  [[ -n "${TERMOAK_UPDATE_PUBKEY:-}" ]] ||
    die "TERMOAK_UPDATE_PUBKEY is missing (the GitHub variable of the same name): without it the app does not update itself"
  case "${TERMOAK_UPDATE_URL:-}" in
    https://*/latest.json) ;;
    "") die "TERMOAK_UPDATE_URL is missing: https://YOUR-SERVER/updates/latest.json (docs/RELEASING.md)" ;;
    *) die "TERMOAK_UPDATE_URL must be https://…/latest.json (currently: $TERMOAK_UPDATE_URL)" ;;
  esac
}

# --- status and version -------------------------------------------------------

cmd_status() {
  git fetch -q --tags origin 2>/dev/null || true
  printf '%-9s %-9s %-16s %s\n' component version 'latest tag' 'commits since'
  local c v last n note
  for c in $COMPONENTS; do
    v="$(version_of "$c")"
    last="$(last_tag_of "$c")"
    note=""
    if [[ -z "$last" ]]; then
      last="-"
      n="$(git rev-list --count HEAD)"
    else
      # shellcheck disable=SC2046
      n="$(git rev-list --count "$last..HEAD" -- $(paths_of "$c"))"
    fi
    if [[ "$n" != 0 ]] && git rev-parse -q --verify "refs/tags/$c-v$v" >/dev/null; then
      note="  (v$v already published: bump the version before publishing)"
    fi
    printf '%-9s %-9s %-16s %s%s\n' "$c" "$v" "$last" "$n" "$note"
  done
}

cmd_version() { # component [X.Y.Z]
  select_component "${1:-}" version
  local new="${2:-}" file
  if [[ -z "$new" ]]; then
    echo "$version"
    return
  fi
  new="${new#v}"
  [[ "$new" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || die "\"$new\" is not an X.Y.Z version"
  [[ "$new" != "$version" ]] || die "$component is already at $new"
  file="$(manifest_of "$component")"
  # Only the first `version = ` line.
  awk -v v="$new" '!done && /^version = "/ { print "version = \"" v "\""; done = 1; next } { print }' \
    "$file" >"$file.tmp" && mv "$file.tmp" "$file"
  cargo update -q -p termoak-desktop
  say "$component: $version → $new ($file and Cargo.lock)"
}

# --- build --------------------------------------------------------------------

build_desktop_bin() { # rust-target
  cargo build --release --locked --target "$1" --target-dir "$desktop_target"
}

package_desktop() { # binary-dir platform
  # package-desktop.sh names the files with vX.Y.Z.
  DESKTOP_BIN_DIR="$1" DIST_DIR="$dist" scripts/package-desktop.sh "$2" "v$version"
}

build_linux() {
  say "Desktop $version: Linux"
  build_desktop_bin x86_64-unknown-linux-gnu
  package_desktop "$desktop_target/x86_64-unknown-linux-gnu/release" linux-x86_64
}

build_windows() {
  add_targets x86_64-pc-windows-gnu
  say "Desktop $version: Windows"
  build_desktop_bin x86_64-pc-windows-gnu
  package_desktop "$desktop_target/x86_64-pc-windows-gnu/release" windows-x86_64
}

build_macos() {
  [[ "$(uname -s)" == Darwin ]] || die "macOS can only be built on a Mac"
  add_targets aarch64-apple-darwin x86_64-apple-darwin
  say "Desktop $version: macOS (universal)"
  build_desktop_bin aarch64-apple-darwin
  build_desktop_bin x86_64-apple-darwin
  mkdir -p "$desktop_target/universal"
  lipo -create -output "$desktop_target/universal/termoak-desktop" \
    "$desktop_target/aarch64-apple-darwin/release/termoak-desktop" \
    "$desktop_target/x86_64-apple-darwin/release/termoak-desktop"
  package_desktop "$desktop_target/universal" macos-universal
}

# Builds linux and windows inside the scripts/builder.Dockerfile image.
build_in_docker() { # platforms...
  command -v docker >/dev/null || die "Docker is missing (or use DOCKER=0 with the tools installed)"
  local toolchain
  toolchain="$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)"
  say "Build image (Ubuntu 22.04, Rust $toolchain for the desktop app)"
  docker build --platform linux/amd64 --build-arg "DESKTOP_TOOLCHAIN=$toolchain" \
    -t termoak-builder - <scripts/builder.Dockerfile
  # Run as this machine's user so dist/ and target/ stay owned by it.
  # The cargo registry is kept in target/builder so it isn't downloaded every time.
  docker run --rm --platform linux/amd64 --user "$(id -u):$(id -g)" \
    -v "$root:/src" -w /src -e HOME=/tmp \
    -e BUILD_TARGET_DIR=/src/target/builder -e CARGO_HOME=/src/target/builder/cargo \
    -e TERMOAK_UPDATE_PUBKEY -e TERMOAK_UPDATE_URL -e TERMOAK_OFFICIAL_SERVER \
    termoak-builder scripts/release-local.sh build "$component" "$@"
}

cmd_build() { # desktop [platforms...]
  select_component "${1:-}" build
  shift
  check_update_env
  local platforms=("$@") p in_docker=() native=()
  if [[ ${#platforms[@]} -eq 0 ]]; then
    platforms=(linux windows)
    if [[ "$(uname -s)" == Darwin ]]; then
      if command -v docker >/dev/null; then
        platforms=(macos "${platforms[@]}")
      else
        echo "Without Docker on this Mac, only macOS is built." >&2
        platforms=(macos)
      fi
    fi
  fi
  for p in "${platforms[@]}"; do
    case "$p" in
      linux | windows)
        if [[ -n "${TERMOAK_BUILDER:-}" || "${DOCKER:-1}" == 0 ]]; then native+=("$p"); else in_docker+=("$p"); fi ;;
      macos) native+=("$p") ;;
      *) die "unknown platform: $p (linux, windows or macos)" ;;
    esac
  done

  # The outer call empties dist/<component>/; the one inside Docker adds to it.
  if [[ -z "${TERMOAK_BUILDER:-}" ]]; then
    rm -rf "$dist"
    mkdir -p "$dist"
    echo "$tag" >"$dist/.version"
  fi
  if [[ ${#in_docker[@]} -gt 0 ]]; then build_in_docker "${in_docker[@]}"; fi
  for p in ${native[@]+"${native[@]}"}; do "build_$p"; done

  if [[ -z "${TERMOAK_BUILDER:-}" ]]; then
    say "Done: $tag in dist/$component/"
    ls -l "$dist"
  fi
}

# --- GitHub API (curl) --------------------------------------------------------

github_setup() {
  command -v curl >/dev/null || die "curl is missing"
  command -v python3 >/dev/null || die "python3 is missing (needed to read GitHub's JSON responses)"
  local file="${XDG_CONFIG_HOME:-$HOME/.config}/termoak/github-token"
  github_token="${GITHUB_TOKEN:-}"
  if [[ -z "$github_token" && -f "$file" ]]; then
    github_token="$(tr -d '[:space:]' <"$file")"
  fi
  # Otherwise, the login of the GitHub CLI if it is installed (`gh auth login`).
  if [[ -z "$github_token" ]] && command -v gh >/dev/null; then
    github_token="$(gh auth token 2>/dev/null || true)"
  fi
  [[ -n "$github_token" ]] ||
    die "the GitHub token is missing: GITHUB_TOKEN, $file or \`gh auth login\`"
  # This repository; REPO=owner/repository publishes somewhere else (a fork).
  repo="${REPO:-TermoakSSH/desktop}"
  [[ "$repo" == */* ]] || die "cannot tell which repository this is: set REPO=owner/repository"
}

# Calls the API. Leaves the response in api_body and the HTTP status in api_status.
github() { # method path-or-url [curl arguments...]
  local method="$1" url="$2" out
  shift 2
  [[ "$url" == https://* ]] || url="https://api.github.com$url"
  # -L: GitHub redirects downloads to its storage (curl does not forward the
  # token to another domain).
  out="$(curl -sS -L -X "$method" -w '\n%{http_code}' \
    -H "Authorization: Bearer $github_token" -H "Accept: application/vnd.github+json" \
    -H "X-GitHub-Api-Version: 2022-11-28" "$@" "$url")" || die "could not connect to GitHub"
  api_status="${out##*$'\n'}"
  api_body="${out%$'\n'*}"
}
# Like github(), but stops if GitHub answers with an error.
github_ok() { # what-we-were-doing method path [curl arguments...]
  local what="$1"
  shift
  github "$@"
  if [[ "$api_status" != 2* ]]; then
    printf '%s\n' "$api_body" >&2
    die "GitHub answered $api_status when trying to $what"
  fi
}
# Python expression over the JSON response (in `d`).
json() {
  printf '%s' "$api_body" | python3 -c "import json, sys; d = json.load(sys.stdin); v = $1; print('' if v is None else v)"
}
# JSON object from key value pairs. `draft` is a boolean; everything else is
# a string (make_latest too: the API expects "true" or "false").
json_object() {
  python3 -c '
import json, sys
a = sys.argv[1:]
print(json.dumps({k: (v == "true") if k == "draft" else v for k, v in zip(a[::2], a[1::2])}))' "$@"
}

# --- download -----------------------------------------------------------------

cmd_download() { # component id
  select_component "${1:-}" download
  local run="${2:-}" work
  [[ -n "$run" ]] || die "missing run id (the number in its Actions URL)"
  command -v unzip >/dev/null || die "unzip is missing"
  github_setup
  github_ok "list the artifacts of run $run" GET "/repos/$repo/actions/runs/$run/artifacts?per_page=100"
  local urls url
  urls="$(json "'\\n'.join(a['archive_download_url'] for a in d['artifacts'] if a['name'].startswith('$component-') and not a['expired'])")"
  [[ -n "$urls" ]] || die "that run has no $component artifacts (or they expired)"
  work="$(mktemp -d)"
  while IFS= read -r url; do
    curl -fsSL -H "Authorization: Bearer $github_token" -o "$work/a.zip" "$url" ||
      die "could not download an artifact"
    unzip -q -o "$work/a.zip" -d "$work/files"
    rm -f "$work/a.zip"
  done <<<"$urls"
  find "$work" -type f -name "*v$version*" | grep -q . ||
    die "that run is not for $tag: set its version with VERSION=X.Y.Z"
  rm -rf "$dist"
  mkdir -p "$dist"
  find "$work" -type f -exec mv {} "$dist/" \;
  rm -rf "$work"
  echo "$tag" >"$dist/.version"
  say "Artifacts of $tag in dist/$component/"
  ls -l "$dist"
}

# --- publish ------------------------------------------------------------------

# termoak-release (termoak-update crate) of the core version this app is
# built with (the tag of termoak-core in Cargo.toml), installed once into
# target/tools/<tag>; inside the build image if this machine has no Rust.
core_tag() { sed -n 's/^termoak-core = {.*tag = "\([^"]*\)".*/\1/p' Cargo.toml | head -1; }

release_tool() {
  local tag
  tag="$(core_tag)"
  [[ -n "$tag" ]] || die "cannot read the tag of termoak-core in Cargo.toml"
  if command -v cargo >/dev/null; then
    if [[ ! -x "$root/target/tools/$tag/bin/termoak-release" ]]; then
      say "Installing termoak-release from TermoakSSH/core $tag"
      cargo install -q --locked --git https://github.com/TermoakSSH/core --tag "$tag" \
        --root "$root/target/tools/$tag" termoak-update --bin termoak-release
    fi
    "$root/target/tools/$tag/bin/termoak-release" "$@"
  else
    # Same paths inside the container, so the file arguments work as they are.
    docker run --rm --platform linux/amd64 --user "$(id -u):$(id -g)" \
      -v "$root:$root" -w "$root" -e HOME=/tmp -e CARGO_HOME="$root/target/builder/cargo" \
      -e TERMOAK_UPDATE_SECRET -e TERMOAK_UPDATE_PUBKEY \
      termoak-builder bash -c 'set -e
        t="$PWD/target/builder/tools/$0"
        [ -x "$t/bin/termoak-release" ] || cargo install -q --locked --git https://github.com/TermoakSSH/core \
          --tag "$0" --root "$t" termoak-update --bin termoak-release
        exec "$t/bin/termoak-release" "$@"' "$tag" "$@"
  fi
}

sign_desktop() { # base-url
  local base="$1"
  sign() { # platform format file
    [[ -f "$dist/$3" ]] || return 0
    release_tool sign --version "$version" --target "$1" --format "$2" \
      --file "$dist/$3" --url "$base/$3" --manifest "$dist/latest.json" \
      --notes "Termoak $version"
  }
  say "Signing updates"
  sign linux-x86_64 appimage Termoak-linux-x86_64.AppImage
  sign macos-universal app.tar.gz Termoak-macos-universal.app.tar.gz
  sign windows-x86_64 bin Termoak-windows-x86_64.exe
  [[ -f "$dist/latest.json" ]] || die "there is no desktop app in dist/desktop/ to sign"
  cat "$dist/latest.json"
}

cmd_publish() { # component
  select_component "${1:-}" publish
  local commit base existing=0 file files=() prev latest id asset_id manifest_id
  [[ -z "${2:-}" ]] || die "unknown option: $2"
  [[ "$(cat "$dist/.version" 2>/dev/null)" == "$tag" ]] ||
    die "dist/$component/ does not hold $tag: run scripts/release-local.sh build $component first"
  github_setup
  [[ -n "${TERMOAK_UPDATE_SECRET:-}" ]] ||
    die "the update secret key is missing: put it once in $config_dir/update-secret (chmod 600) or set TERMOAK_UPDATE_SECRET"
  release_tool check-keys

  base="https://github.com/$repo/releases/download/$tag"
  github GET "/repos/$repo/releases/tags/$tag"
  if [[ "$api_status" == 200 ]]; then
    existing=1
    id="$(json 'd["id"]')"
    say "Release $tag already exists: adding the files"
    rm -f "$dist/latest.json"
    manifest_id="$(json 'next((a["id"] for a in d["assets"] if a["name"] == "latest.json"), None)')"
    if [[ -n "$manifest_id" ]]; then
      curl -fsSL -H "Authorization: Bearer $github_token" -H "Accept: application/octet-stream" \
        -o "$dist/latest.json" "https://api.github.com/repos/$repo/releases/assets/$manifest_id" ||
        die "could not download the latest.json of $tag"
    fi
  elif [[ "$api_status" != 404 ]]; then
    printf '%s\n' "$api_body" >&2
    die "GitHub answered $api_status when looking up release $tag (does the token have access to the repository?)"
  else
    commit="${COMMIT:-$(git rev-parse HEAD)}"
    git fetch -q --tags origin 2>/dev/null || true
    [[ -n "$(git branch -r --contains "$commit" 2>/dev/null)" ]] ||
      die "commit $commit is not on GitHub: push it first (git push)"
  fi

  sign_desktop "$base"

  # latest.json goes last: it never points to a file that is not there yet.
  for file in "$dist"/*; do
    [[ -f "$file" && "$(basename "$file")" != latest.json ]] && files+=("$file")
  done
  [[ -f "$dist/latest.json" ]] && files+=("$dist/latest.json")
  [[ ${#files[@]} -gt 0 ]] || die "nothing to publish in dist/$component/"

  if [[ $existing == 0 ]]; then
    # An earlier attempt that failed halfway leaves a draft: reuse it.
    github_ok "look for drafts" GET "/repos/$repo/releases?per_page=100"
    id="$(json "next((r['id'] for r in d if r['draft'] and r['tag_name'] == '$tag'), None)")"
  fi
  if [[ $existing == 0 && -n "$id" ]]; then
    say "Found a draft of $tag from an earlier attempt: completing it"
  elif [[ $existing == 0 ]]; then
    # Notes since the previous version of this same component.
    prev="$(last_tag_of "$component")"
    if [[ -n "$prev" && "$prev" != "$tag" ]]; then
      github_ok "generate the notes" POST "/repos/$repo/releases/generate-notes" \
        -d "$(json_object tag_name "$tag" target_commitish "$commit" previous_tag_name "$prev")"
    else
      github_ok "generate the notes" POST "/repos/$repo/releases/generate-notes" \
        -d "$(json_object tag_name "$tag" target_commitish "$commit")"
    fi
    say "Creating release $tag in $repo ($commit) as a draft"
    github_ok "create the release" POST "/repos/$repo/releases" \
      -d "$(json_object tag_name "$tag" target_commitish "$commit" \
        name "$(title_of "$component") $version" body "$(json 'd["body"]')" draft true)"
    id="$(json 'd["id"]')"
  fi

  for file in ${files[@]+"${files[@]}"}; do
    # If one with that name already exists (existing release), it is replaced.
    github_ok "read the release" GET "/repos/$repo/releases/$id"
    asset_id="$(json "next((a['id'] for a in d['assets'] if a['name'] == '$(basename "$file")'), None)")"
    if [[ -n "$asset_id" ]]; then
      github_ok "delete $(basename "$file")" DELETE "/repos/$repo/releases/assets/$asset_id"
    fi
    echo "  uploading $(basename "$file")"
    github_ok "upload $(basename "$file")" POST \
      "https://uploads.github.com/repos/$repo/releases/$id/assets?name=$(basename "$file")" \
      -H "Content-Type: application/octet-stream" --data-binary "@$file"
  done

  if [[ $existing == 0 ]]; then
    # "Latest" on GitHub: without its own server, the app looks for updates
    # at releases/latest/download/latest.json.
    latest=true
    github_ok "publish the release" PATCH "/repos/$repo/releases/$id" \
      -d "$(json_object draft false make_latest "$latest")"
  fi
  say "Published $tag: https://github.com/$repo/releases/tag/$tag"
}

# With exit in every branch bash does not read this file again: it can be
# edited (or git pulled) while it builds.
case "${1:-}" in
  status) cmd_status; exit ;;
  version) shift; cmd_version "$@"; exit ;;
  build) shift; cmd_build "$@"; exit ;;
  publish) shift; cmd_publish "$@"; exit ;;
  download) shift; cmd_download "$@"; exit ;;
  *) awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "$0"; exit 1 ;;
esac
