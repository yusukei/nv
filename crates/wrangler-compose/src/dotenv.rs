//! Reading `.env.<env>` (DES-005). Supports the basic dotenv form:
//! `NAME=value`, `export NAME=value`, `#` comments, single/double quotes.

use std::collections::BTreeMap;

pub fn parse(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        out.insert(name.to_string(), unquote(value.trim()));
    }
    out
}

fn unquote(v: &str) -> String {
    let bytes = v.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if first == b'"' && last == b'"' {
            return v[1..v.len() - 1].replace("\\n", "\n").replace("\\\"", "\"");
        }
        if first == b'\'' && last == b'\'' {
            return v[1..v.len() - 1].to_string();
        }
    }
    // Unquoted values end at an inline comment.
    match v.find(" #") {
        Some(i) => v[..i].trim_end().to_string(),
        None => v.to_string(),
    }
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
}
