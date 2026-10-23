#!/usr/bin/env bash
# plugged.host: formats the tree and runs the checks a change must pass, on the build host (never on
# the panel box), and copies the files rustfmt rewrote back.
#
#   plugged-host/check.sh               # fmt clippy test
#   plugged-host/check.sh clippy test   # only some: fmt (cargo fmt, files copied back),
#                                       # clippy (cargo clippy -p wings-rs), test (cargo test -p wings-rs)
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BUILD_HOST=${BUILD_HOST:-plugged-build}
BUILD_DIR=${BUILD_DIR:-/root/wings}
CHECKS=${*:-fmt clippy test}

echo "==> copying the tree to $BUILD_HOST:$BUILD_DIR"
rsync -a --delete --exclude target --exclude application/bins --exclude application/seccomp.min.json "$ROOT/" "$BUILD_HOST:$BUILD_DIR/"

status=0
ssh "$BUILD_HOST" "set -uo pipefail
  renice -n 15 -p \$\$ >/dev/null; ionice -c 3 -p \$\$
  . \$HOME/.cargo/env
  cd '$BUILD_DIR'
  # build.rs writes seccomp.min.json, only when seccomp.json looks changed
  [ -f application/seccomp.min.json ] || touch application/seccomp.json
  export RUSTFLAGS='-C link-arg=-fuse-ld=mold'
  failed=''
  run() { name=\$1; shift; echo \"==> \$name\"; if ! \"\$@\"; then failed=\"\$failed \$name\"; fi; }
  : > /tmp/wings-formatted
  for check in $CHECKS; do
    case \$check in
      fmt) marker=\$(mktemp); sleep 1; run fmt cargo fmt --all
           find . -type f -name '*.rs' -newer \"\$marker\" -not -path './target/*' -not -path './.git/*' | sed 's|^\./||' > /tmp/wings-formatted
           rm -f \"\$marker\" ;;
      clippy) run clippy cargo clippy --profile release-fast -p wings-rs ;;
      # The inotify tests run on their own: the other tests' mock states each hold an inotify
      # instance, and in one parallel run they use up the build host's per-user limit (128).
      test) run test cargo test --profile release-fast -p wings-rs -- --skip inotify
            run test-inotify cargo test --profile release-fast -p wings-rs -- inotify ;;
      *) echo \"unknown check \$check\"; failed=\"\$failed \$check\" ;;
    esac
  done
  if [ -n \"\$failed\" ]; then echo \"==> failed:\$failed\"; exit 1; fi
  echo '==> all passed'" || status=$?

rsync -a --files-from=<(ssh "$BUILD_HOST" cat /tmp/wings-formatted) "$BUILD_HOST:$BUILD_DIR/" "$ROOT/"
ssh "$BUILD_HOST" cat /tmp/wings-formatted | sed 's/^/formatted: /'
exit $status
