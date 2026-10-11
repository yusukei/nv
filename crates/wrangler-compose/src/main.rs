//! wrangler-compose: Cloudflare Workers environments, run like docker compose.

#[macro_use]
mod i18n;
mod build;
mod compose;
mod configs;
mod confirm;
mod ctx;
mod data;
mod dotenv;
mod files;
mod git;
mod interp;
mod interrupt;
mod local;
mod r2proxy;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use ctx::Ctx;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "wrangler-compose",
    version,
    about = "Run the services you develop locally like docker compose, and deploy the same build safely to Cloudflare"
)]
struct Cli {
    /// Environment: local (this machine) or a remote one; also read from CLOUDFLARE_ENV
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
    /// Build the Workers without starting or deploying anything
    Build {
        /// Services to build (default: every Worker)
        services: Vec<String>,
    },
    /// Start every service locally (--env local) or deploy the same build remotely
    Up {
        /// Build again before starting
        #[arg(long)]
        build: bool,
        /// Remove Workers of services no longer in the compose file
        #[arg(long)]
        remove_orphans: bool,
        /// Services to start, with what they depend on (default: all)
        services: Vec<String>,
    },
    /// Stop (local) or remove (remote) the Workers; data stays unless --purge
    Down {
        /// Also delete the data (D1, R2, KV, Queues)
        #[arg(long)]
        purge: bool,
        /// Services to take down (default: all)
        services: Vec<String>,
    },
    /// Show the state of every service
    Ps,
    /// Show the Workers' logs, each line prefixed with its service
    Logs {
        /// Keep following new lines
        #[arg(short, long)]
        follow: bool,
        /// Services to show (default: all)
        services: Vec<String>,
    },
    /// Copy D1, R2 and KV contents into <dest>/<env>/<time>/ (default: ./backups)
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
        "手元で開発したサービスを、docker compose のように動かし、安全にリモートへ展開する",
    ),
    (
        "build",
        "Worker を組み立てるだけで、何も動かさない・上げない",
    ),
    (
        "up",
        "手元(--env local)ですべてを動かす。リモートには手元と同じ組み立てを展開する",
    ),
    (
        "down",
        "Worker を止める(手元)・下ろす(リモート)。データは --purge のときだけ消す",
    ),
    ("ps", "サービスごとの状態を並べる"),
    ("logs", "Worker のログを、行の頭にサービス名を付けて出す"),
    (
        "backup",
        "D1・R2・KV の中身を <書き出し先>/<環境>/<日時>/ に控える(既定は ./backups)",
    ),
    ("restore", "控えの中身を環境に戻す"),
];
const JA_ARGS: &[(&str, &str, &str)] = &[
    (
        "",
        "env",
        "環境(local は手元、それ以外はリモート)。CLOUDFLARE_ENV でも指定できる",
    ),
    (
        "",
        "dry_run",
        "すべて確かめて何をするかを示し、何も変えない",
    ),
    (
        "build",
        "services",
        "組み立てるサービス(既定はすべての Worker)",
    ),
    ("up", "build", "組み立て直してから動かす"),
    (
        "up",
        "remove_orphans",
        "compose ファイルから消えたサービスの Worker を消す",
    ),
    (
        "up",
        "services",
        "動かすサービス(依存先も含む。既定はすべて)",
    ),
    ("down", "purge", "データ(D1・R2・KV・Queue)も消す"),
    ("down", "services", "下ろすサービス(既定はすべて)"),
    ("logs", "follow", "新しい行を待って出し続ける"),
    ("logs", "services", "出すサービス(既定はすべて)"),
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
    // Every operation needs an environment (REQ-006).
    let Some(env) = env else {
        bail!(tr!(
            "--env <環境>(または CLOUDFLARE_ENV)で環境を指定してください(手元なら --env local)。何もしていません",
            "choose an environment with --env <name> (or CLOUDFLARE_ENV); use --env local for this machine. Nothing was done"
        ));
    };
    let ctx = Ctx::open(&env, cli.dry_run)?;
    if !ctx.local {
        bail!(tr!(
            "リモートの環境({env})の操作はまだ作っていません。いまは --env local だけが使えます",
            "remote environments ({env}) are not implemented yet; only --env local works for now"
        ));
    }
    match cli.command {
        Cmd::Build { services } => build::build(&ctx, &services).map(|_| ()),
        Cmd::Up {
            build,
            remove_orphans: _,
            services,
        } => local::up(&ctx, build, &services),
        Cmd::Down { purge, services } => local::down(&ctx, purge, &services),
        Cmd::Ps => local::ps(&ctx),
        Cmd::Logs { follow, services } => local::logs(&ctx, follow, &services),
        Cmd::Backup { dest } => local::backup(&ctx, dest),
        Cmd::Restore { drop, from } => local::restore(&ctx, &from, drop),
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
