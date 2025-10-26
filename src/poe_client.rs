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
use tracing::{debug, error, info, warn};

pub struct PoeClientWrapper {
    pub client: PoeClient, // Modify to public for external access
    _model: String,
}

impl PoeClientWrapper {
    pub fn new(model: &str, access_key: &str) -> Self {
        info!("🔑 Initializing POE client | Model: {}", model);

        // Get POE API configuration from environment variables, using defaults
        let poe_base_url =
            std::env::var("POE_BASE_URL").unwrap_or_else(|_| "https://api.poe.com".to_string());
        let poe_file_upload_url = std::env::var("POE_FILE_UPLOAD_URL").unwrap_or_else(|_| {
            "https://www.quora.com/poe_api/file_upload_3RD_PARTY_POST".to_string()
        });

        debug!(
            "🔧 POE Configuration | Base URL: {} | Upload URL: {}",
            poe_base_url, poe_file_upload_url
        );

        Self {
            client: PoeClient::new(model, access_key, &poe_base_url, &poe_file_upload_url),
            _model: model.to_string(),
        }
    }

    /// Get model list for v1/models API
    pub async fn get_v1_model_list(
        &self,
    ) -> Result<poe_api_process::ModelResponse, poe_api_process::PoeError> {
        let start_time = std::time::Instant::now();
        debug!("📋 Sending v1/models API request");

        let result = self.client.get_v1_model_list().await;

        match &result {
            Ok(model_response) => {
                let duration = start_time.elapsed();
                info!(
                    "✅ v1/models API request successful | Model count: {} | Duration: {}",
                    model_response.data.len(),
                    crate::utils::format_duration(duration)
                );
            }
            Err(e) => {
                let duration = start_time.elapsed();
                error!(
                    "❌ v1/models API request failed | Error: {} | Duration: {}",
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
            "📤 Sending streaming request | Message count: {} | Temperature setting: {:?}",
            chat_request.query.len(),
            chat_request.temperature
        );
        let result = self.client.stream_request(chat_request).await;
        match &result {
            Ok(_) => {
                let duration = start_time.elapsed();
                info!(
                    "✅ Streaming request established successfully | Duration: {}",
                    crate::utils::format_duration(duration)
                );
            }
            Err(e) => {
                let duration = start_time.elapsed();
                error!(
                    "❌ Streaming request failed | Error: {} | Duration: {}",
                    e,
                    crate::utils::format_duration(duration)
                );
            }
        }
        result
    }
}

// Convert OpenAI message format to Poe message format
fn openai_message_to_poe(
    msg: &Message,
    role_override: Option<String>,
    chat_completion_request: Option<&ChatCompletionRequest>,
) -> ChatMessage {
    let mut attachments: Vec<Attachment> = vec![];
    let mut texts: Vec<String> = vec![];

    // Process content field
    if let Some(content) = &msg.content {
        match content {
            OpenAiContent::Text(s) => {
                texts.push(s.clone());
            }
            OpenAiContent::Multi(arr) => {
                for item in arr {
                    match item {
                        OpenAiContentItem::Text { text, .. } => texts.push(text.clone()),
                        OpenAiContentItem::ImageUrl { image_url, .. } => {
                            debug!("🖼️  Processing image URL: {}", image_url.url);
                            attachments.push(Attachment {
                                url: image_url.url.clone(),
                                content_type: None,
                            });
                        }
                        OpenAiContentItem::ToolResult { .. } => {
                            debug!("🧰 Skipping tool_result content in message conversion");
                        }
                        OpenAiContentItem::InputAudio { .. } => {
                            debug!("🎧 Skipping input_audio content in message conversion");
                        }
                        OpenAiContentItem::Other(value) => {
                            debug!("🔍 Unhandled content block: {}", value);
                        }
                    }
                }
            }
        }
    }

    // Process tool_calls (if exists)
    if let Some(tool_calls) = &msg.tool_calls {
        debug!(
            "🔧 Processing tool_calls in assistant message, count: {}",
            tool_calls.len()
        );
        // Convert tool_calls to text format and add to content
        for tool_call in tool_calls {
            let tool_call_text = format!(
                "Tool Call: {} ({})\nArguments: {}",
                tool_call.function.name, tool_call.id, tool_call.function.arguments
            );
            texts.push(tool_call_text);
        }
    }

    // Process tool_call_id
    if let Some(tool_call_id) = &msg.tool_call_id {
        debug!(
            "🔧 Processing tool_call_id in tool message: {}",
            tool_call_id
        );
        // Add tool_call_id to the beginning of content
        let tool_id_text = format!("Tool Call ID: {}", tool_call_id);
        texts.insert(0, tool_id_text);
    }

    let mut content = texts.join("\n");

    // If user message and is the last message, apply suffix processing
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
            debug!("📎 Adding {} attachments to message", attachments.len());
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
        "📝 Creating chat request | Model: {} | Message count: {} | Temperature setting: {:?} | Original tool count: {:?} | Filtered tool count: {:?}",
        model,
        messages.len(),
        temperature,
        original_tools.as_ref().map(|t| t.len()),
        tools.as_ref().map(|t| t.len())
    );
    // Get models.yaml configuration from cache
    let config: Arc<Config> = get_cached_config().await;
    // Check if model needs replace_response processing
    let should_replace_response = if let Some(model_config) = config.models.get(model) {
        // Use cached config
        model_config.replace_response.unwrap_or(false)
    } else {
        false
    };
    debug!(
        "🔍 Model {} replace_response setting: {}",
        model, should_replace_response
    );

    // Process tool results messages BEFORE consuming messages
    let mut tool_results = None;
    let mut assistant_tool_calls: Option<Vec<poe_api_process::types::ChatToolCall>> = None;

    // Check if there are tool role messages, and convert them to ToolResult
    let tool_message_count = messages.iter().filter(|msg| msg.role == "tool").count();
    let message_count_for_validation = messages.len(); // Store for later validation

    if tool_message_count > 0 {
        debug!(
            "🔍 Found {} tool messages, building tool_call_id mapping",
            tool_message_count
        );

        // First build mapping from tool_call_id to tool name
        let mut tool_call_id_to_name: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut accumulated_tool_calls: Vec<poe_api_process::types::ChatToolCall> = Vec::new();
        let mut seen_tool_call_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        // Extract tool call info from previous assistant messages
        // CRITICAL: Also store the tool_calls to send to Poe
        for msg in &messages {
            if msg.role == "assistant" {
                if let Some(tool_calls) = &msg.tool_calls {
                    debug!(
                        "🔧 Found assistant message with {} tool_calls",
                        tool_calls.len()
                    );

                    // Build mapping for tool_results name lookup
                    for tool_call in tool_calls {
                        tool_call_id_to_name
                            .insert(tool_call.id.clone(), tool_call.function.name.clone());
                        debug!(
                            "🔧 Mapping tool call | ID: {} | Name: {}",
                            tool_call.id, tool_call.function.name
                        );
                        if seen_tool_call_ids.insert(tool_call.id.clone()) {
                            accumulated_tool_calls.push(tool_call.clone());
                        } else {
                            debug!(
                                "⚠️ Duplicate tool_call ID detected in conversation, skipping duplicate entry | ID: {}",
                                tool_call.id
                            );
                        }
                    }
                }
            }
        }

        if !accumulated_tool_calls.is_empty() {
            assistant_tool_calls = Some(accumulated_tool_calls);
        }

        // Log what we extracted
        if let Some(ref calls) = assistant_tool_calls {
            debug!(
                "✅ Extracted {} tool_calls from assistant messages to send to Poe",
                calls.len()
            );
            for tc in calls {
                debug!("   📞 Tool call: {} (ID: {})", tc.function.name, tc.id);
            }
        } else {
            error!("❌ No tool_calls found in assistant messages but tool messages exist!");
            error!("   This will cause 'No tool call found' error from Poe!");
        }

        debug!(
            "🔍 Tool call mapping built: {} entries | Tool messages to process: {}",
            tool_call_id_to_name.len(),
            tool_message_count
        );

        let mut results = Vec::new();
        for msg in &messages {
            if msg.role == "tool" {
                // Prioritize using new tool_call_id field
                let tool_call_id = if let Some(id) = &msg.tool_call_id {
                    debug!("✅ Tool call ID from field: {}", id);
                    id.clone()
                } else {
                    // If no tool_call_id field, try to extract from content
                    let content_text = get_text_from_openai_content(&msg.content);
                    if let Some(id) = extract_tool_call_id(&content_text) {
                        debug!("⚠️ Tool call ID extracted from content: {}", id);
                        id
                    } else {
                        error!(
                            "❌ Cannot extract tool_call_id from tool message | Content: {:?}",
                            content_text
                        );
                        continue;
                    }
                };

                // Find tool name from mapping, if not found use "unknown"
                let tool_name = tool_call_id_to_name.get(&tool_call_id)
                    .cloned()
                    .unwrap_or_else(|| {
                        error!(
                            "❌ Unable to find tool name for tool_call_id: {} | Available IDs: {:?}",
                            tool_call_id,
                            tool_call_id_to_name.keys().collect::<Vec<_>>()
                        );
                        "unknown".to_string()
                    });

                let content_text = get_text_from_openai_content(&msg.content);
                debug!(
                    "🔧 Processing tool result | tool_call_id: {} | Tool name: {} | Content length: {}",
                    tool_call_id,
                    tool_name,
                    content_text.len()
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
            tool_results = Some(results.clone());
            debug!("✅ Created {} tool results for Poe API", results.len());
            for result in &results {
                debug!(
                    "   📋 Tool result | ID: {} | Name: {} | Content preview: {}",
                    result.tool_call_id,
                    result.name,
                    if result.content.len() > 100 {
                        format!("{}...", &result.content[..100])
                    } else {
                        result.content.clone()
                    }
                );
            }
        } else {
            warn!(
                "⚠️ No valid tool results created despite {} tool messages",
                tool_message_count
            );
        }
    }

    // CRITICAL VALIDATION: If we have tool_results, we MUST have tool_calls
    if tool_results.is_some() && assistant_tool_calls.is_none() {
        error!("❌ CRITICAL: tool_results present but no assistant tool_calls found!");
        error!("   This will cause 'No tool call found' error from Poe");
        error!(
            "   Total messages in conversation: {}",
            message_count_for_validation
        );
    }

    // Validate: All tool_result IDs must exist in tool_calls
    if let (Some(calls), Some(results)) = (&assistant_tool_calls, &tool_results) {
        let call_ids: std::collections::HashSet<String> =
            calls.iter().map(|c| c.id.clone()).collect();
        info!(
            "🔍 Validation | Tool calls count: {} | Tool results count: {}",
            calls.len(),
            results.len()
        );
        for call in calls {
            debug!(
                "   ✅ Tool call available | ID: {} | Name: {}",
                call.id, call.function.name
            );
        }
        for result in results {
            if call_ids.contains(&result.tool_call_id) {
                debug!(
                    "   ✅ Tool result matches | ID: {} | Name: {}",
                    result.tool_call_id, result.name
                );
            } else {
                let available_ids: Vec<&str> = call_ids.iter().map(|id| id.as_str()).collect();
                error!(
                    "❌ CRITICAL MISMATCH: tool_result '{}' references ID '{}' which is NOT in tool_calls!",
                    result.name, result.tool_call_id
                );
                error!("   Available tool_call IDs: {:?}", available_ids);
                error!("   This WILL cause 'No tool call found' error from Poe!");
            }
        }
    }

    let query = messages
        .iter()
        .enumerate()
        .map(|(index, msg)| {
            let original_role = &msg.role;
            let role_override = match original_role.as_str() {
                // Always convert assistant to bot
                "assistant" => Some("bot".to_string()),
                // Always convert developer to user
                "developer" => Some("user".to_string()),
                // Always convert tool to user
                "tool" => Some("user".to_string()),
                // Only convert system to user when replace_response is true
                "system" if should_replace_response => Some("user".to_string()),
                // Keep others as is
                _ => None,
            };
            // Convert OpenAI message to Poe message
            // Apply suffix processing only to the last user message
            let is_last_user_message = msg.role == "user" && index == messages.len() - 1;
            let request_param = if is_last_user_message {
                Some(chat_completion_request)
            } else {
                None
            };
            let poe_message = openai_message_to_poe(msg, role_override, request_param);
            // Log conversion result
            debug!(
                "🔄 Processing message | Original role: {} | Converted role: {} | Content length: {} | Attachment count: {}",
                original_role,
                poe_message.role,
                crate::utils::format_bytes_length(poe_message.content.len()),
                poe_message.attachments.as_ref().map_or(0, |a| a.len())
            );
            poe_message
        })
        .collect();

    // Note: tool_results and assistant_tool_calls were already extracted above
    // before processing messages, so they're ready to use

    ChatRequest {
        version: "1.1".to_string(),
        r#type: "query".to_string(),
        query,
        temperature,
        user_id: "".to_string(),
        conversation_id: "".to_string(),
        message_id: "".to_string(),
        tools,
        tool_calls: assistant_tool_calls,
        tool_results,
        logit_bias,
        stop_sequences: stop,
    }
}
