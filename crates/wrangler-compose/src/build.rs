//! `build` (REQ-007, REQ-008, DES-006, DES-007): each Worker is bundled
//! once, with no bindings and nothing environment-specific in it. Every
//! environment then runs or deploys that same bundle.

use crate::compose::{Kind, Service, Worker};
use crate::ctx::Ctx;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// A built Worker: run or deploy `dir/entry` with `no_bundle`.
#[derive(Debug, Clone)]
pub struct Built {
    pub dir: PathBuf,
    pub entry: String,
    pub hash: String,
}

/// Keys a Worker folder's own Wrangler config may hold: how to build, never
/// how to behave (REQ-007, DES-006).
const BUILD_KEYS: &[&str] = &[
    "$schema",
    "main",
    "build",
    "rules",
    "find_additional_modules",
    "base_dir",
    "preserve_file_names",
    "no_bundle",
    "minify",
    "tsconfig",
    "alias",
    "upload_source_maps",
];

const CONFIG_FILES: [&str; 3] = ["wrangler.jsonc", "wrangler.json", "wrangler.toml"];
const DEFAULT_ENTRIES: [&str; 6] = [
    "src/index.ts",
    "src/index.js",
    "src/index.mjs",
    "index.ts",
    "index.js",
    "index.mjs",
];

/// `build [services]`: build the named Workers (default: all).
pub fn build(ctx: &Ctx, names: &[String]) -> Result<Vec<Built>> {
    let workers = select(ctx, names)?;
    let mut out = Vec::new();
    for (svc, w) in workers {
        out.push(build_one(ctx, svc, w, true)?);
    }
    if !ctx.dry_run {
        say(crate::tr!("組み立てました", "built"));
    }
    Ok(out)
}

/// The Workers among `names` (all when empty).
pub fn select<'a>(ctx: &'a Ctx, names: &[String]) -> Result<Vec<(&'a str, &'a Worker)>> {
    for n in names {
        if !ctx.compose.services.iter().any(|s| &s.name == n) {
            bail!(crate::tr!(
                "サービス「{n}」はありません",
                "there is no service \"{n}\""
            ));
        }
    }
    Ok(ctx
        .compose
        .services
        .iter()
        .filter(|s| names.is_empty() || names.contains(&s.name))
        .filter_map(|s: &Service| match &s.kind {
            Kind::Worker(w) => Some((s.name.as_str(), &**w)),
            _ => None,
        })
        .collect())
}

/// Build one Worker unless the stamp says nothing changed. `report` prints
/// what happened (the watcher in `up` prints its own lines).
pub fn build_one(ctx: &Ctx, svc: &str, w: &Worker, report: bool) -> Result<Built> {
    let dir = ctx.tool_dir().join("build").join(svc);
    let out_dir = dir.join("out");
    let config = build_config(ctx, svc, w)?;
    let entry = entry_name(config["main"].as_str().unwrap_or("index.js"));
    let stamp = dir.join("stamp.json");
    let old: Value = std::fs::read_to_string(&stamp)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let hash = source_hash(ctx, w, &config, &inputs_of(&old))?;
    if old["hash"].as_str() == Some(hash.as_str()) && out_dir.join(&entry).is_file() {
        if report {
            say(crate::tr!(
                "{svc}: 変わっていないので組み立て直しません",
                "{svc}: unchanged, not rebuilt"
            ));
        }
        return Ok(Built {
            dir: out_dir,
            entry,
            hash,
        });
    }
    if ctx.dry_run {
        say(crate::tr!("{svc}: 組み立てる", "{svc}: would build"));
        return Ok(Built {
            dir: out_dir,
            entry,
            hash,
        });
    }
    sync_deps(ctx, &w.build)?;
    std::fs::create_dir_all(&dir)
        .with_context(|| crate::tr!("{} を作れません", "cannot create {}", dir.display()))?;
    let cfg_path = dir.join("wrangler.json");
    std::fs::write(&cfg_path, serde_json::to_vec_pretty(&config)?)?;
    // Build next to the running bundle; it is replaced only by a good build.
    let next = dir.join("next");
    let _ = std::fs::remove_dir_all(&next);
    let meta = dir.join("meta.json");
    let _ = std::fs::remove_file(&meta);
    let mut cmd = ctx.wrangler(["deploy", "--dry-run", "-c"]);
    cmd.arg(&cfg_path)
        .arg("--outdir")
        .arg(&next)
        .arg("--metafile")
        .arg(&meta);
    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context(crate::tr!(
            "Wrangler を起動できません",
            "cannot start Wrangler"
        ))?;
    if !output.status.success() || !next.join(&entry).is_file() {
        eprint!("{}", String::from_utf8_lossy(&output.stdout));
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        let _ = std::fs::remove_dir_all(&next);
        bail!(crate::tr!(
            "{svc} を組み立てられません(上の出力を見てください)",
            "{svc} could not be built (see the output above)"
        ));
    }
    replace_files(&next, &out_dir)?;
    // Files the bundle came from outside the Worker folder (shared code,
    // a tsconfig it extends) also decide it from now on.
    let inputs = outside_inputs(&dir, w, &meta);
    let hash = source_hash(ctx, w, &config, &inputs)?;
    std::fs::write(
        &stamp,
        serde_json::to_vec(&json!({ "hash": hash, "entry": entry, "inputs": inputs }))?,
    )?;
    if report {
        say(crate::tr!("{svc}: 組み立てました", "{svc}: built"));
    }
    Ok(Built {
        dir: out_dir,
        entry,
        hash,
    })
}

/// Copy every file of a finished build over the running bundle, then drop
/// what the new build no longer has. Overwriting in place (not renaming) is
/// what `wrangler dev`'s watcher notices; a failed build never gets here, so
/// the running bundle stays whole.
fn replace_files(from: &Path, to: &Path) -> Result<()> {
    let mut new = Vec::new();
    walk_all(from, from, &mut new)?;
    for rel in &new {
        let dest = to.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(from.join(rel), &dest)?;
    }
    let mut old = Vec::new();
    if to.is_dir() {
        walk_all(to, to, &mut old)?;
    }
    for rel in old.iter().filter(|r| !new.contains(r)) {
        let _ = std::fs::remove_file(to.join(rel));
    }
    let _ = std::fs::remove_dir_all(from);
    Ok(())
}

fn walk_all(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk_all(base, &path, out)?;
        } else {
            out.push(path.strip_prefix(base).unwrap_or(&path).to_path_buf());
        }
    }
    Ok(())
}

fn inputs_of(stamp: &Value) -> Vec<PathBuf> {
    stamp["inputs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(PathBuf::from))
        .collect()
}

/// The bundle's inputs (from esbuild's metafile, relative to the build
/// config's folder `base`) outside the Worker folder and outside
/// node_modules, as absolute paths.
fn outside_inputs(base: &Path, w: &Worker, meta: &Path) -> Vec<PathBuf> {
    let folder = w.build.canonicalize().unwrap_or_else(|_| w.build.clone());
    let Some(v) = std::fs::read_to_string(meta)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = v["inputs"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, _)| base.join(k).canonicalize().ok())
        .filter(|p| {
            !p.starts_with(&folder)
                && !p.components().any(|c| c.as_os_str() == "node_modules")
                && p.is_file()
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The Wrangler config used only for bundling: the Worker folder's build
/// keys (checked), with paths made absolute.
fn build_config(ctx: &Ctx, svc: &str, w: &Worker) -> Result<Value> {
    // Wrangler would read local values from these; here they belong in the
    // compose file (environment) and .env.<env> (secrets).
    for entry in std::fs::read_dir(&w.build).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".dev.vars" || name.starts_with(".dev.vars.") {
            bail!(crate::tr!(
                "{} は使えません。値は wrangler-compose.yaml の services.{svc}.environment に、シークレットは services.{svc}.secrets と .env.<環境> に書いてください",
                "{} is not used; put values in services.{svc}.environment in wrangler-compose.yaml and secrets in services.{svc}.secrets plus .env.<env>",
                entry.path().display()
            ));
        }
    }
    let mut cfg = Map::new();
    if let Some(file) = CONFIG_FILES
        .iter()
        .map(|f| w.build.join(f))
        .find(|p| p.is_file())
    {
        let raw = ctx.node_json(READ_RAW_JS, [&file])?;
        let Value::Object(raw) = raw["config"].clone() else {
            bail!(crate::tr!(
                "{} を読めません",
                "cannot read {}",
                file.display()
            ));
        };
        let bad: Vec<&String> = raw
            .keys()
            .filter(|k| !BUILD_KEYS.contains(&k.as_str()))
            .collect();
        if !bad.is_empty() {
            let lines: Vec<String> = bad
                .iter()
                .map(|k| match where_in_compose(k) {
                    Some(w) => format!("  - {k}: {w}"),
                    None => format!("  - {k}"),
                })
                .collect();
            bail!(crate::tr!(
                "{} に振る舞いに関わる設定があります。Worker のフォルダには組み立てのための設定だけを置き、ほかは wrangler-compose.yaml に書いてください:\n{}",
                "{} holds settings that change behavior; a Worker folder may only hold build settings, the rest belongs in wrangler-compose.yaml:\n{}",
                file.display(),
                lines.join("\n")
            ));
        }
        cfg = raw;
    }
    let main = match cfg.get("main").and_then(Value::as_str) {
        Some(m) => w.build.join(m),
        None => DEFAULT_ENTRIES
            .iter()
            .map(|e| w.build.join(e))
            .find(|p| p.is_file())
            .ok_or_else(|| {
                anyhow::anyhow!(crate::tr!(
                    "{svc} の入口のファイルが見つかりません。{} に src/index.ts などを置くか、そこの wrangler.jsonc に main を書いてください",
                    "cannot find {svc}'s entry file; put src/index.ts (or similar) in {} or set main in a wrangler.jsonc there",
                    w.build.display()
                ))
            })?,
    };
    cfg.insert("main".into(), json!(main));
    // Relative alias targets are relative to the Worker folder's config.
    if let Some(Value::Object(a)) = cfg.get_mut("alias") {
        for v in a.values_mut() {
            if let Some(p) = v
                .as_str()
                .filter(|p| p.starts_with("./") || p.starts_with("../"))
            {
                *v = json!(w.build.join(p));
            }
        }
    }
    for key in ["base_dir", "tsconfig"] {
        if let Some(p) = cfg.get(key).and_then(Value::as_str) {
            let abs = w.build.join(p);
            cfg.insert(key.into(), json!(abs));
        }
    }
    if let Some(Value::Object(b)) = cfg.get_mut("build") {
        let cwd = b
            .get("cwd")
            .and_then(Value::as_str)
            .map(|c| w.build.join(c))
            .unwrap_or_else(|| w.build.clone());
        b.insert("cwd".into(), json!(cwd));
        match b.get("watch_dir") {
            Some(Value::String(wd)) => {
                let abs = w.build.join(wd);
                b.insert("watch_dir".into(), json!(abs));
            }
            Some(Value::Array(list)) => {
                let abs: Vec<Value> = list
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|wd| json!(w.build.join(wd)))
                    .collect();
                b.insert("watch_dir".into(), json!(abs));
            }
            _ => {}
        }
    }
    cfg.shift_remove("$schema");
    cfg.insert("name".into(), json!(svc));
    cfg.insert("compatibility_date".into(), json!(w.compatibility_date));
    if !w.compatibility_flags.is_empty() {
        cfg.insert("compatibility_flags".into(), json!(w.compatibility_flags));
    }
    Ok(Value::Object(cfg))
}

/// Where a behavior key goes instead (REQ-007).
fn where_in_compose(key: &str) -> Option<String> {
    let field = match key {
        "name" => "name",
        "vars" => "environment",
        "routes" | "route" => "routes",
        "triggers" => "crons",
        "compatibility_date" => "compatibility_date",
        "compatibility_flags" => "compatibility_flags",
        "assets" => "assets",
        "durable_objects" | "migrations" => "durable_objects",
        "d1_databases" | "r2_buckets" | "kv_namespaces" | "queues" | "services" => {
            return Some(crate::tr!(
                "services.<名前>.bindings(相手は d1・r2・kv・queue・worker のサービス)",
                "services.<name>.bindings (pointing at d1, r2, kv, queue or worker services)"
            ));
        }
        "env" => {
            return Some(crate::tr!(
                "環境ごとの値は ${{VAR}} と .env.<環境>",
                "per-environment values come from ${{VAR}} and .env.<env>"
            ));
        }
        _ => return None,
    };
    Some(crate::tr!(
        "services.<名前>.{field}",
        "services.<name>.{field}"
    ))
}

/// `src/index.ts` → `index.js` (what `--outdir` writes).
fn entry_name(main: &str) -> String {
    let stem = Path::new(main)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "index".into());
    format!("{stem}.js")
}

/// A Worker folder with its own package.json gets its dependencies first
/// (REQ-018 of the old spec, DES-001): `nv run` installs them when needed.
fn sync_deps(ctx: &Ctx, folder: &Path) -> Result<()> {
    let same = |a: &Path, b: &Path| {
        std::fs::canonicalize(a)
            .ok()
            .is_some_and(|a| std::fs::canonicalize(b).ok() == Some(a))
    };
    if !folder.join("package.json").is_file() || same(folder, ctx.project()) {
        return Ok(());
    }
    let status = ctx
        .node_script("0", std::iter::empty::<&str>())
        .current_dir(folder)
        .stdout(Stdio::null())
        .status()
        .context(crate::tr!("nv を起動できません", "cannot start nv"))?;
    if !status.success() {
        bail!(crate::tr!(
            "{} の依存を入れられません",
            "cannot install the dependencies of {}",
            folder.display()
        ));
    }
    Ok(())
}

/// What decides the bundle: every source file of the Worker folder (not
/// node_modules or tool output), the files it used from outside the folder,
/// the lockfiles that pin its dependencies, the Wrangler version and the
/// build config.
pub fn source_hash(ctx: &Ctx, w: &Worker, config: &Value, inputs: &[PathBuf]) -> Result<String> {
    let mut h = Sha256::new();
    h.update(serde_json::to_vec(config)?);
    h.update(ctx.wrangler_version.as_bytes());
    let mut files = Vec::new();
    walk(&w.build, &w.build, &mut files)?;
    files.sort();
    let mut all: Vec<PathBuf> = files.iter().map(|r| w.build.join(r)).collect();
    all.extend(inputs.iter().cloned());
    for lock in [
        ctx.project().join("package-lock.json"),
        w.build.join("package-lock.json"),
    ] {
        if !all.contains(&lock) {
            all.push(lock);
        }
    }
    for path in all {
        h.update(path.to_string_lossy().as_bytes());
        h.update([0]);
        h.update(std::fs::read(&path).unwrap_or_default());
    }
    Ok(crate::files::hex(&h.finalize()))
}

/// The newest change among what decides a Worker's bundle, for the watcher
/// in `up`.
pub fn newest_change(ctx: &Ctx, svc: &str, w: &Worker) -> Option<std::time::SystemTime> {
    let mut files = Vec::new();
    walk(&w.build, &w.build, &mut files).ok()?;
    let stamp: Value =
        std::fs::read_to_string(ctx.tool_dir().join("build").join(svc).join("stamp.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
    files
        .iter()
        .map(|r| w.build.join(r))
        .chain(inputs_of(&stamp))
        .filter_map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .max()
}

fn walk(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(
            name.as_ref(),
            "node_modules" | ".wrangler" | ".git" | "dist"
        ) {
            continue;
        }
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk(base, &path, out)?;
        } else if ft.is_file() || (ft.is_symlink() && path.is_file()) {
            out.push(path.strip_prefix(base).unwrap_or(&path).to_path_buf());
        }
    }
    Ok(())
}

fn say(msg: String) {
    eprintln!("wrangler-compose: {msg}");
}

const READ_RAW_JS: &str = r#"
const path = require("path");
const req = require("module").createRequire(path.join(process.cwd(), "package.json"));
let out;
try {
  const w = req("wrangler");
  out = { config: w.experimental_readRawConfig({ config: process.argv[1] }).rawConfig || {} };
} catch (e) { out = { error: String(e && e.message || e) }; }
process.stdout.write("\n@@WC@@" + JSON.stringify(out) + "\n");
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_bundle_like_outdir_does() {
        assert_eq!(entry_name("/p/api/src/index.ts"), "index.js");
        assert_eq!(entry_name("worker.mjs"), "worker.js");
    }

    #[test]
    fn behavior_keys_point_into_the_compose_file() {
        assert!(
            where_in_compose("d1_databases")
                .unwrap()
                .contains("bindings")
        );
        assert!(where_in_compose("main").is_none());
        assert!(!BUILD_KEYS.contains(&"vars") && !BUILD_KEYS.contains(&"define"));
    }
}
