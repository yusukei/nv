//! nv: Node.js and npm, managed per project in the style of uv.

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
    /// List downloaded Node versions
    List,
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

fn main() {
    let cli = Cli::parse();
    if let Err(e) = dispatch(cli) {
        eprintln!("nv: error: {e:#}");
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
        Cmd::Node { command } => match command {
            NodeCmd::List => {
                for v in node::installed(&home)? {
                    println!("{v}");
                }
            }
            NodeCmd::Install { version } => {
                node::ensure(&home, version)?;
            }
            NodeCmd::Uninstall { version } => {
                if node::uninstall(&home, version)? {
                    eprintln!("nv: removed Node {version}");
                } else {
                    eprintln!("nv: Node {version} is not installed");
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
