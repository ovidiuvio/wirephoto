//! End-to-end tests: the real router over a temporary library.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use wirephoto::app::App;
use wirephoto::auth::Keys;
use wirephoto::preview::render::{Encoder, Rgb, encode};
use wirephoto_api::Scope;

struct T {
    _dir: tempfile::TempDir,
    lib: std::path::PathBuf,
    app: Arc<App>,
    router: Router,
    key: String,
    root: String,
}

fn jpeg(w: u32, h: u32) -> Vec<u8> {
    let mut px = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            px.extend([(x * 255 / w) as u8, (y * 255 / h) as u8, 120]);
        }
    }
    encode(&Rgb { w, h, px }, Encoder::Turbo).unwrap()
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(w, h, |x, _| {
        image::Rgba([200, 10, 10, if x < w / 2 { 0 } else { 255 }])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

async fn setup() -> T {
    setup_with("").await
}

/// The test library, with `extra` appended to the config (TOML tables).
async fn setup_with(extra: &str) -> T {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("photos");
    std::fs::create_dir_all(lib.join("2026/Lisbon")).unwrap();
    std::fs::write(lib.join("2026/Lisbon/a.jpg"), jpeg(1600, 1200)).unwrap();
    std::fs::write(lib.join("2026/Lisbon/b.jpg"), jpeg(1200, 1600)).unwrap();
    std::fs::write(lib.join("2026/logo.png"), png(300, 100)).unwrap();
    std::fs::write(lib.join("notes.txt"), b"hello").unwrap();
    std::fs::write(lib.join(".DS_Store"), b"junk").unwrap();
    let extra = extra.replace("{dir}", dir.path().to_str().unwrap());
    let extra = if extra.contains("[decode]") {
        extra
    } else {
        format!(
            "[decode]\nprogram = {:?}\n{extra}",
            env!("CARGO_BIN_EXE_wirephoto")
        )
    };
    let config: wirephoto::config::Config = toml::from_str(&format!(
        "data = {:?}\n[[library]]\nid = 'nas'\nname = 'NAS Photos'\npath = {:?}\nwatch = 'poll'\npoll_seconds = 3600\n{extra}",
        dir.path().join("data"),
        lib
    ))
    .unwrap();
    let (app, router) = wirephoto::build(config).unwrap();
    let (a, l) = (app.clone(), app.libraries[0].clone());
    tokio::task::spawn_blocking(move || wirephoto::indexer::scan(&a, &l))
        .await
        .unwrap()
        .unwrap();
    wirephoto::indexer::settle(&app).await.unwrap();
    let (_, key) = Keys::create(
        &app.db,
        "lith",
        Scope::Write,
        None,
        Some("https://lith.example.com".into()),
    )
    .unwrap();
    let root = app.libraries[0].root_id.read().clone();
    T {
        _dir: dir,
        lib,
        app,
        router,
        key,
        root,
    }
}

struct Res {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Res {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&self.body)))
    }
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    }
}

impl T {
    async fn send(&self, req: Request<Body>) -> Res {
        let r = self.router.clone().oneshot(req).await.unwrap();
        let status = r.status();
        let headers = r.headers().clone();
        let body = r.into_body().collect().await.unwrap().to_bytes().to_vec();
        Res {
            status,
            headers,
            body,
        }
    }

    fn req(&self, method: &str, uri: &str) -> axum::http::request::Builder {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {}", self.key))
    }

    async fn get(&self, uri: &str) -> Res {
        self.send(self.req("GET", uri).body(Body::empty()).unwrap())
            .await
    }

    async fn json(&self, method: &str, uri: &str, body: Value) -> Res {
        self.send(
            self.req(method, uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    async fn entry_at(&self, path: &str) -> Value {
        let r = self
            .get(&format!("/v1/entries?library=lib_nas&path={path}"))
            .await;
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        r.json()["items"][0].clone()
    }
}

fn decode_size(jpg: &[u8]) -> (u32, u32) {
    let img = image::load_from_memory_with_format(jpg, image::ImageFormat::Jpeg).unwrap();
    (img.width(), img.height())
}

#[tokio::test]
async fn auth_and_discovery() {
    let t = setup().await;
    let r = t
        .send(Request::get("/v1").body(Body::empty()).unwrap())
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.json()["previews"]["sizes"],
        json!([160, 320, 480, 640, 1280, 2560])
    );
    assert!(r.json().get("key").is_none());
    assert_eq!(t.get("/v1").await.json()["key"]["name"], "lith");

    let r = t
        .send(Request::get("/v1/libraries").body(Body::empty()).unwrap())
        .await;
    assert_eq!(r.status, 401);
    assert_eq!(r.json()["code"], "unauthenticated");
    assert_eq!(
        r.header("content-type").as_deref(),
        Some("application/problem+json")
    );

    let libs = t.get("/v1/libraries").await.json();
    assert_eq!(libs[0]["id"], "lib_nas");
    assert_eq!(libs[0]["root"], t.root.as_str());
    assert_eq!(libs[0]["watch"], "polling");

    let r = t
        .send(
            t.req("GET", "/v1/libraries")
                .header(header::ORIGIN, "https://evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 403);
    let r = t
        .send(
            t.req("GET", "/v1/libraries")
                .header(header::ORIGIN, "https://lith.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 200);

    // A script key (no origin) is refused from browsers.
    let (_, script) = Keys::create(&t.app.db, "script", Scope::Read, None, None).unwrap();
    let r = t
        .send(
            Request::get("/v1/libraries")
                .header(header::AUTHORIZATION, format!("Bearer {script}"))
                .header(header::ORIGIN, "https://lith.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 403);
}

#[tokio::test]
async fn cors_allows_key_origins_only() {
    let t = setup().await;
    let pre = |origin: &str| {
        Request::builder()
            .method("OPTIONS")
            .uri("/v1/entries")
            .header(header::ORIGIN, origin)
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
            .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "authorization")
            .body(Body::empty())
            .unwrap()
    };
    let r = t.send(pre("https://lith.example.com")).await;
    assert_eq!(
        r.header("access-control-allow-origin").as_deref(),
        Some("https://lith.example.com")
    );
    assert_eq!(r.header("access-control-max-age").as_deref(), Some("7200"));
    let r = t.send(pre("https://evil.example")).await;
    assert!(r.header("access-control-allow-origin").is_none());
    // Every upload request passes a browser's preflight: tus, as tus-js-client sends it, and a plain PUT.
    for (method, uri, headers) in [
        (
            "POST",
            "/v1/tus",
            "authorization,tus-resumable,upload-length,upload-metadata,content-type,upload-checksum,x-requested-with",
        ),
        ("HEAD", "/v1/tus/tus_x", "authorization,tus-resumable"),
        (
            "PATCH",
            "/v1/tus/tus_x",
            "authorization,tus-resumable,upload-offset,content-type,upload-checksum",
        ),
        (
            "POST",
            "/v1/tus/tus_x",
            "authorization,tus-resumable,upload-offset,content-type,x-http-method-override",
        ),
        ("DELETE", "/v1/tus/tus_x", "authorization,tus-resumable"),
        ("GET", "/v1/tus/tus_x", "authorization"),
        ("OPTIONS", "/v1/tus", "tus-resumable"),
        (
            "PUT",
            "/v1/entries/ent_x/children/a.jpg",
            "authorization,content-type,content-digest,if-none-match",
        ),
    ] {
        let r = t
            .send(
                Request::builder()
                    .method("OPTIONS")
                    .uri(uri)
                    .header(header::ORIGIN, "https://lith.example.com")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, method)
                    .header(header::ACCESS_CONTROL_REQUEST_HEADERS, headers)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        let allowed = r.header("access-control-allow-headers").unwrap_or_default();
        for h in headers.split(',') {
            assert!(
                allowed.split(',').any(|a| a.trim() == h),
                "{method} {uri}: {h} not in {allowed}"
            );
        }
        let methods = r.header("access-control-allow-methods").unwrap_or_default();
        assert!(methods.contains(method), "{method} {uri}: {methods}");
    }
    // What tus clients read from the answers is exposed to scripts (on real answers; preflights do not say).
    let r = t
        .send(
            t.req("GET", "/v1")
                .header(header::ORIGIN, "https://lith.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let exposed = r
        .header("access-control-expose-headers")
        .unwrap_or_default();
    for h in [
        "location",
        "upload-offset",
        "upload-length",
        "upload-expires",
        "tus-resumable",
        "tus-version",
        "tus-extension",
        "tus-max-size",
        "tus-checksum-algorithm",
        "wirephoto-entry",
        "wirephoto-entry-name",
        "wirephoto-stored",
    ] {
        assert!(
            exposed.split(',').any(|e| e.trim() == h),
            "{h} not exposed: {exposed}"
        );
    }
    // A cross-origin isolated page (COEP require-corp) may embed previews by their signed URLs.
    let r = t.get("/healthz").await;
    assert_eq!(
        r.header("cross-origin-resource-policy").as_deref(),
        Some("cross-origin")
    );
}

#[tokio::test]
async fn listing_facts_and_paging() {
    let t = setup().await;
    let r = t.get(&format!("/v1/entries?parent={}", t.root)).await;
    assert_eq!(r.status, 200);
    let items = r.json()["items"].as_array().unwrap().clone();
    let names: Vec<&str> = items.iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["2026", "notes.txt"], "dotfiles are not indexed");
    assert_eq!(items[0]["kind"], "folder");
    assert_eq!(items[0]["counts"], json!({"folders": 1, "files": 1}));
    assert_eq!(items[1]["kind"], "other");
    assert!(items[1]["urls"].get("preview").is_none());

    let a = t.entry_at("/2026/Lisbon/a.jpg").await;
    assert_eq!(a["kind"], "image");
    assert_eq!(a["mime"], "image/jpeg");
    assert_eq!(a["media"]["width"], 1600);
    assert_eq!(a["media"]["height"], 1200);
    assert_eq!(a["sha256"].as_str().unwrap().len(), 64);
    assert!(
        a["urls"]["preview"]
            .as_str()
            .unwrap()
            .contains("{size}.jpg?v=")
    );
    let b = t.entry_at("/2026/Lisbon/b.jpg").await;
    assert_eq!(b["media"]["height"], 1600);

    // Recursive, filtered by kind, paged one at a time.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut uri = format!(
            "/v1/entries?parent={}&recursive=true&kind=image&sort=-name&limit=1",
            t.root
        );
        if let Some(c) = &cursor {
            uri.push_str(&format!("&cursor={c}"));
        }
        let page = t.get(&uri).await.json();
        for e in page["items"].as_array().unwrap() {
            seen.push(e["name"].as_str().unwrap().to_string());
        }
        match page["next"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(seen, vec!["logo.png", "b.jpg", "a.jpg"]);

    let r = t.get("/v1/entries").await;
    assert_eq!(r.status, 400);
    let r = t.get("/v1/entries?library=lib_nas&sort=nope").await;
    assert_eq!(r.json()["code"], "invalid-request");
}

#[tokio::test]
async fn content_ranges_and_signed_urls() {
    let t = setup().await;
    let a = t.entry_at("/2026/Lisbon/a.jpg").await;
    let original = std::fs::read(t.lib.join("2026/Lisbon/a.jpg")).unwrap();
    let content = a["urls"]["content"].as_str().unwrap().to_string();

    // A signed URL works without a header, like <img> or <video>.
    let r = t
        .send(Request::get(&content).body(Body::empty()).unwrap())
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body, original);
    let digest = format!(
        "sha-256=:{}:",
        wirephoto::util::b64(&Sha256::digest(&original))
    );
    assert_eq!(r.header("repr-digest").as_deref(), Some(digest.as_str()));
    let etag = r.header("etag").unwrap();

    let r = t
        .send(
            Request::get(&content)
                .header(header::RANGE, "bytes=10-19")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 206);
    assert_eq!(r.body, original[10..20].to_vec());
    assert_eq!(
        r.header("content-range").unwrap(),
        format!("bytes 10-19/{}", original.len())
    );

    let r = t
        .send(
            Request::get(&content)
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 304);

    // A sig for another entry, or after the key is revoked, does not work.
    let b = t.entry_at("/2026/Lisbon/b.jpg").await;
    let forged = format!(
        "/v1/entries/{}/content?{}",
        b["id"].as_str().unwrap(),
        content.split('?').nth(1).unwrap()
    );
    assert_eq!(
        t.send(Request::get(&forged).body(Body::empty()).unwrap())
            .await
            .status,
        401
    );
    Keys::revoke(&t.app.db, "lith").unwrap();
    assert_eq!(
        t.send(Request::get(&content).body(Body::empty()).unwrap())
            .await
            .status,
        401
    );
}

#[tokio::test]
async fn previews_single_and_streamed() {
    let t = setup().await;
    let a = t.entry_at("/2026/Lisbon/a.jpg").await;
    let template = a["urls"]["preview"].as_str().unwrap().to_string();

    let r = t
        .send(
            Request::get(template.replace("{size}", "640"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type").as_deref(), Some("image/jpeg"));
    assert!(r.header("cache-control").unwrap().contains("immutable"));
    assert_eq!(
        r.header("server-timing").as_deref(),
        Some("cache;desc=hit"),
        "640 is pre-generated"
    );
    assert_eq!(decode_size(&r.body), (640, 480));

    let r = t
        .send(
            Request::get(template.replace("{size}", "160"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(decode_size(&r.body), (160, 120));
    assert!(
        r.header("server-timing")
            .unwrap()
            .starts_with("cache;desc=miss")
    );

    // Portrait stays portrait; a PNG with transparency becomes an opaque JPEG.
    let b = t.entry_at("/2026/Lisbon/b.jpg").await;
    let r = t
        .send(
            Request::get(
                b["urls"]["preview"]
                    .as_str()
                    .unwrap()
                    .replace("{size}", "320"),
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(decode_size(&r.body), (240, 320));
    let logo = t.entry_at("/2026/logo.png").await;
    let r = t
        .send(
            Request::get(
                logo["urls"]["preview"]
                    .as_str()
                    .unwrap()
                    .replace("{size}", "2560"),
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(
        decode_size(&r.body),
        (300, 100),
        "never larger than the original"
    );

    // A stale version redirects to the current one; unknown sizes do not exist.
    let stale = template
        .replace("{size}", "640")
        .replace(&format!("v={}", a["etag"].as_str().unwrap()), "v=old");
    let r = t
        .send(Request::get(&stale).body(Body::empty()).unwrap())
        .await;
    assert_eq!(r.status, 307);
    assert!(
        r.header("location")
            .unwrap()
            .contains(a["etag"].as_str().unwrap())
    );
    assert_eq!(
        t.send(
            Request::get(template.replace("{size}", "333"))
                .body(Body::empty())
                .unwrap()
        )
        .await
        .status,
        404
    );
    let notes = t.entry_at("/notes.txt").await;
    let r = t
        .get(&format!(
            "/v1/entries/{}/preview/640.jpg",
            notes["id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(r.status, 415);

    // Many at once, streamed as frames.
    let ids = json!([a["id"], b["id"], notes["id"], "ent_nope"]);
    let r = t
        .json("POST", "/v1/previews", json!({"ids": ids, "size": 320}))
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.header("content-type").as_deref(),
        Some("application/vnd.wirephoto.frames")
    );
    let mut frames = std::collections::BTreeMap::new();
    let mut at = 0;
    while at < r.body.len() {
        let index = u32::from_le_bytes(r.body[at..at + 4].try_into().unwrap());
        let status = u16::from_le_bytes(r.body[at + 4..at + 6].try_into().unwrap());
        let len = u32::from_le_bytes(r.body[at + 8..at + 12].try_into().unwrap()) as usize;
        frames.insert(index, (status, r.body[at + 12..at + 12 + len].to_vec()));
        at += 12 + len;
    }
    assert_eq!(frames.len(), 4);
    assert_eq!(frames[&0].0, 200);
    assert_eq!(decode_size(&frames[&0].1), (320, 240));
    assert_eq!(frames[&1].0, 200);
    assert_eq!(frames[&2].0, 415);
    assert_eq!(frames[&3].0, 404);
    assert_eq!(
        serde_json::from_slice::<Value>(&frames[&3].1).unwrap()["code"],
        "not-found"
    );
}

#[tokio::test]
async fn uploads_dedup_and_digests() {
    let t = setup().await;
    let folder = t.entry_at("/2026").await;
    let fid = folder["id"].as_str().unwrap();
    let bytes = jpeg(800, 600);
    let wrong = format!("sha-256=:{}:", wirephoto::util::b64(&[0u8; 32]));
    let r = t
        .send(
            t.req("PUT", &format!("/v1/entries/{fid}/children/new.jpg"))
                .header(header::IF_NONE_MATCH, "*")
                .header("content-digest", wrong)
                .body(Body::from(bytes.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["code"], "digest-mismatch");
    assert!(!t.lib.join("2026/new.jpg").exists());

    let right = format!(
        "sha-256=:{}:",
        wirephoto::util::b64(&Sha256::digest(&bytes))
    );
    let r = t
        .send(
            t.req("PUT", &format!("/v1/entries/{fid}/children/new.jpg"))
                .header(header::IF_NONE_MATCH, "*")
                .header("content-digest", right)
                .body(Body::from(bytes.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let e = r.json();
    assert_eq!(e["media"]["width"], 800);
    assert_eq!(
        e["sha256"].as_str().unwrap(),
        hex::encode(Sha256::digest(&bytes))
    );
    assert_eq!(std::fs::read(t.lib.join("2026/new.jpg")).unwrap(), bytes);
    let leftovers: Vec<_> = std::fs::read_dir(t.lib.join("2026"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(".wp-"))
        .collect();
    assert!(leftovers.is_empty(), "no temporary files left behind");

    let put = |name: &str, body: Vec<u8>| {
        t.req("PUT", &format!("/v1/entries/{fid}/children/{name}"))
            .body(Body::from(body))
            .unwrap()
    };
    // The same bytes again, under any name: nothing stored, the file already there is the answer.
    for name in ["new.jpg", "other-name.jpg"] {
        let r = t.send(put(name, bytes.clone())).await;
        assert_eq!(r.status, 200, "{name}: already there");
        assert_eq!(r.json()["id"], e["id"]);
    }
    assert!(!t.lib.join("2026/other-name.jpg").exists());
    // Different bytes under a taken name: stored beside it, never over it.
    let new_bytes = jpeg(400, 300);
    let r = t.send(put("new.jpg", new_bytes.clone())).await;
    assert_eq!(r.status, 201);
    let e2 = r.json();
    assert_eq!(e2["name"], "new-1.jpg");
    assert_ne!(e2["id"], e["id"]);
    assert_eq!(
        std::fs::read(t.lib.join("2026/new.jpg")).unwrap(),
        bytes,
        "untouched"
    );
    assert_eq!(
        std::fs::read(t.lib.join("2026/new-1.jpg")).unwrap(),
        new_bytes
    );

    // New contents for a file: saved as a new file beside it; the same contents store nothing.
    let id = e["id"].as_str().unwrap();
    let content = |body: Vec<u8>| {
        t.req("PUT", &format!("/v1/entries/{id}/content"))
            .body(Body::from(body))
            .unwrap()
    };
    let r = t.send(content(bytes.clone())).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["id"], e["id"]);
    let third = jpeg(500, 300);
    let r = t.send(content(third.clone())).await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["name"], "new-2.jpg");
    assert_eq!(r.json()["media"]["width"], 500);
    assert_eq!(
        std::fs::read(t.lib.join("2026/new.jpg")).unwrap(),
        bytes,
        "untouched"
    );

    // A read-only key cannot write.
    let (_, ro) = Keys::create(&t.app.db, "viewer", Scope::Read, None, None).unwrap();
    let r = t
        .send(
            Request::put(format!("/v1/entries/{fid}/children/x.jpg"))
                .header(header::AUTHORIZATION, format!("Bearer {ro}"))
                .header(header::IF_NONE_MATCH, "*")
                .body(Body::from(bytes))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 403);
}

#[tokio::test]
async fn organise_trash_and_restore() {
    let t = setup().await;
    let year = t.entry_at("/2026").await;
    let a = t.entry_at("/2026/Lisbon/a.jpg").await;
    let aid = a["id"].as_str().unwrap().to_string();

    // New folder; same name again conflicts.
    let r = t
        .json(
            "POST",
            "/v1/entries",
            json!({"parent": year["id"], "name": "Selects", "kind": "folder"}),
        )
        .await;
    assert_eq!(r.status, 201);
    let selects = r.json();
    assert!(t.lib.join("2026/Selects").is_dir());
    let r = t
        .json(
            "POST",
            "/v1/entries",
            json!({"parent": year["id"], "name": "Selects", "kind": "folder"}),
        )
        .await;
    assert_eq!(r.status, 201, "a taken name gets the next free one");
    assert_eq!(r.json()["name"], "Selects-1");

    // Move + rename keeps the ID.
    let r = t
        .json(
            "PATCH",
            &format!("/v1/entries/{aid}"),
            json!({"parent": selects["id"], "name": "Lisbon-001.jpg"}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["id"], aid.as_str());
    assert_eq!(r.json()["path"], "/2026/Selects/Lisbon-001.jpg");
    assert!(t.lib.join("2026/Selects/Lisbon-001.jpg").exists());
    assert!(!t.lib.join("2026/Lisbon/a.jpg").exists());
    assert_eq!(
        t.json(
            "PATCH",
            &format!("/v1/entries/{aid}"),
            json!({"media": {"rating": 5}})
        )
        .await
        .status,
        400
    );

    // Copy, then the copy's name is taken.
    let r = t
        .json(
            "POST",
            "/v1/entries",
            json!({"parent": year["id"], "copyOf": aid}),
        )
        .await;
    assert_eq!(r.status, 201);
    assert_eq!(r.json()["sha256"], a["sha256"]);
    let r = t
        .json(
            "POST",
            "/v1/entries",
            json!({"parent": year["id"], "copyOf": aid}),
        )
        .await;
    assert_eq!(r.status, 201);
    assert_eq!(r.json()["name"], "Lisbon-001-1.jpg");

    // Moving a folder moves what is in it.
    let lisbon = t.entry_at("/2026/Lisbon").await;
    let r = t
        .json(
            "PATCH",
            &format!("/v1/entries/{}", lisbon["id"].as_str().unwrap()),
            json!({"name": "Portugal"}),
        )
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(t.entry_at("/2026/Portugal/b.jpg").await["name"], "b.jpg");

    // Trash and restore.
    let r = t
        .send(
            t.req("DELETE", &format!("/v1/entries/{aid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 204);
    assert!(!t.lib.join("2026/Selects/Lisbon-001.jpg").exists());
    assert!(
        t.lib
            .join(".wirephoto-trash")
            .join(&aid)
            .join("Lisbon-001.jpg")
            .exists()
    );
    let listed = t
        .get(&format!(
            "/v1/entries?parent={}",
            selects["id"].as_str().unwrap()
        ))
        .await
        .json();
    assert_eq!(listed["items"].as_array().unwrap().len(), 0);
    let trash = t
        .get("/v1/entries?library=lib_nas&trashed=true")
        .await
        .json();
    assert_eq!(trash["items"][0]["id"], aid.as_str());
    assert_eq!(trash["items"][0]["trashed"], true);
    let r = t
        .json(
            "PATCH",
            &format!("/v1/entries/{aid}"),
            json!({"trashed": false}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert!(t.lib.join("2026/Selects/Lisbon-001.jpg").exists());
    assert!(r.json().get("trashed").is_none());

    // Trash a folder; deleting it again is refused, and so is a permanent delete: nothing is ever gone for good
    // through the API.
    let r = t
        .send(
            t.req(
                "DELETE",
                &format!("/v1/entries/{}", selects["id"].as_str().unwrap()),
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(r.status, 204);
    let r = t.get(&format!("/v1/entries/{aid}")).await.json();
    assert_eq!(r["trashed"], true, "what was inside is trashed with it");
    let r = t
        .send(
            t.req(
                "DELETE",
                &format!("/v1/entries/{}", selects["id"].as_str().unwrap()),
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(r.status, 409);
    assert_eq!(r.json()["code"], "conflict");
    let b = t.entry_at("/2026/Portugal/b.jpg").await;
    let r = t
        .send(
            t.req(
                "DELETE",
                &format!("/v1/entries/{}?permanent=true", b["id"].as_str().unwrap()),
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(r.status, 400);
    assert!(t.lib.join("2026/Portugal/b.jpg").exists());
    assert_eq!(
        t.get(&format!("/v1/entries/{aid}")).await.json()["trashed"],
        true
    );
    assert!(
        t.lib
            .join(".wirephoto-trash")
            .join(selects["id"].as_str().unwrap())
            .exists()
    );

    // The root stays.
    assert_eq!(
        t.send(
            t.req("DELETE", &format!("/v1/entries/{}", t.root))
                .body(Body::empty())
                .unwrap()
        )
        .await
        .status,
        409
    );
}

#[tokio::test]
async fn batch_runs_requests_in_order() {
    let t = setup().await;
    let a = t.entry_at("/2026/Lisbon/a.jpg").await;
    let b = t.entry_at("/2026/Lisbon/b.jpg").await;
    let r = t
        .json(
            "POST",
            "/v1/batch",
            json!({"requests": [
                {"method": "PATCH", "path": format!("/v1/entries/{}", a["id"].as_str().unwrap()), "body": {"name": "renamed.jpg"}},
                {"method": "DELETE", "path": format!("/v1/entries/{}", b["id"].as_str().unwrap()), "headers": {"If-Match": "\"wrong\""}},
                {"method": "GET", "path": format!("/v1/entries?library=lib_nas&sha256={}", a["sha256"].as_str().unwrap())},
                {"method": "PUT", "path": format!("/v1/entries/{}/content", a["id"].as_str().unwrap())}
            ]}),
        )
        .await;
    assert_eq!(r.status, 200);
    let res = r.json()["responses"].as_array().unwrap().clone();
    assert_eq!(res[0]["status"], 200);
    assert_eq!(res[0]["body"]["name"], "renamed.jpg");
    assert_eq!(res[1]["status"], 412);
    assert_eq!(res[1]["body"]["code"], "version-mismatch");
    assert_eq!(res[2]["body"]["items"][0]["name"], "renamed.jpg");
    assert_eq!(res[3]["status"], 400, "bytes cannot be batched");
    assert!(t.lib.join("2026/Lisbon/b.jpg").exists());
}

#[tokio::test]
async fn changes_catch_up() {
    let t = setup().await;
    let now = t.get("/v1/changes").await.json();
    let since = now["next"].as_u64().unwrap();
    assert!(since > 0);
    assert!(now["changes"].as_array().unwrap().is_empty());

    let year = t.entry_at("/2026").await;
    let created = t
        .json(
            "POST",
            "/v1/entries",
            json!({"parent": year["id"], "name": "New", "kind": "folder"}),
        )
        .await
        .json();
    // Removed on disk (the API never deletes for good).
    std::fs::remove_dir(t.lib.join("2026/New")).unwrap();
    rescan(&t).await;

    let ch = t.get(&format!("/v1/changes?since={since}")).await.json();
    let list = ch["changes"].as_array().unwrap();
    let types: Vec<&str> = list.iter().map(|c| c["type"].as_str().unwrap()).collect();
    assert_eq!(types, vec!["created", "deleted"]);
    assert_eq!(list[0]["entry"]["id"], created["id"]);
    assert_eq!(list[1]["entry"]["parent"], year["id"]);
    let next = ch["next"].as_u64().unwrap();
    assert!(
        t.get(&format!("/v1/changes?since={next}")).await.json()["changes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn deduplication() {
    let t = setup().await;
    let original = std::fs::read(t.lib.join("2026/Lisbon/a.jpg")).unwrap();
    let sha = hex::encode(Sha256::digest(&original));
    let r = t
        .get(&format!("/v1/entries?library=lib_nas&sha256={sha}"))
        .await
        .json();
    assert_eq!(r["items"].as_array().unwrap().len(), 1);
    assert!(
        t.get("/v1/entries?library=lib_nas&duplicates=true")
            .await
            .json()["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // A copy made behind the server's back, not hashed yet: a lookup with the size hashes it on demand.
    std::fs::write(t.lib.join("2026/copy.jpg"), &original).unwrap();
    let (a, l) = (t.app.clone(), t.app.libraries[0].clone());
    tokio::task::spawn_blocking(move || wirephoto::indexer::scan(&a, &l))
        .await
        .unwrap()
        .unwrap();
    let r = t
        .get(&format!(
            "/v1/entries?library=lib_nas&sha256={sha}&size={}",
            original.len()
        ))
        .await
        .json();
    assert_eq!(r["items"].as_array().unwrap().len(), 2);
    let dups = t
        .get("/v1/entries?library=lib_nas&duplicates=true")
        .await
        .json();
    assert_eq!(dups["items"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn indexer_follows_the_disk() {
    let t = setup().await;
    let b = t.entry_at("/2026/Lisbon/b.jpg").await;
    // Outside changes: a move (same inode), a new file, a deletion.
    std::fs::rename(
        t.lib.join("2026/Lisbon/b.jpg"),
        t.lib.join("2026/moved.jpg"),
    )
    .unwrap();
    std::fs::write(t.lib.join("2026/Lisbon/fresh.jpg"), jpeg(320, 240)).unwrap();
    std::fs::remove_file(t.lib.join("notes.txt")).unwrap();
    let (a, l) = (t.app.clone(), t.app.libraries[0].clone());
    tokio::task::spawn_blocking(move || wirephoto::indexer::scan(&a, &l))
        .await
        .unwrap()
        .unwrap();
    wirephoto::indexer::settle(&t.app).await.unwrap();

    let moved = t.entry_at("/2026/moved.jpg").await;
    assert_eq!(moved["id"], b["id"], "a move keeps the ID");
    let fresh = t.entry_at("/2026/Lisbon/fresh.jpg").await;
    assert_eq!(fresh["media"]["width"], 320);
    let r = t
        .get("/v1/entries?library=lib_nas&path=/notes.txt")
        .await
        .json();
    assert!(r["items"].as_array().unwrap().is_empty());
}

/// Rescans the library and waits for the background passes.
async fn rescan(t: &T) {
    let (a, l) = (t.app.clone(), t.app.libraries[0].clone());
    tokio::task::spawn_blocking(move || wirephoto::indexer::scan(&a, &l))
        .await
        .unwrap()
        .unwrap();
    wirephoto::indexer::settle(&t.app).await.unwrap();
}

#[tokio::test]
async fn worker_crash_costs_one_preview() {
    // The first worker started dies on its first request; the ones after it are real.
    let scratch = tempfile::tempdir().unwrap();
    let script = scratch.path().join("decoder.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nif mkdir {dir:?}/crashed 2>/dev/null; then head -c 1 >/dev/null; kill -SEGV $$; fi\nexec {exe:?} \"$@\"\n",
            dir = scratch.path(),
            exe = env!("CARGO_BIN_EXE_wirephoto"),
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let t = setup_with(&format!(
        "[decode]\nprogram = {script:?}\nworkers = 1\n[cache]\npregenerate = false\n"
    ))
    .await;
    let a = t.entry_at("/2026/Lisbon/a.jpg").await;
    let url = format!("/v1/entries/{}/preview/320.jpg", a["id"].as_str().unwrap());

    let r = t.get(&url).await;
    assert_eq!(
        r.status, 503,
        "the crash costs this preview, and says to try again"
    );
    assert_eq!(r.json()["code"], "preview-failed");
    let r = t.get(&url).await;
    assert_eq!(r.status, 200, "a new worker takes over");
    assert_eq!(decode_size(&r.body), (320, 240));
    assert_eq!(t.app.previews.pool().stats(), (2, 1));
    // The entry still has a preview: a crash is not a verdict on the file.
    assert!(t.entry_at("/2026/Lisbon/a.jpg").await["urls"]["preview"].is_string());
}

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A 2 s 320x240 test-pattern MP4 (moov at the end, so reading it needs seeking).
fn mp4(path: &std::path::Path) {
    let out = std::process::Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=2:size=320x240:rate=10",
        ])
        .args(["-c:v", "mpeg4", "-pix_fmt", "yuv420p", "-y"])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn videos_get_posters_and_durations() {
    if !ffmpeg_available() {
        eprintln!("skipped: no ffmpeg on PATH");
        return;
    }
    let t = setup().await;
    assert!(t.app.previews.pool().has_video());
    mp4(&t.lib.join("2026/clip.mp4"));
    rescan(&t).await;

    let clip = t.entry_at("/2026/clip.mp4").await;
    assert_eq!(clip["kind"], "video");
    assert_eq!(clip["media"]["width"], 320);
    assert_eq!(clip["media"]["height"], 240);
    let d = clip["media"]["duration"].as_f64().unwrap();
    assert!((d - 2.0).abs() < 0.2, "duration {d}");
    let template = clip["urls"]["preview"].as_str().unwrap().to_string();
    let r = t
        .send(
            Request::get(template.replace("{size}", "640"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.header("server-timing").as_deref(),
        Some("cache;desc=hit"),
        "pre-generated"
    );
    assert_eq!(decode_size(&r.body), (320, 240), "never enlarged");
    let r = t
        .send(
            Request::get(template.replace("{size}", "160"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(decode_size(&r.body), (160, 120));

    // Uploaded: probed and given a poster from the written file. (Into another folder: the same bytes beside
    // the first clip would be recognised as already there.)
    let scratch = tempfile::tempdir().unwrap();
    let up = scratch.path().join("up.mp4");
    mp4(&up);
    let folder = t.entry_at("/2026/Lisbon").await;
    let r = t
        .send(
            t.req(
                "PUT",
                &format!(
                    "/v1/entries/{}/children/up.mp4",
                    folder["id"].as_str().unwrap()
                ),
            )
            .header(header::IF_NONE_MATCH, "*")
            .body(Body::from(std::fs::read(&up).unwrap()))
            .unwrap(),
        )
        .await;
    assert_eq!(r.status, 201);
    let e = r.json();
    assert!(e["media"]["duration"].as_f64().unwrap() > 1.5);
    let r = t
        .get(&format!(
            "/v1/entries/{}/preview/320.jpg",
            e["id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(decode_size(&r.body), (320, 240));
}

#[tokio::test]
async fn videos_without_ffmpeg() {
    let t = setup_with(&format!(
        "[decode]\nprogram = {:?}\nffmpeg = '/nonexistent/ffmpeg'\n",
        env!("CARGO_BIN_EXE_wirephoto")
    ))
    .await;
    assert!(!t.app.previews.pool().has_video());
    let mut head = b"\0\0\0\x18ftypisom\0\0\x02\0isomiso2".to_vec();
    head.resize(4096, 0);
    std::fs::write(t.lib.join("2026/clip.mp4"), head).unwrap();
    rescan(&t).await;
    let clip = t.entry_at("/2026/clip.mp4").await;
    assert_eq!(clip["kind"], "video");
    assert!(clip["urls"].get("preview").is_none());
    let r = t
        .get(&format!(
            "/v1/entries/{}/preview/320.jpg",
            clip["id"].as_str().unwrap()
        ))
        .await;
    assert_eq!(r.status, 415);
}

#[tokio::test]
async fn background_and_request_render_once() {
    let t = setup_with("[cache]\npregenerate = false\n").await;
    let lib = t.app.libraries[0].clone();
    let row = |path: &str| {
        let p = path.to_string();
        t.app
            .db
            .read(|c| Ok(wirephoto::db::entry_by_path(c, &lib.api_id, &p)?))
            .unwrap()
            .unwrap()
    };
    let src = |path: &str| wirephoto::preview::Source::File(lib.fs.clone(), path.to_string());
    let previews = &t.app.previews;
    assert_eq!(previews.renders(), 0);

    // Together: whichever starts second waits for the first.
    let a = row("2026/Lisbon/a.jpg");
    let (bg, req) = tokio::join!(
        previews.ensure_cached(&a, src("2026/Lisbon/a.jpg")),
        previews.get(&a, src("2026/Lisbon/a.jpg"), 640),
    );
    bg.unwrap();
    assert!(!req.unwrap().0.is_empty());
    assert_eq!(previews.renders(), 1, "one rendering for both");

    // A request first: pre-generation finds it and renders nothing.
    let b = row("2026/Lisbon/b.jpg");
    previews
        .get(&b, src("2026/Lisbon/b.jpg"), 320)
        .await
        .unwrap();
    assert_eq!(previews.renders(), 2);
    assert!(
        !previews
            .ensure_cached(&b, src("2026/Lisbon/b.jpg"))
            .await
            .unwrap()
    );
    assert_eq!(previews.renders(), 2);
}

fn row_at(t: &T, path: &str) -> Option<wirephoto::db::EntryRow> {
    let lib = t.app.libraries[0].api_id.clone();
    let p = path.to_string();
    t.app
        .db
        .read(|c| Ok(wirephoto::db::entry_by_path(c, &lib, &p)?))
        .unwrap()
}

#[tokio::test]
async fn copies_share_one_set_of_previews() {
    let t = setup().await;
    std::fs::copy(t.lib.join("2026/Lisbon/a.jpg"), t.lib.join("2026/copy.jpg")).unwrap();
    rescan(&t).await;
    let (a, copy) = (
        row_at(&t, "2026/Lisbon/a.jpg").unwrap(),
        row_at(&t, "2026/copy.jpg").unwrap(),
    );
    assert_ne!(a.id, copy.id);
    assert_eq!(a.sha256, copy.sha256);
    let disk = t.app.previews.disk();
    let shared = wirephoto::preview::key(&a, 640);
    assert!(shared.starts_with("sha256-"));
    assert_eq!(shared, wirephoto::preview::key(&copy, 640));
    assert!(disk.contains(&shared).unwrap());
    for r in [&a, &copy] {
        let old = format!("{}:{}:640", r.id, r.etag());
        assert!(
            !disk.contains(&old).unwrap(),
            "{old} moved to the content key"
        );
    }
    // Served from the shared one.
    let renders = t.app.previews.renders();
    let lib = t.app.libraries[0].clone();
    let src = wirephoto::preview::Source::File(lib.fs.clone(), copy.path.clone());
    let (_, timing) = t.app.previews.get(&copy, src, 640).await.unwrap();
    assert!(timing.hit);
    assert_eq!(t.app.previews.renders(), renders);
}

#[tokio::test]
async fn a_moved_folder_is_listed_with_everything_below_it() {
    let t = setup().await;
    let a = row_at(&t, "2026/Lisbon/a.jpg").unwrap();
    std::fs::rename(t.lib.join("2026"), t.lib.join("2027")).unwrap();
    // Arrived while the folder was moving: under a subfolder the index knows, not yet listed there.
    std::fs::write(t.lib.join("2027/Lisbon/c.jpg"), jpeg(800, 600)).unwrap();
    let (app, lib) = (t.app.clone(), t.app.libraries[0].clone());
    tokio::task::spawn_blocking(move || {
        wirephoto::indexer::sync_paths(&app, &lib, ["".to_string()].into_iter().collect())
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        row_at(&t, "2027/Lisbon/a.jpg").unwrap().id,
        a.id,
        "kept its ID"
    );
    assert!(
        row_at(&t, "2027/Lisbon/c.jpg").is_some(),
        "found below the moved folder"
    );
    assert!(row_at(&t, "2026/Lisbon/a.jpg").is_none());
}

#[tokio::test]
async fn previews_of_deleted_files_are_swept() {
    let t = setup().await;
    let (a, b) = (
        row_at(&t, "2026/Lisbon/a.jpg").unwrap(),
        row_at(&t, "2026/Lisbon/b.jpg").unwrap(),
    );
    let disk = t.app.previews.disk().clone();
    let (ka, kb) = (
        wirephoto::preview::key(&a, 640),
        wirephoto::preview::key(&b, 640),
    );
    assert!(disk.contains(&ka).unwrap() && disk.contains(&kb).unwrap());
    // Written under the entry key by an older server, for a file hashed since: moved, not dropped.
    let old_key = format!("{}:{}:1280", a.id, a.etag());
    disk.put(&old_key, b"older-preview").unwrap();
    std::fs::remove_file(t.lib.join("2026/Lisbon/b.jpg")).unwrap();
    rescan(&t).await;
    let app = t.app.clone();
    let n = tokio::task::spawn_blocking(move || wirephoto::indexer::sweep_previews(&app))
        .await
        .unwrap()
        .unwrap();
    assert!(n >= 1);
    assert!(!disk.contains(&kb).unwrap(), "b's previews went with it");
    assert!(disk.contains(&ka).unwrap(), "a's stayed");
    assert!(!disk.contains(&old_key).unwrap());
    assert_eq!(
        disk.get(&wirephoto::preview::key(&a, 1280))
            .unwrap()
            .as_deref(),
        Some(&b"older-preview"[..]),
        "moved to the content key"
    );
}

#[tokio::test]
async fn changes_between_registering_the_watcher_and_applying_it_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("photos");
    std::fs::create_dir_all(lib.join("in")).unwrap();
    std::fs::write(lib.join("in/a.jpg"), jpeg(400, 300)).unwrap();
    let config: wirephoto::config::Config = toml::from_str(&format!(
        "data = {:?}\n[[library]]\nid = 'nas'\nname = 'NAS'\npath = {:?}\nwatch = 'auto'\n[decode]\nprogram = {:?}\n[cache]\npregenerate = false\n",
        dir.path().join("data"),
        lib,
        env!("CARGO_BIN_EXE_wirephoto")
    ))
    .unwrap();
    let (app, _router) = wirephoto::build(config).unwrap();
    let l = app.libraries[0].clone();
    let watching = wirephoto::indexer::begin_watch(&app, &l);
    let (a, l2) = (app.clone(), l.clone());
    tokio::task::spawn_blocking(move || wirephoto::indexer::scan(&a, &l2))
        .await
        .unwrap()
        .unwrap();
    // After the scan listed the folder, before changes are applied: this used to be lost until a restart.
    std::fs::write(lib.join("in/late.jpg"), jpeg(400, 300)).unwrap();
    wirephoto::indexer::watch(app.clone(), l.clone(), watching);
    let found = async {
        loop {
            let hit = app
                .db
                .read(|c| Ok(wirephoto::db::entry_by_path(c, &l.api_id, "in/late.jpg")?))
                .unwrap();
            if hit.is_some() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), found)
        .await
        .expect("the change made before watching was applied");
}

#[tokio::test]
async fn renames_moves_and_restores_never_replace() {
    let t = setup().await;
    let (a, b) = (
        t.entry_at("/2026/Lisbon/a.jpg").await,
        t.entry_at("/2026/Lisbon/b.jpg").await,
    );
    let a_bytes = std::fs::read(t.lib.join("2026/Lisbon/a.jpg")).unwrap();
    // Renamed onto a taken name: the next free one; the other file is untouched.
    let r = t
        .json(
            "PATCH",
            &format!("/v1/entries/{}", b["id"].as_str().unwrap()),
            json!({"name": "a.jpg"}),
        )
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["name"], "a-1.jpg");
    assert_eq!(
        std::fs::read(t.lib.join("2026/Lisbon/a.jpg")).unwrap(),
        a_bytes
    );
    // Trashed, its name taken meanwhile, restored: beside the newcomer.
    let aid = a["id"].as_str().unwrap();
    let r = t
        .send(
            t.req("DELETE", &format!("/v1/entries/{aid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 204);
    std::fs::write(t.lib.join("2026/Lisbon/a.jpg"), b"newcomer").unwrap();
    rescan(&t).await;
    let r = t
        .json(
            "PATCH",
            &format!("/v1/entries/{aid}"),
            json!({"trashed": false}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["name"], "a-2.jpg");
    assert_eq!(
        std::fs::read(t.lib.join("2026/Lisbon/a.jpg")).unwrap(),
        b"newcomer"
    );
    assert_eq!(
        std::fs::read(t.lib.join("2026/Lisbon/a-2.jpg")).unwrap(),
        a_bytes
    );
}

#[tokio::test]
async fn a_crash_during_pregeneration_defers_and_a_restart_retries() {
    // The first worker dies on its first request: a broken environment, not a broken file.
    let scratch = tempfile::tempdir().unwrap();
    let script = scratch.path().join("decoder.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nif mkdir {dir:?}/crashed 2>/dev/null; then head -c 1 >/dev/null; kill -SEGV $$; fi\nexec {exe:?} \"$@\"\n",
            dir = scratch.path(),
            exe = env!("CARGO_BIN_EXE_wirephoto"),
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let t = setup_with(&format!("[decode]\nprogram = {script:?}\nworkers = 1\n")).await;
    let lib = t.app.libraries[0].api_id.clone();
    let count = |sql: &'static str| {
        let lib = lib.clone();
        t.app
            .db
            .read(move |c| Ok(c.query_row(sql, [&lib], |r| r.get::<_, i64>(0))?))
            .unwrap()
    };
    let deferred = "SELECT COUNT(*) FROM entries WHERE library = ? AND preview_done = 2";
    let unpreviewable =
        "SELECT COUNT(*) FROM entries WHERE library = ? AND kind = 1 AND previewable = 0";
    assert_eq!(count(deferred), 1, "the crashed one waits for a retry");
    assert_eq!(count(unpreviewable), 0, "and is not judged unpreviewable");
    // A restart retries it.
    wirephoto::indexer::retry_failed_previews(&t.app).unwrap();
    wirephoto::indexer::settle(&t.app).await.unwrap();
    assert_eq!(count(deferred), 0);
    let done =
        "SELECT COUNT(*) FROM entries WHERE library = ? AND previewable = 1 AND preview_done = 1";
    assert_eq!(
        count(done),
        3,
        "a.jpg, b.jpg and logo.png all have their preview"
    );
}

#[tokio::test]
async fn per_folder_watches_follow_new_folders_and_skip_ignored_ones() {
    // SAFETY: tests that read it only ever see it set; per-folder watching is what Linux always does.
    unsafe { std::env::set_var("WIREPHOTO_WATCH_PER_FOLDER", "1") };
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("photos");
    std::fs::create_dir_all(lib.join("in/@eaDir/x.jpg")).unwrap();
    std::fs::write(lib.join("in/a.jpg"), jpeg(400, 300)).unwrap();
    let config: wirephoto::config::Config = toml::from_str(&format!(
        "data = {:?}\n[[library]]\nid = 'nas'\nname = 'NAS'\npath = {:?}\nwatch = 'auto'\n[decode]\nprogram = {:?}\n[cache]\npregenerate = false\n",
        dir.path().join("data"),
        lib,
        env!("CARGO_BIN_EXE_wirephoto")
    ))
    .unwrap();
    let (app, _router) = wirephoto::build(config).unwrap();
    let l = app.libraries[0].clone();
    let watching = wirephoto::indexer::begin_watch(&app, &l);
    let (a, l2) = (app.clone(), l.clone());
    tokio::task::spawn_blocking(move || wirephoto::indexer::scan(&a, &l2))
        .await
        .unwrap()
        .unwrap();
    wirephoto::indexer::watch(app.clone(), l.clone(), watching);
    let has = |path: &'static str| {
        let (app, l) = (app.clone(), l.clone());
        async move {
            for _ in 0..100 {
                if app
                    .db
                    .read(|c| Ok(wirephoto::db::entry_by_path(c, &l.api_id, path)?))
                    .unwrap()
                    .is_some()
                {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            false
        }
    };
    // A folder made after start, then a file in it: the folder gets its own watch.
    std::fs::create_dir(lib.join("in/new")).unwrap();
    assert!(has("in/new").await, "the new folder is indexed");
    // Time for the new watch to be in place: immediate with inotify (Linux); macOS's FSEvents restarts its
    // stream to add a path, which takes a moment.
    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
    std::fs::write(lib.join("in/new/b.jpg"), jpeg(400, 300)).unwrap();
    assert!(
        has("in/new/b.jpg").await,
        "a file in the new folder is seen"
    );
    assert!(
        l.live.load(std::sync::atomic::Ordering::Relaxed),
        "still watching, not polling"
    );
    assert!(
        !has("in/@eaDir").await || false,
        "ignored folders are neither indexed nor watched"
    );
}

#[tokio::test]
async fn the_audit_trail_records_who_did_what() {
    let t = setup().await;
    let folder = t.entry_at("/2026").await;
    let fid = folder["id"].as_str().unwrap();
    let put = |name: &str, body: Vec<u8>| {
        t.req("PUT", &format!("/v1/entries/{fid}/children/{name}"))
            .body(Body::from(body))
            .unwrap()
    };
    let bytes = jpeg(300, 200);
    let e = t.send(put("x.jpg", bytes.clone())).await.json();
    assert_eq!(t.send(put("again.jpg", bytes)).await.status, 200);
    let id = e["id"].as_str().unwrap();
    assert_eq!(
        t.json(
            "PATCH",
            &format!("/v1/entries/{id}"),
            json!({"name": "y.jpg"})
        )
        .await
        .status,
        200
    );
    assert_eq!(
        t.send(
            t.req("DELETE", &format!("/v1/entries/{id}"))
                .body(Body::empty())
                .unwrap()
        )
        .await
        .status,
        204
    );
    // A refused request.
    let r = t
        .send(
            Request::get(format!("/v1/entries/{fid}"))
                .header(header::AUTHORIZATION, "Bearer wp_nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 401);
    // The denied row is written off the request path; give it a moment.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let rows = wirephoto::audit::list(&t.app.db, 0, None, None, 100).unwrap();
    let ops: Vec<&str> = rows.iter().map(|r| r.op.as_str()).collect();
    assert_eq!(
        ops,
        ["upload", "upload", "move", "delete", "denied"],
        "{rows:#?}"
    );
    assert_eq!(rows[0].status, 201);
    assert_eq!(rows[1].status, 200, "the second upload stored nothing");
    assert!(rows[1].detail.contains("already there as x.jpg"));
    assert!(rows[2].detail.contains("2026/x.jpg -> 2026/y.jpg"));
    assert!(rows[..4].iter().all(|r| r.key.as_deref() == Some("lith")));
    assert_eq!(rows[4].status, 401);
    assert!(rows[4].key.is_none());
}

/// A JPEG whose header is fine (so its facts are read) but whose quantisation table is bogus: the decoder's
/// verdict, not an accident.
fn broken_jpeg() -> Vec<u8> {
    let mut b = vec![
        0xFF, 0xD8, 0xFF, 0xC0, 0, 11, 8, 0, 200, 1, 44, 1, 1, 0x11, 0,
    ];
    b.extend_from_slice(&[0xFF, 0xDB, 0, 67, 0x0F]);
    b.extend_from_slice(&[1u8; 64]);
    b.extend_from_slice(&[0xFF, 0xDA, 0, 8, 1, 1, 0, 0, 63, 0, 1, 2, 3, 0xFF, 0xD9]);
    b
}

#[tokio::test]
async fn a_file_the_decoder_rejects_is_unpreviewable_for_good() {
    let t = setup().await;
    std::fs::write(t.lib.join("2026/broken.jpg"), broken_jpeg()).unwrap();
    rescan(&t).await;
    let row = row_at(&t, "2026/broken.jpg").unwrap();
    assert!(
        !row.previewable,
        "the decoder's verdict is final, not retried at every start"
    );
    assert_eq!(
        t.app
            .db
            .read(|c| Ok(c.query_row(
                "SELECT preview_done FROM entries WHERE id = ?",
                [&row.id],
                |r| r.get::<_, i64>(0)
            )?))
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn pause_holds_background_work_and_resume_releases_it() {
    let t = setup().await;
    wirephoto::indexer::set_paused(&t.app.db, true).unwrap();
    std::fs::write(t.lib.join("2026/new.jpg"), jpeg(500, 400)).unwrap();
    let (a, l) = (t.app.clone(), t.app.libraries[0].clone());
    tokio::task::spawn_blocking(move || wirephoto::indexer::scan(&a, &l))
        .await
        .unwrap()
        .unwrap();
    tokio::spawn(wirephoto::indexer::background(t.app.clone()));
    let read = || row_at(&t, "2026/new.jpg").unwrap().media_state != 0;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert!(
        row_at(&t, "2026/new.jpg").is_some(),
        "listed even while paused"
    );
    assert!(!read(), "but its facts wait");
    wirephoto::indexer::set_paused(&t.app.db, false).unwrap();
    let mut done = false;
    for _ in 0..100 {
        if read() {
            done = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(done, "resumed within the check interval");
}

#[tokio::test]
async fn background_work_steps_aside_while_the_api_is_busy() {
    let t = setup_with("[background]\nyield_seconds = 1\n").await;
    std::fs::write(t.lib.join("2026/new.jpg"), jpeg(500, 400)).unwrap();
    let (a, l) = (t.app.clone(), t.app.libraries[0].clone());
    tokio::task::spawn_blocking(move || wirephoto::indexer::scan(&a, &l))
        .await
        .unwrap()
        .unwrap();
    // A request: hashing waits for the API to go idle.
    assert_eq!(t.get("/v1/libraries").await.status, 200);
    assert!(t.app.api_busy());
    wirephoto::indexer::settle(&t.app).await.unwrap();
    assert!(
        row_at(&t, "2026/new.jpg").unwrap().sha256.is_none(),
        "not hashed while busy"
    );
    let row = row_at(&t, "2026/new.jpg").unwrap();
    assert!(
        t.app
            .previews
            .disk()
            .contains(&wirephoto::preview::key(&row, 640))
            .unwrap(),
        "previews still go on, one at a time"
    );
    // Idle again.
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert!(!t.app.api_busy());
    wirephoto::indexer::settle(&t.app).await.unwrap();
    assert!(row_at(&t, "2026/new.jpg").unwrap().sha256.is_some());
}

#[tokio::test]
async fn only_if_cached_never_renders_from_the_original() {
    let t = setup().await;
    let a = t.entry_at("/2026/Lisbon/a.jpg").await;
    let tpl = a["urls"]["preview"].as_str().unwrap().to_string();
    let at = |size: u32, cached: bool| {
        let mut u = tpl.replace("{size}", &size.to_string());
        if cached {
            u.push_str("&cached=only");
        }
        Request::get(u).body(Body::empty()).unwrap()
    };
    let renders = t.app.previews.renders();
    let r = t.send(at(2560, true)).await;
    assert_eq!(r.status, 504, "not made yet, and not made now");
    assert_eq!(r.json()["code"], "not-cached");
    assert_eq!(t.app.previews.renders(), renders, "nothing rendered");
    // The pre-generated 640, and a smaller one made from it, count as cached.
    assert_eq!(t.send(at(640, true)).await.status, 200);
    assert_eq!(t.send(at(320, true)).await.status, 200);
    // The header form.
    let r = t
        .send(
            Request::get(tpl.replace("{size}", "1280"))
                .header(header::CACHE_CONTROL, "only-if-cached")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 504);
    // Once made, it is served.
    assert_eq!(t.send(at(2560, false)).await.status, 200);
    let r = t.send(at(2560, true)).await;
    assert_eq!(r.status, 200);
    assert_eq!(
        decode_size(&r.body).0,
        1600,
        "never larger than the original"
    );
}

#[tokio::test]
async fn hashing_runs_beside_previews_not_after_them() {
    // Decode workers that take 8 s to start, on a fresh server (no worker started yet): the first
    // preview is stuck for the whole test.
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("slow.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nsleep 8\nexec {:?} \"$@\"\n",
            env!("CARGO_BIN_EXE_wirephoto")
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let lib = dir.path().join("photos");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("a.jpg"), jpeg(500, 400)).unwrap();
    let config: wirephoto::config::Config = toml::from_str(&format!(
        "data = {:?}\n[[library]]\nid = 'nas'\nname = 'NAS'\npath = {:?}\nwatch = 'poll'\npoll_seconds = 3600\n[decode]\nprogram = {script:?}\nworkers = 1\n",
        dir.path().join("data"),
        lib
    ))
    .unwrap();
    let (app, _router) = wirephoto::build(config).unwrap();
    let (a, l) = (app.clone(), app.libraries[0].clone());
    tokio::task::spawn_blocking(move || wirephoto::indexer::scan(&a, &l))
        .await
        .unwrap()
        .unwrap();
    tokio::spawn(wirephoto::indexer::background(app.clone()));
    let row = || {
        let l = app.libraries[0].api_id.clone();
        app.db
            .read(move |c| Ok(wirephoto::db::entry_by_path(c, &l, "a.jpg")?))
            .unwrap()
            .unwrap()
    };
    let mut hashed = false;
    for _ in 0..50 {
        if row().sha256.is_some() {
            hashed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        app.previews.disk().total_bytes(),
        0,
        "the preview is still stuck"
    );
    assert!(
        hashed,
        "yet the file is hashed: hashing does not wait for previews"
    );
}

#[tokio::test]
async fn a_file_that_cannot_be_hashed_does_not_stop_the_others() {
    use std::os::unix::fs::PermissionsExt;
    let t = setup().await;
    std::fs::write(t.lib.join("2026/locked.jpg"), jpeg(300, 200)).unwrap();
    std::fs::write(t.lib.join("2026/open.jpg"), jpeg(320, 200)).unwrap();
    std::fs::set_permissions(
        t.lib.join("2026/locked.jpg"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    // Settles (it used to pick the unreadable file again on every pass, for ever).
    tokio::time::timeout(std::time::Duration::from_secs(20), rescan(&t))
        .await
        .expect("the background passes finish");
    assert!(row_at(&t, "2026/open.jpg").unwrap().sha256.is_some());
    assert!(row_at(&t, "2026/locked.jpg").unwrap().sha256.is_none());
    // Readable again: the next start retries it.
    std::fs::set_permissions(
        t.lib.join("2026/locked.jpg"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    wirephoto::indexer::retry_failed_previews(&t.app).unwrap();
    wirephoto::indexer::settle(&t.app).await.unwrap();
    assert!(row_at(&t, "2026/locked.jpg").unwrap().sha256.is_some());
}

/// Bytes that are not any image, `n` long.
fn noise(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u32).wrapping_mul(2654435761).rotate_left(seed as u32) as u8)
        .collect()
}

/// Files and folders deleted on disk behind the server's back, before it has noticed: never taken for being
/// there.
#[tokio::test]
async fn what_is_gone_from_the_disk_is_never_taken_for_there() {
    let t = setup().await;
    let folder = t.entry_at("/2026").await;
    let fid = folder["id"].as_str().unwrap().to_string();
    let bytes = jpeg(640, 480);
    let sha = hex::encode(Sha256::digest(&bytes));
    let put = |name: &str| {
        t.req("PUT", &format!("/v1/entries/{fid}/children/{name}"))
            .body(Body::from(bytes.clone()))
            .unwrap()
    };
    assert_eq!(t.send(put("one.jpg")).await.status, 201);
    std::fs::remove_file(t.lib.join("2026/one.jpg")).unwrap();

    // The same bytes again: stored, not "already there".
    let r = t.send(put("one.jpg")).await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(std::fs::read(t.lib.join("2026/one.jpg")).unwrap(), bytes);

    // A lookup by hash finds only what is on disk.
    std::fs::remove_file(t.lib.join("2026/one.jpg")).unwrap();
    let r = t
        .get(&format!(
            "/v1/entries?library=lib_nas&sha256={sha}&size={}",
            bytes.len()
        ))
        .await;
    assert_eq!(
        r.json()["items"],
        json!([]),
        "a deleted file is not offered for dedup"
    );
    // Nor does a resumable upload take it for there: the bytes are stored.
    let r = t
        .send(
            t.tus_create(&fid, "two.jpg", bytes.len(), Some(&sha))
                .header(header::CONTENT_TYPE, "application/offset+octet-stream")
                .body(Body::from(bytes.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.header("wirephoto-stored").as_deref(), Some("created"));

    // A folder deleted on disk: writing into it is 404, and the index forgets it.
    let gone = t.entry_at("/2026/Lisbon").await;
    let gid = gone["id"].as_str().unwrap().to_string();
    std::fs::remove_dir_all(t.lib.join("2026/Lisbon")).unwrap();
    let r = t
        .send(
            t.req("PUT", &format!("/v1/entries/{gid}/children/x.jpg"))
                .body(Body::from(bytes.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 404);
    assert!(
        !t.lib.join("2026/Lisbon").exists(),
        "nothing made in its place"
    );
    for _ in 0..50 {
        if row_at(&t, "2026/Lisbon").is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        row_at(&t, "2026/Lisbon").is_none(),
        "the index forgot the folder"
    );
    assert!(row_at(&t, "2026/Lisbon/a.jpg").is_none());
}

/// Watching (as on Linux, a watch per folder): a folder deleted on disk leaves the index, and one made again
/// under its name is watched again.
#[tokio::test]
async fn watched_deletes_leave_the_index_and_a_folder_made_again_is_watched() {
    // SAFETY: tests that read it only ever see it set; per-folder watching is what Linux always does.
    unsafe { std::env::set_var("WIREPHOTO_WATCH_PER_FOLDER", "1") };
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("photos");
    std::fs::create_dir_all(lib.join("trip/day1")).unwrap();
    std::fs::write(lib.join("trip/day1/a.jpg"), jpeg(400, 300)).unwrap();
    let config: wirephoto::config::Config = toml::from_str(&format!(
        "data = {:?}\n[[library]]\nid = 'nas'\nname = 'NAS'\npath = {:?}\nwatch = 'auto'\n[decode]\nprogram = {:?}\n[cache]\npregenerate = false\n",
        dir.path().join("data"),
        lib,
        env!("CARGO_BIN_EXE_wirephoto")
    ))
    .unwrap();
    let (app, _router) = wirephoto::build(config).unwrap();
    let l = app.libraries[0].clone();
    let watching = wirephoto::indexer::begin_watch(&app, &l);
    let (a, l2) = (app.clone(), l.clone());
    tokio::task::spawn_blocking(move || wirephoto::indexer::scan(&a, &l2))
        .await
        .unwrap()
        .unwrap();
    wirephoto::indexer::watch(app.clone(), l.clone(), watching);
    let indexed = |path: &'static str, want: bool| {
        let (app, l) = (app.clone(), l.clone());
        async move {
            for _ in 0..100 {
                let there = app
                    .db
                    .read(|c| Ok(wirephoto::db::entry_by_path(c, &l.api_id, path)?))
                    .unwrap()
                    .is_some();
                if there == want {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            false
        }
    };
    assert!(indexed("trip/day1/a.jpg", true).await);
    std::fs::remove_dir_all(lib.join("trip/day1")).unwrap();
    assert!(
        indexed("trip/day1", false).await,
        "the deleted folder left the index"
    );
    assert!(indexed("trip/day1/a.jpg", false).await);
    // Made again under the same name; then a file in it (after its watch is in place).
    std::fs::create_dir(lib.join("trip/day1")).unwrap();
    assert!(indexed("trip/day1", true).await);
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    std::fs::write(lib.join("trip/day1/b.jpg"), jpeg(400, 300)).unwrap();
    assert!(
        indexed("trip/day1/b.jpg", true).await,
        "the folder made again is watched"
    );
}

impl T {
    /// `POST /v1/tus` for a file of `len` bytes named `name` in `parent`.
    fn tus_create(
        &self,
        parent: &str,
        name: &str,
        len: usize,
        sha256: Option<&str>,
    ) -> axum::http::request::Builder {
        let b = |s: &str| wirephoto::util::b64(s.as_bytes());
        let mut meta = format!("parent {},filename {}", b(parent), b(name));
        if let Some(h) = sha256 {
            meta.push_str(&format!(",sha256 {}", b(h)));
        }
        self.req("POST", "/v1/tus")
            .header("tus-resumable", "1.0.0")
            .header("upload-length", len.to_string())
            .header("upload-metadata", meta)
    }

    fn tus_patch(&self, id: &str, offset: usize) -> axum::http::request::Builder {
        self.req("PATCH", &format!("/v1/tus/{id}"))
            .header("tus-resumable", "1.0.0")
            .header("upload-offset", offset.to_string())
            .header(header::CONTENT_TYPE, "application/offset+octet-stream")
    }

    async fn tus_head(&self, id: &str) -> Res {
        self.send(
            self.req("HEAD", &format!("/v1/tus/{id}"))
                .header("tus-resumable", "1.0.0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }
}

fn tus_id(r: &Res) -> String {
    r.header("location")
        .unwrap()
        .strip_prefix("/v1/tus/")
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn tus_whole_file_resumes_after_a_broken_connection() {
    let t = setup().await;
    assert_eq!(t.get("/v1").await.json()["uploads"]["tus"], "/v1/tus");
    let r = t
        .send(
            Request::builder()
                .method("OPTIONS")
                .uri("/v1/tus")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.header("tus-resumable").as_deref(), Some("1.0.0"));
    assert_eq!(r.header("tus-version").as_deref(), Some("1.0.0"));
    assert!(
        r.header("tus-extension")
            .unwrap()
            .contains("creation-with-upload")
    );
    assert!(r.header("tus-checksum-algorithm").unwrap().contains("sha1"));

    let folder = t.entry_at("/2026").await;
    let fid = folder["id"].as_str().unwrap();
    let bytes = noise(3_000_000, 4);
    let r = t
        .send(
            t.tus_create(fid, "big.raw", bytes.len(), None)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.header("tus-resumable").as_deref(), Some("1.0.0"));
    assert!(r.header("upload-expires").unwrap().ends_with("GMT"));
    let id = tus_id(&r);
    let r = t.tus_head(&id).await;
    assert_eq!(
        (r.status, r.header("upload-offset").as_deref()),
        (StatusCode::OK, Some("0"))
    );
    assert_eq!(r.header("upload-length"), Some(bytes.len().to_string()));
    assert_eq!(r.header("cache-control").as_deref(), Some("no-store"));

    // The whole file in one PATCH, and the connection breaks after the first million bytes: they are kept.
    let first = bytes[..1_000_000].to_vec();
    let stream = futures::stream::iter(vec![
        Ok::<_, std::io::Error>(bytes::Bytes::from(first)),
        Err(std::io::Error::other("connection reset")),
    ]);
    let r = t
        .send(t.tus_patch(&id, 0).body(Body::from_stream(stream)).unwrap())
        .await;
    assert_ne!(r.status, 204);
    let r = t.tus_head(&id).await;
    assert_eq!(r.header("upload-offset").as_deref(), Some("1000000"));

    // The wrong offset, a missing Tus-Resumable, a checksum that does not match: refused, nothing moves.
    let r = t
        .send(t.tus_patch(&id, 0).body(Body::from(bytes.clone())).unwrap())
        .await;
    assert_eq!(r.status, 409);
    let r = t
        .send(
            t.req("PATCH", &format!("/v1/tus/{id}"))
                .header("upload-offset", "1000000")
                .header(header::CONTENT_TYPE, "application/offset+octet-stream")
                .body(Body::from(bytes[1_000_000..].to_vec()))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 412);
    let sha1 = |b: &[u8]| format!("sha1 {}", wirephoto::util::b64(&sha1::Sha1::digest(b)));
    let r = t
        .send(
            t.tus_patch(&id, 1_000_000)
                .header("upload-checksum", sha1(b"other bytes"))
                .body(Body::from(bytes[1_000_000..].to_vec()))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status.as_u16(), 460);
    assert_eq!(
        t.tus_head(&id).await.header("upload-offset").as_deref(),
        Some("1000000")
    );

    // The rest, checked: the file is stored, and the answer says where.
    let r = t
        .send(
            t.tus_patch(&id, 1_000_000)
                .header("upload-checksum", sha1(&bytes[1_000_000..]))
                .body(Body::from(bytes[1_000_000..].to_vec()))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 204, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.header("upload-offset"), Some(bytes.len().to_string()));
    assert_eq!(r.header("wirephoto-stored").as_deref(), Some("created"));
    assert_eq!(
        r.header("wirephoto-entry-name").as_deref(),
        Some("big%2Eraw")
    );
    let entry = r.header("wirephoto-entry").unwrap();
    assert_eq!(std::fs::read(t.lib.join("2026/big.raw")).unwrap(), bytes);
    assert_eq!(
        std::fs::read_dir(t.lib.join(".wp-uploads"))
            .unwrap()
            .count(),
        0,
        "moved, not copied"
    );
    // For a client whose answer was lost.
    let r = t.get(&format!("/v1/tus/{id}")).await;
    assert_eq!(r.json()["entry"]["id"], entry);
    assert_eq!(r.json()["created"], true);
    assert_eq!(
        r.json()["entry"]["sha256"].as_str().unwrap(),
        hex::encode(Sha256::digest(&bytes))
    );
}

#[tokio::test]
async fn tus_creation_with_upload_dedup_names_keys_and_expiry() {
    let t = setup().await;
    let folder = t.entry_at("/2026").await;
    let fid = folder["id"].as_str().unwrap().to_string();
    let octets = |b: axum::http::request::Builder| {
        b.header(header::CONTENT_TYPE, "application/offset+octet-stream")
    };
    let bytes = noise(50_000, 7);
    let sha = hex::encode(Sha256::digest(&bytes));

    // A small file in one request.
    let r = t
        .send(
            octets(t.tus_create(&fid, "x.bin", bytes.len(), Some(&sha)))
                .body(Body::from(bytes.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.header("upload-offset"), Some(bytes.len().to_string()));
    assert_eq!(r.header("wirephoto-stored").as_deref(), Some("created"));
    let first = r.header("wirephoto-entry").unwrap();
    // The same bytes again: the file the folder has.
    let r = t
        .send(
            octets(t.tus_create(&fid, "copy.bin", bytes.len(), None))
                .body(Body::from(bytes.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(r.header("wirephoto-stored").as_deref(), Some("existed"));
    assert_eq!(r.header("wirephoto-entry"), Some(first.clone()));
    // Other bytes under a taken name: the next free one, through a proxy that refuses PATCH.
    let other = noise(70_000, 8);
    let r = t
        .send(
            t.tus_create(&fid, "x.bin", other.len(), None)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let id = tus_id(&r);
    let r = t
        .send(
            t.req("POST", &format!("/v1/tus/{id}"))
                .header("x-http-method-override", "PATCH")
                .header("tus-resumable", "1.0.0")
                .header("upload-offset", "0")
                .header(header::CONTENT_TYPE, "application/offset+octet-stream")
                .body(Body::from(other.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 204, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(
        r.header("wirephoto-entry-name").as_deref(),
        Some("x%2D1%2Ebin")
    );
    assert_eq!(
        std::fs::read(t.lib.join("2026/x.bin")).unwrap(),
        bytes,
        "never overwritten"
    );

    // Bytes that do not match the sha256 they were announced with: nothing stored.
    let r = t
        .send(
            octets(t.tus_create(&fid, "bad.bin", other.len(), Some(&sha)))
                .body(Body::from(other.clone()))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status.as_u16(), 460);
    assert!(!t.lib.join("2026/bad.bin").exists());

    // Someone else's upload is not found; a read-only key creates none.
    let r = t
        .send(
            t.tus_create(&fid, "y.bin", 10, None)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let id = tus_id(&r);
    let (_, viewer) = Keys::create(&t.app.db, "viewer", Scope::Read, None, None).unwrap();
    let as_viewer = |b: axum::http::request::Builder| {
        let mut b = b;
        b.headers_mut().unwrap().insert(
            header::AUTHORIZATION,
            format!("Bearer {viewer}").parse().unwrap(),
        );
        b
    };
    let r = t
        .send(
            as_viewer(
                t.req("HEAD", &format!("/v1/tus/{id}"))
                    .header("tus-resumable", "1.0.0"),
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await;
    assert_eq!(r.status, 404);
    let r = t
        .send(
            as_viewer(t.tus_create(&fid, "z.bin", 10, None))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 403);

    // Terminated.
    let r = t
        .send(
            t.req("DELETE", &format!("/v1/tus/{id}"))
                .header("tus-resumable", "1.0.0")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 204);
    assert_eq!(t.tus_head(&id).await.status, 404);
    assert!(!t.lib.join(format!(".wp-uploads/{id}")).exists());

    // Expired: unknown, and swept with its bytes.
    let r = t
        .send(
            t.tus_create(&fid, "w.bin", 10, None)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    let id = tus_id(&r);
    t.app
        .db
        .write(|tx| {
            tx.execute("UPDATE tus SET expires = 0 WHERE id = ?", [&id])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(t.tus_head(&id).await.status, 404);
    assert_eq!(wirephoto::tus::purge(&t.app).unwrap(), 1);
    assert!(!t.lib.join(format!(".wp-uploads/{id}")).exists());

    // A folder gone from the disk: 404.
    let gone = t.entry_at("/2026/Lisbon").await;
    std::fs::remove_dir_all(t.lib.join("2026/Lisbon")).unwrap();
    let r = t
        .send(
            t.tus_create(gone["id"].as_str().unwrap(), "v.bin", 10, None)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 404);
}
