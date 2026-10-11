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
    let hash = source_hash(&w.build, &config)?;
    let entry = entry_name(config["main"].as_str().unwrap_or("index.js"));
    let stamp = dir.join("stamp.json");
    if let Ok(text) = std::fs::read_to_string(&stamp)
        && serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["hash"].as_str().map(str::to_string))
            == Some(hash.clone())
        && out_dir.join(&entry).is_file()
    {
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
    let _ = std::fs::remove_dir_all(&out_dir);
    let mut cmd = ctx.wrangler(["deploy", "--dry-run", "-c"]);
    cmd.arg(&cfg_path).arg("--outdir").arg(&out_dir);
    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context(crate::tr!(
            "Wrangler を起動できません",
            "cannot start Wrangler"
        ))?;
    if !output.status.success() || !out_dir.join(&entry).is_file() {
        eprint!("{}", String::from_utf8_lossy(&output.stdout));
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        bail!(crate::tr!(
            "{svc} を組み立てられません(上の出力を見てください)",
            "{svc} could not be built (see the output above)"
        ));
    }
    std::fs::write(
        &stamp,
        serde_json::to_vec(&json!({ "hash": hash, "entry": entry }))?,
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

/// The Wrangler config used only for bundling: the Worker folder's build
/// keys (checked), with paths made absolute.
fn build_config(ctx: &Ctx, svc: &str, w: &Worker) -> Result<Value> {
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
        if let Some(wd) = b.get("watch_dir").and_then(Value::as_str) {
            let abs = w.build.join(wd);
            b.insert("watch_dir".into(), json!(abs));
        }
    }
    cfg.remove("$schema");
    cfg.insert("name".into(), json!(svc));
    cfg.insert("compatibility_date".into(), json!(w.compatibility_date));
    if !w.compatibility_flags.is_empty() {
        cfg.insert("compatibility_flags".into(), json!(w.compatibility_flags));
    }
    Ok(Value::Object(cfg))
}

/// Where a behavior key goes instead (REQ-007).
fn where_in_compose(key: &str) -> Option<&'static str> {
    Some(match key {
        "name" => "services.<名前>.name",
        "vars" => "services.<名前>.vars",
        "routes" | "route" => "services.<名前>.routes",
        "triggers" => "services.<名前>.crons",
        "compatibility_date" => "services.<名前>.compatibility_date",
        "compatibility_flags" => "services.<名前>.compatibility_flags",
        "assets" => "services.<名前>.assets",
        "d1_databases" | "r2_buckets" | "kv_namespaces" | "queues" | "services" => {
            "services.<名前>.bindings(相手は d1・r2・kv・queue・worker のサービス)"
        }
        "durable_objects" | "migrations" => "services.<名前>.durable_objects",
        "env" => "環境ごとの値は ${VAR} と .env.<環境>",
        _ => return None,
    })
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
    if !folder.join("package.json").is_file() || folder == ctx.root() {
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
/// node_modules or tool output) and the build config.
pub fn source_hash(folder: &Path, config: &Value) -> Result<String> {
    let mut h = Sha256::new();
    h.update(serde_json::to_vec(config)?);
    let mut files = Vec::new();
    walk(folder, folder, &mut files)?;
    files.sort();
    for rel in files {
        h.update(rel.to_string_lossy().as_bytes());
        h.update([0]);
        h.update(std::fs::read(folder.join(&rel)).unwrap_or_default());
    }
    Ok(crate::files::hex(&h.finalize()))
}

/// The newest change under a Worker folder, for the watcher in `up`.
pub fn newest_change(folder: &Path) -> Option<std::time::SystemTime> {
    let mut files = Vec::new();
    walk(folder, folder, &mut files).ok()?;
    files
        .iter()
        .filter_map(|r| {
            std::fs::metadata(folder.join(r))
                .and_then(|m| m.modified())
                .ok()
        })
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
        } else if ft.is_file() {
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
