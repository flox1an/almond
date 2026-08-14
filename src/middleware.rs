use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderValue, Method, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::models::AppState;

/// Response headers a browser may read back via JavaScript.  `X-Reason` is
/// added so error diagnostics survive the same-origin policy; the cashu and
/// price headers are exposed so a payment-preflight client can read them.
static CORS_EXPOSE: HeaderValue = HeaderValue::from_static(
    "Content-Length, Allow, X-Cashu, X-Price-Per-MB, X-Price-Unit, X-Accepted-Mints, X-Expiration, X-Reason",
);

/// Almond's own administrative surface, held apart from the Blossom endpoints
/// that BUD-01 requires to answer with `Access-Control-Allow-Origin: *`.
///
/// The distinction is deliberate: blob bytes and the standard Blossom API are
/// meant to be reachable from any origin, while `/config`, metrics, and the
/// web-of-trust / upstream diagnostics describe this specific operator's
/// instance and stay behind the `cors_allowed_origins` allowlist.
fn is_internal_path(path: &str) -> bool {
    matches!(
        path,
        "/config" | "/metrics" | "/_metrics" | "/_wot" | "/_upstream" | "/filter-test.html"
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

/// Build a preflight response.  `Access-Control-Allow-Headers: Authorization, *`
/// is the exact pair BUD-01 mandates: `*` does not cover `Authorization` on its
/// own, so the literal token must sit beside it.
fn preflight_response(status: StatusCode, origin: HeaderValue) -> Response {
    Response::builder()
        .status(status)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin)
        .header(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            "GET, HEAD, PUT, DELETE, PATCH, OPTIONS",
        )
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "Authorization, *")
        .header(header::ACCESS_CONTROL_EXPOSE_HEADERS, CORS_EXPOSE.clone())
        .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
        .body(Body::empty())
        .expect("static CORS response is valid")
}

pub async fn cors_middleware(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let internal = is_internal_path(req.uri().path());

    // OPTIONS is a CORS preflight and must not fall through to a handler: the
    // registered `options_upload` route is therefore unreachable by design.
    if req.method() == Method::OPTIONS {
        if internal {
            return match allowed_origin(&state, req.headers().get(header::ORIGIN)) {
                Some(origin) => preflight_response(StatusCode::NO_CONTENT, origin.clone()),
                None => StatusCode::FORBIDDEN.into_response(),
            };
        }
        return preflight_response(StatusCode::NO_CONTENT, HeaderValue::from_static("*"));
    }

    if internal {
        let allowed = allowed_origin(&state, req.headers().get(header::ORIGIN)).cloned();
        let mut response = next.run(req).await;
        if let Some(origin) = allowed {
            response
                .headers_mut()
                .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
            response
                .headers_mut()
                .insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, CORS_EXPOSE.clone());
        }
        response
    } else {
        let mut response = next.run(req).await;
        response.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, CORS_EXPOSE.clone());
        response
    }
}
