#!/usr/bin/env bash
# The codemagic service (AC-021) against a fake Codemagic API
# (fake-codemagic.py) and the fake Cloudflare API, offline: registering the
# app (with the retype) and adding CODEMAGIC_APP_ID to .env, nothing changes
# on a second up, a plain value follows .env, a Secure value only with
# --update-secret, unwritten variables and groups go, another environment's
# app is never touched, down deletes the groups before the app and removes
# the id line, --dry-run and ps only read and never show a value, --env local
# does not touch Codemagic, and the ways it stops. It checks the tool's own
# steps; real Codemagic behavior is checked on a test account (AC-021).
# Needs: nv and wrangler-compose in $BIN, curl, python3, util-linux script.
set -euo pipefail
unset CODEMAGIC_API_TOKEN CODEMAGIC_APP_ID
BIN=${BIN:?set BIN to the folder holding nv and wrangler-compose}
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d)
cp -R "$HERE/fixture/." "$WORK/"
cd "$WORK"
git init -q .
cat >> wrangler-compose.yaml <<'YAML'
  mobile:
    type: codemagic
    repository: https://github.com/example/shop
    groups:
      store:
        environment:
          STORE_DOMAIN: ${WEB_DOMAIN}
          STORE_NOTE: ${STORE_NOTE:-}
        secrets: [STORE_APP_KEY]
      firebase:
        environment: [PROJECT=shop-dev, REGION=asia]
        secrets:
          FB_KEY: ${FIREBASE_KEY}
YAML
for e in ac-1 ac-2; do
  printf 'CLOUDFLARE_ACCOUNT_ID=acc\nSTRIPE_KEY=sk_test_remote\nWEB_DOMAIN=shop-%s.example\nSTORE_APP_KEY=store-secret-1\nFIREBASE_KEY=fb-secret-1' "$e" > ".env.$e"
done
"$BIN/nv" run npm install --no-audit --no-fund >/dev/null

port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }
CF=$(port); CM=$(port)
python3 "$HERE/fake-cloudflare.py" "$CF" 2> api.log &
FAKE_CF=$!
python3 "$HERE/fake-codemagic.py" "$CM" 2> cm.log &
FAKE_CM=$!
trap 'kill $FAKE_CF $FAKE_CM 2>/dev/null || true' EXIT
trap 'echo "FAIL: line $LINENO stopped the test"; tail -20 cm.log' ERR
for p in "$CF" "$CM"; do
  for _ in $(seq 1 50); do curl -s "http://127.0.0.1:$p/" >/dev/null && break; sleep 0.1; done
done
export CLOUDFLARE_API_BASE_URL="http://127.0.0.1:$CF/client/v4" CLOUDFLARE_API_TOKEN=fake CLOUDFLARE_ACCOUNT_ID=acc
export CODEMAGIC_API_BASE_URL="http://127.0.0.1:$CM"
export NO_PROXY=127.0.0.1,localhost no_proxy=127.0.0.1,localhost NV_LANG=en

wc() { "$BIN/wrangler-compose" "$@" 2>&1; }
# Answer the retype through a terminal, as a person would: wc_yes <env> args…
wc_yes() { local e=$1; shift; printf '%s\n' "$e" | script -qec "$BIN/wrangler-compose --env $e $*" /dev/null 2>&1; }
fail() { echo "FAIL: $*"; tail -20 cm.log; exit 1; }
cm() { grep -a -c "$1" cm.log || true; }
# Changes Codemagic was asked to make since the log had $1 lines.
changes_since() { tail -n +"$(($1 + 1))" cm.log | grep -a -c -E '^(POST|PATCH|DELETE) ' || true; }
gets_only_since() { [[ $(tail -n +"$(($1 + 1))" cm.log | grep -a -v -c -E '^GET ' || true) == 0 ]]; }
lines() { command wc -l < cm.log | tr -d ' '; }
state() { curl -fsS "http://127.0.0.1:$CM/__state"; }
# The state of one app: groups → variables → [value, secure], sorted.
app_state() {
  state | python3 -c '
import json, sys
s = json.load(sys.stdin); app = sys.argv[1]
out = {g["name"]: {v["name"]: [v["value"], v["secure"]] for v in g["vars"]} for g in s["groups"].values() if g["app"] == app}
print(json.dumps(out, sort_keys=True))' "$1"
}
app_id() { sed -n 's/^CODEMAGIC_APP_ID=//p' ".env.$1"; }
no_secrets() { ! grep -q -e 'store-secret' -e 'fb-secret' -e 'cm-token' <<<"$1"; }
API="http://127.0.0.1:$CM"

# Stops before anything is touched: no token (and where to find one), a
# compose file without a repository, with an unknown key, with a secret
# written as is.
out=$(wc --env ac-1 --dry-run up) && fail "ran without a token"
grep -q 'no Codemagic API token' <<<"$out" || fail "token message: $out"
grep -q 'Teams > Personal Account > Integrations > Codemagic API' <<<"$out" || fail "where to find the token: $out"
grep -a -q 'accounts' api.log && fail "Cloudflare was touched without a Codemagic token"
cp wrangler-compose.yaml compose.orig
for change in '    repository: https://github.com/example/shop\n|' \
  '    type: codemagic\n|    type: codemagic\n    branch: main\n' \
  '        secrets: [STORE_APP_KEY]\n|        secrets: { STORE_APP_KEY: raw-secret-1 }\n'; do
  python3 -c 'import sys; a, b = sys.argv[1].replace("\\n", "\n").split("|"); s = open("compose.orig").read(); assert a in s; open("wrangler-compose.yaml", "w").write(s.replace(a, b))' "$change"
  out=$(CODEMAGIC_API_TOKEN=cm-token wc --env ac-1 --dry-run up) && fail "ran with $change"
  grep -q 'services.mobile' <<<"$out" || fail "no place in: $out"
  grep -q 'raw-secret-1' <<<"$out" && fail "a secret value was shown: $out"
done
cp compose.orig wrangler-compose.yaml
grep -a -q 'accounts' api.log && fail "Cloudflare was touched with a broken compose file"
[[ $(lines) == 0 ]] || fail "Codemagic was touched before the checks passed"
printf '\nCODEMAGIC_API_TOKEN=cm-token\n' >> .env.ac-1
printf '\nCODEMAGIC_API_TOKEN=cm-token' >> .env.ac-2

# --env local never touches Codemagic and says so. The Codemagic service's
# variables (STORE_APP_KEY, FIREBASE_KEY) need no value there.
printf 'STRIPE_KEY=sk_local\nWEB_DOMAIN=x\n' > .env.local
n=$(lines)
out=$(wc --env local --dry-run up)
grep -q 'mobile: the Codemagic service does nothing locally' <<<"$out" || fail "local notice: $out"
[[ $(lines) == "$n" ]] || fail "--env local touched Codemagic"

# A dry run shows the registration, groups and variables; only reads.
n=$(lines)
out=$(wc --env ac-1 --dry-run up)
grep -q 'mobile: environment ac-1 has no Codemagic app yet; https://github.com/example/shop will be registered' <<<"$out" || fail "registration not announced: $out"
grep -q '+ create     Codemagic app (mobile)' <<<"$out" || fail "plan (app): $out"
grep -q '+ create     Codemagic variable group store' <<<"$out" || fail "plan (group): $out"
grep -q 'STORE_DOMAIN: + "shop-ac-1.example"' <<<"$out" || fail "plan (plain value): $out"
grep -q 'STORE_APP_KEY: + (Secure, value not shown)' <<<"$out" || fail "plan (Secure): $out"
grep -q 'STORE_NOTE' <<<"$out" && fail "an empty value was planned: $out"
no_secrets "$out" || fail "the dry run showed a secret: $out"
gets_only_since "$n" || fail "the dry run changed Codemagic"
# Without a terminal the app is not registered.
out=$(wc --env ac-1 up) && fail "registered without the retype"
[[ $(cm 'POST /apps$') == 0 ]] || fail "registered without the retype"

# up registers the app, writes its id at the end of .env.ac-1 (other lines as
# they were), and makes the groups what the compose file says.
cp .env.ac-1 env.before
out=$(wc_yes ac-1 up)
grep -q 'deployed to environment ac-1' <<<"$out" || fail "first up: $out"
no_secrets "$out" || fail "up showed a secret: $out"
ID1=$(app_id ac-1)
[[ -n "$ID1" ]] || fail "CODEMAGIC_APP_ID not written: $(cat .env.ac-1)"
grep -q "added CODEMAGIC_APP_ID=$ID1" <<<"$out" || fail "id not announced: $out"
[[ "$(cat .env.ac-1)" == "$(cat env.before)"$'\n'"CODEMAGIC_APP_ID=$ID1" ]] || fail ".env.ac-1 changed beyond the id line: $(cat .env.ac-1)"
want='{"firebase": {"FB_KEY": ["fb-secret-1", true], "PROJECT": ["shop-dev", false], "REGION": ["asia", false]}, "store": {"STORE_APP_KEY": ["store-secret-1", true], "STORE_DOMAIN": ["shop-ac-1.example", false]}}'
[[ "$(app_state "$ID1")" == "$want" ]] || fail "groups after up: $(app_state "$ID1")"
[[ $(cm 'POST /api/v3/variable-groups/.*/variables$') == 4 ]] || fail "one bulk call per group and Secure"

# A wrong token: Codemagic's answer at its first call, never the token.
out=$(CODEMAGIC_API_TOKEN=wrong-token-9 wc --env ac-1 --dry-run up) && fail "ran with a wrong token"
grep -q 'Codemagic API 401' <<<"$out" || fail "wrong token answer: $out"
grep -q 'wrong-token-9' <<<"$out" && fail "the token was shown"
# The rate limit: how long to wait.
out=$(CODEMAGIC_API_TOKEN=cm-ratelimited wc --env ac-1 --dry-run up) && fail "ran past the rate limit"
grep -q 'try again in 42 seconds' <<<"$out" || fail "rate limit message: $out"
# Without an app yet, the first call is the registration: it stops there with
# Codemagic's answer, says another up finishes, and writes no id.
out=$(CODEMAGIC_API_TOKEN=wrong-token-9 wc_yes ac-2 up) && fail "registered with a wrong token"
grep -q 'Codemagic API 401' <<<"$out" || fail "wrong token at registration: $out"
grep -q 'Another up finishes the rest' <<<"$out" || fail "how to finish not said: $out"
grep -q 'wrong-token-9' <<<"$out" && fail "the token was shown"
grep -q CODEMAGIC_APP_ID .env.ac-2 && fail "an id was written after a failed registration"

# Nothing changed: no app is added and nothing is changed.
n=$(lines)
out=$(wc --env ac-1 up)
[[ $(changes_since "$n") == 0 ]] || fail "second up changed Codemagic: $(tail -n +$((n + 1)) cm.log)"
grep -q '= unchanged  Codemagic app '"$ID1" <<<"$out" || fail "second up plan: $out"
[[ $(state | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["apps"]))') == 1 ]] || fail "an app was added"

# A plain value follows .env; a Secure one only with --update-secret.
sed -i.bak 's/^WEB_DOMAIN=.*/WEB_DOMAIN=new-ac-1.example/; s/^STORE_APP_KEY=.*/STORE_APP_KEY=store-secret-2/' .env.ac-1 && rm .env.ac-1.bak
out=$(wc --env ac-1 --dry-run up)
grep -q 'STORE_DOMAIN: "shop-ac-1.example" → "new-ac-1.example"' <<<"$out" || fail "plan before/after: $out"
grep -q 'STORE_APP_KEY: Secure, already there; unchanged (--update-secret sets it again)' <<<"$out" || fail "plan kept Secure: $out"
no_secrets "$out" || fail "the dry run showed a secret: $out"
out=$(wc --env ac-1 up)
[[ $(app_state "$ID1") == *'"STORE_DOMAIN": ["new-ac-1.example", false]'* ]] || fail "plain value not changed: $(app_state "$ID1")"
[[ $(app_state "$ID1") == *'"STORE_APP_KEY": ["store-secret-1", true]'* ]] || fail "Secure value changed without --update-secret"
grep -q 'were not set again (.*mobile: store.STORE_APP_KEY, firebase.FB_KEY.*); if you changed their values, add --update-secret' <<<"$out" || fail "--update-secret not suggested: $out"
out=$(wc --env ac-1 up --update-secret)
[[ $(app_state "$ID1") == *'"STORE_APP_KEY": ["store-secret-2", true]'* ]] || fail "--update-secret did not set the Secure value: $(app_state "$ID1")"
no_secrets "$out" || fail "--update-secret showed a secret: $out"
# An empty value is not put in, and goes when it is there.
echo 'STORE_NOTE=note' >> .env.ac-1
wc --env ac-1 up >/dev/null
[[ $(app_state "$ID1") == *'"STORE_NOTE": ["note", false]'* ]] || fail "value not added"
sed -i.bak 's/^STORE_NOTE=note$/STORE_NOTE=/' .env.ac-1 && rm .env.ac-1.bak
wc --env ac-1 up >/dev/null
[[ $(app_state "$ID1") == *STORE_NOTE* ]] && fail "an empty value stayed"

# A group and a variable gone from the compose file go from Codemagic; a
# group added on Codemagic's screen goes too.
curl -fsS -o /dev/null -X POST -H 'x-auth-token: cm-token' -H 'content-type: application/json' \
  "$API/api/v3/apps/$ID1/variable-groups" -d '{"name":"by-hand"}'
python3 - <<'PY'
s = open("wrangler-compose.yaml").read()
a = s.index("      firebase:\n")
s = s[:a]
s = s.replace("          STORE_NOTE: ${STORE_NOTE:-}\n", "")
open("wrangler-compose.yaml", "w").write(s)
PY
out=$(wc --env ac-1 --dry-run up)
grep -q -- '- delete     Codemagic variable group firebase' <<<"$out" || fail "plan (group gone): $out"
grep -q -- '- delete     Codemagic variable group by-hand' <<<"$out" || fail "plan (group by hand): $out"
wc --env ac-1 up >/dev/null
want='{"store": {"STORE_APP_KEY": ["store-secret-2", true], "STORE_DOMAIN": ["new-ac-1.example", false]}}'
[[ "$(app_state "$ID1")" == "$want" ]] || fail "groups after removing: $(app_state "$ID1")"
sed -i.bak 's/^          STORE_DOMAIN: \${WEB_DOMAIN}$/          OTHER: x/' wrangler-compose.yaml && rm wrangler-compose.yaml.bak
wc --env ac-1 up >/dev/null
want='{"store": {"OTHER": ["x", false], "STORE_APP_KEY": ["store-secret-2", true]}}'
[[ "$(app_state "$ID1")" == "$want" ]] || fail "variable not removed: $(app_state "$ID1")"
cp compose.orig wrangler-compose.yaml
wc --env ac-1 up >/dev/null
AC1=$(app_state "$ID1")

# Another environment gets its own app; ac-1's stays as it was.
wc_yes ac-2 up >/dev/null
ID2=$(app_id ac-2)
[[ -n "$ID2" && "$ID2" != "$ID1" ]] || fail "ac-2 has no app of its own"
[[ "$(app_state "$ID1")" == "$AC1" ]] || fail "ac-2 changed ac-1's app"
[[ "$(app_state "$ID2")" == *'"STORE_DOMAIN": ["shop-ac-2.example", false]'* ]] || fail "ac-2 groups: $(app_state "$ID2")"

# ps shows the app and its variables, by GET only, never a value.
n=$(lines)
out=$(wc --env ac-1 ps)
grep -q "^mobile .*codemagic present .*$ID1  shop  https://github.com/example/shop" <<<"$out" || fail "ps app: $out"
grep -q '^mobile.store .*STORE_APP_KEY  Secure' <<<"$out" || fail "ps Secure: $out"
grep -q '^mobile.store .*STORE_DOMAIN  has a value' <<<"$out" || fail "ps plain: $out"
grep -q 'new-ac-1.example' <<<"$out" && fail "ps showed a value"
no_secrets "$out" || fail "ps showed a secret: $out"
gets_only_since "$n" || fail "ps changed Codemagic"

# An id whose app is gone stops, and says how to register again.
out=$(CODEMAGIC_APP_ID=000000000000000000000000 wc --env ac-1 --dry-run up) && fail "ran with a missing app"
grep -q 'Codemagic has no app 000000000000000000000000' <<<"$out" || fail "missing app message: $out"

# down: the groups go before the app (an app deleted first leaves its groups
# undeletable, as the fake imitates), then the id line; ac-2 stays.
out=$(wc --env ac-1 --dry-run down)
grep -q "Codemagic app $ID1 (variable groups: .*); its build history and artifacts go too" <<<"$out" || fail "down dry run: $out"
if wc --env ac-1 down >/dev/null; then fail "down without the retype"; fi
[[ $(cm "DELETE /apps/$ID1") == 0 ]] || fail "deleted without the retype"
n=$(lines)
out=$(wc_yes ac-1 down) || fail "down: $out"
order=$(tail -n +"$((n + 1))" cm.log | grep -a -E '^DELETE ')
[[ $(head -1 <<<"$order") == "DELETE /api/v3/variable-groups/"* ]] || fail "down did not delete the groups first: $order"
[[ $(tail -1 <<<"$order") == "DELETE /apps/$ID1" ]] || fail "down did not delete the app last: $order"
grep -q CODEMAGIC_APP_ID .env.ac-1 && fail "CODEMAGIC_APP_ID line left"
state | python3 -c '
import json, sys
s = json.load(sys.stdin); a1, a2 = sys.argv[1:]
assert a1 not in s["apps"], "ac-1 app left"
assert not [g for g in s["groups"].values() if g["app"] == a1], "ac-1 groups left"
assert a2 in s["apps"], "ac-2 app deleted"' "$ID1" "$ID2" || fail "state after down"
# The fake keeps what the real Codemagic does: groups of an app deleted first
# cannot be deleted.
G=$(curl -fsS -H 'x-auth-token: cm-token' "$API/api/v3/apps/$ID2/variable-groups" | python3 -c 'import json,sys; print(json.load(sys.stdin)["data"][0]["id"])')
code=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE -H 'x-auth-token: cm-token' "$API/apps/$ID2")
[[ $code == 202 ]] || fail "fake app delete: $code"
code=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE -H 'x-auth-token: cm-token' "$API/api/v3/variable-groups/$G")
[[ $code == 500 ]] || fail "fake let an orphaned group go: $code"

# Another up brings ac-1 back with a new app.
wc_yes ac-1 up >/dev/null
ID3=$(app_id ac-1)
[[ -n "$ID3" && "$ID3" != "$ID1" ]] || fail "no new app after down"
[[ "$(app_state "$ID3")" == "$AC1" ]] || fail "not the same state after down and up: $(app_state "$ID3")"
echo "codemagic smoke test passed"
