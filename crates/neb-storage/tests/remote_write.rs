//! Remote publication against a loopback WebDAV/S3 stand-in. Never touches a real NAS.
use neb_storage::{StorageRegistry, WriteCapability};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{Cursor, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Honors If-None-Match and Overwrite: F.
    Strict,
    /// Replaces existing content regardless of preconditions.
    Overwrites,
    /// Fails every GET with 500 to prove single-pass WebDAV upload never re-downloads.
    ForbidGet,
    /// S3 without conditional-write support.
    NoConditional,
}

type Store = Arc<Mutex<HashMap<String, Vec<u8>>>>;

struct Server {
    endpoint: String,
    store: Store,
    methods: Arc<Mutex<Vec<(String, String)>>>,
}

fn hex_sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn serve(kind: &'static str, mode: Mode) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let store: Store = Arc::default();
    let methods: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let shared = store.clone();
    let shared_methods = methods.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let store = shared.clone();
            let methods = shared_methods.clone();
            thread::spawn(move || handle(stream, &store, &methods, kind, mode));
        }
    });
    Server {
        endpoint: format!("http://127.0.0.1:{port}/root/"),
        store,
        methods,
    }
}

fn handle(
    mut stream: TcpStream,
    store: &Store,
    methods: &Mutex<Vec<(String, String)>>,
    kind: &str,
    mode: Mode,
) {
    let mut data = Vec::new();
    let mut buffer = [0u8; 8192];
    let header_end = loop {
        let count = match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => count,
        };
        data.extend_from_slice(&buffer[..count]);
        if let Some(index) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let head = String::from_utf8_lossy(&data[..header_end]).to_string();
    let mut lines = head.lines();
    let mut request_line = lines.next().unwrap().split(' ');
    let method = request_line.next().unwrap().to_owned();
    let path = request_line.next().unwrap().to_owned();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = data[header_end..].to_vec();
    while body.len() < length {
        let count = stream.read(&mut buffer).unwrap_or(0);
        if count == 0 {
            break;
        }
        body.extend_from_slice(&buffer[..count]);
    }
    let key = |path: &str| path.split('?').next().unwrap().to_owned();
    let path = key(&path);
    methods.lock().unwrap().push((method.clone(), path.clone()));
    let mut files = store.lock().unwrap();
    let mut response_etag: Option<String> = None;
    let (status, reply): (&str, Vec<u8>) = match method.as_str() {
        "HEAD" => (
            if files.contains_key(&path) {
                "200 OK"
            } else {
                "404 Not Found"
            },
            vec![],
        ),
        "GET" => match files.get(&path) {
            _ if mode == Mode::ForbidGet => ("500 Internal Server Error", vec![]),
            Some(bytes) => {
                response_etag = Some(format!("\"{}\"", hex_sha(bytes)));
                ("200 OK", bytes.clone())
            }
            None => ("404 Not Found", vec![]),
        },
        "DELETE" => {
            files.remove(&path);
            ("204 No Content", vec![])
        }
        "MKCOL" => ("201 Created", vec![]),
        "PUT" => {
            let conditional = headers.get("if-none-match").map(String::as_str) == Some("*");
            let if_match = headers.get("if-match").cloned();
            if kind == "s3" && mode == Mode::NoConditional && (conditional || if_match.is_some()) {
                (
                    "501 Not Implemented",
                    b"<Error><Code>NotImplemented</Code></Error>".to_vec(),
                )
            } else if kind == "s3" && headers.get("x-amz-content-sha256") != Some(&hex_sha(&body)) {
                (
                    "400 Bad Request",
                    b"<Error><Code>XAmzContentSHA256Mismatch</Code></Error>".to_vec(),
                )
            } else if conditional && mode != Mode::Overwrites && files.contains_key(&path) {
                ("412 Precondition Failed", vec![])
            } else if let Some(expected_etag) = if_match {
                let current_etag = files
                    .get(&path)
                    .map(|existing| format!("\"{}\"", hex_sha(existing)));
                if current_etag.as_deref() != Some(expected_etag.as_str()) {
                    ("412 Precondition Failed", vec![])
                } else {
                    files.insert(path.clone(), body);
                    ("201 Created", vec![])
                }
            } else {
                files.insert(path.clone(), body);
                ("201 Created", vec![])
            }
        }
        "MOVE" => {
            let destination = headers
                .get("destination")
                .map(|url| url.splitn(4, '/').nth(3).map(|p| format!("/{p}")).unwrap())
                .unwrap();
            let no_overwrite = headers.get("overwrite").map(String::as_str) == Some("F");
            if no_overwrite && mode != Mode::Overwrites && files.contains_key(&destination) {
                ("412 Precondition Failed", vec![])
            } else if let Some(bytes) = files.remove(&path) {
                files.insert(destination, bytes);
                ("201 Created", vec![])
            } else {
                ("404 Not Found", vec![])
            }
        }
        _ => ("405 Method Not Allowed", vec![]),
    };
    drop(files);
    let etag_header = response_etag
        .map(|tag| format!("ETag: {tag}\r\n"))
        .unwrap_or_default();
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\n{etag_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
        reply.len()
    );
    let _ = stream.write_all(&reply);
}

fn registry(kind: &str, server: &Server) -> (tempfile::TempDir, StorageRegistry, String) {
    let data = tempfile::tempdir().unwrap();
    let mut registry = StorageRegistry::open(data.path()).unwrap();
    let request = serde_json::from_value(serde_json::json!({
        "name": "NAS", "kind": kind, "endpoint": server.endpoint,
        "bucket": if kind == "s3" { Some("bucket") } else { None },
        "access_key": "key", "secret_key": "secret", "username": "user", "password": "pass",
        "remember_credentials": false
    }))
    .unwrap();
    let id = registry.add(request).unwrap().id;
    (data, registry, id)
}

fn put(writer: &neb_storage::RemoteWriter, key: &str, bytes: &'static [u8]) -> anyhow::Result<()> {
    writer.put_new(key, Cursor::new(bytes), bytes.len() as u64, &hex_sha(bytes))
}

fn keys(server: &Server) -> Vec<String> {
    let mut keys: Vec<_> = server.store.lock().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

#[test]
fn webdav_publishes_once_verifies_and_never_overwrites() {
    let server = serve("webdav", Mode::Strict);
    let (_data, registry, id) = registry("webdav", &server);
    assert_eq!(registry.write_capability(&id), WriteCapability::Unverified);
    let writer = registry.open_writer(&id).unwrap();
    put(&writer, "2026/2026-10-22/a.NEF", b"raw bytes").unwrap();
    assert_eq!(keys(&server), ["/root/2026/2026-10-22/a.NEF"]);
    let conflict = put(&writer, "2026/2026-10-22/a.NEF", b"different").unwrap_err();
    assert!(format!("{conflict:#}").contains("E_CONFLICT"));
    assert_eq!(
        server.store.lock().unwrap()["/root/2026/2026-10-22/a.NEF"],
        b"raw bytes"
    );
    // No temporary .part files remain after success or conflict.
    assert_eq!(keys(&server).len(), 1);
    assert!(writer.exists("2026/2026-10-22/a.NEF").unwrap());
    assert!(!writer.exists("2026/2026-10-22/b.NEF").unwrap());
    // Wrong planned hash: rejected before anything is published.
    let wrong = writer
        .put_new("c.jpg", Cursor::new(b"abc"), 3, &"0".repeat(64))
        .unwrap_err();
    assert!(format!("{wrong:#}").contains("E_VERIFY"));
    assert_eq!(keys(&server).len(), 1);
    assert!(put(&writer, "../escape.jpg", b"x").is_err());
    let check = writer.probe();
    assert_eq!(check.write_capability, "verified", "{:?}", check.error);
    assert!(check.leftover.is_none());
    assert_eq!(registry.write_capability(&id), WriteCapability::Verified);
    assert_eq!(keys(&server).len(), 1, "probe object must be removed");
}

#[test]
fn webdav_that_ignores_overwrite_protection_is_refused() {
    let server = serve("webdav", Mode::Overwrites);
    let (_data, registry, id) = registry("webdav", &server);
    let check = registry.open_writer(&id).unwrap().probe();
    assert_eq!(check.write_capability, "unsupported");
    assert!(check.error.unwrap().contains("E_UNSUPPORTED"));
    assert_eq!(registry.write_capability(&id), WriteCapability::Unsupported);
}

#[test]
fn webdav_streaming_upload_never_redownloads_body_and_caches_mkcol_across_threads() {
    let server = serve("webdav", Mode::ForbidGet);
    let (_data, registry, id) = registry("webdav", &server);
    let writer = registry.open_writer(&id).unwrap();
    thread::scope(|scope| {
        for idx in 0..8 {
            let writer = &writer;
            scope.spawn(move || {
                let payload = format!("frame-{idx}").into_bytes();
                let expected = hex_sha(&payload);
                let computed = writer
                    .put_new_streaming(
                        &format!("2026/2026-11-15/IMG_{idx:04}.JPG"),
                        Cursor::new(payload.clone()),
                        payload.len() as u64,
                        None,
                    )
                    .unwrap();
                assert_eq!(computed, expected);
            });
        }
    });
    assert_eq!(keys(&server).len(), 8);
    let logged = server.methods.lock().unwrap().clone();
    assert!(
        logged.iter().all(|(method, _)| method != "GET"),
        "WebDAV upload must never issue GET read-backs: {logged:?}"
    );
    let mkcols: Vec<_> = logged
        .iter()
        .filter(|(method, _)| method == "MKCOL")
        .collect();
    assert_eq!(
        mkcols.len(),
        2,
        "MKCOL must be cached once per unique folder prefix across concurrent workers: {mkcols:?}"
    );
}

#[test]
fn s3_conditional_put_is_hash_bound_and_unsupported_services_are_refused() {
    let server = serve("s3", Mode::Strict);
    let (_data, strict, id) = registry("s3", &server);
    let writer = strict.open_writer(&id).unwrap();
    put(&writer, "trips/a.NEF", b"raw").unwrap();
    assert_eq!(keys(&server), ["/root/bucket/trips/a.NEF"]);
    let conflict = put(&writer, "trips/a.NEF", b"new").unwrap_err();
    assert!(format!("{conflict:#}").contains("E_CONFLICT"));
    let mismatch = writer
        .put_new("trips/b.NEF", Cursor::new(b"abc"), 3, &hex_sha(b"xyz"))
        .unwrap_err();
    assert!(format!("{mismatch:#}").contains("E_VERIFY"));
    assert_eq!(writer.probe().write_capability, "verified");

    let legacy = serve("s3", Mode::NoConditional);
    let (_legacy_data, old, id) = registry("s3", &legacy);
    let check = old.open_writer(&id).unwrap().probe();
    assert_eq!(check.write_capability, "unsupported");
    assert!(keys(&legacy).is_empty());
}

#[test]
fn local_locations_do_not_open_remote_writers() {
    let data = tempfile::tempdir().unwrap();
    let folder = tempfile::tempdir().unwrap();
    let mut registry = StorageRegistry::open(data.path()).unwrap();
    let request = serde_json::from_value(serde_json::json!({
        "name": "Local", "kind": "local", "endpoint": folder.path(), "remember_credentials": false
    }))
    .unwrap();
    let id = registry.add(request).unwrap().id;
    assert!(registry.open_writer(&id).is_err());
}

#[test]
fn remote_sidecar_read_and_write_enforce_xmp_safety_on_webdav_and_s3() {
    for kind in ["webdav", "s3"] {
        let server = serve(kind, Mode::Strict);
        let (_data, registry, id) = registry(kind, &server);
        let writer = registry.open_writer(&id).unwrap();
        let browser = registry.open_browser(&id).unwrap();

        // Non-.xmp keys are always refused so originals can never be touched.
        assert!(writer.read_sidecar("album/a.NEF", 1024).is_err());
        assert!(writer
            .write_sidecar("album/a.NEF", b"x", None, true)
            .is_err());
        assert!(browser.read_sidecar("album/a.NEF", 1024).is_err());

        assert!(writer
            .read_sidecar("album/a.xmp", 2 * 1024 * 1024)
            .unwrap()
            .is_none());

        writer
            .write_sidecar("album/a.xmp", b"<xmp>v1</xmp>", None, true)
            .unwrap();
        // create_only refuses concurrent creation.
        let conflict = writer
            .write_sidecar("album/a.xmp", b"<xmp>race</xmp>", None, true)
            .unwrap_err();
        assert!(format!("{conflict:#}").contains("E_CONFLICT"));

        let loaded = writer
            .read_sidecar("album/a.xmp", 2 * 1024 * 1024)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.bytes, b"<xmp>v1</xmp>");
        assert_eq!(
            browser
                .read_sidecar("album/a.xmp", 2 * 1024 * 1024)
                .unwrap()
                .as_deref(),
            Some(b"<xmp>v1</xmp>".as_slice())
        );

        // Update existing sidecar with matching ETag succeeds.
        writer
            .write_sidecar(
                "album/a.xmp",
                b"<xmp>v2</xmp>",
                loaded.etag.as_deref(),
                false,
            )
            .unwrap();
        assert_eq!(
            writer
                .read_sidecar("album/a.xmp", 2 * 1024 * 1024)
                .unwrap()
                .unwrap()
                .bytes,
            b"<xmp>v2</xmp>"
        );

        // Stale ETag on S3 produces E_CONFLICT rather than silently clobbering.
        if kind == "s3" {
            let stale = writer
                .write_sidecar("album/a.xmp", b"<xmp>stale</xmp>", Some("\"bad\""), false)
                .unwrap_err();
            assert!(format!("{stale:#}").contains("E_CONFLICT"));
        }
    }

    // Legacy S3 without conditional headers falls back gracefully on existing sidecar updates.
    let legacy = serve("s3", Mode::NoConditional);
    let (_data, registry, id) = registry("s3", &legacy);
    let writer = registry.open_writer(&id).unwrap();
    legacy
        .store
        .lock()
        .unwrap()
        .insert("/root/bucket/a.xmp".into(), b"<xmp>old</xmp>".to_vec());
    let loaded = writer
        .read_sidecar("a.xmp", 2 * 1024 * 1024)
        .unwrap()
        .unwrap();
    writer
        .write_sidecar("a.xmp", b"<xmp>new</xmp>", loaded.etag.as_deref(), false)
        .unwrap();
    assert_eq!(
        legacy.store.lock().unwrap()["/root/bucket/a.xmp"],
        b"<xmp>new</xmp>"
    );
}
