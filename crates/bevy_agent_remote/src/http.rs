use crate::MAX_MESSAGE_BYTES;
use crate::security::RemoteSecurity;
use anyhow::{Context, Result, anyhow};
use std::{
    io::{Read, Write},
    net::TcpStream,
    time::{Duration, Instant},
};
pub(crate) const MAX_HTTP_HEADER_BYTES: usize = 32 * 1024;
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const HTTP_REQUEST_DEADLINE: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub(crate) struct HttpRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: String,
}

impl HttpRequest {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

#[cfg(test)]
pub(crate) fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    read_http_request_until(stream, Instant::now() + HTTP_REQUEST_DEADLINE)
}

pub(crate) fn read_http_request_until(
    stream: &mut TcpStream,
    deadline: Instant,
) -> Result<HttpRequest> {
    let mut bytes = Vec::new();
    let header_end = loop {
        stream.set_read_timeout(Some(remaining_timeout(deadline, HTTP_READ_TIMEOUT)?))?;
        let mut buf = [0; 1024];
        // Peek before consuming: HTTP and the first WebSocket frame can arrive
        // in one packet, and bytes after this request must remain on the socket.
        let available = stream.peek(&mut buf)?;
        if available == 0 {
            return Err(anyhow!("connection closed before HTTP request"));
        }
        let previous = bytes.len();
        bytes.extend_from_slice(&buf[..available]);
        let end = find_header_end(&bytes);
        if end.unwrap_or(bytes.len()) > MAX_HTTP_HEADER_BYTES {
            return Err(anyhow!(
                "HTTP request headers exceed {MAX_HTTP_HEADER_BYTES} bytes"
            ));
        }
        let consume = end.map_or(available, |end| end + 4 - previous);
        bytes.truncate(previous + consume);
        read_exact_until(stream, &mut bytes[previous..], deadline)?;
        if let Some(end) = end {
            break end;
        }
    };

    let headers_text = std::str::from_utf8(&bytes[..header_end])?;
    let mut lines = headers_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow!("missing HTTP request line"))?;
    let parts: Vec<_> = request_line.split_ascii_whitespace().collect();
    if parts.len() != 3
        || !matches!(parts[2], "HTTP/1.1" | "HTTP/1.0")
        || !parts[1].starts_with('/')
        || !parts[0].bytes().all(is_http_token)
    {
        return Err(anyhow!("malformed HTTP request line"));
    }
    let method = parts[0].to_owned();
    let path = parts[1].to_owned();
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow!("malformed HTTP header"))?;
        if key.is_empty()
            || !key.bytes().all(is_http_token)
            || value
                .bytes()
                .any(|byte| byte < 32 && byte != b'\t' || byte == 127)
        {
            return Err(anyhow!("malformed HTTP header"));
        }
        let key = key.to_ascii_lowercase();
        if matches!(
            key.as_str(),
            "content-length"
                | "transfer-encoding"
                | "origin"
                | "host"
                | "content-type"
                | "sec-websocket-key"
                | "sec-websocket-version"
                | "upgrade"
        ) && headers.iter().any(|(existing, _)| existing == &key)
        {
            return Err(anyhow!("duplicate HTTP {key} header"));
        }
        headers.push((key, value.trim().to_owned()));
    }
    if headers.iter().any(|(key, _)| key == "transfer-encoding") {
        return Err(anyhow!("HTTP Transfer-Encoding is not supported"));
    }
    let content_length = match headers.iter().find(|(key, _)| key == "content-length") {
        Some((_, value))
            if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            value
                .parse::<usize>()
                .with_context(|| format!("invalid Content-Length: {value}"))?
        }
        Some((_, value)) => return Err(anyhow!("invalid Content-Length: {value}")),
        None => 0,
    };
    if content_length > MAX_MESSAGE_BYTES {
        return Err(anyhow!(
            "HTTP request body of {content_length} bytes exceeds limit of {MAX_MESSAGE_BYTES}"
        ));
    }
    let mut body = vec![0; content_length];
    read_exact_until(stream, &mut body, deadline)
        .context("HTTP body shorter than declared Content-Length or request deadline exceeded")?;
    Ok(HttpRequest {
        method,
        path,
        headers,
        body: String::from_utf8(body)?,
    })
}

fn is_http_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

pub(crate) fn remaining_timeout(deadline: Instant, maximum: Duration) -> Result<Duration> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| anyhow!("request deadline exceeded"))?;
    if remaining.is_zero() {
        return Err(anyhow!("request deadline exceeded"));
    }
    Ok(remaining.min(maximum))
}

pub(crate) fn read_exact_until(
    stream: &mut TcpStream,
    mut bytes: &mut [u8],
    deadline: Instant,
) -> Result<()> {
    while !bytes.is_empty() {
        stream.set_read_timeout(Some(remaining_timeout(deadline, HTTP_READ_TIMEOUT)?))?;
        let read = stream.read(bytes)?;
        if read == 0 {
            return Err(anyhow!("connection closed before request was complete"));
        }
        bytes = &mut bytes[read..];
    }
    Ok(())
}

pub(crate) fn write_all_until(
    stream: &mut TcpStream,
    mut bytes: &[u8],
    deadline: Instant,
) -> Result<()> {
    while !bytes.is_empty() {
        stream.set_write_timeout(Some(remaining_timeout(deadline, HTTP_WRITE_TIMEOUT)?))?;
        let written = stream.write(bytes)?;
        if written == 0 {
            return Err(anyhow!("connection closed before response was complete"));
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

pub(crate) fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Validate HTTP RPC preconditions: browser `Origin` gating (mirrors the
/// WebSocket rule: tokenless servers reject browser-originated traffic) and
/// mandatory `Content-Type: application/json`.
pub(crate) fn validate_http_rpc(request: &HttpRequest, security: &RemoteSecurity) -> Result<()> {
    validate_http_origin(request, security)?;
    let content_type = request.header("content-type").unwrap_or("");
    if !content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("application/json")
    {
        return Err(anyhow!("POST /rpc requires Content-Type: application/json"));
    }
    Ok(())
}

pub(crate) fn validate_http_origin(request: &HttpRequest, security: &RemoteSecurity) -> Result<()> {
    security.validate()?;
    if security.session_token.is_none() && request.header("origin").is_some() {
        return Err(anyhow!(
            "HTTP requests with an Origin header require a session token"
        ));
    }
    Ok(())
}

/// CORS header value: only echo an explicitly allowed origin, otherwise none.
pub(crate) fn cors_allow_origin(
    request: &HttpRequest,
    security: &RemoteSecurity,
) -> Option<String> {
    let allowed = security.allowed_origin.as_deref()?;
    let origin = request.header("origin")?;
    if origin == allowed {
        Some(allowed.to_string())
    } else {
        None
    }
}

pub(crate) fn write_preflight_response_until(
    stream: &mut TcpStream,
    request: &HttpRequest,
    security: &RemoteSecurity,
    deadline: Instant,
) -> Result<()> {
    security.validate()?;
    let mut response =
        String::from("HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n");
    if let Some(origin) = cors_allow_origin(request, security) {
        response.push_str(&format!("Access-Control-Allow-Origin: {origin}\r\n"));
        response.push_str("Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n");
        response.push_str("Access-Control-Allow-Headers: Content-Type\r\n");
    }
    response.push_str("\r\n");
    write_all_until(stream, response.as_bytes(), deadline)
}

#[cfg(test)]
pub(crate) fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &str,
) -> Result<()> {
    write_http_response_with_security(stream, status, reason, content_type, body, None, None)
}

#[cfg(test)]
pub(crate) fn write_http_response_with_security(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &str,
    request: Option<&HttpRequest>,
    security: Option<&RemoteSecurity>,
) -> Result<()> {
    write_http_response_until(
        stream,
        status,
        reason,
        content_type,
        body,
        request,
        security,
        Instant::now() + HTTP_REQUEST_DEADLINE,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_http_response_until(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &str,
    request: Option<&HttpRequest>,
    security: Option<&RemoteSecurity>,
    deadline: Instant,
) -> Result<()> {
    if body.len() > MAX_MESSAGE_BYTES {
        return Err(anyhow!("HTTP response exceeds message limit"));
    }
    if let Some(security) = security {
        security.validate()?;
    }
    // No wildcard CORS by default. Only echo an explicitly configured
    // `allowed_origin`, and only when it matches the request Origin.
    let cors = match (request, security) {
        (Some(req), Some(sec)) => cors_allow_origin(req, sec),
        _ => None,
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n",
        body.len()
    );
    if let Some(origin) = cors {
        response.push_str(&format!("Access-Control-Allow-Origin: {origin}\r\n"));
        response.push_str("Vary: Origin\r\n");
    }
    response.push_str("\r\n");
    response.push_str(body);
    write_all_until(stream, response.as_bytes(), deadline)
}
