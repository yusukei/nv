//! The Wrangler configs the tool makes (DES-005): users never write them.
//! Locally every resource is named by its key and every Worker by its
//! service name (REQ-006).

use crate::build::Built;
use crate::compose::{Compose, Kind, Resource, Target, Worker};
use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Cloudflare's defaults for queue consumers; the local simulator's own
/// defaults (5, 1 s, 2) differ (DES-008).
const QUEUE_DEFAULTS: (u64, u64, u64) = (10, 5, 3);

pub const GATEWAY: &str = "wc-gateway";

pub fn logs_worker(svc: &str) -> String {
    format!("wc-logs-{svc}")
}

/// The local config of one user Worker, running its built bundle.
pub fn local_worker(c: &Compose, svc: &str, w: &Worker, built: &Built) -> Value {
    let mut cfg = Map::new();
    cfg.insert("name".into(), json!(svc));
    cfg.insert("main".into(), json!(built.dir.join(&built.entry)));
    cfg.insert("no_bundle".into(), json!(true));
    cfg.insert("compatibility_date".into(), json!(w.compatibility_date));
    if !w.compatibility_flags.is_empty() {
        cfg.insert("compatibility_flags".into(), json!(w.compatibility_flags));
    }
    if !w.vars.is_empty() {
        cfg.insert("vars".into(), Value::Object(w.vars.clone()));
    }
    let mut d1 = Vec::new();
    let mut r2 = Vec::new();
    let mut kv = Vec::new();
    let mut producers = Vec::new();
    let mut services = Vec::new();
    let mut dos = Vec::new();
    for (binding, target) in &w.bindings {
        match target {
            Target::D1(k) => {
                d1.push(json!({ "binding": binding, "database_name": k, "database_id": k }))
            }
            Target::R2(k) => {
                let mut b = json!({ "binding": binding, "bucket_name": k });
                // The S3-compatible API's keys (REQ-009): random per project,
                // never fixed ones anyone could know.
                if let Some(s3) = &c.local_s3 {
                    b["local_dev"] = json!({ "experimental_s3_credentials": {
                        "accessKeyId": s3.key, "secretAccessKey": s3.secret } });
                }
                r2.push(b)
            }
            Target::Kv(k) => kv.push(json!({ "binding": binding, "id": k })),
            Target::Queue(k) => producers.push(json!({ "binding": binding, "queue": k })),
            Target::Worker(other) => services.push(json!({ "binding": binding, "service": other })),
            Target::DurableObject { service, class } => {
                let mut b = json!({ "name": binding, "class_name": class });
                if service != svc {
                    b["script_name"] = json!(service);
                }
                dos.push(b);
            }
        }
    }
    // Classes this Worker owns but does not bind itself still need a
    // binding locally so other Workers can reach them.
    for class in &w.durable_objects {
        let bound = w.bindings.iter().any(|(_, t)| {
            matches!(t, Target::DurableObject { service, class: cl } if service == svc && cl == class)
        });
        if !bound {
            dos.push(json!({ "name": format!("__WC_DO_{class}"), "class_name": class }));
        }
    }
    if !d1.is_empty() {
        cfg.insert("d1_databases".into(), json!(d1));
    }
    if !r2.is_empty() {
        cfg.insert("r2_buckets".into(), json!(r2));
    }
    if !kv.is_empty() {
        cfg.insert("kv_namespaces".into(), json!(kv));
    }
    if !services.is_empty() {
        cfg.insert("services".into(), json!(services));
    }
    let consumers: Vec<Value> = w
        .consumes
        .iter()
        .map(|q| {
            json!({ "queue": q, "max_batch_size": QUEUE_DEFAULTS.0,
                    "max_batch_timeout": QUEUE_DEFAULTS.1, "max_retries": QUEUE_DEFAULTS.2 })
        })
        .collect();
    if !producers.is_empty() || !consumers.is_empty() {
        let mut q = Map::new();
        if !producers.is_empty() {
            q.insert("producers".into(), json!(producers));
        }
        if !consumers.is_empty() {
            q.insert("consumers".into(), json!(consumers));
        }
        cfg.insert("queues".into(), Value::Object(q));
    }
    if !dos.is_empty() {
        cfg.insert("durable_objects".into(), json!({ "bindings": dos }));
    }
    if let Some(e) = crate::remote::do_exports(&w.durable_objects, None, &[]) {
        cfg.insert("exports".into(), e);
    }
    if let Some(dir) = &w.assets {
        cfg.insert("assets".into(), json!({ "directory": dir }));
    }
    if !w.crons.is_empty() {
        cfg.insert("triggers".into(), json!({ "crons": w.crons }));
    }
    cfg.insert(
        "tail_consumers".into(),
        json!([{ "service": logs_worker(svc) }]),
    );
    let _ = c;
    Value::Object(cfg)
}

/// The Cloudflare config of one user Worker (DES-005, DES-009): its built
/// bundle as it is (`no_bundle`), bindings by actual names and IDs, and the
/// Durable Object classes it declares (DES-010). It is reachable
/// on workers.dev only when it has `ports` (it is meant to be reached from
/// outside) and no routes of its own.
pub fn remote_worker(
    svc: &str,
    w: &Worker,
    built: &Built,
    ids: &Ids,
    account: &str,
    exports: Option<Value>,
) -> Value {
    let mut cfg = Map::new();
    cfg.insert("name".into(), json!(ids.worker(svc)));
    cfg.insert("account_id".into(), json!(account));
    cfg.insert("main".into(), json!(built.dir.join(&built.entry)));
    cfg.insert("no_bundle".into(), json!(true));
    // Other files the build wrote next to the entry (chunks, wasm); its
    // JavaScript is ES modules, as the bundle is.
    cfg.insert("find_additional_modules".into(), json!(true));
    cfg.insert("base_dir".into(), json!(built.dir));
    cfg.insert(
        "rules".into(),
        json!([{ "type": "ESModule", "globs": ["**/*.js", "**/*.mjs"] }]),
    );
    cfg.insert("compatibility_date".into(), json!(w.compatibility_date));
    if !w.compatibility_flags.is_empty() {
        cfg.insert("compatibility_flags".into(), json!(w.compatibility_flags));
    }
    cfg.insert(
        "workers_dev".into(),
        json!(!w.ports.is_empty() && w.routes.is_empty()),
    );
    cfg.insert("preview_urls".into(), json!(false));
    if !w.vars.is_empty() {
        cfg.insert("vars".into(), Value::Object(w.vars.clone()));
    }
    if !w.routes.is_empty() {
        let routes: Vec<Value> = w.routes.iter().map(|r| route(r)).collect();
        cfg.insert("routes".into(), json!(routes));
    }
    let mut d1 = Vec::new();
    let mut r2 = Vec::new();
    let mut kv = Vec::new();
    let mut producers = Vec::new();
    let mut services = Vec::new();
    let mut dos = Vec::new();
    for (binding, target) in &w.bindings {
        match target {
            Target::D1(k) => {
                let (name, id) = ids.d1(k);
                d1.push(json!({ "binding": binding, "database_name": name, "database_id": id }))
            }
            Target::R2(k) => {
                let mut b = ids.r2(k);
                b["binding"] = json!(binding);
                r2.push(b);
            }
            Target::Kv(k) => kv.push(json!({ "binding": binding, "id": ids.kv(k) })),
            Target::Queue(k) => {
                producers.push(json!({ "binding": binding, "queue": ids.queue(k) }))
            }
            Target::Worker(other) => {
                services.push(json!({ "binding": binding, "service": ids.worker(other) }))
            }
            Target::DurableObject { service, class } => {
                let mut b = json!({ "name": binding, "class_name": class });
                if service != svc {
                    b["script_name"] = json!(ids.worker(service));
                }
                dos.push(b);
            }
        }
    }
    if !d1.is_empty() {
        cfg.insert("d1_databases".into(), json!(d1));
    }
    if !r2.is_empty() {
        cfg.insert("r2_buckets".into(), json!(r2));
    }
    if !kv.is_empty() {
        cfg.insert("kv_namespaces".into(), json!(kv));
    }
    if !services.is_empty() {
        cfg.insert("services".into(), json!(services));
    }
    // Consumers keep Cloudflare's own defaults (the compose file has no
    // settings for them).
    let consumers: Vec<Value> = w
        .consumes
        .iter()
        .map(|q| json!({ "queue": ids.queue(q) }))
        .collect();
    if !producers.is_empty() || !consumers.is_empty() {
        let mut q = Map::new();
        if !producers.is_empty() {
            q.insert("producers".into(), json!(producers));
        }
        if !consumers.is_empty() {
            q.insert("consumers".into(), json!(consumers));
        }
        cfg.insert("queues".into(), Value::Object(q));
    }
    if !dos.is_empty() {
        cfg.insert("durable_objects".into(), json!({ "bindings": dos }));
    }
    if let Some(e) = exports {
        cfg.insert("exports".into(), e);
    }
    if let Some(dir) = &w.assets {
        cfg.insert("assets".into(), json!({ "directory": dir }));
    }
    // An empty list clears crons set by an earlier deploy.
    cfg.insert("triggers".into(), json!({ "crons": w.crons }));
    Value::Object(cfg)
}

/// `example.com/*` is a route on a zone; a bare host name is a custom
/// domain (Cloudflare makes its DNS record and certificate).
fn route(r: &str) -> Value {
    if r.contains('/') || r.contains('*') {
        json!({ "pattern": r })
    } else {
        json!({ "pattern": r, "custom_domain": true })
    }
}

/// The config `d1 migrations apply --remote` reads for each database.
pub fn remote_migrate(
    dir: &Path,
    key: &str,
    migrations: &Path,
    ids: &Ids,
    account: &str,
) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let (name, id) = ids.d1(key);
    let script = dir.join("index.js");
    std::fs::write(
        &script,
        "export default { fetch() { return new Response(null, { status: 404 }) } }\n",
    )?;
    let cfg = json!({ "name": "wc-migrate", "account_id": account, "main": script,
        "compatibility_date": "2026-09-01",
        "d1_databases": [{ "binding": "DB", "database_name": name, "database_id": id,
            "migrations_dir": migrations }] });
    let path = dir.join("wrangler.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&cfg)?)?;
    Ok(path)
}

/// `.dev.vars` for a Worker's secrets (REQ-005): dotenv with quoting.
/// A value as Wrangler's `.dev.vars` reader (dotenv) gives it back exactly:
/// single quotes take everything literally (newlines too) but a `'`;
/// backticks likewise but a `` ` ``; double quotes turn `\n` into a newline.
fn dev_vars_value(v: &str) -> String {
    if !v.contains('\'') {
        format!("'{v}'")
    } else if !v.contains('`') {
        format!("`{v}`")
    } else {
        // Holds both ' and `: double quotes, where only \n and \r change.
        format!("\"{}\"", v.replace('\n', "\\n").replace('\r', "\\r"))
    }
}

pub fn dev_vars(w: &Worker) -> String {
    let mut out = String::new();
    for (name, value) in &w.secrets {
        out.push_str(&format!("{name}={}\n", dev_vars_value(value)));
    }
    out
}

/// The gateway (REQ-017, DES-008): the one Worker reachable from outside,
/// sending each request to the Worker that owns the port it came in on.
/// Requests to `/__wc/` on its own internal port go to the data service.
pub fn gateway(dir: &Path, ports: &[(u16, String)], internal_port: u16) -> Result<Value> {
    let script = dir.join("index.js");
    std::fs::write(&script, GATEWAY_JS)
        .with_context(|| crate::tr!("{} を書けません", "cannot write {}", script.display()))?;
    let map: Map<String, Value> = ports
        .iter()
        .enumerate()
        .map(|(i, (p, _))| (p.to_string(), json!(format!("S{i}"))))
        .collect();
    let mut services: Vec<Value> = ports
        .iter()
        .enumerate()
        .map(|(i, (_, svc))| json!({ "binding": format!("S{i}"), "service": svc }))
        .collect();
    services.push(json!({ "binding": "WC_DATA", "service": DATA }));
    Ok(json!({
        "name": GATEWAY,
        "main": script,
        "compatibility_date": "2026-09-01",
        "vars": { "WC_PORTS": Value::Object(map), "WC_INTERNAL_PORT": internal_port.to_string() },
        "services": services,
    }))
}

pub const DATA: &str = "wc-data";

/// The data service (REQ-017, DES-011): reads and writes this
/// environment's D1, R2 and KV from inside it, for backup, restore, purge
/// and counts. It answers only with the token, written to `.dev.vars`.
/// Actual names and IDs of a remote environment's resources (DES-004).
/// Locally every resource is named and identified by its key instead.
#[derive(Debug, Default, Clone)]
pub struct Ids {
    /// D1 key → (name, UUID).
    pub d1: BTreeMap<String, (String, String)>,
    /// R2 key → (bucket name, jurisdiction).
    pub r2: BTreeMap<String, (String, Option<String>)>,
    /// KV key → namespace ID.
    pub kv: BTreeMap<String, String>,
    /// Queue key → queue name.
    pub queues: BTreeMap<String, String>,
    /// Worker service → script name.
    pub workers: BTreeMap<String, String>,
}

impl Ids {
    fn d1(&self, key: &str) -> (String, String) {
        self.d1
            .get(key)
            .cloned()
            .unwrap_or_else(|| (key.to_string(), key.to_string()))
    }
    fn r2(&self, key: &str) -> Value {
        match self.r2.get(key) {
            Some((name, Some(j))) => json!({ "bucket_name": name, "jurisdiction": j }),
            Some((name, None)) => json!({ "bucket_name": name }),
            None => json!({ "bucket_name": key }),
        }
    }
    fn kv(&self, key: &str) -> String {
        self.kv.get(key).cloned().unwrap_or_else(|| key.to_string())
    }
    fn queue(&self, key: &str) -> String {
        self.queues
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.to_string())
    }
    fn worker(&self, svc: &str) -> String {
        self.workers
            .get(svc)
            .cloned()
            .unwrap_or_else(|| svc.to_string())
    }
}

/// The data service (DES-011). `remote` gives the actual names and IDs;
/// locally (None) it also sends R2 event notifications, which Cloudflare
/// sends by itself remotely.
pub fn data_service(c: &Compose, dir: &Path, remote: Option<&Ids>) -> Result<Value> {
    let local_ids = Ids::default();
    let ids = remote.unwrap_or(&local_ids);
    let script = dir.join("index.js");
    std::fs::write(&script, DATA_JS)?;
    let (mut d1, mut r2, mut kv) = (Vec::new(), Vec::new(), Vec::new());
    let (mut d1m, mut r2m, mut kvm) = (Map::new(), Map::new(), Map::new());
    let mut notify = Vec::new();
    let mut producers: Vec<Value> = Vec::new();
    for s in &c.services {
        match &s.kind {
            // Remotely, only resources that exist get a binding.
            Kind::D1(list) => {
                for r in list {
                    if remote.is_some() && !ids.d1.contains_key(&r.key) {
                        continue;
                    }
                    let b = format!("D1_{}", d1.len());
                    let (name, id) = ids.d1(&r.key);
                    d1.push(json!({ "binding": b, "database_name": name, "database_id": id }));
                    d1m.insert(r.key.clone(), json!(b));
                }
            }
            Kind::R2(list) => {
                for r in list {
                    if remote.is_some() && !ids.r2.contains_key(&r.key) {
                        continue;
                    }
                    let b = format!("R2_{}", r2.len());
                    let mut entry = ids.r2(&r.key);
                    entry["binding"] = json!(b);
                    r2.push(entry);
                    r2m.insert(r.key.clone(), json!(b));
                    if remote.is_some() {
                        continue;
                    }
                    for n in &r.spec.notifications {
                        let q = format!("Q_{}", producers.len());
                        producers.push(json!({ "binding": q, "queue": n.queue }));
                        notify.push(json!({ "bucket": b, "name": r.key, "queue": q,
                            "queue_key": n.queue, "events": n.events,
                            "prefix": n.prefix, "suffix": n.suffix }));
                    }
                }
            }
            Kind::Kv(list) => {
                for r in list {
                    if remote.is_some() && !ids.kv.contains_key(&r.key) {
                        continue;
                    }
                    let b = format!("KV_{}", kv.len());
                    kv.push(json!({ "binding": b, "id": ids.kv(&r.key) }));
                    kvm.insert(r.key.clone(), json!(b));
                }
            }
            _ => {}
        }
    }
    let mut cfg = json!({
        "name": DATA,
        "main": script,
        "compatibility_date": "2026-09-01",
        "vars": { "WC_MAP": { "d1": d1m, "r2": r2m, "kv": kvm }, "WC_NOTIFY": notify },
    });
    if !producers.is_empty() {
        cfg["queues"] = json!({ "producers": producers });
    }
    if !d1.is_empty() {
        cfg["d1_databases"] = json!(d1);
    }
    if !r2.is_empty() {
        cfg["r2_buckets"] = json!(r2);
    }
    if remote.is_none() {
        // The tool's own namespace for local R2 notifications (no compose
        // resource can be named like it: wc- names are the tool's).
        kv.push(json!({ "binding": "WC_STATE", "id": "wc-notify-state" }));
    }
    if !kv.is_empty() {
        cfg["kv_namespaces"] = json!(kv);
    }
    Ok(cfg)
}

/// The per-service log receiver (DES-013): tail events carry no script
/// name locally, so each service gets its own receiver that knows it.
pub fn logs(dir: &Path, svc: &str) -> Result<Value> {
    let script = dir.join("index.js");
    std::fs::write(&script, LOGS_JS)?;
    Ok(json!({
        "name": logs_worker(svc),
        "main": script,
        "compatibility_date": "2026-09-01",
        "vars": { "WC_SERVICE": svc },
    }))
}

/// A config listing every D1 with migrations, used only to apply them.
pub fn migrate(c: &Compose, dir: &Path) -> Result<Option<(PathBuf, Vec<String>)>> {
    let mut dbs = Vec::new();
    let mut names = Vec::new();
    for s in &c.services {
        if let Kind::D1(list) = &s.kind {
            for (i, r) in list.iter().enumerate() {
                let r: &Resource<crate::compose::D1> = r;
                if let Some(m) = &r.spec.migrations {
                    dbs.push(
                        json!({ "binding": format!("DB_{}_{i}", s.name.replace('-', "_")),
                        "database_name": r.key, "database_id": r.key, "migrations_dir": m }),
                    );
                    names.push(r.key.clone());
                }
            }
        }
    }
    if dbs.is_empty() {
        return Ok(None);
    }
    std::fs::create_dir_all(dir)?;
    let script = dir.join("index.js");
    std::fs::write(
        &script,
        "export default { fetch() { return new Response(null, { status: 404 }) } }\n",
    )?;
    let cfg = json!({ "name": "wc-migrate", "main": script, "compatibility_date": "2026-09-01", "d1_databases": dbs });
    let path = dir.join("wrangler.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&cfg)?)?;
    Ok(Some((path, names)))
}

const GATEWAY_JS: &str = r#"// Made by wrangler-compose: routes each local port to its service.
export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    // Requests from a service's port carry the port they came in on; the
    // header is the tool's, so the Worker never sees it.
    const from = request.headers.get("x-wrangler-compose-port");
    if (from === null) {
      if (url.port === env.WC_INTERNAL_PORT && url.pathname.startsWith("/__wc/")) {
        return env.WC_DATA.fetch(request);
      }
      return new Response("wrangler-compose: use a service's port", { status: 404 });
    }
    const binding = env.WC_PORTS[from];
    if (!binding) return new Response(`wrangler-compose: no service on port ${from}`, { status: 404 });
    // Crons do not fire by themselves locally; this runs one now.
    if (request.headers.get("x-wrangler-compose-scheduled") === "1") {
      const r = await env[binding].scheduled({ cron: url.searchParams.get("cron") || "* * * * *", scheduledTime: new Date() });
      return Response.json(r);
    }
    const headers = new Headers(request.headers);
    headers.delete("x-wrangler-compose-port");
    headers.delete("x-wrangler-compose-scheduled");
    return env[binding].fetch(new Request(request, { headers }));
  },
};
"#;

const DATA_JS: &str = r#"// Made by wrangler-compose: the data service. Answers only with the token.
const enc = new TextEncoder();
function same(a, b) {
  const x = enc.encode(a), y = enc.encode(b);
  if (x.length !== y.length) return false;
  let d = 0;
  for (let i = 0; i < x.length; i++) d |= x[i] ^ y[i];
  return d === 0;
}
const b64 = (buf) => btoa(String.fromCharCode(...new Uint8Array(buf)));
const unb64 = (s) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
const out = (v) => (v instanceof ArrayBuffer ? { $blob: b64(v) } : v);
const inp = (v) => (v && typeof v === "object" && "$blob" in v ? unb64(v.$blob) : v);
const json = (v, status = 200) => Response.json(v, { status });

// R2 event notifications (REQ-009): the tool calls /__wc/notify/tick every
// second; each bucket with notifications is compared with the last look and
// the differences go to the queue in Cloudflare's message format. Creates
// cannot be told apart from outside, so they are all reported as PutObject.
// The last look of each rule is kept in WC_STATE (a KV namespace of the
// tool's), so a restart neither loses nor repeats changes, and it moves on
// only after the queue took the messages (a failed send is tried again).
const seen = new Map();
async function listAll(bucket) {
  const out = new Map();
  let cursor;
  do {
    const r = await bucket.list({ cursor });
    for (const o of r.objects) out.set(o.key, { etag: o.etag, uploaded: o.uploaded.getTime(), size: o.size });
    cursor = r.truncated ? r.cursor : undefined;
  } while (cursor);
  return out;
}
async function tick(env) {
  let sent = 0;
  for (const n of env.WC_NOTIFY || []) {
    const id = `${n.name}|${n.queue_key}|${n.events.join(",")}|${n.prefix || ""}|${n.suffix || ""}`;
    const now = await listAll(env[n.bucket]);
    const text = JSON.stringify([...now]);
    let before = seen.get(id);
    if (!before) {
      const saved = await env.WC_STATE.get(id);
      if (saved === null) {
        // First look ever: nothing to compare with yet.
        await env.WC_STATE.put(id, text);
        seen.set(id, { map: now, text });
        continue;
      }
      before = { map: new Map(JSON.parse(saved)), text: saved };
    }
    if (before.text === text) {
      seen.set(id, before);
      continue;
    }
    const match = (k) => (!n.prefix || k.startsWith(n.prefix)) && (!n.suffix || k.endsWith(n.suffix));
    const msgs = [];
    const eventTime = new Date().toISOString();
    if (n.events.includes("object-create")) {
      for (const [k, o] of now) {
        const b = before.map.get(k);
        if ((!b || b.etag !== o.etag || b.uploaded !== o.uploaded) && match(k)) {
          msgs.push({ account: "local", action: "PutObject", bucket: n.name, object: { key: k, size: o.size, eTag: o.etag }, eventTime });
        }
      }
    }
    if (n.events.includes("object-delete")) {
      for (const k of before.map.keys()) {
        if (!now.has(k) && match(k)) msgs.push({ account: "local", action: "DeleteObject", bucket: n.name, object: { key: k }, eventTime });
      }
    }
    for (let i = 0; i < msgs.length; i += 100) {
      await env[n.queue].sendBatch(msgs.slice(i, i + 100).map((body) => ({ body })));
    }
    await env.WC_STATE.put(id, text);
    seen.set(id, { map: now, text });
    sent += msgs.length;
  }
  return sent;
}

export default {
  async fetch(request, env) {
    const auth = request.headers.get("authorization") || "";
    if (!env.WC_TOKEN || !same(auth, `Bearer ${env.WC_TOKEN}`)) return new Response(null, { status: 404 });
    const url = new URL(request.url);
    if (url.pathname === "/__wc/ping") return json({ ok: true });
    if (url.pathname === "/__wc/notify/tick") return json({ sent: await tick(env) });
    // The object or value key travels in the query, never in the path, so
    // no "." or ".." in it can be read as a path step.
    const [, , kind, key, op] = url.pathname.split("/");
    const name = url.searchParams.get("key");
    const meta = () => JSON.parse(decodeURIComponent(request.headers.get("x-wc-meta") || "") || "{}");
    const sendMeta = (m) => ({ "x-wc-meta": encodeURIComponent(JSON.stringify(m)) });
    const binding = env.WC_MAP[kind] && env.WC_MAP[kind][key];
    if (!binding) return json({ error: `no ${kind} ${key}` }, 404);
    const b = env[binding];
    try {
      if (kind === "d1") {
        const body = await request.json();
        if (op === "query") {
          const st = b.prepare(body.sql).bind(...(body.params || []).map(inp));
          const [columns, ...rows] = await st.raw({ columnNames: true });
          return json({ columns: columns || [], rows: rows.map((r) => r.map(out)) });
        }
        if (op === "batch") {
          await b.batch(body.statements.map((s) => b.prepare(s.sql).bind(...(s.params || []).map(inp))));
          return json({ ok: true });
        }
        if (op === "snapshot") {
          // One batch is one transaction: every table read at the same moment.
          const rs = await b.batch(body.statements.map((s) => b.prepare(s.sql).bind(...(s.params || []).map(inp))));
          return json({ results: rs.map((r) => (r.results || []).map((row) => Object.values(row).map(out))) });
        }
        if (op === "exec") {
          await b.exec(body.sql);
          return json({ ok: true });
        }
      }
      if ((op === "object" || op === "value") && name === null) return json({ error: "no key" }, 400);
      if (kind === "r2") {
        if (op === "list") {
          const r = await b.list({ cursor: url.searchParams.get("cursor") || undefined, include: ["httpMetadata", "customMetadata"] });
          return json({
            objects: r.objects.map((o) => ({ key: o.key, size: o.size, etag: o.etag, http: o.httpMetadata || {}, custom: o.customMetadata || {} })),
            cursor: r.truncated ? r.cursor : null,
          });
        }
        if (op === "object" && request.method === "GET") {
          const o = await b.get(name);
          if (!o) return json({ error: "not found" }, 404);
          return new Response(o.body, { headers: sendMeta({ http: o.httpMetadata || {}, custom: o.customMetadata || {} }) });
        }
        if (op === "object" && request.method === "PUT") {
          const m = meta();
          await b.put(name, request.body, { httpMetadata: m.http, customMetadata: m.custom });
          return json({ ok: true });
        }
        if (op === "delete") {
          const body = await request.json();
          await b.delete(body.keys);
          return json({ ok: true });
        }
      }
      if (kind === "kv") {
        if (op === "list") {
          const r = await b.list({ cursor: url.searchParams.get("cursor") || undefined });
          return json({ keys: r.keys, cursor: r.list_complete ? null : r.cursor });
        }
        if (op === "value" && request.method === "GET") {
          const r = await b.getWithMetadata(name, { type: "arrayBuffer" });
          if (r.value === null) return json({ error: "not found" }, 404);
          return new Response(r.value, { headers: sendMeta({ metadata: r.metadata }) });
        }
        if (op === "value" && request.method === "PUT") {
          const m = meta();
          const opts = {};
          if (m.metadata != null) opts.metadata = m.metadata;
          if (m.expiration) opts.expiration = m.expiration;
          await b.put(name, await request.arrayBuffer(), opts);
          return json({ ok: true });
        }
        if (op === "value" && request.method === "DELETE") {
          await b.delete(name);
          return json({ ok: true });
        }
      }
      return json({ error: `unknown operation ${kind}/${op}` }, 400);
    } catch (e) {
      return json({ error: String((e && e.message) || e) }, 500);
    }
  },
};
"#;

const LOGS_JS: &str = r#"// Made by wrangler-compose: forwards one service's logs, tagged with it.
function text(args) {
  return args.map((a) => (typeof a === "string" ? a : JSON.stringify(a))).join(" ");
}
export default {
  async tail(events, env) {
    for (const e of events) {
      const out = (o) => console.log("@@WC@@" + JSON.stringify({ s: env.WC_SERVICE, ...o }));
      const ev = e.event || {};
      for (const l of e.logs || []) out({ t: "log", l: l.level, m: text(l.message || []) });
      for (const x of e.exceptions || []) out({ t: "exc", m: `${x.name}: ${x.message}`, st: x.stack || "" });
      if (ev.request) {
        const u = new URL(ev.request.url);
        out({ t: "req", m: `${ev.request.method} ${u.pathname}${u.search} ${(ev.response && ev.response.status) || e.outcome}` });
      } else if (ev.cron !== undefined) {
        out({ t: "req", m: `cron ${ev.cron} ${e.outcome}` });
      } else if (ev.queue !== undefined) {
        out({ t: "req", m: `queue ${ev.queue} (${ev.batchSize}) ${e.outcome}` });
      }
    }
  },
};
"#;
