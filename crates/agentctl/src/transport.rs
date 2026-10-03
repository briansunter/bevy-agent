//! Minimal HTTP transport with explicit framing and bounded response allocation.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv6Addr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

use crate::protocol::Response;

// Allow the server's 30-second operation deadline and response grace to finish.
const IO_TIMEOUT: Duration = Duration::from_secs(35);
const MAX_RESPONSE_METADATA_BYTES: usize = 32 * 1024;
// Match the remote bridge's complete-message bound.
const MAX_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn post_json_rpc(url: &str, request: &Value) -> Result<Response> {
    post_json_rpc_with_timeout(url, request, IO_TIMEOUT)
}

fn post_json_rpc_with_timeout(url: &str, request: &Value, timeout: Duration) -> Result<Response> {
    let endpoint = HttpEndpoint::parse(url)?;
    let expected_id = request
        .get("id")
        .ok_or_else(|| anyhow!("JSON-RPC request requires an id"))?;
    let body = serde_json::to_vec(request)?;
    if body.len() > MAX_RESPONSE_BODY_BYTES {
        return Err(anyhow!(
            "request exceeds message limit of {MAX_RESPONSE_BODY_BYTES} bytes"
        ));
    }
    let mut stream = DeadlineStream::connect(&endpoint, timeout)?;
    write!(
        stream,
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        endpoint.path,
        endpoint.host_header(),
        body.len(),
    )?;
    stream.write_all(&body)?;
    stream.flush()?;

    let (status, body) = read_http_response(stream)?;
    if !(200..300).contains(&status) {
        return Err(anyhow!(
            "HTTP {status}: {}",
            String::from_utf8_lossy(&body).trim()
        ));
    }
    let value = serde_json::from_slice(&body).context("invalid JSON in HTTP response")?;
    Response::parse(value, expected_id)
}

/// Apply one deadline across connection attempts, request writes, and response
/// reads. A peer cannot extend the call indefinitely by trickling bytes.
struct DeadlineStream {
    stream: TcpStream,
    deadline: Instant,
}

impl DeadlineStream {
    fn connect(endpoint: &HttpEndpoint, timeout: Duration) -> io::Result<Self> {
        let deadline = Instant::now() + timeout;
        let mut last_error = None;
        // Name resolution uses the operating system resolver. Check the same
        // deadline immediately after it returns; it cannot itself be interrupted.
        for address in (endpoint.host.as_str(), endpoint.port).to_socket_addrs()? {
            let remaining = Self::remaining(deadline)?;
            match TcpStream::connect_timeout(&address, remaining) {
                Ok(stream) => return Ok(Self { stream, deadline }),
                Err(error) => last_error = Some(error),
            }
        }
        Self::remaining(deadline)?;
        Err(last_error.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "HTTP host resolved to no addresses",
            )
        }))
    }

    fn remaining(deadline: Instant) -> io::Result<Duration> {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "HTTP call exceeded its total timeout",
                )
            })
    }

    fn finish<T>(&self, result: io::Result<T>) -> io::Result<T> {
        Self::remaining(self.deadline)?;
        result.map_err(|error| {
            if matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "HTTP call exceeded its total timeout",
                )
            } else {
                error
            }
        })
    }
}

impl Read for DeadlineStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream
            .set_read_timeout(Some(Self::remaining(self.deadline)?))?;
        let result = self.stream.read(buf);
        self.finish(result)
    }
}

impl Write for DeadlineStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream
            .set_write_timeout(Some(Self::remaining(self.deadline)?))?;
        let result = self.stream.write(buf);
        self.finish(result)
    }

    fn flush(&mut self) -> io::Result<()> {
        Self::remaining(self.deadline)?;
        let result = self.stream.flush();
        self.finish(result)
    }
}

#[derive(Debug)]
struct HttpEndpoint {
    host: String,
    port: u16,
    path: String,
}

impl HttpEndpoint {
    fn parse(url: &str) -> Result<Self> {
        if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(anyhow!(
                "HTTP URL must not contain whitespace or control characters"
            ));
        }
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| anyhow!("only http:// URLs are supported"))?;
        if rest.contains('#') {
            return Err(anyhow!("HTTP URL fragments are not supported"));
        }
        let split = rest.find(['/', '?']).unwrap_or(rest.len());
        let authority = &rest[..split];
        let path = match &rest[split..] {
            "" => "/rpc".to_string(),
            query if query.starts_with('?') => format!("/rpc{query}"),
            path => path.to_string(),
        };
        if authority.contains('@') {
            return Err(anyhow!("HTTP URL userinfo is not supported"));
        }
        let (host, port) = parse_host_port(authority)?;
        Ok(Self { host, port, path })
    }

    fn host_header(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn parse_host_port(authority: &str) -> Result<(String, u16)> {
    if let Some(bracketed) = authority.strip_prefix('[') {
        let closing = bracketed
            .find(']')
            .ok_or_else(|| anyhow!("invalid bracketed host {authority}"))?;
        let host = &bracketed[..closing];
        host.parse::<Ipv6Addr>()
            .with_context(|| format!("invalid IPv6 host {host:?}"))?;
        let suffix = &bracketed[closing + 1..];
        let port = if suffix.is_empty() {
            80
        } else {
            parse_port(
                suffix
                    .strip_prefix(':')
                    .ok_or_else(|| anyhow!("invalid host/port {authority}"))?,
            )?
        };
        return Ok((host.to_string(), port));
    }
    if authority.matches(':').count() > 1 {
        return Err(anyhow!(
            "IPv6 addresses must be enclosed in brackets: {authority}"
        ));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, parse_port(port)?),
        None => (authority, 80),
    };
    if host.is_empty() {
        return Err(anyhow!("HTTP URL requires a host"));
    }
    if !host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
    {
        return Err(anyhow!("invalid HTTP host {host:?}"));
    }
    Ok((host.to_string(), port))
}

fn parse_port(port: &str) -> Result<u16> {
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(anyhow!("invalid HTTP port {port:?}"));
    }
    port.parse().context("HTTP port is out of range")
}

fn read_http_response(stream: impl Read) -> Result<(u16, Vec<u8>)> {
    let mut reader = BufReader::new(stream);
    let mut metadata_left = MAX_RESPONSE_METADATA_BYTES;
    let (status, content_length, chunked) = loop {
        let line = read_crlf_line(&mut reader, &mut metadata_left)?;
        let line = std::str::from_utf8(&line).context("invalid HTTP status line")?;
        let mut parts = line.splitn(3, ' ');
        let version = parts.next().unwrap_or_default();
        let status = parts.next().unwrap_or_default();
        if !matches!(version, "HTTP/1.0" | "HTTP/1.1")
            || status.len() != 3
            || !status.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(anyhow!("malformed HTTP status line"));
        }
        let status = status.parse::<u16>()?;
        if !(100..600).contains(&status) {
            return Err(anyhow!("invalid HTTP status code {status}"));
        }
        let mut content_length = None;
        let mut chunked = false;
        loop {
            let line = read_crlf_line(&mut reader, &mut metadata_left)?;
            if line.is_empty() {
                break;
            }
            let (name, value) = parse_header(&line)?;
            if name.eq_ignore_ascii_case("content-length") {
                if content_length.is_some() {
                    return Err(anyhow!("duplicate HTTP Content-Length"));
                }
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(anyhow!("invalid HTTP Content-Length"));
                }
                let length = value
                    .parse::<usize>()
                    .context("invalid HTTP Content-Length")?;
                ensure_body_limit(length)?;
                content_length = Some(length);
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                if chunked || !value.eq_ignore_ascii_case("chunked") {
                    return Err(anyhow!("unsupported or duplicate HTTP Transfer-Encoding"));
                }
                chunked = true;
            }
        }
        if content_length.is_some() && chunked {
            return Err(anyhow!(
                "ambiguous HTTP response framing: Content-Length and Transfer-Encoding"
            ));
        }
        if status == 101 {
            return Err(anyhow!("HTTP protocol upgrades are not supported"));
        }
        if status < 200 {
            if chunked || content_length.is_some_and(|length| length != 0) {
                return Err(anyhow!(
                    "informational HTTP responses must not contain a body"
                ));
            }
            continue;
        }
        break (status, content_length, chunked);
    };

    let body = if matches!(status, 204 | 304) {
        Vec::new()
    } else if chunked {
        read_chunked_body(&mut reader, &mut metadata_left)?
    } else if let Some(length) = content_length {
        let mut body = vec![0; length];
        reader
            .read_exact(&mut body)
            .context("HTTP body shorter than declared Content-Length")?;
        body
    } else {
        let mut body = Vec::new();
        reader
            .take((MAX_RESPONSE_BODY_BYTES + 1) as u64)
            .read_to_end(&mut body)?;
        ensure_body_limit(body.len())?;
        body
    };
    Ok((status, body))
}

fn ensure_body_limit(length: usize) -> Result<()> {
    if length > MAX_RESPONSE_BODY_BYTES {
        return Err(anyhow!(
            "HTTP response body exceeds {MAX_RESPONSE_BODY_BYTES} bytes"
        ));
    }
    Ok(())
}

/// `read_until` can allocate without bound before finding a newline. This
/// version checks the shared metadata budget before copying each buffer slice.
fn read_crlf_line(reader: &mut impl BufRead, remaining: &mut usize) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Err(anyhow!("incomplete HTTP response metadata"));
        }
        let newline = buf.iter().position(|b| *b == b'\n');
        let count = newline.map_or(buf.len(), |index| index + 1);
        if count > *remaining {
            return Err(anyhow!(
                "HTTP response metadata exceeds {MAX_RESPONSE_METADATA_BYTES} bytes"
            ));
        }
        line.extend_from_slice(&buf[..count]);
        reader.consume(count);
        *remaining -= count;
        if newline.is_some() {
            if !line.ends_with(b"\r\n") {
                return Err(anyhow!("HTTP response lines must end with CRLF"));
            }
            line.truncate(line.len() - 2);
            return Ok(line);
        }
    }
}

fn parse_header(line: &[u8]) -> Result<(&str, &str)> {
    let line = std::str::from_utf8(line).context("invalid HTTP header encoding")?;
    let (name, value) = line
        .split_once(':')
        .ok_or_else(|| anyhow!("malformed HTTP header"))?;
    if name.is_empty() || !name.bytes().all(is_header_token) {
        return Err(anyhow!("invalid HTTP header name"));
    }
    if value.bytes().any(|b| b.is_ascii_control() && b != b'\t') {
        return Err(anyhow!("invalid HTTP header value"));
    }
    Ok((name, value.trim_matches([' ', '\t'])))
}

fn is_header_token(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn read_chunked_body(reader: &mut impl BufRead, metadata_left: &mut usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = read_crlf_line(reader, metadata_left)?;
        let line = std::str::from_utf8(&line).context("invalid HTTP chunk size")?;
        if line.bytes().any(|b| b.is_ascii_control()) {
            return Err(anyhow!("invalid HTTP chunk extension"));
        }
        let size = line.split(';').next().unwrap_or_default();
        if size.is_empty() || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(anyhow!("invalid HTTP chunk size"));
        }
        let size = usize::from_str_radix(size, 16).context("HTTP chunk size is out of range")?;
        if size == 0 {
            loop {
                let line = read_crlf_line(reader, metadata_left)?;
                if line.is_empty() {
                    return Ok(body);
                }
                let (name, _) = parse_header(&line)?;
                if name.eq_ignore_ascii_case("content-length")
                    || name.eq_ignore_ascii_case("transfer-encoding")
                {
                    return Err(anyhow!("HTTP trailers must not change response framing"));
                }
            }
        }
        let length = body
            .len()
            .checked_add(size)
            .ok_or_else(|| anyhow!("HTTP response body size overflow"))?;
        ensure_body_limit(length)?;
        let offset = body.len();
        body.resize(length, 0);
        reader
            .read_exact(&mut body[offset..])
            .context("incomplete HTTP chunk body")?;
        let mut delimiter = [0; 2];
        reader
            .read_exact(&mut delimiter)
            .context("missing HTTP chunk delimiter")?;
        if delimiter != *b"\r\n" {
            return Err(anyhow!("invalid HTTP chunk delimiter"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn endpoint_defaults_and_explicit_address_formats() {
        let endpoint = HttpEndpoint::parse("http://localhost").unwrap();
        assert_eq!(endpoint.host, "localhost");
        assert_eq!(endpoint.port, 80);
        assert_eq!(endpoint.path, "/rpc");
        let endpoint = HttpEndpoint::parse("http://127.0.0.1:4010/rpc?debug=true").unwrap();
        assert_eq!(endpoint.host_header(), "127.0.0.1:4010");
        assert_eq!(endpoint.path, "/rpc?debug=true");
        let endpoint = HttpEndpoint::parse("http://[::1]:4010/rpc").unwrap();
        assert_eq!(endpoint.host, "::1");
        assert_eq!(endpoint.host_header(), "[::1]:4010");
        assert_eq!(
            HttpEndpoint::parse("http://localhost?x=1").unwrap().path,
            "/rpc?x=1"
        );
    }

    #[test]
    fn endpoint_rejects_invalid_authorities_and_request_injection() {
        for url in [
            "https://localhost/rpc",
            "http://",
            "http:///rpc",
            "http://:4000/rpc",
            "http://user@localhost/rpc",
            "http://user:password@localhost/rpc",
            "http://[]/rpc",
            "http://[localhost]/rpc",
            "http://[::1]extra/rpc",
            "http://[::1/rpc",
            "http://::1:4000/rpc",
            "http://localhost:/rpc",
            "http://localhost:+4000/rpc",
            "http://localhost:65536/rpc",
            "http://localhost\r\nInjected: value/rpc",
            "http://localhost/rpc\r\nInjected: value",
            "http://localhost/rpc with space",
            "http://localhost/rpc#fragment",
            "http://localhost\\evil/rpc",
            "http://localhost\0/rpc",
        ] {
            assert!(HttpEndpoint::parse(url).is_err(), "{url:?}");
        }
    }

    fn parse_http(response: &[u8]) -> Result<(u16, Vec<u8>)> {
        read_http_response(Cursor::new(response))
    }

    #[test]
    fn content_length_frames_the_body_without_consuming_trailing_data() {
        let (status, body) =
            parse_http(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}ignored").unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"{}");
        assert!(parse_http(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n{}").is_err());
    }

    #[test]
    fn supports_chunked_bodies_extensions_and_trailers() {
        let (_, body) = parse_http(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked\r\n\r\n1;name=value\r\n{\r\n1\r\n}\r\n0\r\nX-Trace: okay\r\n\r\nignored").unwrap();
        assert_eq!(body, b"{}");
        let (_, body) =
            parse_http(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n").unwrap();
        assert!(body.is_empty());
    }

    #[test]
    fn supports_close_delimited_and_informational_responses() {
        let (_, body) = parse_http(b"HTTP/1.0 200 OK\r\n\r\n{}").unwrap();
        assert_eq!(body, b"{}");
        let (_, body) = parse_http(
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
        )
        .unwrap();
        assert_eq!(body, b"{}");
        let (_, body) = parse_http(b"HTTP/1.1 204 No Content\r\n\r\n").unwrap();
        assert!(body.is_empty());
    }

    #[test]
    fn rejects_ambiguous_and_invalid_response_framing() {
        for response in [
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: +2\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n",
            "HTTP/1.1 200 OK\n\n{}",
            "HTTP/2 200 OK\r\n\r\n{}",
            "HTTP/1.1 2000 OK\r\n\r\n{}",
            "HTTP/1.1 099 Invalid\r\n\r\n{}",
            "HTTP/1.1 600 Invalid\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\n folded: header\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\nMissingColon\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\nX-Header: invalid\0value\r\n\r\n{}",
            "HTTP/1.1 101 Switching Protocols\r\n\r\n",
            "HTTP/1.1 100 Continue\r\nContent-Length: 2\r\n\r\n{}",
        ] {
            assert!(parse_http(response.as_bytes()).is_err(), "{response:?}");
        }
    }

    #[test]
    fn rejects_invalid_or_incomplete_chunked_bodies() {
        for body in [
            "not-hex\r\n",
            "+2\r\n{}\r\n0\r\n\r\n",
            "2\r\n{",
            "2\r\n{}XX",
            "2\r\n{}\r\n",
            "0\r\nContent-Length: 2\r\n\r\n",
            "0\r\nBadTrailer\r\n\r\n",
            "0\r\nTransfer-Encoding: chunked\r\n\r\n",
            "1;bad\0extension\r\n{\r\n0\r\n\r\n",
        ] {
            let response = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{body}");
            assert!(parse_http(response.as_bytes()).is_err(), "{body:?}");
        }
    }

    #[test]
    fn bounds_declared_body_sizes_and_metadata_before_allocating() {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_RESPONSE_BODY_BYTES + 1
        );
        assert!(
            parse_http(response.as_bytes())
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            MAX_RESPONSE_BODY_BYTES + 1
        );
        assert!(
            parse_http(response.as_bytes())
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nX-Long: {}",
            "x".repeat(MAX_RESPONSE_METADATA_BYTES)
        );
        assert!(
            parse_http(response.as_bytes())
                .unwrap_err()
                .to_string()
                .contains("metadata exceeds")
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nX-Long: {}",
            "x".repeat(MAX_RESPONSE_METADATA_BYTES)
        );
        assert!(
            parse_http(response.as_bytes())
                .unwrap_err()
                .to_string()
                .contains("metadata exceeds")
        );
    }

    // Exercise close-delimited bounds without constructing an oversized input.
    struct EndlessBody {
        prefix: Cursor<&'static [u8]>,
        body_bytes_read: usize,
    }

    impl Read for EndlessBody {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let read = self.prefix.read(out)?;
            if read > 0 {
                return Ok(read);
            }
            out.fill(b'x');
            self.body_bytes_read += out.len();
            Ok(out.len())
        }
    }

    #[test]
    fn bounds_close_delimited_bodies() {
        let mut reader = EndlessBody {
            prefix: Cursor::new(b"HTTP/1.1 200 OK\r\n\r\n"),
            body_bytes_read: 0,
        };
        let error = read_http_response(&mut reader).unwrap_err();
        assert!(error.to_string().contains("body exceeds"));
        // BufReader can prefetch at most one buffer beyond the body limit.
        assert!(reader.body_bytes_read <= MAX_RESPONSE_BODY_BYTES + 8192);
    }

    fn read_test_request(stream: &mut TcpStream) {
        let mut reader = BufReader::new(stream);
        let mut metadata_left = MAX_RESPONSE_METADATA_BYTES;
        let line = read_crlf_line(&mut reader, &mut metadata_left).unwrap();
        assert_eq!(line, b"POST /rpc HTTP/1.1");
        let mut length = 0;
        loop {
            let line = read_crlf_line(&mut reader, &mut metadata_left).unwrap();
            if line.is_empty() {
                break;
            }
            let (name, value) = parse_header(&line).unwrap();
            if name.eq_ignore_ascii_case("content-length") {
                length = value.parse::<usize>().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["method"],
            "agent.info"
        );
    }

    fn mock_server(response: Vec<u8>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/rpc", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            read_test_request(&mut stream);
            stream.write_all(&response).unwrap();
        });
        (url, handle)
    }

    fn test_request() -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": "agent.info", "params": {}})
    }

    fn fixed_response(status: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[test]
    fn posts_requests_and_validates_results_and_rpc_errors() {
        for body in [
            r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"invalid or missing session token"}}"#,
        ] {
            let (url, server) = mock_server(fixed_response("200 OK", body));
            let response = post_json_rpc(&url, &test_request()).unwrap();
            server.join().unwrap();
            assert_eq!(
                response.value(),
                &serde_json::from_str::<Value>(body).unwrap()
            );
        }
    }

    #[test]
    fn rejects_http_errors_and_invalid_rpc_envelopes() {
        let (url, server) = mock_server(fixed_response(
            "500 Internal Server Error",
            "server exploded",
        ));
        let error = post_json_rpc(&url, &test_request()).unwrap_err();
        server.join().unwrap();
        assert_eq!(error.to_string(), "HTTP 500: server exploded");
        for body in [
            r#"{"jsonrpc":"2.0","id":2,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{},"error":null}"#,
            "invalid JSON",
        ] {
            let (url, server) = mock_server(fixed_response("200 OK", body));
            assert!(post_json_rpc(&url, &test_request()).is_err(), "{body}");
            server.join().unwrap();
        }
    }

    #[test]
    fn content_length_completes_while_server_keeps_connection_open() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/rpc", listener.local_addr().unwrap());
        let (release_tx, release_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_test_request(&mut stream);
            stream
                .write_all(&fixed_response(
                    "200 OK",
                    r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
                ))
                .unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(3));
        });
        let (result_tx, result_rx) = mpsc::channel();
        let client = thread::spawn(move || {
            result_tx
                .send(post_json_rpc(&url, &test_request()))
                .unwrap();
        });
        let result = result_rx.recv_timeout(Duration::from_secs(2));
        release_tx.send(()).unwrap();
        server.join().unwrap();
        client.join().unwrap();
        assert!(
            result
                .expect("client waited for EOF instead of Content-Length")
                .is_ok()
        );
    }

    #[test]
    fn trickled_response_cannot_extend_the_total_call_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/rpc", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            read_test_request(&mut stream);
            let body = br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .unwrap();
            for byte in body {
                if stream.write_all(&[*byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(15));
            }
        });
        let started = Instant::now();
        let error = post_json_rpc_with_timeout(&url, &test_request(), Duration::from_millis(120))
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
    }
    #[test]
    fn oversized_outbound_request_is_rejected_before_connecting() {
        let mut request = test_request();
        request["params"] = json!({"huge":"x".repeat(MAX_RESPONSE_BODY_BYTES)});
        let error = post_json_rpc("http://127.0.0.1:1/rpc", &request).unwrap_err();
        assert!(error.to_string().contains("message limit"));
    }
}
