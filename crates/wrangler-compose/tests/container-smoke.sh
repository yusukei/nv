#!/usr/bin/env bash
# Local acceptance smoke test of containers (AC-020, local part): runs
# fixture-containers with `--env local` in two folders at once and checks it
# from outside. Needs: Docker running, nv and wrangler-compose built side by
# side in $BIN, curl, python3.
set -euo pipefail
export NV_LANG=en # the checks read the English messages
BIN=${BIN:?set BIN to the folder holding nv and wrangler-compose}
WC="$BIN/wrangler-compose"
SRC="$(cd "$(dirname "$0")/fixture-containers" && pwd)"
TOP=$(mktemp -d)
A="$TOP/a" B="$TOP/b" C="$TOP/c"

setup() { # <folder> <port> <answer>
  mkdir -p "$1" && cp -R "$SRC/." "$1/"
  cp "$(dirname "$0")/fixture/.gitignore" "$1/.gitignore"
  (cd "$1" && git init -q . && echo "API_PORT=$2" > .env.local && echo "$3" > box/answer.txt)
  (cd "$1" && "$BIN/nv" run npm install --no-audit --no-fund >/dev/null)
}
down_all() { for d in "$A" "$B" "$C"; do [[ -d "$d" ]] && (cd "$d" && "$WC" --env local down >/dev/null 2>&1 || true); done; }
trap down_all EXIT
fail() { echo "FAIL: $*"; for d in "$A" "$B" "$C"; do [[ -f "$d/up.out" ]] && { echo "--- $d/up.out"; tail -30 "$d/up.out"; }; done; down_all; echo "FAIL: $*"; exit 1; }
start() { # <folder> [env...]: runs up in the background; its pid goes to up.pid
  local dir=$1; shift
  ( cd "$dir" && exec env "$@" "$WC" --env local up > up.out 2>&1 ) &
  echo $! > "$dir/up.pid"
  ready "$dir" 1
}
ready() { # <folder> <n>: wait until up has said it runs n times
  for _ in $(seq 1 300); do (( $(grep -c 'running locally' "$1/up.out" || true) >= $2 )) && return 0; sleep 1; done
  fail "up did not start in $1"
}
ids() { docker ps -aq --filter "name=^workerd-$1" | sort | tr '\n' ' '; } # <prefix>
gone() { # <ids>: none of these containers is left
  for i in $1; do docker inspect "$i" >/dev/null 2>&1 && return 1; done; return 0
}
ask() { curl -fsS -m 120 "localhost:$1$2"; }
# The project's local Worker name prefix (`boxes--<mark>--`), from its registry.
prefix() { basename "$(ls "$1/.wrangler/compose/local/registry/"boxes--*--api)" | sed 's/api$//'; }
# down --purge asks to retype the environment name on a terminal.
purge() {
  (cd "$1" && python3 - "$WC" <<'PY'
import os, pty, sys
pid, fd = pty.fork()
if pid == 0:
    os.execvp(sys.argv[1], [sys.argv[1], "--env", "local", "down", "--purge"])
out = b""
while True:
    try:
        chunk = os.read(fd, 1024)
    except OSError:
        break
    if not chunk:
        break
    out += chunk
    if b"(local)" in out and b"sent" not in out:
        os.write(fd, b"local\n")
        out += b"sent"
_, status = os.waitpid(pid, 0)
sys.stdout.write(out.decode(errors="replace"))
sys.exit(os.waitstatus_to_exitcode(status))
PY
  ) > "$1/purge.out" 2>&1 || fail "down --purge: $(cat "$1/purge.out")"
}

setup "$A" 18790 v1
setup "$B" 18791 other

# Without Docker, up stops before anything runs (REQ-020).
if (cd "$A" && WRANGLER_DOCKER_BIN=/usr/bin/false "$WC" --env local up > nodocker.out 2>&1); then fail "up without Docker"; fi
grep -q 'Docker' "$A/nodocker.out" || fail "no-Docker message: $(cat "$A/nodocker.out")"
! grep -q 'running locally' "$A/nodocker.out" || fail "started without Docker"

tags() { docker images --format '{{.Repository}}:{{.Tag}}' 'cloudflare-dev/*' | sort | tr '\n' ' '; }
baseline=$(tags)
# --dry-run builds nothing.
before=$(docker images -q 'cloudflare-dev/*' | sort | tr '\n' ' ')
(cd "$A" && "$WC" --env local --dry-run up >/dev/null 2>&1) || fail "dry-run up"
[[ "$(docker images -q 'cloudflare-dev/*' | sort | tr '\n' ' ')" == "$before" ]] || fail "dry-run built an image"

# Both projects at once: each reaches its own container (definition 3).
start "$A"
grep -q 'instance_type' "$A/up.out" || fail "no notice about limits that do not apply locally"
# ps shows each container; one starts with its first request (REQ-012).
(cd "$A" && "$WC" --env local ps) > "$A/ps.out" 2>&1 || fail "ps"
grep -Eq '^api\.Pinger +container +idle +box/Dockerfile' "$A/ps.out" || fail "ps before a request: $(cat "$A/ps.out")"
start "$B"
[[ $(ask 18790 /ping) == v1 ]] || fail "A ping: $(ask 18790 /ping)"
[[ $(ask 18791 /ping) == other ]] || fail "B ping: $(ask 18791 /ping)"
[[ -n $(ask 18790 /echo) ]] || fail "registry image"
(cd "$A" && "$WC" --env local ps) > "$A/ps.out" 2>&1 || fail "ps"
grep -Eq '^api\.Pinger +container +running 1 ' "$A/ps.out" || fail "ps after a request: $(cat "$A/ps.out")"
grep -Eq '^api\.Echo +container +running 1 +docker\.io/library/busybox:1\.36' "$A/ps.out" || fail "ps of a registry image: $(cat "$A/ps.out")"
PA=$(prefix "$A") PB=$(prefix "$B")
[[ "$PA" != "$PB" ]] || fail "both folders got the same prefix $PA"

# A change to what the image is built from rebuilds it while up runs.
echo v2 > "$A/box/answer.txt"
for _ in $(seq 1 300); do [[ $(curl -fsS -m 20 localhost:18790/ping 2>/dev/null || true) == v2 ]] && break; sleep 1; done
[[ $(ask 18790 /ping) == v2 ]] || fail "rebuild after a Dockerfile input change"
[[ $(ask 18791 /ping) == other ]] || fail "B after A rebuilt"

# Stopping A leaves none of A's containers, and B keeps running.
(cd "$A" && "$WC" --env local down) || fail "down A"
sleep 3
docker ps -a --format '{{.Names}}' | grep -q "^workerd-$PA" && fail "A's containers left: $(docker ps -a --format '{{.Names}}' | grep "^workerd-$PA")"
docker ps --format '{{.Names}}' | grep -q "^workerd-$PB" || fail "B's containers were stopped"
[[ $(ask 18791 /ping) == other ]] || fail "B after A stopped"

# A's images stay after down, and go with down --purge; B's stay.
IA=$(python3 -c 'import json,sys; print(" ".join(json.load(open(sys.argv[1]))))' "$A/.wrangler/compose/local/container-images.json")
[[ -n "$IA" ]] || fail "A recorded no images"
for i in $IA; do docker image inspect "$i" >/dev/null 2>&1 || fail "A's image $i gone after down"; done
purge "$A"
for i in $IA; do docker image inspect "$i" >/dev/null 2>&1 && fail "A's image $i left after down --purge"; done
(cd "$B" && "$WC" --env local down) || fail "down B"
IB=$(python3 -c 'import json,sys; print(" ".join(json.load(open(sys.argv[1]))))' "$B/.wrangler/compose/local/container-images.json")
for i in $IB; do docker image inspect "$i" >/dev/null 2>&1 || fail "B's image $i deleted by A's purge"; done
purge "$B"
# Nothing the test's runs made is left: built images and registry aliases.
[[ "$(tags)" == "$baseline" ]] || fail "images left: $(tags)"

# A compose file without containers needs no Docker.
setup "$C" 18792 unused
python3 - "$C/wrangler-compose.yaml" <<'PY'
import re, sys
p = sys.argv[1]
t = open(p).read()
t = re.sub(r"\n    containers:\n(      .*\n)+", "\n", t + "\n")
open(p, "w").write(t)
PY
start "$C" WRANGLER_DOCKER_BIN=/usr/bin/false
[[ $(ask 18792 /) == api ]] || fail "a project without containers"
(cd "$C" && "$WC" --env local down) || fail "down C"
# Ctrl+C (an interrupt to wrangler-compose) leaves none of A's containers.
start "$A"
[[ $(ask 18790 /ping) == v2 ]] || fail "A after purge"
kill -INT "$(cat "$A/up.pid")"
for _ in $(seq 1 60); do kill -0 "$(cat "$A/up.pid")" 2>/dev/null || break; sleep 1; done
kill -0 "$(cat "$A/up.pid")" 2>/dev/null && fail "up did not stop on Ctrl+C"
[[ -z "$(ids "$PA")" ]] || fail "containers left after Ctrl+C: $(ids "$PA")"

# A changed compose file restarts the runtime: the old containers go, and
# the container answers again.
start "$A"
[[ $(ask 18790 /ping) == v2 ]] || fail "A before the compose change"
old=$(ids "$PA")
[[ -n "$old" ]] || fail "no container to watch"
python3 - "$A/wrangler-compose.yaml" <<'PY'
import sys
p = sys.argv[1]
t = open(p).read().replace("        max_instances: 1\n", "        max_instances: 2\n")
open(p, "w").write(t)
PY
ready "$A" 2
gone "$old" || fail "containers of the run before the compose change are left: $old"
[[ $(ask 18790 /ping) == v2 ]] || fail "A after the compose change"

# After a crash (the tool and Wrangler killed), the containers stay; the
# next up clears them away before it starts.
old=$(ids "$PA")
WPID=$(pgrep -P "$(cat "$A/up.pid")" | head -1)
[[ -n "$WPID" ]] || fail "no Wrangler process under up"
kill -9 "$(cat "$A/up.pid")"
kill -9 -- "-$(ps -o pgid= -p "$WPID" | tr -d ' ')" 2>/dev/null || true
sleep 3
[[ -n "$old" ]] && ! gone "$old" || fail "the crash left no container to clear (test setup)"
start "$A"
gone "$old" || fail "containers left by the crash were not cleared: $old"
[[ $(ask 18790 /ping) == v2 ]] || fail "A after the crash"
(cd "$A" && "$WC" --env local down) || fail "down A at the end"
[[ -z "$(ids "$PA")" ]] || fail "containers left at the end: $(ids "$PA")"
purge "$A"
[[ "$(tags)" == "$baseline" ]] || fail "images left at the end: $(tags)"
echo "container smoke test passed"
