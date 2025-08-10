use crate::cache::get_cached_config;
use crate::evert::{EventContext, EventHandlerManager};
use crate::poe_client::{PoeClientWrapper, create_chat_request};
use crate::types::*;
use crate::utils::{
    convert_poe_error_to_openai, count_completion_tokens, count_message_tokens,
    format_bytes_length, format_duration, process_message_images,
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
    info!("📝 收到新的聊天完成請求");

    let max_size: usize = std::env::var("MAX_REQUEST_SIZE")
        .unwrap_or_else(|_| "1073741824".to_string())
        .parse()
        .unwrap_or(1024 * 1024 * 1024);

    // 從緩存獲取 models.yaml 配置
    let config = get_cached_config().await;
    debug!("🔧 從緩存獲取配置 | 啟用狀態: {:?}", config.enable);

    // 驗證授權
    let access_key = match req.headers().get("Authorization") {
        Some(auth) => {
            let auth_str = auth.to_str().unwrap_or("");
            if let Some(stripped) = auth_str.strip_prefix("Bearer ") {
                debug!("🔑 驗證令牌長度: {}", stripped.len());
                stripped.to_string()
            } else {
                error!("❌ 無效的授權格式");
                res.status_code(StatusCode::UNAUTHORIZED);
                res.render(Json(json!({ "error": "無效的 Authorization" })));
                return;
            }
        }
        None => {
            error!("❌ 缺少授權標頭");
            res.status_code(StatusCode::UNAUTHORIZED);
            res.render(Json(json!({ "error": "缺少 Authorization" })));
            return;
        }
    };

    // 解析請求體
    let chat_request = match req.payload_with_max_size(max_size).await {
        Ok(bytes) => match serde_json::from_slice::<ChatCompletionRequest>(bytes) {
            Ok(req) => {
                debug!(
                    "📊 請求解析成功 | 模型: {} | 訊息數量: {} | 是否串流: {:?}",
                    req.model,
                    req.messages.len(),
                    req.stream
                );
                req
            }
            Err(e) => {
                error!("❌ JSON 解析失敗: {}", e);
                res.status_code(StatusCode::BAD_REQUEST);
                res.render(Json(OpenAIErrorResponse {
                    error: OpenAIError {
                        message: format!("JSON 解析失敗: {}", e),
                        r#type: "invalid_request_error".to_string(),
                        code: "parse_error".to_string(),
                        param: None,
                    },
                }));
                return;
            }
        },
        Err(e) => {
            error!("❌ 請求大小超過限制或讀取失敗: {}", e);
            res.status_code(StatusCode::PAYLOAD_TOO_LARGE);
            res.render(Json(OpenAIErrorResponse {
                error: OpenAIError {
                    message: format!("請求大小超過限制 ({} bytes) 或讀取失敗: {}", max_size, e),
                    r#type: "invalid_request_error".to_string(),
                    code: "payload_too_large".to_string(),
                    param: None,
                },
            }));
            return;
        }
    };

    // 尋找映射的原始模型名稱
    let (display_model, original_model) = if config.enable.unwrap_or(false) {
        let requested_model = chat_request.model.clone();
        // 檢查當前請求的模型是否是某個映射的目標
        let mapping_entry = config.models.iter().find(|(_, cfg)| {
            if let Some(mapping) = &cfg.mapping {
                mapping.to_lowercase() == requested_model.to_lowercase()
            } else {
                false
            }
        });
        if let Some((original_name, _)) = mapping_entry {
            // 如果找到映射，使用原始模型名稱
            debug!("🔄 反向模型映射: {} -> {}", requested_model, original_name);
            (requested_model, original_name.clone())
        } else {
            // 如果沒找到映射，檢查是否有直接映射配置
            if let Some(model_config) = config.models.get(&requested_model) {
                if let Some(mapped_name) = &model_config.mapping {
                    debug!("🔄 直接模型映射: {} -> {}", requested_model, mapped_name);
                    (requested_model.clone(), requested_model)
                } else {
                    // 沒有映射配置，使用原始名稱
                    (requested_model.clone(), requested_model)
                }
            } else {
                // 完全沒有相關配置，使用原始名稱
                (requested_model.clone(), requested_model)
            }
        }
    } else {
        // 配置未啟用，直接使用原始名稱
        (chat_request.model.clone(), chat_request.model.clone())
    };
    info!("🤖 使用模型: {} (原始: {})", display_model, original_model);

    // 創建客戶端
    let client = PoeClientWrapper::new(&original_model, &access_key);

    // 處理消息中的image_url
    let mut messages = chat_request.messages.clone();
    if let Err(e) = process_message_images(&client, &mut messages).await {
        error!("❌ 處理文件上傳失敗: {}", e);
        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
        res.render(Json(OpenAIErrorResponse {
            error: OpenAIError {
                message: format!("處理文件上傳失敗: {}", e),
                r#type: "processing_error".to_string(),
                code: "file_processing_failed".to_string(),
                param: None,
            },
        }));
        return;
    }

    // 計算 prompt_tokens
    let prompt_tokens = count_message_tokens(&messages);
    debug!("📊 計算 prompt_tokens: {}", prompt_tokens);

    let stream = chat_request.stream.unwrap_or(false);
    debug!("🔄 請求模式: {}", if stream { "串流" } else { "非串流" });

    // 創建 chat 請求
    let chat_request_obj = create_chat_request(
        &original_model,
        &chat_request,
    )
    .await;

    // 檢查是否需要包含 usage 統計
    let include_usage = chat_request
        .stream_options
        .as_ref()
        .and_then(|opts| opts.include_usage)
        .unwrap_or(false);
    debug!("📊 是否包含 usage 統計: {}", include_usage);

    // 創建輸出生成器
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
                    info!("🚫 偵測到 Poe 點數不足錯誤，返回 429 狀態碼。");
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
            error!("❌ 建立串流請求失敗: {}", e);
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(json!({ "error": e.to_string() })));
        }
    }

    let duration = start_time.elapsed();
    info!("✅ 請求處理完成 | 耗時: {}", format_duration(duration));
}

// 處理串流響應
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
        "🌊 開始處理串流響應 | ID: {} | 模型: {} | 包含使用統計: {}",
        id, model, include_usage
    );

    // 設置串流響應的頭部
    res.headers_mut()
        .insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
    res.headers_mut()
        .insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    res.headers_mut()
        .insert(header::CONNECTION, "keep-alive".parse().unwrap());

    // 處理事件流並生成輸出
    let processed_stream = output_generator
        .process_stream(Box::pin(event_stream))
        .await;
    res.stream(processed_stream);

    let duration = start_time.elapsed();
    info!(
        "✅ 串流響應處理完成 | ID: {} | 耗時: {}",
        id,
        format_duration(duration)
    );
}

// 處理非串流響應
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
        "📦 開始處理非串流響應 | ID: {} | 模型: {} | 包含使用統計: {}",
        id, model, include_usage
    );

    let handler_manager = EventHandlerManager::new();
    let mut ctx = EventContext::default();

    // 處理所有事件
    while let Some(result) = event_stream.next().await {
        match result {
            Ok(event) => {
                handler_manager.handle(&event, &mut ctx);
                // 檢查是否有錯誤
                if let Some((status, error_response)) = &ctx.error {
                    error!("❌ 處理錯誤: {:?}", error_response);
                    res.status_code(*status);
                    res.render(Json(error_response));
                    return;
                }
                // 檢查是否完成
                if ctx.done {
                    debug!("✅ 收到完成事件");
                    break;
                }
            }
            Err(e) => {
                error!("❌ 處理錯誤: {}", e);
                let (status, error_response) = convert_poe_error_to_openai(&e.to_string(), false);
                res.status_code(status);
                res.render(Json(error_response));
                return;
            }
        }
    }

    // 創建最終響應
    let response = output_generator.create_final_response(&mut ctx);
    res.render(Json(response));

    let duration = start_time.elapsed();
    info!(
        "✅ 非串流響應處理完成 | ID: {} | 耗時: {}",
        id,
        format_duration(duration)
    );
}

// 輸出生成器 - 用於將 EventContext 轉換為最終輸出
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

    // 處理文件引用，將 [ref_id] 替換為 (url)
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
                debug!("🖼️ 替換圖片引用 | ID: {} | URL: {}", ref_id, file_data.url);
                has_replaced = true;
            }
        }

        if has_replaced {
            debug!("✅ 成功替換圖片引用");
        } else if processed.contains('[') && processed.contains(']') {
            warn!(
                "⚠️ 文本包含可能的圖片引用格式，但未找到對應引用: {}",
                processed
            );
        }

        processed
    }

    // 計算 token 使用情況
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

    // 創建角色 chunk
    // 創建角色 chunk
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

    // 思考 chunk
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
    // 創建串流 chunk
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
            "🔧 創建串流片段 | ID: {} | 內容長度: {}",
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

    // 創建工具調用 chunk
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

    // 創建最終完整回應（非串流模式）
    fn create_final_response(&self, ctx: &mut EventContext) -> ChatCompletionResponse {
        // 處理剩餘的 pending_text
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

        // 處理內容，包括文件引用替換
        let content = if let Some(replace_content) = &ctx.replace_buffer {
            self.process_file_references(replace_content, &ctx.file_refs)
        } else {
            self.process_file_references(&ctx.content, &ctx.file_refs)
        };

        // 計算 token
        let (prompt_tokens, completion_tokens, total_tokens) = self.calculate_tokens(ctx);

        // 確定 finish_reason
        let finish_reason = if !ctx.tool_calls.is_empty() {
            "tool_calls".to_string()
        } else {
            "stop".to_string()
        };

        debug!(
            "📤 準備發送回應 | 內容長度: {} | 思考長度: {} | 工具調用數量: {} | 完成原因: {}",
            format_bytes_length(content.len()),
            format_bytes_length(ctx.reasoning_content.len()),
            ctx.tool_calls.len(),
            finish_reason
        );

        // 創建響應
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

    // 直接處理串流事件並產生輸出，無需預讀
    pub async fn process_stream<S>(
        self,
        event_stream: S,
    ) -> impl Stream<Item = Result<String, std::convert::Infallible>> + Send + 'static
    where
        S: Stream<Item = Result<ChatResponse, PoeError>> + Send + Unpin + 'static,
    {
        let ctx = Arc::new(Mutex::new(EventContext::default()));
        let handler_manager = EventHandlerManager::new();

        // 直接用 unfold 邏輯處理事件流
        let stream_processor = stream::unfold(
            (event_stream, false, ctx, handler_manager, self),
            move |(mut event_stream, mut is_done, ctx_arc, handler_manager, generator)| {
                let ctx_arc_clone = Arc::clone(&ctx_arc);
                async move {
                    if is_done {
                        debug!("✅ 串流處理完成");
                        return None;
                    }

                    match event_stream.next().await {
                        Some(Ok(event)) => {
                            // 鎖定上下文並處理事件
                            let mut output_content: Option<String> = None;
                            {
                                let mut ctx_guard = ctx_arc_clone.lock().unwrap();

                                // 處理事件並獲取要發送的內容
                                let chunk_content_opt =
                                    handler_manager.handle(&event, &mut ctx_guard);

                                // 檢查錯誤
                                if let Some((_, error_response)) = &ctx_guard.error {
                                    debug!("❌ 檢測到錯誤，中斷串流");
                                    let error_json = serde_json::to_string(error_response).unwrap();
                                    return Some((
                                        Ok(format!("data: {}\n\n", error_json)),
                                        (event_stream, true, ctx_arc, handler_manager, generator),
                                    ));
                                }

                                // 檢查是否完成
                                if ctx_guard.done {
                                    debug!("✅ 檢測到完成信號");
                                    is_done = true;
                                }

                                // 處理返回的內容
                                match event.event {
                                    ChatEventType::Text => {
                                        if let Some(chunk_content) = chunk_content_opt {
                                            debug!("📝 處理普通 Text 事件");

                                            // 檢查是否是思考內容檢測標記
                                            if chunk_content == "__REASONING_DETECTED__" {
                                                debug!("🧠 檢測到思考內容，準備發送思考片段");

                                                // 獲取最新的思考內容（從上次發送後的新增部分）
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
                                                        // 更新已發送的思考內容長度
                                                        ctx_guard.insert(
                                                            "last_sent_reasoning_len",
                                                            current_reasoning_len,
                                                        );

                                                        // 發送角色塊（如果還沒發送）
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

                                                        // 發送思考內容
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
                                                            "🧠 發送思考片段 | 長度: {}",
                                                            format_bytes_length(output.len())
                                                        );

                                                        output_content = Some(output);
                                                    }
                                                }
                                            } else {
                                                // 正常內容處理
                                                let processed = generator.process_file_references(
                                                    &chunk_content,
                                                    &ctx_guard.file_refs,
                                                );

                                                // 判斷是否需要發送角色塊
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
                                        // 處理文件事件，如果返回了內容，表示有圖片引用需要立即處理
                                        if let Some(chunk_content) = chunk_content_opt {
                                            debug!("🖼️ 處理檔案引用，產生包含URL的輸出");

                                            // 判斷是否需要發送角色塊
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
                                        // 如果 ReplaceResponse 直接返回了內容，說明其中包含了圖片引用
                                        if let Some(chunk_content) = chunk_content_opt {
                                            debug!("🔄 ReplaceResponse 包含圖片引用，直接發送");

                                            // 判斷是否需要發送角色塊
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
                                            debug!("🔧 處理工具調用");
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
                                        // 如果 Done 事件返回了內容，表示有未處理的圖片引用
                                        if let Some(chunk_content) = chunk_content_opt {
                                            if chunk_content != "done" && !ctx_guard.image_urls_sent
                                            {
                                                debug!(
                                                    "✅ Done 事件包含未處理的圖片引用，發送最終內容"
                                                );
                                                let chunk = generator.create_stream_chunk(
                                                    &chunk_content,
                                                    Some("stop".to_string()),
                                                );
                                                let json = serde_json::to_string(&chunk).unwrap();
                                                output_content =
                                                    Some(format!("data: {}\n\n", json));
                                                ctx_guard.image_urls_sent = true; // 標記已發送
                                            } else {
                                                // 一般完成事件
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
                                                        "📊 Token 使用統計 | prompt_tokens: {} | completion_tokens: {} | total_tokens: {}",
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
                                            // 無內容的完成事件
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
                                                    "📊 Token 使用統計 | prompt_tokens: {} | completion_tokens: {} | total_tokens: {}",
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
                                        // 其他事件類型，如果有返回內容也處理
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

                                // 如果沒有輸出內容且需要發送角色塊，則發送
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

                            // 返回輸出內容
                            if let Some(output) = output_content {
                                if !output.trim().is_empty() {
                                    debug!(
                                        "📤 發送串流片段 | 長度: {}",
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
                                    // 空輸出，繼續處理
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
                                // 沒有輸出，但繼續處理
                                Some((
                                    Ok(String::new()),
                                    (event_stream, is_done, ctx_arc, handler_manager, generator),
                                ))
                            }
                        }
                        Some(Err(e)) => {
                            error!("❌ 串流處理錯誤: {}", e);
                            let error_response = convert_poe_error_to_openai(&e.to_string(), false);
                            let error_json = serde_json::to_string(&error_response.1).unwrap();
                            Some((
                                Ok(format!("data: {}\n\n", error_json)),
                                (event_stream, true, ctx_arc, handler_manager, generator),
                            ))
                        }
                        None => {
                            debug!("⏹️ 事件流結束");
                            None
                        }
                    }
                }
            },
        );

        // 添加結束消息
        let done_message = "data: [DONE]\n\n".to_string();

        // 過濾掉空的訊息，並加上結束訊息
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
