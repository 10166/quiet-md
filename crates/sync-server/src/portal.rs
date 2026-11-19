use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Redirect};

const PORTAL_HTML: &str = include_str!("../assets/portal.html");
const PORTAL_BG_PNG: &[u8] = include_bytes!("../assets/bg.png");
const PORTAL_LOGO_PNG: &[u8] = include_bytes!("../assets/logo.png");
const PORTAL_ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");
const PORTAL_CSP: &str = "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'";

pub async fn redirect_root() -> Redirect {
    Redirect::temporary("/portal")
}

pub async fn serve_portal() -> impl IntoResponse {
    (
        StatusCode::OK,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CONTENT_SECURITY_POLICY, PORTAL_CSP),
        ],
        Html(PORTAL_HTML),
    )
}

fn serve_png(bytes: &'static [u8]) -> impl IntoResponse {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
}

pub async fn serve_bg() -> impl IntoResponse {
    serve_png(PORTAL_BG_PNG)
}

pub async fn serve_logo() -> impl IntoResponse {
    serve_png(PORTAL_LOGO_PNG)
}

pub async fn serve_icon() -> impl IntoResponse {
    serve_png(PORTAL_ICON_PNG)
}
