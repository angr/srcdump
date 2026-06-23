#!/usr/bin/env bash
#
# Bump srcdump's version, commit the change, and tag it.
#
# Usage:
#   scripts/bump-version.sh <major|minor|patch>   # bump a component of the current version
#   scripts/bump-version.sh X.Y.Z                  # set an explicit version
#
# The script updates the [package] version in Cargo.toml (and the matching
# Cargo.lock entry), creates a "Bump version to X.Y.Z" commit, and adds an
# annotated vX.Y.Z tag. It refuses to run with a dirty working tree or if the
# target tag already exists. Nothing is pushed.

set -euo pipefail

cd "$(dirname "$0")/.."

die() {
	echo "bump-version: $*" >&2
	exit 1
}

[ $# -eq 1 ] || die "usage: bump-version.sh <major|minor|patch|X.Y.Z>"

command -v git >/dev/null || die "git not found"
git rev-parse --git-dir >/dev/null 2>&1 || die "not a git repository"

# A dirty tree would get swept into the bump commit; insist on a clean one.
[ -z "$(git status --porcelain)" ] || die "working tree is dirty; commit or stash first"

# Read the current [package] version (the first version= under [package]).
current=$(awk '/^\[package\]/{p=1} p&&/^version[[:space:]]*=/{gsub(/[" ]/,"",$3); print $3; exit}' Cargo.toml)
[ -n "$current" ] || die "could not find the package version in Cargo.toml"
[[ "$current" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "current version '$current' is not X.Y.Z"

IFS=. read -r major minor patch <<<"$current"

case "$1" in
	major) new="$((major + 1)).0.0" ;;
	minor) new="${major}.$((minor + 1)).0" ;;
	patch) new="${major}.${minor}.$((patch + 1))" ;;
	*)
		[[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "invalid version '$1' (want major|minor|patch or X.Y.Z)"
		new="$1"
		;;
esac

[ "$new" != "$current" ] || die "new version equals current version ($current)"

tag="v$new"
git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "tag $tag already exists"

# Update Cargo.toml: only the version line within the [package] section.
awk -v new="$new" '
	/^\[/ { in_pkg = ($0 == "[package]") }
	in_pkg && /^version[[:space:]]*=/ && !done {
		print "version = \"" new "\""
		done = 1
		next
	}
	{ print }
' Cargo.toml >Cargo.toml.tmp && mv Cargo.toml.tmp Cargo.toml

# Keep Cargo.lock in sync (regenerates the srcdump entry without touching deps).
if command -v cargo >/dev/null; then
	cargo update --offline -p srcdump >/dev/null 2>&1 || cargo update -p srcdump >/dev/null 2>&1 || true
fi

git add Cargo.toml Cargo.lock 2>/dev/null || git add Cargo.toml
git commit -m "Bump version to $new" >/dev/null
git tag -a "$tag" -m "Release $new"

echo "bump-version: $current -> $new"
echo "  committed: $(git rev-parse --short HEAD) \"Bump version to $new\""
echo "  tagged:    $tag"
echo "  push with: git push origin HEAD $tag"
