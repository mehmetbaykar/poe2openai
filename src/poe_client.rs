use crate::{
    cache::get_cached_config,
    types::*,
    utils::{extract_tool_call_id, filter_tools_for_poe, get_text_from_openai_content},
};
use futures_util::Stream;
use poe_api_process::types::Attachment;
use poe_api_process::{ChatMessage, ChatRequest, ChatResponse, PoeClient, PoeError};
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

        // 從環境變數獲取 POE API 配置，使用預設值
        let poe_base_url =
            std::env::var("POE_BASE_URL").unwrap_or_else(|_| "https://api.poe.com".to_string());
        let poe_file_upload_url = std::env::var("POE_FILE_UPLOAD_URL").unwrap_or_else(|_| {
            "https://www.quora.com/poe_api/file_upload_3RD_PARTY_POST".to_string()
        });

        debug!(
            "🔧 POE 配置 | Base URL: {} | Upload URL: {}",
            poe_base_url, poe_file_upload_url
        );

        Self {
            client: PoeClient::new(model, access_key, &poe_base_url, &poe_file_upload_url),
            _model: model.to_string(),
        }
    }

    /// 獲取 v1/models API 的模型列表
    pub async fn get_v1_model_list(
        &self,
    ) -> Result<poe_api_process::ModelResponse, poe_api_process::PoeError> {
        let start_time = std::time::Instant::now();
        debug!("📋 發送 v1/models API 請求");

        let result = self.client.get_v1_model_list().await;

        match &result {
            Ok(model_response) => {
                let duration = start_time.elapsed();
                info!(
                    "✅ v1/models API 請求成功 | 模型數量: {} | 耗時: {}",
                    model_response.data.len(),
                    crate::utils::format_duration(duration)
                );
            }
            Err(e) => {
                let duration = start_time.elapsed();
                error!(
                    "❌ v1/models API 請求失敗 | 錯誤: {} | 耗時: {}",
                    e,
                    crate::utils::format_duration(duration)
                );
            }
        }

        result
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
fn openai_message_to_poe(
    msg: &Message,
    role_override: Option<String>,
    chat_completion_request: Option<&ChatCompletionRequest>,
) -> ChatMessage {
    let mut attachments: Vec<Attachment> = vec![];
    let mut texts: Vec<String> = vec![];

    // 處理 content 欄位
    if let Some(content) = &msg.content {
        match content {
            OpenAiContent::Text(s) => {
                texts.push(s.clone());
            }
            OpenAiContent::Multi(arr) => {
                for item in arr {
                    match item {
                        OpenAiContentItem::Text { text } => texts.push(text.clone()),
                        OpenAiContentItem::ImageUrl { image_url } => {
                            debug!("🖼️  處理圖片 URL: {}", image_url.url);
                            attachments.push(Attachment {
                                url: image_url.url.clone(),
                                content_type: None,
                            });
                        }
                    }
                }
            }
        }
    }

    // 處理 tool_calls（如果存在）
    if let Some(tool_calls) = &msg.tool_calls {
        debug!(
            "🔧 處理 assistant 消息中的 tool_calls，數量: {}",
            tool_calls.len()
        );
        // 將 tool_calls 轉換為文本格式添加到內容中
        for tool_call in tool_calls {
            let tool_call_text = format!(
                "Tool Call: {} ({})\nArguments: {}",
                tool_call.function.name, tool_call.id, tool_call.function.arguments
            );
            texts.push(tool_call_text);
        }
    }

    // 處理 tool_call_id
    if let Some(tool_call_id) = &msg.tool_call_id {
        debug!("🔧 處理 tool 消息中的 tool_call_id: {}", tool_call_id);
        // 將 tool_call_id 添加到內容開頭
        let tool_id_text = format!("Tool Call ID: {}", tool_call_id);
        texts.insert(0, tool_id_text);
    }

    let mut content = texts.join("\n");

    // 如果是用戶消息且是最後一條消息，應用後綴處理
    if msg.role == "user" {
        if let Some(request) = chat_completion_request {
            content = crate::utils::process_message_content_with_suffixes(&content, request);
        }
    }

    let role = role_override.unwrap_or_else(|| msg.role.clone());
    ChatMessage {
        role,
        content,
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
    chat_completion_request: &ChatCompletionRequest,
) -> ChatRequest {
    let temperature = chat_completion_request.temperature;
    let original_tools = chat_completion_request.tools.clone();
    let tools = filter_tools_for_poe(&original_tools);
    let logit_bias = chat_completion_request.logit_bias.clone();
    let stop = chat_completion_request.stop.clone();

    debug!(
        "📝 創建聊天請求 | 模型: {} | 訊息數量: {} | 溫度設置: {:?} | 原始工具數量: {:?} | 過濾後工具數量: {:?}",
        model,
        messages.len(),
        temperature,
        original_tools.as_ref().map(|t| t.len()),
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
        .enumerate()
        .map(|(index, msg)| {
            let original_role = &msg.role;
            let role_override = match original_role.as_str() {
                // 總是將 assistant 轉換為 bot
                "assistant" => Some("bot".to_string()),
                // 總是將 developer 轉換為 user
                "developer" => Some("user".to_string()),
                // 總是將 tool 轉換為 user
                "tool" => Some("user".to_string()),
                // 只有在 replace_response 為 true 時才轉換 system 為 user
                "system" if should_replace_response => Some("user".to_string()),
                // 其他情況保持原樣
                _ => None,
            };
            // 將 OpenAI 消息轉換為 Poe 消息
            // 只對最後一條用戶消息應用後綴處理
            let is_last_user_message = msg.role == "user" && index == messages.len() - 1;
            let request_param = if is_last_user_message {
                Some(chat_completion_request)
            } else {
                None
            };
            let poe_message = openai_message_to_poe(msg, role_override, request_param);
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
        // 首先建立 tool_call_id 到工具名稱的映射
        let mut tool_call_id_to_name: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        // 從之前的 assistant 消息中提取工具調用信息
        for msg in &messages {
            if msg.role == "assistant" {
                if let Some(tool_calls) = &msg.tool_calls {
                    for tool_call in tool_calls {
                        tool_call_id_to_name
                            .insert(tool_call.id.clone(), tool_call.function.name.clone());
                        debug!(
                            "🔧 映射工具調用 | ID: {} | 名稱: {}",
                            tool_call.id, tool_call.function.name
                        );
                    }
                }
            }
        }

        let mut results = Vec::new();
        for msg in messages {
            if msg.role == "tool" {
                // 優先使用新的 tool_call_id 欄位
                let tool_call_id = if let Some(id) = &msg.tool_call_id {
                    id.clone()
                } else {
                    // 如果沒有 tool_call_id 欄位，嘗試從內容中提取
                    let content_text = get_text_from_openai_content(&msg.content);
                    if let Some(id) = extract_tool_call_id(&content_text) {
                        id
                    } else {
                        debug!("⚠️ 無法從工具消息中提取 tool_call_id");
                        continue;
                    }
                };

                // 從映射中查找工具名稱，如果找不到則使用 "unknown"
                let tool_name = tool_call_id_to_name
                    .get(&tool_call_id)
                    .cloned()
                    .unwrap_or_else(|| {
                        debug!(
                            "⚠️ 無法找到 tool_call_id {} 對應的工具名稱，使用 unknown",
                            tool_call_id
                        );
                        "unknown".to_string()
                    });

                let content_text = get_text_from_openai_content(&msg.content);
                debug!(
                    "🔧 處理工具結果 | tool_call_id: {} | 工具名稱: {}",
                    tool_call_id, tool_name
                );
                results.push(poe_api_process::types::ChatToolResult {
                    role: "tool".to_string(),
                    tool_call_id,
                    name: tool_name,
                    content: content_text,
                });
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
