#!/usr/bin/env bash
# Builds Wings on the build host and fetches the binary.
#
#   plugged-host/build.sh            # -> target/plugged-host/wings-rs
#
# The build host is an ssh alias (BUILD_HOST, default plugged-build, the Ryzen box that builds the
# panel too) with Rust; never build on the panel box. The repository is copied over with its .git, so
# the binary knows its commit (`wings-rs version` -> 1.2.5:<commit>@main). Commit before building
# anything that gets deployed.
#
# By default it builds inside a Debian 12 container (BUILD_IMAGE), so the binary needs glibc 2.34 at
# most and runs on every node, the Debian 12 ones (glibc 2.36) included; a build on the host itself
# (Debian 13) would need 2.39. BUILD_IMAGE= builds on the host directly.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BUILD_HOST=${BUILD_HOST:-plugged-build}
BUILD_IMAGE=${BUILD_IMAGE-rust:1.99-bookworm}
# a container build keeps its own target/ (never mixed with the host's Debian 13 one)
BUILD_DIR=${BUILD_DIR:-$([ -n "$BUILD_IMAGE" ] && echo /root/wings-bookworm || echo /root/wings)}
PROFILE=${PROFILE:-release-fast}
OUT="$ROOT/target/plugged-host"

if [ -n "$(git -C "$ROOT" status --porcelain --untracked-files=no)" ]; then
  echo "warning: uncommitted changes, the binary will name $(git -C "$ROOT" rev-parse --short HEAD) anyway" >&2
fi

echo "==> copying the tree to $BUILD_HOST:$BUILD_DIR"
rsync -a --delete --exclude target --exclude application/bins --exclude application/seccomp.min.json "$ROOT/" "$BUILD_HOST:$BUILD_DIR/"

echo "==> building (profile $PROFILE${BUILD_IMAGE:+, in $BUILD_IMAGE})"
if [ -n "$BUILD_IMAGE" ]; then
  # target/ and the cargo registry live on the host, so later builds are incremental.
  ssh "$BUILD_HOST" "set -euo pipefail
    cd '$BUILD_DIR'
    [ -f application/seccomp.min.json ] || touch application/seccomp.json
    mkdir -p \$HOME/.cache/wings-build-cargo
    nice -n 15 docker run --rm --cpus 6 --memory 10g -v '$BUILD_DIR':/src -v \$HOME/.cache/wings-build-cargo:/usr/local/cargo/registry \\
      -w /src '$BUILD_IMAGE' bash -c 'apt-get update -qq >/dev/null && apt-get install -y -qq clang cmake pkg-config protobuf-compiler libssl-dev >/dev/null 2>&1; cargo build --profile $PROFILE -p wings-rs'"
else
  ssh "$BUILD_HOST" "set -euo pipefail
    # The build host is also the prod panel box (since 2026-10-17): builds yield CPU and disk to it.
    renice -n 15 -p \$\$ >/dev/null; ionice -c 3 -p \$\$
    . \$HOME/.cargo/env
    cd '$BUILD_DIR'
    # build.rs writes seccomp.min.json, only when seccomp.json looks changed
    [ -f application/seccomp.min.json ] || touch application/seccomp.json
    RUSTFLAGS='-C link-arg=-fuse-ld=mold' cargo build --profile '$PROFILE' -p wings-rs"
fi

mkdir -p "$OUT"
rsync -a "$BUILD_HOST:$BUILD_DIR/target/$PROFILE/wings-rs" "$OUT/wings-rs.new"
mv -f "$OUT/wings-rs.new" "$OUT/wings-rs"

echo "==> built $("$OUT/wings-rs" --config /nonexistent version 2>/dev/null | head -1 || true), needs glibc $(grep -ao 'GLIBC_2\.[0-9]*' "$OUT/wings-rs" | sort -Vu | tail -1 | cut -d_ -f2)"
