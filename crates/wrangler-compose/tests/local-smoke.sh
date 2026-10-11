#!/usr/bin/env bash
# Local acceptance smoke test (AC-003, AC-005, AC-008, AC-009, AC-011, AC-012,
# AC-013 local part): runs the fixture with `--env local` and checks it from
# outside. Needs: nv and wrangler-compose built side by side in $BIN, curl.
set -euo pipefail
BIN=${BIN:?set BIN to the folder holding nv and wrangler-compose}
WC="$BIN/wrangler-compose"
WORK=$(mktemp -d)
cp -R "$(dirname "$0")/fixture/." "$WORK/"
cd "$WORK"
git init -q . # .env.local and backups must be ignored by git (AC-004)
echo 'STRIPE_KEY=sk_test_local' > .env.local
"$BIN/nv" run npm install --no-audit --no-fund >/dev/null

fail() { echo "FAIL: $*"; tail -40 up.out || true; "$WC" --env local down >/dev/null 2>&1 || true; exit 1; }
start() {
  "$WC" --env local up > up.out 2>&1 &
  for _ in $(seq 1 180); do grep -q 'running locally' up.out && return 0; sleep 1; done
  fail "up did not start"
}

"$WC" --env local build
start
# Two Workers, a service binding, D1, R2, KV, Queue, Durable Object, secret.
web=$(curl -fsS localhost:8787/)
[[ "$web" == "web <- api hello do=1 secret=yes | file=from-api | secret=none" ]] || fail "web: $web"
# Same D1 written from two Workers at once: nothing lost (AC-009).
pids=()
for _ in $(seq 1 100); do
  curl -fsS -o /dev/null localhost:8787/write & pids+=($!)
  curl -fsS -o /dev/null "localhost:8788/write?src=api" & pids+=($!)
done
for p in "${pids[@]}"; do wait "$p" || fail "write request"; done
[[ $(curl -fsS localhost:8788/count) == '{"n":200}' ]] || fail "concurrent writes"
# R2 port: CORS from the compose file, not "allow everything" (AC-009).
U=http://127.0.0.1:9000/cdn-cgi/local/r2/s3/storage-files
curl -s -i -X OPTIONS "$U/x" -H 'Origin: http://localhost:8787' -H 'Access-Control-Request-Method: PUT' | grep -qi '^access-control-allow-origin: http://localhost:8787' || fail "allowed preflight"
curl -s -i -X OPTIONS "$U/x" -H 'Origin: http://elsewhere.test' -H 'Access-Control-Request-Method: PUT' | grep -q ' 403 ' || fail "refused preflight"
K="wrangler-compose-local:wrangler-compose-local-secret"
[[ $(curl -s -o /dev/null -w '%{http_code}' --aws-sigv4 "aws:amz:auto:s3" --user $K -X PUT --data-binary 'hi' -H 'content-type: text/plain' "$U/pub.txt") == 200 ]] || fail "signed put"
[[ $(curl -fsS -H 'Host: storage-files.localhost:9000' http://127.0.0.1:9000/pub.txt) == hi ]] || fail "public url"
# R2 event notifications reach the queue consumer (AC-009).
curl -fsS -o /dev/null "localhost:8788/upload?k=n1"
for _ in $(seq 1 30); do grep -q '"action":"PutObject","bucket":"storage-uploads"' up.out && break; sleep 1; done
grep -q '"action":"PutObject","bucket":"storage-uploads"' up.out || fail "notification"
# Logs carry the service name (AC-012).
grep -q '^api  *| api handled /' up.out || fail "tagged logs"
"$WC" --env local ps | grep -q 'running' || fail "ps"
# A second up refuses; down from elsewhere stops it (AC-009).
if "$WC" --env local up >/dev/null 2>&1; then fail "second up started"; fi
"$WC" --env local backup
"$WC" --env local down
sleep 1
if curl -s -m 2 localhost:8787/ >/dev/null; then fail "still answering after down"; fi
# Restore into empty local data (AC-013).
rm -rf .wrangler/compose/local/state
"$WC" --env local restore "$(ls -d backups/local/* | tail -1)"
start
[[ $(curl -fsS localhost:8788/count) == '{"n":200}' ]] || fail "restored rows"
"$WC" --env local down
echo "local smoke test passed"
