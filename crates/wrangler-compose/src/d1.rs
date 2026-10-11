//! D1 through Wrangler: which databases exist, migrations, tables.

use crate::project::{D1, Project, has_migrations};
use anyhow::{Result, anyhow, bail};
use serde_json::Value;

/// Remote databases in the account, as (name, id).
pub fn remote(project: &Project) -> Result<Vec<(String, String)>> {
    let v = project.wrangler_json(["d1", "list", "--json"])?;
    let list = v
        .as_array()
        .ok_or_else(|| anyhow!("unexpected output from `d1 list --json`"))?;
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
            bail!(
                "D1 {} ({}) has database_id {id} in the Wrangler config, but no such database exists; Wrangler only creates D1 databases written by name, so remove database_id or create it",
                d1.binding,
                d1.name
            );
        }
    }
    Ok(())
}

pub fn apply_migrations(project: &Project) -> Result<()> {
    for d1 in &project.config.d1 {
        if has_migrations(d1) {
            eprintln!(
                "wrangler-compose: applying migrations to D1 {} ({})",
                d1.binding, d1.name
            );
            project.wrangler(["d1", "migrations", "apply", &d1.binding, "--remote"])?;
        }
    }
    Ok(())
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
        };
        let by_id = D1 {
            binding: "DB".into(),
            name: "other".into(),
            id: Some("uuid-1".into()),
            migrations_dir: "m".into(),
        };
        let missing = D1 {
            binding: "DB".into(),
            name: "app-db".into(),
            id: Some("uuid-2".into()),
            migrations_dir: "m".into(),
        };
        assert!(exists(&by_name, &remote));
        assert!(exists(&by_id, &remote));
        assert!(!exists(&missing, &remote));
    }
}
