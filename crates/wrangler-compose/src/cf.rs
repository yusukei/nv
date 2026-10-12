//! Cloudflare's API for remote environments (DES-004, DES-009): what
//! Wrangler cannot answer as data (lists, settings) is read and changed
//! here, with the credentials of `wrangler login` (REQ-016: nothing else to
//! set up). Deploying and migrations stay with Wrangler.

use crate::ctx::Ctx;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::process::Stdio;

const API: &str = "https://api.cloudflare.com/client/v4";

enum Auth {
    Bearer(String),
    Key { key: String, email: String },
}

pub struct Cf {
    agent: ureq::Agent,
    auth: Auth,
    pub account: String,
}

/// An API answer that was not a success: the HTTP status and Cloudflare's
/// messages, so callers can tell "not there" from real failures.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cloudflare API {}: {}", self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

/// A non-JSON answer: its body and headers.
pub type RawAnswer = (Vec<u8>, Vec<(String, String)>);

/// Whether an error is Cloudflare saying the thing is not there.
pub fn is_missing(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ApiError>()
        .is_some_and(|a| a.status == 404)
}

impl Cf {
    /// Credentials from Wrangler (`wrangler auth token --json`: the login's
    /// token, refreshed, or CLOUDFLARE_API_TOKEN), and the account: the
    /// CLOUDFLARE_ACCOUNT_ID written in `.env.<env>`, which a remote
    /// environment must have, so nothing is deployed to or deleted from an
    /// account by accident (REQ-006). A different CLOUDFLARE_ACCOUNT_ID in
    /// the environment, or an account the login cannot see, stops.
    pub fn connect(ctx: &Ctx) -> Result<Cf> {
        let out = ctx
            .wrangler(["auth", "token", "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .context(crate::tr!(
                "Wrangler を起動できません",
                "cannot start Wrangler"
            ))?;
        let text = String::from_utf8_lossy(&out.stdout);
        // The JSON is the last thing printed (warnings may come first).
        let json = text.find('{').map(|i| &text[i..]).unwrap_or("");
        let v: Value = match serde_json::from_str(json) {
            Ok(v) if out.status.success() => v,
            _ => bail!(crate::tr!(
                "Cloudflare にログインしていません。一度だけ `nv run wrangler login` を打ってください(何もしていません)",
                "not logged in to Cloudflare; run `nv run wrangler login` once (nothing was done)"
            )),
        };
        let auth = match v["type"].as_str() {
            Some("api_key") => Auth::Key {
                key: v["key"].as_str().unwrap_or_default().to_string(),
                email: v["email"].as_str().unwrap_or_default().to_string(),
            },
            _ => Auth::Bearer(v["token"].as_str().unwrap_or_default().to_string()),
        };
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .user_agent(concat!("wrangler-compose/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        let mut cf = Cf {
            agent,
            auth,
            account: String::new(),
        };
        // The accounts this login can see. A token that may not list them
        // still works on the account written in .env.<env>.
        let accounts = cf.list_raw("/accounts").ok();
        let seen = || match &accounts {
            Some(list) if !list.is_empty() => list
                .iter()
                .map(|a| {
                    format!(
                        "{} ({})",
                        a["name"].as_str().unwrap_or("?"),
                        a["id"].as_str().unwrap_or("?")
                    )
                })
                .collect::<Vec<_>>()
                .join(", "),
            _ => "-".to_string(),
        };
        let file = ctx.compose.env_file.display();
        let Some(id) = ctx.compose.account_id.clone() else {
            bail!(crate::tr!(
                "{file} に CLOUDFLARE_ACCOUNT_ID=<アカウントの ID> を書いてください。意図しないアカウントへの展開・削除を防ぐため、リモートの環境では必須です(何もしていません)。このログインで使えるアカウント: {}",
                "write CLOUDFLARE_ACCOUNT_ID=<account id> in {file}; a remote environment needs it so nothing is deployed to or deleted from an unintended account (nothing was done). Accounts this login can use: {}",
                seen()
            ));
        };
        if let Ok(env) = std::env::var("CLOUDFLARE_ACCOUNT_ID")
            && !env.is_empty()
            && env != id
        {
            bail!(crate::tr!(
                "環境変数 CLOUDFLARE_ACCOUNT_ID({env})が {file} の CLOUDFLARE_ACCOUNT_ID({id})と違います。どちらに出すつもりかを確かめ、環境変数を消すか合わせてください(何もしていません)",
                "the environment variable CLOUDFLARE_ACCOUNT_ID ({env}) differs from CLOUDFLARE_ACCOUNT_ID in {file} ({id}); check which account you mean and unset or match the variable (nothing was done)"
            ));
        }
        if let Some(list) = &accounts
            && !list.iter().any(|a| a["id"].as_str() == Some(id.as_str()))
        {
            bail!(crate::tr!(
                "このログインではアカウント {id}({file} の CLOUDFLARE_ACCOUNT_ID)を使えません(何もしていません)。使えるアカウント: {}",
                "this login cannot use account {id} (CLOUDFLARE_ACCOUNT_ID in {file}); nothing was done. Accounts it can use: {}",
                seen()
            ));
        }
        cf.account = id;
        Ok(cf)
    }

    /// One request; the whole answer (`result`, `result_info`, …).
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        headers: &[(&str, &str)],
    ) -> Result<Value> {
        // CLOUDFLARE_API_BASE_URL is Wrangler's own setting for another API
        // address; the tool follows it so both talk to the same place.
        let base = std::env::var("CLOUDFLARE_API_BASE_URL")
            .ok()
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| API.to_string());
        let url = format!("{}{path}", base.trim_end_matches('/'));
        let auth = |b: ureq::RequestBuilder<ureq::typestate::WithoutBody>| match &self.auth {
            Auth::Bearer(t) => b.header("authorization", format!("Bearer {t}")),
            Auth::Key { key, email } => b.header("x-auth-key", key).header("x-auth-email", email),
        };
        let auth_body = |b: ureq::RequestBuilder<ureq::typestate::WithBody>| match &self.auth {
            Auth::Bearer(t) => b.header("authorization", format!("Bearer {t}")),
            Auth::Key { key, email } => b.header("x-auth-key", key).header("x-auth-email", email),
        };
        let res = match method {
            "GET" | "DELETE" => {
                let mut b = if method == "GET" {
                    self.agent.get(&url)
                } else {
                    self.agent.delete(&url)
                };
                for (k, v) in headers {
                    b = b.header(*k, *v);
                }
                auth(b).call()
            }
            _ => {
                let mut b = match method {
                    "POST" => self.agent.post(&url),
                    "PUT" => self.agent.put(&url),
                    _ => self.agent.patch(&url),
                };
                for (k, v) in headers {
                    b = b.header(*k, *v);
                }
                let body = serde_json::to_vec(body.unwrap_or(&json!({})))?;
                auth_body(b.header("content-type", "application/json")).send(&body[..])
            }
        };
        let mut res = res.context(crate::tr!(
            "Cloudflare につながりません",
            "cannot reach Cloudflare"
        ))?;
        let status = res.status().as_u16();
        let text = res.body_mut().read_to_string().unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status >= 300 || v["success"] == false {
            let errors = v["errors"].as_array().cloned().unwrap_or_default();
            let message = errors
                .iter()
                .map(|e| {
                    format!(
                        "{} ({})",
                        e["message"].as_str().unwrap_or("?"),
                        e["code"].as_i64().unwrap_or(0)
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ApiError {
                status,
                message: if message.is_empty() {
                    format!("{method} {path}")
                } else {
                    message
                },
            }
            .into());
        }
        Ok(v)
    }

    /// A call whose body is not JSON (KV values, R2 objects): the body and
    /// headers of the answer, or None when Cloudflare says there is no such
    /// thing (404).
    pub fn raw(
        &self,
        method: &str,
        rest: &str,
        body: Option<&[u8]>,
        headers: &[(&str, &str)],
    ) -> Result<Option<RawAnswer>> {
        let base = std::env::var("CLOUDFLARE_API_BASE_URL")
            .ok()
            .filter(|b| !b.is_empty())
            .unwrap_or_else(|| API.to_string());
        let path = self.account_path(rest);
        let url = format!("{}{path}", base.trim_end_matches('/'));
        let (name, value) = match &self.auth {
            Auth::Bearer(t) => ("authorization", format!("Bearer {t}")),
            Auth::Key { key, .. } => ("x-auth-key", key.clone()),
        };
        let email = match &self.auth {
            Auth::Key { email, .. } => Some(email.clone()),
            _ => None,
        };
        let res = match (method, body) {
            ("GET" | "DELETE", _) => {
                let mut b = if method == "GET" {
                    self.agent.get(&url)
                } else {
                    self.agent.delete(&url)
                };
                for (k, v) in headers {
                    b = b.header(*k, *v);
                }
                b = b.header(name, &value);
                if let Some(e) = &email {
                    b = b.header("x-auth-email", e);
                }
                b.call()
            }
            (_, body) => {
                let mut b = self.agent.put(&url);
                for (k, v) in headers {
                    b = b.header(*k, *v);
                }
                b = b.header(name, &value);
                if let Some(e) = &email {
                    b = b.header("x-auth-email", e);
                }
                b.send(body.unwrap_or_default())
            }
        };
        let mut res = res.context(crate::tr!(
            "Cloudflare につながりません",
            "cannot reach Cloudflare"
        ))?;
        let status = res.status().as_u16();
        if status == 404 {
            return Ok(None);
        }
        let got: Vec<(String, String)> = res
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        let mut bytes = Vec::new();
        use std::io::Read as _;
        res.body_mut().as_reader().read_to_end(&mut bytes)?;
        if status >= 300 {
            let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let message = v["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|e| e["message"].as_str().unwrap_or("?").to_string())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ApiError {
                status,
                message: if message.is_empty() {
                    format!("{method} {path}")
                } else {
                    message
                },
            }
            .into());
        }
        Ok(Some((bytes, got)))
    }

    fn account_path(&self, rest: &str) -> String {
        format!("/accounts/{}{rest}", self.account)
    }

    pub fn get(&self, rest: &str) -> Result<Value> {
        Ok(self.call("GET", &self.account_path(rest), None, &[])?["result"].take())
    }

    /// The whole answer (`result` and `result_info`), for cursor paging.
    pub fn get_full(&self, rest: &str, headers: &[(&str, &str)]) -> Result<Value> {
        self.call("GET", &self.account_path(rest), None, headers)
    }

    pub fn get_with(&self, rest: &str, headers: &[(&str, &str)]) -> Result<Value> {
        Ok(self.call("GET", &self.account_path(rest), None, headers)?["result"].take())
    }

    pub fn post(&self, rest: &str, body: &Value) -> Result<Value> {
        Ok(self.call("POST", &self.account_path(rest), Some(body), &[])?["result"].take())
    }

    pub fn post_with(&self, rest: &str, body: &Value, headers: &[(&str, &str)]) -> Result<Value> {
        Ok(self.call("POST", &self.account_path(rest), Some(body), headers)?["result"].take())
    }


    pub fn put_with(&self, rest: &str, body: &Value, headers: &[(&str, &str)]) -> Result<Value> {
        Ok(self.call("PUT", &self.account_path(rest), Some(body), headers)?["result"].take())
    }

    pub fn delete(&self, rest: &str) -> Result<()> {
        self.call("DELETE", &self.account_path(rest), None, &[])?;
        Ok(())
    }

    pub fn delete_with(&self, rest: &str, headers: &[(&str, &str)]) -> Result<()> {
        self.call("DELETE", &self.account_path(rest), None, headers)?;
        Ok(())
    }

    /// Every page of a paged list (`page`/`per_page`).
    fn list_raw(&self, path: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        for page in 1.. {
            let sep = if path.contains('?') { '&' } else { '?' };
            let v = self.call(
                "GET",
                &format!("{path}{sep}page={page}&per_page=100"),
                None,
                &[],
            )?;
            let items = v["result"].as_array().cloned().unwrap_or_default();
            let n = items.len();
            out.extend(items);
            let total_pages = v["result_info"]["total_pages"].as_u64();
            let done = match total_pages {
                Some(t) => page >= t,
                None => n < 100,
            };
            if done || n == 0 {
                break;
            }
        }
        Ok(out)
    }

    pub fn list(&self, rest: &str) -> Result<Vec<Value>> {
        self.list_raw(&self.account_path(rest))
    }

    /// R2 buckets (cursor paging), in one jurisdiction ("default" for none).
    pub fn r2_buckets(&self, jurisdiction: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let q = match &cursor {
                Some(c) => format!("?per_page=1000&cursor={}", crate::data::enc(c)),
                None => "?per_page=1000".to_string(),
            };
            let v = self.call(
                "GET",
                &self.account_path(&format!("/r2/buckets{q}")),
                None,
                &[("cf-r2-jurisdiction", jurisdiction)],
            )?;
            out.extend(
                v["result"]["buckets"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
            );
            match v["result_info"]["cursor"].as_str() {
                Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                _ => break,
            }
        }
        Ok(out)
    }

    /// One page of a bucket's object keys (cursor paging), and the cursor of
    /// the next page. Wrangler has no list of objects; this is the same R2
    /// API its object commands use, with the login's own token.
    pub fn r2_object_keys(
        &self,
        bucket: &str,
        jurisdiction: &str,
        cursor: Option<&str>,
    ) -> Result<(Vec<String>, Option<String>)> {
        let q = match cursor {
            Some(c) => format!("?per_page=1000&cursor={}", crate::data::enc(c)),
            None => "?per_page=1000".to_string(),
        };
        let v = self.call(
            "GET",
            &self.account_path(&format!("/r2/buckets/{bucket}/objects{q}")),
            None,
            &[("cf-r2-jurisdiction", jurisdiction)],
        )?;
        let items = match &v["result"] {
            Value::Array(a) => a.clone(),
            other => other["objects"].as_array().cloned().unwrap_or_default(),
        };
        let keys = items
            .iter()
            .filter_map(|o| o["key"].as_str().map(str::to_string))
            .collect();
        let next = match (
            v["result_info"]["cursor"].as_str(),
            v["result_info"]["is_truncated"].as_bool(),
        ) {
            (Some(c), Some(true)) if !c.is_empty() => Some(c.to_string()),
            (Some(c), None) if !c.is_empty() => Some(c.to_string()),
            _ => None,
        };
        Ok((keys, next))
    }

    /// Deletes one object (each part of the key encoded, `/` kept).
    pub fn r2_delete_object(&self, bucket: &str, jurisdiction: &str, key: &str) -> Result<()> {
        let path: Vec<String> = key.split('/').map(crate::data::enc).collect();
        self.call(
            "DELETE",
            &self.account_path(&format!("/r2/buckets/{bucket}/objects/{}", path.join("/"))),
            None,
            &[("cf-r2-jurisdiction", jurisdiction)],
        )?;
        Ok(())
    }

    /// Deletes every object of a bucket, page by page, so the bucket itself
    /// can go (Cloudflare refuses to delete a bucket that has objects).
    pub fn r2_empty(&self, bucket: &str, jurisdiction: &str) -> Result<usize> {
        let mut n = 0;
        loop {
            // Always the first page: deleted keys no longer show.
            let (keys, _) = self.r2_object_keys(bucket, jurisdiction, None)?;
            if keys.is_empty() {
                return Ok(n);
            }
            for k in &keys {
                self.r2_delete_object(bucket, jurisdiction, k)?;
            }
            n += keys.len();
        }
    }

    /// The account's workers.dev subdomain, if it has one.
    pub fn workers_subdomain(&self) -> Result<Option<String>> {
        match self.get("/workers/subdomain") {
            Ok(v) => Ok(v["subdomain"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)),
            Err(e) if is_missing(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// `anyhow!` for a value Cloudflare should have sent but did not.
pub fn missing_field(what: &str) -> anyhow::Error {
    anyhow!(crate::tr!(
        "Cloudflare の答えに {what} がありません",
        "Cloudflare's answer has no {what}"
    ))
}
