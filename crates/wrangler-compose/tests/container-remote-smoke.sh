#!/usr/bin/env bash
# Remote flow of containers (AC-020, remote part) against the fake Cloudflare
# API (fake-cloudflare.py) and a registry run locally with Docker, offline
# from Cloudflare: Wrangler builds the image, pushes it to "Cloudflare's
# registry" (the local one, through CLOUDFLARE_CONTAINER_REGISTRY) and makes
# the applications; the tool's own steps are checked: plan, unchanged
# second up, a changed image, a container dropped from the compose file,
# ps, down, never touching what is not this environment's. Real Cloudflare
# behavior is checked on a test account.
# Needs: Docker running, nv and wrangler-compose in $BIN, curl, python3.
set -euo pipefail
export NV_LANG=en # the checks read the English messages
BIN=${BIN:?set BIN to the folder holding nv and wrangler-compose}
HERE=$(cd "$(dirname "$0")" && pwd)
WC="$BIN/wrangler-compose"
TOP=$(mktemp -d)
P="$TOP/p"
mkdir -p "$P" && cp -R "$HERE/fixture-containers/." "$P/" && cp "$HERE/fixture/.gitignore" "$P/"
cd "$P"
git init -q .
echo 'CLOUDFLARE_ACCOUNT_ID=acc' > .env.staging
"$BIN/nv" run npm install --no-audit --no-fund >/dev/null

free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }
PORT=$(free_port) NOPLAN_PORT=$(free_port) RPORT=$(free_port)
REG="wc-smoke-registry-$$"
python3 "$HERE/fake-cloudflare.py" "$PORT" 2> "$TOP/api.log" &
FAKE=$!
FAKE_NO_CONTAINERS=1 python3 "$HERE/fake-cloudflare.py" "$NOPLAN_PORT" 2> "$TOP/api-noplan.log" &
NOPLAN=$!
docker run -d --name "$REG" -p "127.0.0.1:$RPORT:5000" registry:2 >/dev/null
cleanup() {
  kill $FAKE $NOPLAN 2>/dev/null || true
  docker rm -f "$REG" >/dev/null 2>&1 || true
  # The images Wrangler built and tagged for the deploys of this test.
  docker images --format '{{.Repository}}:{{.Tag}}' | grep -E "^(boxes--|localhost:$RPORT/)" | xargs -r docker rmi >/dev/null 2>&1 || true
}
trap cleanup EXIT
for _ in $(seq 1 100); do
  curl -s "http://127.0.0.1:$PORT/" >/dev/null && curl -s "http://127.0.0.1:$NOPLAN_PORT/" >/dev/null &&
    curl -s "http://127.0.0.1:$RPORT/v2/" >/dev/null && break
  sleep 0.2
done
export CLOUDFLARE_API_TOKEN=fake CLOUDFLARE_ACCOUNT_ID=acc
export NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost
export CLOUDFLARE_CONTAINER_REGISTRY="localhost:$RPORT"
api_at() { export CLOUDFLARE_API_BASE_URL="http://127.0.0.1:$1/client/v4"; }
api_at "$PORT"

fail() { echo "FAIL: $*"; tail -20 "$TOP/api.log"; exit 1; }
wc() { "$WC" "$@" 2>&1; }
# Answer the retype on a terminal, as a person would.
wc_yes() {
  python3 - "$WC" "$@" <<'PY'
import os, pty, sys
cmd = sys.argv[1:]
env = cmd[cmd.index("--env") + 1]
pid, fd = pty.fork()
if pid == 0:
    os.execvp(cmd[0], cmd)
out = b""
while True:
    try:
        c = os.read(fd, 4096)
    except OSError:
        break
    if not c:
        break
    sys.stdout.buffer.write(c)
    out += c
    if out.rstrip().endswith(b":") and f"({env})".encode() in out[-300:]:
        os.write(fd, (env + "\n").encode())
        out += b"[sent]"
_, st = os.waitpid(pid, 0)
sys.exit(os.waitstatus_to_exitcode(st))
PY
}
apps() { curl -s -H 'Authorization: Bearer fake' "http://127.0.0.1:$PORT/client/v4/accounts/acc/containers/applications" | python3 -c 'import json,sys; print(" ".join(sorted(a["name"] for a in json.load(sys.stdin)["result"])))'; }
seen() { grep -c "$1" "$TOP/api.log" || true; }

# Without Docker, nothing on Cloudflare is touched (REQ-020).
out=$(WRANGLER_DOCKER_BIN=/usr/bin/false wc --env staging --dry-run up) && fail "dry-run up without Docker"
grep -q 'Docker' <<<"$out" || fail "no-Docker message: $out"
! grep -q 'plan:' <<<"$out" || fail "planned without Docker"

# An account without containers: a compose file with containers stops; one
# without containers works as before.
api_at "$NOPLAN_PORT"
out=$(wc --env staging --dry-run up) && fail "dry-run up on an account without containers"
grep -q 'Workers Paid' <<<"$out" || fail "no-containers message: $out"
cp wrangler-compose.yaml with-containers.yaml
python3 - <<'PY'
import re
t = open("wrangler-compose.yaml").read()
open("wrangler-compose.yaml", "w").write(re.sub(r"\n    containers:\n(      .*\n)+", "\n", t + "\n"))
PY
wc --env staging --dry-run up > noplan.out || fail "a compose file without containers on such an account: $(cat noplan.out)"
mv with-containers.yaml wrangler-compose.yaml
api_at "$PORT"

# Another environment's application and one made by hand are never touched.
for name in boxes--production--api-pinger blog-app; do
  curl -s -X POST -H 'Authorization: Bearer fake' -H 'content-type: application/json' \
    -d "{\"name\":\"$name\",\"configuration\":{\"image\":\"x:1\"}}" \
    "http://127.0.0.1:$PORT/client/v4/accounts/acc/containers/applications" >/dev/null
done

# The plan builds nothing.
before=$(docker images -q | sort | tr '\n' ' ')
out=$(wc --env staging --dry-run up) || fail "dry-run up: $out"
grep -q '+ create     container boxes--staging--api-pinger' <<<"$out" || fail "plan: $out"
grep -q '+ create     container boxes--staging--api-echo' <<<"$out" || fail "plan: $out"
[[ "$(docker images -q | sort | tr '\n' ' ')" == "$before" ]] || fail "dry-run built an image"

# First up: the image is built and pushed, the applications made.
wc_yes --env staging up > up1.out || fail "up: $(cat up1.out)"
[[ "$(apps)" == *boxes--staging--api-echo*boxes--staging--api-pinger* ]] || fail "applications: $(apps)"
curl -s "http://127.0.0.1:$RPORT/v2/_catalog" | grep -q 'boxes--staging--api-pinger' || fail "image not in the registry"
grep -q 'keeps replacing container' up1.out || fail "no rollout notice"

# Second up: nothing changes.
made=$(seen 'APP[+~]')
out=$(wc --env staging up) || fail "second up: $out"
grep -q 'unchanged, not deployed' <<<"$out" || fail "second up deployed: $out"
grep -q '= unchanged  container boxes--staging--api-pinger' <<<"$out" || fail "second plan: $out"
! grep -q 'keeps replacing container' <<<"$out" || fail "rollout notice without a deploy"
[[ $(seen 'APP[+~]') == "$made" ]] || fail "second up changed an application"

# A changed image: shown for that container only, then rolled out.
echo v9 > box/answer.txt
out=$(wc --env staging --dry-run up) || fail "dry-run after a change: $out"
grep -q '~ change     container boxes--staging--api-pinger' <<<"$out" || fail "changed image not planned: $out"
grep -q 'image: changes' <<<"$out" || fail "no image change line: $out"
grep -q '= unchanged  container boxes--staging--api-echo' <<<"$out" || fail "echo should not change: $out"
wc --env staging up > up3.out || fail "up after a change: $(cat up3.out)"
[[ $(seen 'ROLLOUT boxes--staging--api-pinger') -ge 1 ]] || fail "no rollout of the changed container"

# A container dropped from the compose file goes (its class stays).
python3 - <<'PY'
t = open("wrangler-compose.yaml").read()
t = t.replace("      # A registry image.\n      - class: Echo\n        image: docker.io/library/busybox:1.36\n", "")
open("wrangler-compose.yaml", "w").write(t)
PY
ps=$(wc --env staging ps) || fail "ps: $ps"
grep -Eq '^- +container +orphan \(up deletes it\) +boxes--staging--api-echo' <<<"$ps" || fail "ps before the drop: $ps"
out=$(wc --env staging --dry-run up) || fail "dry-run after the drop: $out"
grep -q -- '- delete     container boxes--staging--api-echo' <<<"$out" || fail "drop not planned: $out"
wc --env staging up > up4.out || fail "up after the drop: $(cat up4.out)"
[[ "$(apps)" != *boxes--staging--api-echo* ]] || fail "dropped container left: $(apps)"
ps=$(wc --env staging ps) || fail "ps: $ps"
grep -Eq '^api\.Pinger +container +present +boxes--staging--api-pinger' <<<"$ps" || fail "ps: $ps"

# down: the Worker and its containers go; nothing else.
wc_yes --env staging down > down.out || fail "down: $(cat down.out)"
grep -q 'container boxes--staging--api-pinger' down.out || fail "down did not list the container"
[[ "$(apps)" == "blog-app boxes--production--api-pinger" ]] || fail "after down: $(apps)"
echo "container remote smoke test passed"
