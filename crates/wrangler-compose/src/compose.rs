//! The compose file (REQ-001〜005, DES-002): find it, expand variables,
//! check every key, and turn it into typed services in dependency order.

use crate::interp::{self, Lookup, Missing};
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub const FILE_NAMES: [&str; 2] = ["wrangler-compose.yaml", "wrangler-compose.yml"];

#[derive(Debug)]
pub struct Compose {
    pub name: String,
    /// The folder holding the compose file (REQ-001).
    pub root: PathBuf,
    pub file: PathBuf,
    /// The folder whose package.json gives Wrangler and Node: top-level
    /// `context`, else `root` (REQ-001).
    pub context: PathBuf,
    /// `.env.<env>`, and the `CLOUDFLARE_ACCOUNT_ID` written in it: a
    /// remote environment works only on that account (REQ-006).
    pub env_file: PathBuf,
    pub account_id: Option<String>,
    /// The local S3-compatible API keys (REQ-009): random per project, kept
    /// in `.wrangler/compose/local/s3-keys.json`. None for other environments.
    pub local_s3: Option<S3Keys>,
    /// In dependency order: what a service needs comes before it.
    pub services: Vec<Service>,
}

/// An access key and its secret for the local S3-compatible API.
#[derive(Clone, Debug, PartialEq)]
pub struct S3Keys {
    pub key: String,
    pub secret: String,
    /// Made by this load and not yet written: `up` writes it (nothing else
    /// does, so `--dry-run` changes nothing).
    pub fresh: bool,
}

/// Where the local S3 keys are kept (under the tool's folder, out of git).
pub fn s3_keys_file(root: &Path) -> PathBuf {
    root.join(".wrangler")
        .join("compose")
        .join("local")
        .join("s3-keys.json")
}

/// The kept keys, or new random ones (`fresh`) when none are kept yet.
fn local_s3_keys(root: &Path) -> Result<S3Keys> {
    let file = s3_keys_file(root);
    if let Ok(text) = std::fs::read_to_string(&file) {
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if let (Some(key), Some(secret)) = (v["key"].as_str(), v["secret"].as_str())
            && !key.is_empty()
            && !secret.is_empty()
        {
            return Ok(S3Keys {
                key: key.to_string(),
                secret: secret.to_string(),
                fresh: false,
            });
        }
    }
    let random = |n: usize| -> Result<String> {
        let mut b = vec![0u8; n];
        getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("random: {e}"))?;
        Ok(crate::files::hex(&b))
    };
    Ok(S3Keys {
        key: format!("wc{}", random(10)?),
        secret: random(32)?,
        fresh: true,
    })
}

#[derive(Debug)]
pub struct Service {
    pub name: String,
    pub kind: Kind,
    /// Local ports of an r2 service: its S3-compatible API and public URLs
    /// (REQ-009). Workers keep theirs in `Worker::ports`.
    pub ports: Vec<Port>,
}

#[derive(Debug)]
pub enum Kind {
    Worker(Box<Worker>),
    D1(Vec<Resource<D1>>),
    R2(Vec<Resource<Bucket>>),
    Kv(Vec<Resource<()>>),
    Queue(Vec<Resource<()>>),
}

impl Service {
    /// The last part of its name on Cloudflare: `name:`, else the service
    /// name (REQ-006).
    pub fn cloud_name(&self) -> &str {
        let explicit = match &self.kind {
            Kind::Worker(w) => w.name.as_deref(),
            Kind::D1(l) => l.first().and_then(|r| r.name.as_deref()),
            Kind::R2(l) => l.first().and_then(|r| r.name.as_deref()),
            Kind::Kv(l) => l.first().and_then(|r| r.name.as_deref()),
            Kind::Queue(l) => l.first().and_then(|r| r.name.as_deref()),
        };
        explicit.unwrap_or(&self.name)
    }
}

impl Kind {
    pub fn type_name(&self) -> &'static str {
        match self {
            Kind::Worker(_) => "worker",
            Kind::D1(_) => "d1",
            Kind::R2(_) => "r2",
            Kind::Kv(_) => "kv",
            Kind::Queue(_) => "queue",
        }
    }
}

/// One D1 database, bucket, namespace or queue of a service.
#[derive(Debug)]
pub struct Resource<T> {
    /// `None` in the short form (the service itself is the resource).
    #[allow(dead_code)] // for remote environments (not built yet)
    pub sub: Option<String>,
    /// The resource's own name: the service name, or `<service>-<sub>`
    /// (REQ-006). Locally this is also its actual name.
    pub key: String,
    /// Explicit actual name on Cloudflare (`name:`).
    #[allow(dead_code)] // for remote environments (not built yet)
    pub name: Option<String>,
    pub spec: T,
}

#[derive(Debug, Default)]
pub struct D1 {
    pub migrations: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct Bucket {
    pub public: bool,
    /// A port of its own for the public URL, reached by address from the
    /// LAN (`http://<address>:<public_port>/<key>`, REQ-009).
    pub public_port: Option<u16>,
    pub cors: Vec<Cors>,
    pub lifecycle: Vec<Value>,
    pub notifications: Vec<Notification>,
    pub lock: Vec<Value>,
    pub storage_class: Option<String>,
    pub jurisdiction: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct Cors {
    pub origins: Vec<String>,
    pub methods: Vec<String>,
    pub allowed_headers: Vec<String>,
    pub expose_headers: Vec<String>,
    pub max_age: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Notification {
    /// Resource key of the queue.
    pub queue: String,
    pub events: Vec<String>,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
}

#[derive(Debug, Default)]
pub struct Worker {
    pub build: PathBuf,
    pub name: Option<String>,
    pub ports: Vec<Port>,
    pub depends_on: Vec<String>,
    pub bindings: Vec<(String, Target)>,
    /// Resource keys of the queues this Worker consumes.
    pub consumes: Vec<String>,
    pub durable_objects: Vec<String>,
    /// Cloudflare Containers on its Durable Object classes (REQ-020).
    pub containers: Vec<Container>,
    /// `secrets`: the name the Worker sees → its value (REQ-005).
    pub secrets: BTreeMap<String, String>,
    /// `environment`: values that are not secret (Wrangler's vars).
    pub vars: Map<String, Value>,
    pub routes: Vec<String>,
    pub crons: Vec<String>,
    pub compatibility_date: String,
    pub compatibility_flags: Vec<String>,
    pub assets: Option<PathBuf>,
    /// `placement`, `observability` and `cache`, in Wrangler's shape, only
    /// when written: unwritten ones are left to Cloudflare's defaults.
    pub placement: Option<Value>,
    pub observability: Option<Value>,
    pub cache: Option<Value>,
}

/// A container on one of a Worker's Durable Object classes (REQ-020), with
/// the meaning of Cloudflare's `containers` settings (the `default` policy).
#[derive(Debug, Clone)]
pub struct Container {
    pub class: String,
    pub image: Image,
    /// Where a Dockerfile is built: `image_build_context`, else the
    /// Dockerfile's folder. None for a registry image.
    pub build_context: Option<PathBuf>,
    pub image_vars: BTreeMap<String, String>,
    /// A named type (`lite`…) or `{ vcpu, memory_mib, disk_mb }`.
    pub instance_type: Option<Value>,
    pub max_instances: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Image {
    Dockerfile(PathBuf),
    Registry(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Port {
    pub ip: String,
    pub port: u16,
}

/// What a binding points at. Resources are named by their key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Worker(String),
    D1(String),
    R2(String),
    Kv(String),
    Queue(String),
    DurableObject { service: String, class: String },
}

impl Compose {
    pub fn service(&self, name: &str) -> Option<&Service> {
        self.services.iter().find(|s| s.name == name)
    }

    /// The services a service needs: `depends_on`, binding targets, queues
    /// it consumes, and queues its buckets notify.
    pub fn dependencies(&self, s: &Service) -> BTreeSet<String> {
        let owner = |key: &str| {
            self.services
                .iter()
                .find(|x| resource_keys(x).iter().any(|k| k == key))
                .map(|x| x.name.clone())
        };
        let mut out = BTreeSet::new();
        match &s.kind {
            Kind::Worker(w) => {
                out.extend(w.depends_on.iter().cloned());
                for (_, t) in &w.bindings {
                    match t {
                        Target::Worker(n) => {
                            out.insert(n.clone());
                        }
                        Target::DurableObject { service, .. } => {
                            out.insert(service.clone());
                        }
                        Target::D1(k) | Target::R2(k) | Target::Kv(k) | Target::Queue(k) => {
                            out.extend(owner(k));
                        }
                    }
                }
                for q in &w.consumes {
                    out.extend(owner(q));
                }
            }
            Kind::R2(buckets) => {
                for b in buckets {
                    for n in &b.spec.notifications {
                        out.extend(owner(&n.queue));
                    }
                }
            }
            _ => {}
        }
        out.remove(&s.name);
        out
    }

    /// `names` and everything they need, in dependency order (all when
    /// `names` is empty). Unknown names are an error (REQ-008).
    pub fn closure(&self, names: &[String]) -> Result<Vec<&Service>> {
        for n in names {
            if self.service(n).is_none() {
                bail!(crate::tr!(
                    "サービス「{n}」はありません",
                    "there is no service \"{n}\""
                ));
            }
        }
        if names.is_empty() {
            return Ok(self.services.iter().collect());
        }
        let mut want: BTreeSet<String> = names.iter().cloned().collect();
        let mut todo: Vec<String> = names.to_vec();
        while let Some(n) = todo.pop() {
            if let Some(s) = self.service(&n) {
                for d in self.dependencies(s) {
                    if want.insert(d.clone()) {
                        todo.push(d);
                    }
                }
            }
        }
        Ok(self
            .services
            .iter()
            .filter(|s| want.contains(&s.name))
            .collect())
    }
}

/// Values for `${VAR}`: the process environment, then `.env.<env>`.
struct Vars(BTreeMap<String, String>);

impl Lookup for Vars {
    fn get(&self, name: &str) -> Option<String> {
        // The tool's own variables come first (WC_ is the tool's prefix);
        // they are filled in once the services are read (r2_endpoints).
        if is_r2_var(name) || is_r2_key_var(name) {
            return Some(format!("{MARK}{name}{MARK}"));
        }
        std::env::var(name)
            .ok()
            .or_else(|| self.0.get(name).cloned())
    }
}

/// Stands for a tool variable until its value is known.
const MARK: char = '\u{1}';

/// `WC_R2_<SERVICE>[_<BUCKET>]_S3`: the S3-compatible endpoint of a bucket
/// (REQ-004).
fn is_r2_var(name: &str) -> bool {
    name.len() > "WC_R2__S3".len() && name.starts_with("WC_R2_") && name.ends_with("_S3")
}

/// `WC_R2_<SERVICE>_S3_KEY` / `_S3_SECRET`: the keys of an r2 service's
/// S3-compatible API (REQ-004). Secrets, so only in a Worker's `secrets`.
fn is_r2_key_var(name: &str) -> bool {
    name.starts_with("WC_R2_")
        && ((name.ends_with("_S3_KEY") && name.len() > "WC_R2__S3_KEY".len())
            || (name.ends_with("_S3_SECRET") && name.len() > "WC_R2__S3_SECRET".len()))
}

fn upper(x: &str) -> String {
    x.to_ascii_uppercase().replace('-', "_")
}

fn r2_var_name(service: &str, sub: Option<&str>) -> String {
    match sub {
        None => format!("WC_R2_{}_S3", upper(service)),
        Some(b) => format!("WC_R2_{}_{}_S3", upper(service), upper(b)),
    }
}

/// The keys of every r2 service: locally the project's own random keys;
/// remotely the tool cannot make R2 API tokens (Wrangler has no command
/// for them), so the user gives their own.
fn r2_keys(c: &Compose, env: &str) -> BTreeMap<String, Result<String, String>> {
    let mut out = BTreeMap::new();
    for svc in &c.services {
        let Kind::R2(_) = &svc.kind else { continue };
        let names = [
            format!("WC_R2_{}_S3_KEY", upper(&svc.name)),
            format!("WC_R2_{}_S3_SECRET", upper(&svc.name)),
        ];
        for (i, name) in names.into_iter().enumerate() {
            let v = match &c.local_s3 {
                Some(k) if env == "local" => Ok(if i == 0 {
                    k.key.clone()
                } else {
                    k.secret.clone()
                }),
                _ => Err(crate::tr!(
                    "{name} は手元(--env local)でだけ道具が用意します。{env} では R2 の API トークンの鍵を .env.{env} の変数に書き、${{変数:-${{{name}}}}} のように書いてください",
                    "{name} is provided by the tool only locally (--env local); for {env}, put your R2 API token's keys in variables in .env.{env} and write ${{VAR:-${{{name}}}}}"
                )),
            };
            out.insert(name.clone(), v);
        }
    }
    out
}

/// The S3-compatible endpoint (path style, the bucket in the path) of every
/// bucket: locally the r2 service's own port, remotely
/// `https://<account>[.<jurisdiction>].r2.cloudflarestorage.com/<actual name>`.
fn r2_endpoints(c: &Compose, env: &str) -> BTreeMap<String, Result<String, String>> {
    let mut out = BTreeMap::new();
    for svc in &c.services {
        let Kind::R2(list) = &svc.kind else { continue };
        for r in list {
            let v = if env == "local" {
                match svc.ports.first() {
                    Some(p) => Ok(format!(
                        "http://127.0.0.1:{}/cdn-cgi/local/r2/s3/{}",
                        p.port, r.key
                    )),
                    None => Err(crate::tr!(
                        "services.{} に ports を書いてください(手元の S3 互換の口の場所が決まりません)",
                        "add ports to services.{} (the local S3-compatible API has no fixed place without them)",
                        svc.name
                    )),
                }
            } else {
                match &c.account_id {
                    Some(acct) => {
                        // The name on Cloudflare (REQ-006).
                        let name =
                            format!("{}--{env}--{}", c.name, r.name.as_deref().unwrap_or(&r.key));
                        let host = match r.spec.jurisdiction.as_deref() {
                            Some(j) if j != "default" => format!("{acct}.{j}"),
                            _ => acct.clone(),
                        };
                        Ok(format!("https://{host}.r2.cloudflarestorage.com/{name}"))
                    }
                    None => Err(crate::tr!(
                        ".env.{env} に CLOUDFLARE_ACCOUNT_ID を書いてください(R2 の接続先にアカウントの ID が入ります)",
                        "write CLOUDFLARE_ACCOUNT_ID in .env.{env} (the R2 endpoint holds the account id)"
                    )),
                }
            };
            out.insert(r2_var_name(&svc.name, r.sub.as_deref()), v);
        }
    }
    out
}

/// The tool variables in a string (between MARKs).
fn tool_vars_in(s: &str) -> impl Iterator<Item = &str> {
    s.split(MARK)
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, p)| p)
}

/// The endpoint variables may stand only in a Worker's `environment`
/// values, the key variables only in its `secrets`; every other place is
/// checked here, before the services are read.
fn misplaced_tool_vars(v: &Value, at: &str, place: Option<&str>, out: &mut Vec<String>) {
    match v {
        Value::String(s) if s.contains(MARK) => {
            for name in tool_vars_in(s) {
                let ok = match place {
                    Some("environment") => !is_r2_key_var(name),
                    Some("secrets") => is_r2_key_var(name),
                    _ => false,
                };
                if !ok {
                    out.push(if is_r2_key_var(name) {
                        crate::tr!(
                            "  - {at}: {name} は鍵なので、Worker の secrets の値でだけ使えます",
                            "  - {at}: {name} is a key, so it can only be used in a Worker's secrets values"
                        )
                    } else {
                        crate::tr!(
                            "  - {at}: {name} は Worker の environment の値でだけ使えます",
                            "  - {at}: {name} can only be used in a Worker's environment values"
                        )
                    });
                }
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                misplaced_tool_vars(x, &format!("{at}[{i}]"), None, out);
            }
        }
        Value::Object(o) => {
            for (k, x) in o {
                let here = if at.is_empty() {
                    k.clone()
                } else {
                    format!("{at}.{k}")
                };
                let place = (at.starts_with("services.")
                    && at.matches('.').count() == 1
                    && (k == "environment" || k == "secrets"))
                    .then_some(k.as_str())
                    .or(place);
                misplaced_tool_vars(x, &here, place, out);
            }
        }
        _ => {}
    }
}

/// Puts the tool variables' values into the Workers' `environment` and
/// `secrets`.
fn fill_tool_vars(c: &mut Compose, env: &str) -> Vec<String> {
    let mut endpoints = r2_endpoints(c, env);
    endpoints.extend(r2_keys(c, env));
    let mut errors = Vec::new();
    for svc in &mut c.services {
        let Kind::Worker(w) = &mut svc.kind else {
            continue;
        };
        let vars = w.vars.iter_mut().filter_map(|(k, v)| match v {
            Value::String(t) => Some(("environment", k, t)),
            _ => None,
        });
        let secrets = w.secrets.iter_mut().map(|(k, t)| ("secrets", k, t));
        for (place, k, text) in vars.chain(secrets) {
            if !text.contains(MARK) {
                continue;
            }
            let mut filled = String::new();
            for (i, part) in text.split(MARK).enumerate() {
                if i % 2 == 0 {
                    filled.push_str(part);
                    continue;
                }
                match endpoints.get(part) {
                    Some(Ok(e)) => filled.push_str(e),
                    Some(Err(why)) => errors.push(format!("services.{}.{place}.{k}: {why}", svc.name)),
                    None => errors.push(crate::tr!(
                        "services.{}.{place}.{k}: {part} という R2 の道具の変数はありません(あるもの: {})",
                        "services.{}.{place}.{k}: no R2 tool variable {part} (there are: {})",
                        svc.name,
                        {
                            let names: Vec<String> = endpoints
                                .keys()
                                .filter(|n| is_r2_key_var(n) == is_r2_key_var(part))
                                .cloned()
                                .collect();
                            if names.is_empty() { "-".to_string() } else { names.join(", ") }
                        }
                    )),
                }
            }
            *text = filled;
        }
    }
    errors
}

/// The compose file from `start` upwards.
pub fn find(start: &Path) -> Result<PathBuf> {
    for dir in start.ancestors() {
        let found: Vec<PathBuf> = FILE_NAMES
            .iter()
            .map(|f| dir.join(f))
            .filter(|p| p.is_file())
            .collect();
        match found.len() {
            0 => continue,
            1 => return Ok(found.into_iter().next().unwrap()),
            _ => bail!(crate::tr!(
                "{} に wrangler-compose.yaml と wrangler-compose.yml の両方があります。どちらか 1 つにしてください",
                "{} has both wrangler-compose.yaml and wrangler-compose.yml; keep only one",
                dir.display()
            )),
        }
    }
    bail!(crate::tr!(
        "{} にも、その上のフォルダにも wrangler-compose.yaml がありません。書き始めるには、プロジェクトの根に次のようなファイルを置いてください:\n\n{}",
        "no wrangler-compose.yaml in {} or above. To start, put a file like this at the project root:\n\n{}",
        start.display(),
        STARTER
    ))
}

const STARTER: &str = "services:
  app:
    type: worker
    build: ./app
    compatibility_date: \"2026-09-01\"
    ports: [\"8787:8787\"]
";

/// Read and check the compose file for environment `env`.
pub fn load(file: &Path, env: &str) -> Result<Compose> {
    let root = file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let env_file = root.join(format!(".env.{env}"));
    let file_vars =
        if env_file.is_file() {
            crate::git::require_ignored(&root, &env_file, ".env")?;
            crate::dotenv::parse(&std::fs::read_to_string(&env_file).with_context(|| {
                crate::tr!("{} を読めません", "cannot read {}", env_file.display())
            })?)
        } else {
            BTreeMap::new()
        };
    let text = std::fs::read_to_string(file)
        .with_context(|| crate::tr!("{} を読めません", "cannot read {}", file.display()))?;
    let raw: Value = serde_saphyr::from_str(&text).map_err(|e| {
        anyhow::anyhow!(crate::tr!(
            "{} を YAML として読めません: {e}",
            "{} is not valid YAML: {e}",
            file.display()
        ))
    })?;
    let account_id = file_vars
        .get("CLOUDFLARE_ACCOUNT_ID")
        .filter(|v| !v.is_empty())
        .cloned();
    let vars = Vars(file_vars);
    let raw = docker_forms(strip_extensions(raw), file)?;
    let mut missing = BTreeSet::new();
    // Secrets read variables like any other value, so ones without a value
    // are in the same list (AC-004).
    let expanded = expand_tree(raw, &vars, &mut missing);
    if !missing.is_empty() {
        bail!(missing_message(&missing, env, &env_file));
    }
    let mut outside = Vec::new();
    misplaced_tool_vars(&expanded, "", None, &mut outside);
    if !outside.is_empty() {
        bail!(
            "{}\n{}",
            crate::tr!(
                "{} に誤りがあります(何もしていません):",
                "{} has errors (nothing was done):",
                file.display()
            ),
            outside.join("\n")
        );
    }
    let dir_name = root
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "project".to_string());
    let mut c = Checker::default();
    let compose = c.compose(expanded, &root, file, &dir_name);
    if !c.errors.is_empty() {
        bail!(
            "{}\n{}",
            crate::tr!(
                "{} に誤りがあります(何もしていません):",
                "{} has errors (nothing was done):",
                file.display()
            ),
            c.errors
                .iter()
                .map(|e| format!("  - {e}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    let mut compose = compose.expect("no errors means a compose file");
    compose.env_file = env_file;
    compose.account_id = account_id;
    if env == "local" {
        compose.local_s3 = Some(local_s3_keys(&root)?);
    }
    let errors = fill_tool_vars(&mut compose, env);
    if !errors.is_empty() {
        bail!(
            "{}\n{}",
            crate::tr!(
                "{} に誤りがあります(何もしていません):",
                "{} has errors (nothing was done):",
                file.display()
            ),
            errors
                .iter()
                .map(|e| format!("  - {e}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    Ok(compose)
}

fn missing_message(missing: &BTreeSet<Missing>, env: &str, env_file: &Path) -> String {
    let mut parts = Vec::new();
    let bad: Vec<String> = missing
        .iter()
        .filter(|m| m.invalid)
        .map(|m| format!("  - {}", m.name))
        .collect();
    if !bad.is_empty() {
        parts.push(crate::tr!(
            "${{…}} の書き方が違います(何もしていません。$ そのものは $$ と書きます):\n{}",
            "malformed ${{…}} (nothing was done; write $$ for a literal $):\n{}",
            bad.join("\n")
        ));
    }
    let lines: Vec<String> = missing
        .iter()
        .filter(|m| !m.invalid)
        .map(|m| match &m.message {
            Some(msg) => format!("  - {}: {msg}", m.name),
            None => format!("  - {}", m.name),
        })
        .collect();
    if !lines.is_empty() {
        parts.push(crate::tr!(
            "環境 {env} で、値の無い変数があります(何もしていません)。環境変数か {} に書いてください:\n{}",
            "environment {env} has variables without a value (nothing was done); set them in the environment or in {}:\n{}",
            env_file.display(),
            lines.join("\n")
        ));
    }
    parts.join("\n")
}

/// `x-` keys are skipped at every level (REQ-001), as docker compose does.
fn strip_extensions(v: Value) -> Value {
    match v {
        Value::Array(a) => Value::Array(a.into_iter().map(strip_extensions).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .filter(|(k, _)| !k.starts_with("x-"))
                .map(|(k, x)| (k, strip_extensions(x)))
                .collect(),
        ),
        other => other,
    }
}

/// A service's `environment` and `secrets` take docker compose's two forms
/// (REQ-005): a mapping `NAME: value`, or a list where `NAME=value` sets a
/// value and a bare `NAME` takes the variable of the same name. Both become
/// mappings here, before values are expanded. A secret's value must come
/// from a variable, so a secret never sits in the compose file; that is
/// checked on the text as written. The forms before 0.1.8 (`vars`, the
/// top-level `secrets`) stop with what to write instead.
fn docker_forms(mut v: Value, file: &Path) -> Result<Value> {
    let mut errors = Vec::new();
    if v.get("secrets").is_some() {
        errors.push(crate::tr!(
            "secrets: 一番上の secrets はなくなりました。サービスの secrets に「名前: ${{変数}}」か名前の並び([NAME, …]。同じ名前の変数から取る)で書いてください。Worker から見える名前は、これまでの environment: の変数名です",
            "secrets: the top-level secrets are gone; list them in the service's secrets as NAME: ${{VAR}} or as names ([NAME, …], read from the variable of the same name). The name the Worker sees is the old environment: variable"
        ));
    }
    if let Some(Value::Object(svcs)) = v.get_mut("services") {
        for (sname, sv) in svcs.iter_mut() {
            let Value::Object(o) = sv else { continue };
            if o.contains_key("vars") {
                errors.push(crate::tr!(
                    "services.{sname}.vars: vars は environment に改めました(docker compose と同じ)",
                    "services.{sname}.vars: vars is now environment (as in docker compose)"
                ));
            }
            for key in ["environment", "secrets"] {
                let at = format!("services.{sname}.{key}");
                match o.get_mut(key) {
                    Some(Value::Array(items)) => {
                        let mut m = Map::new();
                        for item in items.iter() {
                            let Value::String(item) = item else {
                                errors.push(crate::tr!(
                                    "{at}: 並びには NAME か NAME=値 を書いてください",
                                    "{at}: list items are NAME or NAME=value"
                                ));
                                continue;
                            };
                            match item.split_once('=') {
                                Some((k, val)) if key == "environment" => {
                                    m.insert(k.to_string(), Value::String(val.to_string()));
                                }
                                Some(_) => errors.push(crate::tr!(
                                    "{at}: シークレットの値は並びに書けません。名前だけを並べると同じ名前の変数から取ります",
                                    "{at}: a secret's value cannot be written in the list; a bare name reads the variable of the same name"
                                )),
                                None => {
                                    m.insert(item.clone(), Value::String(format!("${{{item}}}")));
                                }
                            }
                        }
                        o.insert(key.to_string(), Value::Object(m));
                    }
                    // `NAME:` with no value is an empty value (REQ-002).
                    Some(Value::Object(m)) if key == "environment" => {
                        for val in m.values_mut() {
                            if val.is_null() {
                                *val = Value::String(String::new());
                            }
                        }
                    }
                    Some(Value::Object(m)) if key == "secrets" => {
                        for (name, val) in m.iter() {
                            if !val.as_str().is_some_and(reads_variable) {
                                errors.push(crate::tr!(
                                    "{at}.{name}: シークレットの値は ${{変数}} で環境変数か .env.<環境> から取ってください(compose ファイルに値を書かない)",
                                    "{at}.{name}: take a secret's value from a variable with ${{VAR}} (the environment or .env.<env>), never the compose file"
                                ));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if !errors.is_empty() {
        bail!(
            "{}\n{}",
            crate::tr!(
                "{} に誤りがあります(何もしていません):",
                "{} has errors (nothing was done):",
                file.display()
            ),
            errors
                .iter()
                .map(|e| format!("  - {e}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    Ok(v)
}

/// Whether a value as written refers to a variable (`$VAR` or `${…}`; `$$`
/// is a literal `$`).
fn reads_variable(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'$' {
            match b[i + 1] {
                b'$' => i += 2,
                c if c == b'{' || c == b'_' || c.is_ascii_alphabetic() => return true,
                _ => i += 1,
            }
        } else {
            i += 1;
        }
    }
    false
}

fn expand_tree(v: Value, vars: &dyn Lookup, missing: &mut BTreeSet<Missing>) -> Value {
    match v {
        Value::String(s) => Value::String(interp::expand(&s, vars, missing)),
        Value::Array(a) => Value::Array(
            a.into_iter()
                .map(|x| expand_tree(x, vars, missing))
                .collect(),
        ),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, x)| (k, expand_tree(x, vars, missing)))
                .collect(),
        ),
        other => other,
    }
}

/// Collects every error with where it is, instead of stopping at the first.
#[derive(Default)]
struct Checker {
    errors: Vec<String>,
}

const WORKER_KEYS: &[&str] = &[
    "type",
    "build",
    "name",
    "ports",
    "depends_on",
    "bindings",
    "consumes",
    "durable_objects",
    "secrets",
    "environment",
    "routes",
    "crons",
    "compatibility_date",
    "compatibility_flags",
    "assets",
    "containers",
    "placement",
    "observability",
    "cache",
];
const CONTAINER_KEYS: &[&str] = &[
    "class",
    "image",
    "instance_type",
    "max_instances",
    "image_build_context",
    "image_vars",
];
const INSTANCE_TYPES: &[&str] = &[
    "lite",
    "basic",
    "standard-1",
    "standard-2",
    "standard-3",
    "standard-4",
];
const D1_KEYS: &[&str] = &["name", "migrations"];
const BUCKET_KEYS: &[&str] = &[
    "name",
    "public",
    "public_port",
    "cors",
    "lifecycle",
    "notifications",
    "lock",
    "storage_class",
    "jurisdiction",
];
const NAME_ONLY: &[&str] = &["name"];

impl Checker {
    fn err(&mut self, at: &str, msg: String) {
        self.errors.push(format!("{at}: {msg}"));
    }

    fn compose(&mut self, v: Value, root: &Path, file: &Path, dir_name: &str) -> Option<Compose> {
        let Value::Object(top) = v else {
            self.err(
                "(top)",
                crate::tr!("中身は対応表(キーと値)にしてください", "must be a mapping"),
            );
            return None;
        };
        self.unknown_keys("(top)", &top, &["name", "context", "services"]);
        let context = match top.get("context") {
            None => root.to_path_buf(),
            Some(Value::String(p)) => {
                let dir = root.join(p);
                if !dir.join("package.json").is_file() {
                    self.err(
                        "context",
                        crate::tr!(
                            "{} に package.json がありません(Wrangler を入れた package.json のあるフォルダを書いてください)",
                            "no package.json in {} (name the folder whose package.json has Wrangler)",
                            dir.display()
                        ),
                    );
                }
                std::fs::canonicalize(&dir).unwrap_or(dir)
            }
            Some(_) => {
                self.err(
                    "context",
                    crate::tr!("文字列にしてください", "must be a string"),
                );
                root.to_path_buf()
            }
        };
        let name = match top.get("name") {
            Some(Value::String(s)) => s.clone(),
            Some(_) => {
                self.err(
                    "name",
                    crate::tr!("文字列にしてください", "must be a string"),
                );
                String::new()
            }
            // Required: a folder name differs from PC to PC, and the
            // project is part of every name on Cloudflare (REQ-001).
            None => {
                self.err(
                    "name",
                    crate::tr!(
                        "プロジェクトの名前(一番上の name:)を書いてください",
                        "write the project name (name: at the top)"
                    ),
                );
                String::new()
            }
        };
        let _ = dir_name;
        if !name.is_empty() && (!valid_name(&name) || name.len() < 3) {
            self.err(
                "name",
                crate::tr!(
                    "プロジェクトの名前「{name}」は、英小文字・数字・- の 3 文字以上にしてください(-- と、先頭・末尾の - は使えません)",
                    "project name \"{name}\" must be 3 or more lowercase letters, digits and - (no --, and no - at either end)"
                ),
            );
        }
        let Some(Value::Object(svcs)) = top.get("services") else {
            self.err(
                "services",
                crate::tr!(
                    "サービスを 1 つ以上書いてください",
                    "define at least one service"
                ),
            );
            return None;
        };
        // First pass: names and types, so references can be resolved.
        let mut types: BTreeMap<String, (String, bool, Vec<String>)> = BTreeMap::new();
        for (sname, sv) in svcs {
            let at = format!("services.{sname}");
            if !valid_name(sname) {
                self.err(
                    &at,
                    crate::tr!(
                        "サービスの名前には英小文字・数字・- だけを使ってください(-- と、先頭・末尾の - は使えません)",
                        "service names may only use lowercase letters, digits and - (no --, and no - at either end)"
                    ),
                );
            }
            if sname.starts_with("wc-") {
                self.err(
                    &at,
                    crate::tr!(
                        "wc- で始まる名前は道具のサービスが使うので、ほかの名前にしてください",
                        "names starting with wc- belong to the tool's own services; pick another name"
                    ),
                );
            }
            let Value::Object(o) = sv else {
                self.err(
                    &at,
                    crate::tr!("対応表(キーと値)にしてください", "must be a mapping"),
                );
                continue;
            };
            let Some(Value::String(t)) = o.get("type") else {
                self.err(
                    &format!("{at}.type"),
                    crate::tr!(
                        "型(worker・d1・r2・kv・queue)を書いてください",
                        "set a type (worker, d1, r2, kv or queue)"
                    ),
                );
                continue;
            };
            let plural_key = plural_key(t);
            let (plural, subs) = match plural_key.and_then(|k| o.get(k)) {
                Some(Value::Object(m)) => (true, m.keys().cloned().collect()),
                _ => (false, Vec::new()),
            };
            types.insert(sname.clone(), (t.clone(), plural, subs));
        }
        let mut services = Vec::new();
        for (sname, sv) in svcs {
            let at = format!("services.{sname}");
            let Value::Object(o) = sv else { continue };
            let Some((t, _, _)) = types.get(sname).cloned() else {
                continue;
            };
            let kind = match t.as_str() {
                "worker" => self
                    .worker(&at, o, root, &types)
                    .map(|w| Kind::Worker(Box::new(w))),
                "d1" => Some(Kind::D1(self.resources(
                    &at,
                    sname,
                    o,
                    "databases",
                    D1_KEYS,
                    &[],
                    |c, at, m| c.d1(at, m, root),
                ))),
                "r2" => Some(Kind::R2(self.resources(
                    &at,
                    sname,
                    o,
                    "buckets",
                    BUCKET_KEYS,
                    &["ports"],
                    |c, at, m| c.bucket(at, m, &types),
                ))),
                "kv" => Some(Kind::Kv(self.resources(
                    &at,
                    sname,
                    o,
                    "namespaces",
                    NAME_ONLY,
                    &[],
                    |_, _, _| (),
                ))),
                "queue" => Some(Kind::Queue(self.resources(
                    &at,
                    sname,
                    o,
                    "queues",
                    NAME_ONLY,
                    &[],
                    |_, _, _| (),
                ))),
                other => {
                    self.err(
                        &format!("{at}.type"),
                        crate::tr!(
                            "知らない型「{other}」です(worker・d1・r2・kv・queue のどれか)",
                            "unknown type \"{other}\" (use worker, d1, r2, kv or queue)"
                        ),
                    );
                    None
                }
            };
            if let Some(kind) = kind {
                let ports = if t == "r2" {
                    self.ports(&format!("{at}.ports"), o.get("ports"))
                } else {
                    Vec::new()
                };
                services.push(Service {
                    name: sname.clone(),
                    kind,
                    ports,
                });
            }
        }
        let mut used: BTreeMap<u16, String> = BTreeMap::new();
        for s in &services {
            let mut ports: Vec<Port> = match &s.kind {
                Kind::Worker(w) => w.ports.clone(),
                _ => s.ports.clone(),
            };
            // The public port listens on the r2 service's address.
            if let Kind::R2(list) = &s.kind {
                for b in list {
                    let Some(pp) = b.spec.public_port else { continue };
                    match s.ports.first() {
                        Some(base) => ports.push(Port { ip: base.ip.clone(), port: pp }),
                        None => self.err(
                            &format!("services.{}.public_port", s.name),
                            crate::tr!(
                                "public_port には、この r2 のサービスの ports も要ります(待ち受けるアドレスを ports から取ります)",
                                "public_port needs this r2 service's ports too (the address to listen on comes from ports)"
                            ),
                        ),
                    }
                }
            }
            for p in &ports {
                if let Some(other) = used.insert(p.port, s.name.clone()) {
                    self.err(
                        &format!("services.{}.ports", s.name),
                        crate::tr!(
                            "ポート {} は {other} も使っています",
                            "port {} is also used by {other}",
                            p.port
                        ),
                    );
                }
            }
        }
        self.across(&services);
        let services = self.order(services);
        Some(Compose {
            name,
            root: root.to_path_buf(),
            file: file.to_path_buf(),
            context,
            env_file: PathBuf::new(),
            account_id: None,
            local_s3: None,
            services,
        })
    }

    /// Checks that need every service: names of resources, Durable Object
    /// classes, queue consumers.
    fn across(&mut self, services: &[Service]) {
        // Names on Cloudflare: `name:`, else the service name (REQ-006).
        let mut names: BTreeMap<String, String> = BTreeMap::new();
        for s in services {
            let n = s.cloud_name().to_string();
            if let Some(other) = names.insert(n.clone(), s.name.clone()) {
                self.err(
                    &format!("services.{}", s.name),
                    crate::tr!(
                        "名前「{n}」が {other} と重なります。name: を変えてください",
                        "name \"{n}\" is also {other}'s; change name:"
                    ),
                );
            }
        }
        let mut keys: BTreeMap<String, String> = BTreeMap::new();
        for s in services {
            for k in resource_keys(s) {
                if let Some(other) = keys.insert(k.clone(), s.name.clone()) {
                    self.err(
                        &format!("services.{}", s.name),
                        crate::tr!(
                            "資源の名前「{k}」が {other} の資源と重なります。サービスかバケットなどの名前を変えてください",
                            "resource name \"{k}\" is also a resource of {other}; rename the service or the bucket (etc.)"
                        ),
                    );
                }
            }
        }
        let mut consumers: BTreeMap<String, String> = BTreeMap::new();
        for s in services {
            let Kind::Worker(w) = &s.kind else { continue };
            for (b, t) in &w.bindings {
                if let Target::DurableObject { service, class } = t {
                    let declared = services.iter().any(|x| {
                        x.name == *service
                            && matches!(&x.kind, Kind::Worker(o) if o.durable_objects.contains(class))
                    });
                    if !declared {
                        self.err(
                            &format!("services.{}.bindings.{b}", s.name),
                            crate::tr!(
                                "{service} の durable_objects に {class} がありません",
                                "{service} has no {class} in its durable_objects"
                            ),
                        );
                    }
                }
            }
            for q in &w.consumes {
                if let Some(other) = consumers.insert(q.clone(), s.name.clone()) {
                    self.err(
                        &format!("services.{}.consumes", s.name),
                        crate::tr!(
                            "queue {q} は {other} も受け取ります。1 つの queue を受け取れる Worker は 1 つです",
                            "queue {q} is also consumed by {other}; a queue has one consuming Worker"
                        ),
                    );
                }
            }
        }
    }

    fn unknown_keys(&mut self, at: &str, o: &Map<String, Value>, allowed: &[&str]) {
        for k in o.keys() {
            if k.starts_with("x-") || allowed.contains(&k.as_str()) {
                continue;
            }
            let hint = hint_for(k);
            self.err(
                &format!("{at}.{k}").replace("(top).", ""),
                match hint {
                    Some(h) => crate::tr!(
                        "このキーは使えません。{h}",
                        "this key is not supported; {h}"
                    ),
                    None => crate::tr!("このキーは使えません", "this key is not supported"),
                },
            );
        }
    }

    /// `name:` is the last part of the name on Cloudflare (REQ-006).
    fn check_name_part(&mut self, at: &str, name: Option<&str>) {
        if let Some(n) = name
            && !valid_name(n)
        {
            self.err(
                at,
                crate::tr!(
                    "名前「{n}」には英小文字・数字・- だけを使ってください(-- と、先頭・末尾の - は使えません。Cloudflare では <プロジェクト>--<環境>--{n} になります)",
                    "name \"{n}\" may only use lowercase letters, digits and - (no --, and no - at either end; on Cloudflare it becomes <project>--<env>--{n})"
                ),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn resources<T>(
        &mut self,
        at: &str,
        service: &str,
        o: &Map<String, Value>,
        plural: &str,
        keys: &[&str],
        service_keys: &[&str],
        mut spec: impl FnMut(&mut Self, &str, &Map<String, Value>) -> T,
    ) -> Vec<Resource<T>> {
        let mut out = Vec::new();
        match o.get(plural) {
            // One service is one resource (REQ-002): more resources are more
            // services, each named.
            Some(_) => self.err(
                &format!("{at}.{plural}"),
                crate::tr!(
                    "{plural} はなくなりました。1 つのサービスが資源 1 つです。資源ごとにサービスを分けて書き、名前は name: で付けてください",
                    "{plural} is gone: one service is one resource. Write a service per resource and name it with name:"
                ),
            ),
            None => {
                let mut allowed = vec!["type"];
                allowed.extend_from_slice(keys);
                allowed.extend_from_slice(service_keys);
                self.unknown_keys(at, o, &allowed);
                let name = self.opt_string(&format!("{at}.name"), o.get("name"));
                self.check_name_part(&format!("{at}.name"), name.as_deref());
                let spec = spec(self, at, o);
                out.push(Resource {
                    sub: None,
                    key: service.to_string(),
                    name,
                    spec,
                });
            }
        }
        out
    }

    fn d1(&mut self, at: &str, m: &Map<String, Value>, root: &Path) -> D1 {
        let migrations = self
            .opt_string(&format!("{at}.migrations"), m.get("migrations"))
            .map(|p| root.join(p));
        if let Some(dir) = &migrations
            && !dir.is_dir()
        {
            self.err(
                &format!("{at}.migrations"),
                crate::tr!(
                    "フォルダ {} がありません",
                    "folder {} does not exist",
                    dir.display()
                ),
            );
        }
        D1 { migrations }
    }

    fn bucket(
        &mut self,
        at: &str,
        m: &Map<String, Value>,
        types: &BTreeMap<String, (String, bool, Vec<String>)>,
    ) -> Bucket {
        let mut b = Bucket {
            public: self.boolean(&format!("{at}.public"), m.get("public")),
            ..Bucket::default()
        };
        if let Some(p) = self.number(&format!("{at}.public_port"), m.get("public_port")) {
            match u16::try_from(p) {
                Ok(p) if p > 0 && b.public => b.public_port = Some(p),
                Ok(p) if p > 0 => self.err(
                    &format!("{at}.public_port"),
                    crate::tr!(
                        "public_port は public: true のときだけ書けます",
                        "public_port needs public: true"
                    ),
                ),
                _ => self.err(
                    &format!("{at}.public_port"),
                    crate::tr!("1〜65535 のポート番号にしてください", "must be a port number (1-65535)"),
                ),
            }
        }
        for (i, rule) in self
            .list_of_maps(&format!("{at}.cors"), m.get("cors"))
            .into_iter()
            .enumerate()
        {
            let rat = format!("{at}.cors[{i}]");
            self.unknown_keys(
                &rat,
                &rule,
                &[
                    "origins",
                    "methods",
                    "allowed_headers",
                    "expose_headers",
                    "max_age",
                ],
            );
            let cors = Cors {
                origins: self.strings(&format!("{rat}.origins"), rule.get("origins")),
                methods: self.strings(&format!("{rat}.methods"), rule.get("methods")),
                allowed_headers: self.strings(
                    &format!("{rat}.allowed_headers"),
                    rule.get("allowed_headers"),
                ),
                expose_headers: self
                    .strings(&format!("{rat}.expose_headers"), rule.get("expose_headers")),
                max_age: self.number(&format!("{rat}.max_age"), rule.get("max_age")),
            };
            if cors.origins.is_empty() || cors.methods.is_empty() {
                self.err(
                    &rat,
                    crate::tr!(
                        "origins と methods を書いてください",
                        "set origins and methods"
                    ),
                );
            }
            for meth in &cors.methods {
                if !["GET", "PUT", "POST", "DELETE", "HEAD"].contains(&meth.as_str()) {
                    self.err(
                        &format!("{rat}.methods"),
                        crate::tr!(
                            "「{meth}」は使えません(GET・PUT・POST・DELETE・HEAD)",
                            "\"{meth}\" is not allowed (GET, PUT, POST, DELETE, HEAD)"
                        ),
                    );
                }
            }
            b.cors.push(cors);
        }
        for (i, rule) in self
            .list_of_maps(&format!("{at}.lifecycle"), m.get("lifecycle"))
            .into_iter()
            .enumerate()
        {
            let rat = format!("{at}.lifecycle[{i}]");
            self.unknown_keys(
                &rat,
                &rule,
                &[
                    "prefix",
                    "expire_after_days",
                    "transition_to_ia_after_days",
                    "abort_multipart_after_days",
                ],
            );
            b.lifecycle.push(Value::Object(rule));
        }
        for (i, rule) in self
            .list_of_maps(&format!("{at}.notifications"), m.get("notifications"))
            .into_iter()
            .enumerate()
        {
            let rat = format!("{at}.notifications[{i}]");
            self.unknown_keys(&rat, &rule, &["queue", "events", "prefix", "suffix"]);
            let queue = match rule.get("queue") {
                Some(Value::String(q)) => match resolve_resource(q, "queue", types) {
                    Ok(key) => key,
                    Err(e) => {
                        self.err(&format!("{rat}.queue"), e);
                        String::new()
                    }
                },
                _ => {
                    self.err(
                        &format!("{rat}.queue"),
                        crate::tr!(
                            "送り先の queue のサービスを書いてください",
                            "set the queue service to send to"
                        ),
                    );
                    String::new()
                }
            };
            let events = self.strings(&format!("{rat}.events"), rule.get("events"));
            for e in &events {
                if !["object-create", "object-delete"].contains(&e.as_str()) {
                    self.err(
                        &format!("{rat}.events"),
                        crate::tr!(
                            "「{e}」は使えません(object-create・object-delete)",
                            "\"{e}\" is not allowed (object-create, object-delete)"
                        ),
                    );
                }
            }
            if events.is_empty() {
                self.err(
                    &format!("{rat}.events"),
                    crate::tr!(
                        "object-create か object-delete を書いてください",
                        "set object-create and/or object-delete"
                    ),
                );
            }
            b.notifications.push(Notification {
                queue,
                events,
                prefix: self.opt_string(&format!("{rat}.prefix"), rule.get("prefix")),
                suffix: self.opt_string(&format!("{rat}.suffix"), rule.get("suffix")),
            });
        }
        for (i, rule) in self
            .list_of_maps(&format!("{at}.lock"), m.get("lock"))
            .into_iter()
            .enumerate()
        {
            let rat = format!("{at}.lock[{i}]");
            self.unknown_keys(
                &rat,
                &rule,
                &["prefix", "retain_days", "retain_until", "indefinite"],
            );
            b.lock.push(Value::Object(rule));
        }
        b.storage_class = self.opt_string(&format!("{at}.storage_class"), m.get("storage_class"));
        if let Some(sc) = &b.storage_class
            && !["Standard", "InfrequentAccess"].contains(&sc.as_str())
        {
            self.err(
                &format!("{at}.storage_class"),
                crate::tr!(
                    "Standard か InfrequentAccess にしてください",
                    "must be Standard or InfrequentAccess"
                ),
            );
        }
        b.jurisdiction = self.opt_string(&format!("{at}.jurisdiction"), m.get("jurisdiction"));
        if let Some(j) = &b.jurisdiction
            && !["eu", "fedramp"].contains(&j.as_str())
        {
            self.err(
                &format!("{at}.jurisdiction"),
                crate::tr!("eu か fedramp にしてください", "must be eu or fedramp"),
            );
        }
        b
    }

    fn worker(
        &mut self,
        at: &str,
        o: &Map<String, Value>,
        root: &Path,
        types: &BTreeMap<String, (String, bool, Vec<String>)>,
    ) -> Option<Worker> {
        self.unknown_keys(at, o, WORKER_KEYS);
        let mut w = Worker::default();
        match o.get("build") {
            Some(Value::String(p)) => {
                w.build = root.join(p);
                if !w.build.is_dir() {
                    self.err(
                        &format!("{at}.build"),
                        crate::tr!(
                            "フォルダ {} がありません",
                            "folder {} does not exist",
                            w.build.display()
                        ),
                    );
                }
            }
            _ => self.err(
                &format!("{at}.build"),
                crate::tr!(
                    "Worker のコードのフォルダを書いてください",
                    "set the folder with the Worker's code"
                ),
            ),
        }
        w.name = self.opt_string(&format!("{at}.name"), o.get("name"));
        self.check_name_part(&format!("{at}.name"), w.name.as_deref());
        w.ports = self.ports(&format!("{at}.ports"), o.get("ports"));
        w.depends_on = match o.get("depends_on") {
            Some(Value::Object(m)) => {
                for (dep, cond) in m {
                    let cond_ok = match cond {
                        Value::Object(c) => {
                            matches!(c.get("condition"), None | Some(Value::String(_)))
                                && c.get("condition")
                                    .and_then(Value::as_str)
                                    .is_none_or(|s| s == "service_started")
                        }
                        Value::Null => true,
                        _ => false,
                    };
                    if !cond_ok {
                        self.err(
                            &format!("{at}.depends_on.{dep}"),
                            crate::tr!(
                                "condition は service_started だけが使えます",
                                "only condition: service_started is supported"
                            ),
                        );
                    }
                }
                m.keys().cloned().collect()
            }
            other => self.strings(&format!("{at}.depends_on"), other),
        };
        for dep in &w.depends_on {
            if !types.contains_key(dep) {
                self.err(
                    &format!("{at}.depends_on"),
                    crate::tr!(
                        "サービス「{dep}」はありません",
                        "there is no service \"{dep}\""
                    ),
                );
            }
        }
        match o.get("bindings") {
            None => {}
            Some(Value::Object(m)) => {
                for (bname, target) in m {
                    let bat = format!("{at}.bindings.{bname}");
                    if !valid_binding(bname) {
                        self.err(&bat, crate::tr!("名前は英字・数字・_ だけにしてください(コードから env.<名前> で見えます)", "names may only use letters, digits and _ (seen as env.<name>)"));
                    }
                    let Value::String(t) = target else {
                        self.err(
                            &bat,
                            crate::tr!(
                                "相手のサービスを書いてください",
                                "set the service it points at"
                            ),
                        );
                        continue;
                    };
                    match resolve_target(t, types) {
                        Ok(target) => w.bindings.push((bname.clone(), target)),
                        Err(e) => self.err(&bat, e),
                    }
                }
            }
            Some(_) => self.err(
                &format!("{at}.bindings"),
                crate::tr!(
                    "「名前: 相手」の対応表にしてください",
                    "must be a mapping of name: target"
                ),
            ),
        }
        for q in self.strings(&format!("{at}.consumes"), o.get("consumes")) {
            match resolve_resource(&q, "queue", types) {
                Ok(key) => w.consumes.push(key),
                Err(e) => self.err(&format!("{at}.consumes"), e),
            }
        }
        w.durable_objects =
            self.strings(&format!("{at}.durable_objects"), o.get("durable_objects"));
        w.containers = self.containers(
            &format!("{at}.containers"),
            o.get("containers"),
            root,
            &w.durable_objects,
        );
        match o.get("secrets") {
            None => {}
            Some(Value::Object(m)) => {
                for (name, v) in m {
                    match v {
                        Value::String(v) => {
                            w.secrets.insert(name.clone(), v.clone());
                        }
                        _ => self.err(
                            &format!("{at}.secrets.{name}"),
                            crate::tr!("文字列にしてください", "must be a string"),
                        ),
                    }
                }
            }
            Some(_) => self.err(
                &format!("{at}.secrets"),
                crate::tr!(
                    "「名前: ${{変数}}」の対応表か、名前の並びにしてください",
                    "must be a mapping of NAME: ${{VAR}} or a list of names"
                ),
            ),
        }
        match o.get("environment") {
            None => {}
            Some(Value::Object(m)) => w.vars = m.clone(),
            Some(_) => self.err(
                &format!("{at}.environment"),
                crate::tr!(
                    "「名前: 値」の対応表か、NAME=値 の並びにしてください",
                    "must be a mapping of NAME: value or a list of NAME=value"
                ),
            ),
        }
        // One name space in the Worker's env: bindings, environment and
        // secrets.
        let mut names: BTreeMap<String, &str> = BTreeMap::new();
        for (name, what) in w
            .bindings
            .iter()
            .map(|(b, _)| (b.clone(), "bindings"))
            .chain(w.vars.keys().map(|k| (k.clone(), "environment")))
            .chain(w.secrets.keys().map(|k| (k.clone(), "secrets")))
        {
            if what != "bindings" && !valid_binding(&name) {
                self.err(
                    &format!("{at}.{what}.{name}"),
                    crate::tr!(
                        "名前「{name}」は英字・数字・_ だけにしてください(英字か _ で始める)",
                        "name \"{name}\" may only use letters, digits and _ (starting with a letter or _)"
                    ),
                );
            }
            if name.starts_with("__WC_") {
                self.err(
                    &format!("{at}.{what}.{name}"),
                    crate::tr!(
                        "__WC_ で始まる名前は道具が使います",
                        "names starting with __WC_ belong to the tool"
                    ),
                );
            }
            if let Some(first) = names.insert(name.clone(), what) {
                self.err(
                    &format!("{at}.{what}.{name}"),
                    crate::tr!(
                        "「{name}」は {first} にもあります。コードからは env.{name} 1 つしか見えないので、名前を分けてください",
                        "\"{name}\" is also in {first}; the code sees only one env.{name}, so use different names"
                    ),
                );
            }
        }
        // An entry left empty by its variable (`${DOMAIN:-}`) is as if not
        // written, so one compose file serves an environment on its own
        // domain and one on workers.dev (REQ-002).
        w.routes = self.strings(&format!("{at}.routes"), o.get("routes"));
        w.routes.retain(|r| !r.trim().is_empty());
        w.crons = self.strings(&format!("{at}.crons"), o.get("crons"));
        match o.get("compatibility_date") {
            Some(Value::String(d)) if is_date(d) => w.compatibility_date = d.clone(),
            _ => self.err(&format!("{at}.compatibility_date"), crate::tr!("互換の日付を \"2026-09-01\" の形で書いてください(Worker の振る舞いを決めるので省けません)", "set the compatibility date like \"2026-09-01\" (it decides how the Worker behaves, so it is required)")),
        }
        w.compatibility_flags = self.strings(
            &format!("{at}.compatibility_flags"),
            o.get("compatibility_flags"),
        );
        w.assets = self
            .opt_string(&format!("{at}.assets"), o.get("assets"))
            .map(|p| root.join(p));
        if let Some(dir) = &w.assets
            && !dir.is_dir()
        {
            self.err(
                &format!("{at}.assets"),
                crate::tr!(
                    "フォルダ {} がありません",
                    "folder {} does not exist",
                    dir.display()
                ),
            );
        }
        w.placement = self.settings(&format!("{at}.placement"), o.get("placement"), &["mode"]);
        if let Some(Value::Object(p)) = o.get("placement")
            && !p.contains_key("mode")
        {
            self.err(
                &format!("{at}.placement.mode"),
                crate::tr!("\"smart\" にしてください", "must be \"smart\""),
            );
        }
        w.observability = self.settings(
            &format!("{at}.observability"),
            o.get("observability"),
            &["enabled", "head_sampling_rate"],
        );
        w.cache = self.settings(&format!("{at}.cache"), o.get("cache"), &["enabled"]);
        Some(w)
    }

    /// A mapping of Wrangler settings (`placement`, `observability`,
    /// `cache`) with only the given keys, each checked and kept as written
    /// (`${VAR}` text turned into its true/false or number).
    fn settings(&mut self, at: &str, v: Option<&Value>, keys: &[&str]) -> Option<Value> {
        let o = match v? {
            Value::Object(o) => o,
            _ => {
                self.err(at, crate::tr!("対応表にしてください", "must be a mapping"));
                return None;
            }
        };
        self.unknown_keys(at, o, keys);
        let mut out = Map::new();
        for (k, x) in o {
            let here = format!("{at}.{k}");
            match k.as_str() {
                // Smart Placement is the only mode.
                "mode" => {
                    if x == "smart" {
                        out.insert(k.clone(), x.clone());
                    } else {
                        self.err(
                            &here,
                            crate::tr!("\"smart\" にしてください", "must be \"smart\""),
                        );
                    }
                }
                "enabled" => {
                    let before = self.errors.len();
                    let b = self.boolean(&here, Some(x));
                    if self.errors.len() == before {
                        out.insert(k.clone(), Value::Bool(b));
                    }
                }
                "head_sampling_rate" => {
                    let n = match x {
                        Value::Number(n) => n.as_f64(),
                        Value::String(s) => s.parse::<f64>().ok(),
                        _ => None,
                    };
                    match n {
                        Some(n) if (0.0..=1.0).contains(&n) => {
                            out.insert(k.clone(), serde_json::json!(n));
                        }
                        _ => self.err(
                            &here,
                            crate::tr!(
                                "0 から 1 までの数にしてください",
                                "must be a number from 0 to 1"
                            ),
                        ),
                    }
                }
                _ => {}
            }
        }
        Some(Value::Object(out))
    }

    /// `containers` of a Worker (REQ-020): each on a Durable Object class the
    /// Worker owns, at most one per class.
    fn containers(
        &mut self,
        at: &str,
        v: Option<&Value>,
        root: &Path,
        classes: &[String],
    ) -> Vec<Container> {
        let list = match v {
            None => return Vec::new(),
            Some(Value::Array(a)) => a,
            Some(_) => {
                self.err(
                    at,
                    crate::tr!("一覧にしてください([...])", "must be a list ([...])"),
                );
                return Vec::new();
            }
        };
        let mut out: Vec<Container> = Vec::new();
        for (i, item) in list.iter().enumerate() {
            let at = format!("{at}[{i}]");
            let Value::Object(o) = item else {
                self.err(&at, crate::tr!("対応表にしてください", "must be a mapping"));
                continue;
            };
            self.unknown_keys(&at, o, CONTAINER_KEYS);
            let class = self
                .opt_string(&format!("{at}.class"), o.get("class"))
                .unwrap_or_default();
            if class.is_empty() {
                self.err(
                    &format!("{at}.class"),
                    crate::tr!(
                        "付ける Durable Object のクラスを書いてください",
                        "name the Durable Object class it belongs to"
                    ),
                );
            } else if !classes.contains(&class) {
                self.err(
                    &format!("{at}.class"),
                    crate::tr!(
                        "{class} はこの Worker の durable_objects にありません",
                        "{class} is not in this Worker's durable_objects"
                    ),
                );
            } else if out.iter().any(|c| c.class == class) {
                self.err(
                    &format!("{at}.class"),
                    crate::tr!(
                        "{class} にはもうコンテナがあります(1 つのクラスに 1 つ)",
                        "{class} already has a container (one per class)"
                    ),
                );
            }
            let image = match self.opt_string(&format!("{at}.image"), o.get("image")) {
                None => {
                    self.err(
                        &format!("{at}.image"),
                        crate::tr!(
                            "Dockerfile の場所か、レジストリのイメージ(<名前>:<タグ>)を書いてください",
                            "give a Dockerfile path or a registry image (<name>:<tag>)"
                        ),
                    );
                    None
                }
                Some(s) if root.join(&s).is_file() => Some(Image::Dockerfile(root.join(&s))),
                Some(s) if is_image_ref(&s) => Some(Image::Registry(s)),
                Some(s) => {
                    self.err(
                        &format!("{at}.image"),
                        crate::tr!(
                            "Dockerfile {} がありません(レジストリのイメージなら <名前>:<タグ> の形で書きます)",
                            "Dockerfile {} does not exist (a registry image is written as <name>:<tag>)",
                            root.join(&s).display()
                        ),
                    );
                    None
                }
            };
            let dockerfile = matches!(image, Some(Image::Dockerfile(_)));
            for key in ["image_build_context", "image_vars"] {
                if o.contains_key(key) && matches!(image, Some(Image::Registry(_))) {
                    self.err(
                        &format!("{at}.{key}"),
                        crate::tr!(
                            "Dockerfile から作るときだけ書けます",
                            "only for an image built from a Dockerfile"
                        ),
                    );
                }
            }
            let mut build_context = match (
                &image,
                self.opt_string(
                    &format!("{at}.image_build_context"),
                    o.get("image_build_context"),
                ),
            ) {
                (Some(Image::Dockerfile(_)), Some(dir)) => Some(root.join(dir)),
                (Some(Image::Dockerfile(f)), None) => f.parent().map(Path::to_path_buf),
                _ => None,
            };
            if let Some(dir) = &build_context
                && dockerfile
                && !dir.is_dir()
            {
                self.err(
                    &format!("{at}.image_build_context"),
                    crate::tr!(
                        "フォルダ {} がありません",
                        "folder {} does not exist",
                        dir.display()
                    ),
                );
                build_context = None;
            }
            let mut image_vars = BTreeMap::new();
            match o.get("image_vars") {
                None => {}
                Some(Value::Object(m)) => {
                    for (k, v) in m {
                        match v {
                            Value::String(s) => {
                                image_vars.insert(k.clone(), s.clone());
                            }
                            Value::Number(n) => {
                                image_vars.insert(k.clone(), n.to_string());
                            }
                            _ => self.err(
                                &format!("{at}.image_vars.{k}"),
                                crate::tr!("文字列にしてください", "must be a string"),
                            ),
                        }
                    }
                }
                Some(_) => self.err(
                    &format!("{at}.image_vars"),
                    crate::tr!(
                        "「名前: 値」の対応表にしてください",
                        "must be a mapping of name: value"
                    ),
                ),
            }
            let instance_type =
                self.instance_type(&format!("{at}.instance_type"), o.get("instance_type"));
            let max_instances = self.number(&format!("{at}.max_instances"), o.get("max_instances"));
            if max_instances == Some(0) {
                self.err(
                    &format!("{at}.max_instances"),
                    crate::tr!("1 以上にしてください", "must be 1 or more"),
                );
            }
            if let Some(image) = image {
                out.push(Container {
                    class,
                    image,
                    build_context,
                    image_vars,
                    instance_type,
                    max_instances,
                });
            }
        }
        out
    }

    /// A named instance type, or `{ vcpu, memory_mib, disk_mb }` (REQ-020).
    fn instance_type(&mut self, at: &str, v: Option<&Value>) -> Option<Value> {
        match v {
            None => None,
            Some(Value::String(s)) if INSTANCE_TYPES.contains(&s.as_str()) => Some(v?.clone()),
            Some(Value::Object(o)) => {
                self.unknown_keys(at, o, &["vcpu", "memory_mib", "disk_mb"]);
                let mut ok = true;
                for (k, x) in o {
                    let fine = match k.as_str() {
                        "vcpu" => x.as_f64().is_some_and(|n| n > 0.0),
                        _ => x.as_u64().is_some_and(|n| n > 0),
                    };
                    if !fine {
                        ok = false;
                        self.err(
                            &format!("{at}.{k}"),
                            crate::tr!("0 より大きい数にしてください", "must be a number above 0"),
                        );
                    }
                }
                ok.then(|| v.cloned()).flatten()
            }
            Some(_) => {
                self.err(
                    at,
                    crate::tr!(
                        "{} のどれか、または vcpu・memory_mib・disk_mb の対応表にしてください",
                        "must be one of {} or a mapping of vcpu, memory_mib and disk_mb",
                        INSTANCE_TYPES.join("・")
                    ),
                );
                None
            }
        }
    }

    fn ports(&mut self, at: &str, v: Option<&Value>) -> Vec<Port> {
        let mut out = Vec::new();
        for p in self.strings(at, v) {
            match parse_port(&p) {
                Ok(port) => out.push(port),
                Err(e) => self.err(at, e),
            }
        }
        if out.len() > 1 {
            self.err(
                at,
                crate::tr!(
                    "1 つのサービスに書けるポートは 1 つです",
                    "a service takes one port"
                ),
            );
        }
        out
    }

    /// true/false, also as the text `${VAR}` leaves ("true", "false").
    fn boolean(&mut self, at: &str, v: Option<&Value>) -> bool {
        match v {
            None => false,
            Some(Value::Bool(x)) => *x,
            Some(Value::String(s)) if s == "true" || s == "false" => s == "true",
            Some(_) => {
                self.err(
                    at,
                    crate::tr!("true か false にしてください", "must be true or false"),
                );
                false
            }
        }
    }

    /// A whole number, also as the text `${VAR}` leaves.
    fn number(&mut self, at: &str, v: Option<&Value>) -> Option<u64> {
        match v {
            None => None,
            Some(Value::Number(n)) if n.as_u64().is_some() => n.as_u64(),
            Some(Value::String(s)) if s.parse::<u64>().is_ok() => s.parse().ok(),
            Some(_) => {
                self.err(
                    at,
                    crate::tr!(
                        "0 以上の整数にしてください",
                        "must be a whole number (0 or more)"
                    ),
                );
                None
            }
        }
    }

    fn opt_string(&mut self, at: &str, v: Option<&Value>) -> Option<String> {
        match v {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => {
                self.err(at, crate::tr!("文字列にしてください", "must be a string"));
                None
            }
        }
    }

    fn strings(&mut self, at: &str, v: Option<&Value>) -> Vec<String> {
        match v {
            None => Vec::new(),
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|x| match x {
                    Value::String(s) => Some(s.clone()),
                    _ => {
                        self.err(
                            at,
                            crate::tr!("文字列の一覧にしてください", "must be a list of strings"),
                        );
                        None
                    }
                })
                .collect(),
            Some(_) => {
                self.err(
                    at,
                    crate::tr!("一覧にしてください([...])", "must be a list ([...])"),
                );
                Vec::new()
            }
        }
    }

    fn list_of_maps(&mut self, at: &str, v: Option<&Value>) -> Vec<Map<String, Value>> {
        match v {
            None => Vec::new(),
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|x| match x {
                    Value::Object(m) => Some(m.clone()),
                    _ => {
                        self.err(
                            at,
                            crate::tr!("対応表の一覧にしてください", "must be a list of mappings"),
                        );
                        None
                    }
                })
                .collect(),
            Some(_) => {
                self.err(at, crate::tr!("一覧にしてください", "must be a list"));
                Vec::new()
            }
        }
    }

    /// Dependency order (DES-002). `depends_on` must not loop; bindings may
    /// (two Workers calling each other), so they only order where they can.
    fn order(&mut self, services: Vec<Service>) -> Vec<Service> {
        let names: Vec<String> = services.iter().map(|s| s.name.clone()).collect();
        let resource_service: BTreeMap<String, String> = services
            .iter()
            .flat_map(|s| {
                resource_keys(s)
                    .into_iter()
                    .map(move |k| (k, s.name.clone()))
            })
            .collect();
        let mut hard: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut soft: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for s in &services {
            let h = hard.entry(s.name.clone()).or_default();
            let so = soft.entry(s.name.clone()).or_default();
            match &s.kind {
                Kind::Worker(w) => {
                    h.extend(w.depends_on.iter().cloned());
                    for (_, t) in &w.bindings {
                        let dep = match t {
                            Target::Worker(n) => n.clone(),
                            Target::DurableObject { service, .. } => service.clone(),
                            Target::D1(k) | Target::R2(k) | Target::Kv(k) | Target::Queue(k) => {
                                resource_service.get(k).cloned().unwrap_or_default()
                            }
                        };
                        if dep != s.name {
                            so.insert(dep);
                        }
                    }
                    for q in &w.consumes {
                        so.extend(resource_service.get(q).cloned());
                    }
                }
                Kind::R2(buckets) => {
                    for b in buckets {
                        for n in &b.spec.notifications {
                            so.extend(resource_service.get(&n.queue).cloned());
                        }
                    }
                }
                _ => {}
            }
        }
        let combined: BTreeMap<String, BTreeSet<String>> = names
            .iter()
            .map(|n| (n.clone(), hard[n].union(&soft[n]).cloned().collect()))
            .collect();
        let order = topo(&names, &combined).or_else(|| topo(&names, &hard));
        let Some(order) = order else {
            self.err(
                "depends_on",
                crate::tr!("depends_on が輪になっています", "depends_on forms a cycle"),
            );
            return services;
        };
        let mut by_name: BTreeMap<String, Service> =
            services.into_iter().map(|s| (s.name.clone(), s)).collect();
        order
            .into_iter()
            .filter_map(|n| by_name.remove(&n))
            .collect()
    }
}

/// Stable topological sort: file order where dependencies allow.
fn topo(names: &[String], deps: &BTreeMap<String, BTreeSet<String>>) -> Option<Vec<String>> {
    let mut done: Vec<String> = Vec::new();
    while done.len() < names.len() {
        let next = names.iter().find(|n| {
            !done.contains(n)
                && deps[*n]
                    .iter()
                    .all(|d| done.contains(d) || !names.contains(d))
        })?;
        done.push(next.clone());
    }
    Some(done)
}

pub fn resource_keys(s: &Service) -> Vec<String> {
    match &s.kind {
        Kind::Worker(_) => Vec::new(),
        Kind::D1(r) => r.iter().map(|x| x.key.clone()).collect(),
        Kind::R2(r) => r.iter().map(|x| x.key.clone()).collect(),
        Kind::Kv(r) => r.iter().map(|x| x.key.clone()).collect(),
        Kind::Queue(r) => r.iter().map(|x| x.key.clone()).collect(),
    }
}

fn plural_key(t: &str) -> Option<&'static str> {
    match t {
        "d1" => Some("databases"),
        "r2" => Some("buckets"),
        "kv" => Some("namespaces"),
        "queue" => Some("queues"),
        _ => None,
    }
}

type Types = BTreeMap<String, (String, bool, Vec<String>)>;

/// `cache`, `storage.files` or `api.Counter` → what it points at (REQ-003).
fn resolve_target(t: &str, types: &Types) -> std::result::Result<Target, String> {
    let (svc, sub) = match t.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (t, None),
    };
    let Some((ty, _, _)) = types.get(svc) else {
        return Err(crate::tr!(
            "サービス「{svc}」はありません",
            "there is no service \"{svc}\""
        ));
    };
    if ty == "worker" {
        return Ok(match sub {
            None => Target::Worker(svc.to_string()),
            Some(class) => Target::DurableObject {
                service: svc.to_string(),
                class: class.to_string(),
            },
        });
    }
    let key = resolve_resource(t, ty, types)?;
    Ok(match ty.as_str() {
        "d1" => Target::D1(key),
        "r2" => Target::R2(key),
        "kv" => Target::Kv(key),
        _ => Target::Queue(key),
    })
}

/// A reference to a resource of type `want` → its key.
fn resolve_resource(t: &str, want: &str, types: &Types) -> std::result::Result<String, String> {
    let (svc, sub) = match t.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (t, None),
    };
    let Some((ty, plural, subs)) = types.get(svc) else {
        return Err(crate::tr!(
            "サービス「{svc}」はありません",
            "there is no service \"{svc}\""
        ));
    };
    if ty != want {
        return Err(crate::tr!(
            "「{t}」は {want} ではありません({ty} です)",
            "\"{t}\" is not a {want} (it is a {ty})"
        ));
    }
    match (plural, sub) {
        (false, None) => Ok(svc.to_string()),
        (false, Some(_)) => Err(crate::tr!(
            "サービス「{svc}」は 1 つだけの形なので、「{svc}」と書いてください",
            "service \"{svc}\" holds a single resource; write \"{svc}\""
        )),
        (true, None) => Err(crate::tr!(
            "サービス「{svc}」には複数あるので、「{svc}.<名前>」で指してください({})",
            "service \"{svc}\" holds several; write \"{svc}.<name>\" ({})",
            subs.join(", ")
        )),
        (true, Some(s)) if subs.iter().any(|x| x == s) => Ok(format!("{svc}-{s}")),
        (true, Some(s)) => Err(crate::tr!(
            "サービス「{svc}」に「{s}」はありません",
            "service \"{svc}\" has no \"{s}\""
        )),
    }
}

/// `"8787:8787"` or `"127.0.0.1:8787:8787"` (docker's short syntax; a
/// Worker has no inner port, so both numbers must match). Without an
/// address it listens on every address, as docker does (REQ-009).
/// A registry image as Wrangler takes it: not a path, with a tag or a
/// digest (`docker.io/library/busybox:1.36`, `…@sha256:…`).
fn is_image_ref(s: &str) -> bool {
    if s.starts_with('.')
        || s.starts_with('/')
        || s.contains("://")
        || s.contains(char::is_whitespace)
    {
        return false;
    }
    let last = s.rsplit('/').next().unwrap_or(s);
    s.contains("@sha256:") || last.contains(':')
}

fn parse_port(s: &str) -> std::result::Result<Port, String> {
    let s = s.strip_suffix("/tcp").unwrap_or(s);
    let (ip, rest) = if let Some(r) = s.strip_prefix('[') {
        match r.split_once("]:") {
            Some((ip, rest)) => (ip.to_string(), rest.to_string()),
            None => return Err(bad_port(s)),
        }
    } else {
        let parts: Vec<&str> = s.split(':').collect();
        match parts.len() {
            2 => ("0.0.0.0".to_string(), s.to_string()),
            3 => (parts[0].to_string(), format!("{}:{}", parts[1], parts[2])),
            _ => return Err(bad_port(s)),
        }
    };
    let Some((a, b)) = rest.split_once(':') else {
        return Err(bad_port(s));
    };
    let (Ok(a), Ok(b)) = (a.parse::<u16>(), b.parse::<u16>()) else {
        return Err(bad_port(s));
    };
    if a == 0 || ip.parse::<std::net::IpAddr>().is_err() {
        return Err(bad_port(s));
    }
    if a != b {
        return Err(crate::tr!(
            "「{s}」: Worker には中のポートが無いので、2 つのポートは同じ数にしてください(例: \"{a}:{a}\")",
            "\"{s}\": a Worker has no inner port, so both numbers must be the same (e.g. \"{a}:{a}\")"
        ));
    }
    Ok(Port { ip, port: a })
}

fn bad_port(s: &str) -> String {
    crate::tr!(
        "「{s}」は \"8787:8787\" か \"127.0.0.1:8787:8787\" の形で書いてください",
        "write \"{s}\" as \"8787:8787\" or \"127.0.0.1:8787:8787\""
    )
}

/// Lowercase letters, digits and `-`, never `--` (the separator of names
/// on Cloudflare, REQ-006) nor a `-` at either end.
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
}

fn valid_binding(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    let shape = b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    let num = |r: std::ops::Range<usize>| s.get(r).and_then(|x| x.parse::<u32>().ok()).unwrap_or(0);
    shape && (1..=12).contains(&num(5..7)) && (1..=31).contains(&num(8..10))
}

/// Where a docker compose key's job is done instead (REQ-001).
fn hint_for(key: &str) -> Option<String> {
    Some(match key {
        "image" => crate::tr!("型は type に書きます", "use type for the kind of service"),
        "env_file" => crate::tr!(
            "値は environment か secrets に書き、環境ごとの違いは ${{VAR}} と .env.<環境> で表します",
            "put values in environment or secrets; per-environment values come from ${{VAR}} and .env.<env>"
        ),
        "volumes" => crate::tr!(
            "データは d1・r2・kv・queue のサービスとして書きます",
            "data lives in d1, r2, kv or queue services"
        ),
        "networks" | "links" => crate::tr!(
            "つながりは bindings に書きます",
            "connections go in bindings"
        ),
        "profiles" | "extends" | "include" => crate::tr!(
            "環境の切り替えは --env で行います",
            "switch environments with --env"
        ),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(yaml: &str, env_file: Option<&str>) -> (tempdir::Dir, Result<Compose>) {
        let d = tempdir::Dir::new();
        for f in ["web", "api", "api/migrations"] {
            std::fs::create_dir_all(d.0.join(f)).unwrap();
        }
        std::fs::write(d.0.join("wrangler-compose.yaml"), yaml).unwrap();
        if let Some(e) = env_file {
            std::fs::write(d.0.join(".env.local"), e).unwrap();
        }
        let r = load(&d.0.join("wrangler-compose.yaml"), "local");
        (d, r)
    }

    mod tempdir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new() -> Dir {
                use std::sync::atomic::{AtomicUsize, Ordering};
                static N: AtomicUsize = AtomicUsize::new(0);
                let p = std::env::temp_dir().join(format!(
                    "wc-compose-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::SeqCst)
                ));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    const EXAMPLE: &str = r#"
name: shop
services:
  web:
    type: worker
    build: ./web
    compatibility_date: "2026-09-01"
    ports: ["8787:8787"]
    routes: ["${WEB_DOMAIN}"]
    bindings:
      API: api
      FILES: files
  api:
    type: worker
    build: ./api
    compatibility_date: "2026-09-01"
    ports: ["127.0.0.1:8788:8788"]
    bindings:
      DB: db
      FILES: files
      CACHE: cache
      JOBS: jobs
    consumes: [jobs]
    durable_objects: [Counter]
    secrets: [STRIPE_KEY]
    x-note: anything
  db:
    type: d1
    migrations: ./api/migrations
  files:
    type: r2
    public: true
    cors:
      - origins: ["https://${WEB_DOMAIN}"]
        methods: [GET, PUT]
  uploads:
    type: r2
    name: incoming
    notifications:
      - queue: jobs
        events: [object-create]
  cache:
    type: kv
  jobs:
    type: queue
x-anything: 1
"#;

    #[test]
    fn reads_the_example_in_dependency_order() {
        let (_d, r) = project(EXAMPLE, Some("WEB_DOMAIN=x.example\nSTRIPE_KEY=sk\n"));
        let c = r.unwrap();
        assert_eq!(c.name, "shop");
        let order: Vec<&str> = c.services.iter().map(|s| s.name.as_str()).collect();
        let pos = |n| order.iter().position(|x| *x == n).unwrap();
        assert!(pos("api") < pos("web"));
        assert!(pos("db") < pos("api") && pos("jobs") < pos("uploads"));
        let web = c.services.iter().find(|s| s.name == "web").unwrap();
        let Kind::Worker(w) = &web.kind else { panic!() };
        assert_eq!(w.routes, ["x.example"]);
        assert_eq!(
            w.bindings[1],
            ("FILES".to_string(), Target::R2("files".into()))
        );
        let r2 = |n: &str| match &c.service(n).unwrap().kind {
            Kind::R2(b) => &b[0],
            _ => panic!(),
        };
        assert_eq!(r2("files").spec.cors[0].origins, ["https://x.example"]);
        assert_eq!(r2("uploads").spec.notifications[0].queue, "jobs");
        assert_eq!(c.service("uploads").unwrap().cloud_name(), "incoming");
        assert_eq!(c.service("files").unwrap().cloud_name(), "files");
        let api = c.services.iter().find(|s| s.name == "api").unwrap();
        let Kind::Worker(w) = &api.kind else { panic!() };
        assert_eq!(w.secrets["STRIPE_KEY"], "sk");
    }

    #[test]
    fn reports_every_missing_variable_at_once() {
        let err = project(EXAMPLE, None).1.unwrap_err().to_string();
        assert!(err.contains("WEB_DOMAIN"), "{err}");
    }

    const ONE_WORKER: &str = r#"
name: shop
services:
  web:
    type: worker
    build: ./web
    compatibility_date: "2026-09-01"
"#;

    #[test]
    fn context_defaults_to_the_compose_folder() {
        let (d, r) = project(ONE_WORKER, None);
        assert_eq!(r.unwrap().context, d.0);
    }

    #[test]
    fn context_names_the_folder_with_the_package_json() {
        let d = tempdir::Dir::new();
        let (deploy, app) = (d.0.join("deploy"), d.0.join("app"));
        std::fs::create_dir_all(&deploy).unwrap();
        std::fs::create_dir_all(app.join("web")).unwrap();
        std::fs::write(app.join("package.json"), "{}").unwrap();
        let yaml = format!(
            "context: ../app\n{}",
            ONE_WORKER.replace("./web", "../app/web")
        );
        std::fs::write(deploy.join("wrangler-compose.yaml"), yaml).unwrap();
        let c = load(&deploy.join("wrangler-compose.yaml"), "local").unwrap();
        assert_eq!(c.context, std::fs::canonicalize(&app).unwrap());
        assert_eq!(c.root, deploy);
    }

    #[test]
    fn context_without_a_package_json_stops() {
        let yaml = format!("context: ./web\n{ONE_WORKER}");
        let err = project(&yaml, None).1.unwrap_err().to_string();
        assert!(
            err.contains("context") && err.contains("package.json"),
            "{err}"
        );
    }

    #[test]
    fn stops_with_where_and_why() {
        let yaml = r#"
name: shop
services:
  web:
    type: worker
    build: ./web
    compatibility_date: "2026-09-01"
    image: nginx
    ports: ["8787:8788"]
    bindings:
      A: nothing
      B: storage
  storage:
    type: r2
    buckets:
      files: {}
  db:
    type: d1
    ports: ["1:1"]
"#;
        let err = project(yaml, None).1.unwrap_err().to_string();
        for want in [
            "services.web.image",
            "services.web.ports",
            "services.web.bindings.A",
            "services.web.bindings.B",
            "services.db.ports",
        ] {
            assert!(err.contains(want), "{want} missing in:\n{err}");
        }
    }

    #[test]
    fn stops_on_mistakes_that_would_fail_later() {
        let yaml = r#"
name: shop
services:
  web:
    type: worker
    build: ./web
    compatibility_date: "2026-99-01"
    ports: ["bogus:0:0"]
    bindings:
      X: api.Nope
      DB: cache
      __WC_DO_Counter: api.Counter
    environment:
      DB: "1"
    consumes: [jobs]
  api:
    type: worker
    build: ./api
    compatibility_date: "2026-09-01"
    durable_objects: [Counter]
    consumes: [jobs]
  wc-gateway:
    type: kv
  storage:
    type: r2
    buckets:
      files: {}
  files:
    type: r2
    name: same
  more-files:
    type: r2
    name: same
  cache:
    type: kv
  jobs:
    type: queue
"#;
        let err = project(yaml, None).1.unwrap_err().to_string();
        for want in [
            "services.web.compatibility_date",
            "services.web.ports",
            "services.web.bindings.X",
            "services.web.environment.DB",
            "services.web.bindings.__WC_DO_Counter",
            "services.api.consumes",
            "services.wc-gateway",
            "services.storage.buckets",
            "services.more-files",
        ] {
            assert!(err.contains(want), "{want} missing in:\n{err}");
        }
    }

    #[test]
    fn skips_x_keys_and_reads_interpolated_types() {
        let yaml = r#"
name: shop
x-common: &date "2026-09-01"
services:
  x-template:
    anything: [goes]
  web:
    type: worker
    build: ./web
    compatibility_date: *date
    x-note: here too
    bindings:
      x-later: nothing
      FILES: storage
  storage:
    type: r2
    public: ${PUB:-true}
    cors:
      - origins: ["*"]
        methods: [GET]
        max_age: "${AGE:-600}"
"#;
        let c = project(yaml, None).1.unwrap();
        let Kind::R2(b) = &c.service("storage").unwrap().kind else {
            panic!()
        };
        assert!(b[0].spec.public);
        assert_eq!(b[0].spec.cors[0].max_age, Some(600));
        assert!(c.service("x-template").is_none());
    }

    #[test]
    fn reports_missing_secrets_with_missing_variables() {
        let yaml = r#"
name: shop
services:
  web:
    type: worker
    build: ./web
    compatibility_date: "2026-09-01"
    routes: ["${WEB_DOMAIN}/*"]
    secrets:
      KEY: ${STRIPE_KEY}
"#;
        let err = project(yaml, None).1.unwrap_err().to_string();
        assert!(
            err.contains("WEB_DOMAIN") && err.contains("STRIPE_KEY"),
            "{err}"
        );
    }

    #[test]
    fn an_environment_name_without_a_value_is_empty() {
        let yaml = format!("{ONE_WORKER}    environment:\n      EMPTY:\n      SET: x\n");
        let c = project(&yaml, None).1.unwrap();
        let Kind::Worker(w) = &c.service("web").unwrap().kind else {
            panic!()
        };
        assert_eq!(w.vars["EMPTY"], "");
        assert_eq!(w.vars["SET"], "x");
    }

    #[test]
    fn environment_and_secrets_take_both_docker_forms() {
        let yaml = r#"
name: shop
services:
  web:
    type: worker
    build: ./web
    compatibility_date: "2026-09-01"
    environment: [MODE=prod, REGION]
    secrets: [STRIPE_KEY]
  api:
    type: worker
    build: ./api
    compatibility_date: "2026-09-01"
    environment:
      MODE: dev
    secrets:
      SIGNING: ${SIGNING_KEY}
"#;
        let env = "REGION=apac\nSTRIPE_KEY=sk\nSIGNING_KEY=k1\n";
        let c = project(yaml, Some(env)).1.unwrap();
        let w = |n| match &c.service(n).unwrap().kind {
            Kind::Worker(w) => w,
            _ => panic!(),
        };
        assert_eq!(w("web").vars["MODE"], "prod");
        assert_eq!(w("web").vars["REGION"], "apac");
        assert_eq!(w("web").secrets["STRIPE_KEY"], "sk");
        assert_eq!(w("api").vars["MODE"], "dev");
        assert_eq!(w("api").secrets["SIGNING"], "k1");
    }

    const R2_ENDPOINTS: &str = r#"
name: shop
services:
  web:
    type: worker
    build: ./web
    compatibility_date: "2026-09-01"
    environment:
      FILES: ${WC_R2_STORAGE_S3}
      LOGS: prefix ${WC_R2_ARCHIVE_S3}
      PUBLIC: ${PUBLIC_S3:-${WC_R2_STORAGE_S3}}
  storage:
    type: r2
    ports: ["9050:9050"]
  archive:
    type: r2
    ports: ["9051:9051"]
    name: logs
    jurisdiction: eu
"#;

    fn web_vars(c: &Compose) -> &Map<String, Value> {
        match &c.service("web").unwrap().kind {
            Kind::Worker(w) => &w.vars,
            _ => panic!(),
        }
    }

    #[test]
    fn the_r2_endpoint_variable_is_the_local_port_or_the_account() {
        let c = project(R2_ENDPOINTS, None).1.unwrap();
        let v = web_vars(&c);
        assert_eq!(
            v["FILES"],
            "http://127.0.0.1:9050/cdn-cgi/local/r2/s3/storage"
        );
        assert_eq!(
            v["PUBLIC"],
            "http://127.0.0.1:9050/cdn-cgi/local/r2/s3/storage"
        );

        let d = tempdir::Dir::new();
        std::fs::create_dir_all(d.0.join("web")).unwrap();
        std::fs::write(d.0.join("wrangler-compose.yaml"), R2_ENDPOINTS).unwrap();
        std::fs::write(
            d.0.join(".env.staging"),
            "CLOUDFLARE_ACCOUNT_ID=acc\nPUBLIC_S3=https://cdn.example\n",
        )
        .unwrap();
        let c = load(&d.0.join("wrangler-compose.yaml"), "staging").unwrap();
        let v = web_vars(&c);
        assert_eq!(
            v["FILES"],
            "https://acc.r2.cloudflarestorage.com/shop--staging--storage"
        );
        assert_eq!(
            v["LOGS"],
            "prefix https://acc.eu.r2.cloudflarestorage.com/shop--staging--logs"
        );
        assert_eq!(v["PUBLIC"], "https://cdn.example");
    }

    #[test]
    fn the_r2_endpoint_variable_stops_where_it_cannot_be_known() {
        // No ports locally, an unknown bucket, and a place other than environment.
        let yaml = R2_ENDPOINTS
            .replace("    ports: [\"9050:9050\"]\n", "")
            .replace("${WC_R2_ARCHIVE_S3}", "${WC_R2_NOPE_S3}");
        let err = project(&yaml, None).1.unwrap_err().to_string();
        assert!(
            err.contains("services.web.environment.FILES") && err.contains("ports"),
            "{err}"
        );
        assert!(
            err.contains("WC_R2_NOPE_S3") && err.contains("WC_R2_STORAGE_S3"),
            "{err}"
        );
        let yaml = format!("{ONE_WORKER}    routes: [\"${{WC_R2_STORAGE_S3}}\"]\n");
        let err = project(&yaml, None).1.unwrap_err().to_string();
        assert!(err.contains("services.web.routes[0]"), "{err}");
    }

    const R2_KEYS: &str = r#"
name: shop
services:
  web:
    type: worker
    build: ./web
    compatibility_date: "2026-09-01"
    secrets:
      S3_KEY: ${R2_KEY:-${WC_R2_STORAGE_S3_KEY}}
      S3_SECRET: ${R2_SECRET:-${WC_R2_STORAGE_S3_SECRET}}
  storage:
    type: r2
    ports: ["9050:9050"]
"#;

    fn web_secrets(c: &Compose) -> &BTreeMap<String, String> {
        match &c.service("web").unwrap().kind {
            Kind::Worker(w) => &w.secrets,
            _ => panic!(),
        }
    }

    #[test]
    fn the_local_s3_keys_are_random_kept_and_given_as_secrets() {
        let (d, c) = project(R2_KEYS, None);
        let c = c.unwrap();
        let k = c.local_s3.clone().unwrap();
        assert!(k.fresh);
        assert!(k.key.len() >= 20 && k.secret.len() == 64, "{k:?}");
        assert_ne!(k.key, "wrangler-compose-local");
        assert_eq!(web_secrets(&c)["S3_KEY"], k.key);
        assert_eq!(web_secrets(&c)["S3_SECRET"], k.secret);
        // Another project gets other keys.
        let other = project(R2_KEYS, None).1.unwrap().local_s3.unwrap();
        assert_ne!(other.key, k.key);
        assert_ne!(other.secret, k.secret);
        // Kept keys are read back as they are.
        let file = s3_keys_file(&d.0);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, r#"{"key":"wckept","secret":"s3cret"}"#).unwrap();
        let c = load(&d.0.join("wrangler-compose.yaml"), "local").unwrap();
        assert_eq!(
            c.local_s3,
            Some(S3Keys {
                key: "wckept".into(),
                secret: "s3cret".into(),
                fresh: false
            })
        );
        assert_eq!(web_secrets(&c)["S3_KEY"], "wckept");
    }

    #[test]
    fn the_s3_keys_stay_in_secrets_and_are_not_made_remotely() {
        let yaml = R2_KEYS.replace("    secrets:", "    environment:");
        let err = project(&yaml, None).1.unwrap_err().to_string();
        assert!(
            err.contains("services.web.environment.S3_KEY") && err.contains("secrets"),
            "{err}"
        );

        let d = tempdir::Dir::new();
        std::fs::create_dir_all(d.0.join("web")).unwrap();
        std::fs::write(d.0.join("wrangler-compose.yaml"), R2_KEYS).unwrap();
        std::fs::write(d.0.join(".env.staging"), "CLOUDFLARE_ACCOUNT_ID=acc\n").unwrap();
        let err = load(&d.0.join("wrangler-compose.yaml"), "staging")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("services.web.secrets.S3_KEY") && err.contains(".env.staging"),
            "{err}"
        );
        std::fs::write(
            d.0.join(".env.staging"),
            "CLOUDFLARE_ACCOUNT_ID=acc\nR2_KEY=k\nR2_SECRET=s\n",
        )
        .unwrap();
        let c = load(&d.0.join("wrangler-compose.yaml"), "staging").unwrap();
        assert!(c.local_s3.is_none());
        assert_eq!(web_secrets(&c)["S3_KEY"], "k");
        assert_eq!(web_secrets(&c)["S3_SECRET"], "s");
    }

    #[test]
    fn an_empty_route_is_as_if_not_written() {
        let yaml =
            format!("{ONE_WORKER}    ports: [\"8787:8787\"]\n    routes: [\"${{DOMAIN:-}}\"]\n");
        let c = project(&yaml, None).1.unwrap();
        let Kind::Worker(w) = &c.service("web").unwrap().kind else {
            panic!()
        };
        assert!(w.routes.is_empty());
        let c = project(&yaml, Some("DOMAIN=x.example\n")).1.unwrap();
        let Kind::Worker(w) = &c.service("web").unwrap().kind else {
            panic!()
        };
        assert_eq!(w.routes, ["x.example"]);
    }

    #[test]
    fn a_secret_written_in_the_compose_file_stops() {
        for secrets in [
            "{ KEY: sk_live_1 }",
            "{ KEY: \"$$notavar\" }",
            "[KEY=sk_live_1]",
        ] {
            let yaml = format!("{ONE_WORKER}    secrets: {secrets}\n");
            let err = project(&yaml, Some("KEY=x\n")).1.unwrap_err().to_string();
            assert!(err.contains("services.web.secrets"), "{secrets}: {err}");
            assert!(!err.contains("sk_live_1"), "the secret leaked: {err}");
        }
    }

    #[test]
    fn the_forms_before_0_1_8_stop_with_what_to_write() {
        let yaml = format!(
            "{ONE_WORKER}    vars:\n      A: b\n    secrets: [key]\nsecrets:\n  key:\n    environment: KEY\n"
        );
        let err = project(&yaml, None).1.unwrap_err().to_string();
        assert!(
            err.contains("services.web.vars") && err.contains("environment"),
            "{err}"
        );
        assert!(err.contains("  - secrets: "), "{err}");
    }

    #[test]
    fn depends_on_cycle_stops_but_binding_cycle_does_not() {
        let base = r#"
name: shop
services:
  a:
    type: worker
    build: ./web
    compatibility_date: "2026-09-01"
    bindings: { B: b }
    DEP_A
  b:
    type: worker
    build: ./api
    compatibility_date: "2026-09-01"
    bindings: { A: a }
    DEP_B
"#;
        let ok = base.replace("DEP_A", "").replace("DEP_B", "");
        assert!(project(&ok, None).1.is_ok());
        let cyc = base
            .replace("DEP_A", "depends_on: [b]")
            .replace("DEP_B", "depends_on: [a]");
        assert!(
            project(&cyc, None)
                .1
                .unwrap_err()
                .to_string()
                .contains("depends_on")
        );
    }

    #[test]
    fn names_tell_the_project_and_environment_apart() {
        // The project name is required and 3 or more characters; no part of
        // a name may hold `--`, the separator on Cloudflare.
        let without = ONE_WORKER.replace("name: shop\n", "");
        let err = project(&without, None).1.unwrap_err().to_string();
        assert!(err.contains("name:"), "{err}");
        for bad in ["ab", "a--b", "-ab"] {
            let yaml = ONE_WORKER.replace("name: shop", &format!("name: \"{bad}\""));
            assert!(project(&yaml, None).1.is_err(), "{bad}");
        }
        let yaml = format!("{ONE_WORKER}    name: my--api\n");
        let err = project(&yaml, None).1.unwrap_err().to_string();
        assert!(err.contains("services.web.name"), "{err}");
        let yaml = ONE_WORKER.replace("  web:", "  my--web:");
        assert!(project(&yaml, None).1.is_err());
        assert!(valid_name("staging-eu") && !valid_name("staging--eu"));
    }

    #[test]
    fn parses_ports_like_docker_short_syntax() {
        assert_eq!(
            parse_port("8787:8787").unwrap(),
            Port {
                ip: "0.0.0.0".into(),
                port: 8787
            }
        );
        assert_eq!(parse_port("0.0.0.0:80:80").unwrap().ip, "0.0.0.0");
        assert_eq!(parse_port("[::1]:9:9").unwrap().ip, "::1");
        assert!(parse_port("8787").is_err());
        assert!(parse_port("1:2").is_err());
    }
    /// A project whose `web` Worker owns the Durable Object class `Pinger`,
    /// with a Dockerfile at `box/Dockerfile` (REQ-020).
    fn container_project(containers: &str) -> (tempdir::Dir, Result<Compose>) {
        let d = tempdir::Dir::new();
        for f in ["web", "box"] {
            std::fs::create_dir_all(d.0.join(f)).unwrap();
        }
        std::fs::write(d.0.join("box/Dockerfile"), "FROM busybox\n").unwrap();
        let yaml =
            format!("{ONE_WORKER}    durable_objects: [Pinger]\n    containers:\n{containers}");
        std::fs::write(d.0.join("wrangler-compose.yaml"), yaml).unwrap();
        let r = load(&d.0.join("wrangler-compose.yaml"), "local");
        (d, r)
    }

    fn web(c: &Compose) -> &Worker {
        match &c.services.iter().find(|s| s.name == "web").unwrap().kind {
            Kind::Worker(w) => w,
            _ => unreachable!(),
        }
    }

    #[test]
    fn a_container_with_a_dockerfile_is_read() {
        let (d, c) = container_project(
            "      - class: Pinger\n        image: ./box/Dockerfile\n        instance_type: basic\n        max_instances: 2\n        image_vars: { V: \"1\" }\n",
        );
        let c = c.unwrap();
        let k = &web(&c).containers[0];
        assert_eq!(k.class, "Pinger");
        assert_eq!(k.image, Image::Dockerfile(d.0.join("./box/Dockerfile")));
        assert_eq!(k.build_context, Some(d.0.join("./box")));
        assert_eq!(k.instance_type, Some(serde_json::json!("basic")));
        assert_eq!(k.max_instances, Some(2));
        assert_eq!(k.image_vars.get("V").map(String::as_str), Some("1"));
    }

    #[test]
    fn a_container_from_a_registry_is_read() {
        let (_d, c) = container_project(
            "      - class: Pinger\n        image: docker.io/library/busybox:1.36\n        instance_type: { vcpu: 1, memory_mib: 3072, disk_mb: 4000 }\n",
        );
        let c = c.unwrap();
        let k = &web(&c).containers[0];
        assert_eq!(
            k.image,
            Image::Registry("docker.io/library/busybox:1.36".into())
        );
        assert_eq!(k.build_context, None);
        assert_eq!(k.instance_type.as_ref().unwrap()["memory_mib"], 3072);
    }

    #[test]
    fn container_mistakes_stop_with_where_and_why() {
        for (containers, want) in [
            // a class the Worker does not own
            (
                "      - class: Other\n        image: ./box/Dockerfile\n",
                "durable_objects",
            ),
            // two containers on one class
            (
                "      - class: Pinger\n        image: ./box/Dockerfile\n      - class: Pinger\n        image: ./box/Dockerfile\n",
                "Pinger",
            ),
            // an unknown key
            (
                "      - class: Pinger\n        image: ./box/Dockerfile\n        scheduling_policy: durable_object\n",
                "scheduling_policy",
            ),
            // an unknown instance type
            (
                "      - class: Pinger\n        image: ./box/Dockerfile\n        instance_type: huge\n",
                "instance_type",
            ),
            // a Dockerfile that is not there
            (
                "      - class: Pinger\n        image: ./box/Missing\n",
                "Dockerfile",
            ),
            // build settings on a registry image
            (
                "      - class: Pinger\n        image: docker.io/library/busybox:1.36\n        image_vars: { V: \"1\" }\n",
                "image_vars",
            ),
            // no image
            ("      - class: Pinger\n", "image"),
            // max_instances must be 1 or more
            (
                "      - class: Pinger\n        image: ./box/Dockerfile\n        max_instances: 0\n",
                "max_instances",
            ),
        ] {
            let err = container_project(containers).1.unwrap_err().to_string();
            assert!(
                err.contains("containers") && err.contains(want),
                "{want}: {err}"
            );
        }
    }

    #[test]
    fn one_instance_is_the_smallest_max() {
        let (_d, c) = container_project(
            "      - class: Pinger\n        image: ./box/Dockerfile\n        max_instances: 1\n",
        );
        assert_eq!(web(&c.unwrap()).containers[0].max_instances, Some(1));
    }

    #[test]
    fn placement_observability_and_cache_are_read_as_written() {
        let yaml = format!(
            "{ONE_WORKER}    placement: {{ mode: smart }}\n    observability: {{ enabled: true, head_sampling_rate: 0.5 }}\n    cache: {{ enabled: \"false\" }}\n"
        );
        let c = project(&yaml, None).1.unwrap();
        let w = web(&c);
        assert_eq!(w.placement, Some(serde_json::json!({ "mode": "smart" })));
        assert_eq!(
            w.observability,
            Some(serde_json::json!({ "enabled": true, "head_sampling_rate": 0.5 }))
        );
        assert_eq!(w.cache, Some(serde_json::json!({ "enabled": false })));
        // Unwritten ones stay unset: Cloudflare's defaults apply.
        let c = project(ONE_WORKER, None).1.unwrap();
        let w = web(&c);
        assert!(w.placement.is_none() && w.observability.is_none() && w.cache.is_none());
    }

    #[test]
    fn placement_observability_and_cache_mistakes_say_where() {
        for (extra, want) in [
            ("    placement: smart\n", "web.placement:"),
            ("    placement: { mode: off }\n", "web.placement.mode"),
            ("    placement: {}\n", "web.placement.mode"),
            (
                "    placement: { mode: smart, hint: x }\n",
                "web.placement.hint",
            ),
            (
                "    observability: { enabled: maybe }\n",
                "web.observability.enabled",
            ),
            (
                "    observability: { head_sampling_rate: 1.5 }\n",
                "web.observability.head_sampling_rate",
            ),
            (
                "    observability: { head_sampling_rate: -0.1 }\n",
                "web.observability.head_sampling_rate",
            ),
            (
                "    observability: { head_sampling_rate: all }\n",
                "web.observability.head_sampling_rate",
            ),
            ("    cache: { enabled: 1 }\n", "web.cache.enabled"),
            ("    cache: [true]\n", "web.cache:"),
        ] {
            let err = project(&format!("{ONE_WORKER}{extra}"), None)
                .1
                .unwrap_err()
                .to_string();
            assert!(err.contains(want), "{want}: {err}");
        }
    }
}
