//! Embedded static assets, served read-only under `/ui/assets/`.
//!
//! Vendored Basecoat CSS + Datastar JS are compiled into the binary via
//! `rust-embed` so the concept ships as a single executable with no external
//! asset directory. Pinned versions live in `assets/vendor/VERSIONS.txt`.

use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Assets;

/// Serve a vendored asset by path (e.g. `vendor/datastar.js`).
///
/// Returns 404 for unknown paths. Content-Type is guessed from the extension —
/// enough for the handful of file types the concept ships.
pub async fn serve(Path(path): Path<String>) -> Response {
    match Assets::get(&path) {
        Some(file) => {
            let mime = content_type(&path);
            ([(header::CONTENT_TYPE, mime)], file.data).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn content_type(path: &str) -> &'static str {
    if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".js") {
        "text/javascript; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else {
        "application/octet-stream"
    }
}
