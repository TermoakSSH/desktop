# Image for building the Linux and Windows releases without GitHub Actions
# (used by scripts/release-local.sh). Ubuntu 22.04 (glibc 2.35), like CI:
# the binaries run on Ubuntu 22.04+ and Debian 12+.
#
# - Linux x86_64: native.
# - Linux aarch64: gcc cross compiler.
# - Windows x86_64: mingw-w64; the GPUI shaders come precompiled
#   (vendor/gpui-pre-windows/README.md of TermoakSSH/desktop).
FROM ubuntu:22.04

ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      build-essential ca-certificates curl file git pkg-config zip \
      gcc-aarch64-linux-gnu libc6-dev-arm64-cross \
      gcc-mingw-w64-x86-64 \
      libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-xcb-dev libxcb1-dev \
      libfontconfig-dev libfreetype-dev libvulkan-dev libasound2-dev librsvg2-bin \
 && rm -rf /var/lib/apt/lists/*

# stable for the server and the CLI; the one in the desktop app's
# rust-toolchain.toml for the desktop app (its release script passes it as
# DESKTOP_TOOLCHAIN). The same file is in core, server and desktop.
ARG DESKTOP_TOOLCHAIN=1.95.0
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl -fsSLo /tmp/rustup-init https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init \
 && chmod +x /tmp/rustup-init \
 && /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain stable \
      --target aarch64-unknown-linux-gnu --target x86_64-pc-windows-gnu \
 && rm /tmp/rustup-init \
 && rustup toolchain install "$DESKTOP_TOOLCHAIN" --profile minimal --component rustfmt \
      --target x86_64-pc-windows-gnu \
 && chmod -R a+rX "$RUSTUP_HOME" "$CARGO_HOME"

ENV TERMOAK_BUILDER=1 \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar
