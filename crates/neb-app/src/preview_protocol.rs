//! `neb://localhost/<id>/<tier>/<crop>/<rev>` and `neb://localhost/media/<id>/<rev>`
//! (`http://neb.localhost/...` on Windows): preview JPEG bytes and byte-range video streams.
use neb_core::PreviewTier;
use neb_service::{ByteRangeSpec, CommandError, Service};
use percent_encoding::percent_decode_str;
use std::sync::Arc;
use tauri::http::{header, Method, Request, Response, StatusCode};
use tauri::Manager;

const MAX_ID_LEN: usize = 64;
const APP_ORIGINS: [&str; 2] = ["tauri://localhost", "http://tauri.localhost"];

#[derive(Debug, PartialEq, Eq)]
pub struct PreviewRequest {
    pub id: String,
    pub tier: PreviewTier,
    pub uncropped: bool,
    pub prefer_raw: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct MediaRequest {
    pub id: String,
}

#[derive(Debug, PartialEq, Eq)]
enum ProtocolRoute {
    Preview(PreviewRequest),
    Media(MediaRequest),
}

fn valid_asset_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn valid_revision(rev: &str) -> bool {
    !rev.is_empty() && rev.bytes().all(|b| b.is_ascii_digit())
}

fn parse_route(path: &str) -> Result<ProtocolRoute, &'static str> {
    let decoded = percent_decode_str(path)
        .decode_utf8()
        .map_err(|_| "Preview path is not UTF-8")?;
    let rest = decoded
        .strip_prefix('/')
        .ok_or("Preview path must start with /")?;
    let parts: Vec<&str> = rest.split('/').collect();
    if let ["media", id, rev] = parts.as_slice() {
        if !valid_asset_id(id) {
            return Err("Invalid asset id");
        }
        if !valid_revision(rev) {
            return Err("Revision must be digits");
        }
        return Ok(ProtocolRoute::Media(MediaRequest { id: (*id).to_owned() }));
    }
    parse_request(path).map(ProtocolRoute::Preview)
}

/// Accepts `<id>/<tier>/<crop>/<rev>` (or `<id>/<tier>/<crop>/<rev>/raw`), either
/// raw or percent-encoded as one segment (`convertFileSrc` encodes `/` as `%2F`).
pub fn parse_request(path: &str) -> Result<PreviewRequest, &'static str> {
    let decoded = percent_decode_str(path)
        .decode_utf8()
        .map_err(|_| "Preview path is not UTF-8")?;
    let rest = decoded
        .strip_prefix('/')
        .ok_or("Preview path must start with /")?;
    let parts: Vec<&str> = rest.split('/').collect();
    let (id, tier, crop, rev, prefer_raw) = match parts.as_slice() {
        [id, tier, crop, rev] => (*id, *tier, *crop, *rev, false),
        [id, tier, crop, rev, "raw"] => (*id, *tier, *crop, *rev, true),
        _ => return Err("Preview path must be <id>/<tier>/<crop>/<rev>[/raw]"),
    };
    if !valid_asset_id(id) {
        return Err("Invalid asset id");
    }
    let tier = PreviewTier::parse(tier).ok_or("Unknown preview tier")?;
    let uncropped = match crop {
        "0" => false,
        "1" => true,
        _ => return Err("Crop flag must be 0 or 1"),
    };
    if !valid_revision(rev) {
        return Err("Revision must be digits");
    }
    Ok(PreviewRequest {
        id: id.to_owned(),
        tier,
        uncropped,
        prefer_raw,
    })
}

pub fn parse_range_header(raw: Option<&str>) -> Result<Option<ByteRangeSpec>, &'static str> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let Some(spec) = raw.strip_prefix("bytes=") else {
        return Ok(None);
    };
    let first = spec.split(',').next().unwrap_or("").trim();
    let (left, right) = first.split_once('-').ok_or("Malformed byte range")?;
    let left = left.trim();
    let right = right.trim();
    if left.is_empty() {
        let suffix: u64 = right.parse().map_err(|_| "Invalid suffix byte range")?;
        if suffix == 0 {
            return Err("Zero suffix byte range");
        }
        return Ok(Some(ByteRangeSpec::Suffix(suffix)));
    }
    let start: u64 = left.parse().map_err(|_| "Invalid byte range start")?;
    if right.is_empty() {
        return Ok(Some(ByteRangeSpec::FromTo(start, None)));
    }
    let end: u64 = right.parse().map_err(|_| "Invalid byte range end")?;
    if end < start {
        return Err("Byte range end precedes start");
    }
    Ok(Some(ByteRangeSpec::FromTo(start, Some(end))))
}

pub fn status_for(code: &str) -> StatusCode {
    match code {
        "E_NOT_FOUND" => StatusCode::NOT_FOUND,
        "E_DOWNLOAD_REQUIRED" => StatusCode::CONFLICT,
        "E_UNSUPPORTED_CODEC" => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "E_RANGE_UNSATISFIABLE" => StatusCode::RANGE_NOT_SATISFIABLE,
        "E_CACHE_BUSY" | "E_WORKER" | "E_OFFLINE" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn error_response(status: StatusCode, code: &str, message: &str) -> Response<Vec<u8>> {
    let body = serde_json::to_vec(&serde_json::json!({"code": code, "message": message}))
        .unwrap_or_default();
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .body(body)
        .expect("static error response headers are valid")
}

fn command_error(error: &CommandError) -> Response<Vec<u8>> {
    error_response(status_for(error.code), error.code, &error.message)
}

fn etag_matches(if_none_match: Option<&str>, etag: &str) -> bool {
    if_none_match.is_some_and(|value| {
        value
            .split(',')
            .map(str::trim)
            .any(|candidate| candidate == etag || candidate == "*")
    })
}

/// Checks the ETag first so a revalidation never decodes pixels.
pub fn respond(
    request: &PreviewRequest,
    if_none_match: Option<&str>,
    service: &Service,
) -> Response<Vec<u8>> {
    let etag = match service.preview_etag_with_mode(
        &request.id,
        request.tier,
        request.uncropped,
        request.prefer_raw,
    ) {
        Ok(etag) => etag,
        Err(error) => return command_error(&error),
    };
    if etag_matches(if_none_match, &etag) {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, &etag)
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Vec::new())
            .unwrap_or_else(|_| {
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "E_OPERATION",
                    "Invalid ETag",
                )
            });
    }
    let image = match service.preview_image_with_mode(
        &request.id,
        request.tier,
        request.uncropped,
        request.prefer_raw,
    ) {
        Ok(image) => image,
        Err(error) => return command_error(&error),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/jpeg")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::ETAG, &etag)
        .header("X-Neb-Width", image.width)
        .header("X-Neb-Height", image.height)
        .header("X-Neb-Source", image.source.as_str())
        .body(image.jpeg.clone())
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "E_OPERATION",
                "Invalid preview response headers",
            )
        })
}

fn range_not_satisfiable(total: &str) -> Response<Vec<u8>> {
    let total = if total.is_empty() || !total.bytes().all(|b| b.is_ascii_digit()) {
        "*"
    } else {
        total
    };
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_RANGE, format!("bytes */{total}"))
        .header(header::CACHE_CONTROL, "no-store")
        .body(Vec::new())
        .expect("static 416 headers are valid")
}

pub fn respond_media(
    media: &MediaRequest,
    method: &Method,
    range_header: Option<&str>,
    service: &Service,
) -> Response<Vec<u8>> {
    if *method == Method::OPTIONS {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, HEAD, OPTIONS")
            .header(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                "Range, If-None-Match",
            )
            .header(
                header::ACCESS_CONTROL_EXPOSE_HEADERS,
                "Accept-Ranges, Content-Range, Content-Length",
            )
            .body(Vec::new())
            .expect("static preflight headers are valid");
    }
    if *method != Method::GET && *method != Method::HEAD {
        return error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "E_INVALID",
            "Only GET, HEAD, and OPTIONS are supported",
        );
    }
    let head_only = *method == Method::HEAD;
    let range = match parse_range_header(range_header) {
        Ok(range) => range,
        Err(_) => {
            let total = service
                .read_media(&media.id, None, true)
                .map(|c| c.total_len.to_string())
                .unwrap_or_else(|_| "*".into());
            return range_not_satisfiable(&total);
        }
    };
    let chunk = match service.read_media(&media.id, range, head_only) {
        Ok(chunk) => chunk,
        Err(error) if error.code == "E_RANGE_UNSATISFIABLE" => {
            return range_not_satisfiable(&error.message);
        }
        Err(error) => return command_error(&error),
    };
    let status = if chunk.partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let content_length = if chunk.partial {
        if chunk.total_len == 0 {
            0
        } else {
            chunk.end.saturating_sub(chunk.start) + 1
        }
    } else if head_only {
        chunk.total_len
    } else {
        chunk.bytes.len() as u64
    };
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, chunk.content_type)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, content_length.to_string())
        .header(header::CACHE_CONTROL, "no-cache")
        .header(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            "Accept-Ranges, Content-Range, Content-Length",
        );
    if chunk.partial {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", chunk.start, chunk.end, chunk.total_len),
        );
    }
    builder.body(chunk.bytes).unwrap_or_else(|_| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_OPERATION",
            "Invalid media response headers",
        )
    })
}

fn allowed_origin(origin: Option<&str>) -> Option<&str> {
    origin.filter(|origin| {
        APP_ORIGINS.contains(origin)
            || (cfg!(debug_assertions) && *origin == "http://localhost:5173")
    })
}

fn handle(request: &Request<Vec<u8>>, service: Option<&Service>) -> Response<Vec<u8>> {
    let mut response = match parse_route(request.uri().path()) {
        Err(message) => error_response(StatusCode::BAD_REQUEST, "E_INVALID", message),
        Ok(_) if service.is_none() => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "E_WORKER",
            "Service is still starting",
        ),
        Ok(ProtocolRoute::Preview(preview)) => {
            let if_none_match = request
                .headers()
                .get(header::IF_NONE_MATCH)
                .and_then(|value| value.to_str().ok());
            respond(&preview, if_none_match, service.expect("checked above"))
        }
        Ok(ProtocolRoute::Media(media)) => {
            let range = request
                .headers()
                .get(header::RANGE)
                .and_then(|value| value.to_str().ok());
            respond_media(
                &media,
                request.method(),
                range,
                service.expect("checked above"),
            )
        }
    };
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    if let Some(origin) = allowed_origin(origin).and_then(|o| o.parse().ok()) {
        let headers = response.headers_mut();
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        headers.insert(header::VARY, header::HeaderValue::from_static("Origin"));
    }
    response
}

pub fn register<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder.register_asynchronous_uri_scheme_protocol("neb", |ctx, request, responder| {
        // `Service` is managed in setup; a request before that gets 503.
        let service = ctx
            .app_handle()
            .try_state::<Arc<Service>>()
            .map(|state| Arc::clone(state.inner()));
        tauri::async_runtime::spawn(async move {
            let response =
                tauri::async_runtime::spawn_blocking(move || handle(&request, service.as_deref()))
                    .await
                    .unwrap_or_else(|_| {
                        error_response(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "E_WORKER",
                            "Preview worker stopped unexpectedly",
                        )
                    });
            responder.respond(response);
        });
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::fs;

    fn ppm(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = format!("P6\n{width} {height}\n255\n").into_bytes();
        for y in 0..height {
            for x in 0..width {
                bytes.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, 90]);
            }
        }
        bytes
    }

    fn error_code(response: &Response<Vec<u8>>) -> String {
        let body: Value = serde_json::from_slice(response.body()).unwrap();
        body["code"].as_str().unwrap().to_owned()
    }

    #[test]
    fn parses_valid_paths() {
        let expected = PreviewRequest {
            id: "0f8c-ab".into(),
            tier: PreviewTier::T512,
            uncropped: false,
            prefer_raw: false,
        };
        assert_eq!(parse_request("/0f8c-ab/t512/0/17"), Ok(expected));
        let expected = PreviewRequest {
            id: "0f8c-ab".into(),
            tier: PreviewTier::T512,
            uncropped: false,
            prefer_raw: false,
        };
        assert_eq!(parse_request("/0f8c-ab%2Ft512%2F0%2F17"), Ok(expected));
        assert_eq!(
            parse_request("/x/p2560/1/0"),
            Ok(PreviewRequest {
                id: "x".into(),
                tier: PreviewTier::P2560,
                uncropped: true,
                prefer_raw: false,
            })
        );
        assert_eq!(
            parse_request("/x/full/0/2/raw"),
            Ok(PreviewRequest {
                id: "x".into(),
                tier: PreviewTier::Full,
                uncropped: false,
                prefer_raw: true,
            })
        );
    }

    #[test]
    fn rejects_malformed_paths() {
        let long = format!("/{}/t512/0/1", "a".repeat(65));
        for path in [
            "/../etc/t512/0/1",
            "/..%2Fetc%2Ft512%2F0%2F1",
            "/a/t9999/0/1",
            "/a/t512/2/1",
            "/a/t512/0",
            "/a/t512/0/1/extra",
            "/a b/t512/0/1",
            "/a%20b/t512/0/1",
            "/a/t512/0/x",
            "/a/t512/0/",
            "a/t512/0/1",
            "/%FF/t512/0/1",
            long.as_str(),
        ] {
            assert!(parse_request(path).is_err(), "{path} must be rejected");
        }
        assert!(parse_request(&format!("/{}/t512/0/1", "a".repeat(64))).is_ok());
    }

    #[test]
    fn status_mapping_follows_error_codes() {
        for (code, status) in [
            ("E_NOT_FOUND", 404),
            ("E_DOWNLOAD_REQUIRED", 409),
            ("E_UNSUPPORTED_CODEC", 415),
            ("E_CACHE_BUSY", 503),
            ("E_WORKER", 503),
            ("E_OFFLINE", 503),
            ("E_INVALID", 500),
            ("E_OPERATION", 500),
        ] {
            assert_eq!(status_for(code).as_u16(), status, "{code}");
        }
    }

    #[test]
    fn respond_maps_errors_and_not_modified() {
        let data = tempfile::tempdir().unwrap();
        let photos = tempfile::tempdir().unwrap();
        let source = photos.path().join("source.ppm");
        fs::write(&source, ppm(600, 300)).unwrap();
        let service = Service::open(data.path()).unwrap();
        let opened = service
            .execute("file.open", json!({"path":source}))
            .unwrap();
        let jpeg = service
            .preview_image(opened["id"].as_str().unwrap(), PreviewTier::T1024, false)
            .unwrap()
            .jpeg
            .clone();
        let photo = photos.path().join("photo.jpg");
        fs::write(&photo, jpeg).unwrap();
        let asset = service.execute("file.open", json!({"path":photo})).unwrap();
        let request = PreviewRequest {
            id: asset["id"].as_str().unwrap().into(),
            tier: PreviewTier::T512,
            uncropped: false,
            prefer_raw: false,
        };

        let ok = respond(&request, None, &service);
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(ok.headers()[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(ok.headers()[header::CACHE_CONTROL], "no-cache");
        assert_eq!(ok.headers()["X-Neb-Width"], "512");
        assert_eq!(ok.headers()["X-Neb-Height"], "256");
        assert!(ok.headers().contains_key("X-Neb-Source"));
        assert!(ok.body().starts_with(&[0xFF, 0xD8]));
        let etag = ok.headers()[header::ETAG].to_str().unwrap().to_owned();
        assert!(etag.starts_with('"') && etag.ends_with('"'));

        let cached = respond(&request, Some(&etag), &service);
        assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(cached.headers()[header::ETAG], etag.as_str());
        assert!(cached.body().is_empty());

        let stale = respond(&request, Some("\"other\""), &service);
        assert_eq!(stale.status(), StatusCode::OK);

        let missing = PreviewRequest {
            id: "0000-missing".into(),
            tier: PreviewTier::T512,
            uncropped: false,
            prefer_raw: false,
        };
        let missing = respond(&missing, None, &service);
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(missing.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(error_code(&missing), "E_NOT_FOUND");
    }

    #[test]
    fn handle_rejects_malformed_paths_with_json_400() {
        for path in ["/a/t9999/0/1", "/%FF/t512/0/1"] {
            let request = Request::builder()
                .uri(format!("neb://localhost{path}"))
                .header(header::ORIGIN, "tauri://localhost")
                .body(Vec::new())
                .unwrap();
            // No service: a malformed path must be rejected before any lookup.
            let response = handle(&request, None);
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
            assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
            assert_eq!(error_code(&response), "E_INVALID", "{path}");
            assert_eq!(
                response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
                "tauri://localhost"
            );
        }
    }

    #[test]
    fn cors_allows_only_app_origins() {
        assert_eq!(
            allowed_origin(Some("tauri://localhost")),
            Some("tauri://localhost")
        );
        assert_eq!(
            allowed_origin(Some("http://tauri.localhost")),
            Some("http://tauri.localhost")
        );
        assert_eq!(allowed_origin(Some("https://example.com")), None);
        assert_eq!(allowed_origin(None), None);
    }

    #[test]
    fn media_route_serves_byte_ranges_head_and_416() {
        let data = tempfile::tempdir().unwrap();
        let videos = tempfile::tempdir().unwrap();
        let clip_path = videos.path().join("clip.mp4");
        let mut clip_bytes = Vec::new();
        clip_bytes.extend_from_slice(&20u32.to_be_bytes());
        clip_bytes.extend_from_slice(b"ftypisom\x00\x00\x02\x00isom");
        clip_bytes.extend((0u8..200).collect::<Vec<u8>>());
        let expected_len = clip_bytes.len();
        fs::write(&clip_path, &clip_bytes).unwrap();

        let service = Service::open(data.path()).unwrap();
        let lib = service
            .execute(
                "library.add",
                json!({"name": "Clips", "root": videos.path()}),
            )
            .unwrap();
        service
            .execute("library.scan", json!({"library_id": lib["id"]}))
            .unwrap();
        let listed = service
            .execute("asset.list", json!({"library_id": lib["id"], "format": "video"}))
            .unwrap();
        let id = listed["items"][0]["id"].as_str().unwrap();

        let probe_req = Request::builder()
            .method(Method::GET)
            .uri(format!("neb://localhost/media%2F{id}%2F0"))
            .header(header::RANGE, "bytes=0-1")
            .header(header::ORIGIN, "tauri://localhost")
            .body(Vec::new())
            .unwrap();
        let probe = handle(&probe_req, Some(&service));
        assert_eq!(probe.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(probe.headers()[header::CONTENT_TYPE], "video/mp4");
        assert_eq!(probe.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(
            probe.headers()[header::CONTENT_RANGE],
            format!("bytes 0-1/{expected_len}")
        );
        assert_eq!(probe.body(), &clip_bytes[0..2]);

        let suffix_req = Request::builder()
            .method(Method::GET)
            .uri(format!("neb://localhost/media/{id}/1"))
            .header(header::RANGE, "bytes=-10")
            .body(Vec::new())
            .unwrap();
        let suffix = handle(&suffix_req, Some(&service));
        assert_eq!(suffix.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            suffix.headers()[header::CONTENT_RANGE],
            format!("bytes {}-{}/{expected_len}", expected_len - 10, expected_len - 1)
        );
        assert_eq!(suffix.body(), &clip_bytes[expected_len - 10..]);

        let head_req = Request::builder()
            .method(Method::HEAD)
            .uri(format!("neb://localhost/media/{id}/1"))
            .body(Vec::new())
            .unwrap();
        let head = handle(&head_req, Some(&service));
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(
            head.headers()[header::CONTENT_LENGTH],
            expected_len.to_string()
        );
        assert!(head.body().is_empty());

        let oob_req = Request::builder()
            .method(Method::GET)
            .uri(format!("neb://localhost/media/{id}/1"))
            .header(header::RANGE, format!("bytes={}-", expected_len + 50))
            .body(Vec::new())
            .unwrap();
        let oob = handle(&oob_req, Some(&service));
        assert_eq!(oob.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            oob.headers()[header::CONTENT_RANGE],
            format!("bytes */{expected_len}")
        );
    }
}
