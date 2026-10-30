//! Reading `.env.<env>` (DES-005) with docker compose's rules:
//! `NAME=value`, `export NAME=value`, `#` comments, and
//! - unquoted values end at ` #` and are trimmed;
//! - `'single'` values are taken as written;
//! - `"double"` values may span lines and turn `\n`, `\t`, `\r`, `\"` and
//!   `\\` into the characters; anything after the closing quote (a comment)
//!   is ignored;
//! - unquoted and double-quoted values expand `${VAR}` from the process
//!   environment and the names defined earlier in the file.

use crate::interp::{self, Lookup};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

struct Earlier<'a>(&'a BTreeMap<String, String>);

impl Lookup for Earlier<'_> {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name)
            .ok()
            .or_else(|| self.0.get(name).cloned())
    }
}

pub fn parse(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // One entry: skip blank space and comment lines first.
        let line_end = chars[i..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(chars.len(), |p| i + p);
        let line: String = chars[i..line_end].iter().collect();
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i = line_end + 1;
            continue;
        }
        let Some(eq) = line.find('=') else {
            i = line_end + 1;
            continue;
        };
        let name = line[..eq].trim();
        let name = name.strip_prefix("export ").unwrap_or(name).trim();
        // Where the value starts, counted in chars.
        let mut j = i + line[..eq].chars().count() + 1;
        while j < line_end && (chars[j] == ' ' || chars[j] == '\t') {
            j += 1;
        }
        let (value, next) = match chars.get(j) {
            Some('\'') => {
                let close = chars[j + 1..].iter().position(|&c| c == '\'');
                match close {
                    Some(p) => (
                        Raw::Literal(chars[j + 1..j + 1 + p].iter().collect()),
                        end_of_line(&chars, j + 2 + p),
                    ),
                    None => (
                        Raw::Literal(chars[j + 1..line_end].iter().collect()),
                        line_end + 1,
                    ),
                }
            }
            Some('"') => {
                let mut v = String::new();
                let mut k = j + 1;
                let mut closed = false;
                while k < chars.len() {
                    match chars[k] {
                        '\\' if k + 1 < chars.len() => {
                            match chars[k + 1] {
                                'n' => v.push('\n'),
                                't' => v.push('\t'),
                                'r' => v.push('\r'),
                                '"' => v.push('"'),
                                '\\' => v.push('\\'),
                                c => {
                                    v.push('\\');
                                    v.push(c);
                                }
                            }
                            k += 2;
                        }
                        '"' => {
                            closed = true;
                            break;
                        }
                        c => {
                            v.push(c);
                            k += 1;
                        }
                    }
                }
                let next = if closed {
                    end_of_line(&chars, k + 1)
                } else {
                    chars.len()
                };
                (Raw::Expand(v), next)
            }
            _ => {
                let rest: String = chars[j..line_end].iter().collect();
                let v = match rest.find(" #").or_else(|| rest.find("\t#")) {
                    Some(p) => rest[..p].to_string(),
                    None => rest,
                };
                (Raw::Expand(v.trim().to_string()), line_end + 1)
            }
        };
        if !name.is_empty() {
            let value = match value {
                Raw::Literal(v) => v,
                Raw::Expand(v) => {
                    let mut missing = BTreeSet::new();
                    interp::expand(&v, &Earlier(&out), &mut missing)
                }
            };
            out.insert(name.to_string(), value);
        }
        i = next;
    }
    out
}

enum Raw {
    Literal(String),
    Expand(String),
}

/// Adds `NAME=value` as the last line (REQ-007: the one line the tool may
/// write, CODEMAGIC_APP_ID). Every other line and the file's permissions
/// stay as they are.
pub fn append_line(path: &Path, name: &str, value: &str) -> Result<()> {
    let mut text = std::fs::read_to_string(path).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("{name}={value}\n"));
    replace(path, &text)
}

/// Removes the lines starting with `NAME=`, and nothing else.
pub fn remove_lines(path: &Path, name: &str) -> Result<()> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(());
    };
    let start = format!("{name}=");
    let kept: String = text
        .split_inclusive('\n')
        .filter(|l| !l.starts_with(&start))
        .collect();
    if kept != text {
        replace(path, &kept)?;
    }
    Ok(())
}

/// Writes a file in the same folder first and moves it into place, so a
/// stop half-way never leaves a broken `.env` (new files are 0600).
fn replace(path: &Path, text: &str) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.wc-{}.tmp", std::process::id()));
    let perms = std::fs::metadata(path).ok().map(|m| m.permissions());
    let write = || -> Result<()> {
        crate::local::write_private(&tmp, text)?;
        if let Some(p) = &perms {
            std::fs::set_permissions(&tmp, p.clone())?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    };
    let r = write();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

/// Just past the end of the line holding `from` (what follows a closing
/// quote there is a comment).
fn end_of_line(chars: &[char], from: usize) -> usize {
    chars[from.min(chars.len())..]
        .iter()
        .position(|&c| c == '\n')
        .map_or(chars.len(), |p| from + p + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_forms() {
        let env = parse(
            "# comment\nA=1\nexport B = two \nC=\"quoted # not comment\"\nD='single'\nE=plain # trailing\nnot a pair\n=novalue\nF=\n",
        );
        assert_eq!(env["A"], "1");
        assert_eq!(env["B"], "two");
        assert_eq!(env["C"], "quoted # not comment");
        assert_eq!(env["D"], "single");
        assert_eq!(env["E"], "plain");
        assert_eq!(env["F"], "");
        assert_eq!(env.len(), 6);
    }

    #[test]
    fn expands_escapes_in_double_quotes_only() {
        let env = parse("A=\"x\\ny\"\nB='x\\ny'\n");
        assert_eq!(env["A"], "x\ny");
        assert_eq!(env["B"], "x\\ny");
    }

    #[test]
    fn follows_docker_for_comments_lines_and_variables() {
        let env = parse(
            "A=\"x\" # note\nB='y' # note\nM=\"one\ntwo\"\nBASE=/srv\nP=${BASE}/app\nQ=\"${BASE}/q\"\nR='${BASE}'\nS=\"a\\\"b\"\n",
        );
        assert_eq!(env["A"], "x");
        assert_eq!(env["B"], "y");
        assert_eq!(env["M"], "one\ntwo");
        assert_eq!(env["P"], "/srv/app");
        assert_eq!(env["Q"], "/srv/q");
        assert_eq!(env["R"], "${BASE}");
        assert_eq!(env["S"], "a\"b");
    }

    #[test]
    fn writes_and_removes_only_the_app_id_line() {
        let dir = std::env::temp_dir().join(format!("wc-dotenv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(".env.ac-1");
        // No newline at the end: one is added before the new line.
        std::fs::write(&f, "# keep\nA=1\nCODEMAGIC_APP_IDX=keep\nB=\"x\ny\"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o640)).unwrap();
        }
        append_line(&f, "CODEMAGIC_APP_ID", "app1").unwrap();
        let text = std::fs::read_to_string(&f).unwrap();
        assert_eq!(text, "# keep\nA=1\nCODEMAGIC_APP_IDX=keep\nB=\"x\ny\"\nCODEMAGIC_APP_ID=app1\n");
        assert_eq!(parse(&text)["CODEMAGIC_APP_ID"], "app1");
        remove_lines(&f, "CODEMAGIC_APP_ID").unwrap();
        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "# keep\nA=1\nCODEMAGIC_APP_IDX=keep\nB=\"x\ny\"\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o640);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
