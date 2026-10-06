//! The standalone almond server: every process-wide concern (allocator,
//! config file and env, tracing, crypto provider, TLS files, signals,
//! listener) lives here; the library only builds state and routers.

#![allow(clippy::let_underscore_must_use)]

use almond::{config, error::AppError, tls, Config};
use tokio::signal;
use tracing::{error, info, warn};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    // Config file and deprecated names go into the process environment, so
    // this must happen before the tokio runtime starts any thread.
    let legacy_warnings = match config::load_env_file() {
        Ok(warnings) => warnings,
        Err(error) => {
            eprintln!("❌ Configuration error: {error}");
            std::process::exit(1);
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    for warning in legacy_warnings {
        warn!("⚠️ {warning}");
    }

    // Parse and validate configuration — boot errors exit here.
    let cfg = match Config::load() {
        Ok(cfg) => cfg,
        Err(error) => {
            error!("❌ Configuration error: {error}");
            std::process::exit(1);
        }
    };

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to build tokio runtime")
        .block_on(run(cfg));
}

async fn run(cfg: Config) {
    // Install default crypto provider for rustls (required for HTTPS)
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let addr = cfg.bind_addr;

    // Handle HTTPS/TLS setup if enabled
    if cfg.tls_enabled {
        info!("🔐 HTTPS enabled");
        if let Err(e) =
            tls::ensure_tls_certificates(&cfg.tls_cert, &cfg.tls_key, cfg.tls_self_signed)
        {
            error!("❌ Failed to setup TLS certificates: {}", e);
            std::process::exit(1);
        }
    } else {
        info!("⚠️  HTTPS disabled - running in HTTP mode");
    }

    let state = match almond::build_state(&cfg).await {
        Ok(state) => state,
        // Cashu wallet init failure (already logged) historically exited 1;
        // every other startup failure was a panic.
        Err(error) if error.is::<AppError>() => std::process::exit(1),
        Err(error) => panic!("{error}"),
    };

    // Named binding: dropping the set would abort every background job.
    let _tasks = almond::spawn_background_tasks(&state, &cfg);

    let app = almond::create_app(state.clone()).merge(almond::standalone_extras(state));

    // Spawn a task to handle shutdown signals - exit immediately when received
    tokio::spawn(async move {
        let ctrl_c = async {
            signal::ctrl_c()
                .await
                .expect("failed to install Ctrl+C handler");
        };

        #[cfg(unix)]
        let terminate = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler")
                .recv()
                .await;
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            () = ctrl_c => {
                info!("🛑 Received SIGINT (Ctrl+C) - exiting immediately");
                std::process::exit(0);
            },
            () = terminate => {
                info!("🛑 Received SIGTERM - exiting immediately");
                std::process::exit(0);
            },
        }
    });

    // Start server with HTTPS or HTTP
    if cfg.tls_enabled {
        info!("🎧 blossom server listening on https://{}", addr);

        match tls::load_tls_config(&cfg.tls_cert, &cfg.tls_key).await {
            Ok(config) => {
                if let Err(e) = axum_server::bind_rustls(addr, config)
                    .serve(app.into_make_service())
                    .await
                {
                    error!("❌ HTTPS server error: {}", e);
                }
            }
            Err(e) => {
                error!("❌ Failed to load TLS configuration: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        info!("🎧 blossom server listening on http://{}", addr);

        // Create a TcpListener for HTTP
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .expect("Failed to bind to address");

        // Start the server (no graceful shutdown - exit immediately on signal)
        if let Err(e) = axum::serve(listener, app).await {
            error!("❌ Server error: {}", e);
        }
    }
}
