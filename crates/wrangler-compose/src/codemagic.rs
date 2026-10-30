//! Codemagic's API for the codemagic service (REQ-021, DES-022): one app
//! per remote environment, its id kept in `.env.<env>` as
//! CODEMAGIC_APP_ID, and its variable groups made what the compose file
//! says. The token goes only in the `x-auth-token` header; neither it nor
//! a Secure value is ever shown (REQ-015).

use crate::compose::{Codemagic, Compose};
use crate::remote::Act;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const API: &str = "https://api.codemagic.io";
const API_V3: &str = "https://codemagic.io/api/v3";

fn say(msg: String) {
    eprintln!("wrangler-compose: {msg}");
}

/// An answer that was not a success: the HTTP status and Codemagic's
/// message (with the token and Secure values taken out).
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Codemagic API {}: {}", self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

fn is_missing(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ApiError>()
        .is_some_and(|a| a.status == 404)
}

/// The API token, checked before anything on Cloudflare or Codemagic is
/// touched (DES-022).
pub fn token(c: &Compose) -> Result<String> {
    match &c.codemagic_token {
        Some(t) => Ok(t.clone()),
        None => bail!(crate::tr!(
            "Codemagic の API トークンがありません。CODEMAGIC_API_TOKEN=<トークン> を {} か環境変数に書いてください。トークンは Codemagic の画面の Teams > Personal Account > Integrations > Codemagic API で見られます(何もしていません)",
            "no Codemagic API token; write CODEMAGIC_API_TOKEN=<token> in {} or set it in the environment. The token is in Codemagic under Teams > Personal Account > Integrations > Codemagic API (nothing was done)",
            c.env_file.display()
        )),
    }
}

/// The two API addresses. CODEMAGIC_API_BASE_URL (for tests) points both
/// at one place; anything but this machine must be HTTPS, so the token
/// never travels in the clear.
fn bases() -> Result<(String, String)> {
    let Some(base) = std::env::var("CODEMAGIC_API_BASE_URL")
        .ok()
        .filter(|b| !b.is_empty())
    else {
        return Ok((API.to_string(), API_V3.to_string()));
    };
    let base = base.trim_end_matches('/').to_string();
    let local = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|p| {
            base == *p || base.starts_with(&format!("{p}:")) || base.starts_with(&format!("{p}/"))
        });
    if !base.starts_with("https://") && !local {
        bail!(crate::tr!(
            "CODEMAGIC_API_BASE_URL は https:// にしてください(何もしていません)",
            "CODEMAGIC_API_BASE_URL must be https:// (nothing was done)"
        ));
    }
    Ok((base.clone(), format!("{base}/api/v3")))
}

pub struct Cm {
    agent: ureq::Agent,
    token: String,
    api: String,
    v3: String,
    /// Values never to show, even inside Codemagic's own messages.
    hidden: Vec<String>,
}

/// One variable of a group as Codemagic has it (a Secure value is null).
#[derive(Clone, Debug)]
pub struct Var {
    pub id: String,
    pub name: String,
    pub value: Option<String>,
    pub secure: bool,
}

#[derive(Clone, Debug)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub vars: Vec<Var>,
}

impl Cm {
    /// `hidden`: the Secure values about to be sent, kept out of messages.
    pub fn new(token: String, hidden: Vec<String>) -> Result<Cm> {
        let (api, v3) = bases()?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .user_agent(concat!("wrangler-compose/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        let mut hidden: Vec<String> = hidden.into_iter().filter(|v| !v.is_empty()).collect();
        hidden.push(token.clone());
        // Longer first, so a value holding another is hidden whole.
        hidden.sort_by_key(|v| std::cmp::Reverse(v.len()));
        Ok(Cm {
            agent,
            token,
            api,
            v3,
            hidden,
        })
    }

    fn hide(&self, text: &str) -> String {
        let mut t = text.to_string();
        for h in &self.hidden {
            t = t.replace(h.as_str(), "***");
        }
        t
    }

    /// One request; the JSON of the answer (Null when there is none).
    fn call(&self, method: &str, url: &str, body: Option<&Value>) -> Result<Value> {
        let res = match method {
            "GET" | "DELETE" => {
                let b = if method == "GET" {
                    self.agent.get(url)
                } else {
                    self.agent.delete(url)
                };
                b.header("x-auth-token", &self.token).call()
            }
            _ => {
                let b = if method == "POST" {
                    self.agent.post(url)
                } else {
                    self.agent.patch(url)
                };
                let body = serde_json::to_vec(body.unwrap_or(&json!({})))?;
                b.header("x-auth-token", &self.token)
                    .header("content-type", "application/json")
                    .send(&body[..])
            }
        };
        let mut res = res.context(crate::tr!(
            "Codemagic につながりません",
            "cannot reach Codemagic"
        ))?;
        let status = res.status().as_u16();
        let reset = res
            .headers()
            .get("ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let text = res.body_mut().read_to_string().unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status == 429 {
            bail!(crate::tr!(
                "Codemagic の API の回数の上限(1 時間に 5,000 回)に達しました。{} 秒後にもう一度打ってください",
                "Codemagic's API rate limit (5,000 calls an hour) was reached; try again in {} seconds",
                reset.as_deref().unwrap_or("?")
            ));
        }
        if status >= 300 {
            let mut parts: Vec<String> = v["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| e["message"].as_str().map(str::to_string))
                .collect();
            for k in ["message", "detail", "error"] {
                if parts.is_empty()
                    && let Some(m) = v[k].as_str()
                {
                    parts.push(m.to_string());
                }
            }
            let message = if parts.is_empty() {
                format!("{method} {}", url.split('?').next().unwrap_or(url))
            } else {
                parts.join("; ")
            };
            return Err(ApiError {
                status,
                message: self.hide(&message),
            }
            .into());
        }
        Ok(v)
    }

    /// Every page of a v3 list (`page`/`page_size`, up to `total_pages`).
    fn list(&self, path: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        for page in 1.. {
            let v = self.call(
                "GET",
                &format!("{}{path}?page={page}&page_size=100", self.v3),
                None,
            )?;
            let items = v["data"].as_array().cloned().unwrap_or_default();
            let n = items.len();
            out.extend(items);
            let done = match v["total_pages"].as_u64() {
                Some(t) => page >= t,
                None => n < 100,
            };
            if done || n == 0 {
                break;
            }
        }
        Ok(out)
    }

    /// The app, or None when Codemagic has no such app (404).
    pub fn app(&self, id: &str) -> Result<Option<Value>> {
        match self.call("GET", &format!("{}/apps/{id}", self.api), None) {
            Ok(v) => Ok(Some(v["application"].clone())),
            Err(e) if is_missing(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Registers the repository as a new app; its id.
    pub fn register(&self, repository: &str) -> Result<String> {
        let v = self.call(
            "POST",
            &format!("{}/apps", self.api),
            Some(&json!({ "repositoryUrl": repository })),
        )?;
        v["_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                anyhow!(crate::tr!(
                    "Codemagic の答えにアプリの ID(_id)がありません",
                    "Codemagic's answer has no app id (_id)"
                ))
            })
    }

    /// Not in Codemagic's documents, but answers 202 (measured 2026-10-30).
    pub fn delete_app(&self, id: &str) -> Result<()> {
        self.call("DELETE", &format!("{}/apps/{id}", self.api), None)?;
        Ok(())
    }

    /// The app's groups with their variables.
    pub fn groups(&self, app: &str) -> Result<Vec<Group>> {
        let mut out = Vec::new();
        for g in self.list(&format!("/apps/{app}/variable-groups"))? {
            let (Some(id), Some(name)) = (g["id"].as_str(), g["name"].as_str()) else {
                continue;
            };
            let vars = self
                .list(&format!("/variable-groups/{id}/variables"))?
                .iter()
                .filter_map(|x| {
                    Some(Var {
                        id: x["id"].as_str()?.to_string(),
                        name: x["name"].as_str()?.to_string(),
                        value: x["value"].as_str().map(str::to_string),
                        secure: x["secure"].as_bool().unwrap_or(false),
                    })
                })
                .collect();
            out.push(Group {
                id: id.to_string(),
                name: name.to_string(),
                vars,
            });
        }
        Ok(out)
    }

    pub fn create_group(&self, app: &str, name: &str) -> Result<String> {
        let v = self.call(
            "POST",
            &format!("{}/apps/{app}/variable-groups", self.v3),
            Some(&json!({ "name": name })),
        )?;
        v["data"]["id"]
            .as_str()
            .or_else(|| v["id"].as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                anyhow!(crate::tr!(
                    "Codemagic の答えにグループの ID がありません",
                    "Codemagic's answer has no group id"
                ))
            })
    }

    pub fn delete_group(&self, gid: &str) -> Result<()> {
        self.call(
            "DELETE",
            &format!("{}/variable-groups/{gid}", self.v3),
            None,
        )?;
        Ok(())
    }

    /// Adds variables in one call; all Secure or all not.
    pub fn add_vars(&self, gid: &str, secure: bool, vars: &[(String, String)]) -> Result<()> {
        let list: Vec<Value> = vars
            .iter()
            .map(|(n, v)| json!({ "name": n, "value": v }))
            .collect();
        self.call(
            "POST",
            &format!("{}/variable-groups/{gid}/variables", self.v3),
            Some(&json!({ "secure": secure, "variables": list })),
        )?;
        Ok(())
    }

    pub fn update_var(
        &self,
        gid: &str,
        vid: &str,
        value: &str,
        secure: Option<bool>,
    ) -> Result<()> {
        let mut body = json!({ "value": value });
        if let Some(s) = secure {
            body["secure"] = json!(s);
        }
        self.call(
            "PATCH",
            &format!("{}/variable-groups/{gid}/variables/{vid}", self.v3),
            Some(&body),
        )?;
        Ok(())
    }

    pub fn delete_var(&self, gid: &str, vid: &str) -> Result<()> {
        self.call(
            "DELETE",
            &format!("{}/variable-groups/{gid}/variables/{vid}", self.v3),
            None,
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------- comparing

/// One change to the variable groups, in the order they are made: groups
/// to create, variables to add and change, variables to delete, groups to
/// delete (DES-022).
#[derive(Debug, PartialEq)]
pub enum Change {
    CreateGroup(String),
    Add {
        group: String,
        secure: bool,
        vars: Vec<(String, String)>,
    },
    /// `old`: the value before (None when Secure); `secure`: Some when the
    /// variable turns Secure or plain.
    Update {
        group: String,
        id: String,
        name: String,
        old: Option<String>,
        value: String,
        secure: Option<bool>,
        now_secure: bool,
    },
    DeleteVar {
        group: String,
        id: String,
        name: String,
    },
    DeleteGroup {
        id: String,
        name: String,
    },
}

impl Change {
    fn group(&self) -> &str {
        match self {
            Change::CreateGroup(g) => g,
            Change::Add { group, .. }
            | Change::Update { group, .. }
            | Change::DeleteVar { group, .. } => group,
            Change::DeleteGroup { name, .. } => name,
        }
    }

    /// The change in a plan line: values only when they are not Secure.
    fn details(&self) -> Vec<String> {
        match self {
            Change::CreateGroup(_) | Change::DeleteGroup { .. } => Vec::new(),
            Change::Add { secure, vars, .. } => vars
                .iter()
                .map(|(n, v)| {
                    if *secure {
                        crate::tr!(
                            "{n}: +(Secure。値は出さない)",
                            "{n}: + (Secure, value not shown)"
                        )
                    } else {
                        format!("{n}: + {v:?}")
                    }
                })
                .collect(),
            Change::Update {
                name,
                old,
                value,
                secure,
                now_secure,
                ..
            } => vec![match (secure, now_secure) {
                (Some(true), _) => crate::tr!(
                    "{name}: Secure にして入れ直す(値は出さない)",
                    "{name}: made Secure and set again (value not shown)"
                ),
                (Some(false), _) => crate::tr!(
                    "{name}: Secure をやめる → {value:?}",
                    "{name}: no longer Secure → {value:?}"
                ),
                (None, true) => crate::tr!(
                    "{name}: 入れ直す(Secure。値は出さない)",
                    "{name}: set again (Secure, value not shown)"
                ),
                (None, false) => format!(
                    "{name}: {:?} → {value:?}",
                    old.as_deref().unwrap_or_default()
                ),
            }],
            Change::DeleteVar { name, .. } => vec![format!("{name}: -")],
        }
    }
}

/// What up does to the groups, and the Secure variables it leaves as they
/// are (group, name): Codemagic never gives their values back, so they are
/// set again only with --update-secret (REQ-005).
#[derive(Debug, Default)]
pub struct Diff {
    pub changes: Vec<Change>,
    pub kept: Vec<(String, String)>,
}

pub fn diff(want: &Codemagic, now: &[Group], update_secret: bool) -> Diff {
    let mut creates = Vec::new();
    let mut adds = Vec::new();
    let mut deletes = Vec::new();
    let mut kept = Vec::new();
    for g in &want.groups {
        let have = now.iter().find(|x| x.name == g.name);
        if have.is_none() {
            creates.push(Change::CreateGroup(g.name.clone()));
        }
        let vars: &[Var] = have.map(|h| h.vars.as_slice()).unwrap_or(&[]);
        // Name → (value, Secure), what the compose file says.
        let wanted: BTreeMap<&String, (&String, bool)> = g
            .environment
            .iter()
            .map(|(k, v)| (k, (v, false)))
            .chain(g.secrets.iter().map(|(k, v)| (k, (v, true))))
            .collect();
        let mut new: [Vec<(String, String)>; 2] = [Vec::new(), Vec::new()];
        for (name, (value, secure)) in &wanted {
            let existing = vars.iter().find(|v| &&v.name == name);
            match existing {
                // An empty value is not put in (and goes if it is there).
                _ if value.is_empty() => {}
                None => new[usize::from(*secure)].push(((*name).clone(), (*value).clone())),
                Some(v) if v.secure != *secure => adds.push(Change::Update {
                    group: g.name.clone(),
                    id: v.id.clone(),
                    name: v.name.clone(),
                    old: v.value.clone(),
                    value: (*value).clone(),
                    secure: Some(*secure),
                    now_secure: v.secure,
                }),
                Some(v) if *secure => {
                    if update_secret {
                        adds.push(Change::Update {
                            group: g.name.clone(),
                            id: v.id.clone(),
                            name: v.name.clone(),
                            old: None,
                            value: (*value).clone(),
                            secure: None,
                            now_secure: true,
                        });
                    } else {
                        kept.push((g.name.clone(), v.name.clone()));
                    }
                }
                Some(v) if v.value.as_deref() != Some(value.as_str()) => {
                    adds.push(Change::Update {
                        group: g.name.clone(),
                        id: v.id.clone(),
                        name: v.name.clone(),
                        old: v.value.clone(),
                        value: (*value).clone(),
                        secure: None,
                        now_secure: false,
                    })
                }
                Some(_) => {}
            }
        }
        for (i, list) in new.into_iter().enumerate() {
            if !list.is_empty() {
                adds.push(Change::Add {
                    group: g.name.clone(),
                    secure: i == 1,
                    vars: list,
                });
            }
        }
        // Variables the compose file does not have, or whose value is empty;
        // a second one of the same name too.
        let mut seen: Vec<&str> = Vec::new();
        for v in vars {
            let unwanted = wanted
                .get(&v.name)
                .is_none_or(|(value, _)| value.is_empty());
            if unwanted || seen.contains(&v.name.as_str()) {
                deletes.push(Change::DeleteVar {
                    group: g.name.clone(),
                    id: v.id.clone(),
                    name: v.name.clone(),
                });
            }
            seen.push(&v.name);
        }
    }
    let mut groups_gone = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for h in now {
        if !want.groups.iter().any(|g| g.name == h.name) || seen.contains(&h.name.as_str()) {
            groups_gone.push(Change::DeleteGroup {
                id: h.id.clone(),
                name: h.name.clone(),
            });
        }
        seen.push(&h.name);
    }
    let mut changes = creates;
    changes.extend(adds);
    changes.extend(deletes);
    changes.extend(groups_gone);
    Diff { changes, kept }
}

// ---------------------------------------------------------------- up

/// What Codemagic has for this environment now (read with GET only).
pub struct Now {
    /// CODEMAGIC_APP_ID; None means up registers a new app.
    pub app_id: Option<String>,
    pub groups: Vec<Group>,
}

/// Reads the app and its groups. An id whose app is gone stops: up does not
/// register another one behind the user's back.
pub fn read(cm: &Cm, c: &Compose) -> Result<Now> {
    let Some(id) = c.codemagic_app_id.clone() else {
        return Ok(Now {
            app_id: None,
            groups: Vec::new(),
        });
    };
    if cm.app(&id)?.is_none() {
        bail!(crate::tr!(
            "Codemagic にアプリ {id}(CODEMAGIC_APP_ID)がありません。{} から CODEMAGIC_APP_ID の行を消して up すると、新しく登録します(何もしていません)",
            "Codemagic has no app {id} (CODEMAGIC_APP_ID); remove the CODEMAGIC_APP_ID line from {} and up registers a new one (nothing was done)",
            c.env_file.display()
        ));
    }
    let groups = cm.groups(&id)?;
    Ok(Now {
        app_id: Some(id),
        groups,
    })
}

/// The plan lines (REQ-014): the app, then each group of the compose file,
/// then the groups to delete.
pub fn plan(svc: &str, want: &Codemagic, now: &Now, d: &Diff) -> Vec<(char, String, Vec<String>)> {
    let mut out = Vec::new();
    match &now.app_id {
        None => out.push((
            '+',
            crate::tr!("Codemagic のアプリ({svc})", "Codemagic app ({svc})"),
            vec![crate::tr!("登録する: {}", "register {}", want.repository)],
        )),
        Some(id) => out.push((
            '=',
            crate::tr!(
                "Codemagic のアプリ {id}({svc})",
                "Codemagic app {id} ({svc})"
            ),
            Vec::new(),
        )),
    }
    for g in &want.groups {
        let mine: Vec<&Change> = d
            .changes
            .iter()
            .filter(|c| c.group() == g.name && !matches!(c, Change::DeleteGroup { .. }))
            .collect();
        let sign = if mine.iter().any(|c| matches!(c, Change::CreateGroup(_))) {
            '+'
        } else if mine.is_empty() {
            '='
        } else {
            '~'
        };
        let mut details: Vec<String> = mine.iter().flat_map(|c| c.details()).collect();
        for (_, name) in d.kept.iter().filter(|(gr, _)| *gr == g.name) {
            details.push(crate::tr!(
                "{name}: Secure。あるので変えない(入れ直すには --update-secret)",
                "{name}: Secure, already there; unchanged (--update-secret sets it again)"
            ));
        }
        out.push((
            sign,
            crate::tr!(
                "Codemagic の変数グループ {}",
                "Codemagic variable group {}",
                g.name
            ),
            details,
        ));
    }
    for c in &d.changes {
        if let Change::DeleteGroup { name, .. } = c {
            out.push((
                '-',
                crate::tr!(
                    "Codemagic の変数グループ {name}",
                    "Codemagic variable group {name}"
                ),
                vec![crate::tr!(
                    "compose ファイルに無い(中の変数も消える)",
                    "not in the compose file (its variables go too)"
                )],
            ));
        }
    }
    out
}

/// Makes Codemagic what the plan says: registers the app (writing its id to
/// `.env.<env>`), then the group changes in order. Every step is said as it
/// is done, so a stop shows how far it got; another up finishes the rest.
pub fn apply(cm: &Cm, act: &Act, c: &Compose, want: &Codemagic, now: &Now, d: &Diff) -> Result<()> {
    let file = c.env_file.display().to_string();
    let app = match &now.app_id {
        Some(id) => id.clone(),
        None => {
            let mut id = String::new();
            act.run(
                crate::tr!(
                    "Codemagic にアプリを登録する: {}",
                    "registering an app on Codemagic: {}",
                    want.repository
                ),
                || {
                    id = cm.register(&want.repository)?;
                    crate::dotenv::append_line(&c.env_file, "CODEMAGIC_APP_ID", &id).with_context(|| {
                        crate::tr!(
                            "Codemagic にアプリ {id} を登録しましたが、{file} に書けません。CODEMAGIC_APP_ID={id} をご自身で書き足してください",
                            "registered app {id} on Codemagic but cannot write {file}; add CODEMAGIC_APP_ID={id} yourself"
                        )
                    })?;
                    say(crate::tr!(
                        "{file} に CODEMAGIC_APP_ID={id} を書き足しました。パスワード管理などに .env の控えがあれば、そちらも直してください",
                        "added CODEMAGIC_APP_ID={id} to {file}; if you keep a copy of the .env elsewhere (a password manager…), update it too"
                    ));
                    Ok(())
                },
            )?;
            id
        }
    };
    let mut gids: BTreeMap<String, String> = now
        .groups
        .iter()
        .rev()
        .map(|g| (g.name.clone(), g.id.clone()))
        .collect();
    let gid = |gids: &BTreeMap<String, String>, name: &str| {
        gids.get(name).cloned().ok_or_else(|| anyhow!("{name}"))
    };
    for ch in &d.changes {
        let line = ch.details().join(", ");
        match ch {
            Change::CreateGroup(name) => act.run(
                crate::tr!(
                    "Codemagic: 変数グループ {name} を作る",
                    "Codemagic: creating variable group {name}"
                ),
                || {
                    let id = cm.create_group(&app, name)?;
                    gids.insert(name.clone(), id);
                    Ok(())
                },
            )?,
            Change::Add {
                group,
                secure,
                vars,
            } => act.run(
                crate::tr!("Codemagic: {group}: {line}", "Codemagic: {group}: {line}"),
                || cm.add_vars(&gid(&gids, group)?, *secure, vars),
            )?,
            Change::Update {
                group,
                id,
                value,
                secure,
                ..
            } => act.run(
                crate::tr!("Codemagic: {group}: {line}", "Codemagic: {group}: {line}"),
                || cm.update_var(&gid(&gids, group)?, id, value, *secure),
            )?,
            Change::DeleteVar { group, id, .. } => act.run(
                crate::tr!("Codemagic: {group}: {line}", "Codemagic: {group}: {line}"),
                || cm.delete_var(&gid(&gids, group)?, id),
            )?,
            Change::DeleteGroup { id, name } => act.run(
                crate::tr!(
                    "Codemagic: 変数グループ {name} を消す",
                    "Codemagic: deleting variable group {name}"
                ),
                || cm.delete_group(id),
            )?,
        }
    }
    Ok(())
}

/// The Secure values of the compose file (kept out of every message).
pub fn secret_values(want: &Codemagic) -> Vec<String> {
    want.groups
        .iter()
        .flat_map(|g| g.secrets.values().cloned())
        .collect()
}

// ---------------------------------------------------------------- down

/// What down deletes: the app (None when Codemagic no longer has it) and its
/// groups.
pub struct Gone {
    pub id: String,
    pub exists: bool,
    pub groups: Vec<Group>,
}

pub fn read_gone(cm: &Cm, c: &Compose) -> Result<Option<Gone>> {
    let Some(id) = c.codemagic_app_id.clone() else {
        return Ok(None);
    };
    let exists = cm.app(&id)?.is_some();
    let groups = if exists { cm.groups(&id)? } else { Vec::new() };
    Ok(Some(Gone { id, exists, groups }))
}

/// Deletes every group first, then the app: an app deleted first leaves its
/// groups behind, readable but impossible to delete (measured 2026-10-30).
/// Then the CODEMAGIC_APP_ID line goes, unless the id came from the
/// environment.
pub fn delete(cm: &Cm, act: &Act, c: &Compose, g: &Gone) -> Result<()> {
    for gr in &g.groups {
        act.run(
            crate::tr!(
                "Codemagic: 変数グループ {} を消す",
                "Codemagic: deleting variable group {}",
                gr.name
            ),
            || cm.delete_group(&gr.id),
        )?;
    }
    if g.exists {
        act.run(
            crate::tr!(
                "Codemagic: アプリ {} を消す",
                "Codemagic: deleting app {}",
                g.id
            ),
            || cm.delete_app(&g.id),
        )?;
    }
    if !c.codemagic_app_id_from_env {
        let file = c.env_file.display().to_string();
        act.run(
            crate::tr!(
                "{file} から CODEMAGIC_APP_ID の行を消す",
                "removing the CODEMAGIC_APP_ID line from {file}"
            ),
            || crate::dotenv::remove_lines(&c.env_file, "CODEMAGIC_APP_ID"),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------- ps

/// The ps lines of the codemagic service (REQ-012): the app (id, name,
/// repository), each variable's name, whether it is Secure and has a value,
/// and how it differs from the compose file. Never a value.
pub fn ps_lines(cm: &Cm, c: &Compose, svc: &str, want: &Codemagic) -> Result<Vec<[String; 4]>> {
    let mut out = Vec::new();
    let Some(id) = c.codemagic_app_id.clone() else {
        out.push([
            svc.to_string(),
            "codemagic".into(),
            crate::tr!("無い", "missing"),
            crate::tr!("(up が登録する)", "(up registers it)"),
        ]);
        return Ok(out);
    };
    let Some(app) = cm.app(&id)? else {
        out.push([
            svc.to_string(),
            "codemagic".into(),
            crate::tr!("無い", "missing"),
            crate::tr!(
                "{id}(Codemagic にありません。CODEMAGIC_APP_ID の行を消すと up が登録し直す)",
                "{id} (not on Codemagic; remove the CODEMAGIC_APP_ID line and up registers it again)"
            ),
        ]);
        return Ok(out);
    };
    let repo = match &app["repository"] {
        Value::String(s) => s.clone(),
        r => ["htmlUrl", "url", "cloneUrl"]
            .iter()
            .find_map(|k| r[*k].as_str().map(str::to_string))
            .unwrap_or_default(),
    };
    out.push([
        svc.to_string(),
        "codemagic".into(),
        crate::tr!("あり", "present"),
        format!("{id}  {}  {repo}", app["appName"].as_str().unwrap_or("-")),
    ]);
    let groups = cm.groups(&id)?;
    for g in &want.groups {
        let at = format!("{svc}.{}", g.name);
        let Some(have) = groups.iter().find(|x| x.name == g.name) else {
            out.push([
                at,
                "group".into(),
                crate::tr!("無い", "missing"),
                crate::tr!("{}(up が作る)", "{} (up creates it)", g.name),
            ]);
            continue;
        };
        let wanted: BTreeMap<&String, (&String, bool)> = g
            .environment
            .iter()
            .map(|(k, v)| (k, (v, false)))
            .chain(g.secrets.iter().map(|(k, v)| (k, (v, true))))
            .collect();
        for v in &have.vars {
            let kind = if v.secure {
                crate::tr!("Secure", "Secure")
            } else if v.value.as_deref().is_some_and(|x| !x.is_empty()) {
                crate::tr!("値あり", "has a value")
            } else {
                crate::tr!("値なし", "no value")
            };
            let note = match wanted.get(&v.name) {
                None => crate::tr!(
                    "(compose ファイルに無い。up が消す)",
                    "(not in the compose file; up deletes it)"
                ),
                Some((value, _)) if value.is_empty() => {
                    crate::tr!("(値が空。up が消す)", "(empty value; up deletes it)")
                }
                Some((_, secure)) if *secure != v.secure => crate::tr!(
                    "(Secure かが違う。up が直す)",
                    "(Secure differs; up fixes it)"
                ),
                Some((value, false)) if v.value.as_deref() != Some(value.as_str()) => {
                    crate::tr!("(値が違う。up が変える)", "(value differs; up changes it)")
                }
                Some(_) => String::new(),
            };
            out.push([
                at.clone(),
                "variable".into(),
                crate::tr!("あり", "present"),
                format!("{}  {kind}{note}", v.name),
            ]);
        }
        for (name, (value, _)) in &wanted {
            if !value.is_empty() && !have.vars.iter().any(|v| &&v.name == name) {
                out.push([
                    at.clone(),
                    "variable".into(),
                    crate::tr!("無い", "missing"),
                    crate::tr!("{name}(up が作る)", "{name} (up creates it)"),
                ]);
            }
        }
    }
    for h in groups
        .iter()
        .filter(|h| !want.groups.iter().any(|g| g.name == h.name))
    {
        out.push([
            "-".into(),
            "group".into(),
            crate::tr!("残り(up が消す)", "orphan (up deletes it)"),
            h.name.clone(),
        ]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::VarGroup;

    /// (group, environment, secrets)
    type Spec<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a [(&'a str, &'a str)]);

    fn want(groups: &[Spec]) -> Codemagic {
        Codemagic {
            repository: "https://github.com/example/shop".into(),
            groups: groups
                .iter()
                .map(|(n, env, sec)| VarGroup {
                    name: n.to_string(),
                    environment: env
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                    secrets: sec
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                })
                .collect(),
        }
    }

    fn var(id: &str, name: &str, value: Option<&str>, secure: bool) -> Var {
        Var {
            id: id.into(),
            name: name.into(),
            value: value.map(str::to_string),
            secure,
        }
    }

    #[test]
    fn a_new_app_gets_every_group_and_variable() {
        let w = want(&[("store", &[("DOMAIN", "shop.example")], &[("KEY", "k1")])]);
        let d = diff(&w, &[], false);
        assert_eq!(
            d.changes,
            vec![
                Change::CreateGroup("store".into()),
                Change::Add {
                    group: "store".into(),
                    secure: false,
                    vars: vec![("DOMAIN".into(), "shop.example".into())]
                },
                Change::Add {
                    group: "store".into(),
                    secure: true,
                    vars: vec![("KEY".into(), "k1".into())]
                },
            ]
        );
    }

    #[test]
    fn the_same_state_changes_nothing_and_secure_values_stay() {
        let w = want(&[(
            "store",
            &[("DOMAIN", "shop.example")],
            &[("KEY", "changed")],
        )]);
        let now = vec![Group {
            id: "g1".into(),
            name: "store".into(),
            vars: vec![
                var("v1", "DOMAIN", Some("shop.example"), false),
                var("v2", "KEY", None, true),
            ],
        }];
        let d = diff(&w, &now, false);
        assert!(d.changes.is_empty(), "{:?}", d.changes);
        assert_eq!(d.kept, vec![("store".to_string(), "KEY".to_string())]);
        // --update-secret sets the Secure value again.
        let d = diff(&w, &now, true);
        assert!(d.kept.is_empty());
        assert_eq!(d.changes.len(), 1);
        assert!(
            matches!(&d.changes[0], Change::Update { id, value, secure: None, .. } if id == "v2" && value == "changed")
        );
    }

    #[test]
    fn plain_values_follow_and_unwritten_or_empty_ones_go() {
        let w = want(&[("store", &[("DOMAIN", "new.example"), ("EMPTY", "")], &[])]);
        let now = vec![
            Group {
                id: "g1".into(),
                name: "store".into(),
                vars: vec![
                    var("v1", "DOMAIN", Some("old.example"), false),
                    var("v2", "EMPTY", Some("x"), false),
                    var("v3", "OLD", Some("y"), false),
                ],
            },
            Group {
                id: "g2".into(),
                name: "added-by-hand".into(),
                vars: vec![],
            },
        ];
        let d = diff(&w, &now, false);
        assert_eq!(d.changes.len(), 4, "{:?}", d.changes);
        assert!(
            matches!(&d.changes[0], Change::Update { id, old: Some(o), value, .. } if id == "v1" && o == "old.example" && value == "new.example")
        );
        assert!(matches!(&d.changes[1], Change::DeleteVar { id, .. } if id == "v2"));
        assert!(matches!(&d.changes[2], Change::DeleteVar { id, .. } if id == "v3"));
        assert!(matches!(&d.changes[3], Change::DeleteGroup { id, .. } if id == "g2"));
        // An empty value that is not there is not put in.
        let d = diff(&want(&[("store", &[("EMPTY", "")], &[])]), &[], false);
        assert_eq!(d.changes, vec![Change::CreateGroup("store".into())]);
    }

    #[test]
    fn a_variable_turning_secure_or_plain_is_set_again_with_its_value() {
        let w = want(&[("store", &[("A", "plain")], &[("B", "secret")])]);
        let now = vec![Group {
            id: "g1".into(),
            name: "store".into(),
            vars: vec![
                var("v1", "A", None, true),
                var("v2", "B", Some("was-plain"), false),
            ],
        }];
        let d = diff(&w, &now, false);
        assert!(
            matches!(&d.changes[0], Change::Update { id, secure: Some(false), value, .. } if id == "v1" && value == "plain")
        );
        assert!(
            matches!(&d.changes[1], Change::Update { id, secure: Some(true), value, .. } if id == "v2" && value == "secret")
        );
        // The plan never shows a Secure value.
        let shown: Vec<String> = d.changes.iter().flat_map(|c| c.details()).collect();
        assert!(!shown.iter().any(|l| l.contains("secret\"")), "{shown:?}");
    }

    #[test]
    fn messages_never_carry_the_token_or_secure_values() {
        let cm = Cm::new("tok-123".into(), vec!["s3cr3t".into(), String::new()]).unwrap();
        assert_eq!(
            cm.hide("bad token tok-123 and s3cr3t"),
            "bad token *** and ***"
        );
    }
}
