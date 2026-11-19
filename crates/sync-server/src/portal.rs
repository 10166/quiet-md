use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Redirect};

const PORTAL_HTML: &str = include_str!("../assets/portal.html");
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
