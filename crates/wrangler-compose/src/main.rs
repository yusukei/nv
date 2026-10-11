//! wrangler-compose: Cloudflare Workers environments, run like docker compose.

#[macro_use]
mod i18n;
mod backup;
mod confirm;
mod d1;
mod dotenv;
mod down;
mod files;
mod git;
mod interrupt;
mod project;
mod r2;
mod restore;
mod up;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use project::Project;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "wrangler-compose",
    version,
    about = "Build, deploy, tear down, back up and restore a Cloudflare Workers environment"
)]
struct Cli {
    /// Environment in the Wrangler config (env.<name>); also read from CLOUDFLARE_ENV
    #[arg(short, long, global = true)]
    env: Option<String>,
    /// Check everything and show what would happen, without changing anything
    #[arg(long, global = true)]
    dry_run: bool,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build the Worker without uploading anything
    Build,
    /// Apply pending D1 migrations and deploy (first time and updates alike)
    Up,
    /// Delete the Worker; with --purge also its D1 databases and R2 buckets
    Down {
        /// Also delete D1 and R2 data (needs R2 API keys)
        #[arg(long)]
        purge: bool,
    },
    /// Copy D1 and R2 contents into <dest>/<env>/<time>/ (default: ./backups)
    Backup { dest: Option<PathBuf> },
    /// Put a backup's contents into the environment
    Restore {
        /// Delete the current contents first
        #[arg(long)]
        drop: bool,
        /// Backup folder (the one holding manifest.json)
        from: PathBuf,
    },
}

/// Japanese `--help` (REQ-012), same approach as nv.
const JA_ABOUT: &[(&str, &str)] = &[
    (
        "",
        "Cloudflare Workers の環境を組む・出す・消す・控える・戻す",
    ),
    ("build", "Worker を組むだけで、何も上げない"),
    (
        "up",
        "未適用の D1 の移行を当ててデプロイする(初回も更新も同じ)",
    ),
    ("down", "Worker を消す。--purge なら D1 と R2 も消す"),
    (
        "backup",
        "D1 と R2 の中身を <書き出し先>/<環境>/<日時>/ に控える(既定は ./backups)",
    ),
    ("restore", "控えの中身を環境に戻す"),
];
const JA_ARGS: &[(&str, &str, &str)] = &[
    (
        "",
        "env",
        "Wrangler の設定の環境(env.<名前>)。CLOUDFLARE_ENV でも指定できる",
    ),
    (
        "",
        "dry_run",
        "すべて確かめて何をするかを示し、何も変えない",
    ),
    (
        "down",
        "purge",
        "D1 と R2 のデータも消す(R2 の API キーが要る)",
    ),
    ("backup", "dest", "書き出し先"),
    ("restore", "drop", "先にいまの中身を消す"),
    (
        "restore",
        "from",
        "控えのフォルダ(manifest.json のあるもの)",
    ),
];

fn localize(mut cmd: clap::Command, path: &str) -> clap::Command {
    if let Some((_, about)) = JA_ABOUT.iter().find(|(p, _)| *p == path) {
        cmd = cmd.about(*about);
    }
    for (p, id, help) in JA_ARGS {
        if *p == path {
            cmd = cmd.mut_arg(*id, |a| a.help(*help));
        }
    }
    let mut order = 0;
    cmd = cmd
        .help_template("{about-with-newline}\n使い方: {usage}\n\n{all-args}{after-help}")
        .subcommand_help_heading("コマンド")
        .mut_args(|a| {
            order += 1;
            let heading = if a.is_positional() {
                "引数"
            } else {
                "オプション"
            };
            a.help_heading(heading).display_order(order)
        })
        .disable_help_flag(true)
        .arg(
            clap::Arg::new("help")
                .short('h')
                .long("help")
                .action(clap::ArgAction::Help)
                .help("ヘルプを表示する")
                .help_heading("オプション"),
        );
    if path.is_empty() {
        cmd = cmd.disable_version_flag(true).arg(
            clap::Arg::new("version")
                .short('V')
                .long("version")
                .action(clap::ArgAction::Version)
                .help("版を表示する")
                .help_heading("オプション"),
        );
    }
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_string())
        .collect();
    for name in names {
        let sub_path = if path.is_empty() {
            name.clone()
        } else {
            format!("{path} {name}")
        };
        cmd = cmd.mut_subcommand(&name, |s| localize(s, &sub_path));
    }
    cmd
}

fn parse_cli() -> Cli {
    use clap::{CommandFactory, FromArgMatches};
    let mut cmd = Cli::command();
    if i18n::is_ja() {
        cmd = localize(cmd, "");
        // The `help` subcommand appears only once the command is built.
        cmd.build();
        cmd = cmd.mut_subcommand("help", |s| {
            s.about("このメッセージか、指定したコマンドのヘルプを表示する")
        });
    }
    let matches = cmd.get_matches();
    Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
}

fn main() {
    let cli = parse_cli();
    interrupt::install();
    if let Err(e) = dispatch(cli) {
        eprintln!("wrangler-compose: {} {e:#}", tr!("エラー:", "error:"));
        std::process::exit(2);
    }
}

fn dispatch(cli: Cli) -> Result<()> {
    let env = cli.env.or_else(|| {
        std::env::var("CLOUDFLARE_ENV")
            .ok()
            .filter(|e| !e.is_empty())
    });
    // Anything that touches Cloudflare needs an explicit environment (REQ-001).
    if env.is_none() && !matches!(cli.command, Cmd::Build) {
        bail!(tr!(
            "--env <名前>(または CLOUDFLARE_ENV)で環境を指定してください。何もしていません",
            "choose an environment with --env <name> (or CLOUDFLARE_ENV); nothing was done"
        ));
    }
    let mut project = Project::open(env)?;
    project.dry_run = cli.dry_run;
    match cli.command {
        Cmd::Build => up::build(&project),
        Cmd::Up => up::up(&project),
        Cmd::Down { purge: false } => down::down(&project),
        Cmd::Down { purge: true } => down::purge(&project),
        Cmd::Backup { dest } => backup::backup(&project, dest),
        Cmd::Restore { drop, from } => restore::restore(&project, &from, drop),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn cli_definition_is_consistent() {
        use clap::CommandFactory;
        super::Cli::command().debug_assert();
    }
}
