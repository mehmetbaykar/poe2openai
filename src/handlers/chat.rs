use crate::poe_client::{PoeClientWrapper, create_chat_request};
use crate::types::*;
use crate::utils::{
    convert_poe_error_to_openai, count_completion_tokens, count_message_tokens,
    format_bytes_length, format_duration, get_cached_config, process_message_images,
};
use chrono::Utc;
use futures_util::future;
use futures_util::stream::{self, Stream, StreamExt};
use nanoid::nanoid;
use poe_api_process::{ChatEventType, ChatResponse, ChatResponseData, PoeError};
use salvo::http::header;
use salvo::prelude::*;
use serde_json::json;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
    let chat_request_obj = create_chat_request(
        &original_model,
        messages,
        chat_request.temperature,
        chat_request.tools,
        chat_request.logit_bias,
        chat_request.stop,
    )
    .await;
    // 檢查是否需要包含 usage 統計
    let include_usage = chat_request
        .stream_options
        .as_ref()
        .and_then(|opts| opts.include_usage)
        .unwrap_or(false);
    debug!("📊 是否包含 usage 統計: {}", include_usage);
    // 創建一個共享的計數器用於跟踪 completion_tokens
    let completion_tokens_counter = Arc::new(AtomicU32::new(0));
    match client.stream_request(chat_request_obj).await {
        Ok(event_stream) => {
            if stream {
                handle_stream_response(
                    res,
                    event_stream,
                    &display_model,
                    include_usage,
                    prompt_tokens,
                    Arc::clone(&completion_tokens_counter),
                )
                .await;
            } else {
                handle_non_stream_response(
                    res,
                    event_stream,
                    &display_model,
                    include_usage,
                    prompt_tokens,
                    Arc::clone(&completion_tokens_counter),
                )
                .await;
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

async fn handle_stream_response(
    res: &mut Response,
    mut event_stream: Pin<Box<dyn Stream<Item = Result<ChatResponse, PoeError>> + Send>>,
    model: &str,
    include_usage: bool,
    prompt_tokens: u32,
    completion_tokens_counter: Arc<AtomicU32>,
) {
    let start_time = Instant::now();
    let id = nanoid!(10);
    let created = Utc::now().timestamp();
    let model = model.to_string();
    info!(
        "🌊 開始處理串流響應 | ID: {} | 模型: {} | 包含使用統計: {}",
        id, model, include_usage
    );

    res.headers_mut()
        .insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
    res.headers_mut()
        .insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    res.headers_mut()
        .insert(header::CONNECTION, "keep-alive".parse().unwrap());

    let mut replace_response = false;
    let mut full_content = String::new();
    let mut first_two_events = Vec::new();
    let mut file_refs = HashMap::new();
    let mut has_done_event = false;
    let mut has_tool_calls = false;
    let mut initial_tool_calls = Vec::new();

    debug!("🔍 檢查初始事件");
    for _ in 0..3 {
        // 增加檢查事件數量，確保能捕獲到 file 事件
        if let Some(Ok(event)) = event_stream.next().await {
            debug!("📥 收到初始事件: {:?}", event.event);
            // 特別處理：如果是 Done 事件，標記但不消耗它
            if event.event == ChatEventType::Done {
                has_done_event = true;
                debug!("🔍 檢測到 Done 事件，但不消耗它");
                continue; // 跳過這個事件，不添加到 first_two_events
            }

            // 檢查是否有工具調用事件
            if event.event == ChatEventType::Json {
                if let Some(ChatResponseData::ToolCalls(tool_calls)) = &event.data {
                    has_tool_calls = true;
                    initial_tool_calls = tool_calls.clone();
                    debug!("🔍 檢測到工具調用事件: {} 個工具", tool_calls.len());
                }
            }

            first_two_events.push(event);
        }
    }

    for event in first_two_events {
        match event.event {
            ChatEventType::ReplaceResponse => {
                debug!("🔄 檢測到 ReplaceResponse 模式");
                replace_response = true;
                if let Some(ChatResponseData::Text { text }) = event.data {
                    full_content = text;
                }
            }
            ChatEventType::Text => {
                if let Some(ChatResponseData::Text { text }) = event.data {
                    if !replace_response {
                        full_content.push_str(&text);
                    }
                }
            }
            ChatEventType::File => {
                if let Some(ChatResponseData::File(file_data)) = event.data {
                    debug!(
                        "🖼️ 收到檔案事件 | 名稱: {} | URL: {}",
                        file_data.name, file_data.url
                    );
                    file_refs.insert(file_data.inline_ref.clone(), file_data);
                }
            }
            ChatEventType::Json => {
                debug!("📝 收到 JSON 事件");
                // 檢查是否包含工具調用
                if let Some(ChatResponseData::ToolCalls(tool_calls)) = event.data {
                    debug!("🔧 收到工具調用，數量: {}", tool_calls.len());
                    has_tool_calls = true;
                    // 在流式模式下，我們會在後續處理中處理工具調用
                }
            }
            ChatEventType::Error => {
                if !replace_response {
                    if let Some(ChatResponseData::Error { text, allow_retry }) = event.data {
                        error!("❌ 串流處理錯誤: {}", text);
                        let (status, error_response) =
                            convert_poe_error_to_openai(&text, allow_retry);
                        res.status_code(status);
                        res.render(Json(error_response));
                        return;
                    }
                }
            }
            ChatEventType::Done => {
                debug!("✅ 初始事件處理完成");
                has_done_event = true;
            }
        }
    }
    debug!("✅ 初始事件處理完成");

    // 處理圖片引用，替換內容中的引用標記為實際URL
    for (ref_id, file_data) in &file_refs {
        let img_marker = format!("[{}]", ref_id);
        let replacement = format!("({})", file_data.url);
        full_content = full_content.replace(&img_marker, &replacement);
        debug!("🖼️ 替換圖片引用 | ID: {} | URL: {}", ref_id, file_data.url);
    }

    let id_for_log = id.clone();

    if replace_response {
        debug!("🔄 使用 ReplaceResponse 處理模式");

        let processed_stream: Pin<
            Box<dyn Stream<Item = Result<String, std::convert::Infallible>> + Send>,
        > = if has_tool_calls {
            debug!("🔧 檢測到工具調用，使用特殊處理流程");
            // 為工具調用創建特殊處理邏輯
            Box::pin(
                handle_tool_calls_in_stream(
                    id.clone(),
                    created,
                    model.clone(),
                    event_stream,
                    full_content,
                    initial_tool_calls,
                )
                .await,
            )
        } else {
            let id = id.clone();
            let model = model.clone();
            let initial_content_for_handler = full_content.clone();
            let file_refs_for_handler = file_refs.clone();

            Box::pin(stream::once(async move {
                // 將初始內容傳遞給 handle_replace_response，同時傳遞文件引用
                let content = handle_replace_response(
                    event_stream,
                    initial_content_for_handler,
                    file_refs_for_handler,
                    Arc::clone(&completion_tokens_counter),
                    include_usage,
                    has_done_event, // 傳遞是否已經檢測到 Done 事件
                )
                .await;

                // 確保記錄最終要發送的內容
                debug!("📤 準備發送到客戶端的最終內容: {}", content);

                let completion_tokens = if include_usage {
                    completion_tokens_counter.load(Ordering::SeqCst)
                } else {
                    0
                };
                let total_tokens = prompt_tokens + completion_tokens;

                debug!(
                    "📤 ReplaceResponse 處理完成 | 最終內容長度: {} | Token 數: {}",
                    format_bytes_length(content.len()),
                    completion_tokens
                );

                let content_chunk = create_stream_chunk(&id, created, &model, &content, None);
                let content_json = serde_json::to_string(&content_chunk).unwrap();
                let content_message = format!("data: {}\n\n", content_json);

                let final_chunk =
                    create_stream_chunk(&id, created, &model, "", Some("stop".to_string()));

                let final_message = if include_usage {
                    debug!(
                        "📊 Token 使用統計 | prompt_tokens: {} | completion_tokens: {} | total_tokens: {}",
                        prompt_tokens, completion_tokens, total_tokens
                    );
                    let mut final_json = serde_json::to_value(&final_chunk).unwrap();
                    final_json["usage"] = serde_json::json!({
                        "prompt_tokens": prompt_tokens,
                        "completion_tokens": completion_tokens,
                        "total_tokens": total_tokens,
                        "prompt_tokens_details": {"cached_tokens": 0}
                    });
                    format!(
                        "{}data: {}\n\ndata: [DONE]\n\n",
                        content_message,
                        serde_json::to_string(&final_json).unwrap()
                    )
                } else {
                    let final_json = serde_json::to_string(&final_chunk).unwrap();
                    format!(
                        "{}data: {}\n\ndata: [DONE]\n\n",
                        content_message, final_json
                    )
                };

                Ok::<_, std::convert::Infallible>(final_message)
            }))
        };

        res.stream(processed_stream);
    } else {
        debug!("🔄 使用標準串流處理模式");

        // 首先發送角色信息
        let role_delta = Delta {
            role: Some("assistant".to_string()),
            content: None,
            refusal: None,
            tool_calls: None,
        };

        let role_chunk = ChatCompletionChunk {
            id: format!("chatcmpl-{}", id),
            object: "chat.completion.chunk".to_string(),
            created,
            model: model.clone(),
            choices: vec![Choice {
                index: 0,
                delta: role_delta,
                finish_reason: None,
            }],
        };

        let role_json = serde_json::to_string(&role_chunk).unwrap();
        let role_message = format!("data: {}\n\n", role_json);

        // 如果有工具調用，需要先發送
        let tool_message = if has_tool_calls && !initial_tool_calls.is_empty() {
            debug!(
                "🔧 準備發送初始工具調用，數量: {}",
                initial_tool_calls.len()
            );
            let tool_delta = Delta {
                role: None,
                content: None,
                refusal: None,
                tool_calls: Some(initial_tool_calls.clone()),
            };

            let tool_chunk = ChatCompletionChunk {
                id: format!("chatcmpl-{}", id),
                object: "chat.completion.chunk".to_string(),
                created,
                model: model.clone(),
                choices: vec![Choice {
                    index: 0,
                    delta: tool_delta,
                    finish_reason: Some("tool_calls".to_string()),
                }],
            };

            let tool_json = serde_json::to_string(&tool_chunk).unwrap();
            debug!("🔧 創建工具調用訊息: {}", tool_json);
            format!("data: {}\n\n", tool_json)
        } else {
            String::new()
        };

        // 然後處理內容(如果有)
        let content_message = if !full_content.is_empty() && !has_tool_calls {
            let initial_chunk = create_stream_chunk(&id, created, &model, &full_content, None);
            let initial_chunk_json = serde_json::to_string(&initial_chunk).unwrap();
            format!("data: {}\n\n", initial_chunk_json)
        } else {
            String::new()
        };

        // 組合初始消息流
        let initial_messages = if has_tool_calls {
            role_message + &tool_message
        } else if !content_message.is_empty() {
            role_message + &content_message
        } else {
            role_message
        };

        // 基於Arc 共享的累積文本
        let accumulated_text = Arc::new(Mutex::new(full_content.clone()));
        let accumulated_file_refs = Arc::new(Mutex::new(file_refs.clone()));

        // 如果已經有工具調用和完成事件，可以直接結束串流
        let processed_stream: Pin<
            Box<dyn Stream<Item = Result<String, std::convert::Infallible>> + Send>,
        > = if has_tool_calls && has_done_event {
            debug!("🏁 已經收到工具調用和完成事件，直接結束串流");

            let completion_tokens = if include_usage {
                let tokens = count_completion_tokens(&full_content);
                completion_tokens_counter.store(tokens, Ordering::SeqCst);
                tokens
            } else {
                0
            };

            let done_message = if include_usage {
                let total_tokens = prompt_tokens + completion_tokens;
                debug!(
                    "📊 Token 使用統計 | prompt_tokens: {} | completion_tokens: {} | total_tokens: {}",
                    prompt_tokens, completion_tokens, total_tokens
                );

                let final_chunk =
                    create_stream_chunk(&id, created, &model, "", Some("tool_calls".to_string()));
                let mut final_json = serde_json::to_value(&final_chunk).unwrap();
                final_json["usage"] = serde_json::json!({
                    "prompt_tokens": prompt_tokens,
                    "completion_tokens": completion_tokens,
                    "total_tokens": total_tokens,
                    "prompt_tokens_details": {"cached_tokens": 0}
                });
                format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    serde_json::to_string(&final_json).unwrap()
                )
            } else {
                "data: [DONE]\n\n".to_string()
            };

            let full_message = initial_messages + &done_message;
            Box::pin(stream::once(future::ready(
                Ok::<_, std::convert::Infallible>(full_message),
            )))
        } else {
            // 否則繼續處理事件流
            let id = id.clone();
            let model = model.clone();
            let accumulated_text_clone = Arc::clone(&accumulated_text);
            let accumulated_file_refs_clone = Arc::clone(&accumulated_file_refs);

            Box::pin(
                stream::once(future::ready(Ok::<_, std::convert::Infallible>(initial_messages)))
                .chain(stream::unfold(
                    (event_stream, false),
                    move |(mut event_stream, mut is_done)| {
                        let id = id.clone();
                        let model = model.clone();
                        let completion_tokens_counter_clone = Arc::clone(&completion_tokens_counter);
                        let accumulated_text_clone = Arc::clone(&accumulated_text_clone);
                        let accumulated_file_refs_clone = Arc::clone(&accumulated_file_refs_clone);
                        let has_tool_calls_clone = has_tool_calls; // Capture has_tool_calls

                        async move {
                            if is_done {
                                debug!("✅ 串流處理完成");
                                return None;
                            }

                            match event_stream.next().await {
                                Some(Ok(event)) => match event.event {
                                    ChatEventType::Text => {
                                        if let Some(ChatResponseData::Text { text }) = event.data {
                                            // 收集文本以便在最後計算 tokens
                                            let mut text_to_send = text.clone();
                                            // 處理可能含有的圖片引用
                                            let file_refs = accumulated_file_refs_clone.lock().unwrap();
                                            for (ref_id, file_data) in file_refs.iter() {
                                                let img_marker = format!("[{}]", ref_id);
                                                let replacement = format!("({})", file_data.url);
                                                text_to_send = text_to_send.replace(&img_marker, &replacement);
                                            }
                                            accumulated_text_clone.lock().unwrap().push_str(&text_to_send);

                                            // 如果已經有工具調用，則不再發送文本
                                            if !has_tool_calls_clone {
                                                let chunk = create_stream_chunk(
                                                    &id, created, &model, &text_to_send, None,
                                                );
                                                let chunk_json = serde_json::to_string(&chunk).unwrap();
                                                Some((
                                                    Ok(format!("data: {}\n\n", chunk_json)),
                                                    (event_stream, is_done),
                                                ))
                                            } else {
                                                Some((Ok(String::new()), (event_stream, is_done)))
                                            }
                                        } else {
                                            Some((Ok(String::new()), (event_stream, is_done)))
                                        }
                                    }
                                    ChatEventType::File => {
                                        if let Some(ChatResponseData::File(file_data)) = event.data {
                                            debug!("🖼️ 收到檔案事件 | 名稱: {} | URL: {}", file_data.name, file_data.url);
                                            let mut file_refs = accumulated_file_refs_clone.lock().unwrap();
                                            file_refs.insert(file_data.inline_ref.clone(), file_data);
                                            // 檔案事件不直接發送內容，僅保存引用
                                            Some((Ok(String::new()), (event_stream, is_done)))
                                        } else {
                                            Some((Ok(String::new()), (event_stream, is_done)))
                                        }
                                    }
                                    ChatEventType::Json => {
                                        debug!("📝 收到 JSON 事件");
                                        // 處理工具調用事件
                                        if let Some(ChatResponseData::ToolCalls(tool_calls)) = event.data {
                                            debug!("🔧 處理工具調用，數量: {}", tool_calls.len());
                                            // 創建包含工具調用的 delta
                                            let tool_delta = Delta {
                                                role: None,
                                                content: None,
                                                refusal: None,
                                                tool_calls: Some(tool_calls),
                                            };
                                            // 創建包含工具調用的 chunk
                                            let tool_chunk = ChatCompletionChunk {
                                                id: format!("chatcmpl-{}", id),
                                                object: "chat.completion.chunk".to_string(),
                                                created,
                                                model: model.to_string(),
                                                choices: vec![Choice {
                                                    index: 0,
                                                    delta: tool_delta,
                                                    finish_reason: Some("tool_calls".to_string()),
                                                }],
                                            };
                                            let tool_chunk_json =
                                                serde_json::to_string(&tool_chunk).unwrap();
                                            debug!("📤 發送工具調用 chunk");
                                            Some((
                                                Ok(format!("data: {}\n\n", tool_chunk_json)),
                                                (event_stream, is_done),
                                            ))
                                        } else {
                                            debug!("⏭️ 收到 JSON 事件但沒有工具調用");
                                            Some((Ok(String::new()), (event_stream, is_done)))
                                        }
                                    }
                                    ChatEventType::Error => {
                                        if let Some(ChatResponseData::Error { text, allow_retry: _ }) = event.data {
                                            error!("❌ 串流處理錯誤: {}", text);
                                            let error_chunk = json!({
                                                "error": {
                                                    "message": text,
                                                    "type": "stream_error",
                                                    "code": "stream_error"
                                                }
                                            });
                                            let error_message = format!(
                                                "data: {}\n\ndata: [DONE]\n\n",
                                                serde_json::to_string(&error_chunk).unwrap()
                                            );
                                            Some((Ok(error_message), (event_stream, true)))
                                        } else {
                                            Some((Ok(String::new()), (event_stream, is_done)))
                                        }
                                    }
                                    ChatEventType::Done => {
                                        debug!("✅ 串流完成");
                                        is_done = true;
                                        let completion_tokens = if include_usage {
                                            // 獲取累積的完整文本
                                            let full_text = accumulated_text_clone.lock().unwrap().clone();
                                            // 計算完整文本的 tokens 並更新計數器
                                            let tokens = count_completion_tokens(&full_text);
                                            completion_tokens_counter_clone.store(tokens, Ordering::SeqCst);
                                            tokens
                                        } else {
                                            0
                                        };

                                        // 決定完成原因
                                        let finish_reason = if has_tool_calls_clone {
                                            "tool_calls".to_string()
                                        } else {
                                            "stop".to_string()
                                        };

                                        if include_usage {
                                            let total_tokens = prompt_tokens + completion_tokens;
                                            debug!(
                                                "📊 Token 使用統計 | prompt_tokens: {} | completion_tokens: {} | total_tokens: {}",
                                                prompt_tokens, completion_tokens, total_tokens
                                            );
                                            let final_chunk = create_stream_chunk(
                                                &id,
                                                created,
                                                &model,
                                                "",
                                                Some(finish_reason),
                                            );
                                            let mut final_json: serde_json::Value = serde_json::to_value(&final_chunk).unwrap();
                                            final_json["usage"] = serde_json::json!({
                                                "prompt_tokens": prompt_tokens,
                                                "completion_tokens": completion_tokens,
                                                "total_tokens": total_tokens,
                                                "prompt_tokens_details": {"cached_tokens": 0}
                                            });
                                            Some((
                                                Ok(format!(
                                                    "data: {}\n\ndata: [DONE]\n\n",
                                                    serde_json::to_string(&final_json).unwrap()
                                                )),
                                                (event_stream, is_done),
                                            ))
                                        } else {
                                            let final_chunk = create_stream_chunk(
                                                &id,
                                                created,
                                                &model,
                                                "",
                                                Some(finish_reason),
                                            );
                                            let final_chunk_json =
                                                serde_json::to_string(&final_chunk).unwrap();
                                            Some((
                                                Ok(format!(
                                                    "data: {}\n\ndata: [DONE]\n\n",
                                                    final_chunk_json
                                                )),
                                                (event_stream, is_done),
                                            ))
                                        }
                                    }
                                    _ => {
                                        debug!("⏭️ 忽略其他事件類型");
                                        Some((Ok(String::new()), (event_stream, is_done)))
                                    }
                                },
                                _ => None,
                            }
                        }
                    },
                ))
            )
        };

        res.stream(processed_stream);
    }

    let duration = start_time.elapsed();
    info!(
        "✅ 串流響應處理完成 | ID: {} | 耗時: {}",
        id_for_log,
        format_duration(duration)
    );
}

async fn handle_non_stream_response(
    res: &mut Response,
    mut event_stream: Pin<Box<dyn Stream<Item = Result<ChatResponse, PoeError>> + Send>>,
    model: &str,
    include_usage: bool,
    prompt_tokens: u32,
    completion_tokens_counter: Arc<AtomicU32>,
) {
    let start_time = Instant::now();
    let id = nanoid!(10);
    info!(
        "📦 開始處理非串流響應 | ID: {} | 模型: {} | 包含使用統計: {}",
        id, model, include_usage
    );
    let mut replace_response = false;
    let mut full_content = String::new();
    let mut first_three_events = Vec::new();
    let mut accumulated_tool_calls: Vec<poe_api_process::types::ChatToolCall> = Vec::new();
    let mut file_refs = HashMap::new();
    let mut has_done_event = false;
    debug!("🔍 檢查初始事件");
    for _ in 0..3 {
        // 增加檢查事件數量，確保能捕獲到 file 事件
        if let Some(Ok(event)) = event_stream.next().await {
            debug!("📥 收到初始事件: {:?}", event.event);
            // 特別處理：如果是 Done 事件，標記但不消耗它
            if event.event == ChatEventType::Done {
                has_done_event = true;
                debug!("🔍 檢測到 Done 事件，但不消耗它");
                continue; // 跳過這個事件，不添加到 first_three_events
            }
            first_three_events.push(event);
        }
    }
    for event in first_three_events {
        match event.event {
            ChatEventType::ReplaceResponse => {
                debug!("🔄 檢測到 ReplaceResponse 模式");
                replace_response = true;
                if let Some(ChatResponseData::Text { text }) = event.data {
                    let text_clone = text.clone();
                    full_content = text_clone.clone();
                }
            }
            ChatEventType::Text => {
                if let Some(ChatResponseData::Text { text }) = event.data {
                    if !replace_response {
                        full_content.push_str(&text);
                    }
                }
            }
            ChatEventType::File => {
                if let Some(ChatResponseData::File(file_data)) = event.data {
                    debug!(
                        "🖼️ 收到檔案事件 | 名稱: {} | URL: {}",
                        file_data.name, file_data.url
                    );
                    file_refs.insert(file_data.inline_ref.clone(), file_data);
                }
            }
            ChatEventType::Json => {
                debug!("📝 收到 JSON 事件");
                // 檢查是否包含工具調用
                if let Some(ChatResponseData::ToolCalls(tool_calls)) = event.data {
                    debug!("🔧 收到工具調用，數量: {}", tool_calls.len());
                    accumulated_tool_calls.extend(tool_calls);
                }
            }
            ChatEventType::Error => {
                if let Some(ChatResponseData::Error { text, allow_retry }) = event.data {
                    error!("❌ 處理錯誤: {}", text);
                    let (status, error_response) = convert_poe_error_to_openai(&text, allow_retry);
                    res.status_code(status);
                    res.render(Json(error_response));
                    return;
                }
            }
            ChatEventType::Done => {
                debug!("✅ 初始事件處理完成");
                has_done_event = true;
            }
        }
    }
    // 處理圖片引用，替換內容中的引用標記為實際URL
    for (ref_id, file_data) in &file_refs {
        let img_marker = format!("[{}]", ref_id);
        let replacement = format!("({})", file_data.url);
        full_content = full_content.replace(&img_marker, &replacement);
        debug!("🖼️ 替換圖片引用 | ID: {} | URL: {}", ref_id, file_data.url);
    }
    if replace_response {
        debug!("🔄 使用 ReplaceResponse 處理模式 (非串流)");
        // 將初始內容傳遞給 handle_replace_response
        let initial_content_for_handler = full_content.clone();
        let content = handle_replace_response(
            event_stream,
            initial_content_for_handler,
            file_refs,
            Arc::clone(&completion_tokens_counter),
            include_usage,
            has_done_event, // 傳遞是否已經檢測到 Done 事件
        )
        .await;
        debug!(
            "📤 ReplaceResponse 最終內容長度 (非串流): {}",
            format_bytes_length(content.len())
        );
        let completion_tokens = if include_usage {
            completion_tokens_counter.load(Ordering::SeqCst)
        } else {
            0
        };
        let total_tokens = prompt_tokens + completion_tokens;
        if include_usage {
            debug!(
                "📊 Token 使用統計 | prompt_tokens: {} | completion_tokens: {} | total_tokens: {}",
                prompt_tokens, completion_tokens, total_tokens
            );
        }
        // 在 ReplaceResponse 模式下，不處理工具調用
        let mut response = ChatCompletionResponse {
            id: format!("chatcmpl-{}", nanoid!(10)),
            object: "chat.completion".to_string(),
            created: Utc::now().timestamp(),
            model: model.to_string(),
            choices: vec![CompletionChoice {
                index: 0,
                message: CompletionMessage {
                    role: "assistant".to_string(),
                    content,
                    refusal: None,
                    tool_calls: None,
                },
                logprobs: None,
                finish_reason: Some("stop".to_string()),
            }],
            usage: None,
        };
        if include_usage {
            response.usage = Some(serde_json::json!({
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": total_tokens,
                "prompt_tokens_details": {"cached_tokens": 0}
            }));
        }
        res.render(Json(response));
    } else {
        debug!("🔄 使用標準非串流處理模式");
        let mut response_content = full_content;
        let mut response_file_refs = file_refs;
        while let Some(Ok(event)) = event_stream.next().await {
            match event.event {
                ChatEventType::Text => {
                    if let Some(ChatResponseData::Text { text }) = event.data {
                        response_content.push_str(&text);
                    }
                }
                ChatEventType::File => {
                    if let Some(ChatResponseData::File(file_data)) = event.data {
                        debug!(
                            "🖼️ 收到後續檔案事件 | 名稱: {} | URL: {}",
                            file_data.name, file_data.url
                        );
                        response_file_refs.insert(file_data.inline_ref.clone(), file_data);
                    }
                }
                ChatEventType::Json => {
                    // 檢查是否包含工具調用
                    if let Some(ChatResponseData::ToolCalls(tool_calls)) = event.data {
                        debug!("🔧 處理工具調用，數量: {}", tool_calls.len());
                        accumulated_tool_calls.extend(tool_calls);
                    }
                }
                ChatEventType::Error => {
                    if let Some(ChatResponseData::Error { text, allow_retry }) = event.data {
                        error!("❌ 處理錯誤: {}", text);
                        let (status, error_response) =
                            convert_poe_error_to_openai(&text, allow_retry);
                        res.status_code(status);
                        res.render(Json(error_response));
                        return;
                    }
                }
                ChatEventType::Done => {
                    debug!("✅ 回應收集完成");
                    break;
                }
                _ => {
                    debug!("⏭️ 忽略其他事件類型");
                }
            }
        }
        // 完成所有事件處理後，處理圖片引用
        for (ref_id, file_data) in &response_file_refs {
            let img_marker = format!("[{}]", ref_id);
            let replacement = format!("({})", file_data.url);
            response_content = response_content.replace(&img_marker, &replacement);
            debug!(
                "🖼️ 替換後續圖片引用 | ID: {} | URL: {}",
                ref_id, file_data.url
            );
        }
        let completion_tokens = if include_usage {
            let tokens = count_completion_tokens(&response_content);
            completion_tokens_counter.store(tokens, Ordering::SeqCst);
            tokens
        } else {
            0
        };
        let total_tokens = prompt_tokens + completion_tokens;
        // 確定 finish_reason
        let finish_reason = if !accumulated_tool_calls.is_empty() {
            "tool_calls".to_string()
        } else {
            "stop".to_string()
        };
        debug!(
            "📤 準備發送回應 | 內容長度: {} | 工具調用數量: {} | 完成原因: {}",
            format_bytes_length(response_content.len()),
            accumulated_tool_calls.len(),
            finish_reason
        );
        if include_usage {
            debug!(
                "📊 Token 使用統計 | prompt_tokens: {} | completion_tokens: {} | total_tokens: {}",
                prompt_tokens, completion_tokens, total_tokens
            );
        }
        // 創建響應
        let mut response = ChatCompletionResponse {
            id: format!("chatcmpl-{}", id),
            object: "chat.completion".to_string(),
            created: Utc::now().timestamp(),
            model: model.to_string(),
            choices: vec![CompletionChoice {
                index: 0,
                message: CompletionMessage {
                    role: "assistant".to_string(),
                    content: response_content,
                    refusal: None,
                    tool_calls: if accumulated_tool_calls.is_empty() {
                        None
                    } else {
                        Some(accumulated_tool_calls)
                    },
                },
                logprobs: None,
                finish_reason: Some(finish_reason),
            }],
            usage: None,
        };
        if include_usage {
            response.usage = Some(serde_json::json!({
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": total_tokens,
                "prompt_tokens_details": {"cached_tokens": 0}
            }));
        }
        res.render(Json(response));
    }
    let duration = start_time.elapsed();
    info!(
        "✅ 非串流響應處理完成 | ID: {} | 耗時: {}",
        id,
        format_duration(duration)
    );
}

async fn handle_replace_response(
    mut event_stream: Pin<Box<dyn Stream<Item = Result<ChatResponse, PoeError>> + Send>>,
    initial_content: String,
    initial_file_refs: HashMap<String, poe_api_process::types::FileData>,
    completion_tokens_counter: Arc<AtomicU32>,
    include_usage: bool,
    already_has_done_event: bool,
) -> String {
    let start_time = Instant::now();
    debug!(
        "🔄 開始處理 ReplaceResponse 帶檔案 | 初始內容長度: {} | 初始檔案數: {} | 已檢測到 Done 事件: {}",
        format_bytes_length(initial_content.len()),
        initial_file_refs.len(),
        already_has_done_event
    );

    // 使用 Arc + Mutex 來安全地共享狀態
    let last_content = Arc::new(Mutex::new(initial_content));
    let file_refs = Arc::new(Mutex::new(initial_file_refs));
    let done_received = Arc::new(AtomicBool::new(already_has_done_event));
    let first_text_processed = Arc::new(AtomicBool::new(false));

    // 如果已經收到了 Done 事件，直接處理最終內容
    if already_has_done_event {
        debug!("🏁 已經檢測到 Done 事件，跳過事件流處理");
    } else {
        let last_content_clone = Arc::clone(&last_content);
        let file_refs_clone = Arc::clone(&file_refs);
        let done_received_clone = Arc::clone(&done_received);
        let first_text_processed_clone = Arc::clone(&first_text_processed);

        // 創建一個通道，用於通知主任務背景處理已完成
        let (tx, rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            debug!("🏃 啟動背景事件收集任務");
            let mut tx_opt = Some(tx); // 將 tx 放入 Option 中，以便多次處理

            while let Some(result) = event_stream.next().await {
                match result {
                    Ok(event) => {
                        debug!("📥 處理事件: {:?}", event.event);
                        match event.event {
                            ChatEventType::ReplaceResponse => {
                                if let Some(ChatResponseData::Text { text }) = event.data {
                                    debug!(
                                        "📝 更新替換內容 | 長度: {}",
                                        format_bytes_length(text.len())
                                    );
                                    *last_content_clone.lock().unwrap() = text;
                                }
                            }
                            ChatEventType::Text => {
                                // 檢查是否為第一次的 Text 事件
                                let is_first_text =
                                    !first_text_processed_clone.load(Ordering::SeqCst);

                                if let Some(ChatResponseData::Text { text }) = event.data {
                                    if is_first_text {
                                        debug!(
                                            "📝 合併第一個 Text 事件與 ReplaceResponse | Text 長度: {}",
                                            format_bytes_length(text.len())
                                        );
                                        // 將第一個 Text 事件的內容合併到 ReplaceResponse 中
                                        let mut content_guard = last_content_clone.lock().unwrap();
                                        content_guard.push_str(&text);
                                        first_text_processed_clone.store(true, Ordering::SeqCst);
                                    } else {
                                        // 對於後續 Text 事件，附加到最後的內容
                                        debug!(
                                            "📝 附加後續 Text 事件 | 長度: {}",
                                            format_bytes_length(text.len())
                                        );
                                        let mut content_guard = last_content_clone.lock().unwrap();
                                        content_guard.push_str(&text);
                                    }
                                }
                            }
                            ChatEventType::File => {
                                if let Some(ChatResponseData::File(file_data)) = event.data {
                                    debug!(
                                        "🖼️ 收到檔案事件 | 名稱: {} | URL: {} | 引用ID: {}",
                                        file_data.name, file_data.url, file_data.inline_ref
                                    );
                                    file_refs_clone
                                        .lock()
                                        .unwrap()
                                        .insert(file_data.inline_ref.clone(), file_data);
                                }
                            }
                            ChatEventType::Done => {
                                debug!("✅ 背景任務收到完成信號");
                                done_received_clone.store(true, Ordering::SeqCst);
                                // 在收到 Done 事件後等待一小段時間，確保所有事件都被處理
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                // 通知主任務背景處理已完成，取出 tx 並發送
                                if let Some(sender) = tx_opt.take() {
                                    let _ = sender.send(());
                                }
                                break;
                            }
                            _ => {
                                debug!("⏭️ 忽略其他事件類型");
                            }
                        }
                    }
                    Err(e) => {
                        error!("❌ 事件處理錯誤: {}", e);
                        break;
                    }
                }
            }

            // 如果循環結束但未收到 Done 事件，也要通知主任務
            if !done_received_clone.load(Ordering::SeqCst) {
                debug!("⚠️ 事件流結束但未收到完成信號");
                // 使用 take 來獲取並消耗發送者，避免所有權問題
                if let Some(sender) = tx_opt.take() {
                    let _ = sender.send(());
                }
            }
            debug!("👋 背景任務結束");
        });

        // 等待背景任務處理完成
        match rx.await {
            Ok(_) => debug!("✅ 收到背景任務完成通知"),
            Err(e) => error!("❌ 等待背景任務完成時出錯: {}", e),
        }
    }

    // 處理最終內容
    let final_content = {
        let replace_content = last_content.lock().unwrap().clone();
        let file_refs_map = file_refs.lock().unwrap();

        // 處理圖片引用
        let mut processed_content = replace_content.clone();
        for (ref_id, file_data) in file_refs_map.iter() {
            let img_marker = format!("[{}]", ref_id);
            let replacement = format!("({})", file_data.url);
            processed_content = processed_content.replace(&img_marker, &replacement);
            debug!(
                "🖼️ 處理圖片引用 | 標記: {} | 替換為: {}",
                img_marker, replacement
            );
        }

        // 檢查是否有圖片引用被替換
        if processed_content != replace_content {
            debug!(
                "✅ 成功替換圖片引用 | 最終內容長度: {}",
                format_bytes_length(processed_content.len())
            );
        } else if !file_refs_map.is_empty() {
            warn!(
                "⚠️ 有圖片引用但未找到對應標記 | 圖片數: {}",
                file_refs_map.len()
            );
        }

        // 計算 tokens（如果需要）
        if include_usage {
            let tokens = count_completion_tokens(&processed_content);
            completion_tokens_counter.store(tokens, Ordering::SeqCst);
            debug!("📊 計算 completion_tokens: {}", tokens);
        }

        // 額外的日誌，確保最終內容被記錄
        debug!("📤 最終處理結果: {}", processed_content);
        processed_content
    };

    let duration = start_time.elapsed();
    debug!(
        "✅ ReplaceResponse 處理完成 | 最終內容長度: {} | 耗時: {}",
        format_bytes_length(final_content.len()),
        format_duration(duration)
    );

    final_content
}

// 更新後的 handle_tool_calls_in_stream 函數，加入工具調用參數
async fn handle_tool_calls_in_stream(
    id: String,
    created: i64,
    model: String,
    event_stream: Pin<Box<dyn Stream<Item = Result<ChatResponse, PoeError>> + Send>>,
    initial_content: String,
    initial_tool_calls: Vec<poe_api_process::types::ChatToolCall>,
) -> impl Stream<Item = Result<String, std::convert::Infallible>> + Send {
    debug!("🔧 處理帶有工具調用的流式響應");

    // 克隆初始工具調用，用於稍後使用
    let initial_tool_calls_for_role = initial_tool_calls.clone();

    // 先發送初始的 role delta
    let role_delta = Delta {
        role: Some("assistant".to_string()),
        content: None,
        refusal: None,
        tool_calls: None,
    };

    let role_chunk = ChatCompletionChunk {
        id: format!("chatcmpl-{}", id),
        object: "chat.completion.chunk".to_string(),
        created,
        model: model.clone(),
        choices: vec![Choice {
            index: 0,
            delta: role_delta,
            finish_reason: None,
        }],
    };

    let role_json = serde_json::to_string(&role_chunk).unwrap();
    let role_message = format!("data: {}\n\n", role_json);

    // 發送初始工具調用
    let tool_message = if !initial_tool_calls.is_empty() {
        debug!("🔧 發送初始工具調用，數量: {}", initial_tool_calls.len());
        let tool_delta = Delta {
            role: None,
            content: None,
            refusal: None,
            tool_calls: Some(initial_tool_calls.clone()),
        };

        let tool_chunk = ChatCompletionChunk {
            id: format!("chatcmpl-{}", id),
            object: "chat.completion.chunk".to_string(),
            created,
            model: model.clone(),
            choices: vec![Choice {
                index: 0,
                delta: tool_delta,
                finish_reason: Some("tool_calls".to_string()),
            }],
        };

        let tool_json = serde_json::to_string(&tool_chunk).unwrap();
        format!("data: {}\n\n", tool_json)
    } else {
        String::new()
    };

    // 如果有初始內容，發送
    let content_message = if !initial_content.is_empty() {
        let content_delta = Delta {
            role: None,
            content: Some(initial_content.clone()),
            refusal: None,
            tool_calls: None,
        };

        let content_chunk = ChatCompletionChunk {
            id: format!("chatcmpl-{}", id),
            object: "chat.completion.chunk".to_string(),
            created,
            model: model.clone(),
            choices: vec![Choice {
                index: 0,
                delta: content_delta,
                finish_reason: None,
            }],
        };

        let content_json = serde_json::to_string(&content_chunk).unwrap();
        format!("data: {}\n\n", content_json)
    } else {
        String::new()
    };

    let initial_tool_calls_for_closure = initial_tool_calls_for_role.clone();

    // 創建用於處理事件的 unfold stream
    let event_processor = stream::unfold(
        (event_stream, false, Vec::new()), // 增加了一個 Vec 來收集工具調用
        move |(mut event_stream, mut is_done, mut tool_calls)| {
            let id_clone = id.clone();
            let model_clone = model.clone();
            let initial_tool_calls_clone = initial_tool_calls_for_closure.clone(); // 为async块克隆一次

            async move {
                if is_done {
                    return None;
                }

                match event_stream.next().await {
                    Some(Ok(event)) => match event.event {
                        ChatEventType::Text => {
                            if let Some(ChatResponseData::Text { text }) = event.data {
                                // 發送文本 delta
                                let text_delta = Delta {
                                    role: None,
                                    content: Some(text),
                                    refusal: None,
                                    tool_calls: None,
                                };

                                let text_chunk = ChatCompletionChunk {
                                    id: format!("chatcmpl-{}", id_clone),
                                    object: "chat.completion.chunk".to_string(),
                                    created,
                                    model: model_clone.to_string(),
                                    choices: vec![Choice {
                                        index: 0,
                                        delta: text_delta,
                                        finish_reason: None,
                                    }],
                                };

                                let text_json = serde_json::to_string(&text_chunk).unwrap();
                                Some((
                                    Ok(format!("data: {}\n\n", text_json)),
                                    (event_stream, is_done, tool_calls),
                                ))
                            } else {
                                Some((Ok(String::new()), (event_stream, is_done, tool_calls)))
                            }
                        }
                        ChatEventType::Json => {
                            if let Some(ChatResponseData::ToolCalls(new_tool_calls)) = event.data {
                                // 收集工具調用
                                tool_calls.extend(new_tool_calls);
                                Some((Ok(String::new()), (event_stream, is_done, tool_calls)))
                            } else {
                                Some((Ok(String::new()), (event_stream, is_done, tool_calls)))
                            }
                        }
                        ChatEventType::Done => {
                            is_done = true;

                            // 發送收集的工具調用
                            if !tool_calls.is_empty() {
                                let tool_delta = Delta {
                                    role: None,
                                    content: None,
                                    refusal: None,
                                    tool_calls: Some(tool_calls.clone()),
                                };

                                let tool_chunk = ChatCompletionChunk {
                                    id: format!("chatcmpl-{}", id_clone),
                                    object: "chat.completion.chunk".to_string(),
                                    created,
                                    model: model_clone.to_string(),
                                    choices: vec![Choice {
                                        index: 0,
                                        delta: tool_delta,
                                        finish_reason: Some("tool_calls".to_string()),
                                    }],
                                };

                                let tool_json = serde_json::to_string(&tool_chunk).unwrap();

                                // 發送最終 chunk
                                let final_message = format!("data: {}\n\n", tool_json);
                                Some((Ok(final_message), (event_stream, is_done, Vec::new())))
                            } else {
                                // 沒有工具調用，發送普通的完成信息
                                let finish_reason = if !initial_tool_calls_clone.is_empty() {
                                    "tool_calls"
                                } else {
                                    "stop"
                                };

                                let final_chunk = create_stream_chunk(
                                    &id_clone,
                                    created,
                                    &model_clone,
                                    "",
                                    Some(finish_reason.to_string()),
                                );

                                let final_json = serde_json::to_string(&final_chunk).unwrap();
                                let final_message = format!("data: {}\n\n", final_json);
                                Some((Ok(final_message), (event_stream, is_done, Vec::new())))
                            }
                        }
                        _ => Some((Ok(String::new()), (event_stream, is_done, tool_calls))),
                    },
                    Some(Err(_)) => {
                        // 處理錯誤
                        is_done = true;
                        let error_message =
                            "data: {\"error\": \"處理事件時發生錯誤\"}\n\ndata: [DONE]\n\n";
                        Some((
                            Ok(error_message.to_string()),
                            (event_stream, is_done, Vec::new()),
                        ))
                    }
                    None => None,
                }
            }
        },
    );

    // 創建組合後的流
    let first_messages = role_message + &tool_message + &content_message;
    let first_part = stream::once(future::ready(Ok::<_, std::convert::Infallible>(
        first_messages,
    )));

    let second_part: Pin<Box<dyn Stream<Item = Result<String, std::convert::Infallible>> + Send>> =
        Box::pin(event_processor);

    let done_part = stream::once(future::ready(Ok("data: [DONE]\n\n".to_string())));

    Box::pin(first_part.chain(second_part).chain(done_part))
}

fn create_stream_chunk(
    id: &str,
    created: i64,
    model: &str,
    content: &str,
    finish_reason: Option<String>,
) -> ChatCompletionChunk {
    let mut delta = Delta {
        role: None,
        content: None,
        refusal: None,
        tool_calls: None,
    };
    if content.is_empty() && finish_reason.is_none() {
        delta.role = Some("assistant".to_string());
    } else {
        delta.content = Some(content.to_string());
    }
    debug!(
        "🔧 創建串流片段 | ID: {} | 內容長度: {}",
        id,
        if let Some(content) = &delta.content {
            format_bytes_length(content.len())
        } else {
            "0 B".to_string()
        }
    );
    ChatCompletionChunk {
        id: format!("chatcmpl-{}", id),
        object: "chat.completion.chunk".to_string(),
        created,
        model: model.to_string(),
        choices: vec![Choice {
            index: 0,
            delta,
            finish_reason,
        }],
    }
}
