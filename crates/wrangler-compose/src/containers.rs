//! Cloudflare Containers in a local environment (REQ-020, DES-020).
//! `wrangler dev` builds and runs them with Docker; the tool checks Docker
//! first, watches what an image is built from, and clears away what its
//! own runs leave behind (and nothing of other projects).

use crate::compose::{Container, Image};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The prefix Wrangler gives the images it builds for `wrangler dev`.
const DEV_IMAGES: &str = "cloudflare-dev/";

/// The image a line of `wrangler dev`'s output names as just built:
/// BuildKit's `naming to docker.io/cloudflare-dev/<class>:<id>` or the
/// classic builder's `Successfully tagged cloudflare-dev/<class>:<id>`.
pub fn image_tag_in(line: &str) -> Option<String> {
    let rest = line
        .split_once("naming to ")
        .or_else(|| line.split_once("Successfully tagged "))?
        .1;
    let tag = rest.split_whitespace().next()?;
    let tag = tag.strip_prefix("docker.io/").unwrap_or(tag);
    (tag.starts_with(DEV_IMAGES) && tag.contains(':')).then(|| tag.to_string())
}

/// From `docker ps -a` lines (`<name>\t<image>`), the containers of this
/// project (named `workerd-<prefix>…`, sidecars included) and the images
/// they run that Wrangler built for it (`cloudflare-dev/…`).
pub fn ours(listing: &str, prefix: &str) -> (Vec<String>, Vec<String>) {
    let want = format!("workerd-{prefix}");
    let (mut names, mut images) = (Vec::new(), Vec::new());
    for line in listing.lines() {
        let Some((name, image)) = line.split_once('\t') else {
            continue;
        };
        if !name.starts_with(&want) {
            continue;
        }
        names.push(name.to_string());
        if image.starts_with(DEV_IMAGES) && !images.iter().any(|i| i == image) {
            images.push(image.to_string());
        }
    }
    (names, images)
}

/// The aliases `wrangler dev` gave registry images in one start: tags of
/// `cloudflare-dev/<class>` that were not there before and point at the
/// pulled image (`wanted`: class → image ID). Wrangler prints nothing when
/// it makes them. Only a project with the same class and image starting at
/// the same moment could have made one too, and such an alias is only
/// another name of the same image.
pub fn new_aliases(before: &str, after: &str, wanted: &[(String, String)]) -> Vec<String> {
    let old: Vec<&str> = before
        .lines()
        .filter_map(|l| l.split('\t').next())
        .collect();
    let mut out = Vec::new();
    for line in after.lines() {
        let Some((tag, id)) = line.split_once('\t') else {
            continue;
        };
        if old.contains(&tag) {
            continue;
        }
        let repo = tag.rsplit_once(':').map(|(r, _)| r).unwrap_or(tag);
        let ours = wanted.iter().any(|(class, want)| {
            repo == format!("{DEV_IMAGES}{}", class.to_lowercase()) && id == want
        });
        if ours && !out.iter().any(|x| x == tag) {
            out.push(tag.to_string());
        }
    }
    out
}

/// `cloudflare-dev/…` tags and their image IDs, for `new_aliases`.
pub fn alias_listing() -> String {
    docker_out(&[
        "images",
        "--no-trunc",
        "--filter",
        "reference=cloudflare-dev/*",
        "--format",
        "{{.Repository}}:{{.Tag}}\t{{.ID}}",
    ])
    .unwrap_or_default()
}

/// Class → image ID of the containers that use a registry image.
pub fn registry_ids(list: &[Container]) -> Vec<(String, String)> {
    list.iter()
        .filter_map(|k| match &k.image {
            Image::Registry(r) => {
                let id = docker_out(&["image", "inspect", "--format", "{{.Id}}", r])?;
                Some((k.class.clone(), id.trim().to_string()))
            }
            Image::Dockerfile(_) => None,
        })
        .collect()
}

/// From `docker ps -a` lines (`<name>\t<state>`), the running instances of
/// each wanted container (`<local Worker name>`, `<class>`), and this
/// project's containers that belong to none of them (left over, REQ-020).
pub fn instances(
    listing: &str,
    prefix: &str,
    wanted: &[(String, String)],
) -> (Vec<usize>, Vec<String>) {
    let mut counts = vec![0; wanted.len()];
    let mut strays = Vec::new();
    for line in listing.lines() {
        let Some((name, state)) = line.split_once('\t') else {
            continue;
        };
        if !name.starts_with(&format!("workerd-{prefix}")) {
            continue;
        }
        let owner = wanted
            .iter()
            .position(|(worker, class)| name.starts_with(&format!("workerd-{worker}-{class}-")));
        match owner {
            Some(i) if !name.ends_with("-proxy") && state == "running" => counts[i] += 1,
            Some(_) => {}
            None => strays.push(name.to_string()),
        }
    }
    (counts, strays)
}

/// `docker ps -a` as `<name>\t<state>`; None when Docker does not answer.
pub fn container_listing() -> Option<String> {
    docker_out(&["ps", "-a", "--format", "{{.Names}}\t{{.State}}"])
}

/// The name of a container application on Cloudflare (REQ-006, DES-021):
/// `<Worker>-<class>` lowercased with every run of `-` made one (Cloudflare
/// refuses `--`), then the first 8 hex digits of the SHA-256 of
/// `<Worker>-<class>`, so that `a-b`+`c` and `a`+`b-c` never meet. At most
/// 63 characters. Whose it is, is told by its Durable Object, not its name.
pub fn app_name(script: &str, class: &str) -> String {
    use sha2::{Digest, Sha256};
    let full = format!("{script}-{class}");
    let mark = &crate::files::hex(&Sha256::digest(full.as_bytes()))[..8];
    let mut readable = String::new();
    for c in full.to_lowercase().chars() {
        let c = if c.is_ascii_alphanumeric() { c } else { '-' };
        if !(c == '-' && (readable.is_empty() || readable.ends_with('-'))) {
            readable.push(c);
        }
    }
    readable.truncate(63 - 1 - mark.len());
    let readable = readable.trim_end_matches('-');
    format!("{readable}-{mark}")
}

/// What an image is made from, by content (12 hex digits): the Dockerfile,
/// every file under the build context by its path there, and the build
/// variables; or the registry reference. The same on every PC.
pub fn content_hash(k: &Container) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    match &k.image {
        Image::Registry(r) => {
            h.update(b"registry\0");
            h.update(r.as_bytes());
        }
        Image::Dockerfile(f) => {
            h.update(b"dockerfile\0");
            h.update(std::fs::read(f).unwrap_or_default());
            if let Some(dir) = &k.build_context {
                let mut files = Vec::new();
                walk(dir, &mut files);
                files.sort();
                for file in files {
                    let rel = file.strip_prefix(dir).unwrap_or(&file);
                    h.update(b"\0file\0");
                    h.update(rel.to_string_lossy().replace('\\', "/").as_bytes());
                    h.update(b"\0");
                    h.update(std::fs::read(&file).unwrap_or_default());
                }
            }
            for (name, value) in &k.image_vars {
                h.update(format!("\0var\0{name}={value}").as_bytes());
            }
        }
    }
    crate::files::hex(&h.finalize())[..12].to_string()
}

/// `docker build` arguments for a Dockerfile container, as Wrangler builds
/// it for a deploy (linux/amd64, no provenance, the build variables).
pub fn build_args(k: &Container, tag: &str) -> Option<Vec<String>> {
    let Image::Dockerfile(f) = &k.image else {
        return None;
    };
    let mut a: Vec<String> = [
        "build",
        "--load",
        "-t",
        tag,
        "--platform",
        "linux/amd64",
        "--provenance=false",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for (name, value) in &k.image_vars {
        a.push("--build-arg".into());
        a.push(format!("{name}={value}"));
    }
    a.push("-f".into());
    a.push(f.display().to_string());
    let ctx = k
        .build_context
        .clone()
        .or_else(|| f.parent().map(Path::to_path_buf));
    a.push(ctx.unwrap_or_default().display().to_string());
    Some(a)
}

/// Build an image with Docker (its output shown), for `wrangler containers
/// push` (DES-021).
pub fn build(k: &Container, tag: &str) -> Result<()> {
    let Some(args) = build_args(k, tag) else {
        return Ok(());
    };
    let status = docker()
        .args(&args)
        .stdout(std::process::Stdio::null())
        .status()
        .map_err(|e| anyhow::anyhow!("docker: {e}"))?;
    if !status.success() {
        bail!(crate::tr!(
            "コンテナ {} のイメージを作れません(上の出力を見てください)",
            "cannot build the image of container {} (see the output above)",
            k.class
        ));
    }
    Ok(())
}

/// Add images to the record `down --purge` deletes from.
pub fn remember(file: &Path, tags: &[String]) -> Result<()> {
    let mut all = remembered(file);
    let before = all.len();
    for t in tags {
        if !all.contains(t) {
            all.push(t.clone());
        }
    }
    if all.len() == before && file.exists() {
        return Ok(());
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(file, serde_json::to_string_pretty(&all)?)?;
    Ok(())
}

pub fn remembered(file: &Path) -> Vec<String> {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Move Durable Object storage to its new folder name (`old → new`, under
/// `state/v3/do`), unless the new folder already exists.
pub fn move_do_state(state: &Path, moves: &[(String, String)]) -> Result<()> {
    let root = state.join("v3").join("do");
    for (old, new) in moves {
        let (from, to) = (root.join(old), root.join(new));
        if from.is_dir() && !to.exists() {
            std::fs::rename(&from, &to).map_err(|e| {
                anyhow::anyhow!(crate::tr!(
                    "{} を {} へ移せません: {e}",
                    "cannot move {} to {}: {e}",
                    from.display(),
                    to.display()
                ))
            })?;
        }
    }
    Ok(())
}

/// What the images are built from, to rebuild when it changes: the newest
/// change and the number of files (a removed file changes the count).
pub fn inputs_stamp(list: &[Container]) -> (Option<SystemTime>, usize) {
    let mut files = Vec::new();
    for k in list {
        if let Image::Dockerfile(f) = &k.image {
            files.push(f.clone());
            if let Some(dir) = &k.build_context {
                walk(dir, &mut files);
            }
        }
    }
    files.sort();
    files.dedup();
    let newest = files
        .iter()
        .filter_map(|f| std::fs::metadata(f).and_then(|m| m.modified()).ok())
        .max();
    (newest, files.len())
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// The Docker command Wrangler uses too (`WRANGLER_DOCKER_BIN`, else
/// `docker`).
fn docker() -> std::process::Command {
    let bin = std::env::var("WRANGLER_DOCKER_BIN").unwrap_or_else(|_| "docker".into());
    let mut cmd = std::process::Command::new(bin);
    cmd.stdin(std::process::Stdio::null());
    cmd
}

fn docker_out(args: &[&str]) -> Option<String> {
    let out = docker().args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Docker must answer before anything starts, and before anything on
/// Cloudflare is touched (REQ-020). Wrangler runs containers locally on
/// Windows only inside WSL; building for a remote environment works there.
pub fn check_docker(local: bool) -> Result<()> {
    if local && cfg!(windows) {
        bail!(crate::tr!(
            "Windows の手元ではコンテナを動かせません(Wrangler が対応していません)。WSL の中で動かしてください",
            "containers cannot run locally on Windows (Wrangler does not support it); run it inside WSL"
        ));
    }
    if docker_out(&["info", "--format", "{{.ServerVersion}}"]).is_none() {
        bail!(crate::tr!(
            "compose ファイルにコンテナがあるので Docker が要りますが、Docker につながりません。Docker を起動してから、もう一度打ってください(Docker 以外なら WRANGLER_DOCKER_BIN と DOCKER_HOST を設定します)",
            "the compose file has containers, so Docker is needed, but Docker does not answer; start Docker and try again (for other engines, set WRANGLER_DOCKER_BIN and DOCKER_HOST)"
        ));
    }
    Ok(())
}

/// Remove this project's containers (and their sidecars), first noting the
/// images they ran. Quiet when Docker is not there.
pub fn clean_up(prefix: &str, record: &Path) {
    let Some(listing) = docker_out(&["ps", "-a", "--format", "{{.Names}}\t{{.Image}}"]) else {
        return;
    };
    let (names, images) = ours(&listing, prefix);
    let _ = remember(record, &images);
    if !names.is_empty() {
        let mut args = vec!["rm", "-f"];
        args.extend(names.iter().map(String::as_str));
        let _ = docker_out(&args);
    }
}

/// Delete the images this project's runs built (`down --purge`).
pub fn purge_images(record: &Path) -> Result<()> {
    let tags = remembered(record);
    for t in &tags {
        // An image already gone is fine; one still used stays (and is
        // reported by Docker).
        let out = docker().args(["rmi", t]).output();
        if let Ok(o) = out
            && !o.status.success()
        {
            let err = String::from_utf8_lossy(&o.stderr);
            if !err.contains("No such image") {
                eprint!("{err}");
            }
        }
    }
    if record.exists() {
        std::fs::remove_file(record)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn dir(tag: &str) -> Dir {
        let d = std::env::temp_dir().join(format!("wc-containers-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Dir(d)
    }

    #[test]
    fn built_images_are_read_from_wranglers_output() {
        assert_eq!(
            image_tag_in("#6 naming to docker.io/cloudflare-dev/pinger:6cd2d9e1 done").as_deref(),
            Some("cloudflare-dev/pinger:6cd2d9e1")
        );
        assert_eq!(
            image_tag_in("Successfully tagged cloudflare-dev/pinger:6cd2d9e1").as_deref(),
            Some("cloudflare-dev/pinger:6cd2d9e1")
        );
        assert_eq!(
            image_tag_in("#6 naming to docker.io/library/other:1 done"),
            None
        );
        assert_eq!(
            image_tag_in("[wrangler:info] Ready on http://localhost:8787"),
            None
        );
    }

    #[test]
    fn only_this_projects_containers_are_picked() {
        let listing = "\
workerd-shop--1a2b3c4d--api-Pinger-dd92\tcloudflare-dev/pinger:6cd2d9e1
workerd-shop--1a2b3c4d--api-Pinger-dd92-proxy\tcloudflare/proxy-everything:3cb1195
workerd-shop--9f9f9f9f--api-Pinger-dd92\tcloudflare-dev/pinger:aaaaaaaa
workerd-api-Pinger-dd92\tcloudflare-dev/pinger:bbbbbbbb
postgres\tpostgres:16
";
        let (names, images) = ours(listing, "shop--1a2b3c4d--");
        assert_eq!(
            names,
            vec![
                "workerd-shop--1a2b3c4d--api-Pinger-dd92",
                "workerd-shop--1a2b3c4d--api-Pinger-dd92-proxy"
            ]
        );
        assert_eq!(images, vec!["cloudflare-dev/pinger:6cd2d9e1"]);
    }

    #[test]
    fn remembered_images_add_up_without_repeats() {
        let d = dir("record");
        let f = d.0.join("local").join("container-images.json");
        assert!(remembered(&f).is_empty());
        remember(&f, &["cloudflare-dev/a:1".into()]).unwrap();
        remember(
            &f,
            &["cloudflare-dev/a:1".into(), "cloudflare-dev/b:2".into()],
        )
        .unwrap();
        assert_eq!(
            remembered(&f),
            vec!["cloudflare-dev/a:1", "cloudflare-dev/b:2"]
        );
    }

    #[test]
    fn durable_object_storage_moves_to_the_new_name_once() {
        let d = dir("do");
        let root = d.0.join("v3").join("do");
        std::fs::create_dir_all(root.join("api-Counter")).unwrap();
        std::fs::write(root.join("api-Counter/x.sqlite"), "data").unwrap();
        std::fs::create_dir_all(root.join("web-Other")).unwrap();
        std::fs::create_dir_all(root.join("shop--k--web-Other")).unwrap();
        let moves = vec![
            (
                "api-Counter".to_string(),
                "shop--k--api-Counter".to_string(),
            ),
            ("web-Other".to_string(), "shop--k--web-Other".to_string()),
            ("none-Gone".to_string(), "shop--k--none-Gone".to_string()),
        ];
        move_do_state(&d.0, &moves).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("shop--k--api-Counter/x.sqlite")).unwrap(),
            "data"
        );
        assert!(!root.join("api-Counter").exists());
        // The new folder was there already: the old one is left alone.
        assert!(root.join("web-Other").exists());
        assert!(!root.join("shop--k--none-Gone").exists());
        move_do_state(&d.0.join("missing"), &moves).unwrap();
    }

    #[test]
    fn a_change_under_the_build_context_changes_the_stamp() {
        let d = dir("stamp");
        let ctx = d.0.join("box");
        std::fs::create_dir_all(&ctx).unwrap();
        std::fs::write(ctx.join("Dockerfile"), "FROM busybox\n").unwrap();
        let list = vec![Container {
            class: "Pinger".into(),
            image: Image::Dockerfile(ctx.join("Dockerfile")),
            build_context: Some(ctx.clone()),
            image_vars: Default::default(),
            instance_type: None,
            max_instances: None,
        }];
        let before = inputs_stamp(&list);
        assert_eq!(before.1, 1);
        std::fs::write(ctx.join("index.html"), "v2").unwrap();
        let after = inputs_stamp(&list);
        assert_ne!(before, after);
        let registry = vec![Container {
            image: Image::Registry("docker.io/library/busybox:1.36".into()),
            build_context: None,
            ..list[0].clone()
        }];
        assert_eq!(inputs_stamp(&registry), (None, 0));
    }
    #[test]
    fn new_aliases_of_a_registry_image_are_ours() {
        let before = "cloudflare-dev/echo:aaaa1111\tsha256:busy\ncloudflare-dev/pinger:bbbb2222\tsha256:built\n";
        let after = "cloudflare-dev/echo:aaaa1111\tsha256:busy\n\
cloudflare-dev/echo:cccc3333\tsha256:busy\n\
cloudflare-dev/echo:dddd4444\tsha256:other\n\
cloudflare-dev/pinger:bbbb2222\tsha256:built\n\
cloudflare-dev/other:eeee5555\tsha256:busy\n";
        let wanted = vec![("Echo".to_string(), "sha256:busy".to_string())];
        assert_eq!(
            new_aliases(before, after, &wanted),
            vec!["cloudflare-dev/echo:cccc3333"]
        );
        assert!(new_aliases(after, after, &wanted).is_empty());
    }
    #[test]
    fn instances_are_counted_per_class_and_strays_are_found() {
        let listing = "\
workerd-shop--k--api-Pinger-aa11\trunning
workerd-shop--k--api-Pinger-aa11-proxy\trunning
workerd-shop--k--api-Pinger-bb22\trunning
workerd-shop--k--api-Pinger-cc33\texited
workerd-shop--k--api-Old-dd44\texited
workerd-shop--k--api-Old-dd44-proxy\texited
workerd-shop--x--api-Pinger-ee55\trunning
workerd-api-Pinger-ff66\trunning
";
        let wanted = vec![
            ("shop--k--api".to_string(), "Pinger".to_string()),
            ("shop--k--api".to_string(), "Echo".to_string()),
        ];
        let (counts, strays) = instances(listing, "shop--k--", &wanted);
        assert_eq!(counts, vec![2, 0]);
        assert_eq!(
            strays,
            vec![
                "workerd-shop--k--api-Old-dd44",
                "workerd-shop--k--api-Old-dd44-proxy"
            ]
        );
    }
    #[test]
    fn application_names_have_no_double_dash_and_never_collide() {
        let n = app_name("shop--staging--api", "Pinger");
        assert!(n.starts_with("shop-staging-api-pinger-"), "{n}");
        assert!(!n.contains("--"), "{n}");
        assert_eq!(n.len(), "shop-staging-api-pinger-".len() + 8);
        assert_eq!(n, app_name("shop--staging--api", "Pinger"), "stable");
        // a-b + c and a + b-c read the same without the mark.
        assert_ne!(app_name("a-b--c--api", "K"), app_name("a--b-c--api", "K"));
        let long = app_name(&format!("{}--staging--api", "p".repeat(80)), "Pinger");
        assert!(
            long.len() <= 63 && !long.contains("--") && !long.starts_with('-'),
            "{long}"
        );
    }

    fn box_at(root: &Path, answer: &str) -> Container {
        let ctx = root.join("box");
        std::fs::create_dir_all(ctx.join("www")).unwrap();
        std::fs::write(ctx.join("Dockerfile"), "FROM busybox\nCOPY www /www\n").unwrap();
        std::fs::write(ctx.join("www/index.html"), answer).unwrap();
        Container {
            class: "Pinger".into(),
            image: Image::Dockerfile(ctx.join("Dockerfile")),
            build_context: Some(ctx),
            image_vars: Default::default(),
            instance_type: None,
            max_instances: None,
        }
    }

    #[test]
    fn the_content_hash_follows_the_content_not_the_folder() {
        let (a, b) = (dir("hash-a"), dir("hash-b"));
        let (ka, kb) = (box_at(&a.0, "v1"), box_at(&b.0, "v1"));
        assert_eq!(
            content_hash(&ka),
            content_hash(&kb),
            "same content in two folders"
        );
        std::fs::write(b.0.join("box/www/index.html"), "v2").unwrap();
        assert_ne!(content_hash(&ka), content_hash(&kb), "a changed file");
        let mut kv = ka.clone();
        kv.image_vars.insert("V".into(), "1".into());
        assert_ne!(content_hash(&ka), content_hash(&kv), "a build variable");
        let reg = |r: &str| Container {
            image: Image::Registry(r.into()),
            build_context: None,
            ..ka.clone()
        };
        assert_eq!(
            content_hash(&reg("docker.io/library/busybox:1.36")),
            content_hash(&reg("docker.io/library/busybox:1.36"))
        );
        assert_ne!(
            content_hash(&reg("docker.io/library/busybox:1.36")),
            content_hash(&reg("docker.io/library/busybox:1.37"))
        );
        assert_eq!(content_hash(&ka).len(), 12);
    }
    #[test]
    fn images_are_built_as_wrangler_builds_them() {
        let d = dir("args");
        let mut k = box_at(&d.0, "v1");
        k.image_vars.insert("V".into(), "1".into());
        let f = d.0.join("box/Dockerfile");
        let ctx = d.0.join("box");
        assert_eq!(
            build_args(&k, "repo:wc-push").unwrap(),
            vec![
                "build".to_string(),
                "--load".into(),
                "-t".into(),
                "repo:wc-push".into(),
                "--platform".into(),
                "linux/amd64".into(),
                "--provenance=false".into(),
                "--build-arg".into(),
                "V=1".into(),
                "-f".into(),
                f.display().to_string(),
                ctx.display().to_string(),
            ]
        );
        let reg = Container {
            image: Image::Registry("busybox:1".into()),
            build_context: None,
            ..k
        };
        assert!(build_args(&reg, "x:y").is_none());
    }
}
