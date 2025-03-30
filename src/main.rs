use std::{
    collections::HashMap,
    env,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

use tokio::fs;
use tokio::sync::RwLock;
use axum_server;
use dotenv::dotenv;
use tracing_subscriber;
use rugpull_rodeo::{models::AppState, utils::{build_file_index, enforce_storage_limits}, create_app};

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
    fs::create_dir_all(&upload_dir).await.unwrap();
 
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
    
    let app = create_app(state).await;

    println!("listening on {}", addr);

    axum_server::bind(addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

