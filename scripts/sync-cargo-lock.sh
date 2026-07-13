#!/usr/bin/env bash
set -euo pipefail

# Release-please bumps the version in every workspace Cargo.toml, but only
# rewrites the root package entry in Cargo.lock. The member crates keep their
# previous versions, so `cargo fetch --locked` refuses to build the tagged tree.
# Run this on the release pull request branch to bring Cargo.lock back in sync.

for tool in cargo git; do
	if ! command -v "$tool" >/dev/null 2>&1; then
		echo "$tool is required to sync Cargo.lock" >&2
		exit 1
	fi
done

cargo update --workspace --quiet

if git diff --quiet -- Cargo.lock; then
	echo "Cargo.lock already matches the workspace versions"
	exit 0
fi

git config user.name "${GIT_AUTHOR_NAME:-github-actions[bot]}"
git config user.email "${GIT_AUTHOR_EMAIL:-github-actions[bot]@users.noreply.github.com}"
git add Cargo.lock
git commit -m "chore: sync Cargo.lock with bumped crate versions"

branch="$(git rev-parse --abbrev-ref HEAD)"
git push origin "HEAD:refs/heads/$branch"
