use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderValue, Method, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::models::AppState;

static CORS_HEADERS: HeaderValue = HeaderValue::from_static(
    "Content-Type, authorization, x-sha-256, x-content-length, Content-Length, x-content-type, upload-type, upload-length, upload-offset, x-cashu, x-expiration",
);
static CORS_EXPOSE: HeaderValue = HeaderValue::from_static(
    "Content-Length, Allow, X-Cashu, X-Price-Per-MB, X-Price-Unit, X-Accepted-Mints, X-Expiration",
);

fn is_public_blob_path(path: &str) -> bool {
    if !path.starts_with('/') || path[1..].contains('/') {
        return false;
    }
    !matches!(
        path,
        "/" | "/upload"
            | "/mirror"
            | "/list"
            | "/filter"
            | "/report"
            | "/metrics"
            | "/_metrics"
            | "/_wot"
            | "/_upstream"
            | "/index.html"
            | "/filter-test.html"
    )
}

fn allowed_origin<'a>(
    state: &AppState,
    origin: Option<&'a HeaderValue>,
) -> Option<&'a HeaderValue> {
    let origin = origin?;
    let value = origin.to_str().ok()?;
    state
        .cors_allowed_origins
        .iter()
        .any(|allowed| allowed == value)
        .then_some(origin)
}

fn cors_response(status: StatusCode, origin: HeaderValue, methods: &'static str) -> Response {
    Response::builder()
        .status(status)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin)
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, methods)
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, CORS_HEADERS.clone())
        .header(header::ACCESS_CONTROL_EXPOSE_HEADERS, CORS_EXPOSE.clone())
        .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
        .body(Body::empty())
        .expect("static CORS response is valid")
}

/// Blob bytes remain shareable cross-origin.  API, discovery, and diagnostics
/// require an explicit operator allowlist, preventing a website from reading a
/// user's local or administrative Almond instance.
pub async fn cors_middleware(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let public_blob = is_public_blob_path(req.uri().path())
        && matches!(
            req.method(),
            &Method::GET | &Method::HEAD | &Method::OPTIONS
        );
    if req.method() == Method::OPTIONS {
        if public_blob {
            return cors_response(
                StatusCode::NO_CONTENT,
                HeaderValue::from_static("*"),
                "GET, HEAD",
            );
        }
        return match allowed_origin(&state, req.headers().get(header::ORIGIN)) {
            Some(origin) => cors_response(
                StatusCode::NO_CONTENT,
                origin.clone(),
                "GET, PUT, DELETE, PATCH, OPTIONS",
            ),
            None => StatusCode::FORBIDDEN.into_response(),
        };
    }
    let allowed_origin = allowed_origin(&state, req.headers().get(header::ORIGIN)).cloned();
    let mut response = next.run(req).await;
    if public_blob {
        response.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
    } else if let Some(origin) = allowed_origin {
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, CORS_EXPOSE.clone());
    }
    response
}
