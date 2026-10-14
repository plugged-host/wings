#!/usr/bin/env bash
# Installs a built Wings binary on a node and restarts it, rolling back if it does not come up.
#
#   plugged-host/deploy.sh <ssh host> [binary]      # default target/plugged-host/wings-rs
#
# The node runs it as /usr/local/bin/calagopus-wings (unit calagopus-wings, plugged-host/
# calagopus-wings.service); the previous binary stays next to it as calagopus-wings.previous.
# Server containers keep running across the restart (KillMode=process).
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
NODE=${1:?usage: deploy.sh <ssh host> [binary]}
BINARY=${2:-$ROOT/target/plugged-host/wings-rs}

echo "==> $("$BINARY" --config /nonexistent version 2>/dev/null | head -1) -> $NODE"
rsync -a "$BINARY" "$NODE:/usr/local/bin/calagopus-wings.new"

ssh "$NODE" 'set -euo pipefail
  BIN=/usr/local/bin/calagopus-wings
  chmod 755 $BIN.new
  [ -e $BIN ] && cp -p $BIN $BIN.previous
  mv -f $BIN.new $BIN
  systemctl restart calagopus-wings

  # The API answers 401 without a token once it is listening.
  port=$(awk "/^api:/{a=1} a&&/^  port:/{print \$2; exit}" /etc/calagopus-wings/config.yml)
  for _ in $(seq 1 30); do
    code=$(curl -sk -o /dev/null -w "%{http_code}" "https://127.0.0.1:${port:-8443}/api/system" || true)
    if [ "$code" = 401 ]; then
      echo "==> up ($($BIN --config /etc/calagopus-wings/config.yml version | head -1))"
      exit 0
    fi
    sleep 2
  done

  echo "==> did not come up, rolling back" >&2
  journalctl -u calagopus-wings -n 30 --no-pager >&2 || true
  if [ -e $BIN.previous ]; then
    cp -p $BIN.previous $BIN
    systemctl restart calagopus-wings
  fi
  exit 1'
