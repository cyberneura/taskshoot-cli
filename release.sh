#!/usr/bin/env bash
#
# Release the version currently declared in Cargo.toml.
#
# Pushing the tag is what starts the release: .github/workflows/release.yml
# reacts to it, builds every target with cargo-dist, creates the GitHub Release
# and updates the Homebrew tap. This script runs the checks that are awkward to
# undo afterwards, then pushes the tag and publishes to crates.io.
#
# To release a new version, edit the version in Cargo.toml (and keep the plugin
# manifests in step), commit that on main, then run this.
#
#   ./release.sh              # release the version in Cargo.toml
#   ./release.sh --dry-run    # run every check, change nothing
#   ./release.sh --yes        # skip the confirmation prompt
#   ./release.sh --skip-crates-io

set -euo pipefail

cd "$(dirname "$0")"

DRY_RUN=false
ASSUME_YES=false
SKIP_CRATES_IO=false

for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=true ;;
    -y|--yes) ASSUME_YES=true ;;
    --skip-crates-io) SKIP_CRATES_IO=true ;;
    -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

step() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }
fail() { printf '\033[31merror:\033[0m %s\n' "$1" >&2; exit 1; }

command -v jq >/dev/null || fail "jq is required"
command -v gh >/dev/null || fail "gh is required"

# --- version ---------------------------------------------------------------
# cargo metadata rather than grepping Cargo.toml, so a version string that
# appears elsewhere in the file cannot be picked up by mistake.
VERSION=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version')
TAG="v${VERSION}"
step "Releasing ${TAG}"

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
check_manifest plugins/taskshoot/.claude-plugin/plugin.json '.version'
lock_version=$(cargo metadata --format-version 1 --locked >/dev/null 2>&1 && echo ok || echo stale)
[ "$lock_version" = ok ] || fail "Cargo.lock is out of date; run 'cargo build' and commit it"
echo "Cargo.toml, both plugin manifests and Cargo.lock all agree on ${VERSION}"

# --- repository state ------------------------------------------------------
step "Checking the repository"
branch=$(git rev-parse --abbrev-ref HEAD)
[ "$branch" = main ] || fail "on branch '$branch'; releases are cut from main"
[ -z "$(git status --porcelain)" ] || fail "the working tree has uncommitted changes"

git fetch --quiet origin main
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] ||
  fail "main and origin/main differ; pull or push first"

git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null &&
  fail "tag ${TAG} already exists locally"
[ -z "$(git ls-remote --tags origin "refs/tags/${TAG}")" ] ||
  fail "tag ${TAG} already exists on origin"
echo "main is clean and in sync, and ${TAG} is unused"

# --- checks ----------------------------------------------------------------
# These run before anything is published because a pushed tag and a crates.io
# version are both awkward to walk back.
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

if [ "$DRY_RUN" = true ]; then
  step "Dry run complete — nothing was published"
  exit 0
fi

# --- confirm ---------------------------------------------------------------
if [ "$ASSUME_YES" = false ]; then
  printf '\nPush %s and publish %s? [y/N] ' "$TAG" \
    "$([ "$SKIP_CRATES_IO" = true ] && echo "to GitHub only" || echo "to GitHub and crates.io")"
  read -r reply
  case "$reply" in [yY]*) ;; *) fail "aborted" ;; esac
fi

# --- release ---------------------------------------------------------------
# The tag goes first: a tag and a GitHub release can both be deleted, whereas a
# crates.io version can never be republished. Leave the irreversible step last,
# once everything else has succeeded.
step "Pushing ${TAG}"
git tag -a "${TAG}" -m "${TAG}"
git push origin "${TAG}"

if [ "$SKIP_CRATES_IO" = false ]; then
  step "Publishing to crates.io"
  cargo publish
fi

step "Done"
cat <<EOF
Workflow  https://github.com/cyberneura/taskshoot-cli/actions
Release   https://github.com/cyberneura/taskshoot-cli/releases/tag/${TAG}

The GitHub Release appears once the workflow finishes; it builds the binaries,
the installers and the Homebrew formula.
EOF
