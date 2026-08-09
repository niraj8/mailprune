#!/usr/bin/env bash
# Cut a release: bump the version, sync the lock, commit, tag, push.
# Pushing the tag is what fires .github/workflows/release.yml.
#
#   scripts/release.sh 0.2.2
#   DRY_RUN=1 scripts/release.sh 0.2.2
#
# Either the release completes or Cargo.toml/Cargo.lock are left as found.
#
# Afterwards, once the workflow has uploaded the tarballs:
#   make publish-tap
set -euo pipefail

cd "$(dirname "$0")/.."

VERSION=${1:-}
[[ $VERSION =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
  || { echo "usage: scripts/release.sh X.Y.Z" >&2; exit 1; }
TAG=v$VERSION

# A tag is immutable once pushed; never move one.
git rev-parse -q --verify "refs/tags/$TAG" >/dev/null \
  && { echo "tag $TAG already exists" >&2; exit 1; }

# Checked before the bump, ignoring the two files this script owns, so an
# already-edited Cargo.toml is fine but unrelated work in progress is not.
dirty=$(git status --porcelain -- ':!Cargo.toml' ':!Cargo.lock')
[[ -z $dirty ]] || { echo "uncommitted changes outside Cargo.toml/Cargo.lock:" >&2
                     echo "$dirty" >&2; exit 1; }

# Undo the bump on every path except a completed release, so a failed test
# or a dry run never leaves a half-bumped tree behind.
snapshot=$(mktemp -d)
cp Cargo.toml Cargo.lock "$snapshot/"
released=
trap '[[ -n $released ]] || cp "$snapshot/Cargo.toml" "$snapshot/Cargo.lock" .
      rm -rf "$snapshot"' EXIT

current=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
if [[ $current != "$VERSION" ]]; then
  echo "Cargo.toml $current -> $VERSION"
  sed -i '' "0,/^version = \"$current\"/s//version = \"$VERSION\"/" Cargo.toml
fi

# Refreshes the mailprune entry in Cargo.lock, which carries its own copy
# of the version and would otherwise drift.
cargo build --release
cargo test

git --no-pager diff -- Cargo.toml Cargo.lock

if [[ -n ${DRY_RUN:-} ]]; then
  echo "DRY_RUN set, reverting bump"
  exit 0
fi

read -rp "tag and push $TAG? this publishes a release [y/N] " reply
[[ $reply == [yY] ]] || { echo "aborted, reverting bump"; exit 1; }

git commit -q -m "$TAG" -- Cargo.toml Cargo.lock
git tag "$TAG"
git push -q --follow-tags
released=1
echo "pushed $TAG; watch: gh run watch"
