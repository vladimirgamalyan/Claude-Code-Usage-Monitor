//! Minimal HTTP/HTTPS client: one request per connection, JSON in and out.
//!
//! The app talks to a handful of fixed endpoints and needs nothing more than a
//! GET, a POST with a JSON body, the status code and a few response headers.
//! A general-purpose client crate drags in a URL/IDNA stack that costs a
//! quarter of a megabyte in the binary and is never exercised here.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde::de::DeserializeOwned;

const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 3;
/// Guard against a server that streams forever; every response the app reads is
/// a few kilobytes of JSON.
const MAX_BODY: usize = 8 * 1024 * 1024;
const DEFAULT_USER_AGENT: &str = "claude-code-usage-monitor";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpError {
    /// The URL could not be parsed, or used a scheme other than http/https.
    InvalidUrl,
    /// DNS, TCP, TLS, or a proxy that refused the tunnel.
    Connect,
    /// The connection dropped or the response was not valid HTTP.
    Transport,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            HttpError::InvalidUrl => "invalid url",
            HttpError::Connect => "connection failed",
            HttpError::Transport => "transport error",
        };
        f.write_str(text)
    }
}

pub struct Response {
    status: u16,
    /// Header names are lowercased on the way in so lookups can compare directly.
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn json<T: DeserializeOwned>(&self) -> Option<T> {
        serde_json::from_slice(&self.body).ok()
    }
}

pub struct Request {
    method: &'static str,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
}

pub fn get(url: &str) -> Request {
    Request {
        method: "GET",
        url: url.to_string(),
        headers: Vec::new(),
        body: None,
    }
}

pub fn post(url: &str) -> Request {
    Request {
        method: "POST",
        url: url.to_string(),
        headers: Vec::new(),
        body: None,
    }
}

impl Request {
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn call(self) -> Result<Response, HttpError> {
        self.send()
    }

    pub fn send_json(mut self, body: &serde_json::Value) -> Result<Response, HttpError> {
        let encoded = serde_json::to_vec(body).map_err(|_| HttpError::Transport)?;
        if !self.has_header("content-type") {
            self.headers
                .push(("Content-Type".to_string(), "application/json".to_string()));
        }
        self.body = Some(encoded);
        self.send()
    }

    fn has_header(&self, name: &str) -> bool {
        self.headers
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case(name))
    }

    fn send(mut self) -> Result<Response, HttpError> {
        if !self.has_header("user-agent") {
            self.headers
                .push(("User-Agent".to_string(), DEFAULT_USER_AGENT.to_string()));
        }

        let mut url = Url::parse(&self.url)?;
        let mut redirects = 0;

        loop {
            let response = self.send_once(&url)?;

            // Only GET is replayed: re-sending a POST body to a location the
            // server picked is a decision this app should never make silently.
            let redirect = matches!(response.status, 301 | 302 | 303 | 307 | 308)
                && self.method == "GET"
                && redirects < MAX_REDIRECTS;
            if !redirect {
                return Ok(response);
            }

            let Some(location) = response.header("location") else {
                return Ok(response);
            };
            url = url.resolve(location)?;
            redirects += 1;
        }
    }

    fn send_once(&self, url: &Url) -> Result<Response, HttpError> {
        let mut stream = connect(url)?;

        let mut head = format!("{} {} HTTP/1.1\r\n", self.method, url.request_target());
        head.push_str(&format!("Host: {}\r\n", url.host_header()));
        head.push_str("Accept: application/json\r\n");
        // Without this a server may gzip the body, which this client cannot undo.
        head.push_str("Accept-Encoding: identity\r\n");
        head.push_str("Connection: close\r\n");
        for (name, value) in &self.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        match &self.body {
            Some(body) => head.push_str(&format!("Content-Length: {}\r\n", body.len())),
            None if self.method == "POST" => head.push_str("Content-Length: 0\r\n"),
            None => {}
        }
        head.push_str("\r\n");

        stream
            .write_all(head.as_bytes())
            .map_err(|_| HttpError::Transport)?;
        if let Some(body) = &self.body {
            stream.write_all(body).map_err(|_| HttpError::Transport)?;
        }
        stream.flush().map_err(|_| HttpError::Transport)?;

        read_response(stream, self.method)
    }
}

/// A parsed absolute URL, plus any `user:password` the authority carried (which
/// only ever happens for proxy URLs read from the environment).
struct Url {
    tls: bool,
    host: String,
    port: u16,
    /// Path and query, always starting with `/`.
    target: String,
    userinfo: Option<String>,
}

impl Url {
    fn parse(url: &str) -> Result<Self, HttpError> {
        let (scheme, rest) = url.split_once("://").ok_or(HttpError::InvalidUrl)?;
        let tls = match scheme.to_ascii_lowercase().as_str() {
            "https" => true,
            "http" => false,
            _ => return Err(HttpError::InvalidUrl),
        };

        let (authority, target) = match rest.find(['/', '?', '#']) {
            Some(index) if rest.as_bytes()[index] == b'/' => {
                (&rest[..index], rest[index..].to_string())
            }
            Some(index) => (&rest[..index], format!("/{}", &rest[index..])),
            None => (rest, "/".to_string()),
        };

        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((user, host)) => (Some(user.to_string()), host),
            None => (None, authority),
        };

        let (host, port) = split_host_port(hostport, tls)?;
        if host.is_empty() {
            return Err(HttpError::InvalidUrl);
        }

        Ok(Self {
            tls,
            host,
            port,
            target,
            userinfo,
        })
    }

    /// Resolve a `Location` header, which may be absolute or site-relative.
    fn resolve(&self, location: &str) -> Result<Self, HttpError> {
        if location.contains("://") {
            return Url::parse(location);
        }

        let target = if location.starts_with('/') {
            location.to_string()
        } else {
            let base = self
                .target
                .rsplit_once('/')
                .map(|(head, _)| head)
                .unwrap_or("");
            format!("{base}/{location}")
        };

        Ok(Self {
            tls: self.tls,
            host: self.host.clone(),
            port: self.port,
            target,
            userinfo: None,
        })
    }

    fn request_target(&self) -> &str {
        &self.target
    }

    /// `Host` omits the port when it is the scheme default, as browsers do.
    fn host_header(&self) -> String {
        let default = if self.tls { 443 } else { 80 };
        if self.port == default {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

fn split_host_port(hostport: &str, tls: bool) -> Result<(String, u16), HttpError> {
    let default_port = if tls { 443 } else { 80 };

    // IPv6 literals keep their brackets in the authority but not in the host.
    if let Some(rest) = hostport.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or(HttpError::InvalidUrl)?;
        let port = match tail.strip_prefix(':') {
            Some(port) => port.parse().map_err(|_| HttpError::InvalidUrl)?,
            None => default_port,
        };
        return Ok((host.to_string(), port));
    }

    match hostport.rsplit_once(':') {
        Some((host, port)) => {
            let port = port.parse().map_err(|_| HttpError::InvalidUrl)?;
            Ok((host.to_string(), port))
        }
        None => Ok((hostport.to_string(), default_port)),
    }
}

enum Stream {
    Plain(TcpStream),
    Tls(Box<native_tls::TlsStream<TcpStream>>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(stream) => stream.read(buf),
            Stream::Tls(stream) => stream.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(stream) => stream.write(buf),
            Stream::Tls(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(stream) => stream.flush(),
            Stream::Tls(stream) => stream.flush(),
        }
    }
}

fn connect(url: &Url) -> Result<Stream, HttpError> {
    let proxy = proxy_for(url);

    let tcp = match &proxy {
        Some(proxy) => connect_tcp(&proxy.host, proxy.port)?,
        None => connect_tcp(&url.host, url.port)?,
    };

    if let Some(proxy) = &proxy {
        if url.tls {
            // An HTTPS origin behind a proxy needs a tunnel; the TLS session is
            // end-to-end, so the proxy never sees the bearer token.
            open_tunnel(&tcp, proxy, url)?;
        }
    }

    if !url.tls {
        return Ok(Stream::Plain(tcp));
    }

    let connector = native_tls::TlsConnector::new().map_err(|_| HttpError::Connect)?;
    let tls = connector
        .connect(&url.host, tcp)
        .map_err(|_| HttpError::Connect)?;
    Ok(Stream::Tls(Box::new(tls)))
}

fn connect_tcp(host: &str, port: u16) -> Result<TcpStream, HttpError> {
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|_| HttpError::Connect)?;

    let mut last_error = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, TIMEOUT) {
            Ok(stream) => {
                stream
                    .set_read_timeout(Some(TIMEOUT))
                    .map_err(|_| HttpError::Connect)?;
                stream
                    .set_write_timeout(Some(TIMEOUT))
                    .map_err(|_| HttpError::Connect)?;
                return Ok(stream);
            }
            Err(error) => last_error = Some(error),
        }
    }

    let _ = last_error;
    Err(HttpError::Connect)
}

struct Proxy {
    host: String,
    port: u16,
    /// Pre-encoded `Proxy-Authorization` value, when the proxy URL carried credentials.
    auth: Option<String>,
}

/// Read proxy settings the same way command-line tools do: `ALL_PROXY` first,
/// then the scheme-specific variable, with `NO_PROXY` able to veto either.
fn proxy_for(url: &Url) -> Option<Proxy> {
    if no_proxy_matches(&url.host) {
        return None;
    }

    let scheme_var = if url.tls { "HTTPS_PROXY" } else { "HTTP_PROXY" };
    let raw = env_var("ALL_PROXY").or_else(|| env_var(scheme_var))?;

    // Bare `host:port` is a common shorthand in these variables.
    let parsed = if raw.contains("://") {
        Url::parse(&raw).ok()?
    } else {
        Url::parse(&format!("http://{raw}")).ok()?
    };

    let auth = parsed.userinfo.as_deref().map(basic_auth);

    Some(Proxy {
        host: parsed.host,
        port: parsed.port,
        auth,
    })
}

fn no_proxy_matches(host: &str) -> bool {
    let Some(no_proxy) = env_var("NO_PROXY") else {
        return false;
    };

    let host = host.to_ascii_lowercase();
    no_proxy.split(',').any(|entry| {
        let entry = entry.trim().trim_start_matches('.').to_ascii_lowercase();
        if entry.is_empty() {
            return false;
        }
        entry == "*" || host == entry || host.ends_with(&format!(".{entry}"))
    })
}

/// Environment variables for proxies are conventionally accepted in either case.
fn env_var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .or_else(|| std::env::var(name.to_ascii_lowercase()).ok())
        .filter(|value| !value.is_empty())
}

fn open_tunnel(tcp: &TcpStream, proxy: &Proxy, url: &Url) -> Result<(), HttpError> {
    let authority = url.authority();
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(auth) = &proxy.auth {
        request.push_str(&format!("Proxy-Authorization: Basic {auth}\r\n"));
    }
    request.push_str("Proxy-Connection: keep-alive\r\n\r\n");

    let mut stream = tcp;
    stream
        .write_all(request.as_bytes())
        .map_err(|_| HttpError::Connect)?;
    stream.flush().map_err(|_| HttpError::Connect)?;

    let mut reader = BufReader::new(tcp);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|_| HttpError::Connect)?;
    if parse_status(&status_line).ok_or(HttpError::Connect)? != 200 {
        return Err(HttpError::Connect);
    }

    // Drain the tunnel response headers so the TLS handshake starts clean.
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|_| HttpError::Connect)?;
        if read == 0 || line.trim_end().is_empty() {
            return Ok(());
        }
    }
}

fn basic_auth(userinfo: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let input = userinfo.as_bytes();
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);

    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(ALPHABET[(triple >> 18 & 0x3f) as usize] as char);
        out.push(ALPHABET[(triple >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6 & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }

    out
}

fn parse_status(status_line: &str) -> Option<u16> {
    status_line.split(' ').nth(1)?.trim().parse().ok()
}

/// Generic over the source so the framing rules can be tested without a socket.
fn read_response<R: Read>(source: R, method: &str) -> Result<Response, HttpError> {
    let mut reader = BufReader::new(source);

    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|_| HttpError::Transport)?;
    let status = parse_status(&status_line).ok_or(HttpError::Transport)?;

    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|_| HttpError::Transport)?;
        if read == 0 {
            break;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }

    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };

    let body = if method == "HEAD" || status == 204 || status == 304 {
        Vec::new()
    } else if header("transfer-encoding").is_some_and(|value| {
        value
            .split(',')
            .any(|part| part.trim().eq_ignore_ascii_case("chunked"))
    }) {
        read_chunked(&mut reader)?
    } else if let Some(length) =
        header("content-length").and_then(|v| v.trim().parse::<usize>().ok())
    {
        let mut body = vec![0u8; length.min(MAX_BODY)];
        reader
            .read_exact(&mut body)
            .map_err(|_| HttpError::Transport)?;
        body
    } else {
        // No framing headers: the body runs until the server closes, which is
        // why every request announces `Connection: close`.
        let mut body = Vec::new();
        reader
            .take(MAX_BODY as u64)
            .read_to_end(&mut body)
            .map_err(|_| HttpError::Transport)?;
        body
    };

    Ok(Response {
        status,
        headers,
        body,
    })
}

fn read_chunked<R: Read>(reader: &mut BufReader<R>) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::new();

    loop {
        let mut size_line = String::new();
        if reader
            .read_line(&mut size_line)
            .map_err(|_| HttpError::Transport)?
            == 0
        {
            return Err(HttpError::Transport);
        }

        // A chunk size may carry extensions after a semicolon; they are ignored.
        let size_text = size_line
            .trim_end_matches(['\r', '\n'])
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| HttpError::Transport)?;

        if size == 0 {
            // Consume the trailer section, terminated by a blank line.
            loop {
                let mut line = String::new();
                let read = reader
                    .read_line(&mut line)
                    .map_err(|_| HttpError::Transport)?;
                if read == 0 || line.trim_end().is_empty() {
                    return Ok(body);
                }
            }
        }

        if body.len() + size > MAX_BODY {
            return Err(HttpError::Transport);
        }

        let start = body.len();
        body.resize(start + size, 0);
        reader
            .read_exact(&mut body[start..])
            .map_err(|_| HttpError::Transport)?;

        // Each chunk is followed by its own CRLF.
        let mut terminator = String::new();
        reader
            .read_line(&mut terminator)
            .map_err(|_| HttpError::Transport)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_url_parts() {
        let url = Url::parse("https://api.example.com/v1/usage?scope=all").expect("valid url");
        assert!(url.tls);
        assert_eq!(url.host, "api.example.com");
        assert_eq!(url.port, 443);
        assert_eq!(url.request_target(), "/v1/usage?scope=all");
        assert_eq!(url.host_header(), "api.example.com");
    }

    #[test]
    fn defaults_path_and_keeps_explicit_port() {
        let url = Url::parse("http://localhost:8080").expect("valid url");
        assert!(!url.tls);
        assert_eq!(url.port, 8080);
        assert_eq!(url.request_target(), "/");
        assert_eq!(url.host_header(), "localhost:8080");
    }

    #[test]
    fn keeps_query_only_urls_rooted() {
        let url = Url::parse("https://example.com?a=1").expect("valid url");
        assert_eq!(url.request_target(), "/?a=1");
    }

    #[test]
    fn splits_proxy_credentials() {
        let url = Url::parse("http://user:secret@proxy.local:3128").expect("valid url");
        assert_eq!(url.userinfo.as_deref(), Some("user:secret"));
        assert_eq!(url.host, "proxy.local");
        assert_eq!(url.port, 3128);
    }

    #[test]
    fn rejects_unsupported_schemes() {
        assert!(matches!(
            Url::parse("ftp://example.com"),
            Err(HttpError::InvalidUrl)
        ));
        assert!(matches!(
            Url::parse("example.com/no-scheme"),
            Err(HttpError::InvalidUrl)
        ));
    }

    #[test]
    fn reads_chunked_body_with_extensions_and_trailer() {
        let raw = concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Type: application/json\r\n",
            "Transfer-Encoding: chunked\r\n",
            "\r\n",
            // 12 bytes, with a chunk extension the parser must ignore.
            "c;name=value\r\n",
            "{\"five_hour\"\r\n",
            // 9 bytes.
            "9\r\n",
            ":{\"a\":1}}\r\n",
            "0\r\n",
            "X-Trailer: ignored\r\n",
            "\r\n",
        );
        let response = read_response(raw.as_bytes(), "GET").expect("parses");

        assert_eq!(response.status(), 200);
        assert_eq!(response.header("content-type"), Some("application/json"));
        assert_eq!(response.body, b"{\"five_hour\":{\"a\":1}}");
    }

    #[test]
    fn reads_body_framed_by_content_length() {
        // Anything past the declared length belongs to no response and is dropped.
        let raw = "HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n{\"a\":1}leftover";
        let response = read_response(raw.as_bytes(), "GET").expect("parses");

        assert_eq!(response.body, b"{\"a\":1}");
    }

    #[test]
    fn reads_unframed_body_until_close_and_keeps_error_headers() {
        let raw = concat!(
            "HTTP/1.1 429 Too Many Requests\r\n",
            "Anthropic-RateLimit-Unified-5h-Utilization: 0.42\r\n",
            "\r\n",
            "{\"error\":true}",
        );
        let response = read_response(raw.as_bytes(), "GET").expect("parses");

        assert_eq!(response.status(), 429);
        // Header lookups ignore case, which matters for the rate-limit headers.
        assert_eq!(
            response.header("anthropic-ratelimit-unified-5h-utilization"),
            Some("0.42")
        );
        assert_eq!(response.body, b"{\"error\":true}");
    }

    #[test]
    fn skips_body_for_status_codes_that_carry_none() {
        let response =
            read_response("HTTP/1.1 204 No Content\r\n\r\n".as_bytes(), "GET").expect("parses");

        assert_eq!(response.status(), 204);
        assert!(response.body.is_empty());
    }

    #[test]
    fn encodes_basic_auth() {
        assert_eq!(basic_auth("user:secret"), "dXNlcjpzZWNyZXQ=");
        assert_eq!(basic_auth("a"), "YQ==");
        assert_eq!(basic_auth("ab"), "YWI=");
    }

    #[test]
    fn resolves_relative_and_absolute_redirects() {
        let base = Url::parse("https://example.com/a/b?x=1").expect("valid url");

        let rooted = base.resolve("/c").expect("valid redirect");
        assert_eq!(rooted.host, "example.com");
        assert_eq!(rooted.request_target(), "/c");

        let relative = base.resolve("c").expect("valid redirect");
        assert_eq!(relative.request_target(), "/a/c");

        let absolute = base
            .resolve("https://other.example.com/d")
            .expect("valid redirect");
        assert_eq!(absolute.host, "other.example.com");
        assert_eq!(absolute.request_target(), "/d");
    }
}
