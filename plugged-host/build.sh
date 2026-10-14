#!/usr/bin/env bash
# Builds Wings on the build host and fetches the binary.
#
#   plugged-host/build.sh            # -> target/plugged-host/wings-rs
#
# The build host is an ssh alias (BUILD_HOST, default plugged-build, the Ryzen box that builds the
# panel too) with Rust; never build on the panel box. The repository is copied over with its .git, so
# the binary knows its commit (`wings-rs version` -> 1.2.5:<commit>@main). Commit before building
# anything that gets deployed.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BUILD_HOST=${BUILD_HOST:-plugged-build}
BUILD_DIR=${BUILD_DIR:-/root/wings}
PROFILE=${PROFILE:-plugged-host}
OUT="$ROOT/target/plugged-host"

if [ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no)" ]; then
  echo "warning: uncommitted changes, the binary will name $(git -C "$ROOT" rev-parse --short HEAD) anyway" >&2
fi

echo "==> copying the tree to $BUILD_HOST:$BUILD_DIR"
rsync -a --delete --exclude target --exclude application/bins "$ROOT/" "$BUILD_HOST:$BUILD_DIR/"

echo "==> building (profile $PROFILE)"
ssh "$BUILD_HOST" "set -euo pipefail
  . \$HOME/.cargo/env
  cd '$BUILD_DIR'
  RUSTFLAGS='-C link-arg=-fuse-ld=mold' cargo build --profile '$PROFILE' -p wings-rs"

mkdir -p "$OUT"
rsync -a "$BUILD_HOST:$BUILD_DIR/target/$PROFILE/wings-rs" "$OUT/wings-rs.new"
mv -f "$OUT/wings-rs.new" "$OUT/wings-rs"

echo "==> built $("$OUT/wings-rs" --config /nonexistent version 2>/dev/null | head -1)"
