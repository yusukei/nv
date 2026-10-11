//! What every operation shares (DES-001): the environment, the compose
//! file, the folder for what the tool makes, and how Wrangler is run.

use crate::compose::{self, Compose};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The Wrangler this design was checked against (DES facts); older ones
/// lack features the generated configs rely on.
const MIN_WRANGLER: (u64, u64, u64) = (4, 149, 0);

pub struct Ctx {
    pub env: String,
    pub local: bool,
    pub dry_run: bool,
    pub compose: Compose,
    nv: PathBuf,
}

impl Ctx {
    pub fn open(env: &str, dry_run: bool) -> Result<Ctx> {
        if !env
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            || env.is_empty()
        {
            bail!(crate::tr!(
                "環境の名前「{env}」には英小文字・数字・- だけを使ってください",
                "environment name \"{env}\" may only use lowercase letters, digits and -"
            ));
        }
        let cwd = std::env::current_dir()?;
        let file = compose::find(&cwd)?;
        let compose = compose::load(&file, env)?;
        let nv = nv_path()?;
        let ctx = Ctx {
            env: env.to_string(),
            local: env == "local",
            dry_run,
            compose,
            nv,
        };
        ctx.check_wrangler()?;
        Ok(ctx)
    }

    pub fn root(&self) -> &Path {
        &self.compose.root
    }

    /// `.wrangler/compose/`: everything the tool makes (REQ-007).
    pub fn tool_dir(&self) -> PathBuf {
        self.root().join(".wrangler").join("compose")
    }

    /// `.wrangler/compose/<env>/`.
    pub fn env_dir(&self) -> PathBuf {
        self.tool_dir().join(&self.env)
    }

    /// `nv run wrangler <args>` in the compose root, with an empty stdin so
    /// Wrangler never prompts nor writes back into config files.
    pub fn wrangler<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new(&self.nv);
        cmd.args(["run", "wrangler"]).args(args);
        cmd.current_dir(self.root()).stdin(Stdio::null());
        cmd
    }

    /// `nv run node -e <script> <args>` in the compose root.
    pub fn node_script<I, S>(&self, script: &str, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new(&self.nv);
        cmd.args(["run", "node", "-e", script]).args(args);
        cmd.current_dir(self.root()).stdin(Stdio::null());
        cmd
    }

    /// Run a node script and read the JSON it prints after `@@WC@@`.
    pub fn node_json<I, S>(&self, script: &str, args: I) -> Result<Value>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let out = self
            .node_script(script, args)
            .stderr(Stdio::inherit())
            .output()
            .context(crate::tr!("nv を起動できません", "cannot start nv"))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let json = text
            .rsplit_once("@@WC@@")
            .map(|(_, j)| j.trim())
            .ok_or_else(|| {
                anyhow!(crate::tr!(
                    "Wrangler を読めません。compose ファイルのあるフォルダの package.json に wrangler がありますか(`nv run npm install --save-dev wrangler@<版>`)",
                    "cannot load Wrangler; is wrangler in the package.json next to the compose file? (`nv run npm install --save-dev wrangler@<version>`)"
                ))
            })?;
        let v: Value = serde_json::from_str(json).context(crate::tr!(
            "Wrangler からの答えが想定と違います",
            "unexpected answer from Wrangler"
        ))?;
        if let Some(e) = v.get("error").and_then(Value::as_str) {
            bail!("{e}");
        }
        Ok(v)
    }

    fn check_wrangler(&self) -> Result<()> {
        let v = self.node_json(VERSION_JS, std::iter::empty::<&str>())?;
        let version = v["version"].as_str().unwrap_or("0.0.0");
        if parse_version(version) < MIN_WRANGLER {
            bail!(crate::tr!(
                "Wrangler {version} は古すぎます。{}.{}.{} 以上を入れてください(`nv run npm install --save-dev wrangler@latest`)",
                "Wrangler {version} is too old; install {}.{}.{} or newer (`nv run npm install --save-dev wrangler@latest`)",
                MIN_WRANGLER.0,
                MIN_WRANGLER.1,
                MIN_WRANGLER.2
            ));
        }
        Ok(())
    }
}

const VERSION_JS: &str = r#"
const path = require("path");
const req = require("module").createRequire(path.join(process.cwd(), "package.json"));
let out;
try { out = { version: req("wrangler/package.json").version }; }
catch (e) { out = { error: "wrangler is not installed next to the compose file (`nv run npm install --save-dev wrangler@latest`)" }; }
process.stdout.write("\n@@WC@@" + JSON.stringify(out) + "\n");
"#;

fn nv_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("WRANGLER_COMPOSE_NV") {
        // For tests: run with a specific nv.
        return Ok(PathBuf::from(p));
    }
    let me = std::env::current_exe().context(crate::tr!(
        "wrangler-compose の実行ファイルが見つかりません",
        "cannot find the wrangler-compose executable"
    ))?;
    let me = std::fs::canonicalize(&me).unwrap_or(me);
    let nv = me
        .parent()
        .ok_or_else(|| anyhow!("{}", me.display()))?
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

fn parse_version(s: &str) -> (u64, u64, u64) {
    let mut it = s.split(['.', '-']).map(|p| p.parse::<u64>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}
