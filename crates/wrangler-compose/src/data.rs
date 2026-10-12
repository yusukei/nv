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
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const MANIFEST: &str = "manifest.json";
/// 3: D1 values that JSON cannot carry are marked (`tagged`), hidden rowids
/// and AUTOINCREMENT counters are kept.
const FORMAT: u64 = 3;

fn say(msg: String) {
    eprintln!("wrangler-compose: {msg}");
}

/// Talks to one environment's data: locally its data service (127.0.0.1),
/// remotely Cloudflare's own APIs for D1, KV and R2 (REQ-013).
pub struct Client {
    base: String,
    token: String,
    agent: ureq::Agent,
    remote: Option<Remote>,
}

/// A remote environment's data, by compose key: D1 and KV ids, and R2
/// buckets (name, jurisdiction).
pub struct Remote {
    pub cf: crate::cf::Cf,
    pub d1: BTreeMap<String, String>,
    pub kv: BTreeMap<String, String>,
    pub r2: BTreeMap<String, (String, String)>,
}

impl Client {
    pub fn remote(remote: Remote) -> Client {
        let mut c = Client::new(String::new(), String::new());
        c.remote = Some(remote);
        c
    }

    pub fn new(base: String, token: String) -> Client {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(300)))
            // Never route the tool's own local requests through a proxy.
            .proxy(None)
            .build()
            .into();
        Client {
            base,
            token,
            agent,
            remote: None,
        }
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
        if let Some(r) = &self.remote {
            return r.query(db, sql, params);
        }
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
    /// Run read statements in one transaction; rows of each, in order.
    pub fn snapshot(&self, db: &str, sqls: &[String]) -> Result<Vec<Vec<Vec<Value>>>> {
        if let Some(r) = &self.remote {
            return r.snapshot(db, sqls);
        }
        let st: Vec<Value> = sqls.iter().map(|s| json!({ "sql": s })).collect();
        let v = self.post(&format!("d1/{db}/snapshot"), &json!({ "statements": st }))?;
        Ok(v["results"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|rows| {
                rows.as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|r| r.as_array().cloned())
                    .collect()
            })
            .collect())
    }

    pub fn batch(&self, db: &str, statements: Vec<(String, Vec<Value>)>) -> Result<()> {
        if statements.is_empty() {
            return Ok(());
        }
        if let Some(r) = &self.remote {
            return r.batch(db, statements);
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
        if let Some(r) = &self.remote {
            return r.r2_list(bucket, cursor);
        }
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
        if let Some(r) = &self.remote {
            return r.r2_get(bucket, key, dest);
        }
        let res = self
            .agent
            .get(self.url(&format!("r2/{bucket}/object?key={}", enc(key))))
            .header("authorization", format!("Bearer {}", self.token))
            .call()?;
        if res.status().as_u16() != 200 {
            bail!(crate::tr!(
                "{bucket} の {key} を読めません",
                "cannot read {key} from {bucket}"
            ));
        }
        let meta = meta_of(res.headers())?;
        let mut reader = res.into_body().into_reader();
        let (hash, size) = copy_hashing(&mut reader, dest)?;
        Ok((hash, size, meta))
    }

    pub fn r2_put(&self, bucket: &str, key: &str, src: &Path, meta: &Value) -> Result<()> {
        if let Some(r) = &self.remote {
            return r.r2_put(bucket, key, src, meta);
        }
        let file = File::open(src)
            .with_context(|| crate::tr!("{} を開けません", "cannot open {}", src.display()))?;
        let len = file.metadata()?.len();
        let mut res = self
            .agent
            .put(self.url(&format!("r2/{bucket}/object?key={}", enc(key))))
            .header("authorization", format!("Bearer {}", self.token))
            .header("content-length", len.to_string())
            .header("x-wc-meta", enc(&meta.to_string()))
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
        if let Some(r) = &self.remote {
            return r.r2_delete(bucket, keys);
        }
        self.post(&format!("r2/{bucket}/delete"), &json!({ "keys": keys }))?;
        Ok(())
    }

    pub fn kv_list(&self, ns: &str, cursor: Option<&str>) -> Result<(Vec<Value>, Option<String>)> {
        if let Some(r) = &self.remote {
            return r.kv_list(ns, cursor);
        }
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
        if let Some(r) = &self.remote {
            return r.kv_get(ns, key);
        }
        let res = self
            .agent
            .get(self.url(&format!("kv/{ns}/value?key={}", enc(key))))
            .header("authorization", format!("Bearer {}", self.token))
            .call()?;
        match res.status().as_u16() {
            200 => {}
            404 => return Ok(None),
            _ => bail!(crate::tr!(
                "{ns} の {key} を読めません",
                "cannot read {key} from {ns}"
            )),
        }
        let meta = meta_of(res.headers())?;
        let mut body = Vec::new();
        res.into_body().into_reader().read_to_end(&mut body)?;
        Ok(Some((body, meta)))
    }

    pub fn kv_put(&self, ns: &str, key: &str, value: &[u8], meta: &Value) -> Result<()> {
        if let Some(r) = &self.remote {
            return r.kv_put(ns, key, value, meta);
        }
        let mut res = self
            .agent
            .put(self.url(&format!("kv/{ns}/value?key={}", enc(key))))
            .header("authorization", format!("Bearer {}", self.token))
            .header("x-wc-meta", enc(&meta.to_string()))
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
        if let Some(r) = &self.remote {
            return r.kv_delete(ns, key);
        }
        let res = self
            .agent
            .delete(self.url(&format!("kv/{ns}/value?key={}", enc(key))))
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

/// Cloudflare's APIs for a remote environment's data (REQ-013): the same
/// ones Wrangler's d1, kv and r2 commands call with the login.
impl Remote {
    fn d1_id(&self, db: &str) -> Result<&str> {
        self.d1
            .get(db)
            .map(String::as_str)
            .ok_or_else(|| anyhow!(crate::tr!("D1 {db} がありません", "no D1 {db}")))
    }

    fn kv_id(&self, ns: &str) -> Result<&str> {
        self.kv
            .get(ns)
            .map(String::as_str)
            .ok_or_else(|| anyhow!(crate::tr!("KV {ns} がありません", "no KV {ns}")))
    }

    fn bucket(&self, b: &str) -> Result<(&str, &str)> {
        self.r2
            .get(b)
            .map(|(n, j)| (n.as_str(), j.as_str()))
            .ok_or_else(|| anyhow!(crate::tr!("R2 {b} がありません", "no R2 {b}")))
    }

    fn raw_results(v: &Value) -> Vec<(Vec<String>, Vec<Vec<Value>>)> {
        v.as_array()
            .into_iter()
            .flatten()
            .map(|r| {
                let res = &r["results"];
                let cols = res["columns"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c.as_str().map(str::to_string))
                    .collect();
                let rows = res["rows"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|r| r.as_array().cloned())
                    .collect();
                (cols, rows)
            })
            .collect()
    }

    fn query(&self, db: &str, sql: &str, params: Vec<Value>) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
        let id = self.d1_id(db)?;
        let v = self
            .cf
            .post(&format!("/d1/database/{id}/raw"), &json!({ "sql": sql, "params": params }))?;
        Ok(Self::raw_results(&v).into_iter().next().unwrap_or_default())
    }

    /// The statements in one request, which D1 runs as one batch.
    fn snapshot(&self, db: &str, sqls: &[String]) -> Result<Vec<Vec<Vec<Value>>>> {
        let id = self.d1_id(db)?;
        let v = self
            .cf
            .post(&format!("/d1/database/{id}/raw"), &json!({ "sql": sqls.join(";\n") }))?;
        Ok(Self::raw_results(&v).into_iter().map(|(_, rows)| rows).collect())
    }

    /// Statements with their values written in as SQL literals, so a whole
    /// batch goes as one request (D1 runs it as one batch, the deferred
    /// foreign keys included). A statement too long for D1 (100 KB) goes on
    /// its own with bound values.
    fn batch(&self, db: &str, statements: Vec<(String, Vec<Value>)>) -> Result<()> {
        const LIMIT: usize = 90_000;
        let id = self.d1_id(db)?;
        let path = format!("/d1/database/{id}/query");
        let mut group: Vec<String> = Vec::new();
        let mut size = 0;
        let send = |group: &mut Vec<String>| -> Result<()> {
            if !group.is_empty() {
                self.cf.post(&path, &json!({ "sql": group.join(";\n") }))?;
                group.clear();
            }
            Ok(())
        };
        for (sql, params) in statements {
            match inline(&sql, &params) {
                Some(one) if one.len() <= LIMIT => {
                    if size + one.len() > 900_000 {
                        send(&mut group)?;
                        size = 0;
                    }
                    size += one.len();
                    group.push(one);
                }
                _ => {
                    send(&mut group)?;
                    size = 0;
                    self.cf.post(&path, &json!({ "sql": sql, "params": params }))?;
                }
            }
        }
        send(&mut group)
    }

    fn r2_list(&self, bucket: &str, cursor: Option<&str>) -> Result<(Vec<Value>, Option<String>)> {
        let (name, jur) = self.bucket(bucket)?;
        let q = match cursor {
            Some(c) => format!("?per_page=1000&cursor={}", enc(c)),
            None => "?per_page=1000".to_string(),
        };
        let v = self
            .cf
            .get_full(&format!("/r2/buckets/{name}/objects{q}"), &[("cf-r2-jurisdiction", jur)])?;
        let items = match &v["result"] {
            Value::Array(a) => a.clone(),
            other => other["objects"].as_array().cloned().unwrap_or_default(),
        };
        let objects = items
            .iter()
            .map(|o| json!({ "key": o["key"], "size": o["size"], "etag": o["etag"] }))
            .collect();
        let next = v["result_info"]["cursor"]
            .as_str()
            .filter(|c| !c.is_empty() && v["result_info"]["is_truncated"] != false)
            .map(str::to_string);
        Ok((objects, next))
    }

    fn object_path(name: &str, key: &str) -> String {
        let parts: Vec<String> = key.split('/').map(enc).collect();
        format!("/r2/buckets/{name}/objects/{}", parts.join("/"))
    }

    fn r2_get(&self, bucket: &str, key: &str, dest: &Path) -> Result<(String, u64, Value)> {
        let (name, jur) = self.bucket(bucket)?;
        let (bytes, headers) = self
            .cf
            .raw("GET", &Self::object_path(name, key), None, &[("cf-r2-jurisdiction", jur)])?
            .ok_or_else(|| anyhow!(crate::tr!("{bucket} の {key} を読めません", "cannot read {key} from {bucket}")))?;
        let mut http = serde_json::Map::new();
        for (field, header) in HTTP_FIELDS {
            if let Some((_, v)) = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(header)) {
                http.insert(field.to_string(), json!(v));
            }
        }
        let (hash, size) = copy_hashing(&mut bytes.as_slice(), dest)?;
        Ok((hash, size, json!({ "http": http, "custom": {} })))
    }

    fn r2_put(&self, bucket: &str, key: &str, src: &Path, meta: &Value) -> Result<()> {
        let (name, jur) = self.bucket(bucket)?;
        let body = fs::read(src)
            .with_context(|| crate::tr!("{} を開けません", "cannot open {}", src.display()))?;
        let mut h: Vec<(&str, &str)> = vec![("cf-r2-jurisdiction", jur)];
        for (field, header) in HTTP_FIELDS {
            if let Some(v) = meta["http"][field].as_str() {
                h.push((header, v));
            }
        }
        self.cf.raw("PUT", &Self::object_path(name, key), Some(&body), &h)?;
        Ok(())
    }

    fn r2_delete(&self, bucket: &str, keys: &[String]) -> Result<()> {
        let (name, jur) = self.bucket(bucket)?;
        for k in keys {
            self.cf.r2_delete_object(name, jur, k)?;
        }
        Ok(())
    }

    fn kv_list(&self, ns: &str, cursor: Option<&str>) -> Result<(Vec<Value>, Option<String>)> {
        let id = self.kv_id(ns)?;
        let q = match cursor {
            Some(c) => format!("?limit=1000&cursor={}", enc(c)),
            None => "?limit=1000".to_string(),
        };
        let v = self
            .cf
            .get_full(&format!("/storage/kv/namespaces/{id}/keys{q}"), &[])?;
        let keys = v["result"].as_array().cloned().unwrap_or_default();
        let next = v["result_info"]["cursor"]
            .as_str()
            .filter(|c| !c.is_empty())
            .map(str::to_string);
        Ok((keys, next))
    }

    fn kv_get(&self, ns: &str, key: &str) -> Result<Option<(Vec<u8>, Value)>> {
        let id = self.kv_id(ns)?;
        let Some((value, _)) = self.cf.raw(
            "GET",
            &format!("/storage/kv/namespaces/{id}/values/{}", enc(key)),
            None,
            &[],
        )?
        else {
            return Ok(None);
        };
        let metadata = match self
            .cf
            .get(&format!("/storage/kv/namespaces/{id}/metadata/{}", enc(key)))
        {
            Ok(m) => m,
            Err(e) if crate::cf::is_missing(&e) => Value::Null,
            Err(e) => return Err(e),
        };
        Ok(Some((value, json!({ "metadata": metadata }))))
    }

    fn kv_put(&self, ns: &str, key: &str, value: &[u8], meta: &Value) -> Result<()> {
        let id = self.kv_id(ns)?;
        let mut one = json!({ "key": key, "value": base64_lite::encode(value), "base64": true });
        if !meta["metadata"].is_null() {
            one["metadata"] = meta["metadata"].clone();
        }
        if let Some(x) = meta["expiration"].as_u64() {
            one["expiration"] = json!(x);
        }
        self.cf
            .put_with(&format!("/storage/kv/namespaces/{id}/bulk"), &json!([one]), &[])?;
        Ok(())
    }

    fn kv_delete(&self, ns: &str, key: &str) -> Result<()> {
        let id = self.kv_id(ns)?;
        self.cf
            .delete(&format!("/storage/kv/namespaces/{id}/values/{}", enc(key)))
    }
}

/// R2's HTTP metadata, as the binding names it and as headers.
const HTTP_FIELDS: [(&str, &str); 5] = [
    ("contentType", "content-type"),
    ("cacheControl", "cache-control"),
    ("contentDisposition", "content-disposition"),
    ("contentEncoding", "content-encoding"),
    ("contentLanguage", "content-language"),
];

/// `sql` with each `?` outside quotes replaced by its value as an SQL
/// literal. None when a value has no literal form.
fn inline(sql: &str, params: &[Value]) -> Option<String> {
    let mut out = String::with_capacity(sql.len() + 16 * params.len());
    let mut values = params.iter();
    let mut quote: Option<char> = None;
    for c in sql.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
                out.push(c);
            }
            (Some(_), c) => out.push(c),
            (None, '\'' | '"' | '`') => {
                quote = Some(c);
                out.push(c);
            }
            (None, '[') => {
                quote = Some(']');
                out.push(c);
            }
            (None, '?') => out.push_str(&literal(values.next()?)?),
            (None, c) => out.push(c),
        }
    }
    values.next().is_none().then_some(out)
}

fn literal(v: &Value) -> Option<String> {
    Some(match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "1" } else { "0" }.into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("'{}'", s.replace('\'', "''")),
        _ => return None,
    })
}

/// The metadata the data service sends with a value (percent-encoded JSON,
/// so any character survives the header).
pub fn meta_of(h: &ureq::http::HeaderMap) -> Result<Value> {
    let raw = h
        .get("x-wc-meta")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow::anyhow!("the data service sent no metadata"))?;
    Ok(serde_json::from_slice(&dec(raw))?)
}

/// Percent-decode.
pub fn dec(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(h) = std::str::from_utf8(&b[i + 1..i + 3])
            && let Ok(v) = u8::from_str_radix(h, 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// Percent-encode everything but unreserved characters, for a query value
/// or a header (`/`, `.` and `..` mean nothing there).
pub fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
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
        // substr, not LIKE: `_` is a wildcard there and LIKE ignores case.
        "SELECT type, name, sql FROM sqlite_master WHERE lower(substr(name, 1, 7)) <> 'sqlite_' AND substr(name, 1, 4) <> '_cf_' ORDER BY rowid",
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

/// How one table is read for a backup.
struct TablePlan {
    name: String,
    /// Column names as written to the backup (`rowid` first when the table
    /// has a hidden rowid worth keeping).
    cols: Vec<String>,
    select: String,
}

/// A value the backup's JSON could not carry exactly is written as text
/// with a mark (char 1): integers beyond 2^53 (`i:`), reals that JSON
/// would turn into integers or null (`r:`), and text that starts with the
/// mark itself (`t:`). Restore turns them back.
fn tagged(col: &str, alias: &str) -> String {
    format!(
        "CASE WHEN typeof({c}) = 'integer' AND ({c} > 9007199254740991 OR {c} < -9007199254740991) THEN char(1) || 'i:' || {c} \
         WHEN typeof({c}) = 'real' AND ({c} = CAST({c} AS INTEGER) OR {c} > 1.7976931348623157e308 OR {c} < -1.7976931348623157e308) THEN char(1) || 'r:' || printf('%!.17g', {c}) \
         WHEN typeof({c}) = 'text' AND substr({c}, 1, 1) = char(1) THEN char(1) || 't:' || {c} \
         WHEN typeof({c}) = 'blob' THEN char(1) || 'b:' || hex({c}) \
         ELSE {c} END AS \"{alias}\"",
        c = col
    )
}

fn table_plan(client: &Client, db: &str, name: &str, sql: &str) -> Result<TablePlan> {
    // Stored columns only: generated ones are computed again on insert.
    let (_, info) = client.query(
        db,
        "SELECT name, type, pk FROM pragma_table_xinfo(?) WHERE hidden = 0",
        vec![json!(name)],
    )?;
    let names: Vec<String> = info
        .iter()
        .filter_map(|r| r.first().and_then(Value::as_str).map(str::to_string))
        .collect();
    let pks: Vec<&Vec<Value>> = info
        .iter()
        .filter(|r| r.get(2).and_then(Value::as_i64).unwrap_or(0) > 0)
        .collect();
    let without_rowid = sql.to_uppercase().contains("WITHOUT ROWID");
    let alias = pks.len() == 1
        && pks[0]
            .get(1)
            .and_then(Value::as_str)
            .is_some_and(|t| t.eq_ignore_ascii_case("INTEGER"));
    // The hidden rowid (kept so rows keep their numbers), under a name no
    // real column uses.
    let rowid = (!without_rowid && !alias)
        .then(|| {
            ["rowid", "_rowid_", "oid"]
                .into_iter()
                .find(|r| !names.iter().any(|n| n.eq_ignore_ascii_case(r)))
        })
        .flatten();
    let mut cols = Vec::new();
    let mut exprs = Vec::new();
    if let Some(r) = rowid {
        cols.push(r.to_string());
        exprs.push(format!("{r} AS \"c0\""));
    }
    for n in &names {
        exprs.push(tagged(&quote(n), &format!("c{}", exprs.len())));
        cols.push(n.clone());
    }
    let order = match rowid {
        Some(r) => format!(" ORDER BY {r}"),
        None if without_rowid && !pks.is_empty() => {
            let mut p = pks.clone();
            p.sort_by_key(|r| r.get(2).and_then(Value::as_i64).unwrap_or(0));
            let keys: Vec<String> = p
                .iter()
                .filter_map(|r| r.first().and_then(Value::as_str).map(quote))
                .collect();
            format!(" ORDER BY {}", keys.join(", "))
        }
        None => String::new(),
    };
    Ok(TablePlan {
        name: name.to_string(),
        cols,
        select: format!("SELECT {} FROM {}{order}", exprs.join(", "), quote(name)),
    })
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
    let mut plans = Vec::new();
    for (_, name, sql) in objects.iter().filter(|(t, _, _)| t == "table") {
        plans.push(table_plan(
            client,
            db,
            name,
            sql.as_deref().unwrap_or_default(),
        )?);
    }
    let has_seq = !client
        .query(
            db,
            "SELECT 1 FROM sqlite_master WHERE name = 'sqlite_sequence'",
            vec![],
        )?
        .1
        .is_empty();
    // Every table (and the AUTOINCREMENT counters) in one read, so a backup
    // taken while `up` runs is one moment of the database.
    let mut sqls: Vec<String> = plans.iter().map(|p| p.select.clone()).collect();
    if has_seq {
        sqls.push("SELECT name AS \"c0\", seq AS \"c1\" FROM sqlite_sequence".to_string());
    }
    let results = client.snapshot(db, &sqls)?;
    let mut out = BufWriter::new(File::create(dest)?);
    let mut rows_written = 0u64;
    for ((plan, (_, _, sql)), rows) in plans
        .iter()
        .zip(objects.iter().filter(|(t, _, _)| t == "table"))
        .zip(&results)
    {
        writeln!(out, "{}", json!({ "schema": sql }))?;
        for r in rows {
            writeln!(out, "{}", json!({ "t": plan.name, "c": plan.cols, "r": r }))?;
        }
        rows_written += rows.len() as u64;
    }
    if has_seq && let Some(seq) = results.get(plans.len()) {
        for r in seq {
            writeln!(out, "{}", json!({ "seq": r.first(), "n": r.get(1) }))?;
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

/// A backed-up value as an insert placeholder and its parameter (see
/// `tagged`).
fn untag(v: &Value) -> (&'static str, Value) {
    let Some(s) = v.as_str().and_then(|s| s.strip_prefix('\u{1}')) else {
        return ("?", v.clone());
    };
    if let Some(n) = s.strip_prefix("i:") {
        ("CAST(? AS INTEGER)", json!(n))
    } else if let Some(r) = s.strip_prefix("r:") {
        let r = match r {
            "Inf" => "9e999",
            "-Inf" => "-9e999",
            other => other,
        };
        ("CAST(? AS REAL)", json!(r))
    } else if let Some(t) = s.strip_prefix("t:") {
        ("?", json!(t))
    } else if let Some(b) = s.strip_prefix("b:") {
        ("unhex(?)", json!(b))
    } else {
        ("?", v.clone())
    }
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
    // 1. Every table first, so rows never wait for a table that comes later
    //    in the file; note where each table's rows are.
    let mut schema = Vec::new();
    let mut post = Vec::new();
    let mut seq = Vec::new();
    let mut spans: Vec<(String, u64, u64)> = Vec::new();
    let mut reader = BufReader::new(File::open(src)?);
    let mut pos = 0u64;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)? as u64;
        if n == 0 {
            break;
        }
        let v: Value = serde_json::from_str(&line)?;
        if let Some(s) = v["schema"].as_str() {
            schema.push((s.to_string(), vec![]));
        } else if let Some(s) = v["post"].as_str() {
            post.push((s.to_string(), vec![]));
        } else if let Some(name) = v.get("seq").filter(|x| !x.is_null()) {
            // AUTOINCREMENT counters, so new rows never reuse old numbers.
            seq.push((
                "DELETE FROM sqlite_sequence WHERE name = ?".to_string(),
                vec![name.clone()],
            ));
            seq.push((
                "INSERT INTO sqlite_sequence (name, seq) VALUES (?, ?)".to_string(),
                vec![name.clone(), v["n"].clone()],
            ));
        } else if let Some(t) = v["t"].as_str() {
            match spans.last_mut() {
                Some((name, _, end)) if name == t => *end = pos + n,
                _ => spans.push((t.to_string(), pos, pos + n)),
            }
        }
        pos += n;
    }
    client.batch(db, schema)?;

    // 2. Rows, parents before the tables that reference them.
    let order = parents_first(
        client,
        db,
        spans.iter().map(|(t, _, _)| t.clone()).collect(),
    )?;
    let mut rows = 0u64;
    let mut pending: Vec<(String, Vec<Value>)> = Vec::new();
    let flush = |pending: &mut Vec<(String, Vec<Value>)>| -> Result<()> {
        if !pending.is_empty() {
            let mut st = vec![("PRAGMA defer_foreign_keys = true".to_string(), vec![])];
            st.append(pending);
            client.batch(db, st)?;
        }
        Ok(())
    };
    let mut file = File::open(src)?;
    for table in order {
        for (_, start, end) in spans.iter().filter(|(t, _, _)| *t == table) {
            file.seek(SeekFrom::Start(*start))?;
            let part = BufReader::new((&mut file).take(end - start));
            for line in part.lines() {
                let v: Value = serde_json::from_str(&line?)?;
                let cols: Vec<String> = v["c"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c.as_str().map(quote))
                    .collect();
                let (marks, params): (Vec<&str>, Vec<Value>) =
                    v["r"].as_array().into_iter().flatten().map(untag).unzip();
                let sql = format!(
                    "INSERT INTO {} ({}) VALUES ({})",
                    quote(&table),
                    cols.join(", "),
                    marks.join(", ")
                );
                pending.push((sql, params));
                rows += 1;
                if pending.len() >= 50 {
                    flush(&mut pending)?;
                }
            }
        }
        flush(&mut pending)?;
    }

    // 3. Counters, then indexes, triggers and views.
    client.batch(db, seq)?;
    client.batch(db, post)?;
    Ok(rows)
}

/// Tables ordered so a table comes after the tables its foreign keys point
/// at (tables in a cycle keep their order).
fn parents_first(client: &Client, db: &str, tables: Vec<String>) -> Result<Vec<String>> {
    let mut parents: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for t in &tables {
        let (_, rows) = client.query(
            db,
            "SELECT \"table\" FROM pragma_foreign_key_list(?)",
            vec![json!(t)],
        )?;
        let ps = rows
            .iter()
            .filter_map(|r| r.first().and_then(Value::as_str).map(str::to_string))
            .filter(|p| p != t && tables.contains(p))
            .collect();
        parents.insert(t.clone(), ps);
    }
    let mut done: Vec<String> = Vec::new();
    let mut left = tables;
    while !left.is_empty() {
        let ready: Vec<String> = left
            .iter()
            .filter(|t| parents[*t].iter().all(|p| done.contains(p)))
            .cloned()
            .collect();
        if ready.is_empty() {
            done.append(&mut left);
            break;
        }
        left.retain(|t| !ready.contains(t));
        done.extend(ready);
    }
    Ok(done)
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
        let n = count(client, t, &key)?;
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
            // Only files inside the backup folder (a shared backup must not
            // pull other files of this machine into the local data).
            let inside = !rel.is_empty()
                && Path::new(&rel)
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)));
            if !inside {
                bail!(crate::tr!(
                    "控えの目録が、控えのフォルダの外のファイル {rel} を指しています(何もしていません)",
                    "the backup's manifest points outside the backup folder: {rel} (nothing was done)"
                ));
            }
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
        empty(
            client,
            e["type"].as_str().unwrap_or_default(),
            e["key"].as_str().unwrap_or_default(),
        )?;
    }
    Ok(())
}

/// Delete everything in one D1 database, R2 bucket or KV namespace.
pub fn empty(client: &Client, t: &str, key: &str) -> Result<()> {
    match t {
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
    Ok(())
}

pub fn count(client: &Client, t: &str, key: &str) -> Result<usize> {
    Ok(match t {
        "d1" => d1_table_count(client, key)?,
        "r2" => r2_all(client, key)?.len(),
        _ => kv_all(client, key)?.len(),
    })
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
    fn writes_values_into_sql_outside_quotes() {
        let sql = "INSERT INTO \"a?b\" ('?', x, y, z) VALUES (?, CAST(? AS INTEGER), unhex(?), ?)";
        let got = inline(sql, &[json!("it's"), json!("9007199254740993"), json!("00FF"), Value::Null]).unwrap();
        assert_eq!(
            got,
            "INSERT INTO \"a?b\" ('?', x, y, z) VALUES ('it''s', CAST('9007199254740993' AS INTEGER), unhex('00FF'), NULL)"
        );
        // Too few or too many values: not inlined.
        assert!(inline("SELECT ?", &[]).is_none());
        assert!(inline("SELECT 1", &[json!(1)]).is_none());
    }

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
    fn encodes_keys_and_metadata() {
        assert_eq!(enc("a b/../c?.txt"), "a%20b%2F..%2Fc%3F.txt");
        assert_eq!(dec(&enc("é/日本 %")), "é/日本 %".as_bytes());
    }
}
