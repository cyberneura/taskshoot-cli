#!/usr/bin/env bash
#
# Check that both plugin manifests declare the version in Cargo.toml.
#
# A stale plugin manifest would ship a marketplace entry claiming the previous
# release's contents, and tag-on-version-change.yml refuses to tag such a
# version -- which, checked only after the merge, left v0.10.0 on main with no
# tag and no release. So the one copy of this check lives here, and it runs on
# every pull request (test.yml), before the tag (tag-on-version-change.yml) and
# from ./release.sh.
#
#   ./check-manifests.sh            # compare against Cargo.toml
#   ./check-manifests.sh 0.10.0     # compare against the given version

set -euo pipefail

cd "$(dirname "$0")"

command -v jq >/dev/null || { echo "error: jq is required" >&2; exit 1; }

# cargo metadata rather than grepping Cargo.toml, so a version string that
# appears elsewhere in the file cannot be picked up by mistake.
VERSION=${1:-$(cargo metadata --no-deps --format-version 1 \
  | jq -r '.packages[] | select(.name == "taskshoot") | .version')}
if [ -z "$VERSION" ]; then
  echo "error: could not read the version out of Cargo.toml" >&2
  exit 1
fi

# Each field is addressed explicitly, so a version added elsewhere in a
# manifest later on cannot be mistaken for the one being checked.
status=0
for spec in \
  '.claude-plugin/marketplace.json|.plugins[] | select(.name == "taskshoot") | .version' \
  '.claude-plugin/plugin.json|.version'; do
  manifest=${spec%%|*}
  filter=${spec#*|}
  found=$(jq -r "$filter // empty" "$manifest")
  if [ "$found" != "$VERSION" ]; then
    # The ::error:: prefix is an annotation on GitHub Actions and plain text
    # anywhere else.
    echo "::error::${manifest} declares '${found:-nothing}' at ${filter}, but Cargo.toml declares ${VERSION}" >&2
    status=1
  fi
done
[ "$status" -eq 0 ] && echo "both plugin manifests declare ${VERSION}"
exit "$status"
