#!/usr/bin/env bash
#
# Check that the version currently declared in Cargo.toml is fit to release.
#
# The release itself is not started from here any more. Changing the version on
# main is what starts it: .github/workflows/tag-on-version-change.yml puts the
# tag on, the cargo-dist workflow builds every target and creates the GitHub
# Release, .github/workflows/publish-crates.yml publishes to crates.io, and
# cyberneura/homebrew-tap picks the new release up within the hour.
#
# The same checks run in CI, in the `check` job of tag-on-version-change.yml,
# before the tag is created -- that is the copy that cannot be skipped. This one
# is for running them on the bump branch, where a failure costs a fix rather
# than a burnt version number. Nothing it does can be undone, because it changes
# nothing.
#
#   ./release.sh                   # run every check
#   ./release.sh --skip-crates-io  # skip the slow `cargo publish --dry-run`

set -euo pipefail

cd "$(dirname "$0")"

SKIP_CRATES_IO=false

for arg in "$@"; do
  case "$arg" in
    --skip-crates-io) SKIP_CRATES_IO=true ;;
    -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

step() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }
fail() { printf '\033[31merror:\033[0m %s\n' "$1" >&2; exit 1; }

command -v jq >/dev/null || fail "jq is required"

# --- version ---------------------------------------------------------------
# cargo metadata rather than grepping Cargo.toml, so a version string that
# appears elsewhere in the file cannot be picked up by mistake.
VERSION=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version')
TAG="v${VERSION}"
step "Checking ${TAG}"

# Every manifest that carries this version has to agree. A stale plugin manifest
# would ship a marketplace entry claiming the previous release's contents. Each
# field is addressed explicitly, so a version added elsewhere in a manifest
# later on cannot be mistaken for the one being checked.
check_manifest() {
  local manifest=$1 filter=$2 found
  found=$(jq -r "$filter // empty" "$manifest")
  [ "$found" = "$VERSION" ] ||
    fail "$manifest declares '${found:-nothing}' at $filter, but Cargo.toml declares $VERSION"
}
check_manifest .claude-plugin/marketplace.json '.plugins[] | select(.name == "taskshoot") | .version'
check_manifest .claude-plugin/plugin.json '.version'
lock_version=$(cargo metadata --format-version 1 --locked >/dev/null 2>&1 && echo ok || echo stale)
[ "$lock_version" = ok ] || fail "Cargo.lock is out of date; run 'cargo build' and commit it"
echo "Cargo.toml, both plugin manifests and Cargo.lock all agree on ${VERSION}"

# --- repository state ------------------------------------------------------
# The branch is not checked: this runs on the version bump before it is merged,
# which is the point at which the answer is still worth having. What is checked
# is that the version is free, since merging one that is not leaves main
# carrying a version that will never be released.
step "Checking the repository"
[ -z "$(git status --porcelain)" ] || fail "the working tree has uncommitted changes"

git fetch --quiet origin
git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null &&
  fail "tag ${TAG} already exists locally"
[ -z "$(git ls-remote --tags origin "refs/tags/${TAG}")" ] ||
  fail "tag ${TAG} already exists on origin; bump the version"
echo "the working tree is clean and ${TAG} is unused"

# --- checks ----------------------------------------------------------------
# The same checks CI would run, run here because a release that fails after the
# tag is on cannot be retried under the same version.
step "Running checks"
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
./target/release/taskshoot --version | grep -qx "taskshoot ${VERSION}" ||
  fail "the built binary does not report ${VERSION}"

if [ "$SKIP_CRATES_IO" = false ]; then
  step "Checking the crates.io package"
  cargo publish --dry-run
fi

step "Ready"
cat <<EOF
${TAG} is fit to release. Merging this version into main is what releases it:

  tag-on-version-change.yml  puts v${VERSION} on the merge commit
  release.yml (cargo-dist)   builds every target and creates the GitHub Release
  publish-crates.yml         publishes ${VERSION} to crates.io
  cyberneura/homebrew-tap    picks the release up within the hour

Workflows  https://github.com/cyberneura/taskshoot-cli/actions
EOF
