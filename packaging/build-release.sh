#!/bin/sh
# Runs inside the ubuntu:22.04 container of release.sh. Builds Sensay for the
# machine's own architecture and, with a cross compiler, for the other one,
# then packs each build. Ubuntu keeps amd64 on archive.ubuntu.com and arm64
# on ports.ubuntu.com, so the other architecture gets sources of its own.
set -eu

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
host=$(uname -m)
case "$host" in
    aarch64) other=x86_64 ;;
    x86_64) other=aarch64 ;;
    *) echo "unsupported machine $host" >&2; exit 1 ;;
esac
deb_arch() {
    case "$1" in
        x86_64) echo amd64 ;;
        aarch64) echo arm64 ;;
    esac
}
host_deb=$(deb_arch "$host")
other_deb=$(deb_arch "$other")

export DEBIAN_FRONTEND=noninteractive
dpkg --add-architecture "$other_deb"
sed -i "s/^deb http/deb [arch=$host_deb] http/" /etc/apt/sources.list
if [ "$other" = x86_64 ]; then
    mirror=http://archive.ubuntu.com/ubuntu
    security=http://security.ubuntu.com/ubuntu
else
    mirror=http://ports.ubuntu.com/ubuntu-ports
    security=$mirror
fi
cat >> /etc/apt/sources.list <<SOURCES
deb [arch=$other_deb] $mirror jammy main universe
deb [arch=$other_deb] $mirror jammy-updates main universe
deb [arch=$other_deb] $security jammy-security main universe
SOURCES
apt-get update
apt-get install -y --no-install-recommends \
    ca-certificates curl file build-essential pkg-config dpkg-dev rpm \
    "gcc-$(echo "$other" | tr _ -)-linux-gnu" "libc6-dev-$other_deb-cross" \
    libpulse-dev "libpulse-dev:$other_deb" \
    libxkbcommon-x11-0 "libxkbcommon-x11-0:$other_deb" libxcb-xkb1 "libxcb-xkb1:$other_deb"

if [ ! -x /root/.cargo/bin/cargo ]; then
    curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
fi
. /root/.cargo/env
rustup target add "$host-unknown-linux-gnu" "$other-unknown-linux-gnu"

export CARGO_TARGET_DIR=/src/target/release-container
cross=$(echo "$other-unknown-linux-gnu" | tr 'a-z-' 'A-Z_')
target_vars=$(echo "$other-unknown-linux-gnu" | tr - _)
export "CARGO_TARGET_${cross}_LINKER=$other-linux-gnu-gcc"
export "CC_$target_vars=$other-linux-gnu-gcc"
export "AR_$target_vars=$other-linux-gnu-ar"
export PKG_CONFIG_ALLOW_CROSS=1
export "PKG_CONFIG_LIBDIR_$target_vars=/usr/lib/$other-linux-gnu/pkgconfig:/usr/share/pkgconfig"
for arch in "$host" "$other"; do
    # rsync keeps the edit times of the sources, which can be older than the
    # last build: cargo would take build.rs's output as fresh.
    cargo clean --release --target "$arch-unknown-linux-gnu" -p sensay
    cargo build --release --locked --target "$arch-unknown-linux-gnu"
done

tools=$CARGO_TARGET_DIR/tools
mkdir -p "$tools"
release=https://github.com/AppImage
[ -x "$tools/appimagetool" ] || curl -sSfL -o "$tools/appimagetool" \
    "$release/appimagetool/releases/download/continuous/appimagetool-$host.AppImage"
chmod +x "$tools/appimagetool"
for arch in "$host" "$other"; do
    [ -f "$tools/runtime-$arch" ] || curl -sSfL -o "$tools/runtime-$arch" \
        "$release/type2-runtime/releases/download/continuous/runtime-$arch"
done

work=$(mktemp -d)
dist=/src/dist
rm -rf "$dist"
mkdir -p "$dist/aur"

# The installed files under $2/usr, the same in every format.
stage() {
    install -D -m 0755 "$CARGO_TARGET_DIR/$1-unknown-linux-gnu/release/sensay" "$2/usr/bin/sensay"
    install -D -m 0644 data/com.advertumtech.Sensay.desktop \
        "$2/usr/share/applications/com.advertumtech.Sensay.desktop"
    (cd data && find icons -type f) | while read -r icon; do
        install -D -m 0644 "data/$icon" "$2/usr/share/$icon"
    done
    install -D -m 0644 data/70-sensay.rules "$2/usr/lib/udev/rules.d/70-sensay.rules"
    for file in gnome/sensay-pill@sensay.app/*; do
        install -D -m 0644 "$file" "$2/usr/share/gnome-shell/extensions/sensay-pill@sensay.app/${file##*/}"
    done
}

for arch in "$host" "$other"; do
    debian=$(deb_arch "$arch")

    root=$work/deb-$arch
    stage "$arch" "$root"
    mkdir -p "$root/DEBIAN"
    size=$(du -sk "$root/usr" | cut -f 1)
    sed -e "s/@VERSION@/$version/" -e "s/@ARCH@/$debian/" -e "s/@SIZE@/$size/" \
        packaging/deb/control > "$root/DEBIAN/control"
    install -m 0755 packaging/deb/postinst packaging/deb/postrm "$root/DEBIAN/"
    # xz: every dpkg reads it; Ubuntu's own default, zstd, needs a newer one.
    dpkg-deb -Zxz --build --root-owner-group "$root" "$dist/sensay_${version}_$debian.deb"

    root=$work/rpm-$arch
    stage "$arch" "$root"
    rpmbuild -bb --target "$arch" \
        --define "_topdir $work/rpmbuild" \
        --define "_rpmdir $dist" \
        --define "_build_name_fmt %%{NAME}-%%{VERSION}-%%{RELEASE}.%%{ARCH}.rpm" \
        --define "sensay_version $version" \
        --define "sensay_stage $root" \
        packaging/rpm/sensay.spec

    root=$work/tar-$arch/sensay-$version
    stage "$arch" "$root"
    tar -C "$work/tar-$arch" --owner=0 --group=0 -czf "$dist/sensay-$version-$arch.tar.gz" "sensay-$version"

    root=$work/appdir-$arch
    stage "$arch" "$root"
    for library in libxkbcommon-x11.so.0 libxcb-xkb.so.1; do
        cp -L "/usr/lib/$arch-linux-gnu/$library" "$root/usr/lib/"
    done
    ln -s usr/bin/sensay "$root/AppRun"
    cp data/com.advertumtech.Sensay.desktop "$root/"
    cp data/icons/hicolor/256x256/apps/com.advertumtech.Sensay.png "$root/"
    ARCH=$arch "$tools/appimagetool" --appimage-extract-and-run --no-appstream \
        --runtime-file "$tools/runtime-$arch" "$root" "$dist/Sensay-$version-$arch.AppImage"
done

sum() {
    sha256sum "$dist/sensay-$version-$1.tar.gz" | cut -d ' ' -f 1
}
sed -e "s/@VERSION@/$version/" -e "s/@SHA_X86_64@/$(sum x86_64)/" -e "s/@SHA_AARCH64@/$(sum aarch64)/" \
    packaging/aur/PKGBUILD > "$dist/aur/PKGBUILD"
cp packaging/aur/sensay.install "$dist/aur/"
# .SRCINFO is what makepkg --printsrcinfo prints for this PKGBUILD.
bash -c '
    . "$1"
    printf "pkgbase = %s\n" "$pkgname"
    printf "\tpkgdesc = %s\n\tpkgver = %s\n\tpkgrel = %s\n\turl = %s\n\tinstall = %s\n" \
        "$pkgdesc" "$pkgver" "$pkgrel" "$url" "$install"
    for value in "${arch[@]}"; do printf "\tarch = %s\n" "$value"; done
    for value in "${license[@]}"; do printf "\tlicense = %s\n" "$value"; done
    for value in "${depends[@]}"; do printf "\tdepends = %s\n" "$value"; done
    for value in "${provides[@]}"; do printf "\tprovides = %s\n" "$value"; done
    for value in "${conflicts[@]}"; do printf "\tconflicts = %s\n" "$value"; done
    for value in "${options[@]}"; do printf "\toptions = %s\n" "$value"; done
    printf "\tsource_x86_64 = %s\n\tsha256sums_x86_64 = %s\n" "${source_x86_64[0]}" "${sha256sums_x86_64[0]}"
    printf "\tsource_aarch64 = %s\n\tsha256sums_aarch64 = %s\n" "${source_aarch64[0]}" "${sha256sums_aarch64[0]}"
    printf "\npkgname = %s\n" "$pkgname"
' sh "$dist/aur/PKGBUILD" > "$dist/aur/.SRCINFO"

(cd "$dist" && sha256sum ./*.deb ./*.rpm ./*.AppImage ./*.tar.gz > SHA256SUMS)
rm -rf "$work"
ls -l "$dist" "$dist/aur"
