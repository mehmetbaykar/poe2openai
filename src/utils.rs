use crate::types::{Config, ContentItem, Message};
use quick_cache::sync::Cache;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tiktoken_rs::o200k_base;
use tracing::{debug, error, info, warn};

pub static CONFIG_CACHE: std::sync::OnceLock<Cache<String, Arc<Config>>> =
    std::sync::OnceLock::new();

pub fn format_bytes_length(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.2} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

pub fn format_duration(duration: std::time::Duration) -> String {
    if duration.as_secs() > 0 {
        format!("{:.2}s", duration.as_secs_f64())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

pub fn deserialize_content<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::fmt;
    struct ContentVisitor;
    impl<'de> Visitor<'de> for ContentVisitor {
        type Value = String;
        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("string or array of content items")
        }
        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value.to_string())
        }
        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value)
        }
        fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
        where
            S: de::SeqAccess<'de>,
        {
            let mut combined_text = String::new();
            while let Some(item) = seq.next_element::<ContentItem>()? {
                combined_text.push_str(&item.text);
            }
            Ok(combined_text)
        }
    }
    deserializer.deserialize_any(ContentVisitor)
}

pub fn get_config_path(filename: &str) -> PathBuf {
    let config_dir = std::env::var("CONFIG_DIR").unwrap_or_else(|_| "./".to_string());
    let mut path = PathBuf::from(config_dir);
    path.push(filename);
    path
}

pub fn load_config_from_yaml() -> Result<Config, String> {
    let path_str = "models.yaml";
    let path = Path::new(path_str);

    if path.exists() {
        match std::fs::read_to_string(path) {
            Ok(contents) => match serde_yaml::from_str::<Config>(&contents) {
                Ok(config) => {
                    info!("✅ 成功讀取並解析 {}", path_str);
                    Ok(config)
                }
                Err(e) => {
                    error!("❌ 解析 {} 失敗: {}", path_str, e);
                    Err(format!("解析 {} 失敗: {}", path_str, e))
                }
            },
            Err(e) => {
                error!("❌ 讀取 {} 失敗: {}", path_str, e);
                Err(format!("讀取 {} 失敗: {}", path_str, e))
            }
        }
    } else {
        debug!("⚠️  {} 不存在，使用預設空配置", path_str);
        // 返回一個預設的 Config，表示文件不存在或無法讀取
        Ok(Config {
            enable: Some(false),
            models: std::collections::HashMap::new(),
        })
    }
}

pub async fn get_cached_config() -> Arc<Config> {
    let cache_instance = CONFIG_CACHE.get_or_init(|| {
        info!("🚀 正在初始化 YAML 配置緩存...");
        Cache::<String, Arc<Config>>::new(2)
    });

    // 嘗試從緩存獲取，如果失敗則加載
    let config_result = cache_instance.get_or_insert_with("models.yaml", || {
        debug!("💾 YAML 配置緩存未命中，嘗試從 YAML 加載...");
        load_config_from_yaml().map(Arc::new)
    });

    match config_result {
        Ok(config_arc) => {
            debug!("✅ 成功從緩存中取回配置。");
            config_arc
        }
        Err(e) => {
            // 如果從緩存獲取或從文件加載都失敗，返回預設配置
            warn!("⚠️ 無法載入或插入配置到緩存：{}。使用預設空配置。", e);
            Arc::new(Config {
                enable: Some(false),
                models: std::collections::HashMap::new(),
            })
        }
    }
}

/// 計算文本的 token 數量
pub fn count_tokens(text: &str) -> u32 {
    let bpe = match o200k_base() {
        Ok(bpe) => bpe,
        Err(e) => {
            error!("❌ 無法初始化 BPE 編碼器: {}", e);
            return 0;
        }
    };

    let tokens = bpe.encode_with_special_tokens(text);
    tokens.len() as u32
}

/// 計算消息列表的 token 數量
pub fn count_message_tokens(messages: &[Message]) -> u32 {
    let mut total_tokens = 0;

    for message in messages {
        // 每條消息的基本 token 數（角色標記等）
        total_tokens += 4; // 每條消息的基本開銷

        // 計算內容的 token 數
        total_tokens += count_tokens(&message.content);
    }

    // 添加消息格式的額外 token
    total_tokens += 2; // 消息格式的開始和結束標記

    total_tokens
}

/// 計算完成內容的 token 數量
pub fn count_completion_tokens(completion: &str) -> u32 {
    count_tokens(completion)
}
