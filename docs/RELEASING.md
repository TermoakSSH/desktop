# Releasing the desktop app

The desktop app is released with the version in `Cargo.toml` as the
`desktop-vX.Y.Z` GitHub release: the signed AppImage, `.exe`, `.app` and
`.dmg`, and `latest.json`, the manifest the app checks for updates. It is the
release marked as *Latest*: without a server of its own, the app looks for
updates at
`https://github.com/TermoakSSH/desktop/releases/latest/download/latest.json`.

## Signing keys (once)

1. Generate the desktop signing keys with
   `termoak-release keygen` (install it with
   `cargo install --git https://github.com/TermoakSSH/core termoak-update --bin termoak-release`).
2. On GitHub, in *Settings > Secrets and variables > Actions*:
   - **Secrets** tab: `TERMOAK_UPDATE_SECRET`, with the secret key;
   - **Variables** tab: `TERMOAK_UPDATE_PUBKEY`, with the public key. It
     must be a variable, not a secret: it is compiled into the app, and
     without it the app does not update itself.

   Also keep the secret key outside GitHub (a password manager): you need it
   to publish from your machine, and GitHub does not show it again.

## Publishing from your machine

`scripts/release-local.sh` builds and publishes without spending GitHub
Actions minutes. It needs Docker to build Linux and Windows. To publish it
uses the GitHub API with `curl` (no `gh` needed) and a *fine-grained* token
with *Contents: Read and write* on this repository (and *Actions: Read* for
`download`), in `~/.config/termoak/github-token` (mode `600`) or in
`GITHUB_TOKEN`. Updates are signed with `termoak-release` from the same core
tag the app is built with, installed into `target/tools/` the first time.

```sh
scripts/release-local.sh status                  # unpublished changes since the last release
scripts/release-local.sh version desktop 0.2.1   # bump the version (Cargo.toml and Cargo.lock)
git commit -am "Desktop 0.2.1" && git push       # the release is created on a pushed commit

export TERMOAK_UPDATE_PUBKEY=...   # the public key (GitHub variable)
export TERMOAK_UPDATE_URL=https://YOUR-SERVER/updates/latest.json
scripts/release-local.sh build desktop

export TERMOAK_UPDATE_SECRET=...   # the secret key from keygen
scripts/release-local.sh publish desktop
```

- Always bump the version before publishing. An app with the same version as
  the published one would consider itself up to date, and `publish` adds
  files to an existing release instead of creating a new one.
- `build` compiles Linux and Windows in an Ubuntu 22.04 container
  (`scripts/builder.Dockerfile`); the binaries run on Ubuntu 22.04+ and
  Debian 12+. Windows is cross-compiled with mingw-w64: the GPUI shaders are
  precompiled (`vendor/gpui-pre-windows/README.md`).
- macOS can only be built on a Mac: there, `build desktop macos` and
  `publish desktop` add macOS to the release you already published from
  Linux. Until a platform is in `latest.json`, its apps stay on the previous
  version.
- `TERMOAK_UPDATE_URL` makes the app look for updates on your server instead
  (see "Updates and downloads through the server" in the
  [server's deployment guide](https://github.com/TermoakSSH/server/blob/main/docs/DEPLOYMENT.md)).

## Code signing

Distributing on macOS without Gatekeeper warnings requires signing and
notarizing with an Apple Developer account. On Windows, a code signing
certificate avoids the SmartScreen warning. The packaging script
(`scripts/package-desktop.sh`) is ready for both steps to be added.
