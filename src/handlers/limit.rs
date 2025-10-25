use salvo::prelude::*;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::debug;

// Global variable, will be initialized in main.rs
pub static GLOBAL_RATE_LIMITER: tokio::sync::OnceCell<Arc<Mutex<Instant>>> =
    tokio::sync::OnceCell::const_new();

/// Get rate limit interval (milliseconds)
/// Returns None to indicate rate limiting is disabled
fn get_rate_limit_ms() -> Option<Duration> {
    let ms = std::env::var("RATE_LIMIT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(100);

    // If value is 0, it means disable rate limiting
    if ms == 0 {
        None
    } else {
        Some(Duration::from_millis(ms))
    }
}

#[handler]
pub async fn rate_limit_middleware(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    // Get rate limit interval, None means disabled
    if let Some(interval) = get_rate_limit_ms() {
        if let Some(cell) = GLOBAL_RATE_LIMITER.get() {
            let mut lock = cell.lock().await;
            let now = Instant::now();
            let elapsed = now.duration_since(*lock);

            if elapsed < interval {
                let wait = interval - elapsed;
                debug!(
                    "⏳ Request triggered global rate limit, delay {:?}, interval setting: {:?}",
                    wait, interval
                );
                sleep(wait).await;
            }

            *lock = Instant::now();
        }
    } else {
        debug!("🚫 Global rate limit disabled (RATE_LIMIT_MS=0)");
    }

    ctrl.call_next(req, depot, res).await;
}
