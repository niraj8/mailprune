#!/usr/bin/env bash
# Render Formula/mailprune.rb into niraj8/homebrew-tap for a released tag.
#
#   scripts/publish-tap.sh          # uses the version in Cargo.toml
#   scripts/publish-tap.sh v0.2.1   # explicit tag
#   DRY_RUN=1 scripts/publish-tap.sh
#
# Expects the release to already exist with all three tarballs uploaded
# (that is what .github/workflows/release.yml does on a v* tag push).
set -euo pipefail

REPO=niraj8/mailprune
TAP=niraj8/homebrew-tap
TARGETS=(aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu)

cd "$(dirname "$0")/.."

TAG=${1:-v$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}
VERSION=${TAG#v}
[[ -n $VERSION ]] || { echo "could not determine version" >&2; exit 1; }

echo "publishing $TAG to $TAP"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# --- hashes -----------------------------------------------------------------
# Hash the published artifacts, never a local build: what brew downloads is
# what must be signed for.
have=$(gh release view "$TAG" -R "$REPO" --json assets --jq '.assets[].name')
for t in "${TARGETS[@]}"; do
  grep -qx "mailprune-$TAG-$t.tar.gz" <<<"$have" \
    || { echo "release $TAG is missing mailprune-$TAG-$t.tar.gz" >&2; exit 1; }
done

gh release download "$TAG" -R "$REPO" -p '*.tar.gz' -D "$work/assets"

declare -A sha
for t in "${TARGETS[@]}"; do
  sha[$t]=$(shasum -a 256 "$work/assets/mailprune-$TAG-$t.tar.gz" | cut -d' ' -f1)
  echo "  $t  ${sha[$t]}"
done

# --- formula ----------------------------------------------------------------
dl=https://github.com/$REPO/releases/download/$TAG

gh repo clone "$TAP" "$work/tap" -- --depth 1 --quiet
mkdir -p "$work/tap/Formula"

cat >"$work/tap/Formula/mailprune.rb" <<EOF
class Mailprune < Formula
  desc "Email triage TUI - stack inbox by sender, bulk trash/archive/unsubscribe"
  homepage "https://github.com/$REPO"
  version "$VERSION"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "$dl/mailprune-$TAG-aarch64-apple-darwin.tar.gz"
      sha256 "${sha[aarch64-apple-darwin]}"
    else
      url "$dl/mailprune-$TAG-x86_64-apple-darwin.tar.gz"
      sha256 "${sha[x86_64-apple-darwin]}"
    end
  end

  on_linux do
    url "$dl/mailprune-$TAG-x86_64-unknown-linux-gnu.tar.gz"
    sha256 "${sha[x86_64-unknown-linux-gnu]}"
  end

  def install
    bin.install "mailprune"
  end

  test do
    assert_match "mailprune", shell_output("#{bin}/mailprune --help")
  end
end
EOF

git -C "$work/tap" add Formula/mailprune.rb
if git -C "$work/tap" diff --cached --quiet; then
  echo "formula already at $TAG, nothing to push"
  exit 0
fi

git -C "$work/tap" --no-pager diff --cached

if [[ -n ${DRY_RUN:-} ]]; then
  echo "DRY_RUN set, not pushing"
  exit 0
fi

git -C "$work/tap" commit -q -m "mailprune $VERSION"
git -C "$work/tap" push -q
echo "pushed mailprune $VERSION to $TAP"
