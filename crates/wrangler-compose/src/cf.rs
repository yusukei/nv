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

/// Whether an error is Cloudflare saying the thing is not there.
pub fn is_missing(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ApiError>()
        .is_some_and(|a| a.status == 404)
}

impl Cf {
    /// Credentials from Wrangler (`wrangler auth token --json`: the login's
    /// token, refreshed, or CLOUDFLARE_API_TOKEN), and the account:
    /// CLOUDFLARE_ACCOUNT_ID, or the only account the login can see.
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
        cf.account = match std::env::var("CLOUDFLARE_ACCOUNT_ID") {
            Ok(id) if !id.is_empty() => id,
            _ => {
                let accounts = cf.list_raw("/accounts")?;
                match accounts.as_slice() {
                    [one] => one["id"].as_str().unwrap_or_default().to_string(),
                    [] => bail!(crate::tr!(
                        "このログインで使える Cloudflare のアカウントがありません",
                        "this login has no Cloudflare account"
                    )),
                    many => {
                        let names: Vec<String> = many
                            .iter()
                            .map(|a| {
                                format!(
                                    "{} ({})",
                                    a["name"].as_str().unwrap_or("?"),
                                    a["id"].as_str().unwrap_or("?")
                                )
                            })
                            .collect();
                        bail!(crate::tr!(
                            "アカウントが複数あります。使うものの ID を環境変数 CLOUDFLARE_ACCOUNT_ID に入れてください: {}",
                            "this login sees several accounts; set CLOUDFLARE_ACCOUNT_ID to the one to use: {}",
                            names.join(", ")
                        ))
                    }
                }
            }
        };
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

    fn account_path(&self, rest: &str) -> String {
        format!("/accounts/{}{rest}", self.account)
    }

    pub fn get(&self, rest: &str) -> Result<Value> {
        Ok(self.call("GET", &self.account_path(rest), None, &[])?["result"].take())
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

    pub fn put(&self, rest: &str, body: &Value) -> Result<Value> {
        Ok(self.call("PUT", &self.account_path(rest), Some(body), &[])?["result"].take())
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
