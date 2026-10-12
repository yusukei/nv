//! The local R2 port of an r2 service (REQ-009): the S3-compatible API with
//! the bucket's own CORS rules (the local simulator allows every origin,
//! which would hide CORS mistakes until production), and public URLs at
//! `http://<bucket>.localhost:<port>/<key>` for `public: true` buckets.
//!
//! Every request gets `Connection: close`, so each connection carries one
//! request and only the request and response heads need to be read.

use crate::compose::Cors;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const S3_PREFIX: &str = "/cdn-cgi/local/r2/s3/";

#[derive(Clone, Debug)]
pub struct Site {
    /// The bucket's local name (its resource key).
    pub key: String,
    pub public: bool,
    pub cors: Vec<Cors>,
}

pub struct Shared {
    pub sites: Vec<Site>,
    /// The gateway's internal port: the simulator's S3 API and the data service.
    pub gateway: SocketAddr,
    pub token: String,
    /// The project's local S3 keys (REQ-009).
    pub s3: crate::compose::S3Keys,
}

/// Accept connections until `stop` is set, then drop the listener so the
/// port is free again (a restart binds it anew).
pub fn accept_until<F>(listener: TcpListener, stop: Arc<AtomicBool>, on_conn: F)
where
    F: Fn(TcpStream) + Send + 'static,
{
    if listener.set_nonblocking(true).is_err() {
        return;
    }
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((conn, _)) => {
                let _ = conn.set_nonblocking(false);
                on_conn(conn);
            }
            // Nothing waiting, or an error such as too many open files:
            // wait a little instead of spinning.
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

pub fn serve(listener: TcpListener, shared: Arc<Shared>, stop: Arc<AtomicBool>) {
    accept_until(listener, stop, move |client| {
        let shared = shared.clone();
        std::thread::spawn(move || {
            let _ = client.set_read_timeout(Some(Duration::from_secs(300)));
            let _ = handle(client, &shared);
        });
    });
}

/// A public bucket's own port (`public_port`, REQ-009): every path is a key
/// of that one bucket, whatever the Host, so other machines on the LAN
/// reach it by the PC's address.
pub fn serve_public(listener: TcpListener, shared: Arc<Shared>, bucket: String, stop: Arc<AtomicBool>) {
    accept_until(listener, stop, move |client| {
        let shared = shared.clone();
        let bucket = bucket.clone();
        std::thread::spawn(move || {
            let _ = client.set_read_timeout(Some(Duration::from_secs(300)));
            let _ = handle_public(client, &shared, &bucket);
        });
    });
}

fn handle_public(mut client: TcpStream, shared: &Shared, bucket: &str) -> std::io::Result<()> {
    let Some((buf, len)) = read_head(&mut client)? else {
        return respond(&mut client, 400, &[], b"bad request");
    };
    let Some(head) = parse(&buf, len, true) else {
        return respond(&mut client, 400, &[], b"bad request");
    };
    let Some(site) = shared.sites.iter().find(|s| s.key == bucket && s.public) else {
        return respond(&mut client, 404, &[], b"not found");
    };
    let origin = head.get("origin").map(str::to_string);
    if head.method == "OPTIONS" && head.get("access-control-request-method").is_some() {
        return preflight(&mut client, site, &head);
    }
    public_get(client, shared, site, &head, origin.as_deref())
}

/// A Worker's port: everything goes to the gateway as it came, except the
/// simulator's S3 API for buckets whose r2 service has its own port. Those
/// answer only there, with the bucket's CORS rules, as in production where
/// a Worker's address has no S3 API at all. Each connection carries one
/// request (`Connection: close`) so every request is seen; upgrades
/// (WebSocket) pass through untouched.
pub fn forward_worker(
    listener: TcpListener,
    gateway: SocketAddr,
    guarded: Arc<Vec<String>>,
    stop: Arc<AtomicBool>,
) {
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    accept_until(listener, stop, move |client| {
        let guarded = guarded.clone();
        std::thread::spawn(move || {
            let _ = forward_one(client, gateway, port, &guarded);
        });
    });
}

/// The header that tells the gateway which port a request came in on (the
/// Host header may name another port behind a proxy, or none for port 80).
/// The gateway removes it before the Worker sees the request.
pub const PORT_HEADER: &str = "x-wrangler-compose-port";
/// Marks a request to run the Worker's cron (`/cdn-cgi/handler/scheduled`).
pub const SCHEDULED_HEADER: &str = "x-wrangler-compose-scheduled";

fn forward_one(
    mut client: TcpStream,
    gateway: SocketAddr,
    port: u16,
    guarded: &[String],
) -> std::io::Result<()> {
    // A connection that never sends a request does not hold a thread for
    // ever; once the head is in, the connection may stay as long as it likes
    // (WebSocket).
    let _ = client.set_read_timeout(Some(Duration::from_secs(300)));
    let Some((buf, len)) = read_head(&mut client)? else {
        return Ok(());
    };
    let _ = client.set_read_timeout(None);
    let Some(head) = parse(&buf, len, true) else {
        return respond(&mut client, 400, &[], b"bad request");
    };
    if let Some(rest) = head.path.strip_prefix(S3_PREFIX) {
        let bucket = rest.split(['/', '?']).next().unwrap_or_default();
        if guarded.iter().any(|g| g == bucket) {
            return respond(
                &mut client,
                404,
                &[],
                b"wrangler-compose: this bucket's S3-compatible API is on its r2 service's port",
            );
        }
    }
    let mut up = TcpStream::connect(gateway)?;
    let upgrade = head.get("upgrade").is_some();
    // Wrangler answers this path itself, for the entry Worker (the gateway);
    // marked like this, the gateway runs the cron of this port's Worker.
    let (path, scheduled) = match head.path.strip_prefix("/cdn-cgi/handler/scheduled") {
        Some(rest) if rest.is_empty() || rest.starts_with('?') => (format!("/{rest}"), true),
        _ => (head.path.clone(), false),
    };
    let mut out = format!("{} {path} HTTP/1.1\r\n", head.method);
    for (k, v) in &head.headers {
        let drop = k.eq_ignore_ascii_case(PORT_HEADER)
            || k.eq_ignore_ascii_case(SCHEDULED_HEADER)
            || (!upgrade
                && (k.eq_ignore_ascii_case("connection") || k.eq_ignore_ascii_case("keep-alive")));
        if !drop {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    out.push_str(&format!("{PORT_HEADER}: {port}\r\n"));
    if scheduled {
        out.push_str(&format!("{SCHEDULED_HEADER}: 1\r\n"));
    }
    if !upgrade {
        out.push_str("connection: close\r\n");
    }
    out.push_str("\r\n");
    up.write_all(out.as_bytes())?;
    up.write_all(&buf[head.raw_len..])?;
    let (mut c_read, mut u_write) = (client.try_clone()?, up.try_clone()?);
    let t = std::thread::spawn(move || {
        let _ = std::io::copy(&mut c_read, &mut u_write);
        let _ = u_write.shutdown(Shutdown::Write);
    });
    if !upgrade {
        // The client must not send a second request on this connection: it
        // would reach the gateway without PORT_HEADER (HTTP keep-alive, which
        // browsers use for every page). Wrangler's answer may keep the
        // connection open, so the answer to the client says close.
        if let Some((rbuf, rlen)) = read_head(&mut up)? {
            client.write_all(&close_head(&rbuf, rlen))?;
            let res = parse(&rbuf, rlen, false);
            // The answer ends at its length, not when Wrangler closes (it may not).
            let _ = copy_body(&mut up, &mut client, &rbuf[rlen..], &head.method, res.as_ref());
        }
        let _ = client.shutdown(Shutdown::Both);
        let _ = up.shutdown(Shutdown::Both);
        let _ = t.join();
        return Ok(());
    }
    let _ = std::io::copy(&mut up, &mut client);
    // The answer is complete: end both directions so the request side's
    // thread stops too.
    let _ = client.shutdown(Shutdown::Both);
    let _ = up.shutdown(Shutdown::Both);
    let _ = t.join();
    Ok(())
}

/// Copy an answer's body to the client and stop at its end: `Content-Length`
/// bytes, or the chunks up to the last one, or (neither) until the upstream
/// closes. HEAD answers and 1xx/204/304 have no body. `first` is what was read
/// with the head.
fn copy_body(
    up: &mut TcpStream,
    client: &mut TcpStream,
    first: &[u8],
    method: &str,
    res: Option<&Head>,
) -> std::io::Result<()> {
    let status: u16 = res.and_then(|h| h.path.parse().ok()).unwrap_or(200);
    if method.eq_ignore_ascii_case("HEAD") || status < 200 || status == 204 || status == 304 {
        return Ok(());
    }
    let chunked = res
        .and_then(|h| h.get("transfer-encoding"))
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let length: Option<u64> = res
        .and_then(|h| h.get("content-length"))
        .and_then(|v| v.parse().ok());
    if let (false, Some(n)) = (chunked, length) {
        let take = (n as usize).min(first.len());
        client.write_all(&first[..take])?;
        let rest = n - take as u64;
        std::io::copy(&mut Read::by_ref(up).take(rest), client)?;
        return Ok(());
    }
    if !chunked {
        client.write_all(first)?;
        std::io::copy(up, client)?;
        return Ok(());
    }
    // Chunked: pass the bytes through while following the chunk sizes.
    let mut buf = first.to_vec();
    let mut pos = 0usize;
    let mut chunk = [0u8; 16 * 1024];
    let mut fill = |buf: &mut Vec<u8>| -> std::io::Result<bool> {
        let k = up.read(&mut chunk)?;
        buf.extend_from_slice(&chunk[..k]);
        Ok(k > 0)
    };
    loop {
        // the size line
        let line_end = loop {
            if let Some(i) = find(&buf[pos..], b"\r\n") {
                break pos + i;
            }
            if !fill(&mut buf)? {
                client.write_all(&buf)?;
                return Ok(());
            }
        };
        let size_text = String::from_utf8_lossy(&buf[pos..line_end]).to_string();
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .unwrap_or(0);
        if size == 0 {
            // trailers end with an empty line
            loop {
                if let Some(i) = find(&buf[line_end..], b"\r\n\r\n") {
                    let end = line_end + i + 4;
                    client.write_all(&buf[..end])?;
                    return Ok(());
                }
                if !fill(&mut buf)? {
                    client.write_all(&buf)?;
                    return Ok(());
                }
            }
        }
        let next = line_end + 2 + size + 2;
        while buf.len() < next {
            if !fill(&mut buf)? {
                client.write_all(&buf)?;
                return Ok(());
            }
        }
        // send what is complete and keep the rest
        client.write_all(&buf[..next])?;
        buf.drain(..next);
        pos = 0;
    }
}

/// The head of an answer, with `Connection: close` in place of whatever it
/// said about the connection (one request per connection).
fn close_head(buf: &[u8], len: usize) -> Vec<u8> {
    let text = String::from_utf8_lossy(&buf[..len]);
    let mut out = String::new();
    for (i, line) in text.split("\r\n").enumerate() {
        if line.is_empty() {
            continue;
        }
        let name = line.split(':').next().unwrap_or("").trim();
        if i > 0
            && (name.eq_ignore_ascii_case("connection") || name.eq_ignore_ascii_case("keep-alive"))
        {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("connection: close\r\n\r\n");
    out.into_bytes()
}

struct Head {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    raw_len: usize,
}

impl Head {
    fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Read until the end of the head; returns it and every byte read.
fn read_head(stream: &mut TcpStream) -> std::io::Result<Option<(Vec<u8>, usize)>> {
    read_head_after(stream, Vec::new())
}

/// The same, starting with bytes already read.
fn read_head_after(
    stream: &mut TcpStream,
    mut buf: Vec<u8>,
) -> std::io::Result<Option<(Vec<u8>, usize)>> {
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            return Ok(Some((buf, i + 4)));
        }
        if buf.len() > 64 * 1024 {
            return Ok(None);
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn parse(buf: &[u8], len: usize, request: bool) -> Option<Head> {
    let text = std::str::from_utf8(&buf[..len]).ok()?;
    let mut lines = text.split("\r\n");
    let first = lines.next()?;
    let mut parts = first.splitn(3, ' ');
    let (a, b) = (parts.next()?.to_string(), parts.next()?.to_string());
    let headers = lines
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            l.split_once(':')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    // For a response, `method` holds the protocol and `path` the status.
    let _ = request;
    Some(Head {
        method: a,
        path: b,
        headers,
        raw_len: len,
    })
}

fn handle(mut client: TcpStream, shared: &Shared) -> std::io::Result<()> {
    let Some((buf, len)) = read_head(&mut client)? else {
        return respond(&mut client, 400, &[], b"bad request");
    };
    let Some(head) = parse(&buf, len, true) else {
        return respond(&mut client, 400, &[], b"bad request");
    };
    let origin = head.get("origin").map(str::to_string);
    if let Some(rest) = head.path.strip_prefix(S3_PREFIX) {
        let bucket = rest.split(['/', '?']).next().unwrap_or_default();
        let Some(site) = shared.sites.iter().find(|s| s.key == bucket) else {
            return respond(&mut client, 404, &[("content-type", "application/xml".into())], b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>NoSuchBucket</Code><Message>The specified bucket does not exist.</Message></Error>");
        };
        if head.method == "OPTIONS" && head.get("access-control-request-method").is_some() {
            return preflight(&mut client, site, &head);
        }
        // From another machine: the simulator takes only local names in
        // Host and Origin, so the request is checked and signed again for
        // localhost (DES-019).
        let rewritten = crate::s3sig::rewrite(
            &head.method,
            &head.path,
            &head.headers,
            &shared.s3.key,
            &shared.s3.secret,
        );
        let head = match rewritten {
            Some(r) => Head {
                method: head.method.clone(),
                path: r.target,
                headers: r.headers,
                raw_len: head.raw_len,
            },
            None => head,
        };
        return proxy_s3(client, shared, site, &head, &buf, origin.as_deref());
    }
    // Public URL: <bucket>.localhost
    let host = head.get("host").unwrap_or_default().to_ascii_lowercase();
    let host = host
        .rsplit_once(':')
        .map(|(h, _)| h.to_string())
        .unwrap_or(host);
    let Some(bucket) = host.strip_suffix(".localhost") else {
        let names: Vec<String> = shared
            .sites
            .iter()
            .filter(|s| s.public)
            .map(|s| format!("{}.localhost", s.key))
            .collect();
        let msg = format!(
            "wrangler-compose: use {S3_PREFIX}<bucket> for the S3-compatible API, or a public bucket's host ({})",
            if names.is_empty() {
                "none".into()
            } else {
                names.join(", ")
            }
        );
        return respond(&mut client, 404, &[], msg.as_bytes());
    };
    let Some(site) = shared.sites.iter().find(|s| s.key == bucket && s.public) else {
        return respond(
            &mut client,
            404,
            &[],
            b"wrangler-compose: no public bucket by that name",
        );
    };
    if head.method == "OPTIONS" && head.get("access-control-request-method").is_some() {
        return preflight(&mut client, site, &head);
    }
    public_get(client, shared, site, &head, origin.as_deref())
}

/// R2's CORS: a rule matches when the origin and method are listed (an
/// origin may contain one `*`).
fn rule_for<'a>(site: &'a Site, origin: &str, method: &str) -> Option<&'a Cors> {
    site.cors.iter().find(|r| {
        r.origins.iter().any(|o| origin_matches(o, origin))
            && r.methods.iter().any(|m| m.eq_ignore_ascii_case(method))
    })
}

fn origin_matches(pattern: &str, origin: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == origin,
        Some((a, b)) => {
            origin.len() >= a.len() + b.len() && origin.starts_with(a) && origin.ends_with(b)
        }
    }
}

fn preflight(client: &mut TcpStream, site: &Site, head: &Head) -> std::io::Result<()> {
    let origin = head.get("origin").unwrap_or_default();
    let method = head
        .get("access-control-request-method")
        .unwrap_or_default();
    let wanted: Vec<String> = head
        .get("access-control-request-headers")
        .unwrap_or_default()
        .split(',')
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .collect();
    let rule = rule_for(site, origin, method).filter(|r| {
        wanted.iter().all(|w| {
            r.allowed_headers
                .iter()
                .any(|a| a == "*" || a.eq_ignore_ascii_case(w))
        })
    });
    let Some(rule) = rule else {
        return respond(
            client,
            403,
            &[],
            b"CORS preflight did not match the bucket's cors rules",
        );
    };
    let mut h = vec![
        ("access-control-allow-origin", origin.to_string()),
        ("access-control-allow-methods", rule.methods.join(", ")),
        ("vary", "Origin".to_string()),
    ];
    if !wanted.is_empty() {
        h.push(("access-control-allow-headers", wanted.join(", ")));
    }
    if let Some(age) = rule.max_age {
        h.push(("access-control-max-age", age.to_string()));
    }
    respond(client, 204, &h, b"")
}

fn cors_headers(site: &Site, origin: Option<&str>, method: &str) -> Vec<(&'static str, String)> {
    let Some(origin) = origin else {
        return Vec::new();
    };
    let Some(rule) = rule_for(site, origin, method) else {
        return Vec::new();
    };
    let mut h = vec![
        ("access-control-allow-origin", origin.to_string()),
        ("vary", "Origin".to_string()),
    ];
    if !rule.expose_headers.is_empty() {
        h.push((
            "access-control-expose-headers",
            rule.expose_headers.join(", "),
        ));
    }
    h
}

fn proxy_s3(
    mut client: TcpStream,
    shared: &Shared,
    site: &Site,
    head: &Head,
    buf: &[u8],
    origin: Option<&str>,
) -> std::io::Result<()> {
    let mut up = TcpStream::connect(shared.gateway)?;
    // The request head goes as it came (the signature covers it), except
    // that the connection closes after one response.
    let mut out = format!("{} {} HTTP/1.1\r\n", head.method, head.path);
    for (k, v) in &head.headers {
        if !k.eq_ignore_ascii_case("connection") && !k.eq_ignore_ascii_case("keep-alive") {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    out.push_str("connection: close\r\n\r\n");
    up.write_all(out.as_bytes())?;
    up.write_all(&buf[head.raw_len..])?;
    let (mut c_read, mut u_write) = (client.try_clone()?, up.try_clone()?);
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut c_read, &mut u_write);
    });
    let Some((mut rbuf, mut rlen)) = read_head(&mut up)? else {
        return respond(&mut client, 502, &[], b"no answer from the local R2");
    };
    // Interim answers (100 Continue for `Expect: 100-continue`) go through
    // as they are; the final answer is the one that gets the CORS headers.
    while status_line(&rbuf).starts_with('1') {
        client.write_all(&rbuf[..rlen])?;
        let rest = rbuf[rlen..].to_vec();
        let Some((b, l)) = read_head_after(&mut up, rest)? else {
            return respond(&mut client, 502, &[], b"no answer from the local R2");
        };
        (rbuf, rlen) = (b, l);
    }
    let Some(rhead) = parse(&rbuf, rlen, false) else {
        return respond(&mut client, 502, &[], b"bad answer from the local R2");
    };
    let mut out = format!("HTTP/1.1 {}\r\n", status_line(&rbuf));
    for (k, v) in &rhead.headers {
        let lk = k.to_ascii_lowercase();
        if lk.starts_with("access-control-")
            || lk == "connection"
            || lk == "keep-alive"
            || lk == "vary"
        {
            continue;
        }
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    for (k, v) in cors_headers(site, origin, &head.method) {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("connection: close\r\n\r\n");
    client.write_all(out.as_bytes())?;
    client.write_all(&rbuf[rlen..])?;
    std::io::copy(&mut up, &mut client)?;
    let _ = client.shutdown(Shutdown::Both);
    let _ = up.shutdown(Shutdown::Both);
    Ok(())
}

fn status_line(buf: &[u8]) -> String {
    let end = find(buf, b"\r\n").unwrap_or(0);
    let line = String::from_utf8_lossy(&buf[..end]);
    line.split_once(' ')
        .map(|(_, s)| s.to_string())
        .unwrap_or_else(|| "502 Bad Gateway".into())
}

/// Serve an object of a public bucket from the data service.
fn public_get(
    mut client: TcpStream,
    shared: &Shared,
    site: &Site,
    head: &Head,
    origin: Option<&str>,
) -> std::io::Result<()> {
    if head.method != "GET" && head.method != "HEAD" {
        return respond(&mut client, 405, &[("allow", "GET, HEAD".into())], b"");
    }
    // The key as R2 sees it: the path without its leading `/`, decoded. It
    // goes to the data service as a query value, so `..` stays part of the
    // key instead of stepping out of the bucket.
    let path = head.path.split('?').next().unwrap_or("/");
    let key = String::from_utf8_lossy(&crate::data::dec(path.trim_start_matches('/'))).into_owned();
    if key.is_empty() {
        return respond(&mut client, 404, &[], b"not found");
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .proxy(None)
        .build()
        .into();
    let url = format!(
        "http://{}/__wc/r2/{}/public?key={}",
        shared.gateway,
        site.key,
        crate::data::enc(&key)
    );
    // Ranges and conditions are answered by the data service, with R2's
    // own ETag and upload time (HTTP's rules, RFC 9110).
    let mut req = if head.method == "HEAD" {
        agent.head(&url)
    } else {
        agent.get(&url)
    }
    .header("authorization", format!("Bearer {}", shared.token));
    for name in [
        "range",
        "if-range",
        "if-match",
        "if-none-match",
        "if-modified-since",
        "if-unmodified-since",
    ] {
        if let Some(v) = head.get(name) {
            req = req.header(name, v);
        }
    }
    let res = match req.call() {
        Ok(r) => r,
        Err(_) => return respond(&mut client, 502, &[], b"no answer from the data service"),
    };
    let status = res.status().as_u16();
    if status == 404 {
        return respond(
            &mut client,
            404,
            &cors_headers(site, origin, &head.method),
            b"not found",
        );
    }
    let meta = crate::data::meta_of(res.headers()).unwrap_or_default();
    let mut h = cors_headers(site, origin, &head.method);
    for (field, name) in [
        ("contentType", "content-type"),
        ("cacheControl", "cache-control"),
        ("contentDisposition", "content-disposition"),
        ("contentEncoding", "content-encoding"),
        ("contentLanguage", "content-language"),
    ] {
        // Stored values come back as headers: never let one break the head.
        if let Some(v) = meta["http"][field].as_str()
            && !v.contains(['\r', '\n'])
        {
            h.push((name, v.to_string()));
        }
    }
    for name in [
        "etag",
        "last-modified",
        "accept-ranges",
        "content-range",
        "content-length",
    ] {
        if let Some(v) = res.headers().get(name).and_then(|v| v.to_str().ok())
            && !v.contains(['\r', '\n'])
        {
            h.push((name, v.to_string()));
        }
    }
    let mut body = Vec::new();
    if head.method != "HEAD" && status != 304 && status != 412 && status != 416 {
        res.into_body().into_reader().read_to_end(&mut body)?;
    }
    respond_raw(&mut client, status, &h, &body)
}

fn respond(
    client: &mut TcpStream,
    status: u16,
    headers: &[(&str, String)],
    body: &[u8],
) -> std::io::Result<()> {
    let mut h: Vec<(&str, String)> = headers.to_vec();
    h.push(("content-length", body.len().to_string()));
    respond_raw(client, status, &h, body)
}

fn respond_raw(
    client: &mut TcpStream,
    status: u16,
    headers: &[(&str, String)],
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        206 => "Partial Content",
        304 => "Not Modified",
        412 => "Precondition Failed",
        416 => "Range Not Satisfiable",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Bad Gateway",
    };
    let mut out = format!("HTTP/1.1 {status} {reason}\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("connection: close\r\n\r\n");
    client.write_all(out.as_bytes())?;
    client.write_all(body)?;
    let _ = client.shutdown(Shutdown::Both);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site() -> Site {
        Site {
            key: "storage-files".into(),
            public: true,
            cors: vec![Cors {
                origins: vec![
                    "http://localhost:8787".into(),
                    "https://*.example.com".into(),
                ],
                methods: vec!["GET".into(), "PUT".into()],
                allowed_headers: vec!["content-type".into()],
                expose_headers: vec!["etag".into()],
                max_age: Some(600),
            }],
        }
    }

    #[test]
    fn matches_rules_like_r2() {
        let s = site();
        assert!(rule_for(&s, "http://localhost:8787", "PUT").is_some());
        assert!(rule_for(&s, "https://app.example.com", "get").is_some());
        assert!(rule_for(&s, "http://localhost:3000", "PUT").is_none());
        assert!(rule_for(&s, "http://localhost:8787", "DELETE").is_none());
        let h = cors_headers(&s, Some("http://localhost:8787"), "GET");
        assert!(
            h.iter()
                .any(|(k, v)| *k == "access-control-allow-origin" && v == "http://localhost:8787")
        );
        assert!(cors_headers(&s, Some("http://evil.test"), "GET").is_empty());
    }

    /// A gateway like Wrangler's: answers every request on a connection and
    /// keeps it open; 200 when the request carries PORT_HEADER, else 404.
    fn keep_alive_gateway() -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut c = c;
                    let mut buf = Vec::new();
                    while let Ok(Some((b, len))) = read_head_after(&mut c, std::mem::take(&mut buf)) {
                        let head = parse(&b, len, true).unwrap();
                        let ok = head.get(PORT_HEADER).is_some();
                        let body: &[u8] = if ok { b"ok" } else { b"no port" };
                        let status = if ok { "200 OK" } else { "404 Not Found" };
                        let res = format!("HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n", body.len());
                        if c.write_all(res.as_bytes()).and_then(|_| c.write_all(body)).is_err() {
                            return;
                        }
                        buf = b[len..].to_vec();
                    }
                });
            }
        });
        addr
    }

    fn get(c: &mut TcpStream) -> std::io::Result<Option<(String, String)>> {
        c.write_all(b"GET / HTTP/1.1\r\nhost: localhost\r\nconnection: keep-alive\r\n\r\n")?;
        let Some((b, len)) = read_head(c)? else {
            return Ok(None);
        };
        let head = parse(&b, len, false).unwrap();
        let n: usize = head.get("content-length").unwrap().parse().unwrap();
        let mut body = b[len..].to_vec();
        while body.len() < n {
            let mut chunk = [0u8; 64];
            let k = c.read(&mut chunk)?;
            if k == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..k]);
        }
        let conn = head.get("connection").unwrap_or("").to_string();
        Ok(Some((format!("{} {}", head.path, String::from_utf8_lossy(&body)), conn)))
    }

    #[test]
    fn a_kept_alive_connection_carries_one_request() {
        let gateway = keep_alive_gateway();
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        // forward_worker accepts in the calling thread until `stop`
        let s = stop.clone();
        std::thread::spawn(move || forward_worker(l, gateway, Arc::new(Vec::new()), s));

        let mut c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (first, conn) = get(&mut c).unwrap().unwrap();
        assert_eq!(first, "200 ok");
        // The client is told to close, so a browser does not send a second
        // request here (it would reach the gateway without PORT_HEADER).
        assert_eq!(conn, "close");
        // And the connection is closed: a second request gets no answer.
        assert!(matches!(get(&mut c), Ok(None) | Err(_)));
        // A new connection works again.
        let mut c2 = TcpStream::connect(addr).unwrap();
        c2.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        assert_eq!(get(&mut c2).unwrap().unwrap().0, "200 ok");
        stop.store(true, Ordering::SeqCst);
    }
}
