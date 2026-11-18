//! A minimal HTTP/1.1 server and client, std only.
//!
//! Enough for this app and nothing more: one thread per connection,
//! keep-alive on the server side, `Content-Length` or chunked bodies in,
//! `Content-Length` out, and a client that always sends `Connection: close`.
//! Every role in the image speaks HTTP to its neighbours through this module,
//! so the edge, the web tier, the API and the store share one implementation.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

/// The largest request or response head accepted (bytes).
pub const MAX_HEAD: usize = 16 * 1024;
/// The largest body accepted (bytes).
pub const MAX_BODY: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub peer: Option<SocketAddr>,
}

impl Request {
    pub fn new(method: &str, target: &str) -> Self {
        Request {
            method: method.to_string(),
            target: target.to_string(),
            version: "HTTP/1.1".to_string(),
            headers: Vec::new(),
            body: Vec::new(),
            peer: None,
        }
    }

    /// The path without the query string.
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("/")
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        header_of(&self.headers, name)
    }

    pub fn set_header(&mut self, name: &str, value: &str) {
        set_header_in(&mut self.headers, name, value);
    }

    /// Whether the client asked to close the connection after this request.
    fn wants_close(&self) -> bool {
        match self.header("connection") {
            Some(v) if v.eq_ignore_ascii_case("close") => true,
            Some(v) if v.eq_ignore_ascii_case("keep-alive") => false,
            _ => self.version == "HTTP/1.0",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        Response { status, headers: vec![("Content-Type".to_string(), content_type.to_string())], body }
    }

    pub fn text(status: u16, body: &str) -> Self {
        Response::new(status, "text/plain; charset=utf-8", body.as_bytes().to_vec())
    }

    /// JSON, pretty-printed as the original Python API did (`"status": "ok"`).
    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        let mut body = serde_json::to_vec_pretty(value).unwrap_or_default();
        body.push(b'\n');
        Response::new(status, "application/json", body)
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        header_of(&self.headers, name)
    }

    pub fn body_str(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub fn header_of<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

pub fn set_header_in(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    headers.push((name.to_string(), value.to_string()));
}

/// Headers that describe one hop and are never forwarded by a proxy.
pub fn is_hop_by_hop(name: &str) -> bool {
    const HOP: [&str; 9] = [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ];
    HOP.iter().any(|h| name.eq_ignore_ascii_case(h)) || name.eq_ignore_ascii_case("content-length")
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

/// `:8080` → `0.0.0.0:8080`; anything with a host is kept.
pub fn listen_addr(spec: &str) -> String {
    if let Some(port) = spec.strip_prefix(':') {
        format!("0.0.0.0:{port}")
    } else {
        spec.to_string()
    }
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Reads one CRLF (or LF) terminated line, counting it against `budget`.
fn read_line<R: BufRead>(r: &mut R, budget: &mut usize) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    let n = r.by_ref().take(*budget as u64 + 1).read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if n > *budget {
        return Err(bad("head too large"));
    }
    *budget -= n;
    while buf.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        buf.pop();
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

fn read_headers<R: BufRead>(r: &mut R, budget: &mut usize) -> io::Result<Vec<(String, String)>> {
    let mut headers = Vec::new();
    loop {
        let line = read_line(r, budget)?.ok_or_else(|| bad("eof in headers"))?;
        if line.is_empty() {
            return Ok(headers);
        }
        let (k, v) = line.split_once(':').ok_or_else(|| bad(format!("bad header line {line:?}")))?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
}

fn read_chunked<R: BufRead>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut budget = MAX_HEAD;
        let line = read_line(r, &mut budget)?.ok_or_else(|| bad("eof in chunk size"))?;
        let size_hex = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| bad("bad chunk size"))?;
        if size == 0 {
            // Trailers, then the empty line.
            loop {
                let mut budget = MAX_HEAD;
                match read_line(r, &mut budget)? {
                    Some(l) if !l.is_empty() => continue,
                    _ => return Ok(body),
                }
            }
        }
        if body.len() + size > MAX_BODY {
            return Err(bad("body too large"));
        }
        let start = body.len();
        body.resize(start + size, 0);
        r.read_exact(&mut body[start..])?;
        let mut crlf = [0u8; 2];
        r.read_exact(&mut crlf)?;
    }
}

fn read_body<R: BufRead>(r: &mut R, headers: &[(String, String)]) -> io::Result<Vec<u8>> {
    if header_of(headers, "transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        return read_chunked(r);
    }
    match header_of(headers, "content-length") {
        Some(len) => {
            let len: usize = len.parse().map_err(|_| bad("bad content-length"))?;
            if len > MAX_BODY {
                return Err(bad("body too large"));
            }
            let mut body = vec![0u8; len];
            r.read_exact(&mut body)?;
            Ok(body)
        }
        None => Ok(Vec::new()),
    }
}

/// Reads one request; `Ok(None)` on a clean end of stream between requests.
pub fn read_request<R: BufRead>(r: &mut R) -> io::Result<Option<Request>> {
    let mut budget = MAX_HEAD;
    let line = loop {
        match read_line(r, &mut budget)? {
            None => return Ok(None),
            Some(l) if l.is_empty() => continue, // stray CRLF between requests
            Some(l) => break l,
        }
    };
    let mut parts = line.split_whitespace();
    let (method, target, version) = match (parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(t), Some(v)) => (m.to_string(), t.to_string(), v.to_string()),
        _ => return Err(bad(format!("bad request line {line:?}"))),
    };
    let headers = read_headers(r, &mut budget)?;
    let body = read_body(r, &headers)?;
    Ok(Some(Request { method, target, version, headers, body, peer: None }))
}

pub fn write_response<W: Write>(w: &mut W, resp: &Response, head_only: bool, close: bool) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {} {}\r\n", resp.status, reason(resp.status));
    for (k, v) in &resp.headers {
        if k.eq_ignore_ascii_case("content-length") || k.eq_ignore_ascii_case("connection") {
            continue;
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
    head.push_str(if close { "Connection: close\r\n\r\n" } else { "Connection: keep-alive\r\n\r\n" });
    w.write_all(head.as_bytes())?;
    if !head_only {
        w.write_all(&resp.body)?;
    }
    w.flush()
}

/// Serves `handler` on `addr` forever, one thread per connection.
pub fn serve<F>(addr: &str, handler: F) -> io::Result<()>
where
    F: Fn(Request) -> Response + Send + Sync + 'static,
{
    let listener = TcpListener::bind(listen_addr(addr))?;
    serve_on(listener, handler)
}

/// As [`serve`], on a listener the caller already bound.
pub fn serve_on<F>(listener: TcpListener, handler: F) -> io::Result<()>
where
    F: Fn(Request) -> Response + Send + Sync + 'static,
{
    let handler = Arc::new(handler);
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[http] accept: {e}");
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        let handler = Arc::clone(&handler);
        std::thread::spawn(move || {
            let _ = handle_connection(stream, handler.as_ref());
        });
    }
    Ok(())
}

/// Spawns [`serve`] on a thread; the bind happens before this returns, so a
/// port already in use is an error here rather than a silent dead thread.
pub fn spawn_server<F>(addr: &str, handler: F) -> io::Result<()>
where
    F: Fn(Request) -> Response + Send + Sync + 'static,
{
    let listener = TcpListener::bind(listen_addr(addr))?;
    std::thread::spawn(move || {
        if let Err(e) = serve_on(listener, handler) {
            eprintln!("[http] server stopped: {e}");
        }
    });
    Ok(())
}

fn handle_connection<F>(stream: TcpStream, handler: &F) -> io::Result<()>
where
    F: Fn(Request) -> Response,
{
    let peer = stream.peer_addr().ok();
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    let _ = stream.set_nodelay(true);
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    loop {
        let mut req = match read_request(&mut reader) {
            Ok(Some(r)) => r,
            Ok(None) => break,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                let resp = if e.to_string().contains("too large") {
                    Response::text(431, "request too large\n")
                } else {
                    Response::text(400, "bad request\n")
                };
                let _ = write_response(&mut writer, &resp, false, true);
                break;
            }
            Err(_) => break,
        };
        req.peer = peer;
        let close = req.wants_close();
        let head_only = req.method == "HEAD";
        let resp = handler(req);
        write_response(&mut writer, &resp, head_only, close)?;
        if close {
            break;
        }
    }
    let _ = writer.shutdown(Shutdown::Both);
    Ok(())
}

/// Sends `req` to `addr` and reads the whole response. Always one request per
/// connection (`Connection: close`).
pub fn send(addr: SocketAddr, host: &str, req: &Request, timeout: Duration) -> io::Result<Response> {
    let stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let _ = stream.set_nodelay(true);
    let mut head = format!("{} {} HTTP/1.1\r\n", req.method, req.target);
    let mut has_host = false;
    for (k, v) in &req.headers {
        if is_hop_by_hop(k) {
            continue;
        }
        if k.eq_ignore_ascii_case("host") {
            has_host = true;
        }
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    if !has_host {
        head.push_str(&format!("Host: {host}\r\n"));
    }
    if !req.body.is_empty() || matches!(req.method.as_str(), "POST" | "PUT" | "PATCH") {
        head.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
    }
    head.push_str("Connection: close\r\n\r\n");
    let mut w = stream.try_clone()?;
    w.write_all(head.as_bytes())?;
    w.write_all(&req.body)?;
    w.flush()?;
    let mut r = BufReader::new(stream);
    read_response(&mut r, req.method == "HEAD")
}

pub fn read_response<R: BufRead>(r: &mut R, head_only: bool) -> io::Result<Response> {
    let mut budget = MAX_HEAD;
    let line = read_line(r, &mut budget)?.ok_or_else(|| bad("empty response"))?;
    let mut parts = line.splitn(3, ' ');
    let _version = parts.next();
    let status: u16 = parts.next().and_then(|s| s.parse().ok()).ok_or_else(|| bad(format!("bad status line {line:?}")))?;
    let headers = read_headers(r, &mut budget)?;
    let body = if head_only || status == 204 || status == 304 || (100..200).contains(&status) {
        Vec::new()
    } else if header_of(&headers, "transfer-encoding").is_some() || header_of(&headers, "content-length").is_some() {
        read_body(r, &headers)?
    } else {
        let mut body = Vec::new();
        r.take(MAX_BODY as u64).read_to_end(&mut body)?;
        body
    };
    Ok(Response { status, headers, body })
}

/// Splits `http://host[:port]/path` into (host, port, path).
pub fn parse_url(url: &str) -> io::Result<(String, u16, String)> {
    let rest = url.strip_prefix("http://").ok_or_else(|| bad(format!("only http:// URLs: {url}")))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => (h.to_string(), p.parse().map_err(|_| bad("bad port"))?),
        _ => (authority.to_string(), 80),
    };
    Ok((host, port, path.to_string()))
}

/// Resolves `host:port` to its first address.
pub fn resolve_one(host: &str, port: u16) -> io::Result<SocketAddr> {
    (host, port).to_socket_addrs()?.next().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{host} has no address")))
}

/// A request to a URL: resolves the host, sends, reads the response.
pub fn call(method: &str, url: &str, headers: &[(&str, &str)], body: &[u8], timeout: Duration) -> io::Result<Response> {
    let (host, port, path) = parse_url(url)?;
    let addr = resolve_one(&host, port)?;
    let mut req = Request::new(method, &path);
    for (k, v) in headers {
        req.headers.push((k.to_string(), v.to_string()));
    }
    req.body = body.to_vec();
    let host_header = if port == 80 { host.clone() } else { format!("{host}:{port}") };
    send(addr, &host_header, &req, timeout)
}

pub fn get(url: &str, timeout: Duration) -> io::Result<Response> {
    call("GET", url, &[], &[], timeout)
}

/// Whether a TCP connection to `host:port` opens within `timeout`.
pub fn tcp_reachable(host: &str, port: u16, timeout: Duration) -> bool {
    match (host, port).to_socket_addrs() {
        Ok(addrs) => addrs.into_iter().any(|a| TcpStream::connect_timeout(&a, timeout).is_ok()),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_a_request_with_a_body() {
        let raw = b"POST /api/orders?x=1 HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\n\r\nhello";
        let req = read_request(&mut Cursor::new(&raw[..])).unwrap().unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path(), "/api/orders");
        assert_eq!(req.header("HOST"), Some("a"));
        assert_eq!(req.body, b"hello");
    }

    #[test]
    fn parses_a_chunked_response() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        let resp = read_response(&mut Cursor::new(&raw[..]), false).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"abcde");
    }

    #[test]
    fn reads_a_close_delimited_response() {
        let raw = b"HTTP/1.0 200 OK\r\n\r\nuntil eof";
        let resp = read_response(&mut Cursor::new(&raw[..]), false).unwrap();
        assert_eq!(resp.body, b"until eof");
    }

    #[test]
    fn refuses_an_oversized_head() {
        let mut raw = b"GET / HTTP/1.1\r\nX: ".to_vec();
        raw.extend(vec![b'a'; MAX_HEAD + 10]);
        raw.extend(b"\r\n\r\n");
        assert!(read_request(&mut Cursor::new(&raw[..])).is_err());
    }

    #[test]
    fn urls_split() {
        assert_eq!(parse_url("http://a:81/x").unwrap(), ("a".into(), 81, "/x".into()));
        assert_eq!(parse_url("http://a").unwrap(), ("a".into(), 80, "/".into()));
        assert_eq!(parse_url("http://127.0.0.1:8081/api/products").unwrap().1, 8081);
    }

    #[test]
    fn round_trip_over_loopback() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let _ = serve_on(listener, |req| Response::text(200, &format!("{} {}", req.method, req.path())));
        });
        let resp = call("GET", &format!("http://{addr}/hello?q"), &[], &[], Duration::from_secs(2)).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body_str(), "GET /hello");
    }
}
