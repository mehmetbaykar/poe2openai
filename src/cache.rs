use crate::types::Config;
use crate::utils::load_config_from_yaml;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::debug;
use tracing::{error, info, warn};

/// Global Sled DB
pub static SLED_DB: OnceLock<sled::Db> = OnceLock::new();

/// Get in-memory sled::Db, only initialized once
pub fn get_sled_db() -> &'static sled::Db {
    SLED_DB.get_or_init(|| {
        sled::Config::new()
            .temporary(true)
            .open()
            .expect("Unable to initialize sled memory cache")
    })
}

/// Store config in sled
pub fn save_config_sled(key: &str, config: &Config) -> Result<(), String> {
    let db = get_sled_db();
    match serde_json::to_vec(config) {
        Ok(bytes) => {
            db.insert(key.as_bytes(), bytes)
                .map_err(|e| format!("Failed to write Sled cache: {}", e))?;
            db.flush().ok();
            Ok(())
        }
        Err(e) => Err(format!("Failed to serialize config: {}", e)),
    }
}

/// Load config
pub fn load_config_sled(key: &str) -> Result<Option<Arc<Config>>, String> {
    let db = get_sled_db();
    match db.get(key.as_bytes()) {
        Ok(Some(bytes)) => match serde_json::from_slice::<Config>(&bytes) {
            Ok(conf) => Ok(Some(Arc::new(conf))),
            Err(e) => {
                error!("❌ Sled config parsing failed: {}", e);
                Err(format!("JSON parsing failed: {}", e))
            }
        },
        Ok(None) => Ok(None),
        Err(e) => {
            error!("❌ Failed to read Sled config: {}", e);
            Err(format!("Loading failed: {}", e))
        }
    }
}

/// Remove a key
pub fn remove_config_sled(key: &str) {
    let db = get_sled_db();
    if let Err(e) = db.remove(key.as_bytes()) {
        warn!("⚠️ Error removing cache from sled: {}", e);
    }
    db.flush().ok();
}

// Get config from cache or YAML
pub async fn get_cached_config() -> Arc<Config> {
    let cache_key = "models.yaml";
    // Try sled read (cache first, fall back to yaml on failure)
    match load_config_sled(cache_key) {
        Ok(Some(arc_cfg)) => {
            debug!("✅ Sled cache hit: {}", cache_key);
            arc_cfg
        }
        Ok(None) | Err(_) => {
            debug!("💾 No config in sled, reading from YAML...");
            match load_config_from_yaml() {
                Ok(conf) => {
                    let _ = save_config_sled(cache_key, &conf);
                    Arc::new(conf)
                }
                Err(e) => {
                    warn!("⚠️ Unable to load config from YAML, falling back to default: {}", e);
                    Arc::new(Config {
                        enable: Some(false),
                        models: std::collections::HashMap::new(),
                        custom_models: None,
                        api_token: None,
                        use_v1_api: None,
                    })
                }
            }
        }
    }
}

// Get URL cache TTL
pub fn get_url_cache_ttl() -> Duration {
    let ttl_seconds = std::env::var("URL_CACHE_TTL_SECONDS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(3 * 24 * 60 * 60); // Default 3 days
    Duration::from_secs(ttl_seconds)
}

// Get URL cache maximum size (MB)
pub fn get_url_cache_size_mb() -> usize {
    std::env::var("URL_CACHE_SIZE_MB")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100) // Default 100MB
}

// Store URL in cache with expiration time
pub fn cache_url(original_url: &str, poe_url: &str, size_bytes: usize) {
    let db = get_sled_db();
    let tree_name = "urls";
    let ttl = get_url_cache_ttl();
    let key = format!("url:{}", original_url);
    // Current time + TTL
    let expires_at = SystemTime::now()
        .checked_add(ttl)
        .unwrap_or_else(|| SystemTime::now() + ttl);
    // Convert to timestamp
    let expires_secs = expires_at
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs();
    // Store data, format "expiration_timestamp:poe_url:size"
    // Ensure colons in URL don't interfere with parsing
    let store_value = format!("{}:{}:{}", expires_secs, poe_url, size_bytes);
    if let Ok(tree) = db.open_tree(tree_name) {
        match tree.insert(key.as_bytes(), store_value.as_bytes()) {
            Ok(_) => {
                debug!("✅ URL cache updated: {}", original_url);
            }
            Err(e) => {
                error!("❌ Failed to save URL cache: {}", e);
            }
        }
    } else {
        error!("❌ Unable to open URL cache tree");
    }
    // Maintain cache size
    check_and_control_cache_size();
}

// Get cached URL
pub fn get_cached_url(original_url: &str) -> Option<(String, usize)> {
    let db = get_sled_db();
    let tree_name = "urls";
    let key = format!("url:{}", original_url);
    let result = match db.open_tree(tree_name) {
        Ok(tree) => tree.get(key.as_bytes()),
        Err(e) => {
            error!("❌ Unable to open URL cache tree: {}", e);
            return None;
        }
    };
    match result {
        Ok(Some(value_bytes)) => {
            if let Ok(value_str) = String::from_utf8(value_bytes.to_vec()) {
                let parts: Vec<&str> = value_str.split(':').collect();
                if parts.len() >= 3 {
                    // Correctly parse format: "expires_at:poe_url:size"
                    if let Ok(expires_secs) = parts[0].parse::<u64>() {
                        let now_secs = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_else(|_| Duration::from_secs(0))
                            .as_secs();
                        // Check if expired
                        if expires_secs > now_secs {
                            // Important fix: URL may contain colons, need to handle correctly
                            // Take first part as expiration time, last part as size, middle parts as URL
                            let size_str = parts.last().unwrap();
                            let poe_url = parts[1..(parts.len() - 1)].join(":");
                            if let Ok(size) = size_str.parse::<usize>() {
                                // Update expiration time (extend TTL)
                                refresh_url_cache_ttl(original_url, &poe_url, size);
                                debug!("✅ URL cache hit and renewed: {}", original_url);
                                return Some((poe_url, size));
                            }
                        } else {
                            // Expired, delete entry
                            if let Ok(tree) = db.open_tree(tree_name) {
                                let _ = tree.remove(key.as_bytes());
                                debug!("🗑️ Deleted expired URL cache: {}", original_url);
                            }
                        }
                    }
                }
            } else {
                error!("❌ Invalid URL cache value format");
            }
            None
        }
        Ok(None) => None,
        Err(e) => {
            error!("❌ Failed to read URL cache: {}", e);
            None
        }
    }
}

// Refresh URL cache TTL
fn refresh_url_cache_ttl(original_url: &str, poe_url: &str, size_bytes: usize) {
    cache_url(original_url, poe_url, size_bytes);
}

// Save base64 hash to cache
pub fn cache_base64(hash: &str, poe_url: &str, size_bytes: usize) {
    let db = get_sled_db();
    let tree_name = "base64";
    let ttl = get_url_cache_ttl();
    let key = format!("base64:{}", hash);
    let hash_prefix = if hash.len() > 8 { &hash[..8] } else { hash };
    // Current time + TTL
    let expires_at = SystemTime::now()
        .checked_add(ttl)
        .unwrap_or_else(|| SystemTime::now() + ttl);
    // Convert to timestamp
    let expires_secs = expires_at
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs();
    // Store data, format "expires_secs:poe_url:size_bytes"
    let store_value = format!("{}:{}:{}", expires_secs, poe_url, size_bytes);
    debug!(
        "💾 Storing base64 cache | Hash: {}... | Size: {}bytes",
        hash_prefix, size_bytes
    );
    match db.open_tree(tree_name) {
        Ok(tree) => match tree.insert(key.as_bytes(), store_value.as_bytes()) {
            Ok(_) => {
                debug!("✅ base64 cache updated | Hash: {}...", hash_prefix);
            }
            Err(e) => {
                error!("❌ Failed to save base64 cache: {} | Hash: {}...", e, hash_prefix);
            }
        },
        Err(e) => {
            error!("❌ Unable to open base64 cache tree: {} | Hash: {}...", e, hash_prefix);
        }
    }
}

// Get base64 hash corresponding URL from cache
pub fn get_cached_base64(hash: &str) -> Option<(String, usize)> {
    let hash_prefix = if hash.len() > 8 { &hash[..8] } else { hash };
    debug!("🔍 Querying base64 cache | Hash: {}...", hash_prefix);
    let db = get_sled_db();
    let tree_name = "base64";
    let key = format!("base64:{}", hash);
    let result = match db.open_tree(tree_name) {
        Ok(tree) => tree.get(key.as_bytes()),
        Err(e) => {
            error!("❌ Unable to open base64 cache tree: {}", e);
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
                        // Check if expired
                        if expires_secs > now_secs {
                            // Important fix: URL may contain colons, need to handle correctly
                            let size_str = parts.last().unwrap();
                            let poe_url = parts[1..(parts.len() - 1)].join(":");
                            if let Ok(size) = size_str.parse::<usize>() {
                                // Update expiration time (extend TTL)
                                refresh_base64_cache_ttl(hash, &poe_url, size);
                                debug!("✅ base64 cache hit and renewed | Hash: {}...", hash_prefix);
                                return Some((poe_url, size));
                            } else {
                                error!("❌ Invalid base64 cache size: {}", size_str);
                            }
                        } else {
                            // Expired, delete entry
                            if let Ok(tree) = db.open_tree(tree_name) {
                                let _ = tree.remove(key.as_bytes());
                                debug!("🗑️ Deleted expired base64 cache | Hash: {}...", hash_prefix);
                            }
                        }
                    } else {
                        error!("❌ Invalid base64 cache timestamp: {}", parts[0]);
                    }
                } else {
                    error!(
                        "❌ Invalid base64 cache format: {} (parts count: {})",
                        value_str,
                        parts.len()
                    );
                }
            } else {
                error!("❌ base64 cache value cannot be parsed as string");
            }
            None
        }
        Ok(None) => None,
        Err(e) => {
            error!("❌ Failed to read base64 cache: {} | Hash: {}...", e, hash_prefix);
            None
        }
    }
}

// Refresh base64 cache TTL
fn refresh_base64_cache_ttl(hash: &str, poe_url: &str, size_bytes: usize) {
    cache_base64(hash, poe_url, size_bytes);
}

// Estimate base64 data size
pub fn estimate_base64_size(data_url: &str) -> usize {
    if let Some(base64_part) = data_url.split(";base64,").nth(1) {
        return (base64_part.len() as f64 * 0.75) as usize;
    }
    0
}

// Check and control cache size
fn check_and_control_cache_size() {
    let db = get_sled_db();
    let max_size_mb = get_url_cache_size_mb();
    let max_size_bytes = max_size_mb * 1024 * 1024;
    // Calculate current cache total size
    let mut current_size = 0;
    let mut entries = Vec::new();

    // Collect url tree entries
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

    // Collect base64 tree entries
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

    // If exceeds maximum size, clean up space
    if current_size > max_size_bytes {
        let excess_bytes = current_size - max_size_bytes;
        let mut bytes_to_free = excess_bytes + (max_size_bytes / 10); // Free 10% extra space
        info!(
            "⚠️ Cache size ({:.2}MB) exceeds limit ({:.2}MB), need to free {:.2}MB",
            current_size as f64 / 1024.0 / 1024.0,
            max_size_bytes as f64 / 1024.0 / 1024.0,
            bytes_to_free as f64 / 1024.0 / 1024.0
        );

        // Sort by expiration time (delete earliest expiring first)
        entries.sort_by_key(|(expires, _, _, _)| *expires);
        let mut deleted = 0;

        for (_, tree_name, key, size) in entries {
            if bytes_to_free == 0 {
                break;
            }
            if let Ok(tree) = db.open_tree(&tree_name) {
                if let Err(e) = tree.remove(&key) {
                    error!("❌ Failed to delete cache entry: {}", e);
                } else {
                    bytes_to_free = bytes_to_free.saturating_sub(size);
                    deleted += 1;
                }
            }
        }

        if deleted > 0 {
            info!("🗑️ Freed {} cache entries", deleted);
        }
    }
}
