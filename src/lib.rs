pub mod models;
pub mod handlers;
pub mod middleware;
pub mod utils;

use axum::{
    middleware::from_fn,
    routing::{get, put, delete},
    Router,
};
use models::AppState;
use middleware::cors_middleware;
use handlers::*;

pub async fn create_app(state: AppState) -> Router {
    Router::new()
        .route("/:filename", get(handle_file_request).head(handle_file_request))
        .route("/upload", put(upload_file))
        .route("/list", get(list_blobs))
        .route("/list/:id", get(list_blobs))
        .route("/mirror", put(mirror_blob))
        .route("/", get(serve_index))
        .route("/sha256", delete(method_not_allowed))
        .layer(from_fn(cors_middleware))
        .with_state(state)
} 