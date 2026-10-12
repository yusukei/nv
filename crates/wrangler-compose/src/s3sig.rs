//! Requests that reach an r2 service's port from another machine (DES-019):
//! the local simulator turns away `/cdn-cgi/` requests whose `Host` or
//! `Origin` is not a local name before it looks at the signature. This
//! checks the signature as it came (the `Host` the client used), and when it
//! holds signs the request again for `localhost`, so the simulator checks it
//! once more and answers as for a local request. A request whose signature
//! does not hold is passed on unsigned again, and the simulator refuses it
//! for the reason R2 would. The canonical request is built the way the
//! simulator builds it (miniflare's r2/s3 worker).

use sha2::{Digest, Sha256};

const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// The names the simulator accepts in `Host` and `Origin` (miniflare's
/// LOCALHOST_HOSTNAMES).
fn is_local(hostname: &str) -> bool {
    matches!(hostname, "localhost" | "127.0.0.1" | "[::1]")
}

/// The host name of a `Host` header (`a.b:9050`, `[::1]:9050`), lower case.
fn hostname_of_host(host: &str) -> String {
    let host = host.trim().to_ascii_lowercase();
    if host.starts_with('[') {
        return host.split_inclusive(']').next().unwrap_or("").to_string();
    }
    host.split(':').next().unwrap_or("").to_string()
}

/// The host name of an `Origin` header, or None when it is not a URL (`null`).
fn hostname_of_origin(origin: &str) -> Option<String> {
    let rest = origin.trim().split_once("://")?.1;
    let host = rest.split('/').next().unwrap_or("");
    let h = hostname_of_host(host);
    (!h.is_empty()).then_some(h)
}

/// What to send on to the simulator.
#[derive(Debug, PartialEq)]
pub struct Rewritten {
    /// The request target (path and query).
    pub target: String,
    pub headers: Vec<(String, String)>,
    /// Whether the signature held for the `Host` the client used and was
    /// replaced by one for `localhost`.
    pub resigned: bool,
}

/// None when the simulator takes the request as it is (local `Host`, and no
/// `Origin` or a local one). Otherwise the request with `Host` set to
/// `localhost` (keeping the port), without `Origin`, and signed again when
/// its signature held.
pub fn rewrite(
    method: &str,
    target: &str,
    headers: &[(String, String)],
    access_key: &str,
    secret: &str,
) -> Option<Rewritten> {
    let get = |name: &str| -> Option<String> {
        let v: Vec<&str> = headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect();
        (!v.is_empty()).then(|| v.join(", "))
    };
    let host = get("host").unwrap_or_default();
    let origin = get("origin");
    let host_ok = is_local(&hostname_of_host(&host));
    let origin_ok = origin
        .as_deref()
        .is_none_or(|o| hostname_of_origin(o).is_some_and(|h| is_local(&h)));
    if host_ok && origin_ok {
        return None;
    }
    let local_host = match host.trim().rsplit_once(':') {
        Some((h, port)) if !h.is_empty() && !port.contains(']') && port.bytes().all(|b| b.is_ascii_digit()) => {
            format!("localhost:{port}")
        }
        _ => "localhost".to_string(),
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("origin") {
            continue;
        }
        if k.eq_ignore_ascii_case("host") {
            if !out.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
                out.push((k.clone(), local_host.clone()));
            }
            continue;
        }
        out.push((k.clone(), v.clone()));
    }
    let get_out = |name: &str| -> Option<String> {
        let v: Vec<&str> = out
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect();
        (!v.is_empty()).then(|| v.join(", "))
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let params = form_pairs(query);
    let mut result = Rewritten {
        target: target.to_string(),
        headers: out.clone(),
        resigned: false,
    };
    if let Some(auth) = get("authorization") {
        // Signed in the Authorization header.
        let Some(fields) = auth.strip_prefix(&format!("{ALGORITHM} ")).map(auth_fields) else {
            return Some(result);
        };
        let (Some(cred), Some(signed), Some(sig)) = (
            fields.get("Credential"),
            fields.get("SignedHeaders"),
            fields.get("Signature"),
        ) else {
            return Some(result);
        };
        let Some(amz_date) = get("x-amz-date").or_else(|| get("date")) else {
            return Some(result);
        };
        let Some(payload) = get("x-amz-content-sha256") else {
            return Some(result);
        };
        let Some(scope) = scope_of(cred, access_key) else {
            return Some(result);
        };
        let signed = signed_headers(signed);
        let q = canonical_query(&params, false);
        let before = signature(method, path, &q, &get, &signed, &payload, &amz_date, &scope, secret);
        if !eq_const(&before, sig) {
            return Some(result);
        }
        let after = signature(method, path, &q, &get_out, &signed, &payload, &amz_date, &scope, secret);
        let new_auth = auth.replacen(&format!("Signature={sig}"), &format!("Signature={after}"), 1);
        for (k, v) in result.headers.iter_mut() {
            if k.eq_ignore_ascii_case("authorization") {
                *v = new_auth.clone();
            }
        }
        result.resigned = true;
        return Some(result);
    }
    // A signed URL.
    let param = |name: &str| params.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    let (Some(alg), Some(cred), Some(amz_date), Some(signed), Some(sig)) = (
        param("X-Amz-Algorithm"),
        param("X-Amz-Credential"),
        param("X-Amz-Date"),
        param("X-Amz-SignedHeaders"),
        param("X-Amz-Signature"),
    ) else {
        return Some(result);
    };
    if alg != ALGORITHM {
        return Some(result);
    }
    let Some(scope) = scope_of(&cred, access_key) else {
        return Some(result);
    };
    let signed = signed_headers(&signed);
    let q = canonical_query(&params, true);
    let before = signature(method, path, &q, &get, &signed, "UNSIGNED-PAYLOAD", &amz_date, &scope, secret);
    if !eq_const(&before, &sig) {
        return Some(result);
    }
    let after = signature(method, path, &q, &get_out, &signed, "UNSIGNED-PAYLOAD", &amz_date, &scope, secret);
    // Replace the value of X-Amz-Signature in the query as it came (hex, so
    // never encoded); everything else stays byte for byte.
    let new_query: Vec<String> = query
        .split('&')
        .map(|part| match part.split_once('=') {
            Some((k, _)) if form_decode(k) == "X-Amz-Signature" => format!("{k}={after}"),
            _ => part.to_string(),
        })
        .collect();
    result.target = format!("{path}?{}", new_query.join("&"));
    result.resigned = true;
    Some(result)
}

fn auth_fields(rest: &str) -> std::collections::HashMap<String, String> {
    rest.split(',')
        .filter_map(|c| c.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// `date/region/service` of a credential for our access key.
fn scope_of(credential: &str, access_key: &str) -> Option<String> {
    let parts: Vec<&str> = credential.split('/').collect();
    if parts.len() < 5 {
        return None;
    }
    let n = parts.len();
    if parts[..n - 4].join("/") != access_key || parts[n - 2] != "s3" || parts[n - 1] != "aws4_request" {
        return None;
    }
    Some(format!("{}/{}/{}/aws4_request", parts[n - 4], parts[n - 3], parts[n - 2]))
}

fn signed_headers(field: &str) -> Vec<String> {
    field.split(';').map(|h| h.to_ascii_lowercase()).collect()
}

#[allow(clippy::too_many_arguments)]
fn signature(
    method: &str,
    path: &str,
    canonical_query: &str,
    get: &dyn Fn(&str) -> Option<String>,
    signed: &[String],
    payload: &str,
    amz_date: &str,
    scope: &str,
    secret: &str,
) -> String {
    let mut headers = String::new();
    for name in signed {
        let v = get(name).unwrap_or_default();
        headers.push_str(&format!("{name}:{}\n", collapse_spaces(v.trim())));
    }
    let canonical = [method, path, canonical_query, &headers, &signed.join(";"), payload].join("\n");
    let to_sign = [ALGORITHM, amz_date, scope, &hex(&Sha256::digest(canonical.as_bytes()))].join("\n");
    let mut parts = scope.split('/');
    let (date, region, service) = (
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
    );
    let mut key = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    key = hmac(&key, region.as_bytes());
    key = hmac(&key, service.as_bytes());
    key = hmac(&key, b"aws4_request");
    hex(&hmac(&key, to_sign.as_bytes()))
}

/// Runs of spaces become one (JavaScript's `/ +/g`; tabs stay).
fn collapse_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev = false;
    for c in s.chars() {
        if c == ' ' {
            if !prev {
                out.push(c);
            }
            prev = true;
        } else {
            out.push(c);
            prev = false;
        }
    }
    out
}

/// The query read like URLSearchParams (`+` is a space, percent-decoded as
/// UTF-8).
fn form_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (form_decode(k), form_decode(v))
        })
        .collect()
}

fn form_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 3 <= b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Encoded and sorted like the simulator (encodeURIComponent, then `!'()*`).
fn canonical_query(params: &[(String, String)], presigned: bool) -> String {
    let mut p: Vec<(String, String)> = params
        .iter()
        .filter(|(k, _)| !(presigned && k == "X-Amz-Signature"))
        .map(|(k, v)| (crate::data::enc(k), crate::data::enc(v)))
        .collect();
    p.sort();
    p.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut k = if key.len() > 64 { Sha256::digest(key).to_vec() } else { key.to_vec() };
    k.resize(64, 0);
    let mut inner = Sha256::new();
    inner.update(k.iter().map(|b| b ^ 0x36).collect::<Vec<u8>>());
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(k.iter().map(|b| b ^ 0x5c).collect::<Vec<u8>>());
    outer.update(inner.finalize());
    outer.finalize().to_vec()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn eq_const(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "wrangler-compose-local";
    const SECRET: &str = "wrangler-compose-local-secret";

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// A signed URL made like a client does, for a host.
    fn presigned(host: &str, path: &str) -> String {
        let amz = "20261012T080000Z";
        let cred = format!("{KEY}/20261012/auto/s3/aws4_request");
        let params = vec![
            ("X-Amz-Algorithm".to_string(), ALGORITHM.to_string()),
            ("X-Amz-Credential".to_string(), cred.clone()),
            ("X-Amz-Date".to_string(), amz.to_string()),
            ("X-Amz-Expires".to_string(), "600".to_string()),
            ("X-Amz-SignedHeaders".to_string(), "host".to_string()),
            ("response-cache-control".to_string(), "no-store".to_string()),
        ];
        let q = canonical_query(&params, true);
        let host = host.to_string();
        let get = move |n: &str| (n == "host").then(|| host.clone());
        let sig = signature("GET", path, &q, &get, &["host".into()], "UNSIGNED-PAYLOAD", amz, "20261012/auto/s3/aws4_request", SECRET);
        format!("{path}?{q}&X-Amz-Signature={sig}")
    }

    #[test]
    fn hmac_matches_rfc4231() {
        // RFC 4231 test case 2
        assert_eq!(
            hex(&hmac(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn local_requests_pass_as_they_are() {
        let t = presigned("localhost:9050", "/cdn-cgi/local/r2/s3/storage/a");
        assert_eq!(rewrite("GET", &t, &h(&[("Host", "localhost:9050")]), KEY, SECRET), None);
        assert_eq!(rewrite("GET", &t, &h(&[("Host", "127.0.0.1:9050"), ("Origin", "http://localhost:8787")]), KEY, SECRET), None);
        assert_eq!(rewrite("GET", &t, &h(&[("Host", "[::1]:9050")]), KEY, SECRET), None);
    }

    #[test]
    fn a_signed_url_for_a_lan_address_is_signed_again_for_localhost() {
        let t = presigned("10.10.0.12:9050", "/cdn-cgi/local/r2/s3/storage/a%20b/c");
        let r = rewrite("GET", &t, &h(&[("Host", "10.10.0.12:9050"), ("Range", "bytes=0-9")]), KEY, SECRET).unwrap();
        assert!(r.resigned);
        assert_eq!(r.headers, h(&[("Host", "localhost:9050"), ("Range", "bytes=0-9")]));
        // the same URL as one made for localhost
        assert_eq!(r.target, presigned("localhost:9050", "/cdn-cgi/local/r2/s3/storage/a%20b/c"));
    }

    #[test]
    fn a_wrong_signature_is_not_signed_again() {
        let t = presigned("10.10.0.12:9050", "/cdn-cgi/local/r2/s3/storage/a");
        let bad = format!("{}0", &t[..t.len() - 1]);
        let r = rewrite("GET", &bad, &h(&[("Host", "10.10.0.12:9050")]), KEY, SECRET).unwrap();
        assert!(!r.resigned);
        assert_eq!(r.target, bad);
        assert_eq!(r.headers, h(&[("Host", "localhost:9050")]));
        // signed for another host
        let other = presigned("10.10.0.13:9050", "/cdn-cgi/local/r2/s3/storage/a");
        assert!(!rewrite("GET", &other, &h(&[("Host", "10.10.0.12:9050")]), KEY, SECRET).unwrap().resigned);
        // another access key
        let r = rewrite("GET", &t, &h(&[("Host", "10.10.0.12:9050")]), "someone-else", SECRET).unwrap();
        assert!(!r.resigned);
    }

    #[test]
    fn a_lan_origin_is_removed() {
        let t = presigned("localhost:9050", "/cdn-cgi/local/r2/s3/storage/a");
        let r = rewrite("GET", &t, &h(&[("Host", "localhost:9050"), ("Origin", "http://10.10.0.12:8787")]), KEY, SECRET).unwrap();
        assert!(r.resigned);
        assert_eq!(r.target, t);
        assert_eq!(r.headers, h(&[("Host", "localhost:9050")]));
        let r = rewrite("GET", &t, &h(&[("Host", "localhost:9050"), ("Origin", "null")]), KEY, SECRET).unwrap();
        assert_eq!(r.headers, h(&[("Host", "localhost:9050")]));
    }

    #[test]
    fn a_header_signed_request_is_signed_again() {
        let amz = "20261012T080000Z";
        let scope = "20261012/auto/s3/aws4_request";
        let path = "/cdn-cgi/local/r2/s3/storage/k";
        let q = canonical_query(&form_pairs("uploadId=x%2By&partNumber=1"), false);
        let signed: Vec<String> = vec!["host".into(), "x-amz-content-sha256".into(), "x-amz-date".into()];
        let payload = "UNSIGNED-PAYLOAD";
        let sign_for = |host: &str| {
            let host = host.to_string();
            let get = move |n: &str| match n {
                "host" => Some(host.clone()),
                "x-amz-content-sha256" => Some(payload.to_string()),
                "x-amz-date" => Some(amz.to_string()),
                _ => None,
            };
            signature("PUT", path, &q, &get, &signed, payload, amz, scope, SECRET)
        };
        let auth = |sig: &str| format!("{ALGORITHM} Credential={KEY}/{scope}, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={sig}");
        let target = format!("{path}?uploadId=x%2By&partNumber=1");
        let headers = h(&[
            ("Host", "10.10.0.12:9050"),
            ("x-amz-content-sha256", payload),
            ("x-amz-date", amz),
            ("Authorization", &auth(&sign_for("10.10.0.12:9050"))),
        ]);
        let r = rewrite("PUT", &target, &headers, KEY, SECRET).unwrap();
        assert!(r.resigned);
        assert_eq!(r.target, target);
        let got = r.headers.iter().find(|(k, _)| k == "Authorization").unwrap().1.clone();
        assert_eq!(got, auth(&sign_for("localhost:9050")));
    }

    #[test]
    fn decodes_and_encodes_the_query_like_the_simulator() {
        assert_eq!(form_decode("a+b%2Fc%E3%81%82%"), "a b/cあ%");
        assert_eq!(
            canonical_query(&form_pairs("b=2&a=x*y&a=('!')"), false),
            "a=%28%27%21%27%29&a=x%2Ay&b=2"
        );
        assert_eq!(collapse_spaces("a   b\t c"), "a b\t c");
    }
}
