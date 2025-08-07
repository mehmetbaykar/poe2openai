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
    // 思考相關欄位
    pub reasoning_content: String,
    pub in_thinking_mode: bool,
    pub thinking_started: bool,
    pub current_reasoning_line: String,
    pub pending_text: String,
    pub metadata: HashMap<String, usize>, // 用於追蹤已發送的內容長度
}

impl EventContext {
    pub fn get(&self, key: &str) -> Option<usize> {
        self.metadata.get(key).copied()
    }

    pub fn insert(&mut self, key: &str, value: usize) {
        self.metadata.insert(key.to_string(), value);
    }
}

// 事件處理器 trait
trait EventHandler {
    fn handle(&self, event: &ChatResponse, ctx: &mut EventContext) -> Option<String>;
}

// 思考內容處理器
#[derive(Clone)]
pub struct ThinkingProcessor;

impl ThinkingProcessor {
    // 檢測思考開始標記
    fn detect_thinking_start(text: &str) -> Option<usize> {
        if let Some(pos) = text.find("*Thinking...*") {
            return Some(pos);
        }
        if let Some(pos) = text.find("Thinking...") {
            return Some(pos);
        }
        None
    }

    // 處理文本並分離思考內容和普通內容
    // 返回 (reasoning_chunk, content_chunk)
    pub fn process_text_chunk(
        ctx: &mut EventContext,
        new_text: &str,
    ) -> (Option<String>, Option<String>) {
        ctx.pending_text.push_str(new_text);

        let mut reasoning_output = None;
        let mut content_output = None;

        // 如果還沒開始思考模式，檢測是否有思考標記
        if !ctx.thinking_started {
            if let Some(thinking_pos) = Self::detect_thinking_start(&ctx.pending_text) {
                debug!("🧠 思考模式開始");
                ctx.thinking_started = true;
                ctx.in_thinking_mode = true;

                // 分離思考標記前後的內容
                let (before_thinking, after_thinking) = ctx.pending_text.split_at(thinking_pos);

                // 思考標記前的內容作為普通內容
                if !before_thinking.trim().is_empty() {
                    ctx.content.push_str(before_thinking);
                    content_output = Some(before_thinking.to_string());
                }

                // 確定標記類型並移除完整標記
                let after_marker = if after_thinking.starts_with("*Thinking...*") {
                    after_thinking.strip_prefix("*Thinking...*").unwrap_or("")
                } else if after_thinking.starts_with("Thinking...") {
                    after_thinking.strip_prefix("Thinking...").unwrap_or("")
                } else {
                    after_thinking
                };

                ctx.pending_text = after_marker.to_string();
            } else {
                // 沒有思考標記，作為普通內容處理
                if !ctx.pending_text.trim().is_empty() {
                    ctx.content.push_str(&ctx.pending_text);
                    content_output = Some(ctx.pending_text.clone());
                    ctx.pending_text.clear();
                }
                return (None, content_output);
            }
        }

        // 思考模式下處理內容
        if ctx.thinking_started && ctx.in_thinking_mode {
            let (reasoning_chunk, remaining_text, thinking_ended) =
                Self::process_thinking_content(ctx);

            if let Some(reasoning_content) = reasoning_chunk {
                reasoning_output = Some(reasoning_content);
            }

            ctx.pending_text = remaining_text;

            // 如果思考結束，處理剩餘內容作為普通內容
            if thinking_ended {
                debug!("🧠 思考模式結束");
                ctx.in_thinking_mode = false;
                if !ctx.pending_text.trim().is_empty() {
                    ctx.content.push_str(&ctx.pending_text);
                    // 如果已經有 content_output，合併內容
                    if let Some(existing_content) = content_output {
                        content_output = Some(format!("{}{}", existing_content, ctx.pending_text));
                    } else {
                        content_output = Some(ctx.pending_text.clone());
                    }
                    ctx.pending_text.clear();
                }
            }
        } else if ctx.thinking_started
            && !ctx.in_thinking_mode
            && !ctx.pending_text.trim().is_empty()
        {
            ctx.content.push_str(&ctx.pending_text);
            content_output = Some(ctx.pending_text.clone());
            ctx.pending_text.clear();
        }

        (reasoning_output, content_output)
    }

    // 處理思考模式下的內容
    // 返回 (reasoning_chunk, remaining_text, thinking_ended)
    fn process_thinking_content(ctx: &mut EventContext) -> (Option<String>, String, bool) {
        let mut reasoning_chunks = Vec::new();
        let mut thinking_ended = false;

        // 按行處理，但需要考慮串流中的不完整行
        let lines: Vec<&str> = ctx.pending_text.lines().collect();
        let mut processed_lines = 0;

        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();

            if trimmed.starts_with("> ") || trimmed == ">" {
                // 思考內容行（包括空的 "> " 行）
                let thinking_content = if trimmed == ">" {
                    "" // 空的思考行
                } else {
                    trimmed.strip_prefix("> ").unwrap_or(trimmed)
                };

                // 檢查是否是完整的行（在串流中可能不完整）
                if i == lines.len() - 1 && !ctx.pending_text.ends_with('\n') {
                    // 最後一行且沒有換行符，可能不完整
                    ctx.current_reasoning_line = thinking_content.to_string();
                    break;
                } else {
                    // 完整的思考行
                    let mut full_line = thinking_content.to_string();

                    // 檢查後續行是否屬於同一段思考（沒有真正的換行分隔）
                    let mut j = i + 1;
                    while j < lines.len() {
                        let next_line = lines[j].trim();
                        if !next_line.starts_with("> ") && !next_line.is_empty() {
                            // 檢查原始文本中是否有真正的換行
                            // 使用更安全的方式查找位置，避免重複文本導致的錯誤
                            if let Some(current_pos) = ctx.pending_text.find(line) {
                                if let Some(relative_next_pos) =
                                    ctx.pending_text[current_pos..].find(next_line)
                                {
                                    let next_pos = current_pos + relative_next_pos;
                                    let start_pos = current_pos + line.len();

                                    // 確保切片邊界正確
                                    if start_pos <= next_pos {
                                        let between_text = &ctx.pending_text[start_pos..next_pos];

                                        if between_text.contains('\n') {
                                            // 有真正的換行，思考內容結束
                                            break;
                                        } else {
                                            // 沒有換行，是同一段內容
                                            full_line.push_str(next_line);
                                            j += 1;
                                        }
                                    } else {
                                        // 位置計算有問題，保守處理：認為有換行
                                        debug!(
                                            "🧠 位置計算異常，保守處理（等待'\\n'換行） | start_pos: {} | next_pos: {}",
                                            start_pos, next_pos
                                        );
                                        break;
                                    }
                                } else {
                                    // 找不到下一行，認為有換行
                                    break;
                                }
                            } else {
                                // 找不到當前行，認為有換行
                                break;
                            }
                        } else if next_line.is_empty() {
                            j += 1;
                        } else {
                            break;
                        }
                    }

                    reasoning_chunks.push(full_line);
                    processed_lines = j;
                }
            } else if trimmed.is_empty() {
                // 空行，繼續
                processed_lines = i + 1;
            } else {
                // 非思考格式的內容，思考結束
                thinking_ended = true;
                processed_lines = i;
                break;
            }
        }

        // 組合思考內容
        let reasoning_output = if !reasoning_chunks.is_empty() {
            let combined_reasoning = reasoning_chunks.join("\n");
            ctx.reasoning_content.push_str(&combined_reasoning);
            if !ctx.reasoning_content.ends_with('\n') {
                ctx.reasoning_content.push('\n');
            }
            Some(combined_reasoning)
        } else {
            None
        };

        // 計算剩餘文本
        let remaining_text = if processed_lines < lines.len() {
            lines[processed_lines..].join("\n")
        } else if !ctx.current_reasoning_line.is_empty() && !thinking_ended {
            // 保留未完成的思考行
            format!("> {}", ctx.current_reasoning_line)
        } else {
            String::new()
        };

        (reasoning_output, remaining_text, thinking_ended)
    }
}

// Text 事件處理器
#[derive(Clone)]
struct TextEventHandler;
impl EventHandler for TextEventHandler {
    fn handle(&self, event: &ChatResponse, ctx: &mut EventContext) -> Option<String> {
        if let Some(ChatResponseData::Text { text }) = &event.data {
            // 處理替換模式
            if ctx.is_replace_mode && !ctx.first_text_processed {
                debug!("📝 合併第一個 Text 事件與 ReplaceResponse");
                if let Some(replace_content) = &mut ctx.replace_buffer {
                    replace_content.push_str(text);
                    ctx.first_text_processed = true;

                    // 先克隆內容，然後釋放借用
                    let content_to_process = replace_content.clone();
                    let _ = replace_content; // 明確釋放借用
                    let (reasoning_output, content_output) =
                        ThinkingProcessor::process_text_chunk(ctx, &content_to_process);

                    if reasoning_output.is_some() {
                        return Some("__REASONING_DETECTED__".to_string());
                    }
                    return content_output;
                } else {
                    // 沒有 replace_buffer，直接添加到 content
                    ctx.content.push_str(text);
                    return Some(text.clone());
                }
            } else if ctx.is_replace_mode && ctx.first_text_processed {
                debug!("🔄 重置替換模式");
                ctx.is_replace_mode = false;
                ctx.first_text_processed = false;

                // 將 replace_buffer 的內容移至 content
                if let Some(replace_content) = ctx.replace_buffer.take() {
                    ctx.content = replace_content;
                }
            }

            // 正常模式處理
            let (reasoning_output, content_output) =
                ThinkingProcessor::process_text_chunk(ctx, text);

            // 如果檢測到思考內容，返回特殊標記
            if reasoning_output.is_some() {
                // 如果同時有普通內容，需要暫存起來等待下次處理
                if let Some(content) = content_output {
                    // 將內容添加到 pending_text 開頭，確保下次處理時能發送
                    ctx.pending_text = format!("{}{}", content, ctx.pending_text);
                }
                return Some("__REASONING_DETECTED__".to_string());
            }

            return content_output;
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
