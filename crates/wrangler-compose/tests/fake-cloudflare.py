"""A small stateful stand-in for Cloudflare's API, for exercising
wrangler-compose's remote flow offline (remote-smoke.sh). It answers what
wrangler-compose and `wrangler deploy` / `d1 migrations apply` ask, as far
as the test needs; it is not a model of Cloudflare. Logs every request to
stderr."""
import json, re, sys, uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ACC = "acc"
S = {
    "scripts": {},      # name -> {migration_tag, classes, secrets{}, subdomain}
    "d1": {},           # name -> uuid
    "d1_applied": {},   # uuid -> [names]
    "kv": {},           # title -> id
    "r2": {},           # (jur,name) -> {cors, managed, lifecycle, lock, notif{qid:rules}}
    "queues": {},       # name -> {id, consumers[]}
}
LOG = []

def ok(result=None, extra=None):
    body = {"success": True, "errors": [], "messages": [], "result": result}
    if extra: body.update(extra)
    return 200, body

def err(status, msg, code=10000):
    return status, {"success": False, "errors": [{"code": code, "message": msg}], "messages": [], "result": None}

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass

    def handle_any(self, method):
        n = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(n) if n else b""
        path = self.path
        sys.stderr.write(f"{method} {path}\n"); sys.stderr.flush()
        LOG.append(f"{method} {path}")
        status, body = route(method, path, raw, self.headers)
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self): self.handle_any("GET")
    def do_POST(self): self.handle_any("POST")
    def do_PUT(self): self.handle_any("PUT")
    def do_PATCH(self): self.handle_any("PATCH")
    def do_DELETE(self): self.handle_any("DELETE")

def metadata_of(raw, headers):
    ct = headers.get("content-type", "")
    m = re.search(r"boundary=(.+)", ct)
    if not m: return {}
    b = m.group(1).strip('"').encode()
    for part in raw.split(b"--" + b):
        if b'name="metadata"' in part:
            body = part.split(b"\r\n\r\n", 1)[1].rsplit(b"\r\n", 1)[0]
            try: return json.loads(body)
            except Exception: return {}
    return {}

def route(method, path, raw, headers):
    p, _, q = path.partition("?")
    try:
        j = json.loads(raw) if raw else None
    except Exception:
        j = None
    if p == "/client/v4/accounts" or p == "/accounts":
        return ok([{"id": ACC, "name": "Test"}], {"result_info": {"page": 1, "total_pages": 1}})
    if p in ("/client/v4/user", "/user"):
        return ok({"id": "u", "email": "t@example.com"})
    if p.startswith("/client/v4"): p = p[len("/client/v4"):]
    a = f"/accounts/{ACC}"
    if not p.startswith(a):
        return ok({})
    r = p[len(a):]
    jur = headers.get("cf-r2-jurisdiction", "default")
    info = {"result_info": {"page": 1, "per_page": 100, "total_pages": 1}}
    # workers
    if r == "/workers/subdomain":
        return ok({"subdomain": "testsub"})
    if r == "/workers/scripts" and method == "GET":
        return ok([{"id": k, "migration_tag": v.get("migration_tag", "")} for k, v in S["scripts"].items()], info)
    if r == "/workers/durable_objects/namespaces":
        out = []
        for k, v in S["scripts"].items():
            for c in v.get("classes", []):
                out.append({"id": k + c, "script": k, "class": c, "name": f"{k}_{c}"})
        return ok(out, info)
    m = re.fullmatch(r"/workers/scripts/([^/]+)(/.*)?", r)
    if m:
        name, rest = m.group(1), m.group(2) or ""
        sc = S["scripts"].get(name)
        if rest == "" and method == "PUT":
            meta = metadata_of(raw, headers)
            s = S["scripts"].setdefault(name, {"classes": [], "secrets": {}})
            mig = meta.get("migrations")
            if mig:
                steps = mig.get("steps") or [mig]
                for st in steps:
                    for c in st.get("new_sqlite_classes", []) + st.get("new_classes", []):
                        if c not in s["classes"]: s["classes"].append(c)
                    for c in st.get("deleted_classes", []):
                        if c in s["classes"]: s["classes"].remove(c)
                s["migration_tag"] = mig.get("new_tag", s.get("migration_tag", ""))
            for b in meta.get("bindings", []):
                if b.get("type") == "secret_text":
                    s["secrets"][b["name"]] = b.get("text")
            s["meta"] = meta
            ex = meta.get("exports") or meta.get("durable_object_exports") or {}
            if isinstance(ex, dict):
                for c, e in ex.items():
                    if e.get("state", "created") == "created" and c not in s["classes"]: s["classes"].append(c)
                    if e.get("state") == "deleted" and c in s["classes"]: s["classes"].remove(c)
            sys.stderr.write("  META " + json.dumps({k: v for k, v in meta.items() if k not in ("bindings",)})[:800] + "\n")
            return ok({"id": name, "etag": "e", "deployment_id": "d1", "startup_time_ms": 1})
        if rest == "" and method == "DELETE":
            if sc is None: return err(404, "not found", 10007)
            # As on Cloudflare: a queue consumer cannot be deleted.
            if any(c.get("script_name") == name for q in S["queues"].values() for c in q["consumers"]):
                return err(403, "Cannot delete this Worker as it is a consumer for a Queue. Remove it from the Queue's consumers first, then retry.", 10064)
            del S["scripts"][name]; return ok(None)
        if rest == "" and method == "GET":
            if sc is None: return err(404, "not found", 10007)
            return ok({})
        if rest == "/secrets" and method == "GET":
            if sc is None: return err(404, "not found", 10007)
            return ok([{"name": k, "type": "secret_text"} for k in sc["secrets"]])
        if rest == "/secrets" and method == "PUT":
            sc["secrets"][j["name"]] = j["text"]; return ok({"name": j["name"], "type": "secret_text"})
        m2 = re.fullmatch(r"/secrets/([^/]+)", rest)
        if m2 and method == "DELETE":
            sc["secrets"].pop(m2.group(1), None); return ok(None)
        if rest == "/subdomain":
            return ok({"enabled": True, "previews_enabled": False})
        if rest == "/settings":
            return ok({"bindings": (sc or {}).get("meta", {}).get("bindings", []), "tags": []})
        if rest == "/deployments" and method == "GET":
            return ok({"deployments": []})
        if rest == "/schedules":
            return ok({"schedules": (j or {}).get("schedules", []) if isinstance(j, dict) else (j or [])})
        if rest.startswith("/versions") and method == "GET":
            return ok({"items": []})
        if rest.startswith("/versions") and method == "POST":
            meta = metadata_of(raw, headers)
            s = S["scripts"].setdefault(name, {"classes": [], "secrets": {}})
            ex = meta.get("exports") or {}
            for c, e in ex.items():
                if e.get("state", "created") == "created" and c not in s["classes"]: s["classes"].append(c)
                if e.get("state") == "deleted" and c in s["classes"]: s["classes"].remove(c)
            sys.stderr.write("  META(version) " + json.dumps({k: v for k, v in meta.items() if k != "bindings"})[:600] + "\n")
            return ok({"id": "v1", "number": 2, "metadata": {"has_preview": False},
                       "resources": {"bindings": meta.get("bindings", []), "script": {"etag": "e", "handlers": ["fetch"]},
                                     "script_runtime": {"usage_model": "standard"}}})
        if rest == "/deployments" and method == "POST":
            return ok({"id": "dep", "source": "wrangler", "strategy": "percentage",
                       "versions": [{"version_id": "v1", "percentage": 100}], "created_on": "2026-10-12T00:00:00Z"})
        return ok({})
    m = re.fullmatch(r"/workers/workers/([^/]+)", r)
    if m:
        name = m.group(1)
        return ok({"id": name, "name": name, "subdomain": {"enabled": True, "previews_enabled": False},
                   "observability": {"enabled": False}, "logpush": False, "tags": [], "tail_consumers": []})
    if r.startswith("/workers/services/"):
        name = r.split("/")[3]
        if name not in S["scripts"]: return err(404, "This Worker does not exist on your account.", 10090)
        return ok({"default_environment": {"environment": "production", "script": {"id": name}}})
    # d1
    if r == "/d1/database" and method == "GET":
        return ok([{"name": k, "uuid": v} for k, v in S["d1"].items()], info)
    if r == "/d1/database" and method == "POST":
        u = str(uuid.uuid4()); S["d1"][j["name"]] = u; return ok({"name": j["name"], "uuid": u})
    m = re.fullmatch(r"/d1/database/([^/]+)(/.*)?", r)
    if m:
        u, rest = m.group(1), m.group(2) or ""
        if rest == "" and method == "DELETE":
            for k, v in list(S["d1"].items()):
                if v == u: del S["d1"][k]
            return ok(None)
        if rest == "" and method == "GET":
            name = next((k for k, v in S["d1"].items() if v == u), None)
            return ok({"uuid": u, "name": name, "version": "production"})
        if rest in ("/query", "/raw"):
            sql = j.get("sql", "")
            sys.stderr.write("  SQL " + sql.replace("\n", " ")[:300] + "\n")
            if "d1_migrations" in sql and sql.strip().upper().startswith("SELECT"):
                sel_name = "name" if "SELECT name" in sql else "*"
                if u not in S["d1_applied"]:
                    return err(400, "no such table: d1_migrations: SQLITE_ERROR", 7500)
                return ok([{"results": [{"name": n} for n in S["d1_applied"][u]], "success": True, "meta": {}}])
            if "FAIL_HERE" in sql:
                return err(400, "near FAIL_HERE: syntax error: SQLITE_ERROR", 7500)
            # migrations apply: record names inserted
            for n in re.findall(r"INSERT INTO \"?d1_migrations\"?\s*\(name\)\s*values\s*\('([^']+)'\)", sql, re.I):
                S["d1_applied"].setdefault(u, []).append(n)
            if re.search(r'CREATE TABLE IF NOT EXISTS "?d1_migrations', sql):
                S["d1_applied"].setdefault(u, [])
            return ok([{"results": [], "success": True, "meta": {"changes": 0}}])
        return ok({})
    # kv
    if r == "/storage/kv/namespaces" and method == "GET":
        return ok([{"title": k, "id": v} for k, v in S["kv"].items()], info)
    if r == "/storage/kv/namespaces" and method == "POST":
        i = uuid.uuid4().hex; S["kv"][j["title"]] = i; return ok({"title": j["title"], "id": i})
    m = re.fullmatch(r"/storage/kv/namespaces/([^/]+)", r)
    if m and method == "DELETE":
        for k, v in list(S["kv"].items()):
            if v == m.group(1): del S["kv"][k]
        return ok(None)
    # r2
    if r == "/r2/buckets" and method == "GET":
        return ok({"buckets": [{"name": n} for (jj, n) in S["r2"] if jj == jur]}, {"result_info": {"cursor": ""}})
    if r == "/r2/buckets" and method == "POST":
        S["r2"][(jur, j["name"])] = {"cors": None, "managed": False, "lifecycle": [{"id": "Default Multipart Abort Rule", "enabled": True, "conditions": {"prefix": ""}, "abortMultipartUploadsTransition": {"condition": {"maxAge": 604800, "type": "Age"}}}], "lock": [], "notif": {}}
        return ok({"name": j["name"]})
    m = re.fullmatch(r"/r2/buckets/([^/]+)(/.*)?", r)
    if m:
        b = S["r2"].get((jur, m.group(1))); rest = m.group(2) or ""
        if b is None: return err(404, "The specified bucket does not exist.", 10006)
        if rest == "" and method == "DELETE": del S["r2"][(jur, m.group(1))]; return ok({})
        if rest == "/cors":
            if method == "GET":
                return err(404, "The CORS configuration does not exist.", 10059) if not b["cors"] else ok({"rules": b["cors"]})
            if method == "PUT": b["cors"] = j["rules"]; return ok({})
            if method == "DELETE": b["cors"] = None; return ok({})
        if rest == "/domains/managed":
            if method == "GET": return ok({"bucketId": "x", "domain": "pub-x.r2.dev", "enabled": b["managed"]})
            b["managed"] = j["enabled"]; return ok({"bucketId": "x", "domain": "pub-x.r2.dev", "enabled": b["managed"]})
        if rest in ("/lifecycle", "/lock"):
            key = rest[1:]
            if method == "GET": return ok({"rules": b[key]})
            b[key] = j["rules"]; return ok({})
        return ok({})
    m = re.fullmatch(r"/event_notifications/r2/([^/]+)/configuration(?:/queues/([^/]+))?", r)
    if m:
        b = S["r2"].get((jur, m.group(1)))
        if b is None: return err(404, "no bucket", 10006)
        if m.group(2) is None:
            # As on Cloudflare: R2 gives queue ids with dashes, Queues without.
            qs = [{"queueId": str(uuid.UUID(qid)), "queueName": next((n for n, q in S["queues"].items() if q["id"] == qid), "?"), "rules": rules} for qid, rules in b["notif"].items()]
            return ok({"bucketName": m.group(1), "queues": qs})
        qid = m.group(2).replace("-", "")
        if method == "PUT": b["notif"][qid] = j["rules"]; return ok({})
        if method == "DELETE": b["notif"].pop(qid, None); return ok({})
    # queues
    if r == "/queues" and method == "GET":
        return ok([{"queue_name": k, "queue_id": v["id"], "consumers": v["consumers"]} for k, v in S["queues"].items()], info)
    if r == "/queues" and method == "POST":
        i = uuid.uuid4().hex; S["queues"][j["queue_name"]] = {"id": i, "consumers": []}
        return ok({"queue_name": j["queue_name"], "queue_id": i})
    m = re.fullmatch(r"/queues/([^/]+)(/consumers(?:/([^/]+))?)?", r)
    if m:
        q = next(((n, v) for n, v in S["queues"].items() if v["id"] == m.group(1) or n == m.group(1)), None)
        if q is None: return err(404, "queue not found", 11000)
        if m.group(2) is None:
            if method == "DELETE":
                # As on Cloudflare: a queue that R2 notifies cannot be deleted.
                if any(q[1]["id"] in b["notif"] for b in S["r2"].values()):
                    return err(400, f"Cannot delete queue '{q[0]}' that serves as a target for event notifications.", 11017)
                del S["queues"][q[0]]; return ok(None)
            return ok({"queue_name": q[0], "queue_id": q[1]["id"], "consumers": q[1]["consumers"]})
        if method == "GET": return ok(q[1]["consumers"])
        if method == "POST":
            c = {"consumer_id": uuid.uuid4().hex, "script_name": j.get("script_name"), "type": "worker", "queue_name": q[0]}
            q[1]["consumers"].append(c); return ok(c)
        if method == "DELETE":
            q[1]["consumers"] = [c for c in q[1]["consumers"] if c["consumer_id"] != m.group(3)]; return ok(None)
        if method == "PUT": return ok({})
    return ok({})

if __name__ == "__main__":
    port = int(sys.argv[1])
    ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
