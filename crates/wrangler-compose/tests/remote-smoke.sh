#!/usr/bin/env bash
# Remote flow against a fake Cloudflare API (fake-cloudflare.py), offline:
# up of a new environment (with the retype), nothing changes on a second up,
# secrets / consumers / Durable Object classes removed from the compose file
# go away, dropped Workers stay until --remove-orphans, a failed migration
# stops what comes after it, ps, down <service>, down. It checks the tool's
# own steps; real Cloudflare behavior is checked on a test account (AC-010).
# Needs: nv and wrangler-compose in $BIN, curl, python3, util-linux script.
set -euo pipefail
BIN=${BIN:?set BIN to the folder holding nv and wrangler-compose}
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d)
cp -R "$HERE/fixture/." "$WORK/"
cd "$WORK"
git init -q .
echo 'STRIPE_KEY=sk_test_remote' > .env.staging
"$BIN/nv" run npm install --no-audit --no-fund >/dev/null

PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
python3 "$HERE/fake-cloudflare.py" "$PORT" 2> api.log &
FAKE=$!
trap 'kill $FAKE 2>/dev/null || true' EXIT
for _ in $(seq 1 50); do curl -s "http://127.0.0.1:$PORT/" >/dev/null && break; sleep 0.1; done
export CLOUDFLARE_API_BASE_URL="http://127.0.0.1:$PORT/client/v4" CLOUDFLARE_API_TOKEN=fake CLOUDFLARE_ACCOUNT_ID=acc
export NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost

wc() { "$BIN/wrangler-compose" "$@" 2>&1; }
# Answer the retype through a terminal, as a person would.
wc_yes() { printf 'staging\n' | script -qec "$BIN/wrangler-compose $*" /dev/null 2>&1; }
fail() { echo "FAIL: $*"; tail -20 api.log; exit 1; }
api() { grep -a -c "$1" api.log || true; }

# A dry run of a new environment says everything and changes nothing.
out=$(wc --env staging --dry-run up)
grep -q 'environment staging does not exist on Cloudflare yet' <<<"$out" || fail "new environment not announced: $out"
grep -q '(dry run) creating D1 shop-staging-db' <<<"$out" || fail "dry run plan: $out"
[[ $(api 'POST /client/v4/accounts/acc/d1/database$') == 0 ]] || fail "dry run created something"
[[ ! -e .wrangler/compose/staging/data-token ]] || fail "dry run wrote the token"
# Without a terminal the new environment is not made.
if wc --env staging up >/dev/null; then fail "new environment without retype"; fi
[[ $(api 'POST /client/v4/accounts/acc/d1/database$') == 0 ]] || fail "created without retype"

out=$(wc_yes --env staging up)
grep -q 'deployed to environment staging' <<<"$out" || fail "first up: $out"
for want in 'POST /client/v4/accounts/acc/d1/database$' 'POST /client/v4/accounts/acc/storage/kv/namespaces$' \
  'POST /client/v4/accounts/acc/queues$' 'PUT .*/r2/buckets/shop-staging-storage-files/cors' \
  'PUT .*/r2/buckets/shop-staging-storage-files/domains/managed' \
  'PUT .*/event_notifications/r2/shop-staging-storage-uploads/configuration/queues/'; do
  [[ $(api "$want") -ge 1 ]] || fail "missing call $want"
done
[[ $(api 'POST /client/v4/accounts/acc/r2/buckets$') == 2 ]] || fail "buckets"
grep -a -q '"exports": {"Counter": {"type": "durable-object", "storage": "sqlite"}}' api.log || fail "Durable Object exports"
grep -a -q "INSERT INTO \"d1_migrations\"\|SQL .*0001_init.sql" api.log || fail "migrations applied"

# Nothing changed: nothing is deployed again.
out=$(wc --env staging up)
grep -q 'api: unchanged, not deployed' <<<"$out" || fail "second up redeployed: $out"
grep -q 'wc-data' <<<"$out" && fail "second up redeployed the data service: $out"

# Removed from the compose file: secret, consumer, Durable Object class.
cp wrangler-compose.yaml compose.orig
python3 - <<'PY'
s = open("wrangler-compose.yaml").read()
for line in ["    secrets: [stripe-key]\n", "    consumes: [jobs]\n", "    durable_objects: [Counter]\n", "      COUNTER: api.Counter\n"]:
    s = s.replace(line, "")
open("wrangler-compose.yaml", "w").write(s)
PY
if wc --env staging up >/dev/null; then fail "class deletion without retype"; fi
out=$(wc_yes --env staging up)
grep -q 'Durable Object classes Counter will be deleted' <<<"$out" || fail "class deletion not announced"
[[ $(api 'DELETE .*/workers/scripts/shop-staging-api/secrets/STRIPE_KEY') == 1 ]] || fail "secret not deleted"
[[ $(api 'DELETE .*/queues/.*/consumers/') == 1 ]] || fail "consumer not removed"
grep -a -q '"Counter": {"type": "durable-object", "state": "deleted"}' api.log || fail "class tombstone"
out=$(wc --env staging up)
grep -q 'api: unchanged, not deployed' <<<"$out" || { echo "$out"; fail "redeployed after the deletion"; }
cp compose.orig wrangler-compose.yaml
wc_yes --env staging up >/dev/null || fail "back to the full compose file"

# A failed migration stops what comes after it.
echo "FAIL_HERE;" > api/migrations/0009_bad.sql
out=$(wc --env staging up) && fail "failed migration did not stop"
grep -q 'migrations of shop-staging-db failed' <<<"$out" || fail "migration failure message: $out"
grep -q 'deploying as shop-staging' <<<"$out" && fail "deployed after a failed migration"
rm api/migrations/0009_bad.sql

# A Worker dropped from the compose file stays until --remove-orphans.
python3 - <<'PY'
s = open("wrangler-compose.yaml").read()
a, b = s.index("  web:\n"), s.index("  api:\n")
open("wrangler-compose.yaml", "w").write(s[:a] + s[b:])
PY
wc --env staging ps | grep -q 'orphan .*shop-staging-web' || fail "ps orphan"
out=$(wc --env staging up)
grep -q 'Workers not in the compose file remain: shop-staging-web' <<<"$out" || fail "orphan not shown"
[[ $(api 'DELETE .*/workers/scripts/shop-staging-web$') == 0 ]] || fail "orphan deleted without asking"
wc_yes --env staging up --remove-orphans >/dev/null
[[ $(api 'DELETE .*/workers/scripts/shop-staging-web$') == 1 ]] || fail "orphan not deleted"
cp compose.orig wrangler-compose.yaml
wc --env staging up >/dev/null

# down: shared data is never purged; down <service>; down everything.
out=$(wc --env staging down --purge api) && fail "purge of shared data"
grep -q 'storage-files (web)' <<<"$out" || fail "purge refusal: $out"
wc_yes --env staging down web >/dev/null
wc --env staging ps | grep -q '^web .*missing' || fail "down web"
wc --env staging ps | grep -q '^api .*present' || fail "down web took api"
wc_yes --env staging down >/dev/null
wc --env staging ps | grep -q '^wc-data .*missing' || fail "data service left"
wc --env staging ps | grep -q '^db .*present' || fail "down deleted data"
echo "remote smoke test passed"
