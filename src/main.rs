use salvo::prelude::*;
use std::env;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info};

mod cache;
mod evert;
mod handlers;
mod poe_client;
mod types;
mod utils;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn get_env_or_default(key: &str, default: &str) -> String {
    let value = env::var(key).unwrap_or_else(|_| default.to_string());
    if key == "ADMIN_PASSWORD" {
        debug!(
            "🔧 Environment variable {} = {}",
            key,
            "*".repeat(value.len())
        );
    } else {
        debug!("🔧 Environment variable {} = {}", key, value);
    }
    value
}

fn setup_logging(log_level: &str) {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_thread_ids(true)
        .with_level(true)
        .with_file(false)
        .with_line_number(false)
        .with_env_filter(log_level)
        .init();
    info!("🚀 Logging system initialized, level: {}", log_level);
}

fn log_cache_settings() {
    // Record cache-related settings
    let cache_ttl_seconds = std::env::var("URL_CACHE_TTL_SECONDS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(3 * 24 * 60 * 60);
    let cache_size_mb = std::env::var("URL_CACHE_SIZE_MB")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100);

    let ttl_days = cache_ttl_seconds / 86400;
    let ttl_hours = (cache_ttl_seconds % 86400) / 3600;
    let ttl_mins = (cache_ttl_seconds % 3600) / 60;
    let ttl_secs = cache_ttl_seconds % 60;

    let ttl_str = if ttl_days > 0 {
        format!(
            "{} days {} hours {} minutes {} seconds",
            ttl_days, ttl_hours, ttl_mins, ttl_secs
        )
    } else if ttl_hours > 0 {
        format!(
            "{} hours {} minutes {} seconds",
            ttl_hours, ttl_mins, ttl_secs
        )
    } else if ttl_mins > 0 {
        format!("{} minutes {} seconds", ttl_mins, ttl_secs)
    } else {
        format!("{} seconds", ttl_secs)
    };

    info!(
        "📦 Poe CDN URL cache settings | TTL: {} | Max space: {}MB",
        ttl_str, cache_size_mb
    );
}

#[tokio::main]
async fn main() {
    let log_level = get_env_or_default("LOG_LEVEL", "debug");
    setup_logging(&log_level);

    // Initialize cache settings
    log_cache_settings();

    // Initialize global rate limit
    let _ = handlers::limit::GLOBAL_RATE_LIMITER.set(Arc::new(tokio::sync::Mutex::new(
        std::time::Instant::now() - Duration::from_secs(60),
    )));

    // Show rate limit settings
    let rate_limit_ms = std::env::var("RATE_LIMIT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(100);

    if rate_limit_ms == 0 {
        info!("⚙️  Global rate limit: Disabled (RATE_LIMIT_MS=0)");
    } else {
        info!(
            "⚙️  Global rate limit: Enabled (every {}ms per request)",
            rate_limit_ms
        );
    }

    let host = get_env_or_default("HOST", "0.0.0.0");
    let port = get_env_or_default("PORT", "8080");
    get_env_or_default("ADMIN_USERNAME", "admin");
    get_env_or_default("ADMIN_PASSWORD", "123456");
    let config_dir = get_env_or_default("CONFIG_DIR", "./");
    let config_path = Path::new(&config_dir).join("models.yaml");
    info!("📁 Configuration file path: {}", config_path.display());
    get_env_or_default("POE_BASE_URL", "https://api.poe.com");
    get_env_or_default(
        "POE_FILE_UPLOAD_URL",
        "https://www.quora.com/poe_api/file_upload_3RD_PARTY_POST",
    );

    let salvo_max_size = get_env_or_default("MAX_REQUEST_SIZE", "1073741824")
        .parse()
        .unwrap_or(1024 * 1024 * 1024); // Default 1GB

    let bind_address = format!("{}:{}", host, port);
    info!("🌟 Starting Poe API To OpenAI API service...");
    debug!("📍 Service bind address: {}", bind_address);

    // Initialize Sled DB
    let _ = cache::get_sled_db();
    info!("💾 Memory database initialization complete");

    let api_router = Router::new()
        .hoop(handlers::cors_middleware)
        .push(
            Router::with_path("models")
                .get(handlers::get_models)
                .options(handlers::cors_middleware),
        )
        .push(
            Router::with_path("chat/completions")
                .hoop(handlers::rate_limit_middleware)
                .post(handlers::chat_completions)
                .options(handlers::cors_middleware),
        )
        .push(
            Router::with_path("api/models")
                .get(handlers::get_models)
                .options(handlers::cors_middleware),
        )
        .push(
            Router::with_path("v1/models")
                .get(handlers::get_models)
                .options(handlers::cors_middleware),
        )
        .push(
            Router::with_path("v1/chat/completions")
                .hoop(handlers::rate_limit_middleware)
                .post(handlers::chat_completions)
                .options(handlers::cors_middleware),
        );

    let router: Router = Router::new()
        .hoop(max_size(salvo_max_size.try_into().unwrap()))
        .push(Router::with_path("static/{**path}").get(StaticDir::new(["static"])))
        .push(handlers::admin_routes())
        .push(api_router);

    info!("🛣️  API route configuration complete");

    let acceptor = TcpListener::new(bind_address.clone()).bind().await;
    info!("🎯 Service started and listening on {}", bind_address);

    Server::new(acceptor).serve(router).await;
}
