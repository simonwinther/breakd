#!/usr/bin/env bash
set -euo pipefail

release_tag="${1:?release tag is required}"
dist="${2:-dist}"
pkgver="${release_tag#v}"

for tool in git gzip; do
	if ! command -v "$tool" >/dev/null 2>&1; then
		echo "$tool is required" >&2
		exit 1
	fi
done

if [[ -z "$pkgver" || "$pkgver" == "$release_tag" ]]; then
	echo "release tag must use the vX.Y.Z format" >&2
	exit 1
fi
if ! git rev-parse --verify --quiet "${release_tag}^{commit}" >/dev/null; then
	echo "release tag $release_tag is not available locally" >&2
	exit 1
fi

mkdir -p "$dist"
archive="$dist/breakd-$pkgver.tar.gz"
git archive --format=tar --prefix="breakd-$pkgver/" "$release_tag" \
	| gzip -n -9 >"$archive"
printf '%s\n' "$archive"
