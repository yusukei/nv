//! wrangler-compose: Cloudflare Workers environments, run like docker compose.

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

fn main() {
    let cli = Cli::parse();
    interrupt::install();
    if let Err(e) = dispatch(cli) {
        eprintln!("wrangler-compose: error: {e:#}");
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
        bail!("choose an environment with --env <name> (or CLOUDFLARE_ENV); nothing was done");
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
