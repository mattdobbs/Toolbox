#!/usr/bin/env bash
# Starts or stops the local test servers the fixture points at:
#   SSH + SFTP echo server on 127.0.0.1:2222 (demo / secret; any public key)
#   IronRDP test server   on 127.0.0.1:3390 (demo / secret)
#
#   tools/e2e/servers.sh start [fixture-dir] [--exit-after <secs>]
#   tools/e2e/servers.sh stop
# --exit-after makes shells log out on their own (automatic pane close).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CORE="$ROOT/core"
export PATH="$HOME/.cargo/bin:$PATH"
unset DEVELOPER_DIR

case "${1:-}" in
  start)
    shift
    DIR="$ROOT/build/e2e"
    if [[ $# -gt 0 && "$1" != --* ]]; then DIR="$1"; shift; fi
    [[ -f "$DIR/host_key" ]] || { echo "no fixture at $DIR; run tools/e2e/make-fixture.sh first" >&2; exit 1; }
    (cd "$CORE" && cargo build -q --examples --features test-support)
    BIN="$CORE/target/debug/examples"
    pkill -f "examples/echo_server" 2>/dev/null || true
    pkill -f "examples/rdp_server" 2>/dev/null || true
    nohup "$BIN/echo_server" --key "$DIR/host_key" --port 2222 "$@" > "$DIR/echo_server.log" 2>&1 &
    nohup "$BIN/rdp_server" > "$DIR/rdp_server.log" 2>&1 &
    for _ in $(seq 1 30); do
      nc -z -w 1 127.0.0.1 2222 2>/dev/null && nc -z -w 1 127.0.0.1 3390 2>/dev/null && break
      sleep 1
    done
    echo "ssh echo: $(nc -z -w 1 127.0.0.1 2222 2>/dev/null && echo up || echo DOWN)   rdp: $(nc -z -w 1 127.0.0.1 3390 2>/dev/null && echo up || echo DOWN)"
    ;;
  stop)
    pkill -f "examples/echo_server" 2>/dev/null || true
    pkill -f "examples/rdp_server" 2>/dev/null || true
    echo "test servers stopped"
    ;;
  *)
    echo "usage: $0 start [fixture-dir] [--exit-after <secs>] | stop" >&2
    exit 2
    ;;
esac
