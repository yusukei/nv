//! R2 objects through its S3 API (DES-010). rusty-s3 signs the requests,
//! ureq sends them.

use crate::dotenv;
use crate::project::{Project, R2};
use anyhow::{Context, Result, anyhow, bail};
use rusty_s3::actions::{ListObjectsV2, ObjectIdentifier, S3Action};
use rusty_s3::{Bucket, Credentials, UrlStyle};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

const KEY_ID: &str = "WRANGLER_COMPOSE_R2_ACCESS_KEY_ID";
const SECRET: &str = "WRANGLER_COMPOSE_R2_SECRET_ACCESS_KEY";
const SIGN_FOR: Duration = Duration::from_secs(15 * 60);
/// Object headers worth keeping across backup and restore.
const KEPT_HEADERS: &[&str] = &[
    "content-type",
    "content-encoding",
    "content-language",
    "content-disposition",
    "cache-control",
    "expires",
];

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .user_agent(concat!("wrangler-compose/", env!("CARGO_PKG_VERSION")))
            // We read error bodies ourselves to give useful messages.
            .http_status_as_error(false)
            .build()
            .into()
    })
}

/// API keys: environment variables first, then `.env.<env>` (REQ-009).
pub fn credentials(project: &Project) -> Option<Credentials> {
    let file = project
        .env
        .as_ref()
        .and_then(|e| std::fs::read_to_string(project.dir.join(format!(".env.{e}"))).ok())
        .map(|t| dotenv::parse(&t))
        .unwrap_or_default();
    let get = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| file.get(name).filter(|v| !v.is_empty()).cloned())
    };
    Some(Credentials::new(get(KEY_ID)?, get(SECRET)?))
}

pub fn missing_keys_message() -> String {
    format!(
        "R2 API keys are needed: set {KEY_ID} and {SECRET} in .env.<env> or the environment (create them in the Cloudflare dashboard under R2 > Manage API tokens)"
    )
}

/// Account ID for the S3 endpoint: CLOUDFLARE_ACCOUNT_ID, the config's
/// account_id, or the only account of the logged-in user.
pub fn account_id(project: &Project) -> Result<String> {
    if let Ok(id) = std::env::var("CLOUDFLARE_ACCOUNT_ID")
        && !id.is_empty()
    {
        return Ok(id);
    }
    if let Some(id) = &project.config.account_id {
        return Ok(id.clone());
    }
    let who = project.wrangler_json(["whoami", "--json"])?;
    let ids: Vec<String> = who["accounts"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    match ids.as_slice() {
        [one] => Ok(one.clone()),
        [] => bail!("cannot find your Cloudflare account; run `nv run wrangler login`"),
        _ => bail!("you have several Cloudflare accounts; set account_id in the Wrangler config"),
    }
}

pub fn endpoint(account: &str, jurisdiction: Option<&str>) -> String {
    match jurisdiction {
        Some(j) => format!("https://{account}.{j}.r2.cloudflarestorage.com"),
        None => format!("https://{account}.r2.cloudflarestorage.com"),
    }
}

pub struct Store {
    pub binding: String,
    pub bucket_name: String,
    bucket: Bucket,
    creds: Credentials,
}

pub struct Object {
    pub key: String,
    pub size: u64,
}

/// A downloaded object: its hash, the headers worth keeping and its user metadata.
pub struct Downloaded {
    pub sha256: String,
    pub headers: Vec<(String, String)>,
    pub metadata: Vec<(String, String)>,
}

impl Store {
    pub fn new(account: &str, r2: &R2, creds: &Credentials) -> Result<Store> {
        let url = endpoint(account, r2.jurisdiction.as_deref())
            .parse()
            .context("invalid R2 endpoint")?;
        let bucket = Bucket::new(url, UrlStyle::Path, r2.bucket.clone(), "auto")
            .map_err(|e| anyhow!("invalid R2 bucket {}: {e}", r2.bucket))?;
        Ok(Store {
            binding: r2.binding.clone(),
            bucket_name: r2.bucket.clone(),
            bucket,
            creds: creds.clone(),
        })
    }

    fn list_page(
        &self,
        token: Option<&str>,
        max: Option<usize>,
    ) -> Result<(Vec<Object>, Option<String>)> {
        let mut action: ListObjectsV2 = self.bucket.list_objects_v2(Some(&self.creds));
        if let Some(t) = token {
            action.with_continuation_token(t.to_string());
        }
        if let Some(m) = max {
            action.with_max_keys(m);
        }
        let url = action.sign(SIGN_FOR);
        let mut res = agent()
            .get(url.as_str())
            .call()
            .with_context(|| format!("cannot reach R2 bucket {}", self.bucket_name))?;
        let status = res.status().as_u16();
        let body = res
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_string()?;
        match status {
            200 => {}
            401 | 403 => bail!(
                "the R2 API keys were refused for bucket {} ({status})",
                self.bucket_name
            ),
            404 => bail!("R2 bucket {} does not exist", self.bucket_name),
            _ => bail!(
                "listing R2 bucket {} failed ({status}): {}",
                self.bucket_name,
                snippet(&body)
            ),
        }
        let parsed = ListObjectsV2::parse_response(&body)
            .map_err(|e| anyhow!("unexpected R2 listing: {e}"))?;
        let objects = parsed
            .contents
            .into_iter()
            .map(|c| Object {
                key: c.key,
                size: c.size,
            })
            .collect();
        Ok((objects, parsed.next_continuation_token))
    }

    /// Proves the keys work for this bucket (REQ-006): one listing call.
    pub fn check_access(&self) -> Result<bool> {
        let (objects, _) = self.list_page(None, Some(1))?;
        Ok(!objects.is_empty())
    }

    pub fn list_all(&self) -> Result<Vec<Object>> {
        let mut all = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let (page, next) = self.list_page(token.as_deref(), None)?;
            all.extend(page);
            match next {
                Some(t) => token = Some(t),
                None => return Ok(all),
            }
        }
    }

    /// Stream an object into `dest`.
    pub fn download(&self, key: &str, dest: &Path) -> Result<Downloaded> {
        let url = self
            .bucket
            .get_object(Some(&self.creds), key)
            .sign(SIGN_FOR);
        let res = agent()
            .get(url.as_str())
            .call()
            .with_context(|| format!("cannot download {key}"))?;
        let status = res.status().as_u16();
        if status != 200 {
            bail!(
                "downloading {key} from {} failed ({status})",
                self.bucket_name
            );
        }
        let mut headers = Vec::new();
        let mut meta = Vec::new();
        for (name, value) in res.headers() {
            let name = name.as_str().to_ascii_lowercase();
            let Ok(value) = value.to_str() else { continue };
            if KEPT_HEADERS.contains(&name.as_str()) {
                headers.push((name, value.to_string()));
            } else if let Some(m) = name.strip_prefix("x-amz-meta-") {
                meta.push((m.to_string(), value.to_string()));
            }
        }
        let mut reader = res.into_body().into_reader();
        let sha256 = crate::files::copy_hashing(&mut reader, dest)?;
        Ok(Downloaded {
            sha256,
            headers,
            metadata: meta,
        })
    }

    pub fn upload(
        &self,
        key: &str,
        src: &Path,
        headers: &[(String, String)],
        meta: &[(String, String)],
    ) -> Result<()> {
        let mut action = self.bucket.put_object(Some(&self.creds), key);
        let mut sent: Vec<(String, String)> = Vec::new();
        for (k, v) in headers {
            action.headers_mut().insert(k.clone(), v.clone());
            sent.push((k.clone(), v.clone()));
        }
        for (k, v) in meta {
            let name = format!("x-amz-meta-{k}");
            action.headers_mut().insert(name.clone(), v.clone());
            sent.push((name, v.clone()));
        }
        let url = action.sign(SIGN_FOR);
        let file =
            std::fs::File::open(src).with_context(|| format!("cannot open {}", src.display()))?;
        let len = file.metadata()?.len();
        let mut req = agent()
            .put(url.as_str())
            .header("content-length", len.to_string());
        for (k, v) in &sent {
            req = req.header(k, v);
        }
        let mut res = req
            .send(ureq::SendBody::from_reader(&mut std::io::BufReader::new(
                file,
            )))
            .with_context(|| format!("cannot upload {key}"))?;
        let status = res.status().as_u16();
        if status != 200 {
            let body = res.body_mut().read_to_string().unwrap_or_default();
            bail!(
                "uploading {key} to {} failed ({status}): {}",
                self.bucket_name,
                snippet(&body)
            );
        }
        Ok(())
    }

    /// Delete every object, 1000 per request. Returns how many were deleted.
    pub fn delete_all(&self) -> Result<usize> {
        let objects = self.list_all()?;
        let mut deleted = 0;
        for chunk in objects.chunks(1000) {
            let ids: Vec<ObjectIdentifier> = chunk
                .iter()
                .map(|o| ObjectIdentifier::new(o.key.clone()))
                .collect();
            let action = self.bucket.delete_objects(Some(&self.creds), ids.iter());
            let url = action.sign(SIGN_FOR);
            let action = self.bucket.delete_objects(Some(&self.creds), ids.iter());
            let (body, md5) = action.body_with_md5();
            let mut res = agent()
                .post(url.as_str())
                .header("content-md5", &md5)
                .send(body.as_bytes())
                .with_context(|| format!("cannot delete objects in {}", self.bucket_name))?;
            let status = res.status().as_u16();
            let text = res.body_mut().read_to_string().unwrap_or_default();
            if status != 200 {
                bail!(
                    "deleting objects in {} failed ({status}): {}",
                    self.bucket_name,
                    snippet(&text)
                );
            }
            if text.contains("<Error>") {
                bail!(
                    "some objects in {} could not be deleted: {}",
                    self.bucket_name,
                    snippet(&text)
                );
            }
            deleted += chunk.len();
        }
        Ok(deleted)
    }
}

fn snippet(s: &str) -> String {
    let s = s.trim();
    let mut out: String = s.chars().take(300).collect();
    if s.chars().count() > 300 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_endpoints_with_and_without_jurisdiction() {
        assert_eq!(
            endpoint("abc", None),
            "https://abc.r2.cloudflarestorage.com"
        );
        assert_eq!(
            endpoint("abc", Some("eu")),
            "https://abc.eu.r2.cloudflarestorage.com"
        );
    }
}
