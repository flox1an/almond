use almond::Config;
use axum::{routing::get, Router};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut cfg = Config::defaults();
    cfg.storage_path = "./embedded-files".into();
    if let Some(bind_addr) = std::env::args().nth(1) {
        cfg.bind_addr = bind_addr.parse()?;
    }
    let listener = tokio::net::TcpListener::bind(cfg.bind_addr).await?;
    cfg.bind_addr = listener.local_addr()?;
    cfg.public_url = Some(format!("http://{}", cfg.bind_addr));
    let state = almond::build_state(&cfg).await?;
    let mut tasks = almond::spawn_background_tasks(&state, &cfg);
    let app = Router::new()
        .route("/", get(|| async { "Embedding host" }))
        .route("/health", get(|| async { "ok" }))
        .merge(almond::create_app(state));
    println!("Embedded Blossom server: http://{}", listener.local_addr()?);
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c()
                .await
                .expect("failed to install Ctrl+C handler");
        })
        .await;
    tasks.shutdown().await;
    result?;
    Ok(())
}
