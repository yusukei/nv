//! Data operations through the data service (REQ-011, REQ-013, REQ-017,
//! DES-011, DES-014): backup, restore, purge counts and migrations. The
//! service runs inside the environment; this side only moves files.
//!
//! A backup is a folder with one file per D1 database (JSON lines: schema,
//! rows), one per KV namespace, the R2 objects, and `manifest.json` written
//! last. The same shape is used for every environment, so any backup can
//! be restored into any environment.

use crate::compose::{Compose, Kind};
use crate::files::hex;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MANIFEST: &str = "manifest.json";
const FORMAT: u64 = 2;

fn say(msg: String) {
    eprintln!("wrangler-compose: {msg}");
}

/// Talks to the data service of one environment.
pub struct Client {
    base: String,
    token: String,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(base: String, token: String) -> Client {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(300)))
            // Never route the tool's own local requests through a proxy.
            .proxy(None)
            .build()
            .into();
        Client { base, token, agent }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/__wc/{path}", self.base)
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let mut res = self
            .agent
            .post(self.url(path))
            .header("authorization", format!("Bearer {}", self.token))
            .header("content-type", "application/json")
            .send(serde_json::to_vec(body)?.as_slice())
            .context(crate::tr!(
                "データのサービスにつながりません",
                "cannot reach the data service"
            ))?;
        let status = res.status().as_u16();
        let text = res.body_mut().read_to_string().unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status != 200 {
            bail!(
                "{}",
                v["error"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("HTTP {status}"))
            );
        }
        Ok(v)
    }

    fn get_json(&self, path: &str) -> Result<Value> {
        let mut res = self
            .agent
            .get(self.url(path))
            .header("authorization", format!("Bearer {}", self.token))
            .call()
            .context(crate::tr!(
                "データのサービスにつながりません",
                "cannot reach the data service"
            ))?;
        let status = res.status().as_u16();
        let text = res.body_mut().read_to_string().unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status != 200 {
            bail!(
                "{}",
                v["error"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("HTTP {status}"))
            );
        }
        Ok(v)
    }

    /// One look for R2 event notifications (the data service sends them).
    pub fn notify_tick(&self) -> Result<()> {
        self.post("notify/tick", &json!({}))?;
        Ok(())
    }

    pub fn query(
        &self,
        db: &str,
        sql: &str,
        params: Vec<Value>,
    ) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
        let v = self.post(
            &format!("d1/{db}/query"),
            &json!({ "sql": sql, "params": params }),
        )?;
        let cols = v["columns"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c.as_str().map(str::to_string))
            .collect();
        let rows = v["rows"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r.as_array().cloned())
            .collect();
        Ok((cols, rows))
    }

    /// Statements run as one transaction.
    pub fn batch(&self, db: &str, statements: Vec<(String, Vec<Value>)>) -> Result<()> {
        if statements.is_empty() {
            return Ok(());
        }
        let st: Vec<Value> = statements
            .into_iter()
            .map(|(s, p)| json!({ "sql": s, "params": p }))
            .collect();
        self.post(&format!("d1/{db}/batch"), &json!({ "statements": st }))?;
        Ok(())
    }

    pub fn r2_list(
        &self,
        bucket: &str,
        cursor: Option<&str>,
    ) -> Result<(Vec<Value>, Option<String>)> {
        let q = cursor
            .map(|c| format!("?cursor={}", enc(c)))
            .unwrap_or_default();
        let v = self.get_json(&format!("r2/{bucket}/list{q}"))?;
        Ok((
            v["objects"].as_array().cloned().unwrap_or_default(),
            v["cursor"].as_str().map(str::to_string),
        ))
    }

    /// Download one object into `dest`; returns (sha256, size, metadata).
    pub fn r2_get(&self, bucket: &str, key: &str, dest: &Path) -> Result<(String, u64, Value)> {
        let res = self
            .agent
            .get(self.url(&format!("r2/{bucket}/object/{}", enc(key))))
            .header("authorization", format!("Bearer {}", self.token))
            .call()?;
        if res.status().as_u16() != 200 {
            bail!(crate::tr!(
                "{bucket} の {key} を読めません",
                "cannot read {key} from {bucket}"
            ));
        }
        let meta: Value = res
            .headers()
            .get("x-wc-meta")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(Value::Null);
        let mut reader = res.into_body().into_reader();
        let (hash, size) = copy_hashing(&mut reader, dest)?;
        Ok((hash, size, meta))
    }

    pub fn r2_put(&self, bucket: &str, key: &str, src: &Path, meta: &Value) -> Result<()> {
        let file = File::open(src)
            .with_context(|| crate::tr!("{} を開けません", "cannot open {}", src.display()))?;
        let len = file.metadata()?.len();
        let mut res = self
            .agent
            .put(self.url(&format!("r2/{bucket}/object/{}", enc(key))))
            .header("authorization", format!("Bearer {}", self.token))
            .header("content-length", len.to_string())
            .header("x-wc-meta", meta.to_string())
            .send(ureq::SendBody::from_reader(&mut BufReader::new(file)))?;
        if res.status().as_u16() != 200 {
            let t = res.body_mut().read_to_string().unwrap_or_default();
            bail!(crate::tr!(
                "{bucket} へ {key} を書けません: {t}",
                "cannot write {key} to {bucket}: {t}"
            ));
        }
        Ok(())
    }

    pub fn r2_delete(&self, bucket: &str, keys: &[String]) -> Result<()> {
        self.post(&format!("r2/{bucket}/delete"), &json!({ "keys": keys }))?;
        Ok(())
    }

    pub fn kv_list(&self, ns: &str, cursor: Option<&str>) -> Result<(Vec<Value>, Option<String>)> {
        let q = cursor
            .map(|c| format!("?cursor={}", enc(c)))
            .unwrap_or_default();
        let v = self.get_json(&format!("kv/{ns}/list{q}"))?;
        Ok((
            v["keys"].as_array().cloned().unwrap_or_default(),
            v["cursor"].as_str().map(str::to_string),
        ))
    }

    pub fn kv_get(&self, ns: &str, key: &str) -> Result<Option<(Vec<u8>, Value)>> {
        let res = self
            .agent
            .get(self.url(&format!("kv/{ns}/value/{}", enc(key))))
            .header("authorization", format!("Bearer {}", self.token))
            .call()?;
        if res.status().as_u16() == 404 {
            return Ok(None);
        }
        let meta: Value = res
            .headers()
            .get("x-wc-meta")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(Value::Null);
        let mut body = Vec::new();
        res.into_body().into_reader().read_to_end(&mut body)?;
        Ok(Some((body, meta)))
    }

    pub fn kv_put(&self, ns: &str, key: &str, value: &[u8], meta: &Value) -> Result<()> {
        let mut res = self
            .agent
            .put(self.url(&format!("kv/{ns}/value/{}", enc(key))))
            .header("authorization", format!("Bearer {}", self.token))
            .header("x-wc-meta", meta.to_string())
            .send(value)?;
        if res.status().as_u16() != 200 {
            let t = res.body_mut().read_to_string().unwrap_or_default();
            bail!(crate::tr!(
                "{ns} へ {key} を書けません: {t}",
                "cannot write {key} to {ns}: {t}"
            ));
        }
        Ok(())
    }

    pub fn kv_delete(&self, ns: &str, key: &str) -> Result<()> {
        let res = self
            .agent
            .delete(self.url(&format!("kv/{ns}/value/{}", enc(key))))
            .header("authorization", format!("Bearer {}", self.token))
            .call()?;
        if res.status().as_u16() != 200 {
            bail!(crate::tr!(
                "{ns} の {key} を消せません",
                "cannot delete {key} from {ns}"
            ));
        }
        Ok(())
    }
}

/// Percent-encode everything but unreserved characters and `/`.
fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn copy_hashing(reader: &mut dyn Read, dest: &Path) -> Result<(String, u64)> {
    let mut file = BufWriter::new(
        File::create(dest)
            .with_context(|| crate::tr!("{} を作れません", "cannot create {}", dest.display()))?,
    );
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        size += n as u64;
    }
    file.flush()?;
    Ok((hex(&h.finalize()), size))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path)
        .with_context(|| crate::tr!("{} を開けません", "cannot open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

/// Every D1/R2/KV resource of the compose file: (type, key).
pub fn resources(c: &Compose) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for s in &c.services {
        match &s.kind {
            Kind::D1(l) => out.extend(l.iter().map(|r| ("d1", r.key.clone()))),
            Kind::R2(l) => out.extend(l.iter().map(|r| ("r2", r.key.clone()))),
            Kind::Kv(l) => out.extend(l.iter().map(|r| ("kv", r.key.clone()))),
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------- D1

/// User tables, views and triggers (not SQLite's or Cloudflare's own).
fn d1_objects(client: &Client, db: &str) -> Result<Vec<(String, String, Option<String>)>> {
    let (_, rows) = client.query(
        db,
        "SELECT type, name, sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' AND name NOT LIKE '_cf_%' ORDER BY rowid",
        vec![],
    )?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.first()
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                r.get(1)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                r.get(2).and_then(Value::as_str).map(str::to_string),
            )
        })
        .collect())
}

pub fn d1_table_count(client: &Client, db: &str) -> Result<usize> {
    Ok(d1_objects(client, db)?
        .iter()
        .filter(|(t, _, _)| t == "table")
        .count())
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn dump_d1(client: &Client, db: &str, dest: &Path) -> Result<u64> {
    let objects = d1_objects(client, db)?;
    if let Some((_, name, _)) = objects.iter().find(|(t, _, sql)| {
        t == "table"
            && sql
                .as_deref()
                .is_some_and(|s| s.to_uppercase().starts_with("CREATE VIRTUAL"))
    }) {
        bail!(crate::tr!(
            "D1 {db} の表 {name} は仮想表(FTS など)なので控えられません",
            "D1 {db}: table {name} is a virtual table (FTS etc.), which cannot be backed up"
        ));
    }
    let mut out = BufWriter::new(File::create(dest)?);
    let mut rows_written = 0u64;
    for (_, name, sql) in objects.iter().filter(|(t, _, _)| t == "table") {
        writeln!(out, "{}", json!({ "schema": sql }))?;
        let mut offset = 0usize;
        loop {
            let (cols, rows) = client.query(
                db,
                &format!("SELECT * FROM {} LIMIT 500 OFFSET {offset}", quote(name)),
                vec![],
            )?;
            for r in &rows {
                writeln!(out, "{}", json!({ "t": name, "c": cols, "r": r }))?;
            }
            rows_written += rows.len() as u64;
            if rows.len() < 500 {
                break;
            }
            offset += 500;
        }
    }
    for (_, _, sql) in objects
        .iter()
        .filter(|(t, _, sql)| t != "table" && sql.is_some())
    {
        writeln!(out, "{}", json!({ "post": sql }))?;
    }
    out.flush()?;
    Ok(rows_written)
}

fn drop_d1(client: &Client, db: &str) -> Result<()> {
    let objects = d1_objects(client, db)?;
    let mut st = vec![("PRAGMA defer_foreign_keys = true".to_string(), vec![])];
    for kind in ["view", "trigger", "table"] {
        for (t, name, _) in objects.iter().filter(|(t, _, _)| t == kind) {
            st.push((
                format!("DROP {} IF EXISTS {}", t.to_uppercase(), quote(name)),
                vec![],
            ));
        }
    }
    client.batch(db, st)
}

fn load_d1(client: &Client, db: &str, src: &Path) -> Result<u64> {
    let file = File::open(src)?;
    let mut schema = Vec::new();
    let mut post = Vec::new();
    let mut pending: Vec<(String, Vec<Value>)> = Vec::new();
    let mut rows = 0u64;
    let flush = |pending: &mut Vec<(String, Vec<Value>)>| -> Result<()> {
        if !pending.is_empty() {
            let mut st = vec![("PRAGMA defer_foreign_keys = true".to_string(), vec![])];
            st.append(pending);
            client.batch(db, st)?;
        }
        Ok(())
    };
    for line in BufReader::new(file).lines() {
        let v: Value = serde_json::from_str(&line?)?;
        if let Some(s) = v["schema"].as_str() {
            schema.push((s.to_string(), vec![]));
        } else if let Some(s) = v["post"].as_str() {
            post.push((s.to_string(), vec![]));
        } else if let Some(t) = v["t"].as_str() {
            if !schema.is_empty() {
                client.batch(db, std::mem::take(&mut schema))?;
            }
            let cols: Vec<String> = v["c"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c.as_str().map(quote))
                .collect();
            let marks = vec!["?"; cols.len()].join(", ");
            let sql = format!(
                "INSERT INTO {} ({}) VALUES ({marks})",
                quote(t),
                cols.join(", ")
            );
            pending.push((sql, v["r"].as_array().cloned().unwrap_or_default()));
            rows += 1;
            if pending.len() >= 50 {
                flush(&mut pending)?;
            }
        }
    }
    client.batch(db, schema)?;
    flush(&mut pending)?;
    client.batch(db, post)?;
    Ok(rows)
}

/// Pending migrations of every D1 with a migrations folder (for `ps`).
pub fn pending_migrations(
    ctx: &crate::ctx::Ctx,
    client: &Client,
) -> Result<std::collections::BTreeMap<String, usize>> {
    let mut out = std::collections::BTreeMap::new();
    for s in &ctx.compose.services {
        let Kind::D1(list) = &s.kind else { continue };
        for r in list {
            let Some(dir) = &r.spec.migrations else {
                continue;
            };
            let (_, has) = client.query(&r.key, "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'd1_migrations'", vec![])?;
            let applied: Vec<String> = if has
                .first()
                .and_then(|r| r.first())
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0
            {
                client
                    .query(&r.key, "SELECT name FROM d1_migrations", vec![])?
                    .1
                    .iter()
                    .filter_map(|x| x.first().and_then(Value::as_str).map(str::to_string))
                    .collect()
            } else {
                Vec::new()
            };
            let n = fs::read_dir(dir)?
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".sql") && !applied.contains(n))
                .count();
            out.insert(r.key.clone(), n);
        }
    }
    Ok(out)
}

/// Apply pending migrations through the data service (used while the
/// environment runs, so no second process touches the database). Statement
/// splitting is Wrangler's own (`unstable_splitSqlQuery`).
pub fn apply_migrations(ctx: &crate::ctx::Ctx, client: &Client) -> Result<()> {
    for s in &ctx.compose.services {
        let Kind::D1(list) = &s.kind else { continue };
        for r in list {
            let Some(dir) = &r.spec.migrations else {
                continue;
            };
            client.batch(
                &r.key,
                vec![("CREATE TABLE IF NOT EXISTS d1_migrations(id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT UNIQUE, applied_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP NOT NULL)".into(), vec![])],
            )?;
            let (_, rows) = client.query(&r.key, "SELECT name FROM d1_migrations", vec![])?;
            let applied: Vec<String> = rows
                .iter()
                .filter_map(|x| x.first().and_then(Value::as_str).map(str::to_string))
                .collect();
            let mut files: Vec<String> = fs::read_dir(dir)?
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".sql"))
                .collect();
            files.sort();
            for f in files.iter().filter(|f| !applied.contains(f)) {
                let split = ctx.node_json(SPLIT_JS, [dir.join(f)])?;
                let mut st: Vec<(String, Vec<Value>)> = split["statements"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|x| x.as_str().map(|s| (s.to_string(), vec![])))
                    .collect();
                st.push((
                    "INSERT INTO d1_migrations (name) VALUES (?)".into(),
                    vec![json!(f)],
                ));
                client.batch(&r.key, st).with_context(|| {
                    crate::tr!(
                        "{} の移行 {f} が失敗しました",
                        "migration {f} of {} failed",
                        r.key
                    )
                })?;
                say(crate::tr!(
                    "{}: 移行 {f} を当てました",
                    "{}: applied migration {f}",
                    r.key
                ));
            }
        }
    }
    Ok(())
}

const SPLIT_JS: &str = r#"
const path = require("path");
const fs = require("fs");
const req = require("module").createRequire(path.join(process.cwd(), "package.json"));
let out;
try { out = { statements: req("wrangler").unstable_splitSqlQuery(fs.readFileSync(process.argv[1], "utf8")) }; }
catch (e) { out = { error: String(e && e.message || e) }; }
process.stdout.write("\n@@WC@@" + JSON.stringify(out) + "\n");
"#;

// ---------------------------------------------------------------- R2 / KV

fn r2_all(client: &Client, bucket: &str) -> Result<Vec<Value>> {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        crate::interrupt::check()?;
        let (objs, next) = client.r2_list(bucket, cursor.as_deref())?;
        all.extend(objs);
        match next {
            Some(c) => cursor = Some(c),
            None => return Ok(all),
        }
    }
}

fn kv_all(client: &Client, ns: &str) -> Result<Vec<Value>> {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        crate::interrupt::check()?;
        let (keys, next) = client.kv_list(ns, cursor.as_deref())?;
        all.extend(keys);
        match next {
            Some(c) => cursor = Some(c),
            None => return Ok(all),
        }
    }
}

/// How much data each resource holds, for `down --purge` and `restore`.
pub fn counts(c: &Compose, client: &Client) -> Result<Vec<(&'static str, String, usize)>> {
    let mut out = Vec::new();
    for (t, key) in resources(c) {
        let n = match t {
            "d1" => d1_table_count(client, &key)?,
            "r2" => r2_all(client, &key)?.len(),
            _ => kv_all(client, &key)?.len(),
        };
        out.push((t, key, n));
    }
    Ok(out)
}

pub fn describe_count(t: &str, key: &str, n: usize) -> String {
    match t {
        "d1" => crate::tr!("D1 {key}: 表 {n}", "D1 {key}: {n} tables"),
        "r2" => crate::tr!("R2 {key}: オブジェクト {n}", "R2 {key}: {n} objects"),
        _ => crate::tr!("KV {key}: キー {n}", "KV {key}: {n} keys"),
    }
}

// ---------------------------------------------------------------- backup / restore

pub fn backup(ctx: &crate::ctx::Ctx, client: &Client, dest: Option<PathBuf>) -> Result<PathBuf> {
    let base = match dest {
        Some(d) => std::env::current_dir()?.join(d),
        None => ctx.root().join("backups"),
    };
    let dir = base.join(&ctx.env).join(timestamp());
    crate::git::require_ignored(
        ctx.root(),
        &dir,
        crate::tr!("控えの書き出し先", "the backup folder").as_str(),
    )?;
    let list = resources(&ctx.compose);
    if ctx.dry_run {
        for (t, key, n) in counts(&ctx.compose, client)? {
            say(crate::tr!(
                "書き出す: {}",
                "would back up: {}",
                describe_count(t, &key, n)
            ));
        }
        say(crate::tr!(
            "書き出し先: {} (dry-run なので何も書いていません)",
            "destination: {} (dry run: nothing was written)",
            dir.display()
        ));
        return Ok(dir);
    }
    fs::create_dir_all(&dir)
        .with_context(|| crate::tr!("{} を作れません", "cannot create {}", dir.display()))?;
    let mut entries = Vec::new();
    for (t, key) in list {
        crate::interrupt::check()?;
        match t {
            "d1" => {
                let rel = format!("d1/{key}.jsonl");
                fs::create_dir_all(dir.join("d1"))?;
                let rows = dump_d1(client, &key, &dir.join(&rel))?;
                entries.push(json!({ "type": "d1", "key": key, "file": rel, "rows": rows, "sha256": sha256_file(&dir.join(&rel))? }));
                say(crate::tr!("D1 {key}: {rows} 行", "D1 {key}: {rows} rows"));
            }
            "r2" => {
                let objs = r2_all(client, &key)?;
                let folder = format!("r2/{key}");
                fs::create_dir_all(dir.join(&folder))?;
                let mut list = Vec::new();
                for (i, o) in objs.iter().enumerate() {
                    crate::interrupt::check()?;
                    let k = o["key"].as_str().unwrap_or_default();
                    let rel = format!("{folder}/{i:08}");
                    let (sha, size, meta) = client.r2_get(&key, k, &dir.join(&rel))?;
                    list.push(
                        json!({ "key": k, "file": rel, "size": size, "sha256": sha, "meta": meta }),
                    );
                }
                say(crate::tr!(
                    "R2 {key}: オブジェクト {}",
                    "R2 {key}: {} objects",
                    list.len()
                ));
                entries.push(json!({ "type": "r2", "key": key, "objects": list }));
            }
            _ => {
                let keys = kv_all(client, &key)?;
                let rel = format!("kv/{key}.jsonl");
                fs::create_dir_all(dir.join("kv"))?;
                let mut out = BufWriter::new(File::create(dir.join(&rel))?);
                let mut n = 0;
                for k in &keys {
                    crate::interrupt::check()?;
                    let name = k["name"].as_str().unwrap_or_default();
                    if let Some((value, meta)) = client.kv_get(&key, name)? {
                        use base64_lite::encode;
                        writeln!(
                            out,
                            "{}",
                            json!({ "name": name, "value": encode(&value), "metadata": meta["metadata"], "expiration": k["expiration"] })
                        )?;
                        n += 1;
                    }
                }
                out.flush()?;
                say(crate::tr!("KV {key}: キー {n}", "KV {key}: {n} keys"));
                entries.push(json!({ "type": "kv", "key": key, "file": rel, "count": n, "sha256": sha256_file(&dir.join(&rel))? }));
            }
        }
    }
    // Written last: a folder without it is an unfinished backup (REQ-013).
    let manifest = json!({
        "format": FORMAT,
        "project": ctx.compose.name,
        "environment": ctx.env,
        "created": timestamp(),
        "resources": entries,
    });
    let tmp = dir.join(".manifest.json.partial");
    fs::write(&tmp, serde_json::to_vec_pretty(&manifest)?)?;
    fs::rename(&tmp, dir.join(MANIFEST))?;
    say(crate::tr!(
        "控えました: {}",
        "backed up to {}",
        dir.display()
    ));
    Ok(dir)
}

pub fn read_manifest(dir: &Path) -> Result<Value> {
    let path = dir.join(MANIFEST);
    if !path.is_file() {
        bail!(crate::tr!(
            "{} に {MANIFEST} がありません。控えではないか、書き出しが終わっていません(何もしていません)",
            "{} has no {MANIFEST}: it is not a backup, or the backup did not finish (nothing was done)",
            dir.display()
        ));
    }
    let v: Value = serde_json::from_slice(&fs::read(&path)?)
        .with_context(|| crate::tr!("{} が壊れています", "{} is damaged", path.display()))?;
    if v["format"].as_u64() != Some(FORMAT) {
        bail!(crate::tr!(
            "{} はこの版の wrangler-compose では読めない形です",
            "{} has a format this version of wrangler-compose cannot read",
            path.display()
        ));
    }
    Ok(v)
}

/// What `restore` will do, checked before anything changes.
pub struct RestorePlan {
    pub from_env: String,
    pub entries: Vec<Value>,
}

pub fn plan_restore(c: &Compose, from: &Path) -> Result<RestorePlan> {
    let m = read_manifest(from)?;
    let have = resources(c);
    let entries: Vec<Value> = m["resources"].as_array().cloned().unwrap_or_default();
    for e in &entries {
        let t = e["type"].as_str().unwrap_or_default();
        let key = e["key"].as_str().unwrap_or_default();
        if !have.iter().any(|(ht, hk)| *ht == t && hk == key) {
            bail!(crate::tr!(
                "控えの {t} {key} に当たるものが compose ファイルにありません(何もしていません)",
                "the backup's {t} {key} has no match in the compose file (nothing was done)"
            ));
        }
        // Check every file before anything is deleted.
        let files: Vec<(String, String)> = match t {
            "r2" => e["objects"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|o| {
                    (
                        o["file"].as_str().unwrap_or_default().to_string(),
                        o["sha256"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect(),
            _ => vec![(
                e["file"].as_str().unwrap_or_default().to_string(),
                e["sha256"].as_str().unwrap_or_default().to_string(),
            )],
        };
        for (rel, sha) in files {
            let path = from.join(&rel);
            if sha256_file(&path).ok().as_deref() != Some(sha.as_str()) {
                bail!(crate::tr!(
                    "控えの {} が無いか、目録と合いません(何もしていません)",
                    "{} in the backup is missing or does not match the manifest (nothing was done)",
                    path.display()
                ));
            }
        }
    }
    Ok(RestorePlan {
        from_env: m["environment"].as_str().unwrap_or("?").to_string(),
        entries,
    })
}

/// Delete the current contents of the backup's resources.
pub fn drop_targets(client: &Client, plan: &RestorePlan) -> Result<()> {
    for e in &plan.entries {
        let key = e["key"].as_str().unwrap_or_default();
        match e["type"].as_str().unwrap_or_default() {
            "d1" => drop_d1(client, key)?,
            "r2" => {
                let keys: Vec<String> = r2_all(client, key)?
                    .iter()
                    .filter_map(|o| o["key"].as_str().map(str::to_string))
                    .collect();
                for chunk in keys.chunks(1000) {
                    client.r2_delete(key, chunk)?;
                }
            }
            _ => {
                for k in kv_all(client, key)? {
                    client.kv_delete(key, k["name"].as_str().unwrap_or_default())?;
                }
            }
        }
    }
    Ok(())
}

/// Resources of the backup that are not empty now.
pub fn non_empty(client: &Client, plan: &RestorePlan) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for e in &plan.entries {
        let key = e["key"].as_str().unwrap_or_default();
        let t = e["type"].as_str().unwrap_or_default();
        let n = match t {
            "d1" => d1_table_count(client, key)?,
            "r2" => client.r2_list(key, None)?.0.len(),
            _ => client.kv_list(key, None)?.0.len(),
        };
        if n > 0 {
            out.push(describe_count(t, key, n));
        }
    }
    Ok(out)
}

pub fn load(client: &Client, plan: &RestorePlan, from: &Path) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for e in &plan.entries {
        crate::interrupt::check()?;
        let key = e["key"].as_str().unwrap_or_default();
        match e["type"].as_str().unwrap_or_default() {
            "d1" => {
                let rows = load_d1(
                    client,
                    key,
                    &from.join(e["file"].as_str().unwrap_or_default()),
                )?;
                say(crate::tr!(
                    "D1 {key}: {rows} 行を戻しました",
                    "D1 {key}: restored {rows} rows"
                ));
            }
            "r2" => {
                let mut n = 0;
                for o in e["objects"].as_array().into_iter().flatten() {
                    crate::interrupt::check()?;
                    let k = o["key"].as_str().unwrap_or_default();
                    client.r2_put(
                        key,
                        k,
                        &from.join(o["file"].as_str().unwrap_or_default()),
                        &o["meta"],
                    )?;
                    n += 1;
                }
                say(crate::tr!(
                    "R2 {key}: オブジェクト {n} を戻しました",
                    "R2 {key}: restored {n} objects"
                ));
            }
            _ => {
                let file = File::open(from.join(e["file"].as_str().unwrap_or_default()))?;
                let (mut n, mut expired) = (0, 0);
                for line in BufReader::new(file).lines() {
                    let v: Value = serde_json::from_str(&line?)?;
                    if v["expiration"].as_u64().is_some_and(|x| x <= now + 60) {
                        expired += 1;
                        continue;
                    }
                    let value = base64_lite::decode(v["value"].as_str().unwrap_or_default())
                        .ok_or_else(|| {
                            anyhow!(crate::tr!(
                                "KV {key} の控えが壊れています",
                                "KV {key}'s backup is damaged"
                            ))
                        })?;
                    let meta = json!({ "metadata": v["metadata"], "expiration": v["expiration"] });
                    client.kv_put(key, v["name"].as_str().unwrap_or_default(), &value, &meta)?;
                    n += 1;
                }
                say(crate::tr!(
                    "KV {key}: キー {n} を戻しました(期限切れで戻さなかったもの {expired})",
                    "KV {key}: restored {n} keys ({expired} expired, skipped)"
                ));
            }
        }
    }
    Ok(())
}

/// UTC time as yyyyMMddTHHmmssZ, without a date library.
pub fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_utc(secs)
}

fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Minimal standard base64 (KV values in backups).
mod base64_lite {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(data: &[u8]) -> String {
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    pub fn decode(s: &str) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let vals: Vec<u8> = s
            .bytes()
            .filter(|&c| c != b'=')
            .map(|c| A.iter().position(|&a| a == c).map(|p| p as u8))
            .collect::<Option<_>>()?;
        for chunk in vals.chunks(4) {
            let mut n = 0u32;
            for (i, &v) in chunk.iter().enumerate() {
                n |= (v as u32) << (18 - 6 * i);
            }
            for i in 0..chunk.len().saturating_sub(1) {
                out.push((n >> (16 - 8 * i) & 255) as u8);
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_utc_timestamps() {
        assert_eq!(format_utc(0), "19700101T000000Z");
        assert_eq!(format_utc(1_791_696_543), "20261011T052903Z");
    }

    #[test]
    fn base64_round_trips() {
        for s in [&b""[..], b"a", b"ab", b"abc", b"\x00\xff\x10binary"] {
            assert_eq!(base64_lite::decode(&base64_lite::encode(s)).unwrap(), s);
        }
        assert_eq!(base64_lite::encode(b"hello"), "aGVsbG8=");
    }

    #[test]
    fn encodes_object_keys_for_paths() {
        assert_eq!(enc("a b/c?.txt"), "a%20b/c%3F.txt");
    }
}
