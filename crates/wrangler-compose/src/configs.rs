//! The Wrangler configs the tool makes (DES-005): users never write them.
//! Locally every resource is named by its key and every Worker by its
//! service name (REQ-006).

use crate::build::Built;
use crate::compose::{Compose, Kind, Resource, Target, Worker};
use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
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

/// Every resource key → its type ("d1", "r2", …).
pub fn resource_types(c: &Compose) -> BTreeMap<String, &'static str> {
    let mut out = BTreeMap::new();
    for s in &c.services {
        let t = s.kind.type_name();
        for k in crate::compose::resource_keys(s) {
            out.insert(k, t);
        }
    }
    out
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
            Target::D1(k) => d1.push(json!({ "binding": binding, "database_name": k, "database_id": k })),
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
    cfg.insert("tail_consumers".into(), json!([{ "service": logs_worker(svc) }]));
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
pub fn gateway(dir: &Path, ports: &[(u16, String)]) -> Result<Value> {
    let script = dir.join("index.js");
    std::fs::write(&script, GATEWAY_JS)
        .with_context(|| crate::tr!("{} を書けません", "cannot write {}", script.display()))?;
    let map: Map<String, Value> = ports
        .iter()
        .enumerate()
        .map(|(i, (p, _))| (p.to_string(), json!(format!("S{i}"))))
        .collect();
    let services: Vec<Value> = ports
        .iter()
        .enumerate()
        .map(|(i, (_, svc))| json!({ "binding": format!("S{i}"), "service": svc }))
        .collect();
    Ok(json!({
        "name": GATEWAY,
        "main": script,
        "compatibility_date": "2026-09-01",
        "vars": { "WC_PORTS": Value::Object(map) },
        "services": services,
    }))
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
                    dbs.push(json!({ "binding": format!("DB_{}_{i}", s.name.replace('-', "_")),
                        "database_name": r.key, "database_id": r.key, "migrations_dir": m }));
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
    std::fs::write(&script, "export default { fetch() { return new Response(null, { status: 404 }) } }\n")?;
    let cfg = json!({ "name": "wc-migrate", "main": script, "compatibility_date": "2026-09-01", "d1_databases": dbs });
    let path = dir.join("wrangler.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&cfg)?)?;
    Ok(Some((path, names)))
}

const GATEWAY_JS: &str = r#"// Made by wrangler-compose: routes each local port to its service.
export default {
  async fetch(request, env) {
    const port = new URL(request.url).port;
    const binding = env.WC_PORTS[port];
    if (!binding) return new Response(`wrangler-compose: no service on port ${port}`, { status: 404 });
    return env[binding].fetch(request);
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
