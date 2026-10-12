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

# Whatever happens, nothing is left running.
trap '"$WC" --env local down >/dev/null 2>&1 || true' EXIT
fail() { echo "FAIL: $*"; tail -40 up.out || true; "$WC" --env local down >/dev/null 2>&1 || true; exit 1; }
start() {
  "$WC" --env local up > up.out 2>&1 &
  for _ in $(seq 1 180); do grep -q 'running locally' up.out && return 0; sleep 1; done
  fail "up did not start"
}

"$WC" --env local build
# The tool's folder holds secrets: up refuses while git does not ignore it.
cp .gitignore gi.bak && grep -v '^.wrangler/$' gi.bak > .gitignore
if "$WC" --env local up > refused.out 2>&1; then fail "up ran with .wrangler/ not ignored"; fi
grep -q 'not ignored by git' refused.out || fail "gitignore message: $(cat refused.out)"
mv gi.bak .gitignore
# A secret with characters .dev.vars files treat specially arrives unchanged.
export STRIPE_KEY="sk\"a\\b\$HOME'x y"
"$WC" --env local --dry-run up >/dev/null 2>&1 || fail "dry-run up before start"
start
running() { grep -c 'running locally' up.out || true; }
# Two Workers, a service binding, D1, R2, KV, Queue, Durable Object, secret.
web=$(curl -fsS localhost:8787/)
[[ "$web" == "web <- api hello do=1 secret=yes | file=from-api | secret=none" ]] || fail "web: $web"
# Workers are registered in the project's own dev registry, not the machine's.
[[ -f .wrangler/compose/local/registry/web ]] || fail "project dev registry"
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
# The S3 keys are random per project, kept where only the user can read them,
# and never printed (REQ-009). The fixed keys of older versions are refused.
KF=.wrangler/compose/local/s3-keys.json
[[ $(stat -f %Lp "$KF" 2>/dev/null || stat -c %a "$KF") == 600 ]] || fail "s3-keys.json mode"
K="$(sed -n 's/.*"key": *"\([^"]*\)".*/\1/p' "$KF"):$(sed -n 's/.*"secret": *"\([^"]*\)".*/\1/p' "$KF")"
[[ "$K" == wc*:* ]] || fail "s3 keys: $(cat "$KF")"
! grep -q "${K#*:}" up.out || fail "s3 secret printed"
[[ $(curl -s -o /dev/null -w '%{http_code}' --aws-sigv4 "aws:amz:auto:s3" --user wrangler-compose-local:wrangler-compose-local-secret "$U/x") == 401 ]] || fail "old fixed keys accepted"
[[ $(curl -s -o /dev/null -w '%{http_code}' --aws-sigv4 "aws:amz:auto:s3" --user $K -X PUT --data-binary 'hi' -H 'content-type: text/plain' "$U/pub.txt") == 200 ]] || fail "signed put"
[[ $(curl -fsS -H 'Host: storage-files.localhost:9000' http://127.0.0.1:9000/pub.txt) == hi ]] || fail "public url"
# A client waiting for 100 Continue still gets the bucket's CORS headers.
curl -s -i --aws-sigv4 "aws:amz:auto:s3" --user $K -X PUT --data-binary 'expect' -H 'Expect: 100-continue' -H 'Origin: http://localhost:8787' "$U/e.txt" | grep -qi '^access-control-allow-origin: http://localhost:8787' || fail "100-continue"
# From another machine (AC-009): requests signed for the PC's LAN address
# (192.0.2.10 here, reached through --connect-to) pass like local ones, and a
# wrong signature is still refused.
L=http://192.0.2.10:9000/cdn-cgi/local/r2/s3/storage-files
C=(--connect-to 192.0.2.10:9000:127.0.0.1:9000)
[[ $(curl -s "${C[@]}" -o /dev/null -w '%{http_code}' --aws-sigv4 "aws:amz:auto:s3" --user $K -X PUT --data-binary 'lan' "$L/lan.txt") == 200 ]] || fail "LAN signed put"
[[ $(curl -s "${C[@]}" --aws-sigv4 "aws:amz:auto:s3" --user $K -r 0-1 "$L/lan.txt") == la ]] || fail "LAN signed range get"
curl -s -i "${C[@]}" --aws-sigv4 "aws:amz:auto:s3" --user $K -H 'Origin: http://localhost:8787' "$L/lan.txt" | grep -qi '^access-control-allow-origin: http://localhost:8787' || fail "LAN CORS"
[[ $(curl -s "${C[@]}" -o /dev/null -w '%{http_code}' --aws-sigv4 "aws:amz:auto:s3" --user "${K%%:*}:wrong" "$L/lan.txt") == 403 ]] || fail "LAN wrong signature"
# A host name is never signed again, even with the right keys: that is what a
# web page rebinding its name to 127.0.0.1 would send (DES-019).
N=(--connect-to evil.test:9000:127.0.0.1:9000)
[[ $(curl -s "${N[@]}" -o /dev/null -w '%{http_code}' --aws-sigv4 "aws:amz:auto:s3" --user $K "http://evil.test:9000/cdn-cgi/local/r2/s3/storage-files/lan.txt") == 403 ]] || fail "host name signed again"
# R2 event notifications reach the queue consumer (AC-009).
curl -fsS -o /dev/null "localhost:8788/upload?k=n1"
for _ in $(seq 1 30); do grep -q '"action":"PutObject","bucket":"storage-uploads"' up.out && break; sleep 1; done
grep -q '"action":"PutObject","bucket":"storage-uploads"' up.out || fail "notification"
# A Worker's port has no S3 API for buckets with their own port, so their
# CORS rules cannot be skipped (REQ-009).
[[ $(curl -s -o /dev/null -w '%{http_code}' localhost:8787/cdn-cgi/local/r2/s3/storage-files/x) == 404 ]] || fail "S3 API on a Worker port"
# A public URL stays inside its bucket.
out=$(curl -s --path-as-is -H 'Host: storage-files.localhost:9000' "http://127.0.0.1:9000/../../../kv/cache/list")
[[ "$out" != *keys* ]] || fail "public URL left its bucket: $out"
# Data that a careless restore loses: parents after children, generated
# columns, odd table names, non-ASCII metadata, keys with "..".
[[ $(curl -fsS localhost:8788/seed) == seeded ]] || fail "seed"
BLOG='{"name":"Ann","slug":"hello","c":1,"title":"café 日本","dot":"dot","e":null,"kv":{"note":"日本"}}'
[[ $(curl -fsS localhost:8788/blog) == "$BLOG" ]] || fail "blog: $(curl -sS localhost:8788/blog)"
VALUES='{"n":"9007199254740993","x":"real","id":42,"t":1}'
[[ $(curl -fsS localhost:8788/values) == "$VALUES" ]] || fail "values: $(curl -sS localhost:8788/values)"
# --purge of a service whose data another service uses stops first (AC-011).
if "$WC" --env local down --purge api > purge.out 2>&1; then fail "purge of shared data"; fi
grep -q 'storage-files (web)' purge.out || fail "purge message: $(cat purge.out)"
curl -fsS -o /dev/null localhost:8787/ || fail "purge refusal stopped the services"
# Editing the compose file restarts on the same ports; a broken edit keeps
# the services running until it is fixed (REQ-009).
cp wrangler-compose.yaml compose.bak
echo "# edited" >> wrangler-compose.yaml
for _ in $(seq 1 120); do [[ $(running) -ge 2 ]] && break; sleep 1; done
[[ $(running) -ge 2 ]] || fail "restart"
[[ $(curl -fsS localhost:8787/write -o /dev/null -w '%{http_code}') == 200 ]] || fail "after restart"
echo "services: [" >> wrangler-compose.yaml
for _ in $(seq 1 20); do grep -q 'cannot load the compose file' up.out && break; sleep 1; done
grep -q 'cannot load the compose file' up.out || fail "broken compose not reported"
curl -fsS -o /dev/null localhost:8787/ || fail "broken compose stopped the services"
cp compose.bak wrangler-compose.yaml
for _ in $(seq 1 120); do [[ $(running) -ge 3 ]] && break; sleep 1; done
[[ $(running) -ge 3 ]] || fail "restart after fix"
# Notifications carry on across restarts: nothing repeated, nothing lost.
curl -fsS -o /dev/null "localhost:8788/remove?k=n1"
for _ in $(seq 1 30); do grep -q '"action":"DeleteObject","bucket":"storage-uploads","object":{"key":"up/n1"' up.out && break; sleep 1; done
grep -q '"action":"DeleteObject","bucket":"storage-uploads","object":{"key":"up/n1"' up.out || fail "delete notification after restart"
[[ $(grep -c '"action":"PutObject","bucket":"storage-uploads","object":{"key":"up/n1"' up.out) == 1 ]] || fail "notification repeated after restart"
[[ $(curl -fsS localhost:8788/secret) == "$STRIPE_KEY" ]] || fail "secret changed: $(curl -sS localhost:8788/secret)"
# A dry run says a real up would stop while one runs.
if "$WC" --env local --dry-run up >/dev/null 2>&1; then fail "dry-run up while running"; fi
# Routing does not depend on the Host header (proxies rewrite it).
[[ $(curl -fsS -H 'Host: example.test' localhost:8787/shared) == one ]] || fail "routing with another Host"
# Crons run on request (they do not fire by themselves locally).
curl -fsS -o /dev/null "localhost:8788/cdn-cgi/handler/scheduled?cron=*/5%20*%20*%20*%20*" || fail "scheduled"
[[ $(curl -fsS localhost:8788/cron) == "*/5 * * * *" ]] || fail "cron: $(curl -sS localhost:8788/cron)"
# Code shared from outside the Worker folder rebuilds the Worker.
echo 'export const SHARED = "two";' > shared/value.ts
for _ in $(seq 1 60); do [[ $(curl -fsS localhost:8787/shared) == two ]] && break; sleep 1; done
[[ $(curl -fsS localhost:8787/shared) == two ]] || fail "shared code change"
# Every line of a multi-line log carries the service name.
grep -q '^api  *| second line' up.out || fail "multi-line log"
# Logs carry the service name (AC-012).
grep -q '^api  *| api handled /' up.out || fail "tagged logs"
out=$("$WC" --env local ps); grep -q 'running' <<<"$out" || fail "ps: $out"
# down <service> takes only that service down; up brings it back (AC-011).
"$WC" --env local down web >/dev/null 2>&1 || fail "down web"
if curl -s -m 2 localhost:8787/ >/dev/null; then fail "web still answers after down web"; fi
curl -fsS -o /dev/null localhost:8788/count || fail "api stopped with web"
out=$("$WC" --env local ps); grep -q '^web .* stopped' <<<"$out" || fail "ps after down web: $out"
"$WC" --env local up web >/dev/null 2>&1 || fail "up web"
curl -fsS -o /dev/null localhost:8787/shared || fail "web did not come back"
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
[[ $(curl -fsS localhost:8788/count) == '{"n":201}' ]] || fail "restored rows"
[[ $(curl -fsS localhost:8788/blog) == "$BLOG" ]] || fail "restored blog: $(curl -sS localhost:8788/blog)"
[[ $(curl -fsS localhost:8788/values) == "$VALUES" ]] || fail "restored values: $(curl -sS localhost:8788/values)"
[[ $(curl -fsS localhost:8788/next) == '{"id":4}' ]] || fail "AUTOINCREMENT after restore: $(curl -sS localhost:8788/next)"
# A restore that fails half-way puts the previous contents back (Linux:
# --drop asks to retype the name, given here through a terminal).
if [[ $(uname) == Linux ]]; then
  BAD=badbackup && cp -R "$(ls -d backups/local/* | tail -1)" "$BAD"
  python3 - "$BAD" <<'PY'
import hashlib, json, sys
d = sys.argv[1]
m = json.load(open(f"{d}/manifest.json"))
e = next(x for x in m["resources"] if x["type"] == "d1")
lines = open(f"{d}/{e['file']}").read().splitlines()
row = next(l for l in lines if '"t":"items"' in l)
open(f"{d}/{e['file']}", "w").write("\n".join(lines + [row]) + "\n")  # a duplicate key
e["sha256"] = hashlib.sha256(open(f"{d}/{e['file']}", "rb").read()).hexdigest()
json.dump(m, open(f"{d}/manifest.json", "w"))
PY
  if printf 'local\n' | script -qec "$WC --env local restore --drop $BAD" /dev/null > bad.out 2>&1; then fail "bad restore succeeded"; fi
  grep -q 'previous contents are back' bad.out || fail "no rollback: $(cat bad.out)"
  [[ $(curl -fsS localhost:8788/count) == '{"n":201}' ]] || fail "rows after rollback"
  [[ $(curl -fsS localhost:8788/values) == "$VALUES" ]] || fail "values after rollback"
fi
"$WC" --env local down
echo "local smoke test passed"
