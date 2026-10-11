//! `nv install skill` / `nv uninstall skill` (DES-009).

use crate::paths::{home_dir, remove_any};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const MARKER: &str = ".nv-managed";

struct Skill {
    name: &'static str,
    body: &'static str,
}

const SKILLS: &[Skill] = &[
    Skill {
        name: "nv",
        body: include_str!("../skills/nv/SKILL.md"),
    },
    Skill {
        name: "wrangler-compose",
        body: include_str!("../skills/wrangler-compose/SKILL.md"),
    },
];

struct Agent {
    label: &'static str,
    /// Skills folder, if this agent is present.
    dir: Option<PathBuf>,
    why_absent: &'static str,
}

fn agents(home: &Path) -> Vec<Agent> {
    let claude = home.join(".claude");
    vec![
        // Codex reads user skills from ~/.agents/skills, shared with other agents (DES-009).
        Agent {
            label: "Codex (~/.agents/skills)",
            dir: Some(home.join(".agents/skills")),
            why_absent: "",
        },
        Agent {
            label: "Claude Code",
            dir: claude.is_dir().then(|| claude.join("skills")),
            why_absent: "~/.claude",
        },
    ]
}

/// Install (or refresh) every skill. With `only_if_installed`, touch only
/// folders nv installed before; used after `nv self update`.
pub fn install(only_if_installed: bool) -> Result<()> {
    let home = home_dir()?;
    for agent in agents(&home) {
        let Some(dir) = agent.dir else {
            if !only_if_installed {
                eprintln!(
                    "nv: {}",
                    crate::tr!(
                        "{} には入れませんでした({} がありません)",
                        "skipped {}: {} not found",
                        agent.label,
                        agent.why_absent
                    )
                );
            }
            continue;
        };
        for skill in SKILLS {
            let dest = dir.join(skill.name);
            let ours = dest.join(MARKER).is_file();
            if only_if_installed && !ours {
                continue;
            }
            if dest.exists() && !ours {
                eprintln!(
                    "nv: {}",
                    crate::tr!(
                        "{} は nv が入れたものではないので、触らずに残しました",
                        "left {} alone: it exists and was not installed by nv",
                        dest.display()
                    )
                );
                continue;
            }
            write_skill(&dir, skill).with_context(|| {
                crate::tr!(
                    "{} にスキルを入れられません",
                    "cannot install the skill into {}",
                    dest.display()
                )
            })?;
            eprintln!(
                "nv: {}",
                crate::tr!(
                    "スキル {} を {} 向けに {} へ入れました",
                    "installed skill {} for {} at {}",
                    skill.name,
                    agent.label,
                    dest.display()
                )
            );
        }
    }
    Ok(())
}

fn write_skill(dir: &Path, skill: &Skill) -> Result<()> {
    fs::create_dir_all(dir)?;
    let dest = dir.join(skill.name);
    let staging = dir.join(format!(".{}.nv-staging", skill.name));
    remove_any(&staging)?;
    fs::create_dir(&staging)?;
    fs::write(staging.join("SKILL.md"), skill.body)?;
    fs::write(
        staging.join(MARKER),
        concat!(env!("CARGO_PKG_VERSION"), "\n"),
    )?;
    remove_any(&dest)?;
    fs::rename(&staging, &dest)?;
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let home = home_dir()?;
    let mut removed = 0;
    for agent in agents(&home) {
        let Some(dir) = agent.dir else { continue };
        for skill in SKILLS {
            let dest = dir.join(skill.name);
            if dest.join(MARKER).is_file() {
                remove_any(&dest).with_context(|| {
                    crate::tr!("{} を消せません", "cannot remove {}", dest.display())
                })?;
                eprintln!(
                    "nv: {}",
                    crate::tr!(
                        "スキル {} を {} 向けの {} から消しました",
                        "removed skill {} for {} from {}",
                        skill.name,
                        agent.label,
                        dest.display()
                    )
                );
                removed += 1;
            }
        }
    }
    if removed == 0 {
        eprintln!(
            "nv: {}",
            crate::tr!(
                "nv が入れたスキルはありませんでした",
                "no skills installed by nv were found"
            )
        );
    }
    Ok(())
}
