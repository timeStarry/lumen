//! End-to-end remote `.xmp` sidecar sync tests over loopback WebDAV and S3 servers.
use neb_core::xmp;
use neb_service::Service;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

type Store = Arc<Mutex<HashMap<String, Vec<u8>>>>;

struct MockServer {
    endpoint: String,
    store: Store,
    read_only: Arc<AtomicBool>,
}

fn hex_sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn serve(kind: &'static str) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let store: Store = Arc::default();
    let read_only = Arc::new(AtomicBool::new(false));
    let shared_store = store.clone();
    let shared_ro = read_only.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let store = shared_store.clone();
            let ro = shared_ro.clone();
            thread::spawn(move || handle(stream, &store, &ro, kind));
        }
    });
    MockServer {
        endpoint: format!("http://127.0.0.1:{port}"),
        store,
        read_only,
    }
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn handle(mut stream: TcpStream, store: &Store, read_only: &AtomicBool, kind: &str) {
    let mut data = Vec::new();
    let mut buffer = [0u8; 8192];
    let header_end = loop {
        let count = match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        data.extend_from_slice(&buffer[..count]);
        if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&data[..header_end]).to_string();
    let mut lines = head.lines();
    let mut first = lines.next().unwrap().split(' ');
    let method = first.next().unwrap().to_owned();
    let raw_target = first.next().unwrap().to_owned();
    let (path, query) = match raw_target.split_once('?') {
        Some((p, q)) => (p.to_owned(), q.to_owned()),
        None => (raw_target, String::new()),
    };
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
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

    let files = store.lock().unwrap();
    let mut etag_out: Option<String> = None;
    let mut content_type = "application/octet-stream";

    let (status, reply): (&str, Vec<u8>) = match method.as_str() {
        "PROPFIND" => {
            content_type = "application/xml";
            let prefix = path.trim_end_matches('/');
            let mut responses = String::new();
            let mut keys: Vec<_> = files.keys().cloned().collect();
            keys.sort();
            for key in keys {
                let rel = if prefix.is_empty() {
                    key.trim_start_matches('/')
                } else if let Some(rest) = key.strip_prefix(&format!("{prefix}/")) {
                    rest
                } else {
                    continue;
                };
                if rel.is_empty() || rel.contains('/') {
                    continue;
                }
                let size = files[&key].len();
                responses.push_str(&format!(
                    "<d:response><d:href>{}</d:href><d:propstat><d:prop><d:getcontentlength>{size}</d:getcontentlength><d:resourcetype/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                    xml_escape(&key)
                ));
            }
            let xml = format!("<d:multistatus xmlns:d=\"DAV:\">{responses}</d:multistatus>");
            ("207 Multi-Status", xml.into_bytes())
        }
        "GET" if kind == "s3" && query.contains("list-type=2") => {
            content_type = "application/xml";
            let bucket_prefix = "/bucket/";
            let list_prefix = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("prefix="))
                .unwrap_or("")
                .replace("%2F", "/")
                .replace("%2f", "/");
            let mut contents = String::new();
            let mut keys: Vec<_> = files.keys().cloned().collect();
            keys.sort();
            for key in keys {
                let Some(obj_key) = key.strip_prefix(bucket_prefix) else {
                    continue;
                };
                if !list_prefix.is_empty() && !obj_key.starts_with(&list_prefix) {
                    continue;
                }
                let size = files[&key].len();
                contents.push_str(&format!(
                    "<Contents><Key>{}</Key><Size>{size}</Size></Contents>",
                    xml_escape(obj_key)
                ));
            }
            let xml = format!(
                "<ListBucketResult><IsTruncated>false</IsTruncated>{contents}</ListBucketResult>"
            );
            ("200 OK", xml.into_bytes())
        }
        "HEAD" => (
            if files.contains_key(&path) {
                "200 OK"
            } else {
                "404 Not Found"
            },
            vec![],
        ),
        "GET" => match files.get(&path) {
            Some(bytes) => {
                etag_out = Some(format!("\"{}\"", hex_sha(bytes)));
                ("200 OK", bytes.clone())
            }
            None => ("404 Not Found", vec![]),
        },
        _ if read_only.load(Ordering::Relaxed) => ("403 Forbidden", vec![]),
        "MKCOL" => ("201 Created", vec![]),
        "DELETE" => {
            drop(files);
            store.lock().unwrap().remove(&path);
            ("204 No Content", vec![])
        }
        "PUT" => {
            drop(files);
            let mut files = store.lock().unwrap();
            let if_none_match = headers.get("if-none-match").map(String::as_str) == Some("*");
            let if_match = headers.get("if-match").cloned();
            if kind == "s3" && headers.get("x-amz-content-sha256") != Some(&hex_sha(&body)) {
                (
                    "400 Bad Request",
                    b"<Error><Code>XAmzContentSHA256Mismatch</Code></Error>".to_vec(),
                )
            } else if if_none_match && files.contains_key(&path) {
                ("412 Precondition Failed", vec![])
            } else if let Some(expected) = if_match {
                let current = files.get(&path).map(|b| format!("\"{}\"", hex_sha(b)));
                if current.as_deref() != Some(expected.as_str()) {
                    ("412 Precondition Failed", vec![])
                } else {
                    files.insert(path, body);
                    ("201 Created", vec![])
                }
            } else {
                files.insert(path, body);
                ("201 Created", vec![])
            }
        }
        "MOVE" => {
            drop(files);
            let mut files = store.lock().unwrap();
            let destination = headers["destination"]
                .splitn(4, '/')
                .nth(3)
                .map(|p| format!("/{p}"))
                .unwrap();
            let no_overwrite = headers.get("overwrite").map(String::as_str) == Some("F");
            if no_overwrite && files.contains_key(&destination) {
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

    let etag_header = etag_out
        .map(|t| format!("ETag: {t}\r\n"))
        .unwrap_or_default();
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\n{etag_header}Content-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.len()
    );
    let _ = stream.write_all(&reply);
}

#[test]
fn remote_webdav_and_s3_libraries_sync_xmp_on_scan_auto_write_and_manual_commands() {
    for kind in ["webdav", "s3"] {
        let server = serve(kind);
        let prefix_root = if kind == "s3" { "/bucket" } else { "" };
        let key_with_xmp = format!("{prefix_root}/album/DSC_0100.NEF");
        let key_sidecar = format!("{prefix_root}/album/DSC_0100.xmp");
        let key_plain = format!("{prefix_root}/album/DSC_0200.NEF");
        let key_plain_sidecar = format!("{prefix_root}/album/DSC_0200.xmp");
        let key_untouched = format!("{prefix_root}/album/DSC_0300.NEF");
        let key_untouched_sidecar = format!("{prefix_root}/album/DSC_0300.xmp");

        let adobe_xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="Adobe XMP Core 7.0-c000">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
    xmp:Rating="3"
    xmp:Label="Red"
    crs:Exposure2012="+0.55"
    crs:Contrast2012="+12"/>
 </rdf:RDF>
</x:xmpmeta>"#;

        {
            let mut store = server.store.lock().unwrap();
            store.insert(key_with_xmp.clone(), b"raw-100".to_vec());
            store.insert(key_sidecar.clone(), adobe_xmp.as_bytes().to_vec());
            store.insert(key_plain.clone(), b"raw-200".to_vec());
            store.insert(key_untouched.clone(), b"raw-300".to_vec());
        }

        let data = tempfile::tempdir().unwrap();
        let svc = Service::open(data.path()).unwrap();
        let source = svc
            .execute(
                "storage.add",
                json!({
                    "name": format!("Remote {kind}"),
                    "kind": kind,
                    "endpoint": server.endpoint,
                    "bucket": if kind == "s3" { Some("bucket") } else { None },
                    "access_key": "ak",
                    "secret_key": "sk",
                    "username": "u",
                    "password": "p",
                    "remember_credentials": false
                }),
            )
            .unwrap();
        let library = svc
            .execute(
                "library.add",
                json!({"name": "NAS", "storage_id": source["id"], "prefix": "album"}),
            )
            .unwrap();
        let lib_id = library["id"].as_str().unwrap();

        // 1. Remote scan imports existing `.xmp` marks onto newly discovered remote assets.
        let scan = svc
            .execute("library.scan", json!({"library_id": lib_id}))
            .unwrap();
        assert_eq!(scan["indexed"], 3, "{scan}");

        let list = svc
            .execute("asset.list", json!({"library_id": lib_id}))
            .unwrap();
        let items = list["items"].as_array().unwrap();
        let find_id = |name: &str| {
            items.iter().find(|a| a["filename"] == name).unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        let id_100 = find_id("DSC_0100.NEF");
        let id_200 = find_id("DSC_0200.NEF");
        let _id_300 = find_id("DSC_0300.NEF");

        let a100 = svc.execute("asset.get", json!({"id": id_100})).unwrap();
        assert_eq!(a100["rating"], 3);
        assert_eq!(a100["color_label"], "red");

        // 2. Automatic `.xmp` sync on rating, flag, crop, keywords, and batch mark.
        svc.execute("asset.rate", json!({"id": id_100, "rating": 5}))
            .unwrap();
        svc.execute("asset.flag", json!({"id": id_100, "flag": "pick"}))
            .unwrap();
        svc.execute(
            "asset.crop",
            json!({"id": id_100, "x": 0.1, "y": 0.2, "width": 0.5, "height": 0.6}),
        )
        .unwrap();
        svc.execute(
            "asset.keywords.set",
            json!({"ids": [id_100], "add": ["人像 > 客片"]}),
        )
        .unwrap();
        svc.execute(
            "asset.mark_batch",
            json!({"ids": [id_200], "rating": 4, "flag": "reject", "color_label": "blue"}),
        )
        .unwrap();

        assert!(svc.flush_remote_xmp(Duration::from_secs(10)));

        // Verify original NEFs were never modified and sidecars were updated/created properly.
        {
            let store = server.store.lock().unwrap();
            assert_eq!(store[&key_with_xmp], b"raw-100");
            assert_eq!(store[&key_plain], b"raw-200");
            assert_eq!(store[&key_untouched], b"raw-300");
            assert!(!store.contains_key(&key_untouched_sidecar));

            let xml_100 = std::str::from_utf8(&store[&key_sidecar]).unwrap();
            assert!(
                xml_100.contains(r#"crs:Exposure2012="+0.55""#),
                "foreign Camera Raw settings must be preserved: {xml_100}"
            );
            let parsed_100 = xmp::parse_xmp_str(xml_100).unwrap();
            assert_eq!(parsed_100.rating, Some(5));
            assert_eq!(parsed_100.flag.as_deref(), Some("pick"));
            assert_eq!(parsed_100.color_label.as_deref(), Some("red"));
            assert!(parsed_100.crop.flatten().is_some());
            assert_eq!(parsed_100.keywords, vec!["人像 > 客片".to_owned()]);

            let parsed_200 =
                xmp::parse_xmp_str(std::str::from_utf8(&store[&key_plain_sidecar]).unwrap())
                    .unwrap();
            assert_eq!(parsed_200.rating, Some(4));
            assert_eq!(parsed_200.flag.as_deref(), Some("reject"));
            assert_eq!(parsed_200.color_label.as_deref(), Some("blue"));
        }

        // 3. Auto-write OFF + manual `asset.xmp.read` & `asset.xmp.write` on remote library.
        svc.execute("xmp.policy.set", json!({"auto_write": false}))
            .unwrap();
        svc.execute("asset.rate", json!({"id": id_200, "rating": 1}))
            .unwrap();
        assert!(svc.flush_remote_xmp(Duration::from_secs(2)));
        // Remote sidecar for DSC_0200 still has 4 stars because auto_write is OFF.
        let read_report = svc
            .execute("asset.xmp.read", json!({"ids": [id_200]}))
            .unwrap();
        assert_eq!(read_report["updated"], 1, "{read_report}");
        assert_eq!(
            svc.execute("asset.get", json!({"id": id_200})).unwrap()["rating"],
            4
        );

        // Now clear DSC_0100's keywords while auto-write is OFF, change DSC_0200 to 2 stars,
        // and write whole library manually — removed keywords must be stripped from DSC_0100.xmp.
        svc.execute(
            "asset.keywords.set",
            json!({"ids": [id_100], "remove": ["人像 > 客片"]}),
        )
        .unwrap();
        svc.execute("asset.rate", json!({"id": id_200, "rating": 2}))
            .unwrap();
        let write_report = svc
            .execute("asset.xmp.write", json!({"library_id": lib_id}))
            .unwrap();
        assert_eq!(write_report["written"], 2, "{write_report}");
        assert_eq!(write_report["skipped"], 1, "{write_report}");
        assert_eq!(write_report["errors"].as_array().unwrap().len(), 0);
        {
            let store = server.store.lock().unwrap();
            let parsed_100 =
                xmp::parse_xmp_str(std::str::from_utf8(&store[&key_sidecar]).unwrap()).unwrap();
            assert!(
                parsed_100.keywords.is_empty(),
                "deleted keywords must be stripped on manual asset.xmp.write: {:?}",
                parsed_100.keywords
            );
            let parsed_200 =
                xmp::parse_xmp_str(std::str::from_utf8(&store[&key_plain_sidecar]).unwrap())
                    .unwrap();
            assert_eq!(parsed_200.rating, Some(2));
        }

        // 4. Read-only server (403): auto-write never breaks catalog mutations or poisons read availability!
        server.read_only.store(true, Ordering::Relaxed);
        svc.execute("xmp.policy.set", json!({"auto_write": true}))
            .unwrap();
        svc.execute("asset.rate", json!({"id": id_200, "rating": 5}))
            .unwrap();
        assert!(svc.flush_remote_xmp(Duration::from_secs(5)));
        let err_report = svc
            .execute("asset.xmp.write", json!({"ids": [id_200]}))
            .unwrap();
        assert_eq!(err_report["written"], 0);
        assert_eq!(err_report["errors"].as_array().unwrap().len(), 1);
        let libs = svc.execute("library.list", json!({})).unwrap();
        assert_eq!(
            libs[0]["availability"], "online",
            "read-only 403 on sidecar write must not poison library availability to auth_required ({kind}): {libs}"
        );
        assert_eq!(libs[0]["connected"], true);

        // 5. Remote RAW + JPEG paired library (`pair_raw_jpeg: true`) + mixed-case sidecar sync.
        server.read_only.store(false, Ordering::Relaxed);
        let pair_nef = format!("{prefix_root}/paired/DSC_0888.NEF");
        let pair_jpg = format!("{prefix_root}/paired/DSC_0888.JPG");
        let pair_xmp = format!("{prefix_root}/paired/DSC_0888.xmp");
        let mixed_nef = format!("{prefix_root}/paired/DSC_0900.NEF");
        let mixed_xmp = format!("{prefix_root}/paired/dsc_0900.Xmp");
        {
            let mut store = server.store.lock().unwrap();
            store.insert(pair_nef.clone(), b"pair-raw".to_vec());
            store.insert(pair_jpg.clone(), b"pair-jpg".to_vec());
            store.insert(mixed_nef.clone(), b"mixed-raw".to_vec());
            store.insert(mixed_xmp.clone(), adobe_xmp.as_bytes().to_vec());
        }
        let paired_lib = svc
            .execute(
                "library.add",
                json!({
                    "name": "Paired NAS",
                    "storage_id": source["id"],
                    "prefix": "paired",
                    "pair_raw_jpeg": true
                }),
            )
            .unwrap();
        let paired_lib_id = paired_lib["id"].as_str().unwrap();
        let paired_scan = svc
            .execute("library.scan", json!({"library_id": paired_lib_id}))
            .unwrap();
        assert_eq!(paired_scan["indexed"], 3, "{paired_scan}");

        let paired_list = svc
            .execute("asset.list", json!({"library_id": paired_lib_id}))
            .unwrap();
        let paired_items = paired_list["items"].as_array().unwrap();
        assert_eq!(paired_items.len(), 2, "RAW+JPEG must collapse to 1 card");
        let pair_rep = paired_items
            .iter()
            .find(|a| a["filename"].as_str().unwrap().starts_with("DSC_0888"))
            .unwrap();
        let rep_id = pair_rep["id"].as_str().unwrap().to_owned();
        let rep_detail = svc.execute("asset.get", json!({"id": rep_id})).unwrap();
        let variants = rep_detail["variants"].as_array().unwrap();
        let raw_id = variants[0]["id"].as_str().unwrap().to_owned();
        let jpg_id = variants[1]["id"].as_str().unwrap().to_owned();

        // Rating and cropping the grid representative writes shared `DSC_0888.xmp` and is never clobbered by unmarked RAW on library write.
        svc.execute("asset.rate", json!({"id": jpg_id, "rating": 5}))
            .unwrap();
        svc.execute(
            "asset.crop",
            json!({"id": jpg_id, "x": 0.05, "y": 0.1, "width": 0.8, "height": 0.7}),
        )
        .unwrap();
        assert!(svc.flush_remote_xmp(Duration::from_secs(5)));

        let paired_write = svc
            .execute("asset.xmp.write", json!({"library_id": paired_lib_id}))
            .unwrap();
        assert_eq!(paired_write["written"], 2, "{paired_write}");
        assert_eq!(paired_write["skipped"], 1, "{paired_write}");
        {
            let store = server.store.lock().unwrap();
            let parsed_pair =
                xmp::parse_xmp_str(std::str::from_utf8(&store[&pair_xmp]).unwrap()).unwrap();
            assert_eq!(parsed_pair.rating, Some(5));
            assert!(parsed_pair.crop.flatten().is_some());
            // Mixed-case `dsc_0900.Xmp` must be updated in place rather than duplicated as `DSC_0900.xmp`.
            assert!(store.contains_key(&mixed_xmp));
            assert!(!store.contains_key(&format!("{prefix_root}/paired/DSC_0900.xmp")));
        }

        // External update to `DSC_0888.xmp` read via `[jpg_id]` must update both JPEG and RAW halves.
        {
            let mut store = server.store.lock().unwrap();
            let prev_xml = std::str::from_utf8(&store[&pair_xmp]).unwrap().to_owned();
            let updated_bytes =
                xmp::prepare_sidecar_bytes(Some(&prev_xml), 2, "pick", Some("green"), None, None)
                    .unwrap()
                    .unwrap();
            store.insert(pair_xmp, updated_bytes);
        }
        let paired_read = svc
            .execute("asset.xmp.read", json!({"ids": [jpg_id]}))
            .unwrap();
        assert_eq!(paired_read["updated"], 2, "{paired_read}");
        for member_id in [&raw_id, &jpg_id] {
            let member = svc.execute("asset.get", json!({"id": member_id})).unwrap();
            assert_eq!(member["rating"], 2);
            assert_eq!(member["flag"], "pick");
            assert_eq!(member["color_label"], "green");
            assert!(member["crop"].is_null());
        }
    }
}
