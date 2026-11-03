#!/usr/bin/env bash
# Static assets with their settings (REQ-002): runs fixture-assets with
# `--env local` and checks from outside that a single-page application gets
# index.html for its own paths, /api/* reaches the Worker first and the
# Worker reads files through its ASSETS binding, while `assets: <folder>`
# behaves as before. Then the remote flow against the fake Cloudflare API
# (fake-cloudflare.py): the settings and the binding go up with the Worker,
# nothing changes on a second up, and a changed file deploys again.
# Needs: nv and wrangler-compose in $BIN, curl, python3, script.
set -euo pipefail
export NV_LANG=en # the checks read the English messages
BIN=${BIN:?set BIN to the folder holding nv and wrangler-compose}
WC="$BIN/wrangler-compose"
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d)
cp -R "$HERE/fixture-assets/." "$WORK/"
cp "$HERE/fixture/.gitignore" "$WORK/.gitignore"
cd "$WORK"
git init -q .
"$BIN/nv" run npm install --no-audit --no-fund >/dev/null

FAKE=
trap '"$WC" --env local down >/dev/null 2>&1 || true; [[ -n "$FAKE" ]] && kill $FAKE 2>/dev/null || true' EXIT
fail() { echo "FAIL: $*"; tail -40 up.out 2>/dev/null || true; tail -20 api.log 2>/dev/null || true; exit 1; }

# Mistakes stop before anything runs, saying where.
cp wrangler-compose.yaml good.yaml
sed 's/single-page-application/spa/' good.yaml > wrangler-compose.yaml
out=$("$WC" --env local up 2>&1) && fail "up ran with not_found_handling: spa"
grep -q 'services.app.assets.not_found_handling' <<<"$out" || fail "where: $out"
cp good.yaml wrangler-compose.yaml

"$WC" --env local up > up.out 2>&1 &
for _ in $(seq 1 180); do grep -q 'running locally' up.out && break; sleep 1; done
grep -q 'running locally' up.out || fail "up did not start"
cfg=.wrangler/compose/local/run/app/wrangler.json
python3 - "$cfg" <<'PY' || fail "local config: $(cat "$cfg")"
import json, sys
a = json.load(open(sys.argv[1]))["assets"]
assert a["not_found_handling"] == "single-page-application", a
assert a["html_handling"] == "auto-trailing-slash", a
assert a["run_worker_first"] == ["/api/*"], a
assert a["binding"] == "ASSETS", a
assert a["directory"].endswith("site"), a
PY
INDEX=$(cat site/index.html)
A=localhost:8795 P=localhost:8796
[[ $(curl -fsS $A/) == "$INDEX" ]] || fail "app /: $(curl -sS $A/)"
# The application's own paths get index.html, with 200.
[[ $(curl -fsS $A/p/123) == "$INDEX" ]] || fail "app /p/123: $(curl -sS $A/p/123)"
[[ $(curl -fsS $A/deep/a/b) == "$INDEX" ]] || fail "app /deep/a/b"
[[ $(curl -fsS $A/assets/app.js) == "$(cat site/assets/app.js)" ]] || fail "app file"
# /api/* reaches the Worker first, and the Worker reads files through ASSETS.
[[ $(curl -fsS $A/api/hello) == api ]] || fail "run_worker_first: $(curl -sS $A/api/hello)"
[[ $(curl -fsS $A/api/file) == "$(cat site/assets/app.js)" ]] || fail "ASSETS file: $(curl -sS $A/api/file)"
[[ $(curl -fsS $A/api/page) == "$INDEX" ]] || fail "ASSETS fallback: $(curl -sS $A/api/page)"
# The folder alone, as before: a path with no file goes to the Worker.
[[ $(curl -fsS $P/) == "$INDEX" ]] || fail "plain /"
[[ $(curl -fsS $P/p/123) == "plain worker /p/123" ]] || fail "plain /p/123: $(curl -sS $P/p/123)"
[[ $(python3 -c 'import json,sys; print(sorted(json.load(open(sys.argv[1]))["assets"]))' .wrangler/compose/local/run/plain/wrangler.json) == "['directory']" ]] \
  || fail "plain config: $(cat .wrangler/compose/local/run/plain/wrangler.json)"
"$WC" --env local down >/dev/null 2>&1
wait

# Remote, against the fake Cloudflare API.
PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
python3 "$HERE/fake-cloudflare.py" "$PORT" 2> api.log &
FAKE=$!
for _ in $(seq 1 50); do curl -s "http://127.0.0.1:$PORT/" >/dev/null && break; sleep 0.1; done
export CLOUDFLARE_API_BASE_URL="http://127.0.0.1:$PORT/client/v4" CLOUDFLARE_API_TOKEN=fake CLOUDFLARE_ACCOUNT_ID=acc
export NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost
echo 'CLOUDFLARE_ACCOUNT_ID=acc' > .env.staging
# Answer the retype through a terminal, as a person would.
if [[ $(uname) == Darwin ]]; then
  wc_yes() { { printf 'staging\n'; sleep 3; } | script -q /dev/null "$WC" --env staging "$@" 2>&1; }
else
  wc_yes() { printf 'staging\n' | script -qec "$WC --env staging $*" /dev/null 2>&1; }
fi
settings() { curl -fsS -H 'Authorization: Bearer fake' "$CLOUDFLARE_API_BASE_URL/accounts/acc/workers/scripts/pages--staging--$1/settings"; }

out=$("$WC" --env staging --dry-run up 2>&1) || fail "dry-run: $out"
grep -q 'bindings.ASSETS: + assets' <<<"$out" || fail "plan without the ASSETS binding: $out"
out=$(wc_yes up) || fail "up: $out"
grep -a 'META .*"single-page-application"' api.log | grep -q '"run_worker_first": \["/api/\*"\]' || fail "assets settings not sent: $(grep -a META api.log)"
grep -a -q 'ASSETS .*"/assets/app.js", "/index.html"' api.log || fail "files not sent: $(grep -a ASSETS api.log)"
settings app | grep -q '"type": *"assets", *"name": *"ASSETS"\|"name": *"ASSETS", *"type": *"assets"' || fail "binding: $(settings app)"
# Nothing changed: nothing is deployed, and the plan shows no change.
out=$("$WC" --env staging up 2>&1) || fail "second up: $out"
grep -q 'app: unchanged, not deployed' <<<"$out" || fail "second up deployed app: $out"
grep -q 'plain: unchanged, not deployed' <<<"$out" || fail "second up deployed plain: $out"
# A changed file, outside the Worker's folder, deploys again.
echo 'console.log("v2");' > site/assets/app.js
out=$("$WC" --env staging --dry-run up 2>&1) || fail "dry-run after change: $out"
grep -q 'code: changes' <<<"$out" || fail "plan misses the changed file: $out"
out=$("$WC" --env staging up 2>&1) || fail "up after change: $out"
grep -q 'app: deploying' <<<"$out" || fail "changed file not deployed: $out"
grep -q 'plain: deploying' <<<"$out" || fail "changed file not deployed to plain: $out"
echo "OK: assets smoke"
