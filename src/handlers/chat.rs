use crate::cache::get_cached_config;
use crate::evert::{EventContext, EventHandlerManager};
use crate::poe_client::{PoeClientWrapper, create_chat_request};
use crate::types::*;
use crate::utils::{
    convert_poe_error_to_openai, count_completion_tokens, count_message_tokens,
    format_bytes_length, format_duration, process_message_images, redact_headers,
    redact_json_fields, pretty_json_truncated,
};
use chrono::Utc;
use futures_util::future::{self};
use futures_util::stream::{self, Stream, StreamExt};
use nanoid::nanoid;
use poe_api_process::ChatResponseData;
use poe_api_process::{ChatEventType, ChatResponse, PoeError};
use salvo::http::header;
use salvo::prelude::*;
use serde_json::json;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::{debug, error, info, warn};

#[handler]
pub async fn chat_completions(req: &mut Request, res: &mut Response) {
    let start_time = Instant::now();
    
    // Structure request/response logging with separator
    debug!("------ Incoming Request [POST] {} ------", req.uri());
    
    // Log inbound request metadata with redacted headers
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let redacted_headers = redact_headers(req.headers());
    
    debug!("📝 Request metadata | Method: {} | Path: {} | Query: {} | Headers: {:?}", 
        method, path, query, redacted_headers);

    let max_size: usize = std::env::var("MAX_REQUEST_SIZE")
        .unwrap_or_else(|_| "1073741824".to_string())
        .parse()
        .unwrap_or(1024 * 1024 * 1024);

    // Get cached models.yaml configuration
    let config = get_cached_config().await;
    debug!("🔧 Retrieved config from cache | Enable status: {:?}", config.enable);

    // Validate authorization
    let access_key = match req.headers().get("Authorization") {
        Some(auth) => {
            let auth_str = auth.to_str().unwrap_or("");
            if let Some(stripped) = auth_str.strip_prefix("Bearer ") {
                debug!("🔑 Validated token length: {}", stripped.len());
                stripped.to_string()
            } else {
                error!("❌ Invalid authorization format");
                res.status_code(StatusCode::UNAUTHORIZED);
                res.render(Json(json!({ "error": "Invalid Authorization" })));
                return;
            }
        }
        None => {
            error!("❌ Missing authorization header");
            res.status_code(StatusCode::UNAUTHORIZED);
            res.render(Json(json!({ "error": "Missing Authorization" })));
            return;
        }
    };

    // Parse request body
    let chat_request = match req.payload_with_max_size(max_size).await {
        Ok(bytes) => match serde_json::from_slice::<ChatCompletionRequest>(bytes) {
            Ok(req) => {
                debug!(
                    "📊 Request parsed successfully | Model: {} | Messages: {} | Stream: {:?}",
                    req.model,
                    req.messages.len(),
                    req.stream
                );
                
                // Log sanitized and truncated request body
                let request_value = serde_json::to_value(&req).unwrap_or_else(|_| json!(null));
                let redacted_request = redact_json_fields(&request_value);
                let pretty_request = pretty_json_truncated(&redacted_request, 64 * 1024);
                debug!("📋 Request body (sanitized, truncated):\n{}", pretty_request);
                
                req
            }
            Err(e) => {
                error!("❌ JSON parsing failed: {}", e);
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(OpenAIErrorResponse {
                    error: OpenAIError {
                        message: format!("JSON parsing failed: {}", e),
                        r#type: "invalid_request_error".to_string(),
                        code: "parse_error".to_string(),
                        param: None,
                    },
                }));
                return;
            }
        },
        Err(e) => {
            error!("❌ Request size exceeded limit or read failed: {}", e);
            res.status_code(StatusCode::PAYLOAD_TOO_LARGE);
            res.render(Json(OpenAIErrorResponse {
                error: OpenAIError {
                    message: format!("Request size exceeded limit ({} bytes) or read failed: {}", max_size, e),
                    r#type: "invalid_request_error".to_string(),
                    code: "payload_too_large".to_string(),
                    param: None,
                },
            }));
            return;
        }
    };

    // Find mapped original model name
    let (display_model, original_model) = if config.enable.unwrap_or(false) {
        let requested_model = chat_request.model.clone();
        // Check if current request model is a mapping target
        let mapping_entry = config.models.iter().find(|(_, cfg)| {
            if let Some(mapping) = &cfg.mapping {
                mapping.to_lowercase() == requested_model.to_lowercase()
            } else {
                false
            }
        });
        if let Some((original_name, _)) = mapping_entry {
            // If mapping found, use original model name
            debug!("🔄 Reverse model mapping: {} -> {}", requested_model, original_name);
            (requested_model, original_name.clone())
        } else {
            // If no mapping found, check for direct mapping configuration
            if let Some(model_config) = config.models.get(&requested_model) {
                if let Some(mapped_name) = &model_config.mapping {
                    debug!("🔄 Direct model mapping: {} -> {}", requested_model, mapped_name);
                    (requested_model.clone(), requested_model)
                } else {
                    // No mapping configuration, use original name
                    (requested_model.clone(), requested_model)
                }
            } else {
                // No related configuration at all, use original name
                (requested_model.clone(), requested_model)
            }
        }
    } else {
        // Configuration not enabled, use original name directly
        (chat_request.model.clone(), chat_request.model.clone())
    };
    info!("🤖 Using model: {} (original: {})", display_model, original_model);

    // Create client
    let client = PoeClientWrapper::new(&original_model, &access_key);

    // Process image_url in messages
    let mut messages = chat_request.messages.clone();
    if let Err(e) = process_message_images(&client, &mut messages).await {
        error!("❌ File upload processing failed: {}", e);
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Json(OpenAIErrorResponse {
            error: OpenAIError {
                message: format!("File upload processing failed: {}", e),
                r#type: "processing_error".to_string(),
                code: "file_processing_failed".to_string(),
                param: None,
            },
        }));
        return;
    }

    // Calculate prompt_tokens
    let prompt_tokens = count_message_tokens(&messages);
    debug!("📊 Calculated prompt_tokens: {}", prompt_tokens);

    let stream = chat_request.stream.unwrap_or(false);
    debug!("🔄 Request mode: {}", if stream { "streaming" } else { "non-streaming" });

    // Create chat request
    let chat_request_obj = create_chat_request(&original_model, messages, &chat_request).await;

    // Check if usage statistics should be included
    let include_usage = chat_request
        .stream_options
        .as_ref()
        .and_then(|opts| opts.include_usage)
        .unwrap_or(false);
    debug!("📊 Include usage statistics: {}", include_usage);

    // Create output generator
    let output_generator =
        OutputGenerator::new(display_model.clone(), prompt_tokens, include_usage);

    match client.stream_request(chat_request_obj).await {
        Ok(mut event_stream) => {
            let first_event = event_stream.next().await;

            if let Some(Ok(ChatResponse {
                event: ChatEventType::Error,
                data: Some(ChatResponseData::Error { text, allow_retry }),
            })) = &first_event
            {
                let insufficient_points_msg_1 =
                    "This bot needs more points to answer your request.";
                let insufficient_points_msg_2 =
                    "You do not have enough points to message this bot.";

                if text.contains(insufficient_points_msg_1)
                    || text.contains(insufficient_points_msg_2)
                {
                    info!("🚫 Detected Poe points insufficient error, returning 429 status code.");
                    let status = StatusCode::TOO_MANY_REQUESTS;
                    let body = OpenAIErrorResponse {
                        error: OpenAIError {
                            message: "You have exceeded your message quota for this model. Please try again later.".to_string(),
                            r#type: "insufficient_quota".to_string(),
                            code: "insufficient_quota".to_string(),
                            param: None,
                        },
                    };
                    res.status_code(status);
                    res.render(Json(body));
                    return;
                } else {
                    let (status, body) = convert_poe_error_to_openai(text, *allow_retry);
                    res.status_code(status);
                    res.render(Json(body));
                    return;
                }
            }

            let reconstituted_stream: Pin<
                Box<dyn Stream<Item = Result<ChatResponse, PoeError>> + Send>,
            > = if let Some(first) = first_event {
                Box::pin(stream::once(async { first }).chain(event_stream))
            } else {
                Box::pin(stream::empty())
            };

            if stream {
                handle_stream_response(res, reconstituted_stream, output_generator).await;
            } else {
                handle_non_stream_response(res, reconstituted_stream, output_generator).await;
            }
        }
        Err(e) => {
            error!("❌ Failed to create streaming request: {}", e);
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(json!({ "error": e.to_string() })));
        }
    }

    let duration = start_time.elapsed();
    info!("✅ Request processing completed | Duration: {}", format_duration(duration));
}

// Handle streaming response
async fn handle_stream_response(
    res: &mut Response,
    event_stream: Pin<Box<dyn Stream<Item = Result<ChatResponse, PoeError>> + Send>>,
    output_generator: OutputGenerator,
) {
    let start_time = Instant::now();
    let id = output_generator.id.clone();
    let model = output_generator.model.clone();
    let include_usage = output_generator.include_usage;
    info!(
        "🌊 Starting streaming response handling | ID: {} | Model: {} | Include usage: {}",
        id, model, include_usage
    );

    // Set streaming response headers
    res.headers_mut()
        .insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
    res.headers_mut()
        .insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    res.headers_mut()
        .insert(header::CONNECTION, "keep-alive".parse().unwrap());

    // Process event stream and generate output
    let processed_stream = output_generator
        .process_stream(Box::pin(event_stream))
        .await;
    res.stream(processed_stream);

    let duration = start_time.elapsed();
    info!(
        "✅ Streaming response processing completed | ID: {} | Duration: {}",
        id,
        format_duration(duration)
    );
}

// Handle non-streaming response
async fn handle_non_stream_response(
    res: &mut Response,
    mut event_stream: Pin<Box<dyn Stream<Item = Result<ChatResponse, PoeError>> + Send>>,
    output_generator: OutputGenerator,
) {
    let start_time = Instant::now();
    let id = output_generator.id.clone();
    let model = output_generator.model.clone();
    let include_usage = output_generator.include_usage;
    info!(
        "📦 Starting non-streaming response handling | ID: {} | Model: {} | Include usage: {}",
        id, model, include_usage
    );

    let handler_manager = EventHandlerManager::new();
    let mut ctx = EventContext::default();

    // Process all events
    while let Some(result) = event_stream.next().await {
        match result {
            Ok(event) => {
                handler_manager.handle(&event, &mut ctx);
                // Check for errors
                if let Some((status, error_response)) = &ctx.error {
                    error!("❌ Processing error: {:?}", error_response);
                    res.status_code(*status);
                    res.render(Json(error_response));
                    return;
                }
                // Check if completed
                if ctx.done {
                    debug!("✅ Received completion event");
                    break;
                }
            }
            Err(e) => {
                error!("❌ Processing error: {}", e);
                let (status, error_response) = convert_poe_error_to_openai(&e.to_string(), false);
                res.status_code(status);
                res.render(Json(error_response));
                return;
            }
        }
    }

    // Create final response
    let response = output_generator.create_final_response(&mut ctx);
    
    // Log the response before rendering
    let response_value = serde_json::to_value(&response).unwrap_or_else(|_| json!(null));
    let redacted_response = redact_json_fields(&response_value);
    let pretty_response = pretty_json_truncated(&redacted_response, 64 * 1024);
    debug!("📤 Response body (sanitized, truncated):\n{}", pretty_response);
    
    debug!("------ Outgoing Response [200] /v1/chat/completions ------");
    
    res.render(Json(response));

    let duration = start_time.elapsed();
    info!(
        "✅ Non-streaming response processing completed | ID: {} | Duration: {}",
        id,
        format_duration(duration)
    );
}

// Output generator - for converting EventContext to final output
#[derive(Clone)]
struct OutputGenerator {
    id: String,
    created: i64,
    model: String,
    prompt_tokens: u32,
    include_usage: bool,
}

impl OutputGenerator {
    fn new(model: String, prompt_tokens: u32, include_usage: bool) -> Self {
        Self {
            id: nanoid!(10),
            created: Utc::now().timestamp(),
            model,
            prompt_tokens,
            include_usage,
        }
    }

    // Process file references, replacing [ref_id] with (url)
    fn process_file_references(
        &self,
        content: &str,
        file_refs: &HashMap<String, poe_api_process::types::FileData>,
    ) -> String {
        if file_refs.is_empty() {
            return content.to_string();
        }
        let mut processed = content.to_string();
        let mut has_replaced = false;

        for (ref_id, file_data) in file_refs {
            let img_marker = format!("[{}]", ref_id);
            if processed.contains(&img_marker) {
                let replacement = format!("({})", file_data.url);
                processed = processed.replace(&img_marker, &replacement);
                debug!("🖼️ Replaced image reference | ID: {} | URL: {}", ref_id, file_data.url);
                has_replaced = true;
            }
        }

        if has_replaced {
            debug!("✅ Successfully replaced image references");
        } else if processed.contains('[') && processed.contains(']') {
            warn!(
                "⚠️ Text contains potential image reference format, but no corresponding reference found: {}",
                processed
            );
        }

        processed
    }

    // Calculate token usage
    fn calculate_tokens(&self, ctx: &mut EventContext) -> (u32, u32, u32) {
        let content = match &ctx.replace_buffer {
            Some(replace_content) => replace_content,
            None => &ctx.content,
        };
        let completion_tokens = count_completion_tokens(content);
        ctx.completion_tokens = completion_tokens;
        let total_tokens = self.prompt_tokens + completion_tokens;
        (self.prompt_tokens, completion_tokens, total_tokens)
    }

    // Create role chunk
    // Create role chunk
    fn create_role_chunk(&self) -> ChatCompletionChunk {
        let role_delta = Delta {
            role: Some("assistant".to_string()),
            content: None,
            refusal: None,
            tool_calls: None,
            reasoning_content: None,
        };
        ChatCompletionChunk {
            id: format!("chatcmpl-{}", self.id),
            object: "chat.completion.chunk".to_string(),
            created: self.created,
            model: self.model.clone(),
            choices: vec![Choice {
                index: 0,
                delta: role_delta,
                finish_reason: None,
            }],
        }
    }

    // Thinking chunk
    fn create_reasoning_chunk(&self, reasoning_content: &str) -> ChatCompletionChunk {
        let reasoning_delta = Delta {
            role: None,
            content: None,
            refusal: None,
            tool_calls: None,
            reasoning_content: Some(reasoning_content.to_string()),
        };
        ChatCompletionChunk {
            id: format!("chatcmpl-{}", self.id),
            object: "chat.completion.chunk".to_string(),
            created: self.created,
            model: self.model.clone(),
            choices: vec![Choice {
                index: 0,
                delta: reasoning_delta,
                finish_reason: None,
            }],
        }
    }
    // Create stream chunk
    fn create_stream_chunk(
        &self,
        content: &str,
        finish_reason: Option<String>,
    ) -> ChatCompletionChunk {
        let mut delta = Delta {
            role: None,
            content: None,
            refusal: None,
            tool_calls: None,
            reasoning_content: None,
        };
        delta.content = Some(content.to_string());
        debug!(
            "🔧 Creating stream chunk | ID: {} | Content length: {}",
            self.id,
            format_bytes_length(content.len())
        );
        ChatCompletionChunk {
            id: format!("chatcmpl-{}", self.id),
            object: "chat.completion.chunk".to_string(),
            created: self.created,
            model: self.model.clone(),
            choices: vec![Choice {
                index: 0,
                delta,
                finish_reason,
            }],
        }
    }

    // Create tool call chunk
    fn create_tool_calls_chunk(
        &self,
        tool_calls: &[poe_api_process::types::ChatToolCall],
    ) -> ChatCompletionChunk {
        let tool_delta = Delta {
            role: None,
            content: None,
            refusal: None,
            tool_calls: Some(tool_calls.to_vec()),
            reasoning_content: None,
        };
        ChatCompletionChunk {
            id: format!("chatcmpl-{}", self.id),
            object: "chat.completion.chunk".to_string(),
            created: self.created,
            model: self.model.clone(),
            choices: vec![Choice {
                index: 0,
                delta: tool_delta,
                finish_reason: Some("tool_calls".to_string()),
            }],
        }
    }

    // Create final full response (non-streaming mode)
    fn create_final_response(&self, ctx: &mut EventContext) -> ChatCompletionResponse {
        // Process remaining pending_text
        if !ctx.pending_text.trim().is_empty() {
            use crate::evert::ThinkingProcessor;
            let (reasoning_output, content_output) = ThinkingProcessor::process_text_chunk(ctx, "");
            if let Some(final_reasoning) = reasoning_output {
                ctx.reasoning_content.push_str(&final_reasoning);
            }
            if let Some(final_content) = content_output {
                ctx.content.push_str(&final_content);
            }
        }

        // Process content, including file reference replacement
        let content = if let Some(replace_content) = &ctx.replace_buffer {
            self.process_file_references(replace_content, &ctx.file_refs)
        } else {
            self.process_file_references(&ctx.content, &ctx.file_refs)
        };

        // Calculate tokens
        let (prompt_tokens, completion_tokens, total_tokens) = self.calculate_tokens(ctx);

        // Determine finish_reason
        let finish_reason = if !ctx.tool_calls.is_empty() {
            "tool_calls".to_string()
        } else {
            "stop".to_string()
        };

        debug!(
            "📤 Preparing response | Content length: {} | Reasoning length: {} | Tool calls count: {} | Finish reason: {}",
            format_bytes_length(content.len()),
            format_bytes_length(ctx.reasoning_content.len()),
            ctx.tool_calls.len(),
            finish_reason
        );

        // Create response
        let mut response = ChatCompletionResponse {
            id: format!("chatcmpl-{}", self.id),
            object: "chat.completion".to_string(),
            created: self.created,
            model: self.model.clone(),
            choices: vec![CompletionChoice {
                index: 0,
                message: CompletionMessage {
                    role: "assistant".to_string(),
                    content,
                    refusal: None,
                    tool_calls: if ctx.tool_calls.is_empty() {
                        None
                    } else {
                        Some(ctx.tool_calls.clone())
                    },
                    reasoning_content: if ctx.reasoning_content.trim().is_empty() {
                        None
                    } else {
                        Some(ctx.reasoning_content.clone())
                    },
                },
                logprobs: None,
                finish_reason: Some(finish_reason),
            }],
            usage: None,
        };

        if self.include_usage {
            response.usage = Some(serde_json::json!({
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": total_tokens,
                "prompt_tokens_details": {"cached_tokens": 0}
            }));
        }

        response
    }

    // Directly process stream events and generate output, without pre-reading
    pub async fn process_stream<S>(
        self,
        event_stream: S,
    ) -> impl Stream<Item = Result<String, std::convert::Infallible>> + Send + 'static
    where
        S: Stream<Item = Result<ChatResponse, PoeError>> + Send + Unpin + 'static,
    {
        let ctx = Arc::new(Mutex::new(EventContext::default()));
        let handler_manager = EventHandlerManager::new();

        // Directly use unfold logic to process event stream
        let stream_processor = stream::unfold(
            (event_stream, false, ctx, handler_manager, self),
            move |(mut event_stream, mut is_done, ctx_arc, handler_manager, generator)| {
                let ctx_arc_clone = Arc::clone(&ctx_arc);
                async move {
                    if is_done {
                        debug!("✅ Streaming processing completed");
                        return None;
                    }

                    match event_stream.next().await {
                        Some(Ok(event)) => {
                            // Lock context and process event
                            let mut output_content: Option<String> = None;
                            {
                                let mut ctx_guard = ctx_arc_clone.lock().unwrap();

                                // Process event and get content to send
                                let chunk_content_opt =
                                    handler_manager.handle(&event, &mut ctx_guard);

                                // Check for errors
                                if let Some((_, error_response)) = &ctx_guard.error {
                                    debug!("❌ Detected error, interrupting stream");
                                    let error_json = serde_json::to_string(error_response).unwrap();
                                    return Some((
                                        Ok(format!("data: {}\n\n", error_json)),
                                        (event_stream, true, ctx_arc, handler_manager, generator),
                                    ));
                                }

                                // Check if completed
                                if ctx_guard.done {
                                    debug!("✅ Detected completion signal");
                                    is_done = true;
                                }

                                // Process returned content
                                match event.event {
                                    ChatEventType::Text => {
                                        if let Some(chunk_content) = chunk_content_opt {
                                            debug!("📝 Processing normal Text event");

                                            // Check if it's a reasoning content detection marker
                                            if chunk_content == "__REASONING_DETECTED__" {
                                                debug!("🧠 Detected reasoning content, preparing to send reasoning chunk");

                                                // Get the latest reasoning content (newly added since last sent)
                                                let current_reasoning_len =
                                                    ctx_guard.reasoning_content.len();
                                                let last_sent_reasoning_len = ctx_guard
                                                    .get("last_sent_reasoning_len")
                                                    .unwrap_or(0);

                                                if current_reasoning_len > last_sent_reasoning_len {
                                                    let new_reasoning = ctx_guard.reasoning_content
                                                        [last_sent_reasoning_len..]
                                                        .to_string();

                                                    if !new_reasoning.trim().is_empty() {
                                                        // Update the length of sent reasoning content
                                                        ctx_guard.insert(
                                                            "last_sent_reasoning_len",
                                                            current_reasoning_len,
                                                        );

                                                        // Send role chunk (if not sent yet)
                                                        let mut output_parts = Vec::new();

                                                        if !ctx_guard.role_chunk_sent {
                                                            let role_chunk =
                                                                generator.create_role_chunk();
                                                            let role_json =
                                                                serde_json::to_string(&role_chunk)
                                                                    .unwrap();
                                                            output_parts.push(format!(
                                                                "data: {}",
                                                                role_json
                                                            ));
                                                            ctx_guard.role_chunk_sent = true;
                                                        }

                                                        // Send reasoning content
                                                        let reasoning_chunk = generator
                                                            .create_reasoning_chunk(&new_reasoning);
                                                        let reasoning_json =
                                                            serde_json::to_string(&reasoning_chunk)
                                                                .unwrap();
                                                        output_parts.push(format!(
                                                            "data: {}",
                                                            reasoning_json
                                                        ));

                                                        let output =
                                                            output_parts.join("\n\n") + "\n\n";
                                                        debug!(
                                                            "🧠 Sending reasoning chunk | Length: {}",
                                                            format_bytes_length(output.len())
                                                        );

                                                        output_content = Some(output);
                                                    }
                                                }
                                            } else {
                                                // Normal content processing
                                                let processed = generator.process_file_references(
                                                    &chunk_content,
                                                    &ctx_guard.file_refs,
                                                );

                                                // Determine if role chunk needs to be sent
                                                if !ctx_guard.role_chunk_sent {
                                                    let role_chunk = generator.create_role_chunk();
                                                    let role_json =
                                                        serde_json::to_string(&role_chunk).unwrap();
                                                    ctx_guard.role_chunk_sent = true;

                                                    let content_chunk = generator
                                                        .create_stream_chunk(&processed, None);
                                                    let content_json =
                                                        serde_json::to_string(&content_chunk)
                                                            .unwrap();

                                                    output_content = Some(format!(
                                                        "data: {}\n\ndata: {}\n\n",
                                                        role_json, content_json
                                                    ));
                                                } else {
                                                    let chunk = generator
                                                        .create_stream_chunk(&processed, None);
                                                    let json =
                                                        serde_json::to_string(&chunk).unwrap();
                                                    output_content =
                                                        Some(format!("data: {}\n\n", json));
                                                }
                                            }
                                        }
                                    }
                                    ChatEventType::File => {
                                        // Process file event, if content is returned, it means there are image references to be processed immediately
                                        if let Some(chunk_content) = chunk_content_opt {
                                            debug!("🖼️ Processing file reference, generating output with URL");

                                            // Determine if role chunk needs to be sent
                                            if !ctx_guard.role_chunk_sent {
                                                let role_chunk = generator.create_role_chunk();
                                                let role_json =
                                                    serde_json::to_string(&role_chunk).unwrap();
                                                ctx_guard.role_chunk_sent = true;

                                                let content_chunk = generator
                                                    .create_stream_chunk(&chunk_content, None);
                                                let content_json =
                                                    serde_json::to_string(&content_chunk).unwrap();

                                                output_content = Some(format!(
                                                    "data: {}\n\ndata: {}\n\n",
                                                    role_json, content_json
                                                ));
                                            } else {
                                                let chunk = generator
                                                    .create_stream_chunk(&chunk_content, None);
                                                let json = serde_json::to_string(&chunk).unwrap();
                                                output_content =
                                                    Some(format!("data: {}\n\n", json));
                                            }
                                        }
                                    }
                                    ChatEventType::ReplaceResponse => {
                                        // If ReplaceResponse directly returns content, it means it contains image references
                                        if let Some(chunk_content) = chunk_content_opt {
                                            debug!("🔄 ReplaceResponse contains image references, sending directly");

                                            // Determine if role chunk needs to be sent
                                            if !ctx_guard.role_chunk_sent {
                                                let role_chunk = generator.create_role_chunk();
                                                let role_json =
                                                    serde_json::to_string(&role_chunk).unwrap();
                                                ctx_guard.role_chunk_sent = true;

                                                let content_chunk = generator
                                                    .create_stream_chunk(&chunk_content, None);
                                                let content_json =
                                                    serde_json::to_string(&content_chunk).unwrap();

                                                output_content = Some(format!(
                                                    "data: {}\n\ndata: {}\n\n",
                                                    role_json, content_json
                                                ));
                                            } else {
                                                let chunk = generator
                                                    .create_stream_chunk(&chunk_content, None);
                                                let json = serde_json::to_string(&chunk).unwrap();
                                                output_content =
                                                    Some(format!("data: {}\n\n", json));
                                            }
                                        }
                                    }
                                    ChatEventType::Json => {
                                        if !ctx_guard.tool_calls.is_empty() {
                                            debug!("🔧 Processing tool calls");
                                            let tool_chunk = generator
                                                .create_tool_calls_chunk(&ctx_guard.tool_calls);
                                            let json = serde_json::to_string(&tool_chunk).unwrap();

                                            if !ctx_guard.role_chunk_sent {
                                                let role_chunk = generator.create_role_chunk();
                                                let role_json =
                                                    serde_json::to_string(&role_chunk).unwrap();
                                                ctx_guard.role_chunk_sent = true;
                                                output_content = Some(format!(
                                                    "data: {}\n\ndata: {}\n\n",
                                                    role_json, json
                                                ));
                                            } else {
                                                output_content =
                                                    Some(format!("data: {}\n\n", json));
                                            }
                                        }
                                    }
                                    ChatEventType::Done => {
                                        // If Done event returns content, it means there are unprocessed image references
                                        if let Some(chunk_content) = chunk_content_opt {
                                            if chunk_content != "done" && !ctx_guard.image_urls_sent
                                            {
                                                debug!(
                                                    "✅ Done event contains unprocessed image references, sending final content"
                                                );
                                                let chunk = generator.create_stream_chunk(
                                                    &chunk_content,
                                                    Some("stop".to_string()),
                                                );
                                                let json = serde_json::to_string(&chunk).unwrap();
                                                output_content =
                                                    Some(format!("data: {}\n\n", json));
                                                ctx_guard.image_urls_sent = true; // Mark as sent
                                            } else {
                                                // General completion event
                                                let (
                                                    prompt_tokens,
                                                    completion_tokens,
                                                    total_tokens,
                                                ) = generator.calculate_tokens(&mut ctx_guard);
                                                let finish_reason =
                                                    if !ctx_guard.tool_calls.is_empty() {
                                                        "tool_calls"
                                                    } else {
                                                        "stop"
                                                    };
                                                let final_chunk = generator.create_stream_chunk(
                                                    "",
                                                    Some(finish_reason.to_string()),
                                                );
                                                let final_json = if generator.include_usage {
                                                    debug!(
                                                        "📊 Token usage statistics | prompt_tokens: {} | completion_tokens: {} | total_tokens: {}",
                                                        prompt_tokens,
                                                        completion_tokens,
                                                        total_tokens
                                                    );
                                                    let mut json_value =
                                                        serde_json::to_value(&final_chunk).unwrap();
                                                    json_value["usage"] = serde_json::json!({
                                                        "prompt_tokens": prompt_tokens,
                                                        "completion_tokens": completion_tokens,
                                                        "total_tokens": total_tokens,
                                                        "prompt_tokens_details": {"cached_tokens": 0}
                                                    });
                                                    serde_json::to_string(&json_value).unwrap()
                                                } else {
                                                    serde_json::to_string(&final_chunk).unwrap()
                                                };

                                                if !ctx_guard.role_chunk_sent {
                                                    let role_chunk = generator.create_role_chunk();
                                                    let role_json =
                                                        serde_json::to_string(&role_chunk).unwrap();
                                                    ctx_guard.role_chunk_sent = true;
                                                    output_content = Some(format!(
                                                        "data: {}\n\ndata: {}\n\n",
                                                        role_json, final_json
                                                    ));
                                                } else {
                                                    output_content =
                                                        Some(format!("data: {}\n\n", final_json));
                                                }
                                            }
                                        } else {
                                            // Completion event with no content
                                            let (prompt_tokens, completion_tokens, total_tokens) =
                                                generator.calculate_tokens(&mut ctx_guard);
                                            let finish_reason = if !ctx_guard.tool_calls.is_empty()
                                            {
                                                "tool_calls"
                                            } else {
                                                "stop"
                                            };
                                            let final_chunk = generator.create_stream_chunk(
                                                "",
                                                Some(finish_reason.to_string()),
                                            );
                                            let final_json = if generator.include_usage {
                                                debug!(
                                                    "📊 Token usage statistics | prompt_tokens: {} | completion_tokens: {} | total_tokens: {}",
                                                    prompt_tokens, completion_tokens, total_tokens
                                                );
                                                let mut json_value =
                                                    serde_json::to_value(&final_chunk).unwrap();
                                                json_value["usage"] = serde_json::json!({
                                                    "prompt_tokens": prompt_tokens,
                                                    "completion_tokens": completion_tokens,
                                                    "total_tokens": total_tokens,
                                                    "prompt_tokens_details": {"cached_tokens": 0}
                                                });
                                                serde_json::to_string(&json_value).unwrap()
                                            } else {
                                                serde_json::to_string(&final_chunk).unwrap()
                                            };

                                            if !ctx_guard.role_chunk_sent {
                                                let role_chunk = generator.create_role_chunk();
                                                let role_json =
                                                    serde_json::to_string(&role_chunk).unwrap();
                                                ctx_guard.role_chunk_sent = true;
                                                output_content = Some(format!(
                                                    "data: {}\n\ndata: {}\n\n",
                                                    role_json, final_json
                                                ));
                                            } else {
                                                output_content =
                                                    Some(format!("data: {}\n\n", final_json));
                                            }
                                        }
                                    }
                                    _ => {
                                        // Other event types, if content is returned, process it
                                        if let Some(chunk_content) = chunk_content_opt {
                                            if !ctx_guard.role_chunk_sent {
                                                let role_chunk = generator.create_role_chunk();
                                                let role_json =
                                                    serde_json::to_string(&role_chunk).unwrap();
                                                ctx_guard.role_chunk_sent = true;

                                                let content_chunk = generator
                                                    .create_stream_chunk(&chunk_content, None);
                                                let content_json =
                                                    serde_json::to_string(&content_chunk).unwrap();

                                                output_content = Some(format!(
                                                    "data: {}\n\ndata: {}\n\n",
                                                    role_json, content_json
                                                ));
                                            } else {
                                                let chunk = generator
                                                    .create_stream_chunk(&chunk_content, None);
                                                let json = serde_json::to_string(&chunk).unwrap();
                                                output_content =
                                                    Some(format!("data: {}\n\n", json));
                                            }
                                        }
                                    }
                                }

                                // If no output content and role chunk needs to be sent, send it
                                if output_content.is_none()
                                    && !ctx_guard.role_chunk_sent
                                    && (event.event == ChatEventType::Text
                                        || event.event == ChatEventType::ReplaceResponse
                                        || event.event == ChatEventType::File)
                                {
                                    let role_chunk = generator.create_role_chunk();
                                    let role_json = serde_json::to_string(&role_chunk).unwrap();
                                    ctx_guard.role_chunk_sent = true;
                                    output_content = Some(format!("data: {}\n\n", role_json));
                                }
                            }

                            // Return output content
                            if let Some(output) = output_content {
                                if !output.trim().is_empty() {
                                    debug!(
                                        "📤 Sending stream chunk | Length: {}",
                                        format_bytes_length(output.len())
                                    );
                                    Some((
                                        Ok(output),
                                        (
                                            event_stream,
                                            is_done,
                                            ctx_arc,
                                            handler_manager,
                                            generator,
                                        ),
                                    ))
                                } else {
                                    // Empty output, continue processing
                                    Some((
                                        Ok(String::new()),
                                        (
                                            event_stream,
                                            is_done,
                                            ctx_arc,
                                            handler_manager,
                                            generator,
                                        ),
                                    ))
                                }
                            } else {
                                // No output, but continue processing
                                Some((
                                    Ok(String::new()),
                                    (event_stream, is_done, ctx_arc, handler_manager, generator),
                                ))
                            }
                        }
                        Some(Err(e)) => {
                            error!("❌ Streaming processing error: {}", e);
                            let error_response = convert_poe_error_to_openai(&e.to_string(), false);
                            let error_json = serde_json::to_string(&error_response.1).unwrap();
                            Some((
                                Ok(format!("data: {}\n\n", error_json)),
                                (event_stream, true, ctx_arc, handler_manager, generator),
                            ))
                        }
                        None => {
                            debug!("⏹️ Event stream ended");
                            None
                        }
                    }
                }
            },
        );

        // Add done message
        let done_message = "data: [DONE]\n\n".to_string();

        // Filter out empty messages and add done message
        Box::pin(
            stream_processor
                .filter(|result| {
                    future::ready(match result {
                        Ok(s) => !s.is_empty(),
                        Err(_) => true,
                    })
                })
                .chain(stream::once(future::ready(Ok(done_message)))),
        )
    }
}
