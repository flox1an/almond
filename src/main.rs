use std::{
    collections::HashMap,
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::body::to_bytes;
use axum::extract::Path as AxumPath;
use axum::extract::Query;
use axum::extract::State;
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    middleware::from_fn,
    response::Response,
    routing::{delete, get, put},
    Json, Router,
};
use axum_server;
use dotenv::dotenv;
use futures_util::StreamExt;
use mime_guess::from_path;
use regex::Regex;
use reqwest::header as reqwest_header;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio::sync::RwLock;
use tokio_util::io::ReaderStream;
use tracing::{error, info, warn};
use tracing_subscriber;

#[derive(Clone)]
struct AppState {
    upload_dir: PathBuf,
    file_index: Arc<RwLock<HashMap<String, PathBuf>>>,
    max_total_size: u64,
    max_total_files: usize,
    bind_addr: String,
    public_url: String,
    cleanup_interval_secs: u64,
}

impl AppState {
    fn create_blob_descriptor(
        &self,
        sha256: &str,
        size: u64,
        content_type: Option<String>,
    ) -> BlobDescriptor {
        BlobDescriptor {
            url: format!("{}/{}", self.public_url, sha256),
            sha256: sha256.to_string(),
            size,
            r#type: content_type,
            uploaded: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        }
    }
}

#[derive(Serialize)]
struct BlobDescriptor {
    url: String,
    sha256: String,
    size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    r#type: Option<String>,
    uploaded: u64,
}

async fn load_app_state() -> AppState {
    dotenv().ok();

    let max_total_size = env::var("MAX_TOTAL_SIZE")
        .unwrap_or_else(|_| "99999999999".to_string())
        .parse()
        .expect("Invalid value for MAX_TOTAL_SIZE");

    let max_total_files = env::var("MAX_TOTAL_FILES")
        .unwrap_or_else(|_| "1000000".to_string())
        .parse()
        .expect("Invalid value for MAX_TOTAL_FILES");

    let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());

    let public_url = env::var("PUBLIC_URL").unwrap_or_else(|_| "http://127.0.0.1:3000".to_string());

    let upload_dir = PathBuf::from("./files");
    tokio::fs::create_dir_all(&upload_dir).await.unwrap();
 
    let file_index = Arc::new(RwLock::new(HashMap::new()));
    build_file_index(&upload_dir, &file_index).await;

    let cleanup_interval_secs = env::var("CLEANUP_INTERVAL_SECS")
        .unwrap_or_else(|_| "60".to_string())
        .parse()
        .expect("Invalid value for CLEANUP_INTERVAL_SECS");

    AppState {
        upload_dir,
        file_index,
        max_total_size,
        max_total_files,
        bind_addr,
        public_url,
        cleanup_interval_secs,
    }
}

fn start_cleanup_job(state: AppState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(
            state.cleanup_interval_secs,
        ));
        loop {
            interval.tick().await;
            enforce_storage_limits(&state).await;
        }
    });
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let state = load_app_state().await;
    let addr = state
        .bind_addr
        .parse::<SocketAddr>()
        .expect("Invalid address format");

    start_cleanup_job(state.clone());

    let app = Router::new()
        .route(
            "/:filename",
            get(handle_file_request).head(handle_file_request),
        )
        .route("/upload", put(upload_file))
        .route("/list", get(list_blobs))
        .route("/list/:id", get(list_blobs))
        .route("/mirror", put(mirror_blob))
        .route("/", get(serve_index))
        .route("/sha256", delete(method_not_allowed))
        .layer(from_fn(cors_middleware))
        .with_state(state);

    println!("listening on {}", addr);

    // Enable HTTP/2 support
    axum_server::bind(addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

fn get_sha256_hash_from_filename(filename: &str) -> Option<String> {
    let re = Regex::new(r"^([a-fA-F0-9]{64})(\.[a-zA-Z0-9]+)?$").unwrap();
    if let Some(captures) = re.captures(filename) {
        Some(captures[1].to_string()) // Return the first capture group (the hash)
    } else {
        None
    }
}

async fn build_file_index(upload_dir: &Path, index: &RwLock<HashMap<String, PathBuf>>) {
    let mut map = HashMap::new();
    if let Ok(mut entries) = fs::read_dir(upload_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.is_file() {
                if let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) {
                    let key = name[..64.min(name.len())].to_string();
                    map.insert(key, path);
                }
            }
        }
    }
    *index.write().await = map;
}


#[derive(Debug, Deserialize)]
struct ListQuery {
    since: Option<u64>,
    until: Option<u64>,
}

async fn list_blobs(
    State(state): State<AppState>,
    Query(params): Query<ListQuery>,
) -> Result<Json<Vec<BlobDescriptor>>, (StatusCode, String)> {
    let mut entries = fs::read_dir(&state.upload_dir)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut blobs = vec![];

    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    {
        let path = entry.path();
        if path.is_file() {
            let metadata = entry
                .metadata()
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            let created = metadata.created().unwrap_or(SystemTime::now());
            let timestamp = created
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            if let Some(since) = params.since {
                if timestamp < since {
                    continue;
                }
            }
            if let Some(until) = params.until {
                if timestamp > until {
                    continue;
                }
            }

            let filename = path.file_name().unwrap().to_string_lossy().to_string();
            let sha256 = filename[..64.min(filename.len())].to_string();
            let size = metadata.len();
            let mime = from_path(&path)
                .first()
                .map(|m| m.essence_str().to_string());

            let url = format!("{}/{}", state.public_url, filename);

            blobs.push(BlobDescriptor {
                url,
                sha256,
                size,
                r#type: mime,
                uploaded: timestamp,
            });
        }
    }

    Ok(Json(blobs))
}

async fn handle_file_request(
    AxumPath(filename): AxumPath<String>,
    State(state): State<AppState>,
    req: Request<Body>,
) -> Result<Response, StatusCode> {
    info!("get for url: {}", filename);

    if let Some(filename) = get_sha256_hash_from_filename(&filename) {
        info!("Found file: {}", filename);

        match find_file(&state.file_index, &filename).await {
            Some(path) => {
                let metadata = fs::metadata(&path)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                let mime = from_path(&path)
                    .first()
                    .map(|m| m.essence_str().to_string())
                    .unwrap_or("application/octet-stream".into());
                let size = metadata.len();

                if req.method() == axum::http::Method::HEAD {
                    return Ok(Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, mime)
                        .header(header::CONTENT_LENGTH, size)
                        .body(Body::empty())
                        .unwrap());
                } else {
                    return serve_file_with_range(path, req).await;
                }
            }
            None => Err(StatusCode::NOT_FOUND),
        }
    } else {
        Err(StatusCode::BAD_REQUEST)
    }
}

async fn enforce_storage_limits(state: &AppState) {
    let mut entries = match fs::read_dir(&state.upload_dir).await {
        Ok(entries) => entries,
        Err(_) => return,
    };

    let mut files: Vec<(PathBuf, u64, SystemTime)> = vec![];
    let mut total_size = 0;

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if let Ok(metadata) = entry.metadata().await {
            if metadata.is_file() {
                let size = metadata.len();
                let created = metadata.created().unwrap_or(SystemTime::now());
                total_size += size;
                files.push((path, size, created));
            }
        }
    }

    let total_files = files.len();
    info!(
        "Storage check: {} files using {:.2} MB",
        total_files,
        total_size as f64 / 1_048_576.0
    );

    if total_files <= state.max_total_files && total_size <= state.max_total_size {
        return;
    }

    files.sort_by_key(|f| f.2);

    let mut removed = 0;
    for (path, size, _) in &files {
        if fs::remove_file(path).await.is_ok() {
            let key = path
                .file_name()
                .and_then(|f| f.to_str())
                .map(|s| s[..64.min(s.len())].to_string());
            if let Some(k) = key {
                state.file_index.write().await.remove(&k);
            }
            total_size = total_size.saturating_sub(*size);
            removed += 1;
            warn!("Deleted file: {} ({} bytes)", path.display(), size);
        }
        if files.len() - removed <= state.max_total_files && total_size <= state.max_total_size {
            break;
        }
    }
    info!("Deleted {} file(s) to enforce storage limits.", removed);
}

async fn upload_file(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Result<Response, StatusCode> {
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let extension = content_type
        .as_ref()
        .and_then(|ct| mime_guess::get_mime_extensions_str(ct))
        .and_then(|mime| mime.first().map(|ext| ext.to_string()));

    let body = req.into_body();
    let mut stream = body.into_data_stream();
    let mut buffer = Vec::new();
    let mut hasher = Sha256::new();

    while let Some(chunk) = stream.next().await {
        let data = chunk.map_err(|_| StatusCode::BAD_REQUEST)?;
        hasher.update(&data);
        buffer.extend_from_slice(&data);
    }

    let sha256 = format!("{:x}", hasher.finalize());
    let size = buffer.len() as u64;

    enforce_storage_limits(&state).await;

    let filepath = match extension {
        Some(ext) => state.upload_dir.join(format!("{}.{}", sha256, ext)),
        None => state.upload_dir.join(&sha256),
    };
    let mut file = File::create(&filepath).await.map_err(|e| {
        error!("File create error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    file.write_all(&buffer)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let key = sha256[..64.min(sha256.len())].to_string();
    state.file_index.write().await.insert(key.clone(), filepath);

    let descriptor = state.create_blob_descriptor(&sha256, size, content_type);

    Ok(Response::builder()
        .status(StatusCode::CREATED)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&descriptor).unwrap()))
        .unwrap())
}

async fn find_file(index: &RwLock<HashMap<String, PathBuf>>, base_name: &str) -> Option<PathBuf> {
    let index = index.read().await;
    index.get(base_name).cloned()
}

async fn serve_file_with_range(path: PathBuf, req: Request<Body>) -> Result<Response, StatusCode> {
    use axum::http::header::RANGE;
    let range_header = req.headers().get(RANGE).and_then(|r| r.to_str().ok());

    let mut file = File::open(&path)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let metadata = file
        .metadata()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let total_size = metadata.len();

    if let Some(range_header) = range_header {
        if let Some(range) = parse_range_header(range_header, total_size) {
            let (start, end) = range;
            let length = end - start + 1;
            file.seek(SeekFrom::Start(start))
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let stream = ReaderStream::new(file.take(length));
            let body = Body::from_stream(stream);

            let mime = from_path(&path)
                .first()
                .map(|m| m.essence_str().to_string())
                .unwrap_or("application/octet-stream".into());

            return Ok(Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, mime)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", start, end, total_size),
                )
                .body(body)
                .unwrap());
        }
    }

    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            from_path(&path)
                .first()
                .map(|m| m.essence_str().to_string())
                .unwrap_or("application/octet-stream".into()),
        )
        .body(body)
        .unwrap())
}

fn parse_range_header(header_value: &str, total_size: u64) -> Option<(u64, u64)> {
    if !header_value.starts_with("bytes=") {
        return None;
    }
    let range = &header_value[6..];
    let parts: Vec<&str> = range.split('-').collect();
    if parts.len() != 2 {
        return None;
    }
    let start = parts[0].parse::<u64>().ok()?;
    let end = if parts[1].is_empty() {
        total_size - 1
    } else {
        parts[1].parse::<u64>().ok()?
    };
    if start > end || end >= total_size {
        return None;
    }
    Some((start, end))
}

async fn cors_middleware(
    req: Request<Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if req.method() == axum::http::Method::OPTIONS {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(
                header::ACCESS_CONTROL_ALLOW_METHODS,
                "GET, PUT, DELETE, OPTIONS",
            )
            .header(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                "Content-Type, authorization",
            )
            .header(header::ACCESS_CONTROL_EXPOSE_HEADERS, "Content-Length")
            .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
            .body(Body::empty())
            .unwrap();
    }

    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        "GET, PUT, DELETE, OPTIONS".parse().unwrap(),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        "Content-Type, authorization".parse().unwrap(),
    );
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        "Content-Length".parse().unwrap(),
    );
    headers.insert(header::ACCESS_CONTROL_MAX_AGE, "86400".parse().unwrap());
    response
}

async fn mirror_blob(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Result<Response, StatusCode> {
    let body_bytes = to_bytes(req.into_body(), usize::MAX)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let body: Value = serde_json::from_slice(&body_bytes).map_err(|_| StatusCode::BAD_REQUEST)?;

    let url = body
        .get("url")
        .and_then(Value::as_str)
        .ok_or(StatusCode::BAD_REQUEST)?;

    // Extract expected SHA256 from the URL (assuming it's part of the URL)
    let expected_sha256 = url
        .split('/')
        .last()
        .unwrap_or("")
        .split('.')
        .next()
        .unwrap_or("");

    info!("Starting to mirror blob from URL: {}", url);

    let client = Client::new();

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    if !response.status().is_success() {
        warn!("Failed to download blob, status: {}", response.status());
        return Err(StatusCode::BAD_REQUEST);
    }

    info!("Successfully downloaded blob from URL: {}", url);

    let content_type = response
        .headers()
        .get(reqwest_header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let blob_bytes = response
        .bytes()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut hasher = Sha256::new();
    hasher.update(&blob_bytes);
    let sha256 = format!("{:x}", hasher.finalize());

    // Validate the SHA256 hash
    if sha256 != expected_sha256 {
        error!(
            "SHA256 mismatch: expected {}, got {}",
            expected_sha256, sha256
        );
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    info!("Calculated SHA256: {}", sha256);

    let filename = format!(
        "{}.{}",
        sha256,
        content_type.split('/').last().unwrap_or("bin")
    );
    let filepath = state.upload_dir.join(filename);
    info!("Saving blob to: {}", filepath.display());

    let mut file = File::create(&filepath)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    file.write_all(&blob_bytes)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    info!("Blob saved successfully to: {}", filepath.display());

    let descriptor =
        state.create_blob_descriptor(&sha256, blob_bytes.len() as u64, Some(content_type));

    state
        .file_index
        .write()
        .await
        .insert(sha256.clone(), filepath);
    info!("Blob descriptor created for SHA256: {}", sha256);

    Ok(Response::builder()
        .status(StatusCode::CREATED)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_string(&descriptor).unwrap()))
        .unwrap())
}

async fn serve_index() -> Result<Response, StatusCode> {
    let html_content = include_str!("index.html");

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html")
        .body(Body::from(html_content))
        .unwrap())
}

async fn method_not_allowed() -> Result<Response, StatusCode> {
    Ok(Response::builder()
        .status(StatusCode::METHOD_NOT_ALLOWED)
        .body("Method Not Allowed".into())
        .unwrap())
}
