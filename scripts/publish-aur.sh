#!/usr/bin/env bash
set -euo pipefail

version="${1:?version is required}"
archdist="${2:-dist/arch}"
pkgname="${AUR_PKGNAME:-breakd}"

if [[ -z "${AUR_SSH_PRIVATE_KEY:-}" ]]; then
	echo "AUR_SSH_PRIVATE_KEY is required to publish the AUR package" >&2
	exit 1
fi

for tool in git ssh ssh-keygen ssh-keyscan; do
	if ! command -v "$tool" >/dev/null 2>&1; then
		echo "$tool is required to publish the AUR package" >&2
		exit 1
	fi
done

if [[ ! -f "$archdist/PKGBUILD" || ! -f "$archdist/.SRCINFO" ]]; then
	echo "$archdist/PKGBUILD and $archdist/.SRCINFO are required" >&2
	exit 1
fi

tmp="$(mktemp -d)"
cleanup() {
	rm -rf "$tmp"
}
trap cleanup EXIT

install -m 700 -d "$tmp/ssh"
printf '%s\n' "$AUR_SSH_PRIVATE_KEY" >"$tmp/ssh/aur"
chmod 600 "$tmp/ssh/aur"
ssh-keyscan -t rsa,ed25519 aur.archlinux.org >"$tmp/ssh/known_hosts"
expected_fingerprint="SHA256:RFzBCUItH9LZS0cKB5UE6ceAYhBD5C8GeOBip8Z11+4"
actual_fingerprint="$(
	ssh-keygen -lf "$tmp/ssh/known_hosts" -E sha256 \
		| awk '$4 == "(ED25519)" { print $2 }'
)"
if [[ "$actual_fingerprint" != "$expected_fingerprint" ]]; then
	echo "AUR SSH host key fingerprint does not match aur.archlinux.org" >&2
	exit 1
fi
export GIT_SSH_COMMAND="ssh -i $tmp/ssh/aur -o IdentitiesOnly=yes -o UserKnownHostsFile=$tmp/ssh/known_hosts -o StrictHostKeyChecking=yes"

aur_url="ssh://aur@aur.archlinux.org/$pkgname.git"
repo="$tmp/$pkgname"
git clone "$aur_url" "$repo"

cp "$archdist/PKGBUILD" "$repo/PKGBUILD"
cp "$archdist/.SRCINFO" "$repo/.SRCINFO"

git -C "$repo" config user.name "${GIT_AUTHOR_NAME:-github-actions[bot]}"
git -C "$repo" config user.email "${GIT_AUTHOR_EMAIL:-github-actions[bot]@users.noreply.github.com}"
git -C "$repo" add PKGBUILD .SRCINFO
if git -C "$repo" diff --cached --quiet; then
	echo "AUR package $pkgname already matches $version"
	exit 0
fi
git -C "$repo" commit -m "Update $pkgname to $version"
git -C "$repo" push origin HEAD:master
