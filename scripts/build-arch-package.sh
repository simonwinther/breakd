#!/usr/bin/env bash
set -euo pipefail

release_tag="${1:?release tag is required}"
source_archive="${2:?source archive is required}"
archdist="${3:-dist/arch}"
pkgname="breakd"
pkgver="${release_tag#v}"
pkgrel="1"
repo_url="https://github.com/simonwinther/breakd"
archive_name="$pkgname-$pkgver.tar.gz"

for tool in makepkg sha256sum; do
	if ! command -v "$tool" >/dev/null 2>&1; then
		echo "$tool is required" >&2
		exit 1
	fi
done

if [[ -z "$pkgver" || "$pkgver" == "$release_tag" ]]; then
	echo "release tag must use the vX.Y.Z format" >&2
	exit 1
fi
if [[ ! -f "$source_archive" ]]; then
	echo "source archive $source_archive does not exist" >&2
	exit 1
fi

if [[ -d "$archdist" ]]; then
	chmod -R u+w "$archdist" 2>/dev/null || true
fi
rm -rf "$archdist"
mkdir -p "$archdist"
cp "$source_archive" "$archdist/$archive_name"
source_sha="$(sha256sum "$source_archive" | awk '{print $1}')"

cat >"$archdist/PKGBUILD" <<PKGBUILD
# Maintainer: Simon Winther <simonwinther@users.noreply.github.com>
pkgname=$pkgname
pkgver=$pkgver
pkgrel=$pkgrel
pkgdesc='Wayland-native break reminder with multi-monitor overlays'
arch=('x86_64')
url='$repo_url'
license=('MIT' 'BSD-2-Clause')
depends=('cairo' 'glib2' 'glibc' 'graphene' 'gtk4' 'gtk4-layer-shell')
makedepends=('cargo' 'pkgconf')
options=('!debug')
source=("$archive_name::$repo_url/releases/download/$release_tag/$archive_name")
sha256sums=('$source_sha')

prepare() {
  cd "\$pkgname-\$pkgver"
  export RUSTUP_TOOLCHAIN=stable
  export CARGO_HOME="\$srcdir/cargo-home"
  cargo fetch --locked --target "\$CARCH-unknown-linux-gnu"
}

build() {
  cd "\$pkgname-\$pkgver"
  export RUSTUP_TOOLCHAIN=stable
  export CARGO_HOME="\$srcdir/cargo-home"
  export CARGO_TARGET_DIR="\$srcdir/target"
  export RUSTFLAGS="\${RUSTFLAGS:-} --remap-path-prefix=\$srcdir=."
  cargo build --frozen --release --workspace
}

check() {
  cd "\$pkgname-\$pkgver"
  export RUSTUP_TOOLCHAIN=stable
  export CARGO_HOME="\$srcdir/cargo-home"
  export CARGO_TARGET_DIR="\$srcdir/target"
  export RUSTFLAGS="\${RUSTFLAGS:-} --remap-path-prefix=\$srcdir=."
  cargo test --frozen --workspace
}

package() {
  cd "\$pkgname-\$pkgver"
  install -Dm755 "\$srcdir/target/release/breakd" "\$pkgdir/usr/bin/breakd"
  install -Dm644 packaging/systemd/breakd.service \
    "\$pkgdir/usr/lib/systemd/user/breakd.service"
  install -Dm644 packaging/io.github.simonwinther.breakd.settings.desktop \
    "\$pkgdir/usr/share/applications/io.github.simonwinther.breakd.settings.desktop"
  install -Dm644 config.example.toml \
    "\$pkgdir/usr/share/doc/breakd/config.example.toml"
  install -Dm644 README.md "\$pkgdir/usr/share/doc/breakd/README.md"
  install -Dm644 LICENSE "\$pkgdir/usr/share/licenses/breakd/LICENSE"
  install -Dm644 THIRD_PARTY_NOTICES.md \
    "\$pkgdir/usr/share/licenses/breakd/THIRD_PARTY_NOTICES.md"
}
PKGBUILD

(
	cd "$archdist"
	makepkg --printsrcinfo >.SRCINFO
	makepkg --force --cleanbuild
)

chmod -R u+w "$archdist/src" "$archdist/pkg" 2>/dev/null || true
rm -rf "${archdist:?}/src" "${archdist:?}/pkg"
rm -f "$archdist/$archive_name"
