//! Ephemeral loopback HTTP/1.1 byte-range streaming server (`127.0.0.1:0`) for `<video>`.
//!
//! Custom WebView URI schemes (`neb://` via `WKURLSchemeHandler`) buffer responses into
//! one-shot `Vec<u8>` buffers and stall when macOS `AVPlayer` issues open-ended
//! `Range: bytes=0-<EOF>` probes before seeking to trailing `moov` atoms in camera `.MOV`s.
//! Serving `<video>` over a capability-protected `127.0.0.1:<port>/media/<token>/<id>/<rev>`
//! loopback socket lets native OS media engines seek and stream in `64 KiB` chunks directly.
use crate::preview_protocol::{parse_range_header, status_for};
use neb_service::{CommandError, Service};
use percent_encoding::percent_decode_str;
use std::{
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tauri::http::StatusCode;
use uuid::Uuid;

const MAX_HEADER_BYTES: usize = 8192;
const STREAM_BUF_BYTES: usize = 64 * 1024;
const MAX_CONCURRENT_STREAMS: usize = 64;
const MAX_ID_LEN: usize = 64;
const APP_ORIGINS: [&str; 2] = ["tauri://localhost", "http://tauri.localhost"];

pub struct MediaServer {
    base_url: String,
}

impl MediaServer {
    pub fn start(service: Arc<Service>) -> io::Result<Self> {
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let base_url = format!("http://127.0.0.1:{port}/media/{token}");
        let active = Arc::new(AtomicUsize::new(0));
        let expected_token = Arc::new(token);

        std::thread::Builder::new()
            .name("neb-media-listener".into())
            .spawn(move || {
                for incoming in listener.incoming() {
                    let Ok(mut stream) = incoming else {
                        continue;
                    };
                    let prev = active.fetch_add(1, Ordering::AcqRel);
                    if prev >= MAX_CONCURRENT_STREAMS {
                        active.fetch_sub(1, Ordering::AcqRel);
                        let _ = write_simple_response(
                            &mut stream,
                            StatusCode::SERVICE_UNAVAILABLE,
                            "text/plain; charset=utf-8",
                            &[],
                            b"Too many concurrent media streams",
                        );
                        continue;
                    }
                    let svc = Arc::clone(&service);
                    let tok = Arc::clone(&expected_token);
                    let counter = Arc::clone(&active);
                    if std::thread::Builder::new()
                        .name("neb-media-stream".into())
                        .spawn(move || {
                            struct Guard(Arc<AtomicUsize>);
                            impl Drop for Guard {
                                fn drop(&mut self) {
                                    self.0.fetch_sub(1, Ordering::AcqRel);
                                }
                            }
                            let _guard = Guard(counter);
                            let _ = handle_connection(stream, &svc, &tok);
                        })
                        .is_err()
                    {
                        active.fetch_sub(1, Ordering::AcqRel);
                    }
                }
            })?;

        Ok(Self { base_url })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn allowed_origin(origin: &str) -> bool {
    APP_ORIGINS.contains(&origin) || (cfg!(debug_assertions) && origin == "http://localhost:5173")
}

fn valid_asset_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn valid_revision(rev: &str) -> bool {
    !rev.is_empty() && rev.bytes().all(|b| b.is_ascii_digit())
}

fn read_request_head(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Request headers too large",
            ));
        }
    }
    Ok(buf)
}

fn status_reason(status: StatusCode) -> &'static str {
    match status {
        StatusCode::OK => "OK",
        StatusCode::PARTIAL_CONTENT => "Partial Content",
        StatusCode::NO_CONTENT => "No Content",
        StatusCode::BAD_REQUEST => "Bad Request",
        StatusCode::FORBIDDEN => "Forbidden",
        StatusCode::NOT_FOUND => "Not Found",
        StatusCode::METHOD_NOT_ALLOWED => "Method Not Allowed",
        StatusCode::CONFLICT => "Conflict",
        StatusCode::RANGE_NOT_SATISFIABLE => "Range Not Satisfiable",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "Unsupported Media Type",
        StatusCode::SERVICE_UNAVAILABLE => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

fn write_simple_response(
    stream: &mut TcpStream,
    status: StatusCode,
    content_type: &str,
    extra_headers: &[(&str, String)],
    body: &[u8],
) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        status.as_u16(),
        status_reason(status),
        body.len()
    );
    for (k, v) in extra_headers {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if !body.is_empty() {
        stream.write_all(body)?;
    }
    stream.flush()
}

fn write_command_error(
    stream: &mut TcpStream,
    err: &CommandError,
    cors_origin: Option<&str>,
) -> io::Result<()> {
    let status = status_for(err.code);
    let body = serde_json::to_vec(&serde_json::json!({
        "code": err.code,
        "message": err.message,
    }))
    .unwrap_or_default();
    let mut extra = Vec::new();
    if let Some(origin) = cors_origin {
        extra.push(("Access-Control-Allow-Origin", origin.to_owned()));
        extra.push(("Vary", "Origin".into()));
    }
    write_simple_response(stream, status, "application/json", &extra, &body)
}

fn is_client_disconnect(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
    )
}

fn handle_connection(mut stream: TcpStream, service: &Service, expected_token: &str) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));

    let raw = read_request_head(&mut stream)?;
    if raw.is_empty() {
        return Ok(());
    }
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(raw.len());
    let Ok(head_str) = std::str::from_utf8(&raw[..head_end]) else {
        return write_simple_response(
            &mut stream,
            StatusCode::BAD_REQUEST,
            "text/plain; charset=utf-8",
            &[],
            b"Invalid UTF-8 headers",
        );
    };

    let mut lines = head_str.split("\r\n");
    let Some(request_line) = lines.next() else {
        return Ok(());
    };
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(raw_target)) = (parts.next(), parts.next()) else {
        return write_simple_response(
            &mut stream,
            StatusCode::BAD_REQUEST,
            "text/plain; charset=utf-8",
            &[],
            b"Malformed request line",
        );
    };

    let mut range_header: Option<&str> = None;
    let mut origin_header: Option<&str> = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim();
            if name.eq_ignore_ascii_case("range") {
                range_header = Some(value);
            } else if name.eq_ignore_ascii_case("origin") {
                origin_header = Some(value);
            }
        }
    }

    if let Some(origin) = origin_header {
        if origin != "null" && !allowed_origin(origin) {
            return write_simple_response(
                &mut stream,
                StatusCode::FORBIDDEN,
                "text/plain; charset=utf-8",
                &[],
                b"Forbidden origin",
            );
        }
    }
    let cors_origin = origin_header.filter(|o| allowed_origin(o));

    let path_only = raw_target.split('?').next().unwrap_or(raw_target);
    let Ok(decoded) = percent_decode_str(path_only).decode_utf8() else {
        return write_simple_response(
            &mut stream,
            StatusCode::BAD_REQUEST,
            "text/plain; charset=utf-8",
            &[],
            b"Invalid path encoding",
        );
    };
    let segs: Vec<&str> = decoded
        .strip_prefix('/')
        .unwrap_or(&decoded)
        .split('/')
        .collect();
    let ["media", req_token, id, rev] = segs.as_slice() else {
        return write_simple_response(
            &mut stream,
            StatusCode::NOT_FOUND,
            "text/plain; charset=utf-8",
            &[],
            b"Not found",
        );
    };

    if !constant_time_eq(req_token.as_bytes(), expected_token.as_bytes()) {
        return write_simple_response(
            &mut stream,
            StatusCode::FORBIDDEN,
            "text/plain; charset=utf-8",
            &[],
            b"Invalid capability token",
        );
    }
    if !valid_asset_id(id) || !valid_revision(rev) {
        return write_simple_response(
            &mut stream,
            StatusCode::BAD_REQUEST,
            "text/plain; charset=utf-8",
            &[],
            b"Invalid media path",
        );
    }

    if method.eq_ignore_ascii_case("OPTIONS") {
        let mut extra = vec![
            ("Accept-Ranges", "bytes".into()),
            ("Access-Control-Allow-Methods", "GET, HEAD, OPTIONS".into()),
            ("Access-Control-Allow-Headers", "Range, If-None-Match".into()),
            (
                "Access-Control-Expose-Headers",
                "Accept-Ranges, Content-Range, Content-Length".into(),
            ),
        ];
        if let Some(origin) = cors_origin {
            extra.push(("Access-Control-Allow-Origin", origin.to_owned()));
            extra.push(("Vary", "Origin".into()));
        }
        return write_simple_response(&mut stream, StatusCode::NO_CONTENT, "text/plain", &extra, &[]);
    }

    let head_only = method.eq_ignore_ascii_case("HEAD");
    if !head_only && !method.eq_ignore_ascii_case("GET") {
        return write_simple_response(
            &mut stream,
            StatusCode::METHOD_NOT_ALLOWED,
            "text/plain; charset=utf-8",
            &[],
            b"Only GET, HEAD, and OPTIONS are supported",
        );
    }

    let range = match parse_range_header(range_header) {
        Ok(r) => r,
        Err(_) => {
            let total = service
                .open_media_stream(id, None, true)
                .map(|m| m.total_len.to_string())
                .unwrap_or_else(|_| "*".into());
            let mut extra = vec![
                ("Accept-Ranges", "bytes".into()),
                ("Content-Range", format!("bytes */{total}")),
            ];
            if let Some(origin) = cors_origin {
                extra.push(("Access-Control-Allow-Origin", origin.to_owned()));
                extra.push(("Vary", "Origin".into()));
            }
            return write_simple_response(
                &mut stream,
                StatusCode::RANGE_NOT_SATISFIABLE,
                "text/plain; charset=utf-8",
                &extra,
                &[],
            );
        }
    };

    let media = match service.open_media_stream(id, range, head_only) {
        Ok(m) => m,
        Err(err) if err.code == "E_RANGE_UNSATISFIABLE" => {
            let total = if err.message.is_empty() || !err.message.bytes().all(|b| b.is_ascii_digit()) {
                "*"
            } else {
                err.message.as_str()
            };
            let mut extra = vec![
                ("Accept-Ranges", "bytes".into()),
                ("Content-Range", format!("bytes */{total}")),
            ];
            if let Some(origin) = cors_origin {
                extra.push(("Access-Control-Allow-Origin", origin.to_owned()));
                extra.push(("Vary", "Origin".into()));
            }
            return write_simple_response(
                &mut stream,
                StatusCode::RANGE_NOT_SATISFIABLE,
                "text/plain; charset=utf-8",
                &extra,
                &[],
            );
        }
        Err(err) => return write_command_error(&mut stream, &err, cors_origin),
    };

    let status = if media.partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let content_len = if media.total_len == 0 {
        0
    } else if media.partial {
        media.end.saturating_sub(media.start) + 1
    } else {
        media.total_len
    };

    let etag = format!("\"{id}-{rev}-{}\"", media.total_len);
    let mut header_block = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nETag: {}\r\nCache-Control: private, max-age=3600\r\nAccess-Control-Expose-Headers: Accept-Ranges, Content-Range, Content-Length, ETag\r\nConnection: close\r\n",
        status.as_u16(),
        status_reason(status),
        media.content_type,
        content_len,
        etag
    );
    if media.partial {
        header_block.push_str(&format!(
            "Content-Range: bytes {}-{}/{}\r\n",
            media.start, media.end, media.total_len
        ));
    }
    if let Some(origin) = cors_origin {
        header_block.push_str(&format!("Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n"));
    }
    header_block.push_str("\r\n");

    if let Err(err) = stream.write_all(header_block.as_bytes()) {
        if is_client_disconnect(err.kind()) {
            return Ok(());
        }
        return Err(err);
    }

    if let Some(mut reader) = media.reader {
        let mut buf = [0u8; STREAM_BUF_BYTES];
        let mut remaining = content_len;
        while remaining > 0 {
            let to_read = buf.len().min(remaining as usize);
            let n = reader.read(&mut buf[..to_read])?;
            if n == 0 {
                break;
            }
            if let Err(err) = stream.write_all(&buf[..n]) {
                if is_client_disconnect(err.kind()) {
                    return Ok(());
                }
                return Err(err);
            }
            remaining -= n as u64;
        }
    }
    let _ = stream.flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    #[test]
    fn rejects_invalid_capability_token_and_streams_unclamped_ranges() {
        let data_dir = tempfile::tempdir().unwrap();
        let lib_dir = tempfile::tempdir().unwrap();
        let mut payload = vec![0u8; 300_000];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        // Prepend minimal valid ftyp + mvhd so scanner indexes clip.mp4 as video
        let ftyp_moov: &[u8] = &[
            0, 0, 0, 20, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm', 0, 0, 2, 0, b'i', b's',
            b'o', b'm',
        ];
        payload[..ftyp_moov.len()].copy_from_slice(ftyp_moov);
        fs::write(lib_dir.path().join("clip.mp4"), &payload).unwrap();

        let service = Arc::new(Service::open(data_dir.path()).unwrap());
        let lib = service
            .execute(
                "library.add",
                json!({ "name": "Videos", "root": lib_dir.path() }),
            )
            .unwrap();
        service
            .execute("library.scan", json!({ "library_id": lib["id"] }))
            .unwrap();
        let list = service
            .execute("asset.list", json!({ "library_id": lib["id"] }))
            .unwrap();
        let asset_id = list["items"][0]["id"].as_str().unwrap();

        let server = MediaServer::start(Arc::clone(&service)).unwrap();
        let base = server.base_url();
        let host_port = base
            .strip_prefix("http://")
            .unwrap()
            .split('/')
            .next()
            .unwrap();

        // 1. Wrong token returns 403 Forbidden.
        {
            let mut conn = TcpStream::connect(host_port).unwrap();
            write!(
                conn,
                "GET /media/00000000000000000000000000000000/{asset_id}/0 HTTP/1.1\r\nHost: {host_port}\r\n\r\n"
            )
            .unwrap();
            let mut resp = String::new();
            conn.read_to_string(&mut resp).unwrap();
            assert!(resp.starts_with("HTTP/1.1 403 "), "got: {resp}");
        }

        // 2. AVPlayer 2-byte initial probe (`Range: bytes=0-1`) returns 206 + exact 2 bytes.
        let valid_path = format!(
            "/{}/{asset_id}/0",
            base.strip_prefix(&format!("http://{host_port}/")).unwrap()
        );
        {
            let mut conn = TcpStream::connect(host_port).unwrap();
            write!(
                conn,
                "GET {valid_path} HTTP/1.1\r\nHost: {host_port}\r\nRange: bytes=0-1\r\n\r\n"
            )
            .unwrap();
            let mut resp = Vec::new();
            conn.read_to_end(&mut resp).unwrap();
            let sep = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
            let headers = std::str::from_utf8(&resp[..sep]).unwrap();
            assert!(headers.starts_with("HTTP/1.1 206 Partial Content"));
            assert!(headers.contains("Content-Range: bytes 0-1/300000"));
            assert_eq!(&resp[sep + 4..], &payload[0..2]);
        }

        // 3. Suffix tail read (`Range: bytes=-64`) for trailing `moov` box returns exact last 64 bytes.
        {
            let mut conn = TcpStream::connect(host_port).unwrap();
            write!(
                conn,
                "GET {valid_path} HTTP/1.1\r\nHost: {host_port}\r\nRange: bytes=-64\r\n\r\n"
            )
            .unwrap();
            let mut resp = Vec::new();
            conn.read_to_end(&mut resp).unwrap();
            let sep = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
            let headers = std::str::from_utf8(&resp[..sep]).unwrap();
            assert!(headers.contains("Content-Range: bytes 299936-299999/300000"));
            assert_eq!(&resp[sep + 4..], &payload[299_936..]);
        }
    }
}
