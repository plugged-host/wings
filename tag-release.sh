#!/bin/bash
# plugged.host: tags a Wings release, v{VERSION}, and pushes the tag. The Release workflow builds
# the binaries and images and leaves a draft GitHub release to publish.
#
#   ./tag-release.sh 3.1.0
#
# VERSION at the repository root must already say 3.1.0: it is the release every build reports.
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "Usage: $0 <version>"
    echo "Example stable: $0 3.1.0"
    echo "Example pre-release: $0 3.1.0-pre.1"
    exit 1
fi

VERSION=$1
ROOT=$(cd "$(dirname "$0")" && pwd)

REGEX="^([0-9]+\.[0-9]+\.[0-9]+|[0-9]+\.[0-9]+\.[0-9]+-pre\.[0-9]+)$"

if [[ ! $VERSION =~ $REGEX ]]; then
    echo "Error: Invalid version format '$VERSION'."
    echo "Allowed formats are strict SemVer:"
    echo "  - Stable:      x.x.x         (e.g., 3.1.0)"
    echo "  - Pre-release: x.x.x-pre.x   (e.g., 3.1.0-pre.1)"
    exit 1
fi

FILE_VERSION=$(tr -d '[:space:]' < "$ROOT/VERSION")
if [ "$FILE_VERSION" != "$VERSION" ]; then
    echo "Error: VERSION says '$FILE_VERSION', not '$VERSION'. Bump and commit it first." >&2
    exit 1
fi

if [ -n "$(git -C "$ROOT" status --porcelain)" ]; then
    echo "Error: the working tree has uncommitted changes. Commit and push them first." >&2
    exit 1
fi

TAG_NAME="v$VERSION"

echo "Applying tag: $TAG_NAME..."

if git -C "$ROOT" rev-parse -q --verify "refs/tags/$TAG_NAME" >/dev/null; then
    echo "Error: tag '$TAG_NAME' already exists locally." >&2
    exit 1
fi

if git -C "$ROOT" tag -a "$TAG_NAME" -m "plugged.host Wings $VERSION"; then
    echo "Tag created successfully."
else
    echo "Error: Failed to create tag '$TAG_NAME'." >&2
    exit 1
fi

echo "Pushing $TAG_NAME to origin..."

if git -C "$ROOT" push origin "$TAG_NAME"; then
    echo "Success! Pushed $TAG_NAME to origin."
    echo "Your GitHub Actions release pipeline should trigger shortly."
    echo "Publish the draft release when it appears: gh release edit $TAG_NAME --draft=false --latest"
else
    echo "Error: Failed to push tag. Check your network or git permissions." >&2
    exit 1
fi
