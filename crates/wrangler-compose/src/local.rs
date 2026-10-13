//! The local environment (REQ-009, REQ-011, REQ-012, DES-008): every service
//! runs on this machine, in one runtime, without touching Cloudflare.

use crate::build::{self, Built};
use crate::compose::{Kind, Port, Service, Target};
use crate::configs;
use crate::ctx::Ctx;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
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
    /// Wrangler's dev registry (where `wrangler dev` sessions find each
    /// other's Workers by name). The default is one folder for the whole
    /// machine, so two projects with a Worker of the same name would
    /// overwrite each other's entries; each project keeps its own.
    fn registry(&self) -> PathBuf {
        self.dir.join("registry")
    }
    fn log(&self) -> PathBuf {
        self.dir.join("logs").join("up.log")
    }
    /// The images `wrangler dev` built for this project's containers, for
    /// `down --purge` (DES-020).
    fn container_images(&self) -> PathBuf {
        self.dir.join("container-images.json")
    }
    /// Workers taken down with `down <service>` while `up` runs.
    fn stopped(&self) -> PathBuf {
        self.dir.join("stopped.json")
    }
}

fn read_stopped(p: &Paths) -> Vec<String> {
    std::fs::read_to_string(p.stopped())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_stopped(p: &Paths, names: &[String]) -> Result<()> {
    if names.is_empty() {
        let _ = std::fs::remove_file(p.stopped());
        return Ok(());
    }
    std::fs::write(p.stopped(), serde_json::to_vec(names)?)?;
    Ok(())
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

/// Wait until the running `up` has restarted with the Workers `ok` wants
/// (or has stopped, when no Worker is left).
fn wait_for_record(p: &Paths, ok: impl Fn(&[String]) -> bool) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(300) {
        std::thread::sleep(Duration::from_millis(250));
        if try_lock(p)?.is_some() {
            return Ok(());
        }
        if let Some(r) = read_record(p)
            && r["gateway"].as_u64().unwrap_or(0) != 0
        {
            let running: Vec<String> = r["services"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s["service"].as_str().map(str::to_string))
                .collect();
            if ok(&running) {
                return Ok(());
            }
        }
    }
    bail!(crate::tr!(
        "動いている up が 5 分たっても動き直しません。その端末の出力を見てください",
        "the running up did not restart within 5 minutes; see its terminal"
    ))
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
    save_s3_keys(ctx)?;
    let Some(_lock) = try_lock(&p)? else {
        // Services taken down with `down <service>` come back into the
        // running `up` (REQ-011); otherwise a second `up` stops (REQ-009).
        let stopped = read_stopped(&p);
        let back: Vec<String> = if names.is_empty() {
            stopped.clone()
        } else {
            let wanted: Vec<String> = ctx
                .compose
                .closure(names)?
                .iter()
                .map(|s| s.name.clone())
                .collect();
            stopped
                .iter()
                .filter(|s| wanted.contains(s))
                .cloned()
                .collect()
        };
        if !back.is_empty() {
            let rest: Vec<String> = stopped.into_iter().filter(|s| !back.contains(s)).collect();
            write_stopped(&p, &rest)?;
            wait_for_record(&p, |running| back.iter().all(|b| running.contains(b)))?;
            say(crate::tr!(
                "動いている up に {} を戻しました",
                "{} is back in the running up",
                back.join(", ")
            ));
            return Ok(());
        }
        let urls = read_record(&p).map(|r| urls_text(&r)).unwrap_or_default();
        bail!(crate::tr!(
            "このプロジェクトでは、ほかの端末で --env local up が動いています{urls}。止めるには --env local down を打ってください",
            "--env local up is already running for this project{urls}; stop it with --env local down"
        ));
    };
    // A restart re-reads the compose file in this process, keeping the lock.
    let mut reopened: Option<Ctx> = None;
    let mut rebuild = rebuild;
    write_stopped(&p, &[])?;
    loop {
        let c = reopened.as_ref().unwrap_or(ctx);
        let stopped = read_stopped(&p);
        let services: Vec<&Service> = c
            .compose
            .closure(names)?
            .into_iter()
            .filter(|s| !stopped.contains(&s.name))
            .collect();
        if !services.iter().any(|s| matches!(s.kind, Kind::Worker(_))) {
            let _ = std::fs::remove_file(p.record());
            write_stopped(&p, &[])?;
            say(crate::tr!(
                "動かす Worker が無くなったので止めました",
                "no Worker is left to run, so up stopped"
            ));
            return Ok(());
        }
        notices(&services);
        // Who holds the lock is known from the start, so `down` can stop an
        // `up` that is still building or applying migrations.
        write_private(
            &p.record(),
            &serde_json::to_string_pretty(&record(std::process::id(), 0, "", &[]))?,
        )?;
        let next = match run_once(c, &p, &services, rebuild)? {
            Stop::Done => {
                write_stopped(&p, &[])?;
                return Ok(());
            }
            Stop::Restart => Ctx::open(&ctx.env, false)?,
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
    // Containers need Docker before anything starts (REQ-020).
    let containers: Vec<crate::compose::Container> = workers
        .iter()
        .flat_map(|(_, w)| w.containers.iter().cloned())
        .collect();
    let prefix = configs::local_prefix(&ctx.compose);
    if !containers.is_empty() {
        crate::containers::check_docker(true)?;
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
            let l = claim_port(&s.name, port)?;
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
        let l = claim_port(&s.name, port)?;
        // A public bucket's own port, on the same address (REQ-009).
        let mut own = Vec::new();
        for b in buckets {
            if let Some(pp) = b.spec.public_port {
                let at = Port { ip: port.ip.clone(), port: pp };
                own.push((b.key.clone(), at.clone(), claim_port(&s.name, &at)?));
            }
        }
        let sites: Vec<crate::r2proxy::Site> = buckets
            .iter()
            .map(|b| crate::r2proxy::Site {
                key: b.key.clone(),
                public: b.spec.public,
                cors: b.spec.cors.clone(),
            })
            .collect();
        r2_sites.push((s.name.clone(), port.clone(), l, sites, own));
    }
    let gateway_port = free_port()?;

    // 3. Configs (DES-005): gateway first, then the Workers and their log
    //    receivers.
    let run = p.run();
    let _ = std::fs::remove_dir_all(&run);
    let mut configs = Vec::new();
    let gw_dir = run.join(configs::GATEWAY);
    std::fs::create_dir_all(&gw_dir)?;
    let gw_map: Vec<(u16, String)> = port_map
        .iter()
        .map(|(port, svc)| (*port, configs::local_name(&ctx.compose, svc)))
        .collect();
    configs.push(write_cfg(
        &gw_dir,
        &configs::gateway(&gw_dir, &gw_map, gateway_port)?,
    )?);
    let token = new_token()?;
    configs.push(data_config(ctx, &run, &token)?);
    for (s, w) in &workers {
        let dir = run.join(&s.name);
        std::fs::create_dir_all(&dir)?;
        let cfg = configs::local_worker(&ctx.compose, &s.name, w, &builts[&s.name]);
        configs.push(write_cfg(&dir, &cfg)?);
        let vars = configs::dev_vars(w);
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

    // Durable Object storage kept under the Worker names of earlier
    // versions moves to the local names (DES-020); containers a run left
    // behind go away first.
    let moves: Vec<(String, String)> = workers
        .iter()
        .flat_map(|(s, w)| {
            let local = configs::local_name(&ctx.compose, &s.name);
            w.durable_objects
                .iter()
                .map(move |c| (format!("{}-{c}", s.name), format!("{local}-{c}")))
        })
        .collect();
    crate::containers::move_do_state(&p.state(), &moves)?;
    if !containers.is_empty() {
        crate::containers::clean_up(&prefix, &p.container_images());
    }

    // 5. One `wrangler dev` for everything: one runtime, so Workers sharing
    //    a resource never corrupt it (DES facts).
    std::fs::create_dir_all(p.log().parent().unwrap())?;
    let log_file = Arc::new(Mutex::new(
        OpenOptions::new().create(true).append(true).open(p.log())?,
    ));
    let registry = containers
        .iter()
        .any(|k| matches!(k.image, crate::compose::Image::Registry(_)));
    let aliases_before = if registry {
        crate::containers::alias_listing()
    } else {
        String::new()
    };
    let Some(mut child) = spawn_runtime(ctx, p, &configs, gateway_port, Some(log_file))? else {
        return Ok(Stop::Done);
    };
    if registry {
        let made = crate::containers::new_aliases(
            &aliases_before,
            &crate::containers::alias_listing(),
            &crate::containers::registry_ids(&containers),
        );
        crate::containers::remember(&p.container_images(), &made)?;
    }

    // 6. Forward every declared port to the gateway, then say where things
    //    are. The listeners close when this run ends, so a restart can bind
    //    the same ports again.
    let stop = Arc::new(AtomicBool::new(false));
    let mut accepters = Vec::new();
    let guarded: Arc<Vec<String>> = Arc::new(
        r2_sites
            .iter()
            .flat_map(|(_, _, _, sites, _)| sites.iter().map(|x| x.key.clone()))
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
    for (svc, port, l, sites, own) in r2_sites {
        let base = format!("http://{}:{}", url_host(&port.ip), port.port);
        let mut public: Vec<String> = sites
            .iter()
            .filter(|x| x.public)
            .map(|x| format!("http://{}.localhost:{}/", x.key, port.port))
            .collect();
        public.extend(
            own.iter()
                .map(|(_, at, _)| format!("http://{}:{}/", url_host(&at.ip), at.port)),
        );
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
            s3: ctx
                .compose
                .local_s3
                .clone()
                .expect("a local compose file has local S3 keys"),
        });
        for (key, _, pl) in own {
            let (shared, stop) = (shared.clone(), stop.clone());
            accepters.push(std::thread::spawn(move || {
                crate::r2proxy::serve_public(pl, shared, key, stop)
            }));
        }
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
    let stopped_then = read_stopped(p);
    let mut seen: BTreeMap<String, Option<SystemTime>> = workers
        .iter()
        .map(|(s, w)| (s.name.clone(), build::newest_change(ctx, &s.name, w)))
        .collect();
    let has_notify = ctx.compose.services.iter().any(|s| {
        matches!(&s.kind, Kind::R2(list) if list.iter().any(|r| !r.spec.notifications.is_empty()))
    });
    let notifier = has_notify.then(|| {
        crate::data::Client::new(format!("http://127.0.0.1:{gateway_port}"), token.clone())
    });
    let images_from = crate::containers::inputs_stamp(&containers);
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
        if let Some(status) = child.0.try_wait()? {
            let _ = std::fs::remove_file(p.record());
            bail!(crate::tr!(
                "Wrangler が止まりました({status})",
                "Wrangler stopped ({status})"
            ));
        }
        if read_stopped(p) != stopped_then {
            say(crate::tr!(
                "止めるサービスが変わったので、動かし直します",
                "the services to run changed; restarting"
            ));
            break Stop::Restart;
        }
        let now_stamp = stamps(&compose_files);
        if now_stamp != compose_stamp {
            // Restart only into a compose file that loads; a half-edited one
            // keeps the running services as they are.
            compose_stamp = now_stamp;
            match crate::compose::load(&ctx.compose.file, &ctx.env) {
                Ok(_) => {
                    say(crate::tr!(
                        "compose ファイルか .env.local が変わったので、動かし直します",
                        "the compose file or .env.local changed; restarting"
                    ));
                    break Stop::Restart;
                }
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
        // Wrangler builds images only when it starts (DES-020).
        let now_images = crate::containers::inputs_stamp(&containers);
        if now_images != images_from {
            say(crate::tr!(
                "コンテナのイメージの元が変わったので、作り直して動かし直します",
                "what a container image is built from changed; rebuilding and restarting"
            ));
            break Stop::Restart;
        }
        let mut restart = false;
        let mut rebuilt = false;
        for (s, w) in &workers {
            let now = build::newest_change(ctx, &s.name, w);
            if seen.get(&s.name) != Some(&now) {
                seen.insert(s.name.clone(), now);
                match build::build_one(ctx, &s.name, w, false) {
                    // A new entry file name (main changed) needs new configs.
                    Ok(b) if Some(&b.entry) != builts.get(&s.name).map(|x| &x.entry) => {
                        say(crate::tr!(
                            "{}: 入口のファイルが変わったので、動かし直します",
                            "{}: the entry file changed; restarting",
                            s.name
                        ));
                        restart = true;
                    }
                    Ok(b) if Some(&b.hash) != builts.get(&s.name).map(|x| &x.hash) => {
                        // `wrangler dev` does not pick up a new bundle of a
                        // Worker that is not its first config (measured with
                        // 4.149.0), so the runtime starts again with it.
                        say(crate::tr!(
                            "{}: 組み立て直したので、動かし直します",
                            "{}: rebuilt; restarting",
                            s.name
                        ));
                        builts.insert(s.name.clone(), b);
                        rebuilt = true;
                    }
                    Ok(_) => {}
                    Err(e) => say(format!("{e:#}")),
                }
            }
        }
        if restart || rebuilt {
            break Stop::Restart;
        }
    };
    drop(child);
    if !containers.is_empty() {
        crate::containers::clean_up(&prefix, &p.container_images());
    }
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
                "  {svc:<16} R2 の S3 互換の口: {base}/cdn-cgi/local/r2/s3(パス形式・地域 auto。鍵は Worker の secrets に ${{{key}}}・${{{secret}}} で渡す。値は {})\n  {:<16} バケット: {}",
                "  {svc:<16} R2 S3-compatible API: {base}/cdn-cgi/local/r2/s3 (path style, region auto; pass the keys to a Worker's secrets as ${{{key}}} and ${{{secret}}}; the values are in {})\n  {:<16} buckets: {}",
                crate::compose::s3_keys_file(ctx.root()).display(),
                "",
                buckets.join(", "),
                key = format!(
                    "WC_R2_{}_S3_KEY",
                    svc.to_ascii_uppercase().replace('-', "_")
                ),
                secret = format!(
                    "WC_R2_{}_S3_SECRET",
                    svc.to_ascii_uppercase().replace('-', "_")
                )
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
    if without.is_empty() {
        return;
    }
    match ports.first() {
        Some((port, _)) => say(crate::tr!(
            "{} には ports が無いので、S3 互換の口は http://127.0.0.1:{port}/cdn-cgi/local/r2/s3 で、CORS はバケットの設定によらずすべて許され、公開の URL もありません。本番と同じにするには ports を書いてください",
            "{} has no ports: its S3-compatible API is at http://127.0.0.1:{port}/cdn-cgi/local/r2/s3, allows every CORS origin regardless of the bucket's rules, and has no public URLs. Add ports to behave like production",
            without.join(", ")
        )),
        // No Worker port either: nothing outside reaches the bucket at all.
        None => say(crate::tr!(
            "{} には ports が無いので、手元では S3 互換の API にも公開の URL にも外から届きません(Worker のバインディングからだけ使えます)。本番と同じにするには ports を書いてください",
            "{} has no ports: locally neither its S3-compatible API nor its public URLs can be reached from outside (only a Worker's binding uses it). Add ports to behave like production",
            without.join(", ")
        )),
    }
}

/// What `--dry-run up` shows (REQ-014): nothing is built, written or run.
fn dry_run(ctx: &Ctx, services: &[&Service]) -> Result<()> {
    // What a real `up` would stop on, found without changing anything.
    let mut problems = Vec::new();
    if try_lock(&Paths::new(ctx))?.is_none() {
        problems.push(crate::tr!(
            "--env local up がもう動いています",
            "--env local up is already running"
        ));
    }
    for s in services {
        let ports = match &s.kind {
            Kind::Worker(w) => &w.ports,
            _ => &s.ports,
        };
        for port in ports {
            if let Err(e) = claim_port(&s.name, port) {
                problems.push(format!("{e:#}"));
            }
        }
    }
    for s in services {
        match &s.kind {
            Kind::Worker(w) => {
                // Checks the Worker folder (REQ-007) and says whether it
                // would be built.
                if let Err(e) = build::build_one(ctx, &s.name, w, true) {
                    problems.push(format!("{e:#}"));
                }
                for (k, v) in &w.vars {
                    let v = v
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string());
                    say(format!("{}: environment.{k} = {v}", s.name));
                }
                let port = w
                    .ports
                    .first()
                    .map(|p| format!("http://{}:{}", url_host(&p.ip), p.port))
                    .unwrap_or_else(|| "-".into());
                let secrets: Vec<&str> = w.secrets.keys().map(String::as_str).collect();
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
    if !problems.is_empty() {
        bail!(
            "{}\n{}",
            crate::tr!(
                "本当に打つと、次の理由で止まります:",
                "a real run would stop because:"
            ),
            problems
                .iter()
                .map(|p| format!("  - {p}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
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
            for k in &w.containers {
                let mut set = Vec::new();
                if k.max_instances.is_some() {
                    set.push("max_instances");
                }
                if k.instance_type.is_some() {
                    set.push("instance_type");
                }
                if !set.is_empty() {
                    say(crate::tr!(
                        "{}: コンテナ {} の {} は手元では効きません(数も大きさも本物の上限で止まりません)",
                        "{}: container {}: {} do not apply locally (no real limits on count or size)",
                        s.name,
                        k.class,
                        set.join(", ")
                    ));
                }
            }
            if !w.containers.is_empty() && cfg!(all(target_os = "macos", target_arch = "aarch64")) {
                say(crate::tr!(
                    "{}: コンテナは linux/amd64 で作り、この Mac ではまねて動かすので遅くなります",
                    "{}: containers are built for linux/amd64 and emulated on this Mac, so they run slower",
                    s.name
                ));
            }
            if !w.crons.is_empty() {
                let port = w.ports.first().map(|p| p.port).unwrap_or(8787);
                say(crate::tr!(
                    "{}: 定期実行({})は手元では時刻が来ても動きません。試すには curl \"http://127.0.0.1:{port}/cdn-cgi/handler/scheduled?cron=<式>\"",
                    "{}: crons ({}) do not fire locally; trigger one with curl \"http://127.0.0.1:{port}/cdn-cgi/handler/scheduled?cron=<expr>\"",
                    s.name,
                    w.crons.join(", ")
                ));
            }
        }
        if let Kind::R2(buckets) = &s.kind {
            for b in buckets {
                let mut set = Vec::new();
                if !b.spec.lifecycle.is_empty() {
                    set.push("lifecycle");
                }
                if !b.spec.lock.is_empty() {
                    set.push("lock");
                }
                if b.spec.storage_class.is_some() {
                    set.push("storage_class");
                }
                if b.spec.jurisdiction.is_some() {
                    set.push("jurisdiction");
                }
                if !set.is_empty() {
                    say(crate::tr!(
                        "{}: {} は手元では効きません(リモートの環境でだけ設定されます)",
                        "{}: {} do not apply locally (they are set in remote environments only)",
                        b.key,
                        set.join(", ")
                    ));
                }
            }
        }
    }
}

/// The data service's config and token file (REQ-017).
fn data_config(ctx: &Ctx, run: &Path, token: &str) -> Result<PathBuf> {
    let dir = run.join(configs::DATA);
    std::fs::create_dir_all(&dir)?;
    let path = write_cfg(&dir, &configs::data_service(&ctx.compose, &dir, None)?)?;
    write_private(&dir.join(".dev.vars"), &format!("WC_TOKEN=\"{token}\"\n"))?;
    Ok(path)
}

/// 256 random bits for the data service's token (DES-011).
pub(crate) fn new_token() -> Result<String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| anyhow!("random: {e}"))?;
    Ok(crate::files::hex(&b))
}

/// Start one `wrangler dev` with `configs` and wait until it serves.
/// `None` means the user pressed Ctrl+C while it started.
///
/// Wrangler picks its devtools port itself (9229, else the next free one)
/// before workerd binds it, so two `up`s starting together can pick the
/// same one and one runtime dies. Each start asks for a port the system
/// chose instead, and a start whose port was taken in the meantime is
/// tried again.
fn spawn_runtime(
    ctx: &Ctx,
    p: &Paths,
    configs: &[PathBuf],
    port: u16,
    log_file: Option<Arc<Mutex<File>>>,
) -> Result<Option<Runtime>> {
    const TRIES: u32 = 3;
    for attempt in 1.. {
        match start_runtime(ctx, p, configs, port, free_port()?, log_file.clone())? {
            Start::Ready(r) => return Ok(Some(r)),
            Start::Interrupted => return Ok(None),
            Start::PortTaken(_) if attempt < TRIES => say(crate::tr!(
                "Wrangler の使うポートがほかのものと重なったので、起動し直します",
                "a port Wrangler uses was taken by something else; starting it again"
            )),
            Start::PortTaken(status) => bail!(crate::tr!(
                "Wrangler の使うポートが {TRIES} 回ともほかのものと重なって止まりました({status})。少し待ってから、もう一度打ってください",
                "Wrangler stopped because a port it uses was taken, {TRIES} times in a row ({status}); wait a moment and try again"
            )),
        }
    }
    unreachable!()
}

enum Start {
    Ready(Runtime),
    Interrupted,
    /// Wrangler stopped while starting because a port was already in use.
    PortTaken(std::process::ExitStatus),
}

fn start_runtime(
    ctx: &Ctx,
    p: &Paths,
    configs: &[PathBuf],
    port: u16,
    inspector_port: u16,
    log_file: Option<Arc<Mutex<File>>>,
) -> Result<Start> {
    let mut cmd = ctx.wrangler(["dev"]);
    for c in configs {
        cmd.arg("-c").arg(c);
    }
    cmd.args(["--ip", "127.0.0.1", "--port", &port.to_string()])
        .args(["--inspector-port", &inspector_port.to_string()])
        .arg("--persist-to")
        .arg(p.state())
        .env("WRANGLER_REGISTRY_PATH", p.registry())
        .env("WRANGLER_SEND_METRICS", "false")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_group(&mut cmd);
    let mut child = Runtime(cmd.spawn().context(crate::tr!(
        "Wrangler を起動できません",
        "cannot start Wrangler"
    ))?);
    let ready = Arc::new(AtomicBool::new(false));
    let in_use = Arc::new(AtomicBool::new(false));
    let show = log_file.is_some();
    let (tx, rx) = mpsc::channel::<()>();
    for stream in [
        child
            .0
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        child
            .0
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let (ready, in_use) = (ready.clone(), in_use.clone());
        let tx = tx.clone();
        let log_file = log_file.clone();
        let images = p.container_images();
        std::thread::spawn(move || {
            let mut f = LogFilter::default();
            for line in BufReader::new(stream).lines().map_while(|l| l.ok()) {
                if let Some(tag) = crate::containers::image_tag_in(&line) {
                    let _ = crate::containers::remember(&images, &[tag]);
                }
                if line.contains("Ready on") && !ready.swap(true, Ordering::SeqCst) {
                    let _ = tx.send(());
                }
                if line.contains("Address already in use") {
                    in_use.store(true, Ordering::SeqCst);
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
            Ok(()) => return Ok(Start::Ready(child)),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::sleep(Duration::from_millis(200))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if let Some(status) = child.0.try_wait()? {
            if in_use.load(Ordering::SeqCst) {
                return Ok(Start::PortTaken(status));
            }
            bail!(crate::tr!(
                "Wrangler が止まりました({status})。上の出力を見てください",
                "Wrangler stopped ({status}); see the output above"
            ));
        }
        if crate::interrupt::check().is_err() {
            return Ok(Start::Interrupted);
        }
        if started.elapsed() > Duration::from_secs(180) {
            bail!(crate::tr!(
                "Wrangler が 3 分たっても動き出しません",
                "Wrangler did not become ready within 3 minutes"
            ));
        }
    }
}

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

/// Keep the local S3 keys this load made (REQ-009), so later loads and
/// other tools see the same ones. Only the user can read the file.
fn save_s3_keys(ctx: &Ctx) -> Result<()> {
    let Some(k) = ctx.compose.local_s3.as_ref().filter(|k| k.fresh) else {
        return Ok(());
    };
    let file = crate::compose::s3_keys_file(ctx.root());
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_private(
        &file,
        &serde_json::to_string_pretty(&serde_json::json!({ "key": k.key, "secret": k.secret }))?,
    )
}

/// Run `f` against the data service: the running `up`'s, or a temporary
/// runtime with only the gateway and the data service (DES-011).
fn with_data<T>(ctx: &Ctx, f: impl FnOnce(&crate::data::Client, bool) -> Result<T>) -> Result<T> {
    let p = Paths::new(ctx);
    tool_dir_ignored(ctx)?;
    if !ctx.local {
        // A remote environment's own data service, on Cloudflare.
        return f(&crate::remote::data_client(ctx)?, false);
    }
    match try_lock(&p)? {
        None => {
            let rec = read_record(&p).ok_or_else(|| {
                anyhow!(crate::tr!(
                    "動いている up が分かりません",
                    "cannot find the running up"
                ))
            })?;
            let port = rec["gateway"].as_u64().unwrap_or(0);
            if port == 0 {
                bail!(crate::tr!(
                    "--env local up がまだ動き出していません。動き出してから、もう一度打ってください",
                    "--env local up is still starting; try again once it is running"
                ));
            }
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
            let Some(child) = spawn_runtime(ctx, &p, &cfgs, port, None)? else {
                bail!(crate::tr!("中断しました(Ctrl+C)", "interrupted (Ctrl+C)"));
            };
            let r = f(
                &crate::data::Client::new(format!("http://127.0.0.1:{port}"), token),
                false,
            );
            drop(child);
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
        if names.is_empty() {
            say(crate::tr!(
                "動いている up を止めます。dry-run なので何も変えていません",
                "would stop the running up; dry run: nothing was changed"
            ));
        } else {
            say(crate::tr!(
                "動いている up から {} を下ろします。dry-run なので何も変えていません",
                "would take {} out of the running up; dry run: nothing was changed",
                names.join(", ")
            ));
        }
        return Ok(());
    }
    if names.is_empty() {
        stop_running(&p)?;
        if has_containers(ctx) {
            crate::containers::clean_up(
                &configs::local_prefix(&ctx.compose),
                &p.container_images(),
            );
        }
    } else {
        stop_services(ctx, &p, names)?;
    }
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
            // The images this project's runs built (REQ-020).
            crate::containers::purge_images(&p.container_images())?;
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

fn has_containers(ctx: &Ctx) -> bool {
    ctx.compose
        .services
        .iter()
        .any(|s| matches!(&s.kind, Kind::Worker(w) if !w.containers.is_empty()))
}

/// The data a service holds or uses: its own D1 databases, R2 buckets and
/// KV namespaces, and for a Worker, what it binds and its Durable Objects'
/// storage (`<worker>-<Class>`).
fn data_of(c: &crate::compose::Compose, s: &Service) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    match &s.kind {
        Kind::Worker(w) => {
            for (_, t) in &w.bindings {
                match t {
                    Target::D1(k) => out.push(("d1", k.clone())),
                    Target::R2(k) => out.push(("r2", k.clone())),
                    Target::Kv(k) => out.push(("kv", k.clone())),
                    Target::DurableObject { service, class } => {
                        out.push(("do", format!("{}-{class}", configs::local_name(c, service))))
                    }
                    Target::Worker(_) | Target::Queue(_) => {}
                }
            }
            for class in &w.durable_objects {
                out.push(("do", format!("{}-{class}", configs::local_name(c, &s.name))));
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
pub(crate) fn purge_targets(
    c: &crate::compose::Compose,
    names: &[String],
) -> Result<Vec<(&'static str, String)>> {
    let mut targets: Vec<(&'static str, String)> = Vec::new();
    for s in c.services.iter().filter(|s| names.contains(&s.name)) {
        for d in data_of(c, s) {
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
        for d in data_of(c, s) {
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

/// Take the named Workers out of the running `up` (the rest keep running);
/// data services have nothing to stop locally (their data stays).
fn stop_services(ctx: &Ctx, p: &Paths, names: &[String]) -> Result<()> {
    let workers: Vec<String> = names
        .iter()
        .filter(|n| {
            matches!(
                ctx.compose.service(n).map(|s| &s.kind),
                Some(Kind::Worker(_))
            )
        })
        .cloned()
        .collect();
    for n in names.iter().filter(|n| !workers.contains(n)) {
        say(crate::tr!(
            "{n} はデータの資源なので、手元では止めるものがありません(データは残ります)",
            "{n} holds data, so locally there is nothing to stop (the data stays)"
        ));
    }
    if workers.is_empty() {
        return Ok(());
    }
    if try_lock(p)?.is_some() {
        say(crate::tr!(
            "--env local up は動いていません",
            "--env local up is not running"
        ));
        return Ok(());
    }
    let mut stopped = read_stopped(p);
    for w in &workers {
        if !stopped.contains(w) {
            stopped.push(w.clone());
        }
    }
    write_stopped(p, &stopped)?;
    wait_for_record(p, |running| !workers.iter().any(|w| running.contains(w)))?;
    say(crate::tr!(
        "{} を下ろしました(ほかのサービスは動いています。戻すには up)",
        "took down {} (the other services keep running; up brings it back)",
        workers.join(", ")
    ));
    Ok(())
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
    // Containers are counted from Docker (REQ-020); nothing to ask when
    // the compose file has none.
    let prefix = configs::local_prefix(&ctx.compose);
    let wanted: Vec<(String, String)> = ctx
        .compose
        .services
        .iter()
        .flat_map(|s| match &s.kind {
            Kind::Worker(w) => w
                .containers
                .iter()
                .map(|k| (configs::local_name(&ctx.compose, &s.name), k.class.clone()))
                .collect(),
            _ => Vec::new(),
        })
        .collect();
    let docker = if has_containers(ctx) {
        crate::containers::container_listing()
    } else {
        None
    };
    let (counts, strays) = match &docker {
        Some(listing) => crate::containers::instances(listing, &prefix, &wanted),
        None => (vec![0; wanted.len()], Vec::new()),
    };
    let mut next = 0;
    crate::out!(
        "{:<16} {:<8} {:<10} URL / RESOURCES",
        "SERVICE", "TYPE", "STATE"
    );
    for s in &ctx.compose.services {
        match &s.kind {
            Kind::Worker(w) => {
                // Running only when the up runs it (not taken down by name).
                let listed = rec
                    .as_ref()
                    .and_then(|r| r["services"].as_array())
                    .is_some_and(|l| l.iter().any(|x| x["service"] == s.name.as_str()));
                let state = if running && listed {
                    crate::tr!("動作中", "running")
                } else {
                    crate::tr!("停止", "stopped")
                };
                crate::out!(
                    "{:<16} {:<8} {:<10} {}",
                    s.name,
                    "worker",
                    state,
                    url_of(&s.name).unwrap_or_else(|| "-".into())
                );
                for k in &w.containers {
                    let n = counts[next];
                    next += 1;
                    let state = if docker.is_none() {
                        crate::tr!("不明", "unknown")
                    } else if n > 0 {
                        crate::tr!("動作中 {n}", "running {n}")
                    } else if running && listed {
                        // A container starts with its first request.
                        crate::tr!("待機", "idle")
                    } else {
                        crate::tr!("停止", "stopped")
                    };
                    let image = match &k.image {
                        crate::compose::Image::Dockerfile(f) => f
                            .strip_prefix(ctx.root())
                            .unwrap_or(f)
                            .display()
                            .to_string(),
                        crate::compose::Image::Registry(r) => r.clone(),
                    };
                    crate::out!(
                        "{:<16} {:<8} {:<10} {}",
                        format!("{}.{}", s.name, k.class),
                        "container",
                        state,
                        image
                    );
                }
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
                crate::out!(
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
                crate::out!(
                    "{:<16} {:<8} {:<10} {}",
                    s.name,
                    other.type_name(),
                    "-",
                    keys
                );
            }
        }
    }
    if has_containers(ctx) && docker.is_none() {
        say(crate::tr!(
            "Docker につながらないので、コンテナの状態は分かりません",
            "Docker does not answer, so the containers' state is unknown"
        ));
    }
    if !strays.is_empty() {
        // Containers of this project that the compose file no longer has.
        crate::out!(
            "{:<16} {:<8} {:<10} {}",
            crate::tr!("(残り)", "(left over)"),
            "container",
            "-",
            strays.join(", ")
        );
    }
    if running {
        for tool in [configs::GATEWAY, configs::DATA] {
            crate::out!(
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
                    crate::out!("{line}");
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
        "{} のバックアップ(環境 {})を {} へ戻します",
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
                    "いまの中身を消してバックアップに入れ替える",
                    "replace the current contents with the backup"
                ),
            )?;
        }
        // What is there now is kept aside first, so a restore that fails
        // half-way puts it back instead of leaving half of each.
        let before = if drop && !full.is_empty() {
            say(crate::tr!(
                "失敗したときに戻せるよう、いまの中身をバックアップします",
                "keeping the current contents, to put back if this fails"
            ));
            let dir =
                crate::data::backup(ctx, c, Some(Paths::new(ctx).dir.join("before-restore")))?;
            say(crate::tr!(
                "いまの中身を {} にバックアップしました",
                "the current contents are kept in {}",
                dir.display()
            ));
            crate::data::drop_targets(c, &plan)?;
            Some(dir)
        } else {
            None
        };
        let loaded =
            crate::data::load(c, &plan, from).and_then(|_| crate::data::apply_migrations(ctx, c));
        if let Err(e) = loaded {
            say(crate::tr!(
                "戻せませんでした: {e:#}。元の中身に戻します",
                "the restore failed: {e:#}; putting the previous contents back"
            ));
            // Putting back can fail too: then say where the previous
            // contents are and how to restore them by hand.
            let put_back = crate::data::drop_targets(c, &plan).and_then(|_| match &before {
                Some(dir) => crate::data::plan_restore(&ctx.compose, dir)
                    .and_then(|back| crate::data::load(c, &back, dir)),
                None => Ok(()),
            });
            if let Err(e2) = put_back {
                match &before {
                    Some(dir) => bail!(crate::tr!(
                        "元の中身にも戻せませんでした: {e2:#}。元の中身は {} にあります。`wrangler-compose --env {} restore --drop {}` で戻せます",
                        "putting the previous contents back failed too: {e2:#}. They are in {}; `wrangler-compose --env {} restore --drop {}` restores them",
                        dir.display(),
                        ctx.env,
                        dir.display()
                    )),
                    None => return Err(e2),
                }
            }
            if let Some(dir) = &before {
                say(crate::tr!(
                    "元の中身に戻しました(バックアップ: {})",
                    "the previous contents are back (kept in {})",
                    dir.display()
                ));
            }
            bail!(crate::tr!(
                "バックアップを戻せませんでした(中身は戻す前のままです)",
                "the backup was not restored (the contents are as before)"
            ));
        }
        if let Some(dir) = before {
            let _ = std::fs::remove_dir_all(dir);
        }
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
            // Every line of a multi-line message carries the service.
            let tag = |m: &str| {
                if m.is_empty() {
                    return format!("{svc:<12} | ");
                }
                m.lines()
                    .map(|l| format!("{svc:<12} | {l}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let mut out = tag(msg);
            if v["t"] == "exc" {
                out = tag(&format!("✘ {msg}"));
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

pub(crate) fn strip_ansi(s: &str) -> String {
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
pub(crate) fn write_private(path: &Path, text: &str) -> Result<()> {
    // A new file, so an old one's looser permissions never carry over.
    let _ = std::fs::remove_file(path);
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

/// Listen on a declared port (REQ-009). Binding alone does not show that
/// the port is free: on macOS a listener on 127.0.0.1 binds while another
/// program listens on 0.0.0.0 (and the other way round), and then quietly
/// takes that program's localhost traffic. So whoever already answers
/// there counts as using the port.
fn claim_port(name: &str, port: &crate::compose::Port) -> Result<TcpListener> {
    let addr: SocketAddr = format!("{}:{}", bracket(&port.ip), port.port)
        .parse()
        .map_err(|_| {
            anyhow!(crate::tr!(
                "アドレス {} が読めません",
                "cannot parse address {}",
                port.ip
            ))
        })?;
    if let Some(other) = answering(&addr) {
        let at = if other == addr {
            String::new()
        } else {
            format!(" {other}")
        };
        bail!(crate::tr!(
            "{name} の {addr} で待ち受けられません(ほかのものが{at}で待ち受けています)。それを止めるか、ports を変えてください",
            "cannot listen on {addr} for {name} (something else listens{at}); stop it or change ports"
        ));
    }
    TcpListener::bind(addr).map_err(|e| {
        anyhow!(crate::tr!(
            "{name} の {addr} で待ち受けられません({e})。ほかのものが使っていないか、ports を変えてください",
            "cannot listen on {addr} for {name} ({e}); is something else using it? change ports"
        ))
    })
}

/// Where something already accepts connections that would reach `addr`:
/// the loopback addresses for a loopback or any-address port (what
/// "localhost" reaches), the address itself otherwise.
fn answering(addr: &SocketAddr) -> Option<SocketAddr> {
    let ip = addr.ip();
    let targets: Vec<SocketAddr> = if ip.is_unspecified() || ip.is_loopback() {
        vec![
            (Ipv4Addr::LOCALHOST, addr.port()).into(),
            (Ipv6Addr::LOCALHOST, addr.port()).into(),
        ]
    } else {
        vec![*addr]
    };
    targets
        .into_iter()
        .find(|t| TcpStream::connect_timeout(t, Duration::from_millis(300)).is_ok())
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

#[cfg(unix)]
fn own_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

#[cfg(not(unix))]
fn own_group(_cmd: &mut Command) {}

/// Stop Wrangler and everything under it, and wait until it is gone.
/// The runtime's process group: stopped when this goes away, on every
/// path out (errors included), so no workerd is left holding ports or data.
struct Runtime(Child);

impl Drop for Runtime {
    fn drop(&mut self) {
        stop_child(&mut self.0);
    }
}

fn stop_child(child: &mut Child) {
    let pid = child.id();
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-s", "TERM", "--", &format!("-{pid}")])
            .stderr(Stdio::null())
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

    fn port(ip: &str, port: u16) -> crate::compose::Port {
        crate::compose::Port {
            ip: ip.into(),
            port,
        }
    }

    #[test]
    fn a_port_someone_answers_on_through_any_address_is_taken() {
        let other = TcpListener::bind("0.0.0.0:0").unwrap();
        let p = other.local_addr().unwrap().port();
        let e = claim_port("web", &port("127.0.0.1", p)).unwrap_err();
        assert!(format!("{e:#}").contains(&format!("127.0.0.1:{p}")));
        assert!(
            format!("{e:#}").contains("else listens") || format!("{e:#}").contains("ほかのものが")
        );
    }

    #[test]
    fn a_port_someone_answers_on_through_loopback_is_taken() {
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = other.local_addr().unwrap().port();
        assert!(claim_port("web", &port("0.0.0.0", p)).is_err());
    }

    #[test]
    fn a_free_port_is_claimed() {
        let p = free_port().unwrap();
        let l = claim_port("web", &port("127.0.0.1", p)).unwrap();
        assert_eq!(l.local_addr().unwrap().port(), p);
    }
}
