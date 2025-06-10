use crate::types::Config;
use crate::utils::load_config_from_yaml;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::debug;
use tracing::{error, info, warn};

/// 全域 Sled DB
pub static SLED_DB: OnceLock<sled::Db> = OnceLock::new();

/// 取得 in-memory sled::Db，僅一次初始化
pub fn get_sled_db() -> &'static sled::Db {
    SLED_DB.get_or_init(|| {
        sled::Config::new()
            .temporary(true)
            .open()
            .expect("無法初始化 sled 記憶體緩存")
    })
}

/// 存 config 進 sled
pub fn save_config_sled(key: &str, config: &Config) -> Result<(), String> {
    let db = get_sled_db();
    match serde_json::to_vec(config) {
        Ok(bytes) => {
            db.insert(key.as_bytes(), bytes)
                .map_err(|e| format!("寫入 Sled 緩存失敗：{}", e))?;
            db.flush().ok();
            Ok(())
        }
        Err(e) => Err(format!("序列化設定失敗: {}", e)),
    }
}

/// 讀 config
pub fn load_config_sled(key: &str) -> Result<Option<Arc<Config>>, String> {
    let db = get_sled_db();
    match db.get(key.as_bytes()) {
        Ok(Some(bytes)) => match serde_json::from_slice::<Config>(&bytes) {
            Ok(conf) => Ok(Some(Arc::new(conf))),
            Err(e) => {
                error!("❌ Sled 解析設定失敗: {}", e);
                Err(format!("JSON 解析失敗: {}", e))
            }
        },
        Ok(None) => Ok(None),
        Err(e) => {
            error!("❌ 讀取 Sled 設定失敗: {}", e);
            Err(format!("載入失敗: {}", e))
        }
    }
}

/// 移除某個 key
pub fn remove_config_sled(key: &str) {
    let db = get_sled_db();
    if let Err(e) = db.remove(key.as_bytes()) {
        warn!("⚠️ 從 sled 移除緩存時發生錯誤: {}", e);
    }
    db.flush().ok();
}

// 從緩存或 YAML 取得設定
pub async fn get_cached_config() -> Arc<Config> {
    let cache_key = "models.yaml";
    // 嘗試 sled 讀取（緩存優先，失敗再 yaml）
    match load_config_sled(cache_key) {
        Ok(Some(arc_cfg)) => {
            debug!("✅ Sled 緩存命中: {}", cache_key);
            arc_cfg
        }
        Ok(None) | Err(_) => {
            debug!("💾 sled 中無設定，從 YAML 讀取...");
            match load_config_from_yaml() {
                Ok(conf) => {
                    let _ = save_config_sled(cache_key, &conf);
                    Arc::new(conf)
                }
                Err(e) => {
                    warn!("⚠️ 無法從 YAML 載入設定，回退預設: {}", e);
                    Arc::new(Config {
                        enable: Some(false),
                        models: std::collections::HashMap::new(),
                        custom_models: None,
                    })
                }
            }
        }
    }
}

// 獲取URL緩存的TTL
pub fn get_url_cache_ttl() -> Duration {
    let ttl_seconds = std::env::var("URL_CACHE_TTL_SECONDS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(3 * 24 * 60 * 60); // 默認3天
    Duration::from_secs(ttl_seconds)
}

// 獲取URL緩存最大容量（MB）
pub fn get_url_cache_size_mb() -> usize {
    std::env::var("URL_CACHE_SIZE_MB")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100) // 默認100MB
}

// 存儲URL在緩存中，帶有過期時間
pub fn cache_url(original_url: &str, poe_url: &str, size_bytes: usize) {
    let db = get_sled_db();
    let tree_name = "urls";
    let ttl = get_url_cache_ttl();
    let key = format!("url:{}", original_url);
    // 當前時間 + TTL
    let expires_at = SystemTime::now()
        .checked_add(ttl)
        .unwrap_or_else(|| SystemTime::now() + ttl);
    // 轉換為時間戳
    let expires_secs = expires_at
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs();
    // 儲存數據，使用格式 "過期時間戳:poe_url:大小"
    // 確保URL中的冒號不會干擾解析
    let store_value = format!("{}:{}:{}", expires_secs, poe_url, size_bytes);
    if let Ok(tree) = db.open_tree(tree_name) {
        match tree.insert(key.as_bytes(), store_value.as_bytes()) {
            Ok(_) => {
                debug!("✅ URL緩存已更新: {}", original_url);
            }
            Err(e) => {
                error!("❌ 保存URL緩存失敗: {}", e);
            }
        }
    } else {
        error!("❌ 無法開啟URL緩存樹");
    }
    // 維護緩存大小
    check_and_control_cache_size();
}

// 獲取緩存的URL
pub fn get_cached_url(original_url: &str) -> Option<(String, usize)> {
    let db = get_sled_db();
    let tree_name = "urls";
    let key = format!("url:{}", original_url);
    let result = match db.open_tree(tree_name) {
        Ok(tree) => tree.get(key.as_bytes()),
        Err(e) => {
            error!("❌ 無法開啟URL緩存樹: {}", e);
            return None;
        }
    };
    match result {
        Ok(Some(value_bytes)) => {
            if let Ok(value_str) = String::from_utf8(value_bytes.to_vec()) {
                let parts: Vec<&str> = value_str.split(':').collect();
                if parts.len() >= 3 {
                    // 正確解析格式: "expires_at:poe_url:size"
                    if let Ok(expires_secs) = parts[0].parse::<u64>() {
                        let now_secs = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_else(|_| Duration::from_secs(0))
                            .as_secs();
                        // 檢查是否過期
                        if expires_secs > now_secs {
                            // 一個重要的修復：URL中可能含有冒號，需要正確處理
                            // 取第一個部分為過期時間，最後一個部分為大小，中間的都是URL
                            let size_str = parts.last().unwrap();
                            let poe_url = parts[1..(parts.len() - 1)].join(":");
                            if let Ok(size) = size_str.parse::<usize>() {
                                // 更新過期時間（延長TTL）
                                refresh_url_cache_ttl(original_url, &poe_url, size);
                                debug!("✅ URL緩存命中並續期: {}", original_url);
                                return Some((poe_url, size));
                            }
                        } else {
                            // 已過期，刪除項目
                            if let Ok(tree) = db.open_tree(tree_name) {
                                let _ = tree.remove(key.as_bytes());
                                debug!("🗑️ 刪除過期URL緩存: {}", original_url);
                            }
                        }
                    }
                }
            } else {
                error!("❌ 無效的URL緩存值格式");
            }
            None
        }
        Ok(None) => None,
        Err(e) => {
            error!("❌ 讀取URL緩存失敗: {}", e);
            None
        }
    }
}

// 刷新URL緩存的TTL
fn refresh_url_cache_ttl(original_url: &str, poe_url: &str, size_bytes: usize) {
    cache_url(original_url, poe_url, size_bytes);
}

// 保存base64哈希到緩存
pub fn cache_base64(hash: &str, poe_url: &str, size_bytes: usize) {
    let db = get_sled_db();
    let tree_name = "base64";
    let ttl = get_url_cache_ttl();
    let key = format!("base64:{}", hash);
    let hash_prefix = if hash.len() > 8 { &hash[..8] } else { hash };
    // 當前時間 + TTL
    let expires_at = SystemTime::now()
        .checked_add(ttl)
        .unwrap_or_else(|| SystemTime::now() + ttl);
    // 轉換為時間戳
    let expires_secs = expires_at
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs();
    // 儲存數據，格式為 "expires_secs:poe_url:size_bytes"
    let store_value = format!("{}:{}:{}", expires_secs, poe_url, size_bytes);
    debug!(
        "💾 儲存base64緩存 | 哈希: {}... | 大小: {}bytes",
        hash_prefix, size_bytes
    );
    match db.open_tree(tree_name) {
        Ok(tree) => match tree.insert(key.as_bytes(), store_value.as_bytes()) {
            Ok(_) => {
                debug!("✅ base64緩存已更新 | 哈希: {}...", hash_prefix);
            }
            Err(e) => {
                error!("❌ 保存base64緩存失敗: {} | 哈希: {}...", e, hash_prefix);
            }
        },
        Err(e) => {
            error!("❌ 無法開啟base64緩存樹: {} | 哈希: {}...", e, hash_prefix);
        }
    }
}

// 從緩存獲取base64哈希對應的URL
pub fn get_cached_base64(hash: &str) -> Option<(String, usize)> {
    let hash_prefix = if hash.len() > 8 { &hash[..8] } else { hash };
    debug!("🔍 查詢base64緩存 | 哈希: {}...", hash_prefix);
    let db = get_sled_db();
    let tree_name = "base64";
    let key = format!("base64:{}", hash);
    let result = match db.open_tree(tree_name) {
        Ok(tree) => tree.get(key.as_bytes()),
        Err(e) => {
            error!("❌ 無法開啟base64緩存樹: {}", e);
            return None;
        }
    };
    match result {
        Ok(Some(value_bytes)) => {
            if let Ok(value_str) = String::from_utf8(value_bytes.to_vec()) {
                let parts: Vec<&str> = value_str.split(':').collect();
                if parts.len() >= 3 {
                    if let Ok(expires_secs) = parts[0].parse::<u64>() {
                        let now_secs = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_else(|_| Duration::from_secs(0))
                            .as_secs();
                        // 檢查是否過期
                        if expires_secs > now_secs {
                            // 一個重要的修復：URL中可能含有冒號，需要正確處理
                            let size_str = parts.last().unwrap();
                            let poe_url = parts[1..(parts.len() - 1)].join(":");
                            if let Ok(size) = size_str.parse::<usize>() {
                                // 更新過期時間（延長TTL）
                                refresh_base64_cache_ttl(hash, &poe_url, size);
                                debug!("✅ base64緩存命中並續期 | 哈希: {}...", hash_prefix);
                                return Some((poe_url, size));
                            } else {
                                error!("❌ base64緩存大小無效: {}", size_str);
                            }
                        } else {
                            // 已過期，刪除項目
                            if let Ok(tree) = db.open_tree(tree_name) {
                                let _ = tree.remove(key.as_bytes());
                                debug!("🗑️ 刪除過期base64緩存 | 哈希: {}...", hash_prefix);
                            }
                        }
                    } else {
                        error!("❌ base64緩存時間戳無效: {}", parts[0]);
                    }
                } else {
                    error!(
                        "❌ base64緩存格式錯誤: {} (部分數: {})",
                        value_str,
                        parts.len()
                    );
                }
            } else {
                error!("❌ base64緩存值無法解析為字符串");
            }
            None
        }
        Ok(None) => None,
        Err(e) => {
            error!("❌ 讀取base64緩存失敗: {} | 哈希: {}...", e, hash_prefix);
            None
        }
    }
}

// 刷新base64緩存的TTL
fn refresh_base64_cache_ttl(hash: &str, poe_url: &str, size_bytes: usize) {
    cache_base64(hash, poe_url, size_bytes);
}

// 估算base64數據大小
pub fn estimate_base64_size(data_url: &str) -> usize {
    if let Some(base64_part) = data_url.split(";base64,").nth(1) {
        return (base64_part.len() as f64 * 0.75) as usize;
    }
    0
}

// 檢查並控制緩存大小
fn check_and_control_cache_size() {
    let db = get_sled_db();
    let max_size_mb = get_url_cache_size_mb();
    let max_size_bytes = max_size_mb * 1024 * 1024;
    // 計算當前緩存總大小
    let mut current_size = 0;
    let mut entries = Vec::new();

    // 收集url樹的項目
    if let Ok(tree) = db.open_tree("urls") {
        for (key, value) in tree.iter().flatten() {
            if let Ok(value_str) = String::from_utf8(value.to_vec()) {
                let parts: Vec<&str> = value_str.split(':').collect();
                if parts.len() >= 3 {
                    if let Ok(expires_secs) = parts[0].parse::<u64>() {
                        if let Ok(size) = parts.last().unwrap().parse::<usize>() {
                            current_size += size;
                            entries.push((expires_secs, "urls".to_string(), key.to_vec(), size));
                        }
                    }
                }
            }
        }
    }

    // 收集base64樹的項目
    if let Ok(tree) = db.open_tree("base64") {
        for (key, value) in tree.iter().flatten() {
            if let Ok(value_str) = String::from_utf8(value.to_vec()) {
                let parts: Vec<&str> = value_str.split(':').collect();
                if parts.len() >= 3 {
                    if let Ok(expires_secs) = parts[0].parse::<u64>() {
                        if let Ok(size) = parts.last().unwrap().parse::<usize>() {
                            current_size += size;
                            entries.push((expires_secs, "base64".to_string(), key.to_vec(), size));
                        }
                    }
                }
            }
        }
    }

    // 如果超過最大大小，清理空間
    if current_size > max_size_bytes {
        let excess_bytes = current_size - max_size_bytes;
        let mut bytes_to_free = excess_bytes + (max_size_bytes / 10); // 多釋放10%空間
        info!(
            "⚠️ 緩存大小 ({:.2}MB) 超出限制 ({:.2}MB)，需釋放 {:.2}MB",
            current_size as f64 / 1024.0 / 1024.0,
            max_size_bytes as f64 / 1024.0 / 1024.0,
            bytes_to_free as f64 / 1024.0 / 1024.0
        );

        // 按過期時間排序（最早過期的先刪除）
        entries.sort_by_key(|(expires, _, _, _)| *expires);
        let mut deleted = 0;

        for (_, tree_name, key, size) in entries {
            if bytes_to_free == 0 {
                break;
            }
            if let Ok(tree) = db.open_tree(&tree_name) {
                if let Err(e) = tree.remove(&key) {
                    error!("❌ 刪除緩存項失敗: {}", e);
                } else {
                    bytes_to_free = bytes_to_free.saturating_sub(size);
                    deleted += 1;
                }
            }
        }

        if deleted > 0 {
            info!("🗑️ 已釋放 {} 個緩存項", deleted);
        }
    }
}
