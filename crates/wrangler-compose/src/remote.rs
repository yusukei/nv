//! Remote environments (REQ-010–REQ-013, DES-004, DES-009–DES-014): every
//! service runs on Cloudflare under names the tool decides, and nothing of
//! one environment touches another (REQ-006).

use crate::build::{self, Built};
use crate::cf::{self, Cf};
use crate::compose::{Compose, Kind, Service};
use crate::configs::{self, Ids};
use crate::ctx::Ctx;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;

fn say(msg: String) {
    eprintln!("wrangler-compose: {msg}");
}

// ---------------------------------------------------------------- names

/// One D1 database, R2 bucket, KV namespace or queue of the compose file.
#[derive(Debug, Clone)]
pub struct Res {
    pub kind: &'static str,
    pub key: String,
    pub service: String,
    /// Its name on Cloudflare: `<project>--<env>--<name: or service>`.
    pub name: String,
    pub jurisdiction: Option<String>,
    pub storage_class: Option<String>,
}

/// Actual names on Cloudflare (DES-004).
#[derive(Debug)]
pub struct Names {
    /// Worker service → script name.
    pub workers: BTreeMap<String, String>,
    pub resources: Vec<Res>,
}

impl Names {
    fn res(&self, key: &str) -> Option<&Res> {
        self.resources.iter().find(|r| r.key == key)
    }
}

/// `<project>--<env>--`: every name of this environment starts with it, and
/// nothing else does (REQ-006), so the name alone tells whose a thing is.
pub fn env_prefix(c: &Compose, env: &str) -> String {
    format!("{}--{env}--", c.name)
}

pub fn names(c: &Compose, env: &str) -> Result<Names> {
    let prefix = env_prefix(c, env);
    let mut workers = BTreeMap::new();
    let mut resources = Vec::new();
    let mut bad = Vec::new();
    for s in &c.services {
        match &s.kind {
            Kind::Worker(_) => {
                let name = format!("{prefix}{}", s.cloud_name());
                if let Some(why) = check_name("worker", &name) {
                    bad.push(format!("services.{}: {name}: {why}", s.name));
                }
                workers.insert(s.name.clone(), name);
            }
            Kind::D1(list) => {
                for r in list {
                    resources.push(res("d1", s, &r.key, &r.name, &prefix, None, None));
                }
            }
            Kind::R2(list) => {
                for r in list {
                    resources.push(res(
                        "r2",
                        s,
                        &r.key,
                        &r.name,
                        &prefix,
                        r.spec.jurisdiction.clone(),
                        r.spec.storage_class.clone(),
                    ));
                }
            }
            Kind::Kv(list) => {
                for r in list {
                    resources.push(res("kv", s, &r.key, &r.name, &prefix, None, None));
                }
            }
            Kind::Queue(list) => {
                for r in list {
                    resources.push(res("queue", s, &r.key, &r.name, &prefix, None, None));
                }
            }
        }
    }
    for r in &resources {
        if let Some(why) = check_name(r.kind, &r.name) {
            bad.push(format!(
                "services.{} ({}): {}: {why}",
                r.service, r.key, r.name
            ));
        }
    }
    if !bad.is_empty() {
        bail!(crate::tr!(
            "Cloudflare で使えない名前になります(何もしていません)。短い名前を name: に書いてください(プロジェクトの名前を短くしてもよい):\n  {}",
            "these names cannot be used on Cloudflare (nothing was done); give a shorter name: (or shorten the project name):\n  {}",
            bad.join("\n  ")
        ));
    }
    Ok(Names {
        workers,
        resources,
    })
}

fn res(
    kind: &'static str,
    s: &Service,
    key: &str,
    name: &Option<String>,
    prefix: &str,
    jurisdiction: Option<String>,
    storage_class: Option<String>,
) -> Res {
    Res {
        kind,
        key: key.to_string(),
        service: s.name.clone(),
        name: format!("{prefix}{}", name.as_deref().unwrap_or(&s.name)),
        jurisdiction,
        storage_class,
    }
}

/// Why `name` cannot be a Cloudflare `kind` name, if it cannot.
fn check_name(kind: &str, name: &str) -> Option<String> {
    let max = match kind {
        "kv" => 512,
        _ => 63,
    };
    let min = if kind == "r2" { 3 } else { 1 };
    if name.len() > max || name.len() < min {
        return Some(crate::tr!(
            "{min}〜{max} 文字にしてください(いま {} 文字)",
            "must be {min} to {max} characters (it is {})",
            name.len()
        ));
    }
    if kind == "kv" {
        return None;
    }
    let ok = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    (!ok).then(|| {
        crate::tr!(
            "英小文字・数字・- だけで、- で始まり終わらない名前にしてください",
            "use lowercase letters, digits and - (not at either end)"
        )
    })
}

// ---------------------------------------------------------------- what exists

/// What Cloudflare has now, looked up by name every time (DES-004), so
/// another machine (without the tool's records) finds the same things.
#[derive(Default)]
struct Seen {
    scripts: BTreeMap<String, Value>,
    d1: BTreeMap<String, String>,
    kv: BTreeMap<String, String>,
    /// (jurisdiction or "default", bucket name).
    r2: BTreeSet<(String, String)>,
    queues: BTreeMap<String, Value>,
    /// Script name → its Durable Object classes on Cloudflare.
    classes: BTreeMap<String, Vec<String>>,
    /// (script, class) → Durable Object namespace id: what tells whose a
    /// container application is (DES-021).
    namespaces: BTreeMap<(String, String), String>,
    /// (script, class) of namespaces on the old key-value storage.
    subdomain: Option<String>,
    /// Container applications by name (REQ-020), and why they could not be
    /// listed (an account without containers).
    apps: BTreeMap<String, Value>,
    apps_error: Option<String>,
}

/// Deletes a Worker, first taking it off the queues it consumes: Cloudflare
/// refuses to delete a queue consumer (10064).
fn delete_worker(cf: &Cf, seen: &Seen, name: &str) -> Result<()> {
    // Cloudflare keeps a Worker's container applications when the Worker
    // goes (REQ-020): they go first, found by name.
    for app in seen.apps_of(name, &[]) {
        delete_app(cf, &app)?;
    }
    for q in seen.queues.values() {
        let qid = q["queue_id"].as_str().unwrap_or_default();
        for c in q["consumers"].as_array().into_iter().flatten() {
            if c["script_name"] == name || c["script"] == name {
                let cid = c["consumer_id"].as_str().unwrap_or_default();
                cf.delete(&format!("/queues/{qid}/consumers/{cid}"))?;
            }
        }
    }
    cf.delete(&format!("/workers/scripts/{name}"))
}

fn look(cf: &Cf, n: &Names) -> Result<Seen> {
    let scripts = cf
        .list("/workers/scripts")?
        .into_iter()
        .filter_map(|s| Some((s["id"].as_str()?.to_string(), s)))
        .collect();
    let d1 = cf
        .list("/d1/database")?
        .into_iter()
        .filter_map(|d| {
            Some((
                d["name"].as_str()?.to_string(),
                d["uuid"].as_str()?.to_string(),
            ))
        })
        .collect();
    let kv = cf
        .list("/storage/kv/namespaces")?
        .into_iter()
        .filter_map(|k| {
            Some((
                k["title"].as_str()?.to_string(),
                k["id"].as_str()?.to_string(),
            ))
        })
        .collect();
    let mut jurisdictions: BTreeSet<String> = n
        .resources
        .iter()
        .filter(|r| r.kind == "r2")
        .filter_map(|r| r.jurisdiction.clone())
        .collect();
    jurisdictions.insert("default".into());
    let mut r2 = BTreeSet::new();
    for j in jurisdictions {
        for b in cf.r2_buckets(&j)? {
            if let Some(name) = b["name"].as_str() {
                r2.insert((j.clone(), name.to_string()));
            }
        }
    }
    let queues = cf
        .list("/queues")?
        .into_iter()
        .filter_map(|q| Some((q["queue_name"].as_str()?.to_string(), q)))
        .collect();
    let mut classes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut namespaces = BTreeMap::new();
    for ns in cf.list("/workers/durable_objects/namespaces")? {
        if let (Some(script), Some(class)) = (ns["script"].as_str(), ns["class"].as_str()) {
            classes
                .entry(script.to_string())
                .or_default()
                .push(class.to_string());
            if let Some(id) = ns["id"].as_str() {
                namespaces.insert((script.to_string(), class.to_string()), id.to_string());
            }
        }
    }
    let (apps, apps_error) = match cf.list("/containers/applications") {
        Ok(list) => (
            list.into_iter()
                .filter_map(|a| Some((a["name"].as_str()?.to_string(), a)))
                .collect(),
            None,
        ),
        Err(e) => (BTreeMap::new(), Some(format!("{e:#}"))),
    };
    Ok(Seen {
        scripts,
        d1,
        kv,
        r2,
        queues,
        classes,
        namespaces,
        subdomain: cf.workers_subdomain()?,
        apps,
        apps_error,
    })
}

/// Something on Cloudflare whose name says it is this environment's
/// (REQ-006): only these are ever deleted.
struct Found {
    kind: &'static str,
    name: String,
    /// D1 uuid, KV id or queue id; the bucket name for R2.
    id: String,
    /// R2 only: the jurisdiction ("default" for none).
    jurisdiction: String,
}

impl Seen {
    /// The container applications of a Worker's Durable Object classes (on
    /// Cloudflare or in `also`): (name, id). An application is this Worker's
    /// when it is tied to the namespace of one of its classes; when that
    /// namespace is gone, when it has the name the tool gives it (DES-021).
    /// Never by a name's start alone (REQ-006).
    fn apps_of(&self, script: &str, also: &[String]) -> Vec<(String, String)> {
        let mut classes: Vec<String> = self.classes.get(script).cloned().unwrap_or_default();
        for c in also {
            if !classes.contains(c) {
                classes.push(c.clone());
            }
        }
        let mut out: Vec<(String, String)> = Vec::new();
        for class in &classes {
            let ns = self.namespaces.get(&(script.to_string(), class.clone()));
            let want = crate::containers::app_name(script, class);
            for (name, app) in &self.apps {
                let bound = app["durable_objects"]["namespace_id"].as_str();
                let ours = match ns {
                    Some(ns) => bound == Some(ns.as_str()),
                    None => *name == want,
                };
                if ours
                    && let Some(id) = app["id"].as_str()
                    && !out.iter().any(|(_, x)| x == id)
                {
                    out.push((name.clone(), id.to_string()));
                }
            }
        }
        out
    }

    /// Workers named `<project>--<env>--…`.
    fn env_workers(&self, prefix: &str) -> Vec<String> {
        self.scripts
            .keys()
            .filter(|n| n.starts_with(prefix))
            .cloned()
            .collect()
    }

    /// D1, KV, R2 and queues named `<project>--<env>--…`.
    fn env_resources(&self, prefix: &str) -> Vec<Found> {
        let mut out = Vec::new();
        for (name, id) in &self.d1 {
            if name.starts_with(prefix) {
                out.push(Found { kind: "d1", name: name.clone(), id: id.clone(), jurisdiction: String::new() });
            }
        }
        for (name, id) in &self.kv {
            if name.starts_with(prefix) {
                out.push(Found { kind: "kv", name: name.clone(), id: id.clone(), jurisdiction: String::new() });
            }
        }
        for (j, name) in &self.r2 {
            if name.starts_with(prefix) {
                out.push(Found { kind: "r2", name: name.clone(), id: name.clone(), jurisdiction: j.clone() });
            }
        }
        for (name, q) in &self.queues {
            if name.starts_with(prefix) {
                let id = q["queue_id"].as_str().unwrap_or_default().to_string();
                out.push(Found { kind: "queue", name: name.clone(), id, jurisdiction: String::new() });
            }
        }
        out
    }

    fn has(&self, r: &Res) -> bool {
        match r.kind {
            "d1" => self.d1.contains_key(&r.name),
            "kv" => self.kv.contains_key(&r.name),
            "r2" => self.r2.contains(&(jur(r), r.name.clone())),
            _ => self.queues.contains_key(&r.name),
        }
    }

    /// The IDs of what exists, by compose key and service.
    fn ids(&self, n: &Names) -> Ids {
        let mut ids = Ids::default();
        for r in &n.resources {
            match r.kind {
                "d1" => {
                    if let Some(id) = self.d1.get(&r.name) {
                        ids.d1.insert(r.key.clone(), (r.name.clone(), id.clone()));
                    }
                }
                "kv" => {
                    if let Some(id) = self.kv.get(&r.name) {
                        ids.kv.insert(r.key.clone(), id.clone());
                    }
                }
                "r2" => {
                    if self.has(r) {
                        ids.r2
                            .insert(r.key.clone(), (r.name.clone(), r.jurisdiction.clone()));
                    }
                }
                _ => {
                    if self.has(r) {
                        ids.queues.insert(r.key.clone(), r.name.clone());
                    }
                }
            }
        }
        ids.workers = n.workers.clone();
        ids
    }
}

fn delete_app(cf: &Cf, (name, id): &(String, String)) -> Result<()> {
    say(crate::tr!(
        "消す: コンテナ {name}",
        "deleting container {name}"
    ));
    cf.delete(&format!("/containers/applications/{id}"))
}

/// Container applications of a Worker that the compose file no longer
/// wants (a container or a class gone): (name, id).
fn stale_apps(
    seen: &Seen,
    script: &str,
    w: &crate::compose::Worker,
    dropped: Option<&Vec<String>>,
) -> Vec<(String, String)> {
    let mut also = w.durable_objects.clone();
    also.extend(dropped.into_iter().flatten().cloned());
    seen.apps_of(script, &also)
        .into_iter()
        .filter(|(name, _)| {
            !w.containers
                .iter()
                .any(|k| &crate::containers::app_name(script, &k.class) == name)
        })
        .collect()
}

/// Plan lines of a Worker's containers (REQ-014, REQ-020).
fn container_plan(
    seen: &Seen,
    script: &str,
    w: &crate::compose::Worker,
    root: &Path,
    deployed: &BTreeMap<String, String>,
    dropped: Option<&Vec<String>>,
) -> Vec<(char, String, Vec<String>)> {
    let mut out = Vec::new();
    for k in &w.containers {
        let name = crate::containers::app_name(script, &k.class);
        let image = match &k.image {
            crate::compose::Image::Dockerfile(f) => {
                format!("Dockerfile {}", f.strip_prefix(root).unwrap_or(f).display())
            }
            crate::compose::Image::Registry(r) => r.clone(),
        };
        let want_type = match &k.instance_type {
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => "lite".into(),
        };
        let want_max = k.max_instances.unwrap_or(20);
        let Some(app) = seen.apps.get(&name) else {
            out.push((
                '+',
                crate::tr!("コンテナ {name}", "container {name}"),
                vec![
                    crate::tr!("イメージ: {image}", "image: {image}"),
                    format!("instance_type: {want_type}"),
                    format!("max_instances: {want_max}"),
                ],
            ));
            continue;
        };
        let mut d = Vec::new();
        let hash = crate::containers::content_hash(k);
        match deployed.get(&k.class) {
            Some(h) if *h == hash => {}
            Some(_) => d.push(crate::tr!(
                "イメージ: 変わる({image})",
                "image: changes ({image})"
            )),
            None => d.push(crate::tr!(
                "イメージ: 変わるかもしれない(前の展開に記録が無い)",
                "image: may change (the last deploy left no record)"
            )),
        }
        // Cloudflare reports the resources, not the type's name (measured).
        let c = &app["configuration"];
        if let (Some(hv), Some(hm), Some(hd), Some((wv, wm, wd))) = (
            c["vcpu"].as_f64(),
            c["memory_mib"].as_u64(),
            c["disk"]["size_mb"].as_u64(),
            crate::containers::resources(k.instance_type.as_ref()),
        ) && (hv, hm, hd) != (wv, wm, wd)
        {
            d.push(format!(
                "instance_type: vcpu {hv}・{hm}MiB・{hd}MB → vcpu {wv}・{wm}MiB・{wd}MB ({want_type})"
            ));
        }
        if let Some(have) = app["max_instances"].as_u64()
            && have != want_max
        {
            d.push(format!("max_instances: {have} → {want_max}"));
        }
        let sign = if d.is_empty() { '=' } else { '~' };
        out.push((sign, crate::tr!("コンテナ {name}", "container {name}"), d));
    }
    for (name, _) in stale_apps(seen, script, w, dropped) {
        out.push((
            '-',
            crate::tr!("コンテナ {name}", "container {name}"),
            vec![crate::tr!(
                "レジストリのイメージも消す",
                "its images in the registry go too"
            )],
        ));
    }
    out
}

fn jur(r: &Res) -> String {
    r.jurisdiction.clone().unwrap_or_else(|| "default".into())
}

/// The header that puts a bucket call in the bucket's jurisdiction.
fn jur_header(r: &Res) -> Vec<(&'static str, String)> {
    r.jurisdiction
        .iter()
        .map(|j| ("cf-r2-jurisdiction", j.clone()))
        .collect()
}

fn headers<'a>(h: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    h.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

// ---------------------------------------------------------------- records

/// The tool's copy of what it deployed (`.wrangler/compose/<env>/`): only
/// to skip unchanged Workers and to find Workers the compose file dropped.
/// Losing it costs a redeploy, never a wrong result.
fn records_path(ctx: &Ctx) -> PathBuf {
    ctx.env_dir().join("deployed.json")
}

fn read_records(ctx: &Ctx) -> Value {
    std::fs::read_to_string(records_path(ctx))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({ "workers": {} }))
}

fn write_records(ctx: &Ctx, v: &Value) -> Result<()> {
    std::fs::create_dir_all(ctx.env_dir())?;
    std::fs::write(records_path(ctx), serde_json::to_vec_pretty(v)?)?;
    Ok(())
}

fn hash_of(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    crate::files::hex(&h.finalize())
}

// ---------------------------------------------------------------- doing

/// Says what happens; in a dry run only says it (DES-015: the same steps
/// either way, so a dry run stops where the real one would).
struct Act {
    dry: bool,
    /// Collects what would be done instead of saying it (the plan).
    sink: Option<std::cell::RefCell<Vec<String>>>,
}

impl Act {
    fn new(dry: bool) -> Act {
        Act { dry, sink: None }
    }

    /// Changes nothing; what would be done is kept for the plan.
    fn collect() -> Act {
        Act {
            dry: true,
            sink: Some(std::cell::RefCell::new(Vec::new())),
        }
    }

    fn taken(&self) -> Vec<String> {
        self.sink
            .as_ref()
            .map(|s| s.borrow_mut().drain(..).collect())
            .unwrap_or_default()
    }

    fn run(&self, what: String, f: impl FnOnce() -> Result<()>) -> Result<()> {
        if let Some(sink) = &self.sink {
            sink.borrow_mut().push(what);
            Ok(())
        } else if self.dry {
            say(crate::tr!("(dry-run) {what}", "(dry run) {what}"));
            Ok(())
        } else {
            say(what);
            f()
        }
    }
}

fn wrangler(ctx: &Ctx, cf: &Cf, args: &[&str]) -> std::process::Command {
    let mut cmd = ctx.wrangler(args);
    cmd.env("CLOUDFLARE_ACCOUNT_ID", &cf.account);
    cmd
}

/// Run Wrangler; on failure show its output and stop with `what`.
/// From `wrangler containers images list --json` (`[{ name, tags }]`), the
/// images (`<repository>:<tag>`) in the given repositories: the ones named
/// after this environment's container applications (REQ-011, DES-021).
fn registry_targets(list: &Value, repos: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for r in list.as_array().into_iter().flatten() {
        let Some(name) = r["name"].as_str() else {
            continue;
        };
        if !repos.iter().any(|x| x == name) {
            continue;
        }
        for tag in r["tags"].as_array().into_iter().flatten() {
            if let Some(tag) = tag.as_str() {
                out.push(format!("{name}:{tag}"));
            }
        }
    }
    out
}

/// The images in the account's Cloudflare registry (through Wrangler, which
/// logs in to the registry with the login's rights).
fn registry_images(ctx: &Ctx, cf: &Cf) -> Result<Value> {
    let out = wrangler(ctx, cf, &["containers", "images", "list", "--json"])
        .output()
        .context(crate::tr!(
            "Wrangler を起動できません",
            "cannot start Wrangler"
        ))?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        bail!(crate::tr!(
            "Cloudflare のレジストリのイメージを読めません: {}{}",
            "cannot list the images in Cloudflare's registry: {}{}",
            text,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let json = text.find('[').map(|i| &text[i..]).unwrap_or("[]");
    Ok(serde_json::from_str(json).unwrap_or(Value::Null))
}

fn run_wrangler(mut cmd: std::process::Command, what: String) -> Result<()> {
    let out = cmd.output().context(crate::tr!(
        "Wrangler を起動できません",
        "cannot start Wrangler"
    ))?;
    if !out.status.success() {
        eprint!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        bail!("{what}");
    }
    Ok(())
}

// ---------------------------------------------------------------- up

pub fn up(ctx: &Ctx, rebuild: bool, only: &[String], purge: bool) -> Result<()> {
    let services = ctx.compose.closure(only)?;
    let has_containers = services
        .iter()
        .any(|s| matches!(&s.kind, Kind::Worker(w) if !w.containers.is_empty()));
    let n = names(&ctx.compose, &ctx.env)?;
    let act = Act::new(ctx.dry_run);

    // 1. The same bundles as locally (REQ-010, DES-007), before anything on
    //    Cloudflare.
    let mut builts: BTreeMap<String, Built> = BTreeMap::new();
    for s in &services {
        if let Kind::Worker(w) = &s.kind {
            if rebuild && !ctx.dry_run {
                let _ = std::fs::remove_file(
                    ctx.tool_dir()
                        .join("build")
                        .join(&s.name)
                        .join("stamp.json"),
                );
            }
            builts.insert(s.name.clone(), build::build_one(ctx, &s.name, w, true)?);
        }
    }

    // Images are built from Dockerfiles with Docker: it must answer before
    // anything on Cloudflare is touched (REQ-020).
    let dockerfiles = services.iter().any(|s| {
        matches!(&s.kind, Kind::Worker(w) if w.containers.iter()
            .any(|k| matches!(k.image, crate::compose::Image::Dockerfile(_))))
    });
    if dockerfiles {
        crate::containers::check_docker(false)?;
    }

    // 2. What Cloudflare has.
    let cf = Cf::connect(ctx)?;
    let seen = look(&cf, &n)?;
    if has_containers && let Some(e) = &seen.apps_error {
        bail!(crate::tr!(
            "Cloudflare のコンテナを読めないので、何もしていません(コンテナは Workers の有料プランで使えます): {e}",
            "cannot read Cloudflare's containers, so nothing was done (containers need the Workers Paid plan): {e}"
        ));
    }
    let mut records = read_records(ctx);
    let first = !n.workers.values().any(|w| seen.scripts.contains_key(w))
        && !n.resources.iter().any(|r| seen.has(r));

    // 3. What needs a yes first: a new environment, Durable Object classes
    //    that go away with their data, and dropped Workers to delete.
    let mut asks = Vec::new();
    if first {
        asks.push(crate::tr!(
            "環境 {} はまだ Cloudflare にありません。新しく作ります",
            "environment {} does not exist on Cloudflare yet; it will be created",
            ctx.env
        ));
    }
    // Durable Object classes gone from the compose file: deleted (with
    // their data) or kept, as the user chooses on the spot (REQ-010).
    let mut dropped_classes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut kept_classes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for s in &services {
        if let Kind::Worker(w) = &s.kind {
            let script = &n.workers[&s.name];
            let now = seen.classes.get(script).cloned().unwrap_or_default();
            for c in now.into_iter().filter(|c| !w.durable_objects.contains(c)) {
                let what = crate::tr!(
                    "{}: Durable Object のクラス {c} が compose ファイルにありません。消すと保存データも消えます。残すなら、コードからもクラスを消さないでください",
                    "{}: Durable Object class {c} is not in the compose file. Deleting it deletes its data too; to keep it, keep the class in the code as well",
                    s.name
                );
                let delete = if ctx.dry_run {
                    say(crate::tr!(
                        "{what}(up のときに、消すか残すかを聞きます)",
                        "{what} (up asks whether to delete or keep it)"
                    ));
                    false
                } else {
                    let d = crate::confirm::delete_or_keep(&ctx.env, &what)?;
                    say(if d {
                        crate::tr!("{}: クラス {c} を消します", "{}: deleting class {c}", s.name)
                    } else {
                        crate::tr!("{}: クラス {c} を残します", "{}: keeping class {c}", s.name)
                    });
                    d
                };
                let to = if delete { &mut dropped_classes } else { &mut kept_classes };
                to.entry(s.name.clone()).or_default().push(c);
            }
        }
    }
    // This environment's Workers the compose file no longer has (by name,
    // REQ-006): they would keep answering on their URLs, so up deletes them.
    // One with Durable Objects takes their data along: delete, keep or stop.
    let prefix = env_prefix(&ctx.compose, &ctx.env);
    let known: BTreeSet<&String> = n.workers.values().collect();
    let mut orphans = Vec::new();
    for w in seen.env_workers(&prefix) {
        if known.contains(&w) {
            continue;
        }
        let classes = seen.classes.get(&w).cloned().unwrap_or_default();
        let delete = if classes.is_empty() {
            say(crate::tr!(
                "compose ファイルに無い Worker {w} を消します",
                "Worker {w} is not in the compose file; it will be deleted"
            ));
            true
        } else {
            let what = crate::tr!(
                "compose ファイルに無い Worker {w} は Durable Object {} を持っています。消すと保存データも消えます",
                "Worker {w} is not in the compose file and has Durable Objects {}; deleting it deletes their data",
                classes.join(", ")
            );
            if ctx.dry_run {
                say(crate::tr!(
                    "{what}(up のときに、消すか残すかを聞きます)",
                    "{what} (up asks whether to delete or keep it)"
                ));
                false
            } else {
                crate::confirm::delete_or_keep(&ctx.env, &what)?
            }
        };
        if delete {
            orphans.push(w);
        }
    }
    // Its D1, KV, R2 and queues the compose file no longer has: shown, and
    // deleted only with --purge (REQ-010).
    let in_compose: BTreeSet<&str> = n.resources.iter().map(|r| r.name.as_str()).collect();
    let ours = seen.env_resources(&prefix);
    let gone: Vec<&Found> = ours
        .iter()
        .filter(|f| !in_compose.contains(f.name.as_str()))
        .collect();
    if !gone.is_empty() {
        let list = gone
            .iter()
            .map(|f| format!("{} {}", kind_label(f.kind), f.name))
            .collect::<Vec<_>>()
            .join(", ");
        if purge {
            asks.push(crate::tr!(
                "compose ファイルに無い資源を、データごと消します: {list}",
                "resources not in the compose file will be deleted with their data: {list}"
            ));
        } else {
            say(crate::tr!(
                "compose ファイルに無い資源があります: {list}(データごと消すには up --purge)",
                "resources not in the compose file remain: {list} (up --purge deletes them with their data)"
            ));
        }
    }
    if !asks.is_empty() {
        for a in &asks {
            say(a.clone());
        }
        if !ctx.dry_run {
            crate::confirm::retype_env(
                &ctx.env,
                &crate::tr!("これを進める", "go ahead with this"),
            )?;
        }
    }

    // The plan (REQ-014): what changes against what Cloudflare has now,
    // the same from every PC. A dry run ends with it.
    let mut ids = seen.ids(&n);
    let mut plan = Plan::default();
    for s in &services {
        let in_service: Vec<&Res> = n.resources.iter().filter(|r| r.service == s.name).collect();
        for r in in_service {
            let exists = seen.has(r);
            let mut details = Vec::new();
            if let Kind::R2(list) = &s.kind {
                for b in list {
                    let collect = Act::collect();
                    bucket_settings(&cf, &collect, r, &b.spec, &n, exists)?;
                    details.extend(collect.taken());
                }
            }
            if let Kind::D1(list) = &s.kind {
                for d in list {
                    let Some(dir) = &d.spec.migrations else { continue };
                    let pending = match ids.d1.get(&d.key) {
                        Some((_, id)) => pending_remote(&cf, id, dir)?,
                        None => migration_files(dir)?,
                    };
                    if !pending.is_empty() {
                        details.push(crate::tr!(
                            "移行を当てる: {}",
                            "migrations to apply: {}",
                            pending.join(", ")
                        ));
                    }
                }
            }
            let sign = if !exists {
                '+'
            } else if details.is_empty() {
                '='
            } else {
                '~'
            };
            plan.add(sign, format!("{} {}", kind_label(r.kind), r.name), details);
        }
        if let Kind::Worker(w) = &s.kind {
            let script = n.workers[&s.name].clone();
            let declared: Vec<String> = w
                .durable_objects
                .iter()
                .chain(kept_classes.get(&s.name).into_iter().flatten())
                .cloned()
                .collect();
            let cfg = configs::remote_worker(
                &s.name,
                w,
                &builts[&s.name],
                &ids,
                &cf.account,
                do_exports(&declared, dropped_classes.get(&s.name)),
            );
            let want = wanted_shape(&cfg, &w.secrets);
            let hashes = container_hashes(w);
            let tag = code_tag(&builts[&s.name], &cfg, &hashes);
            if !seen.scripts.contains_key(&script) {
                let mut d = shape_changes(&Shape::default(), &want);
                d.retain(|x| !x.starts_with("compatibility_flags: [] → []"));
                plan.add('+', format!("Worker {script}"), d);
                for (sign, what, d) in container_plan(
                    &seen,
                    &script,
                    w,
                    ctx.root(),
                    &BTreeMap::new(),
                    dropped_classes.get(&s.name),
                ) {
                    plan.add(sign, what, d);
                }
                continue;
            }
            let (now, deployed, message) = current_shape(&cf, &script)?;
            let containers = container_plan(
                &seen,
                &script,
                w,
                ctx.root(),
                &message_hashes(message.as_deref()),
                dropped_classes.get(&s.name),
            );
            let mut d = shape_changes(&now, &want);
            if deployed.as_deref() != Some(tag.as_str()) {
                d.push(crate::tr!("コード: 変わる", "code: changes"));
            }
            for c in dropped_classes.get(&s.name).into_iter().flatten() {
                d.push(crate::tr!(
                    "Durable Object のクラス {c}: 消す(保存データも消える)",
                    "Durable Object class {c}: delete (with its data)"
                ));
            }
            if containers.iter().any(|(sign, _, _)| *sign != '=') {
                // A container to make, change or delete goes with a deploy.
                plan.changed.insert(s.name.clone());
            }
            if d.is_empty() {
                plan.add('=', format!("Worker {script}"), d);
            } else {
                plan.changed.insert(s.name.clone());
                plan.add('~', format!("Worker {script}"), d);
            }
            for (sign, what, d) in containers {
                plan.add(sign, what, d);
            }
        }
    }
    for o in &orphans {
        plan.add('-', format!("Worker {o}"), vec![]);
        for (name, _) in seen.apps_of(o, &[]) {
            plan.add(
                '-',
                crate::tr!("コンテナ {name}", "container {name}"),
                vec![crate::tr!(
                    "レジストリのイメージも消す",
                    "its images in the registry go too"
                )],
            );
        }
    }
    for g in &gone {
        if purge {
            plan.add(
                '-',
                format!("{} {}", kind_label(g.kind), g.name),
                vec![crate::tr!("データも消える", "with its data")],
            );
        } else {
            plan.add(
                '=',
                format!("{} {}", kind_label(g.kind), g.name),
                vec![crate::tr!(
                    "compose ファイルに無い(消すには up --purge)",
                    "not in the compose file (up --purge deletes it)"
                )],
            );
        }
    }
    plan.show();
    if ctx.dry_run {
        say(crate::tr!(
            "dry-run なので何も変えていません",
            "dry run: nothing was changed"
        ));
        return Ok(());
    }

    // 4. Resources: make what is missing (REQ-010).
    let wanted: BTreeSet<&str> = services
        .iter()
        .flat_map(|s| crate::compose::resource_keys(s))
        .map(|k| n.res(&k).map(|r| r.name.as_str()).unwrap_or(""))
        .collect();
    for r in n
        .resources
        .iter()
        .filter(|r| wanted.contains(r.name.as_str()))
    {
        if seen.has(r) {
            continue;
        }
        act.run(
            crate::tr!("作る: {} {}", "creating {} {}", kind_label(r.kind), r.name),
            || create(&cf, r, &mut ids),
        )?;
    }

    // 5. Bucket settings (DES-018), now that queues exist.
    for s in &services {
        if let Kind::R2(list) = &s.kind {
            for b in list {
                let r = n.res(&b.key).ok_or_else(|| anyhow!("{}", b.key))?;
                bucket_settings(&cf, &act, r, &b.spec, &n, ids.r2.contains_key(&b.key))?;
            }
        }
    }


    // 6. A repository Cloudflare's registry no longer has (after a
    //    down --purge): its image is pushed first with `wrangler containers
    //    push`. The registry still answers for a deleted digest, so the
    //    deploy would skip the push of an unchanged image and then find no
    //    image to run (measured 2026-10-13, DES-021).
    if dockerfiles {
        let list = match registry_images(ctx, &cf) {
            Ok(l) => Some(l),
            Err(e) => {
                say(crate::tr!(
                    "Cloudflare のレジストリの一覧を読めないので、イメージを先に送る確かめを飛ばします: {e:#}",
                    "cannot list Cloudflare's registry, so the check for images to push first is skipped: {e:#}"
                ));
                None
            }
        };
        for s in list.as_ref().map(|_| services.as_slice()).unwrap_or(&[]) {
            let Kind::Worker(w) = &s.kind else { continue };
            for k in &w.containers {
                if !matches!(k.image, crate::compose::Image::Dockerfile(_)) {
                    continue;
                }
                let repo = crate::containers::app_name(&n.workers[&s.name], &k.class);
                if !registry_targets(
                    list.as_ref().unwrap_or(&Value::Null),
                    std::slice::from_ref(&repo),
                )
                .is_empty()
                {
                    continue;
                }
                // A new tag each time: Cloudflare's registry does not take
                // back a tag name deleted a short while ago (measured).
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let tag = format!(
                    "{repo}:wc-{}-{secs}",
                    &crate::containers::content_hash(k)[..8]
                );
                act.run(
                    crate::tr!(
                        "{}: コンテナ {} のイメージを作って Cloudflare のレジストリに送る",
                        "{}: building and pushing the image of container {} to Cloudflare's registry",
                        s.name,
                        k.class
                    ),
                    || {
                        crate::containers::build(k, &tag)?;
                        run_wrangler(
                            wrangler(ctx, &cf, &["containers", "push", &tag]),
                            crate::tr!(
                                "{} を Cloudflare のレジストリに送れません(上の出力を見てください)",
                                "cannot push {} to Cloudflare's registry (see the output above)",
                                tag
                            ),
                        )
                    },
                )?;
            }
        }
    }

    // 7. In dependency order: migrations, then Workers (REQ-010). A failed
    //    migration stops everything after it.
    let mut containers_deployed = false;
    // Container applications this up deletes: their images in the registry
    // go too (REQ-020, the user's decision 2026-10-13).
    let mut gone_apps: Vec<String> = Vec::new();
    for s in &services {
        match &s.kind {
            Kind::D1(list) => {
                for r in list {
                    let Some(dir) = &r.spec.migrations else {
                        continue;
                    };
                    let res = n.res(&r.key).ok_or_else(|| anyhow!("{}", r.key))?;
                    let pending = match ids.d1.get(&r.key) {
                        Some((_, id)) => pending_remote(&cf, id, dir)?,
                        None => migration_files(dir)?,
                    };
                    if pending.is_empty() {
                        continue;
                    }
                    act.run(
                        crate::tr!(
                            "{}: 移行を当てる: {}",
                            "{}: applying migrations: {}",
                            res.name,
                            pending.join(", ")
                        ),
                        || {
                            let cfg = configs::remote_migrate(
                                &ctx.env_dir().join("run").join("wc-migrate").join(&r.key),
                                &r.key,
                                dir,
                                &ids,
                                &cf.account,
                            )?;
                            let mut cmd = wrangler(
                                ctx,
                                &cf,
                                &["d1", "migrations", "apply", &res.name, "--remote", "-c"],
                            );
                            cmd.arg(&cfg);
                            run_wrangler(
                                cmd,
                                crate::tr!(
                                    "{} の移行が失敗しました。これを使う Worker と、それより後のサービスは展開していません(上の出力を見てください)",
                                    "migrations of {} failed; the Workers using it and every service after it were not deployed (see the output above)",
                                    res.name
                                ),
                            )
                        },
                    )?;
                }
            }
            Kind::Worker(w) => {
                let deployed = deploy_worker(
                    ctx,
                    &cf,
                    &act,
                    &n,
                    &seen,
                    &ids,
                    s,
                    w,
                    &builts[&s.name],
                    dropped_classes.get(&s.name),
                    kept_classes.get(&s.name),
                    plan.changed.contains(&s.name),
                    &mut records,
                    &mut gone_apps,
                )?;
                if deployed && !w.containers.is_empty() {
                    containers_deployed = true;
                }
            }
            _ => {}
        }
    }

    // 8. This environment's Workers and (with --purge) resources the compose
    //    file no longer has (REQ-010).
    for o in &orphans {
        gone_apps.extend(seen.apps_of(o, &[]).into_iter().map(|(name, _)| name));
        act.run(
            crate::tr!("消す: Worker {o}", "deleting Worker {o}"),
            || {
                delete_worker(&cf, &seen, o)?;
                remove_record(&mut records, o);
                Ok(())
            },
        )?;
    }
    if purge {
        for r in &gone {
            delete_found(&cf, &act, &seen, &ours, r)?;
        }
    }
    // Images of the deleted applications: nothing could find them later.
    if !gone_apps.is_empty() {
        match registry_images(ctx, &cf) {
            Ok(list) => {
                for i in registry_targets(&list, &gone_apps) {
                    act.run(
                        crate::tr!(
                            "消す: レジストリのイメージ {i}",
                            "deleting registry image {i}"
                        ),
                        || {
                            run_wrangler(
                                wrangler(ctx, &cf, &["containers", "images", "delete", &i]),
                                crate::tr!(
                                    "レジストリのイメージ {i} を消せません(上の出力を見てください)",
                                    "cannot delete registry image {i} (see the output above)"
                                ),
                            )
                        },
                    )?;
                }
            }
            Err(e) => say(crate::tr!(
                "Cloudflare のレジストリの一覧を読めないので、消したコンテナのイメージは残ります: {e:#}",
                "cannot list Cloudflare's registry, so the images of the deleted containers remain: {e:#}"
            )),
        }
    }
    if ctx.dry_run {
        say(crate::tr!(
            "dry-run なので何も変えていません",
            "dry run: nothing was changed"
        ));
        return Ok(());
    }
    write_records(ctx, &records)?;
    say(crate::tr!(
        "環境 {} に展開しました",
        "deployed to environment {}",
        ctx.env
    ));
    if containers_deployed {
        say(crate::tr!(
            "コンテナの入れ替えは、このあとも Cloudflare で段階的に続きます。初めての展開では、コンテナが動き出すまで数分かかることがあります",
            "Cloudflare keeps replacing container instances step by step after this; on a first deploy, containers may take a few minutes to start"
        ));
    }
    for s in &services {
        if let Kind::Worker(w) = &s.kind {
            let script = &n.workers[&s.name];
            let mut urls: Vec<String> = w.routes.clone();
            if !w.ports.is_empty()
                && w.routes.is_empty()
                && let Some(subdomain) = &seen.subdomain
            {
                urls.push(format!("https://{script}.{subdomain}.workers.dev"));
            }
            eprintln!(
                "  {:<16} {}",
                s.name,
                if urls.is_empty() {
                    crate::tr!(
                        "(外からの入口なし。ほかのサービスから呼ぶ)",
                        "(not reachable from outside; called by other services)"
                    )
                } else {
                    urls.join(", ")
                }
            );
        }
    }
    Ok(())
}


fn kind_label(kind: &str) -> &'static str {
    match kind {
        "d1" => "D1",
        "r2" => "R2",
        "kv" => "KV",
        _ => "Queue",
    }
}

fn create(cf: &Cf, r: &Res, ids: &mut Ids) -> Result<()> {
    match r.kind {
        "d1" => {
            let v = cf.post("/d1/database", &json!({ "name": r.name }))?;
            let id = v["uuid"]
                .as_str()
                .ok_or_else(|| cf::missing_field("uuid"))?;
            ids.d1
                .insert(r.key.clone(), (r.name.clone(), id.to_string()));
        }
        "kv" => {
            let v = cf.post("/storage/kv/namespaces", &json!({ "title": r.name }))?;
            let id = v["id"].as_str().ok_or_else(|| cf::missing_field("id"))?;
            ids.kv.insert(r.key.clone(), id.to_string());
        }
        "r2" => {
            let mut body = json!({ "name": r.name });
            if let Some(sc) = &r.storage_class {
                body["storageClass"] = json!(sc);
            }
            cf.post_with("/r2/buckets", &body, &headers(&jur_header(r)))?;
            ids.r2
                .insert(r.key.clone(), (r.name.clone(), r.jurisdiction.clone()));
        }
        _ => {
            cf.post("/queues", &json!({ "queue_name": r.name }))?;
            ids.queues.insert(r.key.clone(), r.name.clone());
        }
    }
    Ok(())
}

/// Make a bucket's settings what the compose file says (DES-018). Rules
/// the tool did not make (ids not starting with `wc-`) are left alone.
fn bucket_settings(
    cf: &Cf,
    act: &Act,
    r: &Res,
    b: &crate::compose::Bucket,
    n: &Names,
    exists: bool,
) -> Result<()> {
    let h = jur_header(r);
    let h = headers(&h);
    let base = format!("/r2/buckets/{}", r.name);
    let read = |path: &str| -> Result<Value> {
        if !exists {
            return Ok(Value::Null);
        }
        match cf.get_with(&format!("{base}{path}"), &h) {
            Ok(v) => Ok(v),
            Err(e) if cf::is_missing(&e) => Ok(Value::Null),
            Err(e) => Err(e),
        }
    };

    // public (the r2.dev URL)
    let public_now = read("/domains/managed")?["enabled"] == true;
    if public_now != b.public {
        act.run(
            crate::tr!(
                "{}: 公開の URL を {}",
                "{}: public URL {}",
                r.name,
                if b.public {
                    crate::tr!("開く", "on")
                } else {
                    crate::tr!("閉じる", "off")
                }
            ),
            || {
                cf.put_with(
                    &format!("{base}/domains/managed"),
                    &json!({ "enabled": b.public }),
                    &h,
                )
                .map(|_| ())
            },
        )?;
    }

    // cors
    let want: Vec<Value> = b.cors.iter().map(cors_rule).collect();
    let now: Vec<Value> = read("/cors")?["rules"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|x| {
            let mut x = x.clone();
            if let Some(o) = x.as_object_mut() {
                o.remove("id");
            }
            normalize(&x)
        })
        .collect();
    let want_n: Vec<Value> = want.iter().map(normalize).collect();
    if now != want_n {
        act.run(
            crate::tr!("{}: CORS を合わせる", "{}: setting CORS", r.name),
            || {
                if want.is_empty() {
                    cf.delete_with(&format!("{base}/cors"), &h)
                } else {
                    cf.put_with(&format!("{base}/cors"), &json!({ "rules": want }), &h)
                        .map(|_| ())
                }
            },
        )?;
    }

    // lifecycle and lock: rules named wc-<n> are the tool's
    for (path, want) in [
        ("/lifecycle", lifecycle_rules(&b.lifecycle)),
        ("/lock", lock_rules(&b.lock)),
    ] {
        let now: Vec<Value> = read(path)?["rules"].as_array().cloned().unwrap_or_default();
        let (ours, others): (Vec<Value>, Vec<Value>) = now
            .into_iter()
            .partition(|x| x["id"].as_str().is_some_and(|i| i.starts_with("wc-")));
        let ours_n: Vec<Value> = ours.iter().map(normalize).collect();
        let want_n: Vec<Value> = want.iter().map(normalize).collect();
        if ours_n != want_n {
            let mut rules = others;
            rules.extend(want.clone());
            act.run(
                crate::tr!(
                    "{}: {} を合わせる",
                    "{}: setting {}",
                    r.name,
                    path.trim_start_matches('/')
                ),
                || {
                    cf.put_with(&format!("{base}{path}"), &json!({ "rules": rules }), &h)
                        .map(|_| ())
                },
            )?;
        }
    }

    // event notifications, per queue
    let mut want: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for nt in &b.notifications {
        let q = n.res(&nt.queue).map(|q| q.name.clone()).unwrap_or_default();
        let mut actions = Vec::new();
        if nt.events.iter().any(|e| e == "object-create") {
            actions.extend(["PutObject", "CopyObject", "CompleteMultipartUpload"]);
        }
        if nt.events.iter().any(|e| e == "object-delete") {
            actions.extend(["DeleteObject", "LifecycleDeletion"]);
        }
        let mut rule = json!({ "actions": actions });
        if let Some(p) = &nt.prefix {
            rule["prefix"] = json!(p);
        }
        if let Some(sfx) = &nt.suffix {
            rule["suffix"] = json!(sfx);
        }
        want.entry(q).or_default().push(rule);
    }
    let now: Vec<Value> = if exists {
        match cf.get_with(
            &format!("/event_notifications/r2/{}/configuration", r.name),
            &h,
        ) {
            Ok(v) => v["queues"].as_array().cloned().unwrap_or_default(),
            Err(e) if cf::is_missing(&e) => Vec::new(),
            Err(e) => return Err(e),
        }
    } else {
        Vec::new()
    };
    let rules_of = |rules: &[Value]| -> Vec<Value> {
        let mut v: Vec<Value> = rules
            .iter()
            .map(|x| {
                let mut a: Vec<String> = x["actions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect();
                a.sort();
                json!({ "actions": a, "prefix": x["prefix"].as_str().unwrap_or(""),
                        "suffix": x["suffix"].as_str().unwrap_or("") })
            })
            .collect();
        v.sort_by_key(|x| x.to_string());
        v
    };
    let queue_id = |name: &str| -> Result<String> {
        let qs = cf.list("/queues")?;
        qs.iter()
            .find(|q| q["queue_name"] == name)
            .and_then(|q| q["queue_id"].as_str().map(str::to_string))
            .ok_or_else(|| cf::missing_field(&format!("queue {name}")))
    };
    for q in &now {
        let qname = q["queueName"].as_str().unwrap_or_default();
        let rules = q["rules"].as_array().cloned().unwrap_or_default();
        let same = want
            .get(qname)
            .is_some_and(|w| rules_of(w) == rules_of(&rules));
        if !same {
            let qid = q["queueId"].as_str().unwrap_or_default().to_string();
            act.run(
                crate::tr!(
                    "{}: {qname} への通知を外す",
                    "{}: removing notifications to {qname}",
                    r.name
                ),
                || {
                    cf.delete_with(
                        &format!(
                            "/event_notifications/r2/{}/configuration/queues/{qid}",
                            r.name
                        ),
                        &h,
                    )
                },
            )?;
        }
    }
    for (qname, rules) in &want {
        let same = now.iter().any(|q| {
            q["queueName"] == qname.as_str()
                && rules_of(q["rules"].as_array().map(|v| v.as_slice()).unwrap_or(&[]))
                    == rules_of(rules)
        });
        if !same {
            act.run(
                crate::tr!("{}: {qname} へ通知する", "{}: notifying {qname}", r.name),
                || {
                    let qid = queue_id(qname)?;
                    cf.put_with(
                        &format!(
                            "/event_notifications/r2/{}/configuration/queues/{qid}",
                            r.name
                        ),
                        &json!({ "rules": rules }),
                        &h,
                    )
                    .map(|_| ())
                },
            )?;
        }
    }
    Ok(())
}

fn cors_rule(c: &crate::compose::Cors) -> Value {
    let mut allowed = json!({ "origins": c.origins, "methods": c.methods });
    if !c.allowed_headers.is_empty() {
        allowed["headers"] = json!(c.allowed_headers);
    }
    let mut rule = json!({ "allowed": allowed });
    if !c.expose_headers.is_empty() {
        rule["exposeHeaders"] = json!(c.expose_headers);
    }
    if let Some(age) = c.max_age {
        rule["maxAgeSeconds"] = json!(age);
    }
    rule
}

const DAY: u64 = 86_400;

/// `prefix`, `expire_after_days`, `transition_to_ia_after_days`,
/// `abort_multipart_after_days` → R2 lifecycle rules `wc-<n>`.
fn lifecycle_rules(rules: &[Value]) -> Vec<Value> {
    rules
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let days = |k: &str| {
                r[k].as_u64()
                    .or_else(|| r[k].as_str().and_then(|s| s.parse().ok()))
            };
            let mut rule = json!({
                "id": format!("wc-{}", i + 1),
                "enabled": true,
                "conditions": { "prefix": r["prefix"].as_str().unwrap_or("") },
            });
            if let Some(d) = days("expire_after_days") {
                rule["deleteObjectsTransition"] =
                    json!({ "condition": { "type": "Age", "maxAge": d * DAY } });
            }
            if let Some(d) = days("transition_to_ia_after_days") {
                rule["storageClassTransitions"] = json!([{
                    "condition": { "type": "Age", "maxAge": d * DAY },
                    "storageClass": "InfrequentAccess" }]);
            }
            if let Some(d) = days("abort_multipart_after_days") {
                rule["abortMultipartUploadsTransition"] =
                    json!({ "condition": { "type": "Age", "maxAge": d * DAY } });
            }
            rule
        })
        .collect()
}

/// `prefix` with `retain_days`, `retain_until` or `indefinite: true` → R2
/// lock rules `wc-<n>`.
fn lock_rules(rules: &[Value]) -> Vec<Value> {
    rules
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let condition = if let Some(d) = r["retain_days"]
                .as_u64()
                .or_else(|| r["retain_days"].as_str().and_then(|s| s.parse().ok()))
            {
                json!({ "type": "Age", "maxAgeSeconds": d * DAY })
            } else if let Some(date) = r["retain_until"].as_str() {
                json!({ "type": "Date", "date": date })
            } else {
                json!({ "type": "Indefinite" })
            };
            let mut rule =
                json!({ "id": format!("wc-{}", i + 1), "enabled": true, "condition": condition });
            if let Some(p) = r["prefix"].as_str() {
                rule["prefix"] = json!(p);
            }
            rule
        })
        .collect()
}

/// For comparing settings: object keys in order, empty lists and nulls
/// dropped (Cloudflare leaves out what is empty).
fn normalize(v: &Value) -> Value {
    match v {
        Value::Object(o) => {
            let mut m: BTreeMap<String, Value> = BTreeMap::new();
            for (k, x) in o {
                let x = normalize(x);
                let empty = x.is_null()
                    || x.as_array().is_some_and(|a| a.is_empty())
                    || x.as_object().is_some_and(|a| a.is_empty());
                if !empty {
                    m.insert(k.clone(), x);
                }
            }
            json!(m)
        }
        Value::Array(a) => Value::Array(a.iter().map(normalize).collect()),
        other => other.clone(),
    }
}

/// What a Worker is, the same from every PC: its bundle and settings
/// without local paths or secrets. Deployed as the version's tag, so the
/// next plan compares with what Cloudflare runs (REQ-014).
/// The config as it is the same from every PC: containers by what their
/// images are made from (`hashes`: class → content hash), not by where the
/// Dockerfile is on this PC (REQ-020).
fn portable(cfg: &Value, hashes: &[(String, String)]) -> Value {
    let mut c = cfg.clone();
    if let Some(list) = c.get_mut("containers").and_then(Value::as_array_mut) {
        for k in list {
            let Some(o) = k.as_object_mut() else { continue };
            let class = o
                .get("class_name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some((_, h)) = hashes.iter().find(|(c, _)| c == class) {
                o.insert("image".into(), json!(format!("content:{h}")));
            }
            o.shift_remove("image_build_context");
            o.shift_remove("image_vars");
        }
    }
    c
}

/// Class → content hash of a Worker's containers.
fn container_hashes(w: &crate::compose::Worker) -> Vec<(String, String)> {
    w.containers
        .iter()
        .map(|k| (k.class.clone(), crate::containers::content_hash(k)))
        .collect()
}

/// The deploy message that keeps each container's content hash with the
/// Worker version, so a plan from any PC tells whether an image changes
/// (REQ-014, REQ-020). None when there is none, or when it would not fit
/// Cloudflare's 100 characters.
fn container_message(hashes: &[(String, String)]) -> Option<String> {
    if hashes.is_empty() {
        return None;
    }
    let parts: Vec<String> = hashes.iter().map(|(c, h)| format!("{c}={h}")).collect();
    let msg = format!("wc-containers {}", parts.join(" "));
    (msg.len() <= 100).then_some(msg)
}

/// Class → content hash from a deploy message written by `container_message`.
fn message_hashes(msg: Option<&str>) -> BTreeMap<String, String> {
    let Some(rest) = msg.and_then(|m| m.strip_prefix("wc-containers ")) else {
        return BTreeMap::new();
    };
    rest.split_whitespace()
        .filter_map(|p| p.split_once('='))
        .map(|(c, h)| (c.to_string(), h.to_string()))
        .collect()
}

fn code_tag(built: &Built, cfg: &Value, hashes: &[(String, String)]) -> String {
    let mut c = portable(cfg, hashes);
    if let Some(o) = c.as_object_mut() {
        for k in ["main", "base_dir", "account_id"] {
            o.shift_remove(k);
        }
    }
    // Deleted-class tombstones only matter once (as in deploy_worker).
    if let Some(e) = c.get_mut("exports").and_then(Value::as_object_mut) {
        e.retain(|_, v| v["state"] != "deleted");
        if e.is_empty() {
            c.as_object_mut().map(|o| o.shift_remove("exports"));
        }
    }
    let h = hash_of(&[built.hash.as_bytes(), c.to_string().as_bytes()]);
    format!("wc-{}", &h[..16])
}

/// A Worker's settings that a plan shows: environment values, bindings
/// (name → type), crons and secret names.
#[derive(Default, PartialEq)]
struct Shape {
    vars: BTreeMap<String, String>,
    bindings: BTreeMap<String, String>,
    crons: Vec<String>,
    secrets: BTreeSet<String>,
    date: String,
    flags: Vec<String>,
}

fn wanted_shape(cfg: &Value, secrets: &BTreeMap<String, String>) -> Shape {
    let mut sh = Shape {
        date: cfg["compatibility_date"].as_str().unwrap_or_default().to_string(),
        flags: cfg["compatibility_flags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f.as_str().map(str::to_string))
            .collect(),
        secrets: secrets.keys().cloned().collect(),
        crons: cfg["triggers"]["crons"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c.as_str().map(str::to_string))
            .collect(),
        ..Shape::default()
    };
    for (k, v) in cfg["vars"].as_object().into_iter().flatten() {
        sh.vars.insert(
            k.clone(),
            v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()),
        );
    }
    let mut add = |list: &Value, key: &str, t: &str| {
        for b in list.as_array().into_iter().flatten() {
            if let Some(name) = b[key].as_str() {
                sh.bindings.insert(name.to_string(), t.to_string());
            }
        }
    };
    add(&cfg["d1_databases"], "binding", "d1");
    add(&cfg["r2_buckets"], "binding", "r2_bucket");
    add(&cfg["kv_namespaces"], "binding", "kv_namespace");
    add(&cfg["queues"]["producers"], "binding", "queue");
    add(&cfg["services"], "binding", "service");
    add(&cfg["durable_objects"]["bindings"], "name", "durable_object_namespace");
    sh
}

/// What Cloudflare runs now for a Worker, and its version's tag.
fn current_shape(cf: &Cf, script: &str) -> Result<(Shape, Option<String>, Option<String>)> {
    let get = |path: &str| match cf.get(path) {
        Ok(v) => Ok(v),
        Err(e) if cf::is_missing(&e) => Ok(Value::Null),
        Err(e) => Err(e),
    };
    let settings = get(&format!("/workers/scripts/{script}/settings"))?;
    let mut sh = Shape {
        date: settings["compatibility_date"].as_str().unwrap_or_default().to_string(),
        flags: settings["compatibility_flags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f.as_str().map(str::to_string))
            .collect(),
        ..Shape::default()
    };
    for b in settings["bindings"].as_array().into_iter().flatten() {
        let (Some(name), Some(t)) = (b["name"].as_str(), b["type"].as_str()) else {
            continue;
        };
        match t {
            "plain_text" => {
                sh.vars
                    .insert(name.into(), b["text"].as_str().unwrap_or_default().into());
            }
            "json" => {
                sh.vars.insert(name.into(), b["json"].to_string());
            }
            "secret_text" => {
                sh.secrets.insert(name.into());
            }
            // Not a binding of the Worker's own (what a deploy keeps).
            "inherit" => {}
            other => {
                sh.bindings.insert(name.into(), other.into());
            }
        }
    }
    let sched = get(&format!("/workers/scripts/{script}/schedules"))?;
    sh.crons = sched["schedules"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["cron"].as_str().map(str::to_string))
        .collect();
    for s in get(&format!("/workers/scripts/{script}/secrets"))?
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(n) = s["name"].as_str() {
            sh.secrets.insert(n.into());
        }
    }
    let deps = get(&format!("/workers/scripts/{script}/deployments"))?;
    let version = deps["deployments"][0]["versions"][0]["version_id"]
        .as_str()
        .map(str::to_string);
    let annotations = match version {
        Some(v) => get(&format!("/workers/scripts/{script}/versions/{v}"))?["annotations"].clone(),
        None => Value::Null,
    };
    let text = |k: &str| annotations[k].as_str().map(str::to_string);
    Ok((sh, text("workers/tag"), text("workers/message")))
}

/// Before → after of each setting that differs (secrets by name only).
fn shape_changes(now: &Shape, want: &Shape) -> Vec<String> {
    let mut out = Vec::new();
    if now.date != want.date {
        out.push(format!("compatibility_date: {} → {}", now.date, want.date));
    }
    if now.flags != want.flags {
        out.push(format!(
            "compatibility_flags: [{}] → [{}]",
            now.flags.join(", "),
            want.flags.join(", ")
        ));
    }
    for (k, v) in &want.vars {
        match now.vars.get(k) {
            None => out.push(format!("environment.{k}: + {v:?}")),
            Some(old) if old != v => out.push(format!("environment.{k}: {old:?} → {v:?}")),
            _ => {}
        }
    }
    for k in now.vars.keys().filter(|k| !want.vars.contains_key(*k)) {
        out.push(format!("environment.{k}: -"));
    }
    for (k, t) in &want.bindings {
        match now.bindings.get(k) {
            None => out.push(format!("bindings.{k}: + {t}")),
            Some(old) if old != t => out.push(format!("bindings.{k}: {old} → {t}")),
            _ => {}
        }
    }
    for k in now.bindings.keys().filter(|k| !want.bindings.contains_key(*k)) {
        out.push(format!("bindings.{k}: -"));
    }
    if now.crons != want.crons {
        out.push(format!(
            "crons: [{}] → [{}]",
            now.crons.join(", "),
            want.crons.join(", ")
        ));
    }
    for k in want.secrets.difference(&now.secrets) {
        out.push(crate::tr!("secrets.{k}: +(値は出さない)", "secrets.{k}: + (value not shown)"));
    }
    for k in now.secrets.difference(&want.secrets) {
        out.push(format!("secrets.{k}: -"));
    }
    out
}

/// The plan of an up (REQ-014), shown like terraform plan.
#[derive(Default)]
struct Plan {
    lines: Vec<(char, String, Vec<String>)>,
    /// Worker services whose settings or code differ from Cloudflare's.
    changed: BTreeSet<String>,
}

impl Plan {
    fn add(&mut self, sign: char, what: String, details: Vec<String>) {
        self.lines.push((sign, what, details));
    }

    fn show(&self) {
        say(crate::tr!("実行の計画:", "plan:"));
        let label = |c: char| match c {
            '+' => crate::tr!("作る", "create"),
            '~' => crate::tr!("変える", "change"),
            '-' => crate::tr!("消す", "delete"),
            _ => crate::tr!("変わらない", "unchanged"),
        };
        for (sign, what, details) in &self.lines {
            eprintln!("  {sign} {:<10} {what}", label(*sign));
            for d in details {
                eprintln!("      {d}");
            }
        }
        let count = |c: char| self.lines.iter().filter(|(s, _, _)| *s == c).count();
        say(crate::tr!(
            "作る {}・変える {}・消す {}・変わらない {}",
            "{} to create, {} to change, {} to delete, {} unchanged",
            count('+'),
            count('~'),
            count('-'),
            count('=')
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn deploy_worker(
    ctx: &Ctx,
    cf: &Cf,
    act: &Act,
    n: &Names,
    seen: &Seen,
    ids: &Ids,
    s: &Service,
    w: &crate::compose::Worker,
    built: &Built,
    dropped: Option<&Vec<String>>,
    kept: Option<&Vec<String>>,
    changed_on_cloud: bool,
    records: &mut Value,
    gone_apps: &mut Vec<String>,
) -> Result<bool> {
    let script = n.workers[&s.name].clone();
    let exists = seen.scripts.contains_key(&script);
    let mut deployed = false;
    let declared: Vec<String> = w
        .durable_objects
        .iter()
        .chain(kept.into_iter().flatten())
        .cloned()
        .collect();
    let exports = do_exports(&declared, dropped);
    let cfg = configs::remote_worker(&s.name, w, built, ids, &cf.account, exports);
    let hashes = container_hashes(w);
    let tag = code_tag(built, &cfg, &hashes);
    // Containers the compose file no longer wants go before the deploy.
    for app in stale_apps(seen, &script, w, dropped) {
        gone_apps.push(app.0.clone());
        act.run(
            crate::tr!(
                "{}: コンテナ {} を消す",
                "{}: deleting container {}",
                s.name,
                app.0
            ),
            || cf.delete(&format!("/containers/applications/{}", app.1)),
        )?;
    }
    let secrets = &w.secrets;
    let secret_text = serde_json::to_string(&secrets)?;
    // Deleted-class tombstones only matter once: the next deploy without
    // them is the same Worker.
    let mut same = portable(&cfg, &hashes);
    if let Some(e) = same.get_mut("exports").and_then(Value::as_object_mut) {
        e.retain(|_, v| v["state"] != "deleted");
        if e.is_empty() {
            same.as_object_mut().map(|o| o.shift_remove("exports"));
        }
    }
    let hash = hash_of(&[
        built.hash.as_bytes(),
        same.to_string().as_bytes(),
        secret_text.as_bytes(),
    ]);
    // Unchanged both on Cloudflare (what any PC sees) and since this PC's
    // last deploy (which also covers the secrets' values).
    let unchanged =
        exists && !changed_on_cloud && records["workers"][&script]["hash"] == hash.as_str();
    if unchanged {
        say(crate::tr!(
            "{}: 変わっていないので展開しません",
            "{}: unchanged, not deployed",
            s.name
        ));
    } else {
        deployed = true;
        let names: Vec<&String> = secrets.keys().collect();
        act.run(
            crate::tr!(
                "{}: {} として展開する(シークレット: {})",
                "{}: deploying as {} (secrets: {})",
                s.name,
                script,
                if names.is_empty() {
                    "-".to_string()
                } else {
                    names.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(", ")
                }
            ),
            || {
                let dir = ctx.env_dir().join("run").join(&s.name);
                std::fs::create_dir_all(&dir)?;
                let cfg_path = dir.join("wrangler.json");
                std::fs::write(&cfg_path, serde_json::to_vec_pretty(&cfg)?)?;
                let mut cmd = wrangler(ctx, cf, &["deploy", "--tag", &tag]);
                if let Some(m) = container_message(&hashes) {
                    cmd.arg("--message").arg(m);
                }
                cmd.arg("-c").arg(&cfg_path);
                let file = dir.join("secrets.json");
                if !secrets.is_empty() {
                    crate::local::write_private(&file, &secret_text)?;
                    cmd.arg("--secrets-file").arg(&file);
                }
                let r = run_wrangler(
                    cmd,
                    crate::tr!(
                        "{} を展開できません。それより後のサービスは展開していません(上の出力を見てください)",
                        "cannot deploy {}; the services after it were not deployed (see the output above)",
                        s.name
                    ),
                );
                let _ = std::fs::remove_file(&file);
                r?;
                records["workers"][&script] = json!({ "service": s.name, "hash": hash });
                Ok(())
            },
        )?;
    }

    // Secrets the compose file no longer gives this Worker (REQ-005).
    if exists {
        let listed = match cf.get(&format!("/workers/scripts/{script}/secrets")) {
            Ok(v) => v.as_array().cloned().unwrap_or_default(),
            Err(e) if cf::is_missing(&e) => Vec::new(),
            Err(e) => return Err(e),
        };
        for sec in listed {
            let Some(name) = sec["name"].as_str() else {
                continue;
            };
            if sec["type"] == "secret_text" && !secrets.contains_key(name) {
                act.run(
                    crate::tr!(
                        "{}: シークレット {name} を消す",
                        "{}: deleting secret {name}",
                        s.name
                    ),
                    || cf.delete(&format!("/workers/scripts/{script}/secrets/{name}")),
                )?;
            }
        }
    }

    // Queues it no longer consumes (Wrangler only adds consumers).
    for (qname, q) in &seen.queues {
        let ours = n
            .resources
            .iter()
            .find(|r| r.kind == "queue" && &r.name == qname);
        let Some(qres) = ours else { continue };
        if w.consumes.contains(&qres.key) {
            continue;
        }
        for c in q["consumers"].as_array().into_iter().flatten() {
            if c["script_name"] == script.as_str() || c["script"] == script.as_str() {
                let qid = q["queue_id"].as_str().unwrap_or_default();
                let cid = c["consumer_id"].as_str().unwrap_or_default();
                act.run(
                    crate::tr!(
                        "{}: {qname} を受け取るのをやめる",
                        "{}: no longer consuming {qname}",
                        s.name
                    ),
                    || cf.delete(&format!("/queues/{qid}/consumers/{cid}")),
                )?;
            }
        }
    }
    Ok(deployed)
}

/// Durable Object classes, declared (DES-010): Cloudflare compares the
/// `exports` map with what it has and creates or deletes namespaces, so no
/// migration tags are kept. Dropped classes become `deleted` tombstones
/// (after the user chose to delete them). Every class is SQLite-backed;
/// the old key-value storage is not supported.
pub fn do_exports(want: &[String], dropped: Option<&Vec<String>>) -> Option<Value> {
    let mut map = serde_json::Map::new();
    for c in want {
        map.insert(
            c.clone(),
            json!({ "type": "durable-object", "storage": "sqlite" }),
        );
    }
    for c in dropped.into_iter().flatten() {
        map.insert(
            c.clone(),
            json!({ "type": "durable-object", "state": "deleted" }),
        );
    }
    (!map.is_empty()).then(|| Value::Object(map))
}

fn remove_record(records: &mut Value, script: &str) {
    if let Some(o) = records["workers"].as_object_mut() {
        o.remove(script);
    }
}

fn migration_files(dir: &Path) -> Result<Vec<String>> {
    let mut files: Vec<String> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".sql"))
        .collect();
    files.sort();
    Ok(files)
}

/// Migration files not yet in the database's `d1_migrations`.
fn pending_remote(cf: &Cf, id: &str, dir: &Path) -> Result<Vec<String>> {
    let applied: Vec<String> = match cf.post(
        &format!("/d1/database/{id}/query"),
        &json!({ "sql": "SELECT name FROM d1_migrations" }),
    ) {
        Ok(v) => v[0]["results"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r["name"].as_str().map(str::to_string))
            .collect(),
        // No table yet: nothing applied.
        Err(e) if e.to_string().contains("no such table") => Vec::new(),
        Err(e) => return Err(e),
    };
    Ok(migration_files(dir)?
        .into_iter()
        .filter(|f| !applied.contains(f))
        .collect())
}

// ---------------------------------------------------------------- down

pub fn down(ctx: &Ctx, purge: bool, only: &[String]) -> Result<()> {
    if !only.is_empty() {
        ctx.compose.closure(only)?;
    }
    let n = names(&ctx.compose, &ctx.env)?;
    let prefix = env_prefix(&ctx.compose, &ctx.env);
    let cf = Cf::connect(ctx)?;
    let seen = look(&cf, &n)?;
    let act = Act::new(ctx.dry_run);
    let ours = seen.env_resources(&prefix);

    // Workers to take down, dependents first (REQ-011). Without services
    // named, every Worker of this environment goes, those the compose file
    // no longer has too.
    let order: Vec<&Service> = ctx.compose.services.iter().rev().collect();
    let mut workers: Vec<(String, String)> = order
        .iter()
        .filter(|s| only.is_empty() || only.contains(&s.name))
        .filter_map(|s| match &s.kind {
            Kind::Worker(_) => {
                let name = &n.workers[&s.name];
                seen.scripts
                    .contains_key(name)
                    .then(|| (s.name.clone(), name.clone()))
            }
            _ => None,
        })
        .collect();
    if only.is_empty() {
        let known: BTreeSet<&String> = n.workers.values().collect();
        for w in seen.env_workers(&prefix) {
            if !known.contains(&w) {
                workers.push(("-".into(), w));
            }
        }
    }
    // Data: everything of this environment, or, with services named, theirs
    // and only when no Worker that stays still uses it (REQ-011).
    let resources: Vec<&Found> = if !purge {
        Vec::new()
    } else if only.is_empty() {
        ours.iter().collect()
    } else {
        let keys: BTreeSet<String> = crate::local::purge_targets(&ctx.compose, only)?
            .into_iter()
            .filter(|(k, _)| *k != "do")
            .map(|(_, key)| key)
            .collect();
        let wanted: BTreeSet<&str> = n
            .resources
            .iter()
            .filter(|r| keys.contains(&r.key))
            .map(|r| r.name.as_str())
            .collect();
        ours.iter().filter(|f| wanted.contains(f.name.as_str())).collect()
    };

    // With --purge, the images of this environment's containers in
    // Cloudflare's registry go too (REQ-011, REQ-020): the repositories named
    // after the applications of the Workers taken down, and of the compose
    // file's containers (whose Workers may have gone with an earlier down).
    let mut images = Vec::new();
    if purge && seen.apps_error.is_none() {
        let mut repos: Vec<String> = Vec::new();
        for (_, script) in &workers {
            for c in seen.classes.get(script).into_iter().flatten() {
                repos.push(crate::containers::app_name(script, c));
            }
        }
        for s in ctx
            .compose
            .services
            .iter()
            .filter(|s| only.is_empty() || only.contains(&s.name))
        {
            if let Kind::Worker(w) = &s.kind {
                for k in &w.containers {
                    repos.push(crate::containers::app_name(&n.workers[&s.name], &k.class));
                }
            }
        }
        if !repos.is_empty() {
            images = registry_targets(&registry_images(ctx, &cf)?, &repos);
        }
    }
    if workers.is_empty() && resources.is_empty() && images.is_empty() {
        say(crate::tr!(
            "環境 {} に下ろすものはありません",
            "nothing to take down in environment {}",
            ctx.env
        ));
        return Ok(());
    }
    for (svc, name) in &workers {
        for (app, _) in seen.apps_of(name, &[]) {
            eprintln!("  {}", crate::tr!("コンテナ {app}", "container {app}"));
        }
        let classes = seen.classes.get(name).cloned().unwrap_or_default();
        let what = if classes.is_empty() {
            crate::tr!("Worker {name}({svc})", "Worker {name} ({svc})")
        } else {
            crate::tr!(
                "Worker {name}({svc})。Durable Object {} の中身も消えます",
                "Worker {name} ({svc}); the data of Durable Objects {} goes with it",
                classes.join(", ")
            )
        };
        eprintln!("  {what}");
    }
    // Names only: the contents are not counted remotely (REQ-011).
    for r in &resources {
        eprintln!("  {} {}", kind_label(r.kind), r.name);
    }
    for i in &images {
        eprintln!(
            "  {}",
            crate::tr!("レジストリのイメージ {i}", "registry image {i}")
        );
    }
    if ctx.dry_run {
        say(crate::tr!(
            "上を消します。dry-run なので何も変えていません",
            "the above would be deleted; dry run: nothing was changed"
        ));
        return Ok(());
    }
    crate::confirm::retype_env(
        &ctx.env,
        &if purge {
            crate::tr!(
                "上の Worker とデータを消す",
                "delete the Workers and data above"
            )
        } else {
            crate::tr!(
                "上の Worker を下ろす(データは残る)",
                "take down the Workers above (data stays)"
            )
        },
    )?;

    let mut records = read_records(ctx);
    for (svc, name) in &workers {
        act.run(
            crate::tr!("下ろす: {svc}({name})", "taking down {svc} ({name})"),
            || {
                delete_worker(&cf, &seen, name)?;
                remove_record(&mut records, name);
                Ok(())
            },
        )?;
    }
    for r in &resources {
        delete_found(&cf, &act, &seen, &ours, r)?;
    }
    for i in &images {
        act.run(
            crate::tr!(
                "消す: レジストリのイメージ {i}",
                "deleting registry image {i}"
            ),
            || {
                run_wrangler(
                    wrangler(ctx, &cf, &["containers", "images", "delete", i]),
                    crate::tr!(
                        "レジストリのイメージ {i} を消せません(上の出力を見てください)",
                        "cannot delete registry image {i} (see the output above)"
                    ),
                )
            },
        )?;
    }
    write_records(ctx, &records)?;
    say(crate::tr!("下ろしました", "taken down"));
    Ok(())
}

/// Deletes one resource of this environment: an R2 bucket is emptied first,
/// a queue is taken off its consumers and off the buckets that notify it.
fn delete_found(cf: &Cf, act: &Act, seen: &Seen, ours: &[Found], r: &Found) -> Result<()> {
    if r.kind == "r2" {
        act.run(
            crate::tr!("空にする: R2 {}", "emptying R2 {}", r.name),
            || cf.r2_empty(&r.name, &r.jurisdiction).map(|_| ()),
        )?;
    }
    act.run(
        crate::tr!("消す: {} {}", "deleting {} {}", kind_label(r.kind), r.name),
        || match r.kind {
            "d1" => cf.delete(&format!("/d1/database/{}", r.id)),
            "kv" => cf.delete(&format!("/storage/kv/namespaces/{}", r.id)),
            "r2" => cf.delete_with(
                &format!("/r2/buckets/{}", r.name),
                &[("cf-r2-jurisdiction", r.jurisdiction.as_str())],
            ),
            _ => {
                let qid = r.id.as_str();
                if let Some(q) = seen.queues.get(&r.name) {
                    for c in q["consumers"].as_array().into_iter().flatten() {
                        if let Some(cid) = c["consumer_id"].as_str() {
                            let _ = cf.delete(&format!("/queues/{qid}/consumers/{cid}"));
                        }
                    }
                }
                // Cloudflare refuses to delete a queue that R2 notifies (11017).
                for b in ours.iter().filter(|b| b.kind == "r2") {
                    let h = [("cf-r2-jurisdiction", b.jurisdiction.as_str())];
                    let conf = format!("/event_notifications/r2/{}/configuration", b.name);
                    let now = match cf.get_with(&conf, &h) {
                        Ok(v) => v,
                        Err(e) if cf::is_missing(&e) => continue,
                        Err(e) => return Err(e),
                    };
                    for x in now["queues"].as_array().into_iter().flatten() {
                        // R2 gives the id with dashes, Queues without.
                        if x["queueId"].as_str().unwrap_or_default().replace('-', "") == qid {
                            cf.delete_with(&format!("{conf}/queues/{qid}"), &h)?;
                        }
                    }
                }
                cf.delete(&format!("/queues/{qid}"))
            }
        },
    )
}

// ---------------------------------------------------------------- data

/// A remote environment's data through Cloudflare's APIs for D1, KV and R2
/// (REQ-013): only what is named as this environment's (REQ-006). What does
/// not exist yet cannot be read or written; `up` makes it first.
pub fn data_client(ctx: &Ctx) -> Result<crate::data::Client> {
    let n = names(&ctx.compose, &ctx.env)?;
    let cf = Cf::connect(ctx)?;
    let seen = look(&cf, &n)?;
    let ids = seen.ids(&n);
    let missing: Vec<String> = n
        .resources
        .iter()
        .filter(|r| matches!(r.kind, "d1" | "kv" | "r2") && !seen.has(r))
        .map(|r| format!("{} {}", kind_label(r.kind), r.name))
        .collect();
    if !missing.is_empty() {
        bail!(crate::tr!(
            "環境 {} にまだ無いものがあります: {}。先に up してください(何もしていません)",
            "environment {} does not have these yet: {}; run up first (nothing was done)",
            ctx.env,
            missing.join(", ")
        ));
    }
    let r2 = n
        .resources
        .iter()
        .filter(|r| r.kind == "r2")
        .map(|r| (r.key.clone(), (r.name.clone(), jur(r))))
        .collect();
    Ok(crate::data::Client::remote(crate::data::Remote {
        d1: ids.d1.iter().map(|(k, (_, id))| (k.clone(), id.clone())).collect(),
        kv: ids.kv.clone(),
        r2,
        cf,
    }))
}

// ---------------------------------------------------------------- ps

pub fn ps(ctx: &Ctx) -> Result<()> {
    let n = names(&ctx.compose, &ctx.env)?;
    let cf = Cf::connect(ctx)?;
    let seen = look(&cf, &n)?;
    let ids = seen.ids(&n);
    crate::out!("{:<16} {:<8} {:<10} NAME / URL", "SERVICE", "TYPE", "STATE");
    let state = |has: bool| {
        if has {
            crate::tr!("あり", "present")
        } else {
            crate::tr!("無い", "missing")
        }
    };
    for s in &ctx.compose.services {
        match &s.kind {
            Kind::Worker(w) => {
                let name = &n.workers[&s.name];
                let has = seen.scripts.contains_key(name);
                let mut where_ = vec![name.clone()];
                where_.extend(w.routes.iter().cloned());
                if has
                    && !w.ports.is_empty()
                    && w.routes.is_empty()
                    && let Some(sub) = &seen.subdomain
                {
                    where_.push(format!("https://{name}.{sub}.workers.dev"));
                }
                crate::out!(
                    "{:<16} {:<8} {:<10} {}",
                    s.name,
                    "worker",
                    state(has),
                    where_.join("  ")
                );
                // Its containers (REQ-020), and ones the compose file no
                // longer wants (the next up deletes them).
                for k in &w.containers {
                    let app = crate::containers::app_name(name, &k.class);
                    crate::out!(
                        "{:<16} {:<8} {:<10} {}",
                        format!("{}.{}", s.name, k.class),
                        "container",
                        state(seen.apps.contains_key(&app)),
                        app
                    );
                }
                for (app, _) in stale_apps(&seen, name, w, None) {
                    crate::out!(
                        "{:<16} {:<8} {:<10} {app}",
                        "-",
                        "container",
                        crate::tr!("残り(up が消す)", "orphan (up deletes it)")
                    );
                }
            }
            other => {
                for key in crate::compose::resource_keys(s) {
                    let Some(r) = n.res(&key) else { continue };
                    let has = seen.has(r);
                    let mut note = r.name.clone();
                    if let (Kind::D1(list), Some((_, id))) = (other, ids.d1.get(&key))
                        && let Some(dir) = list
                            .iter()
                            .find(|x| x.key == key)
                            .and_then(|x| x.spec.migrations.as_ref())
                    {
                        let pending = pending_remote(&cf, id, dir).map(|p| p.len()).unwrap_or(0);
                        if pending > 0 {
                            note.push_str(&crate::tr!(
                                "  (未適用の移行 {pending})",
                                "  ({pending} pending migrations)"
                            ));
                        }
                    }
                    crate::out!(
                        "{:<16} {:<8} {:<10} {}",
                        s.name,
                        other.type_name(),
                        state(has),
                        note
                    );
                }
            }
        }
    }
    // This environment's things (by name) the compose file no longer has.
    let prefix = env_prefix(&ctx.compose, &ctx.env);
    let known: BTreeSet<&String> = n.workers.values().collect();
    for o in seen.env_workers(&prefix).into_iter().filter(|w| !known.contains(w)) {
        crate::out!(
            "{:<16} {:<8} {:<10} {o}",
            "-",
            "worker",
            crate::tr!("残り(up が消す)", "orphan (up deletes it)")
        );
        for (app, _) in seen.apps_of(&o, &[]) {
            crate::out!(
                "{:<16} {:<8} {:<10} {app}",
                "-",
                "container",
                crate::tr!("残り(up が消す)", "orphan (up deletes it)")
            );
        }
    }
    let in_compose: BTreeSet<&str> = n.resources.iter().map(|r| r.name.as_str()).collect();
    for f in seen
        .env_resources(&prefix)
        .into_iter()
        .filter(|f| !in_compose.contains(f.name.as_str()))
    {
        crate::out!(
            "{:<16} {:<8} {:<10} {}",
            "-",
            f.kind,
            crate::tr!("残り(up --purge が消す)", "orphan (up --purge deletes it)"),
            f.name
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- logs

/// `wrangler tail` for each Worker, every line tagged with its service
/// (REQ-012). Remote logs start from now (Cloudflare keeps no history to
/// read here), so this always follows until Ctrl+C.
pub fn logs(ctx: &Ctx, follow: bool, only: &[String]) -> Result<()> {
    if !only.is_empty() {
        ctx.compose.closure(only)?;
    }
    let n = names(&ctx.compose, &ctx.env)?;
    let cf = Cf::connect(ctx)?;
    if !follow {
        say(crate::tr!(
            "リモートのログは今からのものだけを追って出します(Ctrl+C で止まります)",
            "remote logs are shown from now on (Ctrl+C stops)"
        ));
    }
    let mut children = Vec::new();
    for s in &ctx.compose.services {
        if !matches!(s.kind, Kind::Worker(_)) || !(only.is_empty() || only.contains(&s.name)) {
            continue;
        }
        let name = n.workers[&s.name].clone();
        let mut cmd = wrangler(ctx, &cf, &["tail", &name, "--format", "json"]);
        cmd.stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = cmd.spawn().context(crate::tr!(
            "Wrangler を起動できません",
            "cannot start Wrangler"
        ))?;
        let out = child.stdout.take().ok_or_else(|| anyhow!("tail"))?;
        let svc = s.name.clone();
        std::thread::spawn(move || {
            for event in JsonObjects::new(std::io::BufReader::new(out)) {
                for line in tail_lines(&svc, &event) {
                    crate::out!("{line}");
                }
            }
        });
        children.push(child);
    }
    while crate::interrupt::check().is_ok() {
        if children
            .iter_mut()
            .all(|c| matches!(c.try_wait(), Ok(Some(_))))
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    for mut c in children {
        let _ = c.kill();
        let _ = c.wait();
    }
    Ok(())
}

/// The lines of one tail event, like the local receivers write them.
pub fn tail_lines(svc: &str, e: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |m: String| {
        for l in m.lines() {
            out.push(format!("{svc:<12} | {l}"));
        }
        if m.is_empty() {
            out.push(format!("{svc:<12} | "));
        }
    };
    for l in e["logs"].as_array().into_iter().flatten() {
        let parts: Vec<String> = l["message"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| match p {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect();
        push(parts.join(" "));
    }
    for x in e["exceptions"].as_array().into_iter().flatten() {
        push(format!(
            "✘ {}: {}",
            x["name"].as_str().unwrap_or("Error"),
            x["message"].as_str().unwrap_or("")
        ));
    }
    let ev = &e["event"];
    if let Some(url) = ev["request"]["url"].as_str() {
        let path = url
            .split_once("://")
            .and_then(|(_, rest)| rest.find('/').map(|i| &rest[i..]))
            .unwrap_or("/");
        let status = ev["response"]["status"]
            .as_u64()
            .map(|s| s.to_string())
            .unwrap_or_else(|| e["outcome"].as_str().unwrap_or("").to_string());
        push(format!(
            "{} {path} {status}",
            ev["request"]["method"].as_str().unwrap_or("GET")
        ));
    } else if let Some(cron) = ev["cron"].as_str() {
        push(format!(
            "cron {cron} {}",
            e["outcome"].as_str().unwrap_or("")
        ));
    } else if let Some(q) = ev["queue"].as_str() {
        push(format!(
            "queue {q} ({}) {}",
            ev["batchSize"].as_u64().unwrap_or(0),
            e["outcome"].as_str().unwrap_or("")
        ));
    }
    out
}

/// Whole JSON objects from a stream that may carry other text between them
/// (`wrangler tail --format json` prints each event over several lines).
pub struct JsonObjects<R> {
    reader: R,
}

impl<R: std::io::BufRead> JsonObjects<R> {
    pub fn new(reader: R) -> Self {
        JsonObjects { reader }
    }
}

impl<R: std::io::BufRead> Iterator for JsonObjects<R> {
    type Item = Value;
    fn next(&mut self) -> Option<Value> {
        let mut buf = String::new();
        let (mut depth, mut in_str, mut esc) = (0usize, false, false);
        loop {
            let mut byte = [0u8; 1];
            match self.reader.read(&mut byte) {
                Ok(0) | Err(_) => return None,
                Ok(_) => {}
            }
            let c = byte[0];
            if depth == 0 && c != b'{' {
                continue;
            }
            buf.push(c as char);
            if in_str {
                match (esc, c) {
                    (true, _) => esc = false,
                    (false, b'\\') => esc = true,
                    (false, b'"') => in_str = false,
                    _ => {}
                }
                continue;
            }
            match c {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        // Bytes were pushed one by one: rebuild as UTF-8.
                        let bytes: Vec<u8> = buf.chars().map(|c| c as u8).collect();
                        if let Ok(v) = serde_json::from_slice(&bytes) {
                            return Some(v);
                        }
                        buf.clear();
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_cloudflare_names() {
        assert!(check_name("worker", "shop-staging-api").is_none());
        assert!(check_name("worker", &"a".repeat(64)).is_some());
        assert!(check_name("r2", "ab").is_some());
        assert!(check_name("queue", "Shop").is_some());
        assert!(check_name("d1", "-x").is_some());
        assert!(check_name("kv", "Any Title").is_none());
    }

    #[test]
    fn declares_durable_object_classes() {
        let want = vec!["Counter".to_string()];
        assert!(do_exports(&[], None).is_none());
        assert_eq!(
            do_exports(&want, Some(&vec!["Old".to_string()])).unwrap(),
            json!({ "Counter": { "type": "durable-object", "storage": "sqlite" },
                    "Old": { "type": "durable-object", "state": "deleted" } })
        );
    }

    #[test]
    fn turns_bucket_settings_into_cloudflare_rules() {
        let l = lifecycle_rules(&[json!({ "prefix": "tmp/", "expire_after_days": 7 })]);
        assert_eq!(l[0]["id"], "wc-1");
        assert_eq!(
            l[0]["deleteObjectsTransition"]["condition"]["maxAge"],
            7 * DAY
        );
        let k = lock_rules(&[json!({ "retain_days": 1 }), json!({ "indefinite": true })]);
        assert_eq!(k[0]["condition"]["maxAgeSeconds"], DAY);
        assert_eq!(k[1]["condition"]["type"], "Indefinite");
        assert_eq!(
            normalize(&json!({ "b": [], "a": 1, "c": null })),
            json!({ "a": 1 })
        );
    }

    #[test]
    fn tags_tail_events_like_local_logs() {
        let e = json!({ "outcome": "ok", "logs": [{ "message": ["hi", { "n": 1 }], "level": "log" }],
            "exceptions": [], "event": { "request": { "url": "https://x.workers.dev/a?b", "method": "POST" },
            "response": { "status": 201 } } });
        assert_eq!(
            tail_lines("api", &e),
            [
                "api          | hi {\"n\":1}",
                "api          | POST /a?b 201"
            ]
        );
        let text = "Connected\n{\n \"a\": \"}{\",\n \"b\": 1\n}\nnoise {\"c\": 2}";
        let got: Vec<Value> = JsonObjects::new(std::io::Cursor::new(text)).collect();
        assert_eq!(got, [json!({ "a": "}{", "b": 1 }), json!({ "c": 2 })]);
    }
    #[test]
    fn container_hashes_ride_in_the_version_message() {
        let hashes = vec![
            ("Pinger".to_string(), "aaaaaaaaaaaa".to_string()),
            ("Echo".to_string(), "bbbbbbbbbbbb".to_string()),
        ];
        let msg = container_message(&hashes).unwrap();
        assert!(msg.len() <= 100, "{msg}");
        let back = message_hashes(Some(&msg));
        assert_eq!(back.get("Pinger").map(String::as_str), Some("aaaaaaaaaaaa"));
        assert_eq!(back.get("Echo").map(String::as_str), Some("bbbbbbbbbbbb"));
        assert!(message_hashes(Some("deployed by hand")).is_empty());
        assert!(message_hashes(None).is_empty());
        assert!(container_message(&[]).is_none());
        // Too long for Cloudflare's message: not written (the plan then
        // says the image may change).
        let many: Vec<(String, String)> = (0..10)
            .map(|i| (format!("VeryLongClassName{i}"), "cccccccccccc".to_string()))
            .collect();
        assert!(container_message(&many).is_none());
    }

    #[test]
    fn the_version_tag_does_not_depend_on_where_the_dockerfile_is() {
        let built = Built {
            dir: "/b".into(),
            entry: "index.js".into(),
            hash: "h".into(),
        };
        let cfg = |path: &str| {
            json!({ "name": "w", "containers": [
            { "class_name": "Pinger", "image": path, "image_build_context": "/x" } ] })
        };
        let h = vec![("Pinger".to_string(), "aaaaaaaaaaaa".to_string())];
        assert_eq!(
            code_tag(&built, &cfg("/home/a/box/Dockerfile"), &h),
            code_tag(&built, &cfg("/Users/b/box/Dockerfile"), &h)
        );
        let h2 = vec![("Pinger".to_string(), "dddddddddddd".to_string())];
        assert_ne!(
            code_tag(&built, &cfg("/home/a/box/Dockerfile"), &h),
            code_tag(&built, &cfg("/home/a/box/Dockerfile"), &h2)
        );
    }

    #[test]
    fn applications_are_found_by_their_durable_object_namespace() {
        let mut seen = Seen::default();
        seen.classes
            .insert("shop--stg--api".into(), vec!["Pinger".into()]);
        seen.namespaces
            .insert(("shop--stg--api".into(), "Pinger".into()), "ns-1".into());
        let app = |id: &str, name: &str, ns: &str| {
            (
                name.to_string(),
                json!({ "id": id, "name": name, "durable_objects": { "namespace_id": ns } }),
            )
        };
        let ours = crate::containers::app_name("shop--stg--api", "Pinger");
        for (name, v) in [
            app("a1", "renamed-by-hand", "ns-1"), // ours by namespace
            app("a2", &ours, "ns-other"),         // our name, another's namespace
            app("a3", "shop-stg-api-v2-pinger-x", "ns-2"), // a look-alike name
            app(
                "a4",
                &crate::containers::app_name("shop--stg--api", "Gone"),
                "ns-gone",
            ), // a deleted class, by name
        ] {
            seen.apps.insert(name, v);
        }
        let mut found: Vec<String> = seen
            .apps_of("shop--stg--api", &["Gone".to_string()])
            .into_iter()
            .map(|(_, id)| id)
            .collect();
        found.sort();
        assert_eq!(found, vec!["a1", "a4"]);
    }

    #[test]
    fn registry_images_to_purge_are_only_this_environments() {
        let ours = crate::containers::app_name("shop--stg--api", "Pinger");
        let list = json!([
            { "name": ours, "tags": ["1b5aa97e", "5bd0d054"] },
            { "name": crate::containers::app_name("shop--prod--api", "Pinger"), "tags": ["aaaa"] },
            { "name": "my-prebuilt-image", "tags": ["1.0"] },
        ]);
        assert_eq!(
            registry_targets(&list, &[ours.clone()]),
            vec![format!("{ours}:1b5aa97e"), format!("{ours}:5bd0d054")]
        );
        assert!(registry_targets(&list, &[]).is_empty());
        assert!(registry_targets(&json!(null), &[ours]).is_empty());
    }

    #[test]
    fn instance_types_are_compared_by_the_resources_cloudflare_reports() {
        use crate::compose::{Container, Image, Worker};
        let k = |t: Option<Value>| Container {
            class: "Pinger".into(),
            image: Image::Registry("docker.io/library/busybox:1.36".into()),
            build_context: None,
            image_vars: Default::default(),
            instance_type: t,
            max_instances: Some(1),
        };
        let script = "shop--stg--api";
        let name = crate::containers::app_name(script, "Pinger");
        // As Cloudflare answers (measured): no instance_type, the resources.
        let app = |vcpu: f64, mem: u64, disk: u64| {
            json!({ "id": "a1", "name": name,
            "configuration": { "instance_type": null, "vcpu": vcpu, "memory_mib": mem,
                               "disk": { "size_mb": disk, "size": "x" } },
            "max_instances": 1 })
        };
        let plan = |want: Option<Value>, have: Value| {
            let mut seen = Seen::default();
            seen.apps.insert(name.clone(), have);
            let mut w = Worker::default();
            w.durable_objects = vec!["Pinger".into()];
            w.containers = vec![k(want)];
            let deployed: BTreeMap<String, String> = [(
                "Pinger".to_string(),
                crate::containers::content_hash(&w.containers[0]),
            )]
            .into();
            container_plan(&seen, script, &w, Path::new("/"), &deployed, None)
        };
        let custom = json!({ "vcpu": 1, "memory_mib": 3072, "disk_mb": 4000 });
        assert_eq!(plan(Some(custom.clone()), app(1.0, 3072, 4000))[0].0, '=');
        assert_eq!(
            plan(None, app(0.0625, 256, 2000))[0].0,
            '=',
            "lite is the default"
        );
        assert_eq!(plan(Some(json!("basic")), app(0.25, 1024, 4000))[0].0, '=');
        let changed = plan(Some(json!("lite")), app(1.0, 3072, 4000));
        assert_eq!(changed[0].0, '~');
        assert!(
            changed[0].2.iter().any(|d| d.starts_with("instance_type:")),
            "{:?}",
            changed[0].2
        );
    }

    #[test]
    fn a_dropped_container_is_planned_with_its_registry_images() {
        use crate::compose::Worker;
        let script = "shop--stg--api";
        let name = crate::containers::app_name(script, "Pinger");
        let mut seen = Seen::default();
        seen.classes.insert(script.into(), vec!["Pinger".into()]);
        seen.apps
            .insert(name.clone(), json!({ "id": "a1", "name": name }));
        let mut w = Worker::default();
        w.durable_objects = vec!["Pinger".into()];
        let lines = container_plan(&seen, script, &w, Path::new("/"), &BTreeMap::new(), None);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].0, '-');
        assert!(
            lines[0]
                .2
                .iter()
                .any(|d| d.contains("registry") || d.contains("レジストリ")),
            "{:?}",
            lines[0].2
        );
    }

}
