//! nv: Node.js and npm, managed per project in the style of uv.

#[macro_use]
mod i18n;
mod deps;
mod exec;
mod http;
mod node;
mod paths;
mod project;
mod run;
mod selfupdate;
mod skill;
mod sync;
mod version;
mod x;

use anyhow::Result;
use clap::{Parser, Subcommand};
use paths::NvHome;
use std::ffi::OsString;
use version::Version;

#[derive(Parser)]
#[command(
    name = "nv",
    version,
    about = "Node.js and npm, pinned per project",
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Install the pinned Node and exactly the dependencies in package-lock.json
    Sync,
    /// Run a command with the pinned Node, npm and the project's tools first on PATH
    // Every argument after the program belongs to it, including --help/--version.
    #[command(disable_help_flag = true, disable_version_flag = true)]
    Run {
        program: OsString,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Run a tool at an exact version without adding it to the project
    #[command(disable_help_flag = true, disable_version_flag = true)]
    X {
        /// Node version to use outside a project
        #[arg(long)]
        node: Option<Version>,
        /// <package>@<exact version>
        package: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Add dependencies to package.json and package-lock.json
    Add {
        /// <package>[@version] (ranges are fine; package-lock.json pins them)
        #[arg(required = true)]
        packages: Vec<String>,
        /// Add as development dependencies
        #[arg(long)]
        dev: bool,
        /// Write the exact version instead of a range
        #[arg(long)]
        exact: bool,
    },
    /// Remove dependencies from package.json and package-lock.json
    Remove {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Manage downloaded Node versions
    Node {
        #[command(subcommand)]
        command: NodeCmd,
    },
    /// Install agent skills (Claude Code, Codex) for nv and wrangler-compose
    Install {
        #[command(subcommand)]
        what: InstallWhat,
    },
    /// Remove agent skills installed by nv
    Uninstall {
        #[command(subcommand)]
        what: UninstallWhat,
    },
    /// Manage nv itself
    #[command(name = "self")]
    Myself {
        #[command(subcommand)]
        command: SelfCmd,
    },
}

#[derive(Subcommand)]
enum NodeCmd {
    /// List downloaded Node versions; with --remote, the versions you can install
    List {
        /// List versions available from the Node distribution site
        #[arg(long)]
        remote: bool,
        /// Only this major line, every version (e.g. 24)
        #[arg(requires = "remote")]
        major: Option<u64>,
        /// Every version, not just the newest of each major line
        #[arg(long, requires = "remote")]
        all: bool,
        /// Only LTS versions
        #[arg(long, requires = "remote")]
        lts: bool,
    },
    /// Download a Node version
    Install {
        #[arg(id = "node_version", value_name = "VERSION")]
        version: Version,
    },
    /// Remove a downloaded Node version
    Uninstall {
        #[arg(id = "node_version", value_name = "VERSION")]
        version: Version,
    },
}

#[derive(Subcommand)]
enum InstallWhat {
    /// Install the skills user-wide
    Skill {
        /// Only refresh skills nv installed before
        #[arg(long, hide = true)]
        if_installed: bool,
    },
}

#[derive(Subcommand)]
enum UninstallWhat {
    /// Remove the skills nv installed
    Skill,
}

#[derive(Subcommand)]
enum SelfCmd {
    /// Replace nv (and wrangler-compose next to it) with the latest release
    Update {
        /// Only report whether a newer version exists
        #[arg(long)]
        check: bool,
    },
}

/// Japanese `--help` (REQ-011): the derive's English texts, replaced per
/// command path ("" is the root, "node install" a nested one).
const JA_ABOUT: &[(&str, &str)] = &[
    ("", "プロジェクトごとに版を固定して Node.js と npm を使う"),
    (
        "sync",
        "固定した版の Node と、package-lock.json のとおりの依存を入れる",
    ),
    (
        "run",
        "固定した Node・npm とプロジェクトの道具を優先してコマンドを動かす",
    ),
    (
        "x",
        "版をそのまま指定した道具を、プロジェクトに入れずに動かす",
    ),
    (
        "add",
        "依存を足し、package.json と package-lock.json に書く",
    ),
    (
        "remove",
        "依存を外し、package.json と package-lock.json から消す",
    ),
    ("node", "取った Node の版を管理する"),
    (
        "node list",
        "取った Node の版を並べる。--remote なら入れられる版を並べる",
    ),
    ("node install", "Node の版を取る"),
    ("node uninstall", "取った Node の版を消す"),
    (
        "install",
        "エージェント(Claude Code・Codex)に nv と wrangler-compose のスキルを入れる",
    ),
    ("install skill", "利用者全体にスキルを入れる"),
    ("uninstall", "nv が入れたスキルを消す"),
    ("uninstall skill", "nv が入れたスキルを消す"),
    ("self", "nv 自身を管理する"),
    (
        "self update",
        "nv(と同じフォルダの wrangler-compose)を最新の Release に入れ替える",
    ),
];
const JA_ARGS: &[(&str, &str, &str)] = &[
    ("x", "node", "プロジェクトの外で使う Node の版"),
    ("x", "package", "<パッケージ>@<版>"),
    ("x", "args", "道具に渡す引数"),
    (
        "add",
        "packages",
        "<パッケージ>[@版](幅のある書き方も可。package-lock.json が固定する)",
    ),
    ("add", "dev", "開発用の依存として足す"),
    ("add", "exact", "版を幅なしで書く"),
    ("remove", "packages", "外すパッケージの名前"),
    (
        "node list",
        "remote",
        "Node の配布元から、入れられる版を並べる",
    ),
    ("node list", "major", "この系列だけを、すべての版で(例: 24)"),
    ("node list", "all", "系列ごとの最新だけでなく、すべての版"),
    ("node list", "lts", "LTS の版だけ"),
    ("self update", "check", "新しい版があるかだけを示す"),
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
    // Keep the declared order once headings change.
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
        });
    // clap's own -h/-V carry English texts; replace them where they exist.
    if !cmd.is_disable_help_flag_set() {
        cmd = cmd.disable_help_flag(true).arg(
            clap::Arg::new("help")
                .short('h')
                .long("help")
                .action(clap::ArgAction::Help)
                .help("ヘルプを表示する")
                .help_heading("オプション"),
        );
    }
    if path.is_empty() {
        cmd = cmd
            .propagate_version(false)
            .version(env!("CARGO_PKG_VERSION"))
            .disable_version_flag(true)
            .arg(
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

/// `nv node list --remote` (REQ-006): one release per line on stdout.
fn list_remote(home: &NvHome, major: Option<u64>, all: bool, lts: bool) -> Result<()> {
    let mut releases = node::remote(None)?;
    if let Some(m) = major {
        releases.retain(|r| r.version.major == m);
    }
    if lts {
        releases.retain(|r| r.lts.is_some());
    }
    if major.is_none() && !all {
        releases = node::latest_per_major(releases);
    }
    if releases.is_empty() {
        eprintln!(
            "nv: {}",
            tr!("当てはまる版はありません", "no matching versions")
        );
        return Ok(());
    }
    let installed = node::installed(home)?;
    for r in releases {
        let mut line = format!("{:<10}", r.version.to_string());
        if let Some(name) = &r.lts {
            line.push_str(&format!("  LTS {name:<10}"));
        } else {
            line.push_str(&" ".repeat(16));
        }
        if installed.contains(&r.version) {
            line.push_str(&format!("  {}", tr!("(入れてある)", "(installed)")));
        }
        println!("{}", line.trim_end());
    }
    Ok(())
}

fn main() {
    let cli = parse_cli();
    if let Err(e) = dispatch(cli) {
        eprintln!("nv: {} {e:#}", tr!("エラー:", "error:"));
        std::process::exit(2);
    }
}

fn dispatch(cli: Cli) -> Result<()> {
    selfupdate::clean_old();
    let home = NvHome::locate()?;
    home.clean_tmp();
    match cli.command {
        Cmd::Sync => {
            let project = project::Project::find()?;
            sync::ensure(&home, &project, true)?;
        }
        Cmd::Run { program, args } => {
            run::run(&home, &program, &args)?;
        }
        Cmd::X {
            node,
            package,
            args,
        } => {
            x::run(&home, &package, node, &args)?;
        }
        Cmd::Add {
            packages,
            dev,
            exact,
        } => deps::add(&home, &packages, dev, exact)?,
        Cmd::Remove { packages } => deps::remove(&home, &packages)?,
        Cmd::Node { command } => match command {
            NodeCmd::List { remote: false, .. } => {
                for v in node::installed(&home)? {
                    println!("{v}");
                }
            }
            NodeCmd::List {
                remote: true,
                major,
                all,
                lts,
            } => list_remote(&home, major, all, lts)?,
            NodeCmd::Install { version } => {
                node::ensure(&home, version)?;
            }
            NodeCmd::Uninstall { version } => {
                if node::uninstall(&home, version)? {
                    eprintln!(
                        "nv: {}",
                        tr!("Node {version} を消しました", "removed Node {version}")
                    );
                } else {
                    eprintln!(
                        "nv: {}",
                        tr!(
                            "Node {version} は入っていません",
                            "Node {version} is not installed"
                        )
                    );
                }
            }
        },
        Cmd::Install {
            what: InstallWhat::Skill { if_installed },
        } => skill::install(if_installed)?,
        Cmd::Uninstall {
            what: UninstallWhat::Skill,
        } => skill::uninstall()?,
        Cmd::Myself {
            command: SelfCmd::Update { check },
        } => selfupdate::update(check)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn cli_definition_is_consistent() {
        use clap::CommandFactory;
        super::Cli::command().debug_assert();
    }
}
