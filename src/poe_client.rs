use crate::{types::*, utils::get_cached_config, utils::get_text_from_openai_content};
use futures_util::Stream;
use poe_api_process::types::Attachment;
use poe_api_process::{ChatMessage, ChatRequest, ChatResponse, PoeClient, PoeError};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info};

pub struct PoeClientWrapper {
    pub client: PoeClient, // 修改為公開，以便外部訪問
    _model: String,
}

impl PoeClientWrapper {
    pub fn new(model: &str, access_key: &str) -> Self {
        info!("🔑 初始化 POE 客戶端 | 模型: {}", model);
        Self {
            client: PoeClient::new(model, access_key),
            _model: model.to_string(),
        }
    }
    pub async fn stream_request(
        &self,
        chat_request: ChatRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<ChatResponse, PoeError>> + Send>>, PoeError> {
        let start_time = Instant::now();
        debug!(
            "📤 發送串流請求 | 訊息數量: {} | 溫度設置: {:?}",
            chat_request.query.len(),
            chat_request.temperature
        );
        let result = self.client.stream_request(chat_request).await;
        match &result {
            Ok(_) => {
                let duration = start_time.elapsed();
                info!(
                    "✅ 串流請求建立成功 | 耗時: {}",
                    crate::utils::format_duration(duration)
                );
            }
            Err(e) => {
                let duration = start_time.elapsed();
                error!(
                    "❌ 串流請求失敗 | 錯誤: {} | 耗時: {}",
                    e,
                    crate::utils::format_duration(duration)
                );
            }
        }
        result
    }
}

// OpenAI 消息格式轉換為 Poe 消息格式的函數
fn openai_message_to_poe(msg: &Message, role_override: Option<String>) -> ChatMessage {
    let mut attachments: Vec<Attachment> = vec![];
    let mut texts: Vec<String> = vec![];

    match &msg.content {
        OpenAiContent::Text(s) => {
            texts.push(s.clone());
        }
        OpenAiContent::Multi(arr) => {
            for item in arr {
                match item {
                    OpenAiContentItem::Text { text } => texts.push(text.clone()),
                    OpenAiContentItem::ImageUrl { image_url } => {
                        debug!("🖼️ 處理圖片 URL: {}", image_url.url);
                        attachments.push(Attachment {
                            url: image_url.url.clone(),
                            content_type: None,
                        });
                    }
                }
            }
        }
    }

    let role = role_override.unwrap_or_else(|| msg.role.clone());
    ChatMessage {
        role,
        content: texts.join("\n"),
        attachments: if !attachments.is_empty() {
            debug!("📎 添加 {} 個附件到消息", attachments.len());
            Some(attachments)
        } else {
            None
        },
        content_type: "text/markdown".to_string(),
    }
}

pub async fn create_chat_request(
    model: &str,
    messages: Vec<Message>,
    temperature: Option<f32>,
    tools: Option<Vec<poe_api_process::types::ChatTool>>,
    logit_bias: Option<HashMap<String, f32>>,
    stop: Option<Vec<String>>,
) -> ChatRequest {
    debug!(
        "📝 創建聊天請求 | 模型: {} | 訊息數量: {} | 溫度設置: {:?} | 工具數量: {:?}",
        model,
        messages.len(),
        temperature,
        tools.as_ref().map(|t| t.len())
    );
    // 從緩存獲取 models.yaml 配置
    let config: Arc<Config> = get_cached_config().await;
    // 檢查模型是否需要 replace_response 處理
    let should_replace_response = if let Some(model_config) = config.models.get(model) {
        // 使用快取的 config
        model_config.replace_response.unwrap_or(false)
    } else {
        false
    };
    debug!(
        "🔍 模型 {} 的 replace_response 設置: {}",
        model, should_replace_response
    );
    let query = messages
        .iter()
        .map(|msg| {
            let original_role = &msg.role;
            let role_override = match original_role.as_str() {
                // 總是將 assistant 轉換為 bot
                "assistant" => Some("bot".to_string()),
                // 總是將 developer 轉換為 user
                "developer" => Some("user".to_string()),
                // 只有在 replace_response 為 true 時才轉換 system 為 user
                "system" if should_replace_response => Some("user".to_string()),
                // 其他情況保持原樣
                _ => None,
            };
            // 將 OpenAI 消息轉換為 Poe 消息
            let poe_message = openai_message_to_poe(msg, role_override);
            // 紀錄轉換結果
            debug!(
                "🔄 處理訊息 | 原始角色: {} | 轉換後角色: {} | 內容長度: {} | 附件數量: {}",
                original_role,
                poe_message.role,
                crate::utils::format_bytes_length(poe_message.content.len()),
                poe_message.attachments.as_ref().map_or(0, |a| a.len())
            );
            poe_message
        })
        .collect();
    // 處理工具結果消息
    let mut tool_results = None;
    // 檢查是否有 tool 角色的消息，並將其轉換為 ToolResult
    if messages.iter().any(|msg| msg.role == "tool") {
        let mut results = Vec::new();
        for msg in &messages {
            if msg.role == "tool" {
                // 從內容中提取文字部分
                let content_text = get_text_from_openai_content(&msg.content);
                if let Some(tool_call_id) = extract_tool_call_id(&content_text) {
                    debug!("🔧 處理工具結果 | tool_call_id: {}", tool_call_id);
                    results.push(poe_api_process::types::ChatToolResult {
                        role: "tool".to_string(),
                        tool_call_id,
                        name: "unknown".to_string(),
                        content: content_text,
                    });
                } else {
                    debug!("⚠️ 無法從工具消息中提取 tool_call_id");
                }
            }
        }
        if !results.is_empty() {
            tool_results = Some(results);
            debug!(
                "🔧 創建了 {} 個工具結果",
                tool_results.as_ref().unwrap().len()
            );
        }
    }
    ChatRequest {
        version: "1.1".to_string(),
        r#type: "query".to_string(),
        query,
        temperature,
        user_id: "".to_string(),
        conversation_id: "".to_string(),
        message_id: "".to_string(),
        tools,
        tool_calls: None,
        tool_results,
        logit_bias,
        stop_sequences: stop,
    }
}

// 從工具消息中提取 tool_call_id
fn extract_tool_call_id(content: &str) -> Option<String> {
    // 嘗試解析 JSON 格式的內容
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(content) {
        if let Some(tool_call_id) = json.get("tool_call_id").and_then(|v| v.as_str()) {
            return Some(tool_call_id.to_string());
        }
    }
    // 嘗試使用簡單的文本解析
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
