//! R2 objects through its S3 API (DES-010). rusty-s3 signs the requests,
//! ureq sends them.

use crate::dotenv;
use crate::project::{Project, R2};
use anyhow::{Context, Result, anyhow, bail};
use rusty_s3::actions::{DeleteObjectsResponse, ListObjectsV2, ObjectIdentifier, S3Action};
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
            // Without these a dropped network hangs forever (ureq's default).
            // Bodies have no total limit: objects can be large.
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_send_request(Some(Duration::from_secs(60)))
            .timeout_recv_response(Some(Duration::from_secs(120)))
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
    crate::tr!(
        "R2 の API キーが要ります。.env.<環境名> か環境変数に {KEY_ID} と {SECRET} を書いてください(Cloudflare のダッシュボードの R2 > Manage API tokens で作れます)",
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
        [] => bail!(crate::tr!(
            "Cloudflare のアカウントが見つかりません。`nv run wrangler login` でログインしてください",
            "cannot find your Cloudflare account; run `nv run wrangler login`"
        )),
        _ => bail!(crate::tr!(
            "Cloudflare のアカウントが複数あります。Wrangler の設定に account_id を書いてください",
            "you have several Cloudflare accounts; set account_id in the Wrangler config"
        )),
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

/// R2 answered 404 for the bucket.
#[derive(Debug)]
pub struct NoSuchBucket(pub String);

impl std::fmt::Display for NoSuchBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = crate::tr!(
            "R2 バケット {} がありません",
            "R2 bucket {} does not exist",
            self.0
        );
        f.write_str(&msg)
    }
}

impl std::error::Error for NoSuchBucket {}

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
            .context(crate::tr!(
                "R2 の口の形が正しくありません",
                "invalid R2 endpoint"
            ))?;
        let bucket = Bucket::new(url, UrlStyle::Path, r2.bucket.clone(), "auto").map_err(|e| {
            anyhow!(crate::tr!(
                "R2 バケット {} の指定が正しくありません: {e}",
                "invalid R2 bucket {}: {e}",
                r2.bucket
            ))
        })?;
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
        let mut res = agent().get(url.as_str()).call().with_context(|| {
            crate::tr!(
                "R2 バケット {} につながりません",
                "cannot reach R2 bucket {}",
                self.bucket_name
            )
        })?;
        let status = res.status().as_u16();
        let body = res
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_string()?;
        match status {
            200 => {}
            401 | 403 => bail!(crate::tr!(
                "R2 の API キーがバケット {} に受け付けられませんでした({status})",
                "the R2 API keys were refused for bucket {} ({status})",
                self.bucket_name
            )),
            404 => return Err(NoSuchBucket(self.bucket_name.clone()).into()),
            _ => bail!(crate::tr!(
                "R2 バケット {} の一覧に失敗しました({status}): {}",
                "listing R2 bucket {} failed ({status}): {}",
                self.bucket_name,
                snippet(&body)
            )),
        }
        let parsed = ListObjectsV2::parse_response(&body).map_err(|e| {
            anyhow!(crate::tr!(
                "R2 の一覧の応答が想定と違います: {e}",
                "unexpected R2 listing: {e}"
            ))
        })?;
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

    /// Whether the bucket exists; errors still mean "keys refused" and the like.
    pub fn exists(&self) -> Result<bool> {
        match self.list_page(None, Some(1)) {
            Ok(_) => Ok(true),
            Err(e) if e.downcast_ref::<NoSuchBucket>().is_some() => Ok(false),
            Err(e) => Err(e),
        }
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
            .with_context(|| crate::tr!("{key} を取れません", "cannot download {key}"))?;
        let status = res.status().as_u16();
        if status != 200 {
            bail!(crate::tr!(
                "{} から {key} を取るのに失敗しました({status})",
                "downloading {key} from {} failed ({status})",
                self.bucket_name
            ));
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
        let file = std::fs::File::open(src)
            .with_context(|| crate::tr!("{} を開けません", "cannot open {}", src.display()))?;
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
            .with_context(|| crate::tr!("{key} を上げられません", "cannot upload {key}"))?;
        let status = res.status().as_u16();
        if status != 200 {
            let body = res.body_mut().read_to_string().unwrap_or_default();
            bail!(crate::tr!(
                "{} へ {key} を上げるのに失敗しました({status}): {}",
                "uploading {key} to {} failed ({status}): {}",
                self.bucket_name,
                snippet(&body)
            ));
        }
        Ok(())
    }

    /// Delete every object, 1000 per request. `deleted` counts what is gone,
    /// also when this fails part way, so the caller can report it.
    pub fn delete_all(&self, deleted: &mut usize) -> Result<()> {
        let objects = self.list_all()?;
        for chunk in objects.chunks(1000) {
            crate::interrupt::check()?;
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
                .with_context(|| {
                    crate::tr!(
                        "{} のオブジェクトを消せません",
                        "cannot delete objects in {}",
                        self.bucket_name
                    )
                })?;
            let status = res.status().as_u16();
            let text = res.body_mut().read_to_string().unwrap_or_default();
            if status != 200 {
                bail!(crate::tr!(
                    "{} のオブジェクトの削除に失敗しました({status}): {}",
                    "deleting objects in {} failed ({status}): {}",
                    self.bucket_name,
                    snippet(&text)
                ));
            }
            match DeleteObjectsResponse::parse(&text) {
                Ok(r) => {
                    *deleted += r.deleted.len();
                    if !r.errors.is_empty() {
                        bail!(crate::tr!(
                            "{} のオブジェクトのうち {} 件を消せませんでした: {}",
                            "{1} objects in {0} could not be deleted: {2}",
                            self.bucket_name,
                            r.errors.len(),
                            snippet(&text)
                        ));
                    }
                }
                // A response without per-key results: the request succeeded.
                Err(_) => *deleted += chunk.len(),
            }
        }
        Ok(())
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
