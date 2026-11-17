//! Il sito staff compilato (`cogs/sfo_site.py`): la pagina `/staff/`, i suoi tre file fissi e la
//! cartella `assets/` prodotta dal build, tutto da `SFO_SITE_DIR` (di norma `sfo-site/dist`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::site_common::{plain, redirect, status_text};

/// I file fissi serviti da `/staff/<nome>`.
const SITE_ASSETS: [&str; 3] = ["app.js", "styles.css", "favicon.ico"];

/// Le intestazioni di ogni file del sito.
const SITE_HEADERS: [(&str, &str); 5] = [
    ("cache-control", "no-cache"),
    ("x-content-type-options", "nosniff"),
    ("x-frame-options", "DENY"),
    ("referrer-policy", "strict-origin-when-cross-origin"),
    ("permissions-policy", "camera=(), microphone=(), geolocation=()"),
];

/// Il tipo di un file dall'estensione (`mimetypes.guess_type`, per quelle che produce il build).
fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("html" | "htm") => "text/html",
        Some("js" | "mjs") => "text/javascript",
        Some("css") => "text/css",
        Some("ico") => "image/x-icon",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("json") => "application/json",
        Some("txt" | "map") => "text/plain",
        _ => "application/octet-stream",
    }
}

fn file_response(path: &Path) -> Response {
    let Ok(body) = std::fs::read(path) else {
        return status_text(StatusCode::NOT_FOUND);
    };
    let mut response = (StatusCode::OK, body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type(path)));
    for (name, value) in SITE_HEADERS {
        headers.insert(HeaderName::from_static(name), HeaderValue::from_static(value));
    }
    response
}

pub fn routes(site_dir: PathBuf) -> Router {
    Router::new()
        .route("/staff", get(|| async { redirect(StatusCode::PERMANENT_REDIRECT, "/staff/") }))
        .route("/staff/", get(index))
        .route("/staff/assets/{asset}", get(built_asset))
        .route("/staff/{asset}", get(fixed_asset))
        .with_state(Arc::new(site_dir))
}

async fn index(State(root): State<Arc<PathBuf>>) -> Response {
    let path = root.join("index.html");
    if !path.is_file() {
        return plain(StatusCode::SERVICE_UNAVAILABLE, "Sito SFO non disponibile");
    }
    file_response(&path)
}

async fn fixed_asset(State(root): State<Arc<PathBuf>>, UrlPath(name): UrlPath<String>) -> Response {
    if !SITE_ASSETS.contains(&name.as_str()) {
        return status_text(StatusCode::NOT_FOUND);
    }
    let path = root.join(&name);
    if !path.is_file() {
        return status_text(StatusCode::NOT_FOUND);
    }
    file_response(&path)
}

/// Un file di `assets/`: solo il nome, mai un percorso, e solo se sta davvero in quella cartella (un
/// collegamento simbolico verso altrove non vale).
async fn built_asset(State(root): State<Arc<PathBuf>>, UrlPath(name): UrlPath<String>) -> Response {
    if name.is_empty() || name == "." || name.contains(['/', '\\', '\0']) {
        return status_text(StatusCode::NOT_FOUND);
    }
    let Ok(asset_root) = root.join("assets").canonicalize() else {
        return status_text(StatusCode::NOT_FOUND);
    };
    let Ok(path) = asset_root.join(&name).canonicalize() else {
        return status_text(StatusCode::NOT_FOUND);
    };
    if path.parent() != Some(asset_root.as_path()) || !path.is_file() {
        return status_text(StatusCode::NOT_FOUND);
    }
    file_response(&path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use tower::ServiceExt;

    fn golden() -> Value {
        serde_json::from_str(include_str!("golden/site_auth_expected.json")).unwrap()
    }

    /// La stessa cartella usata per generare il golden Python.
    fn site_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("assets")).unwrap();
        for (name, content) in [
            ("index.html", &b"<html>staff</html>"[..]),
            ("app.js", b"console.log(1)"),
            ("styles.css", b"body{}"),
            ("favicon.ico", b"\x00\x00\x01\x00"),
            ("secret.txt", b"segreto"),
            ("assets/index-abc.js", b"export{}"),
            ("assets/index-abc.css", b"a{}"),
            ("assets/logo.svg", b"<svg/>"),
            ("assets/logo.png", b"\x89PNG"),
            ("assets/font.woff2", b"wOF2"),
            ("assets/app.js.map", b"{}"),
            ("assets/data.json", b"{}"),
            ("assets/icon.ico", b"\x00\x00\x01\x00"),
            ("assets/page.html", b"<p/>"),
            ("assets/noext", b"x"),
        ] {
            std::fs::write(root.join(name), content).unwrap();
        }
        dir
    }

    async fn call(router: &Router, method: &str, path: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let request = Request::builder().method(method).uri(path).body(Body::empty()).unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        (parts.status, parts.headers, axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec())
    }

    #[tokio::test]
    async fn static_site_matches_the_python_implementation() {
        let dir = site_dir();
        let router = routes(dir.path().to_path_buf());
        let empty = tempfile::tempdir().unwrap();
        let empty_router = routes(empty.path().to_path_buf());
        let golden = golden();
        let cases = golden["static"].as_array().unwrap();
        assert_eq!(cases.len(), 29);
        for case in cases {
            let label = case["label"].as_str().unwrap();
            // Il client di Python normalizza `/..` prima di inviarlo: non è un caso del server.
            let (router, method, path) = match label {
                "redirect" => (&router, "GET", "/staff"),
                "indice" => (&router, "GET", "/staff/"),
                "app_js" => (&router, "GET", "/staff/app.js"),
                "styles" => (&router, "GET", "/staff/styles.css"),
                "favicon" => (&router, "GET", "/staff/favicon.ico"),
                "altro_file" => (&router, "GET", "/staff/secret.txt"),
                "indice_come_asset" => (&router, "GET", "/staff/index.html"),
                "asset_js" => (&router, "GET", "/staff/assets/index-abc.js"),
                "asset_css" => (&router, "GET", "/staff/assets/index-abc.css"),
                "asset_svg" => (&router, "GET", "/staff/assets/logo.svg"),
                "asset_png" => (&router, "GET", "/staff/assets/logo.png"),
                "asset_woff2" => (&router, "GET", "/staff/assets/font.woff2"),
                "asset_map" => (&router, "GET", "/staff/assets/app.js.map"),
                "asset_json" => (&router, "GET", "/staff/assets/data.json"),
                "asset_ico" => (&router, "GET", "/staff/assets/icon.ico"),
                "asset_html" => (&router, "GET", "/staff/assets/page.html"),
                "asset_senza_estensione" => (&router, "GET", "/staff/assets/noext"),
                "asset_mancante" => (&router, "GET", "/staff/assets/nessuno.js"),
                "asset_percorso_relativo" => (&router, "GET", "/staff/assets/..%2Findex.html"),
                "asset_punti" => (&router, "GET", "/staff/assets/.."),
                "asset_cartella" => (&router, "GET", "/staff/assets/%2E"),
                "asset_backslash" => (&router, "GET", "/staff/assets/..%5Cindex.html"),
                "asset_maiuscolo" => (&router, "GET", "/staff/APP.JS"),
                "post_su_indice" => (&router, "POST", "/staff/"),
                "head_indice" => (&router, "HEAD", "/staff/"),
                "senza_sito_indice" => (&empty_router, "GET", "/staff/"),
                "senza_sito_app" => (&empty_router, "GET", "/staff/app.js"),
                "senza_sito_asset" => (&empty_router, "GET", "/staff/assets/index-abc.js"),
                "senza_sito_redirect" => (&empty_router, "GET", "/staff"),
                other => panic!("caso sconosciuto: {other}"),
            };
            let (status, headers, body) = call(router, method, path).await;
            let expected_status = case["status"].as_u64().unwrap();
            if label == "asset_punti" {
                // Il server dice 404 (il client di Python aveva già riscritto il percorso in `/staff/`).
                assert_eq!(status, StatusCode::NOT_FOUND, "{label}");
                continue;
            }
            if method == "POST" {
                assert_eq!(u64::from(status.as_u16()), expected_status, "{label}");
                continue;
            }
            assert_eq!(u64::from(status.as_u16()), expected_status, "{label}: stato");
            let expected_headers = case["headers"].as_object().unwrap();
            for (name, value) in expected_headers {
                assert_eq!(headers.get(name.as_str()).and_then(|v| v.to_str().ok()), value.as_str(), "{label}: {name}");
            }
            if expected_status == 200 && method == "GET" {
                let digest: String = Sha256::digest(&body).iter().map(|byte| format!("{byte:02x}")).collect();
                assert_eq!(digest, case["sha256"].as_str().unwrap(), "{label}: contenuto");
                assert_eq!(body.len() as u64, case["length"].as_u64().unwrap(), "{label}: lunghezza");
            } else if method == "GET" {
                assert_eq!(String::from_utf8(body).unwrap(), case["body"].as_str().unwrap(), "{label}: corpo");
            }
        }
    }
}
