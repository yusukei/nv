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
    /// Its name on Cloudflare (`name:`, or `<project>-<env>-<key>`).
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
    /// The data service older versions deployed (deleted when found).
    pub data: String,
}

impl Names {
    fn res(&self, key: &str) -> Option<&Res> {
        self.resources.iter().find(|r| r.key == key)
    }
}

pub fn names(c: &Compose, env: &str) -> Result<Names> {
    let prefix = format!("{}-{env}-", c.name);
    let mut workers = BTreeMap::new();
    let mut resources = Vec::new();
    let mut bad = Vec::new();
    for s in &c.services {
        match &s.kind {
            Kind::Worker(w) => {
                let name = w
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("{prefix}{}", s.name));
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
        // Only looked for, to delete one an older version left (REQ-017):
        // never deployed, so its length never stops anything.
        data: format!("{prefix}wc-data"),
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
        name: name.clone().unwrap_or_else(|| format!("{prefix}{key}")),
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
struct Seen {
    scripts: BTreeMap<String, Value>,
    d1: BTreeMap<String, String>,
    kv: BTreeMap<String, String>,
    /// (jurisdiction or "default", bucket name).
    r2: BTreeSet<(String, String)>,
    queues: BTreeMap<String, Value>,
    /// Script name → its Durable Object classes on Cloudflare.
    classes: BTreeMap<String, Vec<String>>,
    /// (script, class) of namespaces on the old key-value storage.
    legacy_kv: BTreeSet<(String, String)>,
    subdomain: Option<String>,
}

/// Deletes a Worker, first taking it off the queues it consumes: Cloudflare
/// refuses to delete a queue consumer (10064).
fn delete_worker(cf: &Cf, seen: &Seen, name: &str) -> Result<()> {
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
    let mut legacy_kv = BTreeSet::new();
    for ns in cf.list("/workers/durable_objects/namespaces")? {
        if let (Some(script), Some(class)) = (ns["script"].as_str(), ns["class"].as_str()) {
            classes
                .entry(script.to_string())
                .or_default()
                .push(class.to_string());
            if ns["use_sqlite"] == false {
                legacy_kv.insert((script.to_string(), class.to_string()));
            }
        }
    }
    Ok(Seen {
        scripts,
        d1,
        kv,
        r2,
        queues,
        classes,
        legacy_kv,
        subdomain: cf.workers_subdomain()?,
    })
}

impl Seen {
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
}

impl Act {
    fn run(&self, what: String, f: impl FnOnce() -> Result<()>) -> Result<()> {
        if self.dry {
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

pub fn up(ctx: &Ctx, rebuild: bool, only: &[String], remove_orphans: bool) -> Result<()> {
    let services = ctx.compose.closure(only)?;
    let n = names(&ctx.compose, &ctx.env)?;
    let act = Act { dry: ctx.dry_run };

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

    // 2. What Cloudflare has.
    let cf = Cf::connect(ctx)?;
    let seen = look(&cf, &n)?;
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
    let orphans = orphans(ctx, &n, &seen, &records);
    if !orphans.is_empty() {
        if remove_orphans {
            asks.push(crate::tr!(
                "compose ファイルに無い Worker を消します: {}(データは残ります)",
                "Workers not in the compose file will be deleted: {} (data stays)",
                orphans.join(", ")
            ));
        } else {
            say(crate::tr!(
                "compose ファイルに無い Worker があります: {}(消すには --remove-orphans)",
                "Workers not in the compose file remain: {} (--remove-orphans deletes them)",
                orphans.join(", ")
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

    // 4. Resources: make what is missing (REQ-010).
    let mut ids = seen.ids(&n);
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

    // 6. Nothing of the tool runs in a remote environment (REQ-017): a data
    //    service an older version deployed goes, with its token here.
    remove_old_data_service(ctx, &cf, &act, &n, &seen, &mut records)?;

    // 7. In dependency order: migrations, then Workers (REQ-010). A failed
    //    migration stops everything after it.
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
                deploy_worker(
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
                    &mut records,
                )?;
            }
            _ => {}
        }
    }

    // 8. Dropped Workers, only when asked (REQ-010).
    if remove_orphans {
        for o in &orphans {
            act.run(
                crate::tr!("消す: Worker {o}", "deleting Worker {o}"),
                || {
                    delete_worker(&cf, &seen, o)?;
                    remove_record(&mut records, o);
                    Ok(())
                },
            )?;
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

/// A data service (`<project>-<env>-wc-data`) that versions before 0.1.11
/// deployed: reachable on workers.dev with full access to the environment's
/// data, so it is deleted, and this machine's token for it too.
fn remove_old_data_service(
    ctx: &Ctx,
    cf: &Cf,
    act: &Act,
    n: &Names,
    seen: &Seen,
    records: &mut Value,
) -> Result<()> {
    if seen.scripts.contains_key(&n.data) {
        act.run(
            crate::tr!(
                "以前の版が置いた道具のデータのサービス {} を消す",
                "deleting {}, the data service an older version deployed",
                n.data
            ),
            || {
                run_wrangler(
                    wrangler(ctx, cf, &["delete", "--name", &n.data, "--force"]),
                    crate::tr!("{} を消せません", "cannot delete {}", n.data),
                )?;
                records["data"] = Value::Null;
                Ok(())
            },
        )?;
    }
    if !ctx.dry_run {
        let _ = std::fs::remove_file(ctx.env_dir().join("data-token"));
    }
    Ok(())
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
    records: &mut Value,
) -> Result<()> {
    let script = n.workers[&s.name].clone();
    let exists = seen.scripts.contains_key(&script);
    let legacy: Vec<String> = seen
        .legacy_kv
        .iter()
        .filter(|(sc, _)| *sc == script)
        .map(|(_, c)| c.clone())
        .collect();
    let declared: Vec<String> = w
        .durable_objects
        .iter()
        .chain(kept.into_iter().flatten())
        .cloned()
        .collect();
    let exports = do_exports(&declared, dropped, &legacy);
    let cfg = configs::remote_worker(&s.name, w, built, ids, &cf.account, exports);
    let secrets = &w.secrets;
    let secret_text = serde_json::to_string(&secrets)?;
    // Deleted-class tombstones only matter once: the next deploy without
    // them is the same Worker.
    let mut same = cfg.clone();
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
    let unchanged = exists && records["workers"][&script]["hash"] == hash.as_str();
    if unchanged {
        say(crate::tr!(
            "{}: 変わっていないので展開しません",
            "{}: unchanged, not deployed",
            s.name
        ));
    } else {
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
                let mut cmd = wrangler(ctx, cf, &["deploy", "-c"]);
                cmd.arg(&cfg_path);
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
    Ok(())
}

/// Durable Object classes, declared (DES-010): Cloudflare compares the
/// `exports` map with what it has and creates or deletes namespaces, so no
/// migration tags are kept. Dropped classes become `deleted` tombstones
/// (after the user agreed); classes already on the old key-value storage
/// keep it.
pub fn do_exports(
    want: &[String],
    dropped: Option<&Vec<String>>,
    legacy_kv: &[String],
) -> Option<Value> {
    let mut map = serde_json::Map::new();
    for c in want {
        let storage = if legacy_kv.contains(c) {
            "legacy-kv"
        } else {
            "sqlite"
        };
        map.insert(
            c.clone(),
            json!({ "type": "durable-object", "storage": storage }),
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

/// Workers the compose file no longer has: only those this machine's
/// record says the tool deployed. A name alone does not tell environments
/// apart (`staging` and `staging-eu` share the start), so names are not
/// matched (REQ-006).
fn orphans(ctx: &Ctx, n: &Names, seen: &Seen, records: &Value) -> Vec<String> {
    let known: BTreeSet<&String> = n.workers.values().collect();
    let mut out: BTreeSet<String> = BTreeSet::new();
    for name in records["workers"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, _)| k)
    {
        if !known.contains(name) && seen.scripts.contains_key(name) {
            out.insert(name.clone());
        }
    }
    let _ = ctx;
    out.into_iter().collect()
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
    // Data that a Worker which stays still uses is never deleted (REQ-011).
    let targets: Vec<(&'static str, String)> = if purge {
        if only.is_empty() {
            n.resources
                .iter()
                .map(|r| (r.kind, r.key.clone()))
                .collect()
        } else {
            crate::local::purge_targets(&ctx.compose, only)?
                .into_iter()
                .filter(|(k, _)| *k != "do")
                .collect()
        }
    } else {
        Vec::new()
    };
    let cf = Cf::connect(ctx)?;
    let seen = look(&cf, &n)?;
    let ids = seen.ids(&n);
    let act = Act { dry: ctx.dry_run };

    // Workers to take down, dependents first (REQ-011).
    let order: Vec<&Service> = ctx.compose.services.iter().rev().collect();
    let workers: Vec<(&Service, &String)> = order
        .iter()
        .filter(|s| only.is_empty() || only.contains(&s.name))
        .filter_map(|s| match &s.kind {
            Kind::Worker(_) => {
                let name = &n.workers[&s.name];
                seen.scripts.contains_key(name).then_some((*s, name))
            }
            _ => None,
        })
        .collect();
    let data_too = only.is_empty() && seen.scripts.contains_key(&n.data);
    let resources: Vec<&Res> = targets
        .iter()
        .filter_map(|(_, key)| n.res(key))
        .filter(|r| seen.has(r))
        .collect();

    // R2 buckets cannot be emptied remotely for now, so `--purge` with one
    // stops before anything, the dry run as well (REQ-014).
    if let Some(r) = resources.iter().find(|r| r.kind == "r2") {
        bail!(crate::tr!(
            "R2 のバケット {} は、いまは --purge で消せません(作り直すまで止めています。何もしていません)",
            "R2 bucket {} cannot be removed by --purge for now (stopped until it is rebuilt; nothing was done)",
            r.name
        ));
    }
    if workers.is_empty() && resources.is_empty() && !data_too {
        say(crate::tr!(
            "環境 {} に下ろすものはありません",
            "nothing to take down in environment {}",
            ctx.env
        ));
        return Ok(());
    }
    for (s, name) in &workers {
        let classes = seen.classes.get(*name).cloned().unwrap_or_default();
        if classes.is_empty() {
            eprintln!(
                "  {}",
                crate::tr!("Worker {name}({})", "Worker {name} ({})", s.name)
            );
        } else {
            eprintln!(
                "  {}",
                crate::tr!(
                    "Worker {name}({})。Durable Object {} の中身も消えます",
                    "Worker {name} ({}); the data of Durable Objects {} goes with it",
                    s.name,
                    classes.join(", ")
                )
            );
        }
    }
    // Names only: the contents are not counted remotely (REQ-011).
    for r in &resources {
        eprintln!("  {} {}", kind_label(r.kind), r.name);
    }
    if data_too {
        eprintln!(
            "  {}",
            crate::tr!(
                "以前の版が置いた道具のデータのサービス {}",
                "{}, the data service an older version deployed",
                n.data
            )
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
    for (s, name) in &workers {
        act.run(
            crate::tr!("下ろす: {}({name})", "taking down {} ({name})", s.name),
            || {
                delete_worker(&cf, &seen, name)?;
                remove_record(&mut records, name);
                Ok(())
            },
        )?;
    }
    for r in &resources {
        act.run(
            crate::tr!("消す: {} {}", "deleting {} {}", kind_label(r.kind), r.name),
            || match r.kind {
                "d1" => cf.delete(&format!("/d1/database/{}", ids.d1[&r.key].1)),
                "kv" => cf.delete(&format!("/storage/kv/namespaces/{}", ids.kv[&r.key])),
                "r2" => {
                    cf.delete_with(&format!("/r2/buckets/{}", r.name), &headers(&jur_header(r)))
                }
                _ => {
                    let q = &seen.queues[&r.name];
                    let qid = q["queue_id"].as_str().unwrap_or_default();
                    for c in q["consumers"].as_array().into_iter().flatten() {
                        if let Some(cid) = c["consumer_id"].as_str() {
                            let _ = cf.delete(&format!("/queues/{qid}/consumers/{cid}"));
                        }
                    }
                    // Cloudflare refuses to delete a queue that R2 notifies (11017).
                    for b in n.resources.iter().filter(|b| b.kind == "r2") {
                        let h = jur_header(b);
                        let conf = format!("/event_notifications/r2/{}/configuration", b.name);
                        let now = match cf.get_with(&conf, &headers(&h)) {
                            Ok(v) => v,
                            Err(e) if cf::is_missing(&e) => continue,
                            Err(e) => return Err(e),
                        };
                        for x in now["queues"].as_array().into_iter().flatten() {
                            // R2 gives the id with dashes, Queues without.
                            if x["queueId"].as_str().unwrap_or_default().replace('-', "") == qid {
                                cf.delete_with(&format!("{conf}/queues/{qid}"), &headers(&h))?;
                            }
                        }
                    }
                    cf.delete(&format!("/queues/{qid}"))
                }
            },
        )?;
    }
    if data_too {
        remove_old_data_service(ctx, &cf, &act, &n, &seen, &mut records)?;
    }
    write_records(ctx, &records)?;
    say(crate::tr!("下ろしました", "taken down"));
    Ok(())
}

// ---------------------------------------------------------------- data

/// Remote D1, R2 and KV contents are not read or written by the tool:
/// nothing of the tool runs in a remote environment (REQ-017), and Cloudflare
/// is reached only through Wrangler. backup and restore against a remote
/// environment wait until they are built on Wrangler's commands.
pub fn data_client(ctx: &Ctx) -> Result<crate::data::Client> {
    bail!(crate::tr!(
        "リモートの環境 {} の backup・restore は、いまは使えません(作り直すまで止めています。何もしていません)",
        "backup and restore of the remote environment {} are unavailable for now (stopped until they are rebuilt; nothing was done)",
        ctx.env
    ))
}

// ---------------------------------------------------------------- ps

pub fn ps(ctx: &Ctx) -> Result<()> {
    let n = names(&ctx.compose, &ctx.env)?;
    let cf = Cf::connect(ctx)?;
    let seen = look(&cf, &n)?;
    let ids = seen.ids(&n);
    let records = read_records(ctx);
    println!("{:<16} {:<8} {:<10} NAME / URL", "SERVICE", "TYPE", "STATE");
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
                println!(
                    "{:<16} {:<8} {:<10} {}",
                    s.name,
                    "worker",
                    state(has),
                    where_.join("  ")
                );
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
                    println!(
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
    if seen.scripts.contains_key(&n.data) {
        println!(
            "{:<16} {:<8} {:<10} {}",
            "wc-data",
            "tool",
            crate::tr!("残り", "orphan"),
            crate::tr!(
                "{}(以前の版が置いた道具の Worker。up が消す)",
                "{} (left by an older version; up deletes it)",
                n.data
            )
        );
    }
    for o in orphans(ctx, &n, &seen, &records) {
        println!(
            "{:<16} {:<8} {:<10} {o}",
            "-",
            "worker",
            crate::tr!("残り", "orphan")
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
                    println!("{line}");
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
        assert!(do_exports(&[], None, &[]).is_none());
        assert_eq!(
            do_exports(&want, Some(&vec!["Old".to_string()]), &[]).unwrap(),
            json!({ "Counter": { "type": "durable-object", "storage": "sqlite" },
                    "Old": { "type": "durable-object", "state": "deleted" } })
        );
        assert_eq!(
            do_exports(&want, None, &want).unwrap()["Counter"]["storage"],
            "legacy-kv"
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
}
