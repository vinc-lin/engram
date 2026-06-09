#!/usr/bin/env bash
# engram run controller (WSL2): engram + a reverse SSH tunnel to gateway-host, both in screen.
#
# engram binds loopback (ENGRAM_BIND=127.0.0.1:8088). The reverse tunnel exposes it on
# gateway-host's localhost:8088, where the litellm pass-through (/engram) reaches it — so the single
# LAN endpoint is gateway-host:4000/engram/... . No Windows, no port-proxy.
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$REPO/target/release/engram"
LOG_DIR="$HOME/engram/logs"
ENGRAM_LOG="$LOG_DIR/engram.log"
TUNNEL_LOG="$LOG_DIR/tunnel.log"
ENV_FILE="$HOME/engram/.env"   # on native ext4 — the repo is a v9fs mount (no chmod, flaky SQLite)
# Load .env early so tunnel settings (and ENGRAM_* vars) are available to this controller.
[ -f "$ENV_FILE" ] && { set -a; . "$ENV_FILE"; set +a; }
PORT=8088
# Reverse-tunnel target. Set ENGRAM_TUNNEL_HOST / ENGRAM_TUNNEL_KEY in $ENV_FILE for your host.
G3G="${ENGRAM_TUNNEL_HOST:-user@gateway.example}"
G3G_KEY="${ENGRAM_TUNNEL_KEY:-$HOME/.ssh/id_ed25519}"

up() { screen -ls 2>/dev/null | grep -q "[.]${1}[[:space:]]"; }

start_engram() {
  [ -f "$ENV_FILE" ] || { echo "missing $ENV_FILE (copy .env.example there, chmod 600)"; exit 1; }
  [ -x "$BIN" ] || { echo "missing $BIN — run: cargo build --release"; exit 1; }
  if up engram; then echo "engram: already up"; return; fi
  mkdir -p "$LOG_DIR" "$HOME/engram/data" "$HOME/engram/vault"
  screen -dmS engram bash -c "set -a && . '$ENV_FILE' && set +a && while true; do '$BIN' >> '$ENGRAM_LOG' 2>&1; echo \"[\$(date -Is)] engram exited (\$?) — restart in 2s\" >> '$ENGRAM_LOG'; sleep 2; done"
}

start_tunnel() {
  if [ -z "${ENGRAM_TUNNEL_HOST:-}" ]; then
    echo "tunnel: disabled (set ENGRAM_TUNNEL_HOST + ENGRAM_TUNNEL_KEY in $ENV_FILE to enable)"
    return
  fi
  if [ ! -f "$G3G_KEY" ]; then
    echo "tunnel: key $G3G_KEY not found — skipping (set ENGRAM_TUNNEL_KEY)"
    return
  fi
  if up engram-tunnel; then echo "tunnel: already up"; return; fi
  mkdir -p "$LOG_DIR"
  screen -dmS engram-tunnel bash -c "while true; do ssh -N -o ServerAliveInterval=30 -o ServerAliveCountMax=3 -o ExitOnForwardFailure=yes -o ConnectTimeout=10 -i '$G3G_KEY' -R 127.0.0.1:$PORT:127.0.0.1:$PORT '$G3G' >> '$TUNNEL_LOG' 2>&1; echo \"[\$(date -Is)] tunnel dropped — reconnect in 3s\" >> '$TUNNEL_LOG'; sleep 3; done"
}

case "${1:-}" in
  start)
    start_engram; start_tunnel; sleep 2
    up engram        && echo "engram: up" || { echo "engram: DOWN — tail $ENGRAM_LOG"; tail -n 15 "$ENGRAM_LOG" 2>/dev/null; }
    up engram-tunnel && echo "tunnel: up" || { echo "tunnel: DOWN — tail $TUNNEL_LOG"; tail -n 15 "$TUNNEL_LOG" 2>/dev/null; }
    ;;
  stop)
    screen -S engram -X quit 2>/dev/null || true
    screen -S engram-tunnel -X quit 2>/dev/null || true
    echo "stopped"
    ;;
  status)
    up engram        && echo "engram: up" || echo "engram: down"
    up engram-tunnel && echo "tunnel: up" || echo "tunnel: down"
    curl -sf -m3 "127.0.0.1:$PORT/healthz" >/dev/null && echo "healthz(local): ok" || echo "healthz(local): FAIL"
    ;;
  logs)
    tail -f "$ENGRAM_LOG" "$TUNNEL_LOG"
    ;;
  *)
    echo "usage: $0 {start|stop|status|logs}"; exit 1
    ;;
esac
