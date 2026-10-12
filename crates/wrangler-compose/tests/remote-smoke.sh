#!/usr/bin/env bash
# Remote flow against a fake Cloudflare API (fake-cloudflare.py), offline:
# up of a new environment (with the retype), nothing changes on a second up,
# secrets / consumers / Durable Object classes removed from the compose file
# go away, a dropped Worker goes on the next up and dropped data only with
# up --purge, another environment's things are never touched, a failed migration
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

# The account must be written in .env.<env> (REQ-006): without it, with a
# different CLOUDFLARE_ACCOUNT_ID in the environment, or with an account the
# login cannot see, nothing is done, not even a dry run.
out=$(wc --env staging --dry-run up) && fail "ran without CLOUDFLARE_ACCOUNT_ID in .env.staging"
grep -q 'write CLOUDFLARE_ACCOUNT_ID=<account id>' <<<"$out" || fail "missing account message: $out"
grep -q 'Test (acc)' <<<"$out" || fail "accounts not listed: $out"
echo 'CLOUDFLARE_ACCOUNT_ID=other' >> .env.staging
out=$(wc --env staging --dry-run up) && fail "ran with a different account in the environment"
grep -q 'differs from CLOUDFLARE_ACCOUNT_ID' <<<"$out" || fail "mismatch message: $out"
out=$(CLOUDFLARE_ACCOUNT_ID= wc --env staging --dry-run up) && fail "ran with an account the login cannot see"
grep -q 'this login cannot use account other' <<<"$out" || fail "unknown account message: $out"
sed -i.bak 's/^CLOUDFLARE_ACCOUNT_ID=other$/CLOUDFLARE_ACCOUNT_ID=acc/' .env.staging && rm .env.staging.bak
[[ $(api 'POST ') == 0 && $(api 'PUT ') == 0 && $(api 'DELETE ') == 0 ]] || fail "something changed without the account"

# A dry run of a new environment says everything and changes nothing.
out=$(wc --env staging --dry-run up)
grep -q 'environment staging does not exist on Cloudflare yet' <<<"$out" || fail "new environment not announced: $out"
# The plan, like terraform plan (REQ-014).
grep -q '+ create     D1 shop--staging--db' <<<"$out" || fail "dry run plan: $out"
grep -q '+ create     Worker shop--staging--api' <<<"$out" || fail "dry run plan (Worker): $out"
grep -q 'environment.GREETING: + "hello"' <<<"$out" || fail "dry run plan (environment): $out"
grep -q 'secrets.STRIPE_KEY: + (value not shown)' <<<"$out" || fail "dry run plan (secret): $out"
grep -q 'sk_test_remote' <<<"$out" && fail "dry run showed a secret value"
grep -q 'to create, 0 to change, 0 to delete' <<<"$out" || fail "dry run plan counts: $out"
[[ $(api 'POST /client/v4/accounts/acc/d1/database$') == 0 ]] || fail "dry run created something"
[[ ! -e .wrangler/compose/staging/data-token ]] || fail "dry run wrote the token"
# Without a terminal the new environment is not made.
if wc --env staging up >/dev/null; then fail "new environment without retype"; fi
[[ $(api 'POST /client/v4/accounts/acc/d1/database$') == 0 ]] || fail "created without retype"

out=$(wc_yes --env staging up)
grep -q 'deployed to environment staging' <<<"$out" || fail "first up: $out"
for want in 'POST /client/v4/accounts/acc/d1/database$' 'POST /client/v4/accounts/acc/storage/kv/namespaces$' \
  'POST /client/v4/accounts/acc/queues$' 'PUT .*/r2/buckets/shop--staging--files/cors' \
  'PUT .*/r2/buckets/shop--staging--files/domains/managed' \
  'PUT .*/event_notifications/r2/shop--staging--uploads/configuration/queues/'; do
  [[ $(api "$want") -ge 1 ]] || fail "missing call $want"
done
[[ $(api 'POST /client/v4/accounts/acc/r2/buckets$') == 2 ]] || fail "buckets"
grep -a -q '"exports": {"Counter": {"type": "durable-object", "storage": "sqlite"}}' api.log || fail "Durable Object exports"
grep -a -q "INSERT INTO \"d1_migrations\"\|SQL .*0001_init.sql" api.log || fail "migrations applied"

# Nothing changed: nothing is deployed again, and the plan says so.
out=$(wc --env staging --dry-run up)
grep -q '= unchanged  Worker shop--staging--api' <<<"$out" || fail "plan after up: $out"
grep -q ' 0 to change, 0 to delete' <<<"$out" || fail "plan after up counts: $out"
out=$(wc --env staging up)
grep -q 'api: unchanged, not deployed' <<<"$out" || fail "second up redeployed: $out"
# A change shows before and after.
sed -i.bak 's/GREETING: hello/GREETING: hi/' wrangler-compose.yaml && rm wrangler-compose.yaml.bak
out=$(wc --env staging --dry-run up)
grep -q '~ change     Worker shop--staging--api' <<<"$out" || fail "plan change: $out"
grep -q 'environment.GREETING: "hello" → "hi"' <<<"$out" || fail "plan before/after: $out"
sed -i.bak 's/GREETING: hi/GREETING: hello/' wrangler-compose.yaml && rm wrangler-compose.yaml.bak
grep -q 'wc-data' <<<"$out" && fail "second up redeployed the data service: $out"

# Removed from the compose file: secret, consumer, Durable Object class.
cp wrangler-compose.yaml compose.orig
python3 - <<'PY'
s = open("wrangler-compose.yaml").read()
for line in ["    secrets: [STRIPE_KEY]\n", "    consumes: [jobs]\n", "    durable_objects: [Counter]\n", "      COUNTER: api.Counter\n"]:
    s = s.replace(line, "")
open("wrangler-compose.yaml", "w").write(s)
PY
# A class gone from the compose file: deleted or kept, chosen on the spot;
# without a terminal it stops.
if wc --env staging up >/dev/null; then fail "class choice without a terminal"; fi
out=$(printf '\n' | script -qec "$BIN/wrangler-compose --env staging up" /dev/null 2>&1) && fail "class choice did not stop"
grep -q 'stopped; nothing was changed' <<<"$out" || fail "class choice did not stop: $out"
out=$(printf 'keep\n' | script -qec "$BIN/wrangler-compose --env staging up" /dev/null 2>&1)
grep -q 'keeping class Counter' <<<"$out" || fail "class not kept: $out"
grep -a -q '"state": "deleted"' api.log && fail "kept class deleted"
out=$(wc_yes --env staging up)
grep -q 'deleting class Counter' <<<"$out" || fail "class deletion not announced"
[[ $(api 'DELETE .*/workers/scripts/shop--staging--api/secrets/STRIPE_KEY') == 1 ]] || fail "secret not deleted"
[[ $(api 'DELETE .*/queues/.*/consumers/') == 1 ]] || fail "consumer not removed"
grep -a -q '"Counter": {"type": "durable-object", "state": "deleted"}' api.log || fail "class tombstone"
out=$(wc --env staging up)
grep -q 'api: unchanged, not deployed' <<<"$out" || { echo "$out"; fail "redeployed after the deletion"; }
cp compose.orig wrangler-compose.yaml
wc_yes --env staging up >/dev/null || fail "back to the full compose file"

# A failed migration stops what comes after it.
echo "FAIL_HERE;" > api/migrations/0009_bad.sql
out=$(wc --env staging up) && fail "failed migration did not stop"
grep -q 'migrations of shop--staging--db failed' <<<"$out" || fail "migration failure message: $out"
grep -q 'deploying as shop--staging' <<<"$out" && fail "deployed after a failed migration"
rm api/migrations/0009_bad.sql

# backup and restore of a remote environment (REQ-013): D1 (with values a JSON
# backup could lose and a BLOB), KV and R2 through Cloudflare's APIs.
A="http://127.0.0.1:$PORT/client/v4/accounts/acc"
DB=$(curl -fsS "$A/d1/database" | python3 -c 'import json,sys; print([d["uuid"] for d in json.load(sys.stdin)["result"] if d["name"]=="shop--staging--db"][0])')
KV=$(curl -fsS "$A/storage/kv/namespaces" | python3 -c 'import json,sys; print([d["id"] for d in json.load(sys.stdin)["result"] if d["title"]=="shop--staging--cache"][0])')
q() { curl -fsS -X POST "$A/d1/database/$DB/raw" -d "$(python3 -c 'import json,sys; print(json.dumps({"sql": sys.argv[1]}))' "$1")"; }
q "INSERT INTO items (id, src) VALUES (1, 'it''s'); INSERT INTO big (n, x, t) VALUES (9007199254740993, 1.0, char(1) || 'x'); INSERT INTO authors (id, name) VALUES (7, 'a'); INSERT INTO posts (id, author, title) VALUES (1, 7, 'Hi'); INSERT INTO acf_fields (k) VALUES (x'00ff');" >/dev/null
curl -fsS -o /dev/null -X PUT "$A/storage/kv/namespaces/$KV/bulk" -d '[{"key":"k1","value":"djE=","base64":true,"metadata":{"m":1}}]'
curl -fsS -o /dev/null -X PUT -H 'content-type: text/plain' "$A/r2/buckets/shop--staging--files/objects/dir/f.txt" --data-binary 'file-body'
wc --env staging backup >/dev/null || fail "remote backup: $(wc --env staging backup)"
BK=$(ls -d backups/staging/* | tail -1)
[[ -f "$BK/manifest.json" ]] || fail "remote backup manifest"
q "DELETE FROM posts; DELETE FROM items; DELETE FROM big; DELETE FROM acf_fields;" >/dev/null
curl -fsS -o /dev/null -X DELETE "$A/storage/kv/namespaces/$KV/values/k1"
curl -fsS -o /dev/null -X DELETE "$A/r2/buckets/shop--staging--files/objects/dir/f.txt"
if wc --env staging restore --drop "$BK" >/dev/null; then fail "remote restore --drop without a terminal"; fi
out=$(wc_yes --env staging restore --drop "$BK") || fail "remote restore: $out"
got=$(q "SELECT src FROM items; SELECT CAST(n AS TEXT), typeof(x), hex(t) FROM big; SELECT title FROM posts; SELECT hex(k) FROM acf_fields;")
for want in "it's" '9007199254740993' 'real' '0178' 'Hi' '00FF'; do
  grep -q -- "$want" <<<"$got" || fail "remote restore lost $want: $got"
done
[[ $(curl -fsS "$A/storage/kv/namespaces/$KV/values/k1") == v1 ]] || fail "remote KV restore"
[[ $(curl -fsS "$A/r2/buckets/shop--staging--files/objects/dir/f.txt") == file-body ]] || fail "remote R2 restore"

# Another environment and another project, named alike: never touched (REQ-006).
curl -fsS -o /dev/null -X POST "$A/d1/database" -d '{"name":"shop--staging-eu--db"}'
curl -fsS -o /dev/null -X POST "$A/d1/database" -d '{"name":"shopx--staging--db"}'

# A Worker dropped from the compose file goes on the next up (it would keep
# answering on its URL); a bucket dropped goes only with up --purge.
curl -fsS -o /dev/null -X PUT "$A/r2/buckets/shop--staging--uploads/objects/a/b.txt"
python3 - <<'PY'
s = open("wrangler-compose.yaml").read()
a, b = s.index("  web:\n"), s.index("  api:\n")
s = s[:a] + s[b:]
a, b = s.index("  uploads:\n"), s.index("  cache:\n")
s = s[:a] + s[b:]
s = s.replace("      UPLOADS: uploads\n", "")
open("wrangler-compose.yaml", "w").write(s)
PY
out=$(wc --env staging ps); grep -q 'orphan .*shop--staging--web' <<<"$out" || fail "ps orphan: $out"
out=$(wc --env staging up)
grep -q 'Worker shop--staging--web is not in the compose file' <<<"$out" || fail "orphan not shown: $out"
[[ $(api 'DELETE .*/workers/scripts/shop--staging--web$') == 1 ]] || fail "orphan Worker not deleted by up"
grep -q 'resources not in the compose file remain: R2 shop--staging--uploads' <<<"$out" || fail "dropped bucket not shown: $out"
[[ $(api 'DELETE .*/r2/buckets/shop--staging--uploads$') == 0 ]] || fail "bucket deleted without --purge"
if wc --env staging up --purge >/dev/null; then fail "up --purge without a terminal"; fi
wc_yes --env staging up --purge >/dev/null
[[ $(api 'DELETE .*/r2/buckets/shop--staging--uploads/objects/a/b.txt') == 1 ]] || fail "bucket not emptied"
[[ $(api 'DELETE .*/r2/buckets/shop--staging--uploads$') == 1 ]] || fail "bucket not deleted by up --purge"
cp compose.orig wrangler-compose.yaml
wc --env staging up >/dev/null

# down: shared data is never purged; down <service>; down everything.
out=$(wc --env staging down --purge api) && fail "purge of shared data"
grep -q 'files (web)' <<<"$out" || fail "purge refusal: $out"
wc_yes --env staging down web >/dev/null
wc --env staging ps | grep -q '^web .*missing' || fail "down web"
wc --env staging ps | grep -q '^api .*present' || fail "down web took api"
wc_yes --env staging down >/dev/null
wc --env staging ps | grep -q '^db .*present' || fail "down deleted data"
# down --purge empties a bucket and deletes everything of this environment
# by name, and nothing else.
curl -fsS -o /dev/null -X PUT "$A/r2/buckets/shop--staging--files/objects/x.txt"
wc_yes --env staging down --purge >/dev/null
[[ $(api 'DELETE .*/r2/buckets/shop--staging--files$') == 1 ]] || fail "bucket with objects not purged"
wc --env staging ps | grep -q '^db .*missing' || fail "down --purge left data"
curl -fsS "$A/d1/database" | grep -q 'shop--staging-eu--db' || fail "another environment's D1 deleted"
curl -fsS "$A/d1/database" | grep -q 'shopx--staging--db' || fail "another project's D1 deleted"
echo "remote smoke test passed"
