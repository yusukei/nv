//! The local environment (REQ-009, REQ-011, REQ-012, DES-008): every service
//! runs on this machine, in one runtime, without touching Cloudflare.

use crate::build::{self, Built};
use crate::compose::{Kind, Service, Target};
use crate::configs;
use crate::ctx::Ctx;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime};

fn say(msg: String) {
    eprintln!("wrangler-compose: {msg}");
}

struct Paths {
    dir: PathBuf,
}

impl Paths {
    fn new(ctx: &Ctx) -> Paths {
        Paths { dir: ctx.env_dir() }
    }
    fn lock(&self) -> PathBuf {
        self.dir.join("up.lock")
    }
    fn record(&self) -> PathBuf {
        self.dir.join("up.json")
    }
    fn state(&self) -> PathBuf {
        self.dir.join("state")
    }
    fn run(&self) -> PathBuf {
        self.dir.join("run")
    }
    fn log(&self) -> PathBuf {
        self.dir.join("logs").join("up.log")
    }
}

/// Holds the project's local lock while `up` (or a data operation) runs.
struct Lock {
    _file: File,
}

fn try_lock(p: &Paths) -> Result<Option<Lock>> {
    std::fs::create_dir_all(&p.dir)
        .with_context(|| crate::tr!("{} を作れません", "cannot create {}", p.dir.display()))?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(p.lock())?;
    match file.try_lock() {
        Ok(()) => Ok(Some(Lock { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

fn read_record(p: &Paths) -> Option<Value> {
    std::fs::read_to_string(p.record())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
}

// ---------------------------------------------------------------- up

pub fn up(ctx: &Ctx, rebuild: bool, names: &[String]) -> Result<()> {
    let p = Paths::new(ctx);
    if ctx.dry_run {
        let services = ctx.compose.closure(names)?;
        notices(&services);
        return dry_run(ctx, &services);
    }
    tool_dir_ignored(ctx)?;
    let Some(_lock) = try_lock(&p)? else {
        let urls = read_record(&p).map(|r| urls_text(&r)).unwrap_or_default();
        bail!(crate::tr!(
            "このプロジェクトでは、ほかの端末で --env local up が動いています{urls}。止めるには --env local down を打ってください",
            "--env local up is already running for this project{urls}; stop it with --env local down"
        ));
    };
    // A restart re-reads the compose file in this process, keeping the lock.
    let mut reopened: Option<Ctx> = None;
    let mut rebuild = rebuild;
    loop {
        let c = reopened.as_ref().unwrap_or(ctx);
        let services = c.compose.closure(names)?;
        notices(&services);
        let next = match run_once(c, &p, &services, rebuild)? {
            Stop::Done => return Ok(()),
            Stop::Restart => {
                say(crate::tr!(
                    "compose ファイルか .env.local が変わったので、動かし直します",
                    "the compose file or .env.local changed; restarting"
                ));
                Ctx::open(&ctx.env, false)?
            }
        };
        reopened = Some(next);
        rebuild = false;
    }
}

enum Stop {
    Done,
    Restart,
}

/// What a running `up` writes to `up.json` for `ps`, `down` and `logs`.
fn record(pid: u32, gateway: u16, token: &str, workers: &[(String, Option<String>)]) -> Value {
    json!({
        "pid": pid,
        "gateway": gateway,
        "token": token,
        "services": workers.iter().map(|(s, u)| json!({ "service": s, "url": u })).collect::<Vec<_>>(),
    })
}

fn urls_text(r: &Value) -> String {
    let urls: Vec<String> = r["services"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| Some(format!("{} {}", s["service"].as_str()?, s["url"].as_str()?)))
        .collect();
    if urls.is_empty() {
        String::new()
    } else {
        format!("({})", urls.join(", "))
    }
}

fn run_once(ctx: &Ctx, p: &Paths, services: &[&Service], rebuild: bool) -> Result<Stop> {
    // 1. Build (or reuse) every Worker's bundle (REQ-008).
    let workers: Vec<(&Service, &crate::compose::Worker)> = services
        .iter()
        .filter_map(|s| match &s.kind {
            Kind::Worker(w) => Some((*s, &**w)),
            _ => None,
        })
        .collect();
    if workers.is_empty() {
        bail!(crate::tr!(
            "動かす Worker がありません",
            "there is no Worker to run"
        ));
    }
    let mut builts: BTreeMap<String, Built> = BTreeMap::new();
    for (s, w) in &workers {
        if rebuild {
            let _ = std::fs::remove_file(
                ctx.tool_dir()
                    .join("build")
                    .join(&s.name)
                    .join("stamp.json"),
            );
        }
        builts.insert(s.name.clone(), build::build_one(ctx, &s.name, w, true)?);
    }

    // 2. Ports: every declared one must be free; the gateway gets any port.
    let mut listeners = Vec::new();
    let mut port_map = Vec::new();
    for (s, w) in &workers {
        for port in &w.ports {
            let addr: SocketAddr = format!("{}:{}", bracket(&port.ip), port.port)
                .parse()
                .map_err(|_| {
                    anyhow!(crate::tr!(
                        "アドレス {} が読めません",
                        "cannot parse address {}",
                        port.ip
                    ))
                })?;
            let l = TcpListener::bind(addr).map_err(|e| {
                anyhow!(crate::tr!(
                    "{} の {addr} で待ち受けられません({e})。ほかのものが使っていないか、ports を変えてください",
                    "cannot listen on {addr} for {} ({e}); is something else using it? change ports",
                    s.name
                ))
            })?;
            listeners.push((s.name.clone(), l));
            port_map.push((port.port, s.name.clone()));
        }
    }
    // r2 services with ports: the S3-compatible API with the bucket's CORS
    // rules, and public URLs (REQ-009).
    let mut r2_sites = Vec::new();
    for s in services.iter() {
        let Kind::R2(buckets) = &s.kind else { continue };
        let Some(port) = s.ports.first() else {
            continue;
        };
        let addr: SocketAddr = format!("{}:{}", bracket(&port.ip), port.port)
            .parse()
            .map_err(|_| {
                anyhow!(crate::tr!(
                    "アドレス {} が読めません",
                    "cannot parse address {}",
                    port.ip
                ))
            })?;
        let l = TcpListener::bind(addr).map_err(|e| {
            anyhow!(crate::tr!(
                "{} の {addr} で待ち受けられません({e})。ほかのものが使っていないか、ports を変えてください",
                "cannot listen on {addr} for {} ({e}); is something else using it? change ports",
                s.name
            ))
        })?;
        let sites: Vec<crate::r2proxy::Site> = buckets
            .iter()
            .map(|b| crate::r2proxy::Site {
                key: b.key.clone(),
                public: b.spec.public,
                cors: b.spec.cors.clone(),
            })
            .collect();
        r2_sites.push((s.name.clone(), port.clone(), l, sites));
    }
    let gateway_port = free_port()?;

    // 3. Configs (DES-005): gateway first, then the Workers and their log
    //    receivers.
    let run = p.run();
    let _ = std::fs::remove_dir_all(&run);
    let mut configs = Vec::new();
    let gw_dir = run.join(configs::GATEWAY);
    std::fs::create_dir_all(&gw_dir)?;
    configs.push(write_cfg(
        &gw_dir,
        &configs::gateway(&gw_dir, &port_map, gateway_port)?,
    )?);
    let token = new_token()?;
    configs.push(data_config(ctx, &run, &token)?);
    for (s, w) in &workers {
        let dir = run.join(&s.name);
        std::fs::create_dir_all(&dir)?;
        let cfg = configs::local_worker(&ctx.compose, &s.name, w, &builts[&s.name]);
        configs.push(write_cfg(&dir, &cfg)?);
        let vars = configs::dev_vars(&ctx.compose, w);
        if !vars.is_empty() {
            write_private(&dir.join(".dev.vars"), &vars)?;
        }
    }
    for (s, _) in &workers {
        let dir = run.join(configs::logs_worker(&s.name));
        std::fs::create_dir_all(&dir)?;
        configs.push(write_cfg(&dir, &configs::logs(&dir, &s.name)?)?);
    }

    // 4. Migrations before anything runs (REQ-009).
    if let Some((cfg, dbs)) = configs::migrate(&ctx.compose, &run.join("wc-migrate"))? {
        for db in dbs {
            say(crate::tr!(
                "{db}: 移行を当てます",
                "{db}: applying migrations"
            ));
            let mut cmd = ctx.wrangler(["d1", "migrations", "apply", &db, "--local", "-c"]);
            cmd.arg(&cfg).arg("--persist-to").arg(p.state());
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
                bail!(crate::tr!(
                    "{db} の移行が失敗したので、動かしません(上の出力を見てください)",
                    "migrations of {db} failed, so nothing was started (see the output above)"
                ));
            }
        }
    }

    // 5. One `wrangler dev` for everything: one runtime, so Workers sharing
    //    a resource never corrupt it (DES facts).
    std::fs::create_dir_all(p.log().parent().unwrap())?;
    let log_file = Arc::new(Mutex::new(
        OpenOptions::new().create(true).append(true).open(p.log())?,
    ));
    let Some(mut child) = spawn_runtime(ctx, p, &configs, gateway_port, Some(log_file))? else {
        return Ok(Stop::Done);
    };

    // 6. Forward every declared port to the gateway, then say where things
    //    are. The listeners close when this run ends, so a restart can bind
    //    the same ports again.
    let stop = Arc::new(AtomicBool::new(false));
    let mut accepters = Vec::new();
    let guarded: Arc<Vec<String>> = Arc::new(
        r2_sites
            .iter()
            .flat_map(|(_, _, _, sites)| sites.iter().map(|x| x.key.clone()))
            .collect(),
    );
    for (_, l) in listeners {
        let target: SocketAddr = format!("127.0.0.1:{gateway_port}").parse()?;
        let (guarded, stop) = (guarded.clone(), stop.clone());
        accepters.push(std::thread::spawn(move || {
            crate::r2proxy::forward_worker(l, target, guarded, stop)
        }));
    }
    let mut r2_urls = Vec::new();
    for (svc, port, l, sites) in r2_sites {
        let base = format!("http://{}:{}", url_host(&port.ip), port.port);
        let public: Vec<String> = sites
            .iter()
            .filter(|x| x.public)
            .map(|x| format!("http://{}.localhost:{}/", x.key, port.port))
            .collect();
        r2_urls.push((
            svc,
            base,
            sites.iter().map(|x| x.key.clone()).collect::<Vec<_>>(),
            public,
        ));
        let shared = std::sync::Arc::new(crate::r2proxy::Shared {
            sites,
            gateway: format!("127.0.0.1:{gateway_port}").parse()?,
            token: token.clone(),
        });
        let stop = stop.clone();
        accepters.push(std::thread::spawn(move || {
            crate::r2proxy::serve(l, shared, stop)
        }));
    }
    let mut urls = Vec::new();
    for (s, w) in &workers {
        let url = w
            .ports
            .first()
            .map(|pt| format!("http://{}:{}", url_host(&pt.ip), pt.port));
        urls.push((s.name.clone(), url));
    }
    write_private(
        &p.record(),
        &serde_json::to_string_pretty(&record(std::process::id(), gateway_port, &token, &urls))?,
    )?;
    announce(ctx, &urls, &port_map, &r2_urls);

    // 7. Watch: rebuild a Worker when its code changes (Wrangler reloads
    //    the bundle); restart when the compose file or .env.local changes.
    let compose_files = [ctx.compose.file.clone(), ctx.root().join(".env.local")];
    let mut compose_stamp = stamps(&compose_files);
    let mut seen: BTreeMap<String, Option<SystemTime>> = workers
        .iter()
        .map(|(s, w)| (s.name.clone(), build::newest_change(&w.build)))
        .collect();
    let has_notify = ctx.compose.services.iter().any(|s| {
        matches!(&s.kind, Kind::R2(list) if list.iter().any(|r| !r.spec.notifications.is_empty()))
    });
    let notifier = has_notify.then(|| {
        crate::data::Client::new(format!("http://127.0.0.1:{gateway_port}"), token.clone())
    });
    let mut last_tick = Instant::now();
    let mut last_watch = Instant::now();
    let result = loop {
        std::thread::sleep(Duration::from_millis(250));
        if let Some(n) = &notifier
            && last_tick.elapsed() >= Duration::from_secs(1)
        {
            last_tick = Instant::now();
            if let Err(e) = n.notify_tick() {
                say(crate::tr!(
                    "R2 の通知を送れません: {e:#}",
                    "cannot send R2 notifications: {e:#}"
                ));
            }
        }
        if crate::interrupt::check().is_err() {
            break Stop::Done;
        }
        if let Some(status) = child.try_wait()? {
            let _ = std::fs::remove_file(p.record());
            bail!(crate::tr!(
                "Wrangler が止まりました({status})",
                "Wrangler stopped ({status})"
            ));
        }
        let now_stamp = stamps(&compose_files);
        if now_stamp != compose_stamp {
            // Restart only into a compose file that loads; a half-edited one
            // keeps the running services as they are.
            compose_stamp = now_stamp;
            match crate::compose::load(&ctx.compose.file, &ctx.env) {
                Ok(_) => break Stop::Restart,
                Err(e) => say(crate::tr!(
                    "compose ファイルを読めないので、直るまで今のまま動かします: {e:#}",
                    "cannot load the compose file, so the services keep running as they are until it is fixed: {e:#}"
                )),
            }
        }
        if last_watch.elapsed() < Duration::from_millis(500) {
            continue;
        }
        last_watch = Instant::now();
        for (s, w) in &workers {
            let now = build::newest_change(&w.build);
            if seen.get(&s.name) != Some(&now) {
                seen.insert(s.name.clone(), now);
                match build::build_one(ctx, &s.name, w, false) {
                    Ok(b) if Some(&b.hash) != builts.get(&s.name).map(|x| &x.hash) => {
                        say(crate::tr!("{}: 組み立て直しました", "{}: rebuilt", s.name));
                        builts.insert(s.name.clone(), b);
                    }
                    Ok(_) => {}
                    Err(e) => say(format!("{e:#}")),
                }
            }
        }
    };
    stop_child(&mut child);
    stop.store(true, Ordering::SeqCst);
    for a in accepters {
        let _ = a.join();
    }
    let _ = std::fs::remove_file(p.record());
    if matches!(result, Stop::Done) {
        say(crate::tr!("止めました", "stopped"));
    }
    Ok(result)
}

type R2Url = (String, String, Vec<String>, Vec<String>);

fn announce(ctx: &Ctx, urls: &[(String, Option<String>)], ports: &[(u16, String)], r2: &[R2Url]) {
    say(crate::tr!(
        "手元で動いています(Ctrl+C で止まります)",
        "running locally (Ctrl+C stops it)"
    ));
    for (s, u) in urls {
        match u {
            Some(u) => eprintln!("  {s:<16} {u}"),
            None => eprintln!(
                "  {s:<16} {}",
                crate::tr!(
                    "(ports 無し。ほかのサービスから呼ぶ)",
                    "(no ports; reached from other services)"
                )
            ),
        }
    }
    for (svc, base, buckets, public) in r2 {
        eprintln!(
            "{}",
            crate::tr!(
                "  {svc:<16} R2 の S3 互換の口: {base}/cdn-cgi/local/r2/s3(パス形式・地域 auto・鍵 {} / {})\n  {:<16} バケット: {}",
                "  {svc:<16} R2 S3-compatible API: {base}/cdn-cgi/local/r2/s3 (path style, region auto, keys {} / {})\n  {:<16} buckets: {}",
                configs::LOCAL_S3_KEY,
                configs::LOCAL_S3_SECRET,
                "",
                buckets.join(", ")
            )
        );
        for p in public {
            eprintln!(
                "  {:<16} {}",
                "",
                crate::tr!("公開の URL: {p}<キー>", "public URL: {p}<key>")
            );
        }
    }
    // r2 services without ports: reachable through a Worker's port, but
    // without their CORS rules and public URLs.
    let without: Vec<&str> = ctx
        .compose
        .services
        .iter()
        .filter(|s| matches!(s.kind, Kind::R2(_)) && s.ports.is_empty())
        .map(|s| s.name.as_str())
        .collect();
    if let (false, Some((port, _))) = (without.is_empty(), ports.first()) {
        say(crate::tr!(
            "{} には ports が無いので、S3 互換の口は http://127.0.0.1:{port}/cdn-cgi/local/r2/s3 で、CORS はバケットの設定によらずすべて許され、公開の URL もありません。本番と同じにするには ports を書いてください",
            "{} has no ports: its S3-compatible API is at http://127.0.0.1:{port}/cdn-cgi/local/r2/s3, allows every CORS origin regardless of the bucket's rules, and has no public URLs. Add ports to behave like production",
            without.join(", ")
        ));
    }
}

/// What `--dry-run up` shows (REQ-014): nothing is built, written or run.
fn dry_run(ctx: &Ctx, services: &[&Service]) -> Result<()> {
    for s in services {
        match &s.kind {
            Kind::Worker(w) => {
                let port = w
                    .ports
                    .first()
                    .map(|p| format!("http://{}:{}", url_host(&p.ip), p.port))
                    .unwrap_or_else(|| "-".into());
                let secrets: Vec<&str> = w
                    .secrets
                    .iter()
                    .filter_map(|n| ctx.compose.secrets.get(n).map(|x| x.var.as_str()))
                    .collect();
                say(crate::tr!(
                    "{}: 組み立てて手元で動かす({port})。シークレット: {}",
                    "{}: would build and run locally ({port}); secrets: {}",
                    s.name,
                    if secrets.is_empty() {
                        "-".to_string()
                    } else {
                        secrets.join(", ")
                    }
                ));
            }
            Kind::D1(list) => {
                for r in list {
                    let m = r
                        .spec
                        .migrations
                        .as_ref()
                        .map(|d| d.display().to_string())
                        .unwrap_or_else(|| "-".into());
                    say(crate::tr!(
                        "{}: 手元の D1(移行: {m})",
                        "{}: local D1 (migrations: {m})",
                        r.key
                    ));
                }
            }
            other => {
                let keys = crate::compose::resource_keys(s);
                say(format!(
                    "{}: {} {}",
                    s.name,
                    other.type_name(),
                    keys.join(", ")
                ));
            }
        }
    }
    say(crate::tr!(
        "dry-run なので何も変えていません",
        "dry run: nothing was changed"
    ));
    Ok(())
}

/// Things that behave differently locally (REQ-018).
fn notices(services: &[&Service]) {
    for s in services {
        if let Kind::Worker(w) = &s.kind {
            if !w.routes.is_empty() {
                say(crate::tr!(
                    "{}: routes({})は手元では効きません。手元の URL で試してください",
                    "{}: routes ({}) do not apply locally; use the local URL",
                    s.name,
                    w.routes.join(", ")
                ));
            }
            if !w.crons.is_empty() {
                let port = w.ports.first().map(|p| p.port).unwrap_or(8787);
                say(crate::tr!(
                    "{}: 定期実行({})は手元では時刻が来ても動きません。試すには curl \"http://127.0.0.1:{port}/cdn-cgi/local/scheduled?cron=<式>\"",
                    "{}: crons ({}) do not fire locally; trigger one with curl \"http://127.0.0.1:{port}/cdn-cgi/local/scheduled?cron=<expr>\"",
                    s.name,
                    w.crons.join(", ")
                ));
            }
        }
    }
}

/// The data service's config and token file (REQ-017).
fn data_config(ctx: &Ctx, run: &Path, token: &str) -> Result<PathBuf> {
    let dir = run.join(configs::DATA);
    std::fs::create_dir_all(&dir)?;
    let path = write_cfg(&dir, &configs::data_service(&ctx.compose, &dir)?)?;
    write_private(&dir.join(".dev.vars"), &format!("WC_TOKEN=\"{token}\"\n"))?;
    Ok(path)
}

/// 256 random bits for the data service's token (DES-011).
fn new_token() -> Result<String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| anyhow!("random: {e}"))?;
    Ok(crate::files::hex(&b))
}

/// Start one `wrangler dev` with `configs` and wait until it serves.
/// `None` means the user pressed Ctrl+C while it started.
fn spawn_runtime(
    ctx: &Ctx,
    p: &Paths,
    configs: &[PathBuf],
    port: u16,
    log_file: Option<Arc<Mutex<File>>>,
) -> Result<Option<Child>> {
    let mut cmd = ctx.wrangler(["dev"]);
    for c in configs {
        cmd.arg("-c").arg(c);
    }
    cmd.args(["--ip", "127.0.0.1", "--port", &port.to_string()])
        .arg("--persist-to")
        .arg(p.state())
        .env("WRANGLER_SEND_METRICS", "false")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_group(&mut cmd);
    let mut child = cmd.spawn().context(crate::tr!(
        "Wrangler を起動できません",
        "cannot start Wrangler"
    ))?;
    let ready = Arc::new(AtomicBool::new(false));
    let show = log_file.is_some();
    let (tx, rx) = mpsc::channel::<()>();
    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let ready = ready.clone();
        let tx = tx.clone();
        let log_file = log_file.clone();
        std::thread::spawn(move || {
            let mut f = LogFilter::default();
            for line in BufReader::new(stream).lines().map_while(|l| l.ok()) {
                if line.contains("Ready on") && !ready.swap(true, Ordering::SeqCst) {
                    let _ = tx.send(());
                }
                if let Some(out) = f.filter(&line) {
                    // A temporary runtime only shows errors.
                    if show || out.trim_start().starts_with('✘') {
                        eprintln!("{out}");
                    }
                    if let Some(lf) = &log_file
                        && let Ok(mut lf) = lf.lock()
                    {
                        let _ = writeln!(lf, "{out}");
                    }
                }
            }
        });
    }
    drop(tx);
    let started = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(()) => return Ok(Some(child)),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::sleep(Duration::from_millis(200))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if let Some(status) = child.try_wait()? {
            bail!(crate::tr!(
                "Wrangler が止まりました({status})。上の出力を見てください",
                "Wrangler stopped ({status}); see the output above"
            ));
        }
        if crate::interrupt::check().is_err() {
            stop_child(&mut child);
            return Ok(None);
        }
        if started.elapsed() > Duration::from_secs(180) {
            stop_child(&mut child);
            bail!(crate::tr!(
                "Wrangler が 3 分たっても動き出しません",
                "Wrangler did not become ready within 3 minutes"
            ));
        }
    }
}

/// Run `f` against the data service: the running `up`'s, or a temporary
/// runtime with only the gateway and the data service (DES-011).
/// Secrets (.dev.vars) and the data service's token are written under the
/// tool's folder, so it must stay out of git (REQ-007, REQ-015).
fn tool_dir_ignored(ctx: &Ctx) -> Result<()> {
    std::fs::create_dir_all(ctx.tool_dir())?;
    crate::git::require_ignored(
        ctx.root(),
        &ctx.tool_dir(),
        crate::tr!("道具の作業フォルダ", "the tool's folder").as_str(),
    )
}

fn with_data<T>(ctx: &Ctx, f: impl FnOnce(&crate::data::Client, bool) -> Result<T>) -> Result<T> {
    let p = Paths::new(ctx);
    tool_dir_ignored(ctx)?;
    match try_lock(&p)? {
        None => {
            let rec = read_record(&p).ok_or_else(|| {
                anyhow!(crate::tr!(
                    "動いている up が分かりません",
                    "cannot find the running up"
                ))
            })?;
            let port = rec["gateway"].as_u64().unwrap_or(0);
            let token = rec["token"].as_str().unwrap_or_default().to_string();
            f(
                &crate::data::Client::new(format!("http://127.0.0.1:{port}"), token),
                true,
            )
        }
        Some(lock) => {
            let run = p.dir.join("run-data");
            let _ = std::fs::remove_dir_all(&run);
            let port = free_port()?;
            let gw = run.join(configs::GATEWAY);
            std::fs::create_dir_all(&gw)?;
            let token = new_token()?;
            let cfgs = vec![
                write_cfg(&gw, &configs::gateway(&gw, &[], port)?)?,
                data_config(ctx, &run, &token)?,
            ];
            let Some(mut child) = spawn_runtime(ctx, &p, &cfgs, port, None)? else {
                bail!(crate::tr!("中断しました(Ctrl+C)", "interrupted (Ctrl+C)"));
            };
            let r = f(
                &crate::data::Client::new(format!("http://127.0.0.1:{port}"), token),
                false,
            );
            stop_child(&mut child);
            let _ = std::fs::remove_dir_all(&run);
            drop(lock);
            r
        }
    }
}

// ---------------------------------------------------------------- down / ps / logs

pub fn down(ctx: &Ctx, purge: bool, names: &[String]) -> Result<()> {
    let p = Paths::new(ctx);
    if !names.is_empty() {
        ctx.compose.closure(names)?;
    }
    // `--purge <services>` deletes only what those services hold and stops
    // before anything when a service that stays uses it (REQ-011).
    let targets = if purge && !names.is_empty() {
        Some(purge_targets(&ctx.compose, names)?)
    } else {
        None
    };
    if !names.is_empty() {
        // Locally everything runs as one process.
        say(crate::tr!(
            "手元ではすべてのサービスが 1 つにまとまって動くので、まとめて止めます",
            "locally all services run together, so all of them are stopped"
        ));
    }
    if purge {
        let state = p.state();
        match &targets {
            None => say(crate::tr!(
                "手元のデータを消します: {}",
                "this deletes the local data: {}",
                state.display()
            )),
            Some(_) => say(crate::tr!(
                "{} の手元のデータを消します",
                "this deletes the local data of {}",
                names.join(", ")
            )),
        }
        if state.exists() {
            let counts = with_data(ctx, |c, _| match &targets {
                None => crate::data::counts(&ctx.compose, c),
                Some(t) => t
                    .iter()
                    .filter(|(k, _)| *k != "do")
                    .map(|(k, key)| Ok((*k, key.clone(), crate::data::count(c, k, key)?)))
                    .collect(),
            })?;
            for (t, key, n) in counts {
                eprintln!("  {}", crate::data::describe_count(t, &key, n));
            }
            for (_, key) in targets.iter().flatten().filter(|(k, _)| *k == "do") {
                eprintln!(
                    "  {}",
                    crate::tr!("Durable Object {key}", "Durable Object {key}")
                );
            }
        }
        if ctx.dry_run {
            say(crate::tr!(
                "dry-run なので何も変えていません",
                "dry run: nothing was changed"
            ));
            return Ok(());
        }
        crate::confirm::retype_env(
            &ctx.env,
            &crate::tr!("手元のデータを消す", "delete the local data"),
        )?;
    } else if ctx.dry_run {
        say(crate::tr!(
            "動いている up を止めます。dry-run なので何も変えていません",
            "would stop the running up; dry run: nothing was changed"
        ));
        return Ok(());
    }
    stop_running(&p)?;
    if !purge {
        return Ok(());
    }
    match &targets {
        None => {
            let _lock = try_lock(&p)?
                .ok_or_else(|| anyhow!(crate::tr!("up が止まりません", "up did not stop")))?;
            if p.state().exists() {
                std::fs::remove_dir_all(p.state()).with_context(|| {
                    crate::tr!("{} を消せません", "cannot delete {}", p.state().display())
                })?;
            }
        }
        Some(targets) if p.state().exists() => {
            let state = p.state();
            with_data(ctx, |c, _| {
                for (t, key) in targets {
                    if *t == "do" {
                        // Durable Object storage: one folder per class.
                        let dir = state.join("v3").join("do").join(key);
                        if dir.exists() {
                            std::fs::remove_dir_all(&dir).with_context(|| {
                                crate::tr!("{} を消せません", "cannot delete {}", dir.display())
                            })?;
                        }
                    } else {
                        crate::data::empty(c, t, key)?;
                    }
                }
                Ok(())
            })?;
        }
        Some(_) => {}
    }
    say(crate::tr!(
        "手元のデータを消しました",
        "deleted the local data"
    ));
    Ok(())
}

/// The data a service holds or uses: its own D1 databases, R2 buckets and
/// KV namespaces, and for a Worker, what it binds and its Durable Objects'
/// storage (`<worker>-<Class>`).
fn data_of(s: &Service) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    match &s.kind {
        Kind::Worker(w) => {
            for (_, t) in &w.bindings {
                match t {
                    Target::D1(k) => out.push(("d1", k.clone())),
                    Target::R2(k) => out.push(("r2", k.clone())),
                    Target::Kv(k) => out.push(("kv", k.clone())),
                    Target::DurableObject { service, class } => {
                        out.push(("do", format!("{service}-{class}")))
                    }
                    Target::Worker(_) | Target::Queue(_) => {}
                }
            }
            for class in &w.durable_objects {
                out.push(("do", format!("{}-{class}", s.name)));
            }
        }
        Kind::D1(l) => out.extend(l.iter().map(|r| ("d1", r.key.clone()))),
        Kind::R2(l) => out.extend(l.iter().map(|r| ("r2", r.key.clone()))),
        Kind::Kv(l) => out.extend(l.iter().map(|r| ("kv", r.key.clone()))),
        Kind::Queue(_) => {}
    }
    out
}

/// What `down --purge <names>` deletes; an error when a Worker that is not
/// named also uses any of it.
fn purge_targets(
    c: &crate::compose::Compose,
    names: &[String],
) -> Result<Vec<(&'static str, String)>> {
    let mut targets: Vec<(&'static str, String)> = Vec::new();
    for s in c.services.iter().filter(|s| names.contains(&s.name)) {
        for d in data_of(s) {
            if !targets.contains(&d) {
                targets.push(d);
            }
        }
    }
    let mut shared = Vec::new();
    for s in c.services.iter().filter(|s| !names.contains(&s.name)) {
        if !matches!(s.kind, Kind::Worker(_)) {
            continue;
        }
        for d in data_of(s) {
            if targets.contains(&d) {
                shared.push(format!("{} ({})", d.1, s.name));
            }
        }
    }
    if !shared.is_empty() {
        bail!(crate::tr!(
            "{} は残るサービスも使っているので、何も消さずに止めました: {}。一緒に消すなら、そのサービスも名前に加えてください",
            "stopped without deleting anything: {} is also used by services that stay: {}; name those services too to delete it",
            names.join(", "),
            shared.join(", ")
        ));
    }
    Ok(targets)
}

/// Stop a running `up` (from another terminal) and wait until it is gone.
fn stop_running(p: &Paths) -> Result<()> {
    if try_lock(p)?.is_some() {
        say(crate::tr!(
            "--env local up は動いていません",
            "--env local up is not running"
        ));
        return Ok(());
    }
    let pid = read_record(p)
        .and_then(|r| r["pid"].as_u64())
        .ok_or_else(|| {
            anyhow!(crate::tr!(
                "動いている up が分かりません",
                "cannot find the running up"
            ))
        })?;
    terminate(pid as u32);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(250));
        if try_lock(p)?.is_some() {
            say(crate::tr!("止めました", "stopped"));
            return Ok(());
        }
    }
    bail!(crate::tr!(
        "20 秒待っても up が止まりません(pid {pid})",
        "up did not stop within 20 seconds (pid {pid})"
    ))
}

pub fn ps(ctx: &Ctx) -> Result<()> {
    let p = Paths::new(ctx);
    let running = try_lock(&p)?.is_none();
    let rec = if running { read_record(&p) } else { None };
    let url_of = |svc: &str| -> Option<String> {
        rec.as_ref()?["services"]
            .as_array()?
            .iter()
            .find(|s| s["service"] == svc)?["url"]
            .as_str()
            .map(str::to_string)
    };
    // Pending migrations can be counted only while up runs (through the
    // data service; nothing is started just to look).
    let mut pending: BTreeMap<String, usize> = BTreeMap::new();
    if running && let Some(rec) = &rec {
        let client = crate::data::Client::new(
            format!("http://127.0.0.1:{}", rec["gateway"].as_u64().unwrap_or(0)),
            rec["token"].as_str().unwrap_or_default().to_string(),
        );
        pending = crate::data::pending_migrations(ctx, &client).unwrap_or_default();
    }
    println!(
        "{:<16} {:<8} {:<10} URL / RESOURCES",
        "SERVICE", "TYPE", "STATE"
    );
    for s in &ctx.compose.services {
        match &s.kind {
            Kind::Worker(_) => {
                let state = if running {
                    crate::tr!("動作中", "running")
                } else {
                    crate::tr!("停止", "stopped")
                };
                println!(
                    "{:<16} {:<8} {:<10} {}",
                    s.name,
                    "worker",
                    state,
                    url_of(&s.name).unwrap_or_else(|| "-".into())
                );
            }
            Kind::D1(list) => {
                let parts: Vec<String> = list
                    .iter()
                    .map(|r| match pending.get(&r.key) {
                        Some(n) if *n > 0 => crate::tr!(
                            "{} (未適用の移行 {n})",
                            "{} ({n} pending migrations)",
                            r.key
                        ),
                        _ => r.key.clone(),
                    })
                    .collect();
                println!(
                    "{:<16} {:<8} {:<10} {}",
                    s.name,
                    "d1",
                    "-",
                    parts.join(", ")
                );
            }
            other => {
                let mut keys = crate::compose::resource_keys(s).join(", ");
                if let Some(port) = s.ports.first() {
                    keys = format!("{keys}  http://{}:{}", url_host(&port.ip), port.port);
                }
                println!(
                    "{:<16} {:<8} {:<10} {}",
                    s.name,
                    other.type_name(),
                    "-",
                    keys
                );
            }
        }
    }
    if running {
        for tool in [configs::GATEWAY, configs::DATA] {
            println!(
                "{:<16} {:<8} {:<10} -",
                tool,
                crate::tr!("道具", "tool"),
                crate::tr!("動作中", "running")
            );
        }
    }
    Ok(())
}

pub fn logs(ctx: &Ctx, follow: bool, names: &[String]) -> Result<()> {
    for n in names {
        if ctx.compose.service(n).is_none() {
            bail!(crate::tr!(
                "サービス「{n}」はありません",
                "there is no service \"{n}\""
            ));
        }
    }
    let path = Paths::new(ctx).log();
    let wanted = |line: &str| {
        names.is_empty()
            || names
                .iter()
                .any(|n| line.starts_with(&format!("{n} ")) && line.contains(" | "))
    };
    let mut pos = 0u64;
    loop {
        if let Ok(mut f) = File::open(&path) {
            use std::io::{Seek, SeekFrom};
            f.seek(SeekFrom::Start(pos))?;
            let mut buf = String::new();
            f.read_to_string(&mut buf)?;
            pos += buf.len() as u64;
            for line in buf.lines() {
                if wanted(line) {
                    println!("{line}");
                }
            }
        } else if !follow {
            say(crate::tr!(
                "まだログがありません(--env local up を動かすと書かれます)",
                "no logs yet (they are written while --env local up runs)"
            ));
        }
        if !follow {
            return Ok(());
        }
        if crate::interrupt::check().is_err() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

pub fn backup(ctx: &Ctx, dest: Option<PathBuf>) -> Result<()> {
    with_data(ctx, |c, _| crate::data::backup(ctx, c, dest).map(|_| ()))
}

pub fn restore(ctx: &Ctx, from: &Path, drop: bool) -> Result<()> {
    let plan = crate::data::plan_restore(&ctx.compose, from)?;
    say(crate::tr!(
        "{} の控え(環境 {})を {} へ戻します",
        "restoring the backup {} (taken from environment {}) into {}",
        from.display(),
        plan.from_env,
        ctx.env
    ));
    with_data(ctx, |c, running| {
        let full = crate::data::non_empty(c, &plan)?;
        if !full.is_empty() && !drop {
            bail!(crate::tr!(
                "戻す先が空ではありません(何もしていません):\n  {}\nいまの中身を消して入れ替えるには --drop を付けてください",
                "the destination is not empty (nothing was done):\n  {}\nuse --drop to replace the current contents",
                full.join("\n  ")
            ));
        }
        if running {
            say(crate::tr!(
                "up が動いているので、戻している間は Worker から欠けたデータが見えることがあります",
                "up is running, so Workers may see partial data while this runs"
            ));
        }
        if ctx.dry_run {
            if drop && !full.is_empty() {
                say(crate::tr!("消す: {}", "would delete: {}", full.join(", ")));
            }
            say(crate::tr!(
                "dry-run なので何も変えていません",
                "dry run: nothing was changed"
            ));
            return Ok(());
        }
        if drop && !full.is_empty() {
            say(crate::tr!(
                "先に消すもの: {}",
                "to be deleted first: {}",
                full.join(", ")
            ));
            crate::confirm::retype_env(
                &ctx.env,
                &crate::tr!(
                    "いまの中身を消して控えに入れ替える",
                    "replace the current contents with the backup"
                ),
            )?;
            crate::data::drop_targets(c, &plan)?;
        }
        crate::data::load(c, &plan, from)?;
        crate::data::apply_migrations(ctx, c)?;
        say(crate::tr!("戻しました", "restored"));
        Ok(())
    })
}

// ---------------------------------------------------------------- helpers

/// Turns Wrangler's output into what the user sees: Wrangler's own lines,
/// and each Worker's logs from its receiver prefixed with the service.
/// Raw console lines (which carry no service) are dropped, since the
/// receivers report them.
#[derive(Default)]
struct LogFilter {
    in_bindings: bool,
    in_error: bool,
    /// A "Processing <config> configuration:" warning being collected; it is
    /// shown only if it says more than the tool's own S3 setting notice.
    config_warning: Option<Vec<String>>,
}

impl LogFilter {
    fn filter(&mut self, raw: &str) -> Option<String> {
        let line = strip_ansi(raw);
        if let Some((_, json)) = line.split_once("@@WC@@") {
            let v: Value = serde_json::from_str(json).ok()?;
            let svc = v["s"].as_str().unwrap_or("?");
            let msg = v["m"].as_str().unwrap_or("");
            let mut out = format!("{svc:<12} | {msg}");
            if v["t"] == "exc" {
                out = format!("{svc:<12} | ✘ {msg}");
                if let Some(st) = v["st"].as_str().filter(|s| !s.is_empty()) {
                    for l in st.lines().skip(1) {
                        out.push_str(&format!("\n{svc:<12} |   {}", l.trim()));
                    }
                }
            }
            return Some(out);
        }
        let t = line.trim_start();
        if let Some(block) = &mut self.config_warning {
            if t.is_empty() && block.len() > 1 {
                let block = self.config_warning.take().unwrap_or_default();
                let interesting = block
                    .iter()
                    .skip(1)
                    .any(|l| !l.contains("experimental_s3_credentials"));
                return interesting.then(|| block.join("\n"));
            }
            if !t.is_empty() {
                block.push(line);
            }
            return None;
        }
        if t.starts_with('▲') && t.contains("Processing ") && t.contains(" configuration:") {
            self.config_warning = Some(vec![line]);
            return None;
        }
        if line.contains("has access to the following bindings") || t.starts_with("Binding ") {
            self.in_bindings = true;
            return None;
        }
        if self.in_bindings {
            if t.is_empty() {
                self.in_bindings = false;
            }
            return None;
        }
        if t.starts_with("[wrangler:info] ") || t.starts_with("[wrangler:inf]") {
            return None; // requests are reported per service by the receivers
        }
        if t.starts_with('✘') {
            self.in_error = true;
            return Some(line);
        }
        if self.in_error {
            if t.is_empty() {
                self.in_error = false;
                return None;
            }
            return Some(line);
        }
        let system = [
            "⎔",
            "▲",
            "[wrangler:",
            "⛅️",
            "Ready on",
            "Your worker",
            "Total Upload",
        ];
        if system.iter().any(|p| t.starts_with(p)) {
            return Some(line);
        }
        None
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for d in chars.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn write_cfg(dir: &Path, cfg: &Value) -> Result<PathBuf> {
    let path = dir.join("wrangler.json");
    std::fs::write(&path, serde_json::to_vec_pretty(cfg)?)
        .with_context(|| crate::tr!("{} を書けません", "cannot write {}", path.display()))?;
    Ok(path)
}

/// A file only the user can read (secrets).
fn write_private(path: &Path, text: &str) -> Result<()> {
    let mut opts = OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(text.as_bytes())?;
    Ok(())
}

fn free_port() -> Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0")?;
    Ok(l.local_addr()?.port())
}

fn bracket(ip: &str) -> String {
    if ip.contains(':') {
        format!("[{ip}]")
    } else {
        ip.to_string()
    }
}

fn url_host(ip: &str) -> String {
    match ip {
        "0.0.0.0" | "::" => "localhost".into(),
        other => bracket(other),
    }
}

fn stamps(files: &[PathBuf]) -> Vec<Option<SystemTime>> {
    files
        .iter()
        .map(|f| std::fs::metadata(f).and_then(|m| m.modified()).ok())
        .collect()
}

/// Copy bytes both ways between each accepted connection and the gateway.
/// Nothing is parsed, so WebSockets and streams pass through unchanged.
#[cfg(unix)]
fn own_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

#[cfg(not(unix))]
fn own_group(_cmd: &mut Command) {}

/// Stop Wrangler and everything under it, and wait until it is gone.
fn stop_child(child: &mut Child) {
    let pid = child.id();
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-s", "TERM", "--", &format!("-{pid}")])
            .status();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    #[cfg(unix)]
    {
        // Anything left in the group (workerd) gets a hard stop.
        let _ = Command::new("kill")
            .args(["-s", "KILL", "--", &format!("-{pid}")])
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Ask another `up` (by pid) to stop; it then stops its own children.
fn terminate(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-s", "TERM", "--", &pid.to_string()])
            .status();
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_receiver_lines_and_drops_raw_console_lines() {
        let mut f = LogFilter::default();
        assert_eq!(
            f.filter(r#"[wc-logs-api] @@WC@@{"s":"api","t":"log","l":"log","m":"hello 1"}"#)
                .unwrap(),
            "api          | hello 1"
        );
        assert!(f.filter("hello 1").is_none());
        assert!(f.filter("[wrangler:info] GET / 200 OK (3ms)").is_none());
        assert!(
            f.filter("\u{1b}[32m⎔ Reloading local server...\u{1b}[0m")
                .unwrap()
                .starts_with('⎔')
        );
        assert!(
            f.filter("api has access to the following bindings:")
                .is_none()
        );
        assert!(f.filter("env.DB (db)   D1 Database   local").is_none());
        assert!(f.filter("").is_none());
        assert!(f.filter("✘ [ERROR] boom").is_some());
        assert!(f.filter("    at worker.js:1").is_some());
        assert!(f.filter("").is_none());
        // The tool's own S3 setting notice is not shown; anything else is.
        assert!(
            f.filter("▲ [WARNING] Processing run/api/wrangler.json configuration:")
                .is_none()
        );
        assert!(f.filter("").is_none());
        assert!(
            f.filter("    - \"local_dev.experimental_s3_credentials\" fields are experimental")
                .is_none()
        );
        assert!(f.filter("").is_none());
        assert!(
            f.filter("▲ [WARNING] Processing run/web/wrangler.json configuration:")
                .is_none()
        );
        assert!(f.filter("    - something else").is_none());
        assert!(f.filter("").unwrap().contains("something else"));
    }
}
