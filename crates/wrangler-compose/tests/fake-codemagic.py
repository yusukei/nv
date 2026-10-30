"""A small stateful stand-in for Codemagic's API, for exercising
wrangler-compose's codemagic service offline (codemagic-smoke.sh). Both of
Codemagic's addresses are served here: the apps API at / and v3 at /api/v3
(CODEMAGIC_API_BASE_URL points the tool at both). It imitates what was
measured on the real Codemagic (2026-10-30): a Secure value comes back as
null, and an app deleted before its groups leaves them readable but
impossible to delete (500); a GitHub repository the GitHub App does not
reach (here: anything outside github.com/example/) is still registered, as
provider "generic". A GitHub repository sent as Codemagic's screen sends
it ("repo") is linked when the GitHub App reaches it (github.com/example/),
and a second one of the same repository hands back the first app. GitHub's
API is served at /github (GITHUB_API_BASE_URL): the example/ and elsewhere/
repositories, of which example/private-shop needs GH_TOKEN. The token
cm-readonly may only read. Lists come in pages of 2, so paging is
exercised. It is not a model of Codemagic. Logs every request to stderr;
GET /__state gives the whole state (Secure values too) for the test to
check."""
import json, re, sys, threading, uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TOKEN = "cm-token"
GH_TOKEN = "gh-token"
PAGE = 2
S = {"apps": {}, "groups": {}}  # app id -> {repo, name}; group id -> {app, name, vars[]}
LOCK = threading.Lock()


def new_id():
    return uuid.uuid4().hex[:24]


def v3_error(status, msg):
    return status, {"status_code": status, "detail": msg, "errors": [{"message": msg}]}


def paged(items, q):
    page = int(q.get("page", "1"))
    size = min(int(q.get("page_size", "30")), PAGE)
    total = max(1, (len(items) + size - 1) // size)
    return 200, {"data": items[(page - 1) * size: page * size], "page_size": size,
                 "current_page": page, "total_pages": total}


class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass

    def handle_any(self, method):
        n = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(n) if n else b""
        # The test's own probes (readiness, state) are not logged.
        if self.path not in ("/", "/__state"):
            sys.stderr.write(f"{method} {self.path}\n"); sys.stderr.flush()
        with LOCK:
            status, body, headers = route(method, self.path, raw, self.headers)
        data = b"" if body is None else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        for k, v in headers.items():
            self.send_header(k, v)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self): self.handle_any("GET")
    def do_POST(self): self.handle_any("POST")
    def do_PATCH(self): self.handle_any("PATCH")
    def do_DELETE(self): self.handle_any("DELETE")


def route(method, path, raw, headers):
    p, _, qs = path.partition("?")
    q = dict(x.split("=", 1) for x in qs.split("&") if "=" in x)
    if p == "/":
        return 200, {}, {}
    if p == "/__state":
        return 200, S, {}
    m = re.fullmatch(r"/github/repos/([^/]+)/([^/]+)", p)
    if m:
        return github(m.group(1), m.group(2), headers.get("authorization")) + ({},)
    token = headers.get("x-auth-token")
    if token == "cm-readonly" and method != "GET":
        return 403, {"message": "You do not have permission to perform this action"}, {}
    if token == "cm-ratelimited":
        return 429, {"message": "Too many requests"}, {"ratelimit-reset": "42"}
    if token not in (TOKEN, "cm-readonly"):
        return 401, {"message": "Authentication credentials were not provided or are invalid"}, {}
    try:
        j = json.loads(raw) if raw else {}
    except Exception:
        j = {}
    r = respond(method, p, q, j)
    return r[0], r[1], {}


def github(owner, name, auth):
    if owner not in ("example", "elsewhere"):
        return 404, {"message": "Not Found"}
    if auth is not None and auth != f"Bearer {GH_TOKEN}":
        return 401, {"message": "Bad credentials"}
    if name == "private-shop" and auth is None:
        return 404, {"message": "Not Found"}
    gid = int(uuid.uuid5(uuid.NAMESPACE_URL, f"{owner}/{name}").int % 10**9)
    return 200, {"id": gid, "name": name, "full_name": f"{owner}/{name}",
                 "owner": {"login": owner}, "private": name == "private-shop",
                 "html_url": f"https://github.com/{owner}/{name}",
                 "clone_url": f"https://github.com/{owner}/{name}.git",
                 "hooks_url": f"https://api.github.com/repos/{owner}/{name}/hooks",
                 "language": "Rust", "default_branch": "main"}


def app_json(i, app):
    return {"_id": i, "appName": app["name"],
            "repository": {"id": app["gid"], "htmlUrl": app["repo"], "provider": app["provider"]}}


def respond(method, p, q, j):
    # The apps API.
    if p == "/apps" and method == "GET":
        return 200, {"applications": [app_json(i, a) for i, a in S["apps"].items()]}
    if p == "/apps" and method == "POST" and "repo" in j:
        r = j["repo"]
        missing = [k for k in ("name", "gitUrl", "language", "hooksUrl") if not r.get(k)]
        if missing:
            return 400, {"error": "Invalid repository: missing required fields " + ", ".join(missing)}
        for i, a in S["apps"].items():
            if a["provider"] == "github" and a["gid"] == r.get("id"):
                return 200, {"_id": i, "appName": a["name"]}
        i = new_id()
        linked = r.get("provider") == "github" and (r.get("owner") or {}).get("name") == "example"
        S["apps"][i] = {"repo": r.get("htmlUrl"), "name": r["name"], "gid": r.get("id"),
                        "provider": "github" if linked else "generic"}
        return 200, {"_id": i, "appName": r["name"]}
    if p == "/apps" and method == "POST":
        repo = j.get("repositoryUrl") or ""
        if not repo:
            return 400, {"message": "repositoryUrl is required"}
        i = new_id()
        name = re.sub(r"\.git$", "", repo.rstrip("/").rsplit("/", 1)[-1])
        linked = repo.startswith("https://github.com/example/")
        S["apps"][i] = {"repo": repo, "name": name, "gid": None,
                        "provider": "github" if linked else "generic"}
        return 200, {"_id": i, "appName": name}
    m = re.fullmatch(r"/apps/([^/]+)", p)
    if m:
        app = S["apps"].get(m.group(1))
        if app is None:
            return 404, {"message": "Application not found"}
        if method == "GET":
            return 200, {"application": app_json(m.group(1), app)}
        if method == "DELETE":
            # The groups stay behind (see the docstring).
            del S["apps"][m.group(1)]
            return 202, None
    # v3
    m = re.fullmatch(r"/api/v3/apps/([^/]+)/variable-groups", p)
    if m:
        if m.group(1) not in S["apps"]:
            return v3_error(404, "Application not found")
        if method == "GET":
            mine = [{"id": k, "name": g["name"]} for k, g in S["groups"].items() if g["app"] == m.group(1)]
            return paged(mine, q)
        if method == "POST":
            name = j.get("name") or ""
            if not name or "." in name or "$" in name:
                return v3_error(400, "name does not match ^[^.$]*$")
            i = new_id()
            S["groups"][i] = {"app": m.group(1), "name": name, "vars": []}
            return 201, {"data": {"id": i, "name": name}}
    m = re.fullmatch(r"/api/v3/variable-groups/([^/]+)", p)
    if m and method == "DELETE":
        g = S["groups"].get(m.group(1))
        if g is None:
            return v3_error(404, "Variable group not found")
        if g["app"] not in S["apps"]:
            return v3_error(500, "Internal server error")
        del S["groups"][m.group(1)]
        return 204, None
    m = re.fullmatch(r"/api/v3/variable-groups/([^/]+)/variables(?:/([^/]+))?", p)
    if m:
        g = S["groups"].get(m.group(1))
        if g is None:
            return v3_error(404, "Variable group not found")
        vid = m.group(2)
        if vid is None and method == "GET":
            out = [{"id": v["id"], "name": v["name"], "value": None if v["secure"] else v["value"],
                    "secure": v["secure"]} for v in g["vars"]]
            return paged(out, q)
        if vid is None and method == "POST":
            for v in j.get("variables", []):
                if any(x["name"] == v["name"] for x in g["vars"]):
                    return v3_error(400, f"variable {v['name']} already exists")
            for v in j.get("variables", []):
                g["vars"].append({"id": new_id(), "name": v["name"], "value": v["value"],
                                  "secure": bool(j.get("secure"))})
            return 201, None
        var = next((v for v in g["vars"] if v["id"] == vid), None)
        if var is None:
            return v3_error(404, "Variable not found")
        if method == "PATCH":
            for k in ("name", "value", "secure"):
                if j.get(k) is not None:
                    var[k] = j[k]
            return 204, None
        if method == "DELETE":
            g["vars"].remove(var)
            return 204, None
    return v3_error(404, f"no route {method} {p}")


if __name__ == "__main__":
    port = int(sys.argv[1])
    ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
