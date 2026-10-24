#!/bin/sh
# The release files of Sensay, built in an Ubuntu 22.04 container so that the
# program needs glibc 2.35 or newer (Ubuntu 22.04, Debian 12, Fedora, Arch).
# Run from a Linux machine with podman; the files land in dist/:
#
#   packaging/release.sh
#
# For x86_64 and aarch64 each: the .deb, the .rpm, the AppImage, and the
# archive the AUR package downloads; then the AUR package itself in dist/aur
# and SHA256SUMS.
set -eu
cd "$(dirname "$0")/.."
mkdir -p dist
podman run --rm \
    --security-opt label=disable \
    -v "$PWD:/src" \
    -v sensay-release-cargo:/root/.cargo \
    -v sensay-release-rustup:/root/.rustup \
    -w /src \
    docker.io/library/ubuntu:22.04 \
    sh packaging/build-release.sh
