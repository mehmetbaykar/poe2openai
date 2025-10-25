use crate::types::*;
use crate::utils::{convert_poe_error_to_openai, format_bytes_length};
use poe_api_process::{ChatEventType, ChatResponse, ChatResponseData};
use salvo::prelude::*;
use std::collections::HashMap;
use tracing::{debug, error};

// Event accumulation context, used to collect state during event processing
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
    // Reasoning-related fields
    pub reasoning_content: String,
    pub in_thinking_mode: bool,
    pub thinking_started: bool,
    pub current_reasoning_line: String,
    pub pending_text: String,
    pub metadata: HashMap<String, usize>, // Used to track length of sent content
}

impl EventContext {
    pub fn get(&self, key: &str) -> Option<usize> {
        self.metadata.get(key).copied()
    }

    pub fn insert(&mut self, key: &str, value: usize) {
        self.metadata.insert(key.to_string(), value);
    }
}

// Event handler trait
trait EventHandler {
    fn handle(&self, event: &ChatResponse, ctx: &mut EventContext) -> Option<String>;
}

// Thinking content processor
#[derive(Clone)]
pub struct ThinkingProcessor;

impl ThinkingProcessor {
    // Detect thinking start marker
    fn detect_thinking_start(text: &str) -> Option<usize> {
        if let Some(pos) = text.find("*Thinking...*") {
            return Some(pos);
        }
        if let Some(pos) = text.find("Thinking...") {
            return Some(pos);
        }
        None
    }

    // Process text and separate thinking content from normal content
    // Returns (reasoning_chunk, content_chunk)
    pub fn process_text_chunk(
        ctx: &mut EventContext,
        new_text: &str,
    ) -> (Option<String>, Option<String>) {
        ctx.pending_text.push_str(new_text);

        let mut reasoning_output = None;
        let mut content_output = None;

        // If thinking mode hasn't started yet, check for thinking marker
        if !ctx.thinking_started {
            if let Some(thinking_pos) = Self::detect_thinking_start(&ctx.pending_text) {
                debug!("🧠 Thinking mode started");
                ctx.thinking_started = true;
                ctx.in_thinking_mode = true;

                // Separate content before and after the thinking marker
                let (before_thinking, after_thinking) = ctx.pending_text.split_at(thinking_pos);

                // Content before the marker as normal content
                if !before_thinking.trim().is_empty() {
                    ctx.content.push_str(before_thinking);
                    content_output = Some(before_thinking.to_string());
                }

                // Determine marker type and remove full marker
                let after_marker = if after_thinking.starts_with("*Thinking...*") {
                    after_thinking.strip_prefix("*Thinking...*").unwrap_or("")
                } else if after_thinking.starts_with("Thinking...") {
                    after_thinking.strip_prefix("Thinking...").unwrap_or("")
                } else {
                    after_thinking
                };

                ctx.pending_text = after_marker.to_string();
            } else {
                // No thinking marker, process as normal content
                if !ctx.pending_text.trim().is_empty() {
                    ctx.content.push_str(&ctx.pending_text);
                    content_output = Some(ctx.pending_text.clone());
                    ctx.pending_text.clear();
                }
                return (None, content_output);
            }
        }

        // Process content in thinking mode
        if ctx.thinking_started && ctx.in_thinking_mode {
            let (reasoning_chunk, remaining_text, thinking_ended) =
                Self::process_thinking_content(ctx);

            if let Some(reasoning_content) = reasoning_chunk {
                reasoning_output = Some(reasoning_content);
            }

            ctx.pending_text = remaining_text;

            // If thinking ended, process remaining content as normal content
            if thinking_ended {
                debug!("🧠 Thinking mode ended");
                ctx.in_thinking_mode = false;
                if !ctx.pending_text.trim().is_empty() {
                    ctx.content.push_str(&ctx.pending_text);
                    // If content_output already exists, merge content
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

    // Process content in thinking mode
    // Returns (reasoning_chunk, remaining_text, thinking_ended)
    fn process_thinking_content(ctx: &mut EventContext) -> (Option<String>, String, bool) {
        let mut reasoning_chunks = Vec::new();
        let mut thinking_ended = false;

        // Process line by line, but need to consider incomplete lines in streaming
        let lines: Vec<&str> = ctx.pending_text.lines().collect();
        let mut processed_lines = 0;

        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();

            if trimmed.starts_with("> ") || trimmed == ">" {
                // Thinking content line (including empty "> " lines)
                let thinking_content = if trimmed == ">" {
                    "" // Empty thinking line
                } else {
                    trimmed.strip_prefix("> ").unwrap_or(trimmed)
                };

                // Check if it's a complete line (might not be complete in streaming)
                if i == lines.len() - 1 && !ctx.pending_text.ends_with('\n') {
                    // Last line and no newline, might be incomplete
                    ctx.current_reasoning_line = thinking_content.to_string();
                    break;
                } else {
                    // Complete thinking line
                    let mut full_line = thinking_content.to_string();

                    // Check if subsequent lines belong to the same thinking segment (no true newline separation)
                    let mut j = i + 1;
                    while j < lines.len() {
                        let next_line = lines[j].trim();
                        if !next_line.starts_with("> ") && !next_line.is_empty() {
                            // Check if there is a true newline in the original text
                            // Use a safer way to find the position to avoid errors due to duplicate text
                            if let Some(current_pos) = ctx.pending_text.find(line) {
                                if let Some(relative_next_pos) =
                                    ctx.pending_text[current_pos..].find(next_line)
                                {
                                    let next_pos = current_pos + relative_next_pos;
                                    let start_pos = current_pos + line.len();

                                    // Ensure correct slice boundaries
                                    if start_pos <= next_pos {
                                        let between_text = &ctx.pending_text[start_pos..next_pos];

                                        if between_text.contains('\n') {
                                            // True newline, thinking content ends
                                            break;
                                        } else {
                                            // No newline, same content segment
                                            full_line.push_str(next_line);
                                            j += 1;
                                        }
                                    } else {
                                        // Position calculation issue, conservative handling: assume newline
                                        debug!(
                                            "🧠 Position calculation issue, conservative handling (waiting for '\\n' newline) | start_pos: {} | next_pos: {}",
                                            start_pos, next_pos
                                        );
                                        break;
                                    }
                                } else {
                                    // Next line not found, assume newline
                                    break;
                                }
                            } else {
                                // Current line not found, assume newline
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
                // Empty line, continue
                processed_lines = i + 1;
            } else {
                // Non-thinking format content, thinking ends
                thinking_ended = true;
                processed_lines = i;
                break;
            }
        }

        // Combine thinking content
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

        // Calculate remaining text
        let remaining_text = if processed_lines < lines.len() {
            lines[processed_lines..].join("\n")
        } else if !ctx.current_reasoning_line.is_empty() && !thinking_ended {
            // Keep incomplete thinking line
            format!("> {}", ctx.current_reasoning_line)
        } else {
            String::new()
        };

        (reasoning_output, remaining_text, thinking_ended)
    }
}

// Text event handler
#[derive(Clone)]
struct TextEventHandler;
impl EventHandler for TextEventHandler {
    fn handle(&self, event: &ChatResponse, ctx: &mut EventContext) -> Option<String> {
        if let Some(ChatResponseData::Text { text }) = &event.data {
            // Process replace mode
            if ctx.is_replace_mode && !ctx.first_text_processed {
                debug!("📝 Merging first Text event with ReplaceResponse");
                if let Some(replace_content) = &mut ctx.replace_buffer {
                    replace_content.push_str(text);
                    ctx.first_text_processed = true;

                    // Clone content first, then release borrow
                    let content_to_process = replace_content.clone();
                    let _ = replace_content; // Explicitly release borrow
                    let (reasoning_output, content_output) =
                        ThinkingProcessor::process_text_chunk(ctx, &content_to_process);

                    if reasoning_output.is_some() {
                        return Some("__REASONING_DETECTED__".to_string());
                    }
                    return content_output;
                } else {
                    // No replace_buffer, add directly to content
                    ctx.content.push_str(text);
                    return Some(text.clone());
                }
            } else if ctx.is_replace_mode && ctx.first_text_processed {
                debug!("🔄 Resetting replace mode");
                ctx.is_replace_mode = false;
                ctx.first_text_processed = false;

                // Move content from replace_buffer to content
                if let Some(replace_content) = ctx.replace_buffer.take() {
                    ctx.content = replace_content;
                }
            }

            // Normal mode processing
            let (reasoning_output, content_output) =
                ThinkingProcessor::process_text_chunk(ctx, text);

            // If reasoning content detected, return special marker
            if reasoning_output.is_some() {
                // If there is also normal content, need to store it for later processing
                if let Some(content) = content_output {
                    // Add content to the beginning of pending_text to ensure it's sent next time
                    ctx.pending_text = format!("{}{}", content, ctx.pending_text);
                }
                return Some("__REASONING_DETECTED__".to_string());
            }

            return content_output;
        }
        None
    }
}

// File event handler
#[derive(Clone)]
struct FileEventHandler;
impl EventHandler for FileEventHandler {
    fn handle(&self, event: &ChatResponse, ctx: &mut EventContext) -> Option<String> {
        if let Some(ChatResponseData::File(file_data)) = &event.data {
            debug!(
                "🖼️  Processing file event | Name: {} | URL: {}",
                file_data.name, file_data.url
            );
            ctx.file_refs
                .insert(file_data.inline_ref.clone(), file_data.clone());
            ctx.has_new_file_refs = true;

            // If there is a replace_buffer at this time, process it and send it
            if !ctx.image_urls_sent && ctx.replace_buffer.is_some() {
                // Only process if not already sent
                let content = ctx.replace_buffer.as_ref().unwrap();
                if content.contains(&format!("[{}]", file_data.inline_ref)) {
                    debug!(
                        "🖼️  Detected ReplaceResponse containing image reference [{}] to be processed immediately",
                        file_data.inline_ref
                    );
                    // Process image references in this text
                    let mut processed = content.clone();
                    let img_marker = format!("[{}]", file_data.inline_ref);
                    let replacement = format!("({})", file_data.url);
                    processed = processed.replace(&img_marker, &replacement);
                    ctx.image_urls_sent = true; // Mark as sent
                    return Some(processed);
                }
            }
        }
        None
    }
}

// ReplaceResponse event handler
#[derive(Clone)]
struct ReplaceResponseEventHandler;
impl EventHandler for ReplaceResponseEventHandler {
    fn handle(&self, event: &ChatResponse, ctx: &mut EventContext) -> Option<String> {
        if let Some(ChatResponseData::Text { text }) = &event.data {
            debug!(
                "🔄 Processing ReplaceResponse event | Length: {}",
                format_bytes_length(text.len())
            );
            ctx.is_replace_mode = true;
            ctx.replace_buffer = Some(text.clone());
            ctx.first_text_processed = false;

            // Check for file references that need processing
            if !ctx.file_refs.is_empty() && text.contains('[') {
                debug!("🔄 ReplaceResponse might contain image references, check and process");
                // Process image references in this text
                let mut processed = text.clone();
                let mut has_refs = false;

                for (ref_id, file_data) in &ctx.file_refs {
                    let img_marker = format!("[{}]", ref_id);
                    if processed.contains(&img_marker) {
                        let replacement = format!("({})", file_data.url);
                        processed = processed.replace(&img_marker, &replacement);
                        has_refs = true;
                        debug!(
                            "🖼️  Replaced image reference | ID: {} | URL: {}",
                            ref_id, file_data.url
                        );
                    }
                }

                if has_refs {
                    // If image references were actually included, return processed content immediately
                    debug!(
                        "✅ ReplaceResponse contains image references, sending processed content immediately"
                    );
                    ctx.image_urls_sent = true; // Mark as sent
                    return Some(processed);
                }
            }

            // Delay ReplaceResponse output, wait for subsequent Text events
            debug!("🔄 Delaying ReplaceResponse output, waiting for subsequent Text events");
        }
        None // Do not send directly, wait to merge with Text
    }
}

// Json event handler (for Tool Calls)
#[derive(Clone)]
struct JsonEventHandler;
impl EventHandler for JsonEventHandler {
    fn handle(&self, event: &ChatResponse, ctx: &mut EventContext) -> Option<String> {
        debug!("📝 Processing JSON event");
        if let Some(ChatResponseData::ToolCalls(tool_calls)) = &event.data {
            debug!("🔧 Processing tool calls, count: {}", tool_calls.len());
            ctx.tool_calls.extend(tool_calls.clone());
            // Return Some, indicating tool calls need to be sent
            return Some("tool_calls".to_string());
        }
        None
    }
}

// Error event handler
#[derive(Clone)]
struct ErrorEventHandler;
impl EventHandler for ErrorEventHandler {
    fn handle(&self, event: &ChatResponse, ctx: &mut EventContext) -> Option<String> {
        if let Some(ChatResponseData::Error { text, allow_retry }) = &event.data {
            error!("❌ Processing error event: {}", text);
            let (status, error_response) = convert_poe_error_to_openai(text, *allow_retry);
            ctx.error = Some((status, error_response));
            return Some("error".to_string());
        }
        None
    }
}

// Done event handler
#[derive(Clone)]
struct DoneEventHandler;
impl EventHandler for DoneEventHandler {
    fn handle(&self, _event: &ChatResponse, ctx: &mut EventContext) -> Option<String> {
        debug!("✅ Processing Done event");
        ctx.done = true;

        // Only process if image URLs were not sent
        if !ctx.image_urls_sent && ctx.replace_buffer.is_some() && !ctx.file_refs.is_empty() {
            let content = ctx.replace_buffer.as_ref().unwrap();
            debug!(
                "🔍 Checking if there are any unprocessed image references during the completion event"
            );
            let mut processed = content.clone();
            let mut has_refs = false;

            for (ref_id, file_data) in &ctx.file_refs {
                let img_marker = format!("[{}]", ref_id);
                if processed.contains(&img_marker) {
                    let replacement = format!("({})", file_data.url);
                    processed = processed.replace(&img_marker, &replacement);
                    has_refs = true;
                    debug!(
                        "🖼️  Replaced image reference before completion | ID: {} | URL: {}",
                        ref_id, file_data.url
                    );
                }
            }

            if has_refs {
                debug!("✅ Processed image references before completion");
                ctx.image_urls_sent = true; // Mark as sent
                return Some(processed);
            }
        }

        Some("done".to_string())
    }
}

// Event handler manager
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
