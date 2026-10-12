//! Variable interpolation in the compose file (REQ-004, DES-003), with the
//! same rules as docker compose: `${VAR}`, `$VAR`, `${VAR:-default}`,
//! `${VAR-default}`, `${VAR:?message}`, `${VAR?message}`, `${VAR:+alt}`,
//! `${VAR+alt}` and `$$` for a literal `$`. Defaults and alternatives may
//! themselves contain variables.

use std::collections::BTreeSet;

/// Where values come from: the process environment first, then `.env.<env>`.
pub trait Lookup {
    fn get(&self, name: &str) -> Option<String>;
}

/// A variable that had no value and no default. `message` is the text from
/// `${VAR:?message}` when one was given.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Missing {
    pub name: String,
    pub message: Option<String>,
    /// Not a missing value but a malformed template (`$5`, `${}`, an
    /// unterminated `${`), which docker compose also refuses.
    pub invalid: bool,
}

fn invalid(text: String) -> Missing {
    Missing {
        name: text,
        message: None,
        invalid: true,
    }
}

/// Expand `input`. Every missing variable is recorded in `missing` (so the
/// caller can report them all at once) and expands to an empty string.
pub fn expand(input: &str, vars: &dyn Lookup, missing: &mut BTreeSet<Missing>) -> String {
    let mut out = String::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '$' {
            out.push(c);
            i += 1;
            continue;
        }
        match chars.get(i + 1) {
            Some('$') => {
                out.push('$');
                i += 2;
            }
            Some('{') => match matching_brace(&chars, i + 2) {
                Some(end) => {
                    let body: String = chars[i + 2..end].iter().collect();
                    out.push_str(&braced(&body, vars, missing));
                    i = end + 1;
                }
                None => {
                    missing.insert(invalid(chars[i..].iter().collect()));
                    break;
                }
            },
            Some(&n) if n == '_' || n.is_ascii_alphabetic() => {
                let start = i + 1;
                let mut end = start;
                while end < chars.len() && (chars[end] == '_' || chars[end].is_ascii_alphanumeric())
                {
                    end += 1;
                }
                let name: String = chars[start..end].iter().collect();
                out.push_str(&plain(&name, vars, missing));
                i = end;
            }
            // `$` before anything else (`$5`, `$-`, a lone `$`): docker
            // compose refuses the template; `$$` writes a `$`.
            _ => {
                let end = (i + 2).min(chars.len());
                missing.insert(invalid(chars[i..end].iter().collect()));
                i = end;
            }
        }
    }
    out
}

fn matching_brace(chars: &[char], from: usize) -> Option<usize> {
    let mut depth = 1;
    let mut i = from;
    while i < chars.len() {
        match chars[i] {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn plain(name: &str, vars: &dyn Lookup, missing: &mut BTreeSet<Missing>) -> String {
    match vars.get(name) {
        Some(v) => v,
        None => {
            missing.insert(Missing {
                name: name.to_string(),
                message: None,
                invalid: false,
            });
            String::new()
        }
    }
}

fn braced(body: &str, vars: &dyn Lookup, missing: &mut BTreeSet<Missing>) -> String {
    let name_end = body
        .find(|c: char| !(c == '_' || c.is_ascii_alphanumeric()))
        .unwrap_or(body.len());
    let name = &body[..name_end];
    let rest = &body[name_end..];
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        missing.insert(invalid(format!("${{{body}}}")));
        return String::new();
    }
    let value = vars.get(name);
    let set_nonempty = value.as_deref().is_some_and(|v| !v.is_empty());
    let set = value.is_some();
    let (op, arg) = if let Some(a) = rest.strip_prefix(":-") {
        (":-", a)
    } else if let Some(a) = rest.strip_prefix(":?") {
        (":?", a)
    } else if let Some(a) = rest.strip_prefix(":+") {
        (":+", a)
    } else if let Some(a) = rest.strip_prefix('-') {
        ("-", a)
    } else if let Some(a) = rest.strip_prefix('?') {
        ("?", a)
    } else if let Some(a) = rest.strip_prefix('+') {
        ("+", a)
    } else if rest.is_empty() {
        ("", "")
    } else {
        missing.insert(invalid(format!("${{{body}}}")));
        return String::new();
    };
    match op {
        "" => plain(name, vars, missing),
        ":-" if set_nonempty => value.unwrap_or_default(),
        ":-" => expand(arg, vars, missing),
        "-" if set => value.unwrap_or_default(),
        "-" => expand(arg, vars, missing),
        ":+" if set_nonempty => expand(arg, vars, missing),
        ":+" => String::new(),
        "+" if set => expand(arg, vars, missing),
        "+" => String::new(),
        ":?" | "?" => {
            let ok = if op == ":?" { set_nonempty } else { set };
            if ok {
                value.unwrap_or_default()
            } else {
                let message = expand(arg, vars, missing);
                missing.insert(Missing {
                    name: name.to_string(),
                    message: (!message.is_empty()).then_some(message),
                    invalid: false,
                });
                String::new()
            }
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Map(BTreeMap<&'static str, &'static str>);
    impl Lookup for Map {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).map(|v| v.to_string())
        }
    }

    fn vars() -> Map {
        Map(BTreeMap::from([
            ("A", "a"),
            ("EMPTY", ""),
            ("HOST", "x.example"),
        ]))
    }

    fn run(s: &str) -> (String, Vec<String>) {
        let mut missing = BTreeSet::new();
        let out = expand(s, &vars(), &mut missing);
        (out, missing.into_iter().map(|m| m.name).collect())
    }

    #[test]
    fn expands_like_docker_compose() {
        assert_eq!(run("${A}-$A-$$A").0, "a-a-$A");
        assert_eq!(run("${EMPTY:-d}|${EMPTY-d}").0, "d|");
        assert_eq!(run("${NONE:-d}|${NONE-d}").0, "d|d");
        assert_eq!(run("${A:+alt}|${EMPTY:+alt}|${EMPTY+alt}").0, "alt||alt");
        assert_eq!(run("${NONE:-${HOST}}").0, "x.example");
        assert_eq!(run("https://${HOST}/p").0, "https://x.example/p");
        assert_eq!(run("cost $$5").0, "cost $5");
    }

    #[test]
    fn refuses_malformed_templates_like_docker() {
        for bad in [
            "cost $5", "a $- b", "end $", "${}", "${:-x}", "${1A}", "a${Z", "${A!}",
        ] {
            let mut m = BTreeSet::new();
            expand(bad, &vars(), &mut m);
            assert!(m.iter().any(|x| x.invalid), "{bad}: {m:?}");
        }
    }

    #[test]
    fn records_every_missing_variable() {
        let (_, missing) = run("${X}/${Y}/${A}/$Z");
        assert_eq!(missing, ["X", "Y", "Z"]);
        let mut m = BTreeSet::new();
        expand("${EMPTY:?need it}", &vars(), &mut m);
        let got = m.into_iter().next().unwrap();
        assert_eq!(got.name, "EMPTY");
        assert_eq!(got.message.as_deref(), Some("need it"));
    }
}
