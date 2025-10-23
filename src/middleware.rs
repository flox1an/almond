use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    middleware::Next,
    response::Response,
};

pub async fn cors_middleware(req: Request<Body>, next: Next) -> Response {
    if req.method() == axum::http::Method::OPTIONS {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(
                header::ACCESS_CONTROL_ALLOW_METHODS,
                "GET, PUT, DELETE, OPTIONS, PATCH",
            )
            .header(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                "Content-Type, authorization, x-sha-256, x-content-length, x-content-type, upload-type, upload-length, upload-offset",
            )
            .header(header::ACCESS_CONTROL_EXPOSE_HEADERS, "Content-Length")
            .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
            .header(header::ALLOW, "PUT, HEAD, OPTIONS, PATCH")
            .body(Body::empty())
            .unwrap();
    }

    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        "GET, PUT, DELETE, OPTIONS, PATCH".parse().unwrap(),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        "Content-Type, authorization, x-sha-256, x-content-length, x-content-type, upload-type, upload-length, upload-offset".parse().unwrap(),
    );
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        "Content-Length".parse().unwrap(),
    );
    headers.insert(header::ACCESS_CONTROL_MAX_AGE, "86400".parse().unwrap());

    response
}
