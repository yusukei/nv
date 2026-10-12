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
use std::collections::{BTreeMap, BTreeSet};

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
}
