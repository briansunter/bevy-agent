use crate::MAX_MESSAGE_BYTES;
#[cfg(test)]
use crate::http::HTTP_REQUEST_DEADLINE;
use crate::http::{HttpRequest, read_exact_until, write_all_until, write_http_response_until};
use crate::security::RemoteSecurity;
use anyhow::{Result, anyhow};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde_json::json;
use sha1::{Digest, Sha1};
use std::{net::TcpStream, time::Instant};

pub(crate) fn validate_websocket_handshake(
    request: &HttpRequest,
    security: &RemoteSecurity,
) -> Result<()> {
    security.validate()?;
    if !request.body.is_empty() {
        return Err(anyhow!(
            "websocket handshake must not contain a request body"
        ));
    }
    let valid_upgrade = request
        .header("upgrade")
        .map(|value| value.trim().eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    if !valid_upgrade {
        return Err(anyhow!("missing or invalid WebSocket Upgrade header"));
    }

    let connection_upgrade = request
        .header("connection")
        .map(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        })
        .unwrap_or(false);
    if !connection_upgrade {
        return Err(anyhow!("missing WebSocket Connection: Upgrade header"));
    }

    if request.header("sec-websocket-version").map(str::trim) != Some("13") {
        return Err(anyhow!("WebSocket version 13 is required"));
    }

    let valid_key = request
        .header("sec-websocket-key")
        .and_then(|value| BASE64_STANDARD.decode(value.trim()).ok())
        .map(|key| key.len() == 16)
        .unwrap_or(false);
    if !valid_key {
        return Err(anyhow!(
            "Sec-WebSocket-Key must be base64 encoding of 16 bytes"
        ));
    }

    // A browser Origin is a cross-site request signal. Tokenless loopback
    // WebSocket sessions are intended for non-browser local clients; require
    // an explicit session token before accepting browser-originated traffic.
    if security.session_token.is_none() && request.header("origin").is_some() {
        return Err(anyhow!(
            "WebSocket connections with an Origin header require a session token"
        ));
    }

    Ok(())
}

#[derive(Debug)]
pub(crate) enum WebSocketMessage {
    Text(String),
    Ping(Vec<u8>),
    Pong,
    Close,
}

pub(crate) fn websocket_accept_key(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    BASE64_STANDARD.encode(hasher.finalize())
}

#[cfg(test)]
pub(crate) fn read_websocket_text(stream: &mut TcpStream) -> Result<WebSocketMessage> {
    read_websocket_text_until(stream, Instant::now() + HTTP_REQUEST_DEADLINE)
}

pub(crate) fn read_websocket_text_until(
    stream: &mut TcpStream,
    deadline: Instant,
) -> Result<WebSocketMessage> {
    let mut header = [0; 2];
    read_exact_until(stream, &mut header, deadline)?;
    if header[0] & 0x70 != 0 {
        return Err(anyhow!(
            "websocket reserved bits require a negotiated extension"
        ));
    }
    let fin = header[0] & 0x80 != 0;
    let opcode = header[0] & 0x0f;
    if !matches!(opcode, 0x1 | 0x8..=0xa) {
        return Err(anyhow!("unsupported websocket opcode {opcode}"));
    }
    let masked = header[1] & 0x80 != 0;
    let mut len = (header[1] & 0x7f) as u64;
    if len == 126 {
        let mut extended = [0; 2];
        read_exact_until(stream, &mut extended, deadline)?;
        len = u16::from_be_bytes(extended) as u64;
    } else if len == 127 {
        let mut extended = [0; 8];
        read_exact_until(stream, &mut extended, deadline)?;
        len = u64::from_be_bytes(extended);
    }

    let is_control_frame = matches!(opcode, 0x8..=0xa);

    // Validate the frame before allocating its payload. Client-to-server
    // frames must be masked, this server has no continuation support so
    // fragments are rejected, control frames cannot exceed 125 bytes, and the
    // payload must fit the body limit (including 64-bit extended lengths).
    if !masked {
        return Err(anyhow!("unmasked websocket client frame"));
    }
    if !fin {
        return Err(anyhow!("fragmented websocket frames are not supported"));
    }
    if is_control_frame && len > 125 {
        return Err(anyhow!("control frame payload exceeds 125 bytes"));
    }
    if len > MAX_MESSAGE_BYTES as u64 {
        return Err(anyhow!(
            "websocket payload of {len} bytes exceeds limit of {MAX_MESSAGE_BYTES}"
        ));
    }
    if (header[1] & 0x7f == 126 && len < 126) || (header[1] & 0x7f == 127 && len <= u16::MAX as u64)
    {
        return Err(anyhow!(
            "websocket payload length must use its shortest encoding"
        ));
    }

    let mut mask = [0; 4];
    read_exact_until(stream, &mut mask, deadline)?;

    let mut payload = vec![0; len as usize];
    read_exact_until(stream, &mut payload, deadline)?;
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }

    match opcode {
        0x1 => Ok(WebSocketMessage::Text(String::from_utf8(payload)?)),
        0x8 if payload.len() == 1 => Err(anyhow!(
            "websocket close payload must be empty or include a status code"
        )),
        0x8 => {
            if payload.len() >= 2 {
                let code = u16::from_be_bytes([payload[0], payload[1]]);
                if !matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999) {
                    return Err(anyhow!("invalid websocket close status {code}"));
                }
                std::str::from_utf8(&payload[2..])?;
            }
            Ok(WebSocketMessage::Close)
        }
        0x9 => Ok(WebSocketMessage::Ping(payload)),
        0xa => Ok(WebSocketMessage::Pong),
        other => Err(anyhow!("unsupported websocket opcode {other}")),
    }
}

/// A bounded WebSocket session shares its connection deadline with every frame
/// and dispatched request. This keeps a persistent client from monopolizing
/// the serial simulation owner.
pub(crate) fn serve_websocket(
    stream: &mut TcpStream,
    request: &HttpRequest,
    security: &RemoteSecurity,
    deadline: Instant,
    mut dispatch: impl FnMut(String, Instant) -> Result<String>,
) -> Result<()> {
    if let Err(error) = validate_websocket_handshake(request, security) {
        return write_http_response_until(
            stream,
            400,
            "Bad Request",
            "application/json",
            &json!({"error": error.to_string()}).to_string(),
            Some(request),
            Some(security),
            deadline,
        );
    }
    let key = request
        .header("sec-websocket-key")
        .ok_or_else(|| anyhow!("missing Sec-WebSocket-Key"))?;
    let accept = websocket_accept_key(key);
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    write_all_until(stream, response.as_bytes(), deadline)?;
    loop {
        match read_websocket_text_until(stream, deadline)? {
            WebSocketMessage::Text(body) => {
                let response = dispatch(body, deadline)?;
                write_websocket_frame_until(stream, 0x1, response.as_bytes(), deadline)?;
            }
            WebSocketMessage::Ping(payload) => {
                write_websocket_frame_until(stream, 0xa, &payload, deadline)?
            }
            WebSocketMessage::Pong => {}
            WebSocketMessage::Close => {
                return write_websocket_frame_until(stream, 0x8, &[], deadline);
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn write_websocket_text(stream: &mut TcpStream, text: &str) -> Result<()> {
    write_websocket_frame(stream, 0x1, text.as_bytes())
}

#[cfg(test)]
pub(crate) fn write_websocket_frame(
    stream: &mut TcpStream,
    opcode: u8,
    payload: &[u8],
) -> Result<()> {
    write_websocket_frame_until(
        stream,
        opcode,
        payload,
        Instant::now() + HTTP_REQUEST_DEADLINE,
    )
}

pub(crate) fn write_websocket_frame_until(
    stream: &mut TcpStream,
    opcode: u8,
    payload: &[u8],
    deadline: Instant,
) -> Result<()> {
    if payload.len() > MAX_MESSAGE_BYTES {
        return Err(anyhow!("websocket response exceeds message limit"));
    }
    let mut frame = Vec::with_capacity(payload.len() + 10);
    frame.push(0x80 | opcode);
    if payload.len() < 126 {
        frame.push(payload.len() as u8);
    } else if u16::try_from(payload.len()).is_ok() {
        frame.push(126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    write_all_until(stream, &frame, deadline)
}
