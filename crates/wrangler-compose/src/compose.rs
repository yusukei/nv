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
    /// In dependency order: what a service needs comes before it.
    pub services: Vec<Service>,
    /// Top-level `secrets`: name → value (REQ-005).
    pub secrets: BTreeMap<String, Secret>,
}

#[derive(Debug, Clone)]
pub struct Secret {
    /// The variable named by `environment:`; also the name the Worker sees.
    pub var: String,
    pub value: String,
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
    pub secrets: Vec<String>,
    pub vars: Map<String, Value>,
    pub routes: Vec<String>,
    pub crons: Vec<String>,
    pub compatibility_date: String,
    pub compatibility_flags: Vec<String>,
    pub assets: Option<PathBuf>,
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
        std::env::var(name)
            .ok()
            .or_else(|| self.0.get(name).cloned())
    }
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
    let vars = Vars(file_vars);
    let mut missing = BTreeSet::new();
    let expanded = expand_tree(strip_extensions(raw), &vars, &mut missing);
    if !missing.is_empty() {
        // Secrets without a value belong in the same list (AC-004).
        for var in secret_vars(&expanded) {
            if vars.get(&var).is_none() {
                missing.insert(Missing {
                    name: var,
                    message: None,
                });
            }
        }
        bail!(missing_message(&missing, env, &env_file));
    }
    let dir_name = root
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "project".to_string());
    let mut c = Checker::default();
    let compose = c.compose(expanded, &root, file, &dir_name, &vars);
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
    Ok(compose.expect("no errors means a compose file"))
}

fn missing_message(missing: &BTreeSet<Missing>, env: &str, env_file: &Path) -> String {
    let lines: Vec<String> = missing
        .iter()
        .map(|m| match &m.message {
            Some(msg) => format!("  - {}: {msg}", m.name),
            None => format!("  - {}", m.name),
        })
        .collect();
    crate::tr!(
        "環境 {env} で、値の無い変数があります(何もしていません)。環境変数か {} に書いてください:\n{}",
        "environment {env} has variables without a value (nothing was done); set them in the environment or in {}:\n{}",
        env_file.display(),
        lines.join("\n")
    )
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

/// The variables the top-level `secrets` read.
fn secret_vars(v: &Value) -> Vec<String> {
    v["secrets"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(_, s)| s["environment"].as_str().map(str::to_string))
        .collect()
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
    "vars",
    "routes",
    "crons",
    "compatibility_date",
    "compatibility_flags",
    "assets",
];
const D1_KEYS: &[&str] = &["name", "migrations"];
const BUCKET_KEYS: &[&str] = &[
    "name",
    "public",
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

    fn compose(
        &mut self,
        v: Value,
        root: &Path,
        file: &Path,
        dir_name: &str,
        vars: &Vars,
    ) -> Option<Compose> {
        let Value::Object(top) = v else {
            self.err(
                "(top)",
                crate::tr!("中身は対応表(キーと値)にしてください", "must be a mapping"),
            );
            return None;
        };
        self.unknown_keys("(top)", &top, &["name", "services", "secrets"]);
        let name = match top.get("name") {
            Some(Value::String(s)) => s.clone(),
            Some(_) => {
                self.err(
                    "name",
                    crate::tr!("文字列にしてください", "must be a string"),
                );
                String::new()
            }
            None => dir_name.to_lowercase(),
        };
        if !valid_name(&name) {
            self.err(
                "name",
                crate::tr!(
                    "プロジェクトの名前「{name}」には英小文字・数字・- だけを使ってください(省くとフォルダ名になります)",
                    "project name \"{name}\" may only use lowercase letters, digits and - (it defaults to the folder name)"
                ),
            );
        }
        let secrets = self.secrets(top.get("secrets"), vars);
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
                        "サービスの名前には英小文字・数字・- だけを使ってください",
                        "service names may only use lowercase letters, digits and -"
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
                    .worker(&at, o, root, &types, &secrets)
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
            let ports = match &s.kind {
                Kind::Worker(w) => &w.ports,
                _ => &s.ports,
            };
            for p in ports {
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
            services,
            secrets,
        })
    }

    /// Checks that need every service: names of resources, Durable Object
    /// classes, queue consumers.
    fn across(&mut self, services: &[Service]) {
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

    fn secrets(&mut self, v: Option<&Value>, vars: &Vars) -> BTreeMap<String, Secret> {
        let mut out = BTreeMap::new();
        let Some(v) = v else { return out };
        let Value::Object(m) = v else {
            self.err(
                "secrets",
                crate::tr!("対応表にしてください", "must be a mapping"),
            );
            return out;
        };
        let mut missing = Vec::new();
        for (name, sv) in m {
            let at = format!("secrets.{name}");
            let Value::Object(o) = sv else {
                self.err(
                    &at,
                    crate::tr!(
                        "environment: <変数名> を書いてください",
                        "write environment: <variable>"
                    ),
                );
                continue;
            };
            self.unknown_keys(&at, o, &["environment"]);
            let Some(Value::String(var)) = o.get("environment") else {
                self.err(&at, crate::tr!("environment: <変数名> を書いてください(値は環境変数か .env.<環境> から読みます)", "write environment: <variable> (the value comes from the environment or .env.<env>)"));
                continue;
            };
            if !valid_binding(var) {
                self.err(
                    &format!("{at}.environment"),
                    crate::tr!(
                        "変数名「{var}」は英字・数字・_ だけにしてください",
                        "variable name \"{var}\" may only use letters, digits and _"
                    ),
                );
                continue;
            }
            match vars.get(var) {
                Some(value) => {
                    out.insert(
                        name.clone(),
                        Secret {
                            var: var.clone(),
                            value,
                        },
                    );
                }
                None => missing.push(var.clone()),
            }
        }
        if !missing.is_empty() {
            self.err(
                "secrets",
                crate::tr!(
                    "シークレットの値がありません。環境変数か .env.<環境> に書いてください: {}",
                    "secrets have no value; set them in the environment or .env.<env>: {}",
                    missing.join(", ")
                ),
            );
        }
        out
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
            Some(Value::Object(items)) => {
                let mut allowed = vec!["type", plural];
                allowed.extend_from_slice(service_keys);
                self.unknown_keys(at, o, &allowed);
                if items.is_empty() {
                    self.err(
                        &format!("{at}.{plural}"),
                        crate::tr!("1 つ以上書いてください", "define at least one"),
                    );
                }
                for (sub, iv) in items {
                    let iat = format!("{at}.{plural}.{sub}");
                    if !valid_name(sub) {
                        self.err(
                            &iat,
                            crate::tr!(
                                "名前には英小文字・数字・- だけを使ってください",
                                "names may only use lowercase letters, digits and -"
                            ),
                        );
                    }
                    let empty = Map::new();
                    let m = match iv {
                        Value::Object(m) => m,
                        Value::Null => &empty,
                        _ => {
                            self.err(
                                &iat,
                                crate::tr!(
                                    "対応表にしてください(設定が無ければ {{}})",
                                    "must be a mapping ({{}} when it has no settings)"
                                ),
                            );
                            continue;
                        }
                    };
                    self.unknown_keys(&iat, m, keys);
                    let name = self.opt_string(&format!("{iat}.name"), m.get("name"));
                    let spec = spec(self, &iat, m);
                    out.push(Resource {
                        sub: Some(sub.clone()),
                        key: format!("{service}-{sub}"),
                        name,
                        spec,
                    });
                }
            }
            Some(_) => self.err(
                &format!("{at}.{plural}"),
                crate::tr!(
                    "名前をキーにした対応表にしてください",
                    "must be a mapping keyed by name"
                ),
            ),
            None => {
                let mut allowed = vec!["type"];
                allowed.extend_from_slice(keys);
                allowed.extend_from_slice(service_keys);
                self.unknown_keys(at, o, &allowed);
                let name = self.opt_string(&format!("{at}.name"), o.get("name"));
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
        secrets: &BTreeMap<String, Secret>,
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
        w.secrets = self.strings(&format!("{at}.secrets"), o.get("secrets"));
        for s in &w.secrets {
            if !secrets.contains_key(s) && !self.errors.iter().any(|e| e.starts_with("secrets")) {
                self.err(
                    &format!("{at}.secrets"),
                    crate::tr!(
                        "シークレット「{s}」が一番上の secrets にありません",
                        "secret \"{s}\" is not in the top-level secrets"
                    ),
                );
            }
        }
        match o.get("vars") {
            None => {}
            Some(Value::Object(m)) => w.vars = m.clone(),
            Some(_) => self.err(
                &format!("{at}.vars"),
                crate::tr!(
                    "「名前: 値」の対応表にしてください",
                    "must be a mapping of name: value"
                ),
            ),
        }
        // One name space in the Worker's env: bindings, vars and secrets.
        let mut names: BTreeMap<String, &str> = BTreeMap::new();
        let secret_names: Vec<String> = w
            .secrets
            .iter()
            .filter_map(|s| secrets.get(s).map(|x| x.var.clone()))
            .collect();
        for (name, what) in w
            .bindings
            .iter()
            .map(|(b, _)| (b.clone(), "bindings"))
            .chain(w.vars.keys().map(|k| (k.clone(), "vars")))
            .chain(secret_names.into_iter().map(|v| (v, "secrets")))
        {
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
        w.routes = self.strings(&format!("{at}.routes"), o.get("routes"));
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
        Some(w)
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
/// Worker has no inner port, so both numbers must match).
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
            2 => ("127.0.0.1".to_string(), s.to_string()),
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

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !s.starts_with('-')
        && !s.ends_with('-')
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
        "environment" | "env_file" => crate::tr!(
            "値は vars か secrets に書き、環境ごとの違いは ${{VAR}} と .env.<環境> で表します",
            "put values in vars or secrets; per-environment values come from ${{VAR}} and .env.<env>"
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
      FILES: storage.files
  api:
    type: worker
    build: ./api
    compatibility_date: "2026-09-01"
    ports: ["127.0.0.1:8788:8788"]
    bindings:
      DB: db
      FILES: storage.files
      CACHE: cache
      JOBS: jobs
    consumes: [jobs]
    durable_objects: [Counter]
    secrets: [stripe-key]
    x-note: anything
  db:
    type: d1
    migrations: ./api/migrations
  storage:
    type: r2
    buckets:
      files:
        public: true
        cors:
          - origins: ["https://${WEB_DOMAIN}"]
            methods: [GET, PUT]
      uploads:
        notifications:
          - queue: jobs
            events: [object-create]
  cache:
    type: kv
  jobs:
    type: queue
secrets:
  stripe-key:
    environment: STRIPE_KEY
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
        assert!(pos("db") < pos("api") && pos("jobs") < pos("storage"));
        let web = c.services.iter().find(|s| s.name == "web").unwrap();
        let Kind::Worker(w) = &web.kind else { panic!() };
        assert_eq!(w.routes, ["x.example"]);
        assert_eq!(
            w.bindings[1],
            ("FILES".to_string(), Target::R2("storage-files".into()))
        );
        let storage = c.services.iter().find(|s| s.name == "storage").unwrap();
        let Kind::R2(b) = &storage.kind else { panic!() };
        assert_eq!(
            b.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(),
            ["storage-files", "storage-uploads"]
        );
        assert_eq!(b[0].spec.cors[0].origins, ["https://x.example"]);
        assert_eq!(b[1].spec.notifications[0].queue, "jobs");
        assert_eq!(c.secrets["stripe-key"].value, "sk");
    }

    #[test]
    fn reports_every_missing_variable_at_once() {
        let err = project(EXAMPLE, None).1.unwrap_err().to_string();
        assert!(err.contains("WEB_DOMAIN"), "{err}");
    }

    #[test]
    fn stops_with_where_and_why() {
        let yaml = r#"
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
    vars:
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
  storage-files:
    type: r2
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
            "services.web.vars.DB",
            "services.web.bindings.__WC_DO_Counter",
            "services.api.consumes",
            "services.wc-gateway",
            "services.storage-files",
        ] {
            assert!(err.contains(want), "{want} missing in:\n{err}");
        }
    }

    #[test]
    fn skips_x_keys_and_reads_interpolated_types() {
        let yaml = r#"
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
secrets:
  x-unused: {}
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
services:
  web:
    type: worker
    build: ./web
    compatibility_date: "2026-09-01"
    routes: ["${WEB_DOMAIN}/*"]
    secrets: [key]
secrets:
  key:
    environment: STRIPE_KEY
"#;
        let err = project(yaml, None).1.unwrap_err().to_string();
        assert!(
            err.contains("WEB_DOMAIN") && err.contains("STRIPE_KEY"),
            "{err}"
        );
    }

    #[test]
    fn depends_on_cycle_stops_but_binding_cycle_does_not() {
        let base = r#"
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
    fn parses_ports_like_docker_short_syntax() {
        assert_eq!(
            parse_port("8787:8787").unwrap(),
            Port {
                ip: "127.0.0.1".into(),
                port: 8787
            }
        );
        assert_eq!(parse_port("0.0.0.0:80:80").unwrap().ip, "0.0.0.0");
        assert_eq!(parse_port("[::1]:9:9").unwrap().ip, "::1");
        assert!(parse_port("8787").is_err());
        assert!(parse_port("1:2").is_err());
    }
}
