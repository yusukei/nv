//! The project, its Wrangler config and how Wrangler is run (DES-002, DES-003).

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CONFIG_FILES: &[&str] = &["wrangler.jsonc", "wrangler.json", "wrangler.toml"];
/// The Wrangler version this tool was verified against (DES-002).
const MIN_WRANGLER: (u64, u64, u64) = (4, 128, 0);

#[derive(Debug)]
pub struct D1 {
    pub binding: String,
    pub name: String,
    pub id: Option<String>,
    pub migrations_dir: PathBuf,
    pub migrations_table: String,
}

#[derive(Debug)]
pub struct R2 {
    pub binding: String,
    pub bucket: String,
    pub jurisdiction: Option<String>,
}

#[derive(Debug)]
pub struct Config {
    pub worker: String,
    pub config_path: PathBuf,
    pub account_id: Option<String>,
    pub d1: Vec<D1>,
    pub r2: Vec<R2>,
    pub routes: Vec<String>,
    pub crons: Vec<String>,
    pub durable_objects: Vec<String>,
}

pub struct Project {
    pub dir: PathBuf,
    nv: PathBuf,
    pub env: Option<String>,
    pub config: Config,
    pub wrangler_version: String,
    /// `--dry-run`: check everything, change nothing (REQ-011).
    pub dry_run: bool,
}

/// Prints the parts of the resolved Wrangler config that wrangler-compose
/// needs, using the project's own Wrangler (DES-003).
const READ_CONFIG_JS: &str = r#"
const path = require("path");
const req = require("module").createRequire(path.join(process.cwd(), "package.json"));
let w, version;
try {
  w = req("wrangler");
  version = req("wrangler/package.json").version;
} catch (e) {
  process.stdout.write("\n@@WRANGLER_COMPOSE@@" + JSON.stringify({ error: "wrangler-missing" }) + "\n");
  process.exit(0);
}
const env = process.argv[1] || undefined;
const raw = w.experimental_readRawConfig({}).rawConfig || {};
const envs = Object.keys(raw.env || {});
let out = { version, envs };
if (!env || envs.includes(env)) {
  const c = w.unstable_readConfig({ env }, { hideWarnings: true });
  out = Object.assign(out, {
    worker: c.name,
    configPath: c.userConfigPath || c.configPath,
    accountId: c.account_id || null,
    d1: (c.d1_databases || []).map(d => ({ binding: d.binding, name: d.database_name, id: d.database_id || null, migrationsDir: d.migrations_dir || "migrations", migrationsTable: d.migrations_table || "d1_migrations" })),
    r2: (c.r2_buckets || []).map(b => ({ binding: b.binding, bucket: b.bucket_name, jurisdiction: b.jurisdiction || null })),
    routes: (c.routes || []).map(r => typeof r === "string" ? r : (r.custom_domain ? `${r.pattern} (custom domain)` : r.pattern)),
    crons: (c.triggers && c.triggers.crons) || [],
    durableObjects: ((c.durable_objects && c.durable_objects.bindings) || []).map(b => b.class_name),
  });
}
process.stdout.write("\n@@WRANGLER_COMPOSE@@" + JSON.stringify(out) + "\n");
"#;

pub fn find_dir(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|d| CONFIG_FILES.iter().any(|f| d.join(f).is_file()))
        .map(Path::to_path_buf)
}

fn nv_path() -> Result<PathBuf> {
    let me = std::env::current_exe().context(crate::tr!(
        "wrangler-compose の実行ファイルが見つかりません",
        "cannot find the wrangler-compose executable"
    ))?;
    let me = std::fs::canonicalize(&me).unwrap_or(me);
    let nv = me
        .parent()
        .ok_or_else(|| {
            anyhow!(crate::tr!(
                "{} のフォルダが分かりません",
                "cannot find the folder of {}",
                me.display()
            ))
        })?
        .join(format!("nv{}", std::env::consts::EXE_SUFFIX));
    if !nv.is_file() {
        bail!(crate::tr!(
            "wrangler-compose と同じフォルダに nv がありません({})。同じ Release の nv をそのフォルダに置いてください",
            "nv was not found next to wrangler-compose ({}); download nv from the same release into that folder",
            nv.display()
        ));
    }
    Ok(nv)
}

impl Project {
    /// Open the project in the current folder for `env` (None: top level).
    pub fn open(env: Option<String>) -> Result<Project> {
        let cwd = std::env::current_dir().context(crate::tr!(
            "いまのフォルダを読めません",
            "cannot read the current folder"
        ))?;
        let dir = find_dir(&cwd).ok_or_else(|| {
            anyhow!(crate::tr!(
                "{} にも、その上のフォルダにも Wrangler の設定(wrangler.jsonc・wrangler.json・wrangler.toml)がありません",
                "no Wrangler config (wrangler.jsonc, wrangler.json or wrangler.toml) in {} or above",
                cwd.display()
            ))
        })?;
        let nv = nv_path()?;
        let mut cmd = Command::new(&nv);
        cmd.args(["run", "node", "-e", READ_CONFIG_JS]);
        if let Some(e) = &env {
            cmd.arg(e);
        }
        let out = cmd
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()
            .with_context(|| crate::tr!("{} を起動できません", "cannot start {}", nv.display()))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let json = stdout
            .rsplit_once("@@WRANGLER_COMPOSE@@")
            .map(|(_, j)| j.trim())
            .ok_or_else(|| {
                anyhow!(crate::tr!(
                    "Wrangler の設定を読めません。プロジェクトの package.json に wrangler がありますか(`nv run npm install --save-dev wrangler@<版>`)",
                    "cannot read the Wrangler config; is wrangler in the project's package.json? (`nv run npm install --save-dev wrangler@<version>`)"
                ))
            })?;
        let v: Value = serde_json::from_str(json).context(crate::tr!(
            "Wrangler の設定を読んだ結果が想定と違います",
            "unexpected output while reading the Wrangler config"
        ))?;
        if v["error"] == "wrangler-missing" {
            bail!(crate::tr!(
                "{} に wrangler が入っていません。`nv run npm install --save-dev wrangler@<版>` でプロジェクトに入れてください",
                "wrangler is not installed in {}; add it to the project with `nv run npm install --save-dev wrangler@<version>`",
                dir.display()
            ));
        }

        let version = v["version"].as_str().unwrap_or("0.0.0");
        if parse_version(version) < MIN_WRANGLER {
            bail!(crate::tr!(
                "プロジェクトの Wrangler は {version} です。wrangler-compose には {}.{}.{} 以上が要ります(`nv run npm install --save-dev wrangler@<版>`)",
                "the project's Wrangler is {version}; wrangler-compose needs {}.{}.{} or newer (`nv run npm install --save-dev wrangler@<version>`)",
                MIN_WRANGLER.0,
                MIN_WRANGLER.1,
                MIN_WRANGLER.2
            ));
        }
        let envs: Vec<String> = strings(&v["envs"]);
        if let Some(e) = &env
            && !envs.contains(e)
        {
            let list = if envs.is_empty() {
                crate::tr!("(なし)", "(none)")
            } else {
                envs.join(", ")
            };
            bail!(crate::tr!(
                "環境 `{e}` は Wrangler の設定にありません。ある環境: {list}",
                "environment `{e}` is not in the Wrangler config; available: {list}"
            ));
        }
        let config = parse_config(&v, &dir)?;
        Ok(Project {
            dir,
            nv,
            env,
            config,
            wrangler_version: version.to_string(),
            dry_run: false,
        })
    }

    pub fn env_name(&self) -> &str {
        self.env.as_deref().unwrap_or("(top level)") // the config's own word; not translated
    }

    fn wrangler_cmd<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new(&self.nv);
        cmd.args(["run", "wrangler"]).args(args);
        if let Some(e) = &self.env {
            cmd.args(["--env", e]);
        }
        // Empty stdin: Wrangler sees no terminal, so it neither prompts nor
        // writes resource IDs back into the config file (DES-002).
        cmd.current_dir(&self.dir).stdin(Stdio::null());
        cmd
    }

    /// Run Wrangler with its output shown to the user.
    pub fn wrangler<I, S>(&self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = self.wrangler_cmd(args);
        let shown = describe(&cmd);
        let status = cmd
            .status()
            .with_context(|| crate::tr!("{shown} を起動できません", "cannot start {shown}"))?;
        if !status.success() {
            bail!(crate::tr!(
                "`{shown}` が失敗しました({status})",
                "`{shown}` failed ({status})"
            ));
        }
        Ok(())
    }

    /// Run Wrangler attached to the user's terminal so it can ask its own
    /// questions. Only for `delete`: run non-interactively, Wrangler answers
    /// "yes" to "other Workers depend on this one, delete anyway?" by itself
    /// (DES-009). Deploys never use this (config write-back, DES-002).
    pub fn wrangler_interactive<I, S>(&self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = self.wrangler_cmd(args);
        cmd.stdin(Stdio::inherit());
        let shown = describe(&cmd);
        let status = cmd
            .status()
            .with_context(|| crate::tr!("{shown} を起動できません", "cannot start {shown}"))?;
        if !status.success() {
            bail!(crate::tr!(
                "`{shown}` が失敗しました({status})",
                "`{shown}` failed ({status})"
            ));
        }
        Ok(())
    }

    /// Whether the Worker of this environment exists on Cloudflare. Wrangler
    /// has no "exists" query, so this lists its secrets (read-only) and reads
    /// Wrangler's "not found" answer (DES-009).
    pub fn worker_exists(&self) -> Result<bool> {
        let mut cmd = self.wrangler_cmd(["secret", "list", "--format", "json"]);
        let shown = describe(&cmd);
        let out = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| crate::tr!("{shown} を起動できません", "cannot start {shown}"))?;
        if out.status.success() {
            return Ok(true);
        }
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("not found") {
            return Ok(false);
        }
        bail!(crate::tr!(
            "`{shown}` が失敗しました({}): {}",
            "`{shown}` failed ({}): {}",
            out.status,
            err.trim()
        ));
    }

    /// Run Wrangler and parse the JSON it prints on stdout.
    pub fn wrangler_json<I, S>(&self, args: I) -> Result<Value>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = self.wrangler_cmd(args);
        let shown = describe(&cmd);
        let out = cmd
            .stderr(Stdio::inherit())
            .output()
            .with_context(|| crate::tr!("{shown} を起動できません", "cannot start {shown}"))?;
        if !out.status.success() {
            bail!(crate::tr!(
                "`{shown}` が失敗しました({})",
                "`{shown}` failed ({})",
                out.status
            ));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let start = text.find(['[', '{']).ok_or_else(|| {
            anyhow!(crate::tr!(
                "`{shown}` が JSON を出しませんでした",
                "`{shown}` printed no JSON"
            ))
        })?;
        serde_json::from_str(&text[start..]).with_context(|| {
            crate::tr!(
                "`{shown}` の出力が想定と違います",
                "`{shown}` printed unexpected output"
            )
        })
    }

    pub fn config_file_bytes(&self) -> Result<Vec<u8>> {
        std::fs::read(&self.config.config_path).with_context(|| {
            crate::tr!(
                "{} を読めません",
                "cannot read {}",
                self.config.config_path.display()
            )
        })
    }
}

fn describe(cmd: &Command) -> String {
    let args: Vec<String> = cmd
        .get_args()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    args.join(" ")
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn opt_string(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

fn parse_config(v: &Value, dir: &Path) -> Result<Config> {
    let missing = || {
        anyhow!(crate::tr!(
            "この版の Wrangler では設定を読めませんでした",
            "the Wrangler config could not be read with this Wrangler version"
        ))
    };
    let worker = opt_string(&v["worker"]).ok_or_else(missing)?;
    let config_path = PathBuf::from(opt_string(&v["configPath"]).ok_or_else(missing)?);
    let d1 = v["d1"]
        .as_array()
        .ok_or_else(missing)?
        .iter()
        .map(|d| -> Result<D1> {
            Ok(D1 {
                binding: opt_string(&d["binding"]).ok_or_else(missing)?,
                name: opt_string(&d["name"]).ok_or_else(missing)?,
                id: opt_string(&d["id"]),
                migrations_dir: dir
                    .join(opt_string(&d["migrationsDir"]).unwrap_or_else(|| "migrations".into())),
                migrations_table: opt_string(&d["migrationsTable"])
                    .unwrap_or_else(|| "d1_migrations".into()),
            })
        })
        .collect::<Result<_>>()?;
    let r2 = v["r2"]
        .as_array()
        .ok_or_else(missing)?
        .iter()
        .map(|b| -> Result<R2> {
            Ok(R2 {
                binding: opt_string(&b["binding"]).ok_or_else(missing)?,
                bucket: opt_string(&b["bucket"]).ok_or_else(missing)?,
                jurisdiction: opt_string(&b["jurisdiction"]),
            })
        })
        .collect::<Result<_>>()?;
    Ok(Config {
        worker,
        config_path,
        account_id: opt_string(&v["accountId"]),
        d1,
        r2,
        routes: strings(&v["routes"]),
        crons: strings(&v["crons"]),
        durable_objects: strings(&v["durableObjects"]),
    })
}

fn parse_version(s: &str) -> (u64, u64, u64) {
    let mut it = s.split(['.', '-']).map(|p| p.parse::<u64>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

/// Whether a D1's migrations folder holds any migration files.
pub fn has_migrations(d1: &D1) -> bool {
    std::fs::read_dir(&d1.migrations_dir)
        .map(|rd| {
            rd.flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "sql"))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compares_wrangler_versions() {
        assert!(parse_version("4.128.0") >= MIN_WRANGLER);
        assert!(parse_version("4.131.2") >= MIN_WRANGLER);
        assert!(parse_version("4.99.9") < MIN_WRANGLER);
        assert!(parse_version("3.200.0") < MIN_WRANGLER);
    }

    #[test]
    fn reads_the_config_summary() {
        let v = json!({
            "version": "4.128.0", "envs": ["staging"],
            "worker": "app-staging", "configPath": "/p/wrangler.jsonc", "accountId": null,
            "d1": [{"binding": "DB", "name": "app-db", "id": null, "migrationsDir": "migrations"}],
            "r2": [{"binding": "BUCKET", "bucket": "app-files", "jurisdiction": "eu"}],
            "routes": ["app.example.com (custom domain)"], "crons": ["0 0 * * *"], "durableObjects": []
        });
        let c = parse_config(&v, Path::new("/p")).unwrap();
        assert_eq!(c.worker, "app-staging");
        assert_eq!(c.d1[0].migrations_dir, Path::new("/p/migrations"));
        assert_eq!(c.d1[0].id, None);
        assert_eq!(c.r2[0].jurisdiction.as_deref(), Some("eu"));
        assert!(parse_config(&json!({"version": "4.128.0"}), Path::new("/p")).is_err());
    }
}
