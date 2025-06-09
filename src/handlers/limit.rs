use salvo::prelude::*;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::debug;

// 全局變量，將在 main.rs 中初始化
pub static GLOBAL_RATE_LIMITER: tokio::sync::OnceCell<Arc<Mutex<Instant>>> =
    tokio::sync::OnceCell::const_new();

/// 取得速率限制間隔 (毫秒)
/// 返回 None 表示禁用速率限制
fn get_rate_limit_ms() -> Option<Duration> {
    let ms = std::env::var("RATE_LIMIT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(100);

    // 如果值為 0，表示禁用速率限制
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
    // 獲取速率限制間隔，None 表示禁用
    if let Some(interval) = get_rate_limit_ms() {
        if let Some(cell) = GLOBAL_RATE_LIMITER.get() {
            let mut lock = cell.lock().await;
            let now = Instant::now();
            let elapsed = now.duration_since(*lock);

            if elapsed < interval {
                let wait = interval - elapsed;
                debug!(
                    "⏳ 請求觸發全局速率限制，延遲 {:?}，間隔設定: {:?}",
                    wait, interval
                );
                sleep(wait).await;
            }

            *lock = Instant::now();
        }
    } else {
        debug!("🚫 全局速率限制已禁用 (RATE_LIMIT_MS=0)");
    }

    ctrl.call_next(req, depot, res).await;
}
