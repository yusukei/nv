//! D1 through Wrangler: which databases exist, migrations, tables.

use crate::project::{D1, Project, has_migrations};
use anyhow::{Result, anyhow, bail};
use serde_json::Value;

/// Remote databases in the account, as (name, id).
pub fn remote(project: &Project) -> Result<Vec<(String, String)>> {
    let v = project.wrangler_json(["d1", "list", "--json"])?;
    let list = v.as_array().ok_or_else(|| {
        anyhow!(crate::tr!(
            "`d1 list --json` の出力が想定と違います",
            "unexpected output from `d1 list --json`"
        ))
    })?;
    Ok(list
        .iter()
        .filter_map(|d| {
            Some((
                d["name"].as_str()?.to_string(),
                d["uuid"].as_str()?.to_string(),
            ))
        })
        .collect())
}

pub fn exists(d1: &D1, remote: &[(String, String)]) -> bool {
    match &d1.id {
        Some(id) => remote.iter().any(|(_, rid)| rid == id),
        None => remote.iter().any(|(name, _)| name == &d1.name),
    }
}

/// Stop if a D1 pinned by database_id is missing: Wrangler will not create
/// it (REQ-004).
pub fn require_pinned_exist(project: &Project, remote: &[(String, String)]) -> Result<()> {
    for d1 in &project.config.d1 {
        if let Some(id) = &d1.id
            && !exists(d1, remote)
        {
            bail!(crate::tr!(
                "D1 {} ({}) は Wrangler の設定に database_id {id} が書いてありますが、その D1 がありません。Wrangler が作るのは名前だけで書いた D1 なので、database_id を外すか、D1 を作ってください",
                "D1 {} ({}) has database_id {id} in the Wrangler config, but no such database exists; Wrangler only creates D1 databases written by name, so remove database_id or create it",
                d1.binding,
                d1.name
            ));
        }
    }
    Ok(())
}

pub fn apply_migrations(project: &Project) -> Result<()> {
    for d1 in &project.config.d1 {
        if has_migrations(d1) {
            eprintln!(
                "wrangler-compose: {}",
                crate::tr!(
                    "D1 {} ({}) に移行を当てます",
                    "applying migrations to D1 {} ({})",
                    d1.binding,
                    d1.name
                )
            );
            project.wrangler(["d1", "migrations", "apply", &d1.binding, "--remote"])?;
        }
    }
    Ok(())
}

/// Migration file names in the folder, in the order Wrangler applies them.
fn local_migrations(d1: &D1) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(&d1.migrations_dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .filter(|n| n.ends_with(".sql"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// For a database that does not exist yet, every migration is pending.
pub fn pending_migrations_all(d1: &D1) -> Vec<String> {
    local_migrations(d1)
}

/// Migrations not applied yet, read without writing anything: `d1 migrations
/// list` would create the history table when it is missing (DES-016).
pub fn pending_migrations(project: &Project, d1: &D1) -> Result<Vec<String>> {
    let local = local_migrations(d1);
    let table = d1.migrations_table.replace('\'', "''");
    let check = format!("SELECT name FROM sqlite_master WHERE type='table' AND name='{table}'");
    let v = project.wrangler_json([
        "d1",
        "execute",
        &d1.binding,
        "--remote",
        "--json",
        "--command",
        &check,
    ])?;
    if names_from_execute(&v).is_empty() {
        return Ok(local);
    }
    let quoted = d1.migrations_table.replace('"', "\"\"");
    let v = project.wrangler_json([
        "d1",
        "execute",
        &d1.binding,
        "--remote",
        "--json",
        "--command",
        &format!("SELECT name FROM \"{quoted}\""),
    ])?;
    let applied = names_from_execute(&v);
    Ok(local.into_iter().filter(|n| !applied.contains(n)).collect())
}

/// Tables a user created: everything but SQLite's and D1's own (`sqlite_*`, `_cf_*`).
pub fn user_tables(project: &Project, d1: &D1) -> Result<Vec<String>> {
    let sql = "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '\\_cf\\_%' ESCAPE '\\'";
    let v = project.wrangler_json([
        "d1",
        "execute",
        &d1.binding,
        "--remote",
        "--json",
        "--command",
        sql,
    ])?;
    Ok(names_from_execute(&v))
}

fn names_from_execute(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .flat_map(|r| r["results"].as_array().cloned().unwrap_or_default())
        .filter_map(|row| row["name"].as_str().map(str::to_string))
        .collect()
}

/// Drop every user table, view and trigger in one batch (DES-013).
pub fn drop_everything(project: &Project, d1: &D1) -> Result<()> {
    let sql = "SELECT type, name FROM sqlite_master WHERE type IN ('table','view','trigger') AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '\\_cf\\_%' ESCAPE '\\'";
    let v = project.wrangler_json([
        "d1",
        "execute",
        &d1.binding,
        "--remote",
        "--json",
        "--command",
        sql,
    ])?;
    let rows: Vec<(String, String)> = v
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|r| r["results"].as_array().cloned().unwrap_or_default())
        .filter_map(|row| {
            Some((
                row["type"].as_str()?.to_string(),
                row["name"].as_str()?.to_string(),
            ))
        })
        .collect();
    if rows.is_empty() {
        return Ok(());
    }
    let mut stmt = String::from("PRAGMA defer_foreign_keys=ON;");
    // Triggers and views first, then tables.
    for kind in ["trigger", "view", "table"] {
        for (t, name) in rows.iter().filter(|(t, _)| t == kind) {
            stmt.push_str(&format!(
                " DROP {} IF EXISTS \"{}\";",
                t.to_uppercase(),
                name.replace('"', "\"\"")
            ));
        }
    }
    project.wrangler([
        "d1",
        "execute",
        &d1.binding,
        "--remote",
        "--yes",
        "--command",
        &stmt,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_names_from_execute_output() {
        let v =
            json!([{"results": [{"name": "items"}, {"name": "d1_migrations"}], "success": true}]);
        assert_eq!(names_from_execute(&v), vec!["items", "d1_migrations"]);
        assert!(names_from_execute(&json!([{"results": []}])).is_empty());
    }

    #[test]
    fn matches_by_id_when_pinned_and_by_name_otherwise() {
        let remote = vec![("app-db".to_string(), "uuid-1".to_string())];
        let by_name = D1 {
            binding: "DB".into(),
            name: "app-db".into(),
            id: None,
            migrations_dir: "m".into(),
            migrations_table: "d1_migrations".into(),
        };
        let by_id = D1 {
            binding: "DB".into(),
            name: "other".into(),
            id: Some("uuid-1".into()),
            migrations_dir: "m".into(),
            migrations_table: "d1_migrations".into(),
        };
        let missing = D1 {
            binding: "DB".into(),
            name: "app-db".into(),
            id: Some("uuid-2".into()),
            migrations_dir: "m".into(),
            migrations_table: "d1_migrations".into(),
        };
        assert!(exists(&by_name, &remote));
        assert!(exists(&by_id, &remote));
        assert!(!exists(&missing, &remote));
    }
}
