//! `nv add` / `nv remove` (REQ-012, DES-014): npm changes package.json and
//! package-lock.json; nv supplies the pinned Node and keeps its stamp right.

use crate::exec;
use crate::node;
use crate::paths::NvHome;
use crate::project::Project;
use crate::sync;
use anyhow::{Context, Result, bail};
use serde_json::Value;

pub fn add(home: &NvHome, packages: &[String], dev: bool, exact: bool) -> Result<()> {
    let project = Project::find()?;
    let node = node::ensure(home, project.node_version()?)?;
    let _lock = sync::lock_project(home, &project)?;
    let mut args = vec!["install", if dev { "--save-dev" } else { "--save" }];
    if exact {
        args.push("--save-exact");
    }
    args.extend(["--no-audit", "--no-fund"]);
    args.extend(packages.iter().map(String::as_str));
    exec::npm(home, &node, &project.dir, &args)?;
    sync::record(&project, &node)?;
    eprintln!(
        "nv: {}",
        tr!(
            "依存に足しました: {}",
            "added to dependencies: {}",
            packages.join(", ")
        )
    );
    Ok(())
}

pub fn remove(home: &NvHome, packages: &[String]) -> Result<()> {
    let project = Project::find()?;
    // npm quietly "succeeds" for names that are not dependencies; say so instead.
    let declared = declared_dependencies(&project)?;
    let missing: Vec<&String> = packages.iter().filter(|p| !declared.contains(p)).collect();
    if !missing.is_empty() {
        bail!(tr!(
            "{} は package.json の依存にありません。何も変えていません",
            "{} not in package.json dependencies; nothing was changed",
            missing
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let node = node::ensure(home, project.node_version()?)?;
    let _lock = sync::lock_project(home, &project)?;
    let mut args = vec!["uninstall", "--no-audit", "--no-fund"];
    args.extend(packages.iter().map(String::as_str));
    exec::npm(home, &node, &project.dir, &args)?;
    sync::record(&project, &node)?;
    eprintln!(
        "nv: {}",
        tr!(
            "依存から外しました: {}",
            "removed from dependencies: {}",
            packages.join(", ")
        )
    );
    Ok(())
}

fn declared_dependencies(project: &Project) -> Result<Vec<String>> {
    let path = project.dir.join("package.json");
    let text = std::fs::read_to_string(&path)
        .with_context(|| tr!("{} を読めません", "cannot read {}", path.display()))?;
    let json: Value = serde_json::from_str(&text).with_context(|| {
        tr!(
            "{} の JSON が正しくありません",
            "{} is not valid JSON",
            path.display()
        )
    })?;
    Ok(names_in(&json))
}

fn names_in(json: &Value) -> Vec<String> {
    ["dependencies", "devDependencies", "optionalDependencies"]
        .iter()
        .filter_map(|k| json.get(k).and_then(Value::as_object))
        .flat_map(|m| m.keys().cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collects_declared_dependency_names() {
        let pkg = json!({
            "dependencies": {"semver": "^7.6.3"},
            "devDependencies": {"typescript": "5.6.3"},
            "optionalDependencies": {"fsevents": "*"},
            "peerDependencies": {"react": "*"}
        });
        let mut names = names_in(&pkg);
        names.sort();
        assert_eq!(names, vec!["fsevents", "semver", "typescript"]);
    }
}
