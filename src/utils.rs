use crate::poe_client::PoeClientWrapper;
use crate::types::{Config, ImageUrlContent, Message, OpenAiContent, OpenAiContentItem};
use crate::types::{OpenAIError, OpenAIErrorResponse};
use base64::prelude::*;
use nanoid::nanoid;
use poe_api_process::FileUploadRequest;
use salvo::http::StatusCode;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use tiktoken_rs::o200k_base;
use tracing::{debug, error, info, warn};

// Process files/images in messages
pub async fn process_message_images(
    poe_client: &PoeClientWrapper,
    messages: &mut [Message],
) -> Result<(), Box<dyn std::error::Error>> {
    // Collect URLs that need processing
    let mut external_urls = Vec::new();
    let mut data_urls = Vec::new();
    let mut url_indices = Vec::new();
    let mut data_url_indices = Vec::new();
    let mut temp_files: Vec<PathBuf> = Vec::new();

    // Collect all URLs that need processing in messages
    for (msg_idx, message) in messages.iter().enumerate() {
        if let Some(OpenAiContent::Multi(items)) = &message.content {
            for (item_idx, item) in items.iter().enumerate() {
                if let OpenAiContentItem::ImageUrl { image_url, .. } = item {
                    if image_url.url.starts_with("data:") {
                        // Process data URL
                        debug!("🔍 Found data URL");
                        data_urls.push(image_url.url.clone());
                        data_url_indices.push((msg_idx, item_idx));
                    } else if !is_poe_cdn_url(&image_url.url) {
                        // Process external URLs that need uploading
                        debug!(
                            "🔍 Found external URL that needs uploading: {}",
                            image_url.url
                        );
                        external_urls.push(image_url.url.clone());
                        url_indices.push((msg_idx, item_idx));
                    }
                }
            }
        }
    }

    // Process external URLs
    if !external_urls.is_empty() {
        debug!(
            "🔄 Preparing to process {} external URLs",
            external_urls.len()
        );

        // Divide external URLs into cache hit and miss groups
        let mut urls_to_upload = Vec::new();
        let mut urls_indices_to_upload = Vec::new();

        for (idx, (msg_idx, item_idx)) in url_indices.iter().enumerate() {
            let url = &external_urls[idx];

            // Check cache
            if let Some((poe_url, _)) = crate::cache::get_cached_url(url) {
                debug!("✅ URL cache hit: {} -> {}", url, poe_url);

                if let Some(OpenAiContent::Multi(items)) = &mut messages[*msg_idx].content {
                    if let OpenAiContentItem::ImageUrl { image_url, .. } = &mut items[*item_idx] {
                        debug!("🔄 Replace URL from cache: {}", poe_url);
                        image_url.url = poe_url;
                    }
                }
            } else {
                // Cache miss, need to upload
                debug!("❌ URL cache miss: {}", url);
                urls_to_upload.push(url.clone());
                urls_indices_to_upload.push((*msg_idx, *item_idx));
            }
        }

        // Upload uncached URLs
        if !urls_to_upload.is_empty() {
            debug!("🔄 Uploading {} uncached URLs", urls_to_upload.len());

            let upload_requests: Vec<FileUploadRequest> = urls_to_upload
                .iter()
                .map(|url| FileUploadRequest::RemoteFile {
                    download_url: url.clone(),
                })
                .collect();

            match poe_client.client.upload_files_batch(upload_requests).await {
                Ok(responses) => {
                    debug!("✅ Successfully uploaded {} external URLs", responses.len());

                    // Update cache and save URL mappings
                    for (idx, ((msg_idx, item_idx), response)) in urls_indices_to_upload
                        .iter()
                        .zip(responses.iter())
                        .enumerate()
                    {
                        let original_url = &urls_to_upload[idx];

                        // Estimate size (default 1MB, can be optimized in actual usage)
                        let size_bytes = 1024 * 1024;

                        // Add to cache
                        crate::cache::cache_url(original_url, &response.attachment_url, size_bytes);

                        if let Some(OpenAiContent::Multi(items)) = &mut messages[*msg_idx].content {
                            if let OpenAiContentItem::ImageUrl { image_url, .. } =
                                &mut items[*item_idx]
                            {
                                debug!(
                                    "🔄 Replace URL | Original: {} | Poe: {}",
                                    image_url.url, response.attachment_url
                                );
                                image_url.url = response.attachment_url.clone();
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("❌ Failed to upload external URLs: {}", e);
                    return Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("Failed to upload external URLs: {}", e),
                    )));
                }
            }
        }
    }

    // Process data URLs
    if !data_urls.is_empty() {
        debug!("🔄 Preparing to process {} data URLs", data_urls.len());

        // Divide into cache hit and miss groups
        let mut data_to_upload = Vec::new();
        let mut data_indices_to_upload = Vec::new();
        let mut data_hashes = Vec::new();

        for (idx, (msg_idx, item_idx)) in data_url_indices.iter().enumerate() {
            let data_url = &data_urls[idx];
            let hash = hash_base64_content(data_url);

            debug!("🔍 Calculated data URL hash | Hash head: {}...", &hash[..8]);

            // Check cache
            if let Some((poe_url, _)) = crate::cache::get_cached_base64(&hash) {
                debug!(
                    "✅ base64 cache hit | Hash: {}... -> {}",
                    &hash[..8],
                    poe_url
                );

                if let Some(OpenAiContent::Multi(items)) = &mut messages[*msg_idx].content {
                    if let OpenAiContentItem::ImageUrl { image_url, .. } = &mut items[*item_idx] {
                        debug!("🔄 Replace base64 from cache | URL: {}", poe_url);
                        image_url.url = poe_url;
                    }
                }
            } else {
                // Cache miss, need to upload
                debug!("❌ base64 cache miss | Hash: {}...", &hash[..8]);
                data_to_upload.push(data_url.clone());
                data_indices_to_upload.push((idx, (*msg_idx, *item_idx)));
                data_hashes.push(hash);
            }
        }

        // Upload uncached data URLs
        if !data_to_upload.is_empty() {
            let mut upload_requests = Vec::new();

            // Convert data URLs to temporary files
            for data_url in data_to_upload.iter() {
                // Extract MIME type from data URL
                let mime_type = if data_url.starts_with("data:") {
                    let parts: Vec<&str> = data_url.split(";base64,").collect();
                    if !parts.is_empty() {
                        let mime_part = parts[0].trim_start_matches("data:");
                        debug!("🔍 Extracted MIME type: {}", mime_part);
                        Some(mime_part.to_string())
                    } else {
                        None
                    }
                } else {
                    None
                };

                match handle_data_url_to_temp_file(data_url) {
                    Ok(file_path) => {
                        debug!(
                            "📄 Created temporary file successfully: {}",
                            file_path.display()
                        );
                        upload_requests.push(FileUploadRequest::LocalFile {
                            file: file_path.to_string_lossy().to_string(),
                            mime_type,
                        });
                        temp_files.push(file_path);
                    }
                    Err(e) => {
                        error!("❌ Failed to process data URL: {}", e);
                        // Clean up created temporary files
                        for path in &temp_files {
                            if let Err(e) = fs::remove_file(path) {
                                warn!(
                                    "⚠️ Unable to delete temporary file {}: {}",
                                    path.display(),
                                    e
                                );
                            }
                        }
                        return Err(Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("Failed to process data URL: {}", e),
                        )));
                    }
                }
            }

            // Upload temporary files
            if !upload_requests.is_empty() {
                match poe_client.client.upload_files_batch(upload_requests).await {
                    Ok(responses) => {
                        debug!(
                            "✅ Successfully uploaded {} temporary files",
                            responses.len()
                        );

                        // Update cache and save URL mappings
                        for (idx, response) in responses.iter().enumerate() {
                            let (_, (msg_idx, item_idx)) = data_indices_to_upload[idx];
                            let hash = &data_hashes[idx];
                            let data_url = &data_to_upload[idx];

                            // Estimate size
                            let size = crate::cache::estimate_base64_size(data_url);

                            // Add to cache
                            crate::cache::cache_base64(hash, &response.attachment_url, size);

                            debug!(
                                "🔄 Map base64 hash to Poe URL | Hash: {}... -> {}",
                                &hash[..8],
                                response.attachment_url
                            );

                            if let Some(OpenAiContent::Multi(items)) =
                                &mut messages[msg_idx].content
                            {
                                if let OpenAiContentItem::ImageUrl { image_url, .. } =
                                    &mut items[item_idx]
                                {
                                    debug!(
                                        "🔄 Replace data URL | Poe: {}",
                                        response.attachment_url
                                    );
                                    image_url.url = response.attachment_url.clone();
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("❌ Failed to upload temporary files: {}", e);
                        // Clean up temporary files
                        for path in &temp_files {
                            if let Err(e) = fs::remove_file(path) {
                                warn!(
                                    "⚠️ Unable to delete temporary file {}: {}",
                                    path.display(),
                                    e
                                );
                            }
                        }
                        return Err(Box::new(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("Failed to upload temporary files: {}", e),
                        )));
                    }
                }
            }

            // Clean up temporary files
            for path in &temp_files {
                if let Err(e) = fs::remove_file(path) {
                    warn!(
                        "⚠️ Unable to delete temporary file {}: {}",
                        path.display(),
                        e
                    );
                } else {
                    debug!("🗑️ Deleted temporary file: {}", path.display());
                }
            }
        }
    }

    // Process Poe CDN links in AI replies, add them to user message's image_url
    if messages.len() >= 2 {
        // Find the last AI reply and user message
        let last_bot_idx = messages
            .iter()
            .enumerate()
            .filter(|(_, msg)| msg.role == "assistant")
            .last()
            .map(|(i, _)| i);
        let last_user_idx = messages
            .iter()
            .enumerate()
            .filter(|(_, msg)| msg.role == "user")
            .last()
            .map(|(i, _)| i);

        if let (Some(bot_idx), Some(user_idx)) = (last_bot_idx, last_user_idx) {
            // Extract Poe CDN links from AI reply
            let poe_cdn_urls = extract_poe_cdn_urls_from_message(&messages[bot_idx]);
            if !poe_cdn_urls.is_empty() {
                debug!(
                    "🔄 Extracted {} Poe CDN links from AI reply, adding to user message",
                    poe_cdn_urls.len()
                );
                // Add these links to user message's image_url
                let user_msg = &mut messages[user_idx];
                match &mut user_msg.content {
                    Some(OpenAiContent::Text(text)) => {
                        // Convert text message to multi-part message, add images
                        let mut items = Vec::new();
                        items.push(OpenAiContentItem::Text {
                            r#type: Some("text".to_string()),
                            text: text.clone(),
                            extra: HashMap::new(),
                        });
                        for url in poe_cdn_urls.iter() {
                            items.push(OpenAiContentItem::ImageUrl {
                                r#type: Some("image_url".to_string()),
                                image_url: ImageUrlContent {
                                    url: url.clone(),
                                    extra: HashMap::new(),
                                },
                                extra: HashMap::new(),
                            });
                        }
                        user_msg.content = Some(OpenAiContent::Multi(items));
                    }
                    Some(OpenAiContent::Multi(items)) => {
                        // Already multi-part message, add images directly
                        for url in poe_cdn_urls.iter() {
                            items.push(OpenAiContentItem::ImageUrl {
                                r#type: Some("image_url".to_string()),
                                image_url: ImageUrlContent {
                                    url: url.clone(),
                                    extra: HashMap::new(),
                                },
                                extra: HashMap::new(),
                            });
                        }
                    }
                    None => {
                        // If no content, create new multi-part message
                        let mut items = Vec::new();
                        for url in poe_cdn_urls.iter() {
                            items.push(OpenAiContentItem::ImageUrl {
                                r#type: Some("image_url".to_string()),
                                image_url: ImageUrlContent {
                                    url: url.clone(),
                                    extra: HashMap::new(),
                                },
                                extra: HashMap::new(),
                            });
                        }
                        user_msg.content = Some(OpenAiContent::Multi(items));
                    }
                }
            }
        }
    }

    Ok(())
}

// Get pure text content from OpenAIContent
pub fn get_text_from_openai_content(content: &Option<OpenAiContent>) -> String {
    match content {
        Some(OpenAiContent::Text(s)) => s.clone(),
        Some(OpenAiContent::Multi(items)) => {
            let mut text_parts = Vec::new();
            for item in items {
                match item {
                    OpenAiContentItem::Text { text, .. } => match serde_json::to_string(text) {
                        Ok(processed_text) => {
                            let processed_text = processed_text.trim_matches('"').to_string();
                            let processed_text = processed_text.replace("\\\"", "\"");
                            text_parts.push(processed_text);
                        }
                        Err(_) => {
                            text_parts.push(text.clone());
                        }
                    },
                    OpenAiContentItem::ToolResult { content, .. } => {
                        if content.is_string() {
                            if let Some(text) = content.as_str() {
                                text_parts.push(text.to_string());
                            }
                        } else if !content.is_null() {
                            if let Ok(serialized) = serde_json::to_string(content) {
                                text_parts.push(serialized);
                            }
                        }
                    }
                    OpenAiContentItem::Other(value) => {
                        if let Some(text) = value.get("text").and_then(|v| v.as_str()) {
                            text_parts.push(text.to_string());
                        }
                    }
                    _ => {}
                }
            }
            text_parts.join("\n")
        }
        None => String::new(),
    }
}

// Check if URL is a Poe CDN link
pub fn is_poe_cdn_url(url: &str) -> bool {
    url.starts_with("https://pfst.cf2.poecdn.net")
}

// Extract Poe CDN links from message
pub fn extract_poe_cdn_urls_from_message(message: &Message) -> Vec<String> {
    let mut urls = Vec::new();
    match &message.content {
        Some(OpenAiContent::Multi(items)) => {
            for item in items {
                if let OpenAiContentItem::ImageUrl { image_url, .. } = item {
                    if is_poe_cdn_url(&image_url.url) {
                        urls.push(image_url.url.clone());
                    }
                } else if let OpenAiContentItem::Text { text, .. } = item {
                    // Extract Poe CDN URL from text
                    extract_urls_from_markdown(text, &mut urls);
                }
            }
        }
        Some(OpenAiContent::Text(text)) => {
            // Extract Poe CDN URL from plain text message
            extract_urls_from_markdown(text, &mut urls);
        }
        None => {
            // No content, return empty list
        }
    }
    urls
}

// Helper function to extract Poe CDN URLs from Markdown text
fn extract_urls_from_markdown(text: &str, urls: &mut Vec<String>) {
    // Extract Markdown image format URL: ![alt](url)
    let re_md_img = regex::Regex::new(r"!\[.*?\]\((https?://[^\s)]+)\)").unwrap();
    for cap in re_md_img.captures_iter(text) {
        if let Some(url) = cap.get(1) {
            let url_str = url.as_str();
            if is_poe_cdn_url(url_str) {
                urls.push(url_str.to_string());
            }
        }
    }
    // Also handle directly appearing URLs
    for word in text.split_whitespace() {
        if is_poe_cdn_url(word) {
            urls.push(word.to_string());
        }
    }
}

// Handle base64 data URL, store it as a temporary file
pub fn handle_data_url_to_temp_file(data_url: &str) -> Result<PathBuf, String> {
    // 1. Validate data URL format
    if !data_url.starts_with("data:") {
        return Err("Invalid data URL format".to_string());
    }
    // 2. Separate MIME type and base64 data
    let parts: Vec<&str> = data_url.split(";base64,").collect();
    if parts.len() != 2 {
        return Err("Invalid data URL format: missing base64 separator".to_string());
    }
    // 3. Extract MIME type
    let mime_type = parts[0].strip_prefix("data:").unwrap_or(parts[0]);
    debug!("🔍 Extracted MIME type: {}", mime_type);
    // 4. Determine file extension based on MIME type
    let file_ext = mime_type_to_extension(mime_type).unwrap_or("bin");
    debug!("📄 Using file extension: {}", file_ext);
    // 5. Decode base64 data (only use BASE64_STANDARD)
    let base64_data = parts[1];
    debug!("🔢 Base64 data length: {}", base64_data.len());
    let decoded = match BASE64_STANDARD.decode(base64_data) {
        Ok(data) => {
            debug!(
                "✅ Base64 decoding successful | Data size: {} bytes",
                data.len()
            );
            data
        }
        Err(e) => {
            error!("❌ Base64 decoding failed: {}", e);
            return Err(format!("Base64 decoding failed: {}", e));
        }
    };
    // 6. Create temporary file
    let temp_dir = std::env::temp_dir();
    let file_name = format!("poe2openai_{}.{}", nanoid!(16), file_ext);
    let file_path = temp_dir.join(&file_name);
    // 7. Write data to temporary file
    match fs::write(&file_path, &decoded) {
        Ok(_) => {
            debug!(
                "✅ Successfully wrote temporary file: {}",
                file_path.display()
            );
            Ok(file_path)
        }
        Err(e) => {
            error!("❌ Failed to write temporary file: {}", e);
            Err(format!("Failed to write temporary file: {}", e))
        }
    }
}

// Get file extension from MIME type
fn mime_type_to_extension(mime_type: &str) -> Option<&str> {
    match mime_type {
        "image/jpeg" | "image/jpg" => Some("jpeg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/svg+xml" => Some("svg"),
        "image/bmp" => Some("bmp"),
        "image/tiff" => Some("tiff"),
        "application/pdf" => Some("pdf"),
        "text/plain" => Some("txt"),
        "text/csv" => Some("csv"),
        "application/json" => Some("json"),
        "application/xml" | "text/xml" => Some("xml"),
        "application/zip" => Some("zip"),
        "application/x-tar" => Some("tar"),
        "application/x-gzip" => Some("gz"),
        "audio/mpeg" => Some("mp3"),
        "audio/wav" => Some("wav"),
        "audio/ogg" => Some("ogg"),
        "video/mp4" => Some("mp4"),
        "video/mpeg" => Some("mpeg"),
        "video/quicktime" => Some("mov"),
        _ => None,
    }
}

pub fn convert_poe_error_to_openai(
    error_text: &str,
    allow_retry: bool,
) -> (StatusCode, OpenAIErrorResponse) {
    debug!(
        "🔄 Converting error response | Error text: {}, Allow retry: {}",
        error_text, allow_retry
    );
    let (status, error_type, code) = if error_text.contains("Internal server error") {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "internal_error",
        )
    } else if error_text.contains("rate limit") {
        (
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_exceeded",
            "rate_limit_exceeded",
        )
    } else if error_text.contains("Invalid token") || error_text.contains("Unauthorized") {
        (StatusCode::UNAUTHORIZED, "invalid_auth", "invalid_api_key")
    } else if error_text.contains("Bot does not exist") {
        (StatusCode::NOT_FOUND, "model_not_found", "model_not_found")
    } else {
        (StatusCode::BAD_REQUEST, "invalid_request", "bad_request")
    };
    debug!(
        "📋 Error conversion result | Status code: {} | Error type: {}",
        status.as_u16(),
        error_type
    );
    (
        status,
        OpenAIErrorResponse {
            error: OpenAIError {
                message: error_text.to_string(),
                r#type: error_type.to_string(),
                code: code.to_string(),
                param: None,
            },
        },
    )
}

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

pub fn get_config_path(filename: &str) -> PathBuf {
    let config_dir = std::env::var("CONFIG_DIR").unwrap_or_else(|_| "./".to_string());
    let mut path = PathBuf::from(config_dir);
    path.push(filename);
    path
}

pub fn load_config_from_yaml() -> Result<Config, String> {
    let path_str = "models.yaml";
    let path = get_config_path(path_str);
    if path.exists() {
        match std::fs::read_to_string(path) {
            Ok(contents) => match serde_yaml::from_str::<Config>(&contents) {
                Ok(config) => {
                    info!("✅ Successfully read and parsed {}", path_str);
                    Ok(config)
                }
                Err(e) => {
                    error!("❌ Failed to parse {}: {}", path_str, e);
                    Err(format!("Failed to parse {}: {}", path_str, e))
                }
            },
            Err(e) => {
                error!("❌ Failed to read {}: {}", path_str, e);
                Err(format!("Failed to read {}: {}", path_str, e))
            }
        }
    } else {
        debug!(
            "⚠️  {} does not exist, using default empty config",
            path_str
        );
        // Return a default Config, indicating the file does not exist or cannot be read
        Ok(Config {
            enable: Some(false),
            models: std::collections::HashMap::new(),
            custom_models: None,
            api_token: None,
            use_v1_api: None,
        })
    }
}

/// Calculate token count for text
pub fn count_tokens(text: &str) -> u32 {
    let bpe = match o200k_base() {
        Ok(bpe) => bpe,
        Err(e) => {
            error!("❌ Failed to initialize BPE encoder: {}", e);
            return 0;
        }
    };
    let tokens = bpe.encode_with_special_tokens(text);
    tokens.len() as u32
}

/// Calculate token count for message list
pub fn count_message_tokens(messages: &[Message]) -> u32 {
    let mut total_tokens = 0;
    for message in messages {
        // Basic token count for each message (role marker, etc.)
        total_tokens += 4; // Basic overhead for each message
        // Calculate token count for content
        let content_text = get_text_from_openai_content(&message.content);
        total_tokens += count_tokens(&content_text);
    }
    // Add extra tokens for message format
    total_tokens += 2; // Start and end markers for message format
    total_tokens
}

/// Calculate token count for completion content
pub fn count_completion_tokens(completion: &str) -> u32 {
    count_tokens(completion)
}

/// Calculate SHA256 hash for base64 string
pub fn hash_base64_content(base64_str: &str) -> String {
    // Extract pure base64 part, remove MIME type prefix
    let base64_data = match base64_str.split(";base64,").nth(1) {
        Some(data) => data,
        None => base64_str, // If no separator, use the entire string
    };

    let start = &base64_data[..base64_data.len().min(1024)];
    let end = if base64_data.len() > 2048 {
        // Ensure enough length
        &base64_data[base64_data.len() - 1024..]
    } else if base64_data.len() > 1024 {
        &base64_data[1024..] // If length is between 1024-2048, use remaining part
    } else {
        "" // If less than 1024, only use start
    };

    // Combine head and tail data
    let combined = format!("{}{}", start, end);

    // Calculate SHA256 hash
    let mut hasher = Sha256::new();
    hasher.update(combined.as_bytes());
    let result = hasher.finalize();

    // Log hash calculation info for debugging
    let hash = format!("{:x}", result);
    debug!(
        "🔢 Calculating base64 hash | Data length: {} | Calculated length: {} | Hash head: {}...",
        base64_data.len(),
        start.len() + end.len(),
        &hash[..8]
    );

    hash
}

/// Process message content, add appropriate suffix based on request parameters
pub fn process_message_content_with_suffixes(
    content: &str,
    chat_request: &crate::types::ChatCompletionRequest,
) -> String {
    let mut processed_content = content.to_string();

    // Process function tools - check if only name field
    if let Some(tools) = &chat_request.tools {
        for tool in tools {
            // Check if only name field (description is None or empty string)
            let has_description = tool
                .function
                .description
                .as_ref()
                .map(|desc| !desc.is_empty())
                .unwrap_or(false);

            if !has_description {
                let suffix = format!(" --{}", tool.function.name);
                debug!("🔧 Adding function name suffix: {}", suffix);
                processed_content.push_str(&suffix);
            }
        }
    }

    // Process thinking_budget
    let thinking_budget = if let Some(thinking) = &chat_request.thinking {
        thinking.budget_tokens
    } else if let Some(extra_body) = &chat_request.extra_body {
        extra_body
            .google
            .as_ref()
            .and_then(|g| g.thinking_config.as_ref())
            .and_then(|tc| tc.thinking_budget)
    } else {
        None
    };
    if let Some(budget) = thinking_budget {
        // Only add --thinking_budget parameter if it's in a positive range
        if budget >= 0 {
            let suffix = format!(" --thinking_budget {}", budget);
            debug!("🧠 Adding thinking_budget suffix: {}", suffix);
            processed_content.push_str(&suffix);
        } else {
            debug!(
                "🧠 thinking_budget value {} is out of range, skipping --thinking_budget parameter",
                budget
            );
        }
    }

    // Process reasoning_effort
    if let Some(effort) = &chat_request.reasoning_effort {
        // Validate value is a valid option
        let valid_efforts = ["low", "medium", "high"];
        if valid_efforts.contains(&effort.as_str()) {
            let suffix = format!(" --reasoning_effort {}", effort);
            debug!("🎯 Adding reasoning_effort suffix: {}", suffix);
            processed_content.push_str(&suffix);
        } else {
            warn!("⚠️ Invalid reasoning_effort value: {}", effort);
        }
    }

    processed_content
}

/// Filter out tools that only have name fields, these tools should not be passed to poe_api_process
pub fn filter_tools_for_poe(
    tools: &Option<Vec<poe_api_process::types::ChatTool>>,
) -> Option<Vec<poe_api_process::types::ChatTool>> {
    if let Some(tools_vec) = tools {
        let mut filtered_tools = Vec::new();

        for tool in tools_vec {
            let function_name = tool.function.name.trim();
            if function_name.is_empty() {
                warn!("⚠️ Skipping tool without function name: {:?}", tool.extra);
                continue;
            }

            if tool
                .function
                .description
                .as_ref()
                .map(|desc| desc.trim().is_empty())
                .unwrap_or(true)
            {
                debug!(
                    "🔧 Retaining tool '{}' without description to stay OpenAI-compatible",
                    function_name
                );
            }

            filtered_tools.push(tool.clone());
        }

        if filtered_tools.is_empty() {
            debug!("🔧 No valid tools remain after validation, removing all tools");
            None
        } else {
            debug!(
                "🔧 Filtered down to {} tools (originally {})",
                filtered_tools.len(),
                tools_vec.len()
            );
            Some(filtered_tools)
        }
    } else {
        None
    }
}

/// Extract tool_call_id from tool message
pub fn extract_tool_call_id(content: &str) -> Option<String> {
    // Try to parse JSON content
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(content) {
        if let Some(tool_call_id) = json.get("tool_call_id").and_then(|v| v.as_str()) {
            return Some(tool_call_id.to_string());
        }
    }
    // Try simple text parsing
    if let Some(start) = content.find("tool_call_id") {
        if let Some(id_start) = content[start..].find('"') {
            if let Some(id_end) = content[start + id_start + 1..].find('"') {
                return Some(
                    content[start + id_start + 1..start + id_start + 1 + id_end].to_string(),
                );
            }
        }
    }
    None
}

/// Validate that tool messages reference valid tool calls from previous assistant messages
/// Returns Ok(()) if valid, or Err with detailed error message if invalid
pub fn validate_tool_sequence(messages: &[Message]) -> Result<(), String> {
    let tool_message_count = messages.iter().filter(|m| m.role == "tool").count();

    if tool_message_count == 0 {
        // No tool messages, validation passes
        return Ok(());
    }

    debug!("🔍 Validating {} tool messages", tool_message_count);

    // Find all assistant messages with tool_calls
    let mut valid_tool_call_ids = std::collections::HashSet::new();
    for msg in messages.iter() {
        if msg.role == "assistant" {
            if let Some(tool_calls) = &msg.tool_calls {
                for tool_call in tool_calls {
                    valid_tool_call_ids.insert(tool_call.id.clone());
                    debug!("✅ Found valid tool_call_id: {}", tool_call.id);
                }
            }
        }
    }

    if valid_tool_call_ids.is_empty() {
        let error_msg = format!(
            "Tool messages present ({} found) but no assistant messages with tool_calls found in conversation",
            tool_message_count
        );
        error!("❌ {}", error_msg);
        return Err(error_msg);
    }

    // Validate each tool message references a valid tool_call_id
    for tool_msg in messages.iter().filter(|m| m.role == "tool") {
        let tool_call_id = if let Some(id) = &tool_msg.tool_call_id {
            id.clone()
        } else {
            // Try to extract from content
            let content_text = get_text_from_openai_content(&tool_msg.content);
            if let Some(id) = extract_tool_call_id(&content_text) {
                id
            } else {
                let error_msg = format!(
                    "Tool message missing tool_call_id field and cannot extract from content: {:?}",
                    content_text.chars().take(100).collect::<String>()
                );
                error!("❌ {}", error_msg);
                return Err(error_msg);
            }
        };

        if !valid_tool_call_ids.contains(&tool_call_id) {
            let error_msg = format!(
                "Tool message references unknown tool_call_id: {} | Valid IDs: {:?}",
                tool_call_id, valid_tool_call_ids
            );
            error!("❌ {}", error_msg);
            return Err(error_msg);
        }

        debug!("✅ Tool message validated: tool_call_id={}", tool_call_id);
    }

    debug!(
        "✅ All {} tool messages validated successfully",
        tool_message_count
    );
    Ok(())
}
/// Safely truncate a string by byte length to a valid Unicode boundary
pub fn truncate_str_by_bytes(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }

    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }

    let truncated = format!("{}… [truncated {} bytes]", &s[..end], s.len() - end);
    (truncated, true)
}

/// Redact sensitive headers from a HeaderMap
pub fn redact_headers(headers: &salvo::http::HeaderMap) -> HashMap<String, String> {
    let mut redacted = HashMap::new();

    for (key, value) in headers.iter() {
        let name = key.as_str();
        let value_str = value.to_str().unwrap_or("<invalid>");

        let redacted_value = match name.to_ascii_lowercase().as_str() {
            "authorization" | "cookie" | "set-cookie" => "<redacted>".to_string(),
            _ => value_str.to_string(),
        };

        redacted.insert(name.to_string(), redacted_value);
    }

    redacted
}

#[cfg(test)]
mod tests {
    use super::*;
    use poe_api_process::types::{ChatTool, FunctionDefinition};

    #[test]
    fn filter_tools_preserves_entries_without_description() {
        let tool = ChatTool {
            r#type: "function".to_string(),
            function: FunctionDefinition {
                name: "search".to_string(),
                description: None,
                parameters: None,
                returns: None,
                strict: None,
                extra: HashMap::new(),
            },
            extra: HashMap::new(),
        };

        let filtered = filter_tools_for_poe(&Some(vec![tool.clone()])).expect("tool should remain");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].function.name, "search");
    }

    #[test]
    fn filter_tools_drops_entries_without_name() {
        let mut tool = ChatTool::default();
        tool.function.name = String::new();

        assert!(filter_tools_for_poe(&Some(vec![tool])).is_none());
    }
}

/// Redact sensitive JSON fields (token, password, *cookie* - case insensitive)
pub fn redact_json_fields(value: &Value) -> Value {
    match value {
        Value::String(s) => {
            // Check if this string contains sensitive data patterns
            if s.len() > 100 && (s.starts_with("eyJ") || s.starts_with("Bearer ") || s.len() > 500)
            {
                Value::String("<redacted>".to_string())
            } else {
                Value::String(s.clone())
            }
        }
        Value::Object(obj) => {
            let mut redacted_obj = serde_json::Map::new();
            for (k, v) in obj {
                let key_lower = k.to_lowercase();
                let should_redact = key_lower.contains("token")
                    || key_lower.contains("password")
                    || key_lower.contains("cookie")
                    || key_lower == "authorization";

                if should_redact {
                    redacted_obj.insert(k.clone(), Value::String("<redacted>".to_string()));
                } else {
                    redacted_obj.insert(k.clone(), redact_json_fields(v));
                }
            }
            Value::Object(redacted_obj)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(redact_json_fields).collect()),
        _ => value.clone(),
    }
}

/// Create a pretty JSON string with truncation
pub fn pretty_json_truncated(value: &Value, max_bytes: usize) -> String {
    let pretty =
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "Failed to serialize".to_string());

    if pretty.len() <= max_bytes {
        pretty
    } else {
        let (truncated, _) = truncate_str_by_bytes(&pretty, max_bytes);
        truncated
    }
}
