use axum::{
    body::Body,
    http::{header, HeaderValue, Request, StatusCode},
    middleware::Next,
    response::Response,
};

// Static header values - using from_static() is infallible for compile-time strings
static CORS_ORIGIN: HeaderValue = HeaderValue::from_static("*");
static CORS_METHODS: HeaderValue = HeaderValue::from_static("GET, PUT, DELETE, OPTIONS, PATCH");
static CORS_HEADERS: HeaderValue = HeaderValue::from_static(
    "Content-Type, authorization, x-sha-256, x-content-length, Content-Length, x-content-type, upload-type, upload-length, upload-offset, x-cashu",
);
static CORS_EXPOSE: HeaderValue = HeaderValue::from_static("Content-Length, Allow, X-Cashu, X-Price-Per-MB, X-Price-Unit, X-Accepted-Mints");
static CORS_MAX_AGE: HeaderValue = HeaderValue::from_static("86400");
static CORS_ALLOW: HeaderValue = HeaderValue::from_static("PUT, HEAD, OPTIONS, PATCH");

pub async fn cors_middleware(req: Request<Body>, next: Next) -> Response {
    if req.method() == axum::http::Method::OPTIONS {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, CORS_ORIGIN.clone())
            .header(header::ACCESS_CONTROL_ALLOW_METHODS, CORS_METHODS.clone())
            .header(header::ACCESS_CONTROL_ALLOW_HEADERS, CORS_HEADERS.clone())
            .header(header::ACCESS_CONTROL_EXPOSE_HEADERS, CORS_EXPOSE.clone())
            .header(header::ACCESS_CONTROL_MAX_AGE, CORS_MAX_AGE.clone())
            .header(header::ALLOW, CORS_ALLOW.clone())
            .body(Body::empty())
            .expect("Failed to build CORS preflight response");
    }

    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, CORS_ORIGIN.clone());
    headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, CORS_METHODS.clone());
    headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, CORS_HEADERS.clone());
    headers.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, CORS_EXPOSE.clone());
    headers.insert(header::ACCESS_CONTROL_MAX_AGE, CORS_MAX_AGE.clone());

    response
}
