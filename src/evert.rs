use crate::types::*;
use crate::utils::{convert_poe_error_to_openai, format_bytes_length};
use poe_api_process::{ChatEventType, ChatResponse, ChatResponseData};
use salvo::prelude::*;
use std::collections::HashMap;
use tracing::{debug, error};

// 事件積累上下文，用於收集處理事件期間的狀態
#[derive(Debug, Clone, Default)]
pub struct EventContext {
    pub content: String,
    pub replace_buffer: Option<String>,
    pub file_refs: HashMap<String, poe_api_process::types::FileData>,
    pub tool_calls: Vec<poe_api_process::types::ChatToolCall>,
    is_replace_mode: bool,
    pub error: Option<(StatusCode, OpenAIErrorResponse)>,
    pub done: bool,
    pub completion_tokens: u32,
    first_text_processed: bool,
    pub role_chunk_sent: bool,
    has_new_file_refs: bool,
    pub image_urls_sent: bool,
}

// 事件處理器 trait
trait EventHandler {
    fn handle(&self, event: &ChatResponse, ctx: &mut EventContext) -> Option<String>;
}

// Text 事件處理器
#[derive(Clone)]
struct TextEventHandler;
impl EventHandler for TextEventHandler {
    fn handle(&self, event: &ChatResponse, ctx: &mut EventContext) -> Option<String> {
        if let Some(ChatResponseData::Text { text }) = &event.data {
            debug!(
                "📝 處理文本事件 | 長度: {} | is_replace_mode: {} | first_text_processed: {}",
                format_bytes_length(text.len()),
                ctx.is_replace_mode,
                ctx.first_text_processed
            );

            // 如果是替換模式且第一個文本未處理，需要合併替換緩衝區與新文本
            if ctx.is_replace_mode && !ctx.first_text_processed {
                debug!("📝 合併第一個 Text 事件與 ReplaceResponse");
                if let Some(replace_content) = &mut ctx.replace_buffer {
                    replace_content.push_str(text);
                    ctx.first_text_processed = true;
                    // 返回合併後的內容以發送合併片段
                    return Some(replace_content.clone());
                } else {
                    // 沒有 replace_buffer，直接添加到 content
                    ctx.content.push_str(text);
                    return Some(text.clone());
                }
            }
            // 如果是替換模式且第一個文本已處理，則重置為非替換模式
            else if ctx.is_replace_mode && ctx.first_text_processed {
                debug!("🔄 重置替換模式，轉為直接文本模式");
                ctx.is_replace_mode = false;
                ctx.first_text_processed = false;

                // 將 replace_buffer 的內容移至 content
                if let Some(replace_content) = ctx.replace_buffer.take() {
                    ctx.content = replace_content;
                }
                // 直接將新文本添加到 content
                ctx.content.push_str(text);
                return Some(text.clone());
            } else {
                // 非 replace 模式，直接累積並返回文本
                ctx.content.push_str(text);
                return Some(text.clone());
            }
        }
        None
    }
}

// File 事件處理器
#[derive(Clone)]
struct FileEventHandler;
impl EventHandler for FileEventHandler {
    fn handle(&self, event: &ChatResponse, ctx: &mut EventContext) -> Option<String> {
        if let Some(ChatResponseData::File(file_data)) = &event.data {
            debug!(
                "🖼️  處理檔案事件 | 名稱: {} | URL: {}",
                file_data.name, file_data.url
            );
            ctx.file_refs
                .insert(file_data.inline_ref.clone(), file_data.clone());
            ctx.has_new_file_refs = true;

            // 如果此時有 replace_buffer，處理它並發送
            if !ctx.image_urls_sent && ctx.replace_buffer.is_some() {
                // 只處理未發送過的
                let content = ctx.replace_buffer.as_ref().unwrap();
                if content.contains(&format!("[{}]", file_data.inline_ref)) {
                    debug!(
                        "🖼️ 檢測到 ReplaceResponse 包含圖片引用 [{}]，立即處理",
                        file_data.inline_ref
                    );
                    // 處理這個文本中的圖片引用
                    let mut processed = content.clone();
                    let img_marker = format!("[{}]", file_data.inline_ref);
                    let replacement = format!("({})", file_data.url);
                    processed = processed.replace(&img_marker, &replacement);
                    ctx.image_urls_sent = true; // 標記已發送
                    return Some(processed);
                }
            }
        }
        None
    }
}

// ReplaceResponse 事件處理器
#[derive(Clone)]
struct ReplaceResponseEventHandler;
impl EventHandler for ReplaceResponseEventHandler {
    fn handle(&self, event: &ChatResponse, ctx: &mut EventContext) -> Option<String> {
        if let Some(ChatResponseData::Text { text }) = &event.data {
            debug!(
                "🔄 處理 ReplaceResponse 事件 | 長度: {}",
                format_bytes_length(text.len())
            );
            ctx.is_replace_mode = true;
            ctx.replace_buffer = Some(text.clone());
            ctx.first_text_processed = false;

            // 檢查是否有文件引用需要處理
            if !ctx.file_refs.is_empty() && text.contains('[') {
                debug!("🔄 ReplaceResponse 可能包含圖片引用，檢查並處理");
                // 處理這個文本中的圖片引用
                let mut processed = text.clone();
                let mut has_refs = false;

                for (ref_id, file_data) in &ctx.file_refs {
                    let img_marker = format!("[{}]", ref_id);
                    if processed.contains(&img_marker) {
                        let replacement = format!("({})", file_data.url);
                        processed = processed.replace(&img_marker, &replacement);
                        has_refs = true;
                        debug!("🖼️  替換圖片引用 | ID: {} | URL: {}", ref_id, file_data.url);
                    }
                }

                if has_refs {
                    // 如果確實包含了圖片引用，立即返回處理後的內容
                    debug!("✅ ReplaceResponse 含有圖片引用，立即發送處理後內容");
                    ctx.image_urls_sent = true; // 標記已發送
                    return Some(processed);
                }
            }

            // 推遲 ReplaceResponse 的輸出，等待後續 Text 事件
            debug!("🔄 推遲 ReplaceResponse 的輸出，等待後續 Text 事件");
        }
        None // 不直接發送，等待與 Text 合併
    }
}

// Json 事件處理器 (用於 Tool Calls)
#[derive(Clone)]
struct JsonEventHandler;
impl EventHandler for JsonEventHandler {
    fn handle(&self, event: &ChatResponse, ctx: &mut EventContext) -> Option<String> {
        debug!("📝 處理 JSON 事件");
        if let Some(ChatResponseData::ToolCalls(tool_calls)) = &event.data {
            debug!("🔧 處理工具調用，數量: {}", tool_calls.len());
            ctx.tool_calls.extend(tool_calls.clone());
            // 返回 Some，表示需要發送工具調用
            return Some("tool_calls".to_string());
        }
        None
    }
}

// Error 事件處理器
#[derive(Clone)]
struct ErrorEventHandler;
impl EventHandler for ErrorEventHandler {
    fn handle(&self, event: &ChatResponse, ctx: &mut EventContext) -> Option<String> {
        if let Some(ChatResponseData::Error { text, allow_retry }) = &event.data {
            error!("❌ 處理錯誤事件: {}", text);
            let (status, error_response) = convert_poe_error_to_openai(text, *allow_retry);
            ctx.error = Some((status, error_response));
            return Some("error".to_string());
        }
        None
    }
}

// Done 事件處理器
#[derive(Clone)]
struct DoneEventHandler;
impl EventHandler for DoneEventHandler {
    fn handle(&self, _event: &ChatResponse, ctx: &mut EventContext) -> Option<String> {
        debug!("✅ 處理 Done 事件");
        ctx.done = true;

        // 只有當未發送過圖片URL時才處理
        if !ctx.image_urls_sent && ctx.replace_buffer.is_some() && !ctx.file_refs.is_empty() {
            let content = ctx.replace_buffer.as_ref().unwrap();
            debug!("🔍 檢查完成事件時是否有未處理的圖片引用");
            let mut processed = content.clone();
            let mut has_refs = false;

            for (ref_id, file_data) in &ctx.file_refs {
                let img_marker = format!("[{}]", ref_id);
                if processed.contains(&img_marker) {
                    let replacement = format!("({})", file_data.url);
                    processed = processed.replace(&img_marker, &replacement);
                    has_refs = true;
                    debug!(
                        "🖼️ 完成前替換圖片引用 | ID: {} | URL: {}",
                        ref_id, file_data.url
                    );
                }
            }

            if has_refs {
                debug!("✅ 完成前處理了圖片引用");
                ctx.image_urls_sent = true; // 標記已發送
                return Some(processed);
            }
        }

        Some("done".to_string())
    }
}

// 事件處理器管理器
#[derive(Clone)]
pub struct EventHandlerManager {
    text_handler: TextEventHandler,
    file_handler: FileEventHandler,
    replace_handler: ReplaceResponseEventHandler,
    json_handler: JsonEventHandler,
    error_handler: ErrorEventHandler,
    done_handler: DoneEventHandler,
}

impl EventHandlerManager {
    pub fn new() -> Self {
        Self {
            text_handler: TextEventHandler,
            file_handler: FileEventHandler,
            replace_handler: ReplaceResponseEventHandler,
            json_handler: JsonEventHandler,
            error_handler: ErrorEventHandler,
            done_handler: DoneEventHandler,
        }
    }

    pub fn handle(&self, event: &ChatResponse, ctx: &mut EventContext) -> Option<String> {
        match event.event {
            ChatEventType::Text => self.text_handler.handle(event, ctx),
            ChatEventType::File => self.file_handler.handle(event, ctx),
            ChatEventType::ReplaceResponse => self.replace_handler.handle(event, ctx),
            ChatEventType::Json => self.json_handler.handle(event, ctx),
            ChatEventType::Error => self.error_handler.handle(event, ctx),
            ChatEventType::Done => self.done_handler.handle(event, ctx),
        }
    }
}
