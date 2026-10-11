//! The Wrangler configs the tool makes (DES-005): users never write them.
//! Locally every resource is named by its key and every Worker by its
//! service name (REQ-006).

use crate::build::Built;
use crate::compose::{Compose, Kind, Resource, Target, Worker};
use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// The S3-compatible API keys for local R2 (REQ-009, REQ-016): the tool
/// decides them; they only work on this machine.
pub const LOCAL_S3_KEY: &str = "wrangler-compose-local";
pub const LOCAL_S3_SECRET: &str = "wrangler-compose-local-secret";

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
            Target::R2(k) => r2.push(json!({
                "binding": binding,
                "bucket_name": k,
                "local_dev": { "experimental_s3_credentials": {
                    "accessKeyId": LOCAL_S3_KEY, "secretAccessKey": LOCAL_S3_SECRET } }
            })),
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
    if !w.durable_objects.is_empty() {
        cfg.insert(
            "migrations".into(),
            json!([{ "tag": "wc-1", "new_sqlite_classes": w.durable_objects }]),
        );
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

/// `.dev.vars` for a Worker's secrets (REQ-005): dotenv with quoting.
pub fn dev_vars(c: &Compose, w: &Worker) -> String {
    let mut out = String::new();
    for s in &w.secrets {
        if let Some(sec) = c.secrets.get(s) {
            let v = sec
                .value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n");
            out.push_str(&format!("{}=\"{v}\"\n", sec.var));
        }
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
pub fn data_service(c: &Compose, dir: &Path) -> Result<Value> {
    let script = dir.join("index.js");
    std::fs::write(&script, DATA_JS)?;
    let (mut d1, mut r2, mut kv) = (Vec::new(), Vec::new(), Vec::new());
    let (mut d1m, mut r2m, mut kvm) = (Map::new(), Map::new(), Map::new());
    let mut notify = Vec::new();
    let mut producers: Vec<Value> = Vec::new();
    for s in &c.services {
        match &s.kind {
            Kind::D1(list) => {
                for r in list {
                    let b = format!("D1_{}", d1.len());
                    d1.push(json!({ "binding": b, "database_name": r.key, "database_id": r.key }));
                    d1m.insert(r.key.clone(), json!(b));
                }
            }
            Kind::R2(list) => {
                for r in list {
                    let b = format!("R2_{}", r2.len());
                    r2.push(json!({ "binding": b, "bucket_name": r.key }));
                    r2m.insert(r.key.clone(), json!(b));
                    for n in &r.spec.notifications {
                        let q = format!("Q_{}", producers.len());
                        producers.push(json!({ "binding": q, "queue": n.queue }));
                        notify.push(json!({ "bucket": b, "name": r.key, "queue": q,
                            "events": n.events, "prefix": n.prefix, "suffix": n.suffix }));
                    }
                }
            }
            Kind::Kv(list) => {
                for r in list {
                    let b = format!("KV_{}", kv.len());
                    kv.push(json!({ "binding": b, "id": r.key }));
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
    const port = url.port;
    if (port === env.WC_INTERNAL_PORT && url.pathname.startsWith("/__wc/")) {
      return env.WC_DATA.fetch(request);
    }
    const binding = env.WC_PORTS[port];
    if (!binding) return new Response(`wrangler-compose: no service on port ${port}`, { status: 404 });
    return env[binding].fetch(request);
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
const seen = new Map();
async function listAll(bucket) {
  const out = new Map();
  let cursor;
  do {
    const r = await bucket.list({ cursor });
    for (const o of r.objects) out.set(o.key, o);
    cursor = r.truncated ? r.cursor : undefined;
  } while (cursor);
  return out;
}
async function tick(env) {
  let sent = 0;
  for (const n of env.WC_NOTIFY || []) {
    const now = await listAll(env[n.bucket]);
    const before = seen.get(n.bucket + n.queue);
    seen.set(n.bucket + n.queue, now);
    if (!before) continue;
    const match = (k) => (!n.prefix || k.startsWith(n.prefix)) && (!n.suffix || k.endsWith(n.suffix));
    const msgs = [];
    const eventTime = new Date().toISOString();
    if (n.events.includes("object-create")) {
      for (const [k, o] of now) {
        const b = before.get(k);
        if ((!b || b.etag !== o.etag || b.uploaded.getTime() !== o.uploaded.getTime()) && match(k)) {
          msgs.push({ account: "local", action: "PutObject", bucket: n.name, object: { key: k, size: o.size, eTag: o.etag }, eventTime });
        }
      }
    }
    if (n.events.includes("object-delete")) {
      for (const k of before.keys()) {
        if (!now.has(k) && match(k)) msgs.push({ account: "local", action: "DeleteObject", bucket: n.name, object: { key: k }, eventTime });
      }
    }
    for (let i = 0; i < msgs.length; i += 100) {
      await env[n.queue].sendBatch(msgs.slice(i, i + 100).map((body) => ({ body })));
    }
    sent += msgs.length;
  }
  return sent;
}

export default {
  async fetch(request, env) {
    const auth = request.headers.get("authorization") || "";
    if (!env.WC_TOKEN || !same(auth, `Bearer ${env.WC_TOKEN}`)) return new Response(null, { status: 404 });
    const url = new URL(request.url);
    if (url.pathname === "/__wc/notify/tick") return json({ sent: await tick(env) });
    const [, , kind, key, op, ...rest] = url.pathname.split("/");
    const name = decodeURIComponent(rest.join("/"));
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
        if (op === "exec") {
          await b.exec(body.sql);
          return json({ ok: true });
        }
      }
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
          return new Response(o.body, { headers: { "x-wc-meta": JSON.stringify({ http: o.httpMetadata || {}, custom: o.customMetadata || {} }) } });
        }
        if (op === "object" && request.method === "PUT") {
          const meta = JSON.parse(request.headers.get("x-wc-meta") || "{}");
          await b.put(name, request.body, { httpMetadata: meta.http, customMetadata: meta.custom });
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
          return new Response(r.value, { headers: { "x-wc-meta": JSON.stringify({ metadata: r.metadata }) } });
        }
        if (op === "value" && request.method === "PUT") {
          const meta = JSON.parse(request.headers.get("x-wc-meta") || "{}");
          const opts = {};
          if (meta.metadata != null) opts.metadata = meta.metadata;
          if (meta.expiration) opts.expiration = meta.expiration;
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
